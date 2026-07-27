//! Frontend access to the state-location commands.
//!
//! Uses the fallible `raw_invoke` binding rather than the `-> JsValue` one most
//! services use: a migration that fails must say so. The other binding has no
//! rejection path and would silently report success.

use crate::models::state_location::{
    ConflictPolicy, MigrationPlan, MigrationReport, StateLocation, StateLocationInfo,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, String> {
    JsFuture::from(raw_invoke(cmd, args))
        .await
        .map_err(js_error_to_string)
}

fn js_error_to_string(value: JsValue) -> String {
    value.as_string().unwrap_or_else(|| {
        js_sys::JSON::stringify(&value)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_else(|| "Tauri command failed".to_string())
    })
}

pub async fn get_state_location(project_id: String) -> Result<StateLocationInfo, String> {
    // Tauri expects camelCase parameter names on the wire.
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "projectId": project_id }))
        .map_err(|e| e.to_string())?;
    let response = invoke("get_state_location", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn plan_state_migration(
    project_id: String,
    target: StateLocation,
) -> Result<MigrationPlan, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "projectId": project_id,
        "target": target,
    }))
    .map_err(|e| e.to_string())?;
    let response = invoke("plan_state_migration", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn apply_state_migration(
    project_id: String,
    target: StateLocation,
    policy: Option<ConflictPolicy>,
) -> Result<MigrationReport, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "projectId": project_id,
        "target": target,
        "policy": policy,
    }))
    .map_err(|e| e.to_string())?;
    let response = invoke("apply_state_migration", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

/// Record the preference without moving any files.
pub async fn set_state_location(
    project_id: String,
    location: StateLocation,
) -> Result<StateLocationInfo, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "projectId": project_id,
        "location": location,
    }))
    .map_err(|e| e.to_string())?;
    let response = invoke("set_state_location", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}
