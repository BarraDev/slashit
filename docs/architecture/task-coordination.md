# Task-linked coordination

Open **Task coordination** in the Task Drawer. The Task must be idle,
unapproved and have an existing Task Checkout. Supply a goal, inspect the
Coordinator's explanation and exact proposed Worker request, then approve,
edit and approve, or reject. Rejection launches no Worker. Approval launches
one Worker in the recorded checkout, followed by a fresh read-only Coordinator
step. The human sees the Worker result and Coordinator recommendation, and
chooses Continue, Redirect, Reject result or Approve / finish round.
Continue and Redirect permit a new human goal and proposal; every new Worker
still needs approval. Finishing a round does not approve or deliver the Task.

## Ownership and durable state

`domain/conversation.rs` models one Conversation per Task in this slice.
Conversation and Participant ids are independent UUIDs. Participants have
fixed logical roles Human, Coordinator and Worker. Ordered rounds retain
human goals, proposed and approved request text, rejection, edits, results,
final recommendations and decisions. Each Run attempt records its logical
Participant, stage, exact projection, timestamps and outcome. None contains
provider session identity.

`AppPaths::conversation` resolves an owner-private JSON file under the normal
data directory, `conversations/<task-id>.json`. It never lives in Task or the
checkout. Writes use the existing private atomic replacement helper. Commands
serialize short read/modify/write sections, check the client's expected
revision, and persist before publishing or starting a process. A failed write
does not update a frontend cache or trigger an execution.

## Execution and context

The existing TaskExecutor helper-owner map and admission gate hold a
coordination lease across process startup, cancellation, result persistence,
and the optional Worker-to-summary transition. No second live registry exists.
Normal Task execution, AI review, PR helpers and coordination cannot own the
same Task concurrently. The ordinary Stop command kills and reaps the current
agent; coordination stop preserves TaskStatus. Task completion/deletion uses
the same ownership-ending mechanism before touching its checkout.

Coordinator Runs use ClaudeRunner's restricted read-only tool policy. Worker
Runs use its coding policy, disable MCP, and use only the existing Task
Checkout as the working directory. The checkout's branch and Git operation
state are checked before starting. Successful Worker changes use the existing
checkout commit helper. Task lifecycle and delivery commands remain separate;
already human-approved or delivered Tasks refuse coordination. Previous Task
QA records are historical and are not a review of the new delegated change.

The initial Coordinator projection contains bounded Task title/description,
human goal and the previous round's human decision. The terminal provider
result must be strict JSON with exactly `request` and `explanation`, both
nonempty and at most 16000 UTF-8 bytes. Streaming assistant prose, fenced JSON,
unknown fields and duplicate keys do not satisfy this contract.

The Worker projection contains only bounded Task title/description and the
authoritative approved request. It has no Coordinator explanation, transcript,
raw provider output, Run history or provider session. The summary projection
contains the human goal, approved request, whether it was edited and the
bounded Worker result. Both projections are deterministic and saved for
inspection. Prompt construction selects fields explicitly; it never serializes
a whole Conversation for an agent. Each provider process starts fresh.

## Recovery

Loading a Conversation never executes anything. Proposed and Rejected stay
as recorded. Approved request text remains authoritative. A saved Started Run
is evidence of an attempt, never evidence of a live process: only executor
handles establish liveness. When no Run is live, the UI offers explicit
recovery for an unfinished step, warning that interrupted tool effects may
already exist in the checkout. The human must inspect it before continuing.

Explicit recovery marks unfinished attempts Interrupted and starts a new Run
from the reconstructed projection. A returned Worker result proceeds directly
to a fresh Coordinator summary; it never reruns the Worker. Expected revisions
and task exclusivity reject stale or overlapping approvals. This is not
exactly-once execution across crashes: a process may have changed files before
its result reached disk. Recovery is an explicit new attempt, not silent replay.

## Boundaries and proof

The slice has no provider resume, ACP work, generic provider router, parallel
Workers, autonomous loops, multi-Task conversations or delivery redesign.
The normal Task path creates no Conversation. Coordination tests cover strict
parsing, persistence, negative context isolation, real fake-provider processes,
edited approval, stop, duplicate admission, fresh recovery and Task invariance.
The desktop journey drives the rendered controls, uses the existing Task
Checkout, inspects provider invocation prompts, and reopens the application
to compare persisted state. CI substitutes the fake executable for Claude.
