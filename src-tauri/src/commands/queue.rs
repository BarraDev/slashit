use crate::config::QueueConfig;
use crate::domain::{Task, TaskStatus};
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

/// Durably, rather than through a `QueueManager` method that only ever
/// mutated the shared map: that ordering published the status change in
/// memory before persistence was attempted at all. See
/// `lifecycle::commit_task`.
///
/// `add_to_queue`/`bulk_add_to_queue`/`requeue_task` all reach this, and none
/// of them are restricted to a task with no active owner: a task selected for
/// a bulk "add to queue" or an explicit requeue can be `InProgress` or
/// `AiReview` right now. The lease and the ownership-ending guard below give
/// this the same contract `update_task_status` has -- a previous owner cannot
/// be left running under a status this write is about to overwrite.
async fn enqueue_durably(
    state: &tauri::State<'_, crate::AppState>,
    task_id: Uuid,
) -> Result<(), String> {
    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    let old_status = state
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .map(|t| t.status.clone());

    if !matches!(&old_status, None | Some(TaskStatus::Queue)) {
        let running = state
            .executor
            .get()
            .map(|e| e.as_ref() as &dyn crate::lifecycle::ExecutionOwnership);
        crate::lifecycle::end_active_ownership(running, task_id).await?;
    }

    let amend = move |staged: &mut HashMap<Uuid, Task>| {
        if let Some(task) = staged.get_mut(&task_id) {
            task.status = TaskStatus::Queue;
        }
    };
    match crate::lifecycle::commit_task(&state.task.tasks, &state.storage, task_id, &amend).await?
    {
        Some(_) => Ok(()),
        None => Err(format!("Task {} not found", task_id)),
    }
}

#[tauri::command]
pub async fn add_to_queue(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<(), String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    enqueue_durably(&state, task_id).await
}

#[tauri::command]
pub async fn bulk_add_to_queue(
    state: tauri::State<'_, crate::AppState>,
    task_ids: Vec<String>,
) -> Result<Vec<String>, String> {
    let mut results = Vec::new();

    for task_id in task_ids {
        match Uuid::parse_str(&task_id) {
            Ok(id) => match enqueue_durably(&state, id).await {
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
    let select = |tasks: &HashMap<Uuid, Task>| manager.select_promotable(tasks);
    let amend = |staged: &mut HashMap<Uuid, Task>, task_id: Uuid| {
        if let Some(task) = staged.get_mut(&task_id) {
            QueueManager::apply_promotion(task);
        }
    };
    let promoted =
        crate::lifecycle::commit_selected(&state.task.tasks, &state.storage, select, amend)
            .await?;
    Ok(promoted.map(|task| task.id.to_string()))
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
    enqueue_durably(&state, task_id).await
}

/// Unit 5C2's corrective pass, for `enqueue_durably`'s two front doors
/// (`add_to_queue`/`bulk_add_to_queue`, `requeue_task`): the same ownership
/// contract proven for `update_task_status`/`reorder_task` in
/// `crate::commands::task::lifecycle_ownership`, driven through
/// `add_to_queue` itself.
#[cfg(test)]
mod lifecycle_ownership {
    use super::*;
    use crate::test_helpers::{attach_test_executor, create_test_task_full};
    use tauri::Manager;

    async fn test_state() -> (tempfile::TempDir, crate::AppState) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let (state, _report) = crate::app_core::build_state_with_paths(paths)
            .await
            .expect("state should build under a fresh tempdir");
        (tmp, state)
    }

    async fn seed(state: &crate::AppState, status: TaskStatus) -> Uuid {
        let project_id = Uuid::new_v4();
        let task = create_test_task_full("under test", project_id, status, 0);
        let task_id = task.id;
        state.task.tasks.write().await.insert(task_id, task.clone());
        state
            .storage
            .save_project_tasks(project_id, &[task])
            .expect("seed the board");
        task_id
    }

    #[tokio::test]
    async fn add_to_queue_ends_a_running_execution_before_requeuing_the_task() {
        let (_tmp, state) = test_state().await;
        let executor = attach_test_executor(&state);
        let task_id = seed(&state, TaskStatus::InProgress).await;
        let cleaned_up = executor.register_fake_running_execution_for_test(task_id).await;

        let app = tauri::test::mock_app();
        app.manage(state);

        let result = add_to_queue(app.state(), task_id.to_string()).await;

        assert!(result.is_ok(), "{result:?}");
        assert!(
            cleaned_up.load(std::sync::atomic::Ordering::SeqCst),
            "a bulk/manual 'add to queue' must end a task's previous owner before \
             overwriting its status with Queue"
        );
        assert_eq!(executor.running_task_count().await, 0);
        let live: &crate::AppState = app.state::<crate::AppState>().inner();
        assert_eq!(
            live.task.tasks.read().await.get(&task_id).unwrap().status,
            TaskStatus::Queue
        );
    }

    #[tokio::test(start_paused = true)]
    async fn add_to_queue_refuses_when_ownership_cannot_be_ended_in_time() {
        let (_tmp, state) = test_state().await;
        let executor = attach_test_executor(&state);
        let task_id = seed(&state, TaskStatus::InProgress).await;
        executor.register_unkillable_running_execution_for_test(task_id).await;

        let app = tauri::test::mock_app();
        app.manage(state);

        let result = add_to_queue(app.state(), task_id.to_string()).await;

        assert!(result.is_err(), "{result:?}");
        let live: &crate::AppState = app.state::<crate::AppState>().inner();
        assert_eq!(
            live.task.tasks.read().await.get(&task_id).unwrap().status,
            TaskStatus::InProgress,
            "the previous durable status must survive a refused enqueue"
        );
    }
}
