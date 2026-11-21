// SPDX-License-Identifier: MPL-2.0
//! Human Review decisions: approving a task's changes, requesting changes to
//! them, and delivering approved changes as a pull request.
//!
//! Approval and delivery are separate on purpose. Approving records what a
//! person decided, durably, before anything talks to a forge; opening the pull
//! request is a second step whose failure is recorded next to the approval
//! and never undoes it. See [`crate::domain::task::HumanReviewRecord`].

use crate::commands::gui_admission::GuiMutation;
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
    pub(crate) fn start(&self, task_id: Uuid) -> DeliveryMark {
        *self.lock().entry(task_id).or_default() += 1;
        DeliveryMark { deliveries: self.clone(), task_id }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, u32>> {
        // The map is always left consistent, so a panic elsewhere while it
        // was held is no reason to stop answering.
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub(crate) struct DeliveryMark {
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
/// `tip` is the branch commit the person was shown when they decided. The
/// approval records that commit, and only that commit: if the branch has moved
/// since, nothing is recorded and the person is asked to review it again.
///
/// `Err` means the approval was not recorded. Once it is recorded, a pull
/// request that cannot be opened is reported in [`ApprovalOutcome::pr`], not
/// as an error, because the approval happened.
#[tauri::command]
pub async fn approve_task(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    create_pr: bool,
    tip: Option<String>,
) -> Result<ApprovalOutcome, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    crate::commands::project::refuse_if_task_project_removed(&state, task_id).await?;
    approve(&state, task_id, create_pr, tip.as_deref()).await
}

/// Open the pull request for a task whose changes are already approved,
/// without approving them again.
#[tauri::command]
pub async fn create_approved_task_pr(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<ApprovalOutcome, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    crate::commands::project::refuse_if_task_project_removed(&state, task_id).await?;
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
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    without_pull_request: Option<bool>,
) -> Result<Task, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    crate::commands::project::refuse_if_task_project_removed(&state, task_id).await?;
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
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    feedback: String,
) -> Result<Task, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    crate::commands::project::refuse_if_task_project_removed(&state, task_id).await?;
    request_changes(&state, task_id, &feedback).await
}

/// Whether a task's approval still covers its branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalCoverage {
    /// The task is not approved, or has no branch to bind it to.
    NotApplicable,
    /// The branch is at the approved commit.
    Current,
    /// The branch moved on from the approved commit (it is still on the
    /// branch) or replaced it (`rewritten`).
    Moved,
    /// The approval did not record the commit.
    Unrecorded,
    /// The branch cannot be found.
    BranchMissing,
}

/// The head of the Task's published pull request, as the status poll last
/// read it. It is read from memory: asking GitHub is the poll's job, never a
/// question the panel's cadence asks. The approval is not renewed by it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PublishedHead {
    /// The head commit GitHub reported at `observed_at`. `None` when the latest
    /// attempt gave none, so an older head is never offered as current.
    pub head: Option<String>,
    /// When `head` was read from GitHub.
    pub observed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Why the head is not known: the latest attempt failed, or none has
    /// succeeded yet.
    pub unverified: Option<String>,
}

/// How old GitHub's answer about the head may be before it is no longer
/// trusted as the head now. A poll that has not answered for two intervals may
/// have missed a push, so the head is then unverified rather than current.
const PUBLISHED_HEAD_FRESH_SECONDS: i64 = 2 * crate::pr_status::POLL_INTERVAL.as_secs() as i64;

/// The published head as the cache holds it at `now`. Only the latest attempt
/// counts: a failed attempt makes the head unknown even when an earlier one
/// read it, and so does an answer older than [`PUBLISHED_HEAD_FRESH_SECONDS`].
fn published_head(entry: Option<&crate::pr_status::PrStatusEntry>, now: chrono::DateTime<chrono::Utc>) -> PublishedHead {
    let unknown = |why: String| PublishedHead { head: None, observed_at: None, unverified: Some(why) };
    let Some(entry) = entry else {
        return unknown("GitHub has not answered about this pull request yet.".to_string());
    };
    if let Some(error) = &entry.error {
        return unknown(format!("GitHub could not be asked: {error}."));
    }
    match entry.status.as_ref().and_then(|s| s.head_oid.clone()) {
        Some(head) => match entry.fetched_at {
            Some(at) if (now - at).num_seconds() <= PUBLISHED_HEAD_FRESH_SECONDS => {
                PublishedHead { head: Some(head), observed_at: Some(at), unverified: None }
            }
            Some(_) => unknown(format!(
                "GitHub's last answer about the pull request's head is more than {} minutes old.",
                PUBLISHED_HEAD_FRESH_SECONDS / 60
            )),
            None => unknown("GitHub's answer about the pull request's head has no read time.".to_string()),
        },
        None => unknown("GitHub's answer did not name the pull request's head commit.".to_string()),
    }
}

/// The pull request the Task publishes to, as the status cache keys it. `None`
/// when the Task has no pull request. The address is the Task's own `pr_url`,
/// or its first GitHub reference when no `pr_url` is recorded. A recorded
/// address whose reference is missing is read from the URL itself, so a Task
/// linked by `pr_url` alone is still checked. An address that cannot be read
/// is an error, which the panel reports as unverified.
fn published_key(task: &Task) -> Option<Result<crate::pr_status::PrKey, String>> {
    let address = task.authoritative_pr_url()?;
    let referenced = task.external_refs.iter().find_map(|r| match r {
        crate::domain::ExternalRef::GithubPr { url, repo, number, .. } if url == address => {
            Some(crate::pr_status::PrKey::new(repo.as_str(), *number))
        }
        _ => None,
    });
    let key = referenced.or_else(|| match crate::commands::pr::parse_pr_url_to_ref(address)? {
        crate::domain::ExternalRef::GithubPr { repo, number, .. } => Some(crate::pr_status::PrKey::new(repo, number)),
        _ => None,
    });
    Some(key.ok_or_else(|| "the pull request's address cannot be read as a GitHub pull request".to_string()))
}

#[cfg(test)]
mod published_head_tests {
    use super::*;
    use crate::pr_status::{parse_pr_status, PrFetchError, PrFetchErrorKind, PrStatus, PrStatusEntry};
    use serde_json::json;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn entry(status: Option<PrStatus>, error: Option<PrFetchError>) -> PrStatusEntry {
        PrStatusEntry {
            repo: "o/r".into(),
            number: 9,
            status,
            fetched_at: Some(chrono::Utc::now()),
            attempted_at: chrono::Utc::now(),
            error,
        }
    }

    fn read_head(head: &str) -> PrStatus {
        parse_pr_status(&json!({ "state": "OPEN", "headRefOid": head }))
    }

    #[test]
    fn a_head_the_latest_attempt_read_is_the_published_head() {
        let read = published_head(Some(&entry(Some(read_head(HEAD)), None)), chrono::Utc::now());
        assert_eq!(read.head.as_deref(), Some(HEAD));
        assert!(read.observed_at.is_some());
        assert_eq!(read.unverified, None);
    }

    #[test]
    fn no_answer_or_an_abbreviated_head_is_unverified_never_a_head() {
        let none = published_head(None, chrono::Utc::now());
        assert_eq!(none.head, None);
        assert!(none.unverified.is_some_and(|w| w.contains("not answered")));

        let abbreviated = published_head(Some(&entry(Some(read_head("0123456")), None)), chrono::Utc::now());
        assert_eq!(abbreviated.head, None, "an abbreviated head cannot be checked against a commit");
        assert!(abbreviated.unverified.is_some());
    }

    #[test]
    fn a_failed_latest_attempt_hides_the_head_an_earlier_one_read() {
        let error = PrFetchError { kind: PrFetchErrorKind::Timeout, message: "took too long".into() };
        let failed = published_head(Some(&entry(Some(read_head(HEAD)), Some(error))), chrono::Utc::now());
        assert_eq!(failed.head, None, "an earlier head says nothing about the pull request now");
        assert!(failed.unverified.is_some_and(|w| w.contains("took too long")));
    }

    /// A head read long ago may have been overtaken by a push nobody has seen,
    /// so it is unverified once the poll has been silent for two intervals.
    #[test]
    fn a_head_older_than_two_poll_intervals_is_unverified_not_current() {
        let now = chrono::Utc::now();
        let mut old = entry(Some(read_head(HEAD)), None);
        old.fetched_at = Some(now - chrono::Duration::seconds(PUBLISHED_HEAD_FRESH_SECONDS + 1));
        let stale = published_head(Some(&old), now);
        assert_eq!(stale.head, None, "an old head is not the head now");
        assert!(stale.unverified.is_some_and(|w| w.contains("more than")));

        old.fetched_at = Some(now - chrono::Duration::seconds(PUBLISHED_HEAD_FRESH_SECONDS));
        assert_eq!(published_head(Some(&old), now).head.as_deref(), Some(HEAD), "a head at the limit is still fresh");
    }

