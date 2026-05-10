use crate::domain::AgentLogEntry;
use uuid::Uuid;

#[tauri::command]
pub async fn execute_task(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<(), String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let executor = state.executor.get()
        .ok_or("Executor not initialized")?;
    executor.execute_task(task_id).await
}

#[tauri::command]
pub async fn stop_task_execution(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<(), String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let executor = state.executor.get()
        .ok_or("Executor not initialized")?;
    executor.stop_task(task_id).await
}

#[tauri::command]
pub async fn get_execution_status(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Option<String>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let tasks = state.task.tasks.read().await;
    Ok(tasks.get(&task_id).map(|t| format!("{:?}", t.phase)))
}

#[tauri::command]
pub async fn get_task_output(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Vec<AgentLogEntry>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let executor = state.executor.get()
        .ok_or("Executor not initialized")?;
    Ok(executor.get_task_output(task_id).await)
}
