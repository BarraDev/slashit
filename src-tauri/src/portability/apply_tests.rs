// SPDX-License-Identifier: MPL-2.0
//! Import apply: the round trip, inertness, refusals that write nothing, the
//! failure points of the commit order, and a concurrent double apply. Driven
//! through the IPC dispatcher, so the production handlers are what is tested.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use futures::FutureExt;
use slashit_durable_fs::{fault, Stage};
use slashit_ipc::{IpcRequest, IpcResponse};
use tempfile::TempDir;
use uuid::Uuid;

use crate::config::paths::StateLocation;
use crate::domain::TaskStatus;
use crate::ipc::handlers::dispatch;

use super::apply::{self, Live};
use super::bundle;
use super::tests::{export_to, fixture_tasks, peer, project_id, seed, test_ctx};
use crate::ipc::IpcContext;

fn alpha_id() -> Uuid {
    Uuid::from_u128(10)
}

fn beta_id() -> Uuid {
    Uuid::from_u128(11)
}

fn live(ctx: &IpcContext) -> Live<'_> {
    Live {
        gate: &ctx.project_gate,
        tasks: &*ctx.tasks,
        projects: &*ctx.projects,
        repositories: &*ctx.repositories,
        storage: &ctx.storage,
    }
}

/// Export the fixture Project of `source` and return the file's bytes.
async fn exported_bytes(source: &IpcContext, dir: &TempDir) -> Vec<u8> {
    let path = dir.path().join("export.json");
    let response = export_to(source, &path).await;
    assert!(response.ok, "export refused: {:?}", response.error);
    fs::read(&path).unwrap()
}

fn write_bundle(dir: &TempDir, bytes: &[u8]) -> PathBuf {
    let path = dir.path().join("import.json");
    fs::write(&path, bytes).unwrap();
    path
}

fn checksum_of(bytes: &[u8]) -> String {
    bundle::decode(bytes).unwrap().checksum
}

async fn apply_with(ctx: &IpcContext, path: &Path, checksum: &str) -> IpcResponse {
    dispatch(
        IpcRequest::ApplyImport {
            path: path.to_string_lossy().into_owned(),
            checksum: checksum.to_string(),
        },
        ctx,
        &peer(),
    )
    .await
    .response
}

fn board_name(ctx: &IpcContext) -> String {
    let board = ctx
        .storage
        .tasks_board_files(project_id())
        .last()
        .cloned()
        .unwrap();
    board.file_name().unwrap().to_string_lossy().into_owned()
}

/// Every file whose name names the imported Project, anywhere in the state tree.
fn leftovers(ctx: &IpcContext) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![
        ctx.storage.paths().data_dir().to_path_buf(),
        ctx.storage.paths().config_dir().to_path_buf(),
    ];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().contains(&project_id().to_string()))
            {
                found.push(path);
            }
        }
    }
    found
}

async fn assert_target_empty(ctx: &IpcContext) {
    assert!(
        ctx.projects.read().await.is_empty(),
        "no Project may be visible"
    );
    assert!(ctx.tasks.read().await.is_empty(), "no Task may be visible");
    let config = ctx.storage.load_config().unwrap();
    assert!(!config.projects.contains_key(&project_id().to_string()));
    assert!(
        leftovers(ctx).is_empty(),
        "no board or temporary file may remain: {:?}",
        leftovers(ctx)
    );
}

