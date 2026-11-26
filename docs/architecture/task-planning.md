# Task Planning phase: contract

Status: **planned**. This is a design. Nothing in it is implemented unless
section 1 says so. The binding async rules remain
[async-safety.md](../engineering/async-safety.md); vocabulary is in
[product-model.md](product-model.md).

## 1. Current behavior

- `Task.planning_mode` is set at creation, changed by the desktop
  `update_task` command, carried by Project export and import, and shown as a
  "Plan" badge and a toggle labelled "AI will plan before coding".
- No execution path reads it. The coding prompt (`queue/prompt.rs`) and the
  approval digests do not include it. A Task with the flag runs exactly like
  one without it.
- `TaskPhase::Planning` has a badge and a label. No production code sets it.
- SlashIt records no run cost, and no run sets `max_budget_usd`.

The toggle's label is therefore false until the gate in section 6 ships.

## 2. Contract and invariants

For a Task with `planning_mode = true`:

1. No coding run starts without a Human-approved plan that matches the Task's
   current input and execution context. The comparison happens in the same
   board write that records the run's start.
2. A planner invocation happens only on an explicit Human action. There is no
   automatic generation, retry or replan, including after a restart.
3. Every invocation is counted durably before its process starts, and the
   count is bounded.
4. A planner cannot edit the Task Checkout.
5. One approval admits one run.
6. Unknown is reported as unknown: an interrupted invocation is not "failed at
   no cost", and a missing cost is not zero.

A Task with `planning_mode = false` behaves as today.

Out of scope for the first version: Coordinator, CLI or agent approval;
OpenCode as planner; automatic plan approval; per-step execution or progress
tracking against the plan; gating the AI Review fix and PR apply agents (they
act on the result of a run that already passed the gate).

The plan is a checkpoint on **what a Human agreed to start**. It is not a
sandbox: nothing forces the coding Worker to follow it, and Review remains the
check on what was done.

## 3. Terms

**Task revision.** A Task has no revision counter, and this design adds none.
The revision a plan is made for is a digest of what planning reads:

```text
input_digest = framed_digest("slashit.plan-input.v1",
    build_task_prompt(task, None),   // title, description, metadata, subtasks,
                                     // references, pending review feedback
    task.model, task.provider,
    task.planner_model)
```

Deriving it from `build_task_prompt` keeps one list of covered fields, as
`task_input_digest` does. Review mode and reviewer model are excluded: they do
not change what is planned, and the existing approval digests still cover
them. A priority change does invalidate a plan, because the prompt carries it.

**Execution context.** What the planner looked at and what the run will start
from:

| Field | Why |
|---|---|
| `repository_id` | The Project may be re-associated |
| `branch`, `checkout_path` | The run must use the checkout that was read |
| `head` | Any new commit changes the code planned against |
| `dirty_fingerprint` | Same fingerprint `NewAttemptDecision` uses for uncommitted work |
| `dependencies` | The recorded dependency identifiers |
| `workspace_instructions_digest` | Digest of the copied Workspace instruction text, or none |

**Plan revision.** One immutable plan document. Any change, by the planner or a
Human, makes a new revision.

## 4. Durable artifact

### 4.1 Provider-neutral representation

```rust
struct PlanDocument {
    schema_version: u16,          // 1
    summary: String,
    steps: Vec<PlanStep>,         // 1..=30
    assumptions: Vec<String>,
    risks: Vec<String>,
    open_questions: Vec<String>,
    verification: Vec<String>,    // how the result will be checked
}
struct PlanStep { title: String, detail: String, paths: Vec<String> }
```

- `paths` are informational. They are not an allow list and are never opened or
  executed by SlashIt.
- A provider adapter turns its final answer into this type. It returns a
  document or an error; free text is never stored as a plan. Claude uses
  `--json-schema` and `final_structured_output`. Another provider must pass the
  same parser and the same read-only requirement (section 7).
- Validation: unknown fields refused; at most 16 KiB serialized; at least one
  step and a non-empty summary; control characters other than newline and tab
  refused. Over-limit output is a failed attempt, never truncated, because a
  Human must approve the whole plan.
- Text passes `slashit_activity::redact` before storage. Redaction is best
  effort. The digest and the Human review cover the stored (redacted) text.
- `content_digest` is a framed digest of the canonical serialization.

### 4.2 Record on the Task

`Task.planning: Option<PlanningRecord>`, omitted when absent.

