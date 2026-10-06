# Design notes: agent supervision, resume, dispatch and related ideas

Status: proposed. Nothing here is built. Each note is one of the six ideas
collected in issue [#73](https://github.com/BarraDev/slashit/issues/73), from a
review of a comparable tool used only as a reference; implementation issues are
filed only after a note is accepted.
The audit reflects `main` at the time of writing. Vocabulary follows
[product-model.md](../architecture/product-model.md).

## Verdicts

| Idea | Verdict |
|---|---|
| Agent attention states | KEEP, folded into managed run supervision; no new Task status model |
| Cross-Project dashboard | DROPPED as a page; possible read-only Workspace filter later |
| Resume agent sessions | KEEP, best-effort, provider session state follows Participant/execution lineage |
| Coordinator dispatch | KEEP as a deferred note, narrow; multi-agent orchestration DEFERRED, not rejected |
| Stale-agent reaper | KEEP managed execution supervision; DROP automatic reaping and scanning |
| Per-Project setup hooks | KEEP as design, deferred |

Direction context:
[agent-runtime-and-conversations.md](agent-runtime-and-conversations.md).
Ownership principle: SlashIt owns conversation and execution metadata, not the
user's repository or the provider's configuration.

## 1. Agent attention states and managed run supervision

Problem. A card does not say whether an agent is actually running or the
user is needed.

Two separate questions, never merged. Attention asks "does the person need
to act?" and is already answered by `slashit-attention` (`AttentionReason`:
Failed, Review, PrNotCreated), derived from persisted facts. Run state asks
"what is the execution SlashIt owns doing right now?" and is a different
model. Needs review is already Attention. Working is not an Attention reason.

Working is not derivable from `TaskStatus` or `TaskPhase`. A persisted status
does not prove a live process (after a crash an InProgress or AiReview record
has no process until hydration resets it to Queue), and a queued task may be
waiting on capacity or admission. Working must project live supervision:
SlashIt holds the child process or protocol handle it started, so it can say
Starting, Working, Stopping, Finished or Failed from that handle.

Run state is held in memory by the supervisor and projected to the UI. It is
not a persisted Task field and creates no new Task status model. Nothing new
is persisted on the Task; what is persisted is only what already is (timeline
entries, outcome). Provider session ids (note 3) have no persisted home yet
and are not a Task field.

Three status vocabularies already overlap: `domain::AgentStatus`
(Starting, Running, Stopping, Stopped, Failed), `AgentSlotStatus` (Idle,
Working, WaitingForInput, Completed, Failed), and the executor's
`AgentEvent`, with frontend mirrors of the agent status and event types in
`src/models/agent.rs`. The supervision work must reconcile these rather than
add a fourth.

WaitingForInput is a supported-future run state with no current producer; it
may only come from a reliable structured provider or protocol signal and is
never inferred from silence or output pauses. Audit of main: the only
`AgentSlotStatus::WaitingForInput` is a declared variant that nothing sets
(`queue/workflow.rs` sets only Idle). The Claude runner is non-interactive
(`claude -p`, prompt on stdin) and parses `system`, `assistant`, `tool_use`
and `result` events, none of which means "waiting for the human". ACP
(`src-tauri/src/acp`) has a free-form `status` string notification with no
defined vocabulary and no permission or input request. Nothing consumes the
notifications at all: `subscribe_notifications` has no caller outside
`src-tauri/src/acp`, and only the child's stderr is logged. `session.rs` and
`stream.rs` are not compiled (`acp/mod.rs` declares only `protocol` and
`client`). So no provider currently produces a trustworthy signal. Out of
scope: any Conversation model, external process scanning, repo or provider
hooks, notifications.

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

Disposition. A read-only Workspace projection or filter remains possible
later: a filter (Projects whose scope names that Workspace) over the existing
attention summary and rail, with no new page and no new store. File only when
a real multi-Project user asks.

## 3. Resume provider sessions after a crash

Three things, kept apart. (a) Resuming a SlashIt task: startup hydration
already resets tasks left InProgress or AiReview to Queue (recording an
Interrupted activity), so they restart as fresh runs in their Task Checkout.
(b) Reconnecting a terminal process: out of scope; PTYs die with the app and
agent runs are not PTY based. (c) Resuming a provider session: best-effort.

