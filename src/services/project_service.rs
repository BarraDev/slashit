use crate::models::*;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

pub async fn create_project(name: String, repository_id: Option<String>, agent_type: AgentType) -> Result<Project, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "name": name,
        "repositoryId": repository_id,
        "agentType": agent_type,
    })).unwrap();

    let response = invoke("create_project", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn list_projects() -> Result<Vec<Project>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("list_projects", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn get_project(id: String) -> Result<Option<Project>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "id": id })).unwrap();
    let response = invoke("get_project", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

/// Get the working directory path for a project
pub async fn get_project_path(project_id: String) -> Result<Option<String>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "projectId": project_id })).unwrap();
    let response = invoke("get_project_path", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}