#[tokio::test]
async fn a_bundle_applies_as_an_inert_project_with_its_tasks() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;

    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);
    let response = apply_with(&target, &path, &checksum_of(&bytes)).await;
    assert!(response.ok, "apply failed: {:?}", response.error);

    let projects = target.projects.read().await;
    let imported = projects
        .get(&project_id())
        .expect("the Project keeps its id");
    assert_eq!(imported.repository_id, None, "no Repository is recreated");
    assert_eq!(imported.state_location, StateLocation::External);
    drop(projects);

    let tasks = target.tasks.read().await;
    let alpha = tasks.get(&alpha_id()).expect("Alpha imported");
    let beta = tasks.get(&beta_id()).expect("Beta imported");
    assert_eq!(
        beta.dependencies,
        vec![alpha_id()],
        "dependencies are preserved"
    );
    assert_eq!(
        beta.status,
        TaskStatus::Backlog,
        "a review wait without a checkout returns to the backlog"
    );
    assert!(
        !beta.human_review.is_approved(),
        "no approval is current after import"
    );
    assert!(
        !beta.human_review.entries.is_empty(),
        "the review history is kept"
    );
    for task in [alpha, beta] {
        assert!(task.worktree_path.is_none(), "no checkout path");
        assert!(task.branch_name.is_none(), "no branch name");
        assert!(task.checkout_admission.is_none(), "no admission");
        assert!(task.approved_execution.is_none(), "no approval to execute");
    }
    drop(tasks);
    assert!(target.repositories.read().await.is_empty());

    assert_eq!(
        target
            .storage
            .load_project_tasks(project_id())
            .unwrap()
            .len(),
        2
    );
    assert!(target
        .storage
        .load_config()
        .unwrap()
        .projects
        .contains_key(&project_id().to_string()));

    let report = response.data;
    assert_eq!(report["tasks"], 2);
    assert_eq!(report["durable"], true);
    let normalization = report["normalization"].as_array().unwrap();
    assert!(normalization
        .iter()
        .any(|n| n["reason"] == "human_review_without_checkout"));
}

#[tokio::test]
async fn a_repeated_apply_is_refused_and_changes_nothing() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);
    let sum = checksum_of(&bytes);
    assert!(apply_with(&target, &path, &sum).await.ok);
    let board = target
        .storage
        .tasks_board_files(project_id())
        .pop()
        .unwrap();
    let before = fs::read(&board).unwrap();

    let again = apply_with(&target, &path, &sum).await;
    assert!(!again.ok);
    assert!(again.error.unwrap().contains("refused"));
    assert_eq!(
        fs::read(&board).unwrap(),
        before,
        "the committed board is untouched"
    );
    assert_eq!(target.tasks.read().await.len(), 2);
}

#[tokio::test]
async fn a_checksum_other_than_the_confirmed_one_writes_nothing() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);

    let response = apply_with(&target, &path, &"0".repeat(64)).await;
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("checksum"));
    assert_target_empty(&target).await;
}

#[tokio::test]
async fn an_existing_project_with_the_same_id_refuses_without_writing() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    seed(&target, None, &[]).await;
    let path = write_bundle(&dst, &bytes);

    let response = apply_with(&target, &path, &checksum_of(&bytes)).await;
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("project_id"));
    assert!(target.tasks.read().await.is_empty(), "no Task was added");
    assert!(target
        .storage
        .tasks_board_files(project_id())
        .iter()
        .all(|p| !p.exists()));
}

#[tokio::test]
async fn a_failed_board_publication_reverts_the_project() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);
    let name = board_name(&target);

    let _fault = fault::fail_at_file(Stage::Replace, ErrorKind::Other, &name);
    let response = apply_with(&target, &path, &checksum_of(&bytes)).await;
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("reverted"));
    drop(_fault);
    assert_target_empty(&target).await;
}

#[tokio::test]
async fn a_failed_project_commit_leaves_no_board_behind() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);

    let _fault = fault::fail_at_file(Stage::Replace, ErrorKind::Other, "config.toml");
    let response = apply_with(&target, &path, &checksum_of(&bytes)).await;
    assert!(!response.ok);
    drop(_fault);
    assert_target_empty(&target).await;
}

#[tokio::test]
async fn a_board_published_but_not_confirmed_is_reported_as_not_durable() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);
    let name = board_name(&target);

    let _fault = fault::fail_at_file(Stage::SyncDirectory, ErrorKind::Other, &name);
    let response = apply_with(&target, &path, &checksum_of(&bytes)).await;
    drop(_fault);
    assert!(
        response.ok,
        "the change is in place, so it is not reported as failed: {:?}",
        response.error
    );
    let report = response.data;
    assert_eq!(report["durable"], false);
    assert_eq!(target.tasks.read().await.len(), 2);
    assert_eq!(
        target
            .storage
            .load_project_tasks(project_id())
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn concurrent_applies_of_one_bundle_commit_exactly_once() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (_dst, target) = test_ctx().await;
    let sum = checksum_of(&bytes);

    let (first, second) = tokio::join!(
        apply::apply(live(&target), &bytes, &sum),
        apply::apply(live(&target), &bytes, &sum),
    );
    assert!(first.is_ok() ^ second.is_ok(), "exactly one apply commits");
    assert_eq!(target.tasks.read().await.len(), 2);
    assert_eq!(
        target
            .storage
            .load_project_tasks(project_id())
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn the_preview_reports_the_checksum_an_apply_needs() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);

    let preview = dispatch(
        IpcRequest::PreviewImport {
            path: path.to_string_lossy().into_owned(),
        },
        &target,
        &peer(),
    )
    .await
    .response;
    assert!(preview.ok);
    let reported = preview.data["checksum"].as_str().unwrap().to_string();
    assert_eq!(reported, checksum_of(&bytes));
}

