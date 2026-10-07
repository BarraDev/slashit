# Agent runtime and Project Conversations

This document describes the implemented Project Conversation and its runtime
boundaries. SlashIt owns durable conversation state and the semantic results of
Runs; provider processes and provider-specific session state remain separate.
Vocabulary follows [product-model.md](../architecture/product-model.md).

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

## Current Project Conversation

Each Project has one primary Conversation with a distinct UUID and a private
SlashIt data file. A per-Project pointer locates that Conversation. Ordered
entries record Human messages, Coordinator replies, proposed actions, human
decisions, Worker starts/results, and run failures. The Workspace contains
Projects but does not own their Conversations. Tasks and Task Checkouts do not
own Conversations.

The Conversation is usable with zero Tasks and no repository checkout. It
survives application restart. Runtime Run ownership is registered with the
existing TaskExecutor under the Conversation UUID; it does not invent a Task id
or use TaskStatus as liveness. Runs are fresh provider processes and do not
depend on provider session continuity. A process restart has no live Run owner.

A Human message is durably written before a Coordinator starts. The Coordinator
receives a bounded projection: Project identity and root, current message,
selected recent conversational messages, and a bounded index of Tasks. It uses
read-only tools. Durable history and the projection sent to a provider are
separate; provider transcripts and arbitrary Project records are not included.

The strict output contract is either `Reply { text }` or
`DelegateToTask { text, target_task_id, request }`. Prose cannot trigger an
action. A proposal is persisted and displayed, but cannot start a Worker until
a Human explicitly approves it. Approval can replace the request; the saved
approved request is the execution authority.

A Worker is admitted only for an existing Task in the Conversation's Project.
The Task must already have a valid Git Task Checkout on its recorded branch,
and the existing Task executor must grant exclusive Task ownership and agent
capacity. Worker context contains the target Task, approved request, and
selected bounded dependencies. It excludes the full Conversation, unrelated
Tasks, and provider transcripts. The Coordinator is read-only, so it does not
share mutation of the Task Checkout.

The Worker result is saved as Conversation-owned evidence. SlashIt then starts
a fresh read-only Coordinator Run with the selected context and that result
marked as untrusted evidence. The Coordinator replies in the same Conversation,
which remains open for ordinary Human messages or another separately approved
action.

After restart no Run is considered live. An interrupted Coordinator can be
retried from saved Conversation state. A Worker that may have partially changed
a checkout is marked interrupted and is not silently replayed. Once a Worker
result is durable, recovery continues with a fresh Coordinator Run rather than
rerunning the Worker. This prevents automatic replay; it does not promise
exactly-once external or repository side effects across crashes.

### Deleting a Project

Deleting a Project removes its Conversation, so the order of writes matters.
Deletion first takes the Project's Conversation lock and refuses while a
Conversation Run is live. It then writes a durable `<project-id>.delete-pending`
record next to the Conversation files, removes the Project from config, deletes
the Conversation document and then its pointer, and finally clears the record.
The pointer is the last thing removed, so a partial cleanup always leaves a
reference from which it can be retried.

At startup each record is resolved against the config as loaded without
salvage and with an explicit `projects` section; a fallback or defaulted config
cannot prove that a Project is gone, so when that load fails every record is
left for a later start. If the Project is still present, removal
never committed and the record is discarded. If it is absent, the record
authorizes the retry, including a strict scan for a Conversation document whose
pointer was never published. Documents are only deleted when they prove both
their own id and their owning Project. Any failure leaves the record in place
and is reported at startup rather than treated as success.

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
from live-run and agent-count projections. Nothing about a live Run is
persisted: after a restart there is no Run until SlashIt starts one. Project Conversation runs are separately registered in the
same executor by Conversation UUID, share its admission capacity, and expose a
stop signal without manufacturing a Task identity. `AgentSlotStatus`
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

## Future scope

The current implementation includes Project Conversations, ordinary replies,
bounded Task visibility, `DelegateToTask`, explicit Human approval, and a
Task-scoped Worker. Task creation, tracker synchronization, Workspace
Conversations, context compaction, multiple Workers, provider-session resume,
and richer provider adapters remain future work. ACP remains an adapter
boundary and is not part of the Project Conversation execution path.
