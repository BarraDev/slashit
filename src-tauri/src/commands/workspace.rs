use crate::domain::{Workspace, WorkspaceStatus};
use crate::jj::JjManager;
use uuid::Uuid;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

type Workspaces = Arc<RwLock<HashMap<Uuid, Workspace>>>;

#[derive(Clone)]
pub struct WorkspaceState {
    pub workspaces: Workspaces,
    pub jj_manager: Arc<JjManager>,
    pub base_dir: PathBuf,
}

impl WorkspaceState {
    pub fn new() -> Result<Self, std::io::Error> {
        let proj_dirs = directories::ProjectDirs::from("com", "barradev", "slashit-app")
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "Failed to get project directories"))?;
        let base_dir = proj_dirs.data_dir().join("projects");
        std::fs::create_dir_all(&base_dir)?;
        Ok(Self {
            workspaces: Arc::new(RwLock::new(HashMap::new())),
            jj_manager: Arc::new(JjManager::new()),
            base_dir,
        })
    }
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::new().expect("Failed to create WorkspaceState")
    }
}

#[tauri::command]
pub async fn create_workspace(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    name: String,
    base_revision: Option<String>,
) -> Result<Workspace, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    let id = Uuid::new_v4();

    let project_path = state.workspace.base_dir.join(project_id.to_string());
    let _workspace_path = project_path.join(&name);

    std::fs::create_dir_all(&project_path).map_err(|e| e.to_string())?;

    let workspace_path_str = state.workspace
        .jj_manager
        .create_workspace(&project_path, &name, base_revision.as_deref())
        .map_err(|e| e.to_string())?;

    let workspace = Workspace {
        id,
        project_id,
        name,
        path: workspace_path_str,
        base_revision,
        current_change_id: None,
        created_at: chrono::Utc::now(),
    };

    state.workspace.workspaces.write().await.insert(id, workspace.clone());
    Ok(workspace)
}

#[tauri::command]
pub async fn list_workspaces(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<Vec<Workspace>, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    let workspaces = state.workspace.workspaces.read().await;
    Ok(workspaces
        .values()
        .filter(|w| w.project_id == project_id)
        .cloned()
        .collect())
}

#[tauri::command]
pub async fn remove_workspace(
    state: tauri::State<'_, crate::AppState>,
    workspace_id: String,
) -> Result<bool, String> {
    let workspace_id = Uuid::parse_str(&workspace_id).map_err(|e| e.to_string())?;
    let workspace = {
        let workspaces = state.workspace.workspaces.read().await;
        workspaces.get(&workspace_id).cloned()
    };

    if let Some(workspace) = workspace {
        let workspace_path = PathBuf::from(&workspace.path);
        let project_path = workspace_path.parent().unwrap_or(&workspace_path);
        state.workspace
            .jj_manager
            .remove_workspace(project_path, &workspace.name)
            .map_err(|e| e.to_string())?;

        Ok(state.workspace.workspaces.write().await.remove(&workspace_id).is_some())
    } else {
        Ok(false)
    }
}

#[tauri::command]
pub async fn get_workspace_status(
    state: tauri::State<'_, crate::AppState>,
    workspace_id: String,
) -> Result<WorkspaceStatus, String> {
    let workspace_id = Uuid::parse_str(&workspace_id).map_err(|e| e.to_string())?;
    let workspaces = state.workspace.workspaces.read().await;

    let workspace = workspaces
        .get(&workspace_id)
        .ok_or("Workspace not found")?;

    let status = state.workspace
        .jj_manager
        .get_status(PathBuf::from(&workspace.path).as_path())
        .map_err(|e| e.to_string())?;

    Ok(WorkspaceStatus {
        workspace_id,
        current_change_id: status.current_change_id,
        pending_changes: status.pending_changes,
        conflicted: status.conflicted,
    })
}
