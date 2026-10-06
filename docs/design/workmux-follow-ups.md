# Design notes: follow-ups from the Workmux review

Status: proposed. Nothing here is built. Each note is one of the six ideas
from issue 73; implementation issues are filed only after a note is accepted.
The audit reflects `main` at the time of writing. Vocabulary follows
[product-model.md](../architecture/product-model.md).

## Verdicts

| Idea | Verdict |
|---|---|
| Agent attention states | KEEP, folded into managed run supervision; no new Task status model |
| Cross-Project dashboard | DROPPED as a page; possible read-only Workspace filter later |
| Resume agent sessions | KEEP, best-effort, provider session identity owned by the Run |
| Coordinator dispatch | KEEP, narrow; multi-agent orchestration DEFERRED, not rejected |
| Stale-agent reaper | KEEP managed execution supervision; DROP automatic reaping and scanning |
| Per-Project setup hooks | KEEP as design, deferred |

Direction context: [agent-runtime-and-conversations.md](agent-runtime-and-conversations.md).
Ownership principle: SlashIt owns conversation and execution metadata, not
the user's repository or the provider's configuration.

## 1. Agent attention states and managed run supervision

Problem. A card does not say whether an agent is actually running or the
user is needed.

Two separate questions, never merged. Attention asks "does the person need
to act?" and is already answered by `slashit-attention` (`AttentionReason`:
Failed, Review, PrNotCreated), derived from persisted facts. Run state asks
"what is the execution SlashIt owns doing right now?" and is a different
model. Needs review is already Attention. Working is not an Attention reason.

Working is not derivable from `TaskStatus` or `TaskPhase`. A persisted
status does not prove a live process (after a crash an InProgress record has
no process until hydration requeues it), and a queued task may be waiting on
capacity or admission. Working must project live supervision: SlashIt holds
the child process or protocol handle it started, so it can say Starting,
Working, Stopping, Finished or Failed from that handle.

Run state is held in memory by the supervisor and projected to the UI. It is
not a persisted Task field and creates no new Task status model. What is
persisted is only what already is (timeline entries, outcome) plus the
provider session id from note 3.

WaitingForInput is a supported-future run state with no current producer; it
may only come from a reliable structured provider or protocol signal and is
never inferred from silence or output pauses. See the audit in the revised
report. Out of scope: any Conversation model, external process scanning,
repo or provider hooks, notifications.

Smallest first version: an in-memory registry of runs the executor starts,
fed by the runner's existing structured events and live activity, with a
Working label on the card and drawer driven by it. The Working chip is part
of this work, not a separate item.

Acceptance: a task with a live managed run shows Working; a queued or
crashed-and-not-yet-requeued task does not; after an app restart no run is
shown until one is actually started; Attention chips are unchanged.

## 2. Cross-Project dashboard: DROPPED as a separate view

Rationale. The project rail already reads `get_attention_summary` across
Projects, and `ListTasks` / `ListTerminals` / `QueueStatus` already give a
global running view through the CLI. A Workspace-level page would add a third
place to read the same facts and invite cross-Project task semantics (moving,
ordering, ownership) that the product model deliberately avoids: a Task
belongs to exactly one Project. Membership lives on the Project, so a
Workspace aggregate is only a filter.

Disposition. A read-only Workspace projection or filter remains possible later; add a Workspace filter (Projects whose scope
names that Workspace) to the existing attention summary and rail; read-only,
no new page, no new store. File only when a real multi-Project user asks.

## 3. Resume provider sessions after a crash

Three things, kept apart. (a) Resuming a SlashIt task: startup hydration
already requeues tasks left InProgress as fresh runs in their Task Checkout.
(b) Reconnecting a terminal process: out of scope; PTYs die with the app and
agent runs are not PTY based. (c) Resuming a provider session: best-effort.

Provider session identity belongs to the Run, Participant and provider
adapter, not to the Task. SlashIt records the id the provider reports and
never invents one that it treats as a provider session. Current code:
`ClaudeRunConfig` has `session_id` (`--session-id`) and `resume_session`
(`--resume`); the runner captures the id from the `system` init event and the
`result` event; the executor passes a self-generated UUID as `--session-id`
and no production caller sets `resume_session`. The id is not persisted. The
claude CLI help lists `--resume`, `--session-id`, `--continue`,
`--fork-session` and `--no-session-persistence`.

Behavior: persist the provider-reported id on the run record when available;
on interrupted-run recovery try `--resume` once; if it fails, run fresh;
never loop; write what happened to the activity timeline.

