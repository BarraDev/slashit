use crate::agents::runner::{ClaudeRunner, ClaudeRunConfig, ClaudeEvent};
use crate::domain::{Task, TaskStatus, TaskPhase, AgentExecution, AgentStatus, AgentLogEntry, LogLevel, QaSignoff, QaStatus};
use crate::queue::prompt::{build_task_prompt, build_review_prompt, build_fix_prompt};
use crate::queue::QueueManager;
use crate::worktree::{WorktreeManager, WorktreeInfo};
use std::collections::HashMap;
use std::sync::Arc;
use crate::events::{EventSink, SharedEventSink};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Event emitted to the frontend via Tauri events.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum AgentEvent {
    #[serde(rename = "log")]
    Log { task_id: String, level: LogLevel, message: String },
    #[serde(rename = "phase_change")]
    PhaseChange { task_id: String, phase: TaskPhase, progress: u8 },
    #[serde(rename = "tool_use")]
    ToolUse { task_id: String, tool: String },
    #[serde(rename = "completed")]
    Completed { task_id: String, success: bool, message: Option<String> },
    #[serde(rename = "error")]
    Error { task_id: String, message: String },
}

type Tasks = Arc<RwLock<HashMap<Uuid, Task>>>;

/// Emit an `AgentEvent` through any sink.
///
/// The executor produces exactly one event name, so the conversion lives here
/// rather than forcing every call site to name it.
trait AgentEmit {
    fn agent_event(&self, event: AgentEvent);
}

impl<T: EventSink + ?Sized> AgentEmit for T {
    fn agent_event(&self, event: AgentEvent) {
        match serde_json::to_value(&event) {
            Ok(value) => self.emit_json("agent-event", value),
            Err(e) => eprintln!("[executor] dropping agent event: {e}"),
        }
    }
}

pub struct TaskExecutor {
    tasks: Tasks,
    queue_manager: Arc<RwLock<QueueManager>>,
    executions: Arc<RwLock<HashMap<Uuid, AgentExecution>>>,
    running_handles: Arc<RwLock<HashMap<Uuid, JoinHandle<()>>>>,
    reviewing_handles: Arc<RwLock<HashMap<Uuid, JoinHandle<()>>>>,
    /// Task ids with a worktree-cleanup attempt currently in flight, so a
    /// merged-PR transition and the periodic retry pass can never schedule
    /// two removals for the same task at once. See
    /// [`spawn_worktree_cleanup`](Self::spawn_worktree_cleanup).
    cleanup_in_flight: Arc<RwLock<std::collections::HashSet<Uuid>>>,
    /// The last cleanup warning already reported for a task, so the ~30s retry
    /// pass reports a persistent failure once rather than on every sweep.
    ///
    /// Retrying is deliberately unchanged — the reference must stay recorded
    /// until removal is confirmed — but the conditions that keep failing are
    /// steady states (a project that resolves to no repository, a directory
    /// git will not remove, a task file that cannot be written), so repeating
    /// the same sentence to the user twice a minute forever is noise, not
    /// information. Keyed by message so a *different* failure still surfaces,
    /// and cleared on success, so a condition that recurs is reported again.
    /// In-memory only: a restart is exactly when the underlying state may have
    /// changed, so re-reporting once then is correct.
    cleanup_last_warning: Arc<RwLock<HashMap<Uuid, String>>>,
    logs: Arc<RwLock<HashMap<Uuid, Vec<AgentLogEntry>>>>,
    projects: Arc<RwLock<HashMap<Uuid, crate::domain::Project>>>,
    repositories: Arc<RwLock<HashMap<Uuid, crate::domain::Repository>>>,
    workspace_registry: Arc<RwLock<crate::config::WorkspaceRegistry>>,
    storage: crate::config::Storage,
    worktree_manager: Arc<WorktreeManager>,
    events: SharedEventSink,
    pr_check_counter: std::sync::atomic::AtomicU32,
}

pub struct TaskExecutorConfig {
    pub tasks: Tasks,
    pub queue_manager: Arc<RwLock<QueueManager>>,
    pub executions: Arc<RwLock<HashMap<Uuid, AgentExecution>>>,
    pub logs: Arc<RwLock<HashMap<Uuid, Vec<AgentLogEntry>>>>,
    pub projects: Arc<RwLock<HashMap<Uuid, crate::domain::Project>>>,
    pub repositories: Arc<RwLock<HashMap<Uuid, crate::domain::Repository>>>,
    pub workspace_registry: Arc<RwLock<crate::config::WorkspaceRegistry>>,
    pub storage: crate::config::Storage,
    pub worktree_manager: Arc<WorktreeManager>,
    pub events: SharedEventSink,
}

