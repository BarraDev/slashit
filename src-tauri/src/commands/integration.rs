//! Integrating a Task's approved work into a local branch.
//!
//! This is a delivery action, distinct from completing a Task locally: that
//! one closes the Task and keeps its branch, this one advances a local branch
//! to the work a person approved. It is asked for by a person, never
//! automatic, never pushes, and never deletes or rewrites the Task branch.
//! It adds no Task status and no second lifecycle: the Task stays whatever it
//! was, and the integration is a recorded fact about its approved work
//! (`Task::local_integration`).
//!
//! What is integrated is the exact commit that was approved
//! (`HumanReviewEntry::approved_tip`), not whatever the branch holds later.
//! An approval that recorded no commit has to be reconfirmed against a
//! commit a person was shown ([`reconfirm`]) before anything is integrated.
//!
//! Order of a new integration, so a crash at any point is recoverable from
//! what is on disk: plan (objects only) -> pin the result -> persist
//! `Pending` -> mark the repository -> compare-and-swap the target ->
//! persist `Integrated` -> unpin. The target branch alone says whether the
//! move happened, which is what [`resume`] and [`discard`] read.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::task::{ActivityKind, BranchOrigin};
use crate::domain::{IntegrationState, IntegrationStrategy, LocalIntegration, Task, TaskStatus};
use crate::worktree::integration::{self as git, Observed, Plan, PlanError};
use crate::worktree::restack::is_ancestor;

/// What a person is shown before confirming, and what the confirmation is
/// checked against.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreviewState {
    /// Integrating would advance the target as described.
    Ready,
    /// The target already holds the approved work; confirming records that
    /// and moves nothing.
    AlreadyContained,
    /// The Task's current approved work is in the target.
    Integrated,
    /// An integration was started and not finished.
    Pending,
    /// The approval did not record which commit was approved. A person has
    /// to confirm the commit shown in `reconfirm`.
    NeedsReconfirmation,
    /// It cannot be done now; `reason` says why.
    Refused,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Reconfirm {
    /// The Task branch's tip: the commit that confirming would approve.
    pub branch_tip: String,
    /// The commits on it the target does not have, oldest first.
    pub commits: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IntegrationPreview {
    pub state: PreviewState,
    pub target_branch: Option<String>,
    /// The commit the target names now. Send it back with the confirmation.
    pub target_tip: Option<String>,
    pub approved_tip: Option<String>,
    pub strategy: Option<IntegrationStrategy>,
    /// The commits the target would gain from the approved work, oldest first
    /// (a bounded list of `<short id> <subject>`).
    pub commits: Vec<String>,
    /// Commits on the Task branch after the approved one. They are not
    /// approved and are not integrated.
    pub unapproved_commits: u32,
    pub conflicts: Vec<String>,
    /// The checkout that has the target branch checked out, when that is
    /// what refused it.
    pub checked_out_at: Option<String>,
    pub integration: Option<LocalIntegration>,
    /// For a pending integration: `applied`, `unmoved`, `moved` or
    /// `missing`, as the target branch shows it.
    pub observed: Option<String>,
    pub reconfirm: Option<Reconfirm>,
    pub reason: Option<String>,
}

impl IntegrationPreview {
    fn new(state: PreviewState) -> Self {
        Self {
            state,
            target_branch: None,
            target_tip: None,
            approved_tip: None,
            strategy: None,
            commits: Vec::new(),
            unapproved_commits: 0,
            conflicts: Vec::new(),
            checked_out_at: None,
            integration: None,
            observed: None,
            reconfirm: None,
            reason: None,
        }
    }

    fn refused(reason: impl Into<String>) -> Self {
        Self { reason: Some(reason.into()), ..Self::new(PreviewState::Refused) }
    }
}

/// What a person confirmed: exactly the target and approved commit the
/// preview showed. An integration whose plan no longer matches is refused.
#[derive(Debug, Clone, Deserialize)]
pub struct IntegrationConfirmation {
    pub target_branch: String,
    pub target_tip: String,
    pub approved_tip: String,
}

#[tauri::command]
pub async fn get_integration_preview(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<IntegrationPreview, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    preview(&state, task_id).await
}

#[tauri::command]
pub async fn integrate_task_locally(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    confirmation: IntegrationConfirmation,
) -> Result<Task, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    integrate(&state, task_id, confirmation).await
}

#[tauri::command]
pub async fn reconfirm_approved_tip(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    tip: String,
) -> Result<Task, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    reconfirm(&state, task_id, &tip).await
}

#[tauri::command]
pub async fn resume_local_integration(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Task, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    resume(&state, task_id).await
}

#[tauri::command]
pub async fn discard_local_integration(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Task, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    discard(&state, task_id).await
}

/// The tip of `task`'s branch now, for an approval to record. `None` when
/// the task has no branch or it cannot be read: the approval is then
/// recorded without a commit and has to be reconfirmed before integrating,
/// which is the safe direction.
pub(crate) async fn tip_to_approve(state: &crate::AppState, task: &Task) -> Option<String> {
    let branch = task.branch_name.as_deref()?;
    let repo = repo_path_of(state, task.project_id).await.ok()?;
    git::branch_tip(Path::new(&repo), branch).await.ok().flatten()
}

async fn repo_path_of(state: &crate::AppState, project_id: Uuid) -> Result<String, String> {
    let repository_id = {
        let projects = state.project.projects.read().await;
        projects
            .get(&project_id)
            .ok_or("Project not found")?
            .repository_id
            .ok_or("No repository is linked to this project")?
    };
    let repositories = state.repository.repositories.read().await;
    Ok(repositories.get(&repository_id).ok_or("Repository not found")?.local_path.clone())
}

async fn current(state: &crate::AppState, task_id: Uuid) -> Result<Task, String> {
    state.task.tasks.read().await.get(&task_id).cloned().ok_or_else(|| "Task not found".to_string())
}

/// The lock that serializes integrations into one branch of one project's
/// repository. A task lease is only per Task; two Tasks integrating into the
/// same branch must not interleave their plans. Always taken before a Task's
/// lease, never after.
///
/// The id is derived from the project and branch for this process only (the
/// locks are process memory), by hashing twice with different prefixes to
/// fill the 128 bits.
fn target_lock_id(project_id: Uuid, branch: &str) -> Uuid {
    use std::hash::{Hash, Hasher};
    let half = |salt: u8| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (salt, project_id, branch).hash(&mut hasher);
        u128::from(hasher.finish())
    };
    Uuid::from_u128((half(1) << 64) | half(2))
}

/// Everything the checks below share, resolved once.
struct Prepared {
    task: Task,
    repo_path: String,
    branch: String,
    branch_tip: String,
    target: String,
    target_tip: String,
}

