use crate::models::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

/// A rejected Tauri invocation carries the backend's `String` error as a
/// `JsValue`. `{:?}`-formatting it renders `JsValue("…")` verbatim — this
/// unwraps to the actual message so callers (toasts, etc.) show the real
/// error text instead of its debug wrapper.
fn js_error_to_string(value: JsValue) -> String {
    value.as_string().unwrap_or_else(|| {
        js_sys::JSON::stringify(&value)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_else(|| "Tauri command failed".to_string())
    })
}

/// Like `invoke`, but surfaces a command's `Err` instead of trapping on it —
/// used by commands whose failure the caller must display, such as attach
/// and detach refusing an ambiguous membership change.
async fn invoke_checked(cmd: &str, args: JsValue) -> Result<JsValue, String> {
    JsFuture::from(raw_invoke(cmd, args))
        .await
        .map_err(js_error_to_string)
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

/// Attach a standalone project to a workspace. Fails if the project already
/// belongs to a workspace, or if the workspace does not exist.
pub async fn attach_project_to_workspace(project_id: String, workspace_id: String) -> Result<Project, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "projectId": project_id,
        "workspaceId": workspace_id,
    })).unwrap();

    let response = invoke_checked("attach_project_to_workspace", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

/// Detach a project from its workspace, returning it to standalone. Fails if
/// the project is already standalone.
pub async fn detach_project_from_workspace(project_id: String) -> Result<Project, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "projectId": project_id,
    })).unwrap();

    let response = invoke_checked("detach_project_from_workspace", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}
