# Agent runtime and conversations

Status: architectural direction, not an implementation plan. No current
feature commitment. Nothing here is built, and issue 73 does not implement
any of it. Its purpose is to keep today's bounded decisions from closing
doors. Vocabulary follows [product-model.md](../architecture/product-model.md).

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
- **Provider session.** An opaque identity and lifecycle that belongs to a
  Run or Participant at a provider. It is reported by the provider; SlashIt
  records it and does not invent or interpret it. It is not a universal
  singleton of a Task.
- **Context projection.** The bounded subset of Conversation context SlashIt
  sends a Participant. Agents do not receive the full history by default.
- **Task.** Work, delivery and Task Checkout ownership. Unchanged and kept
  separate from the above.

## Direction

- One Task may eventually involve several Participants and Runs, each
  possibly with its own provider session.
- Agent-to-agent communication is mediated and recorded by SlashIt; agents
  do not talk to each other out of band.
- SlashIt projects context into each Participant rather than sharing history.
- A human stays a first-class Participant: approvals, answers and review
  decisions belong in the model.
- Conversation and orchestration are deferred future architecture. They are
  not rejected and not scheduled.

## ACP

ACP is a provider or protocol adapter boundary. It must not become SlashIt's
domain model. Current support (`src-tauri/src/acp`) is rudimentary: four
requests (initialize, create, send_prompt, stop), two notifications (log and
a free-text status), no cancellation of a running prompt, no session
loading, no recovery, and notifications that the app only logs. Making it a
dependable adapter needs lifecycle, notification, cancellation, session and
recovery work. That is deliberately not tracked as one large issue now.

## What this means for the six #73 ideas

Managed run supervision is the shared foundation for attention-adjacent
status, session resume and any future leftover-process surfacing; see
[workmux-follow-ups.md](workmux-follow-ups.md). Provenance on dispatched
tasks must not preclude Conversations later.
