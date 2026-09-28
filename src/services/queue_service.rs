use crate::models::{QueueConfig, Task, TaskStatus};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;

    /// The same command bridge, with a rejected command surfaced as an error
    /// rather than thrown past the caller.
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke, catch)]
    async fn try_invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;
}

/// Put a task in the queue, top of the column, and return the record the
/// backend committed (`None` if the task no longer exists).
///
/// This is the one way the UI enqueues a single task: the drawer's Start and
/// Retry and the card menu's "Add to Queue" all call it, so they cannot
/// disagree about what enqueuing does. It only changes the task's status; the
/// executor's own scheduler decides when a queued task runs, under the
/// configured capacity and ordering. Leaving `Error` resets the attempt's
/// execution state and keeps its checkout, so a re-queued task continues
/// where it stopped.
pub async fn enqueue_task(task_id: String) -> Result<Option<Task>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "taskId": task_id,
        "newStatus": TaskStatus::Queue,
        "newPosition": 0,
    }))
    .map_err(|e| e.to_string())?;
    let value = try_invoke("reorder_task", args).await.map_err(|error| {
        error.as_string().unwrap_or_else(|| {
            js_sys::JSON::stringify(&error)
                .ok()
                .and_then(|s| s.as_string())
                .unwrap_or_else(|| "reorder_task failed".to_string())
        })
    })?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

pub async fn update_queue_config(_project_id: String, config: QueueConfig) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "parallelTaskLimit": config.parallel_task_limit,
        "autoPromote": config.auto_promote,
        "fifoOrdering": config.fifo_ordering,
        "useCoderabbit": config.use_coderabbit,
    })).unwrap();
    let response = invoke("update_queue_config", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn bulk_add_to_queue(task_ids: Vec<String>) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskIds": task_ids })).unwrap();
    let response = invoke("bulk_add_to_queue", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn get_queue_capacity() -> Result<usize, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("get_queue_capacity", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

