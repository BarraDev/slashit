# Dogfood reliability, durable execution history and Arch Linux packaging

Migration status (2026-11-10): the DECIDED entries are being moved to their
subject documents. Partial-edit policy: docs/architecture/task-checkouts-and-stacks.md
("Failed review-fix runs"). Package names, location and publication limits:
packaging/arch/README.md ("Owner decisions", PR #336). Diagnostic bounds and the
#220 milestone rule: docs/architecture/run-diagnostics.md (PR #337). This file
is proposed for supersession, not deletion, until the owner approves closing #334.

Status: working plan. Keep it current as each unit lands. Each entry carries one
of these labels:

- VERIFIED: checked against `main` or a reproducible command.
- DECIDED: an owner decision, with the request that recorded it.
- PROPOSED: an engineering proposal, not approved.
- IMPLEMENTED: on a published bookmark, with the revision named.
- BLOCKED: cannot proceed without the named input.
- DEFERRED: intentionally not started.

Detailed evidence (dogfood Task records, run logs, private reports) lives in the
maintainer's private reports. This document holds the decisions and the state
needed to continue without that material.

## 1. Objective

Three independent workstreams, delivered as separate reviewable pull requests:

- A. AI Review outcome integrity.
- B. Durable execution history: a bounded, redacted diagnostic tail per Run.
- C. Arch Linux packaging: `slashit-bin` (verified upstream binary) and
  `slashit` (source build of a tagged release), validated locally first.

## 2. Verified baseline

- `main` was `a4ca87be` (`fix(executor): run the review-fix agent on the Task's
  own provider (#331)`) when this plan started. Recent merges: #329 (`slashit
  runs`, Task execution evidence over CLI and IPC), #330 (dogfood supervisor
  hardening), #331 (provider-neutral review-fix agent).
- VERIFIED: `slashit runs` exists on main. An earlier investigation that said it
  did not is wrong.
- VERIFIED: `QaStatus` has three values, `Approved`, `FixesApplied` and
  `Rejected` (`src-tauri/src/domain/task.rs`). The UI labels `FixesApplied` as
  "AI fixes applied" (`src/components/task_review.rs`).
- VERIFIED: before PR #333, a fix run that succeeded and whose commit step
  reported `NothingToCommit` was recorded as `QaStatus::FixesApplied`, while the
  activity trail already said `AiFixUnchanged`.

## 3. Decisions

- DECIDED (first request): three workstreams, each in its own PR. Publication of
  development PRs is authorized. Merging is not.
- DECIDED (first request): no public release and no AUR push without an explicit
  owner decision. The isolated dogfood instance and its rollback files are not
  touched by any unit in this plan.
- DECIDED (first request): `slashit runs` is extended, not duplicated. Issue
  #220 owns Task history retention. Issue #218 owns future telemetry and routing.
  Smart routing, subscription tracking and model switching are out of scope.
- DECIDED (second request): a partial checkout left by a failed AI Review fix
  run is preserved and never silently discarded. It is never silently
  incorporated into a later Worker or review-fix commit. The uncertain state is
  explicit, and a Human decides before the affected work continues. Any discard
  is a separate authorized operation that states what is lost. Automatic cleanup
  is not an acceptable shortcut.
- DECIDED (second request): initial diagnostic bounds, to be verified and amended
  with evidence: at most 64 KiB of sanitized diagnostic output per Run; diagnostic
  tails kept for the 20 most recent Runs per Task; stable Run identity and
  outcome metadata kept independently of tail retention; expired, truncated and
  unavailable output represented explicitly; a best-effort checkpointed tail for
  interrupted Runs, with no claim that a complete transcript survives a crash.
  No raw transcripts, prompts, source code or credentials are collected.
- DECIDED (second request): lifecycle milestones for Issue #220 survive
  independently of diagnostic-output limits. Historical approvals, review
  decisions and recovery evidence are never silently deleted because a UI limit
  was reached. A lifecycle-retention change needs explicit compatibility and
  safety tests.
- DECIDED (second request): package names `slashit-bin` and `slashit` are
  approved, conditional on AUR namespace and ownership verification.
  `packaging/arch/` is the location. No Git submodules.
- DECIDED (second request): local packaging development and validation are
  authorized. Public AUR publication is NOT authorized. No publication credential
  is configured, no AUR repository is pushed, and no package is installed into
  the owner's normal environment without a separate authorization.
- PROPOSED: `docs/plans/` as the engineering-plan location, and
  `docs/architecture/run-diagnostics.md` for the diagnostic contract.

## 4. Workstream A: AI Review integrity

### Distinctions the system must keep

1. Provider turn completed.
2. Provider claims success.
3. Checkout actually changed.
4. Changes were committed.
5. Review findings were actually resolved.
6. Human approved the result.

Items 1 and 2 never imply 3. Item 3 never implies 5. Item 6 is always separate.

### Unit A1: no-op fix is not "fixes applied" (IMPLEMENTED, PR #333)

- Change: when the fix run succeeds and its commit step reports nothing to commit,
  the signoff is `Rejected`, and the issue list gains "The fix agent changed no
  files, so the review findings are not addressed." The activity trail keeps
  `AiFixUnchanged`.
- Test: `a_fix_run_that_changes_no_files_is_not_recorded_as_fixes_applied`, a
  clean checkout with a fixer that edits nothing. Passes.
- Fixture correction (IMPLEMENTED, same PR): `a_fix_agent_that_succeeds_records_fixes`
  now commits the Worker baseline first, runs an editing fixer, and asserts that
  the fix commit holds only `review-fix.txt`. It no longer passes on a no-op.
- Baseline failures, resolved as environmental (VERIFIED): five executor tests
  failed on unmodified `main` and on the PR tree in this environment. Three
  `unrecorded_acquisition` tests failed because `acp_fixture_agent` was missing.
  That binary is gated behind the `acp-fixture` feature, which only the
  dev-dependency enables, so `cargo test --lib` does not build it. With
  `cargo build -p slashit-ui --features acp-fixture --bin acp_fixture_agent`, all
  376 executor tests pass on the PR tree. The product code is not at fault.
  Whether CI builds the fixture is NOT VERIFIED here.
- Revision: PR #333 head `1fa3d35b` (`fix(review): make the successful-fix test
  edit the checkout after the Worker baseline`), on the earlier commit
  `c45ffd23`.

### Partial-edit analysis (VERIFIED defect by reading, NOT yet reproduced end to end)

- A failed fix run is recorded as `AiFixFailed`. Its message says the checkout
  "may hold partial edits from the fix agent that were not recorded". The Task
  then goes to Human Review. Existing tests cover that message.
- VERIFIED defect: `commit_changes` in `executor.rs` runs after every Worker run
  and commits everything uncommitted in the checkout as `task: {title}`. A
  partial edit left by a failed fix is therefore incorporated into the next
  Worker commit without any Human decision. That breaks the DECIDED policy.
- The five-state distinction the owner requires (no fix started; failed without
  edits; failed after partial edits; completed without edits; changes needing
  review) is not represented in the Task record today. Only `AiFixFailed` and
  `AiFixUnchanged` exist.
- PROPOSED recovery mechanism (not implemented): record the checkout's tree
  before a fix run and after it. A failed run with a changed tree is marked as
  holding partial edits. A Task in that state refuses further commits until a
  Human chooses keep (review) or discard. The mechanism needs a new persisted
  Task field, with a serde default for old records, and a Human decision surface.
  It is a separate unit with synthetic-checkout tests.
- Next step for the partial-edit unit: a synthetic repro through `commit_changes`
  with a leftover edit, to pin the defect before the fix.

### Provider-path defect (VERIFIED in #331, not validated on a real OpenCode Task)

- Task records showed `ai_fix_failed` ("the review-fix agent runs on Claude, which
  cannot take it") before #331, which runs the fix on the Task's own provider.
- NOT VERIFIED: a real OpenCode Task through a full review cycle on `main`. This
  is a dogfood validation step, not a code change, and it does not run paid
  providers in this plan.

## 5. Workstream B: durable execution history

### Verified architecture (main `a4ca87be`, read in this session)

- `GetTaskEvidence` is built in `src-tauri/src/ipc/handlers.rs`. Runs come from
  Task Activity. Live output is the in-memory buffer, last 32 entries. The reply
  is capped at 512 KiB, and an oversized reply is refused whole. The IPC frame
  limit is 1 MiB.
- Task Activity (`crates/slashit-activity/src/lib.rs`) is capped at 200 entries.
  `push` drops the oldest tool row first, then the oldest milestone of any kind.
- VERIFIED blocker for #220: once milestones alone exceed the cap (about 67 runs
  at three milestones each), run starts, completions, recovery records and review
  verdicts are evicted. This conflicts with the DECIDED rule that outcomes are not
  silently deleted. It is pinned by the ignored test
  `milestones_outlive_the_cap_once_tool_rows_are_gone` in
  `crates/slashit-activity/src/tests.rs`, which fails by design.
- Streamed output is `ExecutionLogs` in `queue/executor.rs`. It is capped at 1000
  entries, drops silently with no count, has no per-message bound, and is lost on
  restart.
- Task state is one `tasks.toml` per Project, rewritten whole on every write.
  Task has `deny_unknown_fields`, so a build that does not know a new Task field
  refuses the board.
- Redaction (`slashit_activity::redact`) is word-based and best effort.

### Contract (IMPLEMENTED as design only, PR #337)

- The contract is `docs/architecture/run-diagnostics.md`: the tail is a Task field,
  not an Activity kind; outcomes come only from Activity; the tail holds
  diagnostic lines and excludes assistant prose; redaction happens before the
  cut; states are complete, truncated, checkpoint, and unavailable (with reasons
  `no_checkpoint`, `not_recorded`, `expired`).
- Amendments needing owner approval (PROPOSED):
  - A1: a 256 KiB aggregate diagnostic limit per Task, because every write
    rewrites the whole board. Twenty tails of 64 KiB each would be about 1.25 MiB
    per Task.
  - A2: echo at most 16 KiB per Run for the five newest Runs in the IPC reply, to
    stay under the 512 KiB reply cap, since JSON escaping can expand control
    characters up to six times.
  - D1: a v0.0.3 build refuses a board with a new Task field. Projects with tails
    would not open in that build. A release decision is needed.
- The 30 s checkpoint cadence is PROPOSED, not decided.
- Exact `dropped_count` needs a per-execution total counter that does not exist
  today.

### Status

- BLOCKED: no implementation until the #220 retention rule (milestones survive
  the cap) is decided and tested, and D1 is decided. The contract is not an
  approved implementation plan until then.
- DEFERRED: any change to the Activity cap, which needs its own #220 unit with
  compatibility and safety tests.
- Not done: no source change for Workstream B in this plan.

## 6. Workstream C: Arch Linux packaging

### Verified release facts

- Latest upstream release is `v0.0.3`, a prerelease, published 2026-10-29. Its
  asset `SlashIt_0.0.3_amd64.deb` has SHA-256
  `a17357f71c21b54d2ba61f98a1f9d82bb386d4cc608bcc894f1741cfb44daa15`. The checksum
  matches the GitHub asset digest. No `SHA256SUMS` file is published.
- The `.deb` contains `/usr/bin/slashit-ui`, `/usr/bin/slashitd`, `/usr/bin/slashit`
  (the CLI), a desktop entry with `Exec=slashit-ui`, three icons, and the license.
  It has no maintainer scripts.
- Its runtime dependencies (`libwebkit2gtk-4.1-0`, `libgtk-3-0`,
  `libayatana-appindicator3-1`) map to Arch packages that are present. The
  binaries need at most GLIBC_2.34 and report no unresolved symbols against Arch
  libraries. Verdict: compatible, no blocker (VERIFIED by the packaging lane;
  runtime not launched).
- VERIFIED: the updater classifies an install from the Tauri bundle marker, which
  a binary taken from the `.deb` retains. A `.deb`-marked Arch install is therefore
  classified as Deb. A source build has no marker and is classified as Unpackaged.
  Both refuse self-update before any byte is written, so no package-managed file
  is replaced.
- VERIFIED wording defect: the Deb refusal said "Update it with apt or dpkg". Fixed
  in PR #335 to name pacman too. The Unpackaged refusal still suggests the AppImage.
  That is wrong for a pacman-owned source build and needs an owner decision.
- AUR RPC check (read-only, at the time of the check): `slashit-bin` and `slashit`
  had no results. Ownership and history were not checked.

### Recipes and validation (IMPLEMENTED for local validation, PR #336)

- `packaging/arch/slashit-bin/PKGBUILD` and `.SRCINFO`: the pinned `.deb`, its
  SHA-256 (no `SKIP`, no `latest` URL), `provides=('slashit')`,
  `conflicts=('slashit')`, and `!strip` because the binaries are the published
  artifact.
- `packaging/arch/slashit/PKGBUILD` and `.SRCINFO`: the tagged source archive
  with its SHA-256, `conflicts=('slashit-bin')`. NOT built locally. Only
  `makepkg --verifysource` was run on it.
- `packaging/arch/README.md`: local builds, the difference between the two
  variants, and the makepkg directories.
- VERIFIED: `makepkg` as a normal user, with every build directory in scratch,
  produced `slashit-bin-0.0.3-1-x86_64.pkg.tar.zst` (about 19.6 MB). The listing
  shows the three binaries, the desktop file, the icons and the license. No
  Debian scripts were carried over. `namcap` reports only expected warnings.
- NOT TESTED: installation, launch, Pacman operations, and any update behavior on
  a real Arch desktop.
- Open items before the source recipe is reproducible:
  - trunk downloads Tailwind 4.1.10 and needs wasm-bindgen 0.2.114 at build time;
    neither is pinned.
  - Arch's rust differs from the project's `rust-toolchain.toml` pin.
  - The `Maintainer:` field is a placeholder and must be set before publication.
- DEFERRED: a full source build of `slashit`. Disk headroom and build size were not
  established.

### Status

- BLOCKED on the owner: AUR publication (not authorized). Maintainer identity, the
  unpackaged refusal wording, and the source-recipe pins are open.

## 7. Dependencies

- A depends on nothing. Its partial-edit unit depends on an owner-approved state
  design (section 4).
- B depends on the #220 retention decision and on D1 (section 5).
- C depends on the approved names (done), a maintainer identity, and a separate
  AUR authorization.

## 8. Tests and evidence

| Unit | Evidence | Result |
| --- | --- | --- |
| A1, PR #333 | `cargo test -p slashit-ui --lib queue::executor::tests::` with the acp-fixture binary built | 376 passed, 0 failed |
| A1, PR #333 | `cargo clippy -p slashit-ui --lib --tests` | No warning in `executor.rs`. Warnings in `commands/pr.rs`, `commands/pr/publication.rs` and `portability/apply_tests.rs` predate this plan |
| Updater, PR #335 | `cargo test -p slashit-ui --lib commands::updater` | 41 passed |
| B, PR #337 | `cargo test -p slashit-activity` | 35 passed, 1 ignored (the pinned #220 defect) |
| B, PR #337 | `cargo test -p slashit-activity milestones_outlive -- --ignored` | Fails as designed |
| C, PR #336 | `makepkg` build and `namcap` for `slashit-bin` | Package built; expected warnings only |

## 9. Published pull requests

- PR #333, `ai-review-noop-fix`, head `1fa3d35b` (A1, with the fixture correction).
- PR #334, `dogfood-reliability-plan`, this plan. Its head is the bookmark head
  of the PR, which this document does not record.
- PR #336, `arch-packaging-recipes`, head `6925e4ac` (Arch recipes, local
  validation only).
- PR #335, `arch-updater-deb-wording`, head `a783820c` (Deb refusal names pacman).
- PR #337, `run-diagnostics-contract`, head `22ac1383` (design and pinned test,
  no source change).

Each entry names the exact revision published. A later review fix is a child
change, and the bookmark advances by fast-forward.

## 10. Known risks

- Partial fix-run edits can be committed by a later Worker commit (VERIFIED,
  section 4). The owner's policy is not yet enforced.
- The Activity cap can evict run outcomes once milestones alone fill it (VERIFIED,
  section 5). Blocks Workstream B.
- Five executor tests need the `acp-fixture` binary. Whether CI builds it is not
  verified.
- A build with a new Task field is refused by v0.0.3 (D1).
- The source Arch recipe is not reproducible until its tool versions are pinned.

## 11. Remaining decisions for the owner

1. Approve or amend A1 (256 KiB per-Task aggregate) and A2 (16 KiB echo for the
   five newest Runs) in section 5.
2. Decide the #220 retention rule: how milestones survive the 200-entry cap, with
   compatibility and safety tests. Required before any diagnostic tail.
3. Decide D1: whether the release channel accepts a Task field that older builds
   refuse, or whether the diagnostic field waits for a version bump.
4. Decide the partial-edit recovery state design (section 4), which needs a
   persisted Task field and a Human decision surface.
5. Decide the Unpackaged refusal wording for pacman-owned source builds.
6. Set the maintainer identity for the PKGBUILDs.
7. Decide the timing of any AUR publication, which remains unauthorized.

## 12. Next recommended work

1. Owner review of PR #333, #335, #336 and #337. Merging is the owner's decision.
2. Partial-edit unit: a synthetic repro through `commit_changes`, then the state
   design in section 4.
3. Pin the source recipe's trunk tools, then build `slashit` from source only with
   verified disk headroom.
4. Run the `slashit-bin` package in an isolated environment, with launch and
   uninstall checks, once the owner authorizes installation.
5. Retention unit under #220, with the milestones test turned from ignored to
   passing, before any diagnostic-tail implementation.
