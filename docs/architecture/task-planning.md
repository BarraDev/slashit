# Task Planning phase: contract

Status: **planned**. This is a design. Nothing in it is implemented unless
section 1 says so. The binding async rules remain
[async-safety.md](../engineering/async-safety.md), with
[concurrency-standard.md](../engineering/concurrency-standard.md) as the
reference; vocabulary is in [product-model.md](product-model.md).

Choices marked **(O1)** to **(O11)** are not settled. The text describes the
recommended option so the contract is complete; section 15 lists each one for a
Human to decide.

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
7. No Task lifecycle lease, process admission lease, Project gate or board
   guard is held for the lifetime of a planner process. The process has one
   owner, which needs none of the first three to finish; it takes the board
   guard only for its single outcome write.
8. Cancellation, Task deletion and shutdown signal that owner and wait, with a
   bound, until it has reaped the process. They never wait for a lock that the
   owner releases only when the process exits.

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
           | Cancelled { by: Human | TaskChange | Shutdown } | Interrupted,
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
- **Compatibility (O5).** Boards and Activity refuse unknown fields and kinds. A
  build without this feature cannot load a board once any Task has a planning
  record. Release notes must say so.
- Project export carries `planning_mode` as today, and no plan, approval or
  attempt. An imported Task needs a new plan.

## 5. States and transitions

The durable state is derived from the record, in this order. It is not stored
twice, and `TaskStatus` and `TaskPhase` gain no values. `TaskPhase::Planning`
stays unset: startup reconciliation resets the phase of a Backlog Task, so the
phase cannot carry this.

| State | Condition |
|---|---|
| `Off` | `planning_mode = false` |
| `Generating` | the newest attempt is `Running` |
| `Approved` | a live approval whose `input_digest` matches the Task |
| `AwaitingReview` | the newest revision matches the Task, with no live approval |
| `Stale` | a revision exists, and the newest or approved one no longer matches |
| `NeedsPlan` | no revision (the newest attempt, if any, says why) |

A persisted `Running` does not prove that anything is running. Whether it is
comes from the owner registry (section 8), and the UI shows one of:

| Shown | Persisted | Registry |
|---|---|---|
| Preparing | no new attempt yet | owner, no process yet |
| Generating | `Running` | owner, not cancelled |
| Stopping | `Running` | owner, cancel requested |
| Outcome not recorded | `Running` | no owner |

`Stale` computed from the Task alone covers input only. Context drift needs Git,
so it is found when the plan is opened, approved or consumed.

| # | From | Event (actor) | Guard | Write | To |
|---|---|---|---|---|---|
| T0 | `Off` | planning turned on (Human) | Task not In Progress | `planning_mode = true`; a Queued Task goes to Backlog | derived from the record |
| T1 | any but `Off`, `Generating` | Generate (Human) | G1 | attempt `Running`, count, checkout record, revoke approval | `Generating` |
| T2 | `AwaitingReview`, `Stale`, `Approved` | Request revision with feedback (Human) | G1, feedback non-empty | as T1, with feedback and parent | `Generating` |
| T3 | `Generating` | valid document (owner) | G4, input unchanged | revision, `Succeeded`, cost | `AwaitingReview` |
| T4 | `Generating` | valid document (owner) | G4, Task input changed | revision, `Succeeded`, cost | `Stale` |
| T5 | `Generating` | error, timeout, turn or budget limit, invalid output, checkout changed (owner) | G4 | `Failed`, cost | previous derived state |
| T6 | `Generating` | cancel accepted (owner, after the process is reaped) | G4 | `Cancelled`, cost | previous derived state |
| T7 | `Generating` | reconciliation finds no owner (startup, or before a plan command) | Task lease held, or startup; registry empty for the Task | `Interrupted`, cost `Unknown` | previous derived state |
| T8 | `AwaitingReview`, `Approved`, `Stale` | Edit plan (Human) | valid document, no owner | Human revision under current input and freshly read context, revoke approval | `AwaitingReview` |
| T9 | `AwaitingReview` | Approve (Human) | G2 | approval | `Approved` |
| T10 | `AwaitingReview` or `Approved` with drifted context | Re-approve (Human) | G2, Human shown old and new context | revision copy under new context, approval | `Approved` |
| T11 | `Approved` | Revoke (Human) | no run starting | drop approval | `AwaitingReview` |
| T12 | `Approved` | run start | G3 | `executed` entry, drop approval, `RunStarted` | `AwaitingReview` (consumed) |
| T13 | `Approved` | run start, G3 fails | | drop approval, `PlanRefused`, Task to Backlog | `Stale` or `AwaitingReview` |
| T14 | any but `Off` | Task input edited | | none (derived) | `Stale`; a `Generating` Task stays `Generating` and ends in T4 |
| T15 | any but `Generating` | planning turned off (Human) **(O9)** | | `planning_mode = false`, drop approval | `Off` |

