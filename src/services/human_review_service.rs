//! Human Review decisions: approve, request changes, open the pull request
//! for approved changes, and close a task without merging it.

use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::models::{Task, TaskStatus};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

async fn invoke<T: for<'de> Deserialize<'de>>(cmd: &str, args: serde_json::Value) -> Result<T, String> {
    let args = serde_wasm_bindgen::to_value(&args).map_err(|e| e.to_string())?;
    let response = JsFuture::from(raw_invoke(cmd, args)).await.map_err(|e| {
        e.as_string().unwrap_or_else(|| {
            js_sys::JSON::stringify(&e)
                .ok()
                .and_then(|s| s.as_string())
                .unwrap_or_else(|| format!("{cmd} failed"))
        })
    })?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

/// Mirrors the backend `PrAvailability`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrAvailability {
    Available,
    Unavailable { reason: String },
}

/// Mirrors the backend `PrDelivery`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrDelivery {
    Created { url: String },
    Failed { reason: String },
    Unavailable { reason: String },
}

/// Mirrors the backend `ApprovalOutcome`.
#[derive(Debug, Clone, Deserialize)]
pub struct ApprovalOutcome {
    pub task: Task,
    pub pr: Option<PrDelivery>,
}

/// Mirrors the backend `ProjectAttention`: the tasks in one project that need
/// the user. Only projects with at least one are sent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProjectAttention {
    pub project_id: uuid::Uuid,
    pub tasks: Vec<TaskAttention>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TaskAttention {
    pub task_id: uuid::Uuid,
    pub reason: crate::models::AttentionReason,
}

/// Which tasks need the user, in every project, without reading any board.
pub async fn get_attention_summary() -> Result<Vec<ProjectAttention>, String> {
    invoke("get_attention_summary", serde_json::json!({})).await
}

pub async fn get_pr_availability(task_id: String) -> Result<PrAvailability, String> {
    invoke("get_pr_availability", serde_json::json!({ "taskId": task_id })).await
}

/// `Err` means the approval was not recorded.
pub async fn approve_task(task_id: String, create_pr: bool) -> Result<ApprovalOutcome, String> {
    invoke("approve_task", serde_json::json!({ "taskId": task_id, "createPr": create_pr })).await
}

pub async fn create_approved_task_pr(task_id: String) -> Result<ApprovalOutcome, String> {
    invoke("create_approved_task_pr", serde_json::json!({ "taskId": task_id })).await
}

/// `Err` means neither the feedback nor the requeue was recorded.
pub async fn request_task_changes(task_id: String, feedback: String) -> Result<Task, String> {
    invoke(
        "request_task_changes",
        serde_json::json!({ "taskId": task_id, "feedback": feedback }),
    )
    .await
}

/// Move a task from Human Review to Done after the person confirmed they are
/// closing it without merging it.
pub async fn close_without_merging(task_id: String, position: i32) -> Result<Option<Task>, String> {
    invoke(
        "reorder_task",
        serde_json::json!({
            "taskId": task_id,
            "newStatus": TaskStatus::Done,
            "newPosition": position,
            "closeWithoutMerge": true,
        }),
    )
    .await
}
