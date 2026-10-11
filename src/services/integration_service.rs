//! Integrating a task's approved work into a local branch: the preview the
//! drawer shows, the confirmation, and finishing or dropping an integration
//! that was started and not finished.
//!
//! Every call is a person's action. `Err` always means nothing was changed by
//! that call, or says what was.

use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::models::integration::{IntegrationConfirmation, IntegrationPreview};
use crate::models::Task;

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

/// What integrating would do now. Changes nothing.
pub async fn get_integration_preview(task_id: String) -> Result<IntegrationPreview, String> {
    invoke("get_integration_preview", serde_json::json!({ "taskId": task_id })).await
}

/// Integrate exactly what the preview showed.
pub async fn integrate_task_locally(task_id: String, confirmation: IntegrationConfirmation) -> Result<Task, String> {
    invoke(
        "integrate_task_locally",
        serde_json::json!({ "taskId": task_id, "confirmation": confirmation }),
    )
    .await
}

/// Integrate exactly what the preview showed, and complete the task, which
/// is in Human Review. An answer that is not completed means the
/// integration is recorded and the completion is still owed.
pub async fn complete_and_integrate_task(
    task_id: String,
    confirmation: IntegrationConfirmation,
) -> Result<Task, String> {
    invoke(
        "complete_and_integrate_task",
        serde_json::json!({ "taskId": task_id, "confirmation": confirmation }),
    )
    .await
}

/// Record the commit the person was shown as the approved one, for an
/// approval that did not record it.
pub async fn reconfirm_approved_tip(task_id: String, tip: String) -> Result<Task, String> {
    invoke("reconfirm_approved_tip", serde_json::json!({ "taskId": task_id, "tip": tip })).await
}

pub async fn resume_local_integration(task_id: String) -> Result<Task, String> {
    invoke("resume_local_integration", serde_json::json!({ "taskId": task_id })).await
}

pub async fn discard_local_integration(task_id: String) -> Result<Task, String> {
    invoke("discard_local_integration", serde_json::json!({ "taskId": task_id })).await
}
