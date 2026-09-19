use uuid::Uuid;
use crate::worktree::WorktreeManager;

#[tauri::command]
pub async fn create_worktree(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<String, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;

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

#[tauri::command]
pub async fn cleanup_worktree(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<(), String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;

    let (wt_path, branch, project_id) = {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).ok_or("Task not found")?;
        (
            task.worktree_path.clone().ok_or("No worktree for this task")?,
            task.branch_name.clone().unwrap_or_default(),
            task.project_id,
        )
    };

    let repo_path = {
        let projects = state.project.projects.read().await;
        let project = projects.get(&project_id).ok_or("Project not found")?;
        let repo_id = project.repository_id.ok_or("No repository linked")?;
        drop(projects);

        let repos = state.repository.repositories.read().await;
        let repo = repos.get(&repo_id).ok_or("Repository not found")?;
        repo.local_path.clone()
    };

    state.worktree_manager.remove(&wt_path, &branch, &repo_path).await?;

    // The clear has to reach disk before it reaches shared memory, and the
    // failure has to reach the caller. Clearing `worktree_path` in memory is
    // what removes a task from `tasks_eligible_for_cleanup_retry`, which
    // selects on the in-memory value, so a clear published over a failed write
    // takes the task out of the only pass that would have reconciled it. This
    // command is also not restricted to `Done` tasks, which that pass filters
    // on, so returning the error to the dialog is the whole recovery story
    // here: the user can press the button again.
    //
    // `branch_name` stays for a later PR creation, as before. The helper also
    // declines to clear a path the task no longer records, so a re-run that
    // recreated a worktree while this removal was in flight keeps its
    // reference instead of having it erased by a cleanup that never touched
    // it.
    crate::commands::task::clear_worktree_path_durably(
        &state.task.tasks,
        &state.storage,
        task_id,
        &wt_path,
    )
    .await?;

    Ok(())
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
