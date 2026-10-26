// SPDX-License-Identifier: MPL-2.0
use crate::commands::gui_admission::GuiMutation;
use crate::config::QueueConfig;
use crate::domain::{Task, TaskStatus};
use crate::queue::{PromotionChoice, PromotionOutcome, QueueManager};
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
    _gui: GuiMutation,
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
    let written = storage.update_config(move |config| config.queue = persisted);
    // The file now holds the new settings whenever it was replaced, durable or
    // not, so the running manager follows it either way.
    let published = written.as_ref().err().is_none_or(|e| e.is_published());
    if published {
        manager.set_config(next.clone()).await;
    }
    written.map_err(|e| {
        if e.is_published() {
            e.describe("save the queue settings")
        } else {
            format!("Queue settings were not saved: {e}")
        }
    })?;
    Ok(next)
}

/// Queue a Task through the lifecycle's own entry point, the one the CLI
/// reaches as well. See `lifecycle::enqueue_current`.
///
/// `add_to_queue`/`bulk_add_to_queue`/`requeue_task` all reach this, and none
/// of them are restricted to a task with no active owner: a task selected for
/// a bulk "add to queue" or an explicit requeue can be `InProgress` or
/// `AiReview` right now. The lifecycle ends such an owner before its write,
/// the same contract `update_task_status` has. A refusal that is already known
/// (a Task whose Worker handoff is still `Started`, for example) is returned
/// without ending that owner, so a request that cannot be written never stops
/// a running Worker.
async fn enqueue_durably(
    state: &tauri::State<'_, crate::AppState>,
    task_id: Uuid,
) -> Result<(), String> {
    let running = state
        .executor
        .get()
        .map(|e| e.as_ref() as &dyn crate::lifecycle::ExecutionOwnership);
    let verdict = crate::lifecycle::enqueue_current(
        &state.task.tasks,
        &state.storage,
        &state.task_lifecycle_locks,
        running,
        task_id,
    )
    .await?;
    match verdict.refusal(task_id) {
        Some(refusal) => Err(refusal),
        None => Ok(()),
    }
}

#[tauri::command]
pub async fn add_to_queue(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<(), String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    enqueue_durably(&state, task_id).await
}

#[tauri::command]
pub async fn bulk_add_to_queue(
    _gui: GuiMutation,
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
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
) -> Result<Option<String>, String> {
    // Promotion asks the executor to start the task, so it waits while new
    // work is paused for disk space; the task stays queued.
    state.start_guard.check().await.map_err(|block| block.to_string())?;
    let executor = state.executor.get().ok_or_else(|| NO_EXECUTOR_PROMOTION.to_string())?;
    match executor.promote_queued(PromotionChoice::Next, |_, _| {}).await? {
        PromotionOutcome::Promoted(task) => Ok(Some(task.id.to_string())),
        PromotionOutcome::NothingEligible => Ok(None),
        refused @ PromotionOutcome::NoCapacity => {
            Err(executor.queued_promotion_refusal(None, &refused).await)
        }
    }
}

/// The answer when no executor is running. Every promotion needs an admission
/// permit from the executor, so without one nothing is promoted.
pub(crate) const NO_EXECUTOR_PROMOTION: &str =
    "The task executor is not running, so nothing was promoted.";

#[tauri::command]
pub async fn get_queue_capacity(
    state: tauri::State<'_, crate::AppState>,
) -> Result<usize, String> {
    let manager = state.queue.manager.read().await;
    Ok(manager.get_capacity_available().await)
}

