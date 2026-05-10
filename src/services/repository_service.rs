use crate::models::*;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

pub async fn pick_folder() -> Result<Option<String>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("pick_folder", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn check_is_git_repo(path: String) -> Result<bool, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "path": path })).unwrap();
    let response = invoke("check_is_git_repo", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn create_repository(local_path: String, remote_url: Option<String>) -> Result<Repository, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "localPath": local_path,
        "remoteUrl": remote_url,
    })).unwrap();

    let response = invoke("create_repository", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn list_repositories() -> Result<Vec<Repository>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("list_repositories", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

