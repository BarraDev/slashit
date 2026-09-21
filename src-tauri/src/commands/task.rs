use crate::domain::{
    Task, TaskStatus, TaskCategory, TaskPriority, TaskComplexity,
    TaskImpact, SecuritySeverity, TaskPhase, Subtask, Project, Repository,
};
use crate::domain::task::ExternalRef;
use crate::config::Storage;
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Resolve a project's repository local path.
///
/// Used to give worktree cleanup (spawned after a task's status changes) the
/// `repo_path` that `WorktreeManager::remove` needs to run its git commands
/// in the right directory. Takes the project/repository maps directly so it
/// can be called from inside a `tauri::async_runtime::spawn`ed closure that
/// only cloned those two `Arc`s, not the whole `AppState`.
async fn resolve_repo_path(
    projects: &Arc<RwLock<HashMap<Uuid, Project>>>,
    repositories: &Arc<RwLock<HashMap<Uuid, Repository>>>,
    project_id: Uuid,
) -> Result<String, String> {
    let projects_r = projects.read().await;
    let project = projects_r.get(&project_id).ok_or("Project not found")?;
    let repo_id = project.repository_id.ok_or("No repository linked")?;
    drop(projects_r);

    let repos = repositories.read().await;
    let repo = repos.get(&repo_id).ok_or("Repository not found")?;
    Ok(repo.local_path.clone())
}

/// Resource handles [`cleanup_worktree`] needs, grouped so the function
/// itself only takes the values that vary per call.
struct WorktreeCleanupCtx<'a> {
    worktree_manager: &'a crate::worktree::WorktreeManager,
    projects: &'a Arc<RwLock<HashMap<Uuid, Project>>>,
    repositories: &'a Arc<RwLock<HashMap<Uuid, Repository>>>,
    tasks: &'a Tasks,
    storage: &'a Storage,
}

/// Core of [`spawn_worktree_cleanup`], split out so tests can `.await` it
/// directly instead of going through `tauri::async_runtime::spawn`.
///
/// `worktree_path` is the only persisted record of this directory. Every
/// call site used to clear it — followed immediately by a synchronous
/// persist — before the detached removal here even ran, so a crash or a
/// failed `git worktree remove` orphaned the directory with nothing left
/// pointing back to it. A `resolve_repo_path` or `WorktreeManager::remove`
/// failure is logged with the task id and path instead, and leaves
/// `worktree_path` untouched so the directory stays recoverable.
///
/// What that recovery is depends on the caller, so the failure messages below
/// deliberately do not promise a retry. A transition into `Done` is revisited
/// by the executor's pass, which filters on `status == Done`; `delete_task`
/// leaves no task at all, and there the retained branch is the only record of
/// what the task produced, which is one of the reasons removal never touches
/// it.
///
/// Re-queuing a task does not reach here at all — see
/// [`StatusTransitionEffect::ResetExecutionState`].
async fn cleanup_worktree(ctx: WorktreeCleanupCtx<'_>, task_id: Uuid, project_id: Uuid, wt_path: &str) {
    let repo_path = match resolve_repo_path(ctx.projects, ctx.repositories, project_id).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "Warning: worktree cleanup for task {task_id} could not resolve a repository path ({e}); {wt_path} is left on disk and this call cleared no reference to it."
            );
            return;
        }
    };

    if let Err(e) = ctx.worktree_manager.remove(wt_path, &repo_path).await {
        eprintln!(
            "Warning: failed to remove worktree {wt_path} for task {task_id}: {e}. This call cleared no reference to it."
        );
        return;
    }

    if let Err(e) = clear_worktree_path_durably(ctx.tasks, ctx.storage, task_id, wt_path).await {
        // Nothing here can retry: this path is reached from a status change or
        // a delete, neither of which runs again on its own, and the executor's
        // retry pass only looks at `Done` tasks. Leaving the reference in place
        // is still the right outcome — the next start reconciles it, and any
        // later save for this project rewrites the file — but the failure has
        // to be visible rather than swallowed.
        eprintln!("Warning: worktree cleanup for task {task_id}: {e}");
    }
}

