use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

pub async fn force_quit() -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    invoke("force_quit", args).await;
    Ok(())
}

pub async fn get_active_process_count() -> Result<serde_json::Value, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("get_active_process_count", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}
