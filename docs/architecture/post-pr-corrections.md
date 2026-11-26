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
arrival, plus two guards that keep pull request status reads from cutting a
correction short. Everything after the entry is the existing Human Review
loop. `a_post_pr_correction_needs_no_new_review_record_field` shows that the
record already expresses the whole sequence.

Approve again keeps its #355 rule: once a pull request exists, an approval is
not renewed within its arrival. A later commit is reviewed in a new arrival
instead. The delivered approval therefore stays final at the commit it
covered, and a new approval is a separate history entry.

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
  its live owner. For a published Task in Queue, In Progress or AI Review, or
  in Human Review with no current approval, they record only the pull
  request's state: no status change, and no owner ended. A Human Review Task
  with a current approval (the delivery step) is still moved, as today.
- Test: `a_refresh_during_a_correction_run_records_the_state_and_keeps_the_run`,
  `a_refresh_of_a_reopened_review_does_not_move_it_to_pr_created`,
  `the_prompt_of_a_published_tasks_correction_carries_the_feedback`.

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
the remote does not already contain it, then the existing link moves the Task
to PR Created. Approval coverage (#355) shows the approval as current only
once the poll reads the pull request head at that commit.

- Test: `approving_a_correction_fast_forwards_the_existing_pull_request`
  (fake `gh` and a local bare remote; asserts one push of exactly the
  approved object ID, no new pull request, status PR Created),
  `a_remote_ahead_of_the_correction_is_refused_and_keeps_the_approval`.

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

## Failure cases

| Case | Outcome |
|---|---|
| Publication record `Pending`/`Unknown` | R1 and R2 refused, nothing written. Publish again reconciles the record. |
| Restack, integration, cleanup or recovery unfinished | R1 and R2 refused by `queue_entry_refusal`. |
| Board write fails | Nothing changed (`WriteError::NotPublished`), or the board adopts the published file (`PublishedNotDurable`) and the reply says so. R2 is one write, so its feedback and the Queue move land together or not at all. |
| PR status refresh during the correction | G1: state recorded, Task and run untouched. |
| PR merged during the correction run | The poll records it and does not complete the Task (it is not in the PR lifecycle). At R5 the merged pull request is found and the push is refused ("not open"). The person moves the Task to Done or opens a follow-up Task. No second pull request is opened. |
| PR merged while the correction waits in Human Review | New guard G2: `in_pr_lifecycle` completes a Human Review Task on merge only when its current arrival is approved. Otherwise the merge is recorded with an error message saying the correction is not in the merge, and the Task stays. |
| PR closed | R5 refused with the existing "not open" message, and the approval stays. |
| Remote has commits the correction lacks | R5 push refused as non-fast-forward, nothing forced, approval kept, `pr_error` recorded. The person integrates locally, then uses R1, then R4 and R5. |
| Branch moved after the commit was shown | R4 refused (`shown_tip_is_current`). |
| Correction run fails | Error, as any run. The change request stays pending (`withdraw_approval` leaves feedback alone), so a retry gets it again. |
| Delivery fails after approval | Existing behavior: the approval stands, `pr_error` is recorded, and Create PR retries. |
| Agent or PR helper owns the Task | A live run refuses. A PR helper is ended under the lease, as any move ends it. |

## Implementation Tasks

G1 and G2 are safe alone and must land before R1 and R2. Otherwise a
reopened review could be pulled back to PR Created with no approval of its
head.

1. **Keep pull request status reads from moving or stopping a correction (G1).**
   `refresh_task_pr_state` and `link_pr_to_task`, `commands/pr.rs`. Tests:
   `a_refresh_during_a_correction_run_records_the_state_and_keeps_the_run`,
   `a_refresh_of_a_reopened_review_does_not_move_it_to_pr_created`.
2. **Complete a merged Human Review Task only when it is approved (G2).**
   `in_pr_lifecycle` and `complete_merged_task`, `queue/executor.rs`. Test:
   `a_merge_heard_while_a_correction_awaits_review_does_not_complete_the_task`.
3. **Reopen review (R1).** `reopen_task_review`, the shared refusal predicate,
   the `record_move` rule for a move from PR Created to Human Review, and the
   R6 message. Tests from R1 and R6. Update the Approval coverage section of
   `product-model.md`.
4. **Request changes on a published Task (R2).** Widen `request_changes` in a
   single write. Tests from R2, R3's prompt test, R4, and R5's backend tests.
5. **Human Review UI and acceptance.** Review again and Request changes in the
   drawer of a PR Created Task, the Approve & Update PR label, and earlier
   approvals shown as history. A desktop acceptance journey covers approve,
   PR, request changes, run, approve the new commit, update the PR, and the
   coverage shown as current at the new head with the old approval in
   history.
