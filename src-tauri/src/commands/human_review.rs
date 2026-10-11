//! Human Review decisions: approving a task's changes, requesting changes to
//! them, and delivering approved changes as a pull request.
//!
//! Approval and delivery are separate on purpose. Approving records what a
//! person decided, durably, before anything talks to a forge; opening the pull
//! request is a second step whose failure is recorded next to the approval
//! and never undoes it. See [`crate::domain::task::HumanReviewRecord`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use uuid::Uuid;

use crate::commands::pr::{create_pr_for_approved_task, pr_availability, PrAvailability};
use crate::commands::task::renumber_column;
use crate::domain::task::HumanReviewDecision;
use crate::domain::{Task, TaskStatus};

/// The longest feedback a change request may carry, in characters. It goes
/// into the next run's prompt whole.
pub const MAX_FEEDBACK_CHARS: usize = 20_000;

/// The tasks whose pull request is being opened right now.
///
/// Process memory only, and deliberately so: an attempt that is running is a
/// fact about this process, and it ends with it. While a task is in here it
/// is waiting on SlashIt, not on a person, even though its record may still
/// carry the failure of an earlier attempt (see
/// [`crate::domain::Task::needs_you`]).
#[derive(Clone, Default)]
pub struct DeliveriesInFlight(Arc<Mutex<HashMap<Uuid, u32>>>);

impl DeliveriesInFlight {
    pub fn contains(&self, task_id: Uuid) -> bool {
        self.lock().contains_key(&task_id)
    }

    /// Mark `task_id` as being delivered until the returned guard is dropped.
    /// Counted, so two overlapping attempts do not end each other's mark.
    fn start(&self, task_id: Uuid) -> DeliveryMark {
        *self.lock().entry(task_id).or_default() += 1;
        DeliveryMark { deliveries: self.clone(), task_id }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, u32>> {
        // The map is always left consistent, so a panic elsewhere while it
        // was held is no reason to stop answering.
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

struct DeliveryMark {
    deliveries: DeliveriesInFlight,
    task_id: Uuid,
}

impl Drop for DeliveryMark {
    fn drop(&mut self) {
        let mut running = self.deliveries.lock();
        if let Some(count) = running.get_mut(&self.task_id) {
            *count -= 1;
            if *count == 0 {
                running.remove(&self.task_id);
            }
        }
    }
}

/// What approving (or retrying delivery of) a task did.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalOutcome {
    /// The task as recorded once everything below had finished.
    pub task: Task,
    /// What happened to the pull request, when one was asked for.
    pub pr: Option<PrDelivery>,
}

/// The pull request side of an approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrDelivery {
    Created { url: String },
    /// The attempt failed. The approval stands, and the reason is also
    /// recorded on the task for as long as no pull request exists.
    Failed { reason: String },
    /// SlashIt cannot open a pull request for this project, so none was
    /// attempted: nothing was pushed.
    Unavailable { reason: String },
}

/// Approve the changes of a task in Human Review, and optionally open a pull
/// request for them.
///
/// `Err` means the approval was not recorded. Once it is recorded, a pull
/// request that cannot be opened is reported in [`ApprovalOutcome::pr`], not
/// as an error, because the approval happened.
#[tauri::command]
pub async fn approve_task(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    create_pr: bool,
) -> Result<ApprovalOutcome, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    approve(&state, task_id, create_pr).await
}

/// Open the pull request for a task whose changes are already approved,
/// without approving them again.
#[tauri::command]
pub async fn create_approved_task_pr(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<ApprovalOutcome, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    retry_delivery(&state, task_id).await
}

/// Finish approved changes without a pull request: for a project SlashIt
/// cannot open one for, or, with `without_pull_request`, for one it can when
/// a person chooses to keep the work local.
///
/// Nothing is merged, pushed or opened: the Task Checkout is removed and the
/// task becomes Done, and its branch keeps the work. Publishing is a later,
/// separate decision.
#[tauri::command]
pub async fn complete_task_locally(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    without_pull_request: Option<bool>,
) -> Result<Task, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    complete_locally(&state, task_id, without_pull_request.unwrap_or(false)).await
}

/// Record a change request for a task in Human Review and queue it to run
/// again with that feedback.
///
/// One durable write does both, so there is no outcome in which the feedback
/// is saved and the task silently stays put, or the task runs again without
/// the feedback. `Err` means neither happened.
#[tauri::command]
pub async fn request_task_changes(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    feedback: String,
) -> Result<Task, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    request_changes(&state, task_id, &feedback).await
}

pub(crate) async fn approve(
    state: &crate::AppState,
    task_id: Uuid,
    create_pr: bool,
) -> Result<ApprovalOutcome, String> {
    let task = record_approval(state, task_id).await?;
    if !create_pr {
        return Ok(ApprovalOutcome { task, pr: None });
    }
    let pr = deliver(state, task_id).await;
    Ok(ApprovalOutcome { task: current(state, task_id).await.unwrap_or(task), pr: Some(pr) })
}

pub(crate) async fn retry_delivery(
    state: &crate::AppState,
    task_id: Uuid,
) -> Result<ApprovalOutcome, String> {
    let task = current(state, task_id).await.ok_or("Task not found")?;
    if task.status != TaskStatus::HumanReview || !task.human_review.is_approved() {
        return Err(
            "Only a task in Human Review whose changes are approved can have its pull request \
             opened from here."
                .to_string(),
        );
    }
    let pr = deliver(state, task_id).await;
    Ok(ApprovalOutcome { task: current(state, task_id).await.unwrap_or(task), pr: Some(pr) })
}

