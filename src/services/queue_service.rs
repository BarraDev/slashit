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

/// What became of a request to queue a task.
#[derive(Debug, Clone)]
pub enum EnqueueOutcome {
    /// The task is in the queue.
    Queued(Task),
    /// The task was no longer where the caller saw it, so nothing was
    /// changed. This is the task as it is now.
    AlreadyMoved(Task),
    /// There is no such task.
    Missing,
}

/// Put a task in the queue, top of the column, provided it is still in
/// `seen_in`: the column the person saw it in when they asked.
///
/// This is the one way the UI enqueues a single task: the drawer's Start and
/// Retry and the card menu's "Add to Queue" all call it, so they cannot
/// disagree about what enqueuing does. It only changes the task's status; the
/// executor's own scheduler decides when a queued task runs, under the
/// configured capacity and ordering. Leaving `Error` resets the attempt's
/// execution state and keeps its checkout, so a re-queued task continues
/// where it stopped.
///
/// `seen_in` is checked by the backend under the task's lifecycle lease. A
/// view that is behind -- a drawer still showing Backlog after the scheduler
/// started the task -- therefore gets [`EnqueueOutcome::AlreadyMoved`] with
/// the task as it is, instead of ending the running agent and queuing the
/// task again.
pub async fn enqueue_task(task_id: String, seen_in: TaskStatus) -> Result<EnqueueOutcome, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "taskId": task_id,
        "newStatus": TaskStatus::Queue,
        "newPosition": 0,
        "expectedStatus": seen_in,
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
    let answer: Option<Task> = serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())?;
    // The backend answers a refusal with the task as it is, so the returned
    // status is what tells the two apart.
    Ok(match answer {
        Some(task) if task.status == TaskStatus::Queue => EnqueueOutcome::Queued(task),
        Some(task) => EnqueueOutcome::AlreadyMoved(task),
        None => EnqueueOutcome::Missing,
    })
}

/// A rejected command, as the text the backend rejected it with.
fn rejection_text(error: JsValue, command: &str) -> String {
    error.as_string().unwrap_or_else(|| {
        js_sys::JSON::stringify(&error)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_else(|| format!("{command} failed"))
    })
}

/// The Queue settings the backend is running under right now.
pub async fn get_queue_config() -> Result<QueueConfig, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).map_err(|e| e.to_string())?;
    let value = try_invoke("get_queue_config", args)
        .await
        .map_err(|error| rejection_text(error, "get_queue_config"))?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

/// Save the Queue settings. The backend answers with the settings it is now
/// running under, which is what the caller should show: it is the proof the
/// change was saved, not an echo of what was asked for. A refusal -- the
/// settings could not be written, or a value is out of range -- arrives as an
/// error and leaves the running settings as they were.
pub async fn update_queue_config(config: QueueConfig) -> Result<QueueConfig, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "parallelTaskLimit": config.parallel_task_limit,
        "autoPromote": config.auto_promote,
        "fifoOrdering": config.fifo_ordering,
        "useCoderabbit": config.use_coderabbit,
    }))
    .map_err(|e| e.to_string())?;
    let value = try_invoke("update_queue_config", args)
        .await
        .map_err(|error| rejection_text(error, "update_queue_config"))?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
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