/// Run one exact queued task now, the "Run now" answer on a queued Task card,
/// in its drawer and in its context menu.
///
/// The task named by `task_id` is promoted Queue → InProgress through the same
/// durable select-under-the-write-lock, amend and commit the scheduler's
/// automatic promotion and "promote next task" use
/// ([`QueueManager::apply_promotion`] and [`crate::lifecycle::commit_selected`]),
/// so the run that starts afterwards is the executor's own start path, and a
/// promotion is durable exactly as their promotions are. Auto-promote is not
/// consulted and not changed: the automatic poll still decides its own
/// promotions, and turning it off stays off.
///
/// `expected_status` is the column the caller saw the task in. When given and
/// the task is no longer in Queue, nothing is changed and the answer is the
/// task as it now is, so a caller whose view is behind -- a previous click
/// already promoted the task, or it was moved -- shows the task where it
/// really stands instead of promoting a second time. The check runs under the
/// task's lifecycle lease, so a promotion in flight is finished before this
/// reads the answer. Repeated clicks are also guarded by the task's own
/// status: only a task still in Queue can be promoted, so a second request
/// writes nothing.
#[tauri::command]
pub async fn run_queued_task(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    expected_status: Option<TaskStatus>,
) -> Result<Option<Task>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    // A run asks for a new execution; while new work is paused for disk
    // space it is refused before anything is read or written, and the block
    // is what the user is shown.
    state.start_guard.check().await.map_err(|block| block.to_string())?;
    let executor = state.executor.get().ok_or_else(|| NO_EXECUTOR_PROMOTION.to_string())?;

    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    // Read under the lease: whatever the caller saw may no longer be true.
    let Some(task) = state.task.tasks.read().await.get(&task_id).cloned() else {
        return Ok(None);
    };
    // A stale view: nothing was changed, and the task as it now stands is the
    // answer. This is also the repeated click after a successful one: the
    // task left Queue, so there is no second promotion to make.
    if expected_status
        .as_ref()
        .is_some_and(|expected| *expected != task.status)
        || task.status != TaskStatus::Queue
    {
        return Ok(state.task.tasks.read().await.get(&task_id).cloned());
    }

    match executor.promote_queued(PromotionChoice::Exact(task_id), |_, _| {}).await? {
        PromotionOutcome::Promoted(task) => Ok(Some(*task)),
        // Nothing was written. The task may have been deleted or moved in the
        // race before the write, so answer with where it stands; a task still
        // queued is refused with the reason its promotion was refused.
        refused => {
            let current = state.task.tasks.read().await.get(&task_id).cloned();
            match current {
                None => Ok(None),
                Some(task) if task.status != TaskStatus::Queue => Ok(Some(task)),
                Some(_) => Err(executor.queued_promotion_refusal(Some(task_id), &refused).await),
            }
        }
    }
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
    _gui: GuiMutation,
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
    use crate::domain::TaskStatus;
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

        let result = add_to_queue(GuiMutation::untracked(), app.state(), task_id.to_string()).await;

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

        let result = add_to_queue(GuiMutation::untracked(), app.state(), task_id.to_string()).await;

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
    use crate::domain::TaskStatus;
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

        crate::commands::task::reorder_task(GuiMutation::untracked(), 
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
            None,
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
    async fn settings_the_file_holds_but_could_not_confirm_are_applied_and_reported() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let queue = QueueState::default();
        let storage = Storage::with_paths((*paths).clone());
        storage.save_config(&Default::default()).unwrap();

        let _fault = slashit_durable_fs::fault::fail_at_file(
            slashit_durable_fs::Stage::SyncDirectory,
            std::io::ErrorKind::Other,
            "config.toml",
        );
        let error = apply_queue_config_patch(&queue, &storage, every_field_changed())
            .await
            .expect_err("the durability is unconfirmed");

        assert!(error.contains("could not confirm"), "{error}");
        let expected = queue.manager.read().await.config().clone();
        assert_ne!(expected, QueueConfig::default(), "memory follows the file the change is in");
        // A restart reads the same settings the running queue uses.
        assert_eq!(Storage::with_paths((*paths).clone()).load_config().unwrap().queue, expected);
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

/// The queue's manual "Run now": the promotion is durable, exact-task and
/// refused with a reason, through the same lifecycle the scheduler's
/// promotion uses.
#[cfg(test)]
mod run_queued_task {
    use super::*;
    use crate::domain::{TaskPhase, TaskStatus};
    use crate::test_helpers::create_test_task_full;
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

    async fn seed(state: &crate::AppState, project_id: Uuid, status: TaskStatus) -> Task {
        let task = create_test_task_full("queued work", project_id, status, 0);
        state.task.tasks.write().await.insert(task.id, task.clone());
        state
            .storage
            .save_project_tasks(project_id, std::slice::from_ref(&task))
            .expect("seed the board");
        task
    }

    fn app_with(state: crate::AppState) -> tauri::App<tauri::test::MockRuntime> {
        // A promotion holds an admission permit from the executor, so the
        // commands need one, as they do in the running application.
        crate::test_helpers::attach_test_executor(&state);
        let app = tauri::test::mock_app();
        app.manage(state);
        app
    }

    #[tokio::test(start_paused = true)]
    async fn a_click_promotes_the_selected_queued_task_exactly() {
        let (_tmp, state) = test_state().await;
        let project_id = Uuid::new_v4();
        let target = seed(&state, project_id, TaskStatus::Queue).await;
        let other = seed(&state, project_id, TaskStatus::Queue).await;
        let app = app_with(state);

        let answer = run_queued_task(
            GuiMutation::untracked(),
            app.state(),
            target.id.to_string(),
            Some(TaskStatus::Queue),
        )
        .await
        .expect("promote");

        let promoted = answer.expect("the promoted record");
        assert_eq!(promoted.id, target.id);
        assert_eq!(promoted.status, TaskStatus::InProgress);
        // Only the selected task moved: not a listed field is reset except
        // what the promotion resets, and the queue's next pick is untouched.
        let live: &crate::AppState = app.state::<crate::AppState>().inner();
        let tasks = live.task.tasks.read().await;
        assert_eq!(tasks[&other.id].status, TaskStatus::Queue);
        assert_eq!(promoted.phase, TaskPhase::Idle);
        assert_eq!(promoted.phase_progress, 0);
        assert_eq!(promoted.overall_progress, 0);
        assert_eq!(promoted.error_message, None);
        // The promotion was durably written, not only published to memory.
        let board: Vec<Task> = live
            .storage
            .load_project_tasks(project_id)
            .expect("the board file");
        let on_disk = board.iter().find(|t| t.id == target.id).expect("the task");
        assert_eq!(on_disk.status, TaskStatus::InProgress);
    }

    #[tokio::test(start_paused = true)]
    async fn a_repeated_click_after_a_successful_one_promotes_nothing_again() {
        let (_tmp, state) = test_state().await;
        let project_id = Uuid::new_v4();
        let target = seed(&state, project_id, TaskStatus::Queue).await;
        let app = app_with(state);

        run_queued_task(
            GuiMutation::untracked(),
            app.state(),
            target.id.to_string(),
            Some(TaskStatus::Queue),
        )
        .await
        .expect("the first promotion");

        // The second click arrives with the stale (still queued) view the
        // first click was made from.
        let second = run_queued_task(
            GuiMutation::untracked(),
            app.state(),
            target.id.to_string(),
            Some(TaskStatus::Queue),
        )
        .await
        .expect("the repeated click is answered, not failed");

        assert_eq!(second.expect("the task as it now stands").status, TaskStatus::InProgress);
        let tasks = app.state::<crate::AppState>().inner().task.tasks.read().await;
        assert_eq!(tasks[&target.id].status, TaskStatus::InProgress);
    }

    #[tokio::test(start_paused = true)]
    async fn a_click_on_a_task_that_has_moved_queues_nothing_and_shows_where_it_is() {
        let (_tmp, state) = test_state().await;
        let project_id = Uuid::new_v4();
        let target = seed(&state, project_id, TaskStatus::Backlog).await;
        let app = app_with(state);

        let answer = run_queued_task(
            GuiMutation::untracked(),
            app.state(),
            target.id.to_string(),
            Some(TaskStatus::Queue),
        )
        .await
        .expect("answered with the task as it is");

        assert_eq!(answer.expect("task").status, TaskStatus::Backlog);
        let tasks = app.state::<crate::AppState>().inner().task.tasks.read().await;
        assert_eq!(tasks[&target.id].status, TaskStatus::Backlog);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_queue_refuses_the_click_and_names_the_parallel_limit() {
        let (_tmp, state) = test_state().await;
        let project_id = Uuid::new_v4();
        let target = seed(&state, project_id, TaskStatus::Queue).await;
        for _ in 0..3 {
            seed(&state, project_id, TaskStatus::InProgress).await;
        }
        let app = app_with(state);
        let live: &crate::AppState = app.state::<crate::AppState>().inner();
        assert_eq!(live.queue.manager.read().await.get_in_progress_count().await, 3);

        let error = run_queued_task(
            GuiMutation::untracked(),
            app.state(),
            target.id.to_string(),
            Some(TaskStatus::Queue),
        )
        .await
        .expect_err("the queue is full");

        assert!(error.contains("parallel limit of 3"), "{error}");
        assert_eq!(live.task.tasks.read().await[&target.id].status, TaskStatus::Queue);
    }

    #[tokio::test(start_paused = true)]
    async fn a_click_while_new_work_is_paused_for_disk_space_names_the_block() {
        // Debug builds only; the desktop acceptance journeys use the same
        // override. Test builds replace the application's guard with
        // `plenty_of_disk()` in `build_state_with_paths`, so this test installs
        // the debug-override guard itself, after the variable is set.
        let space = tempfile::TempDir::new().expect("tempdir");
        let space_file = space.path().join("space");
        std::fs::write(&space_file, "1000000000 1\n").unwrap();
        let (_tmp, mut state) = test_state().await;
        // The override is process-wide, so hold the shared environment lock
        // from the set until the variable is removed again. It is taken after
        // the state is built, so the lock is not held across that await.
        let env_guard = crate::config::paths::ENV_LOCK.lock().await;
        std::env::set_var(crate::queue::start_guard::DEBUG_DISK_SPACE_FILE, &space_file);
        state.start_guard = Arc::new(crate::queue::start_guard::StartGuard::for_data_dir(
            _tmp.path().join("data"),
        ));
        std::env::remove_var(crate::queue::start_guard::DEBUG_DISK_SPACE_FILE);
        drop(env_guard);
        let project_id = Uuid::new_v4();
        let target = seed(&state, project_id, TaskStatus::Queue).await;

        let app = app_with(state);
        let live: &crate::AppState = app.state::<crate::AppState>().inner();
        let error = run_queued_task(
            GuiMutation::untracked(),
            app.state(),
            target.id.to_string(),
            Some(TaskStatus::Queue),
        )
        .await
        .expect_err("the paused board refuses the click");

        assert!(error.contains("New work paused") || error.to_lowercase().contains("disk"), "{error}");
        // Nothing was promoted: the guard runs before anything is read.
        assert_eq!(live.task.tasks.read().await[&target.id].status, TaskStatus::Queue);
    }

    #[tokio::test]
    async fn an_interrupted_cleanup_is_named_as_the_refusal() {
        let tasks: Tasks = Arc::new(RwLock::new(HashMap::new()));
        let manager = QueueManager::with_config(tasks);
        let mut queued = create_test_task_full("queued work", Uuid::new_v4(), TaskStatus::Queue, 0);
        queued.cleanup_in_flight = true;

        let reason = crate::queue::queued_refusal_reason(
            &manager,
                &crate::config::Storage::with_paths(crate::config::paths::AppPaths::with_roots(
                    std::path::PathBuf::from("unused-config-dir"),
                    std::path::PathBuf::from("unused-data-dir"),
                    std::path::PathBuf::from("unused-cache-dir"),
                    std::path::PathBuf::from("unused-runtime-dir"),
                )),
                &queued,
            )
            .await;

        assert!(reason.contains("the cleanup of its Task Checkout was interrupted"), "{reason}");
    }

    #[tokio::test]
    async fn an_unfinished_restack_is_named_as_the_refusal() {
        let tasks: Tasks = Arc::new(RwLock::new(HashMap::new()));
        let manager = QueueManager::with_config(tasks);
        let mut queued = create_test_task_full("queued work", Uuid::new_v4(), TaskStatus::Queue, 0);
        queued.pending_republish = Some(crate::domain::PendingRepublish {
            parent_branch: "p".to_string(),
            parent_pr: 1,
            pr_number: Some(2),
            default_branch: "main".to_string(),
            fork_point: "a".repeat(40),
            previous_tip: "b".repeat(40),
            onto: "c".repeat(40),
            rewritten_tip: None,
        });

        let reason = crate::queue::queued_refusal_reason(
            &manager,
                &crate::config::Storage::with_paths(crate::config::paths::AppPaths::with_roots(
                    std::path::PathBuf::from("unused-config-dir"),
                    std::path::PathBuf::from("unused-data-dir"),
                    std::path::PathBuf::from("unused-cache-dir"),
                    std::path::PathBuf::from("unused-runtime-dir"),
                )),
                &queued,
            )
            .await;

        assert!(reason.contains("restack"), "{reason}");
    }
}

/// The desktop's Add to Queue and the lifecycle's own entry point, which the
/// CLI shares, leave a stale Task in the same durable state.
#[cfg(test)]
mod enqueue_front_doors {
    use super::*;
    use crate::domain::{TaskPhase, TaskStatus};
    use crate::test_helpers::create_test_task_full;
    use tauri::Manager;

    /// The fields an enqueue is meant to settle, for the one Task it queued.
    fn queued_record(task: &Task) -> (TaskStatus, TaskPhase, u8, u8, Option<String>, bool, i32) {
        (
            task.status.clone(),
            task.phase.clone(),
            task.phase_progress,
            task.overall_progress,
            task.error_message.clone(),
            task.human_review.is_approved(),
            task.position,
        )
    }

    #[tokio::test]
    async fn add_to_queue_and_the_lifecycle_entry_point_leave_the_same_record() {
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
        let stale = |title: &str| {
            let mut task = create_test_task_full(title, project_id, TaskStatus::Error, 0);
            task.phase = TaskPhase::Coding;
            task.phase_progress = 40;
            task.overall_progress = 40;
            task.error_message = Some("old failure".into());
            task
        };
        let by_desktop = stale("by desktop");
        let by_lifecycle = stale("by lifecycle");
        let (desktop_id, lifecycle_id) = (by_desktop.id, by_lifecycle.id);
        {
            let mut tasks = state.task.tasks.write().await;
            tasks.insert(desktop_id, by_desktop.clone());
            tasks.insert(lifecycle_id, by_lifecycle.clone());
        }
        state
            .storage
            .save_project_tasks(project_id, &[by_desktop, by_lifecycle])
            .expect("seed the board");

        let app = tauri::test::mock_app();
        app.manage(state);
        let live: &crate::AppState = app.state::<crate::AppState>().inner();

        add_to_queue(GuiMutation::untracked(), app.state(), desktop_id.to_string())
            .await
            .expect("the desktop enqueue");
        let verdict = crate::lifecycle::enqueue_current(
            &live.task.tasks,
            &live.storage,
            &live.task_lifecycle_locks,
            None,
            lifecycle_id,
        )
        .await
        .expect("the lifecycle enqueue");
        assert!(matches!(verdict, crate::lifecycle::EnqueueVerdict::Queued(_)), "{verdict:?}");

        let tasks = live.task.tasks.read().await;
        let desktop = queued_record(&tasks[&desktop_id]);
        let lifecycle = queued_record(&tasks[&lifecycle_id]);
        assert_eq!(desktop.0, TaskStatus::Queue);
        assert_eq!(desktop.1, TaskPhase::Idle);
        assert_eq!(desktop.4, None);
        assert_eq!(desktop.2, 0);
        assert_eq!(desktop.3, 0);
        assert_eq!(lifecycle, (desktop.0.clone(), desktop.1.clone(), 0, 0, None, false, 0), "the record of the second entry");
        assert_eq!(desktop.6, 1, "the first Task is pushed down by the second entry, not left colliding");
    }
}
