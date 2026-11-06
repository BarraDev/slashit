// SPDX-License-Identifier: MPL-2.0
//! Import apply: the round trip, inertness, refusals that write nothing, the
//! failure points of the commit order, and a concurrent double apply. Driven
//! through the IPC dispatcher, so the production handlers are what is tested.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

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
    let mut pending = vec![ctx.storage.paths().data_dir().to_path_buf()];
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
