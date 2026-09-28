//! A project's repository setup: whether it can run tasks, and the explicit
//! actions that make it able to.
//!
//! Every mutation here runs only because a person asked for it in the
//! project's repository settings (or in the Create Project form): choosing
//! the project's base branch, asking origin for its default branch, and
//! putting a folder under version control. None of them runs when a folder
//! is opened or a task is started. See `worktree::readiness` for what is
//! reported.

use crate::domain::{Project, ProjectBase};
use crate::worktree::readiness::{readiness, Readiness};
use crate::worktree::vcs_init::{self, InitPreview, VcsInitKind};
use crate::worktree::{project_base, remote_head};
use uuid::Uuid;

/// The project `project_id`, and its repository's local path.
async fn project_and_path(
    state: &crate::AppState,
    project_id: Uuid,
) -> Result<(Project, String), String> {
    let project = state
        .project
        .projects
        .read()
        .await
        .get(&project_id)
        .cloned()
        .ok_or_else(|| format!("Project {project_id} not found"))?;
    let repository_id = project
        .repository_id
        .ok_or_else(|| format!("Project {} has no repository folder linked", project.name))?;
    let path = state
        .repository
        .repositories
        .read()
        .await
        .get(&repository_id)
        .map(|r| r.local_path.clone())
        .ok_or_else(|| format!("The repository of project {} is not known", project.name))?;
    Ok((project, path))
}

/// Record `base` as the project's local base, durably before memory: a
/// failed write leaves both as they were.
pub(crate) async fn record_project_base(
    state: &crate::AppState,
    project_id: Uuid,
    base: Option<ProjectBase>,
) -> Result<(), String> {
    let mut projects = state.project.projects.write().await;
    let mut proposed = projects.clone();
    let project = proposed
        .get_mut(&project_id)
        .ok_or_else(|| format!("Project {project_id} not found"))?;
    project.base = base;
    project.updated_at = chrono::Utc::now();
    crate::commands::project::try_persist_projects(&state.storage, &proposed)
        .map_err(|e| format!("Failed to save the project's base branch: {e}"))?;
    *projects = proposed;
    Ok(())
}

async fn report(state: &crate::AppState, project_id: Uuid) -> Result<Readiness, String> {
    let (project, path) = project_and_path(state, project_id).await?;
    readiness(&path, project.base.as_ref()).await
}

/// Whether the project can run tasks and deliver them, and why not.
#[tauri::command]
pub async fn get_project_readiness(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<Readiness, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    report(&state, project_id).await
}

/// Make the local branch `branch` the project's base, on explicit request.
#[tauri::command]
pub async fn set_project_base(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    branch: String,
) -> Result<Readiness, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    set_project_base_core(&state, project_id, &branch).await
}

pub(crate) async fn set_project_base_core(
    state: &crate::AppState,
    project_id: Uuid,
    branch: &str,
) -> Result<Readiness, String> {
    let (_, path) = project_and_path(state, project_id).await?;
    let base = project_base::validate_choice(&path, branch).await?;
    record_project_base(state, project_id, Some(base)).await?;
    report(state, project_id).await
}

/// Ask origin for its default branch and record it locally
/// (`git remote set-head origin --auto`), on explicit request.
#[tauri::command]
pub async fn detect_remote_default_branch(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<Readiness, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    detect_remote_default_branch_core(&state, project_id).await
}

pub(crate) async fn detect_remote_default_branch_core(
    state: &crate::AppState,
    project_id: Uuid,
) -> Result<Readiness, String> {
    let (_, path) = project_and_path(state, project_id).await?;
    remote_head::detect_default_branch(&path).await?;
    report(state, project_id).await
}

/// What initializing version control in `path` would do. Writes nothing.
#[tauri::command]
pub async fn preview_vcs_initialization(path: String) -> Result<InitPreview, String> {
    vcs_init::preview(&path).await
}

/// Put the project's folder under version control with a first commit (or
/// give its commit-less Git repository one), then capture the branch the
/// tool created as the project's base. On explicit request only.
#[tauri::command]
pub async fn initialize_project_vcs(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    kind: VcsInitKind,
) -> Result<Readiness, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    initialize_project_vcs_core(&state, project_id, kind).await
}

pub(crate) async fn initialize_project_vcs_core(
    state: &crate::AppState,
    project_id: Uuid,
    kind: VcsInitKind,
) -> Result<Readiness, String> {
    let (_, path) = project_and_path(state, project_id).await?;
    let outcome = vcs_init::initialize(&path, kind).await?;
    record_project_base(
        state,
        project_id,
        Some(ProjectBase::LocalBranch {
            branch: outcome.branch,
        }),
    )
    .await
    .map_err(|e| format!("Version control was initialized in {path}, but {e}"))?;
    report(state, project_id).await
}

/// The base a newly registered project starts with: its repository's
/// unambiguous base branch, if it has one (see `worktree::project_base`).
pub(crate) async fn captured_base(repository_path: Option<&str>) -> Option<ProjectBase> {
    let path = repository_path?;
    project_base::propose(path).await.ok()?.into_base()
}
