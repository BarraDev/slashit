use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

pub async fn create_pr(task_id: String) -> Result<String, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id })).unwrap();
    let response = invoke("create_pr", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn bulk_create_prs(task_ids: Vec<String>) -> Result<Vec<String>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskIds": task_ids })).unwrap();
    let response = invoke("bulk_create_prs", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

