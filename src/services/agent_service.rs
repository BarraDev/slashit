// SPDX-License-Identifier: MPL-2.0
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub alias: Option<String>,
}

pub async fn list_available_models() -> Result<Vec<ModelInfo>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("list_available_models", args).await;
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