impl TaskExecutor {
    pub fn new(config: TaskExecutorConfig) -> Self {
        Self {
            tasks: config.tasks,
            queue_manager: config.queue_manager,
            executions: config.executions,
            running_handles: Arc::new(RwLock::new(HashMap::new())),
            reviewing_handles: Arc::new(RwLock::new(HashMap::new())),
            cleanup_in_flight: Arc::new(RwLock::new(std::collections::HashSet::new())),
            cleanup_last_warning: Arc::new(RwLock::new(HashMap::new())),
            logs: config.logs,
            projects: config.projects,
            repositories: config.repositories,
            workspace_registry: config.workspace_registry,
            storage: config.storage,
            worktree_manager: config.worktree_manager,
            events: config.events,
            pr_check_counter: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Start the polling loop.
    pub fn start_polling(self: &Arc<Self>) {
        let executor = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                executor.check_and_execute().await;
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }
        });
    }

    /// How many tasks are executing or under review right now.
    ///
    /// A shutting-down daemon uses this to wait for real work rather than
    /// killing agents mid-run: an aborted task is left marked `in_progress`
    /// with no process behind it, and although startup requeues those, the
    /// work the agent had already done is discarded.
    pub async fn running_task_count(&self) -> usize {
        self.running_handles.read().await.len() + self.reviewing_handles.read().await.len()
    }

    async fn check_and_execute(&self) {
        // Auto-promote tasks from Queue → InProgress when capacity is available
        let manager = self.queue_manager.read().await;
        if manager.config().auto_promote {
            while let Some(task_id) = manager.promote_next_task().await {
                self.events.agent_event(AgentEvent::Log {
                    task_id: task_id.to_string(),
                    level: LogLevel::Info,
                    message: "Auto-promoted from queue".to_string(),
                });
            }
        }
        drop(manager);

        // Find InProgress tasks that haven't started execution yet
        let pending: Vec<Uuid> = {
            let tasks = self.tasks.read().await;
            tasks.values()
                .filter(|t| t.status == TaskStatus::InProgress && t.phase == TaskPhase::Idle)
                .map(|t| t.id)
                .collect()
        };

        let running = self.running_handles.read().await.len();
        let limit = {
            let mgr = self.queue_manager.read().await;
            mgr.config().parallel_task_limit as usize
        };
        let available = limit.saturating_sub(running);

        for task_id in pending.into_iter().take(available) {
            self.spawn_task_execution(task_id).await;
        }

        // Find AiReview tasks that need automated review
        let review_pending: Vec<Uuid> = {
            let tasks = self.tasks.read().await;
            let reviewing = self.reviewing_handles.read().await;
            tasks.values()
                .filter(|t| t.status == TaskStatus::AiReview && t.phase == TaskPhase::QaReview)
                .filter(|t| !reviewing.contains_key(&t.id))
                .map(|t| t.id)
                .collect()
        };

        for task_id in review_pending {
            self.spawn_review(task_id).await;
        }

        // Poll PR status every ~30s (10 cycles at 3s each)
        let counter = self.pr_check_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if counter.is_multiple_of(10) {
            use crate::domain::task::ExternalRef;

            // Find tasks with GithubPr refs that haven't been merged yet
            let pr_tasks: Vec<(Uuid, u32, String)> = {
                let tasks = self.tasks.read().await;
                tasks.values()
                    .filter(|t| matches!(t.status, TaskStatus::PrCreated | TaskStatus::HumanReview | TaskStatus::Done))
                    .flat_map(|t| {
                        t.external_refs.iter().filter_map(move |r| {
                            if let ExternalRef::GithubPr { number, repo, state, .. } = r {
                                // Only poll if not already in a terminal state
                                if state.as_deref() != Some("MERGED") && state.as_deref() != Some("CLOSED") {
                                    return Some((t.id, *number, repo.clone()));
                                }
                            }
                            None
                        })
                    })
                    .collect()
            };

            for (task_id, number, repo_slug) in pr_tasks {
                if let Ok(output) = tokio::process::Command::new("gh")
                    .args(["pr", "view", &number.to_string(), "--repo", &repo_slug, "--json", "state"])
                    .output()
                    .await
                {
                    if output.status.success() {
                        let json_str = String::from_utf8_lossy(&output.stdout);
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) {
                            let state = json.get("state").and_then(|s| s.as_str()).unwrap_or("");

                            // Update the ExternalRef state
                            let mut pending_worktree_removal: Option<(String, String)> = None;
                            {
                                let mut tasks_w = self.tasks.write().await;
                                if let Some(t) = tasks_w.get_mut(&task_id) {
                                    for r in &mut t.external_refs {
                                        if let ExternalRef::GithubPr { number: n, state: ref mut s, .. } = r {
                                            if *n == number {
                                                *s = Some(state.to_string());
                                            }
                                        }
                                    }

                                    match state {
                                        "MERGED" => {
                                            t.status = TaskStatus::Done;
                                            t.overall_progress = 100;
                                            t.phase = TaskPhase::Complete;
                                            t.updated_at = chrono::Utc::now();
                                            // Not cleared here: `worktree_path` is the only
                                            // persisted record of this directory. It is
                                            // cleared (and re-persisted) only once removal
                                            // below actually succeeds, so a crash or a
                                            // failed removal leaves it in place for retry.
                                            if let Some(wt_path) = t.worktree_path.clone() {
                                                let branch = t.branch_name.clone().unwrap_or_default();
                                                pending_worktree_removal = Some((wt_path, branch));
                                            }
                                        }
                                        "CLOSED" => {
                                            t.error_message = Some("PR was closed without merge".to_string());
                                        }
                                        _ => {} // OPEN — update state only
                                    }
                                }
                            }

                            // Spawned after `tasks_w` is dropped above, since
                            // `spawn_worktree_cleanup` takes its own locks.
                            if let Some((wt_path, branch)) = pending_worktree_removal {
                                self.spawn_worktree_cleanup(task_id, wt_path, branch).await;
                            }

                            if state == "MERGED" {
                                Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                                self.events.agent_event(AgentEvent::Completed {
                                    task_id: task_id.to_string(),
                                    success: true,
                                    message: Some("PR merged — task complete".to_string()),
                                });
                            } else if state == "CLOSED" || !state.is_empty() {
                                Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                            }
                        }
                    }
                }
            }

            // Retry worktree cleanup for tasks whose earlier attempt failed.
            //
            // The loop above only polls a `GithubPr` ref while it is not yet
            // in a terminal state, so once a task reaches `Done` its ref is
            // excluded from every future `gh pr view` here — correctly, that
            // call must not run again for an already-merged PR. But it also
            // means the merged-transition cleanup spawned above is the last
            // time that code path ever looks at the task, so a failed
            // removal would otherwise sit on `worktree_path` forever with no
            // automatic path back. This pass, running on the same ~30s
            // cadence, is that path: any `Done` task still holding a
            // `worktree_path` gets another attempt, guarded so a task
            // already being retried is never scheduled twice.
            let cleanup_retries = {
                let tasks = self.tasks.read().await;
                let in_flight = self.cleanup_in_flight.read().await;
                Self::tasks_eligible_for_cleanup_retry(&tasks, &in_flight)
            };
            for (task_id, wt_path, branch) in cleanup_retries {
                self.spawn_worktree_cleanup(task_id, wt_path, branch).await;
            }
        }
    }

    /// Tasks whose worktree cleanup should be (re)tried on this maintenance
    /// pass: already `Done` and still holding a `worktree_path` because a
    /// previous removal attempt failed, and not already being retried by an
    /// in-flight attempt.
    fn tasks_eligible_for_cleanup_retry(
        tasks: &HashMap<Uuid, Task>,
        in_flight: &std::collections::HashSet<Uuid>,
    ) -> Vec<(Uuid, String, String)> {
        tasks
            .values()
            .filter(|t| t.status == TaskStatus::Done)
            .filter(|t| !in_flight.contains(&t.id))
            .filter_map(|t| {
                let wt_path = t.worktree_path.clone()?;
                Some((t.id, wt_path, t.branch_name.clone().unwrap_or_default()))
            })
            .collect()
    }

    /// Reserve `task_id`'s cleanup slot, or refuse if one is already held.
    ///
    /// A `HashSet` rather than tracking `JoinHandle`s: nothing needs to
    /// cancel a cleanup attempt, only prevent two of them running for the
    /// same task at once, and `HashSet::insert`'s own return value makes the
    /// check-and-reserve atomic under a single write lock.
    async fn try_reserve_cleanup(
        in_flight: &RwLock<std::collections::HashSet<Uuid>>,
        task_id: Uuid,
    ) -> bool {
        in_flight.write().await.insert(task_id)
    }

    async fn release_cleanup(in_flight: &RwLock<std::collections::HashSet<Uuid>>, task_id: Uuid) {
        in_flight.write().await.remove(&task_id);
    }

    /// Attempt the actual removal and, only once the cleared record is on
    /// disk, drop `worktree_path` from shared memory.
    ///
    /// `Ok(())` therefore means both that the worktree is gone and that the
    /// task board says so. A persistence failure is reported as a failed
    /// cleanup, which is what keeps the task eligible for the retry pass.
    ///
    /// Takes no `AppHandle`, so it is unit-testable without a running Tauri
    /// app; [`spawn_worktree_cleanup`](Self::spawn_worktree_cleanup) wraps it
    /// with the in-flight guard and user-visible failure logging.
    async fn attempt_worktree_cleanup(
        wt_mgr: &WorktreeManager,
        tasks: &Tasks,
        storage: &crate::config::Storage,
        task_id: Uuid,
        wt_path: &str,
        branch: &str,
        repo_path: &str,
    ) -> Result<(), String> {
        wt_mgr.remove(wt_path, branch, repo_path).await?;

        // Persist before publishing. Shared with the status-change and delete
        // cleanup path in `commands::task`, which had the identical defect:
        // both used to clear `worktree_path` in memory and then persist with
        // the error discarded.
        crate::commands::task::clear_worktree_path_durably(tasks, storage, task_id, wt_path).await
    }

    /// Remove `task_id`'s worktree in the background, guarded so a second
    /// call for the same task while one is already running is a no-op.
    ///
    /// Used both right after a PR is observed merged and by the periodic
    /// retry pass in [`check_and_execute`](Self::check_and_execute) for a
    /// task whose earlier attempt failed — `worktree_path` stays set on
    /// failure, so the next maintenance pass finds it eligible again.
    async fn spawn_worktree_cleanup(&self, task_id: Uuid, wt_path: String, branch: String) {
        if !Self::try_reserve_cleanup(&self.cleanup_in_flight, task_id).await {
            return; // already retrying this task's worktree
        }

        let repo_path = match Self::resolve_working_dir_for_task(
            &self.tasks,
            &self.projects,
            &self.repositories,
            task_id,
        )
        .await
        {
            Ok(path) => path,
            Err(e) => {
                Self::warn_cleanup_once(
                    &self.app_handle,
                    &self.cleanup_last_warning,
                    task_id,
                    format!("Cannot resolve repo path to clean up worktree: {}", e),
                )
                .await;
                Self::release_cleanup(&self.cleanup_in_flight, task_id).await;
                return;
            }
        };

        let wt_mgr = self.worktree_manager.clone();
        let tasks = self.tasks.clone();
        let storage = self.storage.clone();
        let app_handle = self.app_handle.clone();
        let in_flight = self.cleanup_in_flight.clone();
        let last_warning = self.cleanup_last_warning.clone();
        let wt_path_done = wt_path.clone();

        tokio::spawn(async move {
            match Self::attempt_worktree_cleanup(
                &wt_mgr, &tasks, &storage, task_id, &wt_path, &branch, &repo_path,
            )
            .await
            {
                Err(e) => {
                    Self::warn_cleanup_once(
                        &app_handle,
                        &last_warning,
                        task_id,
                        format!(
                            "Failed to remove worktree {}: {}. It stays recorded on the task and will be retried automatically on a later maintenance pass.",
                            wt_path_done, e
                        ),
                    )
                    .await;
                }
                Ok(()) => {
                    // Cleanup converged, so a failure that recurs later is news
                    // again rather than a repeat.
                    last_warning.write().await.remove(&task_id);
                }
            }
            Self::release_cleanup(&in_flight, task_id).await;
        });
    }

    /// Emit a cleanup warning unless the identical one was already reported for
    /// this task and nothing has succeeded since.
    ///
    /// The retry cadence is unaffected: this suppresses only the repeated
    /// event, never an attempt. A different message always gets through, so a
    /// failure that changes character is still visible.
    async fn warn_cleanup_once(
        app_handle: &tauri::AppHandle,
        last_warning: &RwLock<HashMap<Uuid, String>>,
        task_id: Uuid,
        message: String,
    ) {
        if !Self::record_cleanup_warning(last_warning, task_id, &message).await {
            return;
        }

        let _ = app_handle.emit(
            "agent-event",
            AgentEvent::Log {
                task_id: task_id.to_string(),
                level: LogLevel::Warn,
                message,
            },
        );
    }

    /// Record `message` as this task's latest cleanup warning, returning
    /// whether it is new and therefore worth reporting.
    ///
    /// Split out from [`warn_cleanup_once`](Self::warn_cleanup_once) so the
    /// suppression rule is testable without a running Tauri app.
    async fn record_cleanup_warning(
        last_warning: &RwLock<HashMap<Uuid, String>>,
        task_id: Uuid,
        message: &str,
    ) -> bool {
        let mut seen = last_warning.write().await;
        if seen.get(&task_id).is_some_and(|previous| previous == message) {
            return false;
        }
        seen.insert(task_id, message.to_string());
        true
    }

    async fn spawn_task_execution(&self, task_id: Uuid) {
        // Resolve repo path from task → project → repository chain
        let repo_path = match Self::resolve_working_dir_for_task(
            &self.tasks, &self.projects, &self.repositories, task_id,
        ).await {
            Ok(dir) => dir,
            Err(e) => {
                self.events.agent_event(AgentEvent::Error {
                    task_id: task_id.to_string(),
                    message: format!("Cannot resolve working directory: {}", e),
                });
                Self::set_task_error_static(&self.tasks, &self.storage, task_id, &e).await;
                return;
            }
        };

        // Check if task already has a branch (re-queue after completion)
        let existing_branch = {
            let tasks_r = self.tasks.read().await;
            tasks_r.get(&task_id).and_then(|t| t.branch_name.clone())
        };

        let branch_name = existing_branch
            .clone()
            .unwrap_or_else(|| WorktreeManager::branch_for_task(task_id));

        // Check dependencies for stacked branching (only for new branches).
        // git-spice manages the stack — we just decide when to use it:
        // Stack when the dependency has a branch that hasn't been merged to main yet.
        // If merged, base on main normally (the code is already there).
        let base_branch = if existing_branch.is_none() {
            let tasks_r = self.tasks.read().await;
            let deps = tasks_r.get(&task_id)
                .map(|t| t.dependencies.clone())
                .unwrap_or_default();
            if let Some(dep_id) = deps.first() {
                tasks_r.get(dep_id).and_then(|dep_task| {
                    // Dependency must have a branch
                    let branch = dep_task.branch_name.as_ref()?;
                    // If dependency is Done and has no active PR, code is in main
                    let is_done = dep_task.status == TaskStatus::Done;
                    let has_pr = dep_task.external_refs.iter().any(|r| r.is_pr());
                    if is_done && !has_pr {
                        return None; // merged via main, no stack needed
                    }
                    Some(branch.clone())
                })
            } else {
                None
            }
        } else {
            None
        };

        // Helper closure to handle worktree success
        let handle_worktree_ok = |info: &WorktreeInfo, app: &SharedEventSink, msg: &str| {
            app.agent_event(AgentEvent::Log {
                task_id: task_id.to_string(),
                level: LogLevel::Info,
                message: format!("{}: {}", msg, info.path),
            });
        };

        // Create or reattach to worktree for this task
        let (working_dir, _worktree_path) = if let Some(ref _existing) = existing_branch {
            // Reattach to existing branch
            match self.worktree_manager.reattach(&repo_path, &branch_name).await {
                Ok(info) => {
                    handle_worktree_ok(&info, &self.events, "Reattached worktree");
                    {
                        let mut tasks_w = self.tasks.write().await;
                        if let Some(t) = tasks_w.get_mut(&task_id) {
                            t.worktree_path = Some(info.path.clone());
                            t.branch_name = Some(info.branch.clone());
                        }
                    }
                    Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                    (info.path.clone(), Some(info.path))
                }
                Err(e) => {
                    self.events.agent_event(AgentEvent::Log {
                        task_id: task_id.to_string(),
                        level: LogLevel::Warn,
                        message: format!("Worktree reattach failed ({}), using repo dir", e),
                    });
                    (repo_path.clone(), None)
                }
            }
        } else if let Some(ref parent_branch) = base_branch {
            // Stacked branch based on parent dependency
            match self.worktree_manager.create_stacked_branch(&repo_path, &branch_name, parent_branch).await {
                Ok(info) => {
                    handle_worktree_ok(&info, &self.events, "Created stacked worktree");
                    {
                        let mut tasks_w = self.tasks.write().await;
                        if let Some(t) = tasks_w.get_mut(&task_id) {
                            t.worktree_path = Some(info.path.clone());
                            t.branch_name = Some(info.branch.clone());
                        }
                    }
                    Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                    (info.path.clone(), Some(info.path))
                }
                Err(e) => {
                    // Fallback to normal create if stacking fails
                    self.events.agent_event(AgentEvent::Log {
                        task_id: task_id.to_string(),
                        level: LogLevel::Warn,
                        message: format!("Stacked branch failed ({}), falling back to normal create", e),
                    });
                    match self.worktree_manager.create(&repo_path, &branch_name).await {
                        Ok(info) => {
                            handle_worktree_ok(&info, &self.events, "Created worktree (fallback)");
                            {
                                let mut tasks_w = self.tasks.write().await;
                                if let Some(t) = tasks_w.get_mut(&task_id) {
                                    t.worktree_path = Some(info.path.clone());
                                    t.branch_name = Some(info.branch.clone());
                                }
                            }
                            Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                            (info.path.clone(), Some(info.path))
                        }
                        Err(e2) => {
                            self.events.agent_event(AgentEvent::Log {
                                task_id: task_id.to_string(),
                                level: LogLevel::Warn,
                                message: format!("Worktree creation failed ({}), using repo dir", e2),
                            });
                            (repo_path.clone(), None)
                        }
                    }
                }
            }
        } else {
            // Normal new branch
            match self.worktree_manager.create(&repo_path, &branch_name).await {
                Ok(info) => {
                    handle_worktree_ok(&info, &self.events, "Created worktree");
                    {
                        let mut tasks_w = self.tasks.write().await;
                        if let Some(t) = tasks_w.get_mut(&task_id) {
                            t.worktree_path = Some(info.path.clone());
                            t.branch_name = Some(info.branch.clone());
                        }
                    }
                    Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                    (info.path.clone(), Some(info.path))
                }
                Err(e) => {
                    self.events.agent_event(AgentEvent::Log {
                        task_id: task_id.to_string(),
                        level: LogLevel::Warn,
                        message: format!("Worktree creation failed ({}), using repo dir", e),
                    });
                    (repo_path.clone(), None)
                }
            }
        };

        let (prompt, task_model) = {
            let tasks = self.tasks.read().await;
            match tasks.get(&task_id) {
                Some(t) => {
                    let model = if t.model.is_empty() || t.model == "default" {
                        None
                    } else {
                        Some(t.model.clone())
                    };
                    (build_task_prompt(t, Some(working_dir.as_str())), model)
                },
                None => return,
            }
        };

        // Update phase
        self.update_task_phase(task_id, TaskPhase::Coding, 5).await;

        // If the project is attached to a workspace, run Claude from the
        // workspace root and expose the task's working dir via --add-dir so
        // the agent inherits the workspace's instruction files.
        let (claude_cwd, claude_add_dirs) =
            Self::resolve_workspace_launch(&self.tasks, &self.projects, &self.workspace_registry, task_id, &working_dir).await;

        let tasks = self.tasks.clone();
        let executions = self.executions.clone();
        let running_handles = self.running_handles.clone();
        let logs = self.logs.clone();
        let events = self.events.clone();
        let storage = self.storage.clone();
        let working_dir_for_commit = working_dir.clone();

        let handle = tokio::spawn(async move {
            let execution_id = Uuid::new_v4();
            let now = chrono::Utc::now();

            // Create execution record
            let execution = AgentExecution {
                id: execution_id,
                worktree_id: None,
                task_id: Some(task_id),
                agent_type: "claude-code".to_string(),
                status: AgentStatus::Starting,
                started_at: now,
                stopped_at: None,
            };
            executions.write().await.insert(execution_id, execution);
            logs.write().await.insert(execution_id, Vec::new());

            events.agent_event(AgentEvent::Log {
                task_id: task_id.to_string(),
                level: LogLevel::Info,
                message: format!("Starting Claude agent in {}", working_dir),
            });

            // Start Claude runner
            let runner = match ClaudeRunner::start(ClaudeRunConfig {
                prompt,
                working_dir: claude_cwd.clone(),
                allowed_tools: vec![
                    "Read".to_string(), "Edit".to_string(), "Write".to_string(),
                    "Bash".to_string(), "Glob".to_string(), "Grep".to_string(),
                ],
                max_turns: Some(50),
                max_budget_usd: None,
                session_id: Some(Uuid::new_v4().to_string()),
                resume_session: None,
                model: task_model,
                system_prompt: None,
                permission_mode: None, // defaults to --dangerously-skip-permissions
                disable_mcp: false,
                additional_dirs: claude_add_dirs.clone(),
            }).await {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("Failed to start claude: {}", e);
                    events.agent_event(AgentEvent::Error {
                        task_id: task_id.to_string(),
                        message: msg.clone(),
                    });
                    Self::set_task_error_static(&tasks, &storage, task_id, &msg).await;
                    return;
                }
            };

            // Update status
            if let Some(exec) = executions.write().await.get_mut(&execution_id) {
                exec.status = AgentStatus::Running;
            }

            events.agent_event(AgentEvent::PhaseChange {
                task_id: task_id.to_string(),
                phase: TaskPhase::Coding,
                progress: 10,
            });

            // Subscribe to events and forward to frontend
            let mut event_rx = runner.subscribe();
            let events_stream = events.clone();
            let task_id_str = task_id.to_string();
            let logs_events = logs.clone();
            let execution_id_events = execution_id;
            let tasks_for_events = tasks.clone();

            tokio::spawn(async move {
                while let Ok(event) = event_rx.recv().await {
                    match &event {
                        ClaudeEvent::TextDelta { text } => {
                            events_stream.agent_event(AgentEvent::Log {
                                task_id: task_id_str.clone(),
                                level: LogLevel::Info,
                                message: text.clone(),
                            });
                        }
                        ClaudeEvent::ToolUse { tool, .. } => {
                            events_stream.agent_event(AgentEvent::ToolUse {
                                task_id: task_id_str.clone(),
                                tool: tool.clone(),
                            });
                            let entry = AgentLogEntry {
                                timestamp: chrono::Utc::now(),
                                level: LogLevel::Info,
                                message: format!("Using tool: {}", tool),
                            };
                            logs_events.write().await.entry(execution_id_events)
                                .or_insert_with(Vec::new)
                                .push(entry);
                        }
                        ClaudeEvent::SystemInit { session_id, model, .. } => {
                            // Capture actual model on the task
                            if let Some(m) = model {
                                let mut tasks_w = tasks_for_events.write().await;
                                if let Some(t) = tasks_w.get_mut(&task_id) {
                                    t.model = m.clone();
                                }
                            }
                            let entry = AgentLogEntry {
                                timestamp: chrono::Utc::now(),
                                level: LogLevel::Info,
                                message: format!("Session started: {} (model: {})",
                                    session_id,
                                    model.as_deref().unwrap_or("unknown")),
                            };
                            logs_events.write().await.entry(execution_id_events)
                                .or_insert_with(Vec::new)
                                .push(entry);
                        }
                        ClaudeEvent::Error { message } => {
                            events_stream.agent_event(AgentEvent::Error {
                                task_id: task_id_str.clone(),
                                message: message.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            });

            // Wait for completion
            match runner.wait().await {
                Ok(_) => {
                    // Commit changes in the worktree/working dir
                    Self::commit_changes(&tasks, task_id, &working_dir_for_commit, &events).await;

                    // Move to AiReview for automated review before human review
                    Self::update_task_phase_static(&tasks, task_id, TaskPhase::QaReview, 80).await;
                    {
                        let mut tasks_w = tasks.write().await;
                        if let Some(t) = tasks_w.get_mut(&task_id) {
                            t.status = TaskStatus::AiReview;
                            t.overall_progress = 80;
                            t.updated_at = chrono::Utc::now();
                        }
                    }
                    events.agent_event(AgentEvent::Completed {
                        task_id: task_id.to_string(),
                        success: true,
                        message: Some("Agent completed — moving to AI review".to_string()),
                    });
                    Self::persist_task_static(&tasks, &storage, task_id).await;
                }
                Err(err_msg) => {
                    // Include accumulated stdout if stderr was empty
                    let full_msg = if err_msg.contains("no details") {
                        let stdout_output = runner.get_output().await;
                        let last_lines: String = stdout_output.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
                        if last_lines.is_empty() {
                            err_msg.clone()
                        } else {
                            format!("{} — {}", err_msg, last_lines)
                        }
                    } else {
                        err_msg.clone()
                    };
                    events.agent_event(AgentEvent::Error {
                        task_id: task_id.to_string(),
                        message: full_msg.clone(),
                    });
                    Self::set_task_error_static(&tasks, &storage, task_id, &full_msg).await;
                }
            }

            // Cleanup
            let _ = runner.kill().await;
            running_handles.write().await.remove(&task_id);

            if let Some(exec) = executions.write().await.get_mut(&execution_id) {
                exec.status = AgentStatus::Stopped;
                exec.stopped_at = Some(chrono::Utc::now());
            }
        });

        self.running_handles.write().await.insert(task_id, handle);
    }

    pub async fn execute_task(&self, task_id: Uuid) -> Result<(), String> {
        {
            let tasks = self.tasks.read().await;
            let task = tasks.get(&task_id).ok_or("Task not found")?;
            if task.status != TaskStatus::InProgress && task.status != TaskStatus::Queue {
                return Err(format!("Task in {:?}, expected InProgress or Queue", task.status));
            }
        }
        {
            let mut tasks = self.tasks.write().await;
            if let Some(t) = tasks.get_mut(&task_id) {
                t.status = TaskStatus::InProgress;
                t.phase = TaskPhase::Idle;
                t.updated_at = chrono::Utc::now();
            }
        }
        self.spawn_task_execution(task_id).await;
        Ok(())
    }

    pub async fn stop_task(&self, task_id: Uuid) -> Result<(), String> {
        if let Some(handle) = self.running_handles.write().await.remove(&task_id) {
            handle.abort();
        }
        {
            let mut tasks = self.tasks.write().await;
            if let Some(t) = tasks.get_mut(&task_id) {
                t.phase = TaskPhase::Idle;
                t.phase_progress = 0;
                t.updated_at = chrono::Utc::now();
            }
        }
        Ok(())
    }

    async fn spawn_review(&self, task_id: Uuid) {
        // Use worktree path if available, otherwise repo path
        let worktree_path = {
            let tasks_r = self.tasks.read().await;
            tasks_r.get(&task_id).and_then(|t| t.worktree_path.clone())
        };
        let working_dir = if let Some(wt) = worktree_path {
            wt
        } else {
            match Self::resolve_working_dir_for_task(
                &self.tasks, &self.projects, &self.repositories, task_id,
            ).await {
                Ok(dir) => dir,
                Err(e) => {
                    self.events.agent_event(AgentEvent::Log {
                        task_id: task_id.to_string(),
                        level: LogLevel::Warn,
                        message: format!("Cannot resolve working dir for review: {}", e),
                    });
                    let signoff = QaSignoff {
                        status: QaStatus::Rejected,
                        issues_found: vec![format!("AI review skipped: {}", e)],
                        timestamp: chrono::Utc::now(),
                        session_id: Uuid::new_v4(),
                    };
                    Self::transition_to_human_review(&self.tasks, &self.storage, task_id, Some(signoff)).await;
                    return;
                }
            }
        };

        let tasks = self.tasks.clone();
        let reviewing_handles = self.reviewing_handles.clone();
        let events = self.events.clone();
        let storage = self.storage.clone();
        let queue_manager = self.queue_manager.clone();

        // Mark phase as actively reviewing
        Self::update_task_phase_static(&tasks, task_id, TaskPhase::QaReview, 85).await;

        let handle = tokio::spawn(async move {
            let task_id_str = task_id.to_string();

            events.agent_event(AgentEvent::Log {
                task_id: task_id_str.clone(),
                level: LogLevel::Info,
                message: "Starting AI review...".to_string(),
            });

            // Get diff — try jj first, fallback to git
            let diff = match Self::get_diff(&working_dir).await {
                Some(d) => d,
                None => {
                    events.agent_event(AgentEvent::Log {
                        task_id: task_id_str.clone(),
                        level: LogLevel::Warn,
                        message: "Could not get diff (jj/git), skipping AI review".to_string(),
                    });
                    let signoff = QaSignoff {
                        status: QaStatus::Rejected,
                        issues_found: vec!["AI review skipped: diff failed".to_string()],
                        timestamp: chrono::Utc::now(),
                        session_id: Uuid::new_v4(),
                    };
                    Self::transition_to_human_review(&tasks, &storage, task_id, Some(signoff)).await;
                    reviewing_handles.write().await.remove(&task_id);
                    return;
                }
            };

            if diff.trim().is_empty() {
                events.agent_event(AgentEvent::Log {
                    task_id: task_id_str.clone(),
                    level: LogLevel::Info,
                    message: "No changes detected, skipping review".to_string(),
                });
                Self::transition_to_human_review(&tasks, &storage, task_id, None).await;
                reviewing_handles.write().await.remove(&task_id);
                return;
            }

            // Launch Claude review + CodeRabbit in parallel
            let review_prompt = {
                let tasks_r = tasks.read().await;
                match tasks_r.get(&task_id) {
                    Some(t) => build_review_prompt(t, &diff),
                    None => {
                        reviewing_handles.write().await.remove(&task_id);
                        return;
                    }
                }
            };

            // A) Claude review
            let claude_review = async {
                let runner = ClaudeRunner::start(ClaudeRunConfig {
                    prompt: review_prompt,
                    working_dir: working_dir.clone(),
                    allowed_tools: vec![
                        "Read".to_string(), "Glob".to_string(), "Grep".to_string(),
                    ],
                    max_turns: Some(10),
                    max_budget_usd: None,
                    session_id: Some(Uuid::new_v4().to_string()),
                    resume_session: None,
                    model: None,
                    system_prompt: None,
                    permission_mode: None,
                    disable_mcp: false,
            additional_dirs: Vec::new(),
                }).await;

                match runner {
                    Ok(r) => {
                        let _success = r.wait().await.unwrap_or(false);
                        let output = r.get_output().await;
                        let _ = r.kill().await;
                        output
                    }
                    Err(e) => format!("Claude review error: {}", e),
                }
            };

            // B) CodeRabbit review (if available)
            let use_coderabbit = {
                let mgr = queue_manager.read().await;
                mgr.config().use_coderabbit
            };

            let coderabbit_review = async {
                if !use_coderabbit {
                    return String::new();
                }

                // Check if coderabbit binary exists
                match tokio::process::Command::new("which")
                    .arg("coderabbit")
                    .output()
                    .await
                {
                    Ok(output) if output.status.success() => {}
                    _ => {
                        events.agent_event(AgentEvent::Log {
                            task_id: task_id_str.clone(),
                            level: LogLevel::Warn,
                            message: "CodeRabbit enabled but CLI not found. Install it or disable in Queue Settings.".to_string(),
                        });
                        return String::new();
                    }
                }

                events.agent_event(AgentEvent::Log {
                    task_id: task_id_str.clone(),
                    level: LogLevel::Info,
                    message: "Running CodeRabbit review...".to_string(),
                });

                match tokio::process::Command::new("coderabbit")
                    .args([
                        "review",
                        "--prompt-only",
                        "--type", "uncommitted",
                        "--cwd", &working_dir,
                        "--no-color",
                    ])
                    .output()
                    .await
                {
                    Ok(output) if output.status.success() => {
                        String::from_utf8_lossy(&output.stdout).to_string()
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        format!("CodeRabbit warning: {}", stderr)
                    }
                    Err(e) => {
                        format!("CodeRabbit error: {}", e)
                    }
                }
            };

            // Run both reviews in parallel
            let (claude_result, coderabbit_result) = tokio::join!(claude_review, coderabbit_review);

            // Merge findings
            let has_claude_issues = claude_result.contains("CHANGES_REQUESTED");
            let has_coderabbit_issues = !coderabbit_result.is_empty()
                && !coderabbit_result.starts_with("CodeRabbit error:")
                && !coderabbit_result.starts_with("CodeRabbit warning:");

            if has_claude_issues || has_coderabbit_issues {
                events.agent_event(AgentEvent::Log {
                    task_id: task_id_str.clone(),
                    level: LogLevel::Info,
                    message: "Issues found — validating and fixing...".to_string(),
                });

                // Build combined findings
                let mut findings = String::new();
                if has_claude_issues {
                    findings.push_str("### Claude Code Review\n");
                    findings.push_str(&claude_result);
                    findings.push('\n');
                }
                if has_coderabbit_issues {
                    findings.push_str("### CodeRabbit Review\n");
                    findings.push_str(&coderabbit_result);
                    findings.push('\n');
                }

                // Spawn fix agent that validates before fixing
                let fix_prompt = {
                    let tasks_r = tasks.read().await;
                    match tasks_r.get(&task_id) {
                        Some(t) => build_fix_prompt(t, &findings),
                        None => {
                            reviewing_handles.write().await.remove(&task_id);
                            return;
                        }
                    }
                };

                let fix_result = match ClaudeRunner::start(ClaudeRunConfig {
                    prompt: fix_prompt,
                    working_dir: working_dir.clone(),
                    allowed_tools: vec![
                        "Read".to_string(), "Edit".to_string(), "Write".to_string(),
                        "Glob".to_string(), "Grep".to_string(),
                    ],
                    max_turns: Some(20),
                    max_budget_usd: None,
                    session_id: Some(Uuid::new_v4().to_string()),
                    resume_session: None,
                    model: None,
                    system_prompt: None,
                    permission_mode: None,
                    disable_mcp: false,
            additional_dirs: Vec::new(),
                }).await {
                    Ok(r) => {
                        let _success = r.wait().await.unwrap_or(false);
                        let _ = r.kill().await;
                        // Re-describe in jj after fixes
                        let _ = tokio::process::Command::new("jj")
                            .args(["describe", "-m", &format!("task: {} (with review fixes)", {
                                let tasks_r = tasks.read().await;
                                tasks_r.get(&task_id).map(|t| t.title.clone()).unwrap_or_default()
                            })])
                            .current_dir(&working_dir)
                            .output()
                            .await;
                        let _ = tokio::process::Command::new("jj")
                            .args(["git", "export"])
                            .current_dir(&working_dir)
                            .output()
                            .await;
                        true
                    }
                    Err(e) => {
                        events.agent_event(AgentEvent::Log {
                            task_id: task_id_str.clone(),
                            level: LogLevel::Error,
                            message: format!("Fix agent failed to start: {}", e),
                        });
                        false
                    }
                };

                let issues: Vec<String> = findings.lines()
                    .filter(|l| l.starts_with("- ISSUE:") || l.starts_with("ISSUE:"))
                    .map(|l| l.to_string())
                    .collect();

                let signoff = QaSignoff {
                    status: if fix_result { QaStatus::FixesApplied } else { QaStatus::Rejected },
                    issues_found: issues,
                    timestamp: chrono::Utc::now(),
                    session_id: Uuid::new_v4(),
                };

                Self::transition_to_human_review(&tasks, &storage, task_id, Some(signoff)).await;
            } else {
                // All clear — no issues
                events.agent_event(AgentEvent::Log {
                    task_id: task_id_str.clone(),
                    level: LogLevel::Info,
                    message: "AI review passed — moving to human review".to_string(),
                });

                let signoff = QaSignoff {
                    status: QaStatus::Approved,
                    issues_found: Vec::new(),
                    timestamp: chrono::Utc::now(),
                    session_id: Uuid::new_v4(),
                };

                Self::transition_to_human_review(&tasks, &storage, task_id, Some(signoff)).await;
            }

            reviewing_handles.write().await.remove(&task_id);
        });

        self.reviewing_handles.write().await.insert(task_id, handle);
    }

    async fn transition_to_human_review(
        tasks: &Tasks,
        storage: &crate::config::Storage,
        task_id: Uuid,
        signoff: Option<QaSignoff>,
    ) {
        {
            let mut tasks_w = tasks.write().await;
            if let Some(t) = tasks_w.get_mut(&task_id) {
                t.status = TaskStatus::HumanReview;
                t.phase = TaskPhase::Complete;
                t.phase_progress = 95;
                t.overall_progress = 90;
                t.updated_at = chrono::Utc::now();
                if let Some(s) = signoff {
                    t.qa_signoff = Some(s);
                }
            }
        }
        Self::persist_task_static(tasks, storage, task_id).await;
    }

    pub async fn get_task_output(&self, task_id: Uuid) -> Vec<AgentLogEntry> {
        let executions = self.executions.read().await;
        let eid = executions.values()
            .find(|e| e.task_id == Some(task_id))
            .map(|e| e.id);
        if let Some(eid) = eid {
            self.logs.read().await.get(&eid).cloned().unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    // --- Helpers ---

    /// Resolve agent launch context.
    ///
    /// If the task's project is attached to a workspace *and that
    /// workspace's root still exists on disk*, return `(workspace_root,
    /// [task_working_dir])` so Claude runs from the workspace cwd with the
    /// task tree exposed via `--add-dir`. Otherwise return
    /// `(task_working_dir, [])` — the same fallback used when there is no
    /// workspace at all. A registered root that has been unmounted, renamed,
    /// or deleted must not fail the task: the task's own worktree is
    /// unaffected by the workspace root disappearing, so launching from it
    /// directly is strictly safer than erroring out from underneath a task
    /// whose own working directory is still perfectly valid.
    async fn resolve_workspace_launch(
        tasks: &Tasks,
        projects: &Arc<RwLock<HashMap<Uuid, crate::domain::Project>>>,
        workspace_registry: &Arc<RwLock<crate::config::WorkspaceRegistry>>,
        task_id: Uuid,
        task_working_dir: &str,
    ) -> (String, Vec<std::path::PathBuf>) {
        let project_id = {
            let tasks_r = tasks.read().await;
            tasks_r.get(&task_id).map(|t| t.project_id)
        };
        let Some(project_id) = project_id else {
            return (task_working_dir.to_string(), Vec::new());
        };

        let workspace_id = {
            let projects_r = projects.read().await;
            projects_r.get(&project_id).and_then(|p| p.scope.workspace_id())
        };
        let Some(workspace_id) = workspace_id else {
            return (task_working_dir.to_string(), Vec::new());
        };

        let root_path = {
            let registry_r = workspace_registry.read().await;
            registry_r.get(&workspace_id).map(|ws| ws.root_path.as_path().to_path_buf())
        };

        match root_path {
            Some(root) if root.is_dir() => (
                root.to_string_lossy().to_string(),
                vec![std::path::PathBuf::from(task_working_dir)],
            ),
            _ => (task_working_dir.to_string(), Vec::new()),
        }
    }

    async fn resolve_working_dir_for_task(
        tasks: &Tasks,
        projects: &Arc<RwLock<HashMap<Uuid, crate::domain::Project>>>,
        repositories: &Arc<RwLock<HashMap<Uuid, crate::domain::Repository>>>,
        task_id: Uuid,
    ) -> Result<String, String> {
        let tasks_r = tasks.read().await;
        let task = tasks_r.get(&task_id)
            .ok_or_else(|| format!("Task {} not found", task_id))?;
        let project_id = task.project_id;
        drop(tasks_r);

        let projects_r = projects.read().await;
        let project = projects_r.get(&project_id)
            .ok_or_else(|| format!("Project {} not found", project_id))?;
        let repo_id = project.repository_id
            .ok_or_else(|| format!("Project {} has no repository linked", project_id))?;
        drop(projects_r);

        let repos = repositories.read().await;
        let repo = repos.get(&repo_id)
            .ok_or_else(|| format!("Repository {} not found", repo_id))?;
        Ok(repo.local_path.clone())
    }

    /// Commit agent changes in the worktree/working directory.
    async fn commit_changes(
        tasks: &Tasks,
        task_id: Uuid,
        working_dir: &str,
        events: &SharedEventSink,
    ) {
        let title = {
            let tasks_r = tasks.read().await;
            tasks_r.get(&task_id).map(|t| t.title.clone()).unwrap_or_default()
        };
        let task_id_str = task_id.to_string();

        // Try jj first (for jj-managed repos)
        let jj_ok = if let Ok(output) = tokio::process::Command::new("jj")
            .args(["describe", "-m", &format!("task: {}", title)])
            .current_dir(working_dir)
            .output()
            .await
        {
            if output.status.success() {
                let _ = tokio::process::Command::new("jj")
                    .args(["git", "export"])
                    .current_dir(working_dir)
                    .output()
                    .await;
                events.agent_event(AgentEvent::Log {
                    task_id: task_id_str.clone(),
                    level: LogLevel::Info,
                    message: "Committed via jj".to_string(),
                });
                true
            } else {
                false
            }
        } else {
            false
        };

        // Fallback to git (for worktrees or git-only repos)
        if !jj_ok {
            let _ = tokio::process::Command::new("git")
                .args(["add", "-A"])
                .current_dir(working_dir)
                .output()
                .await;

            let commit_msg = format!("task: {}", title);
            match tokio::process::Command::new("git")
                .args(["commit", "-m", &commit_msg, "--allow-empty"])
                .current_dir(working_dir)
                .output()
                .await
            {
                Ok(output) if output.status.success() => {
                    events.agent_event(AgentEvent::Log {
                        task_id: task_id_str,
                        level: LogLevel::Info,
                        message: "Committed via git".to_string(),
                    });
                }
                _ => {
                    events.agent_event(AgentEvent::Log {
                        task_id: task_id_str,
                        level: LogLevel::Warn,
                        message: "Git commit skipped (no changes or error)".to_string(),
                    });
                }
            }
        }
    }

    async fn update_task_phase(&self, task_id: Uuid, phase: TaskPhase, progress: u8) {
        Self::update_task_phase_static(&self.tasks, task_id, phase.clone(), progress).await;
        self.events.agent_event(AgentEvent::PhaseChange {
            task_id: task_id.to_string(),
            phase,
            progress,
        });
    }

    async fn update_task_phase_static(tasks: &Tasks, task_id: Uuid, phase: TaskPhase, progress: u8) {
        let mut tasks = tasks.write().await;
        if let Some(t) = tasks.get_mut(&task_id) {
            t.phase = phase;
            t.phase_progress = progress;
            t.updated_at = chrono::Utc::now();
        }
    }

    async fn set_task_error_static(tasks: &Tasks, storage: &crate::config::Storage, task_id: Uuid, msg: &str) {
        {
            let mut tasks = tasks.write().await;
            if let Some(t) = tasks.get_mut(&task_id) {
                t.status = TaskStatus::Error;
                t.phase = TaskPhase::Failed;
                t.error_message = Some(msg.to_string());
                t.updated_at = chrono::Utc::now();
            }
        }
        Self::persist_task_static(tasks, storage, task_id).await;
    }

    /// Get diff from jj or git (whichever is available).
    async fn get_diff(working_dir: &str) -> Option<String> {
        // Try jj first
        if let Ok(output) = tokio::process::Command::new("jj")
            .args(["diff", "--git"])
            .current_dir(working_dir)
            .output()
            .await
        {
            if output.status.success() {
                return Some(String::from_utf8_lossy(&output.stdout).to_string());
            }
        }
        // Fallback to git
        if let Ok(output) = tokio::process::Command::new("git")
            .args(["diff", "HEAD"])
            .current_dir(working_dir)
            .output()
            .await
        {
            if output.status.success() {
                return Some(String::from_utf8_lossy(&output.stdout).to_string());
            }
        }
        None
    }

    async fn persist_task_static(tasks: &Tasks, storage: &crate::config::Storage, task_id: Uuid) {
        let tasks_r = tasks.read().await;
        if let Some(task) = tasks_r.get(&task_id) {
            let project_id = task.project_id;
            let project_tasks: Vec<Task> = tasks_r.values()
                .filter(|t| t.project_id == project_id)
                .cloned()
                .collect();
            let _ = storage.save_project_tasks(project_id, &project_tasks);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceRegistry;
    use crate::domain::{AgentConfig, AgentType, Project, ProjectScope, Workspace, WorkspaceRoot};
    use crate::test_helpers::create_test_task_full;

    fn test_project(id: Uuid, scope: ProjectScope) -> Project {
        Project {
            id,
            name: "test-project".to_string(),
            repository_id: None,
            scope,
            state_location: crate::config::paths::StateLocation::External,
            agent_type: AgentType::ClaudeCode,
            agent_config: AgentConfig {
                agent_type: AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn test_registry() -> (WorkspaceRegistry, tempfile::TempDir) {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let registry = WorkspaceRegistry::load_from(temp.path().join("workspaces.toml"))
            .expect("a missing registry file should still load empty");
        (registry, temp)
    }

    #[tokio::test]
    async fn resolve_workspace_launch_uses_the_workspace_root_when_it_exists() {
        let workspace_root = tempfile::TempDir::new().expect("workspace root tempdir");
        let workspace = Workspace::new(
            "ws".to_string(),
            WorkspaceRoot::try_new(workspace_root.path()).expect("real dir must validate"),
        );
        let workspace_id = workspace.id;

        let (mut registry, _reg_temp) = test_registry();
        registry.upsert(workspace).expect("upsert should succeed");

        let project_id = Uuid::new_v4();
        let projects = Arc::new(RwLock::new(HashMap::from([(
            project_id,
            test_project(project_id, ProjectScope::InWorkspace { workspace_id }),
        )])));

        let task = create_test_task_full("t", project_id, TaskStatus::InProgress, 0);
        let task_id = task.id;
        let tasks: Tasks = Arc::new(RwLock::new(HashMap::from([(task_id, task)])));

        let (root, extra_dirs) = TaskExecutor::resolve_workspace_launch(
            &tasks,
            &projects,
            &Arc::new(RwLock::new(registry)),
            task_id,
            "/tmp/task-working-dir",
        )
        .await;

        assert_eq!(root, workspace_root.path().canonicalize().unwrap().to_string_lossy());
        assert_eq!(extra_dirs, vec![std::path::PathBuf::from("/tmp/task-working-dir")]);
    }

    #[tokio::test]
    async fn resolve_workspace_launch_falls_back_to_task_dir_when_the_registered_root_is_missing() {
        // The registry holds a root that no longer exists on disk — e.g. an
        // unmounted drive, a rename, or a deleted folder — which
        // `WorkspaceRoot::from_trusted` allows (registry loads never
        // re-validate). The task's own worktree is unaffected and must still
        // be usable.
        let vanished = tempfile::TempDir::new().expect("tempdir").path().join("gone");
        let workspace = Workspace::new("ws".to_string(), WorkspaceRoot::from_trusted(vanished));
        let workspace_id = workspace.id;

        let (mut registry, _reg_temp) = test_registry();
        registry.upsert(workspace).expect("upsert should succeed even for a missing root");

        let project_id = Uuid::new_v4();
        let projects = Arc::new(RwLock::new(HashMap::from([(
            project_id,
            test_project(project_id, ProjectScope::InWorkspace { workspace_id }),
        )])));

        let task = create_test_task_full("t", project_id, TaskStatus::InProgress, 0);
        let task_id = task.id;
        let tasks: Tasks = Arc::new(RwLock::new(HashMap::from([(task_id, task)])));

        let (root, extra_dirs) = TaskExecutor::resolve_workspace_launch(
            &tasks,
            &projects,
            &Arc::new(RwLock::new(registry)),
            task_id,
            "/tmp/task-working-dir",
        )
        .await;

        assert_eq!(
            root, "/tmp/task-working-dir",
            "a missing workspace root must fall back to the task's own working directory"
        );
        assert!(
            extra_dirs.is_empty(),
            "the fallback must match the no-workspace shape exactly (no --add-dir entries)"
        );
    }

    fn test_storage() -> (crate::config::Storage, tempfile::TempDir) {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        let storage =
            crate::config::Storage::with_paths(crate::config::paths::AppPaths::with_roots(
                root.join("config"),
                root.join("data"),
                root.join("cache"),
                root.join("runtime"),
            ));
        (storage, temp)
    }

    #[tokio::test]
    async fn try_reserve_cleanup_prevents_a_second_concurrent_attempt_for_the_same_task() {
        let in_flight: RwLock<std::collections::HashSet<Uuid>> =
            RwLock::new(std::collections::HashSet::new());
        let task_id = Uuid::new_v4();

        assert!(
            TaskExecutor::try_reserve_cleanup(&in_flight, task_id).await,
            "the first reservation for a task must succeed"
        );
        assert!(
            !TaskExecutor::try_reserve_cleanup(&in_flight, task_id).await,
            "a second reservation while the first is still held must be refused"
        );

        TaskExecutor::release_cleanup(&in_flight, task_id).await;

        assert!(
            TaskExecutor::try_reserve_cleanup(&in_flight, task_id).await,
            "releasing the guard must allow a later attempt to reserve it again"
        );
    }

    #[test]
    fn tasks_eligible_for_cleanup_retry_finds_only_done_tasks_with_a_retained_worktree_path() {
        let mut tasks = HashMap::new();

        let eligible = create_test_task_full(
            "merged, cleanup failed",
            Uuid::new_v4(),
            TaskStatus::Done,
            0,
        );
        let eligible_id = eligible.id;
        let mut eligible = eligible;
        eligible.worktree_path = Some("/tmp/eligible".to_string());
        eligible.branch_name = Some("eligible-branch".to_string());
        tasks.insert(eligible_id, eligible);

        let mut already_clean =
            create_test_task_full("merged, already clean", Uuid::new_v4(), TaskStatus::Done, 0);
        already_clean.worktree_path = None;
        tasks.insert(already_clean.id, already_clean);

        let mut still_running = create_test_task_full(
            "not complete yet",
            Uuid::new_v4(),
            TaskStatus::InProgress,
            0,
        );
        still_running.worktree_path = Some("/tmp/still-running".to_string());
        tasks.insert(still_running.id, still_running);

        let mut already_retrying = create_test_task_full(
            "merged, retry in flight",
            Uuid::new_v4(),
            TaskStatus::Done,
            0,
        );
        already_retrying.worktree_path = Some("/tmp/already-retrying".to_string());
        let already_retrying_id = already_retrying.id;
        tasks.insert(already_retrying_id, already_retrying);

        let in_flight = std::collections::HashSet::from([already_retrying_id]);

        let result = TaskExecutor::tasks_eligible_for_cleanup_retry(&tasks, &in_flight);

        assert_eq!(
            result,
            vec![(eligible_id, "/tmp/eligible".to_string(), "eligible-branch".to_string())],
            "only the Done task with a retained worktree_path and no in-flight attempt must be eligible"
        );
    }

    #[tokio::test]
    async fn attempt_worktree_cleanup_preserves_the_path_on_failure_and_clears_it_once_it_succeeds()
    {
        let (storage, _storage_temp) = test_storage();
        let repo_temp = tempfile::TempDir::new().expect("repo temp dir");
        let blocking_dir = tempfile::TempDir::new().expect("worktree temp dir");
        let wt_path = blocking_dir.path().to_str().unwrap().to_string();

        let wt_mgr = WorktreeManager::new(
            Arc::new(crate::config::paths::AppPaths::with_roots(
                repo_temp.path().join("config"),
                repo_temp.path().join("data"),
                repo_temp.path().join("cache"),
                repo_temp.path().join("runtime"),
            )),
            crate::config::paths::WorktreePlacement::Managed,
        );

        let mut task = create_test_task_full("merge cleanup", Uuid::new_v4(), TaskStatus::Done, 0);
        task.worktree_path = Some(wt_path.clone());
        let task_id = task.id;
        let tasks: Tasks = Arc::new(RwLock::new(HashMap::from([(task_id, task)])));

        // `repo_temp` is not a git repository and `wt_path` was never
        // registered as a worktree, so every fallback inside `remove` fails
        // at the git level — but the directory genuinely still exists, so
        // this is a real (not faked) removal failure.
        let first = TaskExecutor::attempt_worktree_cleanup(
            &wt_mgr,
            &tasks,
            &storage,
            task_id,
            &wt_path,
            "some-branch",
            repo_temp.path().to_str().unwrap(),
        )
        .await;

        assert!(
            first.is_err(),
            "removal must fail while the directory still exists"
        );
        assert_eq!(
            tasks
                .read()
                .await
                .get(&task_id)
                .unwrap()
                .worktree_path
                .as_deref(),
            Some(wt_path.as_str()),
            "a failed attempt must leave worktree_path in place for a later retry"
        );

        // Simulate whatever was blocking removal having been resolved by
        // the time the next maintenance pass retries.
        std::fs::remove_dir_all(&wt_path).unwrap();

        let second = TaskExecutor::attempt_worktree_cleanup(
            &wt_mgr,
            &tasks,
            &storage,
            task_id,
            &wt_path,
            "some-branch",
            repo_temp.path().to_str().unwrap(),
        )
        .await;

        assert!(
            second.is_ok(),
            "removal must succeed once the directory is actually gone"
        );
        assert_eq!(
            tasks.read().await.get(&task_id).unwrap().worktree_path,
            None,
            "a successful retry must clear worktree_path"
        );
    }

    #[tokio::test]
    async fn repeated_identical_cleanup_warnings_are_reported_once() {
        // The ~30s retry pass re-attempts a permanently failing cleanup
        // forever, which is the intended safety behaviour. What must not
        // repeat forever is the user-visible event.
        let seen: RwLock<HashMap<Uuid, String>> = RwLock::new(HashMap::new());
        let task_id = Uuid::new_v4();

        assert!(
            TaskExecutor::record_cleanup_warning(&seen, task_id, "no repository").await,
            "the first failure must be reported"
        );
        assert!(
            !TaskExecutor::record_cleanup_warning(&seen, task_id, "no repository").await,
            "the same failure on the next sweep must not be reported again"
        );
        assert!(
            TaskExecutor::record_cleanup_warning(&seen, task_id, "directory is locked").await,
            "a failure that changes character must still be reported"
        );
        assert!(
            TaskExecutor::record_cleanup_warning(&seen, Uuid::new_v4(), "no repository").await,
            "suppression must be per task, not global"
        );

        // A success clears the record, so a condition that recurs is news.
        seen.write().await.remove(&task_id);
        assert!(
            TaskExecutor::record_cleanup_warning(&seen, task_id, "directory is locked").await,
            "a failure recurring after a success must be reported again"
        );
    }

    /// Make the next `save_project_tasks` fail deterministically by putting a
    /// regular file where the tasks directory has to be, so `create_dir_all`
    /// inside the atomic write fails with `NotADirectory`. No permission bits,
    /// so it behaves the same for every user including root.
    fn block_task_persistence(storage: &crate::config::Storage) -> std::path::PathBuf {
        let blocker = storage.paths().config_dir().join("tasks");
        let _ = std::fs::remove_dir_all(&blocker);
        std::fs::write(&blocker, b"not a directory").expect("place persistence blocker");
        blocker
    }

    /// A manager and repository directory for which every git removal fails
    /// but the worktree directory is genuinely absent — the exact shape of a
    /// cleanup retry, where `remove` correctly reports `Ok`.
    fn cleanup_fixture() -> (WorktreeManager, tempfile::TempDir, String) {
        let repo_temp = tempfile::TempDir::new().expect("repo temp dir");
        let wt_mgr = WorktreeManager::new(
            Arc::new(crate::config::paths::AppPaths::with_roots(
                repo_temp.path().join("config"),
                repo_temp.path().join("data"),
                repo_temp.path().join("cache"),
                repo_temp.path().join("runtime"),
            )),
            crate::config::paths::WorktreePlacement::Managed,
        );
        let wt_path = repo_temp
            .path()
            .join("already-gone")
            .to_str()
            .unwrap()
            .to_string();
        (wt_mgr, repo_temp, wt_path)
    }

    #[tokio::test]
    async fn attempt_worktree_cleanup_fails_and_retains_the_path_when_the_board_cannot_be_saved() {
        // The worktree is physically gone but the task file cannot be written.
        // Reporting success here would strand the task: disk would still name
        // a worktree that no longer exists, and the retry pass selects on the
        // in-memory value, so nothing would ever revisit it.
        let (storage, _storage_temp) = test_storage();
        let (wt_mgr, repo_temp, wt_path) = cleanup_fixture();
        let repo_path = repo_temp.path().to_str().unwrap().to_string();

        let mut task = create_test_task_full("blocked save", Uuid::new_v4(), TaskStatus::Done, 0);
        task.worktree_path = Some(wt_path.clone());
        let project_id = task.project_id;
        let task_id = task.id;
        let tasks: Tasks = Arc::new(RwLock::new(HashMap::from([(task_id, task)])));

        let blocker = block_task_persistence(&storage);

        let result = TaskExecutor::attempt_worktree_cleanup(
            &wt_mgr, &tasks, &storage, task_id, &wt_path, "task-branch", &repo_path,
        )
        .await;

        assert!(
            result.is_err(),
            "cleanup must not report success when the cleared record could not be persisted"
        );
        assert_eq!(
            tasks.read().await.get(&task_id).unwrap().worktree_path.as_deref(),
            Some(wt_path.as_str()),
            "memory must keep the reference so the retry pass still selects the task"
        );
        assert!(
            TaskExecutor::tasks_eligible_for_cleanup_retry(
                &*tasks.read().await,
                &std::collections::HashSet::new(),
            )
            .iter()
            .any(|(id, _, _)| *id == task_id),
            "the task must remain eligible for a later retry"
        );

        // Once persistence works again, the retry converges: `remove` on an
        // already-absent directory is still `Ok`, so the record is cleared.
        std::fs::remove_file(&blocker).expect("unblock persistence");

        TaskExecutor::attempt_worktree_cleanup(
            &wt_mgr, &tasks, &storage, task_id, &wt_path, "task-branch", &repo_path,
        )
        .await
        .expect("the retry must succeed once the board can be saved");

        assert_eq!(
            tasks.read().await.get(&task_id).unwrap().worktree_path,
            None
        );
        let persisted = storage.load_project_tasks(project_id).expect("load tasks");
        assert_eq!(
            persisted.iter().find(|t| t.id == task_id).unwrap().worktree_path,
            None,
            "disk must agree with memory once cleanup reports success"
        );
    }

    #[tokio::test]
    async fn attempt_worktree_cleanup_keeps_sibling_tasks_when_it_clears_one() {
        // The staged snapshot is the whole project, so it must carry sibling
        // tasks through unchanged rather than writing only the cleaned task.
        let (storage, _storage_temp) = test_storage();
        let (wt_mgr, repo_temp, wt_path) = cleanup_fixture();
        let repo_path = repo_temp.path().to_str().unwrap().to_string();

        let project_id = Uuid::new_v4();
        let mut target = create_test_task_full("cleaned", project_id, TaskStatus::Done, 0);
        target.worktree_path = Some(wt_path.clone());
        let target_id = target.id;

        let mut sibling = create_test_task_full("untouched", project_id, TaskStatus::InProgress, 1);
        sibling.worktree_path = Some("/some/other/worktree".to_string());
        let sibling_id = sibling.id;

        let tasks: Tasks = Arc::new(RwLock::new(HashMap::from([
            (target_id, target),
            (sibling_id, sibling),
        ])));

        TaskExecutor::attempt_worktree_cleanup(
            &wt_mgr, &tasks, &storage, target_id, &wt_path, "task-branch", &repo_path,
        )
        .await
        .expect("cleanup should succeed");

        let persisted = storage.load_project_tasks(project_id).expect("load tasks");
        assert_eq!(persisted.len(), 2, "both tasks must survive the write");
        assert_eq!(
            persisted.iter().find(|t| t.id == target_id).unwrap().worktree_path,
            None
        );
        assert_eq!(
            persisted
                .iter()
                .find(|t| t.id == sibling_id)
                .unwrap()
                .worktree_path
                .as_deref(),
            Some("/some/other/worktree"),
            "a sibling's worktree reference must not be collateral damage"
        );
    }

    #[tokio::test]
    async fn attempt_worktree_cleanup_does_not_clear_a_newer_worktree_path() {
        // A cleanup for an old path can still be in flight when the task is
        // re-run and records a new worktree. It must not erase the new one.
        let (storage, _storage_temp) = test_storage();
        let (wt_mgr, repo_temp, wt_path) = cleanup_fixture();
        let repo_path = repo_temp.path().to_str().unwrap().to_string();

        let mut task = create_test_task_full("re-run", Uuid::new_v4(), TaskStatus::Done, 0);
        task.worktree_path = Some("/a/newer/worktree".to_string());
        let task_id = task.id;
        let tasks: Tasks = Arc::new(RwLock::new(HashMap::from([(task_id, task)])));

        TaskExecutor::attempt_worktree_cleanup(
            &wt_mgr, &tasks, &storage, task_id, &wt_path, "task-branch", &repo_path,
        )
        .await
        .expect("the stale cleanup itself is not a failure");

        assert_eq!(
            tasks.read().await.get(&task_id).unwrap().worktree_path.as_deref(),
            Some("/a/newer/worktree"),
            "an older cleanup must not clear a replacement worktree reference"
        );
    }
}