/// Clear `task_id`'s `worktree_path`, but only once the cleared record is on
/// disk.
///
/// `worktree_path` is the only persisted record of a worktree, so publishing
/// the clear to shared memory before the write succeeds leaves the board
/// disagreeing with disk with nothing left to reconcile them: the executor's
/// retry pass selects on the *in-memory* value, so a task cleared in memory is
/// never revisited, and the stale file survives until some unrelated mutation
/// happens to rewrite it. This is the same persist-before-publish contract the
/// project and repository saves use.
///
/// The whole transaction runs under one write guard, so no sibling save can
/// land between the write and the in-memory commit.
///
/// `Err` means the write failed and the reference is still recorded in both
/// places, which is what keeps a `Done` task eligible for the retry pass.
pub(crate) async fn clear_worktree_path_durably(
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    wt_path: &str,
) -> Result<(), String> {
    let mut tasks_w = tasks.write().await;

    let Some(task) = tasks_w.get(&task_id) else {
        return Ok(()); // deleted while its worktree was being removed
    };
    if task.worktree_path.as_deref() != Some(wt_path) {
        // The task now records a different worktree, so a re-run recreated one
        // after this cleanup started. Clearing that reference would discard a
        // worktree this call never removed.
        return Ok(());
    }
    let project_id = task.project_id;

    let staged: Vec<Task> = tasks_w
        .values()
        .filter(|t| t.project_id == project_id)
        .map(|t| {
            let mut staged = t.clone();
            if staged.id == task_id {
                staged.worktree_path = None;
            }
            staged
        })
        .collect();

    storage
        .save_project_tasks(project_id, &staged)
        .map_err(|e| {
            format!("worktree {wt_path} was removed, but clearing it from the task board failed: {e}")
        })?;

    if let Some(t) = tasks_w.get_mut(&task_id) {
        t.worktree_path = None;
    }
    Ok(())
}

