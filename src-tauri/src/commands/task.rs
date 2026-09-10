use crate::domain::{
    Task, TaskStatus, TaskCategory, TaskPriority, TaskComplexity,
    TaskImpact, SecuritySeverity, TaskPhase, Subtask,
};
use crate::domain::task::ExternalRef;
use crate::config::Storage;
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Assemble the handles [`crate::lifecycle::terminalize`] needs out of an
/// `AppState`.
///
/// Kept here rather than on `AppState` because `crate::lifecycle` is
/// deliberately free of every `tauri::` reference so the daemon links it, and
/// `AppState` is not: the daemon builds the same context from its own handles.
pub(crate) fn terminalize_ctx(state: &crate::AppState) -> crate::lifecycle::TerminalizeCtx<'_> {
    crate::lifecycle::TerminalizeCtx {
        tasks: &state.task.tasks,
        projects: &state.project.projects,
        repositories: &state.repository.repositories,
        worktree_manager: &state.worktree_manager,
        storage: &state.storage,
        authority: &state.task_lifecycle_locks,
        // `get()` rather than waiting: before the executor is initialised there
        // is no queue, so there is nothing that could be running.
        running: state
            .executor
            .get()
            .map(|e| e.as_ref() as &dyn crate::lifecycle::ExecutionOwnership),
    }
}

/// How a status transition affects a task's execution state and its worktree.
///
/// Resetting execution state and destroying a worktree are different
/// operations, and these variants keep them apart. One variant used to do
/// both, which made re-queuing a failed task also delete the work that task
/// had produced. Worse, a task's branch and worktree path are derived from its
/// id, which a retry does not change, so that asynchronous deletion raced the
/// retry recreating them at the very same path: the cleanup could remove the
/// *successful* second attempt's directory, branch and commits, then clear
/// `worktree_path`, leaving a task reporting completion with nothing on disk
/// behind it.
///
/// Every transition maps to exactly one variant, so no call site can spawn
/// cleanup twice by evaluating overlapping conditions in the wrong order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTransitionEffect {
    /// Moving back into the workflow: out of `Error`, or back to an early
    /// column. Clears phase, progress and the error message, which is what
    /// makes the task eligible to be picked up and run again.
    ///
    /// Deliberately preserves `worktree_path` and `branch_name`: the next
    /// execution reattaches to that branch and continues from the work already
    /// there. Discarding a worktree is an explicit destructive action, not a
    /// side effect of moving a card back into a column.
    ResetExecutionState,
    /// Moving to `Done`: the task is finished with its worktree, so remove it.
    /// Everything else — `branch_name` included, for PR creation — is left
    /// untouched.
    CleanUpWorktree,
    /// No effect on execution state or worktree.
    None,
}

/// `Done` is tested first so a task finishing *out of* `Error` still gets its
/// terminal cleanup. Calling a task done is an explicit statement that its
/// worktree is no longer needed; re-queuing that same task is the opposite.
fn classify_status_transition(old_status: &TaskStatus, new_status: &TaskStatus) -> StatusTransitionEffect {
    if matches!(new_status, TaskStatus::Done) {
        StatusTransitionEffect::CleanUpWorktree
    } else if *old_status == TaskStatus::Error
        || matches!(new_status, TaskStatus::Backlog | TaskStatus::Queue | TaskStatus::InProgress)
    {
        StatusTransitionEffect::ResetExecutionState
    } else {
        StatusTransitionEffect::None
    }
}

pub type Tasks = Arc<RwLock<HashMap<Uuid, Task>>>;

/// Helper function to persist tasks for a project after mutation
fn persist_project_tasks(storage: &Storage, tasks: &HashMap<Uuid, Task>, project_id: Uuid) {
    let project_tasks: Vec<Task> = tasks
        .values()
        .filter(|t| t.project_id == project_id)
        .cloned()
        .collect();
    
    if let Err(e) = storage.save_project_tasks(project_id, &project_tasks) {
        eprintln!("Warning: Failed to persist tasks for project {}: {}", project_id, e);
    }
}

#[derive(Clone)]
pub struct TaskState {
    pub tasks: Tasks,
}