#[tokio::test]
async fn an_invalid_reviewer_model_refuses_the_bundle_before_any_write() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let mut payload = bundle::decode(&bytes).unwrap().payload;
    payload.tasks[0].reviewer_model = Some("-x".into());
    let tampered = bundle::encode(&payload, chrono::Utc::now(), "test").unwrap();

    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &tampered);
    // Decode refuses the tampered payload before any checksum is compared.
    let response = apply_with(&target, &path, &"0".repeat(64)).await;
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("reviewer model"));
    assert_target_empty(&target).await;
}

#[tokio::test]
async fn apply_waits_for_the_project_lifecycle_gate() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (_dst, target) = test_ctx().await;
    let sum = checksum_of(&bytes);

    let gate = target.project_gate.write(project_id()).await;
    // Polled once, without a sleep: an apply that did not wait for the gate would
    // complete on this first poll.
    let while_held = apply::apply(live(&target), &bytes, &sum).now_or_never();
    assert!(
        while_held.is_none(),
        "apply must wait while the gate is held"
    );
    assert!(target.tasks.read().await.is_empty());
    drop(gate);

    assert!(apply::apply(live(&target), &bytes, &sum).await.is_ok());
    assert_eq!(target.tasks.read().await.len(), 2);
}

#[tokio::test]
async fn a_failed_revert_is_reported_as_an_incomplete_import() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);

    // Replace writes, in order: the import record and the Project commit (let
    // through), the board publication (fails), then the revert of the Project
    // commit (fails).
    let _faults = fault::fail_in_order(&[
        (Stage::Replace, ErrorKind::Other, 2),
        (Stage::Replace, ErrorKind::Other, 0),
    ]);
    let response = apply_with(&target, &path, &checksum_of(&bytes)).await;
    drop(_faults);
    assert!(!response.ok);
    let message = response.error.unwrap();
    assert!(message.contains("without its Tasks"), "{message}");
    assert!(message.contains("Restart SlashIt"), "{message}");
    assert!(
        !target.projects.read().await.contains_key(&project_id()),
        "an unrecovered import is not usable in this process"
    );
    assert!(target.tasks.read().await.is_empty());
    assert!(target
        .storage
        .load_config()
        .unwrap()
        .projects
        .contains_key(&project_id().to_string()));
    assert_eq!(
        target.storage.pending_project_imports().unwrap(),
        vec![project_id()],
        "the record is kept for the next start"
    );
}

/// The import record's file name, for targeting a fault at it.
fn record_name() -> String {
    format!("{}.import-pending", project_id())
}

#[tokio::test]
async fn an_import_record_that_cannot_be_written_refuses_before_any_commit() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);

    let _fault = fault::fail_at_file(Stage::Replace, ErrorKind::Other, &record_name());
    let response = apply_with(&target, &path, &checksum_of(&bytes)).await;
    drop(_fault);
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("nothing was imported"));
    assert_target_empty(&target).await;
}

#[tokio::test]
async fn a_board_not_confirmed_durable_keeps_the_record_for_the_next_start() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);

    // Directory syncs in order: the record, the Project commit, then the board's
    // publication. The third one fails: the board is in place but not confirmed.
    let _fault = fault::fail_after(Stage::SyncDirectory, ErrorKind::Other, 2);
    let response = apply_with(&target, &path, &checksum_of(&bytes)).await;
    drop(_fault);
    assert!(response.ok, "{:?}", response.error);
    assert_eq!(response.data["durable"], false);
    assert_eq!(
        target.storage.pending_project_imports().unwrap(),
        vec![project_id()],
        "the record stays until the board is confirmed"
    );
}

#[tokio::test]
async fn a_record_in_place_but_not_confirmed_lets_the_import_proceed_as_not_durable() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);

    // The first directory sync is the record's; it is published but unconfirmed.
    let _fault = fault::fail_at(Stage::SyncDirectory, ErrorKind::Other);
    let response = apply_with(&target, &path, &checksum_of(&bytes)).await;
    drop(_fault);
    assert!(response.ok, "{:?}", response.error);
    assert_eq!(response.data["durable"], false);
    assert_eq!(target.tasks.read().await.len(), 2);
    assert!(target.storage.pending_project_imports().unwrap().is_empty());
}

