# Corrections after a pull request exists

Status: design contract, not yet implemented. The implementation Tasks are
listed at the end. Until they land, [product-model.md](product-model.md#approval-coverage)
describes the behavior that ships.

## Problem

A Task in PR Created holds an approval of the commit it delivered. When that
pull request needs a correction (a CI fix, a reviewer's request, a commit the
owner made by hand), no path does all of this: request the correction, run
the Task again, approve the new commit exactly, publish it to the same pull
request, and keep the earlier approval and decisions in the history.

| Path tried | Why it fails today |
|---|---|
| Request Changes | `request_changes` (`commands/human_review.rs`) requires Human Review and refuses a current approval. |
| Approve again | `approve_again` refuses once a pull request exists ("This task has a pull request ... its approval is not renewed"). |
| Drag PR Created to Human Review | `Task::record_move` withdraws an approval only when the Task leaves Human Review, so the published approval stays current and both paths above still refuse. |
| Drag PR Created to Queue | Retires the approval, but records no feedback, so the run gets only the description it already delivered. |
| New attempt | Worker recovery, not a correction. |

The characterization tests in `src-tauri/src/domain/task.rs`
(`today_a_move_back_to_human_review_keeps_the_published_approval_current`,
`today_a_queue_move_from_pr_created_retires_the_approval_without_feedback`)
pin the last two rows.

## Decision

The approval provenance work (PR #355) made each approval bind one exact
commit and show as stale when the branch or the pull request head moves.
The fix needs no new record and no state machine. It reuses
**Human Review arrivals**, the mechanism that already separates one review
from the next:

- An approval is current only within its arrival (`HumanReviewRecord::current_decision`).
- `withdraw_approval` closes the arrival. The entry stays in `entries` with its
  `approved_tip`, and nothing is current until a new decision is made.
- Feedback for the next run is the change requests of the current arrival
  (`pending_feedback`).
- Delivery through an existing pull request already exists: Approve & Create
  PR finds the branch's pull request (`find_existing_pr_for_branch`, every
  state), fast-forwards it to the approved commit by object ID
  (`publish_approved_to_existing_pr`), links it and moves the Task to PR
  Created.

So the design adds two entry transitions from a published Task into a new
arrival, one guard (G1, with its evidence rule G2) that lets an open pull
request read move a correction to PR Created only once the pull request is
observed at the approved commit, and one completion rule (D) that finishes a Task on merge
only when the approved revision is the revision that was merged. Everything
after the entry is the existing Human Review loop. `a_post_pr_correction_needs_no_new_review_record_field` shows that the
record already expresses the whole sequence.

Approve again keeps its #355 rule: once a pull request exists, an approval is
not renewed within its arrival. A later commit is reviewed in a new arrival
instead. The delivered approval therefore stays final at the commit it
covered, and a new approval is a separate history entry.

An approval of the corrected commit does not prove delivery. A Task is
complete only when GitHub reports that the pull request merged at the approved
commit (D). Otherwise the merge is recorded, the Task is held, and the Human
decides.

Approval and publication remain separate. Explicit Publish to PR
(`commands/pr/publication.rs`) keeps working as it does now: it publishes
without approving, and the panel then shows the approval as stale.

## Transitions

"Published Task" means a Task with a pull request (`pr_url` or a GitHub pull
request reference). Every transition takes the Task's lifecycle lease, checks
the refusals below under it, and writes once through `lifecycle::commit_task`.

### Refusals shared by R1 and R2

These are refused before any write, the Task is left as it was, and the
reason is named:

- `Task::queue_entry_refusal()`: an unfinished checkout cleanup, a pending
  republish (restack), Worker recovery, an unresolved review-fix residue, or
  an unfinished local integration.
- `pr_publication` needs reconciliation (`Pending` or `Unknown`). The person
  publishes again, which reconciles the record first.
- A delivery is in flight (`DeliveriesInFlight::contains`).
- The pull request is recorded merged or closed. A correction cannot reach it
  any more, so the message suggests a follow-up Task.
- A run or AI review is live. A PR helper (review Apply or Sync) is ended,
  exactly as any status move ends it. Apply's own analyzed-head binding (PR
  #302) refuses the next Apply against the moved head.

### R1. Reopen review

From PR Created, or from Human Review with a current approval and a pull
request, to Human Review with a new arrival and no decision.

- Effect: `withdraw_approval()` (a new arrival), `qa_signoff = None`,
  `pr_error = None`, status Human Review, and `ActivityKind::ReadyForReview { arrival }`
  recorded with the Moved entry (when the column changes). Adding no new
  activity kind keeps the board readable by earlier builds.
- Reached by a new `reopen_task_review` command (drawer: **Review again**) and
  by any manual move from PR Created to Human Review: `Task::record_move` also
  withdraws the approval for that move, so a drag, an IPC move and a
  Coordinator move all behave like the command.
- Used when the branch moved without a SlashIt run (an owner's commit, a
  local merge of the remote), and as the recovery when R5 is refused because
  the remote is ahead.
- Test: `reopening_review_of_a_published_task_retires_the_approval_into_history`
  (replaces `today_a_move_back_to_human_review_keeps_the_published_approval_current`),
  `reopening_review_is_refused_while_a_publication_is_unsettled`,
  `reopening_review_by_drag_matches_the_command`.

### R2. Request changes on a published Task

From PR Created with a current approval, to Queue, carrying the feedback.

- Effect, in one write: `withdraw_approval()`, then
  `push(ChangesRequested, feedback)` in the new arrival, `qa_signoff = None`,
  `pr_error = None`, status Queue, `reset_execution_state()`. The branch and
  the Task Checkout are kept. This is today's `request_changes` with the
  status check widened to PR Created and the approval withdrawn first, so the
  outcome where the feedback is saved but the Task stays put cannot occur.
- A reopened Task (R1) is already in Human Review with no decision, so
  today's Request Changes serves it unchanged.
- Test: `changes_requested_on_a_published_task_queue_it_with_the_feedback`
  (asserts the old approval entry and its `approved_tip` survive, and that
  `pending_feedback` is the new text),
  `a_change_request_on_a_published_task_with_an_unsettled_publication_records_nothing`.

### R3. Correction run

Queue, In Progress, AI Review, then Human Review. This is the existing
executor path: the prompt carries the feedback under "Human Review Feedback",
and `transition_to_human_review` records a new arrival.

- New guard G1. `refresh_task_pr_state` and `link_pr_to_task` (`commands/pr.rs`)
  move a Task to PR Created whenever its pull request is OPEN, and first end
  its live owner. For a published Task (one already linked to that pull
  request when the read starts) they record only the pull request's state,
  with no status change and no owner ended, unless the open delivery evidence
  (G2) is verified. In Queue, In Progress or AI Review, or in Human Review
  with no current approval, G2 never holds, so the Task and its run stay
  where they are.
- G1 also covers a CLOSED answer during an active post-PR correction (a
  published Task that R1 or R2 moved out of PR Created and that has not yet
  returned to it). Today `refresh_task_pr_state` ends the live owner before
  its write unless the Task is pinned, and the write sets PR Created for
  CLOSED as well as OPEN (`matches!(refreshed, "CLOSED" | "OPEN")`, and the
  same test in `pr_refresh_changes`). A pull request closed during a
  correction run would therefore stop the run and pull the Task out of the
  correction. Under G1, a CLOSED answer for such a Task records only the
  pull request's state: the correction owner is not ended, and the Task's
  status, its Run and its Task Checkout are kept. The observation does not
  end or invalidate the correction. G2 does not apply, because a closed pull
  request is never open delivery evidence; R5 later refuses the push with the
  existing "not open" message (see Failure cases).
- Outside a correction, CLOSED keeps today's behavior and this design does
  not change it: a Task not yet linked to the pull request, or a published
  Task already in PR Created, has its owner ended (unless pinned), the state
  recorded, and its status set (or kept) at PR Created. The CLOSED rule above
  applies only to an active post-PR correction, not to every published Task.
- Test: `a_refresh_during_a_correction_run_records_the_state_and_keeps_the_run`,
  `a_closed_pr_observed_during_a_correction_records_the_state_and_keeps_the_run`
  (deterministic, with a fake `gh` answer of `state` CLOSED: the Task stays
  in its correction status, the live owner is not ended, the Run and Task
  Checkout are unchanged, and only the reference's state reads CLOSED),
  `a_refresh_of_a_reopened_review_does_not_move_it_to_pr_created`,
  `the_prompt_of_a_published_tasks_correction_carries_the_feedback`.

### G2. Open delivery evidence

Open-PR reconciliation moves a published Task from Human Review to PR Created
only on verified publication evidence that the open pull request now holds
the approved revision. The check is one pure function,
`open_delivery(task, observed)` in `domain/publication.rs`, the open-PR
counterpart of D's `merged_delivery`. It runs under the lifecycle lease, on
the Task as staged, inside the same write that would set PR Created. Both
callers pass it the `state` and full `headRefOid` of one `gh pr view` answer
(`refresh_pr_url`'s `PrStatus`; `link_pr_to_task` reads the same answer
instead of the state alone).

Evidence, all required:

- **Approval evidence.** The current decision of the current arrival is
  Approved, with a recorded `approved_tip` B (as in D).
- **Observation.** The answer reports `state` OPEN and a full `headRefOid` H,
  and H equals B. An abbreviated, missing or different head is not evidence.
- **Publication evidence.** `pr_publication` is `Published` for B, for this
  pull request (`commit` = B, `pr_url` = the linked URL). R5 writes this
  record for every correction, including the `At` case where nothing is
  pushed. An absent record, one naming another commit or pull request, or one
  that is `Pending`, `Unknown` or `Refused` is not evidence.

Verdicts:

| Verdict | When | Effect |
|---|---|---|
| Delivered | All evidence present | Status PR Created, as today's link. |
| Not yet observed | Any evidence missing | Only the pull request's state is recorded. Status stays Human Review, the approval of B stays current, no owner is ended, and `pr_error` names B, H and what is missing. Check now asks again. |

**Routes that read an open pull request.** Post-publication recovery
(R5a back to PR Created) can be reached by more than one read, and each
must hand G2 the head of the answer it acts on:

- **Check now and the background poll.** The drawer's Refresh (Check now)
  calls the frontend's `refresh_task_prs` (`src/components/pr_status.rs`),
  which invokes the `refresh_pr_status` command (`commands/pr.rs`). That
  command refreshes each linked reference through `PrStatuses::refresh_each`
  and calls `TaskExecutor::apply_polled_pr_state` (`queue/executor.rs`) with
  the answer, exactly as the background poll (`poll_pull_requests`) does.
  Today `apply_polled_pr_state(task_id, key, state, merge_commit)` receives
  only the state and merge commit: `PrStatus.head_oid` is parsed in
  `pr_status.rs` but dropped at both call sites, so G2 would see no observed
  head on this route. Today an OPEN answer here only records the state
  (`record_pr_poll_state`) and never moves the Task. For Check now to
  complete recovery, `apply_polled_pr_state` must take the `head_oid` of
  the same cached answer whose `state` it acts on (the answer it already
  re-checks against the cache before acting), and pass it to
  `open_delivery`. Both call sites pass it.
- **`refresh_task_pr_state`** (`commands/pr.rs`, reached from the Kanban
  PR-review apply and reply-sync). Today it ends the live owner and sets PR
  Created on any OPEN answer (`matches!(refreshed, "CLOSED" | "OPEN")`, and
  the same test in `pr_refresh_changes`) with no approval-provenance check.
  For a published Task, that OPEN move is gated by G2 on the `head_oid` of
  the same `refresh_pr_url` answer, and `pr_refresh_changes` must not report
  a move G2 would refuse.
- **`link_pr_to_task`**, at the end of R5, as described above.

On every route, an OPEN observation whose head is missing, abbreviated or
different from B is Not yet observed. It does not complete recovery: it
does not set PR Created or otherwise restore the published delivery state,
does not end or invalidate the correction owner (the live run, a PR helper,
or B's approval), and does not remove or clean up the Task Checkout. Only
the pull request's state is recorded.

A Task not yet linked to the pull request (the first Approve & Create PR)
keeps today's move: the pull request was just opened from the approved commit
SlashIt pushed, and approval coverage (#355) reports any head that differs.

D is unchanged and is not weakened by G2. G2 decides only whether an open
pull request may show the correction as delivered. Completion still requires
the merged head to equal the approved commit, so a Task G2 kept in Human
Review that merges at the old head is held by D, and approval provenance
(#355: the exact commit on each approval, stale once the branch or head
moves) is read, never rewritten, by both rules.

- Tests (domain unit tests of `open_delivery` for each row, and command tests
  with a fake `gh` answer):
  `an_approved_correction_is_not_moved_to_pr_created_while_the_pr_holds_the_old_commit`
  (see R5a), `a_published_correction_observed_at_the_approved_head_moves_to_pr_created`,
  `an_open_answer_without_a_full_head_keeps_the_correction_in_human_review`,
  `an_approved_correction_without_a_published_record_stays_in_human_review`,
  `an_unsettled_publication_keeps_the_correction_in_human_review`,
  `a_first_pull_request_link_still_moves_the_task_to_pr_created`.

### R4. Approve the corrected commit

Human Review, no decision, to Human Review, approved. Unchanged:
`record_approval` records the commit the person was shown and refuses a
branch that moved since. The entry is appended, so the history reads
approved (old commit), changes requested, approved (new commit).

- Test: `an_approval_after_a_post_pr_correction_records_the_new_commit_and_keeps_the_old`.

### R5. Update the existing pull request

Human Review, approved, to PR Created. This is the existing Approve & Create
PR or Create PR path. The button reads **Approve & Update PR** when a pull
request is linked. The approved commit is pushed without force and only when
the remote does not already contain it. The existing link then moves the Task
to PR Created only when G2 is verified: the publication record is `Published`
at the approved commit and the same `gh pr view` answer reads the pull
request's head at that commit. Otherwise the Task stays in Human Review with
its approval current (R5a). Approval coverage (#355) then shows the approval
as current, because the head it reads is the approved commit.

The push to an existing pull request writes `pr_publication` through the
existing publication record (`domain/publication.rs`): `pending` before the
push, `classify_outcome` after reading the remote back, and `replay` and
`reconcile` for a retry or a crash. Explicit Publish to PR already writes this
record, so a person or the panel can tell "approved, not yet published" from
"published", and a crash during the push leaves a `Pending` record rather than
nothing. When the remote already holds the commit (`At`), the record is written
`Published` with no push. A remote that `Contains` the commit plus later
commits is published as today, but coverage shows the approval as stale, and D
holds the Task if the pull request merges at that later head.

- Test: `approving_a_correction_fast_forwards_the_existing_pull_request`
  (fake `gh` and a local bare remote; asserts one push of exactly the
  approved object ID, `pr_publication` Published at it, no new pull request,
  and status PR Created only after the answer reads the head at it),
  `a_remote_ahead_of_the_correction_is_refused_and_keeps_the_approval`.

### R5a. B approved while the open pull request still holds A

Human Review, approved at B, pull request OPEN at head A (A ≠ B). This is the
state after R4 and before R5 finishes: R5 not run yet, its push refused or
failed, a crash left `pr_publication` `Pending` or `Unknown`, or GitHub's
answer still reports the old head.

- Effect of any open-PR read (Check now, a PR helper's refresh, the link at
  the end of R5): G2 is not verified, so only the pull request's state is
  recorded. The Task stays in Human Review, the approval of B stays current
  and is not withdrawn, the approval of A stays in history, and nothing is
  cleaned up. The panel shows coverage as not covering the head (B approved,
  pull request at A), and `pr_error` names both.
- Leaves only by: R5 publishing B and a read observing H = B (to PR Created);
  Review again (R1, which withdraws B's approval into history) followed by
  Request Changes;
  a merge, which D holds because the merged head A is not the approved B; or
  a person's manual move, recorded as a Moved entry, after which coverage
  still reports B as not at the head.
- Test: `an_approved_correction_is_not_moved_to_pr_created_while_the_pr_holds_the_old_commit`
  (entries A approved, changes requested, B approved; fake `gh` OPEN at A;
  `pr_publication` absent, then Pending at B, then Published at B with the
  answer still at A: status Human Review each time; then the answer at B:
  PR Created, entries unchanged).

### D. Merged pull request: completion needs delivery evidence

This rule replaces the decision `complete_merged_task` makes today
(`queue/executor.rs`), where a merge heard in PR Created or Human Review
completes the Task without checking what was merged. It applies to every Task
in those two columns, whether or not it had a correction. A Task a person
already moved to Done keeps today's behavior, because the person made that
decision.

The check is one pure function, `merged_delivery(task, merged_head,
merge_commit)` in `domain/publication.rs`. It runs under the lifecycle lease
in `complete_merged_task`, on the Task as staged, before `terminalize_leased`,
and in the two command-side merged completions described below.
`apply_polled_pr_state` must pass it the `head_oid` of the same cached answer
it already uses for `merge_commit`. This is not wired today: the function
receives only the state and merge commit (see G2's routes), so the head is a
new parameter, filled at both call sites.

**Approval evidence.** The current decision of the current arrival is
Approved, with a recorded `approved_tip` A. This is
`HumanReviewRecord::current_decision` and `approved_tip`, and nothing new is
recorded.

**Merge evidence.** A single `gh pr view` answer (the existing poll fields)
reports `state` MERGED, a full `headRefOid` H, which is the pull request's last
head when it merged, and a full `mergeCommit` M. `parse_pr_status` keeps only
full object IDs, so an abbreviated ID is not evidence. The existing check that
the answer is still the cached one applies.

**Publication evidence.** H equals A. GitHub's head at merge time is the
branch SlashIt published to, so H = A shows both that A was published and
that A is the revision that merged. The local `pr_publication` record is
SlashIt's own account and must agree. When it needs reconciliation (`Pending`
or `Unknown`), the evidence is ambiguous. A Task delivered by Create PR has no
record, so its absence is not a contradiction.

**Squash and rebase merges.** M is a new commit on the base. After a squash
or rebase merge, neither A nor H is an ancestor of M, and M's tree differs
from A's tree whenever the base moved. D therefore never checks M's ancestry
or tree, and never needs the merge method: the same rule (H = A) covers merge
commits, squash and rebase. M is recorded on the reference
(`record_merge_commit`), as today, because dependent Tasks check their base
for it.

Verdicts:

| Verdict | When | Effect |
|---|---|---|
| Delivered | Approval evidence, merge evidence, H = A, and no unsettled publication | Today's completion: `record_first` records MERGED and M, then the Task moves to Done with checkout cleanup. All approval entries remain in history. |
| Unavailable (passing) | The answer has no full H or no full M, the poll failed, or the cache moved on | Nothing is written, and the reference stays OPEN in the record. The next poll or Check now asks again. This is the existing passing-refusal path. |
| Held (stable) | No current approval (a premature merge), an approval with no recorded tip, H ≠ A (a stale approval, or an approved correction that was never published), or an unsettled `pr_publication` | The existing stable-blocker latch (the `RepositoryUnresolved` arm): MERGED and M are recorded, so the poll stops asking. `error_message` names A, H and the reason. Status, Task Checkout, branch, `human_review.entries`, `pr_publication` and `activity` are unchanged apart from the PrMerged entry. Nothing is cleaned up. |

A held Task's evidence does not change later, because GitHub never moves a
merged head. So a hold is never reconsidered automatically. The Human resolves
it in one of these ways:

- Move the Task to Done. This is an explicit decision, recorded as a Moved
  entry, and the old and new approvals stay in history.
- Open a follow-up Task for the work the merge lacks, and keep or finish this
  one.
- For an unsettled `pr_publication`, use Publish again to reconcile it first.
  This changes the explanation, not the verdict.

R1 and R2 are refused once the merge is recorded (see the shared refusals).

**Command-side merged completion.** Two commands in `commands/pr.rs` also
complete a Task on a merged answer, and D governs both exactly as it governs
`complete_merged_task`:

- **`refresh_task_pr_state`** (Kanban PR-review apply and reply-sync). Today,
  on a MERGED answer from `refresh_pr_url`, it ends the live owner, records
  MERGED and the merge commit on the reference, and then calls
  `terminalize_leased` with Done.
- **The PR-link path** (`link_pr_to_task` and `link_pr_to_task_reserved`),
  when the linked pull request is already merged. Today it ends the live
  owner, records the link and the remote state, and then calls
  `terminalize_leased` with Done.

`terminalize_leased` checks project removal, republish, worker recovery,
review-fix residue and integration refusals. It checks no approval and no
merged head, so it cannot host D. Each command instead calls
`merged_delivery` itself, under the lifecycle lease it already holds, on the
Task as staged, with the `state`, full `headRefOid` and full `mergeCommit` of
the one `gh pr view` answer it acts on (`refresh_task_pr_state` already has
it as `PrStatus`; the link path reads the same answer, as G2 requires). The
verdict is decided before the command ends the live owner, before its
record write, and before `terminalize_leased`:

- **Delivered.** Today's sequence: the owner is ended, MERGED and M are
  recorded, and `terminalize_leased` moves the Task to Done with checkout
  cleanup.
- **Unavailable.** No completion write and no cleanup. No owner is ended,
  the reference's state is not set to MERGED, no merge commit is recorded,
  the status is unchanged, and `terminalize_leased` is not called. The link
  path still records the link the person asked for (`pr_url` and the
  reference), with no merged state. The next refresh or poll asks again.
- **Held.** Only the stable latch and the error, as in D's table: MERGED
  and M on the reference, and `error_message` naming A, H and the reason.
  No owner is ended, `terminalize_leased` is not called, and the status,
  Task Checkout, branch, `human_review.entries`, `pr_publication` and
  `activity` (apart from the PrMerged entry) are unchanged.

These paths complete a Task automatically on a merged answer, even though
they reach `terminalize_leased` with `Origin::User`. Explicit Human
completion (a person moving the Task to Done, including to resolve a hold)
stays a separate operation under its own existing authorization contract,
and D does not gate it.

Deterministic regression tests for these paths use a fake `gh` answer, as
the existing command tests do, and assert status, Task Checkout presence,
owner state, the reference's state and merge commit, and `error_message`:

- `refresh_task_pr_state_completes_a_merge_at_the_approved_commit` and
  `linking_a_pr_merged_at_the_approved_commit_completes_the_task` (H = A:
  Done, checkout cleaned up, M recorded).
- `refresh_task_pr_state_holds_a_merge_past_the_approved_commit` and
  `linking_a_pr_merged_at_another_head_holds_the_task` (H ≠ A: status and
  Task Checkout kept, owner not ended, MERGED and M recorded, the error
  names A and H, `terminalize_leased` not reached).
- `refresh_task_pr_state_holds_a_merge_with_no_current_approval` (a premature
  merge reached through this command).
- `refresh_task_pr_state_without_a_full_merged_head_changes_nothing` and
  `linking_a_merged_pr_without_a_full_head_records_only_the_link`
  (Unavailable: no MERGED state, no merge commit, no status change, no
  owner ended, no cleanup).

Tests (executor tests use a fake `gh` answer, as the existing merge tests do,
and `merged_delivery` has domain unit tests for each row):

- Premature merge: `a_merge_heard_while_a_correction_awaits_review_holds_the_task`
  (Human Review, new arrival, no decision) and
  `a_merge_heard_during_a_correction_run_is_held_when_the_run_reaches_review`
  (In Progress records nothing. Once the run reaches Human Review, the next
  poll holds it.)
- Stale approval: `a_merge_past_the_approved_commit_holds_the_task` (PR
  Created, A approved, H a descendant of A pushed outside SlashIt. This is the
  verified PR Created occurrence.)
- Approved but unpublished correction:
  `a_merge_of_the_old_head_after_the_correction_was_approved_holds_the_task`
  (B approved in Human Review, R5 not run, H = A, `pr_publication` names A or
  is absent).
- Successful squash merge: `a_squash_merge_of_the_approved_correction_completes_the_task`
  (H = B, M a commit with no ancestry to B, `pr_publication` Published at B.
  The Task reaches Done, M is recorded, and entries keep (A approved, changes
  requested, B approved).) Plus `parse_pr_status`'s
  `a_squash_merged_answer_reads_the_merged_head_beside_the_merge_commit`,
  which exists now and pins the answer shape.
- Unavailable delivery evidence:
  `a_merge_answer_without_a_full_head_changes_nothing_and_is_asked_again`,
  `a_merge_answer_without_a_merge_commit_changes_nothing_and_is_asked_again`,
  and `an_unsettled_publication_holds_a_merged_task` (ambiguous, so held).
- `an_approval_without_a_recorded_commit_holds_a_merged_task` (a legacy
  approval).
- `a_held_merge_keeps_the_checkout_branch_and_review_history` (the hold writes
  only the latch and the error).

### R6. Approve again on a published Task (unchanged refusal)

`approve_again` still refuses once a pull request exists. Its message names
the way forward: "Choose Review again to review the branch as it is now."

- Test: the existing renewal refusal test, with the message updated.

### R7. Publish to PR without approval (unchanged)

`publish_task_to_pr` publishes a confirmed commit and records no decision.
Coverage then reports the approval as not covering the pull request head.

- Test: existing `publishes_exactly_the_confirmed_commit_and_does_not_approve_the_task`.

## What is preserved and what becomes stale

| Data | R1 / R2 |
|---|---|
| `human_review.entries` (every decision, `sequence`, `arrival`, `approved_tip`) | Preserved. Never rewritten or removed. |
| The published approval | Stale. It stays in history and is not current. It is never renewed in place. |
| `human_review.pr_error` | Cleared. It described the previous delivery. |
| `qa_signoff` | Cleared, as every new arrival does. The earlier AI review outcomes stay in the activity timeline. |
| `pr_url`, `external_refs` | Preserved. The same pull request is the delivery target. |
| `pr_publication` | Preserved. A settled record is history, and an unsettled one refuses R1 and R2. |
| `pr_review_plan` | Preserved. Its `analyzed_head_oid` makes it stale on its own once the head moves (PR #302). |
| Branch, Task Checkout, `base_commit` | Preserved. The correction continues from them. |
| `activity` | Appended (Moved, ReadyForReview, run entries). |

A Held verdict under D preserves everything in this table except
`external_refs`, where the pull request reference records MERGED and M.
Nothing becomes stale through a hold: the approval was already not proof of
what merged, and `error_message` says so.

## Failure cases

| Case | Outcome |
|---|---|
| Publication record `Pending`/`Unknown` | R1 and R2 refused, nothing written. Publish again reconciles the record. |
| Restack, integration, cleanup or recovery unfinished | R1 and R2 refused by `queue_entry_refusal`. |
| Board write fails | Nothing changed (`WriteError::NotPublished`), or the board adopts the published file (`PublishedNotDurable`) and the reply says so. R2 is one write, so its feedback and the Queue move land together or not at all. |
| PR status refresh during the correction | G1: state recorded, Task and run untouched. |
| B approved, pull request still at A (R5 not run, refused, crashed, or GitHub not yet reporting B) | G2 not verified: state recorded, Task stays in Human Review with B's approval current. Check now or a retried R5 moves it once the head reads B and the record is Published at B. |
| Pull request head reported abbreviated or missing | G2 not verified, nothing moved. |
| `pr_publication` absent or naming another commit while the head reads B (B pushed outside SlashIt) | G2 not verified. Approve & Update PR finds the remote `At` B, records Published with no push, and the next read moves the Task. |
| PR merged during the correction run | Nothing is recorded while the Task is outside the PR lifecycle, as today. When the run reaches Human Review, D holds it (no current approval). At R5 the merged pull request is found and the push is refused ("not open"). No second pull request is opened. |
| PR merged while the correction waits in Human Review | D holds it: no current approval, or H ≠ the approved commit. |
| PR merged after the correction was approved but before R5 | D holds it: H is the old head, not the approved commit. |
| PR merged at a head past the approval (commits pushed outside SlashIt) | D holds it: H ≠ A. |
| PR squash- or rebase-merged at the approved commit | D completes it. The merge commit's ancestry is not checked. |
| Merge answer without a full head or merge commit, or the poll fails | D finds the evidence unavailable. Nothing is written, and the poll asks again. |
| Merged with `pr_publication` Pending or Unknown | D holds it as ambiguous, and Publish again reconciles the record. |
| PR closed during the correction (run or Human Review) | G1: the CLOSED state is recorded, and the owner, status, Run and Task Checkout are kept. The correction is not ended. |
| PR closed | R5 refused with the existing "not open" message, and the approval stays. |
| Remote has commits the correction lacks | R5 push refused as non-fast-forward, nothing forced, approval kept, `pr_error` recorded. The person integrates locally, then uses R1, then R4 and R5. |
| Branch moved after the commit was shown | R4 refused (`shown_tip_is_current`). |
| Correction run fails | Error, as any run. The change request stays pending (`withdraw_approval` leaves feedback alone), so a retry gets it again. |
| Delivery fails after approval | Existing behavior: the approval stands, `pr_error` is recorded, and Create PR retries. |
| Agent or PR helper owns the Task | A live run refuses. A PR helper is ended under the lease, as any move ends it. |

## Implementation Tasks

G1, G2 and D are safe alone and must land before R1 and R2. Otherwise a
reopened review, or an approved correction the pull request does not yet
hold, could be pulled to PR Created with no delivery of its approved head.

1. **Move a published Task to PR Created only on open delivery evidence
   (G1, G2).** `open_delivery` in `domain/publication.rs`; `refresh_task_pr_state`
   and `link_pr_to_task` in `commands/pr.rs` pass it the state and full head
   of one answer and, for a Task already linked, record only the state and
   end no owner unless it is verified. In `refresh_task_pr_state` (and its
   `pr_refresh_changes` test), a CLOSED answer for a Task in an active
   post-PR correction records only the state and ends no owner; CLOSED
   outside a correction keeps today's move. The OPEN move in
   `refresh_task_pr_state` and `pr_refresh_changes` is gated by
   `open_delivery` on the `head_oid` of the same answer. Add a `head_oid`
   parameter to `apply_polled_pr_state`, filled from the same `PrStatus` at
   both call sites (`refresh_pr_status` for Check now, `poll_pull_requests`
   for the background poll), and pass it to `open_delivery` for an OPEN
   answer on a published Task; D uses the same parameter. Tests from G1
   (including
   `a_closed_pr_observed_during_a_correction_records_the_state_and_keeps_the_run`),
   G2 and R5a, plus deterministic regression tests with a fake `gh` answer
   for each route (Check now through `refresh_pr_status`, the poll through
   `apply_polled_pr_state`, and `refresh_task_pr_state`):
   `check_now_at_the_approved_head_moves_a_published_correction_to_pr_created`,
   `check_now_without_a_head_keeps_the_correction_in_human_review`,
   `check_now_at_another_head_keeps_the_correction_in_human_review`, and
   `refresh_task_pr_state_at_another_head_does_not_move_a_published_correction`.
   Each refusal case asserts status Human Review, B's approval current, no
   owner ended, the Task Checkout present, and only the reference's state
   recorded.
2. **Complete a merged Task only on delivery evidence (D).**
   `merged_delivery` in `domain/publication.rs`, the head passed through
   `apply_polled_pr_state`, and the Held verdict added to the stable-blocker
   latch in `complete_merged_task`, `queue/executor.rs`. The same check, with
   the same three verdicts, in the command-side merged completions of
   `commands/pr.rs`: `refresh_task_pr_state` and the PR-link path for an
   already-merged pull request each call `merged_delivery` under the
   lifecycle lease before ending the owner, recording MERGED or the merge
   commit, or calling `terminalize_leased`; Unavailable writes no completion
   state and cleans nothing up, and Held writes only the latch and the
   error. Explicit Human completion keeps its own contract. Tests from D,
   including its command-side regression tests. This
   changes behavior for Tasks without corrections too: a pull request merged
   at a head nobody approved in SlashIt is held, not completed. Say so in
   `product-model.md`.
3. **Reopen review (R1).** `reopen_task_review`, the shared refusal predicate,
   the `record_move` rule for a move from PR Created to Human Review, and the
   R6 message. Tests from R1 and R6. Update the Approval coverage section of
   `product-model.md`.
4. **Request changes on a published Task (R2), and record R5's publication.**
   Widen `request_changes` in a single write, and have
   `publish_approved_to_existing_pr` write `pr_publication` through the
   existing record functions. Tests from R2, R3's prompt test, R4, and R5's
   backend tests.
5. **Human Review UI and acceptance.** Review again and Request changes in the
   drawer of a PR Created Task, the Approve & Update PR label, and earlier
   approvals shown as history. A desktop acceptance journey covers approve,
   PR, request changes, run, approve the new commit, update the PR, and the
   coverage shown as current at the new head with the old approval in
   history.
