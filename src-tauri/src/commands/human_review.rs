//! Human Review decisions: approving a task's changes, requesting changes to
//! them, and delivering approved changes as a pull request.
//!
//! Approval and delivery are separate on purpose. Approving records what a
//! person decided, durably, before anything talks to a forge; opening the pull
//! request is a second step whose failure is recorded next to the approval
//! and never undoes it. See [`crate::domain::task::HumanReviewRecord`].

use std::collections::HashMap;

use serde::Serialize;
use uuid::Uuid;

use crate::commands::pr::{create_pr_for_approved_task, pr_availability, PrAvailability};
use crate::commands::task::renumber_column;
use crate::domain::task::HumanReviewDecision;
use crate::domain::{Task, TaskStatus};

/// The longest feedback a change request may carry, in characters. It goes
/// into the next run's prompt whole.
pub const MAX_FEEDBACK_CHARS: usize = 20_000;

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
    let amend = move |staged: &mut HashMap<Uuid, Task>| {
        if let Some(t) = staged.get_mut(&task_id) {
            if t.status == TaskStatus::HumanReview && t.human_review.current_decision().is_none() {
                t.human_review.push(HumanReviewDecision::Approved, None, now);
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
async fn deliver(state: &crate::AppState, task_id: Uuid) -> PrDelivery {
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
async fn record_pr_failure(state: &crate::AppState, task_id: Uuid, reason: String) -> PrDelivery {
    let note = reason.clone();
    let amend = move |staged: &mut HashMap<Uuid, Task>| {
        if let Some(t) = staged.get_mut(&task_id) {
            t.human_review.pr_error = Some(note.clone());
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
}
