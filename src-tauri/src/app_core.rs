//! Building the application state, once, for whichever front end wants it.
//!
//! This used to live inline in `lib.rs::run()`, interleaved with
//! `tauri::Builder`, and it hydrated state with `blocking_write()` /
//! `blocking_read()` on tokio locks. That worked only because `run()` executes
//! *before* the Tokio runtime starts. Calling the same code from inside a
//! `#[tokio::main]` daemon panics immediately — tokio refuses a blocking lock
//! acquisition on a runtime thread.
//!
//! So the hydration is async and lives here, and both entry points — the
//! desktop app and `slashitd` — call [`build_state`]. There is exactly one
//! implementation of "what is SlashIt's state at startup".

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::config::paths::AppPaths;
use crate::config::Storage;
use crate::domain::{self, Task};
use crate::pty::PtyState;
use crate::{commands, config, lifecycle, storage_accounting, worktree, AppState};

/// What hydration found and did, for the caller to log.
///
/// Returned rather than printed so a daemon can emit it as structured output
/// and the GUI can keep its existing human-readable lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupReport {
    pub repositories: usize,
    pub projects: usize,
    pub tasks: usize,
    /// Projects whose tasks were rewritten by a migration and saved back.
    pub migrated_projects: usize,
    /// Of `migrated_projects`, how many failed to persist to disk.
    ///
    /// The reconciled state still lands in the returned `AppState` even when
    /// this is nonzero: the migrations here (model normalization, orphaned
    /// task recovery, worktree adoption/clearing) are idempotent and
    /// re-derived from disk/filesystem/git state, not from any record of
    /// having run before. Continuing with the reconciled in-memory state is
    /// what makes the recovery it performs actually take effect this run;
    /// rolling it back would put a crashed task's status right back to
    /// "running" with nothing driving it. An ordinary save triggered by any
    /// later mutation of the same project will carry this state to disk, and
    /// a restart before that happens simply repeats the same reconciliation
    /// against the same stale file and reaches the same result — so nothing
    /// is lost, only possibly redone. This field exists so a caller's success
    /// message cannot claim disk durability that did not happen.
    pub unsaved_migrated_projects: usize,
    /// Worktrees found at a new location and re-linked rather than dropped.
    pub adopted_worktrees: usize,
    /// Worktrees that could not be found anywhere, whose reference was cleared.
    pub cleared_worktrees: usize,
    /// Tasks whose interrupted cleanup was shown to have finished, so the
    /// reference to the removed worktree could be dropped.
    pub reconciled_interrupted_cleanups: usize,
    /// Tasks held back from running because a cleanup was interrupted and the
    /// checkout it was removing, or git's registration of it, is still there.
    pub quarantined_worktrees: usize,
    /// Tasks whose checkout is gone while git still holds a locked
    /// registration of their branch. The reference is kept, and the task says
    /// what the user can do about it.
    pub locked_worktrees: usize,
}

/// Resolve the OS directories and build state from what is on disk.
pub async fn build_state() -> anyhow::Result<(AppState, StartupReport)> {
    let paths = Arc::new(AppPaths::new()?);
    build_state_with_paths(paths).await
}

