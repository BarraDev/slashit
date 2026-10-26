use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

async fn invoke_diff(cmd: &str, task_id: String) -> Result<String, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id }))
        .map_err(|e| e.to_string())?;
    let response = JsFuture::from(raw_invoke(cmd, args)).await.map_err(|e| {
        e.as_string().unwrap_or_else(|| {
            js_sys::JSON::stringify(&e)
                .ok()
                .and_then(|s| s.as_string())
                .unwrap_or_else(|| format!("{cmd} failed"))
        })
    })?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn get_task_diff(task_id: String) -> Result<String, String> {
    invoke_diff("get_task_diff", task_id).await
}

pub async fn get_task_diff_stat(task_id: String) -> Result<String, String> {
    invoke_diff("get_task_diff_stat", task_id).await
}
