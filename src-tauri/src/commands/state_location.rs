//! Commands for inspecting and changing where a project's state is stored.
//!
//! The user-facing contract is narrow on purpose: a project's *shareable*
//! state (its board) can live outside the project or inside it, and moving
//! between the two is an explicit, previewable action. Secrets, runtime files
//! and machine-local caches are not part of that choice and are never offered
//! — see [`crate::config::paths`] for the classification.

use crate::config::migration::{ConflictPolicy, MigrationPlan, MigrationReport, StateMigrator};
use crate::config::paths::{ProjectKey, ResolvedLocation, StateLocation};
use crate::domain::Project;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Everything the settings UI needs to describe a project's storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateLocationInfo {
    pub project_id: String,
    /// The stored preference, including `Auto`.
    pub location: StateLocation,
    /// What `Auto` currently resolves to. Shown so the user is never guessing.
    pub resolved: ResolvedLocation,
    pub project_root: String,
    /// Where state is right now.
    pub current_dir: String,
    pub external_dir: String,
    pub in_project_dir: String,
    /// False when the project has no repository, so there is no path to key
    /// storage off and the choice cannot be offered.
    pub can_choose: bool,
}

/// Resolve the project and its repository root, or explain why we cannot.
async fn project_root(
    state: &tauri::State<'_, crate::AppState>,
    project_id: Uuid,
) -> Result<(Project, PathBuf), String> {
    let project = {
        let projects = state.project.projects.read().await;
        projects
            .get(&project_id)
            .cloned()
            .ok_or_else(|| format!("Project {project_id} not found"))?
    };

    let repository_id = project
        .repository_id
        .ok_or_else(|| "This project has no repository, so it has no folder to store state in.".to_string())?;

    let root = {
        let repositories = state.repository.repositories.read().await;
        repositories
            .get(&repository_id)
            .map(|r| PathBuf::from(&r.local_path))
            .ok_or_else(|| format!("Repository {repository_id} not found"))?
    };

    Ok((project, root))
}

fn info_for(
    state: &tauri::State<'_, crate::AppState>,
    project: &Project,
    root: &Path,
) -> StateLocationInfo {
    let key = ProjectKey::for_path(root).key;
    let paths = &state.paths;

    StateLocationInfo {
        project_id: project.id.to_string(),
        location: project.state_location,
        resolved: project.state_location.resolve(root),
        project_root: root.display().to_string(),
        current_dir: paths
            .project_state_dir(&key, project.id, root, project.state_location)
            .display()
            .to_string(),
        external_dir: paths
            .project_state_dir(&key, project.id, root, StateLocation::External)
            .display()
            .to_string(),
        in_project_dir: paths
            .project_state_dir(&key, project.id, root, StateLocation::InProject)
            .display()
            .to_string(),
        can_choose: true,
    }
}

#[tauri::command]
pub async fn get_state_location(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<StateLocationInfo, String> {
    let id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;

    match project_root(&state, id).await {
        Ok((project, root)) => Ok(info_for(&state, &project, &root)),
        Err(_) => {
            // A project with no repository still renders a settings panel; it
            // just cannot be given a choice. Returning an error instead would
            // make the whole panel look broken.
            let projects = state.project.projects.read().await;
            let project = projects
                .get(&id)
                .ok_or_else(|| format!("Project {id} not found"))?;
            Ok(StateLocationInfo {
                project_id: project.id.to_string(),
                location: project.state_location,
                resolved: ResolvedLocation::External,
                project_root: String::new(),
                current_dir: String::new(),
                external_dir: String::new(),
                in_project_dir: String::new(),
                can_choose: false,
            })
        }
    }
}

/// Describe what moving to `target` would do, without touching anything.
#[tauri::command]
pub async fn plan_state_migration(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    target: StateLocation,
) -> Result<MigrationPlan, String> {
    let id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    let (project, root) = project_root(&state, id).await?;

    let key = ProjectKey::for_path(&root).key;
    let from = state
        .paths
        .project_state_dir(&key, project.id, &root, project.state_location);
    let to = state
        .paths
        .project_state_dir(&key, project.id, &root, target);

    StateMigrator::plan(&from, &to).map_err(|e| e.to_string())
}

/// Move the project's state and persist the new preference.
///
/// The preference is written only after the move succeeds. Recording it first
/// would leave the project pointing at a directory the data never reached.
#[tauri::command]
pub async fn apply_state_migration(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    target: StateLocation,
    policy: Option<ConflictPolicy>,
) -> Result<MigrationReport, String> {
    let id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    let (project, root) = project_root(&state, id).await?;

    let key = ProjectKey::for_path(&root).key;
    let from = state
        .paths
        .project_state_dir(&key, project.id, &root, project.state_location);
    let to = state
        .paths
        .project_state_dir(&key, project.id, &root, target);

    let report = StateMigrator::migrate(&from, &to, policy.unwrap_or_default())
        .map_err(|e| e.to_string())?;

    {
        let mut projects = state.project.projects.write().await;
        if let Some(project) = projects.get_mut(&id) {
            project.state_location = target;
            project.updated_at = chrono::Utc::now();
        }
        // The files have already moved, so a failure to record where they went
        // must be reported rather than logged. Swallowing it would leave the
        // project reading from the directory the data just left, and the user
        // would open an empty board with nothing to explain it.
        crate::commands::project::try_persist_projects(&state.storage, &projects).map_err(|e| {
            format!(
                "State was moved to {} but the setting could not be saved: {e}. \
                 Re-select the location in Settings to finish.",
                to.display()
            )
        })?;
    }

    // Saving config rebuilt the routing table, so subsequent task reads and
    // writes already resolve to the new directory.
    Ok(report)
}

/// Set the preference without moving anything.
///
/// Offered separately because a user who has already moved their files by hand
/// needs a way to tell SlashIt where they are, and forcing a migration through
/// would then be wrong.
#[tauri::command]
pub async fn set_state_location(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    location: StateLocation,
) -> Result<StateLocationInfo, String> {
    let id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;

    {
        let mut projects = state.project.projects.write().await;
        let project = projects
            .get_mut(&id)
            .ok_or_else(|| format!("Project {id} not found"))?;
        project.state_location = location;
        project.updated_at = chrono::Utc::now();
        crate::commands::project::persist_projects(&state.storage, &projects);
    }

    get_state_location(state, project_id).await
}

/// Remove the empty `.slashit/` directories that earlier versions created.
///
/// Only ever removes a directory it can prove is empty, so a populated
/// in-project state directory is left alone even if the project has since been
/// switched to external storage.
#[tauri::command]
pub async fn clean_legacy_state_dirs(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<String>, String> {
    let roots: Vec<PathBuf> = {
        let repositories = state.repository.repositories.read().await;
        repositories
            .values()
            .map(|r| PathBuf::from(&r.local_path))
            .collect()
    };

    let mut removed = Vec::new();
    for root in roots {
        let dir = crate::config::paths::AppPaths::in_project_state(&root);
        if StateMigrator::remove_empty_legacy_dir(&dir) {
            removed.push(dir.display().to_string());
        }
    }
    Ok(removed)
}