Provider session state is opaque provider-specific state associated with the
Participant and its execution lineage, not with the Task alone. A Run may
create or resume that session. SlashIt records the id the provider reports
and never invents or interprets it as a provider session. Current code:
`ClaudeRunConfig` has `session_id` (`--session-id`) and `resume_session`
(`--resume`); the runner captures the id from the `system` init event and the
`result` event; the executor generates its own `Uuid::new_v4()` and passes it as
`--session-id`, and no production caller sets `resume_session`. That
generated value is only a request to the provider; it must not be treated
as the provider-reported id, and a resume must use the id the provider
reported, not the one SlashIt asked for. No id is persisted today. The
claude CLI help lists `--resume`, `--session-id`, `--continue`,
`--fork-session` and `--no-session-persistence`.

Behavior: persist the provider-reported id when available. No Run or attempt
entity exists today, so this id has no persisted home yet; the resume work
must decide where it lives (a run or attempt record) and it is not a Task
field; on interrupted-run recovery try `--resume` once; if it fails, run
fresh; never loop; write what happened to the activity timeline.

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
already classifies the agent-spawning verbs (`spawns_agent()`): an
OS-verified local peer may use them, a token-only TCP peer may not, and
mutations are audit logged. So an orchestrating agent
running as the same OS user can already dispatch. The idea is therefore not
"build dispatch" but "decide what is safe to give an agent".

`spawns_agent()` (`crates/slashit-ipc/src/protocol.rs`) covers CreateTask,
MoveTask, EnqueueTask and also EditTask (editing a queued or running task
rewrites the prompt a later run uses); DeleteTask is not in that set.
`docs/architecture/ipc-security.md` lists the same four verbs.

Gaps. No caller identity beyond "the owner"; any caller can edit or delete
tasks in any Project, not only tasks it created; created task text is
attacker-influenced input that becomes another agent's prompt.

Decision. KEEP as a deferred note: a small authorization design for a local
coordinator; orchestration itself is deferred, see below.

Behavior. Tasks created over the channel record `created_via: cli` (plus an
optional caller label from `--source`, informational only). A restricted mode,
off by default, limits a caller to creating tasks and queueing tasks it
created in one named Project. For this first step there are no dependency
graphs, result collection or fan-out: the coordinator polls `slashit tasks`.

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

Smallest first version (deferred; kept as a note, not scheduled). Provenance
field and a `--json` output audit so an orchestrator can read task ids and
status reliably.

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
that owns processes is outside the scope of these notes.

## 6. SlashIt-owned per-Project setup hooks

Problem. A new Task Checkout often needs setup (dependency install, env files)
before an agent is useful. (An external git-worktree tool had offered such
hooks.)

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
3. Dispatch provenance: a kept but deferred note; no issue now.
4. Setup hooks: deferred, code-execution surface.

Only items 1 and 2 are proposed as near-term implementation issues.

## Self-review

- Duplication: the dashboard page was dropped because the rail and CLI
  already show the facts; the Working label no longer has its own item and
  is part of run supervision, kept separate from `AttentionReason`.
- Working is not derived from `TaskStatus` or `TaskPhase`; it projects live
  supervision, and nothing new is persisted on the Task.
- Provider assumptions: session state is associated with the Participant and
  its execution lineage, the id must be the provider-reported one, and
  resume is best-effort with one fresh fallback. Resume behavior after a
  mid-turn kill is unverified and is a spike question.
- Process management: supervision covers only runs SlashIt started. There is
  no scan, no kill path and no assumption about processes after a crash;
  any future leftover surfacing needs persisted identity and strong
  ownership proof first.
- Dispatch and hooks are code-execution surfaces. The notes keep the OS
  peer boundary, deny repository-supplied hooks and never interpolate task
  text. Orchestration is deferred, not rejected, and the provenance design
  must not close that path.
- Unverified: whether hydration requeue always reuses the same Task
  Checkout; all spike questions in note 3; ACP notification semantics beyond
  the free-form status string.