/// An import whose process ended after the Project was committed and before its
/// board was published, with the revert never run: the state root, the bundle
/// bytes, the paths, and the bundle file for the retry.
type InterruptedImport = (
    TempDir,
    TempDir,
    Vec<u8>,
    std::sync::Arc<crate::config::paths::AppPaths>,
    PathBuf,
);

async fn interrupted_import() -> InterruptedImport {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);
    let paths = std::sync::Arc::new(target.storage.paths().clone());

    // Replace writes, in order: the import record and the Project commit (let
    // through), the board publication (fails), then the revert (fails).
    let _faults = fault::fail_in_order(&[
        (Stage::Replace, ErrorKind::Other, 2),
        (Stage::Replace, ErrorKind::Other, 0),
    ]);
    let response = apply_with(&target, &path, &checksum_of(&bytes)).await;
    drop(_faults);
    assert!(!response.ok);
    drop(target);
    (src, dst, bytes, paths, path)
}

#[tokio::test]
async fn an_import_interrupted_after_its_commit_is_undone_at_the_next_start() {
    let (_src, _dst, bytes, paths, path) = interrupted_import().await;

    // The next start, through the production startup path, before any Project is used.
    let (state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert!(
        report.project_import_recovery_failures.is_empty(),
        "{:?}",
        report.project_import_recovery_failures
    );
    assert!(!state
        .project
        .projects
        .read()
        .await
        .contains_key(&project_id()));
    let storage = crate::config::Storage::with_paths((*paths).clone());
    let config = storage.load_config().unwrap();
    assert!(!config.projects.contains_key(&project_id().to_string()));
    assert!(storage.pending_project_imports().unwrap().is_empty());
    assert!(storage
        .tasks_board_files(project_id())
        .iter()
        .all(|p| !p.exists()));

    // The import can now be retried, and it creates the Project once.
    let retry = crate::test_helpers::ipc_test_context(paths.clone());
    let response = apply_with(&retry, &path, &checksum_of(&bytes)).await;
    assert!(response.ok, "{:?}", response.error);
    assert_eq!(retry.tasks.read().await.len(), 2);
    assert_eq!(storage.load_project_tasks(project_id()).unwrap().len(), 2);
}

#[tokio::test]
async fn startup_undoes_an_interrupted_import_and_leaves_other_projects_alone() {
    let (_src, _dst, _bytes, paths, _path) = interrupted_import().await;
    let other_id = Uuid::from_u128(77);
    let storage = crate::config::Storage::with_paths((*paths).clone());
    let mut other = super::tests::project(None);
    other.id = other_id;
    other.name = "Unrelated".into();
    let mut projects = HashMap::from([(other_id, other.clone())]);
    projects.insert(project_id(), super::tests::project(None));
    crate::commands::project::try_persist_projects(&storage, &projects).unwrap();

    let (state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert!(report.project_import_recovery_failures.is_empty());
    let loaded = state.project.projects.read().await;
    assert_eq!(
        loaded.get(&other_id).map(|p| p.name.as_str()),
        Some("Unrelated")
    );
    assert!(!loaded.contains_key(&project_id()));
}

#[tokio::test]
async fn a_record_without_a_project_is_cleared_at_the_next_start() {
    let (tmp, ctx) = test_ctx().await;
    // An installed app always has a configuration with a projects section; only
    // such a file makes the absence of a Project authoritative.
    crate::commands::project::try_persist_projects(&ctx.storage, &HashMap::new()).unwrap();
    let paths = std::sync::Arc::new(ctx.storage.paths().clone());
    ctx.storage
        .mark_project_import_pending(project_id())
        .unwrap();
    drop(ctx);

    let (_state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert!(report.project_import_recovery_failures.is_empty());
    let storage = crate::config::Storage::with_paths((*paths).clone());
    assert!(storage.pending_project_imports().unwrap().is_empty());
    drop(tmp);
}

#[tokio::test]
async fn a_record_beside_a_completed_board_keeps_the_project_and_its_tasks() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);
    assert!(apply_with(&target, &path, &checksum_of(&bytes)).await.ok);
    // A removal that never ran would leave exactly this record.
    target
        .storage
        .mark_project_import_pending(project_id())
        .unwrap();
    let paths = std::sync::Arc::new(target.storage.paths().clone());
    drop(target);

    let (state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert!(report.project_import_recovery_failures.is_empty());
    assert!(state.project.projects.read().await.contains_key(&project_id()));
    let storage = crate::config::Storage::with_paths((*paths).clone());
    assert!(storage.pending_project_imports().unwrap().is_empty());
    assert_eq!(storage.load_project_tasks(project_id()).unwrap().len(), 2);
}

#[tokio::test]
async fn a_configuration_the_filesystem_cannot_synchronize_is_not_reported_durable() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);

    let _fault = fault::fail_at_file(Stage::SyncDirectory, ErrorKind::Unsupported, "config.toml");
    let response = apply_with(&target, &path, &checksum_of(&bytes)).await;
    drop(_fault);
    assert!(response.ok, "the change is in place: {:?}", response.error);
    assert_eq!(response.data["durable"], false);
    assert_eq!(target.tasks.read().await.len(), 2);
}

