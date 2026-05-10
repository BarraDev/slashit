use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], js_name = "listen")]
    fn tauri_event_listen(event: &str, handler: &Closure<dyn Fn(JsValue)>) -> js_sys::Promise;
}

/// Progress event emitted by the backend `address_pr_review` per item / phase.
/// Mirrors `commands::pr::PrReviewProgress` on the backend.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct PrReviewProgress {
    pub task_id: String,
    pub kind: String,
    #[serde(default)]
    pub current: Option<usize>,
    #[serde(default)]
    pub total: Option<usize>,
    #[serde(default)]
    pub comment_id: Option<u64>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Subscribe to backend `pr-review-progress` Tauri events. Returns an opaque
/// guard whose `Drop` is a no-op — the closure must be kept alive (it is
/// `.forget()`ed inside) for the lifetime of the page. Calling repeatedly
/// adds independent listeners; callers should subscribe once per modal open
/// and ignore events whose `task_id` does not match.
pub fn subscribe_pr_review_progress<F: Fn(PrReviewProgress) + 'static>(handler: F) {
    let cb = Closure::wrap(Box::new(move |event: JsValue| {
        // Tauri delivers `{ event, id, payload }` — pull `payload` and decode.
        let payload = js_sys::Reflect::get(&event, &JsValue::from_str("payload"))
            .unwrap_or(JsValue::NULL);
        match serde_wasm_bindgen::from_value::<PrReviewProgress>(payload) {
            Ok(ev) => handler(ev),
            Err(e) => leptos::logging::warn!("[pr-review] bad progress payload: {:?}", e),
        }
    }) as Box<dyn Fn(JsValue)>);
    let promise = tauri_event_listen("pr-review-progress", &cb);
    wasm_bindgen_futures::spawn_local(async move {
        let _ = JsFuture::from(promise).await;
    });
    cb.forget();
}

async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, String> {
    JsFuture::from(raw_invoke(cmd, args))
        .await
        .map_err(js_error_to_string)
}

fn js_error_to_string(value: JsValue) -> String {
    value.as_string().unwrap_or_else(|| {
        js_sys::JSON::stringify(&value)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_else(|| "Tauri command failed".to_string())
    })
}

pub async fn create_pr(task_id: String) -> Result<String, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id })).unwrap();
    let response = invoke("create_pr", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn bulk_create_prs(task_ids: Vec<String>) -> Result<Vec<String>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskIds": task_ids })).unwrap();
    let response = invoke("bulk_create_prs", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn sync_existing_pr(task_id: String) -> Result<Option<crate::models::Task>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id })).unwrap();
    let response = invoke("sync_existing_pr", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PrCandidate {
    pub url: String,
    pub number: u32,
    pub title: String,
    pub state: String,
    pub head_ref_name: String,
    pub reason: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PrPushRecoveryPlan {
    pub branch_name: String,
    pub commit_sha: String,
    pub commit_subject: String,
    pub author_name: String,
    pub author_email: String,
    pub suggested_email: Option<String>,
}

pub async fn find_pr_candidates(task_id: String) -> Result<Vec<PrCandidate>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id })).unwrap();
    let response = invoke("find_pr_candidates", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn get_pr_push_recovery(task_id: String) -> Result<PrPushRecoveryPlan, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id })).unwrap();
    let response = invoke("get_pr_push_recovery", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn recover_private_email_and_create_pr(task_id: String, author_email: String) -> Result<String, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "taskId": task_id,
        "authorEmail": author_email,
    })).unwrap();
    let response = invoke("recover_private_email_and_create_pr", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn link_existing_pr(task_id: String, pr_url: String) -> Result<Option<crate::models::Task>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "taskId": task_id,
        "prUrl": pr_url,
    })).unwrap();
    let response = invoke("link_pr", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn analyze_pr_comments(task_id: String) -> Result<crate::models::task::PrReviewPlan, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id })).unwrap();
    let response = invoke("analyze_pr_comments", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AddressPrReviewOptions {
    pub auto_push: bool,
    pub auto_reply: bool,
    #[serde(default)]
    pub dry_run: bool,
}

pub async fn address_pr_review(
    task_id: String,
    plan: crate::models::task::PrReviewPlan,
    options: AddressPrReviewOptions,
) -> Result<crate::models::task::PrReviewApplyResult, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "taskId": task_id,
        "plan": plan,
        "options": options,
    })).unwrap();
    let response = invoke("address_pr_review", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn discuss_pr_review_questions(
    task_id: String,
    plan: crate::models::task::PrReviewPlan,
) -> Result<crate::models::task::PrReviewPlan, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "taskId": task_id,
        "plan": plan,
    })).unwrap();
    let response = invoke("discuss_pr_review_questions", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

/// Result of `sync_pr_review_replies` — mirrors `commands::pr::SyncPrRepliesResult`.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct SyncPrRepliesResult {
    pub replied: u32,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub already_done: u32,
    #[serde(default)]
    pub fix_pending: u32,
}

pub async fn sync_pr_review_replies(task_id: String) -> Result<SyncPrRepliesResult, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id })).unwrap();
    let response = invoke("sync_pr_review_replies", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn refresh_task_pr_state(task_id: String) -> Result<Option<crate::models::Task>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "taskId": task_id })).unwrap();
    let response = invoke("refresh_task_pr_state", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}