/// Why a Task's approved work cannot be integrated at all, before any
/// question about commits.
async fn eligible(state: &crate::AppState, task: &Task) -> Result<(), String> {
    if task.status != TaskStatus::Done || !task.completed_locally() {
        return Err(
            "Only a task that was completed locally can be integrated from here. A task moved to \
             Done in any other way has no approved work on record."
                .to_string(),
        );
    }
    if !task.human_review.is_approved() {
        return Err("This task has no approval on record, so there is nothing approved to integrate.".to_string());
    }
    if task.pr_url.is_some()
        || task.external_refs.iter().any(|r| r.is_pr())
        || state.task.deliveries.contains(task.id)
    {
        return Err(
            "This task has a pull request, or one is being opened, so it is delivered through \
             that pull request and is not integrated locally."
                .to_string(),
        );
    }
    if let Some(refusal) = task.republish_pending_refusal() {
        return Err(refusal);
    }
    if task.cleanup_in_flight {
        return Err("This task's checkout cleanup was interrupted; resolve that first.".to_string());
    }
    Ok(())
}

async fn prepare(state: &crate::AppState, task_id: Uuid) -> Result<Prepared, String> {
    let task = current(state, task_id).await?;
    eligible(state, &task).await?;
    let repo_path = repo_path_of(state, task.project_id).await?;
    let repo = Path::new(&repo_path);
    if let Some(refused) = crate::worktree::vcs::detect(repo).await?.refusal(&repo_path) {
        return Err(refused);
    }
    let branch = task
        .branch_name
        .clone()
        .ok_or("This task has no branch recorded, so there is no work to integrate.")?;
    let branch_tip = git::branch_tip(repo, &branch).await?.ok_or_else(|| {
        format!(
            "The task's branch {branch} no longer exists, so its approved work cannot be shown to \
             belong to it. Restore the branch to integrate it."
        )
    })?;

    let project_base = state
        .project
        .projects
        .read()
        .await
        .get(&task.project_id)
        .and_then(|p| p.base.as_ref().map(|b| b.branch().to_string()));
    let target = match project_base {
        Some(branch) => branch,
        None => git::remote_default_branch(&repo_path).await?.ok_or(
            "This project has no base branch to integrate into. Choose one in Settings > Repository.",
        )?,
    };
    let target_tip = git::branch_tip(repo, &target)
        .await?
        .ok_or_else(|| format!("The target branch {target} does not exist in this repository."))?;

    match &task.branch_origin {
        Some(BranchOrigin::LocalBase { branch: b }) | Some(BranchOrigin::DefaultBase { branch: Some(b) })
            if *b != target =>
        {
            return Err(format!(
                "This task's branch was started from {b}, but the project's integration target is \
                 {target}. SlashIt will not integrate it into a different branch."
            ));
        }
        Some(BranchOrigin::Stacked { parent_branch }) => {
            let contained = match task.base_commit.as_deref() {
                Some(base) => is_ancestor(repo, base, &target_tip).await.unwrap_or(false),
                None => false,
            };
            if !contained {
                return Err(format!(
                    "This task is stacked on {parent_branch}, whose work is not in {target} yet. \
                     Integrate that work first, so this task's changes are not integrated on top of \
                     work that was never approved for {target}."
                ));
            }
        }
        _ => {}
    }
    Ok(Prepared { task, repo_path, branch, branch_tip, target, target_tip })
}

fn merge_message(branch: &str, target: &str) -> String {
    format!("Merge task branch {branch} into {target}")
}

/// A pending integration's plan, as recorded.
fn plan_of(record: &LocalIntegration) -> Plan {
    Plan {
        target_branch: record.target_branch.clone(),
        target_old: record.target_old_oid.clone(),
        approved_tip: record.approved_tip.clone(),
        result: record.result_oid.clone(),
        strategy: record.strategy,
    }
}

fn observed_name(observed: Observed) -> &'static str {
    match observed {
        Observed::Applied => "applied",
        Observed::Unmoved => "unmoved",
        Observed::Moved => "moved",
        Observed::Missing => "missing",
    }
}

/// What integrating would do now, without changing anything. Writes no ref
/// and no object.
pub(crate) async fn preview(state: &crate::AppState, task_id: Uuid) -> Result<IntegrationPreview, String> {
    let task = current(state, task_id).await?;
    if let Some(record) = task.local_integration.as_ref().filter(|r| r.is_pending()) {
        let repo_path = repo_path_of(state, task.project_id).await?;
        let observed = git::observe(Path::new(&repo_path), &plan_of(record)).await?;
        return Ok(IntegrationPreview {
            target_branch: Some(record.target_branch.clone()),
            approved_tip: Some(record.approved_tip.clone()),
            strategy: Some(record.strategy),
            integration: Some(record.clone()),
            observed: Some(observed_name(observed).to_string()),
            ..IntegrationPreview::new(PreviewState::Pending)
        });
    }
    let p = match prepare(state, task_id).await {
        Ok(p) => p,
        Err(reason) if reason == "Task not found" => return Err(reason),
        Err(reason) => return Ok(IntegrationPreview::refused(reason)),
    };
    let repo = Path::new(&p.repo_path);
    let mut out = IntegrationPreview {
        target_branch: Some(p.target.clone()),
        target_tip: Some(p.target_tip.clone()),
        integration: p.task.local_integration.clone(),
        ..IntegrationPreview::new(PreviewState::Refused)
    };

    let Some(approved) = p.task.human_review.approved_tip().map(str::to_string) else {
        out.state = PreviewState::NeedsReconfirmation;
        out.reconfirm = Some(Reconfirm {
            branch_tip: p.branch_tip.clone(),
            commits: git::subjects(repo, &p.target_tip, &p.branch_tip, 50).await.unwrap_or_default(),
        });
        out.reason = Some(
            "This approval did not record which commit was approved. Confirm the commit shown \
             before it can be integrated."
                .to_string(),
        );
        return Ok(out);
    };
    out.approved_tip = Some(approved.clone());
    if !is_ancestor(repo, &approved, &p.branch_tip).await? {
        out.reason = Some(format!(
            "The approved commit {} is no longer on the task's branch {}, so the branch was \
             rewritten after the approval. Nothing will be integrated under that approval.",
            git::short(&approved),
            p.branch
        ));
        return Ok(out);
    }
    out.unapproved_commits = git::count_between(repo, &approved, &p.branch_tip).await?;

    if let Some(record) = &p.task.local_integration {
        if record.state == IntegrationState::Integrated
            && record.approved_tip == approved
            && is_ancestor(repo, &approved, &p.target_tip).await?
        {
            out.state = PreviewState::Integrated;
            return Ok(out);
        }
    }

    let plan = match git::plan(repo, &p.target, &approved, &merge_message(&p.branch, &p.target), false).await {
        Ok(plan) => plan,
        Err(PlanError::Conflicts(paths)) => {
            out.conflicts = paths;
            out.reason = Some(PlanError::Conflicts(out.conflicts.clone()).to_string());
            return Ok(out);
        }
        Err(PlanError::Refused(why)) => {
            out.reason = Some(why);
            return Ok(out);
        }
    };
    out.strategy = Some(plan.strategy);
    out.commits = git::subjects(repo, &p.target_tip, &approved, 50).await.unwrap_or_default();

    if let Some(why) = git::not_behind_origin(repo, &p.target).await? {
        out.reason = Some(why);
        return Ok(out);
    }
    if plan.strategy != IntegrationStrategy::AlreadyContained {
        if let Some(path) = git::checked_out_at(&p.repo_path, &p.target).await? {
            out.reason = Some(git::checked_out_message(&p.target, &path, git::short(&approved)));
            out.checked_out_at = Some(path);
            return Ok(out);
        }
    }
    if let Some(other) = other_pending_on(state, &p.task, &p.target).await {
        out.reason = Some(other);
        return Ok(out);
    }
    out.state = if plan.strategy == IntegrationStrategy::AlreadyContained {
        PreviewState::AlreadyContained
    } else {
        PreviewState::Ready
    };
    Ok(out)
}