/// Build state rooted at explicit directories.
///
/// Tests use this with a tempdir so hydration, migration and the daemon can be
/// exercised without touching the developer's real configuration.
pub async fn build_state_with_paths(
    paths: Arc<AppPaths>,
) -> anyhow::Result<(AppState, StartupReport)> {
    let storage = Storage::with_paths((*paths).clone());

    // Config must be read before the worktree manager is built: it carries the
    // worktree placement policy, and it populates Storage's project routing
    // table, which every later task load depends on.
    let loaded_config = storage.load_config().unwrap_or_else(|e| {
        eprintln!("Warning: Failed to load config from disk: {e}");
        config::storage::AppConfig::default()
    });

    let task_state = commands::task::TaskState::new();
    let queue_state = commands::queue::QueueState::new(task_state.tasks.clone());
    let project_state = commands::project::ProjectState::new();
    let repository_state = commands::repository::RepositoryState::new();
    let executor = Arc::new(tokio::sync::OnceCell::new());
    let storage_accounting = Arc::new(storage_accounting::for_app(
        paths.clone(),
        storage_accounting::Sources {
            tasks: task_state.tasks.clone(),
            projects: project_state.projects.clone(),
            repositories: repository_state.repositories.clone(),
            executor: executor.clone(),
        },
    ));
    // Unit tests build state under a tempdir on whatever disk the machine
    // has; see `test_helpers::plenty_of_disk`.
    #[cfg(test)]
    let start_guard = crate::test_helpers::plenty_of_disk();
    #[cfg(not(test))]
    let start_guard = Arc::new(crate::queue::start_guard::StartGuard::for_data_dir(
        paths.data_dir().to_path_buf(),
    ));
    let app_state = AppState {
        repository: repository_state,
        project: project_state,
        workspace: commands::workspace::WorkspaceState::load(&paths)?,
        task: task_state,
        agent: commands::agent::AgentState::new(),
        session: commands::session::SessionState::new(),
        jj: commands::jj::JjState::new(),
        queue: queue_state,
        roadmap: commands::roadmap::RoadmapState::new(),
        file: commands::file::FileState::new(),
        github: commands::github::GithubState::new(),
        changelog: commands::changelog::ChangelogState::new(),
        mcp: commands::mcp::McpState::new(),
        memory: commands::memory::MemoryState::new(),
        appearance: commands::appearance::AppearanceState::new(),
        updater: commands::updater::UpdaterState::new(),
        pty: PtyState::new(),
        storage,
        // Installed by the caller: a webview sink in the GUI, a logging sink
        // in the daemon.
        events: Arc::new(std::sync::OnceLock::new()),
        worktree_manager: Arc::new(worktree::WorktreeManager::new(paths.clone())),
        // The resolved set, not the persisted one: what the application does
        // has to match what it reports.
        features: Arc::new(tokio::sync::RwLock::new(
            config::features::resolve_startup_flags(&paths),
        )),
        paths,
        executor,
        state_location_locks: Arc::new(commands::state_location::StateLocationLocks::new()),
        task_lifecycle_locks: Arc::new(lifecycle::TaskLifecycleLocks::new()),
        storage_accounting,
        start_guard,
    };

    let mut report = StartupReport::default();

    // Repositories load FIRST, then projects (which reference repository_id),
    // then tasks (which reference project_id).
    {
        let mut repositories = app_state.repository.repositories.write().await;
        for (id_str, mut repository) in loaded_config.repositories {
            match uuid::Uuid::parse_str(&id_str) {
                Ok(id) => {
                    // The key is authoritative; a record whose own id drifted
                    // from it would be unreachable by lookup.
                    repository.id = id;
                    repositories.insert(id, repository);
                }
                Err(_) => eprintln!("Warning: Invalid repository UUID in config: {id_str}"),
            }
        }
        report.repositories = repositories.len();
    }

    {
        let mut projects = app_state.project.projects.write().await;
        for (id_str, mut project) in loaded_config.projects {
            match uuid::Uuid::parse_str(&id_str) {
                Ok(id) => {
                    project.id = id;
                    projects.insert(id, project);
                }
                Err(_) => eprintln!("Warning: Invalid project UUID in config: {id_str}"),
            }
        }
        report.projects = projects.len();
    }

    let loaded_tasks = app_state.storage.load_all_tasks().unwrap_or_else(|e| {
        eprintln!("Warning: Failed to load tasks from disk: {e}");
        Vec::new()
    });

    // Built before the task write lock is taken rather than inside it. The
    // original code held the tasks write guard while acquiring read guards on
    // projects and repositories, which imposed a lock ordering that any future
    // concurrent caller would have to know about. Nothing needs it: the map
    // depends only on projects and repositories.
    let repo_for_project = repository_paths_by_project(&app_state).await;
    // Tasks recording a branch another task in the same repository records
    // too. Neither is re-pointed at a checkout of it below; see
    // `worktree::tasks_sharing_a_recorded_branch`.
    let shared_branch_owner =
        worktree::tasks_sharing_a_recorded_branch(&loaded_tasks, &repo_for_project).await;

    let mut migrated_projects: HashSet<uuid::Uuid> = HashSet::new();
    {
        let mut tasks = app_state.task.tasks.write().await;
        for mut task in loaded_tasks {
            if migrate_task(&mut task) {
                migrated_projects.insert(task.project_id);
            }
            tasks.insert(task.id, task);
        }

        // Verify worktree paths still exist on disk.
        //
        // A path that no longer resolves is not automatically stale: upgrading
        // moves managed worktrees to a new root, so try to adopt the worktree
        // at its current location before discarding the reference. Clearing
        // first would strand the branch and any uncommitted work in it.
        // Cache `git worktree list --porcelain` per repository so tasks that
        // share a repo shell out to git at most once during this loop, rather
        // than once per task. A failed lookup is cached too: retrying per task
        // would not make git work, and every task in that repo must reach the
        // same "could not verify" conclusion anyway.
        let mut porcelain_cache: HashMap<String, Option<String>> = HashMap::new();

        for task in tasks.values_mut() {
            let Some(wt_path) = task.worktree_path.clone() else {
                // No checkout recorded, so an interrupted cleanup has nothing
                // left to be interrupted about: the flag is stale.
                if task.cleanup_in_flight {
                    task.cleanup_in_flight = false;
                    migrated_projects.insert(task.project_id);
                }
                continue;
            };

            // A cleanup that started and never recorded its outcome. The
            // recorded checkout is untrustworthy until something proves the
            // removal finished, and no other reconciliation below may run
            // against it -- adoption in particular would hand a half-removed
            // directory straight back to the executor. See
            // `Task::cleanup_in_flight`.
            if task.cleanup_in_flight {
                // Looked up by path and by branch, as a removal decides (see
                // `worktree::CheckoutState`): a registration whose `HEAD` is
                // detached is invisible by branch alone.
                let state = match repo_for_project.get(&task.project_id) {
                    Some(repo) => {
                        let porcelain = cached_porcelain(&mut porcelain_cache, repo).await;
                        // `None` here is git failing to answer, not git saying
                        // nothing is registered, so it stays quarantined.
                        worktree::CheckoutState::of(
                            porcelain.as_deref().ok_or(GIT_LIST_FAILED),
                            &wt_path,
                            task.branch_name.as_deref(),
                        )
                    }
                    // Nothing to look the registration up in. Absence cannot
                    // be established, so it is not assumed.
                    None => worktree::CheckoutState::of(
                        Err("the task's project resolves to no repository"),
                        &wt_path,
                        task.branch_name.as_deref(),
                    ),
                };

                // Removal deletes the checkout's contents and takes git's
                // registration down last, so a path and a registration that are
                // both confirmed gone is proof the removal ran to the end.
                // Anything else -- either still present, or git unable to say --
                // leaves the task quarantined. Deliberately non-destructive:
                // nothing here runs a git command, and a still-present checkout
                // is left exactly as it was found, because nothing on disk says
                // how far the interrupted removal got and re-running it
                // automatically is the retry loop this design removed.
                if !std::path::Path::new(&wt_path).exists() && state.nothing_registered() {
                    println!(
                        "SlashIt: worktree cleanup for task '{}' was interrupted but had \
                         finished; clearing the reference",
                        task.title
                    );
                    task.worktree_path = None;
                    task.cleanup_in_flight = false;
                    task.error_message = None;
                    report.reconciled_interrupted_cleanups += 1;
                } else {
                    eprintln!(
                        "Warning: worktree cleanup for task '{}' at {} was interrupted and its \
                         outcome is unknown; the task is held back from running until this is \
                         resolved",
                        task.title, wt_path
                    );
                    let mut message = format!(
                        "A cleanup of the worktree at {wt_path} was interrupted and never \
                         finished. The worktree and everything in it were left alone. This task \
                         will not run until the worktree is removed or the cleanup is asked for \
                         again."
                    );
                    // Asking again cannot succeed while git holds the
                    // checkout locked, so the task says so rather than let the
                    // retry be the first to find out.
                    if let Some(worktree::CheckoutRegistration::MissingLocked {
                        path,
                        branch,
                        reason,
                    }) = state.holding()
                    {
                        let repo = repo_for_project
                            .get(&task.project_id)
                            .map(String::as_str)
                            .unwrap_or_default();
                        message.push(' ');
                        message.push_str(&worktree::locked_registration_notice(
                            repo,
                            path,
                            branch.as_deref(),
                            reason.as_deref(),
                        ));
                    }
                    task.error_message = Some(message);
                    report.quarantined_worktrees += 1;
                }
                migrated_projects.insert(task.project_id);
                continue;
            }

            if std::path::Path::new(&wt_path).exists() {
                // Back where it was recorded, as when the drive it is on is
                // mounted again: a lock notice no longer says anything true.
                if drop_lifted_lock_notice(task) {
                    migrated_projects.insert(task.project_id);
                }
                continue;
            }

            let recovery = match (
                task.branch_name.as_deref(),
                repo_for_project.get(&task.project_id),
            ) {
                // Looked up by the recorded path as well as by branch, so a
                // task that records no branch is still checked against what
                // git holds at its path.
                (branch, Some(repo)) => {
                    let porcelain = cached_porcelain(&mut porcelain_cache, repo).await;
                    app_state.worktree_manager.classify_missing_worktree(
                        repo,
                        &wt_path,
                        branch,
                        porcelain.as_deref(),
                    )
                }
                // No branch was ever recorded and no repository resolves, so
                // there is nothing to look a worktree up by or in, and nothing
                // that could ever recreate it.
                (None, None) => worktree::WorktreeRecovery::ConfirmedAbsent,
                // A branch is recorded but the project resolves to no
                // repository. That is not proof the worktree is gone: this
                // table is rebuilt from config on every start, and a config
                // that fails to deserialize cleanly is recovered with no
                // projects and no repositories at all. Clearing here would
                // spend every task's reference on a lookup that never
                // happened.
                (Some(_), None) => worktree::WorktreeRecovery::Unverified,
            };

            match recovery {
                worktree::WorktreeRecovery::Adopt(path) if shared_branch_owner.contains_key(&task.id) => {
                    let other = shared_branch_owner[&task.id];
                    let branch = task.branch_name.as_deref().unwrap_or_default();
                    eprintln!(
                        "Warning: worktree of branch {branch} for task '{}' was found at {path}, \
                         but task {other} records the same branch; keeping the recorded reference",
                        task.title
                    );
                    // Kept, not cleared: the checkout exists, and clearing
                    // would spend the only record of it.
                    task.error_message = Some(format!(
                        "The worktree of branch {branch} is now at {path}, but task {other} records \
                         the same branch, so SlashIt cannot tell which task it belongs to and did \
                         not point this task at it. Delete whichever of the two tasks does not own \
                         that branch."
                    ));
                }
                worktree::WorktreeRecovery::Adopt(path) => {
                    println!(
                        "SlashIt: Adopted relocated worktree for task '{}' at {path}",
                        task.title
                    );
                    task.worktree_path = Some(path);
                    drop_lifted_lock_notice(task);
                    report.adopted_worktrees += 1;
                }
                worktree::WorktreeRecovery::ConfirmedAbsent => {
                    println!(
                        "SlashIt: Worktree dir missing for task '{}', clearing reference",
                        task.title
                    );
                    task.worktree_path = None;
                    drop_lifted_lock_notice(task);
                    report.cleared_worktrees += 1;
                }
                // Git keeps the branch checked out in a checkout that is gone,
                // and refuses to check it out anywhere else until the user
                // unlocks it. Clearing the reference would leave that with
                // nothing on the task pointing at it, so it is kept, and the
                // task says what only the user may do. Nothing is run against
                // git: unlocking is the user's decision. The next start after
                // they do reconciles the task like any other missing checkout.
                worktree::WorktreeRecovery::Locked { path, branch, reason } => {
                    let repo = repo_for_project
                        .get(&task.project_id)
                        .map(String::as_str)
                        .unwrap_or_default();
                    let notice = worktree::locked_registration_notice(
                        repo,
                        &path,
                        branch.as_deref(),
                        reason.as_deref(),
                    );
                    eprintln!("Warning: task '{}': {notice}", task.title);
                    report.locked_worktrees += 1;
                    if task.error_message.as_deref() == Some(notice.as_str()) {
                        continue;
                    }
                    task.error_message = Some(notice);
                }
                // Git could not be consulted, so nothing here proves the
                // worktree is gone. `worktree_path` is the only persisted
                // record of it, and clearing it is not recoverable from here:
                // this loop skips a task that has none, and nothing else
                // revisits a task whose only record of a worktree is gone, so
                // no later start would reconsider it. A transient git failure
                // must not be allowed to spend it. Left untouched, and deliberately not
                // counted as migrated: nothing changed, so there is nothing to
                // persist and the next start verifies again.
                worktree::WorktreeRecovery::Unverified => {
                    eprintln!(
                        "Warning: worktree dir missing for task '{}' at {}, but its absence could \
                         not be confirmed (git worktree list failed, or the project resolved to no \
                         repository); keeping the reference for the next start",
                        task.title, wt_path
                    );
                    continue;
                }
            }
            migrated_projects.insert(task.project_id);
        }

        report.tasks = tasks.len();

        // Persist migrated tasks so the next start does not redo the work.
        //
        // A save failure here is reported but does not stop the loop or fail
        // this function: the reconciled state for this one project stays
        // published in memory (see `unsaved_migrated_projects`), and an
        // unrelated project's save failing must not block every other
        // project from loading.
        for project_id in &migrated_projects {
            let project_tasks: Vec<Task> = tasks
                .values()
                .filter(|t| &t.project_id == project_id)
                .cloned()
                .collect();
            if let Err(e) = app_state
                .storage
                .save_project_tasks(*project_id, &project_tasks)
            {
                eprintln!(
                    "Warning: Failed to persist migrated tasks for project {project_id}: {e}"
                );
                report.unsaved_migrated_projects += 1;
            }
        }
    }
    report.migrated_projects = migrated_projects.len();

    Ok((app_state, report))
}

/// Drop a notice an earlier start, or a refused cleanup, left on `task`
/// about a locked registration of its checkout, once startup has found the
/// checkout again or found git holding nothing for it: it no longer says
/// anything true. Any other message is left as it is. Returns whether the
/// task changed.
fn drop_lifted_lock_notice(task: &mut Task) -> bool {
    if task
        .error_message
        .as_deref()
        .is_some_and(worktree::carries_locked_registration_notice)
    {
        task.error_message = None;
        return true;
    }
    false
}

/// Why a lookup in a listing `cached_porcelain` could not produce has no
/// answer.
const GIT_LIST_FAILED: &str = "`git worktree list` could not be run";