After T12 the revision may be approved again, without a new invocation: by T9
if its context still matches, otherwise by T10. The panel says that a run
already used it.

T10 is allowed only while `input_digest` still matches. If the Task input
changed, only T1, T2 or T8 apply.

**G1 (may invoke).** Checked under the Task lifecycle lease. Task is in Backlog
or Error; Project active; the Task has no owner of any kind (coding run, AI
review or fix, PR helper or side effect, planner); no pending cleanup,
republish, review fix residue or unfinished integration; planner model set;
attempts below the limit; process admission open; the start guard allows new
work; agent capacity and the planner slot free **(O6)**; provider absence proven
(section 8.2). A Task in Worker recovery may plan (its next attempt needs a
plan), and planning changes nothing in its recovery record.

**G2 (may approve).** The revision number and `content_digest` equal what the
Human was shown; `input_digest` matches the Task in the staged write; the
context re-read under the Task lease equals the revision's; no attempt running.
`approval_id` makes a retried command idempotent.

**G3 (may start).** In the write of `record_run_start_for`: approval present;
revision body present with the approved `content_digest`; `input_digest`,
coding provider, coding model and every context field equal. Existing guards
(approved execution, new attempt, membership, residue) must also pass; no
approval substitutes for another.

**G4 (may record an outcome).** In the staged board write: the Task exists and
the attempt with the owner's `request_id` is still `Running`. Otherwise nothing
is written.

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
  Revocation happens through T1, T2, T8, T11, T12, T13 and T15.
- **Human Review "request changes" (O3)** adds feedback to the prompt, so the plan
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
- **Existing flagged Tasks (O2).** When the gate ships, Tasks already holding
  `planning_mode = true` become gated. SlashIt does not flip the flag. A flagged
  Task already Queued is returned to Backlog with the reason; a run in progress
  is not touched.

## 7. The planner invocation

- Claude only, with `ToolAccess::ReadOnly`, `disable_mcp: true`, the structured
  output schema, and no session resume. A CLI without `--restricted` or
  `--json-schema` refuses; there is no fallback to wider access.
- OpenCode is refused as planner: its only policy allows edit and shell. A Task
  whose coding Worker is OpenCode can still be planned by Claude.
- `planner_model` is an explicit Task field, parsed like `reviewer_model`.
  Unset refuses to generate; nothing falls back to the CLI default.
- It reads the Task Checkout, acquired through the executor's own path, so it
  sees what a run would start from. The T1 write records the checkout, as a run
  start does. A checkout locks the Task's dependencies; the UI says so before
  the first generation **(O4)**.
- The prompt is the Task prompt without its "Instructions" section, planning
  instructions, and for a revision the previous document and the Human's
  feedback, each fenced.
- The process is a leader of its own process group and holds the Task's provider
  lease, exactly as a coding Worker does.

## 8. Ownership and synchronization

Two things with different lifetimes are kept apart. The **lifecycle lease**
serializes short decisions about a Task. The **planner owner** lives as long as
the process. This is the split the executor already uses for coding runs and PR
helpers: admission takes the lease, registers an owner and releases it.

