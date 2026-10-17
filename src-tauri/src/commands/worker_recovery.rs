//! Human-guided recovery of an interrupted coding Worker.
//!
//! A Worker whose outcome SlashIt could not attribute (see
//! [`crate::domain::task::WorkerHandoffStage::RecoveryRequired`]) leaves its
//! Task Checkout and branch untouched. This module offers exactly two things
//! on top of that: a read-only snapshot of the preserved evidence, and one
//! human decision that accepts an exact, verified, existing commit as the
//! run's result so the Task continues into the ordinary AI Review lifecycle.
//!
//! Nothing here starts, retries or resumes a Worker, and nothing moves a Git
//! reference or touches the checkout: every Git call is a read, with optional
//! index locks disabled. The snapshot is informational only; acceptance
//! re-reads every consequential fact under the Task's lifecycle lease.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::commands::gui_admission::GuiMutation;
use crate::domain::task::ActivityKind;
use crate::domain::task::{
    Task, TaskPhase, TaskStatus, WorkerHandoffStage, WorkerResultAcceptance,
};

/// The most commits listed in a snapshot; more are reported as truncated.
const MAX_LISTED_COMMITS: usize = 20;
/// The most dirty paths listed in a snapshot.
const MAX_LISTED_PATHS: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceCommit {
    pub sha: String,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckoutEvidence {
    pub path: String,
    pub exists: bool,
    /// Git lists exactly one unlocked registration of this path on the
    /// recorded branch.
    pub registered: bool,
    pub head: Option<String>,
    pub clean: Option<bool>,
    /// A bounded sample of the dirty paths, as `git status --porcelain`
    /// prints them.
    pub dirty_paths: Vec<String>,
}

/// What SlashIt can see about an interrupted Worker right now. Informational
/// only: accepting a result re-verifies everything.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerRecoveryEvidence {
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub repository_id: Option<Uuid>,
    /// `started`, `committed_awaiting_review`, `recovery_required`,
    /// `resolved_to_ai_review`, or `none` when the Task has no handoff.
    pub stage: String,
    pub reason: Option<String>,
    pub run: Option<u32>,
    pub branch: Option<String>,
    pub start_head: Option<String>,
    pub observed_commit: Option<String>,
    /// The exact tip of the recorded branch now.
    pub branch_tip: Option<String>,
    /// Commits from `start_head` to the tip, newest first, bounded.
    pub commits: Vec<EvidenceCommit>,
    pub commits_truncated: bool,
    pub checkout: Option<CheckoutEvidence>,
    /// An executor or review currently owns the Task in this process.
    pub active_execution: bool,
    pub eligible: bool,
    /// Why the tip cannot be accepted now; empty when `eligible`.
    pub refusals: Vec<String>,
}

/// The human decision that accepts a result. Every field is a compare-and-set
/// expectation: the decision applies only if the Task is exactly as shown.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptWorkerResultRequest {
    /// Client-generated; identifies this decision across retries.
    pub decision_id: Uuid,
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub run: u32,
    /// Must be `recovery_required`.
    pub stage: String,
    pub branch: String,
    pub checkout: String,
    #[serde(default)]
    pub observed_commit: Option<String>,
    /// The exact tip the person inspected and approves.
    pub tip: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcceptWorkerResultOutcome {
    /// The decision was recorded durably and the Task is in AI Review.
    Accepted { task: Task },
    /// An identical earlier decision had already been applied.
    AlreadyApplied { task: Task },
    /// The decision was not accepted. Nothing was recorded, except when the
    /// reason says the branch moved after the decision was recorded: then the
    /// Task has been returned to recovery and must be inspected again.
    Refused { reason: String },
    /// The decision is published on the board but its durability could not be
    /// confirmed. It must not be repeated blindly: the Task stays held until
    /// SlashIt re-validates the commit and confirms the write.
    DurabilityUnconfirmed { task: Task, detail: String },
}

fn stage_name(stage: &WorkerHandoffStage) -> &'static str {
    match stage {
        WorkerHandoffStage::Started => "started",
        WorkerHandoffStage::CommittedAwaitingReview { .. } => "committed_awaiting_review",
        WorkerHandoffStage::RecoveryRequired { .. } => "recovery_required",
        WorkerHandoffStage::ResolvedToAiReview { .. } => "resolved_to_ai_review",
    }
}

fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A read-only Git call. `--no-optional-locks` keeps `status` from refreshing
/// the index, so inspection leaves the checkout byte-for-byte alone.
async fn git_read(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .await
        .map_err(|e| format!("git could not run: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub(crate) async fn commit_at(dir: &Path, rev: &str) -> Option<String> {
    git_read(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
    )
    .await
    .ok()
    .map(|out| out.trim().to_string())
    .filter(|sha| is_object_id(sha))
}

/// Everything read for one evaluation: the snapshot, plus the facts acceptance
/// compares against.
struct Evaluation {
    evidence: WorkerRecoveryEvidence,
}

async fn evaluate(state: &crate::AppState, task: &Task) -> Evaluation {
    let handoff = task.worker_handoff.as_ref();
    let mut refusals: Vec<String> = Vec::new();
    let mut evidence = WorkerRecoveryEvidence {
        task_id: task.id,
        project_id: task.project_id,
        repository_id: handoff.and_then(|h| h.repository_id),
        stage: handoff.map_or("none", |h| stage_name(&h.stage)).to_string(),
        reason: None,
        run: handoff.and_then(|h| h.run),
        branch: handoff.and_then(|h| h.branch.clone()),
        start_head: handoff.and_then(|h| h.start_head.clone()),
        observed_commit: None,
        branch_tip: None,
        commits: Vec::new(),
        commits_truncated: false,
        checkout: None,
        active_execution: false,
        eligible: false,
        refusals: Vec::new(),
    };

    let Some(handoff) = handoff else {
        refusals.push("This Task has no Worker recovery evidence.".into());
        evidence.refusals = refusals;
        return Evaluation { evidence };
    };
    if let WorkerHandoffStage::RecoveryRequired {
        reason,
        observed_commit,
    } = &handoff.stage
    {
        evidence.reason = Some(reason.clone());
        evidence.observed_commit = observed_commit.clone();
    } else {
        refusals.push("This Worker is not waiting for a recovery decision.".into());
    }

    if let Some(executor) = state.executor.get() {
        if crate::lifecycle::ExecutionOwnership::is_task_running(executor.as_ref(), task.id).await {
            evidence.active_execution = true;
            refusals.push("An execution or review currently owns this Task.".into());
        }
    }

    // Identity: every recorded fact must still agree with the Task.
    if handoff.task_id != task.id || handoff.project_id != task.project_id {
        refusals.push("The recovery record does not belong to this Task and Project.".into());
    }
    let run = handoff.run;
    if run.is_none() {
        refusals.push("The recovery record has no run ordinal (legacy evidence).".into());
    }
    let start_head = handoff.start_head.clone().filter(|sha| is_object_id(sha));
    if start_head.is_none() {
        refusals.push(
            "The branch HEAD before this run was not reliably recorded, so a result cannot be \
             accepted."
                .into(),
        );
    }
    let branch = handoff.branch.clone().filter(|b| !b.is_empty());
    if branch.is_none() || branch.as_deref() != task.branch_name.as_deref() {
        refusals.push("The recorded branch is missing or differs from the Task's branch.".into());
    }
    if let Some(observed) = evidence.observed_commit.as_deref() {
        if !is_object_id(observed) {
            refusals.push("The recorded observed commit is not a valid object id.".into());
        }
    }

    // Lifecycle: nothing durable may already have ended or advanced this run.
    if !matches!(task.status, TaskStatus::Error | TaskStatus::Backlog) {
        refusals
            .push("The Task has moved to a lifecycle state this decision cannot override.".into());
    }
    if task.cleanup_in_flight
        || task.pending_republish.is_some()
        || task.local_integration.is_some()
        || task.pr_url.is_some()
    {
        refusals.push(
            "The Task has an unfinished cleanup, restack, integration or pull request.".into(),
        );
    }
    if let Some(run) = run {
        // A completion left by an earlier acceptance or reconciliation whose
        // result no longer verified is not a reason to refuse a fresh decision:
        // completions are once-only, so accepting again cannot duplicate it.
        let resolved_before = handoff.acceptance.is_some() || evidence.observed_commit.is_some();
        let ended = task.activity.iter().any(|entry| match &entry.kind {
            ActivityKind::RunCompleted { run: r } => *r == run && !resolved_before,
            ActivityKind::RunFailed { run: Some(r), .. } => *r == run,
            _ => false,
        });
        if ended {
            refusals.push("This run already has a durable completion or failure.".into());
        }
    }

    // Project, repository and checkout.
    let project = state
        .project
        .projects
        .read()
        .await
        .get(&task.project_id)
        .cloned();
    let repo_path = match &project {
        None => {
            refusals.push("The owning Project is unavailable.".into());
            None
        }
        Some(project) => {
            if project.repository_id.is_none() || project.repository_id != handoff.repository_id {
                refusals.push(
                    "The Project repository differs from the one recorded for this run.".into(),
                );
            }
            evidence.repository_id = handoff.repository_id;
            match project.repository_id {
                Some(id) => state
                    .repository
                    .repositories
                    .read()
                    .await
                    .get(&id)
                    .map(|r| r.local_path.clone()),
                None => None,
            }
        }
    };
    if repo_path.is_none() {
        refusals.push("The owning repository is unavailable.".into());
    }

    let checkout_path = task.worktree_path.clone();
    if checkout_path.is_none() {
        refusals.push("The Task Checkout path is missing from the Task record.".into());
    }

    if let (Some(repo), Some(branch)) = (repo_path.as_deref(), branch.as_deref()) {
        let repo_dir = Path::new(repo);
        evidence.branch_tip = commit_at(repo_dir, &format!("refs/heads/{branch}")).await;
        if evidence.branch_tip.is_none() {
            refusals.push("The recorded branch cannot be resolved to a commit.".into());
        }

        if let (Some(tip), Some(start)) = (evidence.branch_tip.as_deref(), start_head.as_deref()) {
            if tip == start {
                refusals.push("The branch tip is still the commit the run started from; there is no result to accept.".into());
            } else {
                match crate::worktree::restack::is_ancestor(repo_dir, start, tip).await {
                    Ok(true) => {}
                    Ok(false) => refusals.push(
                        "The branch tip does not descend from the commit the run started from."
                            .into(),
                    ),
                    Err(e) => refusals.push(format!("Ancestry could not be verified: {e}")),
                }
                if let Some(observed) = evidence
                    .observed_commit
                    .as_deref()
                    .filter(|o| is_object_id(o))
                {
                    if observed != tip {
                        match crate::worktree::restack::is_ancestor(repo_dir, observed, tip).await {
                            Ok(true) => {}
                            Ok(false) => refusals.push(
                                "The recorded observed commit is not an ancestor of the branch tip."
                                    .into(),
                            ),
                            Err(e) => refusals.push(format!("Ancestry could not be verified: {e}")),
                        }
                    }
                }
                if let Ok(listing) = git_read(
                    repo_dir,
                    &[
                        "log",
                        "--format=%H%x1f%s",
                        &format!("-n{}", MAX_LISTED_COMMITS + 1),
                        &format!("{start}..{tip}"),
                    ],
                )
                .await
                {
                    let mut commits: Vec<EvidenceCommit> = listing
                        .lines()
                        .filter_map(|line| {
                            let (sha, subject) = line.split_once('\u{1f}')?;
                            Some(EvidenceCommit {
                                sha: sha.to_string(),
                                subject: subject.to_string(),
                            })
                        })
                        .collect();
                    if commits.len() > MAX_LISTED_COMMITS {
                        commits.truncate(MAX_LISTED_COMMITS);
                        evidence.commits_truncated = true;
                    }
                    evidence.commits = commits;
                }
            }
        }

        if let Some(path) = checkout_path.as_deref() {
            let mut checkout = CheckoutEvidence {
                path: path.to_string(),
                exists: Path::new(path).is_dir(),
                registered: false,
                head: None,
                clean: None,
                dirty_paths: Vec::new(),
            };
            if checkout.exists {
                let dir = Path::new(path);
                checkout.head = commit_at(dir, "HEAD").await;
                if let Ok(status) =
                    git_read(dir, &["status", "--porcelain=v1", "--untracked-files=all"]).await
                {
                    checkout.clean = Some(status.trim().is_empty());
                    checkout.dirty_paths = status
                        .lines()
                        .take(MAX_LISTED_PATHS)
                        .map(str::to_string)
                        .collect();
                }
            }
            if let Some(tip) = evidence.branch_tip.as_deref() {
                match crate::queue::executor::TaskExecutor::verify_worker_checkout(
                    repo,
                    path,
                    branch,
                    tip,
                    crate::queue::executor::WorkerCommitProof::ExactTip,
                )
                .await
                {
                    Ok(()) => checkout.registered = true,
                    Err(refusal) => {
                        // A dirty checkout is registered correctly; only the
                        // cleanliness fact is missing.
                        checkout.registered =
                            matches!(refusal, crate::queue::executor::CheckoutRefusal::Dirty);
                        refusals.push(refusal.to_string());
                    }
                }
            }
            evidence.checkout = Some(checkout);
        }

        // Another Task claiming the same branch makes ownership ambiguous.
        let (snapshot, repo_map) = {
            let all: Vec<Task> = state.task.tasks.read().await.values().cloned().collect();
            let projects = state.project.projects.read().await;
            let repositories = state.repository.repositories.read().await;
            let paths: HashMap<Uuid, String> = projects
                .iter()
                .filter_map(|(id, p)| {
                    Some((*id, repositories.get(&p.repository_id?)?.local_path.clone()))
                })
                .collect();
            (all, paths)
        };
        if crate::worktree::tasks_sharing_a_recorded_branch(&snapshot, &repo_map)
            .await
            .contains_key(&task.id)
        {
            refusals.push("Another Task records ownership of this Git branch.".into());
        }
    }

    refusals.dedup();
    evidence.eligible = refusals.is_empty();
    evidence.refusals = refusals;
    Evaluation { evidence }
}

/// Read-only snapshot of an interrupted Worker's preserved evidence.
#[tauri::command]
pub async fn get_worker_recovery_evidence(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<WorkerRecoveryEvidence, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    worker_recovery_evidence(&state, task_id).await
}

pub(crate) async fn worker_recovery_evidence(
    state: &crate::AppState,
    task_id: Uuid,
) -> Result<WorkerRecoveryEvidence, String> {
    let task = state
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .ok_or("Task not found")?;
    Ok(evaluate(state, &task).await.evidence)
}

/// Accept one exact, verified commit as the result of the interrupted run.
#[tauri::command]
pub async fn accept_worker_result(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    request: AcceptWorkerResultRequest,
) -> Result<AcceptWorkerResultOutcome, String> {
    accept_result(&state, request).await
}

fn refused(reason: impl Into<String>) -> Result<AcceptWorkerResultOutcome, String> {
    Ok(AcceptWorkerResultOutcome::Refused {
        reason: reason.into(),
    })
}

/// Whether `request` repeats the decision already recorded on the handoff.
///
/// Only the latest acceptance is retained, so idempotency is bounded: a
/// `decision_id` that a later decision has superseded is not recognized, and
/// replaying it is treated as a new decision. It never bypasses the Task's
/// current state: it must match the current recovery evidence field by field
/// (run, checkout, observed commit, exact tip), pass the lock-time
/// compare-and-set and `evaluate`, and `RunCompleted` is once-only, so it
/// cannot act on another run or tip, duplicate a completion or start a
/// Worker. Known limit: if the Task has returned to an identical recovery
/// tuple, a superseded id with identical content is accepted as a fresh
/// decision for that episode without the person re-deciding. The desktop
/// panel cannot produce this; only a client holding an old request can.
fn repeats(task: &Task, request: &AcceptWorkerResultRequest) -> Option<bool> {
    let handoff = task.worker_handoff.as_ref()?;
    let recorded = handoff.acceptance.as_ref()?;
    if recorded.decision_id != request.decision_id {
        return None;
    }
    Some(
        handoff.run == Some(request.run)
            && handoff.task_id == request.task_id
            && handoff.project_id == request.project_id
            && handoff.repository_id == Some(request.repository_id)
            && handoff.branch.as_deref() == Some(request.branch.as_str())
            && task.worktree_path.as_deref() == Some(request.checkout.as_str())
            && recorded.commit == request.tip
            && recorded.observed_commit == request.observed_commit,
    )
}

async fn branch_still_at(state: &crate::AppState, task: &Task, tip: &str) -> bool {
    let (Some(branch), Ok(repo)) = (
        task.branch_name.as_deref(),
        crate::commands::integration::repo_path_of(state, task.project_id).await,
    ) else {
        return false;
    };
    commit_at(Path::new(&repo), &format!("refs/heads/{branch}"))
        .await
        .as_deref()
        == Some(tip)
}

/// The final compare-and-set and, only if it holds, the whole acceptance.
/// Answers whether it changed the Task; a `false` leaves it untouched.
fn apply_acceptance(
    current: &mut Task,
    expected: &AcceptWorkerResultRequest,
    decision: &WorkerResultAcceptance,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let still_current = current.id == expected.task_id
        && current.project_id == expected.project_id
        && matches!(current.status, TaskStatus::Error | TaskStatus::Backlog)
        && !current.cleanup_in_flight
        && current.pending_republish.is_none()
        && current.local_integration.is_none()
        && current.pr_url.is_none()
        && current.branch_name.as_deref() == Some(expected.branch.as_str())
        && current.worktree_path.as_deref() == Some(expected.checkout.as_str())
        && current.worker_handoff.as_ref().is_some_and(|h| {
            h.run == Some(expected.run)
                && h.task_id == expected.task_id
                && h.project_id == expected.project_id
                && h.repository_id == Some(expected.repository_id)
                && h.branch.as_deref() == Some(expected.branch.as_str())
                && h.acceptance
                    .as_ref()
                    .is_none_or(|a| a.decision_id != expected.decision_id)
                && matches!(&h.stage,
                    WorkerHandoffStage::RecoveryRequired { observed_commit, .. }
                        if *observed_commit == expected.observed_commit)
        });
    if !still_current {
        return false;
    }
    // A new acceptance replaces whatever was confirmed before. The process-local
    // acknowledgement belongs to the previous evidence, so it is withdrawn in the
    // same transition and re-earned only after this decision's own durable write.
    current.worker_handoff_confirmed = false;
    current.status = TaskStatus::AiReview;
    current.phase = TaskPhase::QaReview;
    current.phase_progress = 80;
    current.overall_progress = 80;
    current.error_message = None;
    current.record_activity_at(now, ActivityKind::RunCompleted { run: expected.run });
    // The run was interrupted, so its provider result was never kept. A person
    // accepting its commit does not make that result available.
    current.record_activity_at(
        now,
        ActivityKind::RunResult {
            run: expected.run,
            result: slashit_activity::RunResult::Unavailable {
                reason: crate::queue::executor::TaskExecutor::RESULT_NOT_KEPT_REASON.to_string(),
            },
        },
    );
    if let Some(handoff) = current.worker_handoff.as_mut() {
        handoff.stage = WorkerHandoffStage::ResolvedToAiReview {
            commit: expected.tip.clone(),
        };
        handoff.acceptance = Some(decision.clone());
    }
    true
}

pub(crate) async fn accept_result(
    state: &crate::AppState,
    request: AcceptWorkerResultRequest,
) -> Result<AcceptWorkerResultOutcome, String> {
    if !is_object_id(&request.tip) {
        return refused("The commit to accept is not a full object id.");
    }
    if request.stage != "recovery_required" {
        return refused("Only a Worker waiting in recovery can be accepted.");
    }
    let task_id = request.task_id;
    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    let task = state
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .ok_or("Task not found")?;

    // Idempotency first: the same decision never has an effect twice, and the
    // same id with different content is never silently reinterpreted.
    match repeats(&task, &request) {
        Some(true) => {
            let resolved = task.worker_handoff.as_ref().is_some_and(|h| {
                matches!(&h.stage, WorkerHandoffStage::ResolvedToAiReview { commit } if *commit == request.tip)
            });
            // "Applied" needs more than the record: the acceptance must be
            // confirmed durable and re-proved in this process, or a later
            // durable lifecycle state must already prove it (the same rule
            // startup uses to decide whether recovery still holds).
            return if resolved && task.worker_recovery_refusal().is_none() {
                Ok(AcceptWorkerResultOutcome::AlreadyApplied { task })
            } else if resolved {
                // Applied, but this process has not confirmed the write or
                // re-proved the commit. Say so; do not repeat the transition.
                Ok(AcceptWorkerResultOutcome::DurabilityUnconfirmed {
                    task,
                    detail: "This decision is recorded but not yet confirmed durable and \
                             re-validated. SlashIt will confirm it before review starts."
                        .into(),
                })
            } else {
                refused(
                    "This decision was applied earlier, but the Task has since returned to \
                     recovery. Inspect it again and make a new decision.",
                )
            };
        }
        Some(false) => {
            return refused("This decision id was already used for a different request.");
        }
        None => {}
    }

    // Compare-and-set against what the person was shown.
    let Some(handoff) = task.worker_handoff.as_ref() else {
        return refused("This Task has no Worker recovery evidence.");
    };
    let WorkerHandoffStage::RecoveryRequired {
        observed_commit, ..
    } = &handoff.stage
    else {
        return refused("This Worker is no longer waiting for a recovery decision.");
    };
    let stale = handoff.run != Some(request.run)
        || handoff.task_id != request.task_id
        || handoff.project_id != request.project_id
        || task.project_id != request.project_id
        || handoff.repository_id != Some(request.repository_id)
        || handoff.branch.as_deref() != Some(request.branch.as_str())
        || task.worktree_path.as_deref() != Some(request.checkout.as_str())
        || *observed_commit != request.observed_commit;
    if stale {
        return refused(
            "The recovery evidence changed after it was shown to you. Inspect it again.",
        );
    }

    let evaluation = evaluate(state, &task).await;
    if evaluation.evidence.branch_tip.as_deref() != Some(request.tip.as_str()) {
        return refused(
            "The branch tip is not the commit you approved. Inspect the current tip and decide \
             again.",
        );
    }
    if !evaluation.evidence.eligible {
        return refused(evaluation.evidence.refusals.join(" "));
    }

    let now = chrono::Utc::now();
    let decision = WorkerResultAcceptance {
        decision_id: request.decision_id,
        commit: request.tip.clone(),
        observed_commit: request.observed_commit.clone(),
        decided_at: now,
    };
    // Evaluated under the board's write lock, with the save that follows: the
    // checks above were a preliminary read. Answering `false` writes nothing.
    let revise = |staged: &mut HashMap<Uuid, Task>| -> bool {
        staged
            .get_mut(&task_id)
            .is_some_and(|current| apply_acceptance(current, &request, &decision, now))
    };

    match crate::lifecycle::record_typed_if_changed(
        &state.task.tasks,
        &state.storage,
        task_id,
        &revise,
    )
    .await
    {
        Err(crate::lifecycle::RecordWriteError::NotPublished(reason)) => {
            refused(format!("Nothing was recorded: {reason}"))
        }
        Err(crate::lifecycle::RecordWriteError::PublishedNotDurable(detail)) => {
            let task = state
                .task
                .tasks
                .read()
                .await
                .get(&task_id)
                .cloned()
                .ok_or("Task not found")?;
            // Left unconfirmed on purpose: startup and the executor poll
            // re-validate the commit before AI Review is admitted.
            Ok(AcceptWorkerResultOutcome::DurabilityUnconfirmed { task, detail })
        }
        Ok(false) => refused(
            "The Task changed while the decision was being recorded, so nothing was recorded.",
        ),
        Ok(true) => {
            // The write is confirmed durable. The acknowledgement that lets
            // review start is earned only if the branch still sits on the
            // accepted commit. SlashIt's own Git changes are serialized by the
            // lease held here; a Git writer outside SlashIt is not, so this is
            // a point-in-time read. If the tip has moved, the Task goes straight
            // back to recovery rather than waiting for the next poll.
            let current = state
                .task
                .tasks
                .read()
                .await
                .get(&task_id)
                .cloned()
                .ok_or("Task not found")?;
            if !branch_still_at(state, &current, &request.tip).await {
                let returned = crate::queue::executor::TaskExecutor::record_recovery_required(
                    &state.task.tasks,
                    &state.storage,
                    task_id,
                    Some(request.run),
                    None,
                    "The branch moved away from the commit you accepted while the decision was \
                     being recorded. Inspect the current tip and decide again.",
                )
                .await;
                return refused(match returned {
                    Ok(()) => "The decision was recorded, but the branch moved away from the \
                               commit you approved before review could start. The Task is back \
                               in recovery; inspect the current tip and decide again."
                        .to_string(),
                    Err(e) => format!(
                        "The decision was recorded, but the branch moved away from the commit \
                         you approved. Review is held and SlashIt will return the Task to \
                         recovery on its next check ({e})."
                    ),
                });
            }
            let mut tasks = state.task.tasks.write().await;
            let task = tasks.get_mut(&task_id).ok_or("Task not found")?;
            task.worker_handoff_confirmed = true;
            Ok(AcceptWorkerResultOutcome::Accepted { task: task.clone() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AgentConfig, AgentType, Project, ProjectScope, Repository};
    use crate::test_helpers::create_test_task_full;
    use slashit_durable_fs::{fault, Stage};
    use std::path::PathBuf;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args([
                "--no-optional-locks",
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=T",
            ])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        state: crate::AppState,
        repo: PathBuf,
        checkout: PathBuf,
        project_id: Uuid,
        repository_id: Uuid,
        task_id: Uuid,
        branch: String,
        start: String,
        tip: String,
    }

    impl Fx {
        fn request(&self, decision_id: Uuid) -> AcceptWorkerResultRequest {
            AcceptWorkerResultRequest {
                decision_id,
                task_id: self.task_id,
                project_id: self.project_id,
                repository_id: self.repository_id,
                run: 1,
                stage: "recovery_required".into(),
                branch: self.branch.clone(),
                checkout: self.checkout.to_string_lossy().to_string(),
                observed_commit: None,
                tip: self.tip.clone(),
            }
        }

        async fn task(&self) -> Task {
            self.state.task.tasks.read().await[&self.task_id].clone()
        }

        fn on_disk(&self) -> Task {
            self.state
                .storage
                .load_project_tasks(self.project_id)
                .unwrap()
                .into_iter()
                .find(|t| t.id == self.task_id)
                .unwrap()
        }

        async fn edit(&self, change: impl FnOnce(&mut Task)) {
            let mut tasks = self.state.task.tasks.write().await;
            change(tasks.get_mut(&self.task_id).unwrap());
            let board: Vec<Task> = tasks.values().cloned().collect();
            self.state
                .storage
                .save_project_tasks(self.project_id, &board)
                .unwrap();
        }

        /// Everything Git and the checkout hold that accepting must never
        /// change: references, registrations, HEAD, status and the index file.
        fn repo_state(&self) -> String {
            let index = git(&self.checkout, &["rev-parse", "--git-path", "index"]);
            let index = if Path::new(&index).is_absolute() {
                PathBuf::from(index)
            } else {
                self.checkout.join(index)
            };
            let stamp = std::fs::metadata(&index)
                .and_then(|m| m.modified())
                .map(|t| format!("{t:?}"))
                .unwrap_or_default();
            format!(
                "{}\n{}\n{}\n{}\n{stamp}\n{}",
                git(&self.repo, &["for-each-ref"]),
                git(&self.repo, &["worktree", "list", "--porcelain"]),
                git(&self.checkout, &["rev-parse", "HEAD"]),
                git(
                    &self.checkout,
                    &["status", "--porcelain=v1", "--untracked-files=all"]
                ),
                self.board_files(),
            )
        }

        /// Every file SlashIt persisted for this fixture, by identity: path,
        /// inode (an atomic replacement changes it), mtime and bytes. Equal
        /// snapshots mean no board file was rewritten.
        fn board_files(&self) -> String {
            fn walk(dir: &Path, out: &mut Vec<String>) {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    return;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        walk(&path, out);
                    } else if let (Ok(meta), Ok(bytes)) =
                        (std::fs::metadata(&path), std::fs::read(&path))
                    {
                        // A rewrite that keeps the bytes still shows up as a new
                        // file identity (Unix) or modification time.
                        #[cfg(unix)]
                        let identity = {
                            use std::os::unix::fs::MetadataExt;
                            format!(
                                "ino={} mtime={}.{}",
                                meta.ino(),
                                meta.mtime(),
                                meta.mtime_nsec()
                            )
                        };
                        #[cfg(not(unix))]
                        let identity = format!("mtime={:?}", meta.modified().ok());
                        out.push(format!(
                            "{} {identity} {:x}",
                            path.display(),
                            md5_like(&bytes)
                        ));
                    }
                }
            }
            fn md5_like(bytes: &[u8]) -> u64 {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                bytes.hash(&mut hasher);
                hasher.finish()
            }
            let mut out = Vec::new();
            for root in ["config", "data"] {
                walk(&self._tmp.path().join(root), &mut out);
            }
            out.sort();
            out.join("\n")
        }

        fn commit_in_checkout(&self, name: &str) -> String {
            std::fs::write(self.checkout.join(name), name).unwrap();
            git(&self.checkout, &["add", "-A"]);
            git(&self.checkout, &["commit", "-q", "-m", name]);
            git(&self.checkout, &["rev-parse", "HEAD"])
        }
    }

    async fn fixture() -> Fx {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let (state, _) = crate::app_core::build_state_with_paths(paths)
            .await
            .unwrap();
        crate::test_helpers::attach_test_executor(&state);

        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("base.txt"), "base").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        let start = git(&repo, &["rev-parse", "HEAD"]);
        let branch = "task-interrupted".to_string();
        let checkout = tmp.path().join("checkout");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                checkout.to_str().unwrap(),
                "main",
            ],
        );

        let repository_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        state.repository.repositories.write().await.insert(
            repository_id,
            Repository {
                id: repository_id,
                local_path: repo.to_string_lossy().to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        state.project.projects.write().await.insert(
            project_id,
            Project {
                id: project_id,
                name: "p".into(),
                repository_id: Some(repository_id),
                scope: ProjectScope::default(),
                state_location: crate::config::paths::StateLocation::External,
                base: None,
                agent_type: AgentType::ClaudeCode,
                agent_config: AgentConfig {
                    agent_type: AgentType::ClaudeCode,
                    command: "claude".into(),
                    args: Vec::new(),
                    env: HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );

        let mut task = create_test_task_full("interrupted", project_id, TaskStatus::Error, 0);
        task.branch_name = Some(branch.clone());
        task.worktree_path = Some(checkout.to_string_lossy().to_string());
        task.phase = TaskPhase::Idle;
        task.error_message = Some("interrupted".into());
        task.worker_handoff = Some(crate::domain::task::WorkerHandoff {
            task_id: task.id,
            project_id,
            repository_id: Some(repository_id),
            run: Some(1),
            branch: Some(branch.clone()),
            start_head: Some(start.clone()),
            acceptance: None,
            stage: WorkerHandoffStage::RecoveryRequired {
                reason: "interrupted".into(),
                observed_commit: None,
            },
        });
        task.record_activity(ActivityKind::RunStarted {
            run: 1,
            addressing_feedback: false,
        });
        task.record_activity(ActivityKind::RecoveryRequired {
            run: Some(1),
            reason: "interrupted".into(),
        });
        let task_id = task.id;
        state.task.tasks.write().await.insert(task_id, task.clone());
        state
            .storage
            .save_project_tasks(project_id, &[task])
            .unwrap();

        let mut fx = Fx {
            _tmp: tmp,
            state,
            repo,
            checkout,
            project_id,
            repository_id,
            task_id,
            branch,
            start,
            tip: String::new(),
        };
        fx.tip = fx.commit_in_checkout("work.txt");
        fx
    }

    fn completions(task: &Task) -> usize {
        task.activity
            .iter()
            .filter(|e| matches!(e.kind, ActivityKind::RunCompleted { run: 1 }))
            .count()
    }

    fn refusal(outcome: Result<AcceptWorkerResultOutcome, String>) -> String {
        match outcome.expect("a refusal is an answer, not an error") {
            AcceptWorkerResultOutcome::Refused { reason } => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    async fn assert_untouched(fx: &Fx, before_task: &Task, before_repo: &str) {
        let after = fx.task().await;
        assert_eq!(after.status, before_task.status);
        assert_eq!(after.worker_handoff, before_task.worker_handoff);
        assert_eq!(after.activity, before_task.activity);
        assert_eq!(after.updated_at, before_task.updated_at);
        assert_eq!(
            serde_json::to_string(&after).unwrap(),
            serde_json::to_string(before_task).unwrap(),
            "the Task is byte-identical in memory"
        );
        assert_eq!(fx.repo_state(), before_repo);
    }

    #[tokio::test]
    async fn the_evidence_query_reads_everything_and_changes_nothing() {
        let fx = fixture().await;
        let before = (fx.task().await, fx.repo_state(), fx.on_disk());

        let evidence = worker_recovery_evidence(&fx.state, fx.task_id)
            .await
            .unwrap();

        assert!(evidence.eligible, "{:?}", evidence.refusals);
        assert_eq!(evidence.stage, "recovery_required");
        assert_eq!(evidence.run, Some(1));
        assert_eq!(evidence.start_head.as_deref(), Some(fx.start.as_str()));
        assert_eq!(evidence.branch_tip.as_deref(), Some(fx.tip.as_str()));
        assert_eq!(evidence.commits.len(), 1);
        assert_eq!(evidence.commits[0].sha, fx.tip);
        let checkout = evidence.checkout.unwrap();
        assert!(checkout.exists && checkout.registered);
        assert_eq!(checkout.clean, Some(true));
        assert!(!evidence.active_execution);
        assert_eq!(fx.task().await.activity, before.0.activity);
        assert_eq!(
            fx.repo_state(),
            before.1,
            "no ref, registration or index changed"
        );
        assert_eq!(fx.on_disk().worker_handoff, before.2.worker_handoff);
    }

    #[tokio::test]
    async fn an_exact_verified_tip_is_accepted_into_ai_review_once() {
        let fx = fixture().await;
        let refs_before = git(&fx.repo, &["for-each-ref"]);
        let head_before = git(&fx.checkout, &["rev-parse", "HEAD"]);
        let id = Uuid::new_v4();

        let outcome = accept_result(&fx.state, fx.request(id)).await.unwrap();

        let AcceptWorkerResultOutcome::Accepted { task } = outcome else {
            panic!("expected Accepted, got {outcome:?}");
        };
        assert_eq!(task.status, TaskStatus::AiReview);
        assert_eq!(task.phase, TaskPhase::QaReview);
        assert_eq!((task.phase_progress, task.overall_progress), (80, 80));
        assert_eq!(task.error_message, None);
        let handoff = task.worker_handoff.as_ref().unwrap();
        assert_eq!(
            handoff.stage,
            WorkerHandoffStage::ResolvedToAiReview {
                commit: fx.tip.clone()
            }
        );
        let acceptance = handoff.acceptance.as_ref().unwrap();
        assert_eq!(
            (acceptance.decision_id, acceptance.commit.as_str()),
            (id, fx.tip.as_str())
        );
        assert_eq!(completions(&task), 1);
        assert!(task.worker_handoff_confirmed);
        assert!(
            task.worker_recovery_refusal().is_none(),
            "AI Review is admitted"
        );
        assert_eq!(task.needs_you(false), None);
        assert_eq!(
            fx.on_disk().worker_handoff,
            task.worker_handoff,
            "the file agrees"
        );
        assert_eq!(completions(&fx.on_disk()), 1);
        assert_eq!(git(&fx.repo, &["for-each-ref"]), refs_before);
        assert_eq!(git(&fx.checkout, &["rev-parse", "HEAD"]), head_before);
    }

    #[tokio::test]
    async fn the_same_decision_again_changes_nothing_and_a_different_one_with_its_id_is_refused() {
        let fx = fixture().await;
        let id = Uuid::new_v4();
        accept_result(&fx.state, fx.request(id)).await.unwrap();
        let settled = fx.task().await;
        let repo_state = fx.repo_state();

        let again = accept_result(&fx.state, fx.request(id)).await.unwrap();
        assert!(
            matches!(again, AcceptWorkerResultOutcome::AlreadyApplied { .. }),
            "{again:?}"
        );
        assert_untouched(&fx, &settled, &repo_state).await;

        let mut conflicting = fx.request(id);
        conflicting.tip = "f".repeat(40);
        let reason = refusal(accept_result(&fx.state, conflicting).await);
        assert!(reason.contains("different request"), "{reason}");

        let reason = refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await);
        assert!(reason.contains("no longer waiting"), "{reason}");
        assert_untouched(&fx, &settled, &repo_state).await;
        assert_eq!(completions(&fx.task().await), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_same_decision_after_a_restart_has_no_second_effect_and_is_confirmed_only_once_revalidated(
    ) {
        let fx = fixture().await;
        let id = Uuid::new_v4();
        accept_result(&fx.state, fx.request(id)).await.unwrap();
        let reloaded = fx.on_disk();
        fx.state
            .task
            .tasks
            .write()
            .await
            .insert(fx.task_id, reloaded);
        let (task, repo_state, board) = (fx.task().await, fx.repo_state(), fx.board_files());

        let early = accept_result(&fx.state, fx.request(id)).await.unwrap();

        assert!(
            matches!(
                early,
                AcceptWorkerResultOutcome::DurabilityUnconfirmed { .. }
            ),
            "{early:?}"
        );
        assert_untouched(&fx, &task, &repo_state).await;
        assert_eq!(fx.board_files(), board, "nothing was written");

        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        fx.state
            .executor
            .get()
            .unwrap()
            .reconcile_committed_worker_handoffs()
            .await;
        let again = accept_result(&fx.state, fx.request(id)).await.unwrap();

        assert!(
            matches!(again, AcceptWorkerResultOutcome::AlreadyApplied { .. }),
            "{again:?}"
        );
        assert_eq!(completions(&fx.task().await), 1);
        assert!(!agent.invocations().contains("claude"));
    }

    #[tokio::test]
    async fn a_task_that_durably_progressed_past_ai_review_still_answers_an_identical_replay() {
        let fx = fixture().await;
        let id = Uuid::new_v4();
        accept_result(&fx.state, fx.request(id)).await.unwrap();
        // Restart (the acknowledgement is lost), then review completed and
        // the Task durably moved on.
        fx.edit(|t| t.status = TaskStatus::HumanReview).await;
        let reloaded = fx.on_disk();
        assert!(!reloaded.worker_handoff_confirmed);
        fx.state
            .task
            .tasks
            .write()
            .await
            .insert(fx.task_id, reloaded);
        let (task, repo_state, board) = (fx.task().await, fx.repo_state(), fx.board_files());

        let again = accept_result(&fx.state, fx.request(id)).await.unwrap();
        assert!(
            matches!(again, AcceptWorkerResultOutcome::AlreadyApplied { .. }),
            "{again:?}"
        );
        let mut conflicting = fx.request(id);
        conflicting.observed_commit = Some("a".repeat(40));
        let reason = refusal(accept_result(&fx.state, conflicting).await);
        assert!(reason.contains("different request"), "{reason}");
        let reason = refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await);
        assert!(reason.contains("no longer waiting"), "{reason}");

        assert_untouched(&fx, &task, &repo_state).await;
        assert_eq!(fx.board_files(), board);
        assert_eq!(completions(&fx.task().await), 1);
    }

    #[tokio::test]
    async fn a_reused_decision_id_with_any_different_field_is_refused_without_a_write() {
        let fx = fixture().await;
        let id = Uuid::new_v4();
        accept_result(&fx.state, fx.request(id)).await.unwrap();
        let (task, repo_state, board) = (fx.task().await, fx.repo_state(), fx.board_files());

        type Change = Box<dyn Fn(&mut AcceptWorkerResultRequest)>;
        let changes: Vec<(&str, Change)> = vec![
            (
                "observed",
                Box::new(|r| r.observed_commit = Some("a".repeat(40))),
            ),
            ("run", Box::new(|r| r.run = 2)),
            ("branch", Box::new(|r| r.branch = "other".into())),
            ("repository", Box::new(|r| r.repository_id = Uuid::new_v4())),
            ("project", Box::new(|r| r.project_id = Uuid::new_v4())),
            ("checkout", Box::new(|r| r.checkout = "/elsewhere".into())),
            ("tip", Box::new(|r| r.tip = "f".repeat(40))),
        ];
        for (field, change) in changes {
            let mut request = fx.request(id);
            change(&mut request);
            let reason = refusal(accept_result(&fx.state, request).await);
            assert!(reason.contains("different request"), "{field}: {reason}");
            assert_untouched(&fx, &task, &repo_state).await;
            assert_eq!(fx.board_files(), board, "{field}: nothing was written");
        }
        assert_eq!(completions(&fx.task().await), 1);
    }

    #[tokio::test]
    async fn a_reused_decision_id_must_repeat_the_observed_commit_exactly_even_after_a_restart() {
        let fx = fixture().await;
        let observed = fx.tip.clone();
        let newer = fx.commit_in_checkout("more.txt");
        fx.edit(|t| {
            t.worker_handoff.as_mut().unwrap().stage = WorkerHandoffStage::RecoveryRequired {
                reason: "interrupted".into(),
                observed_commit: Some(observed.clone()),
            }
        })
        .await;
        let id = Uuid::new_v4();
        let mut original = fx.request(id);
        original.observed_commit = Some(observed.clone());
        original.tip = newer.clone();
        accept_result(&fx.state, original.clone()).await.unwrap();
        assert_eq!(
            fx.on_disk()
                .worker_handoff
                .unwrap()
                .acceptance
                .unwrap()
                .observed_commit,
            Some(observed),
            "the observation is part of the persisted decision"
        );
        let reloaded = fx.on_disk();
        fx.state
            .task
            .tasks
            .write()
            .await
            .insert(fx.task_id, reloaded);
        let (task, repo_state, board) = (fx.task().await, fx.repo_state(), fx.board_files());

        for (name, other) in [("none", None), ("other commit", Some(newer.clone()))] {
            let mut request = original.clone();
            request.observed_commit = other;
            let reason = refusal(accept_result(&fx.state, request).await);
            assert!(reason.contains("different request"), "{name}: {reason}");
            assert_untouched(&fx, &task, &repo_state).await;
            assert_eq!(fx.board_files(), board, "{name}: nothing was written");
        }
        let identical = accept_result(&fx.state, original).await.unwrap();
        assert!(
            matches!(identical, AcceptWorkerResultOutcome::DurabilityUnconfirmed { .. }),
            "an identical replay is recognised (and, before revalidation, unconfirmed): {identical:?}"
        );
        assert_eq!(completions(&fx.task().await), 1);
    }

    #[tokio::test]
    async fn two_racing_decisions_record_one_completion() {
        let fx = fixture().await;
        let (a, b) = tokio::join!(
            accept_result(&fx.state, fx.request(Uuid::new_v4())),
            accept_result(&fx.state, fx.request(Uuid::new_v4())),
        );
        let outcomes = [a.unwrap(), b.unwrap()];
        let accepted = outcomes
            .iter()
            .filter(|o| matches!(o, AcceptWorkerResultOutcome::Accepted { .. }))
            .count();
        let refused = outcomes
            .iter()
            .filter(|o| matches!(o, AcceptWorkerResultOutcome::Refused { .. }))
            .count();
        assert_eq!((accepted, refused), (1, 1), "{outcomes:?}");
        assert_eq!(completions(&fx.task().await), 1);
    }

    #[tokio::test]
    async fn a_tip_that_advanced_past_the_recorded_observation_can_be_accepted_exactly() {
        let fx = fixture().await;
        let observed = fx.tip.clone();
        let newer = fx.commit_in_checkout("more.txt");
        fx.edit(|t| {
            t.worker_handoff.as_mut().unwrap().stage = WorkerHandoffStage::RecoveryRequired {
                reason: "interrupted".into(),
                observed_commit: Some(observed.clone()),
            }
        })
        .await;
        let mut request = fx.request(Uuid::new_v4());
        request.observed_commit = Some(observed);
        request.tip = newer.clone();

        let outcome = accept_result(&fx.state, request).await.unwrap();

        let AcceptWorkerResultOutcome::Accepted { task } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(
            task.worker_handoff.unwrap().stage,
            WorkerHandoffStage::ResolvedToAiReview { commit: newer }
        );
    }

    #[tokio::test]
    async fn a_tip_that_moved_after_it_was_shown_is_never_silently_replaced() {
        let fx = fixture().await;
        let request = fx.request(Uuid::new_v4());
        fx.commit_in_checkout("late.txt");
        let (task, repo_state) = (fx.task().await, fx.repo_state());

        let reason = refusal(accept_result(&fx.state, request).await);

        assert!(reason.contains("not the commit you approved"), "{reason}");
        assert_untouched(&fx, &task, &repo_state).await;
    }

    #[tokio::test]
    async fn an_observed_commit_that_is_not_an_ancestor_of_the_tip_is_refused() {
        let fx = fixture().await;
        git(&fx.repo, &["branch", "side", &fx.start]);
        let side = tempfile::TempDir::new().unwrap();
        let side_dir = side.path().join("side");
        git(
            &fx.repo,
            &["worktree", "add", "-q", side_dir.to_str().unwrap(), "side"],
        );
        std::fs::write(side_dir.join("other.txt"), "x").unwrap();
        git(&side_dir, &["add", "-A"]);
        git(&side_dir, &["commit", "-q", "-m", "other"]);
        let foreign = git(&side_dir, &["rev-parse", "HEAD"]);
        fx.edit(|t| {
            t.worker_handoff.as_mut().unwrap().stage = WorkerHandoffStage::RecoveryRequired {
                reason: "interrupted".into(),
                observed_commit: Some(foreign.clone()),
            }
        })
        .await;
        let mut request = fx.request(Uuid::new_v4());
        request.observed_commit = Some(foreign);

        let reason = refusal(accept_result(&fx.state, request).await);

        assert!(
            reason.contains("observed commit is not an ancestor"),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn a_tip_that_does_not_descend_from_the_recorded_start_is_refused() {
        let fx = fixture().await;
        let unrelated = "e".repeat(40);
        fx.edit(|t| t.worker_handoff.as_mut().unwrap().start_head = Some(unrelated))
            .await;

        let reason = refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await);

        assert!(
            reason.contains("Ancestry") || reason.contains("descend"),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn a_tip_equal_to_the_start_is_not_a_result() {
        let fx = fixture().await;
        git(&fx.checkout, &["reset", "-q", "--hard", &fx.start]);
        let mut request = fx.request(Uuid::new_v4());
        request.tip = fx.start.clone();

        let reason = refusal(accept_result(&fx.state, request).await);

        assert!(reason.contains("no result to accept"), "{reason}");
    }

    #[tokio::test]
    async fn legacy_evidence_without_a_start_head_cannot_be_accepted() {
        let fx = fixture().await;
        fx.edit(|t| t.worker_handoff.as_mut().unwrap().start_head = None)
            .await;

        let reason = refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await);
        assert!(reason.contains("not reliably recorded"), "{reason}");

        fx.edit(|t| {
            let h = t.worker_handoff.as_mut().unwrap();
            h.start_head = Some(fx.start.clone());
            h.run = None;
        })
        .await;
        let evidence = worker_recovery_evidence(&fx.state, fx.task_id)
            .await
            .unwrap();
        assert!(!evidence.eligible);
    }

    #[tokio::test]
    async fn a_dirty_checkout_is_refused_and_left_alone() {
        let fx = fixture().await;
        std::fs::write(fx.checkout.join("scratch.txt"), "unsaved").unwrap();
        let (task, repo_state) = (fx.task().await, fx.repo_state());

        let reason = refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await);

        assert!(reason.contains("dirty"), "{reason}");
        assert_untouched(&fx, &task, &repo_state).await;
        assert_eq!(
            std::fs::read_to_string(fx.checkout.join("scratch.txt")).unwrap(),
            "unsaved"
        );
        let evidence = worker_recovery_evidence(&fx.state, fx.task_id)
            .await
            .unwrap();
        assert_eq!(evidence.checkout.unwrap().clean, Some(false));
    }

    #[tokio::test]
    async fn a_missing_or_unregistered_checkout_is_refused() {
        let fx = fixture().await;
        let moved = fx.checkout.with_extension("moved");
        std::fs::rename(&fx.checkout, &moved).unwrap();
        let reason = refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await);
        assert!(
            reason.contains("missing") || reason.contains("unavailable"),
            "{reason}"
        );

        // A directory at the recorded path that Git does not know.
        std::fs::create_dir_all(&fx.checkout).unwrap();
        git(&fx.checkout, &["init", "-q", "-b", &fx.branch]);
        let reason = refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await);
        assert!(!reason.is_empty());
        assert_eq!(fx.task().await.status, TaskStatus::Error);
    }

    #[tokio::test]
    async fn a_locked_registration_is_refused() {
        let fx = fixture().await;
        git(
            &fx.repo,
            &["worktree", "lock", fx.checkout.to_str().unwrap()],
        );

        let reason = refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await);

        assert!(reason.contains("locked"), "{reason}");
    }

    #[tokio::test]
    async fn another_task_claiming_the_branch_makes_ownership_ambiguous() {
        let fx = fixture().await;
        let mut other = create_test_task_full("other", fx.project_id, TaskStatus::Backlog, 1);
        other.branch_name = Some(fx.branch.clone());
        fx.state.task.tasks.write().await.insert(other.id, other);

        let reason = refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await);

        assert!(reason.contains("Another Task"), "{reason}");
    }

    #[tokio::test]
    async fn a_live_executor_owner_blocks_acceptance() {
        let fx = fixture().await;
        fx.state
            .executor
            .get()
            .unwrap()
            .hold_running_for_test(fx.task_id)
            .await;
        let (task, repo_state) = (fx.task().await, fx.repo_state());

        let reason = refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await);

        assert!(reason.contains("currently owns"), "{reason}");
        assert_untouched(&fx, &task, &repo_state).await;
        let evidence = worker_recovery_evidence(&fx.state, fx.task_id)
            .await
            .unwrap();
        assert!(evidence.active_execution);
    }

    #[tokio::test]
    async fn stale_expectations_are_refused_field_by_field() {
        let fx = fixture().await;
        let base = fx.request(Uuid::new_v4());
        let variants: Vec<(&str, AcceptWorkerResultRequest)> = vec![
            (
                "run",
                AcceptWorkerResultRequest {
                    run: 2,
                    ..base.clone()
                },
            ),
            (
                "branch",
                AcceptWorkerResultRequest {
                    branch: "other".into(),
                    ..base.clone()
                },
            ),
            (
                "checkout",
                AcceptWorkerResultRequest {
                    checkout: "/elsewhere".into(),
                    ..base.clone()
                },
            ),
            (
                "observed",
                AcceptWorkerResultRequest {
                    observed_commit: Some("a".repeat(40)),
                    ..base.clone()
                },
            ),
            (
                "repository",
                AcceptWorkerResultRequest {
                    repository_id: Uuid::new_v4(),
                    ..base.clone()
                },
            ),
            (
                "project",
                AcceptWorkerResultRequest {
                    project_id: Uuid::new_v4(),
                    ..base.clone()
                },
            ),
            (
                "stage",
                AcceptWorkerResultRequest {
                    stage: "started".into(),
                    ..base.clone()
                },
            ),
        ];
        let (task, repo_state) = (fx.task().await, fx.repo_state());
        for (field, request) in variants {
            let outcome = accept_result(&fx.state, request).await;
            assert!(
                matches!(outcome, Ok(AcceptWorkerResultOutcome::Refused { .. })),
                "{field}: {outcome:?}"
            );
        }
        assert_untouched(&fx, &task, &repo_state).await;
    }

    #[tokio::test]
    async fn a_task_that_moved_out_of_recovery_or_already_ended_the_run_is_refused() {
        let fx = fixture().await;
        fx.edit(|t| t.status = TaskStatus::HumanReview).await;
        let (before, repo_state) = (fx.task().await, fx.repo_state());
        let reason = refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await);
        assert!(reason.contains("lifecycle state"), "{reason}");
        assert_untouched(&fx, &before, &repo_state).await;

        fx.edit(|t| {
            t.status = TaskStatus::Error;
            t.record_activity(ActivityKind::RunFailed {
                run: Some(1),
                reason: "x".into(),
            });
        })
        .await;
        let reason = refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await);
        assert!(reason.contains("durable completion or failure"), "{reason}");
    }

    #[tokio::test]
    async fn a_write_that_publishes_nothing_leaves_the_task_in_recovery() {
        let fx = fixture().await;
        let (task, repo_state) = (fx.task().await, fx.repo_state());

        let reason = {
            let _fault = fault::fail_at(Stage::Replace, std::io::ErrorKind::Other);
            refusal(accept_result(&fx.state, fx.request(Uuid::new_v4())).await)
        };

        assert!(reason.contains("Nothing was recorded"), "{reason}");
        assert_untouched(&fx, &task, &repo_state).await;
        assert!(matches!(
            fx.on_disk().worker_handoff.unwrap().stage,
            WorkerHandoffStage::RecoveryRequired { .. }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_published_but_unconfirmed_write_is_reported_and_stays_gated_until_revalidated() {
        let fx = fixture().await;

        let outcome = {
            let _fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
            accept_result(&fx.state, fx.request(Uuid::new_v4()))
                .await
                .unwrap()
        };

        let AcceptWorkerResultOutcome::DurabilityUnconfirmed { task, .. } = outcome else {
            panic!("expected DurabilityUnconfirmed, got {outcome:?}");
        };
        assert_eq!(task.status, TaskStatus::AiReview);
        assert!(
            !task.worker_handoff_confirmed,
            "an unconfirmed write earns no acknowledgement"
        );
        assert!(
            task.worker_recovery_refusal().is_some(),
            "AI Review is not admitted"
        );

        // The executor's reconciliation re-validates the commit and then
        // confirms; it neither repeats the completion nor starts a Worker.
        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        let executor = fx.state.executor.get().unwrap().clone();
        executor.reconcile_committed_worker_handoffs().await;
        let confirmed = fx.task().await;
        assert!(confirmed.worker_handoff_confirmed);
        assert!(confirmed.worker_recovery_refusal().is_none());
        assert_eq!(completions(&confirmed), 1);
        assert!(
            !agent.invocations().contains("claude"),
            "no coding Worker was launched"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn after_a_restart_review_waits_for_the_commit_to_be_revalidated() {
        let fx = fixture().await;
        accept_result(&fx.state, fx.request(Uuid::new_v4()))
            .await
            .unwrap();
        let reloaded = fx.on_disk();
        assert!(
            !reloaded.worker_handoff_confirmed,
            "the acknowledgement is process-local"
        );
        fx.state
            .task
            .tasks
            .write()
            .await
            .insert(fx.task_id, reloaded);
        assert!(fx.task().await.worker_recovery_refusal().is_some());

        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        fx.state
            .executor
            .get()
            .unwrap()
            .reconcile_committed_worker_handoffs()
            .await;

        let task = fx.task().await;
        assert!(task.worker_handoff_confirmed);
        assert_eq!(task.status, TaskStatus::AiReview);
        assert_eq!(completions(&task), 1);
        assert!(!agent.invocations().contains("claude"));
    }

    #[tokio::test]
    async fn if_the_branch_moves_before_a_restart_the_acceptance_is_not_trusted() {
        let fx = fixture().await;
        accept_result(&fx.state, fx.request(Uuid::new_v4()))
            .await
            .unwrap();
        let reloaded = fx.on_disk();
        fx.state
            .task
            .tasks
            .write()
            .await
            .insert(fx.task_id, reloaded);
        fx.commit_in_checkout("sneaky.txt");

        fx.state
            .executor
            .get()
            .unwrap()
            .reconcile_committed_worker_handoffs()
            .await;

        let task = fx.task().await;
        assert!(matches!(
            task.worker_handoff.as_ref().unwrap().stage,
            WorkerHandoffStage::RecoveryRequired { .. }
        ));
        assert_eq!(
            task.needs_you(false),
            Some(crate::domain::task::AttentionReason::Recovery)
        );
    }

    #[tokio::test]
    async fn an_acceptance_that_stopped_verifying_can_be_decided_again_on_the_new_tip() {
        let fx = fixture().await;
        let first = fx.tip.clone();
        accept_result(&fx.state, fx.request(Uuid::new_v4()))
            .await
            .unwrap();
        let reloaded = fx.on_disk();
        fx.state
            .task
            .tasks
            .write()
            .await
            .insert(fx.task_id, reloaded);
        let newer = fx.commit_in_checkout("after.txt");
        fx.state
            .executor
            .get()
            .unwrap()
            .reconcile_committed_worker_handoffs()
            .await;
        assert!(matches!(
            fx.task().await.worker_handoff.unwrap().stage,
            WorkerHandoffStage::RecoveryRequired { .. }
        ));

        let mut request = fx.request(Uuid::new_v4());
        request.observed_commit = Some(first);
        request.tip = newer.clone();
        let outcome = accept_result(&fx.state, request).await.unwrap();

        let AcceptWorkerResultOutcome::Accepted { task } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(
            task.worker_handoff.clone().unwrap().stage,
            WorkerHandoffStage::ResolvedToAiReview { commit: newer }
        );
        assert_eq!(completions(&task), 1, "completions stay once-only");
    }

    // ---- Refusal, barrier and review-admission regressions ----------------

    /// Hold a read guard on the Task board, start `decision`, and wait until
    /// it is queued for the write lock. No timing: a queued writer makes
    /// `try_read` fail, and that is the condition awaited. `at_barrier` runs
    /// with the decision parked between validation and persistence.
    #[cfg(unix)]
    async fn decide_with_barrier<T>(
        fx: &Fx,
        decision: AcceptWorkerResultRequest,
        at_barrier: impl FnOnce() -> T,
    ) -> Result<AcceptWorkerResultOutcome, String> {
        let guard = fx.state.task.tasks.read().await;
        let accept = accept_result(&fx.state, decision);
        let driver = async {
            tokio::time::timeout(std::time::Duration::from_secs(60), async {
                while fx.state.task.tasks.try_read().is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the decision never reached persistence");
            at_barrier();
            drop(guard);
        };
        let (outcome, ()) = tokio::join!(accept, driver);
        outcome
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_tip_that_moves_between_validation_and_persistence_is_never_reviewed() {
        let fx = fixture().await;
        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        let approved = fx.tip.clone();

        let mut moved = String::new();
        let outcome = decide_with_barrier(&fx, fx.request(Uuid::new_v4()), || {
            moved = fx.commit_in_checkout("late.txt");
        })
        .await
        .unwrap();

        let AcceptWorkerResultOutcome::Refused { reason } = outcome else {
            panic!("a decision on a tip that moved is not an acceptance: {outcome:?}");
        };
        assert!(reason.contains("moved away"), "{reason}");
        assert_ne!(moved, approved);

        // The exact approved commit was never replaced by the new tip, and the
        // moved branch and checkout were left exactly as the writer made them.
        let task = fx.task().await;
        assert_eq!(task.status, TaskStatus::Error);
        let handoff = task.worker_handoff.clone().unwrap();
        assert!(matches!(
            &handoff.stage,
            WorkerHandoffStage::RecoveryRequired { observed_commit: Some(c), .. } if *c == approved
        ));
        assert_eq!(handoff.acceptance.unwrap().commit, approved);
        assert!(!task.worker_handoff_confirmed);
        assert_eq!(
            git(
                &fx.repo,
                &["rev-parse", &format!("refs/heads/{}", fx.branch)]
            ),
            moved
        );
        assert_eq!(git(&fx.checkout, &["rev-parse", "HEAD"]), moved);
        assert_eq!(
            git(&fx.checkout, &["status", "--porcelain=v1"]),
            "",
            "the checkout was not touched"
        );
        assert!(matches!(
            fx.on_disk().worker_handoff.unwrap().stage,
            WorkerHandoffStage::RecoveryRequired { .. }
        ));

        fx.state.executor.get().unwrap().poll_once_for_test().await;
        assert_eq!(fx.task().await.status, TaskStatus::Error);
        assert!(
            !agent.invocations().contains("claude"),
            "neither review nor a coding Worker was launched"
        );
        assert_eq!(completions(&fx.task().await), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_tip_that_moves_after_acceptance_is_caught_before_review_is_admitted() {
        let fx = fixture().await;
        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        let accepted = fx.tip.clone();
        let outcome = accept_result(&fx.state, fx.request(Uuid::new_v4()))
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            AcceptWorkerResultOutcome::Accepted { .. }
        ));
        assert!(
            fx.task().await.worker_handoff_confirmed,
            "the acknowledgement was earned while the tip still matched"
        );

        // An outside Git writer moves the branch after the acknowledgement.
        let moved = fx.commit_in_checkout("outside.txt");
        fx.state.executor.get().unwrap().poll_once_for_test().await;

        let task = fx.task().await;
        assert_eq!(task.status, TaskStatus::Error, "review was not admitted");
        assert!(matches!(
            &task.worker_handoff.as_ref().unwrap().stage,
            WorkerHandoffStage::RecoveryRequired { observed_commit: Some(c), .. } if *c == accepted
        ));
        assert!(!task
            .activity
            .iter()
            .any(|e| matches!(e.kind, ActivityKind::AiReviewStarted { .. })));
        assert!(!agent.invocations().contains("claude"));
        assert_eq!(
            git(
                &fx.repo,
                &["rev-parse", &format!("refs/heads/{}", fx.branch)]
            ),
            moved,
            "the outside writer's commit is preserved"
        );
    }

    #[tokio::test]
    async fn review_admission_records_the_start_durably_and_a_later_fix_commit_is_not_recovery() {
        let fx = fixture().await;
        accept_result(&fx.state, fx.request(Uuid::new_v4()))
            .await
            .unwrap();
        let executor = fx.state.executor.get().unwrap().clone();
        let refs_before = git(&fx.repo, &["for-each-ref"]);

        assert!(executor.worker_commit_still_verifies(fx.task_id).await);
        assert!(
            !fx.task()
                .await
                .activity
                .iter()
                .any(|e| matches!(e.kind, ActivityKind::AiReviewStarted { .. })),
            "proving the commit records nothing"
        );
        assert!(executor.record_review_admission(fx.task_id, 1).await);

        let admitted = |task: &Task| {
            task.activity
                .iter()
                .filter(|e| matches!(e.kind, ActivityKind::AiReviewStarted { review: 1 }))
                .count()
        };
        assert_eq!(admitted(&fx.task().await), 1);
        assert_eq!(admitted(&fx.on_disk()), 1, "durable before any Git write");
        assert_eq!(git(&fx.repo, &["for-each-ref"]), refs_before);

        // The review's fix agent commits; a retried review of the same result
        // is admitted without being sent back to recovery.
        let fix = fx.commit_in_checkout("fix.txt");
        assert!(executor.worker_commit_still_verifies(fx.task_id).await);
        assert!(executor.record_review_admission(fx.task_id, 2).await);
        let task = fx.task().await;
        assert_eq!(task.status, TaskStatus::AiReview);
        assert!(matches!(
            task.worker_handoff.as_ref().unwrap().stage,
            WorkerHandoffStage::ResolvedToAiReview { .. }
        ));
        assert_eq!(admitted(&task), 1);
        assert_eq!(git(&fx.checkout, &["rev-parse", "HEAD"]), fix);
    }

    fn started(task: &Task) -> Vec<u32> {
        task.activity
            .iter()
            .filter_map(|e| match e.kind {
                ActivityKind::AiReviewStarted { review } => Some(review),
                _ => None,
            })
            .collect()
    }

    #[cfg(unix)]
    async fn settle(fx: &Fx) {
        let executor = fx.state.executor.get().unwrap().clone();
        for _ in 0..200 {
            if !executor.is_task_running(fx.task_id).await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("the review did not finish");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_review_without_capacity_leaves_no_marker_and_is_proved_again_before_it_starts() {
        let fx = fixture().await;
        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        accept_result(&fx.state, fx.request(Uuid::new_v4()))
            .await
            .unwrap();
        let executor = fx.state.executor.get().unwrap().clone();

        let held = executor.hold_all_capacity_for_test();
        executor.poll_once_for_test().await;
        assert_eq!(started(&fx.task().await), Vec::<u32>::new());
        assert_eq!(started(&fx.on_disk()), Vec::<u32>::new());
        assert_eq!(fx.task().await.status, TaskStatus::AiReview);

        // The branch changes while the review waits for capacity.
        let moved = fx.commit_in_checkout("outside.txt");
        drop(held);
        executor.poll_once_for_test().await;

        let task = fx.task().await;
        assert_eq!(task.status, TaskStatus::Error, "back in recovery");
        assert_eq!(started(&task), Vec::<u32>::new());
        assert!(!agent.invocations().contains("claude"), "nothing ran");
        assert_eq!(
            git(
                &fx.repo,
                &["rev-parse", &format!("refs/heads/{}", fx.branch)]
            ),
            moved
        );
        assert_eq!(completions(&task), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_review_that_waited_for_capacity_starts_once_under_one_number() {
        let fx = fixture().await;
        let _agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        accept_result(&fx.state, fx.request(Uuid::new_v4()))
            .await
            .unwrap();
        let executor = fx.state.executor.get().unwrap().clone();

        let held = executor.hold_all_capacity_for_test();
        executor.poll_once_for_test().await;
        executor.poll_once_for_test().await;
        assert_eq!(started(&fx.task().await), Vec::<u32>::new());
        drop(held);

        executor.poll_once_for_test().await;
        settle(&fx).await;

        let task = fx.task().await;
        assert_eq!(started(&task), vec![1], "one start, one number");
        assert_eq!(started(&fx.on_disk()), vec![1]);
        let numbers: Vec<u32> = task
            .activity
            .iter()
            .filter_map(|e| match e.kind {
                ActivityKind::AiReviewFailed { review, .. }
                | ActivityKind::AiReviewSkipped { review, .. }
                | ActivityKind::AiReviewApproved { review }
                | ActivityKind::AiReviewChangesRequested { review, .. } => Some(review),
                _ => None,
            })
            .collect();
        assert!(
            !numbers.is_empty() && numbers.iter().all(|n| *n == 1),
            "the review ran to an outcome under its admission number: {numbers:?}"
        );
        assert_eq!(completions(&task), 1);
    }

    #[tokio::test]
    async fn after_a_restart_an_admitted_review_is_not_sent_back_to_recovery_by_its_own_fix_commit()
    {
        let fx = fixture().await;
        accept_result(&fx.state, fx.request(Uuid::new_v4()))
            .await
            .unwrap();
        let executor = fx.state.executor.get().unwrap().clone();
        assert!(executor.worker_commit_still_verifies(fx.task_id).await);
        assert!(executor.record_review_admission(fx.task_id, 1).await);
        let fix = fx.commit_in_checkout("fix.txt");

        let reloaded = fx.on_disk();
        assert!(!reloaded.worker_handoff_confirmed);
        fx.state
            .task
            .tasks
            .write()
            .await
            .insert(fx.task_id, reloaded);
        executor.reconcile_committed_worker_handoffs().await;

        let task = fx.task().await;
        assert_eq!(task.status, TaskStatus::AiReview);
        assert!(task.worker_handoff_confirmed);
        assert_eq!(started(&task), vec![1]);
        assert_eq!(completions(&task), 1);
        assert_eq!(git(&fx.checkout, &["rev-parse", "HEAD"]), fix);
    }

    #[tokio::test]
    async fn an_admission_marker_that_is_not_durable_is_withdrawn_and_the_commit_proved_again() {
        let fx = fixture().await;
        accept_result(&fx.state, fx.request(Uuid::new_v4()))
            .await
            .unwrap();
        let executor = fx.state.executor.get().unwrap().clone();
        assert!(executor.worker_commit_still_verifies(fx.task_id).await);

        for stage in [Stage::Replace, Stage::SyncDirectory] {
            let admitted = {
                let _fault = fault::fail_at(stage, std::io::ErrorKind::Other);
                executor.record_review_admission(fx.task_id, 1).await
            };
            assert!(!admitted, "{stage:?}: no review without durable evidence");
            let task = fx.task().await;
            assert_eq!(started(&task), Vec::<u32>::new(), "{stage:?}");
            assert!(!task.review_admitted_for_current_result(), "{stage:?}");
        }

        // An outside writer moves the branch: the next attempt notices.
        fx.commit_in_checkout("outside.txt");
        assert!(!executor.worker_commit_still_verifies(fx.task_id).await);
        assert_eq!(fx.task().await.status, TaskStatus::Error);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_review_does_not_start_while_another_lifecycle_operation_owns_the_task() {
        let fx = fixture().await;
        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        accept_result(&fx.state, fx.request(Uuid::new_v4()))
            .await
            .unwrap();
        let executor = fx.state.executor.get().unwrap().clone();
        let before = fx.board_files();

        let lease = fx
            .state
            .task_lifecycle_locks
            .acquire(fx.task_id)
            .await
            .unwrap();
        executor.spawn_review_for_test(fx.task_id).await;
        drop(lease);

        assert!(!executor.is_task_running(fx.task_id).await);
        assert_eq!(started(&fx.task().await), Vec::<u32>::new());
        assert_eq!(fx.board_files(), before, "nothing was written");
        assert!(!agent.invocations().contains("claude"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unconfirmed_write_repeated_is_never_reported_applied_and_has_no_second_write() {
        let fx = fixture().await;
        let id = Uuid::new_v4();
        let outcome = {
            let _fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
            accept_result(&fx.state, fx.request(id)).await.unwrap()
        };
        assert!(matches!(
            outcome,
            AcceptWorkerResultOutcome::DurabilityUnconfirmed { .. }
        ));
        let (task, repo_state, board) = (fx.task().await, fx.repo_state(), fx.board_files());

        let again = accept_result(&fx.state, fx.request(id)).await.unwrap();

        assert!(
            matches!(
                again,
                AcceptWorkerResultOutcome::DurabilityUnconfirmed { .. }
            ),
            "{again:?}"
        );
        assert_untouched(&fx, &task, &repo_state).await;
        assert_eq!(fx.board_files(), board, "the repeat wrote nothing");
        assert!(
            !fx.task().await.worker_handoff_confirmed,
            "repeating the request does not earn the acknowledgement"
        );
        assert_eq!(completions(&fx.task().await), 1);

        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        fx.state
            .executor
            .get()
            .unwrap()
            .reconcile_committed_worker_handoffs()
            .await;
        assert!(fx.task().await.worker_handoff_confirmed);

        let confirmed = accept_result(&fx.state, fx.request(id)).await.unwrap();
        assert!(
            matches!(confirmed, AcceptWorkerResultOutcome::AlreadyApplied { .. }),
            "{confirmed:?}"
        );
        assert_eq!(completions(&fx.task().await), 1);
        assert!(!agent.invocations().contains("claude"));
    }

    #[tokio::test]
    async fn a_write_that_published_nothing_is_never_reported_applied_on_repeat() {
        let fx = fixture().await;
        let id = Uuid::new_v4();
        {
            let _fault = fault::fail_at(Stage::WriteTemp, std::io::ErrorKind::Other);
            let first = accept_result(&fx.state, fx.request(id)).await.unwrap();
            assert!(
                matches!(first, AcceptWorkerResultOutcome::Refused { .. }),
                "{first:?}"
            );
        }

        let again = accept_result(&fx.state, fx.request(id)).await.unwrap();

        assert!(
            matches!(again, AcceptWorkerResultOutcome::Accepted { .. }),
            "the retry is a first application: {again:?}"
        );
        assert_eq!(completions(&fx.task().await), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_or_stale_decision_never_launches_a_coding_worker() {
        let fx = fixture().await;
        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        let mut stale = fx.request(Uuid::new_v4());
        stale.run = 9;
        let (task, repo_state) = (fx.task().await, fx.repo_state());
        refusal(accept_result(&fx.state, stale).await);

        fx.state.executor.get().unwrap().poll_once_for_test().await;

        assert!(!agent.invocations().contains("claude"));
        assert!(!fx.task().await.is_ready_to_execute());
        assert_untouched(&fx, &task, &repo_state).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_new_human_acceptance_after_failed_revalidation_is_current_and_consistent() {
        let fx = fixture().await;
        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        let executor = fx.state.executor.get().unwrap().clone();
        let first = fx.tip.clone();
        let first_decision = Uuid::new_v4();
        accept_result(&fx.state, fx.request(first_decision))
            .await
            .unwrap();

        // Restart, then the branch moves: the acceptance stops verifying.
        let reloaded = fx.on_disk();
        fx.state
            .task
            .tasks
            .write()
            .await
            .insert(fx.task_id, reloaded);
        let second = fx.commit_in_checkout("after.txt");
        executor.reconcile_committed_worker_handoffs().await;

        let recovering = fx.task().await;
        assert_eq!(recovering.status, TaskStatus::Error);
        assert!(!recovering.is_ready_to_execute(), "never a coding Worker");
        assert!(matches!(
            &recovering.worker_handoff.as_ref().unwrap().stage,
            WorkerHandoffStage::RecoveryRequired { observed_commit: Some(c), .. } if *c == first
        ));
        assert!(!recovering.worker_handoff_confirmed);
        let evidence = worker_recovery_evidence(&fx.state, fx.task_id)
            .await
            .unwrap();
        assert_eq!(evidence.stage, "recovery_required");
        assert_eq!(evidence.branch_tip.as_deref(), Some(second.as_str()));
        assert!(evidence.eligible, "{:?}", evidence.refusals);
        // Repeated evaluation and a poll change nothing and launch nothing.
        let (settled, repo_state) = (fx.task().await, fx.repo_state());
        executor.poll_once_for_test().await;
        executor.reconcile_committed_worker_handoffs().await;
        assert_untouched(&fx, &settled, &repo_state).await;

        // The old decision is not silently treated as current.
        let replay = fx.request(first_decision);
        let reason = refusal(accept_result(&fx.state, replay).await);
        assert!(reason.contains("returned to recovery"), "{reason}");
        let mut altered = fx.request(first_decision);
        altered.observed_commit = Some(first.clone());
        let reason = refusal(accept_result(&fx.state, altered).await);
        assert!(reason.contains("different request"), "{reason}");
        assert_untouched(&fx, &settled, &repo_state).await;

        // A new decision binds the correct run and the new tip.
        let new_decision = Uuid::new_v4();
        let mut request = fx.request(new_decision);
        request.observed_commit = Some(first);
        request.tip = second.clone();
        let outcome = accept_result(&fx.state, request).await.unwrap();
        let AcceptWorkerResultOutcome::Accepted { task } = outcome else {
            panic!("{outcome:?}");
        };
        let handoff = task.worker_handoff.clone().unwrap();
        assert_eq!(handoff.run, Some(1));
        assert_eq!(
            handoff.stage,
            WorkerHandoffStage::ResolvedToAiReview {
                commit: second.clone()
            }
        );
        let acceptance = handoff.acceptance.unwrap();
        assert_eq!(acceptance.decision_id, new_decision);
        assert_eq!(acceptance.commit, second);
        assert_eq!(completions(&task), 1, "no duplicate RunCompleted");
        assert_eq!(
            task.activity
                .iter()
                .filter(|e| matches!(e.kind, ActivityKind::RecoveryRequired { .. }))
                .count(),
            1,
            "recovery is a once-only timeline milestone per run"
        );
        assert!(!task.is_ready_to_execute());

        // Restart again: still deterministic, still one completion.
        fx.state
            .task
            .tasks
            .write()
            .await
            .insert(fx.task_id, fx.on_disk());
        executor.reconcile_committed_worker_handoffs().await;
        let restarted = fx.task().await;
        assert!(restarted.worker_handoff_confirmed);
        assert_eq!(restarted.status, TaskStatus::AiReview);
        assert_eq!(completions(&restarted), 1);
        let (again, repo_state) = (fx.task().await, fx.repo_state());
        executor.reconcile_committed_worker_handoffs().await;
        assert_untouched(&fx, &again, &repo_state).await;
        assert!(!agent.invocations().contains("claude"));
    }

    /// The lock-time predicate is a pure compare-and-set: whichever fact moved
    /// after the preliminary read, it answers `false` and leaves the Task
    /// byte-identical, so the caller's conditional write has nothing to write.
    #[tokio::test]
    async fn the_final_compare_and_set_changes_nothing_when_any_fact_moved() {
        let fx = fixture().await;
        let request = fx.request(Uuid::new_v4());
        let decision = WorkerResultAcceptance {
            decision_id: request.decision_id,
            commit: request.tip.clone(),
            observed_commit: None,
            decided_at: chrono::Utc::now(),
        };
        let base = fx.task().await;
        let recorded_id = request.decision_id;
        type Change = Box<dyn Fn(&mut Task)>;
        let moved: Vec<(&str, Change)> = vec![
            ("status", Box::new(|t| t.status = TaskStatus::HumanReview)),
            ("branch", Box::new(|t| t.branch_name = Some("other".into()))),
            ("checkout", Box::new(|t| t.worktree_path = None)),
            (
                "pr",
                Box::new(|t| t.pr_url = Some("https://x.invalid/1".into())),
            ),
            ("cleanup", Box::new(|t| t.cleanup_in_flight = true)),
            (
                "run",
                Box::new(|t| t.worker_handoff.as_mut().unwrap().run = Some(2)),
            ),
            (
                "repository",
                Box::new(|t| t.worker_handoff.as_mut().unwrap().repository_id = None),
            ),
            (
                "observed",
                Box::new(|t| {
                    t.worker_handoff.as_mut().unwrap().stage =
                        WorkerHandoffStage::RecoveryRequired {
                            reason: "r".into(),
                            observed_commit: Some("a".repeat(40)),
                        }
                }),
            ),
            (
                "stage",
                Box::new(|t| {
                    t.worker_handoff.as_mut().unwrap().stage =
                        WorkerHandoffStage::ResolvedToAiReview {
                            commit: "a".repeat(40),
                        }
                }),
            ),
            (
                "same decision already recorded",
                Box::new(move |t| {
                    t.worker_handoff.as_mut().unwrap().acceptance = Some(WorkerResultAcceptance {
                        decision_id: recorded_id,
                        commit: "a".repeat(40),
                        observed_commit: None,
                        decided_at: chrono::Utc::now(),
                    })
                }),
            ),
        ];
        for (field, change) in moved {
            let mut task = base.clone();
            change(&mut task);
            let before = serde_json::to_string(&task).unwrap();
            assert!(
                !apply_acceptance(&mut task, &request, &decision, chrono::Utc::now()),
                "{field}"
            );
            assert_eq!(serde_json::to_string(&task).unwrap(), before, "{field}");
        }
        let mut task = base.clone();
        assert!(apply_acceptance(
            &mut task,
            &request,
            &decision,
            chrono::Utc::now()
        ));
        assert_eq!(task.status, TaskStatus::AiReview);
    }

    // ---- Stale confirmation and admission-marker trust --------------------

    /// Accept the tip, prove it, and admit review 1 durably. The process then
    /// "crashes": the Task is reloaded from disk and its acknowledgement lost.
    async fn admitted_then_restarted() -> Fx {
        let fx = admitted().await;
        let reloaded = fx.on_disk();
        assert!(!reloaded.worker_handoff_confirmed);
        fx.state
            .task
            .tasks
            .write()
            .await
            .insert(fx.task_id, reloaded);
        fx
    }

    /// Accept the tip, prove it, and admit review 1 durably, in one process.
    async fn admitted() -> Fx {
        let fx = fixture().await;
        let accepted = accept_result(&fx.state, fx.request(Uuid::new_v4()))
            .await
            .unwrap();
        assert!(matches!(
            accepted,
            AcceptWorkerResultOutcome::Accepted { .. }
        ));
        let executor = fx.state.executor.get().unwrap().clone();
        assert!(executor.worker_commit_still_verifies(fx.task_id).await);
        assert!(executor.record_review_admission(fx.task_id, 1).await);
        assert_eq!(started(&fx.task().await), vec![1]);
        fx
    }

    /// Whether startup reconciliation lets the Task go on to review.
    async fn startup_allows(fx: &Fx) -> bool {
        fx.state
            .executor
            .get()
            .unwrap()
            .reconcile_committed_worker_handoffs()
            .await;
        let task = fx.task().await;
        task.status == TaskStatus::AiReview && task.worker_handoff_confirmed
    }

    /// Run `damage` against a Task whose first review was admitted, then ask
    /// both places that decide whether review may go on: startup
    /// reconciliation (after a restart) and the admission of a later review.
    async fn at_both_entry_points(damage: impl Fn(&Fx)) -> Vec<(&'static str, Fx, bool)> {
        let restarted = admitted_then_restarted().await;
        damage(&restarted);
        let allowed = startup_allows(&restarted).await;
        let mut outcomes = vec![("startup", restarted, allowed)];

        let running = admitted().await;
        damage(&running);
        let allowed = running
            .state
            .executor
            .get()
            .unwrap()
            .worker_commit_still_verifies(running.task_id)
            .await;
        outcomes.push(("admission", running, allowed));
        outcomes
    }

    /// Everything a refusal must leave as it found it.
    #[cfg(unix)]
    fn checkout_facts(fx: &Fx) -> String {
        format!(
            "{}\n{}\n{}",
            git(&fx.repo, &["for-each-ref"]),
            git(&fx.checkout, &["rev-parse", "HEAD"]),
            git(
                &fx.checkout,
                &["status", "--porcelain=v1", "--untracked-files=all"]
            ),
        )
    }

    async fn assert_held_for_recovery(entry: &str, fx: &Fx) {
        let task = fx.task().await;
        assert_eq!(task.status, TaskStatus::Error, "{entry}: back in recovery");
        assert!(
            matches!(
                task.worker_handoff.as_ref().unwrap().stage,
                WorkerHandoffStage::RecoveryRequired { .. }
            ),
            "{entry}"
        );
        assert!(!task.worker_handoff_confirmed, "{entry}");
        assert_eq!(started(&task), vec![1], "{entry}: no second admission");
        assert_eq!(completions(&task), 1, "{entry}");
    }

    #[test]
    fn a_new_acceptance_withdraws_the_previous_confirmation() {
        let request = AcceptWorkerResultRequest {
            decision_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            repository_id: Uuid::new_v4(),
            run: 1,
            stage: "recovery_required".into(),
            branch: "b".into(),
            checkout: "/c".into(),
            observed_commit: None,
            tip: "a".repeat(40),
        };
        let mut task = create_test_task_full("t", request.project_id, TaskStatus::Error, 0);
        task.id = request.task_id;
        task.branch_name = Some("b".into());
        task.worktree_path = Some("/c".into());
        task.worker_handoff = Some(crate::domain::task::WorkerHandoff {
            task_id: request.task_id,
            project_id: request.project_id,
            repository_id: Some(request.repository_id),
            run: Some(1),
            branch: Some("b".into()),
            start_head: Some("b".repeat(40)),
            acceptance: None,
            stage: WorkerHandoffStage::RecoveryRequired {
                reason: "r".into(),
                observed_commit: None,
            },
        });
        task.worker_handoff_confirmed = true;
        let decision = WorkerResultAcceptance {
            decision_id: request.decision_id,
            commit: request.tip.clone(),
            observed_commit: None,
            decided_at: chrono::Utc::now(),
        };

        assert!(apply_acceptance(
            &mut task,
            &request,
            &decision,
            chrono::Utc::now()
        ));

        assert!(
            !task.worker_handoff_confirmed,
            "the new evidence has earned nothing yet"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_confirmation_never_survives_recovery_into_a_later_unconfirmed_acceptance() {
        let fx = fixture().await;
        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        let executor = fx.state.executor.get().unwrap().clone();
        let first_tip = fx.tip.clone();
        let first_decision = Uuid::new_v4();

        // One process throughout: nothing is recreated from disk.
        let outcome = accept_result(&fx.state, fx.request(first_decision))
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            AcceptWorkerResultOutcome::Accepted { .. }
        ));
        assert!(fx.task().await.worker_handoff_confirmed);

        // The branch moves and the Task returns to recovery.
        let second_tip = fx.commit_in_checkout("later.txt");
        assert!(!executor.worker_commit_still_verifies(fx.task_id).await);
        let recovering = fx.task().await;
        assert_eq!(recovering.status, TaskStatus::Error);
        assert!(
            !recovering.worker_handoff_confirmed,
            "returning to recovery withdraws the confirmation"
        );

        // A new decision is published but its write cannot be confirmed.
        let second_decision = Uuid::new_v4();
        let mut request = fx.request(second_decision);
        request.observed_commit = Some(first_tip.clone());
        request.tip = second_tip.clone();
        let outcome = {
            let _fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
            accept_result(&fx.state, request.clone()).await.unwrap()
        };
        let AcceptWorkerResultOutcome::DurabilityUnconfirmed { task, .. } = outcome else {
            panic!("expected DurabilityUnconfirmed, got {outcome:?}");
        };
        assert!(!task.worker_handoff_confirmed, "the stale flag is gone");
        assert!(!fx.task().await.worker_handoff_confirmed);
        assert!(fx.task().await.worker_recovery_refusal().is_some());

        // An identical replay is still unconfirmed, and review stays blocked.
        let replay = accept_result(&fx.state, request.clone()).await.unwrap();
        assert!(
            matches!(
                replay,
                AcceptWorkerResultOutcome::DurabilityUnconfirmed { .. }
            ),
            "{replay:?}"
        );
        // An earlier decision cannot confirm or re-apply a later one.
        let earlier = refusal(accept_result(&fx.state, fx.request(first_decision)).await);
        assert!(!earlier.is_empty());
        assert!(!fx.task().await.worker_handoff_confirmed);
        // Review is gated on the acknowledgement: until this process
        // re-validates, the Task is held and nothing was admitted.
        let held = fx.task().await;
        assert_eq!(held.status, TaskStatus::AiReview);
        assert!(held.worker_recovery_refusal().is_some());
        assert!(!held.is_ready_to_execute());
        assert_eq!(started(&held), Vec::<u32>::new(), "no review was admitted");

        // Only its own re-validation confirms it, and for the new commit.
        executor.reconcile_committed_worker_handoffs().await;
        let confirmed = fx.task().await;
        assert!(confirmed.worker_handoff_confirmed);
        let handoff = confirmed.worker_handoff.clone().unwrap();
        assert_eq!(
            handoff.stage,
            WorkerHandoffStage::ResolvedToAiReview {
                commit: second_tip.clone()
            }
        );
        assert_eq!(handoff.acceptance.unwrap().decision_id, second_decision);
        assert_eq!(completions(&confirmed), 1);
        assert!(
            !agent.invocations().contains("claude"),
            "no coding Worker was launched before confirmation"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_new_acceptance_that_published_nothing_leaves_the_previous_state_and_no_phantom() {
        let fx = fixture().await;
        let executor = fx.state.executor.get().unwrap().clone();
        let first_tip = fx.tip.clone();
        let first_decision = Uuid::new_v4();
        accept_result(&fx.state, fx.request(first_decision))
            .await
            .unwrap();
        let second_tip = fx.commit_in_checkout("later.txt");
        assert!(!executor.worker_commit_still_verifies(fx.task_id).await);
        let before = fx.task().await;

        let mut request = fx.request(Uuid::new_v4());
        request.observed_commit = Some(first_tip.clone());
        request.tip = second_tip;
        let reason = {
            let _fault = fault::fail_at(Stage::WriteTemp, std::io::ErrorKind::Other);
            refusal(accept_result(&fx.state, request).await)
        };

        assert!(reason.contains("Nothing was recorded"), "{reason}");
        let after = fx.task().await;
        assert_eq!(after.worker_handoff, before.worker_handoff);
        assert_eq!(
            after
                .worker_handoff
                .as_ref()
                .unwrap()
                .acceptance
                .as_ref()
                .unwrap()
                .decision_id,
            first_decision,
            "the previous authoritative decision is untouched"
        );
        assert!(!after.worker_handoff_confirmed);
        assert_eq!(completions(&after), 1);
    }

    #[tokio::test]
    async fn a_marker_with_an_untouched_branch_lets_review_continue_without_a_second_admission() {
        for (entry, fx, allowed) in at_both_entry_points(|_| {}).await {
            assert!(allowed, "{entry}");
            let task = fx.task().await;
            assert_eq!(task.status, TaskStatus::AiReview, "{entry}");
            assert_eq!(started(&task), vec![1], "{entry}");
            assert_eq!(completions(&task), 1, "{entry}");
        }
    }

    #[tokio::test]
    async fn a_marker_does_not_excuse_a_branch_rewound_before_the_accepted_commit() {
        for (entry, fx, allowed) in at_both_entry_points(|fx| {
            drop(git(&fx.checkout, &["reset", "-q", "--hard", &fx.start]))
        })
        .await
        {
            assert!(!allowed, "{entry}");
            assert_held_for_recovery(entry, &fx).await;
            let reason = fx.task().await.error_message.unwrap();
            assert!(
                reason.contains("rewritten or rewound"),
                "{entry}: the reason names the rewritten history: {reason}"
            );
            assert!(reason.contains(&fx.tip[..12]), "{entry}: {reason}");
        }
    }

    #[tokio::test]
    async fn a_marker_does_not_excuse_a_checkout_switched_to_another_branch() {
        for (entry, fx, allowed) in at_both_entry_points(|fx| {
            git(&fx.checkout, &["checkout", "-q", "-b", "elsewhere"]);
        })
        .await
        {
            assert!(!allowed, "{entry}");
            assert_held_for_recovery(entry, &fx).await;
            assert_eq!(
                git(&fx.checkout, &["rev-parse", "--abbrev-ref", "HEAD"]),
                "elsewhere",
                "{entry}: the checkout was left as found"
            );
        }
    }

    #[tokio::test]
    async fn a_marker_does_not_excuse_a_detached_checkout() {
        for (entry, fx, allowed) in at_both_entry_points(|fx| {
            git(&fx.checkout, &["checkout", "-q", "--detach"]);
        })
        .await
        {
            assert!(!allowed, "{entry}");
            assert_held_for_recovery(entry, &fx).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_marker_does_not_let_a_new_process_absorb_unknown_dirty_changes() {
        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        for (entry, fx, allowed) in at_both_entry_points(|fx| {
            std::fs::write(fx.checkout.join("unknown.txt"), "not ours").unwrap();
        })
        .await
        {
            assert!(!allowed, "{entry}");
            assert_held_for_recovery(entry, &fx).await;
            assert_eq!(
                std::fs::read_to_string(fx.checkout.join("unknown.txt")).unwrap(),
                "not ours",
                "{entry}: the file was preserved"
            );
            assert!(fx.task().await.error_message.unwrap().contains("dirty"));
        }

        // Through the poller: no reviewer, no fix agent, nothing committed.
        let fx = admitted_then_restarted().await;
        std::fs::write(fx.checkout.join("unknown.txt"), "not ours").unwrap();
        let (tip, facts) = (
            git(&fx.checkout, &["rev-parse", "HEAD"]),
            checkout_facts(&fx),
        );
        let executor = fx.state.executor.get().unwrap().clone();
        for _ in 0..3 {
            executor.poll_once_for_test().await;
        }
        assert!(
            !agent.invocations().contains("claude"),
            "nothing was launched"
        );
        assert_eq!(git(&fx.checkout, &["rev-parse", "HEAD"]), tip);
        assert_eq!(checkout_facts(&fx), facts);
        assert_eq!(fx.task().await.status, TaskStatus::Error);
        assert_eq!(started(&fx.task().await), vec![1]);
    }

    #[tokio::test]
    async fn a_marker_still_allows_review_after_a_fix_commit_in_a_clean_checkout() {
        for (entry, fx, allowed) in at_both_entry_points(|fx| {
            fx.commit_in_checkout("fix.txt");
        })
        .await
        {
            assert!(allowed, "{entry}");
            let task = fx.task().await;
            assert_eq!(task.status, TaskStatus::AiReview, "{entry}");
            assert_eq!(started(&task), vec![1], "{entry}");
            assert_eq!(completions(&task), 1, "{entry}");
        }
    }

    #[tokio::test]
    async fn a_marker_does_not_excuse_a_missing_locked_or_duplicated_registration() {
        for (entry, fx, allowed) in
            at_both_entry_points(|fx| std::fs::remove_dir_all(&fx.checkout).unwrap()).await
        {
            assert!(!allowed, "{entry}: missing checkout");
            assert_held_for_recovery(entry, &fx).await;
        }
        for (entry, fx, allowed) in at_both_entry_points(|fx| {
            git(
                &fx.repo,
                &["worktree", "lock", fx.checkout.to_str().unwrap()],
            );
        })
        .await
        {
            assert!(!allowed, "{entry}: locked registration");
            assert_held_for_recovery(entry, &fx).await;
        }
        for (entry, fx, allowed) in at_both_entry_points(|fx| {
            let twin = fx._tmp.path().join("twin");
            git(
                &fx.repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-f",
                    twin.to_str().unwrap(),
                    &fx.branch,
                ],
            );
        })
        .await
        {
            assert!(!allowed, "{entry}: ambiguous ownership");
            assert_held_for_recovery(entry, &fx).await;
        }
    }

    #[tokio::test]
    async fn a_marker_does_not_excuse_an_accepted_commit_that_no_longer_exists() {
        for (entry, fx, allowed) in at_both_entry_points(|fx| {
            git(&fx.checkout, &["reset", "-q", "--hard", &fx.start]);
            git(&fx.repo, &["reflog", "expire", "--expire=now", "--all"]);
            git(&fx.repo, &["gc", "-q", "--prune=now"]);
        })
        .await
        {
            assert!(!allowed, "{entry}");
            assert_held_for_recovery(entry, &fx).await;
            let reason = fx.task().await.error_message.unwrap();
            assert!(
                reason.contains("not in the repository"),
                "{entry}: {reason}"
            );
        }
    }

    #[tokio::test]
    async fn unavailable_git_evidence_never_lets_review_go_on_optimistically() {
        for (entry, fx, allowed) in at_both_entry_points(|fx| {
            std::fs::rename(fx.repo.join(".git"), fx.repo.join(".git-away")).unwrap();
        })
        .await
        {
            assert!(!allowed, "{entry}");
            let reason = fx.task().await.error_message;
            assert!(
                reason.is_some_and(|r| !r.contains("rewritten") && !r.contains("moved")),
                "{entry}: an unreadable repository is not reported as moved history"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_review_admitted_but_never_registered_is_not_duplicated_across_restarts() {
        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        let fx = admitted_then_restarted().await;
        let executor = fx.state.executor.get().unwrap().clone();

        // Repeated restart and reconciliation with no capacity: deterministic,
        // and nothing is admitted a second time or launched.
        let held = executor.hold_all_capacity_for_test();
        for _ in 0..3 {
            fx.state
                .task
                .tasks
                .write()
                .await
                .insert(fx.task_id, fx.on_disk());
            executor.poll_once_for_test().await;
            let task = fx.task().await;
            assert_eq!(task.status, TaskStatus::AiReview);
            assert!(task.worker_handoff_confirmed);
            assert_eq!(started(&task), vec![1]);
            assert_eq!(started(&fx.on_disk()), vec![1]);
            assert_eq!(completions(&task), 1);
        }
        assert!(!agent.invocations().contains("claude"));

        // With capacity, exactly one review runs, to one outcome.
        drop(held);
        executor.poll_once_for_test().await;
        settle(&fx).await;
        executor.poll_once_for_test().await;
        settle(&fx).await;
        let task = fx.task().await;
        let outcomes = task
            .activity
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    ActivityKind::AiReviewFailed { .. }
                        | ActivityKind::AiReviewSkipped { .. }
                        | ActivityKind::AiReviewApproved { .. }
                        | ActivityKind::AiReviewChangesRequested { .. }
                )
            })
            .count();
        assert_eq!(outcomes, 1, "one review, one outcome");
        assert_eq!(completions(&task), 1);
        assert!(!task.is_ready_to_execute(), "no coding Worker is launched");
    }

    #[tokio::test]
    async fn refusals_say_what_is_actually_wrong() {
        use crate::queue::executor::{CheckoutRefusal, TaskExecutor, WorkerCommitProof};
        let verify = |fx: &Fx, commit: &str, proof| {
            let (repo, checkout) = (
                fx.repo.to_string_lossy().to_string(),
                fx.checkout.to_string_lossy().to_string(),
            );
            let (branch, commit) = (fx.branch.clone(), commit.to_string());
            async move {
                TaskExecutor::verify_worker_checkout(&repo, &checkout, &branch, &commit, proof)
                    .await
            }
        };
        let exact = WorkerCommitProof::ExactTip;

        let fx = fixture().await;
        assert_eq!(verify(&fx, &fx.tip, exact).await, Ok(()));

        // Advanced, not rewritten: the recorded commit is still in history.
        let advanced = fx.commit_in_checkout("more.txt");
        assert_eq!(
            verify(&fx, &fx.tip, exact).await,
            Err(CheckoutRefusal::BranchAdvanced {
                expected: fx.tip.clone(),
                tip: advanced.clone()
            })
        );
        assert_eq!(
            verify(&fx, &fx.tip, WorkerCommitProof::ReachableFromTip).await,
            Ok(())
        );

        // Dirty is its own fact.
        std::fs::write(fx.checkout.join("scratch"), "x").unwrap();
        assert_eq!(
            verify(&fx, &advanced, exact).await,
            Err(CheckoutRefusal::Dirty)
        );
        assert_eq!(
            verify(&fx, &fx.tip, WorkerCommitProof::ReachableFromTip).await,
            Err(CheckoutRefusal::Dirty)
        );
        std::fs::remove_file(fx.checkout.join("scratch")).unwrap();

        // A commit Git has never heard of.
        let unknown = "e".repeat(40);
        assert_eq!(
            verify(&fx, &unknown, exact).await,
            Err(CheckoutRefusal::CommitMissing {
                commit: unknown.clone()
            })
        );

        // Rewound: the commit exists as an object but left the branch.
        git(&fx.checkout, &["reset", "-q", "--hard", &fx.start]);
        let rewritten = verify(&fx, &fx.tip, exact).await.unwrap_err();
        assert_eq!(
            rewritten,
            CheckoutRefusal::HistoryRewritten {
                expected: fx.tip.clone(),
                tip: fx.start.clone()
            }
        );
        assert!(rewritten.to_string().contains(&fx.tip[..12]));

        // A detached checkout is a registration question, not a moved branch.
        git(&fx.checkout, &["checkout", "-q", "--detach"]);
        let detached = verify(&fx, &fx.start, exact).await.unwrap_err();
        assert_eq!(detached, CheckoutRefusal::RegisteredElsewhere);
        assert!(!detached.to_string().contains("moved"));
        git(&fx.checkout, &["checkout", "-q", &fx.branch]);

        // A repository Git cannot read is not "the branch moved".
        std::fs::rename(fx.repo.join(".git"), fx.repo.join(".git-away")).unwrap();
        let unreadable = verify(&fx, &fx.start, exact).await.unwrap_err();
        assert!(
            matches!(unreadable, CheckoutRefusal::RegistrationUnreadable(_)),
            "{unreadable:?}"
        );
        let text = unreadable.to_string();
        assert!(
            !text.contains("moved") && !text.contains("rewritten"),
            "{text}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_old_decision_id_after_later_decisions_has_no_effect() {
        let fx = fixture().await;
        let agent = crate::test_helpers::FakeProgram::install("claude", "exit 1").await;
        let executor = fx.state.executor.get().unwrap().clone();
        let first = Uuid::new_v4();
        let first_request = fx.request(first);
        let first_tip = fx.tip.clone();
        accept_result(&fx.state, first_request.clone())
            .await
            .unwrap();

        // Two later decisions on new tips supersede the first record.
        let mut observed = first_tip;
        for name in ["second.txt", "third.txt"] {
            let tip = fx.commit_in_checkout(name);
            assert!(!executor.worker_commit_still_verifies(fx.task_id).await);
            let mut request = fx.request(Uuid::new_v4());
            request.observed_commit = Some(observed.clone());
            request.tip = tip.clone();
            let outcome = accept_result(&fx.state, request).await.unwrap();
            assert!(matches!(
                outcome,
                AcceptWorkerResultOutcome::Accepted { .. }
            ));
            observed = tip;
        }

        // While in review, then back in recovery: the old id changes nothing.
        let (task, facts) = (fx.task().await, fx.repo_state());
        let in_review = refusal(accept_result(&fx.state, first_request.clone()).await);
        assert!(!in_review.is_empty());
        assert_untouched(&fx, &task, &facts).await;

        fx.commit_in_checkout("fourth.txt");
        assert!(!executor.worker_commit_still_verifies(fx.task_id).await);
        let (task, facts) = (fx.task().await, fx.repo_state());
        let in_recovery = refusal(accept_result(&fx.state, first_request).await);
        assert!(!in_recovery.is_empty());
        assert_untouched(&fx, &task, &facts).await;
        assert_eq!(completions(&fx.task().await), 1);
        assert!(!agent.invocations().contains("claude"));
    }
}
