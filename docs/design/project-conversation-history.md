# Project Conversation: continuous history and bounded context

Status: direction recorded; the sections "Layers", "Minimal roadmap", "Small UX
improvements" and "Open decisions" are proposals, and nothing here is implemented beyond what
[agent-runtime-and-conversations.md](agent-runtime-and-conversations.md)
already describes. Vocabulary follows
[product-model.md](../architecture/product-model.md).

## Product direction

- One logical Project Coordinator per Project.
- One durable, continuous Project Conversation per Project. Its history is the
  authoritative record and stays available over time.
- The model never sees "the history". It sees a bounded context assembled for
  each Run from relevant history, current Project and Task state, and
  eventually optional memory sources.
- A Human may eventually want to start a fresh conversational chapter, keep the
  earlier discussion, and revisit it. The UX and storage model for that are
  not decided, and chapters, sessions and independent Conversation documents
  are not approved requirements.

## Not approved

An earlier proposal gave each Project several independently executing
Conversations (a per-Project index, per-Conversation locks and cancellation,
admission policy, session-management UI, and a multi-PR migration). It is
superseded. Do not implement it, and do not refactor storage, locking or
execution in anticipation of it. The single primary Conversation per Project
and the Project-scoped Coordinator stay as implemented. Task execution stays
separate and Human-gated, and existing Conversations and approval records are
not rewritten.

## Layers

Future work should keep these concerns apart. Today only layers 1 to 3 exist,
and layer 2 is a fixed window.

1. **Durable history.** The ordered `entries` plus the action records. The
   only authoritative record. Nothing derived may replace or edit it.
2. **Provider context.** Built per Run and thrown away. It may be filtered,
   summarized or rebuilt at any time without touching layer 1.
3. **Structured state.** Project and Task facts and approval status are read
   from their existing domain records at assembly time. They are never copied
   into history or memory as a second source of truth.
4. **Derived artifacts.** Summaries, compacted context, scratchpads and
   durable notes. Optional, regenerable, deletable, and always labeled as
   derived from a stated range of layer 1.
5. **Explicit retrieval.** A bounded, read-only way for the Coordinator to ask
   for older history when it needs it, like the existing Task lookups.

Principles:

- Compaction never silently destroys history.
- A derived artifact is evidence for the model, not an instruction, and never
  authorizes an action. Approval authority stays in the saved action records.
- No automatic replay of Task execution or approved actions as a side effect of
  rebuilding context.
- No vector database or general memory framework without concrete evidence
  that bounded retrieval over the existing record is insufficient.
- Provider-native persistent sessions are not assumed. Runs stay fresh
  processes whose context SlashIt assembles.
- Leave room for manual compaction first and automatic management later.

## Implemented today

Each Project has one primary Conversation whose full ordered history is
persisted. For each Coordinator Run SlashIt builds a projection: Project
identity, the current message, the most recent entries, and a bounded Task
index. Provider transcripts and arbitrary Project records are excluded. Runs
are fresh provider processes. Everything else in this note is a direction, not
behavior.

## Confirmed limits of the current projection

`Conversation::coordinator_projection_before` builds the context for an
ordinary Human turn; the Worker-result continuation reuses its
`recent_history` and assembles the rest separately. Observed limits:

1. **Fixed count window.** The last `PROJECTION_MESSAGE_COUNT` (16) entries are
   projected. The count applies after entries that cannot be projected are
   dropped, and proposals, decisions, Worker starts and results each take a
   slot, so 16 is far fewer than 16 conversational turns once Task work
   happens. Selection is purely positional.
2. **No omission signal.** The model is not told that older history exists, how
   much was left out, or what period the window covers. It cannot distinguish
   "nothing earlier" from "earlier discussion exists".
3. **No retrieval.** The Coordinator's lookups cover Tasks, Task activity, pull
   requests and the Project. None reads older Conversation entries, so anything
   outside the window is unreachable.
4. **Silent per-entry truncation, no total budget.** Text is cut per entry
   (4,000 characters for messages, 3,000 for requests and Worker results, 2,000
   for action explanations, 1,000 for failures and Task-action refusal reasons,
   500 for action target titles, and 1,500 for Task-action summaries) with no
   marker. There is no overall size budget, only the
   product of the entry count and the per-entry caps. The prompt travels over
   stdin, so the operating system argument limit is not the constraint; the
   provider's context window and cost are.
