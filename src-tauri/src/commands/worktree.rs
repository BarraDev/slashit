use uuid::Uuid;
use crate::domain::{BranchOrigin, ProjectBase};
use crate::worktree::{WorktreeInfo, WorktreeManager};

#[tauri::command]
pub async fn create_worktree(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<String, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    create_worktree_inner(&state, task_id).await
}

/// [`create_worktree`], on an already parsed task id.
async fn create_worktree_inner(state: &crate::AppState, task_id: Uuid) -> Result<String, String> {

    // Acquiring a worktree for a task is a lifecycle ownership change, so it
    // waits behind any cleanup, terminalization or delete already running for
    // the same task. Without this, a create could hand back the very directory
    // a removal is midway through deleting.
    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    // Creating a checkout is what grows the disk, so it waits while new work
    // is paused for disk space, before anything is read or created.
    state.start_guard.check().await.map_err(|block| block.to_string())?;

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

    // Resolve repo path, and the project's local base
    let (repo_path, project_base) = {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).ok_or("Task not found")?;
        let project_id = task.project_id;
        drop(tasks);

        let projects = state.project.projects.read().await;
        let project = projects.get(&project_id).ok_or("Project not found")?;
        let repo_id = project.repository_id.ok_or("No repository linked")?;
        let project_base = project.base.clone();
        drop(projects);

        let repos = state.repository.repositories.read().await;
        let repo = repos.get(&repo_id).ok_or("Repository not found")?;
        (repo.local_path.clone(), project_base)
    };

    // Check if task already has a branch (reattach) or needs a new one
    let existing_branch = {
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_id).and_then(|t| t.branch_name.clone())
    };

    // A branch another task in the repository can claim is not this task's
    // to reattach, adopt or create. See `worktree::refuse_shared_task_branch`.
    let branch = existing_branch.clone().unwrap_or_else(|| WorktreeManager::branch_for_task(task_id));
    crate::worktree::refuse_shared_task_branch(
        &state.task.tasks,
        &state.project.projects,
        &state.repository.repositories,
        task_id,
        &branch,
        &repo_path,
    )
    .await?;

    let acquired = acquire_checkout(
        &state.worktree_manager,
        &repo_path,
        existing_branch.as_deref(),
        project_base.as_ref(),
        task_id,
    )
    .await?;

    // Recorded before it is reported, and taken back if it cannot be: see
    // `WorktreeManager::undo_created_checkout` for why a checkout left behind
    // unrecorded costs the task its starting commit and origin.
    let amend = |staged: &mut std::collections::HashMap<Uuid, crate::domain::Task>| {
        if let Some(task) = staged.get_mut(&task_id) {
            task.worktree_path = Some(acquired.info.path.clone());
            task.branch_name = Some(acquired.info.branch.clone());
            if let Some(base_commit) = &acquired.base_commit {
                task.base_commit = Some(base_commit.clone());
            }
            if let Some(origin) = &acquired.origin {
                task.branch_origin = Some(origin.clone());
            }
        }
    };
    if let Err(e) =
        crate::lifecycle::record(&state.task.tasks, &state.storage, task_id, &amend).await
    {
        let outcome = state
            .worktree_manager
            .release_unrecorded(&repo_path, &acquired.info, acquired.created_at.as_deref())
            .await;
        return Err(format!("The task's checkout could not be recorded ({e}); {outcome}"));
    }

    Ok(acquired.info.path)
}

/// The checkout [`create_worktree`] acquired, and what it establishes about
/// the task's branch.
struct AcquiredCheckout {
    info: WorktreeInfo,
    /// The commit the branch started from, when this call created it.
    /// `None` on reattach and adoption, which keep whatever was recorded.
    base_commit: Option<String>,
    /// What the branch was created from, when that is known. `None` leaves
    /// whatever the task already records.
    origin: Option<BranchOrigin>,
    /// The commit this call created the branch at, together with its
    /// worktree. `None` when the branch already existed, whether its
    /// worktree was adopted or added: nothing of that is this call's to take
    /// back.
    created_at: Option<String>,
}