Spike questions to answer before any implementation issue (all unverified):
does `claude -p` reliably report a resumable id; can a session killed
mid-turn be resumed; does resume require the same working directory; how do
clean completion and interruption differ; how long does Claude retain
sessions.

Out of scope: terminal reattach, other providers, Conversations, cloud or
background sessions.

## 4. Coordinator dispatch through the slashit CLI

Audit. The CLI and `IpcRequest` already expose CreateTask, MoveTask,
EditTask, DeleteTask and EnqueueTask. `docs/architecture/ipc-security.md`
already classifies CreateTask, MoveTask(in_progress) and EnqueueTask as
`spawns_agent()` verbs: an OS-verified local peer may use them, a token-only
TCP peer may not, and mutations are audit logged. So an orchestrating agent
running as the same OS user can already dispatch. The idea is therefore not
"build dispatch" but "decide what is safe to give an agent".

Gaps. No caller identity beyond "the owner"; an agent can edit or delete any
task in any Project, not only tasks it created; created task text is
attacker-influenced input that becomes another agent's prompt.

Decision. KEEP as a small authorization design, not orchestration.

Behavior. Tasks created over the channel record `created_via: cli` (plus an
optional caller label from `--source`, informational only). A restricted
mode, off by default, limits a caller to creating tasks and queueing tasks it
created in one named Project. No dependency graphs, no result collection, no
fan-out: the orchestrator polls `slashit tasks`.

Ownership. The Project owns the tasks; the caller label is provenance, not a
permission principal. Persistence: the provenance field on the task.
Failure: refused verbs return a typed error. Security: the label is
untrusted; the real boundary remains OS peer identity; never relax the TCP
denial of `spawns_agent()` verbs; document that a dispatching agent can run
code as the user.

Out of scope for now: multi-agent Conversation and orchestration, which is
DEFERRED future architecture and not rejected (see the direction doc).
Provenance fields and any restricted mode must not close that path: a caller
label is provenance, and a future Participant model may replace it. Also out
of scope: task dependencies, results API, remote dispatch, per-agent
credentials.

Smallest first version. Provenance field and a `--json` output audit so an
orchestrator can read task ids and status reliably.

Acceptance. Tasks made by the CLI are visibly marked; protocol test still
forces every new verb to be classified; no new mutating verb is added.

## 5. Stale-agent reaper: KEEP supervision, DROP reaping

Managed execution supervision stays (note 1). Automatic process reaping and
scanning are dropped. While SlashIt is alive it observes a run through the
real process or protocol handle it holds. After an abrupt crash SlashIt must
not assume the provider process died or survived. It never scans for or
kills processes by executable name. A future recovery step may persist
process identity (pid and start time) to surface a possible leftover agent;
any destructive action would need strong ownership proof. A future daemon
that owns processes is not part of issue 73.

## 6. SlashIt-owned per-Project setup hooks

Problem. A new Task Checkout often needs setup (dependency install, env
files) before an agent is useful. Worktrunk hooks would have covered this.

Decision. KEEP as a deferred design; no code until a user shows a concrete
need, because there is no setup mechanism today.

Behavior. A Project-level setting (stored by SlashIt, not in the repository
and not in agent config) holds one command list, run once after a Task
Checkout is created and before the agent starts. Run without a shell string
interpolation of task fields, working directory the checkout, environment
limited to a documented set (`SLASHIT_TASK_ID`, `SLASHIT_PROJECT_ID`,
`SLASHIT_CHECKOUT`) plus the inherited user environment, with a timeout.

Trust. The command is authored in the SlashIt UI by the user and stored in
SlashIt state. A repository file must never supply it (a cloned repo would
otherwise run code on task start). Task text is never interpolated.

Failure. Non-zero exit or timeout marks the task Failed with the hook output
in the timeline; the agent does not start; retry reruns the hook. Never
blocks other Projects.

Persistence. Command list on the Project record. Surface: Project settings
field and a timeline entry. Out of scope: teardown hooks, per-event hooks,
repo-provided hooks, injecting into Claude settings, secrets management.

Smallest first version. One post-checkout command, timeout, failure to
Failed.

Acceptance. The command runs once per new checkout in that checkout; a
failure stops the run and is visible; a repo file cannot define it; task
text does not appear in the command line.

## Prioritization

1. Managed run supervision (includes the Working chip). High value, medium
   size, foundation for the rest.
2. Best-effort provider session resume. High value, small to medium,
   provider-drift risk; may depend on 1 and needs the spike answers first.
3. Dispatch provenance: small, no issue now.
4. Setup hooks: deferred, code-execution surface.

Only items 1 and 2 are proposed as near-term implementation issues.
