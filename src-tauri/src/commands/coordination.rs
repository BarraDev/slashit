use crate::coordination::{self, Action, Snapshot};
use uuid::Uuid;

#[tauri::command]
pub async fn get_coordination(
    state: tauri::State<'_, crate::AppState>,
    task_id: Uuid,
) -> Result<Snapshot, String> {
    coordination::snapshot(&state, task_id).await
}

#[tauri::command]
pub async fn act_on_coordination(
    state: tauri::State<'_, crate::AppState>,
    task_id: Uuid,
    revision: u64,
    action: Action,
) -> Result<Snapshot, String> {
    coordination::act(&state, task_id, revision, action).await
}
