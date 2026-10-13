use crate::config::QueueConfig;
use crate::domain::{Task, TaskStatus};
use crate::queue::QueueManager;
use crate::commands::task::Tasks;
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// The Queue's runtime state.
///
/// The [`QueueManager`] owns the one live copy of the [`QueueConfig`]; the
/// executor reads it from there, and `config.toml` is where it is saved. There
/// is no second copy to drift.
#[derive(Clone)]
pub struct QueueState {
    pub manager: Arc<RwLock<QueueManager>>,
}

impl QueueState {
    /// A Queue running under `config`, which the caller loaded from
    /// `config.toml` (or defaulted, on a fresh installation).
    pub fn new(tasks: Tasks, config: QueueConfig) -> Self {
        Self {
            manager: Arc::new(RwLock::new(QueueManager::new(tasks, config))),
        }
    }
}

impl Default for QueueState {
    fn default() -> Self {
        Self::new(Arc::new(RwLock::new(HashMap::new())), QueueConfig::default())
    }
}

#[tauri::command]
pub async fn get_queue_config(
    state: tauri::State<'_, crate::AppState>,
) -> Result<QueueConfig, String> {
    Ok(state.queue.manager.read().await.config().clone())
}

#[tauri::command]
pub async fn update_queue_config(
    state: tauri::State<'_, crate::AppState>,
    parallel_task_limit: Option<u32>,
    auto_promote: Option<bool>,
    fifo_ordering: Option<bool>,
    use_coderabbit: Option<bool>,
) -> Result<QueueConfig, String> {
    let patch = QueueConfigPatch {
        parallel_task_limit,
        auto_promote,
        fifo_ordering,
        use_coderabbit,
    };
    let new_config = apply_queue_config_patch(&state.queue, &state.storage, patch).await?;

    // The executor also reconciles before every admission decision; doing it
    // here as well makes the new limit effective now rather than at the next
    // poll. Absent in a process with no executor yet, which has nothing to
    // reconcile.
    if let Some(executor) = state.executor.get() {
        executor.reconcile_admission().await;
    }

    Ok(new_config)
}

/// The fields of a Queue settings update that were actually supplied.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct QueueConfigPatch {
    pub parallel_task_limit: Option<u32>,
    pub auto_promote: Option<bool>,
    pub fifo_ordering: Option<bool>,
    pub use_coderabbit: Option<bool>,
}

