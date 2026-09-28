//! Frontend access to a project's repository setup commands.
//!
//! Uses the fallible `raw_invoke` binding: an initialization or a detection
//! that fails must say why.

use crate::models::repository_setup::{InitPreview, Readiness, VcsInitKind};
use crate::models::Repository;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

async fn call<T: serde::de::DeserializeOwned>(
    cmd: &str,
    args: serde_json::Value,
) -> Result<T, String> {
    let args = serde_wasm_bindgen::to_value(&args).map_err(|e| e.to_string())?;
    let response = JsFuture::from(raw_invoke(cmd, args))
        .await
        .map_err(|value| {
            value.as_string().unwrap_or_else(|| {
                js_sys::JSON::stringify(&value)
                    .ok()
                    .and_then(|s| s.as_string())
                    .unwrap_or_else(|| "Tauri command failed".to_string())
            })
        })?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn get_project_readiness(project_id: String) -> Result<Readiness, String> {
    call(
        "get_project_readiness",
        serde_json::json!({ "projectId": project_id }),
    )
    .await
}

pub async fn set_project_base(project_id: String, branch: String) -> Result<Readiness, String> {
    call(
        "set_project_base",
        serde_json::json!({ "projectId": project_id, "branch": branch }),
    )
    .await
}

pub async fn detect_remote_default_branch(project_id: String) -> Result<Readiness, String> {
    call(
        "detect_remote_default_branch",
        serde_json::json!({ "projectId": project_id }),
    )
    .await
}

pub async fn preview_vcs_initialization(path: String) -> Result<InitPreview, String> {
    call(
        "preview_vcs_initialization",
        serde_json::json!({ "path": path }),
    )
    .await
}

pub async fn initialize_project_vcs(
    project_id: String,
    kind: VcsInitKind,
) -> Result<Readiness, String> {
    call(
        "initialize_project_vcs",
        serde_json::json!({ "projectId": project_id, "kind": kind }),
    )
    .await
}

/// Register `local_path` as a repository, initializing version control
/// there first when `initialize` says so.
pub async fn create_repository(
    local_path: String,
    remote_url: Option<String>,
    initialize: Option<VcsInitKind>,
) -> Result<Repository, String> {
    call(
        "create_repository",
        serde_json::json!({
            "localPath": local_path,
            "remoteUrl": remote_url,
            "initialize": initialize,
        }),
    )
    .await
}