### 8.1 What is held, by whom, for how long

| Resource | Holder | Held for |
|---|---|---|
| Task lifecycle lease | the command (Generate, Approve, delete, a status move) | its own decision and writes; never a planner's lifetime |
| Process admission lease | Generate | until the owner future is spawned |
| Owner registration | the owner | from admission until the process is reaped and the outcome write was tried |
| Admission permit and planner slot | the owner | same |
| Provider lease | the process (inherited descriptor) | its whole life |
| Process handle | the owner future only | until reaped |
| Board write guard | any writer | one staged write |

The registry is a map in the executor, keyed by Task:

```text
PlannerOwner { request_id, phase, cancel: watch, done: watch }
phase: Open | CancelRequested { by } | Sealed
```

It is behind a synchronous mutex that is never held across `.await` and never
together with the board guard.

**The owner never takes** a Task lifecycle lease, a Project gate or a
Conversation lock, and never waits for process admission. Anything that holds
one of these may therefore wait for the owner without deadlock. Whoever waits
for an owner holds neither the board guard nor the registry mutex.

The owner counts everywhere an owner counts today: `is_task_running`,
`running_task_count`, the live-run answers and the quit confirmation. It is
exclusive with every other owner of the Task, in both directions.

### 8.2 Admission (the start of T1 and T2)

1. Enter process admission. A closed fence refuses.
2. Check the start guard.
3. Acquire the lifecycle lease (bounded, as for any Human command).
4. If an attempt with this `request_id` exists, return it. Nothing else happens.
5. Reconcile an ownerless `Running` attempt (T7). If that write fails, refuse.
   Edit plan, Approve, Revoke and turning planning off do the same under their
   lease before their own guard, so a lost outcome never forces a new invocation.
   The registry is read before the board: the owner writes before it
   deregisters, so an empty registry means no later write is coming.
6. Check G1, take the permit and the slot, and register the owner. From here a
   cancel can be accepted.
7. Prove provider absence (`prove_absent_for_start`) and take the provider
   lease, **before** the checkout is recorded, so a crash cannot leave a
   recorded checkout with no lease file.
8. Acquire the checkout and read the execution context.
9. If a cancel was accepted, stop: release what was taken and write nothing.
10. Write T1. G1's Task conditions are checked again in this staged write, and
    the prompt and `input_digest` come from the same staged Task. A refused or
    not-durable write starts no process.
11. Spawn the owner future, moving the registration, permit, slot and provider
    lease into it. Release the lifecycle and admission leases. The command
    answers **accepted**, not completed.

Any early return releases everything through drop guards, and the registration
guard publishes `done`, so a waiting canceller is answered. Steps 3 to 11 take as
long as the existing start path (Git subprocesses), never as long as a planner.

### 8.3 Completion

The owner future is the only code that holds the process handle, so it is the
only code that kills and reaps it. Others signal it.

1. If a cancel is already requested, do not spawn.
2. Spawn. Then wait for the first of: cancel, the wall-time limit, process exit.
3. Kill the process group and reap, on every branch, including a normal exit.
4. After a normal exit, parse the document and re-read `head` and
   `dirty_fingerprint`. This read is bounded and abandoned if a cancel arrives.
5. **Seal.** Under the registry mutex: `CancelRequested` gives `Cancelled`;
   `Open` gives the result of steps 2 to 4. Either way the phase becomes
   `Sealed`. This single step decides cancel against completion.
6. Write the outcome once, under G4, through the persist-before-publish path.
   Only `planning` and Activity are changed, on the staged Task. The owner never
   writes a Task snapshot it read earlier, and never changes status, phase or
   any input field.
7. Drop the permit, the slot and the registration, then publish `done` with the
   sealed outcome and whether it was recorded.

If the write in step 6 fails, the result is discarded and the owner still ends:
a stuck disk must not keep a Task owned. The attempt stays `Running` with no
owner ("outcome not recorded") until T7. A published but not durable write is
adopted in memory and reported as such, as for other board writes.

