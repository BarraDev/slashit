// SPDX-License-Identifier: MPL-2.0
use crate::models::workspace::Workspace;
use serde::Serialize;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    fn invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

#[derive(Serialize)]
struct CreateArgs<'a> {
    name: &'a str,
    #[serde(rename = "rootPath")]
    root_path: Option<&'a str>,
}

#[derive(Serialize)]
struct IdArgs<'a> {
    #[serde(rename = "workspaceId")]
    workspace_id: &'a str,
}

#[derive(Serialize)]
struct Empty;

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

/// A Workspace that was not created. `workspace` is set when the backend has
/// already saved it, so the caller shows that Workspace instead of creating another.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CreateWorkspaceFailure {
    pub message: String,
    pub workspace: Option<Workspace>,
}

/// Whether a failed creation clears the draft, and the notice to show.
///
/// A saved Workspace is listed after a reload, so its draft is cleared: a retry
/// would create a duplicate. Any other failure keeps the draft for an explicit retry.
pub fn creation_failure_notice(failure: &CreateWorkspaceFailure) -> (bool, String) {
    match &failure.workspace {
        Some(workspace) => (
            true,
            format!(
                "Workspace \"{}\" is listed. {}",
                workspace.name, failure.message
            ),
        ),
        None => (false, failure.message.clone()),
    }
}

/// A rejected creation is a structured failure, or a plain message from an
/// earlier-style rejection.
fn creation_failure(value: JsValue) -> CreateWorkspaceFailure {
    serde_wasm_bindgen::from_value(value.clone()).unwrap_or_else(|_| CreateWorkspaceFailure {
        message: js_error_to_string(value),
        workspace: None,
    })
}

pub async fn create_workspace(
    name: &str,
    root_path: Option<&str>,
) -> Result<Workspace, CreateWorkspaceFailure> {
    let unsent = |e: serde_wasm_bindgen::Error| CreateWorkspaceFailure {
        message: e.to_string(),
        workspace: None,
    };
    let args = serde_wasm_bindgen::to_value(&CreateArgs { name, root_path }).map_err(unsent)?;
    let result = JsFuture::from(invoke("create_workspace", args))
        .await
        .map_err(creation_failure)?;
    serde_wasm_bindgen::from_value(result).map_err(unsent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn workspace() -> Workspace {
        Workspace {
            id: Uuid::nil(),
            name: "Client".into(),
            root_path: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn a_failure_before_creation_keeps_the_draft_and_shows_the_backend_message() {
        let failure = CreateWorkspaceFailure {
            message: "Failed to save the workspace: disk full".into(),
            workspace: None,
        };

        assert_eq!(
            creation_failure_notice(&failure),
            (false, "Failed to save the workspace: disk full".to_string())
        );
    }

    #[test]
    fn an_unconfirmed_save_clears_the_draft_and_names_the_listed_workspace() {
        let failure = CreateWorkspaceFailure {
            message: "The change was applied, but SlashIt could not confirm that it reached stable storage.".into(),
            workspace: Some(workspace()),
        };

        let (clear_draft, notice) = creation_failure_notice(&failure);

        assert!(clear_draft, "a retry would create a duplicate");
        assert!(
            notice.starts_with("Workspace \"Client\" is listed."),
            "{notice}"
        );
        assert!(notice.contains("could not confirm"), "{notice}");
    }
}

pub async fn list_workspaces() -> Result<Vec<Workspace>, String> {
    let args = serde_wasm_bindgen::to_value(&Empty).map_err(|e| e.to_string())?;
    let result = JsFuture::from(invoke("list_workspaces", args))
        .await
        .map_err(js_error_to_string)?;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

/// Renames a Workspace. Membership and its root are unchanged.
pub async fn rename_workspace(workspace_id: &str, name: &str) -> Result<Workspace, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "workspaceId": workspace_id,
        "name": name,
    }))
    .map_err(|e| e.to_string())?;
    let result = JsFuture::from(invoke("rename_workspace", args))
        .await
        .map_err(js_error_to_string)?;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

/// Sets a Workspace's custom root, or returns it to the managed root when
/// `root_path` is `None`. Existing folders are never moved or deleted.
pub async fn set_workspace_root(workspace_id: &str, root_path: Option<&str>) -> Result<Workspace, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "workspaceId": workspace_id,
        "rootPath": root_path,
    }))
    .map_err(|e| e.to_string())?;
    let result = JsFuture::from(invoke("set_workspace_root", args))
        .await
        .map_err(js_error_to_string)?;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

pub async fn get_workspace(workspace_id: &str) -> Result<Workspace, String> {
    let args = serde_wasm_bindgen::to_value(&IdArgs { workspace_id })
        .map_err(|e| e.to_string())?;
    let result = JsFuture::from(invoke("get_workspace", args))
        .await
        .map_err(js_error_to_string)?;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

pub async fn delete_workspace(workspace_id: &str) -> Result<bool, String> {
    let args = serde_wasm_bindgen::to_value(&IdArgs { workspace_id })
        .map_err(|e| e.to_string())?;
    let result = JsFuture::from(invoke("delete_workspace", args))
        .await
        .map_err(js_error_to_string)?;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

pub async fn get_workspace_root(workspace_id: &str) -> Result<crate::models::workspace::WorkspaceRootInfo, String> {
    let args = serde_wasm_bindgen::to_value(&IdArgs { workspace_id })
        .map_err(|e| e.to_string())?;
    let result = JsFuture::from(invoke("get_workspace_root", args))
        .await
        .map_err(js_error_to_string)?;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}
