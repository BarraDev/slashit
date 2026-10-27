// SPDX-License-Identifier: MPL-2.0
//! Human Review decisions: approve, request changes, open the pull request
//! for approved changes, complete approved changes locally, and close a task
//! without merging it.

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

/// `tip` is the branch commit the person was shown. The approval covers only
/// that commit, and is refused if the branch has moved since. `Err` means the
/// approval was not recorded.
pub async fn approve_task(task_id: String, create_pr: bool, tip: Option<String>) -> Result<ApprovalOutcome, String> {
    invoke("approve_task", serde_json::json!({ "taskId": task_id, "createPr": create_pr, "tip": tip })).await
}

pub async fn create_approved_task_pr(task_id: String) -> Result<ApprovalOutcome, String> {
    invoke("create_approved_task_pr", serde_json::json!({ "taskId": task_id })).await
}

/// Finish approved changes without a pull request: the task becomes Done,
/// its Task Checkout is removed and its branch is kept, unmerged.
/// `without_pull_request` is a person's choice to do that in a project that
/// could open one; without it, only a project that cannot is accepted.
/// `Err` means the task was not completed and is where it was.
pub async fn complete_task_locally(task_id: String, without_pull_request: bool) -> Result<Task, String> {
    invoke(
        "complete_task_locally",
        serde_json::json!({ "taskId": task_id, "withoutPullRequest": without_pull_request }),
    )
    .await
}

/// Mirrors the backend `ApprovalCoverage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalCoverage {
    NotApplicable,
    Current,
    Moved,
    Unrecorded,
    BranchMissing,
}

/// Mirrors the backend `ApprovalStatus`: whether the approval still covers
/// the task's branch as it is now.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ApprovalStatus {
    pub coverage: ApprovalCoverage,
    pub approved_tip: Option<String>,
    pub branch_tip: Option<String>,
    pub rewritten: bool,
    pub reason: Option<String>,
}

pub async fn get_approval_status(task_id: String) -> Result<ApprovalStatus, String> {
    invoke("get_approval_status", serde_json::json!({ "taskId": task_id })).await
}

/// Approve the branch as it is now, after it moved past the approved commit.
/// `tip` is the commit the person was shown; `Err` means nothing was approved.
pub async fn approve_task_again(task_id: String, tip: String) -> Result<Task, String> {
    invoke("approve_task_again", serde_json::json!({ "taskId": task_id, "tip": tip })).await
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
