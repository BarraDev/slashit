# Dogfood reliability, durable execution history and Arch Linux packaging

Status: working plan. Keep it current as each unit lands. Each entry is labeled
with one of these states:

- VERIFIED: checked against `main` or a reproducible command in this session.
- DECIDED: an owner decision recorded in the request that started the unit.
- PROPOSED: an engineering proposal, not approved.
- IMPLEMENTED: merged or on a published bookmark, with the revision named.
- BLOCKED: cannot proceed without the named input.
- DEFERRED: intentionally not started.

Detailed evidence (dogfood Task records, run logs, private reports) lives in the
maintainer's private reports. This document holds the decisions and the state
needed to continue without that material.

## 1. Objective

Three independent workstreams, delivered as separate reviewable pull requests:

- A. AI Review outcome integrity: a review-fix run that changes no files must
  not be recorded as applied fixes.
- B. Durable execution history: a bounded, redacted diagnostic tail per Run,
  only if its storage and retention contract is proven safe.
- C. Arch Linux packaging: `slashit-bin` (verified upstream binary) and
  `slashit` (source build of a tagged release), with local validation before any
  AUR publication.

## 2. Verified baseline (2026-11-10)

- `main` is `a4ca87be` (`fix(executor): run the review-fix agent on the Task's
  own provider (#331)`). Recent merges: #329 (`slashit runs` / Task execution
  evidence over CLI and IPC), #330 (dogfood supervisor hardening), #331
  (provider-neutral review-fix agent).
- VERIFIED: `slashit runs` exists on main. An earlier investigation that said it
  did not is wrong.
- VERIFIED: `QaStatus` has three values, `Approved`, `FixesApplied`, `Rejected`
  (`src-tauri/src/domain/task.rs`). The UI labels `FixesApplied` as "AI fixes
  applied" (`src/components/task_review.rs`).
- VERIFIED: in `src-tauri/src/queue/executor.rs`, a fix run that succeeds and
  whose commit step reports `NothingToCommit` was recorded as
  `QaStatus::FixesApplied` (the activity trail already recorded
  `AiFixUnchanged`). That is the AI Review integrity defect.
- VERIFIED: production run completion commits the Worker's work before review
  (`executor.rs`, `commit_checkout` at the completion path), so in production a
  `NothingToCommit` at fix time means the fix changed no files.
- VERIFIED: five executor tests fail on unmodified `main` in this environment
  (`verdict_gate::a_cancelled_opencode_fix_turn_is_not_applied`,
  `verdict_gate::an_opencode_fix_that_edits_the_checkout_is_recorded_as_applied`,
  and three `unrecorded_acquisition` tests). They are not caused by workstream A.
  Their cause is not investigated here.
- VERIFIED: clippy on `slashit-ui` reports warnings in `commands/pr.rs`,
  `commands/pr/publication.rs` and `portability/apply_tests.rs`. None are in the
  review-fix path. They predate this plan.

## 3. Decisions

- DECIDED: three workstreams, each in its own PR. Do not combine them.
- DECIDED: publication of development PRs is authorized. Merging is not.
- DECIDED: no public release, and no AUR push, without an explicit owner
  decision.
- DECIDED: the isolated dogfood instance and its rollback files are not touched
  by any unit in this plan.
- DECIDED: `slashit runs` is extended, not duplicated. Issue #220 owns Task
  history retention. Issue #218 owns future telemetry and routing. Smart
  routing, subscription tracking and model switching are out of scope.
- PROPOSED: `docs/plans/` as the location for engineering plans, since it does
  not already exist.

## 4. Workstream A: AI Review integrity

### Distinctions the system must keep

1. Provider turn completed.
2. Provider claims success.
3. Checkout actually changed.
4. Changes were committed.
5. Review findings were actually resolved.
6. Human approved the result.

Items 1 and 2 never imply 3. Item 3 never implies 5. Item 6 is always separate.

### Unit A1: no-op fix is not "fixes applied" (IMPLEMENTED, pending merge)

- Change: when the fix run succeeds and the checkout has nothing to commit, the
  signoff is `Rejected`, and the issue list gains the line "The fix agent changed
  no files, so the review findings are not addressed." The activity trail keeps
  `AiFixUnchanged`.
- Test: `a_fix_run_that_changes_no_files_is_not_recorded_as_fixes_applied`
  (clean checkout, no-edit fixer). Passes.
- Validation: the review lifecycle suite passes except the five pre-existing
  failures above. Clippy reports no warning in the edited file.
- Known limit: `a_fix_agent_that_succeeds_records_fixes` still uses a fixer that
  edits nothing, but its fixture leaves the Worker's change uncommitted, so the
  commit step commits it and the test sees `FixesApplied`. This test pins the
  fixture, not production behavior. Replace its fixer with an editing one in a
  follow-up.
- Revision: change `ooskwpks` (`fix(executor)...`), on top of `main` `a4ca87be`.

### Partial-edit analysis (VERIFIED by reading, NOT reproduced)

- A failed fix run is recorded as `AiFixFailed`, and the message says the
  checkout "may hold partial edits from the fix agent that were not recorded".
  The Task then goes to Human Review.
