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
older, parallel path that the board does not use. Neither is a second source
of truth for a run. `WaitingForInput` stays out of `AgentStatus` until a
provider or protocol signal exists that can set it.

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