/// Validate, save and then apply a Queue settings update.
///
/// Saving comes first and the running Queue changes only if it succeeds: an
/// update reported as failed must leave the running Queue as it was, and an
/// update reported as succeeded must survive a restart. The manager's write
/// lock is held across both, so two concurrent updates cannot interleave
/// their read-modify-write.
pub(crate) async fn apply_queue_config_patch(
    queue: &QueueState,
    storage: &crate::config::Storage,
    patch: QueueConfigPatch,
) -> Result<QueueConfig, String> {
    let mut manager = queue.manager.write().await;

    let mut next = manager.config().clone();
    if let Some(limit) = patch.parallel_task_limit {
        next.parallel_task_limit = limit;
    }
    if let Some(auto) = patch.auto_promote {
        next.auto_promote = auto;
    }
    if let Some(fifo) = patch.fifo_ordering {
        next.fifo_ordering = fifo;
    }
    if let Some(cr) = patch.use_coderabbit {
        next.use_coderabbit = cr;
    }
    next.validate()?;

    let persisted = next.clone();
    storage
        .update_config(move |config| config.queue = persisted)
        .map_err(|e| format!("Queue settings were not saved: {e}"))?;

    manager.set_config(next.clone()).await;
    Ok(next)
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
        .map(|t| (t.status.clone(), t.integration_unfinished_refusal()));
    let (old_status, unfinished_integration) = match old_status {
        Some((status, refusal)) => (Some(status), refusal),
        None => (None, None),
    };

    // Queueing withdraws the approval a pending local integration is
    // resumed from; refused before an owner is ended or anything is written.
    if old_status != Some(TaskStatus::Queue) {
        if let Some(refusal) = unfinished_integration {
            return Err(refusal);
        }
    }

    if !matches!(&old_status, None | Some(TaskStatus::Queue)) {
        let running = state
            .executor
            .get()
            .map(|e| e.as_ref() as &dyn crate::lifecycle::ExecutionOwnership);
        crate::lifecycle::end_active_ownership(running, task_id).await?;
    }

    let amend = move |staged: &mut HashMap<Uuid, Task>| {
        if let Some(task) = staged.get_mut(&task_id) {
            let from = task.status.clone();
            task.status = TaskStatus::Queue;
            task.record_move(&from);
            // Queued to run again: an approval of the current changes no
            // longer covers what the branch will hold.
            task.human_review.withdraw_approval();
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
    // Promotion asks the executor to start the task, so it waits while new
    // work is paused for disk space; the task stays queued.
    state.start_guard.check().await.map_err(|block| block.to_string())?;
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

/// The Coordinator's approved enqueue and the board's single-Task enqueue
/// (`reorder_task` with `expected_status`, which the drawer's Start and the
/// card menu's "Add to Queue" reach) must leave a Task in the same state.
#[cfg(test)]
mod enqueue_parity {
    use super::*;
    use crate::test_helpers::create_test_task_full;
    use tauri::Manager;

    #[tokio::test]
    async fn the_lifecycle_enqueue_matches_the_boards_single_task_enqueue() {
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

        let project_id = Uuid::new_v4();
        let mut board = Vec::new();
        for (title, status, position) in [
            ("waiting a", TaskStatus::Queue, 0),
            ("waiting b", TaskStatus::Queue, 1),
            ("via board", TaskStatus::Backlog, 0),
            ("via lifecycle", TaskStatus::Backlog, 1),
        ] {
            let mut task = create_test_task_full(title, project_id, status, position);
            // Leftovers an enqueue must reset, identically on both paths.
            task.phase_progress = 40;
            task.overall_progress = 40;
            task.error_message = Some("old failure".into());
            board.push(task);
        }
        let by_board = board[2].id;
        let by_lifecycle = board[3].id;
        {
            let mut tasks = state.task.tasks.write().await;
            for task in &board {
                tasks.insert(task.id, task.clone());
            }
        }
        state.storage.save_project_tasks(project_id, &board).expect("seed the board");

        let app = tauri::test::mock_app();
        app.manage(state);
        let live: &crate::AppState = app.state::<crate::AppState>().inner();

        crate::commands::task::reorder_task(
            app.state(),
            by_board.to_string(),
            Some(TaskStatus::Queue),
            0,
            None,
            Some(TaskStatus::Backlog),
        )
        .await
        .expect("board enqueue");

        let board_after = live.task.tasks.read().await[&by_board].clone();

        // Same starting column for the second Task: restore the neighbours.
        {
            let mut tasks = live.task.tasks.write().await;
            for task in &board[..2] {
                tasks.insert(task.id, task.clone());
            }
            tasks.remove(&by_board);
        }
        let verdict = crate::lifecycle::enqueue_leased(
            &live.task.tasks,
            &live.storage,
            project_id,
            by_lifecycle,
            &TaskStatus::Backlog,
        )
        .await
        .expect("lifecycle enqueue");
        assert!(matches!(verdict, crate::lifecycle::EnqueueVerdict::Queued(_)), "{verdict:?}");

        let tasks = live.task.tasks.read().await;
        let after = &tasks[&by_lifecycle];
        assert_eq!(after.status, TaskStatus::Queue);
        assert_eq!(after.position, 0);
        assert_eq!((after.phase_progress, after.overall_progress), (0, 0));
        assert_eq!(after.error_message, None);
        assert_eq!(after.phase, crate::domain::TaskPhase::Idle);
        assert_eq!(tasks[&board[0].id].position, 1);
        assert_eq!(tasks[&board[1].id].position, 2);
        let kinds = |task: &Task| {
            task.activity.iter().map(|entry| format!("{:?}", entry.kind)).collect::<Vec<_>>()
        };
        assert_eq!(
            (after.status.clone(), after.position, after.phase.clone(), after.phase_progress, after.overall_progress),
            (board_after.status.clone(), board_after.position, board_after.phase.clone(), board_after.phase_progress, board_after.overall_progress),
        );
        assert_eq!(after.error_message, board_after.error_message);
        assert_eq!(after.human_review, board_after.human_review);
        assert_eq!(kinds(after), kinds(&board_after), "both paths record the same activity");
        assert_eq!(kinds(after).len(), 1, "one move is recorded: {:?}", kinds(after));
        assert!(kinds(after)[0].contains("Moved"), "{:?}", kinds(after));
    }
}

#[cfg(test)]
mod queue_settings {
    use super::*;
    use crate::config::paths::AppPaths;
    use crate::config::Storage;

    fn paths_in(root: &std::path::Path) -> Arc<AppPaths> {
        Arc::new(AppPaths::with_roots(
            root.join("config"),
            root.join("data"),
            root.join("cache"),
            root.join("runtime"),
        ))
    }

    /// Start the application over `paths`, as a launch does.
    async fn launch(paths: &Arc<AppPaths>) -> crate::AppState {
        crate::app_core::build_state_with_paths(paths.clone())
            .await
            .expect("build state")
            .0
    }

    async fn running(state: &crate::AppState) -> QueueConfig {
        state.queue.manager.read().await.config().clone()
    }

    /// Every field different from its default, so a field that is not saved
    /// or not restored shows.
    fn every_field_changed() -> QueueConfigPatch {
        QueueConfigPatch {
            parallel_task_limit: Some(7),
            auto_promote: Some(false),
            fifo_ordering: Some(false),
            use_coderabbit: Some(false),
        }
    }

    #[tokio::test]
    async fn a_fresh_installation_runs_under_the_defaults() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = launch(&paths_in(tmp.path())).await;

        assert_eq!(running(&state).await, QueueConfig::default());
        assert_eq!(
            QueueConfig::default(),
            QueueConfig {
                parallel_task_limit: 3,
                auto_promote: true,
                fifo_ordering: true,
                use_coderabbit: true,
            }
        );
    }

    #[tokio::test]
    async fn every_field_is_saved_and_applied() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let state = launch(&paths).await;

        let applied = apply_queue_config_patch(&state.queue, &state.storage, every_field_changed())
            .await
            .expect("update");

        let expected = QueueConfig {
            parallel_task_limit: 7,
            auto_promote: false,
            fifo_ordering: false,
            use_coderabbit: false,
        };
        assert_eq!(applied, expected);
        assert_eq!(running(&state).await, expected, "the running Queue");
        assert_eq!(
            Storage::with_paths((*paths).clone()).load_config().unwrap().queue,
            expected,
            "the saved file"
        );
    }

    #[tokio::test]
    async fn an_update_names_only_the_fields_it_changes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = launch(&paths_in(tmp.path())).await;

        let applied = apply_queue_config_patch(
            &state.queue,
            &state.storage,
            QueueConfigPatch {
                auto_promote: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(
            applied,
            QueueConfig {
                auto_promote: false,
                ..QueueConfig::default()
            }
        );
    }

    #[tokio::test]
    async fn auto_promote_off_survives_a_restart() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        {
            let state = launch(&paths).await;
            apply_queue_config_patch(
                &state.queue,
                &state.storage,
                QueueConfigPatch {
                    auto_promote: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }

        let restarted = launch(&paths).await;

        assert!(
            !running(&restarted).await.auto_promote,
            "a restart must not turn automatic promotion back on"
        );
    }

    #[tokio::test]
    async fn a_custom_limit_survives_a_restart_and_a_second_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        {
            let state = launch(&paths).await;
            apply_queue_config_patch(&state.queue, &state.storage, every_field_changed())
                .await
                .unwrap();
        }
        {
            let second = launch(&paths).await;
            assert_eq!(running(&second).await.parallel_task_limit, 7);
        }

        let third = launch(&paths).await;
        assert_eq!(running(&third).await.parallel_task_limit, 7);
        assert!(!running(&third).await.use_coderabbit);
        assert!(!running(&third).await.fifo_ordering);
    }

    #[tokio::test]
    async fn re_enabling_auto_promote_is_saved_too() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let state = launch(&paths).await;
        for auto in [false, true] {
            apply_queue_config_patch(
                &state.queue,
                &state.storage,
                QueueConfigPatch {
                    auto_promote: Some(auto),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }

        assert!(running(&launch(&paths).await).await.auto_promote);
    }

    #[tokio::test]
    async fn a_failed_write_is_reported_and_changes_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        // The configuration directory is a file, so the settings cannot be
        // written. Deterministic, with no permission or full-disk tricks.
        std::fs::write(tmp.path().join("config"), b"not a directory").unwrap();
        let paths = paths_in(tmp.path());
        let queue = QueueState::default();
        let storage = Storage::with_paths((*paths).clone());

        let error = apply_queue_config_patch(&queue, &storage, every_field_changed())
            .await
            .expect_err("the write cannot succeed");

        assert!(error.contains("not saved"), "{error}");
        assert_eq!(
            queue.manager.read().await.config(),
            &QueueConfig::default(),
            "a settings change that was not saved must not be applied either"
        );
    }

    #[tokio::test]
    async fn a_limit_the_queue_cannot_run_under_is_refused_before_anything_is_saved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let state = launch(&paths).await;

        for limit in [0, 11] {
            let error = apply_queue_config_patch(
                &state.queue,
                &state.storage,
                QueueConfigPatch {
                    parallel_task_limit: Some(limit),
                    ..Default::default()
                },
            )
            .await
            .expect_err("out of range");
            assert!(error.contains("between 1 and 10"), "{error}");
        }

        assert_eq!(running(&state).await, QueueConfig::default());
        assert!(
            !paths.config_file().exists(),
            "a refused update must not have written the file"
        );
    }

    #[tokio::test]
    async fn a_config_written_before_queue_settings_existed_loads_the_defaults_and_keeps_its_other_sections() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(
            paths.config_file(),
            "[ui_preferences]\ntheme = \"light\"\nsidebar_width = 410\n\n[jj_config]\ndefault_branch = \"trunk\"\n",
        )
        .unwrap();

        let state = launch(&paths).await;
        assert_eq!(running(&state).await, QueueConfig::default());

        apply_queue_config_patch(
            &state.queue,
            &state.storage,
            QueueConfigPatch {
                auto_promote: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let saved = Storage::with_paths((*paths).clone()).load_config().unwrap();
        assert!(!saved.queue.auto_promote);
        assert_eq!(saved.ui_preferences.theme, "light");
        assert_eq!(saved.ui_preferences.sidebar_width, 410);
        assert_eq!(saved.jj_config.default_branch, "trunk");
    }

    #[tokio::test]
    async fn a_partial_queue_section_keeps_what_it_says_and_defaults_the_rest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file(), "[queue]\nauto_promote = false\n").unwrap();

        let state = launch(&paths).await;

        assert_eq!(
            running(&state).await,
            QueueConfig {
                auto_promote: false,
                ..QueueConfig::default()
            }
        );
    }

    #[tokio::test]
    async fn a_queue_section_survives_when_another_section_cannot_be_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(
            paths.config_file(),
            "[worktree]\nplacement = \"a-variant-this-build-lacks\"\n\n[queue]\nparallel_task_limit = 2\nauto_promote = false\n",
        )
        .unwrap();

        let state = launch(&paths).await;

        let config = running(&state).await;
        assert!(!config.auto_promote, "paused promotion must not be forgotten");
        assert_eq!(config.parallel_task_limit, 2);
    }

    #[tokio::test]
    async fn the_saved_file_names_the_settings_under_a_queue_section() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let state = launch(&paths).await;
        apply_queue_config_patch(&state.queue, &state.storage, every_field_changed())
            .await
            .unwrap();

        let text = std::fs::read_to_string(paths.config_file()).unwrap();
        let table: toml::Value = toml::from_str(&text).unwrap();
        let queue = table.get("queue").expect("a [queue] section");
        assert_eq!(queue.get("parallel_task_limit").and_then(|v| v.as_integer()), Some(7));
        assert_eq!(queue.get("auto_promote").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(queue.get("fifo_ordering").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(queue.get("use_coderabbit").and_then(|v| v.as_bool()), Some(false));
    }

    #[tokio::test]
    async fn a_hand_edited_limit_the_queue_cannot_run_under_is_brought_into_range() {
        for (written, expected) in [(0, 1), (99, 10)] {
            let tmp = tempfile::TempDir::new().unwrap();
            let paths = paths_in(tmp.path());
            std::fs::create_dir_all(paths.config_dir()).unwrap();
            std::fs::write(
                paths.config_file(),
                format!("[queue]\nparallel_task_limit = {written}\nauto_promote = false\n"),
            )
            .unwrap();

            let config = running(&launch(&paths).await).await;

            assert_eq!(config.parallel_task_limit, expected, "written {written}");
            assert!(!config.auto_promote, "the other fields are kept");
        }
    }

    #[tokio::test]
    async fn an_invalid_limit_is_repaired_on_the_recovery_path_without_re_enabling_promotion() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        // Another section is unreadable, so the strict load fails and the
        // recovery path supplies `[queue]`.
        std::fs::write(
            paths.config_file(),
            "[worktree]\nplacement = \"a-variant-this-build-lacks\"\n\n[queue]\nparallel_task_limit = 0\nauto_promote = false\nfifo_ordering = false\n",
        )
        .unwrap();

        let config = running(&launch(&paths).await).await;

        assert_eq!(config.parallel_task_limit, 1);
        assert!(!config.auto_promote, "repairing the limit must not re-enable promotion");
        assert!(!config.fifo_ordering);
    }
}
