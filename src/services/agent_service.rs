// SPDX-License-Identifier: MPL-2.0
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

/// One model the runtime offers.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeModel {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelCatalog {
    pub provider: String,
    pub models: Vec<RuntimeModel>,
    pub error: Option<String>,
}

/// The models for `provider` (`claude` or `opencode`). `refresh` runs discovery
/// again; otherwise a cached list is returned.
pub async fn list_runtime_models(provider: &str, refresh: bool) -> Result<ModelCatalog, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "provider": provider, "refresh": refresh }))
        .map_err(|e| e.to_string())?;
    let response = invoke("list_runtime_models", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ClaudeCliStatus {
    pub installed: bool,
    pub version: Option<String>,
    pub path: Option<String>,
}

pub async fn check_claude_cli() -> Result<ClaudeCliStatus, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("check_claude_cli", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

