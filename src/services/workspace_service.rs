use crate::models::workspace::Workspace;
use serde::Serialize;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    fn invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

#[derive(Serialize)]
struct CreateArgs<'a> {
    name: &'a str,
    #[serde(rename = "rootPath")]
    root_path: &'a str,
}

#[derive(Serialize)]
struct IdArgs<'a> {
    #[serde(rename = "workspaceId")]
    workspace_id: &'a str,
}

#[derive(Serialize)]
struct Empty;

/// A rejected Tauri invocation carries the backend's `String` error as a
/// `JsValue`. `{:?}`-formatting it renders `JsValue("…")` verbatim — this
/// unwraps to the actual message so callers (toasts, etc.) show the real
/// error text instead of its debug wrapper.
fn js_error_to_string(value: JsValue) -> String {
    value.as_string().unwrap_or_else(|| {
        js_sys::JSON::stringify(&value)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_else(|| "Tauri command failed".to_string())
    })
}

pub async fn create_workspace(name: &str, root_path: &str) -> Result<Workspace, String> {
    let args = serde_wasm_bindgen::to_value(&CreateArgs { name, root_path })
        .map_err(|e| e.to_string())?;
    let result = JsFuture::from(invoke("create_workspace", args))
        .await
        .map_err(js_error_to_string)?;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

pub async fn list_workspaces() -> Result<Vec<Workspace>, String> {
    let args = serde_wasm_bindgen::to_value(&Empty).map_err(|e| e.to_string())?;
    let result = JsFuture::from(invoke("list_workspaces", args))
        .await
        .map_err(js_error_to_string)?;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

pub async fn get_workspace(workspace_id: &str) -> Result<Workspace, String> {
    let args = serde_wasm_bindgen::to_value(&IdArgs { workspace_id })
        .map_err(|e| e.to_string())?;
    let result = JsFuture::from(invoke("get_workspace", args))
        .await
        .map_err(js_error_to_string)?;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

pub async fn delete_workspace(workspace_id: &str) -> Result<bool, String> {
    let args = serde_wasm_bindgen::to_value(&IdArgs { workspace_id })
        .map_err(|e| e.to_string())?;
    let result = JsFuture::from(invoke("delete_workspace", args))
        .await
        .map_err(js_error_to_string)?;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}