The registration guard is created before the process handle, so on a panic the
handle is dropped (killing the group) before the registration disappears. A
panic does not prove that the process was reaped; the provider lease check of
the next start does.

### 8.4 Cancellation

`cancel_plan(task_id, request_id)` takes **no lifecycle lease**.

| Registry answer | Effect | Reply |
|---|---|---|
| `Open` for that request | phase `CancelRequested`, signal | wait for `done` |
| `CancelRequested` | none; the first reason is kept | wait for `done` |
| `Sealed` | none | wait for `done`; report the outcome that won |
| no owner, or another `request_id` | none | "not running", with the recorded attempt |

- An accepted cancel guarantees that no revision is stored from that attempt,
  even if the provider had already answered.
- The wait is bounded by `AGENT_SHUTDOWN_TIMEOUT`. On timeout the reply is
  "cancel requested, still stopping". The request cannot be withdrawn, the owner
  stays registered, and no one may treat the Task as free.
- The reply separates cancelled before anything was written, cancelled and
  recorded, cancelled but not recorded, finished before the cancel, and still
  stopping.
- A repeat is harmless. A stale `request_id` cannot cancel a newer attempt.
- Cancel changes no status.

`end_ownership_under_lease`, which status moves and `stop_task` use, ends a
planner the same way (`by: TaskChange`) while the caller holds the lease. That
is safe only because the owner never asks for it.

### 8.5 Task deletion

Inside `lifecycle::delete`, under its lease:

1. Check the refusals that do not depend on the planner (Project removed,
   unfinished integration or restack). A refusal here leaves the planner running.
2. Cancel the planner (`by: TaskChange`) and wait for `done`. On timeout the
   delete is refused. The message says that cancellation was requested.
3. Run the existing cleanup and record removal. The lease is still held, so no
   new owner can appear.

If step 3 is refused (for example Git will not remove a dirty checkout), the
Task and its checkout remain and the planner stays cancelled. A failed outcome
write in step 2 does not block deletion: the record is about to go.

Only an explicit delete does this. Automatic terminalization sees
`is_task_running` and refuses, as today. Dragging a card to Done is also refused
while an owner exists. Project removal, Project deletion, detach and export
refuse while a planner is registered and cancel nothing **(O8)**.

### 8.6 Shutdown and quiesce

`quiesce_for_shutdown`, in order:

1. Close process admission.
2. Signal every registered planner (`by: Shutdown`), before waiting for
   anything.
3. Wait for admitted starts to drain. An admission in flight reaches step 9 or
   11 of section 8.2.
4. Signal every planner again, which covers an owner registered after step 2.
5. Wait for each `done`, bounded, **without taking any Task lease**.
6. If an owner remains, return an error and keep instance ownership, so
   `drain_execution` retries.

A shutdown given a grace period lets a planner finish within it, like other
owners, and then cancels **(O10)**. `close_admission_cancelling_runs` cancels
planners at once. A quiesce counts planners as owned work. A failed outcome
write does not block shutdown; startup reconciles it.

### 8.7 How the binding rules apply

| async-safety.md rule | Where |
|---|---|
| 1 contract, 2 owner | accepted, running, outcome and unknown: sections 5, 8.1 to 8.4 |
| 3 logical operations | admission under one lease, one seal, compare-and-set write: 8.2, 8.3 |
| 4 binding | G2, G3; restart in section 11 |
| 5 honest cancellation | 8.4, 10 |
| 6 IPC correlation | `request_id` on Generate and cancel |
| 7 bounds | section 10; one outcome write, no automatic retry |
| 8, 9 ordering tests | section 13 |
| 10 not exercised | section 16 |

## 9. Task mutation while a planner is registered