/// Complete an approved task locally: the one lifecycle terminalization, asked
/// for by a person, with the Human Review conditions checked under the task's
/// lease.
///
/// Every refusal leaves the task exactly as it was. The checkout is removed
/// first and `Done` is committed, with the activity entry that says the work
/// was completed locally, in the same write that clears it, so no state
/// exists in which the task is Done while its checkout remains, or the entry
/// is recorded while the checkout was kept. Asked again once it is done, it
/// answers with the task and does nothing. A crash during the removal leaves
/// the task in Human Review with `cleanup_in_flight` set; asking again is the
/// explicit way out that `Origin::User` allows, and it removes what is left
/// and commits once.
///
/// The branch is never touched, and nothing here pushes or opens a pull
/// request. A project that can take one is refused unless a person said, in
/// this very call, that they want the work to stay local
/// (`without_pull_request`): the default delivery of such a project is still
/// its pull request, and nothing here changes it. A task that has a pull
/// request, or is getting one, is refused whatever was said.
pub(crate) async fn complete_locally(
    state: &crate::AppState,
    task_id: Uuid,
    without_pull_request: bool,
) -> Result<Task, String> {
    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    let task = current(state, task_id).await.ok_or("Task not found")?;
    if task.completed_locally() {
        return Ok(task);
    }
    check_reviewable(&task, "completed locally")?;
    if !task.human_review.is_approved() {
        return Err(
            "Approve the changes before completing them locally; this task has no approval."
                .to_string(),
        );
    }
    let has_pr = task.pr_url.is_some() || task.external_refs.iter().any(|r| r.is_pr());
    if has_pr || state.task.deliveries.contains(task_id) {
        return Err(
            "This task has a pull request, or one is being opened, so it is delivered through \
             that pull request, not completed locally."
                .to_string(),
        );
    }
    // Asked for by name, finishing locally does not depend on whether a pull
    // request could be opened.
    if !without_pull_request {
        match pr_availability(state, task_id).await {
            Ok(PrAvailability::Unavailable { .. }) => {}
            Ok(PrAvailability::Available) => {
                return Err(
                    "SlashIt can open a pull request for this project, so these changes are \
                     delivered with Create PR. To keep them local instead, choose to finish \
                     them locally without a pull request."
                        .to_string(),
                )
            }
            Err(reason) => {
                return Err(format!(
                    "SlashIt could not tell whether this project can take a pull request, so the \
                     task was not completed: {reason}"
                ))
            }
        }
    }

    let record = |staged: &mut HashMap<Uuid, Task>| {
        if let Some(t) = staged.get_mut(&task_id) {
            t.overall_progress = 100;
            t.phase = crate::domain::task::TaskPhase::Complete;
            t.record_activity(crate::domain::task::ActivityKind::CompletedLocally);
        }
    };
    let mut request = crate::lifecycle::TerminalizeRequest::new(TaskStatus::Done);
    request.on_success = Some(&record);
    crate::lifecycle::terminalize_leased(
        crate::commands::task::terminalize_ctx(state),
        task_id,
        crate::lifecycle::Origin::User,
        request,
    )
    .await
    .map_err(|refusal| refusal.to_string())
}

async fn record_approval(state: &crate::AppState, task_id: Uuid) -> Result<Task, String> {
    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    let task = current(state, task_id).await.ok_or("Task not found")?;
    check_reviewable(&task, "approved")?;
    match task.human_review.current_decision().map(|e| e.decision) {
        // Asked twice, recorded once.
        Some(HumanReviewDecision::Approved) => return Ok(task),
        Some(HumanReviewDecision::ChangesRequested) => {
            return Err(
                "Changes were already requested for these changes, so they cannot also be \
                 approved."
                    .to_string(),
            )
        }
        None => {}
    }

    let now = chrono::Utc::now();
    // The exact commit being approved, read while the lease is held so that
    // what is recorded is what the person decided on. Integrating later
    // merges this commit and nothing newer.
    let tip = crate::commands::integration::tip_to_approve(state, &task).await;
    let amend = move |staged: &mut HashMap<Uuid, Task>| {
        if let Some(t) = staged.get_mut(&task_id) {
            if t.status == TaskStatus::HumanReview && t.human_review.current_decision().is_none() {
                t.human_review.approve(tip.clone(), now);
                t.human_review.pr_error = None;
            }
        }
    };
    let task = crate::lifecycle::commit_task(&state.task.tasks, &state.storage, task_id, &amend)
        .await
        .map_err(|e| format!("The approval was not saved: {e}"))?
        .ok_or("Task not found")?;
    if !task.human_review.is_approved() {
        return Err("The task changed while it was being approved, so nothing was recorded.".to_string());
    }
    Ok(task)
}

async fn request_changes(
    state: &crate::AppState,
    task_id: Uuid,
    feedback: &str,
) -> Result<Task, String> {
    let feedback = feedback.trim().to_string();
    if feedback.is_empty() {
        return Err("Describe the changes you want before requesting them.".to_string());
    }
    if feedback.chars().count() > MAX_FEEDBACK_CHARS {
        return Err(format!(
            "The feedback is longer than {MAX_FEEDBACK_CHARS} characters. Shorten it and try again."
        ));
    }

    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    let task = current(state, task_id).await.ok_or("Task not found")?;
    check_reviewable(&task, "sent back with changes")?;
    if task.human_review.is_approved() {
        return Err(
            "These changes are already approved, so changes cannot be requested for them."
                .to_string(),
        );
    }

    // The task leaves Human Review for the queue, so whatever owns it now (a
    // pull request helper, say) is ended first, exactly as a status move does.
    let running = state
        .executor
        .get()
        .map(|e| e.as_ref() as &dyn crate::lifecycle::ExecutionOwnership);
    crate::lifecycle::end_active_ownership(running, task_id).await?;

    let project_id = task.project_id;
    let now = chrono::Utc::now();
    let amend = move |staged: &mut HashMap<Uuid, Task>| {
        let Some(t) = staged.get_mut(&task_id) else { return };
        if t.status != TaskStatus::HumanReview || t.human_review.is_approved() {
            return;
        }
        t.human_review.push(HumanReviewDecision::ChangesRequested, Some(feedback.clone()), now);
        t.human_review.pr_error = None;
        // The AI review described the changes being sent back; the next run
        // gets its own.
        t.qa_signoff = None;
        t.status = TaskStatus::Queue;
        // Worktree and branch are kept: the next run continues from them.
        t.reset_execution_state();
        renumber_column(staged, project_id, task_id, &TaskStatus::Queue, i32::MAX);
    };
    let task = crate::lifecycle::commit_task(&state.task.tasks, &state.storage, task_id, &amend)
        .await
        .map_err(|e| {
            format!("Your feedback was not saved and the task was not queued to run again: {e}")
        })?
        .ok_or("Task not found")?;
    let recorded = task.status == TaskStatus::Queue
        && task.human_review.entries.last().is_some_and(|e| {
            e.decision == HumanReviewDecision::ChangesRequested && e.decided_at == now
        });
    if !recorded {
        return Err(
            "The task changed while the feedback was being saved, so nothing was recorded and \
             the task was not queued."
                .to_string(),
        );
    }
    Ok(task)
}

