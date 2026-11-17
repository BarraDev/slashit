// SPDX-License-Identifier: MPL-2.0
use crate::models::github::{GithubIssue, PullRequest};
use crate::services::project_service;
use crate::models::Task;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

/// The Issues page must see a refusal (gh missing, not signed in, GitHub
/// unreachable) as an error, not as a request that never answers, so it goes
/// through the checked invocation rather than the trapping one.
pub async fn get_issues(repo: String, state: &str) -> Result<Vec<GithubIssue>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "repo": repo,
        "state": state,
    })).unwrap();
    let response = project_service::invoke_checked("get_issues", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn get_prs(repo: String) -> Result<Vec<PullRequest>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "repo": repo,
    })).unwrap();
    let response = invoke("get_prs", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn create_task_from_issue(
    repo: String,
    issue_number: i32,
    project_id: String,
) -> Result<Task, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "repo": repo,
        "issueNumber": issue_number,
        "projectId": project_id,
    })).unwrap();
    let response = project_service::invoke_checked("create_task_from_issue", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