/// `git worktree list --porcelain` for `repo`, from `cache` if this loop has
/// already asked, off the runtime thread if it has not.
///
/// `WorktreeManager::worktree_list_porcelain` shells out synchronously, and
/// `build_state_with_paths` is on the Tokio runtime by the time it runs
/// (see the module doc comment on why this file exists at all), so a cache
/// miss blocked the current worker until git exited. Wrapping the one
/// blocking call site rather than the whole loop keeps every other line here
/// -- including the `tasks` write guard held across it -- exactly as it was.
async fn cached_porcelain(cache: &mut HashMap<String, Option<String>>, repo: &str) -> Option<String> {
    if !cache.contains_key(repo) {
        let repo_owned = repo.to_string();
        let listed = match tokio::task::spawn_blocking(move || {
            worktree::WorktreeManager::worktree_list_porcelain(&repo_owned)
        })
        .await
        {
            Ok(listed) => listed,
            // The blocking closure panicked, or the runtime is shutting down
            // mid-task. Neither proves the worktree registration is gone, so
            // this stays the same "git could not be consulted" case as any
            // other failed lookup below -- said explicitly rather than
            // quietly reusing `None`'s other meaning.
            Err(e) => {
                eprintln!("Warning: worktree listing for {repo} did not complete: {e}");
                None
            }
        };
        cache.insert(repo.to_string(), listed);
    }
    cache[repo].clone()
}

/// Map each project to the filesystem path of the repository backing it.
async fn repository_paths_by_project(state: &AppState) -> HashMap<uuid::Uuid, String> {
    let projects = state.project.projects.read().await;
    let repositories = state.repository.repositories.read().await;
    projects
        .iter()
        .filter_map(|(id, project)| {
            let repo = repositories.get(&project.repository_id?)?;
            Some((*id, repo.local_path.clone()))
        })
        .collect()
}

