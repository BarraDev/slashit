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

/// A task execution the executor can still reach.
///
/// The join handle on its own is not enough to stop one. Aborting it drops the
/// future wherever it is parked, and that future is exactly what owns the
/// agent process: the `kill()` that ends the process, the bookkeeping that
/// records the execution as stopped and the removal from `running_handles` are
/// all statements *after* the await it is parked on, so an abort skips every
/// one of them and leaves the agent running with nothing pointing at it.
///
/// `cancel` asks the future to end instead. It stops waiting on the process,
/// then runs the same cleanup any other outcome runs, which is what makes
/// "the stop returned" mean "the agent is gone".
struct RunningTask {
    handle: JoinHandle<()>,
    cancel: tokio::sync::watch::Sender<bool>,
}

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
    running_handles: Arc<RwLock<HashMap<Uuid, RunningTask>>>,
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

    /// Build the polling loop, leaving the choice of runtime to the caller.
    ///
    /// This deliberately returns the future rather than spawning it. The two
    /// callers live in different runtime worlds: `slashitd` runs inside
    /// `#[tokio::main]`, while the desktop app starts the loop from Tauri's
    /// `setup()` closure, which is *not* running on a Tokio worker and has no
    /// reactor in thread-local scope. A `tokio::spawn` inside this function
    /// therefore panicked with "there is no reactor running" for the GUI while
    /// working perfectly for the daemon. Spawning `tauri::async_runtime::spawn`
    /// here instead would only move the problem: this module is deliberately
    /// free of every `tauri::` reference so `slashitd` can link it without the
    /// GUI toolkit, which is the whole reason [`EventSink`] exists.
    ///
    /// Handing back the future keeps runtime ownership explicit at each call
    /// site — `tauri::async_runtime::spawn` in `lib.rs`, `tokio::spawn` in
    /// `daemon.rs` — and makes constructing the loop a runtime-free operation
    /// that cannot panic no matter who calls it. The loop body still uses
    /// `tokio::spawn` and `tokio::time`, so it must be polled on a Tokio
    /// runtime; Tauri's global async runtime is one.
    ///
    /// `shutdown`, when given, lets a caller stop new-task promotion
    /// cooperatively: the loop checks it before every pass and stops after
    /// the current pass finishes, rather than being killed via
    /// `JoinHandle::abort` at an arbitrary `.await` point (worktree creation
    /// shells out to `git`/`jj`, and none of those child processes are
    /// spawned with `kill_on_drop`, so an abort mid-spawn could orphan one).
    /// The desktop GUI has no such handshake and passes `None`; the loop then
    /// simply runs for the life of the process, as it always has.
    ///
    /// A caller that sends a shutdown signal must await the spawned handle
    /// before treating "no new work will be promoted" as true: the shutdown
    /// check only runs between passes, so a pass already past its own check
    /// when the signal is sent can still promote and spawn a task afterward,
    /// and that task's `running_handles` entry does not exist until that pass
    /// finishes. Sampling `running_task_count()` without first awaiting that
    /// handle can therefore observe zero while such a pass is still in flight.
    pub fn polling_loop(
        self: &Arc<Self>,
        shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let executor = Arc::clone(self);
        async move {
            let mut shutdown = shutdown;
            loop {
                if matches!(&shutdown, Some(rx) if *rx.borrow()) {
                    return;
                }
                executor.check_and_execute().await;
                match shutdown.as_mut() {
                    Some(rx) => {
                        tokio::select! {
                            _ = tokio::time::sleep(tokio::time::Duration::from_secs(3)) => {}
                            _ = rx.changed() => {}
                        }
                    }
                    None => tokio::time::sleep(tokio::time::Duration::from_secs(3)).await,
                }
            }
        }
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

    /// Whether the poller should start this task on this pass.
    ///
    /// `InProgress` alone does not mean "running": it is also what a task
    /// promoted out of the queue looks like in the instant before an agent
    /// exists for it. The idle phase is what distinguishes the two, because
    /// [`spawn_task_execution`](Self::spawn_task_execution) moves the task to
    /// `Coding` before it spawns anything.
    ///
    /// The consequence is worth naming where the rule lives: anything that
    /// writes this exact pair is asking for the task to be executed, whatever
    /// it meant to say. That is why [`stop_task`](Self::stop_task) does not
    /// leave a stopped task here.
    fn is_pending(task: &Task) -> bool {
        task.status == TaskStatus::InProgress && task.phase == TaskPhase::Idle
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
                .filter(|t| Self::is_pending(t))
                .map(|t| t.id)
                .collect()
        };

        // A panic inside a spawned task's future skips its own cleanup code
        // entirely (unwinding runs no further statements in that task), so it
        // cannot be relied on to remove its own entry. Pruning finished
        // handles here — reachable on every poll tick — is the backstop that
        // catches that case regardless of which future code path forgets to
        // clean up after itself.
        self.running_handles
            .write()
            .await
            .retain(|_, r| !r.handle.is_finished());
        let running = self.running_handles.read().await.len();
        let limit = {
            let mgr = self.queue_manager.read().await;
            mgr.config().parallel_task_limit as usize
        };
        let available = limit.saturating_sub(running);

        for task_id in pending.into_iter().take(available) {
            self.spawn_task_execution(task_id).await;
        }

        // Same backstop as `running_handles` above: a panicked review task
        // cannot be relied on to remove its own entry, and a leaked one here
        // holds `running_task_count()` above zero forever, which is what
        // daemon shutdown waits on.
        self.reviewing_handles.write().await.retain(|_, h| !h.is_finished());

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
                            let mut pending_worktree_removal: Option<String> = None;
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
                                                pending_worktree_removal = Some(wt_path);
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
                            if let Some(wt_path) = pending_worktree_removal {
                                self.spawn_worktree_cleanup(task_id, wt_path).await;
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
            for (task_id, wt_path) in cleanup_retries {
                self.spawn_worktree_cleanup(task_id, wt_path).await;
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
    ) -> Vec<(Uuid, String)> {
        tasks
            .values()
            .filter(|t| t.status == TaskStatus::Done)
            .filter(|t| !in_flight.contains(&t.id))
            .filter_map(|t| Some((t.id, t.worktree_path.clone()?)))
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
        repo_path: &str,
    ) -> Result<(), String> {
        wt_mgr.remove(wt_path, repo_path).await?;

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
    async fn spawn_worktree_cleanup(&self, task_id: Uuid, wt_path: String) {
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
                    &self.events,
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
        let events = self.events.clone();
        let in_flight = self.cleanup_in_flight.clone();
        let last_warning = self.cleanup_last_warning.clone();
        let wt_path_done = wt_path.clone();

        tokio::spawn(async move {
            match Self::attempt_worktree_cleanup(
                &wt_mgr, &tasks, &storage, task_id, &wt_path, &repo_path,
            )
            .await
            {
                Err(e) => {
                    Self::warn_cleanup_once(
                        &events,
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
        events: &SharedEventSink,
        last_warning: &RwLock<HashMap<Uuid, String>>,
        task_id: Uuid,
        message: String,
    ) {
        if !Self::record_cleanup_warning(last_warning, task_id, &message).await {
            return;
        }

        events.agent_event(AgentEvent::Log {
            task_id: task_id.to_string(),
            level: LogLevel::Warn,
            message,
        });
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
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);

        // The lock is taken before the spawn and held across the insert so
        // there is no instant in which an execution is running and
        // `stop_task` can find nothing to stop. The future takes this same
        // lock, but only to remove itself once it is finished, so it waits
        // here rather than deadlocking.
        let mut handles = self.running_handles.write().await;

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
                    // This early return skips the removal after `runner.wait()`
                    // below, so it must remove itself here or this slot never
                    // frees up.
                    running_handles.write().await.remove(&task_id);
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

            // Wait for the run to finish, or for a stop to end it early.
            //
            // Biased so that a process which has already exited is reported as
            // the completion it is: when both arms are ready the run finished
            // before the stop reached it, and calling that a cancellation would
            // throw away a result the agent had already produced.
            let stopped = tokio::select! {
                biased;

                result = runner.wait() => {
                    match result {
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
                    false
                }

                // Dropping the `wait()` future above releases the child,
                // which is what lets the `kill()` in the shared cleanup below
                // reach the process at all. `wait()` must not be entered
                // again after that — it takes the child's stderr on its way
                // in, so a second call would report a run with no output.
                //
                // A closed channel is treated the same as a stop on purpose:
                // it means the executor is no longer tracking this run, and a
                // run nothing is tracking is exactly what must not be left
                // with a process behind it.
                _ = cancelled.changed() => true,
            };

            if stopped {
                events.agent_event(AgentEvent::Log {
                    task_id: task_id.to_string(),
                    level: LogLevel::Info,
                    message: "Stopped — ending the agent".to_string(),
                });
            }

            // Cleanup — the one path every outcome reaches, cancellation
            // included.
            let _ = runner.kill().await;
            running_handles.write().await.remove(&task_id);

            if let Some(exec) = executions.write().await.get_mut(&execution_id) {
                exec.status = AgentStatus::Stopped;
                exec.stopped_at = Some(chrono::Utc::now());
            }
        });

        handles.insert(task_id, RunningTask { handle, cancel });
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

    /// End a task's execution at the user's request.
    ///
    /// `Ok` means the agent process is gone *and* the stop is on disk, so a
    /// caller that got it can say the run is over rather than that it has
    /// been asked to be over. `Err` means the process was still ended, but
    /// the record of that could not be saved; the message says so, because
    /// the difference decides what the next startup does with the task.
    ///
    /// Two things this deliberately does not do. It does not abort the
    /// execution future: see [`RunningTask`] for why aborting the owner of the
    /// process is what leaves the process behind. And it does not leave the
    /// task `InProgress`, because that plus an idle phase is precisely what
    /// [`is_pending`](Self::is_pending) means by "start this", so a stop that
    /// left it there would be undone by the queue on its next pass.
    ///
    /// Stopping is not discarding: the worktree, the branch and the commits
    /// the run had already made all survive, and a later explicit move back
    /// into a working column reattaches to them.
    pub async fn stop_task(&self, task_id: Uuid) -> Result<(), String> {
        // Taken out of the map before awaiting anything, so the guard is
        // released before the future below tries to remove itself.
        let running = self.running_handles.write().await.remove(&task_id);

        if let Some(running) = running {
            let _ = running.cancel.send(true);
            // A run that finished on its own between the removal and here has
            // already recorded its own outcome; joining it is still correct,
            // it simply returns at once. A panicked run returns an error,
            // which the settling below is what covers.
            let _ = running.handle.await;
        }

        Self::settle_stopped_static(&self.tasks, &self.storage, task_id).await
    }

    /// Record a stopped task as work that is waiting for a person again.
    ///
    /// `Backlog` is chosen out of the states the product already has, not
    /// invented for this: it is the only one that is not runnable without an
    /// explicit action, is not resumed by the hydration pass that requeues
    /// tasks a crash left `InProgress`, keeps the worktree that `Done` would
    /// remove, and claims no failure that did not happen. Moving the card back
    /// into a working column resets execution state and reattaches to the
    /// preserved branch, which is the same route a retry already takes.
    ///
    /// Guarded on `InProgress` so a stop that arrives after the run finished
    /// on its own does not pull a task back out of the review it had already
    /// reached. That is the honest resolution of that race: the run was over
    /// before the stop got there.
    ///
    /// Persists before publishing, the same contract
    /// [`clear_worktree_path_durably`](crate::commands::task::clear_worktree_path_durably)
    /// holds, and for a sharper reason. What survives a failed write is the
    /// `in_progress` this task started as, and the next startup reads that as
    /// a run a crash interrupted and puts it back on the queue. Reporting the
    /// stop from memory alone would therefore hand the user a stop that a
    /// restart quietly undoes. Leaving memory as it was instead of settling
    /// it is also what keeps the failure recoverable: the task is still
    /// `InProgress`, so simply stopping it again once the disk is writable
    /// works, where a memory-only settle would make every later stop a no-op
    /// against a file still saying `in_progress`.
    ///
    /// The whole transaction runs under one write guard, and the save is
    /// synchronous, so no sibling mutation can land between the write and the
    /// in-memory commit.
    ///
    /// The cost of not publishing is worth stating plainly: a task whose
    /// phase was already `Idle` stays [`is_pending`](Self::is_pending) after
    /// a failed write, so the queue may start it again on a later pass. That
    /// is what an unwritable disk does to every other in-flight task too, and
    /// it follows from memory and the file agreeing. Publishing `Backlog`
    /// anyway would buy that one case at the price of a worse one: the guard
    /// above would then send the user's next stop straight to `Ok`, for a
    /// task the file still has as running -- the exact answer this returns a
    /// `Result` to stop giving.
    async fn settle_stopped_static(
        tasks: &Tasks,
        storage: &crate::config::Storage,
        task_id: Uuid,
    ) -> Result<(), String> {
        let mut tasks_w = tasks.write().await;

        let Some(task) = tasks_w.get(&task_id) else {
            return Ok(()); // deleted while its agent was being stopped
        };
        if task.status != TaskStatus::InProgress {
            return Ok(());
        }
        let project_id = task.project_id;

        let stopped = {
            let mut stopped = task.clone();
            stopped.status = TaskStatus::Backlog;
            stopped.reset_execution_state();
            stopped.updated_at = chrono::Utc::now();
            stopped
        };

        let staged: Vec<Task> = tasks_w
            .values()
            .filter(|t| t.project_id == project_id)
            .map(|t| {
                if t.id == task_id {
                    stopped.clone()
                } else {
                    t.clone()
                }
            })
            .collect();

        storage
            .save_project_tasks(project_id, &staged)
            .map_err(|e| {
                format!(
                    "the agent was stopped, but recording task {task_id} as stopped \
                     failed, so it is still saved as running and a restart would start \
                     it again: {e}"
                )
            })?;

        // Published only now, and as exactly the record that reached the
        // disk, so the board cannot show a stop the file does not have.
        tasks_w.insert(task_id, stopped);
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
            // Reported rather than discarded, as the command-side
            // `persist_project_tasks` already does. A board that disagrees
            // with the disk is recoverable while someone knows it happened.
            // Where the disagreement would be acted on rather than merely
            // displayed, publishing waits for the write instead: see
            // `settle_stopped_static`.
            if let Err(e) = storage.save_project_tasks(project_id, &project_tasks) {
                eprintln!("[executor] failed to persist tasks for project {project_id}: {e}");
            }
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

    /// A `TaskExecutor` over nothing but temporary directories.
    ///
    /// Deliberately synchronous and runtime-free, because
    /// [`polling_loop_can_be_built_with_no_ambient_tokio_runtime`] must be
    /// able to build one from a plain `#[test]`. The returned temp dirs must
    /// be kept alive for as long as the executor is used.
    fn test_executor() -> (Arc<TaskExecutor>, Vec<tempfile::TempDir>) {
        let (storage, storage_temp) = test_storage();
        let (registry, reg_temp) = test_registry();
        let paths_temp = tempfile::TempDir::new().expect("paths temp dir");

        let tasks: Tasks = Arc::new(RwLock::new(HashMap::new()));
        let executor = Arc::new(TaskExecutor::new(TaskExecutorConfig {
            tasks: tasks.clone(),
            queue_manager: Arc::new(RwLock::new(crate::queue::QueueManager::new(
                tasks,
                crate::config::queue::QueueConfig::default(),
            ))),
            executions: Arc::new(RwLock::new(HashMap::new())),
            logs: Arc::new(RwLock::new(HashMap::new())),
            projects: Arc::new(RwLock::new(HashMap::new())),
            repositories: Arc::new(RwLock::new(HashMap::new())),
            workspace_registry: Arc::new(RwLock::new(registry)),
            storage,
            worktree_manager: Arc::new(WorktreeManager::new(
                Arc::new(crate::config::paths::AppPaths::with_roots(
                    paths_temp.path().join("config"),
                    paths_temp.path().join("data"),
                    paths_temp.path().join("cache"),
                    paths_temp.path().join("runtime"),
                )),
                crate::config::paths::WorktreePlacement::Managed,
            )),
            events: crate::events::null_sink(),
        }));
        (executor, vec![storage_temp, reg_temp, paths_temp])
    }

    /// Regression guard for the PR #2 GUI startup panic.
    ///
    ///     thread 'main' panicked at src-tauri/src/queue/executor.rs:140:9:
    ///     there is no reactor running, must be called from the context of a
    ///     Tokio 1.x runtime
    ///
    /// The desktop app starts the poller from Tauri's `setup()` closure, which
    /// runs on the main thread before the event loop starts and has no Tokio
    /// runtime entered in thread-local scope. While this function performed the
    /// spawn itself with `tokio::spawn`, that call reached for
    /// `Handle::current()` and aborted the process before a window could open.
    /// The daemon never saw it because `slashitd` is `#[tokio::main]`.
    ///
    /// This is deliberately **not** a `#[tokio::test]`. That attribute enters a
    /// runtime on the test thread, which reproduces the daemon's environment
    /// and not the GUI's — the original bug would have passed such a test. The
    /// two properties proven here are exactly the ones the GUI needs, and the
    /// real `slashit-ui` launch is what covers the rest of `setup()`.
    #[test]
    fn polling_loop_can_be_built_with_no_ambient_tokio_runtime() {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "this test only proves anything while no runtime is entered on \
             this thread; something has made one ambient"
        );

        let (executor, _temps) = test_executor();
        let (tx, rx) = tokio::sync::watch::channel(false);

        // Property one: building the loop is inert. At `d1cf8d58` this line
        // read `executor.start_polling(Some(rx))` and panicked right here.
        let polling = executor.polling_loop(Some(rx));

        // Property two: the future still works when a runtime it was not
        // created inside picks it up later — which is precisely what
        // `tauri::async_runtime::spawn` does at the GUI call site, handing the
        // future to a global runtime built on another thread entirely. Built
        // after `polling` on purpose, so the future provably predates it.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");

        runtime.block_on(async move {
            let handle = tokio::spawn(polling);

            // `pr_check_counter` is bumped at the end of `check_and_execute`,
            // so seeing it above zero proves a full pass ran — the loop is
            // genuinely polling, not merely spawned. Without this the test
            // would still pass if the loop observed shutdown before its first
            // pass, a strictly weaker claim.
            while executor
                .pr_check_counter
                .load(std::sync::atomic::Ordering::Relaxed)
                == 0
            {
                tokio::task::yield_now().await;
            }

            tx.send(true).expect("the polling task holds the receiver");

            // Bounded well under the loop's own 3-second sleep, so resolving
            // in time proves it woke on `rx.changed()`.
            tokio::time::timeout(std::time::Duration::from_secs(1), handle)
                .await
                .expect("the loop must observe shutdown, not wait out its sleep")
                .expect("the loop must not panic");
        });
    }

    #[tokio::test]
    async fn polling_loop_stops_the_loop_once_it_observes_the_shutdown_signal() {
        // Regression guard for the daemon shutdown race: `daemon::run` awaits
        // exactly this `JoinHandle` before trusting `running_task_count()`,
        // on the reasoning that the loop's shutdown check only runs between
        // passes and therefore the handle resolving is proof no pass is still
        // executing.
        //
        // This does not reproduce the specific window CodeRabbit described —
        // a pass already past its shutdown check and partway through
        // promoting a task when the signal arrives — because that requires a
        // task actually in flight through real worktree creation, which has
        // no deterministic pause point without adding a synchronization hook
        // to `check_and_execute` itself, which would be a change to
        // production code well beyond this fix. What this test does prove
        // deterministically, with no sleep-based guessing: the loop is
        // allowed to run at least one real `check_and_execute` pass to
        // completion first (tracked via `pr_check_counter`, incremented at
        // the end of every pass), so this cannot degenerate into "shutdown
        // was already true before the loop was ever polled" — a strictly
        // weaker case an earlier version of this test collapsed into on a
        // single-threaded runtime, since sending on a `watch` channel before
        // the receiver's task is ever scheduled leaves nothing for the first
        // poll to observe but the already-updated value. It then proves the
        // loop wakes via `rx.changed()` rather than merely outlasting its own
        // timer, by bounding the wait well under the loop's 3-second
        // between-passes sleep.
        let (storage, _storage_temp) = test_storage();
        let (registry, _reg_temp) = test_registry();
        let paths_temp = tempfile::TempDir::new().expect("paths temp dir");

        let tasks: Tasks = Arc::new(RwLock::new(HashMap::new()));
        let executor = Arc::new(TaskExecutor::new(TaskExecutorConfig {
            tasks: tasks.clone(),
            queue_manager: Arc::new(RwLock::new(crate::queue::QueueManager::new(
                tasks.clone(),
                crate::config::queue::QueueConfig::default(),
            ))),
            executions: Arc::new(RwLock::new(HashMap::new())),
            logs: Arc::new(RwLock::new(HashMap::new())),
            projects: Arc::new(RwLock::new(HashMap::new())),
            repositories: Arc::new(RwLock::new(HashMap::new())),
            workspace_registry: Arc::new(RwLock::new(registry)),
            storage,
            worktree_manager: Arc::new(WorktreeManager::new(
                Arc::new(crate::config::paths::AppPaths::with_roots(
                    paths_temp.path().join("config"),
                    paths_temp.path().join("data"),
                    paths_temp.path().join("cache"),
                    paths_temp.path().join("runtime"),
                )),
                crate::config::paths::WorktreePlacement::Managed,
            )),
            events: crate::events::null_sink(),
        }));

        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(executor.polling_loop(Some(rx)));

        // Let at least one full pass complete before signalling shutdown.
        // `pr_check_counter` is incremented at the very end of
        // `check_and_execute`, so observing it above zero is proof a pass
        // ran to completion, not merely that the loop task was scheduled.
        while executor
            .pr_check_counter
            .load(std::sync::atomic::Ordering::Relaxed)
            == 0
        {
            tokio::task::yield_now().await;
        }

        tx.send(true).expect("receiver is held by the polling task");

        // Bounded well under the loop's 3-second between-passes sleep:
        // resolving inside this window is proof the loop woke via
        // `rx.changed()`, not that it happened to finish waiting anyway.
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("the poller must wake via `rx.changed()`, not wait out its own sleep")
            .expect("the poller task must not panic");
    }

    #[tokio::test]
    async fn check_and_execute_prunes_a_reviewing_handle_left_behind_by_a_panic() {
        // Mirrors the same backstop already proven for `running_handles`: a
        // panic inside a spawned review task skips its own cleanup code, so
        // nothing but a periodic sweep removes its entry. A leaked entry here
        // holds `running_task_count()` above zero forever, which is exactly
        // what daemon shutdown waits on — so this map needs the same pruning
        // `running_handles` already had, not a separate guarantee.
        let (storage, _storage_temp) = test_storage();
        let (registry, _reg_temp) = test_registry();
        let paths_temp = tempfile::TempDir::new().expect("paths temp dir");

        let tasks: Tasks = Arc::new(RwLock::new(HashMap::new()));
        let executor = TaskExecutor::new(TaskExecutorConfig {
            tasks: tasks.clone(),
            queue_manager: Arc::new(RwLock::new(crate::queue::QueueManager::new(
                tasks.clone(),
                crate::config::queue::QueueConfig::default(),
            ))),
            executions: Arc::new(RwLock::new(HashMap::new())),
            logs: Arc::new(RwLock::new(HashMap::new())),
            projects: Arc::new(RwLock::new(HashMap::new())),
            repositories: Arc::new(RwLock::new(HashMap::new())),
            workspace_registry: Arc::new(RwLock::new(registry)),
            storage,
            worktree_manager: Arc::new(WorktreeManager::new(
                Arc::new(crate::config::paths::AppPaths::with_roots(
                    paths_temp.path().join("config"),
                    paths_temp.path().join("data"),
                    paths_temp.path().join("cache"),
                    paths_temp.path().join("runtime"),
                )),
                crate::config::paths::WorktreePlacement::Managed,
            )),
            events: crate::events::null_sink(),
        });

        let task_id = Uuid::new_v4();
        let handle = tokio::spawn(async {
            panic!("simulated review-task panic, before its own cleanup runs");
        });
        // Give the spawned task a chance to actually run and panic before
        // asserting on it, rather than racing its own scheduling.
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
        executor
            .reviewing_handles
            .write()
            .await
            .insert(task_id, handle);

        executor.check_and_execute().await;

        assert!(
            executor.reviewing_handles.read().await.is_empty(),
            "a finished (including panicked) review handle must not survive a poll pass"
        );
        assert_eq!(
            executor.running_task_count().await,
            0,
            "a leaked reviewing_handles entry would hold this above zero forever, \
             which is what daemon shutdown waits on"
        );
    }

    /// A task in the state the poller starts work from, with a run behind it.
    ///
    /// The worktree and branch are the work the run had already produced;
    /// nothing about stopping may take them away.
    fn running_task(project_id: Uuid) -> Task {
        let mut task = create_test_task_full("running", project_id, TaskStatus::InProgress, 0);
        task.phase = TaskPhase::Coding;
        task.phase_progress = 5;
        task.overall_progress = 5;
        task.worktree_path = Some("/tmp/worktree-under-test".to_string());
        task.branch_name = Some("task-under-test".to_string());
        task
    }

    /// Register an execution the way [`TaskExecutor::spawn_task_execution`]
    /// does, over a future that ends only when it is cancelled.
    ///
    /// `cleaned_up` is set by that future *after* it observes the
    /// cancellation, which is the whole point: it stands for the `kill()` and
    /// the bookkeeping the real execution runs on its way out. An abort would
    /// drop the future at its await and never set it.
    async fn register_cancellable_execution(
        executor: &TaskExecutor,
        task_id: Uuid,
    ) -> Arc<std::sync::atomic::AtomicBool> {
        let cleaned_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let flag = cleaned_up.clone();
        let handle = tokio::spawn(async move {
            let _ = cancelled.changed().await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        executor
            .running_handles
            .write()
            .await
            .insert(task_id, RunningTask { handle, cancel });
        cleaned_up
    }

    #[tokio::test]
    async fn stopping_a_task_lets_the_execution_run_its_own_cleanup_before_returning() {
        // The defect this replaces: `stop_task` aborted the execution future,
        // which drops it at whatever await it is parked on. The future is what
        // owns the agent process, so the `kill()` after that await never ran
        // and the agent outlived the stop. Nothing about task state proves
        // that; only the cleanup having run does.
        let (executor, _temps) = test_executor();
        let task = running_task(Uuid::new_v4());
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);

        let cleaned_up = register_cancellable_execution(&executor, task_id).await;

        executor
            .stop_task(task_id)
            .await
            .expect("stop must succeed");

        assert!(
            cleaned_up.load(std::sync::atomic::Ordering::SeqCst),
            "stop_task returned before the execution finished ending itself, so a caller \
             cannot treat `Ok` as meaning the agent is gone"
        );
        assert!(
            executor.running_handles.read().await.is_empty(),
            "the stopped execution must not be left registered as running"
        );
    }

    #[tokio::test]
    async fn a_stopped_task_settles_where_the_queue_will_not_pick_it_up_again() {
        // `InProgress` + an idle phase is exactly what `is_pending` means by
        // "start this task", and it is what the old stop left behind, so the
        // very next poll pass ran the task again with nothing having asked
        // for it.
        let (executor, _temps) = test_executor();
        let task = running_task(Uuid::new_v4());
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);
        register_cancellable_execution(&executor, task_id).await;

        executor
            .stop_task(task_id)
            .await
            .expect("stop must succeed");

        let tasks = executor.tasks.read().await;
        let stopped = tasks.get(&task_id).expect("the task must still exist");
        assert_eq!(stopped.status, TaskStatus::Backlog);
        assert_eq!(stopped.phase, TaskPhase::Idle);
        assert_eq!(stopped.phase_progress, 0);
        assert_eq!(stopped.overall_progress, 0);
        assert_eq!(
            stopped.error_message, None,
            "nothing failed, so a stop must not claim a failure"
        );
        assert!(
            !TaskExecutor::is_pending(stopped),
            "a stopped task must not satisfy the predicate the poller starts work from"
        );
    }

    #[tokio::test]
    async fn stopping_a_task_keeps_the_work_it_had_already_produced() {
        // Stopping is not discarding, and the product has no operation that
        // discards. The branch is what a later run reattaches to.
        let (executor, _temps) = test_executor();
        let task = running_task(Uuid::new_v4());
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);
        register_cancellable_execution(&executor, task_id).await;

        executor
            .stop_task(task_id)
            .await
            .expect("stop must succeed");

        let tasks = executor.tasks.read().await;
        let stopped = tasks.get(&task_id).expect("the task must still exist");
        assert_eq!(
            stopped.worktree_path.as_deref(),
            Some("/tmp/worktree-under-test")
        );
        assert_eq!(stopped.branch_name.as_deref(), Some("task-under-test"));
    }

    #[tokio::test]
    async fn a_stopped_task_is_still_stopped_after_a_restart() {
        // What is in memory does not survive the application; what is on disk
        // is what the next launch believes. An unpersisted stop would leave
        // `in_progress` there, and hydration requeues exactly that.
        let (executor, _temps) = test_executor();
        let task = running_task(Uuid::new_v4());
        let (task_id, project_id) = (task.id, task.project_id);
        executor.tasks.write().await.insert(task_id, task);
        register_cancellable_execution(&executor, task_id).await;

        executor
            .stop_task(task_id)
            .await
            .expect("stop must succeed");

        let persisted = executor
            .storage
            .load_project_tasks(project_id)
            .expect("the stopped task must have been written");
        let record = persisted
            .iter()
            .find(|t| t.id == task_id)
            .expect("the stopped task must be in the file");
        assert_eq!(record.status, TaskStatus::Backlog);
        assert_eq!(record.phase, TaskPhase::Idle);
        assert_eq!(record.branch_name.as_deref(), Some("task-under-test"));
    }

    #[tokio::test]
    async fn a_stop_that_could_not_be_saved_is_reported_rather_than_returned_as_success() {
        // The failure this forbids is the quiet one. Memory is not what the
        // next launch reads, so a stop that only reached memory is a stop the
        // restart undoes: the file still says `in_progress`, and hydration
        // reads that as a run a crash interrupted and puts it back on the
        // queue. Answering `Ok` there tells the user the run is over and then
        // starts it again behind them.
        let (executor, _temps) = test_executor();
        let task = running_task(Uuid::new_v4());
        let (task_id, project_id) = (task.id, task.project_id);
        executor.tasks.write().await.insert(task_id, task);
        let cleaned_up = register_cancellable_execution(&executor, task_id).await;

        let blocker = block_task_persistence(&executor.storage);

        let result = executor.stop_task(task_id).await;

        let message = result.expect_err(
            "a stop whose outcome never reached the disk must not be reported as a stop",
        );
        assert!(
            message.contains("stopped"),
            "the message must say the agent was stopped, so the user knows what did \
             happen as well as what did not: {message}"
        );
        assert!(
            cleaned_up.load(std::sync::atomic::Ordering::SeqCst),
            "the agent must still have been ended -- the error is about recording the \
             stop, not about failing to perform it"
        );

        {
            // Nothing was published, so the two views agree: this is still the
            // `InProgress` work it was, which is what makes stopping it again
            // the recovery. Settling memory alone and reporting the error
            // would instead send the next stop down the already-settled path
            // and answer `Ok` for a file that still says `in_progress`.
            let tasks = executor.tasks.read().await;
            let held = tasks.get(&task_id).expect("the task must still exist");
            assert_eq!(held.status, TaskStatus::InProgress);
            assert_eq!(held.phase, TaskPhase::Coding);
            assert_eq!(held.phase_progress, 5);
        }

        // What the failed stop left behind is the crash-recoverable record, so
        // once the disk takes writes again that is what lands on it: exactly
        // the `InProgress` hydration reads as a run to requeue. Reporting `Ok`
        // above would have been the forbidden pairing -- success told to the
        // user, runnable work left on disk.
        std::fs::remove_file(&blocker).expect("unblock persistence");
        TaskExecutor::persist_task_static(&executor.tasks, &executor.storage, task_id).await;
        let residual = executor
            .storage
            .load_project_tasks(project_id)
            .expect("read the tasks back")
            .into_iter()
            .find(|t| t.id == task_id)
            .expect("the task must be in the file");
        assert_eq!(
            residual.status,
            TaskStatus::InProgress,
            "the state a failed stop leaves is the one a restart starts again"
        );

        // The same request, once the disk can take it, settles the task -- and
        // only now is the stop something a restart will honour.
        executor
            .stop_task(task_id)
            .await
            .expect("stopping again once persistence works must settle the task");

        let persisted = executor
            .storage
            .load_project_tasks(project_id)
            .expect("read the tasks back");
        let record = persisted
            .iter()
            .find(|t| t.id == task_id)
            .expect("the stopped task must be in the file");
        assert_eq!(record.status, TaskStatus::Backlog);
        assert_eq!(record.phase, TaskPhase::Idle);
        assert_eq!(record.branch_name.as_deref(), Some("task-under-test"));
    }

    #[tokio::test]
    async fn a_stop_that_arrives_after_the_run_finished_leaves_the_result_alone() {
        // The two can happen at once. If the run got there first it produced a
        // real result, and pulling the task back out of the review it reached
        // would discard it -- so the honest resolution is that the stop was
        // too late, not that the run never happened.
        let (executor, _temps) = test_executor();
        let mut task = running_task(Uuid::new_v4());
        task.status = TaskStatus::AiReview;
        task.phase = TaskPhase::QaReview;
        task.overall_progress = 80;
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);

        // A finished execution is what "the run got there first" looks like to
        // the executor: the future ran its own cleanup and recorded the result.
        let (cancel, _cancelled) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async {});
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
        executor
            .running_handles
            .write()
            .await
            .insert(task_id, RunningTask { handle, cancel });

        executor
            .stop_task(task_id)
            .await
            .expect("stop must succeed");

        let tasks = executor.tasks.read().await;
        let task = tasks.get(&task_id).expect("the task must still exist");
        assert_eq!(
            task.status,
            TaskStatus::AiReview,
            "a completed run must not be rewritten as a stopped one"
        );
        assert_eq!(task.phase, TaskPhase::QaReview);
    }

    #[tokio::test]
    async fn stopping_a_task_nothing_is_running_still_takes_it_off_the_queue() {
        // The handle can be gone without the task having settled -- the poll
        // pass prunes a panicked execution's entry, and a panic skips the
        // cleanup that would have recorded an outcome. Leaving the task
        // `in_progress` there means the queue starts it again, which is the
        // behaviour the user just asked to end.
        let (executor, _temps) = test_executor();
        let mut task = running_task(Uuid::new_v4());
        task.phase = TaskPhase::Idle;
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);

        executor
            .stop_task(task_id)
            .await
            .expect("stop must succeed");

        let tasks = executor.tasks.read().await;
        let stopped = tasks.get(&task_id).expect("the task must still exist");
        assert_eq!(stopped.status, TaskStatus::Backlog);
        assert!(!TaskExecutor::is_pending(stopped));
    }

    #[tokio::test]
    async fn a_stopped_task_can_be_started_again_on_the_work_it_kept() {
        // Stopping has to be reversible by an ordinary product action, or it
        // is a way of losing a task rather than of pausing one. Moving the
        // card back into a working column is that action, and it goes through
        // the same classifier every other column move does.
        let (executor, _temps) = test_executor();
        let task = running_task(Uuid::new_v4());
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);
        register_cancellable_execution(&executor, task_id).await;

        executor
            .stop_task(task_id)
            .await
            .expect("stop must succeed");

        let mut tasks = executor.tasks.write().await;
        let restarted = crate::commands::task::update_task_status_logic(
            &mut tasks,
            task_id,
            TaskStatus::InProgress,
        )
        .expect("the stopped task must still be there to move");

        assert!(
            TaskExecutor::is_pending(&restarted),
            "moving a stopped task back into a working column must make it runnable again"
        );
        assert_eq!(
            restarted.branch_name.as_deref(),
            Some("task-under-test"),
            "the run that follows reattaches to this branch, so it must have survived the stop"
        );
        assert_eq!(
            restarted.worktree_path.as_deref(),
            Some("/tmp/worktree-under-test")
        );
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
            vec![(eligible_id, "/tmp/eligible".to_string())],
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
            &wt_mgr, &tasks, &storage, task_id, &wt_path, &repo_path,
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
            .any(|(id, _)| *id == task_id),
            "the task must remain eligible for a later retry"
        );

        // Once persistence works again, the retry converges: `remove` on an
        // already-absent directory is still `Ok`, so the record is cleared.
        std::fs::remove_file(&blocker).expect("unblock persistence");

        TaskExecutor::attempt_worktree_cleanup(
            &wt_mgr, &tasks, &storage, task_id, &wt_path, &repo_path,
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
            &wt_mgr, &tasks, &storage, target_id, &wt_path, &repo_path,
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
            &wt_mgr, &tasks, &storage, task_id, &wt_path, &repo_path,
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
