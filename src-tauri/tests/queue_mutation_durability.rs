//! Queue mutations must stage, persist, then publish -- the same contract
//! Unit 4A already gave the ordinary task-mutation front doors.
//!
//! `QueueManager::enqueue_task`, `requeue_task` and `promote_next_task` used
//! to mutate the shared in-memory map directly and never call `Storage` at
//! all, so a status a card's front door reported as changed was never even
//! attempted on disk: a restart silently reverted it. These tests force a
//! real durable-write failure -- an unwritable directory, not a
//! production-only test hook -- through each of the three desktop commands
//! that reach those paths, and prove the fix: `Err`, unchanged memory,
//! unchanged disk.
//!
//! Unix-only: the durable-write failure is forced by chmod-locking a
//! directory, which has no portable equivalent.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use slashit_ui_lib::config::paths::AppPaths;
use slashit_ui_lib::domain::{TaskPhase, TaskStatus};
use slashit_ui_lib::test_helpers::create_test_task_full;
use tauri::Manager;
use uuid::Uuid;

/// Removes write permission on `dir` for the lifetime of the guard, and
/// restores it on drop -- including on an unwinding panic -- so a failed
/// assertion never leaves a directory `tempfile::TempDir` cannot clean up.
struct Unwritable(std::path::PathBuf);

impl Unwritable {
    fn on(dir: &std::path::Path) -> Self {
        std::fs::create_dir_all(dir).expect("create the directory to lock down");
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500))
            .expect("remove write permission");
        Self(dir.to_path_buf())
    }
}

impl Drop for Unwritable {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

fn make_paths(tmp: &tempfile::TempDir) -> Arc<AppPaths> {
    Arc::new(AppPaths::with_roots(
        tmp.path().join("config"),
        tmp.path().join("data"),
        tmp.path().join("cache"),
        tmp.path().join("runtime"),
    ))
}

/// A task with no repository and no config-registered project routes to the
/// legacy `<config_dir>/tasks/<uuid>.toml` location -- see
/// `Storage::tasks_path` -- which is what `Unwritable` locks down here.
fn legacy_tasks_dir(paths: &AppPaths) -> std::path::PathBuf {
    paths.config_dir().join("tasks")
}

#[tokio::test]
async fn desktop_enqueue_refuses_to_publish_when_the_durable_write_fails() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let paths = make_paths(&tmp);
    let (state, _report) = slashit_ui_lib::app_core::build_state_with_paths(paths.clone())
        .await
        .expect("state should build under a fresh tempdir");

    let project_id = Uuid::new_v4();
    let task = create_test_task_full("subject", project_id, TaskStatus::Backlog, 0);
    let task_id = task.id;
    state.task.tasks.write().await.insert(task_id, task.clone());
    state
        .storage
        .save_project_tasks(project_id, &[task])
        .expect("seed the board with the previous, good value");

    let guard = Unwritable::on(&legacy_tasks_dir(&paths));

    let app = tauri::test::mock_app();
    app.manage(state);

    let result =
        slashit_ui_lib::commands::queue::add_to_queue(app.state(), task_id.to_string()).await;

    assert!(
        result.is_err(),
        "a durable write failure must surface as Err, not a silently stale success: {result:?}"
    );

    let live_state: &slashit_ui_lib::AppState = app.state::<slashit_ui_lib::AppState>().inner();
    let in_memory = live_state
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .expect("the record itself must survive a failed persist");
    assert_eq!(
        in_memory.status,
        TaskStatus::Backlog,
        "memory must keep the previous value when the disk write failed"
    );

    let on_disk = live_state
        .storage
        .load_project_tasks(project_id)
        .expect("the previous file must still be readable");
    let disk_task = on_disk
        .iter()
        .find(|t| t.id == task_id)
        .expect("the previous record must still be there");
    assert_eq!(
        disk_task.status,
        TaskStatus::Backlog,
        "disk must keep the previous value too, not a torn or partial write"
    );

    drop(guard);
}

#[tokio::test]
async fn desktop_requeue_refuses_to_publish_when_the_durable_write_fails() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let paths = make_paths(&tmp);
    let (state, _report) = slashit_ui_lib::app_core::build_state_with_paths(paths.clone())
        .await
        .expect("state should build under a fresh tempdir");

    let project_id = Uuid::new_v4();
    let mut task = create_test_task_full("subject", project_id, TaskStatus::Error, 0);
    task.error_message = Some("boom".to_string());
    let task_id = task.id;
    state.task.tasks.write().await.insert(task_id, task.clone());
    state
        .storage
        .save_project_tasks(project_id, &[task])
        .expect("seed the board with the previous, good value");

    let guard = Unwritable::on(&legacy_tasks_dir(&paths));

    let app = tauri::test::mock_app();
    app.manage(state);

    let result =
        slashit_ui_lib::commands::queue::requeue_task(app.state(), task_id.to_string()).await;

    assert!(
        result.is_err(),
        "a durable write failure must surface as Err, not a silently stale success: {result:?}"
    );

    let live_state: &slashit_ui_lib::AppState = app.state::<slashit_ui_lib::AppState>().inner();
    let in_memory = live_state
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .expect("the record itself must survive a failed persist");
    assert_eq!(
        in_memory.status,
        TaskStatus::Error,
        "memory must keep the previous value when the disk write failed"
    );

    let on_disk = live_state
        .storage
        .load_project_tasks(project_id)
        .expect("the previous file must still be readable");
    let disk_task = on_disk
        .iter()
        .find(|t| t.id == task_id)
        .expect("the previous record must still be there");
    assert_eq!(
        disk_task.status,
        TaskStatus::Error,
        "disk must keep the previous value too, not a torn or partial write"
    );

    drop(guard);
}

#[tokio::test]
async fn desktop_promote_refuses_to_publish_when_the_durable_write_fails() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let paths = make_paths(&tmp);
    let (state, _report) = slashit_ui_lib::app_core::build_state_with_paths(paths.clone())
        .await
        .expect("state should build under a fresh tempdir");

    let project_id = Uuid::new_v4();
    let mut task = create_test_task_full("subject", project_id, TaskStatus::Queue, 0);
    task.phase = TaskPhase::Idle;
    let task_id = task.id;
    state.task.tasks.write().await.insert(task_id, task.clone());
    state
        .storage
        .save_project_tasks(project_id, &[task])
        .expect("seed the board with the previous, good value");

    let guard = Unwritable::on(&legacy_tasks_dir(&paths));

    let app = tauri::test::mock_app();
    app.manage(state);

    let result = slashit_ui_lib::commands::queue::promote_next_task(app.state()).await;

    assert!(
        result.is_err(),
        "a durable write failure must surface as Err, not a silently stale success: {result:?}"
    );

    let live_state: &slashit_ui_lib::AppState = app.state::<slashit_ui_lib::AppState>().inner();
    let in_memory = live_state
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .expect("the record itself must survive a failed persist");
    assert_eq!(
        in_memory.status,
        TaskStatus::Queue,
        "memory must keep the previous value when the disk write failed, \
         not report a promotion that never reached disk"
    );

    let on_disk = live_state
        .storage
        .load_project_tasks(project_id)
        .expect("the previous file must still be readable");
    let disk_task = on_disk
        .iter()
        .find(|t| t.id == task_id)
        .expect("the previous record must still be there");
    assert_eq!(
        disk_task.status,
        TaskStatus::Queue,
        "disk must keep the previous value too, not a torn or partial write"
    );

    drop(guard);
}