/// Open the pull request for approved changes, and record how that went.
///
/// The task counts as being delivered from the first check until its
/// outcome is recorded, so it never reads as needing the user while this
/// runs, nor as settled before the outcome is on the record.
async fn deliver(state: &crate::AppState, task_id: Uuid) -> PrDelivery {
    let _in_flight = state.task.deliveries.start(task_id);
    match pr_availability(state, task_id).await {
        Ok(PrAvailability::Available) => {}
        Ok(PrAvailability::Unavailable { reason }) => return PrDelivery::Unavailable { reason },
        Err(reason) => return record_pr_failure(state, task_id, reason).await,
    }
    match create_pr_for_approved_task(state, task_id).await {
        Ok(url) => {
            // Best effort: the pull request is linked either way, and a stale
            // failure next to it is only shown while no PR is recorded.
            let clear = move |staged: &mut HashMap<Uuid, Task>| {
                if let Some(t) = staged.get_mut(&task_id) {
                    t.human_review.pr_error = None;
                }
            };
            let _ = crate::lifecycle::record(&state.task.tasks, &state.storage, task_id, &clear).await;
            PrDelivery::Created { url }
        }
        Err(reason) => record_pr_failure(state, task_id, reason).await,
    }
}

/// Keep why the pull request could not be opened on the task, so it is still
/// explained after a restart. The failure is reported whether or not that
/// write succeeds.
///
/// Kept only while the task is still in Human Review with its changes
/// approved. Delivery runs outside the lease, so the task may have gone back
/// to work meanwhile, which withdraws the approval; a failure recorded then
/// would describe an approval that is no longer current, and would show as a
/// failed delivery if the task were moved back by hand.
async fn record_pr_failure(state: &crate::AppState, task_id: Uuid, reason: String) -> PrDelivery {
    let note = reason.clone();
    let amend = move |staged: &mut HashMap<Uuid, Task>| {
        if let Some(t) = staged.get_mut(&task_id) {
            if t.status == TaskStatus::HumanReview && t.human_review.is_approved() {
                t.human_review.pr_error = Some(note.clone());
                t.record_activity(crate::domain::task::ActivityKind::DeliveryFailed {
                    reason: note.clone(),
                });
            }
        }
    };
    let _ = crate::lifecycle::record(&state.task.tasks, &state.storage, task_id, &amend).await;
    PrDelivery::Failed { reason }
}

fn check_reviewable(task: &Task, what: &str) -> Result<(), String> {
    if task.status != TaskStatus::HumanReview {
        return Err(format!(
            "Only a task in Human Review can be {what}; this one is not in Human Review."
        ));
    }
    Ok(())
}