/// Make `config.toml` fail strict parsing, so startup reads it through the
/// salvage path and cannot prove a Project is absent.
fn corrupt_config_keeping_projects(config_file: &Path) {
    let mut value: toml::Value =
        toml::from_str(&fs::read_to_string(config_file).unwrap()).unwrap();
    value["ui_preferences"]["theme"] = toml::Value::Integer(5);
    fs::write(config_file, toml::to_string(&value).unwrap()).unwrap();
}

#[tokio::test]
async fn a_record_that_cannot_be_trusted_holds_its_project_back_and_keeps_everything() {
    // A completed import whose record was then damaged or names another Project.
    let untrusted: [Vec<u8>; 3] = [
        b"not a project id".to_vec(),
        Uuid::from_u128(999).to_string().into_bytes(),
        Vec::new(),
    ];
    for content in untrusted {
        let (src, source) = test_ctx().await;
        seed(&source, None, &fixture_tasks()).await;
        let bytes = exported_bytes(&source, &src).await;
        let (dst, target) = test_ctx().await;
        let path = write_bundle(&dst, &bytes);
        assert!(apply_with(&target, &path, &checksum_of(&bytes)).await.ok);
        let record = target.storage.paths().pending_project_import_file(project_id());
        fs::write(&record, &content).unwrap();
        assert!(target
            .storage
            .load_config()
            .unwrap()
            .projects
            .contains_key(&project_id().to_string()));
        let paths = std::sync::Arc::new(target.storage.paths().clone());
        drop(target);

        let (state, report) = crate::app_core::build_state_with_paths(paths.clone())
            .await
            .unwrap();
        assert!(!state.project.projects.read().await.contains_key(&project_id()));
        assert!(state
            .task
            .tasks
            .read()
            .await
            .values()
            .all(|task| task.project_id != project_id()));
        assert_eq!(
            report.project_import_recovery_failures.len(),
            1,
            "{:?}",
            report.project_import_recovery_failures
        );
        assert_eq!(fs::read(&record).unwrap(), content, "the record is kept as evidence");
        let storage = crate::config::Storage::with_paths((*paths).clone());
        assert!(storage
            .load_config()
            .unwrap()
            .projects
            .contains_key(&project_id().to_string()));
        assert!(storage
            .tasks_board_files(project_id())
            .iter()
            .any(|file| file.exists()));
    }
}

