use crate::config::QueueConfig;
use crate::queue::QueueManager;
use crate::commands::task::Tasks;
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct QueueState {
    pub manager: Arc<RwLock<QueueManager>>,
    pub config: Arc<RwLock<QueueConfig>>,
}

impl QueueState {
    pub fn new(tasks: Tasks) -> Self {
        let config = QueueConfig::default();
        let manager = QueueManager::with_config(tasks);
        Self {
            manager: Arc::new(RwLock::new(manager)),
            config: Arc::new(RwLock::new(config)),
        }
    }
}

impl Default for QueueState {
    fn default() -> Self {
        Self::new(Arc::new(RwLock::new(HashMap::new())))
    }
}

#[tauri::command]
pub async fn get_queue_config(
    state: tauri::State<'_, crate::AppState>,
) -> Result<QueueConfig, String> {
    let config = state.queue.config.read().await;
    Ok(config.clone())
}

#[tauri::command]
pub async fn update_queue_config(
    state: tauri::State<'_, crate::AppState>,
    parallel_task_limit: Option<u32>,
    auto_promote: Option<bool>,
    fifo_ordering: Option<bool>,
    use_coderabbit: Option<bool>,
) -> Result<QueueConfig, String> {
    let mut config = state.queue.config.write().await;
    let mut manager = state.queue.manager.write().await;

    if let Some(limit) = parallel_task_limit {
        config.parallel_task_limit = limit;
    }
    if let Some(auto) = auto_promote {
        config.auto_promote = auto;
    }
    if let Some(fifo) = fifo_ordering {
        config.fifo_ordering = fifo;
    }
    if let Some(cr) = use_coderabbit {
        config.use_coderabbit = cr;
    }

    let new_config = config.clone();
    manager.set_config(new_config.clone()).await;

    Ok(new_config)
}

#[tauri::command]
pub async fn add_to_queue(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<(), String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let manager = state.queue.manager.read().await;
    manager.enqueue_task(task_id).await
}

#[tauri::command]
pub async fn bulk_add_to_queue(
    state: tauri::State<'_, crate::AppState>,
    task_ids: Vec<String>,
) -> Result<Vec<String>, String> {
    let manager = state.queue.manager.read().await;
    let mut results = Vec::new();

    for task_id in task_ids {
        match Uuid::parse_str(&task_id) {
            Ok(id) => match manager.enqueue_task(id).await {
                Ok(()) => results.push(format!("Added {} to queue", task_id)),
                Err(e) => results.push(format!("Failed to add {}: {}", task_id, e)),
            },
            Err(e) => results.push(format!("Invalid UUID {}: {}", task_id, e)),
        }
    }

    Ok(results)
}

#[tauri::command]
pub async fn get_queue_position(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Option<usize>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let manager = state.queue.manager.read().await;
    let queued_tasks = manager.get_queued_tasks().await;

    let position = queued_tasks
        .iter()
        .position(|t| t.id == task_id)
        .map(|p| p + 1);

    Ok(position)
}

#[tauri::command]
pub async fn promote_next_task(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Option<String>, String> {
    let manager = state.queue.manager.read().await;
    match manager.promote_next_task().await {
        Some(task_id) => Ok(Some(task_id.to_string())),
        None => Ok(None),
    }
}

#[tauri::command]
pub async fn get_queue_capacity(
    state: tauri::State<'_, crate::AppState>,
) -> Result<usize, String> {
    let manager = state.queue.manager.read().await;
    Ok(manager.get_capacity_available().await)
}

#[tauri::command]
pub async fn get_in_progress_count(
    state: tauri::State<'_, crate::AppState>,
) -> Result<usize, String> {
    let manager = state.queue.manager.read().await;
    Ok(manager.get_in_progress_count().await)
}

#[tauri::command]
pub async fn requeue_task(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<(), String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let manager = state.queue.manager.read().await;
    manager.requeue_task(task_id).await
}
