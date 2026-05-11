use crate::config::WorkspaceRegistry;
use crate::domain::{Workspace, WorkspaceRoot};
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

#[tauri::command]
pub async fn delete_workspace(
    state: tauri::State<'_, crate::AppState>,
    workspace_id: String,
) -> Result<bool, String> {
    let workspace_id = Uuid::parse_str(&workspace_id).map_err(|e| e.to_string())?;
    let mut registry = state.workspace.registry.write().await;
    registry.remove(&workspace_id).map_err(|e| e.to_string())
}
