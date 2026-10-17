use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

pub async fn get_task_diff(task_id: String) -> Result<String, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id })).unwrap();
    let response = invoke("get_task_diff", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn get_task_diff_stat(task_id: String) -> Result<String, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id })).unwrap();
    let response = invoke("get_task_diff_stat", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}