/// A refusal naming another Task whose integration into `target` is still
/// pending, which must be resumed or discarded before another is planned
/// against the same branch.
async fn other_pending_on(state: &crate::AppState, task: &Task, target: &str) -> Option<String> {
    let tasks = state.task.tasks.read().await;
    tasks
        .values()
        .find(|t| {
            t.id != task.id
                && t.project_id == task.project_id
                && t.local_integration.as_ref().is_some_and(|r| r.is_pending() && r.target_branch == target)
        })
        .map(|t| {
            format!(
                "The task \"{}\" has an integration into {target} that was started and not \
                 finished. Resume or discard that one first.",
                t.title
            )
        })
}

/// Integrate the Task's approved work into the target branch the preview
/// showed, if nothing it showed has changed.
pub(crate) async fn integrate(
    state: &crate::AppState,
    task_id: Uuid,
    confirmed: IntegrationConfirmation,
) -> Result<Task, String> {
    let project_id = current(state, task_id).await?.project_id;
    let _target = state.task_lifecycle_locks.acquire(target_lock_id(project_id, &confirmed.target_branch)).await?;
    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    let task = current(state, task_id).await?;
    if task.local_integration.as_ref().is_some_and(|r| r.is_pending()) {
        return resume_locked(state, task_id).await;
    }
    let p = prepare(state, task_id).await?;
    if p.target != confirmed.target_branch {
        return Err(format!(
            "The integration target is {} now, not {}, so nothing was changed. Review it again.",
            p.target, confirmed.target_branch
        ));
    }
    let repo = Path::new(&p.repo_path);
    let approved = p.task.human_review.approved_tip().map(str::to_string).ok_or(
        "This approval did not record which commit was approved. Confirm the commit first.",
    )?;
    if approved != confirmed.approved_tip {
        return Err("The approved commit is not the one that was shown, so nothing was changed. Review it again.".to_string());
    }
    if !is_ancestor(repo, &approved, &p.branch_tip).await? {
        return Err(format!(
            "The approved commit {} is no longer on the task's branch {}; nothing will be \
             integrated under that approval.",
            git::short(&approved),
            p.branch
        ));
    }
    if let Some(record) = &p.task.local_integration {
        if record.state == IntegrationState::Integrated
            && record.approved_tip == approved
            && is_ancestor(repo, &approved, &p.target_tip).await?
        {
            return Ok(p.task);
        }
    }
    if let Some(why) = other_pending_on(state, &p.task, &p.target).await {
        return Err(why);
    }
    if let Some(why) = git::not_behind_origin(repo, &p.target).await? {
        return Err(why);
    }
    sweep_orphan_pins(state, repo).await;

    let plan = git::plan(repo, &p.target, &approved, &merge_message(&p.branch, &p.target), true)
        .await
        .map_err(|e| e.to_string())?;
    if plan.target_old != confirmed.target_tip {
        return Err(format!(
            "{} moved after the integration was shown, so nothing was changed. Review it again.",
            p.target
        ));
    }
    if plan.strategy != IntegrationStrategy::AlreadyContained {
        if let Some(path) = git::checked_out_at(&p.repo_path, &p.target).await? {
            return Err(git::checked_out_message(&p.target, &path, &plan.result));
        }
        git::pin(repo, task_id, &plan.result).await?;
    }

    let record = LocalIntegration {
        state: IntegrationState::Pending,
        target_branch: plan.target_branch.clone(),
        target_old_oid: plan.target_old.clone(),
        approved_tip: plan.approved_tip.clone(),
        result_oid: plan.result.clone(),
        strategy: plan.strategy,
        started_at: chrono::Utc::now(),
        completed_at: None,
    };
    let stored = record.clone();
    let amend = move |staged: &mut HashMap<Uuid, Task>| {
        if let Some(t) = staged.get_mut(&task_id) {
            if t.status == TaskStatus::Done && t.local_integration.as_ref().is_none_or(|r| !r.is_pending()) {
                t.local_integration = Some(stored.clone());
            }
        }
    };
    let saved = crate::lifecycle::commit_task(&state.task.tasks, &state.storage, task_id, &amend)
        .await
        .map_err(|e| format!("The integration was not started, because recording it failed: {e}"))?
        .ok_or("Task not found")?;
    if saved.local_integration.as_ref() != Some(&record) {
        let _ = git::unpin(repo, task_id).await;
        return Err("The task changed while the integration was being started, so nothing was changed.".to_string());
    }
    apply(state, &p.repo_path, task_id, &plan).await
}

/// Move the target (if it has to move), then record the result.
async fn apply(state: &crate::AppState, repo_path: &str, task_id: Uuid, plan: &Plan) -> Result<Task, String> {
    let repo = Path::new(repo_path);
    if plan.strategy != IntegrationStrategy::AlreadyContained {
        git::mark_base(repo, &plan.target_branch, &plan.target_old).await?;
    }
    if let Err(why) = git::advance(repo_path, plan).await {
        return match git::observe(repo, plan).await? {
            // It moved anyway (another process, or a retry of this one that
            // got further than it reported): record what is true.
            Observed::Applied => finish(state, repo, task_id, plan).await,
            Observed::Unmoved => Err(format!(
                "{why} The integration is still recorded as pending: ask again to resume it, or \
                 discard it."
            )),
            Observed::Moved | Observed::Missing => {
                clear_pending(state, repo, task_id, plan).await?;
                Err(format!(
                    "{why} {} is not where the integration was planned from, so the plan was \
                     dropped and nothing was changed. Review it again.",
                    plan.target_branch
                ))
            }
        };
    }
    finish(state, repo, task_id, plan).await
}

/// Record that the target holds the result, and release the pin.
async fn finish(state: &crate::AppState, repo: &Path, task_id: Uuid, plan: &Plan) -> Result<Task, String> {
    let now = chrono::Utc::now();
    let (target, strategy, commit, expected) =
        (plan.target_branch.clone(), plan.strategy, plan.result.clone(), plan.result.clone());
    let amend = move |staged: &mut HashMap<Uuid, Task>| {
        if let Some(t) = staged.get_mut(&task_id) {
            if let Some(record) = t.local_integration.as_mut().filter(|r| r.is_pending() && r.result_oid == expected) {
                record.state = IntegrationState::Integrated;
                record.completed_at = Some(now);
                t.record_activity(ActivityKind::IntegratedLocally {
                    target: target.clone(),
                    strategy: strategy.as_str().to_string(),
                    commit: commit.clone(),
                });
            }
        }
    };
    let saved = crate::lifecycle::commit_task(&state.task.tasks, &state.storage, task_id, &amend)
        .await
        .map_err(|e| {
            format!(
                "{} holds the integrated work, but recording that failed: {e}. Ask again to resume \
                 and finish recording it.",
                plan.target_branch
            )
        })?
        .ok_or("Task not found")?;
    // A pin that cannot be removed is swept later; it holds no ref anyone reads.
    let _ = git::unpin(repo, task_id).await;
    Ok(saved)
}