- A later review round or Worker run does commit the whole checkout. Partial
  edits from a failed fix are therefore committed by the next completion commit.
  The system does not distinguish them from Worker output.
- PROPOSED recovery contract (not implemented): record the checkout state before
  a fix run and after it, and mark a failed run with its partial-edit state. Its
  outcome must be one of: no fix started; failed without edits; failed after
  partial edits; completed without edits; produced changes that need review.
  Needs a synthetic-checkout test before any implementation.
- BLOCKED on an owner decision: whether partial edits should be committed,
  discarded, or kept uncommitted for review. Deleting them is not an option.

### Provider-path defect (VERIFIED in #331, not validated on a real OpenCode Task)

- Task records showed `ai_fix_failed` ("the review-fix agent runs on Claude,
  which cannot take it") before #331. #331 runs the fix on the Task's provider.
- NOT VERIFIED: a real OpenCode Task through a full review cycle on main.
  Validation is a dogfood step, not a code change.

## 5. Workstream B: durable execution history

### Verified current behavior

- Activity has a 200-entry cap (`crates/slashit-activity/src/lib.rs`). Push
  evicts tool rows first, then milestones once no tool row remains.
- On one dogfood Task, 31 entries were evicted, all tool rows. The milestones
  survived. The loss of older milestones is possible but was not observed.
- Streamed run output is in memory only (`ExecutionLogs`, bounded at 1000
  entries) and does not survive restart.
- The durable pieces are the Task record, Activity, the bounded and redacted
  final answer (`run_result`), and the failure reason.

### Status

- PROPOSED: a bounded diagnostic tail per Run (for example 200 lines or 16 KB,
  whichever is smaller), redacted before it is written, stored in the same board
  write as the run's terminal entry, with an explicit `OutputUnavailable` state
  and a `dropped_count`. Read through `GetTaskEvidence`. Serde defaults for old
  records. Not started.
- BLOCKED: the retention and storage contract needs an owner decision on
  (a) which milestones are kept beyond the cap, (b) the output retention size,
  and (c) whether an interrupted run's tail is kept. No storage engine is
  proposed. A database is not justified by current evidence.
- DEFERRED: any change to the Activity cap. It needs its own unit under #220.

## 6. Workstream C: Arch Linux packaging

### Verified facts

- None yet beyond the request. The following must be verified before any recipe
  is written: the release asset names and checksums, the `.deb` contents
  (product name `slash-it`, binaries `slashit-ui`, `slashit`, `slashitd`), the
  runtime library requirements, and whether the `.deb` is compatible with Arch.
- The AUR name `slashit-bin` and the existing ownership of `slashit` are not
  checked.

### Status

- BLOCKED on the owner: no AUR publication until package identity, public
  version and installation behavior are approved.
- PROPOSED layout: `packaging/arch/slashit-bin/`, `packaging/arch/slashit/`,
  each with `PKGBUILD` and `.SRCINFO`. No submodules.
- PROPOSED updater rule (to verify in `src-tauri/src/commands/updater.rs`):
  AppImage keeps the signed in-app updater, Debian uses apt, RPM uses its
  manager, and AUR installs are updated by Pacman. Not implemented.
- DEFERRED: the source-build package until the binary package is validated.

## 7. Dependencies

- A depends on nothing.
- B depends on a retention decision (section 5) and #220.
- C depends on a published release and the owner's package-identity decision.
- Updater classification in C depends on `commands/updater.rs` being read.

## 8. Tests and evidence

| Unit | Evidence | Result |
| --- | --- | --- |
| A1 | `cargo test -p slashit-ui --lib queue::executor::tests::review_lifecycle` | New test passes; 5 pre-existing failures also fail on `main` |
| A1 | `cargo clippy -p slashit-ui --lib --tests` | No warning in `executor.rs`; warnings elsewhere predate this change |

## 9. Published pull requests

- PR #333, `ai-review-noop-fix`, revision `c45ffd23` (unit A1, review fix).
- PR #334, `dogfood-reliability-plan`, revision `753f52e1` (this plan).

Each entry names the exact revision published. A later review fix is a child
change and advances the bookmark by fast-forward.

## 10. Known risks

- The five pre-existing executor test failures on `main` hide any regression in
  the OpenCode review-fix and permission paths.
- Partial fix-run edits can be committed by a later completion commit.
- The activity cap can evict milestones once tool rows are exhausted.
- Clippy warnings on `main` are outside this plan and remain open.

## 11. Remaining decisions for the owner

1. Partial-edit policy after a failed fix run (commit, discard, or keep for
   review).
2. Durable output retention: size, which milestones are kept beyond the cap,
   and whether an interrupted run's tail is kept.
3. Arch package identity: whether `slashit-bin` and `slashit` names are approved
   and who owns them on the AUR.
4. Timing of the first public AUR publication.

## 12. Next recommended work

1. Review and merge the A1 PR after the owner accepts it.
2. Replace the fixture of `a_fix_agent_that_succeeds_records_fixes` with an
   editing fixer, so the success test cannot pass on a no-op.
3. Investigate the five pre-existing executor test failures on `main`.
4. Start the Arch packaging unit by reading the release assets and the `.deb`
   contents, with no recipe written yet.
5. Write the durable-output retention contract as a decision record, then decide
   on the bounded tail.