| Operation | Result |
|---|---|
| Edit title, description, metadata, subtasks, references, feedback, priority, model, provider, planner model | Allowed; the planner continues; the revision is stored `Stale` |
| Edit review mode, reviewer model, position within the column | Allowed; no effect on the plan |
| Generate, same `request_id` | Returns the existing attempt |
| Generate, other `request_id` | Refused |
| Edit plan, Approve, Revoke | Refused |
| Turn planning off | Refused; cancel first **(O9)** |
| Move to Queue or In Progress, "run now", new attempt | Refused before any owner is ended: by the owner check, and later also by the gate |
| Move between other non-terminal columns, `stop_task` | Planner cancelled and reaped, then the move **(O7)**; refused if it cannot be reaped |
| Move to Done, automatic terminalization | Refused |
| Delete the Task | Section 8.5 |
| AI review or fix, PR helper, checkout removal or reclaim | Refused: the Task has an owner |
| Remove or delete the Project, detach, export | Refused **(O8)** |

An edit and the outcome write are ordered by the board write guard. In either
order the derived state is `Stale`, because staleness is computed from digests
and not from which write came first. Correctness therefore does not depend on
every edit command taking the lifecycle lease. Git is not covered by the guard:
a commit made after the context read in 8.3 is caught by G2 or G3.

## 10. Bounds, cost and visibility

Proposed defaults **(O1)**.

| Bound | Value |
|---|---|
| Invocations per Task | 10, counted over every outcome |
| Extra grant | +5 per explicit Human confirmation, recorded |
| Concurrent planners | 1 per instance, 1 per Task |
| Turns per invocation | 20 (`max_turns`) |
| Budget per invocation | USD 1.00 (`max_budget_usd`) |
| Wall time | 10 minutes, then kill and reap |
| Wait for an owner to stop | `AGENT_SHUTDOWN_TIMEOUT`, then report still stopping |
| Outcome writes | 1 per attempt, then reconciliation |
| Plan size, kept bodies | 16 KiB, 5 |
| Automatic retries | 0 |

- The count is the number of `attempts`, written in T1 before the spawn. A
  launch that fails before a process exists still counts; this is simpler than
  proving no call was made.
- The budget is enforced by the provider CLI and can be exceeded by the call in
  flight. The UI calls it a limit passed to the provider, not a guarantee.
- Cost is read from the final `result` event when present, also for a cancelled
  or failed run. A killed, interrupted or silent run records `Unknown`.
- The plan panel shows: state and why; attempts used and remaining; the limits
  before the Human clicks Generate; per attempt the model requested and
  reported, duration, outcome and cost; a total shown as "reported USD X, plus
  N invocations of unknown cost". Reported cost is the provider's figure, not a
  bill.

## 11. Recovery

At startup, only after the process holds instance ownership, and before it
schedules work or accepts commands:

- A `Running` attempt becomes `Interrupted` with cost `Unknown` (T7). It is
  never restarted. Existing revisions are unaffected (T1 had already revoked any
  approval).
- If that write fails, the attempt shows "outcome not recorded". It blocks no
  other Task. The next plan command for the Task retries T7 first (8.2 step 5),
  and no approval or run can use an attempt that produced no stored revision.
- The registry starts empty. Nothing is resumed or adopted.
- A planner that outlived a crash still holds the provider lease. Generation and
  coding starts are refused until it exits. A surviving spawn mark needs a person
  to inspect the checkout, as for a coding run. SlashIt does not kill a process
  from a recorded pid.
- On Windows there is no provider lease, so a surviving planner cannot be ruled
  out. It was launched read-only **(O11)**.
- The checkout is preserved. Nothing is reset or cleaned. A changed context
  simply shows in the next G2 or G3.
- A recorded approval survives restart. It needs no in-process confirmation,
  because G3 re-proves every bound value.
- A live approval, an `InProgress` Task and no `RunStarted`: startup returns the
  Task to Queue as today, and G3 applies when it starts.
- A `RunStarted` write consumed the approval, so recovery of that run follows
  Worker recovery; a new attempt needs a new approval (T9 or T10) or a new plan.