async fn clear_pending(state: &crate::AppState, repo: &Path, task_id: Uuid, plan: &Plan) -> Result<(), String> {
    let result = plan.result.clone();
    let amend = move |staged: &mut HashMap<Uuid, Task>| {
        if let Some(t) = staged.get_mut(&task_id) {
            if t.local_integration.as_ref().is_some_and(|r| r.is_pending() && r.result_oid == result) {
                t.local_integration = None;
            }
        }
    };
    crate::lifecycle::commit_task(&state.task.tasks, &state.storage, task_id, &amend)
        .await
        .map_err(|e| format!("The pending integration could not be cleared: {e}"))?;
    let _ = git::unpin(repo, task_id).await;
    Ok(())
}

/// Finish an integration that was started and not finished.
pub(crate) async fn resume(state: &crate::AppState, task_id: Uuid) -> Result<Task, String> {
    let task = current(state, task_id).await?;
    let record = task
        .local_integration
        .as_ref()
        .filter(|r| r.is_pending())
        .ok_or("This task has no pending integration.")?;
    let _target = state.task_lifecycle_locks.acquire(target_lock_id(task.project_id, &record.target_branch)).await?;
    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;
    resume_locked(state, task_id).await
}

async fn resume_locked(state: &crate::AppState, task_id: Uuid) -> Result<Task, String> {
    let task = current(state, task_id).await?;
    let record = task
        .local_integration
        .clone()
        .filter(|r| r.is_pending())
        .ok_or("This task has no pending integration.")?;
    let repo_path = repo_path_of(state, task.project_id).await?;
    let repo = Path::new(&repo_path);
    let plan = plan_of(&record);
    match git::observe(repo, &plan).await? {
        Observed::Applied => finish(state, repo, task_id, &plan).await,
        Observed::Moved | Observed::Missing => {
            clear_pending(state, repo, task_id, &plan).await?;
            Err(format!(
                "{} is not where the integration was planned from, so the plan was dropped and \
                 nothing was changed. Review it again.",
                record.target_branch
            ))
        }
        Observed::Unmoved => {
            // Re-prove everything that justified the plan before moving
            // anything: the Task, its approval and branch, and the target.
            let p = prepare(state, task_id).await?;
            if p.target != record.target_branch
                || p.task.human_review.approved_tip() != Some(record.approved_tip.as_str())
                || !is_ancestor(repo, &record.approved_tip, &p.branch_tip).await?
            {
                return Err(
                    "The task, its approval or its branch is no longer what this integration was \
                     planned for. Discard it and review again."
                        .to_string(),
                );
            }
            if plan.strategy != IntegrationStrategy::AlreadyContained {
                if !crate::worktree::restack::has_commit(repo, &plan.result).await? {
                    return Err(
                        "The commit this integration planned is gone. Discard it and review again.".to_string()
                    );
                }
                git::pin(repo, task_id, &plan.result).await?;
            }
            if let Some(why) = git::not_behind_origin(repo, &p.target).await? {
                return Err(why);
            }
            apply(state, &repo_path, task_id, &plan).await
        }
    }
}

/// Drop an integration that was started and has not taken effect. Refused
/// once the target holds the work: resume it instead, so that what is true
/// gets recorded.
pub(crate) async fn discard(state: &crate::AppState, task_id: Uuid) -> Result<Task, String> {
    let task = current(state, task_id).await?;
    let record = task
        .local_integration
        .as_ref()
        .filter(|r| r.is_pending())
        .ok_or("This task has no pending integration.")?;
    let _target = state.task_lifecycle_locks.acquire(target_lock_id(task.project_id, &record.target_branch)).await?;
    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    let task = current(state, task_id).await?;
    let record = task
        .local_integration
        .clone()
        .filter(|r| r.is_pending())
        .ok_or("This task has no pending integration.")?;
    let repo_path = repo_path_of(state, task.project_id).await?;
    let repo = Path::new(&repo_path);
    let plan = plan_of(&record);
    if git::observe(repo, &plan).await? == Observed::Applied {
        return Err(format!(
            "{} already holds this work, so it cannot be discarded. Resume it to record that.",
            record.target_branch
        ));
    }
    clear_pending(state, repo, task_id, &plan).await?;
    current(state, task_id).await
}

/// Record the commit a person was shown as the approved one, for an
/// approval that did not record it. The branch must still be at exactly that
/// commit.
pub(crate) async fn reconfirm(state: &crate::AppState, task_id: Uuid, tip: &str) -> Result<Task, String> {
    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;
    let task = current(state, task_id).await?;
    eligible(state, &task).await?;
    if task.human_review.approved_tip().is_some() {
        return Err("This approval already records the commit that was approved.".to_string());
    }
    let repo_path = repo_path_of(state, task.project_id).await?;
    let branch = task.branch_name.as_deref().ok_or("This task has no branch recorded.")?;
    let branch_tip = git::branch_tip(Path::new(&repo_path), branch).await?;
    if branch_tip.as_deref() != Some(tip) {
        return Err(
            "The task's branch is not at the commit that was shown, so nothing was confirmed. \
             Review it again."
                .to_string(),
        );
    }
    let tip = tip.to_string();
    let amend = move |staged: &mut HashMap<Uuid, Task>| {
        if let Some(t) = staged.get_mut(&task_id) {
            t.human_review.reconfirm_tip(&tip);
        }
    };
    let saved = crate::lifecycle::commit_task(&state.task.tasks, &state.storage, task_id, &amend)
        .await
        .map_err(|e| format!("The confirmation was not saved: {e}"))?
        .ok_or("Task not found")?;
    if saved.human_review.approved_tip().is_none() {
        return Err("The task changed while it was being confirmed, so nothing was recorded.".to_string());
    }
    Ok(saved)
}

