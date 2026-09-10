use crate::agents::runner::{ClaudeRunner, ClaudeRunConfig, ClaudeEvent};
use crate::domain::{Task, TaskStatus, TaskPhase, AgentExecution, AgentStatus, AgentLogEntry, LogLevel, QaSignoff, QaStatus};
use crate::queue::prompt::{build_task_prompt, build_review_prompt, build_fix_prompt};
use crate::queue::QueueManager;
use crate::worktree::WorktreeManager;
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
    /// The per-task lifecycle lease, shared with the desktop commands and the
    /// IPC handlers.
    ///
    /// It replaces a `HashSet` of task ids this executor used to keep for the
    /// same purpose, which had two defects an owned guard does not: it was
    /// released by an explicit statement, so a panic inside a cleanup stranded
    /// the task forever, and it was private to the executor, so it never
    /// serialized against a card the user dragged at the same moment.
    lifecycle: Arc<crate::lifecycle::TaskLifecycleLocks>,
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
    pub lifecycle: Arc<crate::lifecycle::TaskLifecycleLocks>,
}

impl TaskExecutor {
    pub fn new(config: TaskExecutorConfig) -> Self {
        Self {
            tasks: config.tasks,
            queue_manager: config.queue_manager,
            executions: config.executions,
            running_handles: Arc::new(RwLock::new(HashMap::new())),
            reviewing_handles: Arc::new(RwLock::new(HashMap::new())),
            lifecycle: config.lifecycle,
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
        task.status == TaskStatus::InProgress
            && task.phase == TaskPhase::Idle
            // A cleanup this or an earlier process started and never recorded
            // the outcome of leaves the recorded checkout untrustworthy, and an
            // agent started against it would be working inside a directory a
            // removal may still be taking apart. Startup quarantines such a
            // task rather than adopting or re-removing it; this is the gate
            // that keeps it out of the queue until someone resolves it.
            && !task.cleanup_in_flight
    }

    /// Whether an agent is attached to this task right now.
    ///
    /// The handle maps are the live ownership fact, and they are the only one:
    /// a task's persisted status cannot answer this, because a crash leaves
    /// `InProgress` behind with no process under it.
    ///
    /// Both maps, because the question this answers is "would deleting this
    /// task's checkout take it away from something". A review run works in the
    /// same directory the execution does -- it reads the diff there, runs the
    /// fix agent there and commits there -- so a terminalization that only
    /// looked at `running_handles` would happily remove a checkout an AI review
    /// was halfway through.
    pub async fn is_task_running(&self, task_id: Uuid) -> bool {
        let running = self
            .running_handles
            .read()
            .await
            .get(&task_id)
            .is_some_and(|r| !r.handle.is_finished());
        running
            || self
                .reviewing_handles
                .read()
                .await
                .get(&task_id)
                .is_some_and(|h| !h.is_finished())
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
            // Declining is ordinary here: the task keeps its state and the next
            // pass, three seconds later, tries again.
            let _started = self.spawn_task_execution(task_id).await;
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
                    // A quarantined task cannot be finished automatically, so
                    // polling it is a `gh pr view` every thirty seconds that
                    // can only ever end in the same refusal. Its interrupted
                    // cleanup is already reported on the task itself.
                    .filter(|t| !t.cleanup_in_flight)
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

                            if state == "MERGED" {
                                self.complete_merged_task(task_id, number, state).await;
                            } else if !state.is_empty() {
                                let mut tasks_w = self.tasks.write().await;
                                if let Some(t) = tasks_w.get_mut(&task_id) {
                                    Self::record_pr_state(t, number, state);
                                    if state == "CLOSED" {
                                        t.error_message =
                                            Some("PR was closed without merge".to_string());
                                    }
                                    t.updated_at = chrono::Utc::now();
                                }
                                drop(tasks_w);
                                Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Write a PR's remote state onto the task's matching external ref.
    fn record_pr_state(task: &mut Task, number: u32, state: &str) {
        for r in &mut task.external_refs {
            if let crate::domain::task::ExternalRef::GithubPr { number: n, state: ref mut s, .. } = r {
                if *n == number {
                    *s = Some(state.to_string());
                }
            }
        }
    }

    /// Finish a task whose pull request GitHub now reports as merged.
    ///
    /// Ordering is the whole content of this function. The merged fact is not
    /// written before the lease is obtained, and that is deliberate: the poll
    /// above skips any ref already recorded `MERGED`, so persisting it while
    /// the task was busy would spend the one automatic completion this task
    /// ever gets on a pass that did nothing. Declining is therefore free -- no
    /// state written, no opportunity consumed, and the next ordinary poll finds
    /// the task exactly as eligible as before.
    ///
    /// The lease is taken here rather than left to [`crate::lifecycle::
    /// terminalize`] so that every refusal is classified while this still owns
    /// the task. Reconstructing what happened after the lease has been released
    /// is guessing about a moment that has passed, and the decisions below --
    /// whether anything durable is owed, and whether another pass may try again
    /// -- are exactly the decisions that must not be guesses.
    ///
    /// What the classification is for: a refusal that is a passing condition
    /// leaves nothing written, so an ordinary later poll retries it. A refusal
    /// that is a stable blocker is recorded durably, which both tells the user
    /// why and stops the polling, because asking GitHub the same question every
    /// thirty seconds forever cannot change a repository that does not resolve.
    /// Neither kind ever re-runs a destructive step on a timer: once the merge
    /// is durable, only an explicit lifecycle action finishes the task.
    async fn complete_merged_task(&self, task_id: Uuid, number: u32, state: &str) {
        // Declined, not failed. Nothing was asked and nothing is owed, so the
        // next pass finds the task exactly as eligible as this one did.
        let Some(_lease) = self.lifecycle.try_acquire(task_id).await else {
            return;
        };

        let record_merge = move |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&task_id) {
                Self::record_pr_state(t, number, state);
            }
        };
        // Only true of a task that is actually finished, so it is withheld from
        // a refusal along with the status itself.
        let mark_complete = move |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&task_id) {
                t.overall_progress = 100;
                t.phase = TaskPhase::Complete;
            }
        };