    /// A Task linked by `pr_url` alone is still checked at the address it names,
    /// and an address that is not a GitHub pull request is an error, not no pull request.
    #[test]
    fn a_task_linked_only_by_its_pr_url_is_checked_at_that_address() {
        let mut task = crate::test_helpers::create_test_task_full(
            "published",
            uuid::Uuid::new_v4(),
            crate::domain::TaskStatus::PrCreated,
            0,
        );
        task.pr_url = Some("https://github.com/BarraDev/slashit/pull/355".to_string());
        let key = published_key(&task).expect("a pull request is recorded").expect("its address is readable");
        assert_eq!(key, crate::pr_status::PrKey::new("BarraDev/slashit", 355));

        task.pr_url = Some("https://example.com/not-github".to_string());
        assert!(published_key(&task).expect("a pull request is recorded").is_err());

        task.pr_url = None;
        assert!(published_key(&task).is_none(), "no pull request is recorded");
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApprovalStatus {
    pub coverage: ApprovalCoverage,
    pub approved_tip: Option<String>,
    /// The branch's tip now: the commit approving again would cover.
    pub branch_tip: Option<String>,
    /// For `Moved`: whether the approved commit is no longer on the branch. `None`
    /// when Git could not answer, which is not the same as a branch that only moved on.
    pub rewritten: Option<bool>,
    /// The published pull request's head, when the Task has a pull request.
    /// It may differ from `approved_tip` even while `coverage` is `Current`,
    /// because another clone can move the pull request without moving the branch.
    pub published: Option<PublishedHead>,
    /// The pull request address the Task had when this answer was read, so the
    /// panel can tell an answer for a link it no longer shows (none, or another
    /// pull request) from the current one.
    pub pull_request: Option<String>,
    /// What a person can act on, when the approval does not cover the branch.
    pub reason: Option<String>,
}

/// Whether the approval of a task still covers the branch as it is. Reads
/// only.
#[tauri::command]
pub async fn get_approval_status(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<ApprovalStatus, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    // The whole answer is bounded, both Git reads in it. A stopped read drops
    // its queries, and each query's child is killed with it.
    tokio::time::timeout(crate::commands::pr::COVERAGE_READ_BOUND, approval_status(&state, task_id))
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "Reading the approval's coverage took longer than {} seconds and was stopped.",
                crate::commands::pr::COVERAGE_READ_BOUND.as_secs()
            ))
        })
}

pub(crate) async fn approval_status(state: &crate::AppState, task_id: Uuid) -> Result<ApprovalStatus, String> {
    let task = current(state, task_id).await.ok_or("Task not found")?;
    let approved_tip = task.human_review.approved_tip().map(str::to_string);
    // Read from the status cache: no GitHub request and no repository access.
    let published = published_key(&task).map(|key| match key {
        Ok(key) => published_head(state.pr_statuses.get(&key).as_ref(), chrono::Utc::now()),
        Err(why) => PublishedHead { head: None, observed_at: None, unverified: Some(why) },
    });
    let none = |coverage| ApprovalStatus {
        coverage,
        approved_tip: approved_tip.clone(),
        branch_tip: None,
        rewritten: None,
        reason: None,
        published: published.clone(),
        pull_request: task.authoritative_pr_url().map(str::to_string),
    };
    if task.branch_name.is_none() {
        return Ok(none(ApprovalCoverage::NotApplicable));
    }
    if !task.human_review.is_approved() {
        // Not approved yet: name the commit an approval would record now, so
        // the person can see it before deciding. A failed read is an error, so
        // the panel can say so and offer a retry; a branch that does not exist
        // is `Ok(None)`, which the panel reports as a missing branch. Read
        // without touching the repository: this is asked again while the panel
        // is shown, and a read must not export.
        let branch_tip = crate::commands::pr::task_branch_tip_read_only(state, task_id).await?;
        return Ok(ApprovalStatus { branch_tip, ..none(ApprovalCoverage::NotApplicable) });
    }
    let branch_tip = crate::commands::pr::task_branch_tip_read_only(state, task_id).await?;
    let refusal = match task.human_review.binds(true, branch_tip.as_deref()) {
        Ok(()) => return Ok(ApprovalStatus { branch_tip, ..none(ApprovalCoverage::Current) }),
        Err(refusal) => refusal,
    };
    let coverage = match refusal {
        crate::domain::task::ApprovalBindingRefusal::Unrecorded => ApprovalCoverage::Unrecorded,
        crate::domain::task::ApprovalBindingRefusal::Moved { .. } => ApprovalCoverage::Moved,
        crate::domain::task::ApprovalBindingRefusal::BranchMissing { .. } => ApprovalCoverage::BranchMissing,
    };
    let mut rewritten = None;
    if let (ApprovalCoverage::Moved, Some(approved), Some(tip), Ok(repo)) = (
        coverage,
        approved_tip.as_deref(),
        branch_tip.as_deref(),
        crate::commands::integration::repo_path_of(state, task.project_id).await,
    ) {
        // Only a definite answer from Git says whether the approved commit is
        // gone; a failed query leaves it unknown. The query is read-only and
        // killed if the read is stopped (see get_approval_status).
        rewritten = crate::commands::pr::commit_is_ancestor_read_only(&repo, approved, tip)
            .await
            .ok()
            .map(|is_ancestor| !is_ancestor);
    }
    Ok(ApprovalStatus { branch_tip, rewritten, reason: Some(refusal.to_string()), ..none(coverage) })
}

/// Approve the Task branch as it is now, after it moved past the commit that
/// was approved. `tip` is the commit the person was shown.
///
/// The earlier approval stays in the history; this adds a new one for `tip`,
/// which is the current decision because it is the last. Nothing is delivered
/// or completed by it.
#[tauri::command]
pub async fn approve_task_again(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    tip: String,
) -> Result<Task, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    crate::commands::project::refuse_if_task_project_removed(&state, task_id).await?;
    approve_again(&state, task_id, &tip).await
}

pub(crate) async fn approve_again(
    state: &crate::AppState,
    task_id: Uuid,
    tip: &str,
) -> Result<Task, String> {
    let _lease = crate::commands::project::lease_active_task(state, task_id).await?;

    let task = current(state, task_id).await.ok_or("Task not found")?;
    check_reviewable(&task, "approved again")?;
    if !task.human_review.is_approved() {
        return Err("Approve the changes first; this task has no approval to renew.".to_string());
    }
    if task.pr_url.is_some()
        || task.external_refs.iter().any(|r| r.is_pr())
        || state.task.deliveries.contains(task.id)
    {
        return Err("This task has a pull request, or one is being opened, so its approval is not renewed.".to_string());
    }
    if task.integrating_current_approval() {
        return Err(
            "An integration of this task into a local branch was started, and it uses the commit \
             that was approved. Resume or discard it first."
                .to_string(),
        );
    }
    if let Some(executor) = state.executor.get() {
        if crate::lifecycle::ExecutionOwnership::is_task_running(executor.as_ref(), task_id).await {
            return Err("An agent is still running for this task. Stop the run first.".to_string());
        }
    }
    let now_tip = crate::commands::pr::task_branch_tip(state, task_id).await?;
    match task.human_review.binds(true, now_tip.as_deref()) {
        Ok(()) => return Ok(task),
        // An approval that never recorded its commit is renewed the same way:
        // the person was shown this commit and approves it.
        Err(
            crate::domain::task::ApprovalBindingRefusal::Moved { .. }
            | crate::domain::task::ApprovalBindingRefusal::Unrecorded,
        ) => {}
        Err(other) => return Err(other.to_string()),
    }
    if now_tip.as_deref() != Some(tip) {
        return Err(
            "The branch moved after it was shown to you, so nothing was approved. Review it again."
                .to_string(),
        );
    }
    let now = chrono::Utc::now();
    let wanted = tip.to_string();
    let amend = move |staged: &mut HashMap<Uuid, Task>| {
        if let Some(t) = staged.get_mut(&task_id) {
            if t.status == TaskStatus::HumanReview
                && t.human_review.is_approved()
                && t.human_review.approved_tip() != Some(wanted.as_str())
            {
                t.human_review.approve_again(wanted.clone(), now);
                t.human_review.pr_error = None;
            }
        }
    };
    let task = crate::lifecycle::commit_task(&state.task.tasks, &state.storage, task_id, &amend)
        .await
        .map_err(|e| format!("The approval was not saved: {e}"))?
        .ok_or("Task not found")?;
    if task.human_review.approved_tip() != Some(tip) {
        return Err("The task changed while it was being approved, so nothing was recorded.".to_string());
    }
    Ok(task)
}

/// The refusal for an approval whose shown commit no longer is the branch tip.
const MOVED_BEFORE_APPROVAL: &str =
    "The branch moved after it was shown to you, so nothing was approved. Review it again.";

/// The refusal for an approval whose branch commit cannot be read now, so it
/// cannot be checked against what the person was shown.
const TIP_NOT_READ: &str =
    "The branch's commit could not be read, so nothing was approved. Check the branch, then review it again.";

/// Whether the commit an approval would record is the one the person was
/// shown. A task without a branch has no commit to show, and is approved as
/// before. For a task with a branch, the approval is refused unless both sides
/// name the same commit: a commit that cannot be read now is never approved
/// blind, because the approval would then cover a state nobody was shown.
fn shown_tip_is_current(has_branch: bool, shown: Option<&str>, current: Option<&str>) -> Result<(), String> {
    if !has_branch {
        return Ok(());
    }
    match (shown, current) {
        (Some(shown), Some(current)) if shown == current => Ok(()),
        (_, None) => Err(TIP_NOT_READ.to_string()),
        (None, Some(_)) => Err(
            "The commit to approve was not shown to you, so nothing was approved. Review it again."
                .to_string(),
        ),
        _ => Err(MOVED_BEFORE_APPROVAL.to_string()),
    }
}