```rust
struct PlanningRecord {
    revisions: Vec<PlanRevision>,        // newest 5 keep a body
    attempts: Vec<PlanAttempt>,          // every invocation, no bodies
    approval: Option<PlanApproval>,      // at most one live
    executed: Vec<ExecutedPlan>,         // { run, revision, content_digest, approval_id }
    extra_attempts_granted: u32,
}
struct PlanRevision {
    number: u32, author: Planner | Human, parent: Option<u32>,
    input_digest, context, content_digest,
    document: Option<PlanDocument>,      // None once expired
    created_at,
}
struct PlanAttempt {
    request_id: Uuid, number: u32, planner_provider, model_requested, model_reported,
    input_digest, context, feedback: Option<String>,   // at most 4 000 characters
    outcome: Running | Succeeded { revision } | Failed { reason }
           | Cancelled | Interrupted,
    cost: Reported { usd, input_tokens, output_tokens, turns } | Unknown,
    started_at, ended_at,
}
struct PlanApproval {
    approval_id: Uuid, revision: u32, content_digest, input_digest, context,
    coding_provider, coding_model, approved_at,
}
```

- Stored in the Project board, so a transition and its evidence are one atomic
  write through `lifecycle` (persist before publish). The board is rewritten on
  every Task write, hence the bounds: 5 bodies of 16 KiB each. The approved
  revision's body never expires while the approval is live. Executed entries
  keep digests; an expired body is shown as expired.
- New Activity kinds: `PlanRequested`, `PlanReady`, `PlanFailed`,
  `PlanInterrupted`, `PlanApproved`, `PlanRefused`.
- **Compatibility.** Boards and Activity refuse unknown fields and kinds. A
  build without this feature cannot load a board once any Task has a planning
  record. Release notes must say so.
- Project export carries `planning_mode` as today, and no plan, approval or
  attempt. An imported Task needs a new plan.

## 5. States and transitions

The state is derived from the record, in this order. It is not stored twice, and
`TaskStatus` and `TaskPhase` gain no values. `TaskPhase::Planning` stays unset:
startup reconciliation resets the phase of a Backlog Task, so the phase cannot
carry this.

| State | Condition |
|---|---|
| `Off` | `planning_mode = false` |
| `Generating` | the newest attempt is `Running` |
| `Approved` | a live approval whose `input_digest` matches the Task |
| `AwaitingReview` | the newest revision matches the Task, with no live approval |
| `Stale` | a revision exists, and the newest or approved one no longer matches |
| `NeedsPlan` | no revision (the newest attempt, if any, says why) |

`Stale` computed from the Task alone covers input only. Context drift needs Git,
so it is found when the plan is opened, approved or consumed.

| # | From | Event (actor) | Guard | Write | To |
|---|---|---|---|---|---|
| T0 | `Off` | planning turned on (Human) | Task not In Progress | `planning_mode = true`; a Queued Task goes to Backlog | derived from the record |
| T1 | any but `Off`, `Generating` | Generate (Human) | G1 | attempt `Running`, count, checkout record, revoke approval | `Generating` |
| T2 | `AwaitingReview`, `Stale`, `Approved` | Request revision with feedback (Human) | G1, feedback non-empty | as T1, with feedback and parent | `Generating` |
| T3 | `Generating` | valid document | input and context unchanged | revision, `Succeeded`, cost | `AwaitingReview` |
| T4 | `Generating` | valid document, Task input changed | | revision, `Succeeded`, cost | `Stale` |
| T5 | `Generating` | error, timeout, turn or budget limit, invalid output, checkout changed | | `Failed`, cost | previous derived state |
| T6 | `Generating` | Cancel (Human), shutdown | provider reaped | `Cancelled`, cost | previous derived state |
| T7 | `Generating` | startup finds it | | `Interrupted`, cost `Unknown` | previous derived state |
| T8 | `AwaitingReview`, `Approved`, `Stale` | Edit plan (Human) | valid document | Human revision under current input and freshly read context, revoke approval | `AwaitingReview` |
| T9 | `AwaitingReview` | Approve (Human) | G2 | approval | `Approved` |
| T10 | `AwaitingReview` or `Approved` with drifted context | Re-approve (Human) | G2, Human shown old and new context | revision copy under new context, approval | `Approved` |
| T11 | `Approved` | Revoke (Human) | no run starting | drop approval | `AwaitingReview` |
| T12 | `Approved` | run start | G3 | `executed` entry, drop approval, `RunStarted` | `AwaitingReview` (consumed) |
| T13 | `Approved` | run start, G3 fails | | drop approval, `PlanRefused`, Task to Backlog | `Stale` or `AwaitingReview` |
| T14 | any but `Off` | Task input edited | | none (derived) | `Stale` |
| T15 | any but `Generating` | planning turned off (Human) | | `planning_mode = false`, drop approval | `Off` |

