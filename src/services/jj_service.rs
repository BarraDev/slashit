use wasm_bindgen::prelude::*;
use serde::{Deserialize, Serialize};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JjStatus {
    pub current_change_id: Option<String>,
    pub pending_changes: bool,
    pub conflicted: bool,
}

pub async fn jj_get_workspace_status(workspace_path: String) -> Result<JjStatus, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "workspacePath": workspace_path })).unwrap();
    let response = invoke("jj_get_workspace_status", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn get_task_diff(task_id: String) -> Result<String, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id })).unwrap();
    let response = invoke("get_task_diff", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn get_task_diff_stat(task_id: String) -> Result<String, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id })).unwrap();
    let response = invoke("get_task_diff_stat", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}
