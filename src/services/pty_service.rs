use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"])]
    async fn listen(event: &str, handler: &Closure<dyn Fn(JsValue)>) -> JsValue;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PtyInfo {
    pub id: String,
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub is_new: bool,
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PtyOutput {
    #[serde(rename = "session_id")]
    pub session_id: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PtyExit {
    #[serde(rename = "session_id")]
    pub session_id: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    pub reason: String,
}

/// Spawn a new PTY session
pub async fn spawn_pty(name: Option<String>, cols: Option<u16>, rows: Option<u16>, working_directory: Option<String>, project_id: Option<String>) -> Result<PtyInfo, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "name": name,
        "cols": cols,
        "rows": rows,
        "workingDirectory": working_directory,
        "projectId": project_id,
    })).map_err(|e| e.to_string())?;

    let result = invoke("spawn_pty", args).await;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

/// Write data to a PTY session
pub async fn write_pty(session_id: String, data: Vec<u8>) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "sessionId": session_id,
        "data": data,
    })).map_err(|e| e.to_string())?;

    invoke("write_pty", args).await;
    Ok(())
}

/// Resize a PTY session
pub async fn resize_pty(session_id: String, cols: u16, rows: u16) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "sessionId": session_id,
        "cols": cols,
        "rows": rows,
    })).map_err(|e| e.to_string())?;

    invoke("resize_pty", args).await;
    Ok(())
}

/// Kill a PTY session
pub async fn kill_pty(session_id: String) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "sessionId": session_id,
    })).map_err(|e| e.to_string())?;

    invoke("kill_pty", args).await;
    Ok(())
}

/// List all PTY sessions
pub async fn list_pty_sessions() -> Result<Vec<PtyInfo>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({}))
        .map_err(|e| e.to_string())?;
    let result = invoke("list_pty_sessions", args).await;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

/// Attach to an existing PTY session
pub async fn attach_pty_session(session_id: String) -> Result<PtyInfo, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "sessionId": session_id,
    })).map_err(|e| e.to_string())?;

    let result = invoke("attach_pty_session", args).await;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

/// Write data to all active PTY sessions
pub async fn write_to_all_ptys(data: Vec<u8>) -> Result<u32, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "data": data,
    })).map_err(|e| e.to_string())?;

    let result = invoke("write_to_all_ptys", args).await;
    if let Some(count) = result.as_f64() {
        Ok(count as u32)
    } else {
        Ok(0)
    }
}

/// Invoke a command on all terminals (writes command + Enter)
pub async fn invoke_all_terminals(command: &str) -> Result<u32, String> {
    let mut data = command.as_bytes().to_vec();
    data.push(b'\r');
    write_to_all_ptys(data).await
}

/// Get scrollback buffer for a session
pub async fn get_pty_scrollback(session_id: String) -> Result<Vec<u8>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "sessionId": session_id,
    })).map_err(|e| e.to_string())?;

    let result = invoke("get_pty_scrollback", args).await;
    if let Some(array) = result.dyn_ref::<js_sys::Array>() {
        let bytes: Vec<u8> = array
            .iter()
            .filter_map(|v| v.as_f64().map(|n| n as u8))
            .collect();
        Ok(bytes)
    } else {
        Err("Failed to parse scrollback data".to_string())
    }
}

/// Register a Tauri event listener
pub fn tauri_listen(event: &str, handler: &Closure<dyn Fn(JsValue)>) {
    wasm_bindgen_futures::spawn_local({
        let event = event.to_string();
        let handler_ref = handler.as_ref().unchecked_ref::<js_sys::Function>().clone();
        async move {
            let window = web_sys::window().expect("no window");
            let tauri = js_sys::Reflect::get(&window, &JsValue::from_str("__TAURI__"))
                .expect("no __TAURI__");
            let event_mod = js_sys::Reflect::get(&tauri, &JsValue::from_str("event"))
                .expect("no event module");
            let listen_fn = js_sys::Reflect::get(&event_mod, &JsValue::from_str("listen"))
                .expect("no listen function");
            let listen_fn: js_sys::Function = listen_fn.unchecked_into();
            let promise = listen_fn.call2(&JsValue::NULL, &JsValue::from_str(&event), &handler_ref)
                .expect("listen call failed");
            let promise: js_sys::Promise = promise.unchecked_into();
            let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
        }
    });
}
