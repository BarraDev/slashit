use uuid::Uuid;
use crate::worktree::WorktreeManager;

#[tauri::command]
pub async fn create_worktree(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<String, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;

    // Acquiring a worktree for a task is a lifecycle ownership change, so it
    // waits behind any cleanup, terminalization or delete already running for
    // the same task. Without this, a create could hand back the very directory
    // a removal is midway through deleting.
    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    // A cleanup that a previous process never finished leaves the recorded
    // checkout untrustworthy: nothing on disk says how much of it a dead
    // `git worktree remove` already took apart. Reattaching to it would hand an
    // agent a half-removed directory. See `Task::cleanup_in_flight`.
    {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).ok_or("Task not found")?;
        if task.cleanup_in_flight {
            return Err(format!(
                "task {task_id} has a worktree cleanup that was interrupted and not yet \
                 resolved; it needs attention before a worktree can be attached again"
            ));
        }
    }

    // Resolve repo path
    let repo_path = {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).ok_or("Task not found")?;
        let project_id = task.project_id;
        drop(tasks);

        let projects = state.project.projects.read().await;
        let project = projects.get(&project_id).ok_or("Project not found")?;
        let repo_id = project.repository_id.ok_or("No repository linked")?;
        drop(projects);

        let repos = state.repository.repositories.read().await;
        let repo = repos.get(&repo_id).ok_or("Repository not found")?;
        repo.local_path.clone()
    };

    // Check if task already has a branch (reattach) or needs a new one
    let existing_branch = {
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_id).and_then(|t| t.branch_name.clone())
    };

    let branch_name = existing_branch.clone().unwrap_or_else(|| WorktreeManager::branch_for_task(task_id));

    let info = if existing_branch.is_some() {
        state.worktree_manager.reattach(&repo_path, &branch_name).await?
    } else {
        state.worktree_manager.create(&repo_path, &branch_name).await?
    };

    // Update task
    {
        let mut tasks = state.task.tasks.write().await;
        if let Some(task) = tasks.get_mut(&task_id) {
            task.worktree_path = Some(info.path.clone());
            task.branch_name = Some(info.branch.clone());
            task.updated_at = chrono::Utc::now();

            // Persist
            let project_id = task.project_id;
            let project_tasks: Vec<_> = tasks.values()
                .filter(|t| t.project_id == project_id)
                .cloned()
                .collect();
            let _ = state.storage.save_project_tasks(project_id, &project_tasks);
        }
    }

    Ok(info.path)
}

/// Remove a task's worktree at the user's explicit request, without saying
/// anything about whether the task is finished.
///
/// Shares [`crate::lifecycle::terminalize_leased`] with the terminal path, so
/// the destructive step, its durable interrupted-cleanup record and its
/// persist-before-publish clearing are the same code. `desired` is the task's
/// current status precisely because this is not a terminal transition: a user
/// discarding a checkout has not said the work is over.
///
/// A refusal reaches the dialog with git's own reason, and the worktree, the
/// branch and everything uncommitted inside survive. The user can press the
/// button again once they have dealt with whatever git objected to.
#[tauri::command]
pub async fn cleanup_worktree(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<(), String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;

    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    let current_status = {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).ok_or("Task not found")?;
        if task.worktree_path.is_none() {
            return Err("No worktree for this task".to_string());
        }
        task.status.clone()
    };

    crate::lifecycle::terminalize_leased(
        crate::commands::task::terminalize_ctx(&state),
        task_id,
        crate::lifecycle::Origin::User,
        crate::lifecycle::TerminalizeRequest::new(current_status),
    )
    .await
    .map(|_| ())
    .map_err(|refusal| refusal.to_string())
}

#[tauri::command]
pub async fn check_worktree_exists(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<bool, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let tasks = state.task.tasks.read().await;

    if let Some(task) = tasks.get(&task_id) {
        if let Some(ref wt_path) = task.worktree_path {
            return Ok(state.worktree_manager.exists(wt_path));
        }
    }

    Ok(false)
}