        let mut request = crate::lifecycle::TerminalizeRequest::new(TaskStatus::Done);
        request.record_first = Some(&record_merge);
        request.on_success = Some(&mark_complete);

        let outcome = crate::lifecycle::terminalize_leased(
            crate::lifecycle::TerminalizeCtx {
                tasks: &self.tasks,
                projects: &self.projects,
                repositories: &self.repositories,
                worktree_manager: &self.worktree_manager,
                storage: &self.storage,
                authority: &self.lifecycle,
                running: Some(self),
            },
            task_id,
            crate::lifecycle::Origin::Automatic,
            request,
        )
        .await;

        match outcome {
            Ok(_) => self.events.agent_event(AgentEvent::Completed {
                task_id: task_id.to_string(),
                success: true,
                message: Some("PR merged — task complete".to_string()),
            }),

            // Answered, and the merge is durable: `record_first` reached the
            // disk in the write that announced the cleanup, before anything
            // destructive ran. The poll above excludes a ref it has already
            // recorded as `MERGED`, so this is said once and no later pass
            // re-attempts the removal.
            Err(refusal @ crate::lifecycle::TerminalizeRefusal::CleanupRefused { .. }) => {
                self.events.agent_event(AgentEvent::Log {
                    task_id: task_id.to_string(),
                    level: LogLevel::Warn,
                    message: format!(
                        "The pull request is merged, but the task was not finished: {refusal}"
                    ),
                })
            }

            // Storage refused a write. Which write decides everything, and the
            // variant alone does not say: from the announcement it means
            // nothing reached the disk at all -- not even the merge -- so the
            // ref is still un-recorded and an ordinary later poll retries this,
            // correctly, because no durable latch exists to say otherwise. From
            // the final commit it means the merge and the in-flight stamp are
            // both durable, the poll's own filters exclude the task on either
            // count, and the next start reconciles it. Reported either way, and
            // no destructive step is scheduled by either.
            Err(refusal @ crate::lifecycle::TerminalizeRefusal::NotRecorded(_)) => {
                self.events.agent_event(AgentEvent::Log {
                    task_id: task_id.to_string(),
                    level: LogLevel::Warn,
                    message: format!(
                        "The pull request is merged, but the task was not finished: {refusal}"
                    ),
                })
            }

            // A stable blocker, and the one refusal that answers before
            // anything durable is written while never being able to resolve
            // itself. Polling it is a `gh pr view` every thirty seconds that
            // can only ever reach here again, so the merge and the reason are
            // recorded now: the ref becomes `MERGED`, which is what takes the
            // task out of the poll's selection, and the card says what the user
            // has to fix. The task stays non-terminal, its worktree and branch
            // are untouched, and no git command has run.
            Err(refusal @ crate::lifecycle::TerminalizeRefusal::RepositoryUnresolved(_)) => {
                let reason = refusal.to_string();
                let latch = move |staged: &mut HashMap<Uuid, Task>| {
                    if let Some(t) = staged.get_mut(&task_id) {
                        Self::record_pr_state(t, number, state);
                        t.error_message = Some(format!(
                            "The pull request is merged, but the task was not finished: {reason}"
                        ));
                    }
                };
                // A failure here writes nothing, which leaves the ref
                // un-recorded and the task eligible again -- the same safe
                // direction as the announcement failure above. It is said in
                // the same event rather than a separate line, because "this is
                // why the task did not finish" and "and that reason did not
                // reach the card either" are one thing the user needs to read
                // together.
                let stored =
                    crate::lifecycle::record(&self.tasks, &self.storage, task_id, &latch).await;
                let message = match stored {
                    Ok(()) => format!(
                        "The pull request is merged, but the task was not finished: {refusal}"
                    ),
                    Err(e) => format!(
                        "The pull request is merged, but the task was not finished: {refusal}. \
                         Recording that on the task failed as well, so this will be tried again: \
                         {e}"
                    ),
                };
                self.events.agent_event(AgentEvent::Log {
                    task_id: task_id.to_string(),
                    level: LogLevel::Warn,
                    message,
                })
            }

            // Passing conditions, and a task that is gone. An agent owns the
            // task, or an interrupted cleanup is quarantined, or the lease was
            // taken between the two lines above. Nothing was asked, nothing was
            // written, nothing is owed -- and saying so every thirty seconds
            // would be noise about a condition the task already reports for
            // itself. Cleanup is never forced under a running agent, and a
            // quarantine is never touched automatically; both simply become
            // eligible again once the condition that is holding them clears.
            Err(crate::lifecycle::TerminalizeRefusal::ExecutionActive)
            | Err(crate::lifecycle::TerminalizeRefusal::Quarantined(_))
            | Err(crate::lifecycle::TerminalizeRefusal::Busy(_))
            | Err(crate::lifecycle::TerminalizeRefusal::TaskNotFound) => {}
        }
    }

    /// Start an agent for `task_id`, or leave the task alone.
    ///
    /// Acquiring the task's worktree is an ownership change, so it happens
    /// under the task's lifecycle lease: while this holds it, no cleanup,
    /// terminalization or delete can be midway through removing the very
    /// checkout being handed to the agent. The lease is released as soon as the
    /// run is registered in `running_handles`, which is from then on the live
    /// ownership fact. Holding it for the agent's whole lifetime would make
    /// `stop_task` -- which needs the same lease to take ownership away --
    /// wait for the run it is trying to end.
    ///
    /// Declining the lease is not a failure: the task keeps whatever state it
    /// had and the next poll, three seconds later, tries again.
    /// Returns whether an execution was actually registered, so a caller that
    /// answers a person can say what happened rather than assume it worked.
    async fn spawn_task_execution(&self, task_id: Uuid) -> bool {
        let Some(_lease) = self.lifecycle.try_acquire(task_id).await else {
            return false; // another lifecycle operation owns this task right now
        };

        // Re-read under the lease. The pending set was sampled before waiting
        // for it, and a task that has since been stopped, finished or
        // quarantined must not be started off that stale reading.
        {
            let tasks = self.tasks.read().await;
            match tasks.get(&task_id) {
                Some(task) if Self::is_pending(task) => {}
                _ => return false,
            }
        }
        if self.is_task_running(task_id).await {
            return false; // one agent per task
        }

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
                return false;
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

        // Acquire the task's worktree, or give up on this attempt.
        //
        // There is deliberately no fallback. Every arm here used to fall
        // through to `repo_path` on failure and carry on, which meant a task
        // whose worktree could not be created ran its agent in the user's main
        // checkout: the prompt, `--add-dir`, the agent's own working directory
        // and `commit_changes` all read this one value, so the run would end by
        // rewriting the current change description or committing every
        // uncommitted file in that repository under a task title. A task
        // without its own worktree has nowhere to work, and saying so is the
        // only safe answer.
        let acquired = if existing_branch.is_some() {
            self.worktree_manager
                .reattach(&repo_path, &branch_name)
                .await
                .map(|info| (info, "Reattached worktree"))
        } else if let Some(parent_branch) = base_branch.as_deref() {
            match self
                .worktree_manager
                .create_stacked_branch(&repo_path, &branch_name, parent_branch)
                .await
            {
                Ok(info) => Ok((info, "Created stacked worktree")),
                Err(e) => {
                    // Stacking is an optimisation, so losing it is not fatal:
                    // an ordinary branch off the default base still gives the
                    // task a worktree of its own.
                    self.events.agent_event(AgentEvent::Log {
                        task_id: task_id.to_string(),
                        level: LogLevel::Warn,
                        message: format!(
                            "Stacked branch failed ({e}), falling back to normal create"
                        ),
                    });
                    self.worktree_manager
                        .create(&repo_path, &branch_name)
                        .await
                        .map(|info| (info, "Created worktree (fallback)"))
                }
            }
        } else {
            self.worktree_manager
                .create(&repo_path, &branch_name)
                .await
                .map(|info| (info, "Created worktree"))
        };

        let (info, what_happened) = match acquired {
            Ok(acquired) => acquired,
            Err(e) => {
                let message = format!(
                    "No worktree could be attached to this task ({e}), so it was not \
                     started. Running it in the repository itself would put the agent's \
                     changes in your working checkout."
                );
                self.events.agent_event(AgentEvent::Error {
                    task_id: task_id.to_string(),
                    message: message.clone(),
                });
                Self::set_task_error_static(&self.tasks, &self.storage, task_id, &message).await;
                return false;
            }
        };

        self.events.agent_event(AgentEvent::Log {
            task_id: task_id.to_string(),
            level: LogLevel::Info,
            message: format!("{}: {}", what_happened, info.path),
        });
        {
            let mut tasks_w = self.tasks.write().await;
            if let Some(t) = tasks_w.get_mut(&task_id) {
                t.worktree_path = Some(info.path.clone());
                t.branch_name = Some(info.branch.clone());
            }
        }
        Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
        let working_dir = info.path;

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
                // Deleted while its worktree was being attached.
                None => return false,
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
        true
    }

    pub async fn execute_task(&self, task_id: Uuid) -> Result<(), String> {
        {
            let tasks = self.tasks.read().await;
            let task = tasks.get(&task_id).ok_or("Task not found")?;
            if task.status != TaskStatus::InProgress && task.status != TaskStatus::Queue {
                return Err(format!("Task in {:?}, expected InProgress or Queue", task.status));
            }
            // See `is_pending`: the recorded checkout is untrustworthy until
            // the interrupted cleanup is resolved, and starting an agent in it
            // is exactly what the quarantine exists to prevent.
            if task.cleanup_in_flight {
                return Err(
                    "This task has a worktree cleanup that was interrupted and not yet \
                     resolved; it needs attention before it can run again"
                        .to_string(),
                );
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
        if !self.spawn_task_execution(task_id).await {
            // The task is left where it is, which is what the three-second
            // poll looks for, so this is a deferral rather than a loss. Saying
            // `Ok` would tell the caller an agent is running when none is.
            return Err(format!(
                "task {task_id} could not be started right now; it stays queued and the \
                 executor will pick it up on its next pass"
            ));
        }
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
        // Stopping is an ownership change, so it takes the task's lifecycle
        // lease like every other one. It cannot deadlock against the run it is
        // ending: `spawn_task_execution` releases the lease as soon as it has
        // registered the run, and the execution future itself never asks for
        // it -- everything it does on the way out (killing the process,
        // removing its `running_handles` entry, recording the execution) uses
        // its own locks. So the longest this can wait is the tail of another
        // lifecycle operation, never the lifetime of an agent.
        let _lease = self.lifecycle.acquire(task_id).await?;

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
    /// Persists before publishing, the same contract every durable write in
    /// [`crate::lifecycle`] holds, and for a sharper reason. What survives a failed write is the
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
        // The task's own worktree, or no review at all.
        //
        // This used to fall back to the repository path, and a review is not a
        // read-only operation: the fix agent runs in this directory and the
        // block below finishes by running `jj describe` in it. Against the
        // user's own checkout that rewrites their current change description
        // under a task title. A task with no worktree has nothing of its own to
        // review, and saying so is the only safe answer.
        let Some(working_dir) = ({
            let tasks_r = self.tasks.read().await;
            tasks_r.get(&task_id).and_then(|t| t.worktree_path.clone())
        }) else {
            let reason = "this task has no worktree of its own, so there is nothing to \
                          review and reviewing the repository itself would put the fix \
                          agent's changes in your working checkout";
            self.events.agent_event(AgentEvent::Log {
                task_id: task_id.to_string(),
                level: LogLevel::Warn,
                message: format!("AI review skipped: {reason}"),
            });
            let signoff = QaSignoff {
                status: QaStatus::Rejected,
                issues_found: vec![format!("AI review skipped: {reason}")],
                timestamp: chrono::Utc::now(),
                session_id: Uuid::new_v4(),
            };
            Self::transition_to_human_review(&self.tasks, &self.storage, task_id, Some(signoff))
                .await;
            return;
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

/// The queue is what knows whether an agent is attached to a task, so it is
/// what [`crate::lifecycle::terminalize`] asks before it deletes a checkout.
#[async_trait::async_trait]
impl crate::lifecycle::ExecutionOwnership for TaskExecutor {
    async fn is_task_running(&self, task_id: Uuid) -> bool {
        TaskExecutor::is_task_running(self, task_id).await
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

    // ===== automatic completion of a merged pull request =====

    /// A task the poll would select: non-terminal, with one open PR ref.
    async fn task_with_open_pr(executor: &TaskExecutor, worktree: Option<&str>) -> (Uuid, Uuid) {
        let project_id = Uuid::new_v4();
        let mut task = crate::test_helpers::create_test_task_full(
            "merged",
            project_id,
            TaskStatus::PrCreated,
            0,
        );
        task.worktree_path = worktree.map(|s| s.to_string());
        task.branch_name = Some("task-abcd1234".to_string());
        task.external_refs.push(crate::domain::task::ExternalRef::GithubPr {
            number: 7,
            repo: "owner/repo".to_string(),
            url: "https://example.invalid/pr/7".to_string(),
            state: None,
        });
        let id = task.id;
        executor.tasks.write().await.insert(id, task.clone());
        executor
            .storage
            .save_project_tasks(project_id, &[task])
            .expect("seed the board");
        (id, project_id)
    }

    fn recorded_pr_state(task: &Task) -> Option<String> {
        task.external_refs.iter().find_map(|r| match r {
            crate::domain::task::ExternalRef::GithubPr { state, .. } => state.clone(),
            _ => None,
        })
    }

    #[tokio::test]
    async fn a_merge_that_cannot_resolve_a_repository_is_recorded_once_instead_of_polled_forever() {
        // No project and no repository, which is what `RepositoryUnresolved`
        // actually is: a task that still names a checkout under a project that
        // resolves to nothing. The refusal answers before any git command, and
        // before this correction it wrote nothing at all -- so the poll picked
        // the same task up again thirty seconds later, and every thirty seconds
        // after that, forever.
        let (executor, _temps) = test_executor();
        let (id, project_id) = task_with_open_pr(&executor, Some("/nonexistent/checkout")).await;

        executor.complete_merged_task(id, 7, "MERGED").await;

        let after = executor.tasks.read().await.get(&id).cloned().expect("task");
        assert_eq!(
            recorded_pr_state(&after).as_deref(),
            Some("MERGED"),
            "the merge is a fact about GitHub and recording it is what takes this task out of \
             the poll's selection"
        );
        assert!(
            after.error_message.is_some(),
            "and the card has to say why a merged pull request did not finish the task"
        );
        assert_eq!(
            after.status,
            TaskStatus::PrCreated,
            "the task is not finished, so it may not be moved as though it were"
        );
        assert_eq!(
            after.worktree_path.as_deref(),
            Some("/nonexistent/checkout"),
            "and nothing may drop the only reference to a checkout that was never touched"
        );
        assert!(!after.cleanup_in_flight, "no cleanup was ever announced");

        let on_disk = executor
            .storage
            .load_project_tasks(project_id)
            .expect("the board must be readable")
            .into_iter()
            .find(|t| t.id == id)
            .expect("the task must be on disk");
        assert_eq!(
            recorded_pr_state(&on_disk).as_deref(),
            Some("MERGED"),
            "the file is what the next start reads, so a latch only in memory is no latch"
        );
    }

    #[tokio::test]
    async fn a_merge_found_while_the_task_is_busy_writes_nothing_and_stays_eligible() {
        let (executor, _temps) = test_executor();
        let (id, _project_id) = task_with_open_pr(&executor, None).await;

        // Someone else owns the task's lifecycle right now. Declining has to
        // cost nothing: writing `MERGED` here would spend the one automatic
        // completion this task ever gets on a pass that did nothing, because
        // the poll skips a ref it has already recorded as merged.
        let held = executor
            .lifecycle
            .try_acquire(id)
            .await
            .expect("the lease must be free to take");

        executor.complete_merged_task(id, 7, "MERGED").await;

        let after = executor.tasks.read().await.get(&id).cloned().expect("task");
        assert_eq!(
            recorded_pr_state(&after),
            None,
            "nothing was attempted, so nothing may have been written -- and an un-recorded ref \
             is exactly what leaves the task as eligible on the next pass as it was on this one"
        );
        assert_eq!(after.status, TaskStatus::PrCreated);
        assert!(!after.cleanup_in_flight);

        drop(held);
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
            lifecycle: Arc::new(crate::lifecycle::TaskLifecycleLocks::new()),
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
            lifecycle: Arc::new(crate::lifecycle::TaskLifecycleLocks::new()),
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
            lifecycle: Arc::new(crate::lifecycle::TaskLifecycleLocks::new()),
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

    /// A task whose worktree cannot be acquired is not started, and above all
    /// is not started somewhere else.
    ///
    /// Every one of the three acquisition arms used to fall through to the
    /// repository path on failure and carry on. That value is not just the
    /// agent's working directory: it also reaches the prompt, `--add-dir` and
    /// `commit_changes`, so the run ended by describing the user's current
    /// change or committing every uncommitted file in their checkout under a
    /// task title. There is no safe substitute for a task's own worktree, so
    /// there is no fallback.
    ///
    /// The repository here is a real directory that is not a git repository,
    /// which is a deterministic `git worktree add` failure needing no
    /// permissions, no timing and no shim.
    #[tokio::test]
    async fn a_task_whose_worktree_cannot_be_acquired_never_runs_in_the_repository_itself() {
        let (executor, temps) = test_executor();

        let not_a_repo = temps[0].path().join("not-a-repository");
        std::fs::create_dir_all(&not_a_repo).unwrap();
        let repo_path = not_a_repo.to_string_lossy().to_string();

        let project_id = Uuid::new_v4();
        let repository_id = Uuid::new_v4();
        executor.repositories.write().await.insert(
            repository_id,
            crate::domain::Repository {
                id: repository_id,
                local_path: repo_path.clone(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        let mut project = test_project(project_id, ProjectScope::Standalone);
        project.repository_id = Some(repository_id);
        executor.projects.write().await.insert(project_id, project);

        let mut task = create_test_task_full("no worktree possible", project_id, TaskStatus::InProgress, 0);
        task.phase = TaskPhase::Idle;
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);

        executor.spawn_task_execution(task_id).await;

        assert!(
            executor.running_handles.read().await.is_empty(),
            "no agent may be started for a task that has nowhere to work"
        );

        let after = executor.tasks.read().await.get(&task_id).cloned().unwrap();
        assert_eq!(after.status, TaskStatus::Error);
        assert_ne!(
            after.worktree_path.as_deref(),
            Some(repo_path.as_str()),
            "the repository is never recorded as the task's worktree"
        );
        assert!(
            after.error_message.as_deref().is_some_and(|m| m.contains("worktree")),
            "the reason has to name what went wrong: {:?}",
            after.error_message
        );

        assert!(
            std::fs::read_dir(&not_a_repo).unwrap().next().is_none(),
            "the repository directory must be exactly as it was found"
        );
    }

    /// An AI review has nowhere to run without the task's own worktree, and
    /// does not borrow the repository instead.
    ///
    /// A review is not a read-only pass over a diff: the fix agent runs in this
    /// directory and the run finishes with `jj describe` in it. Pointed at the
    /// user's checkout that rewrites their current change description under a
    /// task title, and the task never had a worktree of its own to review. The
    /// honest outcome is the one a person can act on -- send it to human review
    /// and say why.
    #[tokio::test]
    async fn a_review_with_no_worktree_is_skipped_rather_than_run_in_the_repository() {
        let (executor, temps) = test_executor();

        let repo = temps[0].path().join("repository");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("untouched.txt"), "the user's own checkout\n").unwrap();
        let repo_path = repo.to_string_lossy().to_string();

        let project_id = Uuid::new_v4();
        let repository_id = Uuid::new_v4();
        executor.repositories.write().await.insert(
            repository_id,
            crate::domain::Repository {
                id: repository_id,
                local_path: repo_path,
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        let mut project = test_project(project_id, ProjectScope::Standalone);
        project.repository_id = Some(repository_id);
        executor.projects.write().await.insert(project_id, project);

        let mut task =
            create_test_task_full("nothing to review", project_id, TaskStatus::AiReview, 0);
        task.phase = TaskPhase::QaReview;
        task.worktree_path = None;
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);

        executor.spawn_review(task_id).await;

        let after = executor.tasks.read().await.get(&task_id).cloned().unwrap();
        assert_eq!(
            after.status,
            TaskStatus::HumanReview,
            "a review that cannot run has to hand the task to a person"
        );
        let signoff = after.qa_signoff.expect("the skip must be recorded as a signoff");
        assert_eq!(signoff.status, QaStatus::Rejected);
        assert!(
            signoff.issues_found.iter().any(|i| i.contains("worktree")),
            "the reason has to name what was missing: {:?}",
            signoff.issues_found
        );

        assert_eq!(
            std::fs::read_dir(&repo).unwrap().count(),
            1,
            "nothing may have been created in the repository"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("untouched.txt")).unwrap(),
            "the user's own checkout\n"
        );
    }

    /// The lease a run was started under is not held for the run's lifetime,
    /// which is what stops `stop_task` waiting on the very execution it is
    /// trying to end.
    ///
    /// Registration in `running_handles` is the live ownership fact from the
    /// moment the run exists; the lease only serializes the handover. Holding
    /// it across the agent's lifetime instead would make every stop wait for
    /// the agent to finish on its own, which is precisely the opposite of what
    /// stopping means.
    #[tokio::test]
    async fn stopping_a_running_task_never_waits_on_a_lease_that_run_is_holding() {
        let (executor, _temps) = test_executor();
        let task = running_task(Uuid::new_v4());
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);
        let cleaned_up = register_cancellable_execution(&executor, task_id).await;

        assert!(
            executor.lifecycle.try_acquire(task_id).await.is_some(),
            "a registered run must leave the task's lifecycle lease free"
        );

        // Bounded so a design that did hold the lease fails here as a named
        // assertion instead of hanging the test binary. The bound is never
        // reached by a correct implementation: the stop takes an uncontended
        // lease and returns as soon as the execution's own cleanup has run.
        let stopped = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            executor.stop_task(task_id),
        )
        .await
        .expect("stop_task must not wait on the run it is ending");

        assert!(stopped.is_ok(), "{stopped:?}");
        assert!(
            cleaned_up.load(std::sync::atomic::Ordering::SeqCst),
            "a successful stop means the execution actually finished its own cleanup"
        );
        assert!(
            executor.lifecycle.try_acquire(task_id).await.is_some(),
            "and released the lease it took to do it"
        );
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
}