/// Attach `existing_branch` again, or create (or adopt) the task's own
/// branch when it has none yet.
async fn acquire_checkout(
    manager: &WorktreeManager,
    repo_path: &str,
    existing_branch: Option<&str>,
    project_base: Option<&ProjectBase>,
    task_id: Uuid,
) -> Result<AcquiredCheckout, String> {
    // A reattach keeps the starting commit and origin recorded when the
    // branch was created.
    if let Some(branch) = existing_branch {
        let info = manager.reattach(repo_path, branch).await?;
        return Ok(AcquiredCheckout { info, base_commit: None, origin: None, created_at: None });
    }
    // This path never stacks: a branch it creates starts at the resolved
    // default base, exactly as the executor's ordinary branches do (see
    // `WorktreeManager::create_or_adopt`), whatever the task depends on.
    match manager
        .create_or_adopt(repo_path, &WorktreeManager::branch_for_task(task_id), project_base)
        .await?
    {
        (info, Some(base)) => Ok(AcquiredCheckout {
            info,
            created_at: Some(base.commit.clone()),
            origin: Some(base.branch_origin()),
            base_commit: Some(base.commit),
        }),
        // An adopted worktree claims no start: its `HEAD` is not where the
        // branch started, and whatever the task already recorded is kept.
        (info, None) => {
            Ok(AcquiredCheckout { info, base_commit: None, origin: None, created_at: None })
        }
    }
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
        );
        (repo, manager)
    }

    /// A worktree created from the Worktree panel while the primary checkout
    /// is on a feature branch starts at `origin/main`, not at that branch's
    /// tip, and records `main` as its default base.
    #[tokio::test]
    async fn a_worktree_created_from_a_feature_checkout_starts_at_the_default_base() {
        let temp = tempfile::TempDir::new().unwrap();
        let (repo, manager) = fixture(&temp);
        let main_tip = git(&repo, &["rev-parse", "refs/remotes/origin/main"]);
        git(&repo, &["checkout", "-q", "-b", "feature-f"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "feature work"]);

        let acquired = acquire_checkout(&manager, repo.to_str().unwrap(), None, None, Uuid::new_v4())
            .await
            .expect("a worktree");

        assert_eq!(git(Path::new(&acquired.info.path), &["rev-parse", "HEAD"]), main_tip);
        assert_eq!(acquired.base_commit.as_deref(), Some(main_tip.as_str()));
        assert_eq!(
            acquired.origin,
            Some(BranchOrigin::DefaultBase { branch: Some("main".to_string()) })
        );
    }

    /// A repository with no default base is refused, and nothing is created
    /// in it.
    #[tokio::test]
    async fn a_repository_without_a_default_base_is_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        let (repo, manager) = fixture(&temp);
        git(&repo, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
        let refs_before = git(&repo, &["for-each-ref", "--format=%(refname) %(objectname)"]);

        let refused = acquire_checkout(&manager, repo.to_str().unwrap(), None, None, Uuid::new_v4())
            .await
            .err()
            .expect("refused");

        assert!(refused.contains("git remote set-head origin"), "{refused}");
        assert_eq!(git(&repo, &["for-each-ref", "--format=%(refname) %(objectname)"]), refs_before);
        assert_eq!(git(&repo, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
    }

    /// A worktree of the task's branch that git has registered at a path of
    /// another tool's choosing is adopted there, with no origin claimed.
    #[tokio::test]
    async fn a_registered_worktree_at_a_custom_path_is_adopted_without_an_origin() {
        let temp = tempfile::TempDir::new().unwrap();
        let (repo, manager) = fixture(&temp);
        let task_id = Uuid::new_v4();
        let branch = WorktreeManager::branch_for_task(task_id);
        let custom = temp.path().join("elsewhere").join(&branch);
        git(&repo, &["worktree", "add", "-q", "-b", &branch, "--", custom.to_str().unwrap()]);

        let acquired = acquire_checkout(&manager, repo.to_str().unwrap(), None, None, task_id)
            .await
            .expect("adopted");

        assert_eq!(
            std::fs::canonicalize(&acquired.info.path).unwrap(),
            std::fs::canonicalize(&custom).unwrap()
        );
        assert_eq!(acquired.base_commit, None, "an adopted worktree's HEAD is not its start");
        assert_eq!(acquired.origin, None);
    }

    /// A reattach records neither a starting commit nor an origin, so the
    /// ones recorded when the branch was created are kept.
    #[tokio::test]
    async fn reattaching_records_no_base_and_no_origin() {
        let temp = tempfile::TempDir::new().unwrap();
        let (repo, manager) = fixture(&temp);
        git(&repo, &["branch", "task-legacy"]);

        let acquired =
            acquire_checkout(&manager, repo.to_str().unwrap(), Some("task-legacy"), None, Uuid::new_v4())
                .await
                .expect("a worktree");

        assert_eq!(acquired.info.branch, "task-legacy");
        assert_eq!(acquired.base_commit, None);
        assert_eq!(acquired.origin, None);
    }

    /// Two tasks whose ids share their first 8 hex digits, which was the
    /// whole of the branch name `task-12345678` earlier versions gave both,
    /// given checkouts from the Worktree panel.
    mod branch_ownership {
        use super::*;
        use crate::domain::TaskStatus;

        const A: Uuid = Uuid::from_u128(0x12345678_0000_4000_8000_000000000001);
        const B: Uuid = Uuid::from_u128(0x12345678_0000_4000_8000_000000000002);
        const BRANCH: &str = "task-12345678";
        const PLACEMENTS: [&str; 3] = ["managed", "legacy", "custom"];

        /// App state holding a repository with a published default base,
        /// registered as a project's, and tasks `A` and `B` recording
        /// `a_branch` and `b_branch`.
        async fn state_with_two_tasks(
            temp: &tempfile::TempDir,
            a_branch: Option<&str>,
            b_branch: Option<&str>,
        ) -> (crate::AppState, std::path::PathBuf) {
            let (repo, _) = fixture(temp);
            let (state, _) =
                crate::app_core::build_state_with_paths(paths(temp)).await.expect("state");
            let repository = crate::domain::Repository {
                id: Uuid::new_v4(),
                local_path: repo.to_string_lossy().to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            };
            let repository_id = repository.id;
            state.repository.repositories.write().await.insert(repository_id, repository);
            let project = crate::domain::Project {
                id: Uuid::new_v4(),
                name: "project".to_string(),
                repository_id: Some(repository_id),
                scope: crate::domain::ProjectScope::Standalone,
                state_location: crate::config::paths::StateLocation::External,
                base: None,
                agent_type: crate::domain::AgentType::ClaudeCode,
                agent_config: crate::domain::AgentConfig {
                    agent_type: crate::domain::AgentType::ClaudeCode,
                    command: "claude".to_string(),
                    args: Vec::new(),
                    env: std::collections::HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            let project_id = project.id;
            state.project.projects.write().await.insert(project_id, project);
            for (id, branch) in [(A, a_branch), (B, b_branch)] {
                let mut task = crate::test_helpers::create_test_task_full(
                    &format!("task {id}"),
                    project_id,
                    TaskStatus::InProgress,
                    0,
                );
                task.id = id;
                task.branch_name = branch.map(str::to_string);
                state.task.tasks.write().await.insert(id, task);
            }
            (state, repo)
        }

        fn paths(temp: &tempfile::TempDir) -> std::sync::Arc<crate::config::paths::AppPaths> {
            std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
                temp.path().join("config"),
                temp.path().join("data"),
                temp.path().join("cache"),
                temp.path().join("runtime"),
            ))
        }

        /// Put a checkout of `branch` at the managed path, the legacy
        /// sibling path, or a custom one, returning where it is.
        async fn place_checkout(
            state: &crate::AppState,
            temp: &tempfile::TempDir,
            repo: &Path,
            branch: &str,
            placement: &str,
        ) -> String {
            let path = match placement {
                "managed" => {
                    return state
                        .worktree_manager
                        .create(repo.to_str().unwrap(), branch)
                        .await
                        .expect("a checkout at the managed path")
                        .path;
                }
                "legacy" => repo.parent().unwrap().join(format!("repository.{branch}")),
                _ => temp.path().join("elsewhere").join(branch),
            };
            git(repo, &["worktree", "add", "-q", "-b", branch, "--", path.to_str().unwrap()]);
            path.to_string_lossy().to_string()
        }

        fn same_path(a: &str, b: &str) -> bool {
            std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(b).unwrap()
        }

        /// Task `B` gets a branch and checkout of its own beside the
        /// checkout of the old shared name that task `A` records, at every
        /// placement, and `A`'s branch and checkout are left as they were.
        #[tokio::test]
        async fn a_task_gets_its_own_branch_beside_a_checkout_another_task_records() {
            for placement in PLACEMENTS {
                let temp = tempfile::TempDir::new().unwrap();
                let (state, repo) = state_with_two_tasks(&temp, Some(BRANCH), None).await;
                let a_path = place_checkout(&state, &temp, &repo, BRANCH, placement).await;
                let a_tip = git(&repo, &["rev-parse", BRANCH]);

                let b_path = create_worktree_inner(&state, B)
                    .await
                    .unwrap_or_else(|e| panic!("{placement}: {e}"));

                let b = state.task.tasks.read().await[&B].clone();
                assert_eq!(b.branch_name, Some(WorktreeManager::branch_for_task(B)), "{placement}");
                assert!(!same_path(&a_path, &b_path), "{placement}: B was given A's checkout");
                assert_eq!(git(&repo, &["rev-parse", BRANCH]), a_tip, "{placement}");
                assert_eq!(git(Path::new(&a_path), &["branch", "--show-current"]), BRANCH);
            }
        }

        /// Task `A`, which records the old shared name, still gets its own
        /// checkout back at every placement beside task `B`.
        #[tokio::test]
        async fn a_task_reattaches_its_own_checkout_beside_a_task_with_the_same_prefix() {
            for placement in PLACEMENTS {
                let temp = tempfile::TempDir::new().unwrap();
                let (state, repo) = state_with_two_tasks(&temp, Some(BRANCH), None).await;
                let path = place_checkout(&state, &temp, &repo, BRANCH, placement).await;

                let acquired = create_worktree_inner(&state, A)
                    .await
                    .unwrap_or_else(|e| panic!("{placement}: {e}"));

                assert!(same_path(&acquired, &path), "{placement}");
            }
        }

        /// A task that records a branch named from its whole id, as earlier
        /// versions may also have written, reattaches its checkout at every
        /// placement.
        #[tokio::test]
        async fn a_task_recording_a_full_id_branch_reattaches_it() {
            let full = WorktreeManager::branch_for_task(A);
            for placement in PLACEMENTS {
                let temp = tempfile::TempDir::new().unwrap();
                let (state, repo) = state_with_two_tasks(&temp, Some(&full), None).await;
                let path = place_checkout(&state, &temp, &repo, &full, placement).await;

                let acquired = create_worktree_inner(&state, A)
                    .await
                    .unwrap_or_else(|e| panic!("{placement}: {e}"));

                assert!(same_path(&acquired, &path), "{placement}");
            }
        }

        /// Neither task recording a branch, each is given a new one of its
        /// own, whichever asks first.
        #[tokio::test]
        async fn two_tasks_sharing_a_prefix_are_each_given_their_own_branch() {
            let temp = tempfile::TempDir::new().unwrap();
            let (state, repo) = state_with_two_tasks(&temp, None, None).await;

            let a_path = create_worktree_inner(&state, A).await.expect("A");
            let b_path = create_worktree_inner(&state, B).await.expect("B");

            assert!(!same_path(&a_path, &b_path));
            let tasks = state.task.tasks.read().await;
            for id in [A, B] {
                assert_eq!(tasks[&id].branch_name, Some(WorktreeManager::branch_for_task(id)));
            }
            assert_eq!(git(&repo, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 3);
        }

        /// Two tasks recording one branch are both refused its checkout, and
        /// nothing about either is changed.
        #[tokio::test]
        async fn two_tasks_recording_one_branch_are_both_refused() {
            let temp = tempfile::TempDir::new().unwrap();
            let (state, repo) = state_with_two_tasks(&temp, Some(BRANCH), Some(BRANCH)).await;
            place_checkout(&state, &temp, &repo, BRANCH, "managed").await;
            let refs = git(&repo, &["for-each-ref", "--format=%(refname) %(objectname)"]);

            for (task, other) in [(A, B), (B, A)] {
                let refused = create_worktree_inner(&state, task).await.expect_err("refused");
                assert!(refused.contains(&other.to_string()), "{refused}");
                assert_eq!(state.task.tasks.read().await[&task].worktree_path, None);
            }
            assert_eq!(git(&repo, &["for-each-ref", "--format=%(refname) %(objectname)"]), refs);
        }

        /// A checkout of the old name that no task records, as a start whose
        /// record was never saved leaves it, is refused to the task it may
        /// belong to, at every placement, with nothing changed; once it is
        /// renamed to that task's name, as the refusal says, the task adopts
        /// it where it is.
        #[tokio::test]
        async fn an_unrecorded_checkout_of_the_old_name_is_adopted_only_once_renamed() {
            for placement in PLACEMENTS {
                let temp = tempfile::TempDir::new().unwrap();
                let (state, repo) = state_with_two_tasks(&temp, Some("feature/a"), None).await;
                let path = place_checkout(&state, &temp, &repo, BRANCH, placement).await;
                let refs = git(&repo, &["for-each-ref", "--format=%(refname) %(objectname)"]);

                let refused = create_worktree_inner(&state, B)
                    .await
                    .err()
                    .unwrap_or_else(|| panic!("{placement}: an orphan was taken or stranded"));
                let full = WorktreeManager::branch_for_task(B);
                assert!(refused.contains(&format!("git branch -m {BRANCH} {full}")), "{refused}");
                assert_eq!(state.task.tasks.read().await[&B].branch_name, None);
                assert_eq!(git(&repo, &["for-each-ref", "--format=%(refname) %(objectname)"]), refs);

                git(&repo, &["branch", "-m", BRANCH, &full]);
                let adopted = create_worktree_inner(&state, B)
                    .await
                    .unwrap_or_else(|e| panic!("{placement}: {e}"));

                assert!(same_path(&adopted, &path), "{placement}");
                assert_eq!(state.task.tasks.read().await[&B].branch_name, Some(full));
            }
        }

        /// A leftover of the old name with no checkout of it is taken up by
        /// exactly what the refusal advises: renamed to the task's name, and
        /// given a worktree, which the task then adopts where it is.
        #[tokio::test]
        async fn a_branch_only_old_name_leftover_is_taken_up_by_the_advised_remedy() {
            let temp = tempfile::TempDir::new().unwrap();
            let (state, repo) = state_with_two_tasks(&temp, Some("feature/a"), None).await;
            git(&repo, &["branch", BRANCH]);
            let full = WorktreeManager::branch_for_task(B);

            let refused = create_worktree_inner(&state, B).await.expect_err("refused");
            assert!(refused.contains(&format!("git branch -m {BRANCH} {full}")), "{refused}");
            assert!(refused.contains(&format!("git worktree add <directory> {full}")), "{refused}");

            git(&repo, &["branch", "-m", BRANCH, &full]);
            let refused = create_worktree_inner(&state, B)
                .await
                .expect_err("a renamed branch with no checkout is not created over");
            assert!(refused.contains("already exists"), "{refused}");

            let dir = temp.path().join("taken-up");
            git(&repo, &["worktree", "add", "-q", dir.to_str().unwrap(), &full]);
            let adopted = create_worktree_inner(&state, B).await.expect("adopted");

            assert!(same_path(&adopted, dir.to_str().unwrap()));
            assert_eq!(state.task.tasks.read().await[&B].branch_name, Some(full));
        }

        /// A task's branch and checkout survive a restart as recorded: its
        /// checkout removed, the app restarted from what it saved, and the
        /// Worktree panel used again, the task is given a new checkout of
        /// the same branch, with the work on it.
        #[tokio::test]
        async fn a_recorded_branch_is_reattached_after_its_checkout_is_removed_and_a_restart() {
            let temp = tempfile::TempDir::new().unwrap();
            let (state, repo) = state_with_two_tasks(&temp, None, None).await;
            let first = create_worktree_inner(&state, B).await.expect("first checkout");
            git(Path::new(&first), &["commit", "-q", "--allow-empty", "-m", "task work"]);
            let full = WorktreeManager::branch_for_task(B);
            let tip = git(&repo, &["rev-parse", &full]);
            git(&repo, &["worktree", "remove", "--", &first]);
            let mut config = crate::config::storage::AppConfig::default();
            for (id, repository) in state.repository.repositories.read().await.iter() {
                config.repositories.insert(id.to_string(), repository.clone());
            }
            for (id, project) in state.project.projects.read().await.iter() {
                config.projects.insert(id.to_string(), project.clone());
            }
            state.storage.save_config(&config).expect("save config");
            drop(state);

            let (state, _) =
                crate::app_core::build_state_with_paths(paths(&temp)).await.expect("restart");
            let reloaded = state.task.tasks.read().await[&B].clone();
            assert_eq!(reloaded.branch_name.as_deref(), Some(full.as_str()));
            assert_eq!(reloaded.worktree_path, None, "startup clears a checkout git confirms gone");

            let second = create_worktree_inner(&state, B).await.expect("second checkout");

            assert_eq!(git(Path::new(&second), &["rev-parse", "HEAD"]), tip);
            assert_eq!(git(Path::new(&second), &["branch", "--show-current"]), full);
        }
    }

    /// A checkout the Worktree command acquires for a task that cannot record
    /// it: refused, with the board and the disk still agreeing, and only what
    /// the command itself created taken back.
    ///
    /// Unix only: an unwritable directory is made with permission bits.
    #[cfg(unix)]
    mod unrecorded_acquisition {
        use super::*;
        use crate::domain::{Task, TaskStatus};

        /// How the command acquires the task's checkout.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Class {
            /// A new branch from the default base, and its worktree.
            Created,
            /// A worktree added for the branch the task records.
            AddedForRecorded,
            /// A worktree git already has registered for the recorded branch.
            ReattachedRegistered,
            /// A worktree registered for the branch the task would be given,
            /// which it never recorded.
            AdoptedUnrecorded,
        }

        struct World {
            temp: tempfile::TempDir,
            state: crate::AppState,
            repo: std::path::PathBuf,
            task_id: Uuid,
            project_id: Uuid,
            main_tip: String,
        }

        fn provenance(t: &Task) -> (Option<String>, Option<String>, Option<String>, Option<BranchOrigin>) {
            (t.worktree_path.clone(), t.branch_name.clone(), t.base_commit.clone(), t.branch_origin.clone())
        }

        fn on_main() -> BranchOrigin {
            BranchOrigin::DefaultBase { branch: Some("main".to_string()) }
        }

        fn refs(repo: &Path) -> String {
            git(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])
        }

        fn worktrees(repo: &Path) -> usize {
            git(repo, &["worktree", "list", "--porcelain"]).matches("worktree ").count()
        }

        /// A project over a repository with a published default base, one
        /// task in it set up for `class`, and the board seeded on disk.
        async fn world(class: Class) -> World {
            let temp = tempfile::TempDir::new().unwrap();
            let (repo, _) = fixture(&temp);
            let main_tip = git(&repo, &["rev-parse", "refs/remotes/origin/main"]);
            let paths = std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
                temp.path().join("config"),
                temp.path().join("data"),
                temp.path().join("cache"),
                temp.path().join("runtime"),
            ));
            let (state, _) = crate::app_core::build_state_with_paths(paths).await.expect("state");
            let repository = crate::domain::Repository {
                id: Uuid::new_v4(),
                local_path: repo.to_string_lossy().to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            };
            let repository_id = repository.id;
            state.repository.repositories.write().await.insert(repository_id, repository);
            let project_id = Uuid::new_v4();
            state.project.projects.write().await.insert(
                project_id,
                crate::domain::Project {
                    id: project_id,
                    name: "project".to_string(),
                    repository_id: Some(repository_id),
                    scope: crate::domain::ProjectScope::Standalone,
                    state_location: crate::config::paths::StateLocation::External,
                    base: None,
                    agent_type: crate::domain::AgentType::ClaudeCode,
                    agent_config: crate::domain::AgentConfig {
                        agent_type: crate::domain::AgentType::ClaudeCode,
                        command: "claude".to_string(),
                        args: Vec::new(),
                        env: std::collections::HashMap::new(),
                        model: None,
                        api_key: None,
                    },
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                },
            );

            let mut task = crate::test_helpers::create_test_task_full(
                "subject",
                project_id,
                TaskStatus::InProgress,
                0,
            );
            let task_id = task.id;
            let branch = WorktreeManager::branch_for_task(task_id);
            let elsewhere = temp.path().join("elsewhere").join(&branch);
            match class {
                Class::Created => {}
                Class::AddedForRecorded | Class::ReattachedRegistered => {
                    git(&repo, &["branch", &branch]);
                    task.branch_name = Some(branch.clone());
                    task.base_commit = Some(main_tip.clone());
                    task.branch_origin = Some(on_main());
                    if class == Class::ReattachedRegistered {
                        git(&repo, &["worktree", "add", "-q", "--", elsewhere.to_str().unwrap(), &branch]);
                    }
                }
                Class::AdoptedUnrecorded => {
                    git(&repo, &["worktree", "add", "-q", "-b", &branch, "--", elsewhere.to_str().unwrap()]);
                }
            }
            state.task.tasks.write().await.insert(task_id, task.clone());
            state.storage.save_project_tasks(project_id, &[task]).expect("seed the board");

            World { temp, state, repo, task_id, project_id, main_tip }
        }

        impl World {
            fn tasks_dir(&self) -> std::path::PathBuf {
                self.temp.path().join("config").join("tasks")
            }

            fn on_disk(&self) -> Task {
                self.state
                    .storage
                    .load_project_tasks(self.project_id)
                    .expect("load the board")
                    .into_iter()
                    .find(|t| t.id == self.task_id)
                    .expect("the task is on disk")
            }

            async fn in_memory(&self) -> Task {
                self.state.task.tasks.read().await[&self.task_id].clone()
            }

            /// What a restart does: read the board back from disk.
            async fn restart(&self) {
                let on_disk = self.on_disk();
                self.state.task.tasks.write().await.insert(self.task_id, on_disk);
            }
        }

        /// The command is refused, nothing it did is shown or recorded, and
        /// only a branch and worktree it created itself are taken back. Once
        /// the storage accepts writes again, asking again after a restart
        /// records where the branch started.
        async fn refused_then_recorded_after_retry(class: Class) {
            let w = world(class).await;
            let before = provenance(&w.in_memory().await);
            let (refs_before, worktrees_before) = (refs(&w.repo), worktrees(&w.repo));
            let Some(unwritable) = crate::test_helpers::UnwritableDir::new(&w.tasks_dir()) else {
                eprintln!("skipped: the tasks directory stays writable (running as root?)");
                return;
            };

            let refused = create_worktree_inner(&w.state, w.task_id)
                .await
                .expect_err("a checkout that cannot be recorded is not reported");

            assert!(refused.contains("could not be recorded"), "{class:?}: {refused}");
            assert_eq!(provenance(&w.in_memory().await), before, "{class:?}: the board");
            assert_eq!(provenance(&w.on_disk()), before, "{class:?}: the disk");
            assert_eq!(refs(&w.repo), refs_before, "{class:?}: no branch is left created or moved");
            let added = usize::from(class == Class::AddedForRecorded);
            assert_eq!(worktrees(&w.repo), worktrees_before + added, "{class:?}: worktrees");
            drop(unwritable);

            w.restart().await;
            create_worktree_inner(&w.state, w.task_id).await.expect("recorded once writes work");

            let recorded = w.on_disk();
            let expected = match class {
                Class::AdoptedUnrecorded => (None, None),
                _ => (Some(w.main_tip.clone()), Some(on_main())),
            };
            assert_eq!((recorded.base_commit, recorded.branch_origin), expected, "{class:?}");
            assert_eq!(recorded.branch_name, Some(WorktreeManager::branch_for_task(w.task_id)));
        }

        #[tokio::test]
        async fn a_created_checkout_is_taken_back_and_created_again_with_its_base() {
            refused_then_recorded_after_retry(Class::Created).await;
        }

        #[tokio::test]
        async fn a_worktree_added_for_a_recorded_branch_is_left_and_the_branch_untouched() {
            refused_then_recorded_after_retry(Class::AddedForRecorded).await;
        }

        #[tokio::test]
        async fn a_registered_worktree_of_the_recorded_branch_is_left_as_it_is() {
            refused_then_recorded_after_retry(Class::ReattachedRegistered).await;
        }

        /// An adopted checkout is never this command's to take back, and its
        /// start stays unknown rather than being invented.
        #[tokio::test]
        async fn an_adopted_unrecorded_checkout_is_left_and_claims_no_start() {
            refused_then_recorded_after_retry(Class::AdoptedUnrecorded).await;
        }

        /// A checkout that gained an untracked file while it was being
        /// created -- here from a `post-checkout` hook -- is not removed, and
        /// the refusal says what was kept.
        #[tokio::test]
        async fn a_created_checkout_holding_untracked_files_is_kept_and_named() {
            let w = world(Class::Created).await;
            let hook = w.repo.join(".git").join("hooks").join("post-checkout");
            std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
            std::fs::write(&hook, "#!/bin/sh\necho made-by-hook > hook-output.txt\n").unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let Some(unwritable) = crate::test_helpers::UnwritableDir::new(&w.tasks_dir()) else {
                eprintln!("skipped: the tasks directory stays writable (running as root?)");
                return;
            };

            let refused = create_worktree_inner(&w.state, w.task_id).await.expect_err("refused");
            drop(unwritable);

            let branch = WorktreeManager::branch_for_task(w.task_id);
            assert!(refused.contains("could not be recorded"), "{refused}");
            assert!(refused.contains("kept"), "the refusal names what was kept: {refused}");
            let checkout = git(&w.repo, &["worktree", "list", "--porcelain"]);
            assert!(checkout.contains(&format!("branch refs/heads/{branch}")), "{checkout}");
            assert_eq!(git(&w.repo, &["rev-parse", &format!("refs/heads/{branch}")]), w.main_tip);
            assert_eq!(provenance(&w.on_disk()), (None, None, None, None));
        }
    }
}