impl TaskState {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for TaskState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTaskParams {
    pub project_id: String,
    pub title: String,
    pub description: Option<String>,
    pub model: String,
    pub planning_mode: bool,
    pub dependencies: Vec<String>,
    pub category: Option<TaskCategory>,
    pub priority: Option<TaskPriority>,
    pub complexity: Option<TaskComplexity>,
    pub impact: Option<TaskImpact>,
    pub security_severity: Option<SecuritySeverity>,
    pub github_issue_url: Option<String>,
    pub gitlab_issue_url: Option<String>,
    pub linear_ticket_id: Option<String>,
}

#[tauri::command]
pub async fn create_task(
    state: tauri::State<'_, crate::AppState>,
    params: CreateTaskParams,
) -> Result<Task, String> {
    let id = Uuid::new_v4();
    let project_id = Uuid::parse_str(&params.project_id).map_err(|e| e.to_string())?;
    let dependencies = params.dependencies
        .into_iter()
        .filter_map(|d| Uuid::parse_str(&d).ok())
        .collect();

    let now = chrono::Utc::now();
    let mut tasks = state.task.tasks.write().await;

    // Calculate position for new task (at the end of backlog)
    let position = {
        let max_pos = tasks
            .values()
            .filter(|t| t.project_id == project_id && t.status == TaskStatus::Backlog)
            .map(|t| t.position)
            .max()
            .unwrap_or(-1);
        max_pos + 1
    };

    let task = Task {
        id,
        project_id,
        title: params.title,
        description: params.description,
        status: TaskStatus::Backlog,
        model: params.model,
        planning_mode: params.planning_mode,
        dependencies,
        worktree_id: None,
        jj_change_id: None,
        category: params.category.unwrap_or_default(),
        priority: params.priority.unwrap_or_default(),
        complexity: params.complexity.unwrap_or_default(),
        impact: params.impact.unwrap_or_default(),
        security_severity: params.security_severity.unwrap_or_default(),
        phase: TaskPhase::Idle,
        phase_progress: 0,
        overall_progress: 0,
        subtasks: Vec::new(),
        sequence_number: 0,
        position,
        github_issue_url: params.github_issue_url,
        gitlab_issue_url: params.gitlab_issue_url,
        linear_ticket_id: params.linear_ticket_id,
        jira_issue_key: None,
        pr_url: None,
        external_refs: Vec::new(),
        qa_signoff: None,
        human_review: None,
        stuck_since: None,
        error_message: None,
        worktree_path: None,
        branch_name: None,
        cleanup_in_flight: false,
        pr_review_plan: None,
        created_at: now,
        updated_at: now,
    };

    tasks.insert(id, task.clone());
    
    // Persist to disk
    persist_project_tasks(&state.storage, &tasks, project_id);
    
    Ok(task)
}

#[tauri::command]
pub async fn list_tasks(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<Vec<Task>, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    let tasks = state.task.tasks.read().await;
    Ok(tasks
        .values()
        .filter(|t| t.project_id == project_id)
        .cloned()
        .collect())
}

/// Move a task to `status`, doing whatever that transition actually requires
/// before saying it happened.
///
/// A move into `Done` is not a field assignment: it is the claim that the task
/// is finished with its worktree, so it goes through
/// [`crate::lifecycle::terminalize`], which removes the checkout first and
/// commits `Done` only if that succeeded. A refusal comes back as `Err` with
/// git's own reason, and the task is exactly where it was -- the frontend
/// already surfaces that and leaves the card in place.
///
/// Every other transition takes the same per-task lease before touching
/// anything. Not for symmetry: a move back into a working column resets the
/// task's execution state, and doing that while a cleanup is midway through
/// removing the checkout would either make the task executable against a
/// directory that is being deleted, or be silently overwritten by the
/// terminal commit that cleanup is about to make. The lease is what makes the
/// two orderings the only two possible.
#[tauri::command]
pub async fn update_task_status(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    status: TaskStatus,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;

    if matches!(status, TaskStatus::Done) {
        return match crate::lifecycle::terminalize(
            terminalize_ctx(&state),
            task_id,
            crate::lifecycle::Origin::User,
            crate::lifecycle::TerminalizeRequest::new(status),
        )
        .await
        {
            Ok(task) => Ok(Some(task)),
            Err(crate::lifecycle::TerminalizeRefusal::TaskNotFound) => Ok(None),
            Err(refusal) => Err(refusal.to_string()),
        };
    }

    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        let old_status = task.status.clone();
        task.status = status.clone();
        task.updated_at = chrono::Utc::now();

        match classify_status_transition(&old_status, &status) {
            StatusTransitionEffect::ResetExecutionState => {
                // `worktree_path` and `branch_name` survive on purpose: the
                // next execution reattaches to them. See
                // `StatusTransitionEffect`.
                task.reset_execution_state();
            }
            // Unreachable: `CleanUpWorktree` is exactly the `Done` case, and
            // that returned above. Left as an explicit arm rather than a
            // catch-all so a future classifier that starts returning it from
            // somewhere else fails to compile here instead of silently
            // skipping the cleanup.
            StatusTransitionEffect::CleanUpWorktree => {}
            StatusTransitionEffect::None => {}
        }

        let updated_task = task.clone();
        let project_id = task.project_id;

        // Persist to disk
        persist_project_tasks(&state.storage, &tasks, project_id);

        Ok(Some(updated_task))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn set_task_dependencies(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    dependencies: Vec<String>,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        task.dependencies = dependencies
            .into_iter()
            .filter_map(|d| Uuid::parse_str(&d).ok())
            .collect();
        task.updated_at = chrono::Utc::now();
        let updated_task = task.clone();
        let project_id = task.project_id;
        
        // Persist to disk
        persist_project_tasks(&state.storage, &tasks, project_id);
        
        Ok(Some(updated_task))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn update_task_metadata(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    category: Option<TaskCategory>,
    priority: Option<TaskPriority>,
    complexity: Option<TaskComplexity>,
    impact: Option<TaskImpact>,
    security_severity: Option<SecuritySeverity>,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        if let Some(cat) = category {
            task.category = cat;
        }
        if let Some(pri) = priority {
            task.priority = pri;
        }
        if let Some(comp) = complexity {
            task.complexity = comp;
        }
        if let Some(imp) = impact {
            task.impact = imp;
        }
        if let Some(sec) = security_severity {
            task.security_severity = sec;
        }
        task.updated_at = chrono::Utc::now();
        let updated_task = task.clone();
        let project_id = task.project_id;
        
        // Persist to disk
        persist_project_tasks(&state.storage, &tasks, project_id);
        
        Ok(Some(updated_task))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn update_task_progress(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    phase: TaskPhase,
    phase_progress: u8,
    overall_progress: u8,
    sequence_number: u32,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        task.phase = phase;
        task.phase_progress = phase_progress.min(100);
        task.overall_progress = overall_progress.min(100);
        task.sequence_number = sequence_number;
        task.updated_at = chrono::Utc::now();
        let updated_task = task.clone();
        let project_id = task.project_id;
        
        // Persist to disk
        persist_project_tasks(&state.storage, &tasks, project_id);
        
        Ok(Some(updated_task))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn add_subtask(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    title: String,
) -> Result<Option<Subtask>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        let subtask = Subtask {
            id: Uuid::new_v4(),
            title,
            completed: false,
        };
        task.subtasks.push(subtask.clone());
        task.updated_at = chrono::Utc::now();
        let project_id = task.project_id;
        
        // Persist to disk
        persist_project_tasks(&state.storage, &tasks, project_id);
        
        Ok(Some(subtask))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn toggle_subtask(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    subtask_id: String,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let subtask_id = Uuid::parse_str(&subtask_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        if let Some(subtask) = task.subtasks.iter_mut().find(|s| s.id == subtask_id) {
            subtask.completed = !subtask.completed;
            task.updated_at = chrono::Utc::now();
            let updated_task = task.clone();
            let project_id = task.project_id;
            
            // Persist to disk
            persist_project_tasks(&state.storage, &tasks, project_id);
            
            return Ok(Some(updated_task));
        }
    }
    Ok(None)
}

#[tauri::command]
pub async fn link_github_issue(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    issue_url: String,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        task.github_issue_url = Some(issue_url.clone());
        // Also add to external_refs if not already there
        if !task.external_refs.iter().any(|r| matches!(r, ExternalRef::GithubIssue { url, .. } if url == &issue_url)) {
            // Parse: https://github.com/{owner}/{repo}/issues/{number}
            let parts: Vec<&str> = issue_url.trim_end_matches('/').split('/').collect();
            if let Some(issues_idx) = parts.iter().position(|&p| p == "issues") {
                if let Some(gh_idx) = parts.iter().position(|&p| p == "github.com") {
                    if let (Some(number_str), true) = (parts.get(issues_idx + 1), gh_idx + 2 < issues_idx) {
                        if let Ok(number) = number_str.parse::<u32>() {
                            let repo = format!("{}/{}", parts[gh_idx + 1], parts[gh_idx + 2]);
                            task.external_refs.push(ExternalRef::GithubIssue {
                                url: issue_url.clone(),
                                number,
                                repo,
                                state: Some("OPEN".to_string()),
                            });
                        }
                    }
                }
            }
        }
        task.updated_at = chrono::Utc::now();
        let updated_task = task.clone();
        let project_id = task.project_id;

        // Persist to disk
        persist_project_tasks(&state.storage, &tasks, project_id);

        Ok(Some(updated_task))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn link_pr(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    pr_url: String,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        task.pr_url = Some(pr_url.clone());
        // Also add to external_refs if not already there
        if !task.external_refs.iter().any(|r| matches!(r, ExternalRef::GithubPr { url, .. } if url == &pr_url)) {
            // Parse: https://github.com/{owner}/{repo}/pull/{number}
            let parts: Vec<&str> = pr_url.trim_end_matches('/').split('/').collect();
            if let Some(pull_idx) = parts.iter().position(|&p| p == "pull") {
                if let Some(gh_idx) = parts.iter().position(|&p| p == "github.com") {
                    if let (Some(number_str), true) = (parts.get(pull_idx + 1), gh_idx + 2 < pull_idx) {
                        if let Ok(number) = number_str.parse::<u32>() {
                            let repo = format!("{}/{}", parts[gh_idx + 1], parts[gh_idx + 2]);
                            task.external_refs.push(ExternalRef::GithubPr {
                                url: pr_url.clone(),
                                number,
                                repo,
                                state: Some("OPEN".to_string()),
                            });
                        }
                    }
                }
            }
        }
        task.updated_at = chrono::Utc::now();
        let updated_task = task.clone();
        let project_id = task.project_id;

        // Persist to disk
        persist_project_tasks(&state.storage, &tasks, project_id);

        Ok(Some(updated_task))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn mark_task_stuck(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        task.stuck_since = Some(chrono::Utc::now());
        task.updated_at = chrono::Utc::now();
        let updated_task = task.clone();
        let project_id = task.project_id;
        
        // Persist to disk
        persist_project_tasks(&state.storage, &tasks, project_id);
        
        Ok(Some(updated_task))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn unstick_task(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        task.stuck_since = None;
        task.updated_at = chrono::Utc::now();
        let updated_task = task.clone();
        let project_id = task.project_id;
        
        // Persist to disk
        persist_project_tasks(&state.storage, &tasks, project_id);
        
        Ok(Some(updated_task))
    } else {
        Ok(None)
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTaskParams {
    pub task_id: String,
    pub title: Option<String>,
    pub description: Option<Option<String>>,
    pub category: Option<TaskCategory>,
    pub priority: Option<TaskPriority>,
    pub complexity: Option<TaskComplexity>,
    pub impact: Option<TaskImpact>,
    pub security_severity: Option<SecuritySeverity>,
    pub model: Option<String>,
    pub planning_mode: Option<bool>,
}

#[tauri::command]
pub async fn update_task(
    state: tauri::State<'_, crate::AppState>,
    params: UpdateTaskParams,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&params.task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        if let Some(t) = params.title {
            task.title = t;
        }
        if let Some(d) = params.description {
            task.description = d;
        }
        if let Some(c) = params.category {
            task.category = c;
        }
        if let Some(p) = params.priority {
            task.priority = p;
        }
        if let Some(c) = params.complexity {
            task.complexity = c;
        }
        if let Some(i) = params.impact {
            task.impact = i;
        }
        if let Some(s) = params.security_severity {
            task.security_severity = s;
        }
        if let Some(m) = params.model {
            task.model = m;
        }
        if let Some(p) = params.planning_mode {
            task.planning_mode = p;
        }
        task.updated_at = chrono::Utc::now();
        let updated_task = task.clone();
        let project_id = task.project_id;
        
        // Persist to disk
        persist_project_tasks(&state.storage, &tasks, project_id);
        
        Ok(Some(updated_task))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn delete_task(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<bool, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;

    // The whole policy -- take the lease, prove no agent owns the task, clean
    // the checkout up safely, and only then remove the record, persisting
    // before publishing -- lives in `lifecycle::delete`, which the daemon's IPC
    // handler calls too. A refusal is returned rather than logged past: the
    // task keeps its record, its `worktree_path`, its branch and its work, and
    // the user can put the checkout right and ask again.
    crate::lifecycle::delete(terminalize_ctx(&state), task_id)
        .await
        .map_err(|refusal| refusal.to_string())
}

/// Renumber `target_status`'s column so `task_id` sits at `new_position` and
/// every card in that column has a distinct, gapless position.
///
/// Takes the map it should mutate rather than reading the live one, so the
/// terminal path can run it inside
/// [`crate::lifecycle::terminalize`](crate::lifecycle::terminalize)'s final
/// write. Positions computed before a cleanup subprocess ran are stale by the
/// time it finishes -- another card may have been dragged into the same column
/// meanwhile -- and committing them would publish the card at a position that
/// was correct a second ago.
fn renumber_column(
    staged: &mut HashMap<Uuid, Task>,
    project_id: Uuid,
    task_id: Uuid,
    target_status: &TaskStatus,
    new_position: i32,
) {
    let mut column: Vec<(Uuid, i32)> = staged
        .values()
        .filter(|t| t.project_id == project_id && t.status == *target_status && t.id != task_id)
        .map(|t| (t.id, t.position))
        .collect();
    column.sort_by_key(|(_, pos)| *pos);

    let clamped = new_position.max(0).min(column.len() as i32) as usize;
    column.insert(clamped, (task_id, 0));

    let now = chrono::Utc::now();
    for (idx, (tid, _)) in column.iter().enumerate() {
        if let Some(task) = staged.get_mut(tid) {
            task.position = idx as i32;
            task.updated_at = now;
        }
    }
}

/// Reorder a task within its column or when moving to a new column.
/// `new_position` is the target position index in the destination column.
///
/// Dragging a card onto `Done` is the same lifecycle decision as calling
/// [`update_task_status`] with `Done`, and reaches the same
/// [`crate::lifecycle::terminalize_leased`], so the two cannot diverge: the
/// worktree is removed first, and the card only lands in the column if that
/// succeeded. The new column positions are computed inside that operation's
/// final write rather than before the removal, so they describe the board as it
/// is when they are saved.
///
/// The lease is taken before anything is read, and the transition is classified
/// exactly once from that one reading. Deciding first and locking afterwards
/// looks equivalent and is not: the status a pre-lease read returns is the
/// status the task had before whatever was holding the lease finished with it,
/// so a drag classified as an ordinary move could become a move into `Done` by
/// the time it was applied -- and would then write `Done` itself, with no
/// cleanup, which is the one thing this design exists to make impossible.
#[tauri::command]
pub async fn reorder_task(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    new_status: Option<TaskStatus>,
    new_position: i32,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;

    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    let Some((project_id, old_status)) = ({
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_id).map(|t| (t.project_id, t.status.clone()))
    }) else {
        return Ok(None);
    };
    let target_status = new_status.unwrap_or_else(|| old_status.clone());

    let effect = if old_status == target_status {
        StatusTransitionEffect::None
    } else {
        classify_status_transition(&old_status, &target_status)
    };

    if effect == StatusTransitionEffect::CleanUpWorktree {
        let column = target_status.clone();
        let reposition = move |staged: &mut HashMap<Uuid, Task>| {
            renumber_column(staged, project_id, task_id, &column, new_position);
        };
        let mut request = crate::lifecycle::TerminalizeRequest::new(target_status);
        request.on_success = Some(&reposition);

        return match crate::lifecycle::terminalize_leased(
            terminalize_ctx(&state),
            task_id,
            crate::lifecycle::Origin::User,
            request,
        )
        .await
        {
            // Read back rather than used directly, because the caller wants
            // the whole committed record and the position it landed at, which
            // is the one the board and the file now agree on.
            Ok(_) => Ok(state.task.tasks.read().await.get(&task_id).cloned()),
            Err(crate::lifecycle::TerminalizeRefusal::TaskNotFound) => Ok(None),
            Err(refusal) => Err(refusal.to_string()),
        };
    }

    let mut tasks = state.task.tasks.write().await;

    if effect != StatusTransitionEffect::None || old_status != target_status {
        if let Some(task) = tasks.get_mut(&task_id) {
            task.status = target_status.clone();
            if effect == StatusTransitionEffect::ResetExecutionState {
                // Worktree and branch preserved; see `StatusTransitionEffect`.
                task.reset_execution_state();
            }
        }
    }

    renumber_column(&mut tasks, project_id, task_id, &target_status, new_position);

    let updated_task = tasks.get(&task_id).cloned();
    persist_project_tasks(&state.storage, &tasks, project_id);

    Ok(updated_task)
}

#[tauri::command]
pub async fn add_external_ref(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    external_ref: ExternalRef,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;
    if let Some(task) = tasks.get_mut(&task_id) {
        task.external_refs.push(external_ref);
        task.updated_at = chrono::Utc::now();
        let updated = task.clone();
        let project_id = task.project_id;
        persist_project_tasks(&state.storage, &tasks, project_id);
        Ok(Some(updated))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn remove_external_ref(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    ref_index: usize,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;
    if let Some(task) = tasks.get_mut(&task_id) {
        if ref_index < task.external_refs.len() {
            task.external_refs.remove(ref_index);
        }
        task.updated_at = chrono::Utc::now();
        let updated = task.clone();
        let project_id = task.project_id;
        persist_project_tasks(&state.storage, &tasks, project_id);
        Ok(Some(updated))
    } else {
        Ok(None)
    }
}

/// Core update_task_status logic extracted for testability.
///
/// Applies status transition rules without Tauri state, worktree removal, or
/// persistence. Classification is delegated to [`classify_status_transition`]
/// — the same function `update_task_status`/`reorder_task` use — so this
/// helper cannot drift into teaching a different contract than production.
///
/// Deliberately does **not** clear `worktree_path`: production only clears it
/// in the same durable write [`crate::lifecycle::terminalize`] makes once the
/// removal has actually succeeded, and this helper runs no removal. Clearing it
/// here would model a cleanup that never happened, exactly the bug the
/// worktree-lifecycle work closed.
#[cfg(test)]
pub fn update_task_status_logic(
    tasks: &mut HashMap<Uuid, Task>,
    task_id: Uuid,
    status: TaskStatus,
) -> Option<Task> {
    if let Some(task) = tasks.get_mut(&task_id) {
        let old_status = task.status.clone();
        task.status = status.clone();
        task.updated_at = chrono::Utc::now();

        if let StatusTransitionEffect::ResetExecutionState = classify_status_transition(&old_status, &status) {
            task.reset_execution_state();
        }

        Some(task.clone())
    } else {
        None
    }
}

/// Core add_external_ref logic extracted for testability
#[cfg(test)]
pub fn add_external_ref_logic(
    tasks: &mut HashMap<Uuid, Task>,
    task_id: Uuid,
    external_ref: ExternalRef,
) -> Option<Task> {
    if let Some(task) = tasks.get_mut(&task_id) {
        task.external_refs.push(external_ref);
        task.updated_at = chrono::Utc::now();
        Some(task.clone())
    } else {
        None
    }
}

/// Core remove_external_ref logic extracted for testability
#[cfg(test)]
pub fn remove_external_ref_logic(
    tasks: &mut HashMap<Uuid, Task>,
    task_id: Uuid,
    ref_index: usize,
) -> Option<Task> {
    if let Some(task) = tasks.get_mut(&task_id) {
        if ref_index < task.external_refs.len() {
            task.external_refs.remove(ref_index);
        }
        task.updated_at = chrono::Utc::now();
        Some(task.clone())
    } else {
        None
    }
}


/// Core reorder logic extracted for testability.
///
/// Delegates the position arithmetic to the production [`renumber_column`] and
/// the transition decision to the production [`classify_status_transition`], so
/// the tests below exercise the code the commands run rather than a copy of it.
/// What it deliberately does not do is any worktree removal: a `Done`
/// transition in the real `reorder_task` goes through
/// [`crate::lifecycle::terminalize`], which needs a repository and a
/// filesystem, so the cases covered here are the column arithmetic and the
/// execution-state reset.
#[cfg(test)]
pub fn reorder_task_logic(
    tasks: &mut HashMap<Uuid, Task>,
    task_id: Uuid,
    new_status: Option<TaskStatus>,
    new_position: i32,
) -> Option<Task> {
    let (project_id, old_status, target_status) = {
        let task = tasks.get(&task_id)?;
        let target = new_status.unwrap_or_else(|| task.status.clone());
        (task.project_id, task.status.clone(), target)
    };

    if old_status != target_status {
        if let Some(task) = tasks.get_mut(&task_id) {
            task.status = target_status.clone();
            if let StatusTransitionEffect::ResetExecutionState =
                classify_status_transition(&old_status, &target_status)
            {
                task.reset_execution_state();
            }
        }
    }

    renumber_column(tasks, project_id, task_id, &target_status, new_position);

    tasks.get(&task_id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{create_test_task_full, Uuid};
    use crate::domain::TaskStatus;

    /// Helper to create a HashMap of tasks for testing
    fn create_test_tasks_map(tasks: Vec<Task>) -> HashMap<Uuid, Task> {
        tasks.into_iter().map(|t| (t.id, t)).collect()
    }
    /// Every status the board has today, so the exhaustive cases below stay
    /// exhaustive when a new one is added.
    const ALL_STATUSES: [TaskStatus; 8] = [
        TaskStatus::Backlog, TaskStatus::Queue, TaskStatus::InProgress, TaskStatus::AiReview,
        TaskStatus::HumanReview, TaskStatus::Done, TaskStatus::PrCreated, TaskStatus::Error,
    ];

    /// A task that failed a run: it carries the error the run reported, and
    /// still points at the worktree and branch that run produced.
    fn failed_task_with_worktree(project_id: Uuid) -> Task {
        let mut task = create_test_task_full("Failed task", project_id, TaskStatus::Error, 0);
        task.phase = TaskPhase::Failed;
        task.phase_progress = 40;
        task.overall_progress = 30;
        task.error_message = Some("the agent reported a failure".to_string());
        task.worktree_path = Some("/tmp/wt/task-abcd1234".to_string());
        task.branch_name = Some("task-abcd1234".to_string());
        task
    }

    #[test]
    fn requeuing_a_failed_task_resets_execution_state_but_keeps_its_work() {
        // The decided retry path: the user drags the card out of Error into
        // Queue, the queue promotes it, and the next execution reattaches to
        // the branch that is still there. Destroying the worktree here would
        // throw away everything the failed attempt had already done, and --
        // because the removal is asynchronous while the task is immediately
        // eligible to run again -- could just as easily destroy what the
        // *retry* produces at that same path.
        assert_eq!(
            classify_status_transition(&TaskStatus::Error, &TaskStatus::Queue),
            StatusTransitionEffect::ResetExecutionState,
            "retrying through Queue must not select worktree cleanup"
        );

        let project_id = Uuid::new_v4();
        let task = failed_task_with_worktree(project_id);
        let task_id = task.id;
        let mut tasks = create_test_tasks_map(vec![task]);

        let updated = update_task_status_logic(&mut tasks, task_id, TaskStatus::Queue)
            .expect("the task exists");

        assert_eq!(updated.status, TaskStatus::Queue);
        assert_eq!(updated.phase, TaskPhase::Idle, "phase must reset so the poller picks it up");
        assert_eq!(updated.phase_progress, 0);
        assert_eq!(updated.overall_progress, 0);
        assert_eq!(updated.error_message, None, "the previous failure must not linger");
        assert_eq!(
            updated.worktree_path.as_deref(),
            Some("/tmp/wt/task-abcd1234"),
            "the retry continues in the existing worktree"
        );
        assert_eq!(
            updated.branch_name.as_deref(),
            Some("task-abcd1234"),
            "the retry reattaches to the existing branch"
        );
    }

    #[test]
    fn retrying_straight_into_progress_preserves_the_worktree_too() {
        // Error -> InProgress skips the Queue column but means the same thing.
        // A caller taking the shortcut must not get a different, destructive
        // lifecycle than one going through Queue.
        assert_eq!(
            classify_status_transition(&TaskStatus::Error, &TaskStatus::InProgress),
            StatusTransitionEffect::ResetExecutionState,
        );

        let project_id = Uuid::new_v4();
        let task = failed_task_with_worktree(project_id);
        let task_id = task.id;
        let mut tasks = create_test_tasks_map(vec![task]);

        let updated = update_task_status_logic(&mut tasks, task_id, TaskStatus::InProgress)
            .expect("the task exists");

        assert_eq!(updated.phase, TaskPhase::Idle);
        assert_eq!(updated.overall_progress, 0);
        assert_eq!(updated.error_message, None);
        assert_eq!(updated.worktree_path.as_deref(), Some("/tmp/wt/task-abcd1234"));
        assert_eq!(updated.branch_name.as_deref(), Some("task-abcd1234"));
    }

    #[test]
    fn moving_a_task_back_into_the_workflow_never_destroys_its_worktree() {
        // Backlog, Queue and InProgress are places work continues from, not
        // places it is abandoned. Preserving the worktree is the safe default;
        // discarding one is a separate explicit action.
        for old in &ALL_STATUSES {
            for new in [TaskStatus::Backlog, TaskStatus::Queue, TaskStatus::InProgress] {
                assert_eq!(
                    classify_status_transition(old, &new),
                    StatusTransitionEffect::ResetExecutionState,
                    "{old:?} -> {new:?} must reset execution state without cleaning up"
                );
            }
        }
    }

    #[test]
    fn finishing_a_task_still_cleans_up_its_worktree() {
        // The fix must not disable terminal cleanup. Done is the point where
        // the product genuinely no longer needs the directory.
        for old in &ALL_STATUSES {
            assert_eq!(
                classify_status_transition(old, &TaskStatus::Done),
                StatusTransitionEffect::CleanUpWorktree,
                "{old:?} -> Done must still clean up"
            );
        }
    }

    #[test]
    fn abandoning_a_failed_task_as_done_cleans_up_instead_of_resetting() {
        // The one transition that satisfies both conditions: old_status is
        // Error *and* new_status is Done. It must resolve to the terminal
        // meaning -- a task deliberately called finished releases its
        // worktree -- rather than being captured by the reset path and
        // silently keeping a directory nothing will ever look at again.
        assert_eq!(
            classify_status_transition(&TaskStatus::Error, &TaskStatus::Done),
            StatusTransitionEffect::CleanUpWorktree,
            "Error -> Done is terminal, not a retry"
        );
    }

    #[test]
    fn every_transition_cleans_up_exactly_when_it_reaches_done() {
        // Exhaustive: cleanup is selected if and only if the task is becoming
        // Done. No other transition may reach `crate::lifecycle::terminalize`,
        // and each pair yields exactly one effect, so no call site can start
        // two cleanups by matching two overlapping conditions.
        for old in &ALL_STATUSES {
            for new in &ALL_STATUSES {
                let effect = classify_status_transition(old, new);
                assert_eq!(
                    effect == StatusTransitionEffect::CleanUpWorktree,
                    *new == TaskStatus::Done,
                    "{old:?} -> {new:?} disagrees with 'cleanup happens only on Done'"
                );
                if effect == StatusTransitionEffect::None {
                    assert!(
                        *old != TaskStatus::Error
                            && !matches!(
                                new,
                                TaskStatus::Backlog | TaskStatus::Queue | TaskStatus::InProgress
                            ),
                        "{old:?} -> {new:?} classified None but should reset execution state"
                    );
                }
            }
        }
    }

    #[test]
    fn dragging_a_card_and_updating_its_status_agree_on_lifecycle() {
        // Two entry points, one classifier. A retry started by dropping a card
        // into Queue must leave the task in exactly the state the direct
        // command leaves it in -- including still holding its worktree.
        //
        // Restricted to transitions that actually change the status. Only
        // `reorder_task` guards on `old != target`, so re-setting a task to
        // the status it already has resets execution state through one entry
        // point and not the other. That predates this fix, is not part of any
        // retry path, and is left alone here rather than silently widened
        // into.
        for new in ALL_STATUSES.iter().filter(|s| **s != TaskStatus::Error) {
            let project_id = Uuid::new_v4();
            let task = failed_task_with_worktree(project_id);
            let task_id = task.id;

            let mut by_command = create_test_tasks_map(vec![task.clone()]);
            let mut by_drag = create_test_tasks_map(vec![task]);

            let commanded = update_task_status_logic(&mut by_command, task_id, new.clone())
                .expect("the task exists");
            let dragged = reorder_task_logic(&mut by_drag, task_id, Some(new.clone()), 0)
                .expect("the task exists");

            assert_eq!(commanded.status, dragged.status, "status differs for -> {new:?}");
            assert_eq!(commanded.phase, dragged.phase, "phase differs for -> {new:?}");
            assert_eq!(
                commanded.phase_progress, dragged.phase_progress,
                "phase_progress differs for -> {new:?}"
            );
            assert_eq!(
                commanded.overall_progress, dragged.overall_progress,
                "overall_progress differs for -> {new:?}"
            );
            assert_eq!(
                commanded.error_message, dragged.error_message,
                "error_message differs for -> {new:?}"
            );
            assert_eq!(
                commanded.worktree_path, dragged.worktree_path,
                "worktree_path differs for -> {new:?}"
            );
            assert_eq!(
                commanded.branch_name, dragged.branch_name,
                "branch_name differs for -> {new:?}"
            );
        }
    }

    #[test]
    fn test_reorder_within_same_column_move_down() {
        // Setup: 3 tasks in Backlog at positions 0, 1, 2
        let project_id = Uuid::new_v4();
        let task1 = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let task2 = create_test_task_full("Task 2", project_id, TaskStatus::Backlog, 1);
        let task3 = create_test_task_full("Task 3", project_id, TaskStatus::Backlog, 2);
        
        let task1_id = task1.id;
        let task2_id = task2.id;
        let task3_id = task3.id;
        
        let mut tasks = create_test_tasks_map(vec![task1, task2, task3]);
        
        // Move task1 from position 0 to position 2 (end)
        let result = reorder_task_logic(&mut tasks, task1_id, None, 2);
        
        assert!(result.is_some());
        let updated_task = result.unwrap();
        assert_eq!(updated_task.position, 2);
        assert_eq!(updated_task.status, TaskStatus::Backlog);
        
        // Verify other tasks shifted up
        assert_eq!(tasks.get(&task2_id).unwrap().position, 0);
        assert_eq!(tasks.get(&task3_id).unwrap().position, 1);
    }

    #[test]
    fn test_reorder_within_same_column_move_up() {
        // Setup: 3 tasks in Backlog at positions 0, 1, 2
        let project_id = Uuid::new_v4();
        let task1 = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let task2 = create_test_task_full("Task 2", project_id, TaskStatus::Backlog, 1);
        let task3 = create_test_task_full("Task 3", project_id, TaskStatus::Backlog, 2);
        
        let task1_id = task1.id;
        let task2_id = task2.id;
        let task3_id = task3.id;
        
        let mut tasks = create_test_tasks_map(vec![task1, task2, task3]);
        
        // Move task3 from position 2 to position 0 (start)
        let result = reorder_task_logic(&mut tasks, task3_id, None, 0);
        
        assert!(result.is_some());
        let updated_task = result.unwrap();
        assert_eq!(updated_task.position, 0);
        
        // Verify other tasks shifted down
        assert_eq!(tasks.get(&task1_id).unwrap().position, 1);
        assert_eq!(tasks.get(&task2_id).unwrap().position, 2);
    }

    #[test]
    fn test_reorder_to_different_column() {
        // Setup: 2 tasks in Backlog, 1 task in InProgress
        let project_id = Uuid::new_v4();
        let task1 = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let task2 = create_test_task_full("Task 2", project_id, TaskStatus::Backlog, 1);
        let task3 = create_test_task_full("Task 3", project_id, TaskStatus::InProgress, 0);
        
        let task1_id = task1.id;
        let task2_id = task2.id;
        let task3_id = task3.id;
        
        let mut tasks = create_test_tasks_map(vec![task1, task2, task3]);
        
        // Move task1 from Backlog to InProgress at position 0 (before task3)
        let result = reorder_task_logic(
            &mut tasks,
            task1_id,
            Some(TaskStatus::InProgress),
            0,
        );
        
        assert!(result.is_some());
        let updated_task = result.unwrap();
        assert_eq!(updated_task.status, TaskStatus::InProgress);
        assert_eq!(updated_task.position, 0);
        
        // task3 should have shifted to position 1
        assert_eq!(tasks.get(&task3_id).unwrap().position, 1);
        
        // task2 should remain at position 0 in Backlog (now only task in that column)
        // Actually, task2 stays in Backlog but since we don't reorder it, its position stays 1
        // But it's the only one left in Backlog, so logically it would be "first"
        assert_eq!(tasks.get(&task2_id).unwrap().status, TaskStatus::Backlog);
    }

    #[test]
    fn test_reorder_clamps_position_to_valid_range() {
        // Setup: 2 tasks in Backlog
        let project_id = Uuid::new_v4();
        let task1 = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let task2 = create_test_task_full("Task 2", project_id, TaskStatus::Backlog, 1);
        
        let task1_id = task1.id;
        let task2_id = task2.id;
        
        let mut tasks = create_test_tasks_map(vec![task1, task2]);
        
        // Try to move task1 to position 100 (way beyond valid range)
        let result = reorder_task_logic(&mut tasks, task1_id, None, 100);
        
        assert!(result.is_some());
        let updated_task = result.unwrap();
        // Should be clamped to position 1 (max valid position with 2 tasks)
        assert_eq!(updated_task.position, 1);
        
        // task2 should be at position 0
        assert_eq!(tasks.get(&task2_id).unwrap().position, 0);
    }

    #[test]
    fn test_reorder_clamps_negative_position_to_zero() {
        // Setup: 2 tasks in Backlog
        let project_id = Uuid::new_v4();
        let task1 = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let task2 = create_test_task_full("Task 2", project_id, TaskStatus::Backlog, 1);
        
        let task2_id = task2.id;
        
        let mut tasks = create_test_tasks_map(vec![task1, task2]);
        
        // Try to move task2 to position -5 (negative)
        let result = reorder_task_logic(&mut tasks, task2_id, None, -5);
        
        assert!(result.is_some());
        let updated_task = result.unwrap();
        // Should be clamped to position 0
        assert_eq!(updated_task.position, 0);
    }

    #[test]
    fn test_reorder_nonexistent_task_returns_none() {
        let project_id = Uuid::new_v4();
        let task1 = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        
        let mut tasks = create_test_tasks_map(vec![task1]);
        
        // Try to reorder a task that doesn't exist
        let fake_id = Uuid::new_v4();
        let result = reorder_task_logic(&mut tasks, fake_id, None, 0);
        
        assert!(result.is_none());
    }

    #[test]
    fn test_reorder_to_empty_column() {
        // Setup: 2 tasks in Backlog, no tasks in InProgress
        let project_id = Uuid::new_v4();
        let task1 = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let task2 = create_test_task_full("Task 2", project_id, TaskStatus::Backlog, 1);
        
        let task1_id = task1.id;
        
        let mut tasks = create_test_tasks_map(vec![task1, task2]);
        
        // Move task1 to InProgress (empty column)
        let result = reorder_task_logic(
            &mut tasks,
            task1_id,
            Some(TaskStatus::InProgress),
            0,
        );
        
        assert!(result.is_some());
        let updated_task = result.unwrap();
        assert_eq!(updated_task.status, TaskStatus::InProgress);
        assert_eq!(updated_task.position, 0);
    }

    #[test]
    fn test_reorder_preserves_task_data() {
        // Ensure reordering doesn't corrupt other task fields
        let project_id = Uuid::new_v4();
        let mut task1 = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        task1.description = Some("Important description".to_string());
        task1.priority = crate::domain::TaskPriority::High;
        
        let original_id = task1.id;
        let original_title = task1.title.clone();
        let original_description = task1.description.clone();
        
        let mut tasks = create_test_tasks_map(vec![task1]);
        
        // Move to a different column
        let result = reorder_task_logic(
            &mut tasks,
            original_id,
            Some(TaskStatus::InProgress),
            0,
        );
        
        assert!(result.is_some());
        let updated_task = result.unwrap();
        
        // Verify other fields are preserved
        assert_eq!(updated_task.id, original_id);
        assert_eq!(updated_task.title, original_title);
        assert_eq!(updated_task.description, original_description);
        assert_eq!(updated_task.priority, crate::domain::TaskPriority::High);
    }

    #[test]
    fn test_reorder_multiple_tasks_same_column() {
        // Setup: 5 tasks in Backlog
        let project_id = Uuid::new_v4();
        let task1 = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let task2 = create_test_task_full("Task 2", project_id, TaskStatus::Backlog, 1);
        let task3 = create_test_task_full("Task 3", project_id, TaskStatus::Backlog, 2);
        let task4 = create_test_task_full("Task 4", project_id, TaskStatus::Backlog, 3);
        let task5 = create_test_task_full("Task 5", project_id, TaskStatus::Backlog, 4);
        
        let task1_id = task1.id;
        let task2_id = task2.id;
        let task3_id = task3.id;
        let task4_id = task4.id;
        let task5_id = task5.id;
        
        let mut tasks = create_test_tasks_map(vec![task1, task2, task3, task4, task5]);
        
        // Move task5 (position 4) to position 1
        let result = reorder_task_logic(&mut tasks, task5_id, None, 1);
        
        assert!(result.is_some());
        let updated_task = result.unwrap();
        assert_eq!(updated_task.position, 1);
        
        // Expected order: task1(0), task5(1), task2(2), task3(3), task4(4)
        assert_eq!(tasks.get(&task1_id).unwrap().position, 0);
        assert_eq!(tasks.get(&task5_id).unwrap().position, 1);
        assert_eq!(tasks.get(&task2_id).unwrap().position, 2);
        assert_eq!(tasks.get(&task3_id).unwrap().position, 3);
        assert_eq!(tasks.get(&task4_id).unwrap().position, 4);
    }

    #[test]
    fn test_reorder_different_projects_isolated() {
        // Ensure tasks from different projects don't affect each other
        let project_a = Uuid::new_v4();
        let project_b = Uuid::new_v4();
        
        let task_a1 = create_test_task_full("Task A1", project_a, TaskStatus::Backlog, 0);
        let task_a2 = create_test_task_full("Task A2", project_a, TaskStatus::Backlog, 1);
        let task_b1 = create_test_task_full("Task B1", project_b, TaskStatus::Backlog, 0);
        let task_b2 = create_test_task_full("Task B2", project_b, TaskStatus::Backlog, 1);
        
        let task_a1_id = task_a1.id;
        let task_b1_id = task_b1.id;
        let task_b2_id = task_b2.id;
        
        let mut tasks = create_test_tasks_map(vec![task_a1, task_a2, task_b1, task_b2]);
        
        // Move task_a1 to position 1 in project A
        let result = reorder_task_logic(&mut tasks, task_a1_id, None, 1);
        
        assert!(result.is_some());
        
        // Project B tasks should be unaffected
        assert_eq!(tasks.get(&task_b1_id).unwrap().position, 0);
        assert_eq!(tasks.get(&task_b2_id).unwrap().position, 1);
    }

    #[test]
    fn test_reorder_updates_timestamp() {
        let project_id = Uuid::new_v4();
        let task1 = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let original_updated_at = task1.updated_at;
        let task1_id = task1.id;

        let mut tasks = create_test_tasks_map(vec![task1]);

        // Small delay to ensure timestamp difference
        std::thread::sleep(std::time::Duration::from_millis(10));

        let result = reorder_task_logic(
            &mut tasks,
            task1_id,
            Some(TaskStatus::InProgress),
            0,
        );

        assert!(result.is_some());
        let updated_task = result.unwrap();

        // Timestamp should be updated
        assert!(updated_task.updated_at > original_updated_at);
    }

    // ===== update_task_status tests =====

    #[test]
    fn test_status_backlog_to_queue_resets_phase_and_progress() {
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        task.phase = TaskPhase::Coding;
        task.phase_progress = 50;
        task.overall_progress = 30;
        let task_id = task.id;

        let mut tasks = create_test_tasks_map(vec![task]);

        let result = update_task_status_logic(&mut tasks, task_id, TaskStatus::Queue);
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.status, TaskStatus::Queue);
        assert_eq!(updated.phase, TaskPhase::Idle);
        assert_eq!(updated.phase_progress, 0);
        assert_eq!(updated.overall_progress, 0);
    }

    #[test]
    fn test_status_queue_to_in_progress_resets_and_clears_error() {
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("Task 1", project_id, TaskStatus::Queue, 0);
        task.phase = TaskPhase::Planning;
        task.error_message = Some("old error".to_string());
        let task_id = task.id;

        let mut tasks = create_test_tasks_map(vec![task]);

        let result = update_task_status_logic(&mut tasks, task_id, TaskStatus::InProgress);
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.status, TaskStatus::InProgress);
        assert_eq!(updated.phase, TaskPhase::Idle);
        assert!(updated.error_message.is_none());
    }

    #[test]
    fn test_status_in_progress_to_error_preserves_error_message() {
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("Task 1", project_id, TaskStatus::InProgress, 0);
        task.phase = TaskPhase::Coding;
        task.phase_progress = 75;
        task.overall_progress = 60;
        task.error_message = Some("build failed".to_string());
        let task_id = task.id;

        let mut tasks = create_test_tasks_map(vec![task]);

        let result = update_task_status_logic(&mut tasks, task_id, TaskStatus::Error);
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.status, TaskStatus::Error);
        // Error status is not in the reset branch (Backlog|Queue|InProgress),
        // and old_status is InProgress (not Error), so no reset happens.
        // The error_message, phase, and progress are preserved.
        assert_eq!(updated.error_message, Some("build failed".to_string()));
        assert_eq!(updated.phase, TaskPhase::Coding);
        assert_eq!(updated.phase_progress, 75);
    }

    #[test]
    fn test_status_any_to_done_retains_worktree_path_until_cleanup_succeeds() {
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("Task 1", project_id, TaskStatus::InProgress, 0);
        task.worktree_path = Some("/tmp/wt-test".to_string());
        task.branch_name = Some("feature-branch".to_string());
        let task_id = task.id;

        let mut tasks = create_test_tasks_map(vec![task]);

        let result = update_task_status_logic(&mut tasks, task_id, TaskStatus::Done);
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.status, TaskStatus::Done);
        // This helper never runs worktree removal, so it must not model
        // removal as already successful — production only clears
        // `worktree_path` in the write that commits a removal that worked.
        assert_eq!(updated.worktree_path, Some("/tmp/wt-test".to_string()));
        // branch_name kept for PR creation either way.
        assert_eq!(updated.branch_name, Some("feature-branch".to_string()));
    }

    #[test]
    fn test_status_error_to_queue_requeue_clears_phase_and_error() {
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("Task 1", project_id, TaskStatus::Error, 0);
        task.phase = TaskPhase::Failed;
        task.phase_progress = 100;
        task.overall_progress = 40;
        task.error_message = Some("compilation error".to_string());
        let task_id = task.id;

        let mut tasks = create_test_tasks_map(vec![task]);

        let result = update_task_status_logic(&mut tasks, task_id, TaskStatus::Queue);
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.status, TaskStatus::Queue);
        assert_eq!(updated.phase, TaskPhase::Idle);
        assert_eq!(updated.phase_progress, 0);
        assert_eq!(updated.overall_progress, 0);
        assert!(updated.error_message.is_none());
    }

    #[test]
    fn test_status_update_nonexistent_task_returns_none() {
        let mut tasks: HashMap<Uuid, Task> = HashMap::new();
        let fake_id = Uuid::new_v4();

        let result = update_task_status_logic(&mut tasks, fake_id, TaskStatus::Queue);
        assert!(result.is_none());
    }

    #[test]
    fn test_status_in_progress_to_backlog_resets_phase_and_progress() {
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("Task 1", project_id, TaskStatus::InProgress, 0);
        task.phase = TaskPhase::Coding;
        task.phase_progress = 80;
        task.overall_progress = 50;
        let task_id = task.id;

        let mut tasks = create_test_tasks_map(vec![task]);

        let result = update_task_status_logic(&mut tasks, task_id, TaskStatus::Backlog);
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.status, TaskStatus::Backlog);
        assert_eq!(updated.phase, TaskPhase::Idle);
        assert_eq!(updated.phase_progress, 0);
        assert_eq!(updated.overall_progress, 0);
    }

    #[test]
    fn test_status_multiple_rapid_transitions() {
        let project_id = Uuid::new_v4();
        let task = create_test_task_full("Task 1", project_id, TaskStatus::Queue, 0);
        let task_id = task.id;
        let mut tasks = create_test_tasks_map(vec![task]);

        // Queue -> InProgress
        let result = update_task_status_logic(&mut tasks, task_id, TaskStatus::InProgress);
        assert!(result.is_some());
        assert_eq!(result.unwrap().status, TaskStatus::InProgress);

        // Simulate some progress
        if let Some(t) = tasks.get_mut(&task_id) {
            t.phase = TaskPhase::Coding;
            t.phase_progress = 50;
            t.error_message = Some("timeout".to_string());
        }

        // InProgress -> Error
        let result = update_task_status_logic(&mut tasks, task_id, TaskStatus::Error);
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.status, TaskStatus::Error);
        // Error_message preserved (old_status=InProgress, new_status=Error: no reset)
        assert_eq!(updated.error_message, Some("timeout".to_string()));

        // Error -> Queue (requeue)
        let result = update_task_status_logic(&mut tasks, task_id, TaskStatus::Queue);
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.status, TaskStatus::Queue);
        assert_eq!(updated.phase, TaskPhase::Idle);
        assert_eq!(updated.phase_progress, 0);
        assert!(updated.error_message.is_none());
    }

    // ===== add_external_ref / remove_external_ref tests =====

    fn make_github_issue_ref(number: u32) -> ExternalRef {
        ExternalRef::GithubIssue {
            url: format!("https://github.com/owner/repo/issues/{}", number),
            number,
            repo: "owner/repo".to_string(),
            state: Some("OPEN".to_string()),
        }
    }

    fn make_jira_ref(key: &str) -> ExternalRef {
        ExternalRef::JiraTicket {
            key: key.to_string(),
            project: "PROJ".to_string(),
        }
    }

    #[test]
    fn test_add_external_ref_to_empty_refs() {
        let project_id = Uuid::new_v4();
        let task = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let task_id = task.id;
        let mut tasks = create_test_tasks_map(vec![task]);

        let ext_ref = make_github_issue_ref(42);
        let result = add_external_ref_logic(&mut tasks, task_id, ext_ref);
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.external_refs.len(), 1);
        assert!(matches!(
            &updated.external_refs[0],
            ExternalRef::GithubIssue { number: 42, .. }
        ));
    }

    #[test]
    fn test_add_multiple_external_refs() {
        let project_id = Uuid::new_v4();
        let task = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let task_id = task.id;
        let mut tasks = create_test_tasks_map(vec![task]);

        add_external_ref_logic(&mut tasks, task_id, make_github_issue_ref(1));
        add_external_ref_logic(&mut tasks, task_id, make_jira_ref("PROJ-100"));
        let result = add_external_ref_logic(&mut tasks, task_id, make_github_issue_ref(3));

        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.external_refs.len(), 3);
        assert!(matches!(&updated.external_refs[0], ExternalRef::GithubIssue { number: 1, .. }));
        assert!(matches!(&updated.external_refs[1], ExternalRef::JiraTicket { .. }));
        assert!(matches!(&updated.external_refs[2], ExternalRef::GithubIssue { number: 3, .. }));
    }

    #[test]
    fn test_add_duplicate_external_ref_both_present() {
        let project_id = Uuid::new_v4();
        let task = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let task_id = task.id;
        let mut tasks = create_test_tasks_map(vec![task]);

        add_external_ref_logic(&mut tasks, task_id, make_github_issue_ref(42));
        let result = add_external_ref_logic(&mut tasks, task_id, make_github_issue_ref(42));

        assert!(result.is_some());
        let updated = result.unwrap();
        // No dedup: both present
        assert_eq!(updated.external_refs.len(), 2);
    }

    #[test]
    fn test_remove_external_ref_at_index_zero() {
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        task.external_refs = vec![make_github_issue_ref(1), make_jira_ref("PROJ-50"), make_github_issue_ref(3)];
        let task_id = task.id;
        let mut tasks = create_test_tasks_map(vec![task]);

        let result = remove_external_ref_logic(&mut tasks, task_id, 0);
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.external_refs.len(), 2);
        // First element should now be the Jira ref (shifted from index 1)
        assert!(matches!(&updated.external_refs[0], ExternalRef::JiraTicket { .. }));
        assert!(matches!(&updated.external_refs[1], ExternalRef::GithubIssue { number: 3, .. }));
    }

    #[test]
    fn test_remove_external_ref_out_of_bounds_no_change() {
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        task.external_refs = vec![make_github_issue_ref(1)];
        let task_id = task.id;
        let mut tasks = create_test_tasks_map(vec![task]);

        // Index 5 is out of bounds for a vec with 1 element
        let result = remove_external_ref_logic(&mut tasks, task_id, 5);
        assert!(result.is_some());
        let updated = result.unwrap();
        // No change: still 1 ref
        assert_eq!(updated.external_refs.len(), 1);
    }

    #[test]
    fn test_add_external_ref_nonexistent_task_returns_none() {
        let mut tasks: HashMap<Uuid, Task> = HashMap::new();
        let fake_id = Uuid::new_v4();
        let result = add_external_ref_logic(&mut tasks, fake_id, make_github_issue_ref(1));
        assert!(result.is_none());
    }

    #[test]
    fn test_remove_external_ref_nonexistent_task_returns_none() {
        let mut tasks: HashMap<Uuid, Task> = HashMap::new();
        let fake_id = Uuid::new_v4();
        let result = remove_external_ref_logic(&mut tasks, fake_id, 0);
        assert!(result.is_none());
    }

    // The delete_task tests that stood here exercised a `#[cfg(test)]` shim
    // that was a bare `HashMap::remove`. `delete_task` now routes through
    // `lifecycle::delete`, which takes the lease, refuses while an agent owns
    // the task, cleans the checkout up first and persists before publishing --
    // none of which the shim could see, so passing it proved nothing about the
    // command. That contract is pinned in `crate::lifecycle::tests` and, for
    // the daemon's front door, in `crate::ipc::handlers::tests`.

    // ===== Edge case: reorder with invalid task_id =====

    #[test]
    fn test_reorder_with_invalid_task_id_returns_none() {
        let project_id = Uuid::new_v4();
        let task = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let mut tasks = create_test_tasks_map(vec![task]);

        let fake_id = Uuid::new_v4();
        let result = reorder_task_logic(&mut tasks, fake_id, Some(TaskStatus::Queue), 0);
        assert!(result.is_none());
    }
}
