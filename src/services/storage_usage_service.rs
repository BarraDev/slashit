//! Frontend access to the storage accounting commands.
//!
//! Uses the fallible `raw_invoke` binding, like `state_service`, so a failed
//! command reaches the view as an error instead of an empty result that
//! would read as "nothing measured".

use crate::models::storage_usage::{StartBlock, StorageStatus};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

async fn invoke<T: serde::de::DeserializeOwned>(cmd: &str) -> Result<T, String> {
    let response = JsFuture::from(raw_invoke(cmd, JsValue::NULL))
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

/// The latest measurement, without measuring.
pub async fn get_storage_usage() -> Result<StorageStatus, String> {
    invoke("get_storage_usage").await
}

/// Measure now, or wait for the measurement already running.
pub async fn refresh_storage_usage() -> Result<StorageStatus, String> {
    invoke("refresh_storage_usage").await
}

/// Why new task executions are paused, or `None` when they are not. A fresh
/// reading of the filesystem, not the last measurement.
pub async fn get_new_work_pause() -> Result<Option<StartBlock>, String> {
    invoke("get_new_work_pause").await
}