## 12. Failure cases

| # | Case | Required result |
|---|---|---|
| F1 | T1 write refused | No process starts, nothing counted, everything released |
| F2 | T1 published but not durable | No process starts; reported as such; the ownerless attempt ends in T7 |
| F3 | Spawn fails | `Failed`, counted, cost `Unknown` |
| F4 | CLI lacks read-only or schema flags | `Failed` with the update message; no wider access |
| F5 | Timeout, turn limit, budget limit | Killed and reaped; `Failed` naming the limit; partial output discarded |
| F6 | Invalid, empty or oversized document | `Failed`; no revision |
| F7 | Checkout changed during planning | `Failed`; the change is left in place |
| F8 | Task edited during planning | Revision stored as `Stale` |
| F9 | Outcome write fails | Owner ends; "outcome not recorded"; never approvable; ends in T7 |
| F10 | Crash after the provider finished, before the write | `Interrupted`; output lost; cost `Unknown` |
| F11 | Cancel races completion | The seal decides; the reply says which won |
| F12 | Two Generate clicks | Same `request_id`: one invocation. Different: the second is refused |
| F13 | Approve races an edit or a new revision | The staged check refuses; no approval |
| F14 | Context drifted after approval | T13, naming the field that differs |
| F15 | Process cannot be reaped in time | Cancel reports still stopping; delete and moves are refused; shutdown keeps instance ownership |
| F16 | Move to Queue while generating | Refused; the planner continues |
| F17 | Project removed or deleted while generating | Refused; the planner continues |
| F18 | Attempt limit reached | Refused, offering the explicit grant |
| F19 | Approved body missing or digest mismatch | Board treated as inconsistent: T13, never "no plan needed" |
| F20 | Run fails or is interrupted after T12 | Approval stays consumed |
| F21 | Delete refused after the planner was cancelled | Task and checkout kept; attempt `Cancelled` |
| F22 | Owner future panics | Process group killed on drop; registration released; ends in T7 |
| F23 | Cancel arrives during admission | Before 8.2 step 9: nothing written or counted. Later: `Cancelled`, counted. No process unless already spawned, which is then reaped |
| F24 | Orphaned planner after a crash | Starts refused until the provider lease is free |

## 13. Test cases

Deterministic barriers, acknowledgements and injected errors; no sleeps. A
stand-in `claude` reads stdin to EOF, records its argv and call count, starts a
child in its group, acknowledges that it is running, and exits only when the
test releases it. Every wait in a test is bounded and fails with a message.

Domain (pure):

- D1 `input_digest` changes for each covered field and not for review mode,
  reviewer model, position or status.
- D2 Document validation: each limit, unknown field, control characters.
- D3 State derivation for every row of both state tables.
- D4 Retention keeps 5 bodies and the approved one; counts and executed entries
  survive.
- D5 A board without planning fields loads and re-serializes without them.

Transitions and gate:

- A1 T0 to T15, each with its allowed source states and one refused source.
- A2 G1 to G4: one test per condition, asserting nothing else changed.
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

- N1 argv carries read-only tools, `--restricted`, strict MCP, the schema,
  turn and budget limits, the planner model; never a bypass flag.
- N2 F1 to F12 and F18, F22, F24, asserting attempt count and spawn count.
- N3 Startup with a `Running` attempt: `Interrupted`, zero spawns, count kept;
  with an injected write failure, retried by the next Generate.
- N4 Cost parsed from a result event; absent or malformed gives `Unknown`.
- N5 Limit and grant: the eleventh call is refused until granted.

### 13.1 Lifecycle race tests

Test seams pause the code at: admission after registration (S1) and after the T1
write, before the leases are released (S2); the owner before spawn (S3), before
the seal (S4), after the seal and before the write (S5), and after the write and
before deregistration (S6). A seam blocks on a channel the test controls.

Unless a row says otherwise, each test ends by asserting: the stand-in's process
group is gone; the provider lease is free; no owner is registered; the permit,
slot and lifecycle lease are free; and the exact attempt outcome and revision
count.

