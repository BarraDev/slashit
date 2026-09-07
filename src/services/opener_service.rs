use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

fn js_error_to_string(value: JsValue) -> String {
    value.as_string().unwrap_or_else(|| {
        js_sys::JSON::stringify(&value)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_else(|| "Tauri command failed".to_string())
    })
}

/// Opens `url` in the user's default external browser via the
/// `tauri-plugin-opener` plugin. Webview `<a target="_blank">` does not open
/// an external browser by itself, so call this from click handlers.
///
/// Uses the fallible `raw_invoke` binding rather than the `-> JsValue` one
/// most services use: a rejected `open_url` call (no default browser
/// configured, no handler registered for the URL scheme, plugin permission
/// denied, ...) must be reported to the caller, not silently treated as a
/// successful open.
pub async fn open_url_external(url: String) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "url": url }))
        .map_err(|e| e.to_string())?;
    JsFuture::from(raw_invoke("plugin:opener|open_url", args))
        .await
        .map_err(js_error_to_string)?;
    Ok(())
}