pub(crate) async fn approve(
    state: &crate::AppState,
    task_id: Uuid,
    create_pr: bool,
    shown_tip: Option<&str>,
) -> Result<ApprovalOutcome, String> {
    // The delivery marker is taken before the approval is recorded, not
    // inside `deliver`: approving runs outside the task's lifecycle lease
    // after `record_approval` returns, so a Project deletion could otherwise
    // answer "nothing is owed for this Task" in the gap between the
    // approval's durable write and the delivery's registration, and delete
    // the Project out from under a delivery that is about to own the Task.
    // Counted marks nest, so `deliver` keeps starting its own.
    let _in_flight = state.task.deliveries.start(task_id);
    let task = record_approval(state, task_id, shown_tip).await?;
    if !create_pr {
        return Ok(ApprovalOutcome { task, pr: None });
    }
    let pr = deliver(state, task_id).await;
    Ok(ApprovalOutcome { task: current(state, task_id).await.unwrap_or(task), pr: Some(pr) })
}

/// Approve as a person would who was shown the branch's current commit. For
/// tests that are not about what was shown.
#[cfg(test)]
pub(crate) async fn approve_shown(
    state: &crate::AppState,
    task_id: Uuid,
    create_pr: bool,
) -> Result<ApprovalOutcome, String> {
    let task = current(state, task_id).await.ok_or("Task not found")?;
    let shown = crate::commands::integration::tip_to_approve(state, &task).await;
    approve(state, task_id, create_pr, shown.as_deref()).await
}

pub(crate) async fn retry_delivery(
    state: &crate::AppState,
    task_id: Uuid,
) -> Result<ApprovalOutcome, String> {
    // Same reason as in [`approve`]: the marker must be visible before the
    // checks below read the task, so a concurrent Project deletion cannot
    // slip between this decision and the delivery that follows it.
    let _in_flight = state.task.deliveries.start(task_id);
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
    let _lease = crate::commands::project::lease_active_task(state, task_id).await?;

    let task = current(state, task_id).await.ok_or("Task not found")?;
    if task.completed_locally() {
        return Ok(task);
    }
    check_completable(state, &task)?;
    // The approval covers one commit. Done is committed only for a branch
    // that is still at it, before the checkout is touched.
    crate::commands::pr::approved_commit_for_delivery(state, task_id).await?;
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
    complete_leased(state, task_id).await.map_err(|refusal| refusal.to_string())
}

/// What has to hold for approved changes to be completed locally, whoever
/// asks: the task is in Human Review with its changes approved, it has no
/// pull request and none is being opened, and no integration of it is half
/// done (whichever way that one ends decides what completing means).
pub(crate) fn check_completable(state: &crate::AppState, task: &Task) -> Result<(), String> {
    check_reviewable(task, "completed locally")?;
    if !task.human_review.is_approved() {
        return Err(
            "Approve the changes before completing them locally; this task has no approval."
                .to_string(),
        );
    }
    let has_pr = task.pr_url.is_some() || task.external_refs.iter().any(|r| r.is_pr());
    if has_pr || state.task.deliveries.contains(task.id) {
        return Err(
            "This task has a pull request, or one is being opened, so it is delivered through \
             that pull request, not completed locally."
                .to_string(),
        );
    }
    if task.local_integration.as_ref().is_some_and(|r| r.is_pending()) {
        return Err(
            "An integration of this task into a local branch was started and not finished. \
             Resume or discard it first."
                .to_string(),
        );
    }
    Ok(())
}