After T12 the revision may be approved again, without a new invocation: by T9
if its context still matches, otherwise by T10. The panel says that a run
already used it.

T10 is allowed only while `input_digest` still matches. If the Task input
changed, only T1, T2 or T8 apply.

**G1 (may invoke).** Task is in Backlog or Error; Project active; no run, plan
or other lifecycle operation holds the Task lease; no pending cleanup,
republish, review fix residue or unfinished integration; planner model set;
attempts below the limit; process admission open. A Task in Worker recovery may
plan (its next attempt needs a plan), and planning changes nothing in its
recovery record.

**G2 (may approve).** The revision number and `content_digest` equal what the
Human was shown; `input_digest` matches the Task in the staged write; the
context re-read under the Task lease equals the revision's; no attempt running.
`approval_id` makes a retried command idempotent.

**G3 (may start).** In the write of `record_run_start_for`: approval present;
revision body present with the approved `content_digest`; `input_digest`,
coding provider, coding model and every context field equal. Existing guards
(approved execution, new attempt, membership, residue) must also pass; no
approval substitutes for another.

## 6. Approval gate

- **One choke point.** Every coding run records its start in
  `record_run_start_for`, so the scheduler, "run now", a new attempt, and a
  Coordinator-approved queue are all gated there.
- **Early refusal.** Entering Queue or In Progress without a live approval that
  matches the input is refused with the reason: `queue_entry_refusal`,
  promotion eligibility, "run now", and Coordinator queue approval. This stops
  a gated Task from holding a capacity slot. It is convenience; G3 is the
  guarantee.
- **Refusal at start (T13)** moves the Task to Backlog, not Error: nothing ran,
  and the scheduler no longer sees it. No run number is spent.
- **Moves keep the approval.** Unlike `approved_execution`, a plan approval
  survives a column change, since the Task must move after it is approved.
  Revocation happens through T2, T8, T11, T12, T13 and T15.
- **Human Review "request changes"** adds feedback to the prompt, so the plan
  is stale. The decision is recorded and the Task waits in Backlog for a plan.
- **Coordinator.** It cannot generate, approve, revoke or change
  `planning_mode`. Its edits to a Task make the plan stale like any edit. An
  additional-request (`ExecuteRequest`) approval is refused for a gated Task in
  version 1, because the plan does not cover the request.
- **Authority.** Plan commands take `GuiMutation` and exist only on the desktop.
  No IPC or CLI verb approves, and none may be added without a separate
  decision.
- **Prompt.** After the run-start check, the approved document is appended to
  the launch prompt as a fenced block (the `request_block` rule: the fence is
  longer than any backtick run inside), under "Approved Plan", followed by a
  statement that it does not override the Task instructions or constraints. The
  plan is model-written from repository content, so the fence is not a defense
  against injection; Human review is the control. The plan is not part of
  `task_input_digest` or `approved_input_digest`.
- **Existing flagged Tasks.** When the gate ships, Tasks already holding
  `planning_mode = true` become gated. SlashIt does not flip the flag. A flagged
  Task already Queued is returned to Backlog with the reason; a run in progress
  is not touched.

## 7. Running the planner

- Starts only through G1, under the Task lifecycle lease and a process
  admission lease, holding the Task's provider lease. At most one planner runs
  per instance in version 1.
- Acquires the Task Checkout through the executor's own path, so it reads
  exactly what a run would start from. The T1 write records the checkout, as a
  run start does. A checkout locks the Task's dependencies; the UI says so
  before the first generation.
- Claude only, with `ToolAccess::ReadOnly`, `disable_mcp: true`, the structured
  output schema, and no session resume. A CLI without `--restricted` or
  `--json-schema` refuses; there is no fallback to wider access.
- OpenCode is refused as planner: its only policy allows edit and shell. A Task
  whose coding Worker is OpenCode can still be planned by Claude.
- `planner_model` is an explicit Task field, parsed like `reviewer_model`.
  Unset refuses to generate; nothing falls back to the CLI default.
