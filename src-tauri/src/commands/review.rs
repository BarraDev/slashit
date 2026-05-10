use crate::domain::{Task, HumanReview};

#[tauri::command]
pub async fn submit_review(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    approved: bool,
    approver: Option<String>,
    feedback: Option<String>,
    spec_hash: Option<String>,
) -> Result<Option<Task>, String> {
    let task_id = uuid::Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let mut tasks = state.task.tasks.write().await;

    if let Some(task) = tasks.get_mut(&task_id) {
        let review = HumanReview {
            approved,
            approver,
            timestamp: Some(chrono::Utc::now()),
            feedback,
            spec_hash,
        };
        task.human_review = Some(review);
        task.updated_at = chrono::Utc::now();
        Ok(Some(task.clone()))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn check_review_valid(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    current_spec_hash: String,
) -> Result<bool, String> {
    let task_id = uuid::Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let tasks = state.task.tasks.read().await;

    if let Some(task) = tasks.get(&task_id) {
        if let Some(review) = &task.human_review {
            if let Some(stored_hash) = &review.spec_hash {
                return Ok(stored_hash == &current_spec_hash);
            }
        }
    }

    Ok(false)
}

#[tauri::command]
pub async fn get_review(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Option<HumanReview>, String> {
    let task_id = uuid::Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let tasks = state.task.tasks.read().await;

    if let Some(task) = tasks.get(&task_id) {
        Ok(task.human_review.clone())
    } else {
        Ok(None)
    }
}
