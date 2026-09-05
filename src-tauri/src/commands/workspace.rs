use crate::config::WorkspaceRegistry;
use crate::domain::{Project, Workspace, WorkspaceRoot};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Clone)]
pub struct WorkspaceState {
    pub registry: Arc<RwLock<WorkspaceRegistry>>,
}

impl WorkspaceState {
    pub fn new() -> Result<Self, std::io::Error> {
        let registry = WorkspaceRegistry::load()?;
        Ok(Self {
            registry: Arc::new(RwLock::new(registry)),
        })
    }
}

#[tauri::command]
pub async fn create_workspace(
    state: tauri::State<'_, crate::AppState>,
    name: String,
    root_path: String,
) -> Result<Workspace, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Workspace name is required".to_string());
    }

    let root = WorkspaceRoot::try_new(&root_path)?;
    let workspace = Workspace::new(name, root.clone());
    let mut registry = state.workspace.registry.write().await;
    if registry.contains_root(root.as_path()) {
        return Err(format!(
            "A workspace is already registered for {}",
            root.as_path().display()
        ));
    }
    registry.upsert(workspace.clone()).map_err(|e| e.to_string())?;
    Ok(workspace)
}

#[tauri::command]
pub async fn list_workspaces(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<Workspace>, String> {
    let registry = state.workspace.registry.read().await;
    Ok(registry.all().cloned().collect())
}

#[tauri::command]
pub async fn get_workspace(
    state: tauri::State<'_, crate::AppState>,
    workspace_id: String,
) -> Result<Workspace, String> {
    let workspace_id = Uuid::parse_str(&workspace_id).map_err(|e| e.to_string())?;
    let registry = state.workspace.registry.read().await;
    registry
        .get(&workspace_id)
        .cloned()
        .ok_or_else(|| "Workspace not found".to_string())
}

/// Names of every project still attached to `workspace_id`.
///
/// Plain and state-free so it can be unit tested without a full `AppState` —
/// the command below is just this check plus the actual registry mutation.
fn projects_referencing(projects: &HashMap<Uuid, Project>, workspace_id: Uuid) -> Vec<String> {
    projects
        .values()
        .filter(|p| p.scope.workspace_id() == Some(workspace_id))
        .map(|p| p.name.clone())
        .collect()
}

#[tauri::command]
pub async fn delete_workspace(
    state: tauri::State<'_, crate::AppState>,
    workspace_id: String,
) -> Result<bool, String> {
    let workspace_id = Uuid::parse_str(&workspace_id).map_err(|e| e.to_string())?;

    // Deleting the workspace out from under a project that still points at it
    // would leave that project's `scope` referencing a workspace id nothing
    // can resolve anymore. Reject the deletion instead of guessing at a
    // cascade — the caller can re-scope or delete those projects first.
    let referencing = {
        let projects = state.project.projects.read().await;
        projects_referencing(&projects, workspace_id)
    };
    if !referencing.is_empty() {
        return Err(format!(
            "Cannot delete workspace: {} project(s) still reference it: {}",
            referencing.len(),
            referencing.join(", ")
        ));
    }

    let mut registry = state.workspace.registry.write().await;
    registry.remove(&workspace_id).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AgentConfig, AgentType, ProjectScope};
    use crate::config::paths::StateLocation;

    fn project(name: &str, scope: ProjectScope) -> Project {
        Project {
            id: Uuid::new_v4(),
            name: name.to_string(),
            repository_id: None,
            scope,
            state_location: StateLocation::External,
            agent_type: AgentType::ClaudeCode,
            agent_config: AgentConfig {
                agent_type: AgentType::ClaudeCode,
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

    #[test]
    fn no_projects_reference_an_unrelated_or_standalone_workspace() {
        let workspace_id = Uuid::new_v4();
        let other_workspace_id = Uuid::new_v4();
        let mut projects = HashMap::new();
        let standalone = project("standalone", ProjectScope::Standalone);
        let other = project(
            "other-workspace",
            ProjectScope::InWorkspace {
                workspace_id: other_workspace_id,
            },
        );
        projects.insert(standalone.id, standalone);
        projects.insert(other.id, other);

        assert!(projects_referencing(&projects, workspace_id).is_empty());
    }

    #[test]
    fn projects_still_in_the_workspace_are_reported_by_name() {
        let workspace_id = Uuid::new_v4();
        let mut projects = HashMap::new();
        let attached = project(
            "board",
            ProjectScope::InWorkspace { workspace_id },
        );
        projects.insert(attached.id, attached);

        let referencing = projects_referencing(&projects, workspace_id);
        assert_eq!(referencing, vec!["board".to_string()]);
    }
}