| # | Forced order | Required result |
|---|---|---|
| R1 | Planner running; test inspects the lease | The lifecycle and admission leases are free |
| R2 | Test holds the lifecycle lease, then cancels | Reaped and recorded `Cancelled` while the lease is still held |
| R3 | Cancel at S1 | Zero spawns, no attempt |
| R4 | Cancel at S3 | Zero spawns, `Cancelled`, counted |
| R5 | Provider has answered; cancel at S4 | `Cancelled`, no revision |
| R6 | Cancel at S5, and again at S6 | `Succeeded` stands; reply says finished |
| R7 | Two cancels with different reasons | One kill; first reason recorded; same outcome to both |
| R8 | Cancel with a previous `request_id` | The current planner is untouched |
| R9 | Delete while running | Planner reaped while delete holds the lease; Task removed; no later write recreates it |
| R10 | Delete while paused at S5 | Delete waits for `done`; Task removed once; no deadlock |
| R11 | Delete with an unfinished integration | Refused; planner still running |
| R12 | Delete whose checkout removal is refused | Task kept, `Cancelled` |
| R13 | Generate paused at S2, then delete | Delete takes the lease after release and ends the new planner |
| R14 | Delete holds the lease, then Generate | Generate answers busy or "no such Task"; zero spawns |
| R15 | Shutdown while running, test holding the lifecycle lease | Reaped and shutdown completes; attempt `Cancelled` by shutdown; Task status unchanged |
| R16 | Shutdown while Generate is paused at S1, then at S2 | Zero spawns, or spawned and reaped; shutdown returns only with no owner |
| R17 | Generate after the admission fence closed | Refused, nothing written |
| R18 | Shutdown with a process that is not reaped (injected) | Error, ownership retained, retry succeeds after release |
| R19 | Cancel with the same injected hold | "Still stopping"; owner stays; Generate, delete and move refused |
| R20 | Cancel, delete and shutdown together, each pair in both orders | One kill, one outcome write at most, all return |
| R21 | Edit paused before and after the outcome write | `Stale` in both orders; the edit and the revision both survive |
| R22 | Status move against completion at S4 and at S5 | The move applies; outcome is the sealed one |
| R23 | Project removal against a running planner, and Generate against a removal holding the leases | Removal refused with the planner running; Generate refused |
| R24 | Run start attempted while a planner is registered | No Worker launch |
| R25 | Outcome write fails; then Cancel, then Approve of an older matching revision; separately, restart | Owner ends; Cancel says not running; T7 once; only the older revision can be approved |

R2, R9, R10 and R15 guard invariants 7 and 8. Each needs the negative control of
concurrency-standard.md: make the owner hold or take the lifecycle lease, see
the test fail by its bound, restore, and record both results.

Desktop acceptance:

- U1 Generate, request revision, edit, approve, run.
- U2 Edit the Task after approval: shown stale, start refused with the reason.
- U3 Quit during generation, restart: the attempt shows how it ended (cancelled
  by shutdown, or finished within a grace period), no new call. Kill the app
  instead: interrupted, cost unknown.
- U4 Cancel, and delete a Task, during generation: no provider process remains.

Not provable with a stand-in, to be reported as `INCONCLUSIVE` until probed on a
real CLI: read-only enforcement, schema with `--restricted` for this prompt,
cost field names, budget enforcement, and whether the CLI leaves descendants
outside its process group.

## 14. Implementation Tasks

