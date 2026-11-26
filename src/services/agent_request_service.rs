// SPDX-License-Identifier: MPL-2.0
//! Requests external agents file for a person to decide.

use serde::Deserialize;
use uuid::Uuid;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::models::Task;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> Result<js_sys::Promise, JsValue>;
}

async fn invoke<T: for<'de> Deserialize<'de>>(cmd: &str, args: serde_json::Value) -> Result<T, String> {
    let args = serde_wasm_bindgen::to_value(&args).map_err(|e| e.to_string())?;
    let text = |e: JsValue| e.as_string().unwrap_or_else(|| format!("{cmd} failed"));
    let response = JsFuture::from(raw_invoke(cmd, args).map_err(text)?)
        .await
        .map_err(text)?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

/// Mirrors the backend `RequestedOperation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestedOperation {
    Approve,
    OpenPr,
}

/// Mirrors the backend `RequestState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    Pending,
    Approved,
    Authorized,
    Refused,
    Expired,
    Invalidated,
    Used,
}

/// The parts of the backend `AgentRequestView` the drawer shows.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AgentRequestView {
    pub id: Uuid,
    pub task_title: String,
    pub state: RequestState,
    pub operation: RequestedOperation,
    pub commit: String,
    pub requested_by: String,
}

#[derive(Deserialize)]
struct DecisionOutcome {
    task: Task,
}

pub async fn list_agent_requests(task_id: String) -> Result<Vec<AgentRequestView>, String> {
    invoke("list_agent_requests", serde_json::json!({ "taskId": task_id })).await
}

/// A person's decision. `commit` is the commit the person was shown; `Err`
/// means nothing was decided.
pub async fn decide_agent_request(
    task_id: String,
    request_id: String,
    commit: String,
    authorize: bool,
) -> Result<Task, String> {
    invoke::<DecisionOutcome>(
        "decide_agent_request",
        serde_json::json!({
            "taskId": task_id,
            "requestId": request_id,
            "commit": commit,
            "authorize": authorize,
        }),
    )
    .await
    .map(|outcome| outcome.task)
}
