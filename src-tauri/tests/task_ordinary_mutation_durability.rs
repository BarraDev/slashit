//! Ordinary (non-terminal) task mutations must stage, persist, then publish.
//!
//! Terminal transitions already went through `crate::lifecycle::terminalize`,
//! which never shows the board a state the disk does not have. The ordinary
//! mutation commands -- `update_task_status`, `reorder_task`,
//! `set_task_dependencies`, and their IPC/CLI equivalents -- used to mutate
//! the shared in-memory map directly and log-and-swallow a `save` failure, so
//! a full disk or a permissions problem left memory holding a change the file
//! never got, the caller was told it succeeded, and a restart silently
//! reverted it. These tests force a real durable-write failure -- an
//! unwritable directory, not a production-only test hook -- through one
//! desktop command and one IPC handler, and prove the fix: `Err`, unchanged
//! memory, unchanged disk, and a freshly restarted `AppState` that agrees.
//!
//! Unix-only: the durable-write failure is forced by chmod-locking a
//! directory, which has no portable equivalent.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use slashit_ipc::{Endpoint, IpcRequest};
use slashit_ui_lib::config::paths::AppPaths;
use slashit_ui_lib::domain::{TaskPhase, TaskStatus};
use slashit_ui_lib::test_helpers::{create_test_task_full, ipc_test_context};
use slashit_ui_lib::AppState;
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
async fn desktop_mutation_refuses_to_publish_when_the_durable_write_fails() {
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

    let result = slashit_ui_lib::commands::task::update_task_status(
        app.state(),
        task_id.to_string(),
        TaskStatus::InProgress,
    )
    .await;

    assert!(
        result.is_err(),
        "a durable write failure must surface as Err, not a silently stale success: {result:?}"
    );

    let live_state: &AppState = app.state::<AppState>().inner();
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
        TaskStatus::Queue,
        "disk must keep the previous value too, not a torn or partial write"
    );

    drop(guard);

    // Restart/readback: a fresh `AppState` built from the same paths -- the
    // next real launch -- must agree with what memory already reported.
    let (restarted, _report) = slashit_ui_lib::app_core::build_state_with_paths(paths)
        .await
        .expect("state should build again once the directory is writable");
    let restarted_task = restarted
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .expect("the task must still be on the board after a restart");
    assert_eq!(
        restarted_task.status,
        TaskStatus::Queue,
        "a restart must read exactly the value the failed write left on disk"
    );
}

#[tokio::test]
async fn ipc_mutation_refuses_to_publish_when_the_durable_write_fails() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let paths = make_paths(&tmp);
    let ctx = ipc_test_context(paths.clone());

    let project_id = Uuid::new_v4();
    let mut task = create_test_task_full("subject", project_id, TaskStatus::Queue, 0);
    task.phase = TaskPhase::Idle;
    let task_id = task.id;
    ctx.tasks.write().await.insert(task_id, task.clone());
    ctx.storage
        .save_project_tasks(project_id, &[task])
        .expect("seed the board with the previous, good value");

    let guard = Unwritable::on(&legacy_tasks_dir(&paths));

    let peer = slashit_ui_lib::ipc::server::PeerContext {
        endpoint: Endpoint::Unix {
            path: std::path::PathBuf::from("/nonexistent-for-tests"),
        },
        os_verified: true,
    };

    let answer = slashit_ui_lib::ipc::handlers::dispatch(
        IpcRequest::MoveTask {
            task_id: task_id.to_string(),
            status: "in_progress".to_string(),
        },
        &ctx,
        &peer,
    )
    .await
    .response;

    assert!(
        !answer.ok,
        "a durable write failure must surface as an IPC error, not success: {answer:?}"
    );

    let in_memory = ctx
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .expect("the record itself must survive a failed persist");
    assert_eq!(
        in_memory.status,
        TaskStatus::Queue,
        "memory must keep the previous value when the disk write failed"
    );

    let on_disk = ctx
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