/// Remove a task's worktree in the background; see [`cleanup_worktree`] for
/// the retain-on-failure invariant this preserves.
fn spawn_worktree_cleanup(state: &crate::AppState, task_id: Uuid, project_id: Uuid, wt_path: String) {
    let wt_mgr = state.worktree_manager.clone();
    let projects = state.project.projects.clone();
    let repositories = state.repository.repositories.clone();
    let tasks = state.task.tasks.clone();
    let storage = state.storage.clone();

    tauri::async_runtime::spawn(async move {
        cleanup_worktree(
            WorktreeCleanupCtx {
                worktree_manager: &wt_mgr,
                projects: &projects,
                repositories: &repositories,
                tasks: &tasks,
                storage: &storage,
            },
            task_id,
            project_id,
            &wt_path,
        )
        .await;
    });
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

#[tauri::command]
pub async fn update_task_status(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    status: TaskStatus,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
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
            StatusTransitionEffect::CleanUpWorktree => {
                // Keep branch_name for PR creation. Removal leaves the
                // branch alone, so it still names something afterwards.
                if let Some(wt_path) = task.worktree_path.clone() {
                    spawn_worktree_cleanup(&state, task_id, task.project_id, wt_path);
                }
            }
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
    let mut tasks = state.task.tasks.write().await;

    // Get project_id before removal for persistence
    let project_id = tasks.get(&task_id).map(|t| t.project_id);

    // Cleanup the worktree before removing the task. The task record itself
    // is about to be deleted, so a failure here has no persisted task left to
    // retain the path on — the failure is logged so the orphaned directory
    // is at least discoverable, per `spawn_worktree_cleanup`'s own logging.
    // The branch survives either way, so the commits the task produced stay
    // reachable even when the record that named them does not.
    if let Some(task) = tasks.get(&task_id) {
        if let Some(wt_path) = task.worktree_path.clone() {
            spawn_worktree_cleanup(&state, task_id, task.project_id, wt_path);
        }
    }

    let removed = tasks.remove(&task_id).is_some();
    
    // Persist to disk if task was removed
    if removed {
        if let Some(pid) = project_id {
            persist_project_tasks(&state.storage, &tasks, pid);
        }
    }
    
    Ok(removed)
}

/// Reorder a task within its column or when moving to a new column
/// new_position is the target position index in the destination column
#[tauri::command]
pub async fn reorder_task(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    new_status: Option<TaskStatus>,
    new_position: i32,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    // Get the task and its current/new status
    let (project_id, old_status, target_status) = {
        if let Some(task) = tasks.get(&task_id) {
            let target = new_status.clone().unwrap_or(task.status.clone());
            (task.project_id, task.status.clone(), target)
        } else {
            return Ok(None);
        }
    };

    // Collect task IDs in the target column, sorted by position
    let mut column_tasks: Vec<(Uuid, i32)> = tasks
        .values()
        .filter(|t| t.project_id == project_id && t.status == target_status && t.id != task_id)
        .map(|t| (t.id, t.position))
        .collect();
    column_tasks.sort_by_key(|(_, pos)| *pos);

    // Insert the moved task at the new position and recalculate positions
    let clamped_position = new_position.max(0).min(column_tasks.len() as i32) as usize;
    column_tasks.insert(clamped_position, (task_id, 0)); // position will be recalculated

    // Update positions for all tasks in the column
    for (idx, (tid, _)) in column_tasks.iter().enumerate() {
        if let Some(task) = tasks.get_mut(tid) {
            task.position = idx as i32;
            task.updated_at = chrono::Utc::now();
        }
    }

    // Update the moved task's status if it changed
    if let Some(task) = tasks.get_mut(&task_id) {
        if old_status != target_status {
            task.status = target_status.clone();

            // Dragging a card is the same lifecycle decision as calling
            // `update_task_status`, and goes through the same classifier so the
            // two cannot diverge.
            match classify_status_transition(&old_status, &target_status) {
                StatusTransitionEffect::ResetExecutionState => {
                    // Worktree and branch preserved; see
                    // `StatusTransitionEffect`.
                    task.reset_execution_state();
                }
                StatusTransitionEffect::CleanUpWorktree => {
                    // Keep branch_name for PR creation. Removal leaves the
                    // branch alone, so it still names something afterwards.
                    if let Some(wt_path) = task.worktree_path.clone() {
                        spawn_worktree_cleanup(&state, task_id, task.project_id, wt_path);
                    }
                }
                StatusTransitionEffect::None => {}
            }
        }
        task.updated_at = chrono::Utc::now();
    }

    let updated_task = tasks.get(&task_id).cloned();

    // Persist to disk
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
/// once [`cleanup_worktree`]'s actual removal succeeds, and this helper never
/// runs any removal. Clearing it here would model a cleanup that never
/// happened, exactly the bug this file's worktree-lifecycle fix closed.
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

/// Core delete_task logic extracted for testability
#[cfg(test)]
pub fn delete_task_logic(
    tasks: &mut HashMap<Uuid, Task>,
    task_id: Uuid,
) -> bool {
    tasks.remove(&task_id).is_some()
}

/// Core reorder logic extracted for testability
/// Returns the updated tasks HashMap after reordering
///
/// Dragging a card and calling `update_task_status` are the same lifecycle
/// decision, so this applies the transition effect through the same
/// [`classify_status_transition`] the production `reorder_task` uses — the two
/// entry points cannot be made to disagree about when execution state resets.
/// Like [`update_task_status_logic`], it runs no worktree removal and so
/// leaves `worktree_path` alone.
#[cfg(test)]
pub fn reorder_task_logic(
    tasks: &mut HashMap<Uuid, Task>,
    task_id: Uuid,
    new_status: Option<TaskStatus>,
    new_position: i32,
) -> Option<Task> {
    // Get the task and its current/new status
    let (project_id, old_status, target_status) = {
        let task = tasks.get(&task_id)?;
        let target = new_status.clone().unwrap_or(task.status.clone());
        (task.project_id, task.status.clone(), target)
    };

    // Collect task IDs in the target column, sorted by position
    let mut column_tasks: Vec<(Uuid, i32)> = tasks
        .values()
        .filter(|t| t.project_id == project_id && t.status == target_status && t.id != task_id)
        .map(|t| (t.id, t.position))
        .collect();
    column_tasks.sort_by_key(|(_, pos)| *pos);

    // Insert the moved task at the new position and recalculate positions
    let clamped_position = new_position.max(0).min(column_tasks.len() as i32) as usize;
    column_tasks.insert(clamped_position, (task_id, 0)); // position will be recalculated

    // Update positions for all tasks in the column
    for (idx, (tid, _)) in column_tasks.iter().enumerate() {
        if let Some(task) = tasks.get_mut(tid) {
            task.position = idx as i32;
            task.updated_at = chrono::Utc::now();
        }
    }

    // Update the moved task's status if it changed
    if let Some(task) = tasks.get_mut(&task_id) {
        if old_status != target_status {
            task.status = target_status.clone();

            if let StatusTransitionEffect::ResetExecutionState =
                classify_status_transition(&old_status, &target_status)
            {
                task.reset_execution_state();
            }
        }
        task.updated_at = chrono::Utc::now();
    }

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

    fn test_worktree_manager() -> crate::worktree::WorktreeManager {
        let root = std::env::temp_dir().join(format!("slashit-task-wt-test-{}", Uuid::new_v4()));
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            root.join("config"),
            root.join("data"),
            root.join("cache"),
            root.join("runtime"),
        ));
        // `Managed` forces the git-native path regardless of whether `wt`
        // happens to be installed on the machine running this test.
        crate::worktree::WorktreeManager::new(paths, crate::config::paths::WorktreePlacement::Managed)
    }

    fn test_storage() -> (Storage, tempfile::TempDir) {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        let storage = Storage::with_paths(crate::config::paths::AppPaths::with_roots(
            root.join("config"),
            root.join("data"),
            root.join("cache"),
            root.join("runtime"),
        ));
        (storage, temp)
    }

    fn create_temp_git_repo() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(tmp.path())
                .output()
                .expect("git command failed to spawn")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(tmp.path().join("f.txt"), "x").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        tmp
    }

    fn test_project(id: Uuid, repository_id: Option<Uuid>) -> Project {
        Project {
            id,
            name: "test-project".to_string(),
            repository_id,
            scope: crate::domain::ProjectScope::Standalone,
            state_location: crate::config::paths::StateLocation::External,
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
        }
    }

    fn test_repository(id: Uuid, local_path: String) -> Repository {
        Repository {
            id,
            local_path,
            remote_url: None,
            remote_type: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn cleanup_worktree_clears_worktree_path_only_after_successful_removal() {
        let repo = create_temp_git_repo();
        let repo_path = repo.path().to_str().unwrap().to_string();
        let wt_mgr = test_worktree_manager();
        let info = wt_mgr
            .create(&repo_path, "cleanup-success")
            .await
            .expect("create failed");

        let project_id = Uuid::new_v4();
        let repo_id = Uuid::new_v4();
        let projects = Arc::new(RwLock::new(HashMap::from([(
            project_id,
            test_project(project_id, Some(repo_id)),
        )])));
        let repositories = Arc::new(RwLock::new(HashMap::from([(
            repo_id,
            test_repository(repo_id, repo_path),
        )])));

        let mut task = create_test_task_full("t", project_id, TaskStatus::Done, 0);
        task.worktree_path = Some(info.path.clone());
        let task_id = task.id;
        let tasks: Tasks = Arc::new(RwLock::new(create_test_tasks_map(vec![task])));
        let (storage, _tmp) = test_storage();

        cleanup_worktree(
            WorktreeCleanupCtx {
                worktree_manager: &wt_mgr,
                projects: &projects,
                repositories: &repositories,
                tasks: &tasks,
                storage: &storage,
            },
            task_id,
            project_id,
            &info.path,
        )
        .await;

        let tasks_r = tasks.read().await;
        assert_eq!(
            tasks_r.get(&task_id).unwrap().worktree_path, None,
            "worktree_path must be cleared once removal actually succeeded"
        );
        assert!(
            !std::path::Path::new(&info.path).exists(),
            "the worktree directory itself must be gone"
        );
    }

    #[tokio::test]
    async fn cleanup_worktree_retains_worktree_path_when_the_board_cannot_be_saved() {
        // This is the status-change and delete path, not the executor's, and
        // it has no retry pass of its own — so publishing the clear to memory
        // while the write failed would leave the board and disk disagreeing
        // with nothing left to reconcile them until the next start.
        let repo = create_temp_git_repo();
        let repo_path = repo.path().to_str().unwrap().to_string();
        let wt_mgr = test_worktree_manager();
        let info = wt_mgr
            .create(&repo_path, "cleanup-unsaveable")
            .await
            .expect("create failed");

        let project_id = Uuid::new_v4();
        let repo_id = Uuid::new_v4();
        let projects = Arc::new(RwLock::new(HashMap::from([(
            project_id,
            test_project(project_id, Some(repo_id)),
        )])));
        let repositories = Arc::new(RwLock::new(HashMap::from([(
            repo_id,
            test_repository(repo_id, repo_path),
        )])));

        let mut task = create_test_task_full("t", project_id, TaskStatus::Done, 0);
        task.worktree_path = Some(info.path.clone());
        let task_id = task.id;
        let tasks: Tasks = Arc::new(RwLock::new(create_test_tasks_map(vec![task])));
        let (storage, _tmp) = test_storage();

        // A regular file where the tasks directory has to be makes the atomic
        // write fail with `NotADirectory`. No permission bits, so this behaves
        // the same for every user including root.
        std::fs::write(
            storage.paths().config_dir().join("tasks"),
            b"not a directory",
        )
        .expect("place persistence blocker");

        cleanup_worktree(
            WorktreeCleanupCtx {
                worktree_manager: &wt_mgr,
                projects: &projects,
                repositories: &repositories,
                tasks: &tasks,
                storage: &storage,
            },
            task_id,
            project_id,
            &info.path,
        )
        .await;

        assert_eq!(
            tasks.read().await.get(&task_id).unwrap().worktree_path.as_deref(),
            Some(info.path.as_str()),
            "the reference must survive in memory when it could not be persisted"
        );
    }

    /// Make every `save_project_tasks` fail deterministically by putting a
    /// regular file where the tasks directory has to be, so the `create_dir_all`
    /// inside the atomic write fails with `AlreadyExists`. No permission bits,
    /// so this behaves identically for every user including root.
    fn block_task_persistence(storage: &Storage) {
        let blocker = storage.paths().config_dir().join("tasks");
        let _ = std::fs::remove_dir_all(&blocker);
        std::fs::write(&blocker, b"not a directory").expect("place persistence blocker");
    }

    /// A task holding `wt_path`, plus a sibling in the same project that the
    /// whole-file rewrite must not drop.
    fn durable_clear_fixture(project_id: Uuid, wt_path: &str) -> (Uuid, Uuid, Tasks) {
        let mut subject = create_test_task_full("subject", project_id, TaskStatus::Done, 0);
        subject.worktree_path = Some(wt_path.to_string());
        let sibling = create_test_task_full("sibling", project_id, TaskStatus::Backlog, 1);
        let (subject_id, sibling_id) = (subject.id, sibling.id);
        (
            subject_id,
            sibling_id,
            Arc::new(RwLock::new(create_test_tasks_map(vec![subject, sibling]))),
        )
    }

    #[tokio::test]
    async fn clear_worktree_path_durably_cannot_lose_a_concurrent_update_to_a_sibling_task() {
        // Regression guard for the stale-snapshot race, carried over from the
        // daemon branch's own independent fix for this same defect. Both
        // branches fixed it; this shared helper is the implementation that
        // survived, because all three cleanup call sites use it, but that
        // branch's concurrency proof was the stronger of the two and is kept
        // here rather than dropped with the commit it came from.
        //
        // The defect: read the task map, build a whole-project snapshot,
        // release the guard, and only then persist. A concurrent write to a
        // *different* task in the same project that reached disk during that
        // gap would be silently reverted by the now-stale snapshot, because
        // `save_project_tasks` rewrites the entire project file.
        //
        // Proven deterministically rather than by timing luck.
        // `tokio::sync::RwLock` is task-fair: waiters are granted the lock in
        // the order they enqueued. `clear_worktree_path_durably` holds a
        // single *write* guard continuously from the `worktree_path` check
        // through the synchronous save that follows — strictly stronger than
        // the read guard the original proof assumed. Taking a write guard here
        // first blocks both competing operations; letting the cleanup enqueue
        // before the sibling writer does means fairness guarantees the writer
        // cannot persist until cleanup's save has already completed.
        let project_id = Uuid::new_v4();
        let wt_path = "/tmp/slashit-test-concurrent-cleanup";
        let (task_id, sibling_id, tasks) = durable_clear_fixture(project_id, wt_path);
        let (storage, _tmp) = test_storage();

        // Block every reader and writer until both competing operations are
        // confirmed enqueued, in the intended FIFO order.
        let blocker = tasks.write().await;

        let cleanup_tasks = tasks.clone();
        let cleanup_storage = storage.clone();
        let cleanup_handle = tokio::spawn(async move {
            clear_worktree_path_durably(&cleanup_tasks, &cleanup_storage, task_id, wt_path).await
        });
        // Let the executor poll `cleanup_handle` at least once, so its
        // `tasks.write()` request registers in the lock's wait queue before
        // the sibling writer's does below.
        tokio::task::yield_now().await;

        let writer_tasks = tasks.clone();
        let writer_storage = storage.clone();
        let writer_handle = tokio::spawn(async move {
            let mut w = writer_tasks.write().await;
            w.get_mut(&sibling_id).unwrap().title = "renamed while cleanup was pending".to_string();
            let snapshot: Vec<Task> = w
                .values()
                .filter(|t| t.project_id == project_id)
                .cloned()
                .collect();
            drop(w);
            writer_storage
                .save_project_tasks(project_id, &snapshot)
                .unwrap();
        });
        tokio::task::yield_now().await;

        // Both are queued behind this guard now, cleanup ahead of the writer.
        drop(blocker);

        cleanup_handle
            .await
            .expect("cleanup task panicked")
            .expect("cleanup should succeed");
        writer_handle.await.expect("writer task panicked");

        let persisted = storage
            .load_project_tasks(project_id)
            .expect("tasks must be readable from disk");
        let sibling = persisted
            .iter()
            .find(|t| t.id == sibling_id)
            .expect("sibling must still be on disk");
        assert_eq!(
            sibling.title, "renamed while cleanup was pending",
            "a concurrent update to a sibling task must survive the cleanup save; a stale \
             whole-project snapshot would have reverted it"
        );

        let subject = persisted
            .iter()
            .find(|t| t.id == task_id)
            .expect("subject must still be on disk");
        assert_eq!(
            subject.worktree_path, None,
            "the cleanup's own clear must also have reached disk"
        );
    }

    #[tokio::test]
    async fn clear_worktree_path_durably_reports_a_failed_write_and_retains_the_path() {
        // The contract `commands::worktree::cleanup_worktree` now depends on.
        // That command is reachable for any status, while the executor's retry
        // pass only revisits `Done` tasks, so this `Err` is the entire recovery
        // story there: it is what lets the dialog tell the user to try again.
        let project_id = Uuid::new_v4();
        let (task_id, _sibling, tasks) = durable_clear_fixture(project_id, "/tmp/wt/task-aaaa1111");
        let (storage, _tmp) = test_storage();
        block_task_persistence(&storage);

        let result =
            clear_worktree_path_durably(&tasks, &storage, task_id, "/tmp/wt/task-aaaa1111").await;

        assert!(result.is_err(), "a failed write must be reported, not swallowed");
        assert_eq!(
            tasks.read().await.get(&task_id).unwrap().worktree_path.as_deref(),
            Some("/tmp/wt/task-aaaa1111"),
            "the reference must survive in memory so the retry pass can still see it"
        );
    }

    #[tokio::test]
    async fn clear_worktree_path_durably_declines_to_clear_a_newer_worktree_path() {
        // `commands::worktree::cleanup_worktree` used to clear
        // `worktree_path` unconditionally, so a re-run that recreated a
        // worktree while the removal was in flight had the reference to its
        // *new* worktree erased by a cleanup that never touched it.
        let project_id = Uuid::new_v4();
        let (task_id, _sibling, tasks) = durable_clear_fixture(project_id, "/tmp/wt/new-worktree");
        let (storage, _tmp) = test_storage();

        clear_worktree_path_durably(&tasks, &storage, task_id, "/tmp/wt/old-worktree")
            .await
            .expect("a stale cleanup is not an error, it simply clears nothing");

        assert_eq!(
            tasks.read().await.get(&task_id).unwrap().worktree_path.as_deref(),
            Some("/tmp/wt/new-worktree"),
            "the replacement worktree must keep its reference"
        );
    }

    #[tokio::test]
    async fn clear_worktree_path_durably_keeps_sibling_tasks() {
        let project_id = Uuid::new_v4();
        let (task_id, sibling_id, tasks) = durable_clear_fixture(project_id, "/tmp/wt/x");
        let (storage, _tmp) = test_storage();

        clear_worktree_path_durably(&tasks, &storage, task_id, "/tmp/wt/x")
            .await
            .expect("save should succeed");

        let persisted = storage.load_project_tasks(project_id).expect("reload tasks");
        assert_eq!(persisted.len(), 2, "the sibling must still be on disk");
        assert!(
            persisted.iter().find(|t| t.id == task_id).unwrap().worktree_path.is_none(),
            "the cleared path must be the one on disk"
        );
        assert_eq!(
            persisted.iter().find(|t| t.id == sibling_id).unwrap().title,
            "sibling",
            "the sibling must survive the whole-file rewrite intact"
        );
    }

    #[tokio::test]
    async fn cleanup_worktree_retains_worktree_path_when_repo_path_cannot_be_resolved() {
        let wt_mgr = test_worktree_manager();
        // `project_id` deliberately has no entry in `projects` below, so
        // `resolve_repo_path` fails before any git command ever runs.
        let project_id = Uuid::new_v4();
        let projects = Arc::new(RwLock::new(HashMap::new()));
        let repositories = Arc::new(RwLock::new(HashMap::new()));

        let mut task = create_test_task_full("t", project_id, TaskStatus::Done, 0);
        task.worktree_path = Some("/tmp/does-not-matter".to_string());
        let task_id = task.id;
        let tasks: Tasks = Arc::new(RwLock::new(create_test_tasks_map(vec![task])));
        let (storage, _tmp) = test_storage();

        cleanup_worktree(
            WorktreeCleanupCtx {
                worktree_manager: &wt_mgr,
                projects: &projects,
                repositories: &repositories,
                tasks: &tasks,
                storage: &storage,
            },
            task_id,
            project_id,
            "/tmp/does-not-matter",
        )
        .await;

        let tasks_r = tasks.read().await;
        assert_eq!(
            tasks_r.get(&task_id).unwrap().worktree_path.as_deref(),
            Some("/tmp/does-not-matter"),
            "an unresolved repo path must leave worktree_path untouched, not clear it"
        );
    }

    #[tokio::test]
    async fn cleanup_worktree_retains_worktree_path_when_removal_fails() {
        let wt_mgr = test_worktree_manager();
        let project_id = Uuid::new_v4();
        let repo_id = Uuid::new_v4();
        // A repo path that does not exist on disk makes every git invocation
        // in `remove_with_git` fail to spawn (its `current_dir` is invalid),
        // giving a real, deterministic removal failure to test against.
        let bogus_repo_path = "/definitely/does/not/exist/slashit-test-xyz".to_string();

        let projects = Arc::new(RwLock::new(HashMap::from([(
            project_id,
            test_project(project_id, Some(repo_id)),
        )])));
        let repositories = Arc::new(RwLock::new(HashMap::from([(
            repo_id,
            test_repository(repo_id, bogus_repo_path),
        )])));

        let mut task = create_test_task_full("t", project_id, TaskStatus::Done, 0);
        task.worktree_path = Some("/tmp/some-worktree-path".to_string());
        let task_id = task.id;
        let tasks: Tasks = Arc::new(RwLock::new(create_test_tasks_map(vec![task])));
        let (storage, _tmp) = test_storage();

        cleanup_worktree(
            WorktreeCleanupCtx {
                worktree_manager: &wt_mgr,
                projects: &projects,
                repositories: &repositories,
                tasks: &tasks,
                storage: &storage,
            },
            task_id,
            project_id,
            "/tmp/some-worktree-path",
        )
        .await;

        let tasks_r = tasks.read().await;
        assert_eq!(
            tasks_r.get(&task_id).unwrap().worktree_path.as_deref(),
            Some("/tmp/some-worktree-path"),
            "a failed removal must leave worktree_path in place for retry"
        );
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
        // Done. No other transition may reach `spawn_worktree_cleanup`, and
        // each pair yields exactly one effect, so no call site can spawn
        // cleanup twice by matching two overlapping conditions.
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
        // `worktree_path` once `cleanup_worktree`'s actual removal succeeds.
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

    // ===== delete_task tests =====

    #[test]
    fn test_delete_existing_task_returns_true_and_removes() {
        let project_id = Uuid::new_v4();
        let task = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let task_id = task.id;
        let mut tasks = create_test_tasks_map(vec![task]);

        let removed = delete_task_logic(&mut tasks, task_id);
        assert!(removed);
        assert!(!tasks.contains_key(&task_id));
    }

    #[test]
    fn test_delete_nonexistent_task_returns_false() {
        let mut tasks: HashMap<Uuid, Task> = HashMap::new();
        let fake_id = Uuid::new_v4();
        let removed = delete_task_logic(&mut tasks, fake_id);
        assert!(!removed);
    }

    #[test]
    fn test_delete_task_does_not_affect_other_tasks() {
        let project_id = Uuid::new_v4();
        let task1 = create_test_task_full("Task 1", project_id, TaskStatus::Backlog, 0);
        let task2 = create_test_task_full("Task 2", project_id, TaskStatus::Queue, 0);
        let task1_id = task1.id;
        let task2_id = task2.id;
        let mut tasks = create_test_tasks_map(vec![task1, task2]);

        let removed = delete_task_logic(&mut tasks, task1_id);
        assert!(removed);
        assert!(!tasks.contains_key(&task1_id));
        assert!(tasks.contains_key(&task2_id));
        assert_eq!(tasks.len(), 1);
    }

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
