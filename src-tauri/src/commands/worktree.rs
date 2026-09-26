use uuid::Uuid;
use crate::domain::BranchOrigin;
use crate::worktree::{WorktreeInfo, WorktreeManager};

#[tauri::command]
pub async fn create_worktree(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<String, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;

    // Acquiring a worktree for a task is a lifecycle ownership change, so it
    // waits behind any cleanup, terminalization or delete already running for
    // the same task. Without this, a create could hand back the very directory
    // a removal is midway through deleting.
    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    // A cleanup that a previous process never finished leaves the recorded
    // checkout untrustworthy: nothing on disk says how much of it a dead
    // `git worktree remove` already took apart. Reattaching to it would hand an
    // agent a half-removed directory. See `Task::cleanup_in_flight`.
    {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).ok_or("Task not found")?;
        if task.cleanup_in_flight {
            return Err(format!(
                "task {task_id} has a worktree cleanup that was interrupted and not yet \
                 resolved; it needs attention before a worktree can be attached again"
            ));
        }
    }

    // Resolve repo path
    let repo_path = {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).ok_or("Task not found")?;
        let project_id = task.project_id;
        drop(tasks);

        let projects = state.project.projects.read().await;
        let project = projects.get(&project_id).ok_or("Project not found")?;
        let repo_id = project.repository_id.ok_or("No repository linked")?;
        drop(projects);

        let repos = state.repository.repositories.read().await;
        let repo = repos.get(&repo_id).ok_or("Repository not found")?;
        repo.local_path.clone()
    };

    // Check if task already has a branch (reattach) or needs a new one
    let existing_branch = {
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_id).and_then(|t| t.branch_name.clone())
    };

    let acquired = acquire_checkout(
        &state.worktree_manager,
        &repo_path,
        existing_branch.as_deref(),
        task_id,
    )
    .await?;

    // Update task
    {
        let mut tasks = state.task.tasks.write().await;
        if let Some(task) = tasks.get_mut(&task_id) {
            task.worktree_path = Some(acquired.info.path.clone());
            task.branch_name = Some(acquired.info.branch.clone());
            if let Some(base_commit) = acquired.base_commit {
                task.base_commit = Some(base_commit);
            }
            if let Some(origin) = acquired.origin {
                task.branch_origin = Some(origin);
            }
            task.updated_at = chrono::Utc::now();

            // Persist
            let project_id = task.project_id;
            let project_tasks: Vec<_> = tasks.values()
                .filter(|t| t.project_id == project_id)
                .cloned()
                .collect();
            let _ = state.storage.save_project_tasks(project_id, &project_tasks);
        }
    }

    Ok(acquired.info.path)
}

/// The checkout [`create_worktree`] acquired, and what it establishes about
/// the task's branch.
struct AcquiredCheckout {
    info: WorktreeInfo,
    /// The commit the branch started from, when this call created it or
    /// adopted a worktree the task never recorded. `None` on reattach, which
    /// keeps the recorded one.
    base_commit: Option<String>,
    /// What the branch was created from, when that is known. `None` leaves
    /// whatever the task already records.
    origin: Option<BranchOrigin>,
}

/// Attach `existing_branch` again, or create (or adopt) the task's own
/// branch when it has none yet.
async fn acquire_checkout(
    manager: &WorktreeManager,
    repo_path: &str,
    existing_branch: Option<&str>,
    task_id: Uuid,
) -> Result<AcquiredCheckout, String> {
    let (info, adopted) = match existing_branch {
        Some(branch) => (manager.reattach(repo_path, branch).await?, false),
        None => {
            manager
                .create_or_adopt(repo_path, &WorktreeManager::branch_for_task(task_id))
                .await?
        }
    };
    let is_fresh = existing_branch.is_none();
    // Captured once, only for a fresh worktree, matching
    // `queue::executor::spawn_task_execution`'s canonical-diff boundary
    // contract: a reattach must keep comparing against the task's original
    // starting point, not wherever `HEAD` is now. Read from the *new*
    // worktree's own `HEAD` (not `repo_path`'s) so nothing else can move it
    // out from under this call between creation and here.
    let base_commit = if is_fresh {
        tokio::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&info.path)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        None
    };
    // This path never stacks: a branch it creates starts wherever the
    // backend starts an ordinary branch, whatever the task depends on. That
    // is recorded as the default base only when `origin/HEAD` provably
    // contains it, by the same check the executor uses (see
    // `WorktreeManager::default_base_origin`); otherwise the origin is
    // unknown. A reattach keeps the origin recorded when the branch was
    // created, and an adopted worktree was made by something that recorded
    // none.
    let origin = if is_fresh && !adopted {
        WorktreeManager::default_base_origin(repo_path, base_commit.as_deref()).await
    } else {
        None
    };
    Ok(AcquiredCheckout { info, base_commit, origin })
}

