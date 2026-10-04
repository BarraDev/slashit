//! Orphaned Task Checkouts and task branches of one Project: a read-only
//! scan, and reclaims that each run only because a person asked for that one
//! item. See `worktree::orphans` for what is an orphan and what a reclaim
//! refuses.

use crate::worktree::{owners_in_repository, OrphanScan, Owners};
use uuid::Uuid;

/// The Project's repository path and what its tasks own.
async fn context(state: &crate::AppState, project_id: &str) -> Result<(String, Owners), String> {
    let project_id = Uuid::parse_str(project_id).map_err(|e| e.to_string())?;
    let repository_id = state
        .project
        .projects
        .read()
        .await
        .get(&project_id)
        .ok_or_else(|| format!("Project {project_id} not found"))?
        .repository_id
        .ok_or("The project has no repository folder linked")?;
    let repo_path = state
        .repository
        .repositories
        .read()
        .await
        .get(&repository_id)
        .map(|r| r.local_path.clone())
        .ok_or("The project's repository is not known")?;
    // A board that cannot be read hides its tasks, and what they own would
    // then look like nobody's.
    let unreadable = state
        .storage
        .unreadable_task_files()
        .map_err(|e| format!("The task boards could not be listed ({e}), so nothing is known to be an orphan."))?;
    if let Some(file) = unreadable.first() {
        return Err(format!(
            "The task board {} could not be read, so tasks may exist that SlashIt cannot see and \
             nothing is known to be an orphan. Repair or restore that file first.",
            file.display()
        ));
    }
    let owners = owners_in_repository(
        &state.task.tasks,
        &state.project.projects,
        &state.repository.repositories,
        &repo_path,
    )
    .await;
    Ok((repo_path, owners))
}

/// What no task owns, among the Project's managed checkouts and task
/// branches. Changes nothing.
#[tauri::command]
pub async fn scan_orphans(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<OrphanScan, String> {
    let (repo_path, owners) = context(&state, &project_id).await?;
    state.worktree_manager.scan_orphans(&repo_path, &owners).await
}

/// Remove one orphaned Task Checkout, never forcing. Its branch stays.
#[tauri::command]
pub async fn reclaim_orphan_checkout(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    path: String,
) -> Result<(), String> {
    let (repo_path, owners) = context(&state, &project_id).await?;
    state.worktree_manager.reclaim_orphan_checkout(&repo_path, &owners, &path).await
}

/// Delete one orphaned task branch whose commits another durable ref has.
#[tauri::command]
pub async fn reclaim_orphan_branch(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    branch: String,
) -> Result<(), String> {
    let (repo_path, owners) = context(&state, &project_id).await?;
    state.worktree_manager.reclaim_orphan_branch(&repo_path, &owners, &branch).await
}