/// Remove integration pins that belong to no pending integration. A pin
/// without a pending record means the target never moved (the record is
/// written before the move), so removing it loses nothing. A Task whose
/// lease is held is left alone: it may be between pinning and recording.
async fn sweep_orphan_pins(state: &crate::AppState, repo: &Path) {
    let Ok(pinned) = git::pinned(repo).await else { return };
    for id in pinned {
        let Ok(task_id) = Uuid::parse_str(&id) else { continue };
        let pending = state
            .task
            .tasks
            .read()
            .await
            .get(&task_id)
            .is_some_and(|t| t.local_integration.as_ref().is_some_and(|r| r.is_pending()));
        if pending {
            continue;
        }
        let Some(_lease) = state.task_lifecycle_locks.try_acquire(task_id).await else { continue };
        let _ = git::unpin(repo, &id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::create_test_task_full;
    use std::path::PathBuf;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn ref_exists(repo: &Path, name: &str) -> bool {
        std::process::Command::new("git")
            .args(["rev-parse", "--verify", "-q", name])
            .current_dir(repo)
            .output()
            .unwrap()
            .status
            .success()
    }

    /// Commit `file` on `branch`, leaving the primary checkout detached so no
    /// branch is checked out unless a test asks for it.
    fn commit_on(repo: &Path, branch: &str, file: &str, content: &str) -> String {
        git(repo, &["checkout", "-q", branch]);
        std::fs::write(repo.join(file), content).unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", &format!("{branch}: {file}")]);
        let tip = git(repo, &["rev-parse", "HEAD"]);
        git(repo, &["checkout", "-q", "--detach"]);
        tip
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        state: crate::AppState,
        repo: PathBuf,
        project_id: Uuid,
        task_id: Uuid,
        branch: String,
        tip: String,
    }

    async fn state() -> (tempfile::TempDir, crate::AppState) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let (state, _) = crate::app_core::build_state_with_paths(paths).await.expect("state");
        (tmp, state)
    }

    /// A no-remote repository on `main`, one task branch with one commit, and
    /// that task Done, completed locally, with its approval recording the
    /// branch tip. The primary checkout is detached, so `main` is free to move.
    async fn fixture() -> Fx {
        let (tmp, state) = state().await;
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
        state.project.projects.write().await.insert(
            project_id,
            crate::domain::Project {
                id: project_id,
                name: "no remote".to_string(),
                repository_id: Some(repository_id),
                scope: crate::domain::project::ProjectScope::Standalone,
                state_location: crate::config::paths::StateLocation::External,
                base: Some(crate::domain::project::ProjectBase::LocalBranch { branch: "main".to_string() }),
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
        git(&repo, &["branch", "task-a"]);
        let tip = commit_on(&repo, "task-a", "a.txt", "a\n");
        let task_id = seed(&state, project_id, "task-a", Some(&tip)).await;
        Fx { _tmp: tmp, state, repo, project_id, task_id, branch: "task-a".to_string(), tip }
    }

    /// A Done task, completed locally, approved at `tip` (or with an approval
    /// that recorded none).
    async fn seed(state: &crate::AppState, project_id: Uuid, branch: &str, tip: Option<&str>) -> Uuid {
        let mut task = create_test_task_full(branch, project_id, TaskStatus::Done, 0);
        task.branch_name = Some(branch.to_string());
        task.human_review.record_arrival();
        task.human_review.approve(tip.map(str::to_string), chrono::Utc::now());
        task.record_activity(ActivityKind::CompletedLocally);
        let id = task.id;
        state.task.tasks.write().await.insert(id, task);
        let all: Vec<Task> =
            state.task.tasks.read().await.values().filter(|t| t.project_id == project_id).cloned().collect();
        state.storage.save_project_tasks(project_id, &all).expect("seed the board");
        id
    }

    fn on_disk(fx: &Fx, id: Uuid) -> Task {
        fx.state
            .storage
            .load_project_tasks(fx.project_id)
            .unwrap()
            .into_iter()
            .find(|t| t.id == id)
            .expect("on disk")
    }

    fn confirmation(preview: &IntegrationPreview) -> IntegrationConfirmation {
        IntegrationConfirmation {
            target_branch: preview.target_branch.clone().expect("a target"),
            target_tip: preview.target_tip.clone().expect("a target tip"),
            approved_tip: preview.approved_tip.clone().expect("an approved tip"),
        }
    }

    async fn ready(fx: &Fx, id: Uuid) -> IntegrationConfirmation {
        let p = preview(&fx.state, id).await.unwrap();
        assert!(
            matches!(p.state, PreviewState::Ready | PreviewState::AlreadyContained),
            "{:?}: {:?}",
            p.state,
            p.reason
        );
        confirmation(&p)
    }

    fn tip_of(repo: &Path, branch: &str) -> String {
        git(repo, &["rev-parse", &format!("refs/heads/{branch}")])
    }

    #[tokio::test]
    async fn an_approved_task_is_fast_forwarded_into_the_target_and_its_branch_is_kept() {
        let fx = fixture().await;
        let before = tip_of(&fx.repo, "main");
        let p = preview(&fx.state, fx.task_id).await.unwrap();
        assert_eq!(p.state, PreviewState::Ready);
        assert_eq!(p.strategy, Some(IntegrationStrategy::FastForward));
        assert_eq!(p.commits.len(), 1);
        assert_eq!(tip_of(&fx.repo, "main"), before, "a preview moves nothing");
        assert!(!ref_exists(&fx.repo, &git::pin_ref(fx.task_id)), "a preview leaves no pin");

        let done = integrate(&fx.state, fx.task_id, confirmation(&p)).await.unwrap();
        assert_eq!(tip_of(&fx.repo, "main"), fx.tip);
        assert_eq!(tip_of(&fx.repo, &fx.branch), fx.tip, "the Task branch is untouched");
        let record = done.local_integration.clone().expect("a record");
        assert_eq!(record.state, IntegrationState::Integrated);
        assert_eq!(record.strategy, IntegrationStrategy::FastForward);
        assert_eq!(record.result_oid, fx.tip);
        assert!(done.activity.iter().any(|e| matches!(
            &e.kind,
            ActivityKind::IntegratedLocally { target, strategy, .. } if target == "main" && strategy == "fast_forward"
        )));
        assert_eq!(done.status, TaskStatus::Done);
        assert!(done.completed_locally());
        assert!(!ref_exists(&fx.repo, &git::pin_ref(fx.task_id)), "the pin is released");
        assert!(ref_exists(&fx.repo, &git::marker_ref("main")), "the repository records that main is integrated into");
        assert_eq!(on_disk(&fx, fx.task_id).local_integration.unwrap().state, IntegrationState::Integrated);

        // Asked again, it answers and does nothing.
        let again = integrate(&fx.state, fx.task_id, confirmation(&p)).await;
        assert_eq!(preview(&fx.state, fx.task_id).await.unwrap().state, PreviewState::Integrated);
        assert!(again.is_ok());
        assert_eq!(tip_of(&fx.repo, "main"), fx.tip);
    }

    #[tokio::test]
    async fn a_diverged_task_is_merged_with_a_merge_commit_that_keeps_both_lines() {
        let fx = fixture().await;
        let other = commit_on(&fx.repo, "main", "other.txt", "other\n");
        let p = preview(&fx.state, fx.task_id).await.unwrap();
        assert_eq!(p.strategy, Some(IntegrationStrategy::MergeCommit));
        assert_eq!(p.state, PreviewState::Ready);

        let done = integrate(&fx.state, fx.task_id, confirmation(&p)).await.unwrap();
        let result = tip_of(&fx.repo, "main");
        assert_eq!(done.local_integration.unwrap().result_oid, result);
        let parents = git(&fx.repo, &["rev-list", "--parents", "-n1", &result]);
        assert_eq!(parents.split_whitespace().skip(1).collect::<Vec<_>>(), vec![other.as_str(), fx.tip.as_str()]);
        assert_eq!(git(&fx.repo, &["show", &format!("{result}:a.txt")]), "a");
        assert_eq!(git(&fx.repo, &["show", &format!("{result}:other.txt")]), "other");
        assert_eq!(tip_of(&fx.repo, &fx.branch), fx.tip, "the Task branch is not rewritten");
    }

    #[tokio::test]
    async fn a_conflict_is_found_before_anything_changes() {
        let fx = fixture().await;
        git(&fx.repo, &["branch", "task-b", "main"]);
        let b_tip = commit_on(&fx.repo, "task-b", "a.txt", "b version\n");
        let b = seed(&fx.state, fx.project_id, "task-b", Some(&b_tip)).await;
        let p = preview(&fx.state, fx.task_id).await.unwrap();
        integrate(&fx.state, fx.task_id, confirmation(&p)).await.unwrap();
        let main_after_a = tip_of(&fx.repo, "main");

        let refused = preview(&fx.state, b).await.unwrap();
        assert_eq!(refused.state, PreviewState::Refused);
        assert_eq!(refused.conflicts, vec!["a.txt".to_string()]);
        let err = integrate(
            &fx.state,
            b,
            IntegrationConfirmation {
                target_branch: "main".into(),
                target_tip: main_after_a.clone(),
                approved_tip: b_tip.clone(),
            },
        )
        .await
        .unwrap_err();
        assert!(err.contains("a.txt") && err.contains("Nothing was changed"), "{err}");
        assert_eq!(tip_of(&fx.repo, "main"), main_after_a);
        assert!(on_disk(&fx, b).local_integration.is_none());
        assert!(!ref_exists(&fx.repo, &git::pin_ref(b)));
    }

    #[tokio::test]
    async fn a_checked_out_target_is_refused_with_the_command_to_run_and_nothing_is_changed() {
        let fx = fixture().await;
        git(&fx.repo, &["checkout", "-q", "main"]);
        let before = tip_of(&fx.repo, "main");
        let p = preview(&fx.state, fx.task_id).await.unwrap();
        assert_eq!(p.state, PreviewState::Refused);
        assert!(p.checked_out_at.is_some());
        assert!(p.reason.as_deref().unwrap().contains("merge --ff-only"));

        let err = integrate(
            &fx.state,
            fx.task_id,
            IntegrationConfirmation {
                target_branch: "main".into(),
                target_tip: before.clone(),
                approved_tip: fx.tip.clone(),
            },
        )
        .await
        .unwrap_err();
        assert!(err.contains("checked out"), "{err}");
        assert_eq!(tip_of(&fx.repo, "main"), before);
        assert_eq!(git(&fx.repo, &["status", "--porcelain"]), "", "the checkout is untouched");
        assert!(on_disk(&fx, fx.task_id).local_integration.is_none());
        assert!(!ref_exists(&fx.repo, &git::pin_ref(fx.task_id)));
    }

    #[tokio::test]
    async fn only_the_approved_commit_is_integrated_never_a_later_one() {
        let fx = fixture().await;
        let later = commit_on(&fx.repo, "task-a", "later.txt", "later\n");
        let p = preview(&fx.state, fx.task_id).await.unwrap();
        assert_eq!(p.approved_tip.as_deref(), Some(fx.tip.as_str()));
        assert_eq!(p.unapproved_commits, 1);

        integrate(&fx.state, fx.task_id, confirmation(&p)).await.unwrap();
        assert_eq!(tip_of(&fx.repo, "main"), fx.tip, "the commit added after approval stays out");
        assert_ne!(tip_of(&fx.repo, "main"), later);

        // The earlier integration says nothing about the newer commit.
        let after = preview(&fx.state, fx.task_id).await.unwrap();
        assert_eq!(after.state, PreviewState::Integrated);
        assert_eq!(after.unapproved_commits, 1, "the new commit is reported as neither approved nor integrated");
    }

    #[tokio::test]
    async fn a_branch_rewritten_after_approval_is_never_integrated_under_that_approval() {
        let fx = fixture().await;
        git(&fx.repo, &["checkout", "-q", "task-a"]);
        git(&fx.repo, &["reset", "-q", "--hard", "main"]);
        std::fs::write(fx.repo.join("rewritten.txt"), "x\n").unwrap();
        git(&fx.repo, &["add", "."]);
        git(&fx.repo, &["commit", "-q", "-m", "rewritten"]);
        git(&fx.repo, &["checkout", "-q", "--detach"]);
        let p = preview(&fx.state, fx.task_id).await.unwrap();
        assert_eq!(p.state, PreviewState::Refused);
        assert!(p.reason.unwrap().contains("rewritten"));
        let before = tip_of(&fx.repo, "main");
        assert!(integrate(
            &fx.state,
            fx.task_id,
            IntegrationConfirmation { target_branch: "main".into(), target_tip: before.clone(), approved_tip: fx.tip.clone() }
        )
        .await
        .is_err());
        assert_eq!(tip_of(&fx.repo, "main"), before);
    }

    #[tokio::test]
    async fn an_approval_without_a_recorded_commit_must_be_reconfirmed_against_the_shown_commit() {
        let fx = fixture().await;
        let legacy = seed(&fx.state, fx.project_id, "task-a", None).await;
        let p = preview(&fx.state, legacy).await.unwrap();
        assert_eq!(p.state, PreviewState::NeedsReconfirmation);
        let shown = p.reconfirm.expect("what to confirm");
        assert_eq!(shown.branch_tip, fx.tip);
        assert_eq!(shown.commits.len(), 1);

        let before = tip_of(&fx.repo, "main");
        assert!(integrate(
            &fx.state,
            legacy,
            IntegrationConfirmation { target_branch: "main".into(), target_tip: before.clone(), approved_tip: fx.tip.clone() }
        )
        .await
        .is_err());
        assert_eq!(tip_of(&fx.repo, "main"), before, "nothing is trusted silently");

        // The branch moved on after it was shown: the confirmation is refused.
        let moved = commit_on(&fx.repo, "task-a", "more.txt", "m\n");
        let err = reconfirm(&fx.state, legacy, &shown.branch_tip).await.unwrap_err();
        assert!(err.contains("not at the commit that was shown"), "{err}");
        assert!(on_disk(&fx, legacy).human_review.approved_tip().is_none());

        let confirmed = reconfirm(&fx.state, legacy, &moved).await.unwrap();
        assert_eq!(confirmed.human_review.approved_tip(), Some(moved.as_str()));
        assert!(confirmed.human_review.entries.last().unwrap().tip_reconfirmed);
        let again = reconfirm(&fx.state, legacy, &moved).await.unwrap_err();
        assert!(again.contains("already records"), "{again}");

        let p = preview(&fx.state, legacy).await.unwrap();
        assert_eq!(p.state, PreviewState::Ready);
        integrate(&fx.state, legacy, confirmation(&p)).await.unwrap();
        assert_eq!(tip_of(&fx.repo, "main"), moved);
    }

    #[tokio::test]
    async fn a_confirmation_for_something_other_than_what_was_shown_changes_nothing() {
        let fx = fixture().await;
        let c = ready(&fx, fx.task_id).await;
        let before = tip_of(&fx.repo, "main");
        let moved = commit_on(&fx.repo, "main", "late.txt", "l\n");

        let err = integrate(&fx.state, fx.task_id, c.clone()).await.unwrap_err();
        assert!(err.contains("moved after the integration was shown"), "{err}");
        assert_eq!(tip_of(&fx.repo, "main"), moved);
        assert_ne!(before, moved);
        assert!(on_disk(&fx, fx.task_id).local_integration.is_none());

        let wrong_tip = IntegrationConfirmation { approved_tip: "0".repeat(40), target_tip: moved.clone(), ..c.clone() };
        assert!(integrate(&fx.state, fx.task_id, wrong_tip).await.unwrap_err().contains("not the one that was shown"));
        let wrong_branch = IntegrationConfirmation { target_branch: "other".into(), target_tip: moved, ..c };
        assert!(integrate(&fx.state, fx.task_id, wrong_branch).await.is_err());
        assert!(!ref_exists(&fx.repo, &git::pin_ref(fx.task_id)));
    }

    #[tokio::test]
    async fn work_already_in_the_target_is_recognised_without_moving_anything() {
        let fx = fixture().await;
        git(&fx.repo, &["branch", "-f", "main", &fx.tip]);
        let p = preview(&fx.state, fx.task_id).await.unwrap();
        assert_eq!(p.state, PreviewState::AlreadyContained);
        let done = integrate(&fx.state, fx.task_id, confirmation(&p)).await.unwrap();
        let record = done.local_integration.unwrap();
        assert_eq!(record.strategy, IntegrationStrategy::AlreadyContained);
        assert_eq!(tip_of(&fx.repo, "main"), fx.tip);
        assert!(!ref_exists(&fx.repo, &git::marker_ref("main")), "nothing was integrated by SlashIt");
    }

    #[tokio::test]
    async fn ineligible_tasks_are_refused_and_left_alone() {
        let fx = fixture().await;
        let before = tip_of(&fx.repo, "main");
        let confirm = IntegrationConfirmation {
            target_branch: "main".into(),
            target_tip: before.clone(),
            approved_tip: fx.tip.clone(),
        };

        // Moved to Done some other way: no completion milestone.
        let amend = move |s: &mut HashMap<Uuid, Task>| {
            let t = s.get_mut(&fx.task_id).unwrap();
            t.activity.clear();
        };
        crate::lifecycle::record(&fx.state.task.tasks, &fx.state.storage, fx.task_id, &amend).await.unwrap();
        let err = integrate(&fx.state, fx.task_id, confirm.clone()).await.unwrap_err();
        assert!(err.contains("completed locally"), "{err}");

        // A pull request.
        let with_pr = seed(&fx.state, fx.project_id, "task-a", Some(&fx.tip)).await;
        let amend = move |s: &mut HashMap<Uuid, Task>| {
            s.get_mut(&with_pr).unwrap().pr_url = Some("https://example.invalid/pr/1".into());
        };
        crate::lifecycle::record(&fx.state.task.tasks, &fx.state.storage, with_pr, &amend).await.unwrap();
        assert!(integrate(&fx.state, with_pr, confirm.clone()).await.unwrap_err().contains("pull request"));

        // A branch that is gone.
        let gone = seed(&fx.state, fx.project_id, "task-gone", Some(&fx.tip)).await;
        let err = integrate(&fx.state, gone, confirm).await.unwrap_err();
        assert!(err.contains("no longer exists"), "{err}");
        assert_eq!(tip_of(&fx.repo, "main"), before);
    }

    #[tokio::test]
    async fn a_stacked_task_waits_for_the_work_it_is_stacked_on() {
        let fx = fixture().await;
        git(&fx.repo, &["branch", "task-child", "task-a"]);
        let child_tip = commit_on(&fx.repo, "task-child", "child.txt", "c\n");
        let child = seed(&fx.state, fx.project_id, "task-child", Some(&child_tip)).await;
        let base = fx.tip.clone();
        let amend = {
            let base = base.clone();
            move |s: &mut HashMap<Uuid, Task>| {
                let t = s.get_mut(&child).unwrap();
                t.branch_origin = Some(BranchOrigin::Stacked { parent_branch: "task-a".into() });
                t.base_commit = Some(base.clone());
            }
        };
        crate::lifecycle::record(&fx.state.task.tasks, &fx.state.storage, child, &amend).await.unwrap();

        let p = preview(&fx.state, child).await.unwrap();
        assert_eq!(p.state, PreviewState::Refused);
        assert!(p.reason.unwrap().contains("Integrate that work first"));

        let parent = preview(&fx.state, fx.task_id).await.unwrap();
        integrate(&fx.state, fx.task_id, confirmation(&parent)).await.unwrap();
        let p = preview(&fx.state, child).await.unwrap();
        assert_eq!(p.state, PreviewState::Ready);
        assert_eq!(p.strategy, Some(IntegrationStrategy::FastForward), "ancestry was preserved, no rewrite");
        integrate(&fx.state, child, confirmation(&p)).await.unwrap();
        assert_eq!(tip_of(&fx.repo, "main"), child_tip);
    }

    // ---- Recovery ----

    /// A planned integration whose record is on disk, with the target not yet
    /// moved: the state a crash between persisting `Pending` and moving the
    /// branch leaves.
    async fn pending_unmoved(fx: &Fx) -> Plan {
        let c = ready(fx, fx.task_id).await;
        let plan = git::plan(&fx.repo, &c.target_branch, &c.approved_tip, "m", true).await.unwrap();
        git::pin(&fx.repo, fx.task_id, &plan.result).await.unwrap();
        let record = LocalIntegration {
            state: IntegrationState::Pending,
            target_branch: plan.target_branch.clone(),
            target_old_oid: plan.target_old.clone(),
            approved_tip: plan.approved_tip.clone(),
            result_oid: plan.result.clone(),
            strategy: plan.strategy,
            started_at: chrono::Utc::now(),
            completed_at: None,
        };
        let id = fx.task_id;
        let amend = move |s: &mut HashMap<Uuid, Task>| s.get_mut(&id).unwrap().local_integration = Some(record.clone());
        crate::lifecycle::record(&fx.state.task.tasks, &fx.state.storage, id, &amend).await.unwrap();
        plan
    }

    #[tokio::test]
    async fn a_crash_before_the_branch_moved_is_finished_by_resuming() {
        let fx = fixture().await;
        let plan = pending_unmoved(&fx).await;
        assert_eq!(tip_of(&fx.repo, "main"), plan.target_old);
        let p = preview(&fx.state, fx.task_id).await.unwrap();
        assert_eq!((p.state, p.observed.as_deref()), (PreviewState::Pending, Some("unmoved")));

        let done = resume(&fx.state, fx.task_id).await.unwrap();
        assert_eq!(done.local_integration.unwrap().state, IntegrationState::Integrated);
        assert_eq!(tip_of(&fx.repo, "main"), plan.result);
        assert!(!ref_exists(&fx.repo, &git::pin_ref(fx.task_id)));
    }

    #[tokio::test]
    async fn a_crash_after_the_branch_moved_is_recorded_without_moving_it_again() {
        let fx = fixture().await;
        let plan = pending_unmoved(&fx).await;
        git::advance(fx.repo.to_str().unwrap(), &plan).await.unwrap();
        let moved_at = tip_of(&fx.repo, "main");
        // The target has since advanced past the result: still applied.
        let later = commit_on(&fx.repo, "main", "later.txt", "l\n");

        let p = preview(&fx.state, fx.task_id).await.unwrap();
        assert_eq!(p.observed.as_deref(), Some("applied"));
        assert!(discard(&fx.state, fx.task_id).await.unwrap_err().contains("already holds"));
        let done = resume(&fx.state, fx.task_id).await.unwrap();
        assert_eq!(done.local_integration.unwrap().state, IntegrationState::Integrated);
        assert_eq!(moved_at, plan.result);
        assert_eq!(tip_of(&fx.repo, "main"), later);
        assert!(done.activity.iter().any(|e| matches!(e.kind, ActivityKind::IntegratedLocally { .. })));
    }

    #[tokio::test]
    async fn a_target_that_moved_elsewhere_drops_the_plan_and_changes_nothing() {
        let fx = fixture().await;
        let plan = pending_unmoved(&fx).await;
        let elsewhere = commit_on(&fx.repo, "main", "else.txt", "e\n");

        let err = resume(&fx.state, fx.task_id).await.unwrap_err();
        assert!(err.contains("plan was dropped"), "{err}");
        assert_eq!(tip_of(&fx.repo, "main"), elsewhere);
        assert_ne!(plan.result, elsewhere);
        assert!(on_disk(&fx, fx.task_id).local_integration.is_none());
        assert!(!ref_exists(&fx.repo, &git::pin_ref(fx.task_id)));
    }

    #[tokio::test]
    async fn a_pending_integration_can_be_discarded_while_nothing_moved() {
        let fx = fixture().await;
        let plan = pending_unmoved(&fx).await;
        let task = discard(&fx.state, fx.task_id).await.unwrap();
        assert!(task.local_integration.is_none());
        assert_eq!(tip_of(&fx.repo, "main"), plan.target_old);
        assert!(!ref_exists(&fx.repo, &git::pin_ref(fx.task_id)));
        assert!(resume(&fx.state, fx.task_id).await.is_err(), "nothing is pending any more");
    }

    #[tokio::test]
    async fn a_pin_without_a_pending_record_is_swept_and_a_pending_ones_is_kept() {
        let fx = fixture().await;
        let orphan = Uuid::new_v4();
        git::pin(&fx.repo, orphan, &fx.tip).await.unwrap();
        let other = seed(&fx.state, fx.project_id, "task-gone", Some(&fx.tip)).await;
        git::pin(&fx.repo, other, &fx.tip).await.unwrap();
        let amend = move |s: &mut HashMap<Uuid, Task>| {
            s.get_mut(&other).unwrap().local_integration = Some(LocalIntegration {
                state: IntegrationState::Pending,
                target_branch: "main".into(),
                target_old_oid: "0".repeat(40),
                approved_tip: "1".repeat(40),
                result_oid: "2".repeat(40),
                strategy: IntegrationStrategy::FastForward,
                started_at: chrono::Utc::now(),
                completed_at: None,
            });
        };
        crate::lifecycle::record(&fx.state.task.tasks, &fx.state.storage, other, &amend).await.unwrap();

        sweep_orphan_pins(&fx.state, &fx.repo).await;
        assert!(!ref_exists(&fx.repo, &git::pin_ref(orphan)));
        assert!(ref_exists(&fx.repo, &git::pin_ref(other)));
    }

    #[tokio::test]
    async fn a_second_task_cannot_be_planned_against_a_target_with_an_unfinished_integration() {
        let fx = fixture().await;
        git(&fx.repo, &["branch", "task-b", "main"]);
        let b_tip = commit_on(&fx.repo, "task-b", "b.txt", "b\n");
        let b = seed(&fx.state, fx.project_id, "task-b", Some(&b_tip)).await;
        pending_unmoved(&fx).await;
        let p = preview(&fx.state, b).await.unwrap();
        assert_eq!(p.state, PreviewState::Refused);
        assert!(p.reason.unwrap().contains("Resume or discard"));
    }

    #[tokio::test]
    async fn two_tasks_integrating_at_once_never_corrupt_the_target() {
        let fx = fixture().await;
        git(&fx.repo, &["branch", "task-b", "main"]);
        let b_tip = commit_on(&fx.repo, "task-b", "b.txt", "b\n");
        let b = seed(&fx.state, fx.project_id, "task-b", Some(&b_tip)).await;
        let ca = ready(&fx, fx.task_id).await;
        let cb = ready(&fx, b).await;

        let (ra, rb) = tokio::join!(integrate(&fx.state, fx.task_id, ca), integrate(&fx.state, b, cb.clone()));
        // Both were planned against the same target, so the second to run
        // finds it moved and is refused rather than merged blind.
        assert!(ra.is_ok() ^ rb.is_ok(), "{ra:?} {rb:?}");
        let tip = tip_of(&fx.repo, "main");
        assert!(tip == fx.tip || tip == b_tip);
        git(&fx.repo, &["fsck", "--no-progress"]);

        // Whichever lost is asked again against the new target and lands.
        for id in [fx.task_id, b] {
            let p = preview(&fx.state, id).await.unwrap();
            if p.state != PreviewState::Integrated {
                integrate(&fx.state, id, confirmation(&p)).await.unwrap();
            }
        }
        for t in [&fx.tip, &b_tip] {
            git(&fx.repo, &["merge-base", "--is-ancestor", t, "main"]);
        }
    }

    #[tokio::test]
    async fn an_integration_never_deletes_or_rewrites_anything_but_the_target() {
        let fx = fixture().await;
        let refs_before = git(&fx.repo, &["for-each-ref", "--format=%(refname) %(objectname)", "refs/heads"]);
        let c = ready(&fx, fx.task_id).await;
        integrate(&fx.state, fx.task_id, c).await.unwrap();
        let refs_after = git(&fx.repo, &["for-each-ref", "--format=%(refname) %(objectname)", "refs/heads"]);
        let changed: Vec<_> = refs_before.lines().filter(|l| !refs_after.lines().any(|a| a == *l)).collect();
        assert_eq!(changed.len(), 1, "only main moved: {changed:?}");
        assert!(changed[0].starts_with("refs/heads/main "));
        assert_eq!(refs_before.lines().count(), refs_after.lines().count(), "no branch was deleted or added");
    }
}
