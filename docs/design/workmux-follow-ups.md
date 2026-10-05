# Design notes: follow-ups from the Workmux review

Status: proposed. Nothing here is built. Each note is one of the six ideas
from issue 73; implementation issues are filed only after a note is accepted.
The audit reflects `main` at the time of writing. Vocabulary follows
[product-model.md](../architecture/product-model.md).

## Verdicts

| Idea | Verdict |
|---|---|
| Agent attention states | KEEP, narrowed to a derived "Working" chip; no new status model |
| Cross-Project dashboard | DROPPED as a page; MERGE into the existing attention summary |
| Resume agent sessions | KEEP, split in three; only task-level resume is in scope |
| Coordinator dispatch | KEEP, narrowed to an authorization audit of existing verbs |
| Stale-agent reaper | DROPPED as a reaper; detect-and-surface folded into task resume work |
| Per-Project setup hooks | KEEP as design, deferred |

## 1. Agent attention states

Problem. A card does not say whether an agent is working or the user is
needed. Workmux distinguishes working, waiting and done.

What exists. `slashit-attention` derives `AttentionReason` (Failed, Review,
PrNotCreated) from persisted status plus Human Review facts and is shared by
backend and frontend; the card chip, the header walk and the rail summary
already use it. `TaskPhase` (Idle, Planning, Coding, QaReview, QaFixing,
Complete, Failed) and `TaskStatus` already say "an agent is running".
"Waiting for input" has no source today: runs are non-interactive
(`claude -p` through the runner, prompt on stdin), so an agent cannot ask a
question and block. "Needs review" is exactly `AttentionReason::Review`.

Decision. Do not add a second status model. Two of the three requested
states already exist (needs review, failed). The remaining useful one is
"working", which is derivable from status InProgress/AiReview plus phase and
needs no persistence.

Behavior. A card in InProgress or AiReview shows a quiet "Working" label with
the phase name. Needs-you chips are unchanged and take precedence.

Model boundary. Presentation only, computed in the frontend from fields the
task record already carries. If a CLI or tray later needs it, it moves into
`slashit-attention` as a separate function, not as a new `AttentionReason`
(attention means "the user is needed"; working means the opposite).

Persistence. None. Failure and recovery: after a crash the status is what the
record says; there is no stored "working" to go stale.

Surface. Card and drawer header text. Security: none.

Out of scope. "Waiting for input" (requires interactive agent sessions and a
provider-specific signal; revisit only if interactive runs ship), terminal
output scraping, notifications.

Smallest first version. Card label from status and phase.

Acceptance. A running task shows Working with its phase; a failed or review
task shows the existing chip and not Working; no new field is written to the
task file; `slashit-attention` tests are unchanged.

## 2. Cross-Project dashboard: DROPPED as a separate view

Rationale. The project rail already reads `get_attention_summary` across
Projects, and `ListTasks` / `ListTerminals` / `QueueStatus` already give a
global running view through the CLI. A Workspace-level page would add a third
place to read the same facts and invite cross-Project task semantics (moving,
ordering, ownership) that the product model deliberately avoids: a Task
belongs to exactly one Project. Membership lives on the Project, so a
Workspace aggregate is only a filter.

Disposition. If wanted later, add a Workspace filter (Projects whose scope
names that Workspace) to the existing attention summary and rail; read-only,
no new page, no new store. File only when a real multi-Project user asks.

## 3. Resume agent sessions after a crash

Three different things, kept apart.

(a) Resuming a SlashIt task. Today startup hydration requeues tasks left
InProgress, so the task restarts from the queue with a fresh run in its
existing Task Checkout. This already works as "resume the task" at the
product level; it just discards the agent's conversation.

(b) Reconnecting a terminal process. PTY sessions are children of the app
process and die with it. Reattaching would need a surviving supervisor (the
daemon could in principle own them). Out of scope: large, new architecture,
and the agent runs are not PTY based.

(c) Resuming a provider session. Verified locally with `claude --help`: the
CLI supports `--resume <session-id>`, `--session-id <uuid>`, `--continue`,
`--fork-session`, and `--no-session-persistence`. The runner already builds
`--session-id` and `--resume` from `ClaudeRunConfig`, but every production
caller passes `resume_session: None`, and the session id is generated per run
(a fresh UUID) and not stored on the task. What I did not verify: whether a
session started with `-p` and killed mid-turn can be resumed cleanly, and how
long Claude retains session files. SlashIt must not promise resume; it can
only try it and fall back.

Decision. Scope to (c) as best-effort inside (a): persist the provider
session id on the run record, and on requeue-after-crash pass `--resume`
when the id is known; if Claude rejects it, retry once as a fresh run and
record in the activity timeline which happened. Provider specifics stay in
`agents/`; the task record stores an opaque optional string plus provider
name.

Persistence. One optional field on the task's run record; absence means
fresh run. Failure: resume failure is never fatal and never loops; one retry
then fresh. Surface: a timeline entry "resumed session" or "started fresh
(resume unavailable)". Security: the session id is not a secret but is not
shown in public text; the checkout path is unchanged.

Out of scope. Terminal reattach, cloud or `--bg` sessions, other providers,
resuming human-driven conversations.

Smallest first version. Persist id, resume once on crash requeue, fall back.

Acceptance. A fake agent that records its argv sees `--resume <id>` after a
simulated crash; a fake that rejects it is rerun fresh exactly once; the
timeline shows which path ran; no id is stored for tasks that never ran.

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

Out of scope. Multi-agent orchestration, task dependencies, results API,
remote dispatch, per-agent credentials.

Smallest first version. Provenance field and a `--json` output audit so an
orchestrator can read task ids and status reliably.

Acceptance. Tasks made by the CLI are visibly marked; protocol test still
forces every new verb to be classified; no new mutating verb is added.

## 5. Stale-agent reaper: DROPPED as a reaper

Rationale. The three parts differ in risk. Detect and surface already exist
for their real cases: orphaned Task Checkouts and branches have a read-only
scan with per-item reclaim (`commands/orphans.rs`), and crash recovery
requeues InProgress tasks. For agent processes: runs are children of the
SlashIt process, so when the app dies the pipes close and `claude -p` exits;
a surviving agent with no owner is unobserved in practice. Killing a process
by pattern is destructive and needs ownership proof SlashIt does not
currently record (no pid persisted per task).

Disposition. Do not build a reaper. If an orphan agent is ever observed,
first record pid and start time on the run record (this piggybacks on the
session-resume field in note 3), then surface "possible leftover agent" in
the existing orphan scan, with kill only on explicit per-item request after
verifying pid, start time and checkout path all match. Revisit with
evidence.

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

| Idea | Value | Size | Risk | Overlap | Order |
|---|---|---|---|---|---|
| Resume (c within a) | high | small-medium | medium, provider drift | queue recovery | 1 |
| Attention Working label | medium | small | low | attention crate | 2 |
| Dispatch provenance | medium | small | low | ipc-security | 3 |
| Setup hooks | medium | medium | high, code execution | none | deferred |

Recommended near term: resume (best-effort, one retry) and the Working label.

## Self-review

- Duplication: the dashboard and the attention expansion were the main
  duplicates and were dropped or narrowed.
- Provider assumptions: resume is isolated in `agents/`, optional and
  falls back; `--resume` behavior after a mid-turn kill is unverified.
- Destructive process management: no reaper is proposed.
- Dispatch and hooks: both are code-execution surfaces; the notes keep the
  OS boundary, deny repo-supplied hooks and never interpolate task text.
- Unverified: whether hydration requeue reuses the same checkout in every
  case, and whether any task field already stores a provider session id
  (a search found none).
