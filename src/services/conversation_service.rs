// SPDX-License-Identifier: MPL-2.0
use crate::models::{ConversationDraft, ConversationExecution, ConversationHumanAction, ConversationSnapshot, ConversationsConfig, SendShortcut};
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
struct RetryArgs { project_id: String, conversation_id: String, revision: u64, action_id: String }
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageArgs { project_id: String, message: String }
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RetryMessageArgs { project_id: String, conversation_id: String, revision: u64 }
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
/// The execution views alone. Read-only on the backend, so the Conversation can refresh them without the restart reconciliation of `get_project_conversation`.
pub async fn get_project_conversation_executions(project_id: String) -> Result<Vec<ConversationExecution>, String> {
    let args = serde_wasm_bindgen::to_value(&ProjectArgs { project_id }).map_err(|error| error.to_string())?;
    let value = JsFuture::from(raw_invoke("get_project_conversation_executions", args)).await.map_err(|error| error.as_string().unwrap_or_else(|| format!("{error:?}")))?;
    serde_wasm_bindgen::from_value(value).map_err(|error| format!("Invalid execution response: {error}"))
}
pub async fn open_project_conversation(project_id: String) -> Result<ConversationSnapshot, String> {
    let args = serde_wasm_bindgen::to_value(&ProjectArgs { project_id }).map_err(|error| error.to_string())?;
    snapshot(raw_invoke("open_project_conversation", args)).await
}
pub async fn retry_project_conversation_continuation(project_id: String, conversation_id: String, revision: u64, action_id: String) -> Result<ConversationSnapshot, String> {
    let args = serde_wasm_bindgen::to_value(&RetryArgs { project_id, conversation_id, revision, action_id }).map_err(|error| error.to_string())?;
    snapshot(raw_invoke("retry_project_conversation_continuation", args)).await
}
pub async fn retry_project_message(project_id: String, conversation_id: String, revision: u64) -> Result<ConversationSnapshot, String> {
    let args = serde_wasm_bindgen::to_value(&RetryMessageArgs { project_id, conversation_id, revision }).map_err(|error| error.to_string())?;
    snapshot(raw_invoke("retry_project_message", args)).await
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DraftSaveArgs { project_id: String, draft: ConversationDraft }
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SendShortcutArgs { send_shortcut: SendShortcut }

async fn call<T: serde::de::DeserializeOwned>(promise: js_sys::Promise, what: &str) -> Result<T, String> {
    let value = JsFuture::from(promise).await.map_err(|error| error.as_string().unwrap_or_else(|| format!("{error:?}")))?;
    serde_wasm_bindgen::from_value(value).map_err(|error| format!("Invalid {what} response: {error}"))
}

/// The saved composer draft of a Project, if any.
pub async fn get_conversation_draft(project_id: String) -> Result<Option<ConversationDraft>, String> {
    let args = serde_wasm_bindgen::to_value(&ProjectArgs { project_id }).map_err(|error| error.to_string())?;
    call(raw_invoke("get_conversation_draft", args), "draft").await
}
/// Save a draft. `Ok(false)` means the backend kept a newer draft; the caller must reload it.
pub async fn save_conversation_draft(project_id: String, draft: ConversationDraft) -> Result<bool, String> {
    let args = serde_wasm_bindgen::to_value(&DraftSaveArgs { project_id, draft }).map_err(|error| error.to_string())?;
    call(raw_invoke("save_conversation_draft", args), "draft save").await
}
pub async fn get_conversations_config() -> Result<ConversationsConfig, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).map_err(|error| error.to_string())?;
    call(raw_invoke("get_conversations_config", args), "Conversation settings").await
}
pub async fn update_conversations_config(send_shortcut: SendShortcut) -> Result<ConversationsConfig, String> {
    let args = serde_wasm_bindgen::to_value(&SendShortcutArgs { send_shortcut }).map_err(|error| error.to_string())?;
    call(raw_invoke("update_conversations_config", args), "Conversation settings").await
}
