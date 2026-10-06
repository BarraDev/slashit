# Agent runtime and conversations

Status: architectural direction, not an implementation plan. No current
feature commitment. Nothing here is built, and the follow-up-ideas issue
[#73](https://github.com/BarraDev/slashit/issues/73) does not implement
any of it. The historical record of the six decisions is
[workmux-follow-ups.md](workmux-follow-ups.md). The purpose of this document
is to keep today's bounded decisions from closing doors. Vocabulary follows
[product-model.md](../architecture/product-model.md).

## Ownership principle

> SlashIt owns conversation and execution metadata, not the user's
> repository or the provider's configuration.

Consequently supervision must never require Git hooks, edits to CLAUDE.md or
AGENTS.md, repository-controlled configuration, global provider
configuration, shell or profile injection, hidden heartbeat files in the
repository, or scanning arbitrary external processes by executable name.
SlashIt may supervise executions it started, or ones explicitly adopted
through a future ownership protocol. Everything else is outside its control
and it does not pretend otherwise.

## Concepts (direction)

- **Conversation.** A persistent coordination context owned by SlashIt:
  who said what to whom, and the human decisions in between.
- **Participant.** A logical agent or human role in a Conversation (for
  example "implementer", "reviewer", "the user").
- **Run.** One concrete execution of a Participant: a process or protocol
  connection SlashIt started, with a start and an end.
- **Provider adapter.** The boundary that starts and observes Runs for one
  provider (Claude Code today, an ACP-backed agent, others later).
- **Provider session.** Opaque provider-specific state associated with a
  Participant and its execution lineage. A Run may create or resume it. The
  provider reports its identity; SlashIt records that identity and does not
  invent or interpret it. It is never a universal identity for a Task.
- **Context projection.** The bounded subset of Conversation context SlashIt
  sends a Participant. Agents do not receive the full history by default.
- **Task.** Work, delivery and Task Checkout ownership. Unchanged and kept
  separate from the above.

## Direction

- One Task may eventually involve several Participants and Runs, each
  possibly with its own provider session.
- Communication between Participants that SlashIt models is mediated and
  recorded by SlashIt; their modeled exchanges are context-projected by
  SlashIt, and they do not communicate out of band.
- Provider-internal workers or subagents that SlashIt does not model as
  Participants remain opaque provider behavior. If SlashIt later adopts one
  as a Participant, the normal ownership, supervision and conversation rules
  apply.
- SlashIt projects context into each Participant rather than sharing history.
- A human stays a first-class Participant: approvals, answers and review
  decisions belong in the model.
- Conversation and orchestration are deferred future architecture. They are
  not rejected and not scheduled.

## Supervision boundaries

- Managed supervision covers only executions SlashIt started or explicitly
  adopted. There is no scanning of arbitrary external processes.
- Monitoring uses no Git or project hooks and no repository or provider
  configuration injection.
- A persisted `TaskStatus` alone never proves a live run. "Working" must
  project actual live-run supervision.
- "Waiting for input" requires a trustworthy structured provider or protocol
  signal. Silence or an idle output stream never implies it.

## Managed run supervision today

The executor's handle maps (`running_handles` for task executions,
`reviewing_handles` for AI review and fix flows, and the helper entries for
task-associated PR-review/fix invocations) are the in-memory registry of Runs
SlashIt started. `AgentStatus` is the run vocabulary: `Starting`
(registered, process not yet started), `Running`, `Stopping` (a stop is being
carried out), and the ended `Stopped` and `Failed`. `get_task_run` and
`get_live_runs` read the registry, and `AgentEvent::RunState` announces each
change, with the ended states sent last. PR side-effect reservations share an
executor map for task-exclusivity but are not provider runs and are excluded
from live-run and agent-count projections. Nothing about a run is persisted:
after a restart there is no run until SlashIt starts one. `AgentSlotStatus`
and the workflow `AgentSlot`s are static templates that no execution updates,
and the Tauri commands in `commands/agent.rs` that drive `AcpClient` are an
older, parallel path that the board does not use. `AgentPanel` has no rendered
call site in the current product, so ACP executions are not task-card or board
Working state; they remain a separate adapter debt rather than being merged
into `TaskExecutor` here. The tray, quit guard and IPC status helper still
include their active execution records in the union with executor ownership,
so the safety surfaces do not assume the executor is the only provider owner.
Neither is a second source of truth for a TaskExecutor run, and ACP startup
failures are terminalized rather than left as active records. `WaitingForInput`
stays out of `AgentStatus` until a provider or protocol signal exists that can
set it.

## Resuming an interrupted coding run

