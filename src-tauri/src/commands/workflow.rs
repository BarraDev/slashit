use crate::domain::AgentLogEntry;
use crate::queue::workflow::{Workflow, WorkflowMode};
use crate::queue::workflow::WorkflowConfig;
use uuid::Uuid;

#[tauri::command]
pub async fn get_workflow_config(
    _state: tauri::State<'_, crate::AppState>,
) -> Result<WorkflowConfig, String> {
    Ok(WorkflowConfig::default())
}

#[tauri::command]
pub async fn update_workflow_config(
    _state: tauri::State<'_, crate::AppState>,
    mode: Option<String>,
    parallel_limit: Option<u32>,
    review_required: Option<bool>,
    auto_commit: Option<bool>,
    max_retries: Option<u32>,
) -> Result<WorkflowConfig, String> {
    let mut config = WorkflowConfig::default();
    if let Some(m) = mode {
        config.mode = match m.as_str() {
            "team" => WorkflowMode::Team,
            _ => WorkflowMode::Single,
        };
    }
    if let Some(limit) = parallel_limit {
        config.parallel_limit = limit;
    }
    if let Some(review) = review_required {
        config.review_required = review;
    }
    if let Some(commit) = auto_commit {
        config.auto_commit = commit;
    }
    if let Some(retries) = max_retries {
        config.max_retries = retries;
    }
    Ok(config)
}

#[tauri::command]
pub async fn list_workflows(
    _state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<Workflow>, String> {
    // Will be wired to orchestrator when it's integrated into AppState
    Ok(Vec::new())
}

#[tauri::command]
pub async fn get_workflow(
    _state: tauri::State<'_, crate::AppState>,
    workflow_id: String,
) -> Result<Option<Workflow>, String> {
    let _wf_id = Uuid::parse_str(&workflow_id).map_err(|e| e.to_string())?;
    Ok(None)
}

#[tauri::command]
pub async fn get_workflow_logs(
    _state: tauri::State<'_, crate::AppState>,
    workflow_id: String,
) -> Result<Vec<AgentLogEntry>, String> {
    let _wf_id = Uuid::parse_str(&workflow_id).map_err(|e| e.to_string())?;
    Ok(Vec::new())
}
