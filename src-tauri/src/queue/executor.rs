use crate::agents::runner::{ClaudeRunner, ClaudeRunConfig, ClaudeEvent, ToolAccess};
use crate::domain::{Task, TaskStatus, TaskPhase, AgentExecution, AgentStatus, AgentLogEntry, LogLevel, QaSignoff, QaStatus};
use crate::queue::admission::{Admission, AdmissionPermit};
use crate::queue::prompt::{build_task_prompt, build_review_prompt, build_fix_prompt};
use crate::queue::QueueManager;
use crate::worktree::{WorktreeInfo, WorktreeManager};
use std::collections::HashMap;
use std::sync::Arc;
use crate::events::{EventSink, SharedEventSink};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// A finished agent run inside the review flow.
struct AgentRun {
    /// The CLI exited successfully and reported no error result.
    success: bool,
    /// Why the run did not succeed, when the runner said.
    failure: Option<String>,
    output: String,
}

/// What the AI reviewer concluded.
#[derive(Debug, PartialEq, Eq)]
enum ReviewVerdict {
    Approved,
    ChangesRequested,
    /// No verdict: the run failed, or it ended without one.
    Failed(String),
}

/// Whether the last line naming a verdict says `VERDICT: APPROVED`, with
/// Markdown emphasis and code marks on that line ignored
/// (`VERDICT: **APPROVED**`).
fn final_verdict_is_approved(output: &str) -> bool {
    output
        .lines()
        .rev()
        .find(|line| line.contains("VERDICT"))
        .is_some_and(|line| {
            let plain: String = line.chars().filter(|c| !matches!(c, '*' | '_' | '`')).collect();
            plain.contains("VERDICT: APPROVED")
        })
}

/// The verdict of a reviewer run. Only a successful run whose final verdict
/// line says `VERDICT: APPROVED` (and that requests no changes) approves; a
/// failed run, or one with no verdict at all, is [`ReviewVerdict::Failed`].
fn review_verdict(run: &AgentRun) -> ReviewVerdict {
    if !run.success {
        let reason = run.failure.clone().unwrap_or_else(|| "the reviewer run did not succeed".to_string());
        return ReviewVerdict::Failed(reason);
    }
    if run.output.contains("CHANGES_REQUESTED") {
        ReviewVerdict::ChangesRequested
    } else if final_verdict_is_approved(&run.output) {
        ReviewVerdict::Approved
    } else if run.output.trim().is_empty() {
        ReviewVerdict::Failed("the reviewer produced no output".to_string())
    } else {
        ReviewVerdict::Failed("the reviewer gave no verdict".to_string())
    }
}

/// A task's worktree, how it was obtained, and the commit it started from,
/// or why none could be attached.
type AcquiredWorktree = Result<(WorktreeInfo, &'static str, Option<String>), String>;

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

/// A review/fix flow the executor can still reach, on exactly the same
/// footing as [`RunningTask`] and for the same reason: `reviewing_handles`
/// used to store a bare `JoinHandle<()>`, which nothing could stop -- an
/// abort would drop the future at whatever `ClaudeRunner` await it was
/// parked on and leave that agent process running with nothing pointing at
/// it, exactly as [`RunningTask`]'s doc explains for execution. `cancel`
/// asks the owning future to end instead, and every subprocess it owns
/// (reviewer agent, fix agent, the CodeRabbit call) is killed by the code
/// that already has it in hand before the future returns -- never by an
/// outer `select!` racing the whole future, which is exactly the shape a
/// process could be dropped-but-not-killed under if a cancellation won
/// between a child spawning and this struct's own bookkeeping catching up.
struct ReviewOwner {
    handle: JoinHandle<()>,
    cancel: tokio::sync::watch::Sender<bool>,
}

/// A live PR-helper Claude invocation (`commands::pr::run_claude_pr_helper`)
/// the executor knows about, on the same task-exclusivity/capacity footing as
/// [`RunningTask`]/[`ReviewOwner`] but shaped differently: the future that
/// actually owns the subprocess runs inside a Tauri command handler this
/// module never spawns, so there is no [`JoinHandle`] here to join. `done`
/// is the substitute -- the [`PrHelperLease`] that registered this entry
/// signals it, unconditionally, the moment that command handler's call into
/// [`TaskExecutor::run_claude_pr_helper`]-adjacent code returns by any path
/// (success, error, or cancellation), so ending this owner can still block
/// on real completion instead of merely on having asked.
///
/// The same map also holds a PR *side-effect* reservation (see
/// [`TaskExecutor::begin_pr_side_effect_under_lease`]): a PR-creation flow
/// that runs no agent but rewrites, pushes and opens a pull request for the
/// task's branch. It is ended and waited on exactly like a helper; `kind`
/// only decides what beginning a new one refuses.
struct PrHelperOwner {
    cancel: tokio::sync::watch::Sender<bool>,
    done: tokio::sync::watch::Receiver<bool>,
    kind: PrOwnerKind,
}

/// Which PR flow a [`PrHelperOwner`] entry stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrOwnerKind {
    /// A PR-helper Claude invocation, holding an admission permit.
    Helper,
    /// A PR side-effect flow (branch rewrite, push, `gh pr create`, the
    /// durable link), holding no admission permit because it runs no agent.
    SideEffect,
}

/// Proof that a PR-helper Claude invocation for `task_id` may run: capacity
/// was drawn from the same [`Admission`] gate execution and AI review/fix
/// share, and no other execution/review/PR-helper flow currently owns this
/// task. RAII: dropping this (on every exit path -- success, `?`-propagated
/// error, or panic unwind) is the one place the registration is retired,
/// which is also what makes it safe to await bounded completion of *without*
/// a `JoinHandle`: whichever thread drops the last `PrHelperLease` for a task
/// is the one whose drop glue flips `done`, unblocking anyone waiting in
/// [`TaskExecutor::end_pr_helper_owner_under_lease`].
///
/// Held by the caller (`commands::pr`) across the whole subprocess lifetime
/// -- start, race against cancellation, kill-if-cancelled, reap -- exactly
/// the same "permit dropped only once the owning flow actually finishes"
/// contract [`AdmissionPermit`] already documents for execution and review.
///
/// A PR side-effect reservation is the same type without a permit: it proves
/// the task is owned by that PR-creation flow, from before its first side
/// effect until its durable link, and is retired on drop the same way.
pub struct PrHelperLease {
    task_id: Uuid,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    done_tx: Option<tokio::sync::watch::Sender<bool>>,
    _permit: Option<AdmissionPermit>,
    handles: Arc<std::sync::Mutex<HashMap<Uuid, PrHelperOwner>>>,
}

impl PrHelperLease {
    pub fn task_id(&self) -> Uuid {
        self.task_id
    }

    /// A receiver a caller can race a subprocess `wait()` against, exactly
    /// the way [`TaskExecutor::run_cancellable_agent`] races execution/
    /// review's own `ClaudeRunner`. Cloned, not moved, because the lease
    /// itself must still observe cancellation after handing one out (to
    /// decide whether a result it is about to persist is stale -- see
    /// [`Self::is_cancelled`]).
    pub fn cancel_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.cancel_rx.clone()
    }

    /// Whether a lifecycle transition has already asked this flow to end.
    /// A caller that is about to persist a result (a PR-review plan, a
    /// discuss/fix outcome) checks this *after* the subprocess finishes and
    /// before writing, so a helper that raced a cancellation and lost cannot
    /// still land a write the cancelling transition never saw.
    pub fn is_cancelled(&self) -> bool {
        *self.cancel_rx.borrow()
    }
}

impl Drop for PrHelperLease {
    fn drop(&mut self) {
        // Synchronous by construction (`std::sync::Mutex`, not the tokio
        // `RwLock` the other two handle maps use) for the same reason
        // `AdmissionPermit`'s own release path is: `Drop` cannot `.await`,
        // and this must run on every exit path, including a panic unwinding
        // through it. The critical section is one hashmap removal with no
        // `.await` inside it, so a blocking lock here never stalls the
        // runtime.
        self.handles.lock().unwrap().remove(&self.task_id);
        if let Some(done_tx) = self.done_tx.take() {
            let _ = done_tx.send(true);
        }
        // `self.permit` (an `Option<AdmissionPermit>`) is dropped along with
        // the rest of `self` right after this method returns, which is what
        // actually returns capacity -- never earlier, and never by this
        // method explicitly, so it is dropped in the same place for every
        // exit path rather than only the ones that remember to do it.
    }
}

/// Why [`TaskExecutor::try_begin_pr_helper`] declined to admit a new PR
/// helper. Every variant is a true, actionable refusal -- "ask again
/// shortly" -- not a bug: capacity and same-task exclusivity are both meant
/// to say no sometimes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrHelperRefusal {
    /// Another lifecycle operation currently owns this task's transition
    /// lease. The same brief contention `spawn_review`'s own `try_acquire`
    /// declines on, not a real conflict with an active agent.
    LifecycleContended,
    /// An execution, an AI review/fix, or another PR helper already owns
    /// this task. The product model is one active agent-owning flow per
    /// task; see the module doc.
    TaskAlreadyOwned,
    /// No free slot in the shared [`Admission`] gate right now.
    NoCapacity,
}

impl std::fmt::Display for PrHelperRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LifecycleContended => write!(
                f,
                "this task is busy with another change right now; try again shortly"
            ),
            Self::TaskAlreadyOwned => write!(
                f,
                "this task already has an active agent, review, or PR-helper run; \
                 wait for it to finish before starting another"
            ),
            Self::NoCapacity => write!(
                f,
                "no agent capacity is available right now; try again shortly"
            ),
        }
    }
}

