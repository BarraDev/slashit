use crate::domain::{Task, QaSignoff, QaStatus};
use uuid::Uuid;

#[tauri::command]
pub async fn submit_qa_review(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    status: QaStatus,
    issues_found: Vec<String>,
    session_id: String,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let session_id = Uuid::parse_str(&session_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        let signoff = QaSignoff {
            status,
            issues_found,
            timestamp: chrono::Utc::now(),
            session_id,
        };
        task.qa_signoff = Some(signoff);
        task.updated_at = chrono::Utc::now();
        Ok(Some(task.clone()))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn get_qa_history(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Option<QaSignoff>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let tasks = state.task.tasks.read().await;

    if let Some(task) = tasks.get(&task_id) {
        Ok(task.qa_signoff.clone())
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn check_recurring_issues(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    issue: String,
) -> Result<bool, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let tasks = state.task.tasks.read().await;

    if let Some(task) = tasks.get(&task_id) {
        if let Some(signoff) = &task.qa_signoff {
            let count = signoff.issues_found.iter()
                .filter(|i| i.to_lowercase() == issue.to_lowercase())
                .count();
            return Ok(count >= 3);
        }
    }

    Ok(false)
}