async fn current(state: &crate::AppState, task_id: Uuid) -> Option<Task> {
    state.task.tasks.read().await.get(&task_id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task::{QaSignoff, QaStatus};
    use crate::test_helpers::create_test_task_full;

    async fn test_state() -> (tempfile::TempDir, crate::AppState) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let (state, _report) = crate::app_core::build_state_with_paths(paths)
            .await
            .expect("state should build under a fresh tempdir");
        (tmp, state)
    }

    const DESCRIPTION: &str = "Count the words in a file.\n\nKeep the CLI flags unchanged.";

    /// A task a run has just carried into Human Review, with an AI review
    /// and a branch and checkout to continue from.
    async fn seed_in_review(state: &crate::AppState) -> (Uuid, Uuid) {
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("under review", project_id, TaskStatus::HumanReview, 0);
        task.description = Some(DESCRIPTION.to_string());
        task.branch_name = Some("task-under-review".to_string());
        task.worktree_path = Some("/tmp/checkout-under-review".to_string());
        task.overall_progress = 90;
        task.qa_signoff = Some(QaSignoff {
            status: QaStatus::Approved,
            issues_found: Vec::new(),
            timestamp: chrono::Utc::now(),
            session_id: Uuid::new_v4(),
        });
        task.human_review.record_arrival();
        let task_id = task.id;
        state.task.tasks.write().await.insert(task_id, task.clone());
        state.storage.save_project_tasks(project_id, &[task]).expect("seed the board");
        (project_id, task_id)
    }

    fn on_disk(state: &crate::AppState, project_id: Uuid, task_id: Uuid) -> Task {
        state
            .storage
            .load_project_tasks(project_id)
            .expect("read the board back")
            .into_iter()
            .find(|t| t.id == task_id)
            .expect("the task is on disk")
    }

    /// What the executor does when a run carries the task back into Human
    /// Review, for tests that do not run one.
    async fn arrive_again(state: &crate::AppState, task_id: Uuid) {
        let amend = move |staged: &mut HashMap<Uuid, Task>| {
            let t = staged.get_mut(&task_id).unwrap();
            t.status = TaskStatus::HumanReview;
            t.human_review.record_arrival();
            let arrival = t.human_review.arrivals;
            t.record_activity(crate::domain::task::ActivityKind::ReadyForReview { arrival });
        };
        crate::lifecycle::record(&state.task.tasks, &state.storage, task_id, &amend).await.unwrap();
    }

    /// Two review cycles, then an approval whose pull request fails twice:
    /// the timeline read from the task file shows each decision once, in
    /// order, and the failed deliveries as failures, never as approvals.
    #[tokio::test]
    async fn review_cycles_and_delivery_retries_read_truthfully_on_the_timeline() {
        let (_tmp, state) = test_state().await;
        let (project_id, task_id) = seed_in_review(&state).await;

        request_changes(&state, task_id, "Also count lines.").await.expect("first request");
        arrive_again(&state, task_id).await;
        request_changes(&state, task_id, "And bytes.").await.expect("second request");
        arrive_again(&state, task_id).await;
        approve(&state, task_id, false).await.expect("approved");
        approve(&state, task_id, false).await.expect("approving again changes nothing");
        record_pr_failure(&state, task_id, "gh: HTTP 422".to_string()).await;
        record_pr_failure(&state, task_id, "gh: HTTP 422".to_string()).await;

        let task = on_disk(&state, project_id, task_id);
        let timeline = task_timeline(&task);
        assert_eq!(
            timeline,
            [
                "Task created",
                "You requested changes: Also count lines.",
                "Ready for your review",
                "You requested changes: And bytes.",
                "Ready for your review",
                "You approved the changes",
                "Pull request not created: gh: HTTP 422",
                "Pull request not created: gh: HTTP 422",
            ]
        );
        assert!(task.needs_you(false).is_some(), "current state still comes from the record");
    }

    /// The timeline the drawer shows, one line per row, from the record.
    fn task_timeline(task: &Task) -> Vec<String> {
        let decisions = task.human_review.entries.iter().map(|e| slashit_activity::Decision {
            sequence: e.sequence,
            at: e.decided_at,
            approved: e.decision == HumanReviewDecision::Approved,
            feedback: e.feedback.as_deref(),
        });
        slashit_activity::timeline(task.created_at, decisions, &task.activity)
            .iter()
            .map(|item| match item.detail() {
                Some(detail) => format!("{}: {detail}", item.title()),
                None => item.title(),
            })
            .collect()
    }

    #[tokio::test]
    async fn requesting_changes_records_the_feedback_apart_from_the_description_and_requeues() {
        let (_tmp, state) = test_state().await;
        let (project_id, task_id) = seed_in_review(&state).await;

        let task = request_changes(&state, task_id, "  Also count lines.\n").await.expect("recorded");

        assert_eq!(task.status, TaskStatus::Queue);
        assert_eq!(task.description.as_deref(), Some(DESCRIPTION), "the description is untouched");
        assert_eq!(task.human_review.pending_feedback(), vec!["Also count lines."]);
        let entry = task.human_review.entries.last().unwrap();
        assert_eq!(entry.decision, HumanReviewDecision::ChangesRequested);
        assert_eq!((entry.sequence, entry.arrival), (1, 1));
        assert!(task.qa_signoff.is_none(), "the old AI review is about the changes sent back");
        assert_eq!(task.overall_progress, 0, "the execution state is reset for the rerun");
        assert_eq!(task.branch_name.as_deref(), Some("task-under-review"), "the branch is kept");
        assert!(task.worktree_path.is_some(), "the checkout is kept for the next run");

        let persisted = on_disk(&state, project_id, task_id);
        assert_eq!(persisted.status, TaskStatus::Queue);
        assert_eq!(persisted.description.as_deref(), Some(DESCRIPTION));
        assert_eq!(persisted.human_review, task.human_review);

        // The next run's prompt carries it.
        let prompt = crate::queue::prompt::build_task_prompt(&persisted, None);
        assert!(prompt.contains("Also count lines."));
    }

    /// Requesting changes is a decision, not new work, so it is recorded on
    /// a critically low disk. The rerun it asks for is new coding work, and
    /// waits in Queue like any other until space returns.
    #[tokio::test]
    async fn changes_can_be_requested_on_a_critical_disk_and_the_rerun_waits() {
        use crate::domain::storage_usage::GIB;
        use tauri::Manager;
        let disk = crate::test_helpers::FakeDisk::with_available(10 * GIB);
        let (_tmp, mut state) = test_state().await;
        state.start_guard = disk.guard();
        let (project_id, task_id) = seed_in_review(&state).await;

        let task = request_changes(&state, task_id, "Also count lines.").await.expect("recorded while paused");
        assert_eq!(task.status, TaskStatus::Queue);
        assert_eq!(on_disk(&state, project_id, task_id).status, TaskStatus::Queue);

        let app = tauri::test::mock_app();
        app.manage(state);
        let waiting = crate::commands::queue::promote_next_task(app.state()).await;
        assert!(waiting.as_ref().is_err_and(|m| m.starts_with("New work paused")), "{waiting:?}");
        let state: tauri::State<'_, crate::AppState> = app.state();
        assert_eq!(on_disk(&state, project_id, task_id).status, TaskStatus::Queue);

        disk.set_available(300 * GIB);
        let promoted = crate::commands::queue::promote_next_task(app.state()).await.expect("promoted");
        assert_eq!(promoted, Some(task_id.to_string()));
    }

    #[tokio::test]
    async fn blank_feedback_is_refused_and_nothing_changes() {
        let (_tmp, state) = test_state().await;
        let (project_id, task_id) = seed_in_review(&state).await;
        let before = on_disk(&state, project_id, task_id);

        for blank in ["", "   ", "\n\t"] {
            let refused = request_changes(&state, task_id, blank).await.expect_err("blank");
            assert!(refused.contains("Describe the changes"), "{refused}");
        }
        let too_long = "x".repeat(MAX_FEEDBACK_CHARS + 1);
        assert!(request_changes(&state, task_id, &too_long).await.is_err());

        let after = on_disk(&state, project_id, task_id);
        assert_eq!(after.status, TaskStatus::HumanReview);
        assert_eq!(after.human_review, before.human_review);
    }

    #[tokio::test]
    async fn approved_changes_cannot_also_be_sent_back_and_vice_versa() {
        let (_tmp, state) = test_state().await;
        let (_project_id, approved) = seed_in_review(&state).await;
        approve(&state, approved, false).await.expect("approved");
        let refused = request_changes(&state, approved, "change it").await.expect_err("approved");
        assert!(refused.contains("already approved"), "{refused}");
        let task = current(&state, approved).await.unwrap();
        assert_eq!(task.status, TaskStatus::HumanReview);
        assert_eq!(task.human_review.entries.len(), 1);

        let (_project_id, sent_back) = seed_in_review(&state).await;
        request_changes(&state, sent_back, "change it").await.expect("recorded");
        let refused = approve(&state, sent_back, false).await.expect_err("queued now");
        assert!(refused.contains("Human Review"), "{refused}");
    }

    /// Sending approved changes back to work retires the approval, through
    /// every front door that does it, so a manual move back into Human
    /// Review cannot open a pull request for commits nobody reviewed.
    #[tokio::test]
    async fn an_approval_does_not_survive_the_task_going_back_to_work() {
        use tauri::Manager;
        let (_tmp, state) = test_state().await;
        let (_p, moved) = seed_in_review(&state).await;
        let (_p, queued) = seed_in_review(&state).await;
        approve(&state, moved, false).await.expect("approved");
        approve(&state, queued, false).await.expect("approved");
        let app = tauri::test::mock_app();
        app.manage(state);

        for status in [TaskStatus::Queue, TaskStatus::HumanReview] {
            crate::commands::task::update_task_status(app.state(), moved.to_string(), status, None)
                .await
                .expect("moved");
        }
        crate::commands::queue::add_to_queue(app.state(), queued.to_string()).await.expect("queued");
        crate::commands::task::update_task_status(app.state(), queued.to_string(), TaskStatus::HumanReview, None)
            .await
            .expect("moved back");

        let state: tauri::State<'_, crate::AppState> = app.state();
        for id in [moved, queued] {
            let task = current(&state, id).await.unwrap();
            assert_eq!(task.status, TaskStatus::HumanReview);
            assert!(!task.human_review.is_approved(), "the approval was of other changes");
            assert!(task.human_review.current_decision().is_none());
            assert_eq!(task.human_review.entries.len(), 1, "the history keeps it");
            assert!(retry_delivery(&state, id).await.is_err());
            let refused = crate::commands::pr::create_pr_for_approved_task(&state, id)
                .await
                .expect_err("not approved any more");
            assert!(refused.contains("no longer"), "{refused}");
        }
    }

    #[tokio::test]
    async fn changes_already_requested_in_this_review_cannot_also_be_approved() {
        let (_tmp, state) = test_state().await;
        let (_p, task_id) = seed_in_review(&state).await;
        request_changes(&state, task_id, "fix it").await.expect("recorded");
        // Put back in Human Review without a run, as a manual move would.
        state.task.tasks.write().await.get_mut(&task_id).unwrap().status = TaskStatus::HumanReview;
        let refused = approve(&state, task_id, false).await.expect_err("changes were requested");
        assert!(refused.contains("already requested"), "{refused}");
    }

    /// A task that comes back from the rerun is a new review: nothing is
    /// decided yet, and the history keeps the earlier request.
    #[tokio::test]
    async fn a_later_review_starts_undecided_and_keeps_the_earlier_feedback() {
        let (_tmp, state) = test_state().await;
        let (_project_id, task_id) = seed_in_review(&state).await;
        request_changes(&state, task_id, "first round of feedback").await.expect("recorded");

        // What the executor does when the rerun reaches Human Review.
        {
            let mut tasks = state.task.tasks.write().await;
            let t = tasks.get_mut(&task_id).unwrap();
            t.status = TaskStatus::HumanReview;
            t.human_review.record_arrival();
        }

        let task = current(&state, task_id).await.unwrap();
        assert!(task.human_review.current_decision().is_none());
        assert!(task.human_review.pending_feedback().is_empty());
        assert_eq!(task.human_review.arrivals, 2);

        request_changes(&state, task_id, "second round").await.expect("recorded");
        let task = current(&state, task_id).await.unwrap();
        let history: Vec<_> = task
            .human_review
            .entries
            .iter()
            .map(|e| (e.sequence, e.arrival, e.feedback.clone().unwrap()))
            .collect();
        assert_eq!(
            history,
            vec![
                (1, 1, "first round of feedback".to_string()),
                (2, 2, "second round".to_string()),
            ]
        );
        assert_eq!(task.human_review.pending_feedback(), vec!["second round"]);
        assert_eq!(task.description.as_deref(), Some(DESCRIPTION));
    }

    // ---- Completing approved changes locally ----

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A project whose repository has no remote, a Task Checkout of its own
    /// with committed work, and a task in Human Review that owns both.
    struct Local {
        _tmp: tempfile::TempDir,
        state: crate::AppState,
        repo: std::path::PathBuf,
        project_id: Uuid,
        task_id: Uuid,
        branch: String,
        worktree: std::path::PathBuf,
    }

    async fn local_project() -> Local {
        let (tmp, state) = test_state().await;
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);

        let repository_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        state.repository.repositories.write().await.insert(
            repository_id,
            crate::domain::Repository {
                id: repository_id,
                local_path: repo.to_str().unwrap().to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        let base = crate::domain::project::ProjectBase::LocalBranch { branch: "main".to_string() };
        state.project.projects.write().await.insert(
            project_id,
            crate::domain::Project {
                id: project_id,
                name: "no remote".to_string(),
                repository_id: Some(repository_id),
                scope: crate::domain::project::ProjectScope::Standalone,
                state_location: crate::config::paths::StateLocation::External,
                base: Some(base.clone()),
                agent_type: crate::domain::AgentType::ClaudeCode,
                agent_config: crate::domain::AgentConfig {
                    agent_type: crate::domain::AgentType::ClaudeCode,
                    command: "claude".to_string(),
                    args: Vec::new(),
                    env: HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );

        let branch = "task-local-done".to_string();
        let (info, _) = state
            .worktree_manager
            .create_or_adopt(repo.to_str().unwrap(), &branch, Some(&base))
            .await
            .expect("a Task Checkout");
        let worktree = std::path::PathBuf::from(&info.path);
        git(&worktree, &["config", "user.email", "test@example.com"]);
        git(&worktree, &["config", "user.name", "Test"]);
        std::fs::write(worktree.join("work.txt"), "work\n").unwrap();
        git(&worktree, &["add", "."]);
        git(&worktree, &["commit", "-q", "-m", "task work"]);

        let mut task = create_test_task_full("finish locally", project_id, TaskStatus::HumanReview, 0);
        task.branch_name = Some(branch.clone());
        task.worktree_path = Some(info.path.clone());
        task.overall_progress = 90;
        task.human_review.record_arrival();
        let task_id = task.id;
        state.task.tasks.write().await.insert(task_id, task.clone());
        state.storage.save_project_tasks(project_id, &[task]).expect("seed the board");
        Local { _tmp: tmp, state, repo, project_id, task_id, branch, worktree }
    }

    impl Local {
        fn on_disk(&self) -> Task {
            on_disk(&self.state, self.project_id, self.task_id)
        }
        async fn approve(&self) {
            approve(&self.state, self.task_id, false).await.expect("approved");
        }
        async fn complete(&self) -> Result<Task, String> {
            complete_locally(&self.state, self.task_id, false).await
        }
        fn branch_exists(&self) -> bool {
            std::process::Command::new("git")
                .args(["rev-parse", "--verify", "-q", &format!("refs/heads/{}", self.branch)])
                .current_dir(&self.repo)
                .output()
                .unwrap()
                .status
                .success()
        }
        /// Everything about the task and its checkout that a refusal must
        /// leave exactly as it found it.
        async fn untouched(&self) {
            let task = current(&self.state, self.task_id).await.unwrap();
            assert_eq!(task.status, TaskStatus::HumanReview);
            assert!(!task.completed_locally());
            assert_eq!(task.overall_progress, 90, "a refusal does not touch progress");
            assert!(task.worktree_path.is_some());
            assert!(self.worktree.join("work.txt").exists(), "the checkout is still there");
            assert!(self.branch_exists());
            let disk = self.on_disk();
            assert_eq!(disk.status, TaskStatus::HumanReview);
            assert_eq!(disk.overall_progress, 90);
            assert!(!disk.completed_locally());
        }
    }

    fn completions(task: &Task) -> usize {
        task.activity.iter().filter(|e| e.kind == slashit_activity::Kind::CompletedLocally).count()
    }

    #[tokio::test]
    async fn an_approved_task_completes_locally_and_keeps_its_branch_and_history() {
        let local = local_project().await;
        local.approve().await;
        let main_before = git(&local.repo, &["rev-parse", "main"]);
        let branch_before = git(&local.repo, &["rev-parse", &local.branch]);

        let done = local.complete().await.expect("completed");

        assert_eq!(done.status, TaskStatus::Done);
        assert!(done.completed_locally());
        assert_eq!(done.overall_progress, 100, "a finished task shows it, as a merged one does");
        assert_eq!(done.phase, crate::domain::task::TaskPhase::Complete);
        assert!(done.worktree_path.is_none(), "the checkout is released");
        assert!(!local.worktree.exists(), "and removed");
        assert!(!done.cleanup_in_flight);
        assert!(done.pr_url.is_none() && done.external_refs.iter().all(|r| !r.is_pr()));
        assert_eq!(done.branch_name.as_deref(), Some(local.branch.as_str()));
        assert!(local.branch_exists(), "the branch survives");
        assert_eq!(git(&local.repo, &["rev-parse", &local.branch]), branch_before, "with its commits");
        assert_eq!(git(&local.repo, &["rev-parse", "main"]), main_before, "and nothing reached main");
        assert!(done.human_review.is_approved(), "the approval stays on the record");
        assert_eq!(done.human_review.entries.len(), 1);
        assert_eq!(done.human_review.arrivals, 1);

        let timeline = task_timeline(&done);
        assert_eq!(
            timeline[timeline.len() - 3..],
            ["You approved the changes", "Moved to Done", "Completed locally, not merged"]
        );

        // A restart reads the same thing.
        let reloaded = local.on_disk();
        assert!(reloaded.completed_locally());
        assert_eq!(reloaded.overall_progress, 100);
        assert_eq!(reloaded.phase, crate::domain::task::TaskPhase::Complete);
        assert_eq!(reloaded.status, TaskStatus::Done);
        assert!(reloaded.worktree_path.is_none());
    }

    #[tokio::test]
    async fn changes_nobody_approved_cannot_be_completed_locally() {
        let local = local_project().await;
        let refused = local.complete().await.expect_err("undecided");
        assert!(refused.contains("Approve the changes"), "{refused}");
        local.untouched().await;

        // Changes requested then back in review: still no approval.
        request_changes(&local.state, local.task_id, "more").await.expect("sent back");
        {
            let mut tasks = local.state.task.tasks.write().await;
            let t = tasks.get_mut(&local.task_id).unwrap();
            t.status = TaskStatus::HumanReview;
            t.human_review.record_arrival();
        }
        let refused = local.complete().await.expect_err("a new review, undecided");
        assert!(refused.contains("Approve the changes"), "{refused}");
        assert!(local.worktree.join("work.txt").exists());
    }

    #[tokio::test]
    async fn a_task_outside_human_review_cannot_be_completed_locally() {
        let local = local_project().await;
        local.approve().await;
        local.state.task.tasks.write().await.get_mut(&local.task_id).unwrap().status = TaskStatus::InProgress;
        let refused = local.complete().await.expect_err("working");
        assert!(refused.contains("Human Review"), "{refused}");
        assert!(local.worktree.join("work.txt").exists());
        assert!(!current(&local.state, local.task_id).await.unwrap().completed_locally());
    }

    #[tokio::test]
    async fn a_task_an_agent_still_owns_is_refused_and_left_alone() {
        let local = local_project().await;
        local.approve().await;
        let executor = crate::test_helpers::attach_test_executor(&local.state);
        executor.register_fake_reviewing_owner_for_test(local.task_id).await;

        let refused = local.complete().await.expect_err("owned");
        assert!(refused.contains("still running"), "{refused}");
        local.untouched().await;
        assert_eq!(executor.running_task_count().await, 1, "refusing does not stop or end anything");
        assert!(
            crate::lifecycle::ExecutionOwnership::is_task_running(executor.as_ref(), local.task_id).await,
            "the owner is still there"
        );
    }

    #[tokio::test]
    async fn a_pull_request_being_opened_blocks_completing_locally() {
        let local = local_project().await;
        local.approve().await;
        let _mark = local.state.task.deliveries.start(local.task_id);
        let refused = local.complete().await.expect_err("being delivered");
        assert!(refused.contains("being opened"), "{refused}");
        local.untouched().await;
    }

    #[tokio::test]
    async fn a_checkout_git_will_not_remove_keeps_the_task_in_review_and_completes_on_retry() {
        let local = local_project().await;
        local.approve().await;
        std::fs::write(local.worktree.join("unsaved.txt"), "not committed\n").unwrap();

        let refused = local.complete().await.expect_err("uncommitted work");
        assert!(refused.contains("was kept"), "{refused}");
        local.untouched().await;
        let task = current(&local.state, local.task_id).await.unwrap();
        assert!(task.error_message.as_deref().is_some_and(|m| m.contains("was kept")));
        assert!(!task.cleanup_in_flight, "the attempt is over");
        assert!(local.worktree.join("unsaved.txt").exists(), "nothing the person wrote is gone");
        assert_eq!(completions(&task), 0, "and no completion was claimed");

        // Dealt with, asked again.
        git(&local.worktree, &["add", "."]);
        git(&local.worktree, &["commit", "-q", "-m", "keep it"]);
        let done = local.complete().await.expect("completed on the second request");
        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(completions(&done), 1);
        assert!(done.error_message.is_none(), "the earlier refusal is cleared");
        assert!(!local.worktree.exists());
        assert!(local.branch_exists());
    }

    #[tokio::test]
    async fn asking_again_after_completion_changes_nothing() {
        let local = local_project().await;
        local.approve().await;
        let first = local.complete().await.expect("completed");
        let on_disk_first = serde_json::to_value(local.on_disk()).unwrap();

        let second = local.complete().await.expect("idempotent");
        let third = local.complete().await.expect("idempotent");

        for again in [&second, &third] {
            assert_eq!(again.status, TaskStatus::Done);
            assert_eq!(completions(again), 1);
            assert_eq!(again.activity, first.activity, "no second entry, no second move");
            assert_eq!(again.updated_at, first.updated_at, "nothing was written");
        }
        assert_eq!(serde_json::to_value(local.on_disk()).unwrap(), on_disk_first);
    }

    #[tokio::test]
    async fn two_requests_at_once_complete_the_task_once() {
        let local = local_project().await;
        local.approve().await;
        let (a, b) = tokio::join!(local.complete(), local.complete());
        let (a, b) = (a.expect("one completes"), b.expect("the other sees it done"));
        assert_eq!(a.status, TaskStatus::Done);
        assert_eq!(b.status, TaskStatus::Done);
        let task = current(&local.state, local.task_id).await.unwrap();
        assert_eq!(completions(&task), 1);
        assert_eq!(
            task.activity.iter().filter(|e| matches!(e.kind, slashit_activity::Kind::Moved { .. })).count(),
            1
        );
    }

    /// A process that died while removing the checkout leaves the task in
    /// Human Review with `cleanup_in_flight` set. Asking again finishes the
    /// job once, whether the removal got as far as the directory or not.
    #[tokio::test]
    async fn a_completion_interrupted_by_a_crash_is_finished_by_asking_again() {
        for removed_before_crash in [false, true] {
            let local = local_project().await;
            local.approve().await;
            {
                let mut tasks = local.state.task.tasks.write().await;
                let t = tasks.get_mut(&local.task_id).unwrap();
                t.cleanup_in_flight = true;
                let snapshot = t.clone();
                local.state.storage.save_project_tasks(local.project_id, &[snapshot]).unwrap();
            }
            if removed_before_crash {
                git(&local.repo, &["worktree", "remove", "--force", local.worktree.to_str().unwrap()]);
            }
            // What a restart sees: nothing is Done, nothing is lost.
            let after_crash = local.on_disk();
            assert_eq!(after_crash.status, TaskStatus::HumanReview);
            assert_eq!(completions(&after_crash), 0);
            assert!(after_crash.human_review.is_approved());

            let done = local.complete().await.expect("finished");
            assert_eq!(done.status, TaskStatus::Done, "removed_before_crash={removed_before_crash}");
            assert!(!done.cleanup_in_flight);
            assert!(done.worktree_path.is_none());
            assert_eq!(completions(&done), 1);
            assert!(local.branch_exists());
            assert_eq!(completions(&local.on_disk()), 1);
        }
    }

    #[tokio::test]
    async fn a_project_that_can_open_a_pull_request_is_not_completed_locally() {
        let local = local_project().await;
        git(&local.repo, &["remote", "add", "origin", "https://github.com/BarraDev/example.git"]);
        local.approve().await;

        let refused = local.complete().await.expect_err("delivered by pull request");
        assert!(refused.contains("Create PR"), "{refused}");
        local.untouched().await;

        // Delivery still sees the project as it did.
        assert_eq!(pr_availability(&local.state, local.task_id).await, Ok(PrAvailability::Available));
    }

    /// Asked for by name, a project with a GitHub remote finishes locally:
    /// no pull request, no push, nothing merged, the branch kept, and the
    /// project's default delivery unchanged afterwards.
    #[tokio::test]
    async fn a_project_with_a_github_remote_finishes_locally_when_that_is_chosen() {
        let local = local_project().await;
        git(&local.repo, &["remote", "add", "origin", "https://github.com/BarraDev/example.git"]);
        local.approve().await;
        let main_before = git(&local.repo, &["rev-parse", "main"]);

        let done = complete_locally(&local.state, local.task_id, true).await.expect("chosen explicitly");
        assert!(done.completed_locally());
        assert_eq!(done.status, TaskStatus::Done);
        assert!(done.pr_url.is_none());
        assert!(local.branch_exists(), "the branch keeps the work");
        assert!(done.worktree_path.is_none());
        assert_eq!(git(&local.repo, &["rev-parse", "main"]), main_before, "nothing was merged");
        assert!(!local.state.task.deliveries.contains(local.task_id), "no pull request was started");
        assert_eq!(completions(&done), 1);

        // Asked again, it answers and records nothing more.
        let again = complete_locally(&local.state, local.task_id, true).await.unwrap();
        assert_eq!(completions(&again), 1);
        // The project's default delivery is still the pull request.
        assert_eq!(pr_availability(&local.state, local.task_id).await, Ok(PrAvailability::Available));
    }

    /// The explicit choice never reaches a task that has, or is getting, a
    /// pull request, and never skips the approval.
    #[tokio::test]
    async fn choosing_to_finish_locally_does_not_override_a_pull_request_or_the_approval() {
        let local = local_project().await;
        git(&local.repo, &["remote", "add", "origin", "https://github.com/BarraDev/example.git"]);

        let refused = complete_locally(&local.state, local.task_id, true).await.expect_err("not approved");
        assert!(refused.contains("Approve the changes"), "{refused}");
        local.approve().await;

        let guard = local.state.task.deliveries.start(local.task_id);
        let refused = complete_locally(&local.state, local.task_id, true).await.expect_err("being opened");
        assert!(refused.contains("pull request"), "{refused}");
        drop(guard);
        local.state.task.tasks.write().await.get_mut(&local.task_id).unwrap().pr_url =
            Some("https://github.com/BarraDev/example/pull/1".to_string());
        let refused = complete_locally(&local.state, local.task_id, true).await.expect_err("has one");
        assert!(refused.contains("pull request"), "{refused}");
        assert!(local.worktree.join("work.txt").exists(), "the checkout was left alone");
    }

    #[tokio::test]
    async fn a_task_with_a_pull_request_is_not_completed_locally() {
        let local = local_project().await;
        local.approve().await;
        local.state.task.tasks.write().await.get_mut(&local.task_id).unwrap().pr_url =
            Some("https://github.com/BarraDev/example/pull/1".to_string());
        let refused = local.complete().await.expect_err("has a pull request");
        assert!(refused.contains("pull request"), "{refused}");
        assert!(local.worktree.join("work.txt").exists());
    }

    #[tokio::test]
    async fn a_remote_that_is_not_github_still_completes_locally() {
        let local = local_project().await;
        git(&local.repo, &["remote", "add", "origin", "https://gitlab.example.com/o/r.git"]);
        local.approve().await;
        let done = local.complete().await.expect("completed");
        assert!(done.completed_locally());
    }

    /// An attempt marks its task until it ends, and one attempt ending never
    /// clears another's mark on the same task.
    #[test]
    fn a_delivery_mark_lasts_exactly_as_long_as_its_attempts() {
        let deliveries = DeliveriesInFlight::default();
        let task_id = Uuid::new_v4();
        assert!(!deliveries.contains(task_id));
        let first = deliveries.start(task_id);
        let second = deliveries.start(task_id);
        drop(first);
        assert!(deliveries.contains(task_id), "the second attempt is still running");
        drop(second);
        assert!(!deliveries.contains(task_id));
    }

    #[tokio::test]
    async fn an_approval_records_the_exact_commit_that_was_approved() {
        let local = local_project().await;
        let tip = git(&local.repo, &["rev-parse", &format!("refs/heads/{}", local.branch)]);
        local.approve().await;
        let task = local.on_disk();
        assert_eq!(task.human_review.approved_tip(), Some(tip.as_str()));

        // Work committed afterwards is not covered by it.
        git(&local.worktree, &["config", "user.email", "test@example.com"]);
        std::fs::write(local.worktree.join("later.txt"), "later\n").unwrap();
        git(&local.worktree, &["add", "."]);
        git(&local.worktree, &["commit", "-q", "-m", "after the approval"]);
        assert_eq!(local.on_disk().human_review.approved_tip(), Some(tip.as_str()));
    }

    #[tokio::test]
    async fn a_task_without_a_branch_is_approved_without_a_commit() {
        let (_tmp, state) = test_state().await;
        let (project_id, task_id) = seed_in_review(&state).await;
        approve(&state, task_id, false).await.expect("approved");
        let task = on_disk(&state, project_id, task_id);
        assert!(task.human_review.is_approved());
        assert_eq!(task.human_review.approved_tip(), None);
    }
}
