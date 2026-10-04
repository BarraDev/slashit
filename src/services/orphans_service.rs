//! Frontend access to the orphan scan and reclaim commands.
//!
//! Uses the fallible `raw_invoke` binding: a refused reclaim must say why.

use crate::models::orphans::OrphanScan;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

async fn call<T: serde::de::DeserializeOwned>(
    cmd: &str,
    args: serde_json::Value,
) -> Result<T, String> {
    let args = serde_wasm_bindgen::to_value(&args).map_err(|e| e.to_string())?;
    let response = JsFuture::from(raw_invoke(cmd, args)).await.map_err(|value| {
        value.as_string().unwrap_or_else(|| {
            js_sys::JSON::stringify(&value)
                .ok()
                .and_then(|s| s.as_string())
                .unwrap_or_else(|| "Tauri command failed".to_string())
        })
    })?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn scan_orphans(project_id: String) -> Result<OrphanScan, String> {
    call("scan_orphans", serde_json::json!({ "projectId": project_id })).await
}

pub async fn reclaim_orphan_checkout(project_id: String, path: String) -> Result<(), String> {
    call(
        "reclaim_orphan_checkout",
        serde_json::json!({ "projectId": project_id, "path": path }),
    )
    .await
}

pub async fn reclaim_orphan_branch(project_id: String, branch: String) -> Result<(), String> {
    call(
        "reclaim_orphan_branch",
        serde_json::json!({ "projectId": project_id, "branch": branch }),
    )
    .await
}