| ID | Task | Depends on | Done when |
|---|---|---|---|
| P0 | Make the UI truthful: label the toggle and badge as not yet enforced, or hide them. Change no Task data | none | No text claims planning happens |
| P1 | Domain: `PlanDocument`, `PlanningRecord`, digests, validation, state derivation, retention, Activity kinds, compatibility note | none | D1 to D5 |
| P2 | Capture run cost and usage from the Claude result event; probe the real CLI and record the field names | none | N4 |
| P3 | `planner_model` field and planner runner: prompt, read-only launch, parsing, limits | P1, P2 | N1, F3 to F6 |
| P4 | Planner owner: registry, seal, cancel, owner counts and exclusivity, status moves, delete, shutdown and quiesce, against a stand-in provider. No user entry point | P1 | R2, R5 to R12, R15, R18 to R20, R22, R24 |
| P5 | Orchestration: admission, T1 to T7, checkout acquisition, startup recovery, wiring P3 into P4 | P3, P4 | N2, N3, N5, all of R1 to R25 |
| P6 | Review and approval commands: T0, T8 to T11, T15, G2 | P5 | their A1, A2, A7 rows |
| P7 | Gate: G3, early refusals, T12, T13, prompt block, Coordinator refusals, existing flagged Tasks. Behind a build-time off switch | P6 | A3 to A6, A8, F14, F19, F20 |
| P8 | Plan panel: review, revision, edit, approval, cancel, live states, limits and cost | P6 | component tests |
| P9 | Enable the gate, restore the label, update product-model.md, release note, acceptance journeys | P0, P7, P8 | U1 to U4, real CLI probes recorded |

P0, P1 and P2 can start in parallel; P3 and P4 can then proceed in parallel. The
gate must not be enabled before P8: without the panel a flagged Task could never
run. P4 to P7 each need the review record from async-safety.md. Decisions in
section 15 must be taken before the Task that implements them starts.

## 15. Open decisions for Human evaluation

None of these is decided. Each recommendation is a proposal.

| # | Question | Alternatives | Recommendation | Needed before |
|---|---|---|---|---|
| O1 | Limits in section 10 | Other values; configurable | Accept as defaults; configure later | P3 |
| O2 | Do Tasks already flagged `planning_mode` become gated? | Gate them; clear the flag; grandfather them | Gate them, announced; the Human can turn the flag off | P7 |
| O3 | Does Human Review "request changes" need a new plan? | New plan; keep the approval path open for feedback | New plan; revisit if the cost is high in use | P7 |
| O4 | May planning create the Task Checkout? | Yes; plan in the primary checkout | Yes; the primary checkout may be the wrong base | P5 |
| O5 | One-time downgrade break from new board fields | Accept; store plans outside the board | Accept with a release note | P1 |
| O6 | Does a planner use an agent capacity slot and obey the start guard? | Shared capacity; separate planner-only limit | Both: one shared slot and at most one planner per instance | P4 |
| O7 | A non-terminal column move or Stop during planning | Cancel, as for other owners; refuse until cancelled | Cancel, for consistency; the UI says so | P4 |
| O8 | Project removal or deletion during planning | Refuse; cancel and proceed | Refuse, as for coding runs. Task deletion cancels (section 8.5); whether it asks for confirmation is also open | P4 |
| O9 | Turning planning off during generation | Refuse; cancel then turn off | Refuse | P6 |
| O10 | Shutdown with a grace period | Let the planner finish within it; cancel at once | Let it finish, as for other owners | P4 |
| O11 | Planning on Windows, without a provider lease | Allow, as coding runs are; refuse | Allow; state the limit | P5 |

## 16. Not verified

No code was run for this design, and no test in section 13 exists yet. The
ownership design follows the executor's existing admission, stop, delete and
shutdown code, which was read, not exercised. Claude Code's cost and usage field
names and its budget behavior were not checked. Whether a read-only run loads
the project `CLAUDE.md` was not checked. The current "request changes"
transition and the dirty fingerprint's coverage were not traced; P5 and P7 must
confirm them before relying on sections 3 and 6. That every other consumer of
the executor's owner maps (live-run reporting, checkout removal and reclaim,
storage accounting) picks up a new owner kind was not checked; P4 must enumerate
them. Cross-process exclusion between two instances on one storage is not
provided by anything here. Verdict for the async contract: `INCONCLUSIVE` until
P4 and P5 pass section 13.
