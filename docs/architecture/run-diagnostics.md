# Run diagnostics: storage contract

Status: proposed contract. Nothing in this document is implemented yet. The
diagnostic tail described in section 3 is blocked by the open decisions in
section 8, and by the retention gap in section 7 (issue #220), which a pinned
test in `crates/slashit-activity/src/tests.rs` makes visible.

Scope: one bounded, redacted diagnostic tail per coding Run of a Task, read
through the existing `GetTaskEvidence` answer (the `slashit runs` command,
PR #329). This is not a second execution-history store, not a telemetry
record (issue #218), and not a source of truth for outcomes.

## 1. Verified current behavior

Citations are to the tree at `main` (a4ca87be) and were read in this
workspace.

Reader:

- `handle_get_task_evidence` (`src-tauri/src/ipc/handlers.rs:915`) clones the
  Task, reads the executor's live snapshot and output, and assembles one
  answer. Runs come only from the Task's Activity (`evidence_runs`,
  `handlers.rs:586`), newest 5 (`MAX_EVIDENCE_RUNS`, `:240`). Output is the
  executor's in-memory buffer, last 32 entries (`evidence_output`, `:506`).
- The aggregate ceiling is `MAX_EVIDENCE_RESPONSE_BYTES = 512 KiB`
  (`:284`), enforced in `evidence_reply` (`:1018`) by refusing the whole
  answer. The IPC frame limit is 1 MiB, and the client reads responses with
  the same limit (`crates/slashit-ipc/src/protocol.rs:27`,
  `crates/slashit-ipc/src/client.rs:111`).

Activity (`crates/slashit-activity/src/lib.rs`):

- `MAX_ENTRIES = 200` (`:35`). `push` (`:491`) removes the oldest tool row
  first and, when none is left, the oldest entry of any kind (`:495-500`).
  This is the #220 gap in section 7.
- `record` (`:401`) refuses a second once-only milestone (run start, run end,
  run result, review verdict, and so on). `sanitize_text` (`:506`) applies the
  redaction and bounds to free text on the way in.
- `RunResult` keeps at most `MAX_RESULT_CHARS = 4 000` characters (`:52`).
- `next_run` (`:545`) derives a Run's number from the entries still present.

Run milestones and where they are written (`src-tauri/src/queue/executor.rs`):

- Start: `RunStarted` and the `Started` handoff stage are one board write before
  the provider is spawned (`record_run_start_for`, `:4650`; the entry is
  recorded at `:4870`).
- Completion: `RunCompleted`, `RunResult` and the buffered tool rows are one
  write (`record_run_completed`, `:8459`, entries at `:8485-8492`). The
  "Agent completed" log line is recorded after that write (`:6043`), so no
  terminal write contains it.
- Failure: `RunFailed` and, when applicable, `RecoveryRequired` are one write
  (`set_task_error_static_with_provider_exit`, `:8688`).
- Tool rows are buffered per run in `RunTools`
  (`src-tauri/src/queue/tool_activity.rs:28`) and applied only inside those
  terminal writes. The event forwarder is drained before the end is recorded
  (`RunEventDrain`, around `:1780-1800`).

Interruption and restart:

- At startup, an Interrupted InProgress Task with a `Started` stage is moved
  to `RecoveryRequired` by `mark_worker_recovery_required`
  (`src-tauri/src/app_core.rs:839-872`, `:913-960`). The `RecoveryRequired`
  record is written in the same board write as the repair. A run with a
  `RunStarted` and no end is treated as unfinished there.

Run output:

- `ExecutionLogs` (`executor.rs:1412`) maps an execution id to a `Vec` of
  entries. `record_output` (`:1421`) drops the oldest entries past
  `EXECUTION_OUTPUT_LIMIT = 1 000` (`:1418`). It keeps no count of dropped
  entries, and it does not bound the length of one message.
- The entries are: assistant text blocks and text deltas (agent prose, see
  `handle`, `:1884-1900`), "Using tool: X" lines, session starts with the
  model name, provider notices, and errors.
- A buffer is created per execution (`:5803`). Nothing removes one, including
  when its Task is deleted.

Persistence:

- A Project's board is one file, `tasks.toml` (`ProjectTasksFile`,
  `src-tauri/src/config/storage.rs:76`; `save_project_tasks`, `:1847`). It is
  written with `slashit_durable_fs::stage` and `publish` (`:1858`, `:1870`).
- Every lifecycle write rewrites the whole board: `stage_project` clones every
  Task of the Project (`src-tauri/src/lifecycle.rs:1816`), and `publish_typed`
  serializes all of them (`:1840`) while holding the tasks write lock.
- A `NotPublished` error leaves memory unchanged. A `PublishedNotDurable`
  error adopts the published board in memory (`publish_typed`,
  `lifecycle.rs:1861-1866`), as `AGENTS.md` requires for the configuration
  rules.

Redaction:

- `redact` (`crates/slashit-activity/src/sanitize.rs:116`) works on
  whitespace-separated words and joins them with single spaces. `sanitize`
  (`:79`) is `redact` followed by `one_line`. Masking is best effort by its own
  module documentation.

Schema and compatibility:

- `Task` has `#[serde(deny_unknown_fields)]` (`src-tauri/src/domain/task.rs:126`).
  So do `Entry` and `Kind` (`crates/slashit-activity/src/lib.rs`).
- The schema tests (`src-tauri/src/config/task_board_schema_tests.rs`) refuse
  an unknown key in every table, and require the fixture to populate every
  field. A new Task field therefore needs a fixture change.
- The released `v0.0.3` tag has the same `deny_unknown_fields` on `Task`
  (checked with `jj file show -r v0.0.3`, `src-tauri/src/domain/task.rs:126`).

Storage accounting and deletion:

- Project boards are measured as "Project boards" (`storage_accounting/inventory.rs:299`).
  A tail stored in the board is counted without new code.
- Deleting a Task removes it from the board in the next write
  (`commands/task.rs:899`). Deleting a Project records the board paths in a
  cleanup record (`config/storage.rs:603-625`) and then deletes them. No
  separate tail file exists, so a tail leaves with its board.

Measured sample: the largest local `tasks.toml` is 31 276 bytes, with 2 Tasks
and no `run_result` entries. This is one small sample, not a worst case.

## 2. Storage contract

### 2.1 What is durable

| Item | Where | Survives restart | Source of truth |
|---|---|---|---|
| Run start, end, failure, recovery | Task Activity | Yes | Yes |
| Final provider answer for a completed Run | Task Activity (`RunResult`) | Yes, at most 4 000 characters | Yes |
| Tool rows (name and short detail) | Task Activity | Yes, capped per Run | Yes |
| Diagnostic tail of a Run (proposed, section 3) | Task record | Yes, after the terminal write or a checkpoint | No. Output only |
| Live run state, execution id, session id | Executor memory | No | No |
| Streamed output (`ExecutionLogs`) | Executor memory | No | No |
| Assistant prose, raw stream, transcripts, prompts, source code, credentials | Never stored | No | No |

### 2.2 Run identity

A Run is keyed by `(Task id, run number)`. The number comes from the Activity
(`next_run`, `slashit-activity/src/lib.rs:545`). It is stable for the life of
the Task as long as the newest Run's entries are kept, and they always are.

The execution id is not persisted and must not be used as a key.

### 2.3 Bounds

| Bound | Status | Value and reason |
|---|---|---|
| B1: tail per Run | Owner decision, kept | At most 64 KiB of stored text, counted as UTF-8 bytes after normalization and redaction. |
| B2: Runs with a tail | Owner decision, kept | The 20 newest Runs per Task. The tail of Run 1 expires in the write that starts Run 21. Outcomes are never removed for this reason. |
| A1: aggregate per Task | Amendment, needs approval | At most 256 KiB of tail text per Task. Oldest tails expire first. See evidence below. |
| A2: echo per Run | Amendment, needs approval | `GetTaskEvidence` echoes tails only for the 5 newest Runs, at most 16 KiB each. |

Evidence for A1. With B1 and B2 alone, one Task can hold 20 x 64 KiB = 1.25 MiB
of tail text. The Activity already admits 200 entries of up to 4 000
characters (`MAX_RESULT_CHARS`), about 800 KB, before any tail. Because every
lifecycle write rewrites the whole Project board (section 1, `lifecycle.rs:1816-1868`),
the size of every Task on the board is paid on every write. Twenty Tasks at the
B1 and B2 ceiling would make each board write about 25 MB before escaping. A1
keeps the per-Task worst case near 1 MB even with the Activity's own ceiling.

Evidence for A2. The reply is refused whole above 512 KiB (`handlers.rs:284, :1018`),
and the frame limit is 1 MiB. Five Runs at 64 KiB are 320 KiB before escaping.
JSON and TOML write a control character as a six-byte escape, so a tail with
many control characters can grow about sixfold. The tail is therefore
normalized before storage (control characters become spaces), and the echo has
its own per-Run limit.

Realistic size (an estimate, not measured). A long coding Run can emit a few
hundred "Using tool" lines of roughly 40 bytes each. That is about 10 to 15 KB
per Run, or 200 to 300 KB per Task at 20 Runs. A board of 20 such Tasks is about
5 MB per write, which is why A1 is worth deciding before the checkpoint cadence
is set.

### 2.4 Content

The tail holds only diagnostic lines of its Run: `Error` and `Warn` entries,
provider notices, the session start with its model name, "Using tool" lines,
the stop line, and the refusal or failure messages the executor already writes.

It excludes assistant text blocks and text deltas (the agent's prose). Those can
carry source code and credentials, and the owner decision forbids collecting
raw transcripts. Decision D4 in section 8 asks for explicit confirmation.

### 2.5 Redaction

- Redaction runs when the tail is built, before the tail enters the Task
  record. The input is the joined normalized text of the whole tail, so a
  credential whose name and value sit on adjacent lines is still matched
  (`redact` splits on whitespace, so the join is what keeps the pair together).
- Redact first, then cut. Cutting first could leave a token prefix that the
  redactor does not recognize.
- The echo redacts again at read time, as the Activity echo already does
  (`handlers.rs` `evidence_text` and `evidence_excerpt`).
- Redaction is best effort. The tail is not a guarantee that no secret is
  stored. The test in section 6 covers the patterns the redactor claims.

### 2.6 Crash consistency

- The terminal tail is written in the same board write as its terminal
  milestone (`RunCompleted`, `RunFailed` or `RecoveryRequired`). The tail is
  snapshotted from `ExecutionLogs` before that write. Lines written after it,
  such as "Agent completed", are not in the persisted tail. The tail ends at
  the terminal write, and the reader says so.
- A `NotPublished` terminal write publishes neither the outcome nor the tail.
  The existing error path handles the Run.
- A `PublishedNotDurable` terminal write adopts the visible board, as the
  existing lifecycle code does. The tail is in the visible file. A crash can
  still revert the write, and then startup marks the Run `RecoveryRequired`
  (section 1), so the tail and the outcome stay consistent with each other.
- Checkpoints are separate writes, and they are best effort. A checkpoint
  failure is logged as a warning and does not fail the Run, following
  `record_observed_model` (`executor.rs:1857`). A checkpoint carries a
  `checkpointed_at` time and is never `complete`. It is a prefix of the output
  the run had produced, not the whole of it.
- Proposed cadence (decision D3): at most one checkpoint per 30 seconds per
  running Task, written only when the tail changed, through the same
  `record_typed_if_changed` pattern as the observed model.
- A checkpoint must not change approval, handoff or review fields. It touches
  the tail only, as the observed-model write does.

### 2.7 What the reader shows

Every Run in the answer has one explicit state. An empty list never means
"nothing was said".

| State | Meaning | Required fields |
|---|---|---|
| `complete` | Terminal tail present, nothing dropped | `entries`, `dropped_count = 0` |
| `truncated` | Terminal tail present, something dropped | `entries`, `dropped_count` (exact) |
| `checkpoint` | Run has a start and no terminal tail; the Run was interrupted | `entries`, `checkpointed_at`, the message "interrupted; this is the last checkpoint" |
| `unavailable` | No tail is held | `reason`: `no_checkpoint` (interrupted before the first checkpoint), `not_recorded` (the Run predates the feature), or `expired` (outside the 20 newest) |

`dropped_count` must be exact. The buffer drops at 1 000 entries and keeps
no total, so the implementation must count every entry written to an
execution. Without that counter, the dropped count is not exact.

`not_recorded` needs a marker. A proposed Task field, `diagnostics_since_run`
(`Option<u32>`, `serde(default)`), records the first Run number that has a
tail. Runs below it are `not_recorded`. Runs at or below the newest Run minus
20 are `expired`.

### 2.8 Outcome and tail

- Outcomes (`status`, `ended_at`, the failure reason, the result) come only
  from the Activity. The tail carries no status.
- The reader derives each Run's state from the Activity and the tail together.
  The tail never decides an outcome.
- Two records of one outcome, one in the Activity and one in the tail, are
  forbidden. The tail holds text and nothing else.

### 2.9 Reader behavior across versions

- A current build reads a legacy board. A Task without the new field loads with
  an empty tail, and its Runs are `not_recorded`. No migration write is needed.
- A current build reads a board written by `v0.0.3` (no tail field). It works as
  above.
- A `v0.0.3` build reads a board that has the new field. `Task` denies unknown
  fields, so that build refuses the whole Project board. This does not lose
  data on disk, but the older build cannot show or save that Project's Tasks.
  A downgrade past this change is therefore blocked for any Project that has
  tails. This is decision D1 in section 8.
- No change to `Kind` is proposed. An enum change would make old builds refuse
  every board, not only boards with tails, so the design avoids it.

### 2.10 Deletion

- Deleting a Task removes its tail in the same write that removes the Task.
- Deleting a Project removes its tails with its board file, under the cleanup
  record that already exists (`config/storage.rs:603-625`).
- The in-memory `ExecutionLogs` entries are not removed when a Task is deleted
  (`executor.rs:5803` is the only insert, and nothing removes). This is a
  pre-existing memory risk, and the implementation should remove the Task's
  entries on delete.

## 3. Proposed diagnostic tail (not implemented)

- The tail is `Task.run_diagnostics`, a map from run number to a record with
  the state, the normalized and redacted entries, `dropped_count`, and
  `checkpointed_at` when the record is a checkpoint.
- It is `serde(default)` and `skip_serializing_if` empty, so a board without it
  loads unchanged.
- It is written only by the executor: at a terminal write (section 2.6), at a
  checkpoint, and by the expiry rule (B2) in the write that makes a newer run.
- Startup needs no change. Its `RecoveryRequired` write already exists.

## 4. Test plan for the implementation (not yet written)

Each test forces the named case with an injected failure or a fixed fixture.
None uses a provider.

1. A completed Run, then a restart: the tail and the outcome both reload.
2. A failed Run: the tail reloads with the failure.
3. An interrupted Run (start present, no end): the reader shows `checkpoint`
   when a checkpoint exists, and `unavailable` with `no_checkpoint` otherwise.
   It never shows an empty list.
4. Truncation at 64 KiB and at the buffer's 1 000 entries: `dropped_count` is
   exact.
5. Expiry: the 21st Run removes the 1st tail. Its outcome stays in the
   Activity. This test also needs the #220 decision for runs past the cap.
6. Redaction: a secret in a diagnostic line, including one split across two
   adjacent lines, never appears in the bytes of `tasks.toml`.
7. A legacy board, with no tail field, loads and its Runs read `not_recorded`.
8. `NotPublished` on the terminal write leaves the prior board and no tail.
9. `PublishedNotDurable` adopts the visible board, and the next write does
   not revert the tail.
10. The tail never carries a status. An outcome appears once, in the Activity.

## 5. Owner bounds: keep, amend, or pending

- B1 (64 KiB per Run): kept as a ceiling. It is feasible per Run, but not per
  Task without A1.
- B2 (20 Runs): kept.
- A1 (256 KiB per Task) and A2 (16 KiB echo per Run, 5 Runs): proposed,
  pending approval. Evidence is in section 2.3.

## 6. Redaction and secrets

Redaction is best effort. The tests in section 4 check the shapes the
redactor already claims (`sanitize.rs` module documentation). They do not claim
to find every secret.

## 7. Retention gap: issue #220 (blocking)

The Activity cap (`slashit-activity/src/lib.rs:491-500`) removes the oldest
milestone once no tool row is left. The owner decision says a limit must never
silently delete historical approvals, review decisions or recovery evidence.
The current code does.

Pinned test: `milestones_outlive_the_cap_once_tool_rows_are_gone` in
`crates/slashit-activity/src/tests.rs`. It is `#[ignore]`, and it fails by
design. Run it with `cargo test -p slashit-activity milestones_outlive -- --ignored`.
At the time of writing it reports: "the recovery record of run 1 was evicted".

Reachability: tool rows are dropped first, so milestone eviction starts only
when the milestones alone exceed 200. Each Run adds about three milestones
(start, end, result), so that happens at about 67 Runs, and review cycles bring
it sooner. Eviction then removes Run 1's start, which is the outcome metadata
the tail contract says must survive expiry.

Any fix is a lifecycle-retention change. The owner decision requires explicit
compatibility and safety tests for it. The fix is not chosen here.

## 8. Open decisions

- D1. Downgrade. A board with tails is refused by `v0.0.3`. Decide whether to
  accept this with a documented procedure (the precedent is
  `docs/architecture/project-removal.md`, section 8), or to delay the change
  until the next release.
- D2. Approve or reject A1 and A2 (section 2.3).
- D3. Checkpoint cadence (proposed 30 seconds per running Task), and whether
  checkpoints for interrupted Runs are part of the first slice.
- D4. Content: diagnostic lines only (proposed), or also assistant text. The
  owner's raw-transcript rule points to diagnostic lines only.
- D5. The #220 retention rule (section 7). Blocking for the expiry test and for
  any claim that outcomes survive beyond about 60 Runs.

## 9. Not verified

- Whether a real provider run emits as many "Using tool" lines as the estimate
  in section 2.3 assumes. The estimate is not measured.
- Power-loss behavior of the board write. `slashit_durable_fs` guarantees are
  per file, and this contract does not add to them.
- Behavior on a filesystem that cannot synchronize directories. The board
  rules in `AGENTS.md` apply unchanged.
- The GUI rendering of tails. Nothing in this contract changes the GUI.