/// The terminalization of [`complete_locally`], for a caller that holds the
/// task's lease and has checked [`check_completable`]: the checkout is
/// removed, then `Done` is committed with the activity entry in one write.
pub(crate) async fn complete_leased(
    state: &crate::AppState,
    task_id: Uuid,
) -> Result<Task, crate::lifecycle::TerminalizeRefusal> {
    let record = |staged: &mut HashMap<Uuid, Task>| {
        if let Some(t) = staged.get_mut(&task_id) {
            t.overall_progress = 100;
            t.phase = crate::domain::task::TaskPhase::Complete;
            t.record_activity(crate::domain::task::ActivityKind::CompletedLocally {});
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
}

async fn record_approval(
    state: &crate::AppState,
    task_id: Uuid,
    shown_tip: Option<&str>,
) -> Result<Task, String> {
    let _lease = crate::commands::project::lease_active_task(state, task_id).await?;

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
    // merges this commit and nothing newer. The person's decision is only
    // valid for the commit they were shown, so a branch that moved since the
    // display is refused here, before anything is written.
    let tip = crate::commands::integration::tip_to_approve(state, &task).await;
    shown_tip_is_current(task.branch_name.is_some(), shown_tip, tip.as_deref())?;
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

    let _lease = crate::commands::project::lease_active_task(state, task_id).await?;

    let task = current(state, task_id).await.ok_or("Task not found")?;
    check_reviewable(&task, "sent back with changes")?;
    if task.human_review.is_approved() {
        return Err(
            "These changes are already approved, so changes cannot be requested for them."
                .to_string(),
        );
    }
    // Refused before any owner is ended and before the feedback is written, so
    // a Task with unfinished cleanup, restack, recovery or integration work
    // keeps its status and its feedback is not recorded.
    if let Some(refusal) = crate::lifecycle::queue_move_refusal(&task, &TaskStatus::Queue) {
        return Err(refusal);
    }

    // The task leaves Human Review for the queue, so whatever owns it now (a
    // pull request helper, say) is ended first, exactly as a status move does.
    let running = state
        .executor
        .get()
        .map(|e| e.as_ref() as &dyn crate::lifecycle::ExecutionOwnership);
    crate::lifecycle::end_active_ownership(running, task_id).await?;
    // Ending the owner can leave a Worker in recovery, so the entry is checked
    // again under the lease before the feedback and the Queue move are written.
    let stopped = current(state, task_id).await.ok_or("Task not found")?;
    if let Some(refusal) = crate::lifecycle::queue_move_refusal(&stopped, &TaskStatus::Queue) {
        return Err(refusal);
    }

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
mod shown_tip_tests {
    use super::{shown_tip_is_current, MOVED_BEFORE_APPROVAL, TIP_NOT_READ};

    const TIP: &str = "0123456789abcdef0123456789abcdef01234567";
    const LATER: &str = "89abcdef0123456789abcdef0123456789abcdef";

    /// Current tip: the commit shown is the commit the branch is at.
    #[test]
    fn an_approval_of_the_commit_that_was_shown_is_accepted() {
        assert_eq!(shown_tip_is_current(true, Some(TIP), Some(TIP)), Ok(()));
    }

    /// Missing evidence: the branch has no readable commit now, so the approval
    /// cannot be tied to what was shown. It is refused whether or not a commit
    /// was shown.
    #[test]
    fn an_unreadable_branch_commit_is_refused_and_never_approved_blind() {
        assert_eq!(shown_tip_is_current(true, None, None), Err(TIP_NOT_READ.to_string()));
        assert_eq!(shown_tip_is_current(true, Some(TIP), None), Err(TIP_NOT_READ.to_string()));
    }

    /// Missing evidence on the client: nothing was shown, but a commit is readable.
    #[test]
    fn an_approval_with_no_commit_shown_is_refused() {
        let err = shown_tip_is_current(true, None, Some(TIP)).unwrap_err();
        assert!(err.contains("was not shown to you"), "{err}");
    }

    /// Stale tip: the branch moved after the commit was shown.
    #[test]
    fn an_approval_of_a_commit_the_branch_has_left_is_refused() {
        assert_eq!(shown_tip_is_current(true, Some(TIP), Some(LATER)), Err(MOVED_BEFORE_APPROVAL.to_string()));
    }

    /// A task without a branch has no commit to show, and stays approvable.
    #[test]
    fn a_task_without_a_branch_needs_no_commit() {
        assert_eq!(shown_tip_is_current(false, None, None), Ok(()));
    }
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

    /// A task a run has just carried into Human Review, with an AI review.
    /// Its project's repository holds the task's branch at a readable commit,
    /// so an approval has a commit to record and a commit to check against.
    async fn seed_in_review(state: &crate::AppState, tmp: &tempfile::TempDir) -> (Uuid, Uuid) {
        let (repo, project_id, _base) = repo_and_project(state, tmp).await;
        git(&repo, &["branch", "task-under-review"]);
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

    /// Runs the executor's poller until the Task reaches `wanted` or fails.
    /// The poller is what starts a queued Task and sends a finished run to
    /// review, so nothing here sets the status by hand. A Task that is still
    /// running is left alone between passes.
    #[cfg(unix)]
    async fn poll_until(state: &crate::AppState, task_id: Uuid, wanted: TaskStatus) -> Task {
        let executor = state.executor.get().expect("an executor is attached").clone();
        for _ in 0..400 {
            let task = state.task.tasks.read().await[&task_id].clone();
            if task.status == wanted || task.status == TaskStatus::Error {
                return task;
            }
            if task.status != TaskStatus::InProgress {
                executor.poll_once_for_test().await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the Task did not reach {wanted:?}");
    }

    /// Two review cycles, then an approval whose pull request fails twice:
    /// the timeline read from the task file shows each decision once, in
    /// order, and the failed deliveries as failures, never as approvals.
    #[tokio::test]
    async fn review_cycles_and_delivery_retries_read_truthfully_on_the_timeline() {
        let (tmp, state) = test_state().await;
        let (project_id, task_id) = seed_in_review(&state, &tmp).await;

        request_changes(&state, task_id, "Also count lines.").await.expect("first request");
        arrive_again(&state, task_id).await;
        request_changes(&state, task_id, "And bytes.").await.expect("second request");
        arrive_again(&state, task_id).await;
        approve_shown(&state, task_id, false).await.expect("approved");
        approve_shown(&state, task_id, false).await.expect("approving again changes nothing");
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
        let (tmp, state) = test_state().await;
        let (project_id, task_id) = seed_in_review(&state, &tmp).await;

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

    /// A Manual run sent back with changes is run again by the executor. Both
    /// runs are real Worker runs that the poller starts and routes; neither
    /// is a shortcut. Each one reaches Human Review with no reviewer. The
    /// second run gets its own record, numbered 2 and in Manual mode, and the
    /// run-1 record is not reused.
    #[cfg(unix)]
    #[tokio::test]
    async fn changes_requested_on_a_manual_run_are_run_again_as_a_second_manual_run() {
        use crate::domain::task::{ActivityKind, ReviewMode};
        use std::path::Path;

        let (tmp, state) = test_state().await;
        let (_repo, project_id, _base) = repo_and_project(&state, &tmp).await;
        let mut task = create_test_task_full("count words", project_id, TaskStatus::Queue, 0);
        task.description = Some(DESCRIPTION.to_string());
        task.review_mode = ReviewMode::Manual;
        let task_id = task.id;
        state.task.tasks.write().await.insert(task_id, task.clone());
        state.storage.save_project_tasks(project_id, &[task]).expect("seed the board");
        crate::test_helpers::attach_test_executor(&state);

        // The fake Worker writes a number that changes on every run, so the
        // second run makes a real commit. Any reviewer would be a second
        // `claude` launch, and the launch count below would show it.
        let counter = tmp.path().join("worker-runs");
        let agent = crate::test_helpers::FakeProgram::install(
            "claude",
            &format!(
                "cat >/dev/null; n=$(cat {counter:?} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {counter:?}; \
                 printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-worker\",\"permissionMode\":\"auto\"}}'; \
                 printf 'run %s\\n' $n > agent_work.txt; \
                 printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}}'"
            ),
        )
        .await;

        // Run 1: the poller starts the Task, and the poller sends the finished
        // Manual run to Human Review.
        let first = poll_until(&state, task_id, TaskStatus::HumanReview).await;
        assert_eq!(first.status, TaskStatus::HumanReview, "{:?}", first.error_message);
        assert!(first.qa_signoff.is_none(), "no AI verdict is invented");
        let first_handoff = first.worker_handoff.clone().expect("run 1 has its record");
        assert_eq!(first_handoff.run, Some(1));
        assert_eq!(first_handoff.review_mode, ReviewMode::Manual);
        assert!(!first_handoff.review_mode_unproven);
        assert_eq!(agent.launches("claude"), 1, "one coding run and no reviewer");

        // A person asks for changes. The Task goes back to Queue with its
        // feedback, and its checkout and branch are kept.
        let queued = request_changes(&state, task_id, "Also count lines.")
            .await
            .expect("changes requested");
        assert_eq!(queued.status, TaskStatus::Queue);

        // Run 2: the poller starts it again through the same executor path.
        let second = poll_until(&state, task_id, TaskStatus::HumanReview).await;
        assert_eq!(second.status, TaskStatus::HumanReview, "{:?}", second.error_message);
        assert!(second.qa_signoff.is_none(), "no AI verdict is invented");
        let handoff = second.worker_handoff.clone().expect("run 2 has its own record");
        assert_eq!(handoff.run, Some(2), "the record is numbered as the second run");
        assert_eq!(handoff.review_mode, ReviewMode::Manual, "Manual is fixed at run 2's start");
        assert!(!handoff.review_mode_unproven, "the record is proven, not rebuilt");
        assert_ne!(
            handoff.start_head, first_handoff.start_head,
            "a new record, started from the commit run 1 made"
        );
        assert_eq!(agent.launches("claude"), 2, "two coding runs, and still no reviewer");
        assert!(second.activity.iter().any(|e| matches!(e.kind, ActivityKind::RunStarted { run: 2, .. })));
        assert!(
            !second.activity.iter().any(|e| matches!(e.kind, ActivityKind::AiReviewStarted { .. })),
            "no AI review ran on either run"
        );
        let skipped: Vec<u32> = second
            .activity
            .iter()
            .filter_map(|e| match e.kind {
                ActivityKind::AiReviewSkipped { review, .. } => Some(review),
                _ => None,
            })
            .collect();
        assert_eq!(skipped, [1, 2], "each Manual run records its skipped AI review once");
        let checkout = second.worktree_path.clone().expect("the checkout is kept");
        assert_eq!(git(Path::new(&checkout), &["show", "HEAD:agent_work.txt"]), "run 2");

        // The records survive a reload from the board on disk.
        let persisted = on_disk(&state, project_id, task_id);
        assert_eq!(persisted.status, TaskStatus::HumanReview);
        assert_eq!(persisted.review_mode, ReviewMode::Manual);
        let persisted_handoff = persisted.worker_handoff.expect("run 2's record is durable");
        assert_eq!(persisted_handoff.run, Some(2));
        assert_eq!(persisted_handoff.review_mode, ReviewMode::Manual);
        assert_eq!(persisted_handoff.start_head, handoff.start_head);
        assert_eq!(persisted.human_review.entries.len(), 1, "one change request was recorded");
    }

    /// Changes cannot be requested while an earlier cleanup of the Task's
    /// checkout is unfinished: the Task would enter Queue with that work still
    /// pending. Nothing is recorded, and the Task stays in Human Review.
    #[tokio::test]
    async fn changes_cannot_be_requested_while_a_cleanup_is_unfinished() {
        let (tmp, state) = test_state().await;
        let (project_id, task_id) = seed_in_review(&state, &tmp).await;
        let mut task = on_disk(&state, project_id, task_id);
        task.cleanup_in_flight = true;
        state.task.tasks.write().await.insert(task_id, task.clone());
        state.storage.save_project_tasks(project_id, &[task]).expect("seed the board");
        let before = on_disk(&state, project_id, task_id);
        let executor = crate::test_helpers::attach_test_executor(&state);
        let cleaned_up = executor.register_fake_running_execution_for_test(task_id).await;

        let Err(refusal) = request_changes(&state, task_id, "Also count lines.").await else {
            panic!("a Task with unfinished cleanup must not be queued by a review decision");
        };

        assert!(refusal.contains("cleanup"), "{refusal}");
        assert!(
            !cleaned_up.load(std::sync::atomic::Ordering::SeqCst),
            "a request that will be refused must not end the owner"
        );
        let after = on_disk(&state, project_id, task_id);
        assert_eq!(after.status, TaskStatus::HumanReview);
        assert_eq!(after.human_review.entries.len(), before.human_review.entries.len());
        assert!(after.human_review.pending_feedback().is_empty(), "no feedback is recorded");
        assert_eq!(state.task.tasks.read().await[&task_id].status, TaskStatus::HumanReview);
    }

    /// Requesting changes is a decision, not new work, so it is recorded on
    /// a critically low disk. The rerun it asks for is new coding work, and
    /// waits in Queue like any other until space returns.
    #[tokio::test]
    async fn changes_can_be_requested_on_a_critical_disk_and_the_rerun_waits() {
        use crate::domain::storage_usage::GIB;
        use tauri::Manager;
        let disk = crate::test_helpers::FakeDisk::with_available(10 * GIB);
        let (tmp, mut state) = test_state().await;
        state.start_guard = disk.guard();
        let (project_id, task_id) = seed_in_review(&state, &tmp).await;
        // The reviewed run's provider has exited: its lease is there and shows no group.
        let lease_path = crate::queue::task_provider_lease_path(&state.storage, task_id);
        std::fs::create_dir_all(lease_path.parent().unwrap()).unwrap();
        std::fs::write(&lease_path, b"").unwrap();

        let task = request_changes(&state, task_id, "Also count lines.").await.expect("recorded while paused");
        assert_eq!(task.status, TaskStatus::Queue);
        assert_eq!(on_disk(&state, project_id, task_id).status, TaskStatus::Queue);

        crate::test_helpers::attach_test_executor(&state);
        let app = tauri::test::mock_app();
        app.manage(state);
        let waiting = crate::commands::queue::promote_next_task(GuiMutation::untracked(), app.state()).await;
        assert!(waiting.as_ref().is_err_and(|m| m.starts_with("New work paused")), "{waiting:?}");
        let state: tauri::State<'_, crate::AppState> = app.state();
        assert_eq!(on_disk(&state, project_id, task_id).status, TaskStatus::Queue);

        disk.set_available(300 * GIB);
        let promoted = crate::commands::queue::promote_next_task(GuiMutation::untracked(), app.state()).await.expect("promoted");
        assert_eq!(promoted, Some(task_id.to_string()));
    }

    #[tokio::test]
    async fn blank_feedback_is_refused_and_nothing_changes() {
        let (tmp, state) = test_state().await;
        let (project_id, task_id) = seed_in_review(&state, &tmp).await;
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
        let (tmp, state) = test_state().await;
        let (_project_id, approved) = seed_in_review(&state, &tmp).await;
        approve_shown(&state, approved, false).await.expect("approved");
        let refused = request_changes(&state, approved, "change it").await.expect_err("approved");
        assert!(refused.contains("already approved"), "{refused}");
        let task = current(&state, approved).await.unwrap();
        assert_eq!(task.status, TaskStatus::HumanReview);
        assert_eq!(task.human_review.entries.len(), 1);

        let (_project_id, sent_back) = seed_in_review(&state, &tmp).await;
        request_changes(&state, sent_back, "change it").await.expect("recorded");
        let refused = approve_shown(&state, sent_back, false).await.expect_err("queued now");
        assert!(refused.contains("Human Review"), "{refused}");
    }

    /// Sending approved changes back to work retires the approval, through
    /// every front door that does it, so a manual move back into Human
    /// Review cannot open a pull request for commits nobody reviewed.
    #[tokio::test]
    async fn an_approval_does_not_survive_the_task_going_back_to_work() {
        use tauri::Manager;
        let (tmp, state) = test_state().await;
        let (_p, moved) = seed_in_review(&state, &tmp).await;
        let (_p, queued) = seed_in_review(&state, &tmp).await;
        approve_shown(&state, moved, false).await.expect("approved");
        approve_shown(&state, queued, false).await.expect("approved");
        let app = tauri::test::mock_app();
        app.manage(state);

        for status in [TaskStatus::Queue, TaskStatus::HumanReview] {
            crate::commands::task::update_task_status(GuiMutation::untracked(), app.state(), moved.to_string(), status, None)
                .await
                .expect("moved");
        }
        crate::commands::queue::add_to_queue(GuiMutation::untracked(), app.state(), queued.to_string()).await.expect("queued");
        crate::commands::task::update_task_status(GuiMutation::untracked(), app.state(), queued.to_string(), TaskStatus::HumanReview, None)
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
        let (tmp, state) = test_state().await;
        let (_p, task_id) = seed_in_review(&state, &tmp).await;
        request_changes(&state, task_id, "fix it").await.expect("recorded");
        // Put back in Human Review without a run, as a manual move would.
        state.task.tasks.write().await.get_mut(&task_id).unwrap().status = TaskStatus::HumanReview;
        let refused = approve_shown(&state, task_id, false).await.expect_err("changes were requested");
        assert!(refused.contains("already requested"), "{refused}");
    }

    /// A task that comes back from the rerun is a new review: nothing is
    /// decided yet, and the history keeps the earlier request.
    #[tokio::test]
    async fn a_later_review_starts_undecided_and_keeps_the_earlier_feedback() {
        let (tmp, state) = test_state().await;
        let (_project_id, task_id) = seed_in_review(&state, &tmp).await;
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

    /// jj with a fixed identity, for the commits these tests make with jj.
    fn jj(dir: &std::path::Path, args: &[&str]) -> String {
        let mut all = vec!["--config", "user.name=Test", "--config", "user.email=test@example.com"];
        all.extend_from_slice(args);
        let out = std::process::Command::new("jj")
            .args(&all)
            .current_dir(dir)
            .output()
            .expect("run jj");
        assert!(out.status.success(), "jj {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The commit `refs/heads/<branch>` names in the backing Git directory of
    /// a jj workspace, if it is there at all.
    fn git_ref_at(git_dir: &std::path::Path, branch: &str, cwd: &std::path::Path) -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(["for-each-ref", "--format=%(objectname)", &format!("refs/heads/{branch}")])
            .current_dir(cwd)
            .output()
            .expect("for-each-ref");
        let listed = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!listed.is_empty()).then_some(listed)
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

    /// A repository on `main` with one commit and no remote, and a project
    /// that uses it with a local branch as its base.
    async fn repo_and_project(
        state: &crate::AppState,
        tmp: &tempfile::TempDir,
    ) -> (std::path::PathBuf, Uuid, crate::domain::project::ProjectBase) {
        // A directory of its own, so one test can seed several projects.
        let repo = tmp.path().join(format!("repo-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        // Repository-local identity: the executor's commits read it, and the
        // test environment has no global configuration.
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
                removed_at: None,
            },
        );
        (repo, project_id, base)
    }

    async fn local_project() -> Local {
        let (tmp, state) = test_state().await;
        let (repo, project_id, base) = repo_and_project(&state, &tmp).await;

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
        task.base_commit = Some(git(&repo, &["rev-parse", "main"]));
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
            approve_shown(&self.state, self.task_id, false).await.expect("approved");
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
        task.activity.iter().filter(|e| e.kind == slashit_activity::Kind::CompletedLocally {}).count()
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
    async fn completed_local_task_still_shows_the_approved_changes() {
        let local = local_project().await;
        local.approve().await;
        let before = crate::commands::jj::fetch_task_diff(&local.state, local.task_id)
            .await
            .expect("diff while checkout exists");
        assert!(before.patch.contains("work.txt"), "{}", before.patch);

        local.complete().await.expect("completed locally");
        assert!(!local.worktree.exists(), "the original diff source is gone");
        let after = crate::commands::jj::fetch_task_diff(&local.state, local.task_id)
            .await
            .expect("diff from the approved commit");
        assert_eq!(after, before);

        // A later branch move must not silently change what the person saw
        // and approved. The approved commit remains available by its SHA.
        git(&local.repo, &["branch", "-f", &local.branch, "main"]);
        let after_branch_move = crate::commands::jj::fetch_task_diff(&local.state, local.task_id)
            .await
            .expect("diff still bound to the approved commit");
        assert_eq!(after_branch_move, before);
    }

    /// Direction 1: an edit that was never committed is not part of an
    /// approval, so it is not in the diff the person decides on.
    #[tokio::test]
    async fn an_uncommitted_addition_is_listed_apart_and_not_in_what_is_approved() {
        let local = local_project().await;
        std::fs::write(local.worktree.join("extra.txt"), "never committed\n").unwrap();
        std::fs::write(local.worktree.join("work.txt"), "work\nunstaged line\n").unwrap();

        let shown = crate::commands::jj::fetch_review_changes(&local.state, local.task_id)
            .await
            .expect("readable")
            .expect("a Task in Human Review");
        assert!(shown.patch.contains("work.txt") && !shown.patch.contains("extra.txt"), "{}", shown.patch);
        assert!(!shown.patch.contains("unstaged line"), "{}", shown.patch);
        assert!(shown.pending_patch.contains("extra.txt") && shown.pending_patch.contains("unstaged line"));
        assert!(!shown.pending_patch.contains("+work\n"), "committed content is not repeated as pending");

        // The plain diff commands show the same primary content.
        let plain = crate::commands::jj::fetch_task_diff(&local.state, local.task_id).await.unwrap();
        assert_eq!((plain.patch, plain.stat), (shown.patch.clone(), shown.stat.clone()));

        // Pending edits do not stop an approval, and it binds the shown commit.
        approve(&local.state, local.task_id, false, Some(&shown.tip)).await.expect("approved");
        assert_eq!(local.on_disk().human_review.approved_tip(), Some(shown.tip.as_str()));
        assert!(local.worktree.join("extra.txt").exists(), "nothing touched the checkout");
    }

    /// Direction 2, the one that matters: an uncommitted edit that undoes
    /// committed content must not hide that content from the person deciding.
    #[tokio::test]
    async fn an_uncommitted_edit_that_undoes_committed_work_cannot_hide_it() {
        let local = local_project().await;
        std::fs::remove_file(local.worktree.join("work.txt")).unwrap();

        let shown = crate::commands::jj::fetch_review_changes(&local.state, local.task_id)
            .await
            .unwrap()
            .expect("a Task in Human Review");
        assert!(shown.patch.contains("+work"), "the committed content is shown: {}", shown.patch);
        assert!(shown.pending_patch.contains("-work"), "the deletion is shown as pending: {}", shown.pending_patch);
        let plain = crate::commands::jj::fetch_task_diff(&local.state, local.task_id).await.unwrap();
        assert!(plain.patch.contains("+work"), "{}", plain.patch);
    }

    /// The commit shown and the commit approved cannot drift apart: what was
    /// shown carries its tip, and a branch that moved since is refused.
    #[tokio::test]
    async fn approving_a_commit_that_is_no_longer_the_one_shown_is_refused() {
        let local = local_project().await;
        let shown = crate::commands::jj::fetch_review_changes(&local.state, local.task_id)
            .await
            .unwrap()
            .unwrap();
        std::fs::write(local.worktree.join("later.txt"), "later\n").unwrap();
        git(&local.worktree, &["add", "."]);
        git(&local.worktree, &["commit", "-q", "-m", "later"]);

        let refused = approve(&local.state, local.task_id, false, Some(&shown.tip)).await.expect_err("moved");
        assert!(!refused.is_empty());
        assert!(local.on_disk().human_review.approved_tip().is_none(), "nothing was recorded");
        let now = crate::commands::jj::fetch_review_changes(&local.state, local.task_id).await.unwrap().unwrap();
        assert_ne!(now.tip, shown.tip);
        assert!(now.patch.contains("later.txt"));
        approve(&local.state, local.task_id, false, Some(&now.tip)).await.expect("the new commit, as shown");
    }

    /// A Task in Human Review whose branch commit cannot be read shows an error,
    /// never the working tree as if it were the approved commit.
    #[tokio::test]
    async fn an_unreadable_review_commit_is_an_error_not_the_working_tree() {
        let local = local_project().await;
        {
            let mut tasks = local.state.task.tasks.write().await;
            tasks.get_mut(&local.task_id).unwrap().branch_name = Some("no-such-branch".into());
        }
        assert!(crate::commands::jj::fetch_review_changes(&local.state, local.task_id).await.is_err());
        assert!(crate::commands::jj::fetch_task_diff(&local.state, local.task_id).await.is_err());
    }

    #[tokio::test]
    async fn there_are_no_review_changes_outside_human_review() {
        let local = local_project().await;
        {
            let mut tasks = local.state.task.tasks.write().await;
            tasks.get_mut(&local.task_id).unwrap().status = TaskStatus::InProgress;
        }
        assert_eq!(crate::commands::jj::fetch_review_changes(&local.state, local.task_id).await, Ok(None));
        // The checkout's own diff is still the working-tree one for a running Task.
        std::fs::write(local.worktree.join("extra.txt"), "x\n").unwrap();
        let plain = crate::commands::jj::fetch_task_diff(&local.state, local.task_id).await.unwrap();
        assert!(plain.patch.contains("extra.txt"));
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
        // Committing it changed the branch, which the approval did not cover.
        let refused = local.complete().await.expect_err("a commit nobody approved");
        assert!(refused.contains("changed since"), "{refused}");
        approve_again(&local.state, local.task_id, &local.tip()).await.expect("approved as it is now");
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
        let (tmp, state) = test_state().await;
        let (project_id, task_id) = seed_in_review(&state, &tmp).await;
        // Without a branch there is no commit to show, so none is required.
        state.task.tasks.write().await.get_mut(&task_id).expect("seeded").branch_name = None;
        approve_shown(&state, task_id, false).await.expect("approved");
        let task = on_disk(&state, project_id, task_id);
        assert!(task.human_review.is_approved());
        assert_eq!(task.human_review.approved_tip(), None);
    }

    /// A Task that never had a branch can be moved into Human Review, approved and
    /// linked to a pull request, each by the command that does it. Its approval then
    /// names no commit, and the status still reports the pull request it is linked to.
    #[tokio::test]
    async fn a_branchless_task_linked_to_a_pull_request_has_no_commit_to_cover_it() {
        use tauri::Manager;
        let (tmp, state) = test_state().await;
        let (_repo, project_id, _base) = repo_and_project(&state, &tmp).await;
        let mut task = create_test_task_full("never run", project_id, TaskStatus::Backlog, 0);
        task.branch_name = None;
        let task_id = task.id;
        state.task.tasks.write().await.insert(task_id, task.clone());
        state.storage.save_project_tasks(project_id, &[task]).expect("seed the board");

        crate::commands::task::set_task_status(&state, task_id, TaskStatus::HumanReview, None)
            .await
            .expect("a Backlog task can be moved into Human Review");
        approve_shown(&state, task_id, false).await.expect("approved without a branch");

        let app = tauri::test::mock_app();
        app.manage(state);
        crate::commands::task::link_pr(
            GuiMutation::untracked(),
            app.state(),
            task_id.to_string(),
            "https://github.com/BarraDev/slashit/pull/7".to_string(),
        )
        .await
        .expect("a pull request can be linked by URL");

        let state = app.state::<crate::AppState>();
        let linked = on_disk(&state, project_id, task_id);
        assert!(linked.human_review.is_approved(), "the approval is recorded");
        assert_eq!(linked.human_review.approved_tip(), None, "no commit was bound to it");
        let status = approval_status(&state, task_id).await.expect("the status is read");
        assert_eq!(status.coverage, ApprovalCoverage::NotApplicable);
        assert_eq!(status.approved_tip, None);
        assert!(status.published.is_some(), "the linked pull request is reported, not hidden");
        assert_eq!(
            status.pull_request.as_deref(),
            Some("https://github.com/BarraDev/slashit/pull/7"),
            "the answer names the pull request it was read for, so a later link change is seen"
        );
    }

    // ---- Approval is bound to the commit that was approved ----

    /// A pull request pushed to by another clone, seen the way the status poll
    /// sees it. Clone A approves commit A and pushes it. Another clone then pushes
    /// B to the same branch, and clone A never fetches. The branch still names
    /// A, so the local answer alone says the approval is current. Only GitHub's
    /// answer, through the status poll, names B. When GitHub cannot answer, the
    /// head is unverified, and the next answer restores it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pull_request_pushed_to_by_another_clone_is_seen_only_through_the_status_poll() {
        use crate::domain::ExternalRef;
        use crate::pr_status::{PrKey, PrStatuses};
        use std::os::unix::fs::PermissionsExt;

        let mut local = local_project().await;
        local.approve().await;
        let approved = local.tip();
        let root = local._tmp.path().to_path_buf();

        // Clone A, the Task checkout's repository, publishes its branch.
        let remote = root.join("remote.git");
        git(&root, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        git(&local.repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git(&local.repo, &["push", "-q", "origin", &local.branch]);

        // Another clone pushes B to the pull request's branch.
        let other = root.join("other");
        git(&root, &["clone", "-q", remote.to_str().unwrap(), other.to_str().unwrap()]);
        git(&other, &["config", "user.email", "test@example.com"]);
        git(&other, &["config", "user.name", "Test"]);
        git(&other, &["checkout", "-q", &local.branch]);
        std::fs::write(other.join("pushed.txt"), "from another clone\n").unwrap();
        git(&other, &["add", "."]);
        git(&other, &["commit", "-q", "-m", "pushed by another clone"]);
        git(&other, &["push", "-q", "origin", &local.branch]);
        let head = git(&other, &["rev-parse", "HEAD"]);
        assert_ne!(head, approved);
        assert_eq!(
            git(&local.repo, &["rev-parse", &format!("refs/remotes/origin/{}", local.branch)]),
            approved,
            "clone A has not fetched: its remote-tracking ref still names A"
        );

        // The Task's pull request, and a fake `gh` that answers for it.
        let key = PrKey::new("o/r", 9);
        {
            let mut tasks = local.state.task.tasks.write().await;
            let task = tasks.get_mut(&local.task_id).unwrap();
            let url = "https://github.com/o/r/pull/9".to_string();
            task.pr_url = Some(url.clone());
            task.external_refs.push(ExternalRef::GithubPr {
                url,
                repo: "o/r".into(),
                number: 9,
                state: Some("OPEN".into()),
                merge_commit: None,
            });
        }
        let dir = root.join("gh");
        std::fs::create_dir_all(&dir).unwrap();
        let answer = serde_json::json!({
            "state": "OPEN",
            "headRefOid": head,
            "statusCheckRollup": [],
            "reviewDecision": "",
            "mergeable": "MERGEABLE",
        });
        std::fs::write(dir.join("answer.json"), answer.to_string()).unwrap();
        let gh = dir.join("gh");
        std::fs::write(
            &gh,
            "#!/bin/sh\nd=\"$(dirname \"$0\")\"\nif [ -f \"$d/fail\" ]; then echo 'connection reset' >&2; exit 1; fi\ncat \"$d/answer.json\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        local.state.pr_statuses = std::sync::Arc::new(PrStatuses::with_program(gh, std::time::Duration::from_secs(20)));
        let published = |status: &ApprovalStatus| status.published.clone().expect("the pull request is recorded");

        // Before the poll has asked, nothing says what the pull request is.
        let before = approval_status(&local.state, local.task_id).await.unwrap();
        assert_eq!(before.coverage, ApprovalCoverage::Current, "the local branch alone reads current");
        assert_eq!(published(&before).head, None);
        assert!(published(&before).unverified.is_some());

        // The poll asks GitHub: the head is B, while the approval and the branch stay at A.
        local.state.pr_statuses.refresh(&key).await.expect("GitHub answers");
        let status = approval_status(&local.state, local.task_id).await.unwrap();
        assert_eq!(status.coverage, ApprovalCoverage::Current);
        assert_eq!(status.approved_tip.as_deref(), Some(approved.as_str()));
        assert_eq!(published(&status).head.as_deref(), Some(head.as_str()));
        assert!(published(&status).observed_at.is_some());
        assert_eq!(published(&status).unverified, None);

        // GitHub cannot answer: the head is unverified, and the last head is not offered.
        std::fs::write(dir.join("fail"), "").unwrap();
        assert!(local.state.pr_statuses.refresh(&key).await.is_err());
        let failed = approval_status(&local.state, local.task_id).await.unwrap();
        assert_eq!(published(&failed).head, None, "a failed answer never keeps the earlier head as current");
        assert!(published(&failed).unverified.is_some_and(|why| why.contains("could not be asked")));

        // GitHub answers again.
        std::fs::remove_file(dir.join("fail")).unwrap();
        local.state.pr_statuses.refresh(&key).await.expect("GitHub answers again");
        assert_eq!(published(&approval_status(&local.state, local.task_id).await.unwrap()).head.as_deref(), Some(head.as_str()));

        // Nothing was fetched or moved to make that observation.
        assert_eq!(local.tip(), approved);
        assert_eq!(
            git(&local.repo, &["rev-parse", &format!("refs/remotes/origin/{}", local.branch)]),
            approved
        );
    }

    impl Local {
        fn tip(&self) -> String {
            git(&self.repo, &["rev-parse", &format!("refs/heads/{}", self.branch)])
        }
        /// A later commit on the Task branch, made in its checkout.
        fn advance(&self) -> String {
            std::fs::write(self.worktree.join("later.txt"), "later\n").unwrap();
            git(&self.worktree, &["add", "."]);
            git(&self.worktree, &["commit", "-q", "-m", "after the approval"]);
            self.tip()
        }
        /// The approved commit replaced by another, so it is no longer on the branch.
        fn rewrite(&self) -> String {
            git(&self.worktree, &["commit", "-q", "--amend", "-m", "rewritten after the approval"]);
            self.tip()
        }
    }

    #[tokio::test]
    async fn a_branch_that_advanced_after_approval_is_not_completed_locally() {
        let local = local_project().await;
        local.approve().await;
        let approved = local.tip();
        let later = local.advance();

        let refused = local.complete().await.expect_err("the branch moved");
        assert!(refused.contains("changed since"), "{refused}");
        local.untouched().await;
        assert_eq!(local.tip(), later, "no commit is discarded");
        let task = local.on_disk();
        assert_eq!(task.human_review.approved_tip(), Some(approved.as_str()), "the approval stays as it was");
        assert_eq!(task.human_review.entries.len(), 1);
    }

    #[tokio::test]
    async fn a_branch_rewritten_after_approval_is_not_completed_locally() {
        let local = local_project().await;
        local.approve().await;
        let approved = local.tip();
        let rewritten = local.rewrite();
        assert_ne!(approved, rewritten);

        let refused = local.complete().await.expect_err("the approved commit is gone");
        assert!(refused.contains("changed since"), "{refused}");
        local.untouched().await;
        assert_eq!(local.tip(), rewritten);
        assert_eq!(local.on_disk().human_review.approved_tip(), Some(approved.as_str()));
    }

    #[tokio::test]
    async fn an_approval_without_a_recorded_commit_is_not_completed_locally_for_a_task_with_a_branch() {
        let local = local_project().await;
        local.approve().await;
        {
            let mut tasks = local.state.task.tasks.write().await;
            let entry = &mut tasks.get_mut(&local.task_id).unwrap().human_review.entries[0];
            entry.approved_tip = None;
        }
        let refused = local.complete().await.expect_err("legacy approval");
        assert!(refused.contains("which commit"), "{refused}");
        local.untouched().await;
    }

    #[tokio::test]
    async fn approving_again_needs_the_commit_that_was_shown_and_changes_nothing_else() {
        let local = local_project().await;
        local.approve().await;
        let approved = local.tip();

        // Still at the approved commit: nothing to renew.
        let same = approve_again(&local.state, local.task_id, &approved).await.expect("no-op");
        assert_eq!(same.human_review.entries.len(), 1);

        let later = local.advance();
        let status = approval_status(&local.state, local.task_id).await.unwrap();
        assert_eq!(status.coverage, ApprovalCoverage::Moved);
        assert_eq!(status.rewritten, Some(false));
        assert_eq!(status.branch_tip.as_deref(), Some(later.as_str()));
        assert_eq!(status.approved_tip.as_deref(), Some(approved.as_str()));

        // A commit other than the one on the branch now is refused.
        let stale = approve_again(&local.state, local.task_id, &approved).await.expect_err("not the tip");
        assert!(stale.contains("moved after it was shown"), "{stale}");
        assert_eq!(local.on_disk().human_review.entries.len(), 1);

        let renewed = approve_again(&local.state, local.task_id, &later).await.expect("approved");
        assert_eq!(renewed.human_review.approved_tip(), Some(later.as_str()));
        assert_eq!(local.on_disk().human_review.entries.len(), 2);
        assert_eq!(approval_status(&local.state, local.task_id).await.unwrap().coverage, ApprovalCoverage::Current);
        let done = local.complete().await.expect("the renewed approval completes it");
        assert_eq!(done.status, TaskStatus::Done);
    }

    /// The stale-approval requirement, proven end to end: approve commit A,
    /// let the branch move to B outside SlashIt, and record what the state
    /// reports. The approval is reported stale, named by both commits, and an
    /// earlier approval stays in the history but is never shown as current:
    /// when the branch moves again, it is the later approval that is reported.
    #[tokio::test]
    async fn an_approval_older_than_the_branch_is_reported_stale_with_both_commits() {
        let local = local_project().await;
        let shown = local.tip();
        local.approve().await;
        let approved = shown.clone();

        // The branch moved outside SlashIt: no task record changed.
        let moved = local.advance();
        assert_ne!(approved, moved);
        let status = approval_status(&local.state, local.task_id).await.unwrap();
        assert_eq!(status.coverage, ApprovalCoverage::Moved);
        assert_eq!(status.approved_tip.as_deref(), Some(approved.as_str()), "the approval covers A, and says so");
        assert_eq!(status.branch_tip.as_deref(), Some(moved.as_str()), "the branch is at B, and that is reported");
        assert_eq!(status.rewritten, Some(false), "A is still on the branch");
        let reason = status.reason.clone().unwrap_or_default();
        assert!(reason.contains(&approved[..12]) && reason.contains(&moved[..12]), "{reason}");

        // Approving the branch as it is now makes that approval the current
        // one; the earlier approval stays in the history.
        approve_again(&local.state, local.task_id, &moved).await.expect("approved");
        let task = local.on_disk();
        assert_eq!(task.human_review.approved_tip(), Some(moved.as_str()), "the renewed approval is the current one");
        assert!(
            task.human_review.entries.iter().any(|e| e.approved_tip.as_deref() == Some(approved.as_str())),
            "the earlier approval is kept in the history"
        );
        assert_eq!(approval_status(&local.state, local.task_id).await.unwrap().coverage, ApprovalCoverage::Current);

        // The branch moves again: the renewed approval is the one reported
        // stale; the first is never shown as current.
        std::fs::write(local.worktree.join("yet_later.txt"), "yet later\n").unwrap();
        git(&local.worktree, &["add", "."]);
        git(&local.worktree, &["commit", "-q", "-m", "moved again"]);
        let later = local.tip();
        let status = approval_status(&local.state, local.task_id).await.unwrap();
        assert_eq!(status.coverage, ApprovalCoverage::Moved);
        assert_eq!(status.approved_tip.as_deref(), Some(moved.as_str()));
        assert_ne!(status.approved_tip.as_deref(), Some(approved.as_str()), "an earlier approval is never the current one");
        assert_eq!(status.branch_tip.as_deref(), Some(later.as_str()));
        let reason = status.reason.clone().unwrap_or_default();
        assert!(reason.contains(&moved[..12]) && reason.contains(&later[..12]), "{reason}");
    }

    /// The race the drawer can lose: the person is shown one commit, the
    /// branch moves to another, and then they press Approve. The approval
    /// must not record the commit they never saw, and the next approval must
    /// record the one they are shown after a fresh inspection.
    #[tokio::test]
    async fn an_approval_is_refused_when_the_branch_moved_after_it_was_shown() {
        let local = local_project().await;
        let shown = local.tip();
        let moved = local.advance();
        assert_ne!(shown, moved);

        let refused = approve(&local.state, local.task_id, false, Some(&shown))
            .await
            .expect_err("the branch moved after it was shown");
        assert!(refused.contains("moved after it was shown"), "{refused}");
        local.untouched().await;
        assert_eq!(local.tip(), moved, "the branch is left where it is");
        let task = local.on_disk();
        assert!(!task.human_review.is_approved(), "nothing was approved");
        assert!(task.human_review.entries.is_empty(), "no decision was recorded");

        // The approval that carries no commit at all is refused the same way.
        let unshown = approve(&local.state, local.task_id, false, None)
            .await
            .expect_err("no commit was shown");
        assert!(unshown.contains("was not shown"), "{unshown}");
        assert!(!local.on_disk().human_review.is_approved());

        // A fresh inspection shows the moved commit, and approving it records it.
        assert_eq!(approval_status(&local.state, local.task_id).await.unwrap().branch_tip.as_deref(), Some(moved.as_str()));
        approve(&local.state, local.task_id, false, Some(&moved)).await.expect("approved as shown");
        assert_eq!(local.on_disk().human_review.approved_tip(), Some(moved.as_str()));
    }

    #[tokio::test]
    async fn an_undecided_task_names_the_commit_an_approval_would_record() {
        let local = local_project().await;
        let status = approval_status(&local.state, local.task_id).await.unwrap();
        assert_eq!(status.coverage, ApprovalCoverage::NotApplicable);
        assert!(status.approved_tip.is_none());
        assert_eq!(status.branch_tip.as_deref(), Some(local.tip().as_str()));
    }

    /// A commit that cannot be read is an error. It must not look like a
    /// branch that has no commit, which the panel shows without explanation.
    #[tokio::test]
    async fn an_undecided_task_whose_commit_cannot_be_read_is_an_error() {
        let local = local_project().await;
        let not_a_repository = tempfile::tempdir().unwrap();
        for repo in local.state.repository.repositories.write().await.values_mut() {
            repo.local_path = not_a_repository.path().to_str().unwrap().to_string();
        }
        assert!(approval_status(&local.state, local.task_id).await.is_err());
    }

    /// A branch that does not exist is a successful read with no commit.
    #[tokio::test]
    async fn an_undecided_task_whose_branch_is_absent_names_no_commit_without_error() {
        let local = local_project().await;
        local.state.task.tasks.write().await.get_mut(&local.task_id).unwrap().branch_name = Some("no-such-branch".into());
        let status = approval_status(&local.state, local.task_id).await.unwrap();
        assert_eq!(status.coverage, ApprovalCoverage::NotApplicable);
        assert!(status.branch_tip.is_none());
    }

    #[tokio::test]
    async fn a_rewritten_branch_is_reported_and_can_be_approved_as_it_is() {
        let local = local_project().await;
        local.approve().await;
        let rewritten = local.rewrite();
        let status = approval_status(&local.state, local.task_id).await.unwrap();
        assert_eq!((status.coverage, status.rewritten), (ApprovalCoverage::Moved, Some(true)));
        approve_again(&local.state, local.task_id, &rewritten).await.expect("approved");
    }

    /// The read-only read answers in a jj repository too, where the branch's
    /// commit now comes from the bookmark rather than from Git's refs. An
    /// approval made through the export path recorded what the export had put
    /// into Git's refs; when the bookmark moves with jj alone afterwards, a
    /// stale approval must be reported against the bookmark's commit. Against
    /// Git's stale ref it would not even be stale, and the approval would
    /// read as current over changes nobody reviewed.
    #[tokio::test]
    async fn a_stale_approval_in_a_jj_repository_is_reported_against_the_bookmark() {
        let (tmp, state) = test_state().await;
        // A non-colocated jj workspace: `git` cannot discover its backing
        // repository from the workspace, so a ref read there is a read of the
        // bookmark's export, never of something git can reach on its own.
        let ws = tmp.path().join(format!("jj-{}", Uuid::new_v4()));
        jj(tmp.path(), &["git", "init", "--no-colocate", ws.to_str().unwrap()]);
        std::fs::write(ws.join("README.md"), "base\n").unwrap();
        jj(&ws, &["commit", "-m", "task commit"]);
        jj(&ws, &["bookmark", "create", "task-branch", "-r", "@-"]);
        let approved = jj(&ws, &["--ignore-working-copy", "log", "--no-graph", "-T", "commit_id", "-r", "task-branch"]);

        // A project whose repository is that workspace, and a task in Human
        // Review whose branch is the bookmark.
        let repository_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        state.repository.repositories.write().await.insert(
            repository_id,
            crate::domain::Repository {
                id: repository_id,
                local_path: ws.to_str().unwrap().to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        state.project.projects.write().await.insert(
            project_id,
            crate::domain::Project {
                id: project_id,
                name: "jj".to_string(),
                repository_id: Some(repository_id),
                scope: crate::domain::project::ProjectScope::Standalone,
                state_location: crate::config::paths::StateLocation::External,
                base: Some(crate::domain::project::ProjectBase::LocalBranch {
                    branch: "task-branch".to_string(),
                }),
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
                removed_at: None,
            },
        );
        let mut task = create_test_task_full("stale in jj", project_id, TaskStatus::HumanReview, 0);
        task.branch_name = Some("task-branch".to_string());
        task.human_review.record_arrival();
        let task_id = task.id;
        state.task.tasks.write().await.insert(task_id, task.clone());
        state.storage.save_project_tasks(project_id, &[task]).expect("seed the board");

        // The approval is made through the export path, as a delivery would
        // read the branch: it records the commit the export put into Git's
        // refs.
        approve_shown(&state, task_id, false).await.expect("approved");
        assert_eq!(on_disk(&state, project_id, task_id).human_review.approved_tip(), Some(approved.as_str()));

        // The bookmark moves with jj alone, as a person working in jj does,
        // and the export that recorded the approval does not happen again:
        // Git's refs still hold the approved commit.
        jj(&ws, &["--ignore-working-copy", "new", "-m", "moved on"]);
        jj(&ws, &["--ignore-working-copy", "bookmark", "set", "task-branch", "-r", "@-"]);
        let moved = jj(&ws, &["--ignore-working-copy", "log", "--no-graph", "-T", "commit_id", "-r", "task-branch"]);
        assert_ne!(moved, approved, "the fixture must have moved the bookmark");
        let git_dir = ws.join(".jj/repo/store/git");
        assert_eq!(
            git_ref_at(&git_dir, "task-branch", tmp.path()),
            Some(approved.clone()),
            "the fixture must have left Git's ref at the approved commit"
        );

        // The read-only status answers the bookmark's commit, so the approval
        // is reported stale against the branch as it is now — and the read
        // exports nothing, being one the panel may ask on a cadence.
        let status = approval_status(&state, task_id).await.unwrap();
        assert_eq!(status.coverage, ApprovalCoverage::Moved);
        assert_eq!(status.approved_tip.as_deref(), Some(approved.as_str()), "the approval covers what it recorded");
        assert_eq!(status.branch_tip.as_deref(), Some(moved.as_str()), "the branch is where the bookmark is now");
        assert_eq!(status.rewritten, Some(false), "the approved commit is still on the branch");
        let reason = status.reason.clone().unwrap_or_default();
        assert!(reason.contains(&approved[..12]) && reason.contains(&moved[..12]), "{reason}");
        assert_eq!(
            git_ref_at(&git_dir, "task-branch", tmp.path()),
            Some(approved),
            "the read must not export the bookmark into Git's refs"
        );
    }

    #[tokio::test]
    async fn approving_again_is_refused_while_an_integration_uses_the_earlier_approval() {
        let local = local_project().await;
        local.approve().await;
        local.advance();
        {
            let mut tasks = local.state.task.tasks.write().await;
            let t = tasks.get_mut(&local.task_id).unwrap();
            t.local_integration = Some(crate::domain::integration::LocalIntegration {
                state: crate::domain::integration::IntegrationState::Pending,
                target_branch: "main".into(),
                target_old_oid: "a".repeat(40),
                approved_tip: "b".repeat(40),
                result_oid: "c".repeat(40),
                strategy: crate::domain::integration::IntegrationStrategy::FastForward,
                checkout_path: None,
                started_at: chrono::Utc::now(),
                completed_at: None,
                completes_task: true,
            });
        }
        let refused = approve_again(&local.state, local.task_id, &local.tip()).await.expect_err("integrating");
        assert!(refused.contains("integration"), "{refused}");
        assert_eq!(local.on_disk().human_review.entries.len(), 1);
    }

    #[tokio::test]
    async fn an_approval_that_never_recorded_its_commit_is_renewed_by_approving_what_was_shown() {
        let local = local_project().await;
        local.approve().await;
        local.state.task.tasks.write().await.get_mut(&local.task_id).unwrap().human_review.entries[0].approved_tip = None;
        let status = approval_status(&local.state, local.task_id).await.unwrap();
        assert_eq!(status.coverage, ApprovalCoverage::Unrecorded);
        assert_eq!(status.branch_tip.as_deref(), Some(local.tip().as_str()));

        let renewed = approve_again(&local.state, local.task_id, &local.tip()).await.expect("approved");
        assert_eq!(renewed.human_review.approved_tip(), Some(local.tip().as_str()));
        local.complete().await.expect("completes");
    }

    #[tokio::test]
    async fn an_integration_record_of_an_earlier_approval_does_not_stop_approving_again() {
        let local = local_project().await;
        local.approve().await;
        local.advance();
        {
            let mut tasks = local.state.task.tasks.write().await;
            let t = tasks.get_mut(&local.task_id).unwrap();
            // Integrated work from an earlier approval of some other commit.
            t.local_integration = Some(crate::domain::integration::LocalIntegration {
                state: crate::domain::integration::IntegrationState::Integrated,
                target_branch: "main".into(),
                target_old_oid: "a".repeat(40),
                approved_tip: "d".repeat(40),
                result_oid: "c".repeat(40),
                strategy: crate::domain::integration::IntegrationStrategy::FastForward,
                checkout_path: None,
                started_at: chrono::Utc::now(),
                completed_at: None,
                completes_task: false,
            });
        }
        approve_again(&local.state, local.task_id, &local.tip()).await.expect("approved");
    }
}
