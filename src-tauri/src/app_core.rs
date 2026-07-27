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
        // than once per task.
        let mut porcelain_cache: HashMap<String, String> = HashMap::new();

        for task in tasks.values_mut() {
            let Some(wt_path) = task.worktree_path.as_ref() else {
                continue;
            };
            if std::path::Path::new(wt_path).exists() {
                continue;
            }

            let adopted = task
                .branch_name
                .as_ref()
                .zip(repo_for_project.get(&task.project_id))
                .and_then(|(branch, repo)| {
                    let porcelain = porcelain_cache
                        .entry(repo.clone())
                        .or_insert_with(|| worktree::WorktreeManager::worktree_list_porcelain(repo));
                    app_state.worktree_manager.adopt_existing(repo, branch, porcelain)
                });

            match adopted {
                Some(path) => {
                    println!(
                        "SlashIt: Adopted relocated worktree for task '{}' at {path}",
                        task.title
                    );
                    task.worktree_path = Some(path);
                    report.adopted_worktrees += 1;
                }
                None => {
                    println!(
                        "SlashIt: Worktree dir missing for task '{}', clearing reference",
                        task.title
                    );
                    task.worktree_path = None;
                    report.cleared_worktrees += 1;
                }
            }
            migrated_projects.insert(task.project_id);
        }

        report.tasks = tasks.len();

        // Persist migrated tasks so the next start does not redo the work.
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
}