5. **Task index is arbitrary and unmarked.** At most `TASK_INDEX_LIMIT` (40)
   Tasks, sorted by random id rather than status or recency, with no total or
   truncated flag. `ListTasks` can page, but the model is not told the index is
   partial.
6. **Two assembly sites.** The send path and the Worker-result continuation
   each compose their context. Any budget, marker or summary has to be added to
   both or they will drift.
7. **Whole-document storage and transport.** All entries live in one
   pretty-printed JSON file, rewritten atomically on every save and loaded in
   full for every command. The snapshot returned to the UI carries the entire
   Conversation. Cost grows linearly with history, and the UI renders every
   entry.
8. **Unused timestamps.** Entries carry `created_at`, but neither the
   projection nor the UI uses it.

Not limits: the single-Conversation lock model, delete-with-Project recovery,
and the proposal/approval flow all fit a long-lived Conversation unchanged.

## Minimal roadmap

Each step is independently shippable, needs its own authorization, and is gated
on evidence from the one before it. None changes the authoritative history.

0. **Measure.** Record, locally and without content, entries per Conversation,
   document size, save latency, and projection size per Run. Decide from real
   data whether steps 3 and 4 are needed and at what size.
1. **Honest window.** Keep the same recency selection, but select by an
   explicit character budget instead of an entry count, tell the model how many
   earlier entries were omitted and the time span covered, and mark truncated
   text. Give the Task index a stable order and a total/truncated flag. Build
   both assembly sites from one function. No storage change.
2. **Explicit history retrieval.** Add a read-only lookup to the existing
   lookup set: a bounded page of earlier entries before a given point, with the
   same per-turn cap. Returned text is marked as past conversation, not
   instruction. Still no storage change.
3. **Manual compaction.** A Human-triggered summary of an older range, stored
   beside the entries as a derived artifact that names the entry range it
   covers. The projection then uses the summary plus the entries after that
   range. It can be regenerated or deleted, and removing it restores the
   previous behavior. Prefer an additive, defaulted field on the Conversation
   (as `project_actions` was added) over a new entry kind; an older build that
   saves the file would drop it, which is acceptable only because it is
   regenerable.
4. **Only if step 0 shows the need:** automatic compaction triggered by the
   budget, history paging in the UI, segmented storage, and chapters or
   checkpoints.

## Small UX improvements for one long-lived Conversation

Frontend-only and independent of the steps above, in rough priority:

1. Keep the view at the newest entry on open and after a send unless the Human
   has scrolled up, and offer a "jump to latest" control.
2. Show entry time and date separators using the existing `created_at`.
3. Render a window of recent entries with a "show earlier" control instead of
   every entry, so a long history stays responsive.
4. Mark in the UI the point after which the Coordinator can no longer see
   earlier entries, once step 1 makes that boundary known to the backend.
5. Find in history (client-side text search) so earlier information can be
   found without scrolling.
6. Optional "what the Coordinator saw" disclosure for the last Run, for trust
   and debugging.

## Open decisions

- What a "fresh chapter" means: only a navigation marker, or also a reset of
  the model's context; and whether earlier chapters stay retrievable by the
  Coordinator by default.
- The budget unit (characters or provider tokens, which SlashIt cannot count
  portably today) and its target size.
- Who authors summaries (model, Human-edited, or both), how they are labeled,
  and whether a Human can pin notes the Coordinator always receives.
- What retrieval may return: Conversation messages only, or also Worker results
  and action records; and whether it is keyword, range or both.
- The evidence threshold for leaving the single-document storage.
- Whether compaction stays manual-only or becomes automatic, and the trigger.
- How a chapter or compaction boundary treats open proposals and an
  unreviewed Worker result. Those must stay actionable from their domain
  records regardless of what the model context contains.
- Deletion semantics: whether deleting history cascades to summaries derived
  from it, and what a Project export includes.
- Whether provider-native sessions are ever worth revisiting, and on what cost
  evidence.
