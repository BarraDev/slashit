//! Frontend access to the unreadable Task board report.
//!
//! Uses the fallible `raw_invoke` binding, as the orphans service does, so a
//! failed read says why instead of looking like an empty answer.

use crate::models::task_boards::{BoardStatus, TaskBoardReport};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

/// One diagnostic read, as the state a board's controls and notices use.
pub async fn read_board_status() -> BoardStatus {
    match list_task_board_diagnostics().await {
        Ok(reports) => BoardStatus::Read(reports),
        Err(error) => BoardStatus::Failed(error),
    }
}

pub async fn list_task_board_diagnostics() -> Result<Vec<TaskBoardReport>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).map_err(|e| e.to_string())?;
    let response = JsFuture::from(raw_invoke("list_task_board_diagnostics", args))
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