/// How long [`TaskExecutor::stop_task`] waits for a cancelled execution's
/// future to finish joining before giving up on this attempt.
///
/// The future itself is not what can run long on the direct process: `cancel`
/// asks it to stop waiting on `runner.wait()` and fall straight to
/// `runner.kill()`, and `SIGKILL` cannot be blocked by the process it targets.
/// What is not bounded by that is a process wedged in an uninterruptible
/// syscall (blocked disk/NFS I/O), which no signal can shorten -- and the
/// caller is holding the task's lifecycle lease for every moment of this
/// wait, so an unbounded one hangs a Stop with no way out. Same order of
/// magnitude as [`crate::lifecycle::ACQUIRE_TIMEOUT`], and the same shape of
/// answer: a timeout here is not a failed stop, it is nothing attempted yet.
const AGENT_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
    reviewing_handles: Arc<RwLock<HashMap<Uuid, ReviewOwner>>>,
    /// PR-helper Claude invocations (`commands::pr::run_claude_pr_helper`)
    /// currently claiming a task. See [`PrHelperOwner`]/[`PrHelperLease`] for
    /// why this is a blocking `std::sync::Mutex` rather than the `tokio::
    /// sync::RwLock` the other two handle maps use.
    pr_helper_handles: Arc<std::sync::Mutex<HashMap<Uuid, PrHelperOwner>>>,
    /// The one admission gate ordinary execution and AI review/fix share.
    ///
    /// See [`crate::queue::admission`] for why this is a semaphore-backed
    /// permit rather than a count derived from the handle maps after the
    /// fact.
    admission: Admission,
    /// Capacity reserved for a task at `Queue -> InProgress` promotion time,
    /// held until that task's execution actually starts and takes ownership
    /// of it (see [`Self::spawn_task_execution`]).
    ///
    /// Reserve-then-promote is what stops auto-promotion from publishing more
    /// `InProgress` work than capacity can actually start once AI review also
    /// draws on [`Self::admission`]: the reservation is taken *before* the
    /// durable promotion write, so a promoted task is provably backed by real
    /// capacity the moment the board can show it, not merely hoped to be.
    reserved_permits: Arc<RwLock<HashMap<Uuid, AdmissionPermit>>>,
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
        // Not async, so this cannot `.await` the lock -- but nothing else can
        // hold it yet either: `config.queue_manager` is freshly constructed
        // by every caller (see `lib.rs`/`daemon.rs`) and handed straight
        // here, so an uncontended `try_read` always succeeds. `reconcile`
        // (called at the top of every `check_and_execute` pass, and before
        // every manual `execute_task`) is what keeps this current from then
        // on; this is only ever a starting point.
        let initial_limit = config
            .queue_manager
            .try_read()
            .map(|mgr| mgr.config().parallel_task_limit as usize)
            .unwrap_or(2);
        Self {
            tasks: config.tasks,
            queue_manager: config.queue_manager,
            executions: config.executions,
            running_handles: Arc::new(RwLock::new(HashMap::new())),
            reviewing_handles: Arc::new(RwLock::new(HashMap::new())),
            pr_helper_handles: Arc::new(std::sync::Mutex::new(HashMap::new())),
            admission: Admission::new(initial_limit),
            reserved_permits: Arc::new(RwLock::new(HashMap::new())),
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
        self.running_handles.read().await.len()
            + self.reviewing_handles.read().await.len()
            + self.pr_helper_handles.lock().unwrap().len()
    }

    /// Bring [`Self::admission`]'s capacity in line with the current runtime
    /// `parallel_task_limit`.
    ///
    /// Called before every place that acquires a permit -- the top of every
    /// poll pass, and every manual `execute_task` -- rather than once at
    /// startup, so a config change the user makes mid-run is respected by the
    /// very next admission decision instead of only the next restart.
    pub(crate) async fn reconcile_admission(&self) {
        let limit = self.queue_manager.read().await.config().parallel_task_limit as usize;
        self.admission.reconcile(limit).await;
    }

    /// Whether the poller should start this task on this pass.
    ///
    /// Delegates to [`Task::is_ready_to_execute`] rather than keeping its own
    /// copy of the condition, so this and the regression tests that pin the
    /// same contract cannot drift the way `is_ready_to_execute`'s own doc
    /// records one of them already had.
    ///
    /// The consequence is worth naming where the rule is used: anything that
    /// writes `InProgress` + an idle phase is asking for the task to be
    /// executed, whatever it meant to say. That is why
    /// [`stop_task`](Self::stop_task) does not leave a stopped task here.
    fn is_pending(task: &Task) -> bool {
        task.is_ready_to_execute()
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
                .is_some_and(|r| !r.handle.is_finished())
            // A PR helper -- read-only or edit-capable alike -- reads (and an
            // edit-capable one writes) the same checkout an execution or
            // review does, so the same "would deleting/reattaching this
            // checkout take it away from something" question applies. Not
            // narrowed to `can_edit`: a read-only helper's checkout still
            // disappears out from under it exactly the same way, and
            // terminalize's whole contract is "refuse if anything is
            // attached", not "refuse only if that thing writes". The same
            // map holds a PR side-effect reservation, which is rewriting,
            // pushing or opening a pull request for this task's branch, and
            // counts for the same reason.
            || self.pr_helper_handles.lock().unwrap().contains_key(&task_id)
    }

    async fn check_and_execute(&self) {
        // One authoritative capacity reading for this whole pass. Every
        // `try_acquire` below -- review admission, promotion reservation,
        // execution admission -- draws on this same reconciled gate, which is
        // what makes "coding + AI review/fix share one limit" a fact about
        // this pass rather than three independent guesses that happened to
        // agree.
        self.reconcile_admission().await;

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
        self.reviewing_handles
            .write()
            .await
            .retain(|_, r| !r.handle.is_finished());

        // Review-ready work is admitted *before* any new coding execution
        // this pass. Otherwise a queue that never empties keeps handing every
        // freed slot to the next coding task, and a task that finished coding
        // and is waiting for review starves behind it forever — completed
        // work piling up with nothing reaching `HumanReview`. `spawn_review`
        // declines gracefully (leaving the task in `AiReview`, retried next
        // pass) if admission has nothing left, so this loop never needs its
        // own capacity bookkeeping.
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

        // Auto-promote tasks from Queue → InProgress when capacity is available.
        //
        // Reserve-then-promote: a permit is taken from the same shared gate
        // *before* the durable promotion write, and transferred (via
        // `reserved_permits`) to whichever execution actually starts the
        // task. A promotion that is not backed by a reservation is exactly
        // how `InProgress` cards used to pile up with nothing running behind
        // them once review started drawing on the same capacity as
        // execution — `select_promotable`'s own `parallel_task_limit` check
        // only ever counted `TaskStatus::InProgress`, which is blind to a
        // review holding a slot.
        //
        // Durably, for the promotion write itself: a promotion this loop
        // makes has to survive a restart just as surely as one a person
        // triggers through `commands::queue`, or an auto-promoted task that
        // crashed before its next explicit save would come back `Queue` on
        // disk while every other part of the running process still believed
        // it was `InProgress`. See `lifecycle::commit_selected`.
        let auto_promote = self.queue_manager.read().await.config().auto_promote;
        if auto_promote {
            loop {
                let Some(permit) = self.admission.try_acquire() else {
                    break; // no capacity left to reserve; try again next pass
                };

                let manager = self.queue_manager.read().await;
                let select = |tasks: &HashMap<Uuid, Task>| manager.select_promotable(tasks);
                let amend = |staged: &mut HashMap<Uuid, Task>, task_id: Uuid| {
                    if let Some(task) = staged.get_mut(&task_id) {
                        QueueManager::apply_promotion(task);
                    }
                };
                let promoted = crate::lifecycle::commit_selected(
                    &self.tasks,
                    &self.storage,
                    select,
                    amend,
                )
                .await;
                drop(manager);

                match promoted {
                    Ok(Some(task)) => {
                        // Transferred, not dropped: the reservation now
                        // belongs to this task until its execution starts
                        // and takes it over (`spawn_task_execution`) or the
                        // task is pruned from `pending` below without ever
                        // starting.
                        self.reserved_permits.write().await.insert(task.id, permit);
                        self.events.agent_event(AgentEvent::Log {
                            task_id: task.id.to_string(),
                            level: LogLevel::Info,
                            message: "Auto-promoted from queue".to_string(),
                        });
                    }
                    // Nothing eligible: the reservation is rescinded simply
                    // by letting `permit` drop here, cleanly returning the
                    // capacity nothing used.
                    Ok(None) => break,
                    Err(e) => {
                        // Nothing was promoted: `commit_selected` never
                        // publishes to the shared map unless the disk write
                        // it depends on already succeeded, so `permit`
                        // dropping here is the same clean rescission as the
                        // `Ok(None)` arm. Stopping here rather than retrying
                        // immediately avoids spinning against a durably
                        // broken write on every poll tick; the next tick
                        // tries again from the same truthful state.
                        eprintln!("[executor] auto-promotion did not persist: {e}");
                        break;
                    }
                }
            }
        }

        // Find InProgress tasks that haven't started execution yet.
        let pending: Vec<Uuid> = {
            let tasks = self.tasks.read().await;
            tasks.values()
                .filter(|t| Self::is_pending(t))
                .map(|t| t.id)
                .collect()
        };

        // Any reservation whose task is no longer pending (deleted, moved
        // again, or otherwise never going to start) is returned here rather
        // than held forever: dropping the removed permit is what gives its
        // capacity back.
        {
            let pending_set: std::collections::HashSet<Uuid> = pending.iter().copied().collect();
            self.reserved_permits
                .write()
                .await
                .retain(|task_id, _| pending_set.contains(task_id));
        }

        for task_id in pending {
            // A reservation taken at promotion time is transferred straight
            // in; anything else (a task that was already `InProgress` before
            // this pass — reattached via drag, or a retry) draws a fresh
            // permit inside `spawn_task_execution` itself. Declining is
            // ordinary either way: the task keeps its state and the next
            // pass, three seconds later, tries again.
            let reserved = self.reserved_permits.write().await.remove(&task_id);
            let _started = self.spawn_task_execution(task_id, reserved).await;
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
                                self.record_pr_poll_state(task_id, number, state).await;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Record a non-`MERGED` pull request state observed by the poll above.
    ///
    /// `CLOSED` is exactly what removes this ref from the poll's own filter
    /// above (`state != Some("CLOSED")`), so it must never be visible in
    /// memory before it is durable: a lost write here would otherwise be a
    /// permanent, silent stop to polling a PR that is not actually recorded
    /// as closed anywhere the next start can see -- see issue #8. Any other
    /// state (`OPEN`, ...) is not selector-terminal and would self-heal on
    /// the next poll regardless; it is persisted the same way here only
    /// because it shares this call site, not because it shares the hazard.
    async fn record_pr_poll_state(&self, task_id: Uuid, number: u32, state: &str) {
        let state_owned = state.to_string();
        let amend = move |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&task_id) {
                Self::record_pr_state(t, number, &state_owned);
                if state_owned == "CLOSED" {
                    t.error_message = Some("PR was closed without merge".to_string());
                }
            }
        };
        if let Err(e) =
            crate::lifecycle::record(&self.tasks, &self.storage, task_id, &amend).await
        {
            self.events.agent_event(AgentEvent::Log {
                task_id: task_id.to_string(),
                level: LogLevel::Warn,
                message: format!(
                    "Observed pull request state {state:?} for task {task_id}, but recording \
                     it failed, so it was not applied and will be retried: {e}"
                ),
            });
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

    /// Find or create the worktree a starting task runs in.
    ///
    /// Returns the branch the task already had, if any, alongside the
    /// outcome: the worktree, what was done to get it, and the commit it
    /// started from when that is known. Nothing here starts an agent.
    /// [`Self::spawn_task_execution`] calls it under the task's lifecycle
    /// lease.
    async fn acquire_task_worktree(
        &self,
        task_id: Uuid,
        repo_path: &str,
    ) -> (Option<String>, AcquiredWorktree) {
        // Check if task already has a branch (re-queue after completion)
        let existing_branch = {
            let tasks_r = self.tasks.read().await;
            tasks_r.get(&task_id).and_then(|t| t.branch_name.clone())
        };

        let branch_name = existing_branch
            .clone()
            .unwrap_or_else(|| WorktreeManager::branch_for_task(task_id));

        // Check dependencies for stacked branching (only for new branches).
        // Stack when the dependency has a branch that hasn't been merged to main yet.
        // If merged, base on main normally (the code is already there).
        let dependency = if existing_branch.is_none() {
            let tasks_r = self.tasks.read().await;
            tasks_r
                .get(&task_id)
                .and_then(|t| t.dependencies.first())
                .and_then(|dep_id| tasks_r.get(dep_id))
                .and_then(|dep_task| {
                    // Dependency must have a branch
                    let branch = dep_task.branch_name.clone()?;
                    let is_done = dep_task.status == TaskStatus::Done;
                    let has_pr = dep_task.external_refs.iter().any(|r| r.is_pr());
                    Some((branch, is_done, has_pr))
                })
        } else {
            None
        };
        let base_branch = match dependency {
            None => None,
            // Done with no pull request: its work reached main without one.
            Some((_, true, false)) => None,
            // Done, and its branch is gone: the pull request was merged and
            // the branch deleted, so its work is on the default base too.
            // Only a definite answer from git counts. An invalid name, or a
            // git that could not be asked, still goes to the stacked path,
            // which refuses it and says why.
            Some((branch, true, true))
                if WorktreeManager::local_branch_exists(repo_path, &branch).await == Ok(false) =>
            {
                self.events.agent_event(AgentEvent::Log {
                    task_id: task_id.to_string(),
                    level: LogLevel::Info,
                    message: format!(
                        "The dependency is done and its branch {branch} no longer exists \
                         locally, so its work is taken to be delivered; starting from the \
                         default base instead of stacking on it"
                    ),
                });
                None
            }
            Some((branch, _, _)) => Some(branch),
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
        // A freshly-created worktree's `base_commit` is captured here, once,
        // by reading `HEAD` back out of the new worktree itself right after
        // `git worktree add` creates it -- not by re-resolving the parent
        // branch name (or `repo_path`'s `HEAD`) in a separate call
        // afterward. `git worktree add -b <branch> [<start-point>]` points
        // the new worktree's `HEAD` at exactly the commit it forked from,
        // with no commits of its own yet, so this is race-free: nothing else
        // can move the *new* worktree's `HEAD` before this line runs,
        // whereas re-querying the parent branch's (or `main`'s) ref after
        // the fact could observe it having moved in the meantime -- exactly
        // the kind of attribution drift this field exists to prevent.
        // `None` on reattach: retry must keep comparing against the original
        // starting point, not wherever the branch has moved to since.
        let acquired = if existing_branch.is_some() {
            self.worktree_manager
                .reattach(repo_path, &branch_name)
                .await
                .map(|info| (info, "Reattached worktree", None))
        } else if let Some(parent_branch) = base_branch.as_deref() {
            // A task stacked on a dependency is not started from anywhere
            // else. It used to fall back to an ordinary branch off the
            // default base with only a warning in the log, which started the
            // agent without the dependency's work it was queued to build on,
            // and nothing afterwards recorded that it was no longer stacked.
            // Nothing is created before the dependency's branch has been
            // checked, and the git path deletes a branch it created but
            // could not attach a worktree to. What an unfinished attempt can
            // still leave (a branch a failed checkout hook left checked out,
            // or one created by a start that died before the task recorded
            // it) is picked up by the next start while it still holds the
            // dependency's work, so an earlier attempt does not block a
            // retry. A branch of the task's name that does not hold it is
            // refused and left alone.
            match self
                .worktree_manager
                .create_stacked_branch(repo_path, &branch_name, parent_branch)
                .await
            {
                // The diff starts at the dependency commit the stack was
                // verified against. Reading `HEAD` instead would be the same
                // commit for a new branch, but on a resumed one it already
                // includes the task's own commits, which would drop out of
                // its diff.
                Ok(stacked) => {
                    let what_happened = if stacked.resumed {
                        "Resumed stacked worktree"
                    } else {
                        "Created stacked worktree"
                    };
                    Ok((stacked.info, what_happened, Some(stacked.dependency_tip)))
                }
                Err(e) => Err(format!(
                    "it depends on the work on branch {parent_branch}, and stacking on that \
                     branch failed: {e}"
                )),
            }
        } else {
            match self.worktree_manager.create(repo_path, &branch_name).await {
                Ok(info) => {
                    let base_commit = Self::resolve_commit(&info.path, "HEAD").await;
                    Ok((info, "Created worktree", base_commit))
                }
                Err(e) => Err(e),
            }
        };
        (existing_branch, acquired)
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
    ///
    /// `external_permit` is a reservation already taken by the caller --
    /// `check_and_execute`'s reserve-then-promote loop, transferring a
    /// permit straight from promotion into the execution it was reserved
    /// for. `None` means no such reservation exists (a manual `execute_task`
    /// call, or a task that was already `InProgress` before this poll pass),
    /// in which case a fresh permit is drawn from [`Self::admission`] here,
    /// so every path that can start an agent -- the poller and a direct
    /// command alike -- goes through the same gate. Returns whether an
    /// execution was actually registered, so a caller that answers a person
    /// can say what happened rather than assume it worked.
    async fn spawn_task_execution(&self, task_id: Uuid, external_permit: Option<AdmissionPermit>) -> bool {
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

        // Capacity, before any of the worktree/prompt work below runs: a
        // reservation already made for this exact task is honored outright;
        // otherwise this call draws its own fresh permit, and declines --
        // same as every other refusal in this function -- if none is free.
        // Dropping an unused `external_permit` on any decline path below
        // returns its capacity immediately; nothing here can lose it.
        let permit = match external_permit {
            Some(p) => p,
            None => match self.admission.try_acquire() {
                Some(p) => p,
                None => return false, // no capacity right now; the next pass tries again
            },
        };

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
                Self::set_task_error_static(&self.tasks, &self.storage, &self.events, task_id, &e).await;
                return false;
            }
        };

        let (existing_branch, acquired) = self.acquire_task_worktree(task_id, &repo_path).await;

        let (info, what_happened, resolved_base_commit) = match acquired {
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
                Self::set_task_error_static(&self.tasks, &self.storage, &self.events, task_id, &message).await;
                return false;
            }
        };

        self.events.agent_event(AgentEvent::Log {
            task_id: task_id.to_string(),
            level: LogLevel::Info,
            message: format!("{}: {}", what_happened, info.path),
        });
        if existing_branch.is_none() && resolved_base_commit.is_none() {
            self.events.agent_event(AgentEvent::Log {
                task_id: task_id.to_string(),
                level: LogLevel::Warn,
                message: "Could not resolve a starting commit for this task's worktree; \
                          its diff boundary will be reported as unknown rather than guessed"
                    .to_string(),
            });
        }
        {
            let mut tasks_w = self.tasks.write().await;
            if let Some(t) = tasks_w.get_mut(&task_id) {
                t.worktree_path = Some(info.path.clone());
                t.branch_name = Some(info.branch.clone());
                if let Some(base_commit) = resolved_base_commit {
                    t.base_commit = Some(base_commit);
                }
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
            // Held for the whole life of this future, not just the
            // synchronous call that started it: capacity is returned when
            // `_permit` drops, which is exactly when this future -- the
            // owner of the real agent process -- returns, on every path
            // including the early-return failure arms below. Removing this
            // task's `running_handles` entry does not by itself free
            // anything; see `crate::queue::admission`.
            let _permit = permit;

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
                // `permission_mode: None` passes --dangerously-skip-permissions.
                tools: ToolAccess::Full {
                    auto_approve: vec![
                        "Read".to_string(), "Edit".to_string(), "Write".to_string(),
                        "Bash".to_string(), "Glob".to_string(), "Grep".to_string(),
                    ],
                    permission_mode: None,
                },
                max_turns: Some(50),
                max_budget_usd: None,
                session_id: Some(Uuid::new_v4().to_string()),
                resume_session: None,
                model: task_model,
                system_prompt: None,
                append_system_prompt: None,
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
                    Self::set_task_error_static(&tasks, &storage, &events, task_id, &msg).await;
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
                            Self::set_task_error_static(&tasks, &storage, &events, task_id, &full_msg).await;
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
        // A direct call, same as the poller: no pre-existing reservation, so
        // this draws its own fresh permit from the shared gate inside
        // `spawn_task_execution`. A frontend pre-check believing capacity is
        // free is not authority here -- this is.
        self.reconcile_admission().await;
        if !self.spawn_task_execution(task_id, None).await {
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
    ///
    /// Reaches whichever of an execution or an AI review/fix currently owns
    /// the task -- the task-exclusivity contract (see
    /// [`crate::queue::admission`]) means at most one of `running_handles`
    /// and `reviewing_handles` can have a live entry for it at once, so this
    /// tries the execution map first and only falls through to the review
    /// map if that found nothing. A review's reviewer-then-fix sequence is
    /// one future behind one [`ReviewOwner`]; cancelling it ends whichever
    /// `ClaudeRunner` it currently owns, the same as [`RunningTask`].
    pub async fn stop_task(&self, task_id: Uuid) -> Result<(), String> {
        // Stopping is an ownership change, so it takes the task's lifecycle
        // lease like every other one. It cannot deadlock against the run it is
        // ending: `spawn_task_execution`/`spawn_review` release the lease as
        // soon as they have registered the run, and neither future itself
        // ever asks for it -- everything each does on the way out (killing
        // the process, removing its handle-map entry, recording the
        // execution) uses its own locks. So the longest this holds the lease
        // for is the tail of another lifecycle operation, or one agent's own
        // bounded shutdown -- never its unbounded lifetime; see
        // [`AGENT_SHUTDOWN_TIMEOUT`].
        let _lease = self.lifecycle.acquire(task_id).await?;

        let ended = self.end_task_owners_under_lease(task_id).await?;

        // `ended` names which map actually held (and joined) a live owner,
        // which is the status the caller provably found the task in a moment
        // ago -- not necessarily the task's status right now. See
        // `settle_stopped_static`'s own doc for why that distinction is the
        // whole safety argument against a stale write: an owner found in
        // `running_handles` may have already finished on its own and
        // recorded `AiReview` by the time the join above returns, and the
        // guard below is what stops this from clobbering that. Nothing found
        // to end (`None`) is the same `InProgress` guard `stop_task` always
        // used for "neither map owned it" -- a crash or an unrecorded
        // outcome may have left the task stranded `InProgress` with no live
        // owner, and that is the one case this settles to `Backlog`.
        let from_status = ended.unwrap_or(TaskStatus::InProgress);
        Self::settle_stopped_static(&self.tasks, &self.storage, task_id, from_status).await
    }

    /// End whatever execution or AI review/fix ownership currently exists for
    /// `task_id`, killing and reaping the underlying process before
    /// returning, and leave the task's persisted status untouched.
    ///
    /// This is [`stop_task`](Self::stop_task)'s ownership-ending mechanics on
    /// their own, without the "settle to `Backlog`" opinion `stop_task`
    /// forms about what an ended run means -- a caller that is about to
    /// write its *own* new status (a lifecycle transition, not a stop) wants
    /// the process gone, not `Backlog`. **The caller must already hold
    /// `task_id`'s lifecycle lease**; this never asks for it, or it would
    /// deadlock against a caller that is calling it from inside one.
    ///
    /// `Ok(Some(status))` names which map a live owner was joined out of
    /// (`InProgress` for an execution, `AiReview` for a review/fix) -- the
    /// status the task provably had the moment ownership was taken, useful to
    /// a caller (`stop_task`) that needs to guard a later write against a
    /// race with the owner's own completion. `Ok(None)` means neither map had
    /// a live entry, so there was nothing to end. `Err` means an owner was
    /// found but did not finish within [`AGENT_SHUTDOWN_TIMEOUT`]: it has
    /// been put back exactly where it was found, and the caller must refuse
    /// whatever it was about to do rather than proceed as if ownership had
    /// ended.
    async fn end_task_owners_under_lease(&self, task_id: Uuid) -> Result<Option<TaskStatus>, String> {
        // Taken out of the map before awaiting anything, so the guard is
        // released before the future below tries to remove itself.
        let running = self.running_handles.write().await.remove(&task_id);

        if let Some(mut owner) = running {
            let _ = owner.cancel.send(true);
            // A run that finished on its own between the removal and here has
            // already recorded its own outcome; joining it is still correct,
            // it simply returns at once. A panicked run returns an error,
            // which the caller's own settling is what covers.
            //
            // `&mut owner.handle` rather than `owner.handle`: a `timeout` that
            // elapses drops the future it was given, and a `JoinHandle`
            // dropped without being polled to completion does not abort the
            // task it names -- it only forgets about it. Borrowing keeps
            // `owner` intact so it can be put back exactly as if this call
            // had never reached in.
            match tokio::time::timeout(AGENT_SHUTDOWN_TIMEOUT, &mut owner.handle).await {
                Ok(_) => {}
                Err(_) => {
                    // Still live, so still owning the checkout: put it back
                    // exactly where it was found rather than leave the map
                    // disagreeing with reality, and refuse instead of telling
                    // a caller ownership ended when the agent has not
                    // actually stopped.
                    self.running_handles.write().await.insert(task_id, owner);
                    return Err(format!(
                        "the agent for task {task_id} is still finishing up; nothing was \
                         changed, so this can simply be asked for again shortly"
                    ));
                }
            }
            return Ok(Some(TaskStatus::InProgress));
        }

        // No execution owns it; an AI review/fix might. Same shape, same
        // bound, same map-then-return ordering -- and that ordering is the
        // whole safety argument against a stale write: whatever this join
        // waits for (including any durable write the review future itself
        // makes on its way out, such as `transition_to_human_review`) always
        // finishes *before* this returns, never after. A caller that commits
        // its own status write only once this has returned therefore never
        // races a review that is still mid-flight -- it either sees the
        // review's own completed result already durable (nothing left to
        // end) or ends it before it can publish anything further.
        let reviewing = self.reviewing_handles.write().await.remove(&task_id);

        if let Some(mut owner) = reviewing {
            let _ = owner.cancel.send(true);
            match tokio::time::timeout(AGENT_SHUTDOWN_TIMEOUT, &mut owner.handle).await {
                Ok(_) => {}
                Err(_) => {
                    self.reviewing_handles.write().await.insert(task_id, owner);
                    return Err(format!(
                        "the AI review for task {task_id} is still finishing up; nothing was \
                         changed, so this can simply be asked for again shortly"
                    ));
                }
            }
            return Ok(Some(TaskStatus::AiReview));
        }

        // Neither map owned it: either nothing was ever running for this
        // task, or an execution already finished and moved status off
        // `InProgress`/`AiReview` entirely on its own.
        Ok(None)
    }

    /// Admit a new PR-helper Claude invocation for `task_id`, or refuse.
    ///
    /// Same shape as `spawn_task_execution`/`spawn_review`'s own admission:
    /// a brief lifecycle lease guards the exclusivity check (so it cannot
    /// race a `terminalize`/status-transition that is mid-flight for the
    /// same task), released before this returns -- never held for the PR
    /// helper's own lifetime, which would make every lifecycle transition
    /// for this task wait out however long the helper takes. The returned
    /// [`PrHelperLease`] is what the caller then holds for that lifetime
    /// instead, exactly the trade `spawn_review` documents for its own
    /// lease.
    ///
    /// `self: &Arc<Self>` because the returned lease's `Drop` needs to reach
    /// this executor's `pr_helper_handles` map on every exit path, including
    /// one this module did not spawn or `.await` to completion itself.
    pub async fn try_begin_pr_helper(
        self: &Arc<Self>,
        task_id: Uuid,
    ) -> Result<PrHelperLease, PrHelperRefusal> {
        let Some(_lease) = self.lifecycle.try_acquire(task_id).await else {
            return Err(PrHelperRefusal::LifecycleContended);
        };

        let already_owned = self.running_handles.read().await.contains_key(&task_id)
            || self.reviewing_handles.read().await.contains_key(&task_id)
            || self.pr_helper_handles.lock().unwrap().contains_key(&task_id);
        if already_owned {
            return Err(PrHelperRefusal::TaskAlreadyOwned);
        }

        // A direct/manual admission draw, same as `execute_task`'s own: no
        // pre-existing reservation feeds this, so the current runtime
        // `parallel_task_limit` must be freshly reconciled before the
        // `try_acquire` below, or a config change made after the last poll
        // pass would not yet be reflected.
        self.reconcile_admission().await;
        let Some(permit) = self.admission.try_acquire() else {
            return Err(PrHelperRefusal::NoCapacity);
        };

        Ok(self.register_pr_owner(task_id, PrOwnerKind::Helper, Some(permit)))
    }

    /// Reserve `task_id` for a PR side-effect flow: a branch-tip rewrite, a
    /// push, `gh pr create`, and the durable link that follows them. **The
    /// caller must already hold `task_id`'s lifecycle lease**, and may release
    /// it as soon as this returns: from then on the returned reservation is
    /// the ownership fact, exactly as a registered execution or review is.
    ///
    /// Ends whatever execution, AI review/fix or PR helper owns the task
    /// first (bounded, like every other front door), then registers the
    /// reservation before the lease is given back, so no other owner can be
    /// admitted in between. While it is held, execution and review decline
    /// the task, a PR helper is refused, and a second PR side-effect flow is
    /// refused rather than cancelling this one. A lifecycle transition ends it
    /// the way it ends a PR helper: it asks it to stop and waits, bounded, for
    /// the flow to let go -- which the flow does at its next step boundary,
    /// never in the middle of a push or a `gh` call.
    ///
    /// No admission permit: this runs no agent, so it draws on no agent
    /// capacity, and a busy queue never refuses a pull request.
    pub async fn begin_pr_side_effect_under_lease(
        self: &Arc<Self>,
        task_id: Uuid,
    ) -> Result<PrHelperLease, String> {
        let in_progress = self
            .pr_helper_handles
            .lock()
            .unwrap()
            .get(&task_id)
            .is_some_and(|owner| owner.kind == PrOwnerKind::SideEffect);
        if in_progress {
            return Err(
                "a pull request operation for this task is already in progress; wait for it \
                 to finish before starting another"
                    .to_string(),
            );
        }

        crate::lifecycle::ExecutionOwnership::end_ownership_under_lease(self.as_ref(), task_id)
            .await?;

        Ok(self.register_pr_owner(task_id, PrOwnerKind::SideEffect, None))
    }

    /// Insert a PR owner into `pr_helper_handles` and hand back the lease
    /// that retires it. Only called with the task's lifecycle lease held and
    /// the map known to have no entry for the task.
    fn register_pr_owner(
        self: &Arc<Self>,
        task_id: Uuid,
        kind: PrOwnerKind,
        permit: Option<AdmissionPermit>,
    ) -> PrHelperLease {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        self.pr_helper_handles.lock().unwrap().insert(
            task_id,
            PrHelperOwner {
                cancel: cancel_tx,
                done: done_rx,
                kind,
            },
        );

        PrHelperLease {
            task_id,
            cancel_rx,
            done_tx: Some(done_tx),
            _permit: permit,
            handles: self.pr_helper_handles.clone(),
        }
    }

    /// End whatever PR-helper Claude invocation currently owns `task_id`,
    /// bounded by [`AGENT_SHUTDOWN_TIMEOUT`] like every other owner-ending
    /// path in this module. **The caller must already hold `task_id`'s
    /// lifecycle lease.**
    ///
    /// Unlike [`Self::end_task_owners_under_lease`], this never removes or
    /// reinserts the map entry itself -- [`PrHelperLease::drop`] is the only
    /// place that does, on every exit path of the flow it belongs to, which
    /// is what lets this simply signal cancellation and wait for that same
    /// `Drop` to run rather than reconstructing a "put it back on timeout"
    /// dance over state it does not own.
    ///
    /// `Ok(true)` means an owner was found and (already finished, or now)
    /// ended. `Ok(false)` means nothing owned this task. `Err` means an
    /// owner was found but did not finish within the bound: nothing was
    /// changed, and the caller must refuse whatever it was about to do.
    async fn end_pr_helper_owner_under_lease(&self, task_id: Uuid) -> Result<bool, String> {
        let entry = {
            let map = self.pr_helper_handles.lock().unwrap();
            map.get(&task_id)
                .map(|o| (o.cancel.clone(), o.done.clone()))
        };
        let Some((cancel, mut done)) = entry else {
            return Ok(false);
        };
        let _ = cancel.send(true);
        if *done.borrow() {
            return Ok(true);
        }
        match tokio::time::timeout(AGENT_SHUTDOWN_TIMEOUT, done.changed()).await {
            Ok(_) => Ok(true),
            Err(_) => Err(format!(
                "the pull request helper or operation for task {task_id} is still finishing \
                 up; nothing was changed, so this can simply be asked for again shortly"
            )),
        }
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
    /// Guarded on `from_status` (see below) so a stop that arrives after the
    /// owned work finished on its own does not pull a task back out of a
    /// state it had already durably reached. That is the honest resolution
    /// of that race: the run was over before the stop got there.
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
    /// `from_status` is the status the caller actually joined an owner out
    /// of (`InProgress` for an execution, `AiReview` for a review/fix, or
    /// `InProgress` again for "nothing was found to join at all" -- see the
    /// three call sites in [`Self::stop_task`]). It is deliberately not
    /// inferred from the task's current status: the whole point of this
    /// guard is to compare what actually happened (this exact status, at the
    /// moment ownership was last provably held) against what the task shows
    /// now, so a stop that arrived after the owned work already finished and
    /// moved on -- to `AiReview` from an execution, to `HumanReview` from a
    /// review -- settles nothing rather than overwriting that result.
    async fn settle_stopped_static(
        tasks: &Tasks,
        storage: &crate::config::Storage,
        task_id: Uuid,
        from_status: TaskStatus,
    ) -> Result<(), String> {
        let mut tasks_w = tasks.write().await;

        let Some(task) = tasks_w.get(&task_id) else {
            return Ok(()); // deleted while its agent was being stopped
        };
        if task.status != from_status {
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

    /// Run one `ClaudeRunner` (the reviewer agent, or the fix agent) to
    /// completion, or kill it and return early if `cancelled` fires first.
    ///
    /// This is the one place a `ClaudeRunner` spawned inside a review/fix
    /// flow is owned: `ClaudeRunner::start` is never itself raced against
    /// cancellation (an outer `select!` around a future still mid-spawn is
    /// exactly the unsafe shape that can drop a just-spawned child with
    /// nothing left pointing at it), so a cancellation arriving before this
    /// call is checked up front and a cancellation arriving during `wait()`
    /// is met with this function's own `kill()` before it returns -- never a
    /// caller racing the whole call from outside.
    ///
    /// `Ok(Some(run))` is a completed run, successful or not (see
    /// [`AgentRun::success`]); `Ok(None)` is a graceful decline (cancelled
    /// before start, or cancelled during `wait()` and killed); `Err` is a
    /// real failure to start.
    async fn run_cancellable_agent(
        config: ClaudeRunConfig,
        cancelled: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<Option<AgentRun>, String> {
        if *cancelled.borrow() {
            return Ok(None);
        }
        let runner = ClaudeRunner::start(config).await?;
        tokio::select! {
            biased;
            result = runner.wait() => {
                let success = matches!(result, Ok(true));
                let failure = result.err();
                let output = runner.get_output().await;
                let _ = runner.kill().await;
                Ok(Some(AgentRun { success, failure, output }))
            }
            _ = cancelled.changed() => {
                let _ = runner.kill().await;
                Ok(None)
            }
        }
    }

    /// Same contract as [`Self::run_cancellable_agent`], for the CodeRabbit
    /// subprocess: owned by the code that spawned it for its whole life, so a
    /// cancellation arriving while it is running kills and reaps it rather
    /// than leaving `tokio::join!` (see [`Self::spawn_review`]) parked on a
    /// branch nothing will ever signal again -- the exact way a parked
    /// joined branch could make a stop hang forever.
    async fn run_cancellable_coderabbit(
        working_dir: &str,
        cancelled: &mut tokio::sync::watch::Receiver<bool>,
    ) -> String {
        if *cancelled.borrow() {
            return String::new();
        }
        let mut child = match tokio::process::Command::new("coderabbit")
            .args(["review", "--prompt-only", "--type", "uncommitted", "--cwd", working_dir, "--no-color"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return format!("CodeRabbit error: {}", e),
        };

        let status = tokio::select! {
            biased;
            status = child.wait() => Some(status),
            _ = cancelled.changed() => None,
        };

        let Some(status) = status else {
            let _ = child.kill().await;
            let _ = child.wait().await; // reap; no zombie left behind
            return String::new();
        };

        use tokio::io::AsyncReadExt;
        let mut stdout_buf = String::new();
        if let Some(mut out) = child.stdout.take() {
            let _ = out.read_to_string(&mut stdout_buf).await;
        }
        let mut stderr_buf = String::new();
        if let Some(mut err) = child.stderr.take() {
            let _ = err.read_to_string(&mut stderr_buf).await;
        }

        match status {
            Ok(s) if s.success() => stdout_buf,
            Ok(_) => format!("CodeRabbit warning: {}", stderr_buf),
            Err(e) => format!("CodeRabbit error: {}", e),
        }
    }

    async fn spawn_review(&self, task_id: Uuid) {
        // Acquiring the task's worktree path is an ownership question, so it
        // happens under the task's lifecycle lease -- exactly the reason
        // `spawn_task_execution` takes it (see that function's doc). Without
        // this, a `terminalize`/delete reachable directly from `commands::
        // task`/the IPC handlers (independent of the poll loop that calls
        // this) could see `is_task_running() == false` a moment before this
        // registers into `reviewing_handles`, remove the checkout, and leave
        // this function's already-scheduled future to run its diff/reviewer/
        // fix-agent/`jj describe` sequence against a directory that is gone.
        // Released as soon as the run is registered (this lease is a local
        // of this synchronous portion of the function, not moved into the
        // spawned future), the same trade `spawn_task_execution` makes:
        // holding it for the review's whole lifetime would make `stop_task`
        // -- which needs this same lease -- wait for the run it is ending.
        let Some(_lease) = self.lifecycle.try_acquire(task_id).await else {
            return; // another lifecycle operation owns this task right now
        };

        // One owner per task, in this direction too: a PR helper or PR
        // side-effect flow that owns the task keeps it until it lets go, and
        // `try_begin_pr_helper` refuses a live review the same way. Checked
        // under the lease, which both sides register under, so neither can
        // slip in between the other's check and its registration. Declining
        // leaves the task in `AiReview`, and the next pass retries it.
        if self.pr_helper_handles.lock().unwrap().contains_key(&task_id) {
            return;
        }

        // The task's own worktree, or no review at all.
        //
        // This used to fall back to the repository path, and a review is not a
        // read-only operation: the fix agent runs in this directory and the
        // block below finishes by running `jj describe` in it. Against the
        // user's own checkout that rewrites their current change description
        // under a task title. A task with no worktree has nothing of its own to
        // review, and saying so is the only safe answer.
        //
        // No admission permit is needed for this path: nothing here spawns a
        // process, so there is no capacity to hold.
        //
        // Read under the lease, not from whatever `check_and_execute`
        // sampled before waiting for it: a task that has since lost its
        // worktree (or been deleted) must not be started off a stale read.
        let Some((working_dir, base_commit)) = ({
            let tasks_r = self.tasks.read().await;
            tasks_r.get(&task_id).and_then(|t| t.worktree_path.clone().map(|w| (w, t.base_commit.clone())))
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
            Self::transition_to_human_review(
                &self.tasks,
                &self.storage,
                &self.events,
                task_id,
                Some(signoff),
            )
            .await;
            return;
        };

        // Capacity, from the same gate execution draws on -- acquired only
        // now that this is actually about to spawn a process. Declining is
        // ordinary here too: the task stays `AiReview`, untouched, and the
        // next pass's `review_pending` scan picks it right back up.
        let Some(permit) = self.admission.try_acquire() else {
            return;
        };

        let tasks = self.tasks.clone();
        let reviewing_handles = self.reviewing_handles.clone();
        let events = self.events.clone();
        let storage = self.storage.clone();
        let queue_manager = self.queue_manager.clone();
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);

        // Mark phase as actively reviewing
        Self::update_task_phase_static(&tasks, task_id, TaskPhase::QaReview, 85).await;

        // Same reasoning as `spawn_task_execution`: the lock is taken before
        // the spawn and held across the insert, so there is no instant in
        // which a review is running and `stop_task` can find nothing to
        // cancel. A future that raced in ahead of the insert would only be
        // able to observe that by trying to acquire this exact same lock to
        // remove itself, which blocks until the insert below releases it.
        let mut handles = self.reviewing_handles.write().await;

        let handle = tokio::spawn(async move {
            // Held for the whole life of this future -- the reviewer agent,
            // the fix agent, and everything between them are one continuous
            // ownership flow, and it does not release or reacquire this
            // permit when it moves from one `ClaudeRunner` to the next. See
            // `crate::queue::admission`.
            let _permit = permit;

            let task_id_str = task_id.to_string();

            events.agent_event(AgentEvent::Log {
                task_id: task_id_str.clone(),
                level: LogLevel::Info,
                message: "Starting AI review...".to_string(),
            });

            // The one canonical task diff -- same boundary the UI's diff
            // modal uses. An unknown boundary or a computation failure are
            // both real problems, distinct from "nothing changed": either
            // one gets an explicit `Rejected` signoff naming the reason,
            // never a silent "no changes" skip. Not raced against
            // cancellation: this is a bounded git subprocess (see
            // `worktree::diff`), not an open-ended agent, so there is
            // nothing here a `select!` would usefully shorten.
            let diff = match crate::worktree::task_diff(&working_dir, base_commit.as_deref()).await {
                Ok(d) => d,
                Err(e) => {
                    if *cancelled.borrow() {
                        reviewing_handles.write().await.remove(&task_id);
                        return;
                    }
                    events.agent_event(AgentEvent::Log {
                        task_id: task_id_str.clone(),
                        level: LogLevel::Warn,
                        message: format!("Could not compute task diff, skipping AI review: {e}"),
                    });
                    let signoff = QaSignoff {
                        status: QaStatus::Rejected,
                        issues_found: vec![format!("AI review skipped: {e}")],
                        timestamp: chrono::Utc::now(),
                        session_id: Uuid::new_v4(),
                    };
                    Self::transition_to_human_review(&tasks, &storage, &events, task_id, Some(signoff)).await;
                    reviewing_handles.write().await.remove(&task_id);
                    return;
                }
            };
            let diff = diff.patch;

            if diff.trim().is_empty() {
                if *cancelled.borrow() {
                    reviewing_handles.write().await.remove(&task_id);
                    return;
                }
                events.agent_event(AgentEvent::Log {
                    task_id: task_id_str.clone(),
                    level: LogLevel::Info,
                    message: "No changes detected, skipping review".to_string(),
                });
                Self::transition_to_human_review(&tasks, &storage, &events, task_id, None).await;
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

            let use_coderabbit = {
                let mgr = queue_manager.read().await;
                mgr.config().use_coderabbit
            };

            // A) Claude review — cancellable via `run_cancellable_agent`,
            // which owns its `ClaudeRunner` for its whole life. Each leg
            // gets its own `Receiver` clone (a `watch::Receiver` cannot be
            // shared, mutably, across two futures polled at once, and these
            // run concurrently in the `tokio::join!` below) and its own
            // clone of everything else it needs, since `working_dir`,
            // `events` and `task_id_str` are all still needed afterward —
            // by the fix-agent stage and by the final signoff writes.
            let mut cancelled_for_claude = cancelled.clone();
            let working_dir_for_claude = working_dir.clone();
            let claude_review = async move {
                match Self::run_cancellable_agent(
                    ClaudeRunConfig {
                        prompt: review_prompt,
                        working_dir: working_dir_for_claude,
                        // The reviewer only reads. `ReadOnly` is what makes
                        // that true: an approval list alone would still leave
                        // it Bash, Edit and the network.
                        tools: ToolAccess::ReadOnly,
                        max_turns: Some(10),
                        max_budget_usd: None,
                        session_id: Some(Uuid::new_v4().to_string()),
                        resume_session: None,
                        model: None,
                        system_prompt: None,
                        append_system_prompt: None,
                        disable_mcp: true,
                        additional_dirs: Vec::new(),
                    },
                    &mut cancelled_for_claude,
                )
                .await
                {
                    Ok(Some(run)) => run,
                    // Cancelled; the outer check below stops this flow.
                    Ok(None) => AgentRun { success: false, failure: None, output: String::new() },
                    Err(e) => AgentRun {
                        success: false,
                        failure: Some(format!("the reviewer could not start: {e}")),
                        output: String::new(),
                    },
                }
            };

            // B) CodeRabbit review (if available) — same cancellable shape.
            let mut cancelled_for_coderabbit = cancelled.clone();
            let working_dir_for_coderabbit = working_dir.clone();
            let events_for_coderabbit = events.clone();
            let task_id_str_for_coderabbit = task_id_str.clone();
            let coderabbit_review = async move {
                if !use_coderabbit {
                    return String::new();
                }
                match tokio::process::Command::new("which").arg("coderabbit").output().await {
                    Ok(output) if output.status.success() => {}
                    _ => {
                        events_for_coderabbit.agent_event(AgentEvent::Log {
                            task_id: task_id_str_for_coderabbit.clone(),
                            level: LogLevel::Warn,
                            message: "CodeRabbit enabled but CLI not found. Install it or disable in Queue Settings.".to_string(),
                        });
                        return String::new();
                    }
                }
                events_for_coderabbit.agent_event(AgentEvent::Log {
                    task_id: task_id_str_for_coderabbit.clone(),
                    level: LogLevel::Info,
                    message: "Running CodeRabbit review...".to_string(),
                });
                Self::run_cancellable_coderabbit(&working_dir_for_coderabbit, &mut cancelled_for_coderabbit).await
            };

            // Run both reviews in parallel. Each leg above already killed and
            // reaped its own subprocess on cancellation, so this always
            // returns promptly once cancelled -- never a parked branch
            // waiting on a process nothing will end.
            let (claude_result, coderabbit_result) = tokio::join!(claude_review, coderabbit_review);

            // The review's outcome is only ever recorded from here on, and
            // every remaining write below is guarded the same way: a
            // cancellation observed here means whatever partial findings
            // exist are not this run's business to publish. `stop_task`
            // owns telling the user it ended; this future's only job past
            // this point is to get out of the way.
            if *cancelled.borrow() {
                reviewing_handles.write().await.remove(&task_id);
                return;
            }

            // A reviewer run that failed, or ended without a verdict, has
            // not reviewed anything, and must not read as a pass.
            let has_claude_issues = match review_verdict(&claude_result) {
                ReviewVerdict::ChangesRequested => true,
                ReviewVerdict::Approved => false,
                ReviewVerdict::Failed(reason) => {
                    let message = format!("AI review failed, so the change is not approved: {reason}");
                    events.agent_event(AgentEvent::Log {
                        task_id: task_id_str.clone(),
                        level: LogLevel::Error,
                        message: message.clone(),
                    });
                    let signoff = QaSignoff {
                        status: QaStatus::Rejected,
                        issues_found: vec![message],
                        timestamp: chrono::Utc::now(),
                        session_id: Uuid::new_v4(),
                    };
                    Self::transition_to_human_review(&tasks, &storage, &events, task_id, Some(signoff)).await;
                    reviewing_handles.write().await.remove(&task_id);
                    return;
                }
            };
            let claude_result = claude_result.output;

            // Merge findings
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

                let fix_outcome = Self::run_cancellable_agent(
                    ClaudeRunConfig {
                        prompt: fix_prompt,
                        working_dir: working_dir.clone(),
                        // An approval list under --dangerously-skip-permissions:
                        // the fix agent still has the full default tool set,
                        // Bash included.
                        tools: ToolAccess::Full {
                            auto_approve: vec![
                                "Read".to_string(), "Edit".to_string(), "Write".to_string(),
                                "Glob".to_string(), "Grep".to_string(),
                            ],
                            permission_mode: None,
                        },
                        max_turns: Some(20),
                        max_budget_usd: None,
                        session_id: Some(Uuid::new_v4().to_string()),
                        resume_session: None,
                        model: None,
                        system_prompt: None,
                        append_system_prompt: None,
                        disable_mcp: false,
                        additional_dirs: Vec::new(),
                    },
                    &mut cancelled,
                )
                .await;

                let fix_result = match fix_outcome {
                    Ok(Some(_output)) => {
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
                    Ok(None) => {
                        // Cancelled during the fix agent's run: it has
                        // already been killed by `run_cancellable_agent`.
                        // Nothing durable is recorded on this path either.
                        reviewing_handles.write().await.remove(&task_id);
                        return;
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

                if *cancelled.borrow() {
                    reviewing_handles.write().await.remove(&task_id);
                    return;
                }

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

                Self::transition_to_human_review(&tasks, &storage, &events, task_id, Some(signoff)).await;
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

                Self::transition_to_human_review(&tasks, &storage, &events, task_id, Some(signoff)).await;
            }

            reviewing_handles.write().await.remove(&task_id);
        });

        handles.insert(task_id, ReviewOwner { handle, cancel });
    }

    /// Move a task to `HumanReview`/`Complete`, durably before the board can
    /// show it.
    ///
    /// `HumanReview` is exactly what removes a task from the AI-review
    /// selector in [`Self::check_and_execute`]: nothing else ever re-selects
    /// it for review, so a publish that outran the disk here would be a
    /// permanent loss of a QA outcome the fix agent had already acted on --
    /// see issue #8. Staging and persisting under one write guard, via
    /// [`crate::lifecycle::record`], is what makes that impossible: the board
    /// can only ever show the state the file already has.
    async fn transition_to_human_review(
        tasks: &Tasks,
        storage: &crate::config::Storage,
        events: &SharedEventSink,
        task_id: Uuid,
        signoff: Option<QaSignoff>,
    ) {
        let amend = move |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&task_id) {
                t.status = TaskStatus::HumanReview;
                t.phase = TaskPhase::Complete;
                t.phase_progress = 95;
                t.overall_progress = 90;
                if let Some(s) = signoff.clone() {
                    t.qa_signoff = Some(s);
                }
            }
        };
        if let Err(e) = crate::lifecycle::record(tasks, storage, task_id, &amend).await {
            events.agent_event(AgentEvent::Log {
                task_id: task_id.to_string(),
                level: LogLevel::Warn,
                message: format!(
                    "Task {task_id} finished review, but recording it as ready for human \
                     review failed, so it was not published and will be re-reviewed rather \
                     than silently skipped: {e}"
                ),
            });
        }
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

    /// Resolve `rev` to a commit hash in `dir`, for durably recording a
    /// task's starting point at the moment its worktree is first created.
    /// `None` on any failure (detached/empty repo, unknown rev, etc.) -- the
    /// task still gets its worktree; its diff boundary is just truthfully
    /// unknown rather than guessed.
    async fn resolve_commit(dir: &str, rev: &str) -> Option<String> {
        let output = tokio::process::Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(dir)
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if sha.is_empty() { None } else { Some(sha) }
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

    /// Move a task to `Error`/`Failed`, durably before the board can show it.
    ///
    /// `Error` matches no executor selector, so the same persist-before-publish
    /// obligation applies as [`Self::transition_to_human_review`] -- see issue
    /// #8. The payload here is only a diagnostic rather than a record of
    /// irreversible work, but a lost write would still silently strand the
    /// task off every automatic path with nothing on disk explaining why.
    async fn set_task_error_static(
        tasks: &Tasks,
        storage: &crate::config::Storage,
        events: &SharedEventSink,
        task_id: Uuid,
        msg: &str,
    ) {
        let msg_owned = msg.to_string();
        let amend = move |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&task_id) {
                t.status = TaskStatus::Error;
                t.phase = TaskPhase::Failed;
                t.error_message = Some(msg_owned.clone());
            }
        };
        if let Err(e) = crate::lifecycle::record(tasks, storage, task_id, &amend).await {
            events.agent_event(AgentEvent::Log {
                task_id: task_id.to_string(),
                level: LogLevel::Warn,
                message: format!(
                    "Task {task_id} failed ({msg}), and recording that failure durably also \
                     failed, so the task was not published as errored and remains eligible to \
                     be picked up again: {e}"
                ),
            });
        }
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

    /// The queue is also what owns `running_handles`/`reviewing_handles`/
    /// `pr_helper_handles`, so it is what a lifecycle-changing status
    /// transition (or an irreversible PR side effect -- see
    /// `commands::pr`'s own callers of [`crate::lifecycle::end_active_ownership`])
    /// asks to end ownership before it commits -- see
    /// [`Self::end_task_owners_under_lease`], which this simply discards the
    /// `from_status` of: a caller reached through this trait is about to
    /// write its own new status, not settle one derived from which map an
    /// owner was found in. PR-helper ownership is ended the same way and
    /// under the same bound, then, so neither an execution/review nor a
    /// PR-helper flow can outlive a transition that is about to move,
    /// terminalize, or PR-link this task out from under it.
    async fn end_ownership_under_lease(&self, task_id: Uuid) -> Result<(), String> {
        self.end_task_owners_under_lease(task_id).await?;
        self.end_pr_helper_owner_under_lease(task_id).await?;
        Ok(())
    }
}

/// TEST-ONLY registration helpers, `pub(crate)` so the lifecycle-front-door
/// tests in `commands::task` and `ipc::handlers` can put a live owner into
/// this module's private handle maps without a real worktree, a real
/// subprocess or a real admission permit.
///
/// What those front-door tests need to prove is ordering and refusal --
/// "ownership is ended (or the transition is refused) before the new status
/// commits" -- not "a real process is actually killed", which this module's
/// own `stop_task` tests already prove against real fixture subprocesses
/// (`review_lifecycle::a_running_reviewer_agent_can_be_stopped_safely` and
/// siblings) over the exact same `end_task_owners_under_lease` mechanics
/// `end_ownership_under_lease` now shares with `stop_task`. Duplicating that
/// subprocess proof at every call site would prove the same fact five times
/// instead of once.
#[cfg(test)]
impl TaskExecutor {
    /// Register a live execution owner the way
    /// [`Self::spawn_task_execution`] does, over a future that ends only
    /// once it is cancelled. Returns a flag the fake owner sets after
    /// observing cancellation, the same "cleanup actually ran" proof
    /// `register_cancellable_execution` gives this module's own tests.
    pub(crate) async fn register_fake_running_execution_for_test(
        &self,
        task_id: Uuid,
    ) -> Arc<std::sync::atomic::AtomicBool> {
        let cleaned_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let flag = cleaned_up.clone();
        let handle = tokio::spawn(async move {
            let _ = cancelled.changed().await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        self.running_handles
            .write()
            .await
            .insert(task_id, RunningTask { handle, cancel });
        cleaned_up
    }

    /// Same as [`Self::register_fake_running_execution_for_test`], for
    /// `reviewing_handles`.
    pub(crate) async fn register_fake_reviewing_owner_for_test(
        &self,
        task_id: Uuid,
    ) -> Arc<std::sync::atomic::AtomicBool> {
        let cleaned_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let flag = cleaned_up.clone();
        let handle = tokio::spawn(async move {
            let _ = cancelled.changed().await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        self.reviewing_handles
            .write()
            .await
            .insert(task_id, ReviewOwner { handle, cancel });
        cleaned_up
    }

    /// Register a live execution owner whose cleanup, once cancelled, makes
    /// its own durable write to the task -- the shape a real completion path
    /// takes (`spawn_task_execution`'s own status write,
    /// `spawn_review`'s `transition_to_human_review`) before it returns.
    ///
    /// For a front-door-level stale-write regression: the whole safety
    /// argument in [`Self::end_task_owners_under_lease`]'s doc is that this
    /// write happens-before the caller's own commit, because the caller
    /// joins the owner's future to completion first. Given `tasks`/`storage`
    /// directly rather than reached through `self`, because the point is to
    /// write into the very same shared map and file the calling test's
    /// `AppState`/`IpcContext` and the command under test both use.
    pub(crate) async fn register_fake_running_execution_with_writeback_for_test(
        &self,
        task_id: Uuid,
        tasks: Tasks,
        storage: crate::config::Storage,
    ) {
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            let _ = cancelled.changed().await;
            let mut tasks_w = tasks.write().await;
            if let Some(t) = tasks_w.get_mut(&task_id) {
                // `title` on purpose: a lifecycle transition's own
                // `reset_execution_state()` legitimately clears
                // `phase`/`error_message` as part of the very move under
                // test, which would make this indistinguishable from the
                // stale write it exists to catch. `title` is untouched by
                // every status transition, so it can only carry the value
                // this closure wrote, from whenever this closure wrote it.
                t.title = "owner's own cleanup ran".to_string();
                let project_id = t.project_id;
                let snapshot: Vec<Task> = tasks_w.values().cloned().collect();
                drop(tasks_w);
                let _ = storage.save_project_tasks(project_id, &snapshot);
            }
        });
        self.running_handles
            .write()
            .await
            .insert(task_id, RunningTask { handle, cancel });
    }

    /// Register a running-execution owner whose future never finishes even
    /// once it observes cancellation -- the shape of a process wedged in an
    /// uninterruptible syscall, mirroring this module's own
    /// `register_execution_that_ignores_cancellation`. For proving a
    /// caller's timeout/refusal path restores the owner and refuses the
    /// transition rather than proceeding as though ownership had ended.
    /// Same as [`Self::register_fake_running_execution_for_test`], except
    /// this one also draws a real [`AdmissionPermit`] from [`Self::admission`]
    /// and holds it exactly the way `spawn_task_execution` does -- moved into
    /// the future, dropped only once cancellation is observed -- rather than
    /// only registering into `running_handles`. For a cross-module test (one
    /// outside this file, so it cannot reach the private `admission` field
    /// directly) that needs to prove a *different* task's admission draw
    /// (a PR helper's, in particular) is genuinely refused while this one is
    /// alive, not merely blocked by same-task exclusivity.
    ///
    /// `#[cfg(unix)]`: its one caller lives in `commands::pr`'s unix-only
    /// `pr_command_ownership` test module (unix-only for the same reason
    /// every other real-subprocess fixture in that file is: `/proc` pid
    /// checks and `chmod`-executable fixture scripts).
    #[cfg(unix)]
    pub(crate) async fn register_fake_running_execution_holding_permit_for_test(
        &self,
        task_id: Uuid,
    ) -> Arc<std::sync::atomic::AtomicBool> {
        let cleaned_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let flag = cleaned_up.clone();
        self.reconcile_admission().await;
        let permit = self
            .admission
            .try_acquire()
            .expect("test setup: a permit must be free before this call");
        let handle = tokio::spawn(async move {
            let _ = cancelled.changed().await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            drop(permit);
        });
        self.running_handles
            .write()
            .await
            .insert(task_id, RunningTask { handle, cancel });
        cleaned_up
    }

    /// Same as [`Self::register_fake_running_execution_for_test`], except a
    /// caller-supplied probe runs synchronously the instant cancellation is
    /// observed, strictly before the "cleaned up" flag is set and therefore
    /// strictly before whatever `.await` is waiting on that flag (here,
    /// [`Self::end_task_owners_under_lease`]'s join of this owner's handle)
    /// can return.
    ///
    /// For a cross-module ordering regression: the probe can read some piece
    /// of external state (a git branch's tip commit, say) at the exact moment
    /// this module considers the owner ended, without teaching this module
    /// anything about git. If the probe observes the *post*-mutation state,
    /// the caller ended ownership too late (after the mutation, not before
    /// it) -- a defect no amount of asserting "the owner is gone by the time
    /// the command returns" can distinguish from the correct ordering, since
    /// both leave the owner gone by then.
    ///
    /// `#[cfg(unix)]`: its callers live in `commands::pr`'s unix-only
    /// `pr_command_ownership` test module, for the same reason every other
    /// real-subprocess/real-git fixture there is.
    #[cfg(unix)]
    pub(crate) async fn register_fake_running_execution_with_probe_for_test(
        &self,
        task_id: Uuid,
        probe: impl FnOnce() + Send + 'static,
    ) -> Arc<std::sync::atomic::AtomicBool> {
        let cleaned_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let flag = cleaned_up.clone();
        let handle = tokio::spawn(async move {
            let _ = cancelled.changed().await;
            probe();
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        self.running_handles
            .write()
            .await
            .insert(task_id, RunningTask { handle, cancel });
        cleaned_up
    }

    pub(crate) async fn register_unkillable_running_execution_for_test(&self, task_id: Uuid) {
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            let _ = cancelled.changed().await;
            std::future::pending::<()>().await;
        });
        self.running_handles
            .write()
            .await
            .insert(task_id, RunningTask { handle, cancel });
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

    /// Removes write permission on `dir` for the lifetime of the guard, and
    /// restores it on drop -- including on an unwinding panic -- so a failed
    /// assertion never leaves a directory `tempfile::TempDir` cannot clean up.
    ///
    /// Unix-only: the underlying assertion is that a chmod-locked directory
    /// makes a durable write fail, which has no portable equivalent (a
    /// Windows readonly attribute on a directory does not block file
    /// creation inside it the way a Unix permission bit does).
    #[cfg(unix)]
    struct Unwritable(std::path::PathBuf);

    #[cfg(unix)]
    impl Unwritable {
        fn on(dir: &std::path::Path) -> Self {
            std::fs::create_dir_all(dir).expect("create the directory to lock down");
            std::fs::set_permissions(
                dir,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o500),
            )
            .expect("remove write permission");
            Self(dir.to_path_buf())
        }
    }

    #[cfg(unix)]
    impl Drop for Unwritable {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(
                &self.0,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
            );
        }
    }

    /// The same defect Unit 4A closed for the ordinary mutation front doors,
    /// found in the one queue mutation that is not behind any front door at
    /// all: the poll's own auto-promotion, which used to move `Queue` tasks
    /// to `InProgress` in the shared map with no call to `Storage` ever in
    /// the loop. A crash right after such a promotion came back `Queue` on
    /// disk while the rest of the running process -- until its own crash --
    /// believed the task was executing. This forces a real durable-write
    /// failure and proves the fix: the poll leaves the task exactly as it
    /// found it, both in memory and on disk, rather than reporting nothing
    /// while quietly diverging from the file.
    #[cfg(unix)]
    #[tokio::test]
    async fn auto_promotion_leaves_the_task_untouched_when_the_durable_write_fails() {
        let (storage, storage_temp) = test_storage();
        let (registry, _reg_temp) = test_registry();
        let paths_temp = tempfile::TempDir::new().expect("paths temp dir");

        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("queued", project_id, TaskStatus::Queue, 0);
        task.phase = TaskPhase::Idle;
        let task_id = task.id;
        let tasks: Tasks = Arc::new(RwLock::new(HashMap::from([(task_id, task.clone())])));
        storage
            .save_project_tasks(project_id, &[task])
            .expect("seed the board with the previous, good value");

        let executor = TaskExecutor::new(TaskExecutorConfig {
            tasks: tasks.clone(),
            queue_manager: Arc::new(RwLock::new(QueueManager::new(
                tasks.clone(),
                crate::config::queue::QueueConfig {
                    auto_promote: true,
                    ..Default::default()
                },
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

        let guard = Unwritable::on(&storage_temp.path().join("config").join("tasks"));

        executor.check_and_execute().await;

        drop(guard);

        let in_memory = tasks
            .read()
            .await
            .get(&task_id)
            .cloned()
            .expect("the record itself must survive a failed persist");
        assert_eq!(
            in_memory.status,
            TaskStatus::Queue,
            "memory must keep the previous value when the disk write failed, \
             not report a promotion that never reached disk"
        );

        let on_disk = executor
            .storage
            .load_project_tasks(project_id)
            .expect("the previous file must still be readable");
        let disk_task = on_disk
            .iter()
            .find(|t| t.id == task_id)
            .expect("the previous record must still be there");
        assert_eq!(
            disk_task.status,
            TaskStatus::Queue,
            "disk must keep the previous value too, not a torn or partial write"
        );
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

    // ===== issue #8: selector-terminal task-state persistence =====
    //
    // `transition_to_human_review`, `record_pr_poll_state`'s `CLOSED` case, and
    // `set_task_error_static` each publish a status that removes the task from
    // every executor selector that would otherwise write it again. Each pair
    // of tests below proves both halves of the contract: a persistence
    // failure must leave memory exactly as it was (so the task stays eligible
    // for whatever would normally repair it), and a persistence success must
    // make memory and disk agree on the new state.

    /// Make every `save_project_tasks` call against `storage_temp` fail
    /// deterministically, by putting a regular file where the legacy tasks
    /// directory has to be -- so the `create_dir_all` inside the atomic write
    /// fails with `AlreadyExists`. No permission bits, so this behaves
    /// identically for every user including root, unlike a read-only
    /// directory.
    fn block_persistence(storage_temp: &tempfile::TempDir) {
        let blocker = storage_temp.path().join("config").join("tasks");
        let _ = std::fs::remove_dir_all(&blocker);
        std::fs::write(&blocker, b"not a directory").expect("place persistence blocker");
    }

    /// Seed one task (plus an untouched sibling in the same project, which
    /// every whole-project rewrite must carry through unchanged) directly on
    /// `executor`'s board and disk.
    async fn seed_task(executor: &TaskExecutor, status: TaskStatus) -> (Uuid, Uuid) {
        let project_id = Uuid::new_v4();
        let task = crate::test_helpers::create_test_task_full("subject", project_id, status, 0);
        let sibling =
            crate::test_helpers::create_test_task_full("sibling", project_id, TaskStatus::Backlog, 1);
        let (id, sibling_id) = (task.id, sibling.id);
        {
            let mut tasks_w = executor.tasks.write().await;
            tasks_w.insert(id, task.clone());
            tasks_w.insert(sibling_id, sibling.clone());
        }
        executor
            .storage
            .save_project_tasks(project_id, &[task, sibling])
            .expect("seed the board");
        (id, project_id)
    }

    fn on_disk_task(executor: &TaskExecutor, project_id: Uuid, task_id: Uuid) -> Task {
        executor
            .storage
            .load_project_tasks(project_id)
            .expect("the board must be readable")
            .into_iter()
            .find(|t| t.id == task_id)
            .expect("the task must be on disk")
    }

    /// Whether any recorded event is a `Warn`-level `agent-event` log whose
    /// message contains `needle`, proving a persistence failure was actually
    /// surfaced rather than silently swallowed.
    fn warn_message_containing(recording: &crate::events::RecordingEventSink, needle: &str) -> bool {
        recording.recorded().iter().any(|(name, payload)| {
            name == "agent-event"
                && payload.get("type").and_then(|v| v.as_str()) == Some("log")
                && payload.get("level").and_then(|v| v.as_str()) == Some("warn")
                && payload
                    .get("message")
                    .and_then(|v| v.as_str())
                    .is_some_and(|m| m.contains(needle))
        })
    }

    // --- record_pr_poll_state("CLOSED") ---

    #[tokio::test]
    async fn a_closed_pr_that_cannot_be_recorded_is_not_exposed_and_stays_pollable() {
        let recording = Arc::new(crate::events::RecordingEventSink::new());
        let (executor, temps) = test_executor_with_events(recording.clone());
        let (id, _project_id) = task_with_open_pr(&executor, None).await;
        block_persistence(&temps[0]);

        executor.record_pr_poll_state(id, 7, "CLOSED").await;

        let after = executor.tasks.read().await.get(&id).cloned().expect("task");
        assert_eq!(
            recorded_pr_state(&after),
            None,
            "a write that did not reach disk must not be visible in memory, or the next poll \
             would treat an un-recorded closure as already recorded and never ask again"
        );
        assert!(
            after.error_message.is_none(),
            "the closed-without-merge explanation is part of the same fact and must not appear \
             alone"
        );
        assert_eq!(after.status, TaskStatus::PrCreated);

        assert!(
            warn_message_containing(&recording, "was not applied and will be retried"),
            "the failure must be surfaced, not silently swallowed; recorded events: {:?}",
            recording.recorded()
        );
    }

    #[tokio::test]
    async fn a_closed_pr_that_is_recorded_is_durable_before_it_is_published() {
        let (executor, _temps) = test_executor();
        let (id, project_id) = task_with_open_pr(&executor, None).await;

        executor.record_pr_poll_state(id, 7, "CLOSED").await;

        let after = executor.tasks.read().await.get(&id).cloned().expect("task");
        assert_eq!(recorded_pr_state(&after).as_deref(), Some("CLOSED"));
        assert_eq!(
            after.error_message.as_deref(),
            Some("PR was closed without merge")
        );

        let on_disk = on_disk_task(&executor, project_id, id);
        assert_eq!(
            recorded_pr_state(&on_disk).as_deref(),
            Some("CLOSED"),
            "the file is what the next start reads, so a latch only in memory is no latch"
        );
        assert_eq!(
            on_disk.error_message.as_deref(),
            Some("PR was closed without merge")
        );
    }

    // --- transition_to_human_review ---

    #[tokio::test]
    async fn a_human_review_transition_that_cannot_be_recorded_is_not_exposed() {
        let recording = Arc::new(crate::events::RecordingEventSink::new());
        let (executor, temps) = test_executor_with_events(recording.clone());
        let (id, _project_id) = seed_task(&executor, TaskStatus::AiReview).await;
        {
            let mut tasks_w = executor.tasks.write().await;
            let t = tasks_w.get_mut(&id).unwrap();
            t.phase = TaskPhase::QaReview;
        }
        block_persistence(&temps[0]);

        let signoff = QaSignoff {
            status: QaStatus::FixesApplied,
            issues_found: vec!["found one".to_string()],
            timestamp: chrono::Utc::now(),
            session_id: Uuid::new_v4(),
        };
        TaskExecutor::transition_to_human_review(
            &executor.tasks,
            &executor.storage,
            &executor.events,
            id,
            Some(signoff),
        )
        .await;

        let after = executor.tasks.read().await.get(&id).cloned().expect("task");
        assert_eq!(
            after.status,
            TaskStatus::AiReview,
            "a lost write must leave the task exactly where the AI-review selector still finds \
             it, not silently moved to a status nothing else ever re-selects"
        );
        assert_eq!(after.phase, TaskPhase::QaReview);
        assert!(
            after.qa_signoff.is_none(),
            "the fix agent's outcome must not be exposed as read unless it is durable, or the \
             next start re-runs the review over a tree that already contains the fix"
        );

        assert!(
            warn_message_containing(&recording, "will be re-reviewed rather than silently skipped"),
            "the failure must be surfaced, not silently swallowed; recorded events: {:?}",
            recording.recorded()
        );
    }

    #[tokio::test]
    async fn a_human_review_transition_that_is_recorded_is_durable_before_it_is_published() {
        let (executor, _temps) = test_executor();
        let (id, project_id) = seed_task(&executor, TaskStatus::AiReview).await;

        let signoff = QaSignoff {
            status: QaStatus::Approved,
            issues_found: Vec::new(),
            timestamp: chrono::Utc::now(),
            session_id: Uuid::new_v4(),
        };
        TaskExecutor::transition_to_human_review(
            &executor.tasks,
            &executor.storage,
            &executor.events,
            id,
            Some(signoff),
        )
        .await;

        let after = executor.tasks.read().await.get(&id).cloned().expect("task");
        assert_eq!(after.status, TaskStatus::HumanReview);
        assert_eq!(after.phase, TaskPhase::Complete);
        assert!(after.qa_signoff.is_some());

        let on_disk = on_disk_task(&executor, project_id, id);
        assert_eq!(
            on_disk.status,
            TaskStatus::HumanReview,
            "the file is what the next start reads, so a transition only in memory is no \
             transition"
        );
        assert_eq!(on_disk.phase, TaskPhase::Complete);
        assert!(on_disk.qa_signoff.is_some());

        // The untouched sibling seeded alongside this task must survive the
        // whole-project rewrite unchanged, proving the staged snapshot carried
        // every task in the project and not just the one being amended.
        let siblings = executor
            .storage
            .load_project_tasks(project_id)
            .expect("board must be readable");
        assert_eq!(
            siblings.len(),
            2,
            "the sibling seeded alongside this task must still be on disk"
        );
    }

    // --- set_task_error_static ---

    #[tokio::test]
    async fn a_task_error_that_cannot_be_recorded_is_not_exposed_and_stays_eligible() {
        let recording = Arc::new(crate::events::RecordingEventSink::new());
        let (executor, temps) = test_executor_with_events(recording.clone());
        let (id, _project_id) = seed_task(&executor, TaskStatus::InProgress).await;
        block_persistence(&temps[0]);

        TaskExecutor::set_task_error_static(
            &executor.tasks,
            &executor.storage,
            &executor.events,
            id,
            "agent could not start",
        )
        .await;

        let after = executor.tasks.read().await.get(&id).cloned().expect("task");
        assert_eq!(
            after.status,
            TaskStatus::InProgress,
            "a lost write must not fabricate a second durable state nothing wrote either -- the \
             task must remain exactly as reachable as it was before this call"
        );
        assert!(after.error_message.is_none());

        assert!(
            warn_message_containing(&recording, "remains eligible to be picked up again"),
            "the failure must be surfaced, not silently swallowed; recorded events: {:?}",
            recording.recorded()
        );
    }

    #[tokio::test]
    async fn a_task_error_that_is_recorded_is_durable_before_it_is_published() {
        let (executor, _temps) = test_executor();
        let (id, project_id) = seed_task(&executor, TaskStatus::InProgress).await;

        TaskExecutor::set_task_error_static(
            &executor.tasks,
            &executor.storage,
            &executor.events,
            id,
            "agent could not start",
        )
        .await;

        let after = executor.tasks.read().await.get(&id).cloned().expect("task");
        assert_eq!(after.status, TaskStatus::Error);
        assert_eq!(after.phase, TaskPhase::Failed);
        assert_eq!(after.error_message.as_deref(), Some("agent could not start"));

        let on_disk = on_disk_task(&executor, project_id, id);
        assert_eq!(on_disk.status, TaskStatus::Error);
        assert_eq!(on_disk.phase, TaskPhase::Failed);
        assert_eq!(
            on_disk.error_message.as_deref(),
            Some("agent could not start"),
            "the file is what the next start reads, so an error only in memory is no error"
        );
    }

    /// A `TaskExecutor` over nothing but temporary directories.
    ///
    /// Deliberately synchronous and runtime-free, because
    /// [`polling_loop_can_be_built_with_no_ambient_tokio_runtime`] must be
    /// able to build one from a plain `#[test]`. The returned temp dirs must
    /// be kept alive for as long as the executor is used.
    fn test_executor() -> (Arc<TaskExecutor>, Vec<tempfile::TempDir>) {
        test_executor_with_events(crate::events::null_sink())
    }

    /// Same as [`test_executor`], but with a caller-supplied sink so a test can
    /// inspect what the executor reported instead of discarding it.
    fn test_executor_with_events(
        events: SharedEventSink,
    ) -> (Arc<TaskExecutor>, Vec<tempfile::TempDir>) {
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
            events,
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

            // `pr_check_counter` is bumped partway through `check_and_execute`,
            // just before the PR-status-polling section runs, so seeing it
            // above zero proves a real pass reached that point — the loop is
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
        // allowed to run at least one real `check_and_execute` pass far
        // enough to reach its PR-status-polling section first (tracked via
        // `pr_check_counter`, incremented partway through every pass, just
        // before that section runs), so this cannot degenerate into "shutdown
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

        // Let at least one pass reach its PR-status-polling section before
        // signalling shutdown. `pr_check_counter` is incremented partway
        // through `check_and_execute`, just before that section runs, so
        // observing it above zero is proof real work ran, not merely that
        // the loop task was scheduled.
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
        let (cancel, _cancelled) = tokio::sync::watch::channel(false);
        executor
            .reviewing_handles
            .write()
            .await
            .insert(task_id, ReviewOwner { handle, cancel });

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

        executor.spawn_task_execution(task_id, None).await;

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

    /// A task whose recorded branch is not a branch name is refused through
    /// the same no-fallback path, before git is given the value.
    ///
    /// `branch_name` comes back from `tasks.toml`, and a board kept in the
    /// project holds whatever the last commit put there. Starting a task that
    /// already carries a branch reattaches to it, which used to run
    /// `git worktree add <dest> <branch>` with the value as it was: `-Bvictim`
    /// force-resets the user's unrelated branch `victim` and then starts the
    /// agent on it.
    #[tokio::test]
    async fn a_task_with_a_poisoned_recorded_branch_is_refused_before_git_runs() {
        let (executor, temps) = test_executor();

        let repo = temps[0].path().join("repository");
        std::fs::create_dir_all(&repo).unwrap();
        let repo_path = repo.to_string_lossy().to_string();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("spawn git");
            assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "first"]);
        git(&["branch", "victim"]);
        git(&["commit", "-q", "--allow-empty", "-m", "second"]);
        let refs_before = git(&["for-each-ref", "--format=%(refname) %(objectname)"]);

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

        let mut task = create_test_task_full("poisoned branch", project_id, TaskStatus::InProgress, 0);
        task.phase = TaskPhase::Idle;
        task.branch_name = Some("-Bvictim".to_string());
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);

        executor.spawn_task_execution(task_id, None).await;

        assert_eq!(
            git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
            refs_before,
            "no ref may move, `victim` above all"
        );
        assert!(
            executor.running_handles.read().await.is_empty(),
            "no agent may be started for a task whose branch was refused"
        );
        assert_eq!(git(&["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
        assert_eq!(git(&["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(git(&["status", "--porcelain"]), "");

        let after = executor.tasks.read().await.get(&task_id).cloned().unwrap();
        assert_eq!(after.status, TaskStatus::Error);
        assert_eq!(after.worktree_path, None, "neither the repository nor anything else is recorded");
        assert_eq!(after.branch_name.as_deref(), Some("-Bvictim"), "a refused value is reported, never rewritten");
        assert!(
            after.error_message.as_deref().is_some_and(|m| m.contains("\"-Bvictim\"")),
            "the reason has to name the refused branch: {:?}",
            after.error_message
        );
    }

    /// Run git in `repo` with a fixed identity, asserting it succeeds.
    fn git_in(repo: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(args)
            .current_dir(repo)
            .output()
            .expect("spawn git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A git repository with one commit on `main`, registered as the
    /// repository of a new standalone project, and a task in that project
    /// that depends on a second task recording `dependency_branch`.
    ///
    /// Returns the repository, the dependent task and the dependency task.
    async fn stacked_task_fixture(
        executor: &TaskExecutor,
        temps: &[tempfile::TempDir],
        dependency_status: TaskStatus,
        dependency_has_pr: bool,
        dependency_branch: &str,
    ) -> (std::path::PathBuf, Uuid, Uuid) {
        let repo = temps[0].path().join("repository");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q", "-b", "main"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);

        let project_id = Uuid::new_v4();
        let repository_id = Uuid::new_v4();
        executor.repositories.write().await.insert(
            repository_id,
            crate::domain::Repository {
                id: repository_id,
                local_path: repo.to_string_lossy().to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        let mut project = test_project(project_id, ProjectScope::Standalone);
        project.repository_id = Some(repository_id);
        executor.projects.write().await.insert(project_id, project);

        let mut dependency = create_test_task_full("dependency", project_id, dependency_status, 0);
        dependency.branch_name = Some(dependency_branch.to_string());
        if dependency_has_pr {
            dependency.external_refs.push(crate::domain::task::ExternalRef::GithubPr {
                url: "https://github.com/test-org/test-repo/pull/7".to_string(),
                number: 7,
                repo: "test-org/test-repo".to_string(),
                state: Some("MERGED".to_string()),
            });
        }
        let dependency_id = dependency.id;
        executor.tasks.write().await.insert(dependency_id, dependency);

        let mut task = create_test_task_full("stacked", project_id, TaskStatus::InProgress, 1);
        task.phase = TaskPhase::Idle;
        task.dependencies = vec![dependency_id];
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);

        (repo, task_id, dependency_id)
    }

    fn refs_of(repo: &std::path::Path) -> String {
        git_in(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])
    }

    fn worktree_count(repo: &std::path::Path) -> usize {
        git_in(repo, &["worktree", "list", "--porcelain"]).matches("worktree ").count()
    }

    /// A task stacked on a dependency whose branch cannot be stacked on is
    /// not started from the default base instead.
    ///
    /// It used to be: the failure was logged as a warning and an ordinary
    /// branch was created off `HEAD`, so the agent ran without the
    /// dependency's work it had been queued to build on, and nothing on the
    /// task recorded that it was no longer stacked. The dependency here is
    /// still in progress and records a branch that is not in the
    /// repository, which fails the stacked path before it creates anything.
    #[tokio::test]
    async fn a_stacked_task_whose_dependency_cannot_be_stacked_on_is_not_started_unstacked() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) = stacked_task_fixture(
            &executor, &temps, TaskStatus::InProgress, false, "task-deadbeef",
        )
        .await;
        let refs_before = refs_of(&repo);

        executor.spawn_task_execution(task_id, None).await;

        assert!(
            executor.running_handles.read().await.is_empty(),
            "no agent may be started for a stacked task that could not be stacked"
        );
        assert_eq!(refs_of(&repo), refs_before, "no branch may be created off the default base instead");
        assert_eq!(worktree_count(&repo), 1);

        let after = executor.tasks.read().await.get(&task_id).cloned().unwrap();
        assert_eq!(after.status, TaskStatus::Error);
        assert_eq!(after.worktree_path, None);
        assert_eq!(after.branch_name, None, "the task keeps no branch, so a later start stacks again");
        assert_eq!(after.base_commit, None);
        assert!(
            after.error_message.as_deref().is_some_and(|m| m.contains("task-deadbeef")),
            "the reason has to name the dependency's branch: {:?}",
            after.error_message
        );
    }

    /// A dependency that is done, with a pull request, and whose local
    /// branch has since been deleted has been delivered: its work is on the
    /// default base, so the task starts there, and the log says why.
    #[tokio::test]
    async fn a_task_whose_done_dependency_branch_is_gone_starts_from_the_default_base() {
        let recording = Arc::new(crate::events::RecordingEventSink::new());
        let (executor, temps) = test_executor_with_events(recording.clone());
        let (repo, task_id, _) =
            stacked_task_fixture(&executor, &temps, TaskStatus::Done, true, "task-deadbeef").await;
        let main_tip = git_in(&repo, &["rev-parse", "main"]);

        let (existing, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;

        assert_eq!(existing, None);
        let (info, what_happened, base_commit) = acquired.expect("an ordinary worktree");
        assert_eq!(what_happened, "Created worktree");
        assert_eq!(git_in(&repo, &["rev-parse", &format!("refs/heads/{}", info.branch)]), main_tip);
        assert_eq!(git_in(std::path::Path::new(&info.path), &["rev-parse", "HEAD"]), main_tip);
        assert_eq!(base_commit.as_deref(), Some(main_tip.as_str()));
        assert_eq!(worktree_count(&repo), 2);

        let recorded = serde_json::to_string(&recording.recorded()).unwrap();
        assert!(
            recorded.contains("task-deadbeef") && recorded.contains("default base"),
            "the log has to say why the task was not stacked: {recorded}"
        );
    }

    /// A done dependency whose branch is still there is stacked on as
    /// before: a local branch is not taken as proof of anything missing.
    #[tokio::test]
    async fn a_done_dependency_whose_branch_is_still_there_is_stacked_on() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) =
            stacked_task_fixture(&executor, &temps, TaskStatus::Done, true, "task-deadbeef").await;
        git_in(&repo, &["branch", "task-deadbeef"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "main moves on"]);
        let dependency_tip = git_in(&repo, &["rev-parse", "task-deadbeef"]);

        let (_, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;

        let (info, what_happened, base_commit) = acquired.expect("a stacked worktree");
        assert_eq!(what_happened, "Created stacked worktree");
        assert_eq!(git_in(std::path::Path::new(&info.path), &["rev-parse", "HEAD"]), dependency_tip);
        assert_eq!(base_commit.as_deref(), Some(dependency_tip.as_str()));
    }

    /// A retry that picks up the branch an earlier attempt left, with the
    /// task's own commit already on it, reports that it resumed and records
    /// the dependency's tip as where the task started. Taking the
    /// worktree's `HEAD` instead would drop that commit out of the task's
    /// diff.
    #[tokio::test]
    async fn a_resumed_stacked_branch_keeps_the_dependency_tip_as_its_base() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) = stacked_task_fixture(
            &executor, &temps, TaskStatus::InProgress, false, "task-deadbeef",
        )
        .await;
        git_in(&repo, &["checkout", "-q", "-b", "task-deadbeef"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "dependency work"]);
        let dependency_tip = git_in(&repo, &["rev-parse", "HEAD"]);
        let task_branch = WorktreeManager::branch_for_task(task_id);
        git_in(&repo, &["checkout", "-q", "-b", &task_branch]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "the task's own work"]);
        let task_tip = git_in(&repo, &["rev-parse", "HEAD"]);
        git_in(&repo, &["checkout", "-q", "main"]);

        let (existing, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;

        assert_eq!(existing, None, "the task never recorded the branch");
        let (info, what_happened, base_commit) = acquired.expect("the leftover branch is resumed");
        assert_eq!(what_happened, "Resumed stacked worktree");
        assert_eq!(info.branch, task_branch);
        assert_eq!(git_in(std::path::Path::new(&info.path), &["rev-parse", "HEAD"]), task_tip);
        assert_eq!(base_commit.as_deref(), Some(dependency_tip.as_str()));
    }

    /// A done dependency whose recorded branch is not a branch name is
    /// refused, not read as a deleted branch and quietly skipped.
    #[tokio::test]
    async fn a_done_dependency_with_an_invalid_recorded_branch_is_refused() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) =
            stacked_task_fixture(&executor, &temps, TaskStatus::Done, true, "-Bvictim").await;
        let refs_before = refs_of(&repo);

        let (_, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;

        let error = acquired.err().expect("the invalid name must be refused");
        assert!(error.contains("\"-Bvictim\""), "{error}");
        assert_eq!(refs_of(&repo), refs_before);
        assert_eq!(worktree_count(&repo), 1);
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

    // ---- The lease-hold bound --------------------------------------------------

    /// Register an execution the way [`register_cancellable_execution`] does,
    /// except the future never finishes even once it observes cancellation --
    /// the shape of a process wedged in an uninterruptible syscall, or any
    /// other tail a `SIGKILL` cannot shorten. Proves `stop_task` bounds the
    /// join rather than trusting the future to end.
    async fn register_execution_that_ignores_cancellation(
        executor: &TaskExecutor,
        task_id: Uuid,
    ) -> tokio::sync::watch::Receiver<bool> {
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let observed = cancelled.clone();
        let handle = tokio::spawn(async move {
            let _ = cancelled.changed().await;
            std::future::pending::<()>().await;
        });
        executor
            .running_handles
            .write()
            .await
            .insert(task_id, RunningTask { handle, cancel });
        observed
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_gives_up_after_a_bound_rather_than_hold_the_lease_forever() {
        // With time paused and no timer anywhere in the stuck future, tokio
        // has nothing to auto-advance past -- so without a bound this test
        // does not fail an assertion, it hangs until the harness kills it.
        // That is the regression, reproduced.
        let (executor, _temps) = test_executor();
        let task = running_task(Uuid::new_v4());
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);
        register_execution_that_ignores_cancellation(&executor, task_id).await;

        let result = tokio::time::timeout(
            AGENT_SHUTDOWN_TIMEOUT + std::time::Duration::from_secs(1),
            executor.stop_task(task_id),
        )
        .await
        .expect("stop_task itself must be what gives up, not this outer bound");

        assert!(
            result.is_err(),
            "a stop that could not confirm the agent ended must not report success"
        );
        assert!(
            executor.running_handles.read().await.contains_key(&task_id),
            "still live, so still owning the checkout: the entry must be put back \
             rather than left removed, or a concurrent read of is_task_running would \
             answer falsely that nothing is running"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_that_times_out_does_not_settle_the_task_as_stopped() {
        // The caller-facing half: a stop that could not confirm the agent
        // ended must not move the task to `Backlog` -- that would tell the
        // person the run is over while the checkout is still owned by
        // something still writing to it.
        let (executor, _temps) = test_executor();
        let task = running_task(Uuid::new_v4());
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);
        register_execution_that_ignores_cancellation(&executor, task_id).await;

        let result = executor.stop_task(task_id).await;
        assert!(result.is_err());

        let tasks = executor.tasks.read().await;
        assert_eq!(
            tasks.get(&task_id).expect("the task survives").status,
            TaskStatus::InProgress,
            "nothing was confirmed ended, so nothing about the task may change"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_stop_still_asked_the_execution_to_end() {
        // The bound is on the join, not on cancellation itself: a stop that
        // times out must still have signalled the future to end, so it is
        // not stuck waiting on a cancellation nobody ever asked for.
        let (executor, _temps) = test_executor();
        let task = running_task(Uuid::new_v4());
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);
        let observed = register_execution_that_ignores_cancellation(&executor, task_id).await;

        let _ = executor.stop_task(task_id).await;

        assert!(
            *observed.borrow(),
            "the stuck future must have observed the cancellation signal, even \
             though it then chose to ignore it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn once_a_stuck_agent_actually_finishes_a_later_stop_ends_it_normally() {
        // The recovery half: the bound is a retry signal, not a permanent
        // refusal. The task that timed out and was put back is still the
        // live fact -- once it actually finishes, the very next call joins
        // it and reports it as ended.
        let (executor, _temps) = test_executor();
        let task = running_task(Uuid::new_v4());
        let task_id = task.id;
        executor.tasks.write().await.insert(task_id, task);

        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            let _ = cancelled.changed().await;
            let _ = release_rx.await;
        });
        executor
            .running_handles
            .write()
            .await
            .insert(task_id, RunningTask { handle, cancel });

        let first = executor.stop_task(task_id).await;
        assert!(first.is_err(), "setup: must still be stuck here");
        assert!(executor.running_handles.read().await.contains_key(&task_id));

        // Now let it actually finish.
        let _ = release_tx.send(());

        let second = executor.stop_task(task_id).await;
        assert!(
            second.is_ok(),
            "a future that has now finished must be reported as ended, not as still \
             going: {second:?}"
        );
        assert!(
            executor.running_handles.read().await.is_empty(),
            "joined and gone: nothing left pointing at it"
        );
        let tasks = executor.tasks.read().await;
        assert_eq!(
            tasks.get(&task_id).expect("the task survives").status,
            TaskStatus::Backlog,
            "the second, successful stop must settle the task"
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

    /// RED before the canonical task diff existed: `commit_changes` committed
    /// the agent's work, and the old `spawn_review` diff (`jj diff` / `git
    /// diff HEAD`) fell back to `git diff HEAD`, which is empty once
    /// everything is already committed -- so the reviewer silently never saw
    /// the change at all. This proves the real production commit step
    /// (`TaskExecutor::commit_changes`) plus the canonical diff
    /// (`crate::worktree::task_diff`) together still show the reviewer the
    /// task's own change afterward.
    #[tokio::test]
    async fn the_ai_reviewer_still_sees_a_task_s_change_after_commit_changes_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(path).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "T"]);
        std::fs::write(path.join("seed.txt"), "seed\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "seed"]);

        let base_commit = {
            let out = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(path)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        // The agent's uncommitted change, exactly as `spawn_task_execution`
        // leaves the worktree right before calling `commit_changes`.
        std::fs::write(path.join("agent_change.txt"), "the agent's work\n").unwrap();

        let task = create_test_task_full("t", Uuid::new_v4(), TaskStatus::InProgress, 0);
        let task_id = task.id;
        let tasks: Tasks = Arc::new(RwLock::new(HashMap::from([(task_id, task)])));
        let events = crate::events::null_sink();

        // The real production commit step -- this is what made the old
        // `git diff HEAD` fallback empty.
        TaskExecutor::commit_changes(&tasks, task_id, path.to_str().unwrap(), &events).await;

        let status_after = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&status_after.stdout).trim().is_empty(),
            "commit_changes should leave a clean tree"
        );

        let diff = crate::worktree::task_diff(path.to_str().unwrap(), Some(&base_commit))
            .await
            .expect("task_diff should succeed against a real base commit");
        assert!(
            !diff.patch.trim().is_empty(),
            "the AI reviewer must still see the task's committed change, got an empty diff"
        );
        assert!(diff.patch.contains("agent_change.txt"));
    }

    // ---- Unit 5C2: review/fix lifecycle ownership and shared capacity ----
    //
    // Real fake `claude` subprocesses (never the user's real Claude config),
    // installed on `PATH` and torn down on `Drop`. `PATH` is process-global,
    // so every test in this module that installs one serializes on the one
    // shared `crate::test_helpers::PATH_LOCK` -- the same lock
    // `commands::pr`'s `MockGh` tests use, for the same reason. A private,
    // module-local lock here would not serialize against that module's own
    // PATH mutations under default parallel `cargo test`; see
    // `test_helpers::PATH_LOCK`'s doc comment for the corrective-pass
    // evidence that this actually raced.
    #[cfg(target_os = "linux")]
    mod review_lifecycle {
        use super::*;
        use crate::test_helpers::PATH_LOCK;

        /// A stand-in `claude` binary that plays either the reviewer role
        /// (its `--allowedTools` has neither `Edit` nor `Bash`) or the fix
        /// role (`--allowedTools` has `Edit`). Only the `--allowedTools`
        /// value is read: the reviewer's `--disallowedTools` names `Edit`
        /// too. Whichever role equals
        /// `block_role` blocks until killed, recording its own pid; every
        /// other invocation exits immediately reporting an issue, which is
        /// what drives the flow from the reviewer into the fix agent.
        struct MockClaude {
            _tmp: tempfile::TempDir,
            pidfile: std::path::PathBuf,
            saved_path: Option<String>,
        }

        impl MockClaude {
            fn install(block_role: &str) -> Self {
                let tmp = tempfile::tempdir().expect("tempdir");
                let bin_dir = tmp.path().join("bin");
                std::fs::create_dir_all(&bin_dir).unwrap();
                let pidfile = tmp.path().join("blocked.pid");

                let script = format!(
                    "#!/bin/sh\n\
                     printf '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-fixture\",\"model\":\"fixture-model\"}}\\n'\n\
                     role=review\n\
                     prev=\n\
                     for a in \"$@\"; do\n\
                     \x20 if [ \"$prev\" = --allowedTools ]; then\n\
                     \x20   case \"$a\" in *Edit*) role=fix ;; esac\n\
                     \x20 fi\n\
                     \x20 prev=$a\n\
                     done\n\
                     if [ \"$role\" = {block_role:?} ]; then\n\
                     \x20 printf '%s\\n' \"$$\" > {pidfile:?}\n\
                     \x20 exec sleep 300\n\
                     fi\n\
                     printf '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s-fixture\",\"result\":\"CHANGES_REQUESTED: fixture wants changes\"}}\\n'\n\
                     exit 0\n",
                    block_role = block_role,
                    pidfile = pidfile,
                );
                let bin = bin_dir.join("claude");
                std::fs::write(&bin, &script).expect("write fixture script");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
                        .expect("chmod fixture script");
                }

                let saved_path = std::env::var("PATH").ok();
                let new_path = match &saved_path {
                    Some(p) => format!("{}:{}", bin_dir.display(), p),
                    None => bin_dir.display().to_string(),
                };
                // Safety: serialized via PATH_LOCK; restored on Drop.
                unsafe {
                    std::env::set_var("PATH", new_path);
                }

                MockClaude { _tmp: tmp, pidfile, saved_path }
            }

            fn blocked_pid(&self) -> Option<i32> {
                std::fs::read_to_string(&self.pidfile).ok()?.trim().parse().ok()
            }
        }

        impl Drop for MockClaude {
            fn drop(&mut self) {
                unsafe {
                    match &self.saved_path {
                        Some(p) => std::env::set_var("PATH", p),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }

        fn pid_is_alive(pid: i32) -> bool {
            std::path::Path::new(&format!("/proc/{pid}")).exists()
        }

        async fn wait_for_pidfile(mock: &MockClaude) -> i32 {
            for _ in 0..400 {
                if let Some(pid) = mock.blocked_pid() {
                    return pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            panic!("fixture never wrote its pid file; the blocking role was never reached");
        }

        /// A real git repo with an uncommitted change, exactly what
        /// `spawn_review` needs to compute a non-empty canonical diff.
        fn git_repo_with_change() -> (tempfile::TempDir, String) {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path();
            let git = |args: &[&str]| {
                let status =
                    std::process::Command::new("git").args(args).current_dir(path).status().unwrap();
                assert!(status.success(), "git {args:?} failed");
            };
            git(&["init", "-q"]);
            git(&["config", "user.email", "t@example.com"]);
            git(&["config", "user.name", "T"]);
            std::fs::write(path.join("seed.txt"), "seed\n").unwrap();
            git(&["add", "-A"]);
            git(&["commit", "-q", "-m", "seed"]);
            let base_commit = {
                let out = std::process::Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .current_dir(path)
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            std::fs::write(path.join("agent_change.txt"), "the agent's work\n").unwrap();
            (dir, base_commit)
        }

        fn reviewing_task(project_id: Uuid, worktree_path: &str, base_commit: &str) -> Task {
            let mut task = create_test_task_full("under review", project_id, TaskStatus::AiReview, 0);
            task.phase = TaskPhase::QaReview;
            task.worktree_path = Some(worktree_path.to_string());
            task.base_commit = Some(base_commit.to_string());
            task
        }

        /// The reviewer's verdict gate, driven through the real
        /// `spawn_review` with a stand-in `claude` that records its argv.
        mod verdict_gate {
            use super::*;

            struct MockReviewer {
                _tmp: tempfile::TempDir,
                args_file: std::path::PathBuf,
                saved_path: Option<String>,
            }

            impl MockReviewer {
                /// `result` is the reviewer's result text; `exit` its status.
                fn install(result: &str, exit: i32) -> Self {
                    let tmp = tempfile::tempdir().expect("tempdir");
                    let bin_dir = tmp.path().join("bin");
                    std::fs::create_dir_all(&bin_dir).unwrap();
                    let args_file = tmp.path().join("args");
                    let result_json = serde_json::to_string(result).unwrap();
                    let script = format!(
                        "#!/bin/sh\n\
                         for a in \"$@\"; do printf '%s\\n' \"$a\" >> {args:?}; done\n\
                         printf '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"model\":\"m\"}}\\n'\n\
                         printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"result\":{result_json}}}'\n\
                         exit {exit}\n",
                        args = args_file,
                    );
                    let bin = bin_dir.join("claude");
                    std::fs::write(&bin, &script).unwrap();
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
                    let saved_path = std::env::var("PATH").ok();
                    let new_path = match &saved_path {
                        Some(p) => format!("{}:{}", bin_dir.display(), p),
                        None => bin_dir.display().to_string(),
                    };
                    // Safety: serialized via PATH_LOCK; restored on Drop.
                    unsafe { std::env::set_var("PATH", new_path) };
                    MockReviewer { _tmp: tmp, args_file, saved_path }
                }

                fn args(&self) -> Vec<String> {
                    std::fs::read_to_string(&self.args_file)
                        .unwrap_or_default()
                        .lines()
                        .map(str::to_string)
                        .collect()
                }
            }

            impl Drop for MockReviewer {
                fn drop(&mut self) {
                    unsafe {
                        match &self.saved_path {
                            Some(p) => std::env::set_var("PATH", p),
                            None => std::env::remove_var("PATH"),
                        }
                    }
                }
            }

            /// Run one AI review to completion and return the recorded signoff.
            async fn review_once(mock: &MockReviewer) -> QaSignoff {
                let (executor, _temps) = test_executor();
                // Only the Claude reviewer is under test, not a CodeRabbit CLI
                // that may or may not be installed on this machine.
                let config = crate::config::queue::QueueConfig {
                    use_coderabbit: false,
                    ..Default::default()
                };
                executor.queue_manager.write().await.set_config(config).await;
                let (repo, base_commit) = git_repo_with_change();
                let task = reviewing_task(Uuid::new_v4(), repo.path().to_str().unwrap(), &base_commit);
                let task_id = task.id;
                executor.tasks.write().await.insert(task_id, task);

                executor.spawn_review(task_id).await;
                for _ in 0..400 {
                    let done = executor.reviewing_handles.read().await.is_empty();
                    if done {
                        let tasks = executor.tasks.read().await;
                        let t = tasks.get(&task_id).expect("task still exists");
                        assert_eq!(t.status, TaskStatus::HumanReview);
                        assert!(!mock.args().is_empty(), "the stand-in reviewer ran");
                        return t.qa_signoff.clone().expect("the review records a signoff");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                panic!("the review never finished");
            }

            fn run(success: bool, output: &str) -> AgentRun {
                AgentRun { success, failure: None, output: output.to_string() }
            }

            #[test]
            fn only_a_successful_run_with_an_approval_verdict_approves() {
                assert_eq!(review_verdict(&run(true, "ok
VERDICT: APPROVED")), ReviewVerdict::Approved);
                assert_eq!(
                    review_verdict(&run(true, "VERDICT: CHANGES_REQUESTED
- ISSUE: [high] a:1 - b")),
                    ReviewVerdict::ChangesRequested
                );
                assert!(matches!(review_verdict(&run(false, "VERDICT: APPROVED")), ReviewVerdict::Failed(_)));
                assert!(matches!(review_verdict(&run(true, "")), ReviewVerdict::Failed(_)));
                assert!(matches!(review_verdict(&run(true, "Looks fine to me.")), ReviewVerdict::Failed(_)));
                assert_eq!(review_verdict(&run(true, "ok\nVERDICT: **APPROVED**")), ReviewVerdict::Approved);
                assert_eq!(review_verdict(&run(true, "`VERDICT: APPROVED`")), ReviewVerdict::Approved);
                assert!(matches!(
                    review_verdict(&run(true, "VERDICT: APPROVED\nVERDICT: pending")),
                    ReviewVerdict::Failed(_)
                ), "only the final verdict line counts");
                assert!(matches!(review_verdict(&run(false, "VERDICT: **APPROVED**")), ReviewVerdict::Failed(_)));
                let failed = AgentRun { failure: Some("exit 2".into()), ..run(false, "") };
                assert_eq!(review_verdict(&failed), ReviewVerdict::Failed("exit 2".into()));
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn a_reviewer_that_exits_non_zero_does_not_approve() {
                let _path_guard = PATH_LOCK.lock().await;
                let mock = MockReviewer::install("VERDICT: APPROVED", 1);
                let signoff = review_once(&mock).await;
                assert_eq!(signoff.status, QaStatus::Rejected);
                assert!(
                    signoff.issues_found.iter().any(|i| i.starts_with("AI review failed")),
                    "{:?}", signoff.issues_found
                );
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn a_reviewer_with_empty_output_does_not_approve() {
                let _path_guard = PATH_LOCK.lock().await;
                let mock = MockReviewer::install("", 0);
                let signoff = review_once(&mock).await;
                assert_eq!(signoff.status, QaStatus::Rejected);
                assert!(
                    signoff.issues_found.iter().any(|i| i.contains("no output")),
                    "{:?}", signoff.issues_found
                );
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn an_explicit_approval_still_approves_and_the_reviewer_runs_read_only() {
                let _path_guard = PATH_LOCK.lock().await;
                let mock = MockReviewer::install("Looks good.\nVERDICT: APPROVED", 0);
                let signoff = review_once(&mock).await;
                assert_eq!(signoff.status, QaStatus::Approved);

                let args = mock.args();
                let value_of = |flag: &str| {
                    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
                };
                assert_eq!(value_of("--tools").as_deref(), Some("Read,Glob,Grep"), "{args:?}");
                assert_eq!(value_of("--permission-mode").as_deref(), Some("dontAsk"), "{args:?}");
                assert!(args.iter().any(|a| a == "--restricted"), "{args:?}");
                assert!(args.iter().any(|a| a == "--strict-mcp-config"), "{args:?}");
                assert!(!args.iter().any(|a| a == "--dangerously-skip-permissions"), "{args:?}");
            }
        }

        /// RED: today, `stop_task` only ever looks at `running_handles`.
        /// Calling it while a task is under AI review touches nothing, so a
        /// real reviewer subprocess is left running with the checkout still
        /// open underneath it, and the call still reports success.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_running_reviewer_agent_can_be_stopped_safely() {
            let _path_guard = PATH_LOCK.lock().await;
            let mock = MockClaude::install("review");

            let (executor, _temps) = test_executor();
            let (repo, base_commit) = git_repo_with_change();
            let task = reviewing_task(Uuid::new_v4(), repo.path().to_str().unwrap(), &base_commit);
            let task_id = task.id;
            executor.tasks.write().await.insert(task_id, task);

            executor.spawn_review(task_id).await;

            let pid = wait_for_pidfile(&mock).await;
            assert!(pid_is_alive(pid), "setup: the fixture reviewer must actually be running");

            let result = tokio::time::timeout(
                AGENT_SHUTDOWN_TIMEOUT + std::time::Duration::from_secs(5),
                executor.stop_task(task_id),
            )
            .await
            .expect("stop_task itself must bound the wait, not this outer timeout");

            assert!(result.is_ok(), "stopping a running review must succeed: {result:?}");
            assert!(
                !pid_is_alive(pid),
                "the reviewer process must be gone once stop_task reports success"
            );
            assert!(
                executor.reviewing_handles.read().await.is_empty(),
                "a stopped review must not still be tracked as in flight"
            );
        }

        /// RED: same defect, one step deeper into the flow -- a fix agent
        /// spawned *after* the reviewer found issues is exactly as
        /// unreachable from `stop_task` as the reviewer itself.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_running_fix_agent_can_be_stopped_safely() {
            let _path_guard = PATH_LOCK.lock().await;
            let mock = MockClaude::install("fix");

            let (executor, _temps) = test_executor();
            let (repo, base_commit) = git_repo_with_change();
            let task = reviewing_task(Uuid::new_v4(), repo.path().to_str().unwrap(), &base_commit);
            let task_id = task.id;
            executor.tasks.write().await.insert(task_id, task);

            executor.spawn_review(task_id).await;

            let pid = wait_for_pidfile(&mock).await;
            assert!(pid_is_alive(pid), "setup: the fixture fix agent must actually be running");

            let result = tokio::time::timeout(
                AGENT_SHUTDOWN_TIMEOUT + std::time::Duration::from_secs(5),
                executor.stop_task(task_id),
            )
            .await
            .expect("stop_task itself must bound the wait, not this outer timeout");

            assert!(result.is_ok(), "stopping a running fix agent must succeed: {result:?}");
            assert!(
                !pid_is_alive(pid),
                "the fix agent process must be gone once stop_task reports success"
            );
            assert!(
                executor.reviewing_handles.read().await.is_empty(),
                "a stopped review must not still be tracked as in flight"
            );
        }

        /// GREEN-pinning (see report §5): before this unit, `reviewing_handles`
        /// carried no cancellation channel at all, so there is no pre-fix
        /// shape of this test that both compiles and exercises real
        /// production types -- the guarantee itself is new. Proves a stop
        /// landing exactly as a review finishes on its own does not clobber
        /// the genuine completion it raced: the completed `HumanReview` write
        /// always happens-before `stop_task`'s own settle step, because
        /// `stop_task` joins the review future in full before touching the
        /// task's status itself.
        #[tokio::test(start_paused = true)]
        async fn a_completion_that_wins_the_race_with_stop_is_not_overwritten() {
            let (executor, _temps) = test_executor();
            let project_id = Uuid::new_v4();
            let task = reviewing_task(project_id, "/tmp/worktree-under-test", "deadbeef");
            let task_id = task.id;
            let storage_tasks: Tasks = executor.tasks.clone();
            storage_tasks.write().await.insert(task_id, task.clone());
            executor
                .storage
                .save_project_tasks(project_id, &[task])
                .expect("seed the board");

            let tasks = executor.tasks.clone();
            let storage = executor.storage.clone();
            let events = executor.events.clone();
            let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
            let handle = tokio::spawn(async move {
                // Simulate "already past the point of no return": by the
                // time cancellation is observed, the review has already
                // decided its outcome and is committed to recording it.
                let _ = cancelled.changed().await;
                let signoff = QaSignoff {
                    status: QaStatus::Approved,
                    issues_found: Vec::new(),
                    timestamp: chrono::Utc::now(),
                    session_id: Uuid::new_v4(),
                };
                TaskExecutor::transition_to_human_review(&tasks, &storage, &events, task_id, Some(signoff))
                    .await;
            });
            executor
                .reviewing_handles
                .write()
                .await
                .insert(task_id, ReviewOwner { handle, cancel });

            let result = executor.stop_task(task_id).await;
            assert!(result.is_ok(), "joining a review that finished must be a successful stop: {result:?}");

            let tasks_r = executor.tasks.read().await;
            let after = tasks_r.get(&task_id).expect("task survives");
            assert_eq!(
                after.status,
                TaskStatus::HumanReview,
                "the review's own completion must stand, not be clobbered back to Backlog"
            );
            assert!(after.qa_signoff.is_some());
        }

        /// GREEN-pinning (see report §5), same reason as above: cancellation
        /// arriving after a `ClaudeRunner` has been started but before this
        /// review's ownership record exists in `reviewing_handles` must still
        /// reach and kill that process, not orphan it. Proven by holding the
        /// registration lock across spawn *and* insert -- mirroring
        /// `running_handles` -- so no external caller can observe the task as
        /// "not tracked" while its process is alive.
        #[tokio::test(flavor = "multi_thread")]
        async fn cancellation_between_process_spawn_and_registration_does_not_orphan_it() {
            let _path_guard = PATH_LOCK.lock().await;
            let mock = MockClaude::install("review");

            let (executor, _temps) = test_executor();
            let (repo, base_commit) = git_repo_with_change();
            let task = reviewing_task(Uuid::new_v4(), repo.path().to_str().unwrap(), &base_commit);
            let task_id = task.id;
            executor.tasks.write().await.insert(task_id, task);

            // No synchronization between the spawn and the stop attempt other
            // than the pidfile: this drives the race as tightly as the real
            // production call sites (the poller's `spawn_review` and a
            // concurrent `stop_task`) can, rather than proving only the
            // already-registered case tests 1/2 above cover.
            executor.spawn_review(task_id).await;
            let pid = wait_for_pidfile(&mock).await;

            let result = executor.stop_task(task_id).await;
            assert!(result.is_ok());
            assert!(
                !pid_is_alive(pid),
                "a process that had already spawned by the time it was cancelled must \
                 still be killed, not orphaned because registration raced it"
            );
        }

        // ---- Part 3: shared admission / capacity truth --------------------

        /// Execution and AI review/fix draw on the exact same gate: a permit
        /// already held (standing in here for "an execution is running")
        /// makes a review decline outright, rather than spawning past a
        /// limit that was only ever checked against `running_handles`.
        #[tokio::test]
        async fn a_review_declines_when_the_only_slot_is_already_held() {
            let (executor, _temps) = test_executor();
            executor
                .queue_manager
                .write()
                .await
                .set_config(crate::config::queue::QueueConfig { parallel_task_limit: 1, ..Default::default() })
                .await;
            executor.reconcile_admission().await;
            let _held = executor.admission.try_acquire().expect("setup: the one slot must be free");

            let (repo, base_commit) = git_repo_with_change();
            let task = reviewing_task(Uuid::new_v4(), repo.path().to_str().unwrap(), &base_commit);
            let task_id = task.id;
            executor.tasks.write().await.insert(task_id, task);

            executor.spawn_review(task_id).await;

            assert!(
                executor.reviewing_handles.read().await.is_empty(),
                "a review must not be admitted while the shared gate has nothing free"
            );
            let tasks = executor.tasks.read().await;
            assert_eq!(
                tasks.get(&task_id).unwrap().status,
                TaskStatus::AiReview,
                "declining is free: the task stays exactly where it was, retried next pass"
            );
        }

        /// A manual `execute_task` call is not exempt from the gate a
        /// frontend pre-check believes is free -- backend admission is the
        /// authority, not the caller's own belief about capacity.
        #[tokio::test]
        async fn direct_execute_task_cannot_bypass_admission() {
            let (executor, _temps) = test_executor();
            executor
                .queue_manager
                .write()
                .await
                .set_config(crate::config::queue::QueueConfig { parallel_task_limit: 1, ..Default::default() })
                .await;
            executor.reconcile_admission().await;
            let _held = executor.admission.try_acquire().expect("setup: the one slot must be free");

            let mut task = create_test_task_full("manual", Uuid::new_v4(), TaskStatus::Queue, 0);
            task.phase = TaskPhase::Idle;
            let task_id = task.id;
            executor.tasks.write().await.insert(task_id, task);

            let result = executor.execute_task(task_id).await;
            assert!(
                result.is_err(),
                "a manual start must be refused, not silently exceed the shared limit: {result:?}"
            );
            assert!(executor.running_handles.read().await.is_empty());
            // Declined at the admission check specifically, before ever
            // reaching repo/worktree resolution (this task has no project or
            // repository configured, so reaching that far would instead
            // record a distinct "cannot resolve working directory" error).
            // A test that only checked `result.is_err()` could not tell
            // admission being bypassed apart from this unrelated failure.
            let tasks = executor.tasks.read().await;
            assert_eq!(
                tasks.get(&task_id).unwrap().error_message,
                None,
                "a capacity decline must be silent, not recorded as though \
                 something else about the task failed"
            );
        }

        /// The starvation regression: with capacity for exactly one active
        /// flow, a review-ready task and a queued coding task both eligible
        /// on the same pass, the review must be the one that gets the slot
        /// -- not because coding is disallowed, but because review is
        /// admitted first each pass. Persistent queued work must not starve
        /// review forever.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_review_ready_task_is_admitted_before_a_new_queued_execution() {
            let _path_guard = PATH_LOCK.lock().await;
            let _mock = MockClaude::install("review"); // blocks, so the permit is still held when we check

            let (executor, _temps) = test_executor();
            executor
                .queue_manager
                .write()
                .await
                .set_config(crate::config::queue::QueueConfig {
                    parallel_task_limit: 1,
                    auto_promote: true,
                    ..Default::default()
                })
                .await;

            let (repo, base_commit) = git_repo_with_change();
            let review_task = reviewing_task(Uuid::new_v4(), repo.path().to_str().unwrap(), &base_commit);
            let review_task_id = review_task.id;
            let project_id = review_task.project_id;
            executor.tasks.write().await.insert(review_task_id, review_task.clone());
            executor
                .storage
                .save_project_tasks(project_id, &[review_task])
                .expect("seed the board");

            let mut queued = create_test_task_full("queued coding work", project_id, TaskStatus::Queue, 1);
            queued.phase = TaskPhase::Idle;
            let queued_id = queued.id;
            executor.tasks.write().await.insert(queued_id, queued.clone());
            {
                let tasks_r = executor.tasks.read().await;
                let staged: Vec<Task> = tasks_r.values().filter(|t| t.project_id == project_id).cloned().collect();
                executor.storage.save_project_tasks(project_id, &staged).expect("seed the board");
            }

            executor.check_and_execute().await;

            assert!(
                executor.reviewing_handles.read().await.contains_key(&review_task_id),
                "the review-ready task must be admitted this pass"
            );
            let tasks = executor.tasks.read().await;
            assert_eq!(
                tasks.get(&queued_id).unwrap().status,
                TaskStatus::Queue,
                "with no capacity left after the review was admitted, the queued task must \
                 not be promoted this pass -- promotion must not visibly exceed what is \
                 actually available"
            );
        }

        /// Permit lifetime is the whole point of `crate::queue::admission`:
        /// capacity stays occupied for as long as the real agent process
        /// does, proven here with a real fixture rather than only the
        /// synthetic proof in `admission`'s own unit tests.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_permit_stays_held_until_the_review_future_actually_finishes() {
            let _path_guard = PATH_LOCK.lock().await;
            let mock = MockClaude::install("review");

            let (executor, _temps) = test_executor();
            executor
                .queue_manager
                .write()
                .await
                .set_config(crate::config::queue::QueueConfig { parallel_task_limit: 1, ..Default::default() })
                .await;
            executor.reconcile_admission().await;

            let (repo, base_commit) = git_repo_with_change();
            let task = reviewing_task(Uuid::new_v4(), repo.path().to_str().unwrap(), &base_commit);
            let task_id = task.id;
            executor.tasks.write().await.insert(task_id, task);

            executor.spawn_review(task_id).await;
            wait_for_pidfile(&mock).await;

            assert!(
                executor.admission.try_acquire().is_none(),
                "the one slot must still be held while the reviewer process is alive, \
                 not returned merely because the process has been registered"
            );

            executor.stop_task(task_id).await.expect("stop must succeed");

            assert!(
                executor.admission.try_acquire().is_some(),
                "once the owning future has actually finished, its permit must be free again"
            );
        }

        /// Adversarial-review finding: `spawn_review` used to acquire no
        /// lifecycle lease at all, unlike `spawn_task_execution`. A
        /// concurrent `terminalize`/delete (reachable directly from
        /// `commands::task`/the IPC handlers, independent of the poller)
        /// could therefore see `is_task_running() == false`, remove the
        /// checkout, and leave an already-scheduled review to run its
        /// diff/reviewer/fix-agent sequence against a directory that is
        /// gone. Proven here at the lease level directly, without needing a
        /// full `terminalize` call: whoever holds the lease first wins, and
        /// a review that could not get it declines cleanly rather than
        /// racing ahead.
        #[tokio::test]
        async fn spawn_review_declines_while_another_lifecycle_operation_holds_the_lease() {
            let (executor, _temps) = test_executor();
            let (repo, base_commit) = git_repo_with_change();
            let task = reviewing_task(Uuid::new_v4(), repo.path().to_str().unwrap(), &base_commit);
            let task_id = task.id;
            executor.tasks.write().await.insert(task_id, task);

            // Stands in for a concurrent terminalize/delete already holding
            // the task's lease.
            let _held_lease = executor
                .lifecycle
                .acquire(task_id)
                .await
                .expect("test setup: the lease must be free to take");

            executor.spawn_review(task_id).await;

            assert!(
                executor.reviewing_handles.read().await.is_empty(),
                "a review must not start against a task another lifecycle operation \
                 currently owns"
            );
            let tasks = executor.tasks.read().await;
            assert_eq!(
                tasks.get(&task_id).unwrap().status,
                TaskStatus::AiReview,
                "declining is free: the task is untouched and the next pass retries it"
            );
        }

        /// Try to start a review of `task_id` and prove it declined: nothing
        /// registered, no reviewer agent spawned, the task untouched.
        async fn review_declines(
            executor: &Arc<TaskExecutor>,
            mock: &MockClaude,
            task_id: Uuid,
            label: &str,
        ) {
            executor.spawn_review(task_id).await;
            assert!(
                executor.reviewing_handles.read().await.is_empty(),
                "{label}: no review may be registered while a PR flow owns the task"
            );
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            assert!(
                mock.blocked_pid().is_none(),
                "{label}: no reviewer agent may have been spawned"
            );
            let tasks = executor.tasks.read().await;
            let task = tasks.get(&task_id).unwrap();
            assert_eq!(task.status, TaskStatus::AiReview, "{label}: declining leaves the task as it was");
            assert_eq!(task.phase, TaskPhase::QaReview, "{label}: declining leaves the task as it was");
        }

        /// One owner per task, in both directions. `try_begin_pr_helper`
        /// already refuses a task under review; a review must equally not
        /// start while a PR helper -- or a PR side-effect operation -- owns
        /// the task: no reviewer agent is spawned and the task is left in
        /// `AiReview` for the next pass. Once the PR flow lets go, the same
        /// review starts normally.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_review_does_not_start_while_a_pr_flow_owns_the_task() {
            let _path_guard = PATH_LOCK.lock().await;
            let mock = MockClaude::install("review");

            let (executor, _temps) = test_executor();
            let (repo, base_commit) = git_repo_with_change();
            let task = reviewing_task(Uuid::new_v4(), repo.path().to_str().unwrap(), &base_commit);
            let task_id = task.id;
            executor.tasks.write().await.insert(task_id, task);

            let helper = executor
                .try_begin_pr_helper(task_id)
                .await
                .expect("setup: the PR helper is admitted");
            review_declines(&executor, &mock, task_id, "PR helper").await;
            drop(helper);

            let reservation = {
                let _lease = executor.lifecycle.acquire(task_id).await.expect("setup: lease");
                executor
                    .begin_pr_side_effect_under_lease(task_id)
                    .await
                    .expect("setup: the PR operation reserves the task")
            };
            review_declines(&executor, &mock, task_id, "PR operation").await;
            drop(reservation);

            executor.spawn_review(task_id).await;
            assert!(
                executor.reviewing_handles.read().await.contains_key(&task_id),
                "once the PR flow has let go, the review starts normally"
            );
            let pid = wait_for_pidfile(&mock).await;
            assert!(pid_is_alive(pid), "the reviewer agent is running");

            executor.stop_task(task_id).await.expect("stop the review");
            assert!(!pid_is_alive(pid));
        }
    }
}
