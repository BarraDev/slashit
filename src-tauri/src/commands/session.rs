use crate::domain::{ChatSession, ChatMessage, MessageRole};
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

type Sessions = Arc<RwLock<HashMap<Uuid, ChatSession>>>;
type Messages = Arc<RwLock<HashMap<Uuid, Vec<ChatMessage>>>>;

#[derive(Clone)]
pub struct SessionState {
    pub sessions: Sessions,
    pub messages: Messages,
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            messages: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

#[tauri::command]
pub async fn create_session(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    title: String,
) -> Result<ChatSession, String> {
    let id = Uuid::new_v4();
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    let now = chrono::Utc::now();

    let session = ChatSession {
        id,
        project_id,
        title,
        created_at: now,
        updated_at: now,
    };

    state.session.sessions.write().await.insert(id, session.clone());
    state.session.messages.write().await.insert(id, vec![]);

    Ok(session)
}

#[tauri::command]
pub async fn send_message(
    state: tauri::State<'_, crate::AppState>,
    session_id: String,
    message: String,
) -> Result<ChatMessage, String> {
    let session_id = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    let id = Uuid::new_v4();

    let chat_message = ChatMessage {
        id,
        session_id,
        role: MessageRole::User,
        content: message,
        timestamp: chrono::Utc::now(),
    };

    state.session
        .messages
        .write()
        .await
        .entry(session_id)
        .or_insert_with(Vec::new)
        .push(chat_message.clone());

    Ok(chat_message)
}

#[tauri::command]
pub async fn get_session_history(
    state: tauri::State<'_, crate::AppState>,
    session_id: String,
) -> Result<Vec<ChatMessage>, String> {
    let session_id = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    let messages = state.session.messages.read().await;

    Ok(messages
        .get(&session_id)
        .cloned()
        .unwrap_or_default())
}

#[tauri::command]
pub async fn list_sessions(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<Vec<ChatSession>, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    let sessions = state.session.sessions.read().await;

    Ok(sessions
        .values()
        .filter(|s| s.project_id == project_id)
        .cloned()
        .collect())
}
