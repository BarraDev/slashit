// SPDX-License-Identifier: MPL-2.0
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
pub(crate) async fn invoke_checked(cmd: &str, args: JsValue) -> Result<JsValue, String> {
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

    // `invoke` traps on a refused create, which aborted the caller silently:
    // no message, and the dialog stayed open. The refusal must come back as Err.
    let response = invoke_checked("create_project", args).await?;
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
/// the project is already standalone, or if it is no longer in
/// `expected_workspace_id` (a stale list).
pub async fn detach_project_from_workspace(
    project_id: String,
    expected_workspace_id: String,
) -> Result<Project, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "projectId": project_id,
        "expectedWorkspaceId": expected_workspace_id,
    })).unwrap();

    let response = invoke_checked("detach_project_from_workspace", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

/// Rename a Project's displayed name. The repository folder is not changed.
pub async fn rename_project(project_id: String, name: String) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "id": project_id,
        "name": name,
    })).unwrap();

    let response = invoke_checked("update_project", args).await?;
    // The backend answers None when the Project no longer exists: not a rename.
    let updated: Option<Project> = serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())?;
    updated
        .map(|_| ())
        .ok_or_else(|| "This Project no longer exists.".to_string())
}

/// Save a Project's PR Comment Review defaults. Nothing else about the Project changes.
pub async fn set_project_pr_review_defaults(
    project_id: String,
    defaults: PrReviewDefaults,
) -> Result<Project, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "id": project_id,
        "autoPush": defaults.auto_push,
    })).unwrap();

    let response = invoke_checked("set_project_pr_review_defaults", args).await?;
    let updated: Option<Project> = serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())?;
    updated.ok_or_else(|| "This Project no longer exists.".to_string())
}

/// Remove a Project from SlashIt's active Projects. Its folder, Tasks, history
/// and identity are kept; opening the same folder again brings it back.
pub async fn remove_project(project_id: String) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "id": project_id,
    })).unwrap();

    invoke_checked("remove_project", args).await?;
    Ok(())
}
