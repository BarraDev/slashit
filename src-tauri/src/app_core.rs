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
use crate::{commands, config, worktree, AppState};

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
    let app_state = AppState {
        repository: commands::repository::RepositoryState::new(),
        project: commands::project::ProjectState::new(),
        workspace: commands::workspace::WorkspaceState::new()?,
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
        worktree_manager: Arc::new(worktree::WorktreeManager::new(
            paths.clone(),
            loaded_config.worktree.placement,
        )),
        // The resolved set, not the persisted one: what the application does
        // has to match what it reports.
        features: Arc::new(tokio::sync::RwLock::new(
            config::features::resolve_startup_flags(&paths),
        )),
        paths,
        executor: Arc::new(tokio::sync::OnceCell::new()),
        state_location_locks: Arc::new(commands::state_location::StateLocationLocks::new()),
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
                continue;
            };
            if std::path::Path::new(&wt_path).exists() {
                continue;
            }

            let recovery = match (
                task.branch_name.as_ref(),
                repo_for_project.get(&task.project_id),
            ) {
                (Some(branch), Some(repo)) => {
                    let porcelain = porcelain_cache
                        .entry(repo.clone())
                        .or_insert_with(|| worktree::WorktreeManager::worktree_list_porcelain(repo));
                    app_state
                        .worktree_manager
                        .classify_missing_worktree(repo, branch, porcelain.as_deref())
                }
                // No branch was ever recorded, so there is nothing to look a
                // worktree up by and nothing that could ever recreate it. No
                // amount of git working would change that answer, so this is
                // genuine absence rather than a failed check.
                (None, _) => worktree::WorktreeRecovery::ConfirmedAbsent,
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
                worktree::WorktreeRecovery::Adopt(path) => {
                    println!(
                        "SlashIt: Adopted relocated worktree for task '{}' at {path}",
                        task.title
                    );
                    task.worktree_path = Some(path);
                    report.adopted_worktrees += 1;
                }
                worktree::WorktreeRecovery::ConfirmedAbsent => {
                    println!(
                        "SlashIt: Worktree dir missing for task '{}', clearing reference",
                        task.title
                    );
                    task.worktree_path = None;
                    report.cleared_worktrees += 1;
                }
                // Git could not be consulted, so nothing here proves the
                // worktree is gone. `worktree_path` is the only persisted
                // record of it, and clearing it is not recoverable from here:
                // this loop skips a task that has none, and the executor's
                // cleanup-retry pass filters on one too, so no later start
                // would reconsider it. A transient git failure must not be
                // allowed to spend it. Left untouched, and deliberately not
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
        // managed/legacy conventions (e.g. one `wt` placed under its own
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
}
