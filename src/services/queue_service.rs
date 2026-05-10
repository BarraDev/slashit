use crate::models::QueueConfig;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
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