/// Remove a task's worktree at the user's explicit request, without saying
/// anything about whether the task is finished.
///
/// Shares [`crate::lifecycle::terminalize_leased`] with the terminal path, so
/// the destructive step, its durable interrupted-cleanup record and its
/// persist-before-publish clearing are the same code. `desired` is the task's
/// current status precisely because this is not a terminal transition: a user
/// discarding a checkout has not said the work is over.
///
/// A refusal reaches the dialog with git's own reason, and the worktree, the
/// branch and everything uncommitted inside survive. The user can press the
/// button again once they have dealt with whatever git objected to.
#[tauri::command]
pub async fn cleanup_worktree(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<(), String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;

    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    let current_status = {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).ok_or("Task not found")?;
        if task.worktree_path.is_none() {
            return Err("No worktree for this task".to_string());
        }
        task.status.clone()
    };

    crate::lifecycle::terminalize_leased(
        crate::commands::task::terminalize_ctx(&state),
        task_id,
        crate::lifecycle::Origin::User,
        crate::lifecycle::TerminalizeRequest::new(current_status),
    )
    .await
    .map(|_| ())
    .map_err(|refusal| refusal.to_string())
}

#[tauri::command]
pub async fn check_worktree_exists(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<bool, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let tasks = state.task.tasks.read().await;

    if let Some(task) = tasks.get(&task_id) {
        if let Some(ref wt_path) = task.worktree_path {
            return Ok(state.worktree_manager.exists(wt_path));
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Run git in `dir` with a fixed identity, asserting it succeeds.
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A repository with one commit on `main`, pushed to a bare `origin`
    /// whose `HEAD` is recorded locally as `refs/remotes/origin/HEAD`, and a
    /// manager that places worktrees itself under `temp`.
    fn fixture(temp: &tempfile::TempDir) -> (std::path::PathBuf, WorktreeManager) {
        let repo = temp.path().join("repository");
        let remote = temp.path().join("origin.git");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        git(&repo, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        git(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git(&repo, &["push", "-q", "origin", "main"]);
        git(&repo, &["remote", "set-head", "origin", "main"]);
        let manager = WorktreeManager::new(
            std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
                temp.path().join("config"),
                temp.path().join("data"),
                temp.path().join("cache"),
                temp.path().join("runtime"),
            )),
            crate::config::paths::WorktreePlacement::Managed,
        );
        (repo, manager)
    }

    /// A worktree created from the Worktree panel while the primary checkout
    /// is on a feature branch starts at that branch's tip, and is not
    /// recorded as coming from the default base.
    #[tokio::test]
    async fn a_worktree_created_from_a_feature_checkout_records_no_origin() {
        let temp = tempfile::TempDir::new().unwrap();
        let (repo, manager) = fixture(&temp);
        git(&repo, &["checkout", "-q", "-b", "feature-f"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "feature work"]);
        let feature_tip = git(&repo, &["rev-parse", "HEAD"]);

        let acquired = acquire_checkout(&manager, repo.to_str().unwrap(), None, Uuid::new_v4())
            .await
            .expect("a worktree");

        assert_eq!(git(Path::new(&acquired.info.path), &["rev-parse", "HEAD"]), feature_tip);
        assert_eq!(acquired.base_commit.as_deref(), Some(feature_tip.as_str()));
        assert_eq!(acquired.origin, None);
    }

    /// Started on a commit `origin/HEAD` contains, the same path records the
    /// default base.
    #[tokio::test]
    async fn a_worktree_created_on_the_default_base_records_it() {
        let temp = tempfile::TempDir::new().unwrap();
        let (repo, manager) = fixture(&temp);
        let main_tip = git(&repo, &["rev-parse", "main"]);

        let acquired = acquire_checkout(&manager, repo.to_str().unwrap(), None, Uuid::new_v4())
            .await
            .expect("a worktree");

        assert_eq!(acquired.base_commit.as_deref(), Some(main_tip.as_str()));
        assert_eq!(acquired.origin, Some(BranchOrigin::DefaultBase));
    }

    /// A reattach records neither a starting commit nor an origin, so the
    /// ones recorded when the branch was created are kept.
    #[tokio::test]
    async fn reattaching_records_no_base_and_no_origin() {
        let temp = tempfile::TempDir::new().unwrap();
        let (repo, manager) = fixture(&temp);
        git(&repo, &["branch", "task-legacy"]);

        let acquired =
            acquire_checkout(&manager, repo.to_str().unwrap(), Some("task-legacy"), Uuid::new_v4())
                .await
                .expect("a worktree");

        assert_eq!(acquired.info.branch, "task-legacy");
        assert_eq!(acquired.base_commit, None);
        assert_eq!(acquired.origin, None);
    }
}