- The prompt is the Task prompt without its "Instructions" section, planning
  instructions, and for a revision the previous document and the Human's
  feedback, each fenced.
- After the process is reaped, `head` and `dirty_fingerprint` are read again. A
  difference fails the attempt (T5).
- The owner of the process observes its result, cancels it on Cancel and
  shutdown, and waits for it before releasing the leases.

## 8. Bounds, cost and visibility

Proposed defaults; the repository owner confirms them.

| Bound | Value |
|---|---|
| Invocations per Task | 10, counted over every outcome |
| Extra grant | +5 per explicit Human confirmation, recorded |
| Concurrent planners | 1 per instance, 1 per Task |
| Turns per invocation | 20 (`max_turns`) |
| Budget per invocation | USD 1.00 (`max_budget_usd`) |
| Wall time | 10 minutes, then kill and reap |
| Plan size, kept bodies | 16 KiB, 5 |
| Automatic retries | 0 |

- The count is the number of `attempts`, written in T1 before the spawn. A
  launch that fails before a process exists still counts; this is simpler than
  proving no call was made.
- The budget is enforced by the provider CLI and can be exceeded by the call in
  flight. The UI calls it a limit passed to the provider, not a guarantee.
- Cost is read from the final `result` event when present. A killed,
  interrupted or silent run records `Unknown`.
- The plan panel shows: state and why; attempts used and remaining; the limits
  before the Human clicks Generate; per attempt the model requested and
  reported, duration, outcome and cost; a total shown as "reported USD X, plus
  N invocations of unknown cost". Reported cost is the provider's figure, not a
  bill.

## 9. Recovery

At startup, before any scheduling:

- A `Running` attempt becomes `Interrupted` with cost `Unknown`. It is never
  restarted. Existing revisions and a matching approval are unaffected (T1 had
  already revoked any approval).
- A new attempt requires the provider lease to be proven free. If it cannot be,
  generation is refused with the same guidance a coding run gives.
- The checkout is preserved. Nothing is reset or cleaned. A changed context
  simply shows in the next G2 or G3.
- A recorded approval survives restart. It needs no in-process confirmation,
  because G3 re-proves every bound value.
- A live approval, an `InProgress` Task and no `RunStarted`: startup returns the
  Task to Queue as today, and G3 applies when it starts.
- A `RunStarted` write consumed the approval, so recovery of that run follows
  Worker recovery; a new attempt needs a new approval (T9 or T10) or a new plan.

## 10. Failure cases

| # | Case | Required result |
|---|---|---|
| F1 | T1 write refused | No process starts, nothing counted |
| F2 | T1 published but not durable | No process starts; reported as such; the visible attempt is later `Interrupted` |
| F3 | Spawn fails | `Failed`, counted, cost `Unknown` |
| F4 | CLI lacks read-only or schema flags | `Failed` with the update message; no wider access |
| F5 | Timeout, turn limit, budget limit | Killed and reaped; `Failed` naming the limit; partial output discarded |
| F6 | Invalid, empty or oversized document | `Failed`; no revision |
| F7 | Checkout changed during planning | `Failed`; the change is left in place |
| F8 | Task edited during planning | Revision stored as `Stale` |
| F9 | Result write fails | Reported as outcome not recorded; stays `Running` until retried or startup marks it `Interrupted`; never approvable from memory |
| F10 | Crash after provider finished, before T3 | `Interrupted`; output lost; cost `Unknown` |
| F11 | Cancel races completion | One write wins under the Task lease; the loser changes nothing |
| F12 | Two Generate clicks | Same `request_id`: one invocation. Different: the second is refused |
| F13 | Approve races an edit or a new revision | The staged check refuses; no approval |
| F14 | Context drifted after approval | T13, naming the field that differs |
| F15 | Delete or finish while `Generating` | The planner is cancelled and reaped under the lease first; if it cannot be, the action is refused |
| F16 | Move to Queue while `Generating` | Refused |
| F17 | Project removed | Generation counts as a live run and blocks removal |
| F18 | Attempt limit reached | Refused, offering the explicit grant |
| F19 | Approved body missing or digest mismatch | Board treated as inconsistent: T13, never "no plan needed" |
| F20 | Run fails or is interrupted after T12 | Approval stays consumed |

## 11. Test cases

Deterministic barriers and injected errors, no sleeps. A stand-in `claude`
reads stdin to EOF and records its argv and call count.

Domain (pure):

- D1 `input_digest` changes for each covered field and not for review mode,
  reviewer model, position or status.