Provider-session resume covers normal coding Runs of Claude Code only. AI review and fix runs, PR helpers, the legacy ACP path, other
providers, terminal reattach and process discovery are not covered.

**What is kept.** While a coding Run is in flight, the Task carries one
`RunRecovery` for it: the run number, the Task Checkout the Run
worked in (path, branch, starting commit), the session id the provider
reported, and whether a resume was already attempted for this interruption.
It lives on the Task only because SlashIt has no durable Run record yet; it
belongs to the Run and its checkout, and the Task never uses it as an
identity. The next Run replaces it, and a Run that ends while SlashIt is
watching (completed, failed, stopped), and any move of the task by a person,
removes it. The
record is deliberately in the task file, so an old file loads with none and
means a normal fresh start. The Run's live ownership stays in memory only
(see [Managed run supervision today](#managed-run-supervision-today)); this
record never says that a process is alive.

**Provider identity is opaque and reported, not requested.** The id is taken
from the provider's own `system`/`init` event and from nothing else. The id
SlashIt asks for with `--session-id` is not evidence that the provider kept
anything, and a failed resume's `result` echoes the id it was asked for. No
code reads the id's format.

**A reported id does not prove a resumable conversation.** On Claude Code
2.1.291, killing the process right around `init` left a session the CLI could
not resume ("No conversation found") in 2 of 2 attempts. Resume is therefore
one best-effort attempt, never a guarantee.

**State.** `Running` while a Run is in flight. A start-up finds a `Running`
record on a task that was in progress and working and turns it into
`Interrupted`; any other `Running` record is dropped. When the next Run
starts in the same checkout, it decides, in the same durable write that
records the Run's start:

- interrupted, same checkout, a reported session, no resume attempted yet:
  resume. The record is written with `resume_attempted` before any process
  for it exists, so a crash during the resume is never retried against the
  same session;
- interrupted, same checkout, but no session or a resume already attempted:
  start a fresh conversation that reconciles with the checkout;
- anything else, including a different checkout or a branch this start just
  created: an ordinary start, which replaces the record.

`resume_attempted` is cleared once a process reports a session of its own for
the Run: a conversation the provider accepted has its own interruption and
its own single resume. A later, genuinely new Run starts with its own record.

**A miss is not a failure.** If the resume ends with the provider having no
such conversation, the Run continues once in a fresh conversation on the same
checkout, holding the same capacity and ownership, and the miss is recorded
on the timeline. The miss is recognized only by the whole observed shape (an
`error_during_execution` result with no turns, no `init` first, naming the
missing session), so any other early failure still fails the task. Ambiguity
means a fresh start, never a retry.

**The checkout is the work.** A resumed conversation keeps what was said, not
whether the tool call it was interrupted in ran, and the CLI does not replay
it (observed: the interrupted call had no result and was not rerun). The call
may have changed the checkout partly, fully or not at all. Both a resumed and
a fresh recovery Run are therefore told, after the normal task prompt, to
inspect the working directory first, reconcile any partial work, keep the
correct work already there, and then continue. Neither is told to discard
anything.

**Timeline.** An attempted resume gets one row, then one outcome: the
session resumed, or the session unavailable and the Run continuing with fresh
context. When nothing can be resumed (no session was recorded, or a resume
was already attempted) only the unavailable row appears. No session ids appear in user-facing text.

**What the evidence is.** Observed on Claude Code 2.1.291 and rechecked on
2.1.292 with `-p --verbose --output-format stream-json` and
`--resume <reported id>`; not a documented contract. Provider retention of
sessions, behavior across CLI versions and whether the working directory
matters for lookup (it did not in the tested runs) are provider-defined and
unverified, so none of them is assumed and a version change alone does not
block a resume.

## ACP

ACP is a provider or protocol adapter boundary. It must not become SlashIt's
domain model. Current support (`src-tauri/src/acp`) is rudimentary: four
requests (initialize, create, send_prompt, stop), two notifications (log and
a free-text status), no cancellation of a running prompt, no session
loading, no recovery, and notifications that nothing consumes
(`subscribe_notifications` has no caller outside the module; the app logs
only the child's stderr). The `session.rs` and `stream.rs` files are not even
compiled, because `acp/mod.rs` declares only `protocol` and `client`. Making
it a dependable adapter needs lifecycle, notification, cancellation, session and
recovery work. That is deliberately not tracked as one large issue now.

## What this means for the six follow-up ideas

Managed run supervision is the shared foundation for attention-adjacent
status, session resume and any future leftover-process surfacing; see
[workmux-follow-ups.md](workmux-follow-ups.md). Provenance on dispatched
tasks must not preclude Conversations later.
