# Context management for Coordinator Runs

Status: the first part, "Implemented", describes what the code does. The
second part, "Future direction (not implemented)", is direction only.
Vocabulary follows [product-model.md](../architecture/product-model.md). The
history and storage direction behind this is in
[project-conversation-history.md](project-conversation-history.md).

## Implemented

### Purpose

Every Coordinator provider invocation receives context that SlashIt assembles
from the Project Conversation, the Task store and the Conversation's records.
One pure function, `build_coordinator_context` in
`src-tauri/src/conversation_context.rs`, assembles it, and every invocation
proves that it stays inside an explicit budget. There is no model call, no
retrieval and no storage change: Conversation files are read and written exactly
as before.

### One Builder, two sites

The Builder calls no provider, touches no file, takes no lock and reads no
clock. The same inputs give byte-identical output and an identical
`ContextSnapshot`. Task data, the Task index and the sizes of the fixed parts
arrive as plain inputs.

Two sites call it (`src-tauri/src/commands/conversation.rs`):

- `answer_human_message`: an ordinary turn and Retry. Inputs are the current
  Human message and the Task index.
- `continue_from_worker_result`: the legacy mediation of a saved Worker result.
  Its input is the returned action instead of a message, and it has no Task
  index. History, omission and open-proposal rules are the same.

The Builder reads a `Conversation` for history and open proposals. Everything
else is a plain value.

### Budget

The unit is a logical character: one Unicode scalar (`str::chars().count()`) of
the serialized text SlashIt sends, after JSON escaping. It is not bytes and not
tokens. The default total is 64,000 and the per-read cap is 6,000
(`CONTEXT_BUDGET_CHARS`, `PER_READ_CAP_CHARS`). Both are experimental starting
values, not product constants.

An invocation's measured payload is the system prompt, the output contract text
and prompt envelope, the serialized strict output schema, the serialized
projection, the read envelope and notices, the serialized read results, and every
omission or truncation marker.

For the first invocation the Builder guarantees
`fixed + projection + read reserve <= total`. The read reserve is
`READS_PER_TURN x per-read cap` plus the measured worst-case read envelope,
notice and array syntax (`read_reserve_chars`). Unused reserve is not given back
to history. Because the reserve arithmetic is not trusted alone, `drive` also
measures the exact prompt about to be sent before every invocation, including
the follow-ups after structured reads, and fails the turn before starting an
invocation that would exceed the total.

Measured fixed sizes (test `fixed_invocation_sizes_are_measured_for_both_sites`;
run it with `--nocapture`):

| Site | System prompt | Prompt text (includes the 3,508-character output contract) | Output schema | Fixed | Read reserve | Left for the projection |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Ordinary turn | 1,009 | 3,733 | 3,711 | 8,453 | 18,301 | 37,246 |
| Continuation | 453 | 3,804 | 3,711 | 7,968 | 18,301 | 37,731 |

In the fake-provider test with three reads, the four invocations of a turn
measured 9,080, 9,460, 9,624 and 9,813 characters of the 64,000 budget (small
read results and an almost empty Conversation).

Relating the budget to a provider's context window is an estimate, not a
guarantee. SlashIt has no tokenizer, and the model catalog carries no window
sizes. The configuration test
`the_budget_plus_assumed_unmeasured_overhead_fits_an_assumed_window_estimate`
measures what SlashIt controls (the fixed parts of both sites and the read
reserve, which the budget already contains) and then states what it cannot
measure as explicit assumptions: a smallest window of 200,000 tokens, one token
per logical character, 40,000 tokens for the provider CLI's own system prompt,
tool definitions and any harness-added context, and 16,000 tokens for the
reply. The provider and harness overhead is unknown; the 40,000 figure is a
placeholder, not a measurement. Nothing here claims a token saving or a
provider-window guarantee.

### Required and optional content

Required, never dropped or silently shortened: the system prompt, the output
contract, the output schema, the envelope, the Project identity,
`history_semantics`, the current Human message (or the returned action of a
continuation), structural metadata and the read reserve.

Optional, filled in this order with what remains: open-proposal references, then
Task index entries, then recent history. The maxima of 20 proposals and 40 Task
index entries are not allocations, and history has no fixed entry count: it
takes as many of the newest entries as fit. Under pressure the Builder includes
fewer, keeps the totals accurate and says what was left out. A full proposal
block or Task index that does not fit never fails a turn.

### Ceilings and markers

Every historical text field has a stable ceiling that always applies, so an
entry renders the same whatever other entries exist: message 4,000, request
3,000, explanation 2,000, target title 500, Worker result 3,000, failure
message 1,000, refusal reason 1,000. A field above its ceiling is cut on a scalar
boundary, keeping a head (two thirds) and a tail (one third) with an inline
marker that names the omitted count, `original_chars` and the record id. Markers
count in the budget. `returned_action` fields of a continuation use the same
ceilings; previously they were uncapped.

