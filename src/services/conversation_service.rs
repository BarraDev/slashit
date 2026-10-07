use crate::models::{ConversationHumanAction, ConversationSnapshot};
use serde::Serialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(command: &str, args: JsValue) -> js_sys::Promise;
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectArgs { project_id: String }
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageArgs { project_id: String, message: String }
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ActionArgs { project_id: String, conversation_id: String, revision: u64, action_id: String, action: ConversationHumanAction }

async fn snapshot(promise: js_sys::Promise) -> Result<ConversationSnapshot, String> {
    let value = JsFuture::from(promise).await.map_err(|error| error.as_string().unwrap_or_else(|| format!("{error:?}")))?;
    serde_wasm_bindgen::from_value(value).map_err(|error| format!("Invalid Conversation response: {error}"))
}

pub async fn get_project_conversation(project_id: String) -> Result<ConversationSnapshot, String> {
    let args = serde_wasm_bindgen::to_value(&ProjectArgs { project_id }).map_err(|error| error.to_string())?;
    snapshot(raw_invoke("get_project_conversation", args)).await
}
pub async fn send_project_message(project_id: String, message: String) -> Result<ConversationSnapshot, String> {
    let args = serde_wasm_bindgen::to_value(&MessageArgs { project_id, message }).map_err(|error| error.to_string())?;
    snapshot(raw_invoke("send_project_message", args)).await
}
pub async fn act_on_project_conversation(project_id: String, conversation_id: String, revision: u64, action_id: String, action: ConversationHumanAction) -> Result<ConversationSnapshot, String> {
    let args = serde_wasm_bindgen::to_value(&ActionArgs { project_id, conversation_id, revision, action_id, action }).map_err(|error| error.to_string())?;
    snapshot(raw_invoke("act_on_project_conversation", args)).await
}
pub async fn stop_project_conversation(project_id: String) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&ProjectArgs { project_id }).map_err(|error| error.to_string())?;
    let value = JsFuture::from(raw_invoke("stop_project_conversation", args)).await.map_err(|error| error.as_string().unwrap_or_else(|| format!("{error:?}")))?;
    serde_wasm_bindgen::from_value(value).map_err(|error| format!("Invalid stop response: {error}"))
}