/// Bring one task up to date with the current model.
///
/// Returns whether anything changed, so only affected projects are rewritten.
fn migrate_task(task: &mut Task) -> bool {
    let mut changed = false;

    // Model names used to be pinned to a specific Claude release. They are now
    // resolved at run time, so an old pin would request a model that no longer
    // exists.
    if task.model.starts_with("claude-3") || task.model.starts_with("claude-2") {
        task.model = "default".to_string();
        changed = true;
    }

    // A task left mid-flight by a crash has no agent behind it any more.
    // Returning it to the queue is what makes a restart resume work rather
    // than stall on a task nothing is driving.
    if matches!(
        task.status,
        domain::TaskStatus::InProgress | domain::TaskStatus::AiReview
    ) {
        println!(
            "SlashIt: Resetting orphaned task '{}' from {:?} back to Queue",
            task.title, task.status
        );
        task.status = domain::TaskStatus::Queue;
        changed = true;
    }

    // A queued task showing a failure phase is inconsistent: the phase belongs
    // to a run that is no longer happening, and the board would render a stale
    // error against a task that is simply waiting.
    if matches!(
        task.status,
        domain::TaskStatus::Backlog | domain::TaskStatus::Queue
    ) && task.phase != domain::TaskPhase::Idle
    {
        task.phase = domain::TaskPhase::Idle;
        task.phase_progress = 0;
        task.overall_progress = 0;
        task.error_message = None;
        changed = true;
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_paths(tmp: &TempDir) -> Arc<AppPaths> {
        Arc::new(AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ))
    }

    fn task_with(status: domain::TaskStatus, phase: domain::TaskPhase, model: &str) -> Task {
        let mut task = crate::test_helpers::create_test_task("A task");
        task.status = status;
        task.phase = phase;
        task.model = model.to_string();
        task
    }

    /// The regression this whole module exists for: hydration must not use
    /// blocking lock acquisition, because that panics inside a runtime.
    #[tokio::test]
    async fn state_builds_inside_a_tokio_runtime() {
        let tmp = TempDir::new().unwrap();
        let (state, report) = build_state_with_paths(test_paths(&tmp))
            .await
            .expect("hydration must succeed on an empty config");

        assert_eq!(report.repositories, 0);
        assert_eq!(report.projects, 0);
        assert_eq!(report.tasks, 0);

        // Locks must be usable afterwards, i.e. nothing was left held.
        assert!(state.task.tasks.read().await.is_empty());
        assert!(state.project.projects.read().await.is_empty());
        assert!(state.repository.repositories.read().await.is_empty());
    }

    #[tokio::test]
    async fn a_migrated_project_that_fails_to_persist_still_publishes_the_recovered_state() {
        // Regression guard for the startup persistence-failure contract: a
        // project whose reconciled tasks cannot be written to disk must not
        // fail the whole build (an unrelated project's disk hiccup must not
        // block every project from opening), and the in-memory state must
        // still carry the recovered task, not the original broken one --
        // rolling it back would put a crashed task straight back into
        // "running" with no agent behind it, exactly what this migration
        // exists to fix. The report must say so, so a caller cannot print a
        // false "saved to disk" message.
        //
        // To force the save specifically (not the load) to fail, this uses a
        // real, documented seam: a routable project whose task file still
        // sits at the legacy path (`load_all_tasks`'s own doc comment notes
        // this happens until the owning project is saved once). The load
        // reads the real file at the legacy path; the migration's save
        // targets the *routed* path, which this test pre-occupies with a
        // directory so the final `fs::rename` fails deterministically -- no
        // chmod, no disk-full simulation, nothing timing-dependent.
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);

        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let repository = domain::Repository {
            id: uuid::Uuid::new_v4(),
            local_path: repo_root.to_string_lossy().to_string(),
            remote_url: None,
            remote_type: None,
            created_at: chrono::Utc::now(),
        };
        let project = domain::Project {
            id: uuid::Uuid::new_v4(),
            name: "test-project".to_string(),
            repository_id: Some(repository.id),
            scope: domain::ProjectScope::Standalone,
            state_location: config::paths::StateLocation::External,
            base: None,
            agent_type: domain::AgentType::ClaudeCode,
            agent_config: domain::AgentConfig {
                agent_type: domain::AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let project_id = project.id;

        let task = task_with(
            domain::TaskStatus::InProgress,
            domain::TaskPhase::Coding,
            "default",
        );
        let task = Task {
            project_id,
            ..task
        };
        let task_id = task.id;

        let seed_storage = Storage::with_paths((*paths).clone());
        let mut cfg = config::storage::AppConfig {
            projects: HashMap::new(),
            repositories: HashMap::new(),
            agent_configs: HashMap::new(),
            jj_config: Default::default(),
            worktree: Default::default(),
            ui_preferences: Default::default(),
        };
        cfg.repositories
            .insert(repository.id.to_string(), repository);
        cfg.projects.insert(project_id.to_string(), project);
        seed_storage.save_config(&cfg).expect("save config");

        // Seed the task at the *legacy* path directly, bypassing
        // `save_project_tasks` (which would write to the now-routable path
        // instead).
        let legacy_file = paths.config_dir().join("tasks").join(format!("{project_id}.toml"));
        std::fs::create_dir_all(legacy_file.parent().unwrap()).unwrap();
        let legacy_contents = toml::to_string_pretty(&config::storage::ProjectTasksFile {
            version: 1,
            tasks: vec![task.clone()],
        })
        .unwrap();
        std::fs::write(&legacy_file, legacy_contents).unwrap();

        // Occupy the routed path -- where the migration's save must write --
        // with a directory, so the load (which falls back to the legacy file
        // above, since nothing routed exists yet) succeeds but the later save
        // cannot rename its temp file onto it.
        let key = config::paths::ProjectKey::for_path(&repo_root).key;
        let routed_dir = paths.project_state_dir(
            &key,
            project_id,
            &repo_root,
            config::paths::StateLocation::External,
        );
        let routed_file = routed_dir.join("tasks.toml");
        std::fs::create_dir_all(&routed_file).unwrap();

        let (state, report) = build_state_with_paths(paths.clone())
            .await
            .expect("a persistence failure for one project must not fail the whole build");

        assert_eq!(report.migrated_projects, 1);
        assert_eq!(
            report.unsaved_migrated_projects, 1,
            "the forced rename failure must be counted, not swallowed"
        );

        let recovered = {
            let tasks = state.task.tasks.read().await;
            let published = tasks.get(&task_id).expect("task must still be published");
            assert_eq!(
                published.status,
                domain::TaskStatus::Queue,
                "the recovered state must still be published even though it could not be saved"
            );
            published.clone()
        };

        // Retry is deterministic and nothing was lost: once whatever blocked
        // the save is gone, the exact recovered state this build already
        // computed (standing in for the ordinary save that any later
        // mutation of this project would trigger) persists cleanly, and a
        // subsequent rebuild -- standing in for the next process start --
        // finds it already reconciled at the routed path and does not
        // migrate it again.
        std::fs::remove_dir(&routed_file).unwrap();
        seed_storage
            .save_project_tasks(project_id, &[recovered])
            .expect("save must succeed once the obstruction is gone");

        let (_, second_report) = build_state_with_paths(paths)
            .await
            .expect("rebuild must succeed once the obstruction is gone");
        assert_eq!(
            second_report.migrated_projects, 0,
            "already-reconciled data must not migrate again"
        );
        assert_eq!(second_report.unsaved_migrated_projects, 0);
    }

    #[tokio::test]
    async fn building_twice_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);

        let (_, first) = build_state_with_paths(paths.clone()).await.unwrap();
        let (_, second) = build_state_with_paths(paths).await.unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn an_orphaned_in_progress_task_returns_to_the_queue() {
        let mut task = task_with(
            domain::TaskStatus::InProgress,
            domain::TaskPhase::Coding,
            "default",
        );
        assert!(migrate_task(&mut task));
        assert_eq!(task.status, domain::TaskStatus::Queue);
        // Queue implies Idle, so the phase reset applies in the same pass.
        assert_eq!(task.phase, domain::TaskPhase::Idle);
    }

    #[test]
    fn an_orphaned_ai_review_task_returns_to_the_queue() {
        let mut task = task_with(
            domain::TaskStatus::AiReview,
            domain::TaskPhase::QaReview,
            "default",
        );
        assert!(migrate_task(&mut task));
        assert_eq!(task.status, domain::TaskStatus::Queue);
    }

    #[test]
    fn a_pinned_legacy_model_is_replaced_with_the_resolved_default() {
        for pinned in ["claude-3-opus-20240229", "claude-2.1"] {
            let mut task = task_with(domain::TaskStatus::Backlog, domain::TaskPhase::Idle, pinned);
            assert!(migrate_task(&mut task), "{pinned} should migrate");
            assert_eq!(task.model, "default");
        }
    }

    #[test]
    fn a_queued_task_does_not_keep_a_stale_failure_phase() {
        let mut task = task_with(
            domain::TaskStatus::Queue,
            domain::TaskPhase::Failed,
            "default",
        );
        task.phase_progress = 80;
        task.overall_progress = 40;
        task.error_message = Some("from a previous run".to_string());

        assert!(migrate_task(&mut task));
        assert_eq!(task.phase, domain::TaskPhase::Idle);
        assert_eq!(task.phase_progress, 0);
        assert_eq!(task.overall_progress, 0);
        assert!(task.error_message.is_none());
    }

    #[test]
    fn a_healthy_task_is_left_alone() {
        // Migration must be a no-op for current data, or every start would
        // rewrite every project's task file.
        let mut task = task_with(domain::TaskStatus::Done, domain::TaskPhase::Idle, "default");
        assert!(!migrate_task(&mut task));

        let mut running = task_with(
            domain::TaskStatus::HumanReview,
            domain::TaskPhase::QaReview,
            "default",
        );
        assert!(!migrate_task(&mut running));
    }

    #[test]
    fn migration_is_idempotent() {
        let mut task = task_with(
            domain::TaskStatus::InProgress,
            domain::TaskPhase::Coding,
            "claude-3-opus",
        );
        assert!(migrate_task(&mut task));
        assert!(
            !migrate_task(&mut task),
            "a second pass must find nothing to do"
        );
    }

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git must be installed to run this test");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    #[tokio::test]
    async fn startup_adopts_a_worktree_registered_at_a_non_conventional_path() {
        // Regression guard: a worktree git still has registered for a task's
        // branch, but at a path that matches neither of SlashIt's own
        // managed/legacy conventions (e.g. one Worktrunk placed under its own
        // naming scheme), must be adopted at startup rather than having its
        // reference cleared -- clearing would strand the worktree with no
        // way for the app to find it again.
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);

        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@example.com"]);
        run_git(&repo_dir, &["config", "user.name", "Test"]);
        std::fs::write(repo_dir.join("README.md"), "hello").unwrap();
        run_git(&repo_dir, &["add", "."]);
        run_git(&repo_dir, &["commit", "-q", "-m", "initial"]);

        let branch = "task-abcd1234";
        let unconventional_worktree = tmp.path().join("wherever-wt-put-it");
        run_git(
            &repo_dir,
            &[
                "worktree",
                "add",
                unconventional_worktree.to_str().unwrap(),
                "-b",
                branch,
            ],
        );

        let repository = domain::Repository {
            id: uuid::Uuid::new_v4(),
            local_path: repo_dir.to_string_lossy().to_string(),
            remote_url: None,
            remote_type: None,
            created_at: chrono::Utc::now(),
        };
        let project = domain::Project {
            id: uuid::Uuid::new_v4(),
            name: "test-project".to_string(),
            repository_id: Some(repository.id),
            scope: domain::ProjectScope::Standalone,
            state_location: config::paths::StateLocation::External,
            base: None,
            agent_type: domain::AgentType::ClaudeCode,
            agent_config: domain::AgentConfig {
                agent_type: domain::AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let mut task = crate::test_helpers::create_test_task("A task");
        task.project_id = project.id;
        task.status = domain::TaskStatus::InProgress;
        task.branch_name = Some(branch.to_string());
        // Recorded location no longer exists -- the trigger for adoption.
        task.worktree_path = Some(
            tmp.path()
                .join("stale-recorded-path")
                .to_string_lossy()
                .to_string(),
        );

        let storage = Storage::with_paths((*paths).clone());
        let mut cfg = config::storage::AppConfig {
            projects: HashMap::new(),
            repositories: HashMap::new(),
            agent_configs: HashMap::new(),
            jj_config: Default::default(),
            worktree: Default::default(),
            ui_preferences: Default::default(),
        };
        cfg.repositories
            .insert(repository.id.to_string(), repository);
        cfg.projects.insert(project.id.to_string(), project.clone());
        storage.save_config(&cfg).expect("save config");
        storage
            .save_project_tasks(project.id, &[task.clone()])
            .expect("save tasks");

        let (state, report) = build_state_with_paths(paths)
            .await
            .expect("hydration must succeed");

        assert_eq!(report.adopted_worktrees, 1, "expected one adoption");
        assert_eq!(report.cleared_worktrees, 0, "must not clear a live worktree");

        let tasks = state.task.tasks.read().await;
        let hydrated = tasks.get(&task.id).expect("task must still exist");
        assert_eq!(
            hydrated.worktree_path.as_deref(),
            Some(unconventional_worktree.to_string_lossy().as_ref()),
            "worktree_path must be repointed at the git-confirmed location, not cleared"
        );
    }

    /// Two tasks that both record one branch, whose recorded checkout moved,
    /// are neither re-pointed at the checkout git has registered for it:
    /// nothing says which of them it belongs to, and a re-pointed reference
    /// would let one task's cleanup or push act on the other's checkout.
    /// Both references are kept as they were, and both tasks say why.
    #[tokio::test]
    async fn startup_does_not_repoint_two_tasks_recording_one_branch_at_its_checkout() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@example.com"]);
        run_git(&repo_dir, &["config", "user.name", "Test"]);
        run_git(&repo_dir, &["commit", "-q", "--allow-empty", "-m", "initial"]);
        let branch = "task-12345678";
        let moved_to = tmp.path().join("wherever-it-moved");
        run_git(&repo_dir, &["worktree", "add", "-q", moved_to.to_str().unwrap(), "-b", branch]);

        let repository = domain::Repository {
            id: uuid::Uuid::new_v4(),
            local_path: repo_dir.to_string_lossy().to_string(),
            remote_url: None,
            remote_type: None,
            created_at: chrono::Utc::now(),
        };
        let project = domain::Project {
            id: uuid::Uuid::new_v4(),
            name: "test-project".to_string(),
            repository_id: Some(repository.id),
            scope: domain::ProjectScope::Standalone,
            state_location: config::paths::StateLocation::External,
            base: None,
            agent_type: domain::AgentType::ClaudeCode,
            agent_config: domain::AgentConfig {
                agent_type: domain::AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let stale = tmp.path().join("stale-recorded-path").to_string_lossy().to_string();
        let ids = [
            uuid::Uuid::from_u128(0x12345678_0000_4000_8000_000000000001),
            uuid::Uuid::from_u128(0x12345678_0000_4000_8000_000000000002),
        ];
        let tasks: Vec<Task> = ids
            .iter()
            .map(|id| {
                let mut task = crate::test_helpers::create_test_task("A task");
                task.id = *id;
                task.project_id = project.id;
                task.status = domain::TaskStatus::Done;
                task.branch_name = Some(branch.to_string());
                task.worktree_path = Some(stale.clone());
                task
            })
            .collect();

        let storage = Storage::with_paths((*paths).clone());
        let mut cfg = config::storage::AppConfig {
            projects: HashMap::new(),
            repositories: HashMap::new(),
            agent_configs: HashMap::new(),
            jj_config: Default::default(),
            worktree: Default::default(),
            ui_preferences: Default::default(),
        };
        cfg.repositories.insert(repository.id.to_string(), repository);
        cfg.projects.insert(project.id.to_string(), project.clone());
        storage.save_config(&cfg).expect("save config");
        storage.save_project_tasks(project.id, &tasks).expect("save tasks");

        let (state, report) = build_state_with_paths(paths).await.expect("hydration must succeed");

        assert_eq!(report.adopted_worktrees, 0, "neither task is given the moved checkout");
        assert_eq!(report.cleared_worktrees, 0, "neither reference is spent");
        let hydrated = state.task.tasks.read().await;
        for (id, other) in [(ids[0], ids[1]), (ids[1], ids[0])] {
            let task = &hydrated[&id];
            assert_eq!(task.worktree_path.as_deref(), Some(stale.as_str()));
            let why = task.error_message.as_deref().unwrap_or_default();
            assert!(why.contains(&other.to_string()), "{why}");
        }
        drop(hydrated);
        let persisted = storage.load_project_tasks(project.id).expect("load tasks");
        assert!(persisted.iter().all(|t| t.error_message.is_some()), "the reason survives a restart");
    }

    /// Two tasks whose ids share their first 8 hex digits, each recording
    /// the branch named from its whole id, are each re-pointed at the
    /// checkout of their own branch when both checkouts moved: the names no
    /// longer collide, so neither is held back on the other's account.
    #[tokio::test]
    async fn startup_repoints_tasks_sharing_a_prefix_at_their_own_checkouts() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        run_git(&repo_dir, &["init", "-q"]);
        run_git(&repo_dir, &["config", "user.email", "test@example.com"]);
        run_git(&repo_dir, &["config", "user.name", "Test"]);
        run_git(&repo_dir, &["commit", "-q", "--allow-empty", "-m", "initial"]);

        let repository = domain::Repository {
            id: uuid::Uuid::new_v4(),
            local_path: repo_dir.to_string_lossy().to_string(),
            remote_url: None,
            remote_type: None,
            created_at: chrono::Utc::now(),
        };
        let project = domain::Project {
            id: uuid::Uuid::new_v4(),
            name: "test-project".to_string(),
            repository_id: Some(repository.id),
            scope: domain::ProjectScope::Standalone,
            state_location: config::paths::StateLocation::External,
            base: None,
            agent_type: domain::AgentType::ClaudeCode,
            agent_config: domain::AgentConfig {
                agent_type: domain::AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let stale = tmp.path().join("stale-recorded-path").to_string_lossy().to_string();
        let ids = [
            uuid::Uuid::from_u128(0x12345678_0000_4000_8000_000000000001),
            uuid::Uuid::from_u128(0x12345678_0000_4000_8000_000000000002),
        ];
        let mut moved_to = HashMap::new();
        let tasks: Vec<Task> = ids
            .iter()
            .map(|id| {
                let branch = worktree::WorktreeManager::branch_for_task(*id);
                let moved = tmp.path().join(format!("moved-{id}"));
                run_git(&repo_dir, &["worktree", "add", "-q", moved.to_str().unwrap(), "-b", &branch]);
                moved_to.insert(*id, moved.to_string_lossy().to_string());
                let mut task = crate::test_helpers::create_test_task("A task");
                task.id = *id;
                task.project_id = project.id;
                task.status = domain::TaskStatus::InProgress;
                task.branch_name = Some(branch);
                task.worktree_path = Some(stale.clone());
                task
            })
            .collect();

        let storage = Storage::with_paths((*paths).clone());
        let mut cfg = config::storage::AppConfig::default();
        cfg.repositories.insert(repository.id.to_string(), repository);
        cfg.projects.insert(project.id.to_string(), project.clone());
        storage.save_config(&cfg).expect("save config");
        storage.save_project_tasks(project.id, &tasks).expect("save tasks");

        let (state, report) = build_state_with_paths(paths).await.expect("hydration must succeed");

        assert_eq!(report.adopted_worktrees, 2);
        let hydrated = state.task.tasks.read().await;
        for id in ids {
            assert_eq!(hydrated[&id].worktree_path.as_deref(), Some(moved_to[&id].as_str()));
            assert_eq!(hydrated[&id].error_message, None);
        }
    }

    /// A repository, a project and a task with an interrupted cleanup recorded
    /// against a real worktree, seeded on disk exactly as a crashed process
    /// would have left them.
    ///
    /// `keep_worktree` decides which of the two cases the startup has to tell
    /// apart, and it does so the only way that is honest: by actually removing
    /// the worktree with git, so the registration goes with it, rather than by
    /// deleting the directory and leaving git's bookkeeping behind.
    async fn interrupted_cleanup_fixture(
        tmp: &TempDir,
        keep_worktree: bool,
    ) -> (Arc<AppPaths>, uuid::Uuid, String) {
        let paths = test_paths(tmp);

        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo_root)
                .output()
                .expect("git must be spawnable");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(repo_root.join("README.md"), "base\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);

        let branch = "task-abcd1234";
        let wt_path = tmp.path().join("checkout").to_string_lossy().to_string();
        git(&["worktree", "add", "-q", &wt_path, "-b", branch]);
        if !keep_worktree {
            git(&["worktree", "remove", &wt_path]);
        }

        let repository = domain::Repository {
            id: uuid::Uuid::new_v4(),
            local_path: repo_root.to_string_lossy().to_string(),
            remote_url: None,
            remote_type: None,
            created_at: chrono::Utc::now(),
        };
        let project = domain::Project {
            id: uuid::Uuid::new_v4(),
            name: "interrupted".to_string(),
            repository_id: Some(repository.id),
            scope: domain::ProjectScope::Standalone,
            state_location: config::paths::StateLocation::External,
            base: None,
            agent_type: domain::AgentType::ClaudeCode,
            agent_config: domain::AgentConfig {
                agent_type: domain::AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        // `InProgress` with an idle phase, which is exactly what the queue
        // reads as "start this task", so the quarantine has something real to
        // hold back.
        let mut task = crate::test_helpers::create_test_task("interrupted mid-cleanup");
        task.project_id = project.id;
        task.status = domain::TaskStatus::InProgress;
        task.phase = domain::TaskPhase::Idle;
        task.branch_name = Some(branch.to_string());
        task.worktree_path = Some(wt_path.clone());
        task.cleanup_in_flight = true;
        let task_id = task.id;

        let storage = Storage::with_paths((*paths).clone());
        let mut cfg = config::storage::AppConfig {
            projects: HashMap::new(),
            repositories: HashMap::new(),
            agent_configs: HashMap::new(),
            jj_config: Default::default(),
            worktree: Default::default(),
            ui_preferences: Default::default(),
        };
        cfg.projects.insert(project.id.to_string(), project.clone());
        cfg.repositories.insert(repository.id.to_string(), repository);
        storage.save_config(&cfg).expect("save config");
        storage.save_project_tasks(project.id, &[task]).expect("save tasks");

        (paths, task_id, wt_path)
    }

    /// A cleanup that was interrupted while the checkout is still there leaves
    /// the task held back, and leaves the checkout completely alone.
    ///
    /// This is the case nothing else can decide. The recorded status, the
    /// recorded path, the directory and git's registration are all byte-for-byte
    /// what a healthy task's would be, and the only remaining difference --
    /// which files inside are already gone -- is unattributable, because an
    /// agent deleting its own files produces the same shape. So startup does
    /// not guess: it does not adopt, it does not re-run the removal, and it
    /// does not let the queue start the task.
    #[tokio::test]
    async fn an_interrupted_cleanup_whose_checkout_is_still_there_is_quarantined() {
        let tmp = TempDir::new().unwrap();
        let (paths, task_id, wt_path) = interrupted_cleanup_fixture(&tmp, true).await;

        let (state, report) = build_state_with_paths(paths)
            .await
            .expect("hydration must succeed");

        assert_eq!(report.quarantined_worktrees, 1);
        assert_eq!(report.reconciled_interrupted_cleanups, 0);
        assert_eq!(
            report.adopted_worktrees, 0,
            "a checkout a dead removal may have been halfway through must not be adopted"
        );

        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).expect("task must still exist");
        assert!(
            task.cleanup_in_flight,
            "the quarantine must survive the startup that observed it"
        );
        assert_eq!(
            task.worktree_path.as_deref(),
            Some(wt_path.as_str()),
            "the only record of the checkout must not be spent"
        );
        assert!(
            task.error_message.is_some(),
            "a state that needs a person has to say so where a person will see it"
        );
        assert!(
            std::path::Path::new(&wt_path).exists(),
            "startup must run nothing destructive"
        );
        assert_ne!(
            task.status,
            domain::TaskStatus::InProgress,
            "a quarantined task must not be left sitting in a column that claims an \
             agent is working on it"
        );
    }

    /// A cleanup that was interrupted after it had already finished is
    /// reconciled, and only the reference is dropped.
    ///
    /// Both the checkout and git's registration are gone, and the order git
    /// removes them in -- contents first, registration last -- is what makes
    /// their joint absence proof the removal reached its end. The task's own
    /// lifecycle status is left exactly where it was: this reconciliation
    /// learned that a directory is gone, which says nothing about whether the
    /// work is finished, so inventing `Done` here would be inventing a fact.
    /// A quarantine whose own persistence fails still leaves the durable state
    /// safe, which is the property that matters and the one that is easy to
    /// state wrongly.
    ///
    /// Startup reconciliation deliberately keeps its result in memory when a
    /// project's save fails -- see `StartupReport::unsaved_migrated_projects`,
    /// which predates this quarantine and exists so no caller can claim
    /// durability that did not happen. The question that leaves open is whether
    /// the *new* reconciliation can publish something the file contradicts in a
    /// direction that is unsafe. It cannot, and this pins why: the load-bearing
    /// field is `cleanup_in_flight`, the quarantine never clears it, and a
    /// failed save therefore leaves the file holding the more cautious value,
    /// not the less. What is lost is the human-readable reason, which the next
    /// start re-derives from the same filesystem and git state and reaches the
    /// same answer from -- so nothing is lost, only possibly redone.
    #[tokio::test]
    async fn a_quarantine_that_cannot_be_persisted_leaves_the_file_no_less_cautious() {
        let tmp = TempDir::new().unwrap();
        let (paths, task_id, wt_path) = interrupted_cleanup_fixture(&tmp, true).await;

        // Let the load succeed and make only the save fail, the same way
        // `a_migrated_project_that_fails_to_persist_still_publishes_the_recovered_state`
        // does: the seeded board is moved to the legacy path the loader falls
        // back to, and the routed path the save must write is occupied by a
        // directory so the atomic rename cannot land.
        let routed = find_routed_tasks_file(paths.data_dir()).expect("the fixture seeded a board");
        let project_id: uuid::Uuid = {
            let seeded: config::storage::ProjectTasksFile =
                toml::from_str(&std::fs::read_to_string(&routed).unwrap()).unwrap();
            seeded.tasks[0].project_id
        };
        let legacy = paths.config_dir().join("tasks").join(format!("{project_id}.toml"));
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::rename(&routed, &legacy).unwrap();
        std::fs::create_dir_all(&routed).unwrap();

        let (state, report) = build_state_with_paths(paths)
            .await
            .expect("one project that cannot be saved must not stop the process from starting");

        assert_eq!(report.quarantined_worktrees, 1);
        assert_eq!(
            report.unsaved_migrated_projects, 1,
            "the report has to say the reconciliation did not reach the disk"
        );

        let on_disk: config::storage::ProjectTasksFile =
            toml::from_str(&std::fs::read_to_string(&legacy).unwrap()).unwrap();
        let durable = on_disk.tasks.iter().find(|t| t.id == task_id).expect("still on disk");
        assert!(
            durable.cleanup_in_flight,
            "the file must still say a cleanup was interrupted, because that flag is what makes \
             the next start quarantine this task again rather than run it"
        );

        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).expect("task must still exist");
        assert!(
            task.cleanup_in_flight,
            "and memory must agree with it, so nothing the file contradicts is exposed as \
             runnable"
        );
        assert_eq!(
            task.worktree_path.as_deref(),
            Some(wt_path.as_str()),
            "the only reference to a checkout nothing touched must survive both"
        );
        assert!(
            std::path::Path::new(&wt_path).exists(),
            "and startup still runs nothing destructive"
        );
    }

    /// The `tasks.toml` a routed save writes, wherever the fixture put it.
    fn find_routed_tasks_file(root: &std::path::Path) -> Option<std::path::PathBuf> {
        for entry in std::fs::read_dir(root).ok()? {
            let path = entry.ok()?.path();
            if path.is_dir() {
                if let Some(found) = find_routed_tasks_file(&path) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|n| n == "tasks.toml") {
                return Some(path);
            }
        }
        None
    }

    #[tokio::test]
    async fn an_interrupted_cleanup_that_had_already_finished_is_reconciled() {
        let tmp = TempDir::new().unwrap();
        let (paths, task_id, wt_path) = interrupted_cleanup_fixture(&tmp, false).await;

        let (state, report) = build_state_with_paths(paths)
            .await
            .expect("hydration must succeed");

        assert_eq!(report.reconciled_interrupted_cleanups, 1);
        assert_eq!(report.quarantined_worktrees, 0);

        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).expect("task must still exist");
        assert!(!task.cleanup_in_flight, "the interrupted cleanup is resolved");
        assert_eq!(task.worktree_path, None, "and the checkout it named is gone");
        // The status this task ends on is decided by the pre-existing
        // crashed-run recovery a few lines above, which puts an `InProgress`
        // task nothing is driving back on the queue. What matters here is what
        // this reconciliation did *not* do: learning that a directory is gone
        // says nothing about whether the work is finished, so it invents no
        // terminal state.
        assert_ne!(
            task.status,
            domain::TaskStatus::Done,
            "a finished removal is not a finished task"
        );
        assert_eq!(task.status, domain::TaskStatus::Queue);
        assert!(!std::path::Path::new(&wt_path).exists());
    }

    #[tokio::test]
    async fn startup_keeps_a_worktree_reference_it_could_not_verify() {
        // Regression guard for issue #6, asserted at the loop level rather
        // than on `classify_missing_worktree` alone.
        //
        // This guarantee was originally written against the reconciliation
        // loop while it still lived in `lib.rs`; extracting the loop into
        // `build_state_with_paths` moved it out from under every test that
        // covered it, because all of the issue-#6 tests target
        // `WorktreeManager` methods directly. A resolution that collapsed
        // `Option<String>` back to a plain `String` here would restore the
        // exact defect and still pass the entire suite. This test is what
        // makes that impossible.
        //
        // The task records a branch, but its project resolves to no
        // repository -- so git is never consulted and absence is never
        // established. `worktree_path` is the only persisted record of the
        // worktree, and nothing later reconsiders a task that has none, so a
        // lookup that never happened must not be allowed to spend it.
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);

        let project = domain::Project {
            id: uuid::Uuid::new_v4(),
            name: "orphaned-project".to_string(),
            // No repository: exactly what a partially-recovered config yields.
            repository_id: None,
            scope: domain::ProjectScope::Standalone,
            state_location: config::paths::StateLocation::External,
            base: None,
            agent_type: domain::AgentType::ClaudeCode,
            agent_config: domain::AgentConfig {
                agent_type: domain::AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let recorded_path = tmp
            .path()
            .join("worktree-that-is-not-on-disk")
            .to_string_lossy()
            .to_string();

        let mut task = crate::test_helpers::create_test_task("A task mid-flight");
        task.project_id = project.id;
        task.status = domain::TaskStatus::InProgress;
        task.branch_name = Some("task-abcd1234".to_string());
        task.worktree_path = Some(recorded_path.clone());

        let storage = Storage::with_paths((*paths).clone());
        let mut cfg = config::storage::AppConfig {
            projects: HashMap::new(),
            repositories: HashMap::new(),
            agent_configs: HashMap::new(),
            jj_config: Default::default(),
            worktree: Default::default(),
            ui_preferences: Default::default(),
        };
        cfg.projects.insert(project.id.to_string(), project.clone());
        storage.save_config(&cfg).expect("save config");
        storage
            .save_project_tasks(project.id, &[task.clone()])
            .expect("save tasks");

        let (state, report) = build_state_with_paths(paths)
            .await
            .expect("hydration must succeed");

        assert_eq!(
            report.cleared_worktrees, 0,
            "an unverifiable absence is not proof of absence and must clear nothing"
        );
        assert_eq!(
            report.adopted_worktrees, 0,
            "nothing was adopted either -- git was never consulted"
        );

        let tasks = state.task.tasks.read().await;
        let hydrated = tasks.get(&task.id).expect("task must still exist");
        assert_eq!(
            hydrated.worktree_path.as_deref(),
            Some(recorded_path.as_str()),
            "worktree_path is the only record of the worktree; a lookup that never \
             happened must not be allowed to spend it"
        );
    }

    /// A task whose checkout SlashIt created at the managed path and whose
    /// directory has since vanished without git removing it, seeded on disk
    /// as the previous process left it. `lock` locks git's registration
    /// first, with that reason; `detach` detaches the checkout's `HEAD`
    /// first, as an agent's `git checkout --detach` or a stopped rebase
    /// leaves it; `in_flight` records a cleanup of it as interrupted.
    ///
    /// Returns the paths, the task, the repository and the recorded checkout.
    async fn vanished_checkout_fixture(
        tmp: &TempDir,
        lock: Option<&str>,
        detach: bool,
        in_flight: bool,
    ) -> (Arc<AppPaths>, uuid::Uuid, String, String) {
        let paths = test_paths(tmp);
        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        run_git(&repo_root, &["init", "-q", "-b", "main"]);
        run_git(&repo_root, &["config", "user.email", "test@example.com"]);
        run_git(&repo_root, &["config", "user.name", "Test"]);
        std::fs::write(repo_root.join("README.md"), "base\n").unwrap();
        run_git(&repo_root, &["add", "."]);
        run_git(&repo_root, &["commit", "-q", "-m", "init"]);
        let repo = repo_root.to_string_lossy().to_string();

        let branch = "task-abcd1234";
        run_git(&repo_root, &["branch", branch]);
        let checkout = worktree::WorktreeManager::new(paths.clone())
            .reattach(&repo, branch)
            .await
            .expect("a checkout at the managed path")
            .path;
        if detach {
            run_git(std::path::Path::new(&checkout), &["checkout", "-q", "--detach"]);
        }
        if let Some(reason) = lock {
            run_git(&repo_root, &["worktree", "lock", "--reason", reason, &checkout]);
        }
        std::fs::remove_dir_all(&checkout).unwrap();

        let repository = domain::Repository {
            id: uuid::Uuid::new_v4(),
            local_path: repo.clone(),
            remote_url: None,
            remote_type: None,
            created_at: chrono::Utc::now(),
        };
        let project = domain::Project {
            id: uuid::Uuid::new_v4(),
            name: "vanished".to_string(),
            repository_id: Some(repository.id),
            scope: domain::ProjectScope::Standalone,
            state_location: config::paths::StateLocation::External,
            base: None,
            agent_type: domain::AgentType::ClaudeCode,
            agent_config: domain::AgentConfig {
                agent_type: domain::AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let mut task = crate::test_helpers::create_test_task("checkout vanished");
        task.project_id = project.id;
        task.status = domain::TaskStatus::HumanReview;
        task.branch_name = Some(branch.to_string());
        task.worktree_path = Some(checkout.clone());
        task.cleanup_in_flight = in_flight;
        let task_id = task.id;

        let storage = Storage::with_paths((*paths).clone());
        let mut cfg = config::storage::AppConfig {
            projects: HashMap::new(),
            repositories: HashMap::new(),
            agent_configs: HashMap::new(),
            jj_config: Default::default(),
            worktree: Default::default(),
            ui_preferences: Default::default(),
        };
        let project_id = project.id;
        cfg.projects.insert(project_id.to_string(), project);
        cfg.repositories.insert(repository.id.to_string(), repository);
        storage.save_config(&cfg).expect("save config");
        storage.save_project_tasks(project_id, &[task]).expect("save tasks");

        (paths, task_id, repo, checkout)
    }

    /// A task whose checkout is missing while git still holds a locked
    /// registration of its branch keeps its reference across restarts, and
    /// says why and what to do. Clearing it would leave the branch checked
    /// out in a checkout nobody can see, with nothing on the task pointing
    /// at it. Once the user unlocks it, the next start reconciles the task
    /// and its checkout can be acquired again.
    #[tokio::test]
    async fn startup_keeps_a_missing_checkout_git_holds_locked_until_it_is_unlocked() {
        let tmp = TempDir::new().unwrap();
        let reason = "on a drive that is not mounted";
        let (paths, task_id, repo, checkout) = vanished_checkout_fixture(&tmp, Some(reason), false, false).await;

        for start in ["first", "second"] {
            let (state, report) = build_state_with_paths(paths.clone())
                .await
                .expect("hydration must succeed");
            assert_eq!(report.cleared_worktrees, 0, "{start} start clears nothing");
            let tasks = state.task.tasks.read().await;
            let task = tasks.get(&task_id).expect("task must still exist");
            assert_eq!(
                task.worktree_path.as_deref(),
                Some(checkout.as_str()),
                "the {start} start keeps the reference while git holds the branch locked"
            );
            let message = task.error_message.as_deref().unwrap_or_default();
            assert!(message.contains(reason), "the lock is named: {message}");
            assert!(
                message.contains("git worktree unlock"),
                "the way out is named: {message}"
            );
        }
        assert!(
            worktree::WorktreeManager::worktree_list_porcelain(&repo)
                .expect("git listing")
                .contains(reason),
            "startup runs nothing destructive: the lock is still there"
        );

        run_git(std::path::Path::new(&repo), &["worktree", "unlock", &checkout]);
        let (state, report) = build_state_with_paths(paths)
            .await
            .expect("hydration must succeed");
        assert_eq!(report.cleared_worktrees, 1);
        {
            let tasks = state.task.tasks.read().await;
            let task = tasks.get(&task_id).expect("task must still exist");
            assert_eq!(task.worktree_path, None, "once unlocked, the reference is reconciled");
            assert_eq!(task.error_message, None, "and the notice about the lock goes with it");
        }
        let again = state
            .worktree_manager
            .reattach(&repo, "task-abcd1234")
            .await
            .expect("the task's checkout can be acquired again");
        assert_eq!(again.path, checkout);
    }

    /// A task whose checkout vanished while git's registration of it is
    /// merely prunable has its reference cleared at startup, and that is only
    /// safe because acquiring the task's checkout again then succeeds.
    #[tokio::test]
    async fn startup_clears_a_prunable_missing_checkout_and_the_task_can_acquire_it_again() {
        let tmp = TempDir::new().unwrap();
        let (paths, task_id, repo, checkout) = vanished_checkout_fixture(&tmp, None, false, false).await;

        let (state, report) = build_state_with_paths(paths)
            .await
            .expect("hydration must succeed");
        assert_eq!(report.cleared_worktrees, 1);
        assert_eq!(
            state.task.tasks.read().await.get(&task_id).unwrap().worktree_path,
            None
        );

        let again = state
            .worktree_manager
            .reattach(&repo, "task-abcd1234")
            .await
            .expect("the task's checkout can be acquired again");
        assert_eq!(again.path, checkout);
        assert!(std::path::Path::new(&again.path).is_dir());
    }

    /// A locked checkout whose `HEAD` is detached holds no branch, so git's
    /// listing shows it only under its path. Startup looks it up by the
    /// recorded path too, and keeps the reference, exactly as a removal of
    /// it refuses. Once unlocked, the next start reconciles it and the
    /// task's checkout can be acquired again at the same path.
    #[tokio::test]
    async fn startup_keeps_a_detached_locked_checkout_it_cannot_find_by_branch() {
        let tmp = TempDir::new().unwrap();
        let reason = "on a drive that is not mounted";
        let (paths, task_id, repo, checkout) =
            vanished_checkout_fixture(&tmp, Some(reason), true, false).await;

        let (state, report) = build_state_with_paths(paths.clone())
            .await
            .expect("hydration must succeed");
        assert_eq!(report.cleared_worktrees, 0);
        {
            let tasks = state.task.tasks.read().await;
            let task = tasks.get(&task_id).expect("task must still exist");
            assert_eq!(task.worktree_path.as_deref(), Some(checkout.as_str()));
            let message = task.error_message.as_deref().unwrap_or_default();
            assert!(message.contains(reason), "the lock is named: {message}");
        }
        assert!(
            state
                .worktree_manager
                .remove(&checkout, &repo, Some("task-abcd1234"))
                .await
                .is_err(),
            "and removal agrees"
        );

        run_git(std::path::Path::new(&repo), &["worktree", "unlock", &checkout]);
        let (state, report) = build_state_with_paths(paths)
            .await
            .expect("hydration must succeed");
        assert_eq!(report.cleared_worktrees, 1);
        // The detached registration is now merely prunable, and acquisition
        // still leaves it for the user (see the next test); once they prune
        // it, the checkout is acquired again.
        assert!(state.worktree_manager.reattach(&repo, "task-abcd1234").await.is_err());
        run_git(std::path::Path::new(&repo), &["worktree", "prune"]);
        let again = state
            .worktree_manager
            .reattach(&repo, "task-abcd1234")
            .await
            .expect("the task's checkout can be acquired again");
        assert_eq!(again.path, checkout);
    }

    /// A checkout whose `HEAD` was detached and whose directory vanished
    /// leaves a prunable registration that names no branch. Startup clears the
    /// task's reference, as for any prunable registration, but acquiring the
    /// task's checkout afterwards does not clear the registration: its `HEAD`
    /// may be the only thing naming commits made there. It refuses, naming
    /// that commit, which is what then reaches the task.
    #[tokio::test]
    async fn a_detached_prunable_checkout_is_cleared_at_startup_but_never_pruned_by_acquisition() {
        let tmp = TempDir::new().unwrap();
        let (paths, task_id, repo, checkout) =
            vanished_checkout_fixture(&tmp, None, true, false).await;
        let head = worktree::WorktreeManager::worktree_list_porcelain(&repo)
            .expect("git listing")
            .split("\0\0")
            .find(|r| r.starts_with(&format!("worktree {checkout}\0")))
            .and_then(|r| r.split('\0').find_map(|f| f.strip_prefix("HEAD ")).map(str::to_string))
            .expect("the detached registration and its HEAD");

        let (state, report) = build_state_with_paths(paths)
            .await
            .expect("hydration must succeed");
        assert_eq!(report.cleared_worktrees, 1);
        assert_eq!(state.task.tasks.read().await[&task_id].worktree_path, None);

        let refused = state
            .worktree_manager
            .reattach(&repo, "task-abcd1234")
            .await
            .err()
            .expect("the detached registration is not pruned automatically");
        assert!(refused.contains(&head), "the detached commit is named: {refused}");
        assert!(
            worktree::WorktreeManager::worktree_list_porcelain(&repo)
                .expect("git listing")
                .contains(&format!("worktree {checkout}")),
            "and it is still registered"
        );
    }

    /// The notice a lock left on a task goes once the task's checkout is
    /// found again: whether the checkout reappears where it was recorded, as
    /// when the drive it is on is mounted, or the lock was lifted and the
    /// branch is found checked out somewhere else, and adopted there.
    #[tokio::test]
    async fn a_locked_registration_notice_goes_once_the_checkout_is_found_again() {
        let reason = "on a drive that is not mounted";
        let notice_of = |state: &AppState, task_id| {
            let state = state.task.tasks.clone();
            async move { state.read().await[&task_id].error_message.clone() }
        };

        // Mounted again: the recorded directory is back.
        let tmp = TempDir::new().unwrap();
        let (paths, task_id, _, checkout) =
            vanished_checkout_fixture(&tmp, Some(reason), false, false).await;
        let (state, _) = build_state_with_paths(paths.clone()).await.expect("hydration");
        assert!(notice_of(&state, task_id).await.is_some_and(|m| m.contains(reason)));
        drop(state);
        std::fs::create_dir_all(&checkout).unwrap();
        let (state, _) = build_state_with_paths(paths).await.expect("hydration");
        assert_eq!(notice_of(&state, task_id).await, None, "the checkout is back");

        // Unlocked, and the branch checked out somewhere else.
        let tmp = TempDir::new().unwrap();
        let (paths, task_id, repo, checkout) =
            vanished_checkout_fixture(&tmp, Some(reason), false, false).await;
        let (state, _) = build_state_with_paths(paths.clone()).await.expect("hydration");
        assert!(notice_of(&state, task_id).await.is_some_and(|m| m.contains(reason)));
        drop(state);
        let repo_dir = std::path::Path::new(&repo);
        run_git(repo_dir, &["worktree", "unlock", &checkout]);
        run_git(repo_dir, &["worktree", "prune"]);
        let elsewhere = tmp.path().join("elsewhere").to_string_lossy().to_string();
        run_git(repo_dir, &["worktree", "add", "-q", "--", &elsewhere, "task-abcd1234"]);
        let (state, report) = build_state_with_paths(paths).await.expect("hydration");
        assert_eq!(report.adopted_worktrees, 1);
        let tasks = state.task.tasks.read().await;
        assert_eq!(tasks[&task_id].worktree_path.as_deref(), Some(elsewhere.as_str()));
        assert_eq!(tasks[&task_id].error_message, None, "the lock is gone and the checkout found");
    }

    /// An interrupted cleanup of a checkout that is gone is reconciled only
    /// when git holds nothing for it by path or by branch. A locked
    /// registration whose `HEAD` is detached is invisible by branch, and
    /// keeps the task quarantined, saying which lock.
    #[tokio::test]
    async fn an_interrupted_cleanup_of_a_detached_locked_checkout_stays_quarantined() {
        let tmp = TempDir::new().unwrap();
        let reason = "on a drive that is not mounted";
        let (paths, task_id, _, checkout) =
            vanished_checkout_fixture(&tmp, Some(reason), true, true).await;

        let (state, report) = build_state_with_paths(paths)
            .await
            .expect("hydration must succeed");
        assert_eq!(report.reconciled_interrupted_cleanups, 0);
        assert_eq!(report.quarantined_worktrees, 1);
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).expect("task must still exist");
        assert!(task.cleanup_in_flight);
        assert_eq!(task.worktree_path.as_deref(), Some(checkout.as_str()));
        let message = task.error_message.as_deref().unwrap_or_default();
        assert!(message.contains(reason), "the lock is named: {message}");
    }

    /// A cleanup refused because of a locked registration leaves the reason
    /// on the task. Once the user unlocks and restarts, the reference is
    /// reconciled, and that reason goes with it rather than go on claiming a
    /// lock that is no longer there.
    #[tokio::test]
    async fn a_refused_cleanup_notice_goes_once_the_lock_is_lifted() {
        let tmp = TempDir::new().unwrap();
        let reason = "on a drive that is not mounted";
        let (paths, task_id, repo, checkout) =
            vanished_checkout_fixture(&tmp, Some(reason), false, false).await;

        let (state, _) = build_state_with_paths(paths.clone())
            .await
            .expect("hydration must succeed");
        let refusal = crate::lifecycle::terminalize(
            crate::commands::task::terminalize_ctx(&state),
            task_id,
            crate::lifecycle::Origin::User,
            crate::lifecycle::TerminalizeRequest::new(domain::TaskStatus::Done),
        )
        .await
        .expect_err("git holds the checkout locked");
        assert!(matches!(
            refusal,
            crate::lifecycle::TerminalizeRefusal::CleanupRefused { .. }
        ));
        let kept = state.task.tasks.read().await[&task_id].error_message.clone();
        assert!(
            kept.as_deref().unwrap_or_default().starts_with("The worktree at"),
            "the refused cleanup's own wording is on the task: {kept:?}"
        );
        drop(state);

        run_git(std::path::Path::new(&repo), &["worktree", "unlock", &checkout]);
        let (state, report) = build_state_with_paths(paths)
            .await
            .expect("hydration must succeed");
        assert_eq!(report.cleared_worktrees, 1);
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_id).expect("task must still exist");
        assert_eq!(task.worktree_path, None);
        assert_eq!(task.error_message, None, "the notice about the lifted lock is gone");
    }

    /// Proves the mechanism `cached_porcelain` relies on -- `tokio::task::
    /// spawn_blocking` around a synchronous call -- genuinely frees the
    /// runtime while that call is in flight, rather than merely reading as
    /// though it does.
    ///
    /// This does not shell out to a real `git`: an earlier version of this
    /// test used a fake `git` installed on `PATH` to slow down the real
    /// `worktree_list_porcelain` call, but `PATH` is a single process-global
    /// value and mutating it raced with every other test's own `git`/`jj`
    /// subprocess spawns, causing intermittent, unrelated test failures
    /// under the default parallel test runner (reproduced: two different
    /// `commands::repository::tests` failures across four `cargo test --lib`
    /// runs, neither reproducible in isolation). `cached_porcelain`'s own
    /// body is two lines -- `spawn_blocking(move || worktree_list_porcelain(...))
    /// .await` -- so which synchronous call sits inside `spawn_blocking` does
    /// not change whether the wrapping actually yields the runtime; a
    /// synthetic blocking closure proves the same thing this same way,
    /// without touching global process state.
    mod spawn_blocking_off_runtime_thread {
        use super::*;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::Duration;

        /// Counts how many times another task actually got to run while the
        /// blocking work was in flight. A current-thread runtime can only
        /// interleave this loop's `yield_now` points with other work if its
        /// one worker thread is free to schedule them -- a synchronous
        /// blocking call on that same thread starves it completely, so the
        /// final count is a direct measurement of scheduler interleaving, not
        /// a timing guess.
        fn spawn_progress_counter() -> (tokio::task::JoinHandle<()>, Arc<AtomicUsize>, Arc<AtomicBool>) {
            let counter = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let (counter_clone, stop_clone) = (counter.clone(), stop.clone());
            let handle = tokio::spawn(async move {
                while !stop_clone.load(Ordering::Relaxed) {
                    counter_clone.fetch_add(1, Ordering::Relaxed);
                    tokio::task::yield_now().await;
                }
            });
            (handle, counter, stop)
        }

        const BLOCKING_WORK: Duration = Duration::from_millis(50);

        #[tokio::test(flavor = "current_thread")]
        async fn spawn_blocking_lets_another_task_make_progress_while_it_runs() {
            let (handle, counter, stop) = spawn_progress_counter();

            // The exact shape `cached_porcelain` uses: a synchronous call
            // wrapped in `spawn_blocking` and awaited.
            tokio::task::spawn_blocking(|| std::thread::sleep(BLOCKING_WORK))
                .await
                .unwrap();

            stop.store(true, Ordering::Relaxed);
            handle.await.unwrap();

            assert!(
                counter.load(Ordering::Relaxed) > 0,
                "another task must be able to run while spawn_blocking's work is in \
                 flight -- a count of zero means the runtime's single worker thread was \
                 starved for the whole call"
            );
        }

        /// The RED case: the same blocking work called directly on the
        /// runtime thread, the way `cached_porcelain` called
        /// `worktree_list_porcelain` before this fix.
        #[tokio::test(flavor = "current_thread")]
        async fn calling_the_same_blocking_work_directly_starves_the_runtime() {
            let (handle, counter, stop) = spawn_progress_counter();

            std::thread::sleep(BLOCKING_WORK);

            stop.store(true, Ordering::Relaxed);
            handle.await.unwrap();

            assert_eq!(
                counter.load(Ordering::Relaxed),
                0,
                "a synchronous call made directly on a current-thread runtime must starve \
                 every other task -- this is the failure mode cached_porcelain exists to \
                 avoid, reproduced here to prove the progress-counter technique above \
                 actually distinguishes the two cases rather than always passing"
            );
        }
    }

    /// A Project whose Workspace the registry lost -- here because a corrupt
    /// `workspaces.toml` was quarantined -- still loads as a member of that
    /// Workspace. Startup must not guess a repair: the Workspaces page shows
    /// the dangling membership, and the user decides to detach it.
    #[tokio::test]
    async fn startup_keeps_a_membership_whose_workspace_the_registry_lost() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);
        let lost_workspace = uuid::Uuid::new_v4();
        let project = domain::Project {
            id: uuid::Uuid::new_v4(),
            name: "orphaned-member".to_string(),
            repository_id: None,
            scope: domain::ProjectScope::InWorkspace { workspace_id: lost_workspace },
            state_location: config::paths::StateLocation::External,
            base: None,
            agent_type: domain::AgentType::ClaudeCode,
            agent_config: domain::AgentConfig {
                agent_type: domain::AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let storage = Storage::with_paths((*paths).clone());
        let mut cfg = config::storage::AppConfig::default();
        cfg.projects.insert(project.id.to_string(), project.clone());
        storage.save_config(&cfg).expect("save config");
        let config_before = std::fs::read(paths.config_file()).unwrap();
        std::fs::write(paths.workspaces_file(), "this is [[not toml").unwrap();

        let (state, report) = build_state_with_paths(paths.clone())
            .await
            .expect("a lost registry must not fail startup");

        assert_eq!(report.projects, 1);
        // Quarantining is what proves startup read the registry at these
        // paths, rather than the one in the real OS config directory.
        assert!(
            !paths.workspaces_file().exists(),
            "the corrupt registry at these paths must have been quarantined"
        );
        let quarantined = std::fs::read_dir(paths.workspaces_file().parent().unwrap())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("workspaces.toml.corrupt-")
            })
            .count();
        assert_eq!(quarantined, 1, "the corrupt registry must be kept as a backup");
        assert!(
            state.workspace.registry.read().await.get(&lost_workspace).is_none(),
            "the registry must have started empty"
        );
        assert_eq!(
            state.project.projects.read().await[&project.id].scope.workspace_id(),
            Some(lost_workspace),
            "startup must not rewrite a membership it cannot resolve"
        );
        assert_eq!(
            std::fs::read(paths.config_file()).unwrap(),
            config_before,
            "startup must not persist a rewritten membership either"
        );
    }
}
