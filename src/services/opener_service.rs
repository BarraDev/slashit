use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

/// Opens `url` in the user's default external browser via the
/// `tauri-plugin-opener` plugin. Webview `<a target="_blank">` does not open
/// an external browser by itself, so call this from click handlers.
pub async fn open_url_external(url: String) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "url": url }))
        .map_err(|e| e.to_string())?;
    let _ = invoke("plugin:opener|open_url", args).await;
    Ok(())
}