When the history that survives its ceilings does not fit, only evidence is
shortened further: Worker results and failure messages, oldest first, to 1,000
characters. Human messages and decision entries are never cut below their
ceilings. If it still does not fit, the oldest entries are omitted and the
newest kept.

Each history entry carries `entry_id` and `created_at`. `history_omitted` is
present only when a projectable entry is not shown because of the budget. It
gives the count, the first and last omitted `created_at` and counts by kind.
Included plus omitted entries equal the projectable entries. Entries whose
action record is missing were never projectable and are not counted.

### Open proposals

`open_proposals` is built from the Conversation's `project_actions` whose status
is `Proposed`, not from history position, so a proposal older than the entries
shown in history is present. Each reference has `action_id`, `kind`, the target
Task id where there is one, the status read from the record at build time,
`created_at` and a summary of at most 400 characters. The list is newest first,
at most 20. `open_proposals_total`, `open_proposals_omitted` and, when any are
omitted, `open_proposals_oldest_omitted_created_at` are always accurate, whether
the omission comes from the cap of 20 or from budget pressure.

What the model sees does not change what the Human can decide: the decision path
reads the same records and is untouched, so a proposal left out of the block can
still be approved or rejected.

### Task index

The index keeps its order (Backlog position, then id), `tasks_total` and
`tasks_truncated`. It adds `tasks_included`. Titles are clipped to 200
characters with a marker. Under pressure fewer than 40 entries are shown, and
`tasks_truncated` is true whenever any Task is not shown. The Coordinator pages
with `list_tasks` from offset `tasks_included`.

### Read caps

A read result is at most the per-read cap once serialized. A larger result gains
`result_truncated` with its `original_chars` and is shrunk by dropping the least
valuable items while its counts stay honest: a Task page lowers `returned`,
sets `truncated` and points `next_offset` at the first Task not shown; an
activity read drops the oldest events and counts them in
`earlier_events_omitted`; a pull-request read drops entries after the first and
then check names; one Task loses dependencies, then subtasks, then description.
A result that still does not fit becomes a small `too_large` notice. No read
capability was added.

### Unfittable required content

If the required content alone does not fit, the Builder returns
`ContextError::RequiredExceedsBudget`, naming the largest component and the
sizes. On an ordinary turn the Human message stays saved and unmodified, no
provider starts, and a non-retryable `RunFailed` entry says the message and
required context exceed the context budget and asks for a shorter request. A
later, smaller message is answered normally. The send command can reach this
with the default budget: `validate_text` accepts a message of up to 16,000
bytes, but JSON escaping can expand it, since each U+0001 becomes six characters.
Sixteen thousand U+0001 characters serialize to 96,000 characters, over the
64,000 default, so the refusal is covered by a regression test at the default
budget as well as by an injected smaller budget.
On a continuation the error is the existing retryable continuation error: the
Worker result stays unmediated and Retry is offered again. A follow-up
invocation that would exceed the total is an ordinary retryable turn failure.

### Snapshot

`ContextSnapshot` is returned with each built context: Conversation id and
revision, site, budget, fixed, reserved, projection and used characters,
included entry ids, the omitted count and span, shortened field count, open
proposals total and included, and Tasks included and total. It holds counts and
identifiers only, never message text, titles or summaries. It is not persisted.

### Records authorize, text does not

Only records report what is open, approved or applied: `open_proposals` and
history entries rendered from decision and outcome records. A message, a reply
or Worker text that says "approved" is a statement, not an approval. Both system
prompts say so, and that omission and truncation markers mean content is missing
from this view, not that it never existed or happened.

### Serialization

The projection is serialized from a typed struct, so key order is declared and
tested on the final bytes: stable keys first, volatile ones last, the current
message (or returned action) at the end. This is for determinism and testing.
Any provider prompt-cache benefit is an unverified hypothesis and is not
measured.

## Future direction (not implemented)

None of this exists in the code.

- **Historical retrieval.** A read-only lookup for older Conversation entries
  would need its own design. Arbitrarily large exact records will not fit one
  capped read, so pagination or field-level retrieval has to be designed first.
- **Derived summaries.** If added, a summary would be a regenerable, untrusted,
  per-Conversation cache kept outside the Conversation file, never an authority
  over the records.
- **Compaction.** Only on evidence that the deterministic Builder is not enough.
- **Project Memory.** A curated, durable source of Project knowledge shared by
  several agent roles, gated independently of semantic compaction. The
  motivations are repeated rediscovery of the same knowledge and reuse across
  agents. Memory would never be Task state, an approval, a pending request, an
  open TODO, a secret or a Conversation summary, and no agent would write
  accepted Memory directly.