#[tokio::test]
async fn a_completed_import_under_a_salvaged_configuration_loads_none_of_its_tasks() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);
    assert!(apply_with(&target, &path, &checksum_of(&bytes)).await.ok);
    // A record left beside a completed board, as a confirmed-not-durable link would.
    target.storage.mark_project_import_pending(project_id()).unwrap();
    let paths = std::sync::Arc::new(target.storage.paths().clone());
    drop(target);

    let config_file = paths.config_file();
    let valid_config = fs::read(&config_file).unwrap();
    let record = paths.pending_project_import_file(project_id());
    corrupt_config_keeping_projects(&config_file);
    let corrupt_config = fs::read(&config_file).unwrap();
    let storage = crate::config::Storage::with_paths((*paths).clone());
    assert!(storage.load_config_strict().is_err());

    // The salvaged configuration carries no Project, so its board must not load alone.
    let (state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert!(!state.project.projects.read().await.contains_key(&project_id()));
    assert!(state
        .task
        .tasks
        .read()
        .await
        .values()
        .all(|task| task.project_id != project_id()));
    assert_eq!(report.project_import_recovery_failures.len(), 1);
    assert!(record.exists(), "the record is kept");
    assert_eq!(
        fs::read(&config_file).unwrap(),
        corrupt_config,
        "a salvaged configuration is not rewritten"
    );

    // Once the configuration loads completely, the next start restores the import.
    fs::write(&config_file, &valid_config).unwrap();
    let (state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert!(report.project_import_recovery_failures.is_empty());
    assert!(state.project.projects.read().await.contains_key(&project_id()));
    assert_eq!(
        state
            .task
            .tasks
            .read()
            .await
            .values()
            .filter(|task| task.project_id == project_id())
            .count(),
        2
    );
    assert!(!record.exists());
}

#[tokio::test]
async fn an_unreadable_record_holds_back_an_interrupted_import_until_it_is_repaired() {
    let (_src, _dst, _bytes, paths, _path) = interrupted_import().await;
    let record = paths.pending_project_import_file(project_id());
    let valid_record = fs::read(&record).unwrap();
    fs::write(&record, b"garbage").unwrap();
    let config_before = fs::read(paths.config_file()).unwrap();

    let (state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert!(!state.project.projects.read().await.contains_key(&project_id()));
    assert_eq!(report.project_import_recovery_failures.len(), 1);
    assert_eq!(fs::read(&record).unwrap(), b"garbage", "the record is kept");
    assert_eq!(
        fs::read(paths.config_file()).unwrap(),
        config_before,
        "the configuration is not changed for an untrusted record"
    );

    // Repaired, the record settles the interrupted import as usual.
    fs::write(&record, &valid_record).unwrap();
    let (state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert!(report.project_import_recovery_failures.is_empty());
    assert!(!state.project.projects.read().await.contains_key(&project_id()));
    assert!(!record.exists());
}

#[tokio::test]
async fn a_held_back_project_cannot_be_imported_again_over_its_record() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);
    assert!(apply_with(&target, &path, &checksum_of(&bytes)).await.ok);
    let record = target.storage.paths().pending_project_import_file(project_id());
    fs::write(&record, b"garbage").unwrap();
    let paths = std::sync::Arc::new(target.storage.paths().clone());
    drop(target);

    let (_state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert_eq!(report.project_import_recovery_failures.len(), 1);
    let config_before = fs::read(paths.config_file()).unwrap();

    // A fresh instance: nothing in memory names the Project, only the disk does.
    let retry = crate::test_helpers::ipc_test_context(paths.clone());
    let response = apply_with(&retry, &path, &checksum_of(&bytes)).await;
    assert!(!response.ok, "a held-back Project is not imported over its record");
    assert_eq!(fs::read(&record).unwrap(), b"garbage", "the record is kept");
    assert_eq!(fs::read(paths.config_file()).unwrap(), config_before);
    assert!(retry.tasks.read().await.is_empty());

    let (state, _report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert!(state
        .task
        .tasks
        .read()
        .await
        .values()
        .all(|task| task.project_id != project_id()));
}

#[tokio::test]
async fn a_held_back_project_keeps_its_pending_conversation_deletion() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);
    assert!(apply_with(&target, &path, &checksum_of(&bytes)).await.ok);
    let conversation = target.storage.create_primary_conversation(project_id()).unwrap();
    target
        .storage
        .mark_project_conversation_deletion_pending(project_id())
        .unwrap();
    let paths = std::sync::Arc::new(target.storage.paths().clone());
    let pointer = paths.primary_conversation_file(project_id());
    let document = paths.conversation_file(conversation.id);
    let record = paths.pending_project_import_file(project_id());
    drop(target);
    fs::write(&record, b"garbage").unwrap();

    let (_state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert_eq!(report.project_import_recovery_failures.len(), 1);
    assert!(
        pointer.exists() && document.exists(),
        "absence from memory is not deletion: the Conversation stays"
    );
    assert!(paths
        .pending_project_conversation_deletion_file(project_id())
        .exists());
}

#[tokio::test]
async fn a_record_without_its_project_holds_back_the_board_it_cannot_own() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);
    assert!(apply_with(&target, &path, &checksum_of(&bytes)).await.ok);
    // The configuration lost the Project's entry while its board and record remain.
    crate::commands::project::try_persist_projects(&target.storage, &HashMap::new()).unwrap();
    target.storage.mark_project_import_pending(project_id()).unwrap();
    let paths = std::sync::Arc::new(target.storage.paths().clone());
    let record = paths.pending_project_import_file(project_id());
    drop(target);

    let (state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert_eq!(report.project_import_recovery_failures.len(), 1);
    assert!(state
        .task
        .tasks
        .read()
        .await
        .values()
        .all(|task| task.project_id != project_id()));
    assert!(record.exists(), "the record is kept: the import cannot be shown complete");
    let storage = crate::config::Storage::with_paths((*paths).clone());
    assert!(storage.tasks_board_files(project_id()).iter().any(|file| file.exists()));
}

#[tokio::test]
async fn a_record_whose_name_is_not_the_canonical_id_is_kept_and_reported() {
    let (tmp, ctx) = test_ctx().await;
    crate::commands::project::try_persist_projects(&ctx.storage, &HashMap::new()).unwrap();
    let paths = std::sync::Arc::new(ctx.storage.paths().clone());
    let id = Uuid::parse_str("8f3c2a1e-9b7d-4c6e-a5f0-1d2e3f4a5b6c").unwrap();
    let lower = id.to_string();
    let upper = lower.to_uppercase();
    assert_ne!(lower, upper);
    let record = paths.config_dir().join(format!("{upper}.import-pending"));
    fs::write(&record, &lower).unwrap();
    drop(ctx);

    let (_state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert_eq!(report.project_import_recovery_failures.len(), 1);
    assert!(record.exists(), "a record that cannot be cleared by its canonical path is kept");
    drop(tmp);
}

#[tokio::test]
async fn a_held_back_entry_survives_a_settlement_and_a_later_save() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);
    assert!(apply_with(&target, &path, &checksum_of(&bytes)).await.ok);
    // Held back: a completed import whose record was damaged.
    let held_record = target.storage.paths().pending_project_import_file(project_id());
    fs::write(&held_record, b"garbage").unwrap();
    // Settled in the same startup: an interrupted import of another Project.
    let other_id = Uuid::from_u128(77);
    let mut other = super::tests::project(None);
    other.id = other_id;
    other.name = "Interrupted".into();
    let mut projects = HashMap::from([(other_id, other)]);
    projects.insert(project_id(), super::tests::project(None));
    crate::commands::project::try_persist_projects(&target.storage, &projects).unwrap();
    target.storage.mark_project_import_pending(other_id).unwrap();
    let paths = std::sync::Arc::new(target.storage.paths().clone());
    drop(target);

    let (state, report) = crate::app_core::build_state_with_paths(paths.clone())
        .await
        .unwrap();
    assert_eq!(report.project_import_recovery_failures.len(), 1);
    assert!(!state.project.projects.read().await.contains_key(&other_id));
    let storage = crate::config::Storage::with_paths((*paths).clone());
    assert!(storage.load_config().unwrap().projects.contains_key(&project_id().to_string()));
    assert!(!storage.load_config().unwrap().projects.contains_key(&other_id.to_string()));

    // A later save by this running instance must not drop the held-back entry.
    let memory = state.project.projects.read().await.clone();
    crate::commands::project::try_persist_projects(&state.storage, &memory).unwrap();
    assert!(
        storage
            .load_config()
            .unwrap()
            .projects
            .contains_key(&project_id().to_string()),
        "a save does not drop a held-back Project"
    );
}

#[tokio::test]
async fn an_import_whose_configuration_is_not_confirmed_keeps_its_record() {
    let (src, source) = test_ctx().await;
    seed(&source, None, &fixture_tasks()).await;
    let bytes = exported_bytes(&source, &src).await;
    let (dst, target) = test_ctx().await;
    let path = write_bundle(&dst, &bytes);

    let _fault = fault::fail_at_file(Stage::SyncDirectory, ErrorKind::Unsupported, "config.toml");
    let response = apply_with(&target, &path, &checksum_of(&bytes)).await;
    drop(_fault);
    assert!(response.ok, "{:?}", response.error);
    assert_eq!(response.data["durable"], false);
    // The board is durable, but the Project's entry is not. The record stays, so
    // the next start can hold the import back if the entry was lost.
    assert_eq!(
        target.storage.pending_project_imports().unwrap(),
        vec![project_id()]
    );
}
