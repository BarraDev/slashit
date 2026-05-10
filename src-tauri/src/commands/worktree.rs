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

    let (wt_path, branch) = {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).ok_or("Task not found")?;
        (
            task.worktree_path.clone().ok_or("No worktree for this task")?,
            task.branch_name.clone().unwrap_or_default(),
        )
    };

    state.worktree_manager.remove(&wt_path, &branch).await?;

    // Clear task fields
    {
        let mut tasks = state.task.tasks.write().await;
        if let Some(task) = tasks.get_mut(&task_id) {
            task.worktree_path = None;
            // Keep branch_name for potential PR creation
            task.updated_at = chrono::Utc::now();

            let project_id = task.project_id;
            let project_tasks: Vec<_> = tasks.values()
                .filter(|t| t.project_id == project_id)
                .cloned()
                .collect();
            let _ = state.storage.save_project_tasks(project_id, &project_tasks);
        }
    }

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
