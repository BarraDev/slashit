use crate::models::*;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

pub async fn start_agent(workspace_id: String, task_id: Option<String>) -> Result<AgentExecution, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "workspaceId": workspace_id,
        "taskId": task_id,
    })).unwrap();

    let response = invoke("start_agent", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn stop_agent(execution_id: String) -> Result<bool, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "executionId": execution_id })).unwrap();
    let response = invoke("stop_agent", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn get_agent_status(execution_id: String) -> Result<Option<AgentExecution>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "executionId": execution_id })).unwrap();
    let response = invoke("get_agent_status", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub alias: Option<String>,
}

pub async fn list_available_models() -> Result<Vec<ModelInfo>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("list_available_models", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ClaudeCliStatus {
    pub installed: bool,
    pub version: Option<String>,
    pub path: Option<String>,
}

pub async fn check_claude_cli() -> Result<ClaudeCliStatus, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("check_claude_cli", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

