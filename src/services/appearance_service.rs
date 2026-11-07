// SPDX-License-Identifier: MPL-2.0
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

pub async fn get_use_project_rail() -> Result<bool, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("get_use_project_rail", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}