- D2 Document validation: each limit, unknown field, control characters.
- D3 State derivation for every row of the state table.
- D4 Retention keeps 5 bodies and the approved one; counts and executed entries
  survive.
- D5 A board without planning fields loads and re-serializes without them.

Transitions and gate:

- A1 T0 to T15, each with its allowed source states and one refused source.
- A2 G1, G2, G3: one test per condition, asserting nothing else changed.
- A3 Each start path (poll, "run now", new attempt, Coordinator queue approval,
  review-feedback rerun) with no, stale and valid approval; zero Worker
  launches in the first two.
- A4 Approval consumed in the same write as `RunStarted`; a refused write leaves
  both unchanged.
- A5 Restart between approval and start; restart with a drifted checkout.
- A6 `planning_mode = false` runs exactly as before, with the same digests.
- A7 Every plan command is classified as a mutation; no IPC verb reaches one.
- A8 The plan cannot close its fence; the approval digests ignore it.

Planner and recovery:

- P1 argv carries read-only tools, `--restricted`, strict MCP, the schema,
  turn and budget limits, the planner model; never a bypass flag.
- P2 F1 to F12, one test each, asserting attempt count and spawn count.
- P3 Barrier tests for F11, F13, F15, F16.
- P4 Startup with a `Running` attempt: `Interrupted`, zero spawns, count kept.
- P5 Cancel and shutdown reap the process group before releasing leases.
- P6 Cost parsed from a result event; absent or malformed gives `Unknown`.
- P7 Limit and grant: the eleventh call is refused until granted.

Desktop acceptance:

- U1 Generate, request revision, edit, approve, run.
- U2 Edit the Task after approval: shown stale, start refused with the reason.
- U3 Quit during generation, restart: interrupted, no new call, cost unknown.

Not provable with a stand-in, to be reported as `INCONCLUSIVE` until probed on a
real CLI: read-only enforcement, schema with `--restricted` for this prompt,
cost field names, budget enforcement.

## 12. Implementation Tasks

| ID | Task | Depends on | Done when |
|---|---|---|---|
| P0 | Make the UI truthful: label the toggle and badge as not yet enforced, or hide them. Change no Task data | none | No text claims planning happens |
| P1 | Domain: `PlanDocument`, `PlanningRecord`, digests, validation, state derivation, retention, Activity kinds, compatibility note | none | D1 to D5 |
| P2 | Capture run cost and usage from the Claude result event; probe the real CLI and record the field names | none | P6 |
| P3 | `planner_model` field and planner runner: prompt, read-only launch, parsing, limits | P1, P2 | P1 tests, F3 to F6 |
| P4 | Orchestration: T1 to T7, leases, checkout acquisition, cancel, shutdown, startup recovery | P1, P3 | P2 to P5, P7 |
| P5 | Review and approval commands: T0, T8 to T11, T15, G2 | P1, P4 | their A1, A2, A7 rows |
| P6 | Gate: G3, early refusals, T12, T13, prompt block, Coordinator refusals, existing flagged Tasks. Behind a build-time off switch | P5 | A3 to A6, A8, F14, F19, F20 |
| P7 | Plan panel: review, revision, edit, approval, limits and cost | P5 | component tests |
| P8 | Enable the gate, restore the label, update product-model.md, release note, acceptance journeys | P0, P6, P7 | U1 to U3, real CLI probes recorded |

P0, P1 and P2 can start in parallel. The gate must not be enabled before P7:
without the panel a flagged Task could never run. P4, P5 and P6 each need the
review record from async-safety.md.

## 13. Decisions for the repository owner

| # | Decision | Recommendation |
|---|---|---|
| O1 | Limits in section 8 | Accept as defaults; make configurable later |
| O2 | Existing flagged Tasks become gated | Yes, announced; the Human can turn the flag off |
| O3 | Review feedback requires a new plan | Yes; revisit if the cost is high in use |
| O4 | Planning creates the Task Checkout | Yes; planning in the primary checkout would read the wrong base |
| O5 | One-time downgrade break from new board fields | Accept with a release note |

## 14. Not verified

No code was run for this design. Claude Code's cost and usage field names and
its budget behavior were not checked. Whether a read-only run loads the
project `CLAUDE.md` was not checked. The current "request changes" transition,
the Project removal check and the dirty fingerprint's coverage were not traced;
P4 to P6 must confirm them before relying on sections 3, 6 and F17.
