//! Read-only accounting of the disk SlashIt uses.
//!
//! - [`walk`] measures one directory tree without following links out of it.
//! - [`inventory`] decides what each thing under SlashIt's directories is and
//!   whose it is, and totals it into a
//!   [`StorageSummary`](crate::domain::storage_usage::StorageSummary).
//! - [`refresh`] runs at most one measurement at a time.
//!
//! Nothing here deletes, moves or changes anything. The model and what each
//! total means are documented in [`crate::domain::storage_usage`].

pub mod inventory;
pub mod refresh;
pub mod walk;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{OnceCell, RwLock};
use uuid::Uuid;

use crate::config::paths::{AppPaths, ProjectKey};
use crate::domain::storage_usage::{FilesystemSpace, PressurePolicy};
use crate::domain::{Project, Repository, Task};
use crate::queue::TaskExecutor;
use inventory::{GitBuildOutputProbe, Inputs, RecordedCheckout};
pub use refresh::StorageAccounting;

/// Where the accounting reads tasks, and whether agents are working in
/// them, from.
#[derive(Clone)]
pub struct Sources {
    pub tasks: Arc<RwLock<HashMap<Uuid, Task>>>,
    pub projects: Arc<RwLock<HashMap<Uuid, Project>>>,
    pub repositories: Arc<RwLock<HashMap<Uuid, Repository>>>,
    /// Set once the application has started its executor. Until then no
    /// agent can be working anywhere.
    pub executor: Arc<OnceCell<Arc<TaskExecutor>>>,
}

/// The accounting the application runs: SlashIt's own directories, the
/// tasks on its boards, and the real filesystem.
pub fn for_app(paths: Arc<AppPaths>, sources: Sources) -> StorageAccounting {
    StorageAccounting::new(Box::new(move || {
        let (paths, sources) = (paths.clone(), sources.clone());
        Box::pin(async move {
            let recorded = recorded_checkouts(&sources).await;
            tokio::task::spawn_blocking(move || {
                let checkouts = with_worktree_roots(&paths, recorded);
                inventory::measure(&Inputs {
                    paths: &paths,
                    checkouts: &checkouts,
                    probe: &GitBuildOutputProbe,
                    space: &filesystem_space,
                    policy: PressurePolicy::default(),
                    limits: walk::WalkLimits::default(),
                })
            })
            .await
            .map_err(|e| format!("the measurement stopped unexpectedly: {e}"))
        })
    }))
}

/// Every Task Checkout a task records, as the task records it, with whether
/// an agent is working in it and the task's repository path.
///
/// Holds one lock at a time, so it cannot take part in a lock-order
/// inversion with a writer elsewhere.
async fn recorded_checkouts(sources: &Sources) -> Vec<(RecordedCheckout, Option<String>)> {
    let recorded: Vec<(Uuid, Uuid, RecordedCheckout)> = sources
        .tasks
        .read()
        .await
        .values()
        .filter_map(|task| {
            let path = task.worktree_path.as_deref()?;
            Some((
                task.id,
                task.project_id,
                RecordedCheckout {
                    task_title: task.title.clone(),
                    project_name: None,
                    status: task.status.clone(),
                    cleanup_in_flight: task.cleanup_in_flight,
                    agent_attached: false,
                    path: PathBuf::from(path),
                    worktree_root: None,
                },
            ))
        })
        .collect();

    let projects: HashMap<Uuid, (String, Option<Uuid>)> = sources
        .projects
        .read()
        .await
        .iter()
        .map(|(id, p)| (*id, (p.name.clone(), p.repository_id)))
        .collect();
    let repositories: HashMap<Uuid, String> = sources
        .repositories
        .read()
        .await
        .iter()
        .map(|(id, r)| (*id, r.local_path.clone()))
        .collect();

    let mut checkouts = Vec::with_capacity(recorded.len());
    for (task_id, project_id, mut checkout) in recorded {
        let mut repository = None;
        if let Some((name, repository_id)) = projects.get(&project_id) {
            checkout.project_name = Some(name.clone());
            repository = repository_id.and_then(|id| repositories.get(&id)).cloned();
        }
        checkout.agent_attached = match sources.executor.get() {
            Some(executor) => executor.is_task_running(task_id).await,
            None => false,
        };
        checkouts.push((checkout, repository));
    }
    checkouts
}

/// Fill in where SlashIt would have placed each checkout. Blocking: a
/// repository's key canonicalizes its path, which may be on a slow mount, so
/// this runs with the walk rather than on the async runtime, once per
/// repository.
fn with_worktree_roots(paths: &AppPaths, recorded: Vec<(RecordedCheckout, Option<String>)>) -> Vec<RecordedCheckout> {
    let mut roots: HashMap<String, PathBuf> = HashMap::new();
    recorded
        .into_iter()
        .map(|(mut checkout, repository)| {
            checkout.worktree_root = repository.map(|repo| {
                roots
                    .entry(repo)
                    .or_insert_with_key(|repo| paths.worktrees_root(&ProjectKey::for_path(Path::new(repo)).key))
                    .clone()
            });
            checkout
        })
        .collect()
}

pub(crate) fn filesystem_space(path: &Path) -> std::io::Result<FilesystemSpace> {
    let stats = fs4::statvfs(path)?;
    Ok(FilesystemSpace {
        total_bytes: stats.total_space(),
        available_bytes: stats.available_space(),
    })
}
