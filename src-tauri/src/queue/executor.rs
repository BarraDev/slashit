use crate::agents::runner::{ClaudeRunner, ClaudeRunConfig, ClaudeEvent, ToolAccess};
use crate::domain::{Task, TaskStatus, TaskPhase, AgentExecution, AgentStatus, AgentLogEntry, LogLevel, QaSignoff, QaStatus, BranchOrigin};
use crate::domain::task::ExternalRef;
use crate::queue::admission::{Admission, AdmissionPermit};
use crate::queue::start_guard::StartGuard;
use crate::queue::tool_activity::RunTools;
use crate::domain::task::ActivityKind;
use crate::queue::prompt::{build_task_prompt, build_review_prompt, build_fix_prompt};
use crate::queue::QueueManager;
use crate::worktree::{CheckoutCommit, WorktreeInfo, WorktreeManager};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use crate::events::{EventSink, SharedEventSink};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// A coding run whose start is on disk.
struct RunStart {
    working_dir: String,
    /// The run's number, as its recorded [`ActivityKind::RunStarted`] has it.
    run: u32,
}

/// The phase progress a task shows from the moment its run starts.
const RUN_START_PROGRESS: u8 = 5;

/// A coding run that has ended, for the write that records how.
struct RunEnd {
    run: u32,
    /// Its tool calls, if any were buffered.
    tools: Option<RunTools>,
    /// Why it failed, as its timeline row says it, when that is not the
    /// error the task records: that error can quote the agent's output, and
    /// the timeline never keeps any.
    timeline_reason: Option<String>,
}

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

/// Whether the last line naming a verdict is exactly `VERDICT: APPROVED`,
/// ignoring surrounding whitespace, Markdown emphasis and code marks
/// (`VERDICT: **APPROVED**`), and the `- ` bullet the review prompt lists
/// the verdicts with. A line that only mentions the token, such as one
/// quoting it, is not an approval.
fn final_verdict_is_approved(output: &str) -> bool {
    output
        .lines()
        .rev()
        .find(|line| line.contains("VERDICT"))
        .is_some_and(|line| {
            let plain: String = line.chars().filter(|c| !matches!(c, '*' | '_' | '`')).collect();
            let plain = plain.trim();
            let plain = plain.strip_prefix('-').map_or(plain, str::trim_start);
            plain == "VERDICT: APPROVED"
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

/// Why nothing is committed for a task that records no branch. Every
/// checkout SlashIt acquires or adopts records its branch alongside its
/// path, so only a task record written by hand or by a much older version
/// can lack it, and committing to whatever the checkout has out could put
/// the work on a branch the task never names.
const NO_TASK_BRANCH: &str =
    "this task records no branch, so SlashIt cannot tell which branch its work belongs on, and \
     nothing was committed";

/// What a fix-agent run means for the review's signoff.
#[derive(Debug, PartialEq, Eq)]
enum FixOutcome {
    Applied,
    /// Cancelled before or during the run; nothing is recorded.
    Cancelled,
    /// The run could not start, exited non-zero, or reported an error.
    Failed(String),
}

/// The outcome of a fix-agent run. Only a successful run applied fixes: one
/// that exited non-zero or reported an error failed, like one that never
/// started.
fn fix_outcome(outcome: Result<Option<AgentRun>, String>) -> FixOutcome {
    match outcome {
        Ok(Some(run)) if run.success => FixOutcome::Applied,
        Ok(Some(run)) => {
            let reason = run.failure.unwrap_or_else(|| "the fix agent run did not succeed".to_string());
            FixOutcome::Failed(format!("Fix agent failed: {reason}"))
        }
        Ok(None) => FixOutcome::Cancelled,
        Err(e) => FixOutcome::Failed(format!("Fix agent failed to start: {e}")),
    }
}

/// A task's worktree, or why none could be attached.
type AcquiredWorktree = Result<Acquired, String>;

/// The worktree a starting task was given and what is known about where its
/// branch came from.
struct Acquired {
    info: WorktreeInfo,
    /// What was done to get the worktree, for the log.
    what_happened: &'static str,
    /// The commit the task's branch started from, when it was created or
    /// resumed here. `None` on reattach, which keeps the recorded one.
    base_commit: Option<String>,
    /// What the task's branch was created from, when it was created or
    /// resumed here and that is known. `None` on reattach, which keeps the
    /// recorded one, and for a branch whose origin is unknown: one adopted
    /// from an unrecorded start, or an ordinary branch that did not start on
    /// the proven default base.
    origin: Option<BranchOrigin>,
    /// The commit this start created the task's branch at, together with its
    /// worktree. `None` when the branch already existed -- reattached,
    /// adopted, or a stacked branch resumed -- which is never this start's to
    /// take back if the task cannot record it.
    created_at: Option<String>,
}

/// Whether `task` sits where a delivered pull request lives, so that the
/// poll may finish it when the pull request merges and record a failure when
/// it closes.
///
/// A pull request can be linked to a task in any column: `link_pr` and
/// external references attach one without moving the task, and a task whose
/// pull request is open can be dragged back to In Progress, the Queue or the
/// Backlog to be worked on again. The poll keeps asking about those too, so
/// their cards stay current, but only these columns hand the answer to the
/// task's lifecycle.
fn in_pr_lifecycle(task: &Task) -> bool {
    matches!(task.status, TaskStatus::PrCreated | TaskStatus::HumanReview | TaskStatus::Done)
}

/// The pull requests the background poll asks about, each with the tasks
/// linked to it, in a stable order.
///
/// Every GitHub pull request linked to a task is asked about, in whatever
/// column the task is, unless:
///
/// - the task's interrupted cleanup is being reported on the task itself
///   (`cleanup_in_flight`): it cannot be finished automatically, so asking is
///   a `gh` call that can only end in the same refusal;
/// - the task records the pull request as merged or closed, which is final;
/// - the task is outside [`in_pr_lifecycle`] and this process already heard
///   GitHub say merged. Nothing durable follows from that there (see
///   [`TaskExecutor::apply_polled_pr_state`]), and the card already shows
///   it, so asking again changes nothing until the task moves or SlashIt
///   restarts. A closure heard there is recorded, which the previous rule
///   covers; one whose recording failed is asked about again, so the write
///   is retried.
fn polled_pull_requests(
    tasks: &HashMap<Uuid, Task>,
    statuses: &crate::pr_status::PrStatuses,
) -> Vec<(crate::pr_status::PrKey, Vec<Uuid>)> {
    let mut polled: std::collections::BTreeMap<(String, u32), Vec<Uuid>> = Default::default();
    for task in tasks.values().filter(|t| !t.cleanup_in_flight) {
        for r in &task.external_refs {
            let ExternalRef::GithubPr { number, repo, state, .. } = r else {
                continue;
            };
            if state.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("MERGED") || s.eq_ignore_ascii_case("CLOSED")) {
                continue;
            }
            let key = crate::pr_status::PrKey::new(repo.clone(), *number);
            let heard_merged = || {
                statuses
                    .get(&key)
                    .and_then(|e| e.status)
                    .is_some_and(|s| s.state == crate::pr_status::PrState::Merged)
            };
            if !in_pr_lifecycle(task) && heard_merged() {
                continue;
            }
            let ids = polled.entry((key.repo, key.number)).or_default();
            if !ids.contains(&task.id) {
                ids.push(task.id);
            }
        }
    }
    polled
        .into_iter()
        .map(|((repo, number), mut ids)| {
            ids.sort();
            (crate::pr_status::PrKey::new(repo, number), ids)
        })
        .collect()
}

/// Write `state` onto every reference `task` has to the pull request `key`,
/// and note it in the task's activity when it is a milestone. Answers whether
/// anything changed.
fn record_observed_pr_state(task: &mut Task, key: &crate::pr_status::PrKey, state: &str) -> bool {
    let mut changed = false;
    for r in &mut task.external_refs {
        if let ExternalRef::GithubPr { number, repo, state: recorded, .. } = r {
            // Merged and closed are final once recorded. An answer that says
            // otherwise is an older reading arriving late (a poll that
            // started before a refresh recorded the merge), not news.
            let final_recorded = recorded
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("MERGED") || s.eq_ignore_ascii_case("CLOSED"));
            if *number == key.number && *repo == key.repo && !final_recorded && recorded.as_deref() != Some(state) {
                *recorded = Some(state.to_string());
                changed = true;
            }
        }
    }
    if changed {
        task.record_pr_state(key.number, state);
    }
    changed
}

/// What the pull requests recorded on a dependency say about its work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordedPr {
    /// No pull request is recorded.
    None,
    /// At least one was recorded as merged.
    Merged,
    /// Every one was recorded as closed, and none as merged.
    ClosedUnmerged,
    /// Open, or of a state SlashIt never recorded.
    NotMerged,
}

impl RecordedPr {
    fn of(task: &Task) -> Self {
        let states: Vec<Option<&str>> = task
            .external_refs
            .iter()
            .filter_map(|r| match r {
                ExternalRef::GithubPr { state, .. } => Some(state.as_deref()),
                _ => None,
            })
            .collect();
        if states.is_empty() {
            Self::None
        } else if states.iter().any(|s| matches!(s, Some(st) if st.eq_ignore_ascii_case("MERGED"))) {
            Self::Merged
        } else if states.iter().all(|s| matches!(s, Some(st) if st.eq_ignore_ascii_case("CLOSED"))) {
            Self::ClosedUnmerged
        } else {
            Self::NotMerged
        }
    }
}

/// Event emitted to the frontend via Tauri events.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum AgentEvent {
    #[serde(rename = "log")]
    Log { task_id: String, level: LogLevel, message: String },
    /// Text the agent itself wrote, as opposed to a [`AgentEvent::Log`] line
    /// SlashIt writes about the run. Kept apart so a surface that shows what
    /// the agent is doing never shows executor bookkeeping (paths, commits)
    /// as though the agent had said it.
    #[serde(rename = "output")]
    Output { task_id: String, text: String },
    #[serde(rename = "phase_change")]
    PhaseChange { task_id: String, phase: TaskPhase, progress: u8 },
    #[serde(rename = "tool_use")]
    ToolUse { task_id: String, tool: String },
    #[serde(rename = "completed")]
    Completed { task_id: String, success: bool, message: Option<String> },
    #[serde(rename = "error")]
    Error { task_id: String, message: String },
    /// A run SlashIt owns changed state. Non-terminal states (starting,
    /// running, stopping) mean SlashIt holds the run now; `stopped` and
    /// `failed` are sent last, once the run no longer exists, and are what a
    /// listener removes it on. Sent for every change, so a listener that
    /// reads [`LiveRun`]s once on mount and then follows these never needs
    /// to poll.
    #[serde(rename = "run_state")]
    RunState { task_id: String, status: AgentStatus },
}

type Tasks = Arc<RwLock<HashMap<Uuid, Task>>>;
type ExecutionRecords = Arc<RwLock<HashMap<Uuid, AgentExecution>>>;

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
    /// The execution record this run keeps in `executions`, which is where
    /// its [`AgentStatus`] lives. Known before the future first runs, so the
    /// run's status is readable from the moment it is registered, not only
    /// once the future has inserted its record.
    execution_id: Option<Uuid>,
}

/// A fresh Project Conversation Run registered with the existing executor.
/// Its provider future holds this lease until it has killed/reaped Claude and
/// persisted the semantic outcome. The key is Conversation identity, never a
/// synthetic Task id.
pub struct ProjectRunLease {
    conversation_id: Uuid,
    owners: Arc<std::sync::Mutex<HashMap<Uuid, tokio::sync::watch::Sender<bool>>>>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    _permit: Option<AdmissionPermit>,
}

impl ProjectRunLease {
    pub fn cancel_receiver(&self) -> tokio::sync::watch::Receiver<bool> { self.cancel_rx.clone() }
    pub fn is_cancelled(&self) -> bool { *self.cancel_rx.borrow() }
}

impl Drop for ProjectRunLease {
    fn drop(&mut self) { self.owners.lock().unwrap().remove(&self.conversation_id); }
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
    kind: PrOwnerKind,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    done_tx: Option<tokio::sync::watch::Sender<bool>>,
    _permit: Option<AdmissionPermit>,
    handles: Arc<std::sync::Mutex<HashMap<Uuid, PrHelperOwner>>>,
    events: SharedEventSink,
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
        if self.kind == PrOwnerKind::Helper {
            self.events.agent_event(AgentEvent::RunState {
                task_id: self.task_id.to_string(),
                status: AgentStatus::Stopped,
            });
        }
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
    /// An unfinished restack of the task's published branch owns it; see
    /// `Task::republish_pending_refusal`.
    RepublishPending,
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
            Self::RepublishPending => write!(
                f,
                "a restack of this task's published branch is unfinished; resume or discard it \
                 in the task's pull request section first"
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

/// How long a task whose worktree could not be recorded waits before the
/// poller starts it again; see [`TaskExecutor::refuse_while_unrecorded_backoff`].
///
/// Twenty poll intervals: long enough that storage refusing writes costs one
/// checkout created and taken back per task a minute rather than one every
/// poll, short enough that a task resumes on its own soon after the storage
/// recovers.
const UNRECORDED_ACQUISITION_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// One task's backoff after a start could not record its worktree.
#[derive(Debug, Clone, Copy)]
struct UnrecordedBackoff {
    since: std::time::Instant,
    /// Whether a refusal inside this backoff has been reported yet. The
    /// poller discards what a start returns, so without one report a task
    /// moved back to In Progress would sit there with no visible reason;
    /// with one per poll it would repeat every three seconds.
    announced: bool,
}

/// Which start [`TaskExecutor::start_execution_under_lease`] is deciding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartRequest {
    /// The poller starting a task that is already waiting to run
    /// (`InProgress` and idle). Anything else is left for a later pass.
    Pending,
    /// A person asking for a `Queue` or `InProgress` task to start now. A
    /// task not already waiting to run is moved there as part of the start.
    Direct,
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

type ExecutionLogs = Arc<RwLock<HashMap<Uuid, Vec<AgentLogEntry>>>>;

/// The most output entries one execution keeps; older ones are dropped
/// first. The buffer lives in memory for the whole session and is what a
/// Task's "recent output" is read from, so it is bounded rather than left to
/// grow with however long an agent talks.
const EXECUTION_OUTPUT_LIMIT: usize = 1_000;

/// Append one entry to an execution's output.
async fn record_output(logs: &ExecutionLogs, execution_id: Uuid, level: LogLevel, message: String) {
    let mut logs = logs.write().await;
    let output = logs.entry(execution_id).or_default();
    output.push(AgentLogEntry { timestamp: chrono::Utc::now(), level, message });
    if output.len() > EXECUTION_OUTPUT_LIMIT {
        let excess = output.len() - EXECUTION_OUTPUT_LIMIT;
        output.drain(..excess);
    }
}

/// The non-blank text blocks of an assistant message, in order.
///
/// Claude Code without partial messages reports what the agent says only as
/// whole `assistant` messages, so this is where a run's own words come from.
fn assistant_text_blocks(content: &serde_json::Value) -> Vec<String> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .collect()
}

/// What a Task's agent is doing right now, and what its latest execution in
/// this session produced. Read-only; see [`TaskExecutor::task_run`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskRunSnapshot {
    /// Whether an agent-owning flow (an execution, or an AI review/fix) is
    /// live for the task, which is exactly when
    /// [`TaskExecutor::stop_task`] has something to end.
    pub live: bool,
    /// What SlashIt knows of the run: `starting`, `running` or `stopping`
    /// exactly while [`Self::live`], otherwise how the task's latest
    /// execution this session ended (`stopped` or `failed`). `None` when
    /// nothing has run for the task since the app started.
    pub status: Option<AgentStatus>,
    /// The task's most recent execution, if one ran in this session.
    pub last_execution: Option<ExecutionSnapshot>,
}

/// A run SlashIt owns right now: the task it works on and its non-terminal
/// state. The board reads these once when it opens and follows
/// [`AgentEvent::RunState`] after.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LiveRun {
    pub task_id: Uuid,
    pub status: AgentStatus,
}

/// Announces the end of a run on every way out of the future that owns it,
/// the unwinding one included, so a run never stays live in a listener
/// because one of many exits forgot to say so. Declared before the run's
/// capacity permit, so the permit is released first and an announcement never
/// precedes the capacity it frees.
struct RunEndAnnouncer {
    events: SharedEventSink,
    task_id: Uuid,
    end: AgentStatus,
    execution: Option<(ExecutionRecords, Uuid)>,
}

impl RunEndAnnouncer {
    fn new(events: SharedEventSink, task_id: Uuid) -> Self {
        Self { events, task_id, end: AgentStatus::Stopped, execution: None }
    }

    fn for_execution(
        events: SharedEventSink,
        task_id: Uuid,
        executions: ExecutionRecords,
        execution_id: Uuid,
    ) -> Self {
        Self { events, task_id, end: AgentStatus::Stopped, execution: Some((executions, execution_id)) }
    }
}

impl Drop for RunEndAnnouncer {
    fn drop(&mut self) {
        if let Some((executions, execution_id)) = &self.execution {
            match executions.try_write() {
                Ok(mut executions) => {
                    if let Some(execution) = executions.get_mut(execution_id) {
                        if matches!(execution.status, AgentStatus::Starting | AgentStatus::Running | AgentStatus::Stopping) {
                            execution.status = AgentStatus::Failed("execution owner panicked".to_string());
                            execution.stopped_at = Some(chrono::Utc::now());
                            self.end = execution.status.clone();
                        }
                    }
                }
                Err(_) if tokio::runtime::Handle::try_current().is_ok() => {
                    let events = self.events.clone();
                    let task_id = self.task_id;
                    let end = self.end.clone();
                    let executions = executions.clone();
                    let execution_id = *execution_id;
                    tokio::spawn(async move {
                        let mut executions = executions.write().await;
                        let mut end = end;
                        if let Some(execution) = executions.get_mut(&execution_id) {
                            if matches!(execution.status, AgentStatus::Starting | AgentStatus::Running | AgentStatus::Stopping) {
                                execution.status = AgentStatus::Failed("execution owner panicked".to_string());
                                execution.stopped_at = Some(chrono::Utc::now());
                                end = execution.status.clone();
                            }
                        }
                        events.agent_event(AgentEvent::RunState {
                            task_id: task_id.to_string(),
                            status: end,
                        });
                    });
                    return;
                }
                Err(_) => {
                    eprintln!("[executor] could not defer terminal cleanup for execution {execution_id}");
                }
            }
        }
        self.events.agent_event(AgentEvent::RunState {
            task_id: self.task_id.to_string(),
            status: self.end.clone(),
        });
    }
}

/// Marks a task whose run is being ended: from the moment its handle leaves
/// `running_handles`/`reviewing_handles` until its future has been joined
/// (or put back). Without it the run would read as gone while its process is
/// still shutting down.
struct StoppingMark {
    ending: Arc<std::sync::Mutex<HashMap<Uuid, Option<Uuid>>>>,
    task_id: Uuid,
}

impl Drop for StoppingMark {
    fn drop(&mut self) {
        self.ending.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.task_id);
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecutionSnapshot {
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// `None` while the execution's agent is still running.
    pub stopped_at: Option<chrono::DateTime<chrono::Utc>>,
    pub output: Vec<AgentLogEntry>,
}

pub struct TaskExecutor {
    tasks: Tasks,
    queue_manager: Arc<RwLock<QueueManager>>,
    executions: Arc<RwLock<HashMap<Uuid, AgentExecution>>>,
    running_handles: Arc<RwLock<HashMap<Uuid, RunningTask>>>,
    reviewing_handles: Arc<RwLock<HashMap<Uuid, ReviewOwner>>>,
    /// Tasks whose run is being ended right now; see [`StoppingMark`].
    ending_runs: Arc<std::sync::Mutex<HashMap<Uuid, Option<Uuid>>>>,
    /// PR-helper Claude invocations (`commands::pr::run_claude_pr_helper`)
    /// currently claiming a task. See [`PrHelperOwner`]/[`PrHelperLease`] for
    /// why this is a blocking `std::sync::Mutex` rather than the `tokio::
    /// sync::RwLock` the other two handle maps use.
    pr_helper_handles: Arc<std::sync::Mutex<HashMap<Uuid, PrHelperOwner>>>,
    /// Project Conversation Runs share this executor's admission gate and
    /// cancellation ownership. They are keyed by Conversation identity.
    project_runs: Arc<std::sync::Mutex<HashMap<Uuid, tokio::sync::watch::Sender<bool>>>>,
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
    logs: ExecutionLogs,
    projects: Arc<RwLock<HashMap<Uuid, crate::domain::Project>>>,
    repositories: Arc<RwLock<HashMap<Uuid, crate::domain::Repository>>>,
    workspace_registry: Arc<RwLock<crate::config::WorkspaceRegistry>>,
    storage: crate::config::Storage,
    worktree_manager: Arc<WorktreeManager>,
    events: SharedEventSink,
    pr_check_counter: std::sync::atomic::AtomicU32,
    /// Where the background poll asks about pull requests, and records what
    /// it heard. Shared with the refresh commands and the board.
    pr_statuses: Arc<crate::pr_status::PrStatuses>,
    /// When a start of each task last failed to record the worktree it was
    /// given. See [`TaskExecutor::refuse_while_unrecorded_backoff`].
    unrecorded_acquisitions: std::sync::Mutex<HashMap<Uuid, UnrecordedBackoff>>,
    /// Whether there is disk space to begin a new execution. Asked before
    /// anything a start does, and never about work already running.
    start_guard: Arc<StartGuard>,
    /// The tool calls of each task's current coding run, held until the
    /// write that ends the run records them on the task. See
    /// [`crate::queue::tool_activity`].
    run_tools: SharedRunTools,
}

type SharedRunTools = Arc<std::sync::Mutex<HashMap<Uuid, RunTools>>>;

/// Take the buffered tool calls of `task_id`'s run, if it made any.
fn take_run_tools(run_tools: &SharedRunTools, task_id: Uuid) -> Option<RunTools> {
    run_tools.lock().unwrap_or_else(|p| p.into_inner()).remove(&task_id)
}

/// What a coding run's events are forwarded with: its output to the logs and
/// the frontend, its tool calls to the run's [`RunTools`].
struct RunEventForwarder {
    task_id: Uuid,
    run: u32,
    execution_id: Uuid,
    logs: ExecutionLogs,
    events: SharedEventSink,
    tasks: Tasks,
    run_tools: SharedRunTools,
    working_dir: String,
}

/// A running [`RunEventForwarder`], to be drained before its run's end is
/// recorded. Dropping it drains it too, without waiting.
struct RunEventDrain {
    request: tokio::sync::oneshot::Sender<()>,
    forwarder: JoinHandle<()>,
}

impl RunEventDrain {
    /// Wait until every event the run has already sent is handled, then stop
    /// forwarding.
    ///
    /// Only for once nothing more will be sent -- after
    /// [`ClaudeRunner::wait`] has joined the stdout reader -- because the
    /// channel itself stays open as long as the runner does. What is left to
    /// handle is then only what is queued when the drain is asked for, so
    /// this never waits on the agent or on anything still writing. An event the channel dropped because the forwarder fell more than
    /// its capacity behind is lost, and forwarding carries on with the next.
    ///
    /// Two ends drain without the reader joined, and so keep only the calls
    /// sent before the drain: a stopped run, whose process was killed
    /// mid-output, and a `wait` that failed to wait for the process at all,
    /// where joining could wait on a process that is still running.
    async fn drain(self) {
        let _ = self.request.send(());
        let _ = self.forwarder.await;
    }
}

impl RunEventForwarder {
    fn spawn(self, events: tokio::sync::broadcast::Receiver<ClaudeEvent>) -> RunEventDrain {
        let (request, drain) = tokio::sync::oneshot::channel();
        RunEventDrain { request, forwarder: tokio::spawn(self.forward(events, drain)) }
    }

    async fn forward(
        self,
        mut events: tokio::sync::broadcast::Receiver<ClaudeEvent>,
        mut drain: tokio::sync::oneshot::Receiver<()>,
    ) {
        use tokio::sync::broadcast::error::{RecvError, TryRecvError};
        loop {
            tokio::select! {
                received = events.recv() => match received {
                    Ok(event) => self.handle(event).await,
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                },
                // Asked to drain, or dropped by a run that ended without
                // asking: handle what is queued now and stop. Only what is
                // queued now, so a writer that outlived the agent's process
                // cannot keep the drain going.
                _ = &mut drain => {
                    for _ in 0..events.len() {
                        match events.try_recv() {
                            Ok(event) => self.handle(event).await,
                            Err(TryRecvError::Lagged(_)) => {}
                            Err(TryRecvError::Empty | TryRecvError::Closed) => return,
                        }
                    }
                    return;
                }
            }
        }
    }

    async fn handle(&self, event: ClaudeEvent) {
        let task_id = self.task_id;
        let task_id_str = task_id.to_string();
        // Each entry is recorded before its event is emitted, so a listener
        // that reacts to the event by reading the output back finds the entry
        // already there.
        match &event {
            ClaudeEvent::TextDelta { text } => {
                record_output(&self.logs, self.execution_id, LogLevel::Info, text.clone()).await;
                self.events.agent_event(AgentEvent::Output { task_id: task_id_str, text: text.clone() });
            }
            ClaudeEvent::AssistantMessage { content } => {
                for text in assistant_text_blocks(content) {
                    record_output(&self.logs, self.execution_id, LogLevel::Info, text.clone()).await;
                    self.events.agent_event(AgentEvent::Output { task_id: task_id_str.clone(), text });
                }
            }
            ClaudeEvent::ToolUse { tool, input } => {
                // Only a complete call is buffered: a partial stream announces
                // the same call once without its input before the whole
                // message repeats it.
                if let Some(input) = input {
                    let mut buffered = self.run_tools.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(tools) = buffered.get_mut(&task_id).filter(|t| t.run() == self.run) {
                        tools.push(chrono::Utc::now(), tool, input, &self.working_dir);
                    }
                }
                record_output(&self.logs, self.execution_id, LogLevel::Info, format!("Using tool: {}", tool)).await;
                self.events.agent_event(AgentEvent::ToolUse { task_id: task_id_str, tool: tool.clone() });
            }
            ClaudeEvent::SystemInit { session_id, model, .. } => {
                // Capture actual model on the task
                if let Some(m) = model {
                    let mut tasks_w = self.tasks.write().await;
                    if let Some(t) = tasks_w.get_mut(&task_id) {
                        t.model = m.clone();
                    }
                }
                record_output(
                    &self.logs,
                    self.execution_id,
                    LogLevel::Info,
                    format!("Session started: {} (model: {})", session_id, model.as_deref().unwrap_or("unknown")),
                )
                .await;
            }
            ClaudeEvent::Error { message } => {
                record_output(&self.logs, self.execution_id, LogLevel::Error, message.clone()).await;
                self.events.agent_event(AgentEvent::Error { task_id: task_id_str, message: message.clone() });
            }
            _ => {}
        }
    }
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
    pub start_guard: Arc<StartGuard>,
    pub pr_statuses: Arc<crate::pr_status::PrStatuses>,
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
            ending_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            pr_helper_handles: Arc::new(std::sync::Mutex::new(HashMap::new())),
            project_runs: Arc::new(std::sync::Mutex::new(HashMap::new())),
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
            unrecorded_acquisitions: std::sync::Mutex::new(HashMap::new()),
            start_guard: config.start_guard,
            pr_statuses: config.pr_statuses,
            run_tools: Arc::new(std::sync::Mutex::new(HashMap::new())),
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
            + self.project_runs.lock().unwrap().len()
    }

    /// Register one Project-scoped fresh Run with the same capacity gate used
    /// by Task execution. A Task Worker passes `reserve_capacity = false`
    /// because its Task ownership lease already holds that capacity.
    pub async fn begin_project_run(&self, conversation_id: Uuid, reserve_capacity: bool) -> Result<ProjectRunLease, String> {
        let permit = if reserve_capacity {
            self.start_guard.check().await.map_err(|block| block.to_string())?;
            self.reconcile_admission().await;
            Some(self.admission.try_acquire().ok_or("No agent capacity is available right now")?)
        } else { None };
        let mut owners = self.project_runs.lock().unwrap();
        if owners.contains_key(&conversation_id) { return Err("A Run is already active for this Conversation".into()); }
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        owners.insert(conversation_id, cancel_tx.clone());
        Ok(ProjectRunLease { conversation_id, owners: self.project_runs.clone(), cancel_rx, _permit: permit })
    }

    pub fn project_run_is_live(&self, conversation_id: Uuid) -> bool {
        self.project_runs.lock().unwrap().contains_key(&conversation_id)
    }

    pub fn active_project_run_count(&self) -> usize { self.project_runs.lock().unwrap().len() }

    pub fn stop_project_run(&self, conversation_id: Uuid) -> Result<(), String> {
        let sender = self.project_runs.lock().unwrap().get(&conversation_id).cloned().ok_or("No live Run for this Conversation")?;
        sender.send(true).map_err(|_| "Conversation Run already ended".to_string())
    }

    /// Return the executor-owned provider flows in two forms for safety
    /// surfaces that also see the legacy ACP execution records:
    ///
    /// * execution ids are the records this executor itself inserted for
    ///   coding runs, so callers can avoid counting those records twice;
    /// * task ids are unrecorded provider flows (AI review/fix, PR helpers,
    ///   and a coding run before its execution record exists).
    ///
    /// PR side-effect reservations are deliberately absent: they still count
    /// as owned work for lifecycle protection and daemon shutdown, but they do
    /// not own a provider process and must not inflate an agent count.
    pub async fn active_agent_owners(&self) -> (HashSet<Uuid>, HashSet<Uuid>) {
        let mut execution_ids = HashSet::new();
        let mut task_ids = HashSet::new();

        for (task_id, run) in self.running_handles.read().await.iter() {
            if !run.handle.is_finished() {
                if let Some(execution_id) = run.execution_id {
                    execution_ids.insert(execution_id);
                } else {
                    task_ids.insert(*task_id);
                }
            }
        }
        for (task_id, run) in self.reviewing_handles.read().await.iter() {
            if !run.handle.is_finished() {
                task_ids.insert(*task_id);
            }
        }
        for (task_id, execution_id) in self.ending_runs.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            if let Some(execution_id) = execution_id {
                execution_ids.insert(*execution_id);
            } else {
                task_ids.insert(*task_id);
            }
        }
        for (task_id, owner) in self.pr_helper_handles.lock().unwrap().iter() {
            if owner.kind == PrOwnerKind::Helper {
                task_ids.insert(*task_id);
            }
        }

        (execution_ids, task_ids)
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
        // New work waits while disk space is critically low or cannot be
        // checked: nothing is promoted, so queued tasks stay `Queue`, and the
        // next pass asks again. Checked before any capacity is reserved, so
        // a paused pass holds no permit. Reviews above are continuations of
        // work already started and are not paused. Space running out between
        // this check and the start below leaves a promoted task `InProgress`
        // and idle; `spawn_task_execution` refuses it without an error, and a
        // later pass starts it once space returns.
        let auto_promote = self.queue_manager.read().await.config().auto_promote
            && self.start_guard.check().await.is_ok();
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

        // Poll pull request status every ~30s (10 cycles at 3s each)
        let counter = self.pr_check_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if counter.is_multiple_of(10) {
            self.poll_pull_requests().await;
        }
    }

    /// Ask GitHub about every pull request [`polled_pull_requests`] selects,
    /// keep each answer in the shared cache, and act on the durable part.
    ///
    /// A few are asked at a time ([`crate::pr_status::PrStatuses::refresh_each`]),
    /// each bounded by [`crate::pr_status::FETCH_TIMEOUT`], so one hung `gh`
    /// holds up this pass by at most that long and never stops the others
    /// from being asked. A failed answer is already recorded in the cache
    /// beside the last good one; nothing durable follows from it.
    async fn poll_pull_requests(&self) {
        let (keys, task_ids): (Vec<_>, Vec<_>) = {
            let tasks = self.tasks.read().await;
            polled_pull_requests(&tasks, &self.pr_statuses).into_iter().unzip()
        };
        let answers = self.pr_statuses.refresh_each(keys).await;

        for ((key, answer), task_ids) in answers.into_iter().zip(task_ids) {
            let Ok(status) = answer else { continue };
            for task_id in task_ids {
                self.apply_polled_pr_state(task_id, &key, status.state).await;
            }
        }
    }

    /// Act on the state GitHub reported for `key`, linked to `task_id`.
    ///
    /// Merged and closed finish or fail a task only in the columns a
    /// delivered pull request lives in (see [`in_pr_lifecycle`]); anywhere
    /// else the user has put the task back to work, and a poll does not take
    /// it away from them. The board still shows the state, from the cache.
    ///
    /// Outside those columns a closure is still recorded on the pull request
    /// reference, and noted in the task's activity; nothing else changes: no
    /// column, no error, no stop. SlashIt treats a recorded closure as final,
    /// in every column, and leaving the reference `OPEN` would have every
    /// later start of SlashIt ask about it again, and would tell what reads
    /// the record that it is still open. The only such reader that decides
    /// anything is a dependent task's start, which refuses a dependency
    /// whose branch is gone unless its pull request is recorded merged:
    /// recording the closure changes why it refuses, not whether.
    ///
    /// A merge heard there is not recorded. Recording it is the task's one
    /// automatic completion: the poll never asks about a reference recorded
    /// `MERGED` again, so a task moved back to a delivery column would never
    /// be finished. It would also let a dependent task start from the
    /// default base instead of refusing, on the word of a task whose work
    /// the user has taken back.
    ///
    /// Also what an explicit "check now" does (`commands::pr::refresh_pr_status`),
    /// so asking by hand has exactly the background poll's consequences.
    pub(crate) async fn apply_polled_pr_state(&self, task_id: Uuid, key: &crate::pr_status::PrKey, state: crate::pr_status::PrState) {
        use crate::pr_status::PrState;

        let Some(recorded) = state.as_recorded() else {
            return;
        };
        // Answers are applied once a whole batch has settled, so a newer
        // reading may have landed in the meantime, say an open after a
        // reopen. Only what the cache still says is acted on.
        if self.pr_statuses.get(key).and_then(|e| e.status).is_some_and(|s| s.state != state) {
            return;
        }
        let lifecycle = {
            let tasks = self.tasks.read().await;
            match tasks.get(&task_id) {
                // Its interrupted cleanup is reported on the task and only an
                // explicit lifecycle action finishes it; see
                // `polled_pull_requests`.
                Some(t) if t.cleanup_in_flight => return,
                Some(t) => in_pr_lifecycle(t),
                None => return,
            }
        };
        match state {
            PrState::Merged if lifecycle => self.complete_merged_task(task_id, key.number, recorded).await,
            PrState::Merged => {}
            _ => self.record_pr_poll_state(task_id, key, recorded).await,
        }
    }

    /// Record a non-`MERGED` pull request state observed by the poll above.
    ///
    /// Written only when it changes what the task records: a pull request
    /// still open thirty seconds later is not a change to the task, and
    /// rewriting its project's file for it every poll is churn.
    ///
    /// A closure also records its explanation as the task's error, without
    /// moving it, only where [`in_pr_lifecycle`] says the task is waiting on
    /// that pull request, judged on the task as it is when the write is
    /// staged.
    ///
    /// `CLOSED` is exactly what removes this ref from the poll's own
    /// selection, so it must never be visible in memory before it is durable:
    /// a lost write here would otherwise be a permanent, silent stop to
    /// polling a PR that is not actually recorded as closed anywhere the next
    /// start can see -- see issue #8. Any other state (`OPEN`, ...) is not
    /// selector-terminal and would self-heal on the next poll regardless; it
    /// is persisted the same way here only because it shares this call site,
    /// not because it shares the hazard.
    async fn record_pr_poll_state(&self, task_id: Uuid, key: &crate::pr_status::PrKey, state: &str) {
        let state_owned = state.to_string();
        let key_owned = key.clone();
        let revise = move |staged: &mut HashMap<Uuid, Task>| {
            let Some(t) = staged.get_mut(&task_id) else {
                return false;
            };
            let changed = record_observed_pr_state(t, &key_owned, &state_owned);
            if changed && state_owned == "CLOSED" && in_pr_lifecycle(t) {
                t.error_message = Some("PR was closed without merge".to_string());
            }
            changed
        };
        if let Err(e) =
            crate::lifecycle::record_if_changed(&self.tasks, &self.storage, task_id, &revise).await
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
        task.record_pr_state(number, state);
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
        // The poll chose to finish this task from its column before the lease
        // was held; a move out of the delivery columns since then is the
        // user taking it back, and wins. The next poll decides again.
        if !self.tasks.read().await.get(&task_id).is_some_and(in_pr_lifecycle) {
            return;
        }

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
            //
            // A pending restack of the task's published branch is the same
            // kind of blocker: it answers before anything is written, only the
            // user's resume or discard can end it, and the checkout it keeps is
            // its recovery evidence. The merge is latched for the same reasons;
            // the restack record, its backup and the checkout are not touched.
            Err(
                refusal @ (crate::lifecycle::TerminalizeRefusal::RepositoryUnresolved(_)
                | crate::lifecycle::TerminalizeRefusal::RepublishPending),
            ) => {
                let reason = refusal.to_string();
                let republish_pending =
                    matches!(refusal, crate::lifecycle::TerminalizeRefusal::RepublishPending);
                let latch = move |staged: &mut HashMap<Uuid, Task>| {
                    if let Some(t) = staged.get_mut(&task_id) {
                        Self::record_pr_state(t, number, state);
                        t.error_message = Some(if republish_pending {
                            crate::lifecycle::merged_while_republish_pending_message()
                        } else {
                            format!("The pull request is merged, but the task was not finished: {reason}")
                        });
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

        // Before any arm below reattaches, adopts, resumes or creates it: a
        // branch another task in the repository can claim is not this
        // task's to use. See `worktree::refuse_shared_task_branch`.
        if let Err(refused) = crate::worktree::refuse_shared_task_branch(
            &self.tasks,
            &self.projects,
            &self.repositories,
            task_id,
            &branch_name,
            repo_path,
        )
        .await
        {
            return (existing_branch, Err(refused));
        }

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
                    Some((branch, is_done, RecordedPr::of(dep_task)))
                })
        } else {
            None
        };
        let base_branch = match dependency {
            None => None,
            // Done with no pull request: its work reached main without one.
            Some((_, true, RecordedPr::None)) => None,
            // A dependency with a pull request whose branch git confirms is
            // gone is only taken as delivered when that pull request was
            // recorded as merged. A missing branch on its own proves
            // nothing: a pull request closed without merging, or one still
            // open, can lose its local branch just the same, and starting
            // from the default base then runs the agent without the work it
            // was queued to build on. Only a definite answer from git
            // counts. An invalid name, or a git that could not be asked,
            // still goes to the stacked path, which refuses it and says why.
            Some((branch, _, recorded))
                if recorded != RecordedPr::None
                    && WorktreeManager::local_branch_exists(repo_path, &branch).await == Ok(false) =>
            {
                if recorded != RecordedPr::Merged {
                    let why = if recorded == RecordedPr::ClosedUnmerged {
                        "was closed without being merged, so that work was never delivered"
                    } else {
                        "is not recorded as merged; move the dependency to PR Created so \
                         SlashIt can record its merge, or restore the branch"
                    };
                    return (
                        existing_branch,
                        Err(format!(
                            "it depends on the work on branch {branch}, which no longer exists \
                             locally, and the dependency's pull request {why}"
                        )),
                    );
                }
                self.events.agent_event(AgentEvent::Log {
                    task_id: task_id.to_string(),
                    level: LogLevel::Info,
                    message: format!(
                        "The dependency's pull request was merged and its branch {branch} \
                         no longer exists locally, so its work is delivered; starting an \
                         ordinary branch instead of stacking on it"
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
        // A new branch's `base_commit` is the exact commit it was created
        // at, as the creating call resolved and verified it -- not
        // re-resolved from the parent branch's (or `main`'s) ref afterwards,
        // which could observe it having moved in the meantime, exactly the
        // kind of attribution drift this field exists to prevent. `None` on
        // reattach: retry must keep comparing against the original starting
        // point, not wherever the branch has moved to since.
        let acquired = if existing_branch.is_some() {
            self.worktree_manager
                .reattach(repo_path, &branch_name)
                .await
                .map(|info| Acquired {
                    info,
                    what_happened: "Reattached worktree",
                    base_commit: None,
                    origin: None,
                    created_at: None,
                })
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
                    Ok(Acquired {
                        info: stacked.info,
                        what_happened,
                        created_at: (!stacked.resumed).then(|| stacked.dependency_tip.clone()),
                        base_commit: Some(stacked.dependency_tip),
                        origin: Some(BranchOrigin::Stacked {
                            parent_branch: parent_branch.to_string(),
                        }),
                    })
                }
                Err(e) => Err(format!(
                    "it depends on the work on branch {parent_branch}, and stacking on that \
                     branch failed: {e}"
                )),
            }
        } else {
            // An ordinary branch starts at the exact commit
            // `refs/remotes/origin/<D>` names for the repository's default
            // branch `D`, or, with no remote default, at the project's local
            // base branch, resolved once before the branch is created (see
            // `worktree::default_base::resolve_default_base`), and records
            // both. A repository with no such base refuses the task instead
            // of starting it from wherever the primary checkout happens to be.
            // An adopted worktree gets neither a starting commit nor an
            // origin, and whatever the task already recorded is kept: its
            // `HEAD` may hold the task's own commits, or whatever a hook
            // moved it to after a start refused it, and an earlier start that
            // never recorded it may have stacked it. A task with no starting
            // commit has no known diff boundary, as for one recorded before
            // starting commits were kept.
            let project_base = self.project_base_of(task_id).await;
            match self
                .worktree_manager
                .create_or_adopt(repo_path, &branch_name, project_base.as_ref())
                .await
            {
                Ok((info, Some(base))) => Ok(Acquired {
                    info,
                    what_happened: "Created worktree",
                    created_at: Some(base.commit.clone()),
                    origin: Some(base.branch_origin()),
                    base_commit: Some(base.commit),
                }),
                Ok((info, None)) => Ok(Acquired {
                    info,
                    what_happened: "Adopted worktree",
                    base_commit: None,
                    origin: None,
                    created_at: None,
                }),
                Err(e) => Err(e),
            }
        };
        (existing_branch, acquired)
    }

    /// The local base (`domain::ProjectBase`) of the project `task_id`
    /// belongs to, as recorded now.
    async fn project_base_of(&self, task_id: Uuid) -> Option<crate::domain::ProjectBase> {
        let project_id = self.tasks.read().await.get(&task_id)?.project_id;
        self.projects.read().await.get(&project_id)?.base.clone()
    }

    /// Record a run's start on the task, durably before the board shows it:
    /// the worktree it was given, the `Coding` phase, and the run's
    /// [`ActivityKind::RunStarted`] with the number it is given here.
    ///
    /// One write, so a run has a number only if its start is on disk: an agent
    /// is never launched for a run the timeline does not have, and the next
    /// run's number, counted from the recorded starts, is never one already
    /// used.
    ///
    /// The starting commit and the branch's origin are written only when
    /// this start created (or resumed) the branch. A reattach leaves the ones
    /// recorded by the start that created it, so a retry or a restart keeps
    /// the stack parent the branch was actually built on.
    ///
    /// `Err` means nothing was recorded and the agent must not start. A
    /// checkout this start created is taken back first (see
    /// [`WorktreeManager::undo_created_checkout`]): left behind unrecorded,
    /// the next start would adopt it, and an adopted checkout records no
    /// starting commit and no origin. The message says what was kept, if
    /// anything.
    async fn record_run_start(
        &self,
        task_id: Uuid,
        repo_path: &str,
        acquired: Acquired,
    ) -> Result<RunStart, String> {
        let run = std::sync::atomic::AtomicU32::new(0);
        let amend = |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&task_id) {
                t.worktree_path = Some(acquired.info.path.clone());
                t.branch_name = Some(acquired.info.branch.clone());
                if let Some(base_commit) = &acquired.base_commit {
                    t.base_commit = Some(base_commit.clone());
                }
                if let Some(origin) = &acquired.origin {
                    t.branch_origin = Some(origin.clone());
                }
                t.phase = TaskPhase::Coding;
                t.phase_progress = RUN_START_PROGRESS;
                let next = t.next_run();
                let addressing_feedback = !t.human_review.pending_feedback().is_empty();
                t.record_activity(ActivityKind::RunStarted { run: next, addressing_feedback });
                run.store(next, std::sync::atomic::Ordering::Relaxed);
            }
        };
        match crate::lifecycle::record(&self.tasks, &self.storage, task_id, &amend).await {
            // `record` fails for a task that is not on the board, so a
            // successful write always ran the amend on it.
            Ok(()) => Ok(RunStart {
                working_dir: acquired.info.path,
                run: run.load(std::sync::atomic::Ordering::Relaxed),
            }),
            Err(e) => {
                let outcome = self
                    .worktree_manager
                    .release_unrecorded(repo_path, &acquired.info, acquired.created_at.as_deref())
                    .await;
                Err(format!(
                    "The task's worktree could not be recorded ({e}), so the task was not \
                     started; {outcome}"
                ))
            }
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
    ///
    /// `external_permit` is a reservation already taken by the caller --
    /// `check_and_execute`'s reserve-then-promote loop, transferring a
    /// permit straight from promotion into the execution it was reserved
    /// for. `None` means no such reservation exists (a manual `execute_task`
    /// call, or a task that was already `InProgress` before this poll pass),
    /// in which case a fresh permit is drawn from [`Self::admission`] here,
    /// so every path that can start an agent -- the poller and a direct
    /// command alike -- goes through the same gate.
    ///
    /// `Ok(true)` means an execution was registered, `Ok(false)` that this
    /// pass declined and the task is left for a later one, and `Err` that
    /// the task was refused, saying why, so a caller that answers a person
    /// can say what happened rather than assume it worked. A refusal for disk
    /// space leaves the task exactly as it was; most others record an error
    /// on it.
    async fn spawn_task_execution(
        &self,
        task_id: Uuid,
        external_permit: Option<AdmissionPermit>,
    ) -> Result<bool, String> {
        let Some(lease) = self.lifecycle.try_acquire(task_id).await else {
            return Ok(false); // another lifecycle operation owns this task right now
        };
        self.start_execution_under_lease(&lease, task_id, StartRequest::Pending, external_permit)
            .await
    }

    /// [`Self::spawn_task_execution`] for a caller that already holds
    /// `task_id`'s lifecycle lease, which `lease` stands for.
    ///
    /// The one place a task execution is decided, under the lease, so no
    /// other lifecycle transition can land between the answer and the start
    /// it allowed. A live owner, the task's own state, the unrecorded-worktree
    /// backoff and disk space are all asked before anything about the task is
    /// written, so any of those refusals leaves memory and disk exactly as
    /// they were.
    async fn start_execution_under_lease(
        &self,
        _lease: &crate::lifecycle::LifecycleLease,
        task_id: Uuid,
        request: StartRequest,
        external_permit: Option<AdmissionPermit>,
    ) -> Result<bool, String> {
        // One agent per task. Asked before the task is read: a run's future
        // writes its outcome without the lease and only then leaves
        // `running_handles`, so once this says nothing is running, the read
        // below sees that outcome rather than the status it replaced.
        if self.is_task_running(task_id).await {
            return match request {
                StartRequest::Pending => Ok(false),
                StartRequest::Direct => Err(format!("task {task_id} already has an agent working on it")),
            };
        }

        // Re-read under the lease. Whatever the caller saw before waiting
        // for it may no longer be true, and a task that has since been
        // stopped, finished or quarantined must not be started off that stale
        // reading.
        let promote = {
            let tasks = self.tasks.read().await;
            let task = tasks.get(&task_id);
            // An unfinished restack of the task's published branch owns it
            // (see `Task::republish_pending_refusal`): no agent may advance
            // the branch under it.
            if let Some(refusal) = task.and_then(Task::republish_pending_refusal) {
                return match request {
                    StartRequest::Pending => Ok(false),
                    StartRequest::Direct => Err(refusal),
                };
            }
            match request {
                StartRequest::Pending => match task {
                    Some(task) if Self::is_pending(task) => false,
                    _ => return Ok(false),
                },
                StartRequest::Direct => {
                    let task = task.ok_or("Task not found")?;
                    if task.status != TaskStatus::InProgress && task.status != TaskStatus::Queue {
                        return Err(format!("Task in {:?}, expected InProgress or Queue", task.status));
                    }
                    // See `is_pending`: the recorded checkout is untrustworthy
                    // until the interrupted cleanup is resolved, and starting
                    // an agent in it is exactly what the quarantine exists to
                    // prevent.
                    if task.cleanup_in_flight {
                        return Err(
                            "This task has a worktree cleanup that was interrupted and not yet \
                             resolved; it needs attention before it can run again"
                                .to_string(),
                        );
                    }
                    !Self::is_pending(task)
                }
            }
        };
        self.refuse_while_unrecorded_backoff(task_id)?;

        // Disk space, before capacity and before anything below writes the
        // task, creates a checkout, a branch or an agent. A refusal changes
        // nothing about the task, and drops `external_permit` unused, so a
        // paused task holds no capacity and starts on a later pass once space
        // is back.
        if let Err(block) = self.start_guard.check().await {
            return Err(block.to_string());
        }

        // Capacity, before any of the worktree/prompt work below runs: a
        // reservation already made for this exact task is honored outright;
        // otherwise this call draws its own fresh permit, and declines --
        // same as every other refusal in this function -- if none is free.
        // Dropping an unused `external_permit` on any decline path below
        // returns its capacity immediately; nothing here can lose it.
        let permit = match external_permit {
            Some(p) => Some(p),
            None => self.admission.try_acquire(),
        };

        // A direct start of a task that is not already waiting to run makes
        // it wait to run: the same durable promotion the poller makes, so the
        // board and the file agree. Written only after the disk check, and
        // under the lease, so no stop or finish can be overwritten by it.
        // From here on the task is durably waiting to run: a capacity
        // deferral below leaves it for a later pass, and a failure to attach
        // a checkout records an error on it.
        if promote {
            // Checked again inside the write, which is what actually commits,
            // so a status that changed since the read above is left alone.
            let promoted = std::sync::atomic::AtomicBool::new(false);
            let amend = |staged: &mut HashMap<Uuid, Task>| {
                if let Some(task) = staged.get_mut(&task_id) {
                    if matches!(task.status, TaskStatus::Queue | TaskStatus::InProgress)
                        && !task.cleanup_in_flight
                    {
                        QueueManager::apply_promotion(task);
                        promoted.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            };
            crate::lifecycle::record(&self.tasks, &self.storage, task_id, &amend)
                .await
                .map_err(|e| {
                    format!("task {task_id} was not started: moving it to In Progress could not be recorded: {e}")
                })?;
            if !promoted.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(format!(
                    "task {task_id} changed while it was being started, so it was not started"
                ));
            }
        }

        let Some(permit) = permit else {
            return Ok(false); // no capacity right now; the next pass tries again
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
                Self::set_task_error_static(&self.tasks, &self.storage, &self.events, task_id, &e, None).await;
                return Err(format!("Cannot resolve working directory: {e}"));
            }
        };

        let (existing_branch, acquired) = self.acquire_task_worktree(task_id, &repo_path).await;

        let acquired = match acquired {
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
                Self::set_task_error_static(&self.tasks, &self.storage, &self.events, task_id, &message, None).await;
                return Err(message);
            }
        };

        self.events.agent_event(AgentEvent::Log {
            task_id: task_id.to_string(),
            level: LogLevel::Info,
            message: format!("{}: {}", acquired.what_happened, acquired.info.path),
        });
        if existing_branch.is_none() && acquired.base_commit.is_none() {
            self.events.agent_event(AgentEvent::Log {
                task_id: task_id.to_string(),
                level: LogLevel::Warn,
                message: "No starting commit is known for this task's worktree; its diff \
                          boundary will be reported as unknown rather than guessed"
                    .to_string(),
            });
        }
        let RunStart { working_dir, run } = match self.record_run_start(task_id, &repo_path, acquired).await {
            Ok(start) => {
                self.unrecorded_backoff_lock().remove(&task_id);
                start
            }
            Err(message) => {
                self.events.agent_event(AgentEvent::Error {
                    task_id: task_id.to_string(),
                    message: message.clone(),
                });
                // Usually refused by the same storage that refused the
                // record, which leaves the task pending; the backoff is what
                // then keeps the poller from creating and taking back a
                // checkout on every pass.
                Self::set_task_error_static(&self.tasks, &self.storage, &self.events, task_id, &message, None).await;
                let mut backoff = self.unrecorded_backoff_lock();
                // Entries otherwise leave only when their own task is looked
                // up again; this drops the expired ones of tasks that never
                // are, such as a task deleted meanwhile.
                backoff.retain(|_, b| b.since.elapsed() < UNRECORDED_ACQUISITION_BACKOFF);
                backoff.insert(
                    task_id,
                    UnrecordedBackoff { since: std::time::Instant::now(), announced: false },
                );
                drop(backoff);
                return Err(message);
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
                // Deleted while its worktree was being attached.
                None => return Ok(false),
            }
        };

        self.run_tools
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(task_id, RunTools::new(run));
        self.events.agent_event(AgentEvent::PhaseChange {
            task_id: task_id.to_string(),
            phase: TaskPhase::Coding,
            progress: RUN_START_PROGRESS,
        });

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
        let run_tools = self.run_tools.clone();
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        // Named before the future exists so the registered run can already be
        // asked its status; the future records the execution under this id.
        let execution_id = Uuid::new_v4();

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
            let mut announcer = RunEndAnnouncer::for_execution(
                events.clone(),
                task_id,
                executions.clone(),
                execution_id,
            );
            let _permit = permit;

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
            events.agent_event(AgentEvent::RunState {
                task_id: task_id.to_string(),
                status: AgentStatus::Starting,
            });

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
                    record_output(&logs, execution_id, LogLevel::Error, msg.clone()).await;
                    let ended = RunEnd { run, tools: take_run_tools(&run_tools, task_id), timeline_reason: None };
                    Self::set_task_error_static(&tasks, &storage, &events, task_id, &msg, Some(ended)).await;
                    // This early return skips the removal after `runner.wait()`
                    // below, so it must remove itself here or this slot never
                    // frees up.
                    running_handles.write().await.remove(&task_id);
                    if let Some(exec) = executions.write().await.get_mut(&execution_id) {
                        exec.status = AgentStatus::Failed(msg.clone());
                        exec.stopped_at = Some(chrono::Utc::now());
                    }
                    announcer.end = AgentStatus::Failed(msg.clone());
                    // Announced last, as the end of every run is: see below.
                    events.agent_event(AgentEvent::Error {
                        task_id: task_id.to_string(),
                        message: msg,
                    });
                    return;
                }
            };

            // Update status
            if let Some(exec) = executions.write().await.get_mut(&execution_id) {
                exec.status = AgentStatus::Running;
            }
            events.agent_event(AgentEvent::RunState {
                task_id: task_id.to_string(),
                status: AgentStatus::Running,
            });

            events.agent_event(AgentEvent::PhaseChange {
                task_id: task_id.to_string(),
                phase: TaskPhase::Coding,
                progress: 10,
            });

            // Forward the run's events to the frontend, buffering its tool
            // calls for the write that ends the run.
            let mut event_drain = Some(RunEventForwarder {
                task_id,
                run,
                execution_id,
                logs: logs.clone(),
                events: events.clone(),
                tasks: tasks.clone(),
                run_tools: run_tools.clone(),
                working_dir: working_dir.clone(),
            }
            .spawn(runner.subscribe()));

            // Wait for the run to finish, or for a stop to end it early.
            //
            // Biased so that a process which has already exited is reported as
            // the completion it is: when both arms are ready the run finished
            // before the stop reached it, and calling that a cancellation would
            // throw away a result the agent had already produced.
            // How the run ended, announced only once the run is fully over:
            // the failure recorded, the handle gone and the execution marked
            // stopped. A listener that reacts by reading the task back then
            // sees where it settled, not a run that still looks live.
            let mut ending: Option<AgentEvent> = None;
            let stopped = tokio::select! {
                biased;

                result = runner.wait() => {
                    // `wait` has joined the run's stdout reader, so every
                    // event of this run is already sent. Handling them all
                    // before the run's end is recorded is what puts its last
                    // tool calls on the timeline.
                    if let Some(drain) = event_drain.take() {
                        drain.drain().await;
                    }
                    match result {
                        Ok(_) => 'finished: {
                            // A run whose work could not be committed has
                            // not finished its task: it fails the same way a
                            // failed run does, and is not reviewed.
                            if let Err(message) =
                                Self::commit_changes(&tasks, task_id, &working_dir_for_commit, &events).await
                            {
                                record_output(&logs, execution_id, LogLevel::Error, message.clone()).await;
                                let ended = RunEnd { run, tools: take_run_tools(&run_tools, task_id), timeline_reason: None };
                                Self::set_task_error_static(&tasks, &storage, &events, task_id, &message, Some(ended)).await;
                                ending = Some(AgentEvent::Error {
                                    task_id: task_id.to_string(),
                                    message,
                                });
                                break 'finished;
                            }

                            // The move to AI review, the run's end and its
                            // tool calls are one durable write, published
                            // only once it is on disk.
                            let tools = take_run_tools(&run_tools, task_id);
                            ending = Some(
                                match Self::record_run_completed(&tasks, &storage, task_id, run, tools.as_ref()).await {
                                    Ok(()) => {
                                        let completed = "Agent completed — moving to AI review".to_string();
                                        record_output(&logs, execution_id, LogLevel::Info, completed.clone()).await;
                                        AgentEvent::Completed {
                                            task_id: task_id.to_string(),
                                            success: true,
                                            message: Some(completed),
                                        }
                                    }
                                    Err(e) => {
                                        // Nothing was published: memory and
                                        // disk both still have the run in
                                        // progress. A finish that cannot be
                                        // recorded ends the run as a failure,
                                        // the durable transition every other
                                        // unfinished run takes.
                                        let message = format!(
                                            "The agent finished and its work was committed, but moving the \
                                             task to AI review could not be saved, so it was not reviewed: {e}"
                                        );
                                        record_output(&logs, execution_id, LogLevel::Error, message.clone()).await;
                                        let ended = RunEnd { run, tools, timeline_reason: None };
                                        Self::set_task_error_static(&tasks, &storage, &events, task_id, &message, Some(ended)).await;
                                        AgentEvent::Error { task_id: task_id.to_string(), message }
                                    }
                                },
                            );
                        }
                        Err(err_msg) => {
                            // Nothing on stderr or in the result event said
                            // why: quote the end of the output instead, cut to
                            // one bounded line so a transcript never becomes
                            // the error.
                            let full_msg = if err_msg.contains("no details") {
                                let stdout_output = runner.get_output().await;
                                let last_lines: String = stdout_output.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
                                if last_lines.is_empty() {
                                    err_msg.clone()
                                } else {
                                    format!(
                                        "{} — {}",
                                        err_msg,
                                        crate::agents::runner::truncate_one_line_tail(&last_lines, 400)
                                    )
                                }
                            } else {
                                err_msg.clone()
                            };
                            record_output(&logs, execution_id, LogLevel::Error, full_msg.clone()).await;
                            // The quoted output stays on the task's error; the
                            // timeline keeps only why the run ended.
                            let ended = RunEnd {
                                run,
                                tools: take_run_tools(&run_tools, task_id),
                                timeline_reason: (full_msg != err_msg).then(|| err_msg.clone()),
                            };
                            Self::set_task_error_static(&tasks, &storage, &events, task_id, &full_msg, Some(ended)).await;
                            ending = Some(AgentEvent::Error {
                                task_id: task_id.to_string(),
                                message: full_msg,
                            });
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
                let message = "Stopped — ending the agent".to_string();
                record_output(&logs, execution_id, LogLevel::Info, message.clone()).await;
                events.agent_event(AgentEvent::Log {
                    task_id: task_id.to_string(),
                    level: LogLevel::Info,
                    message,
                });
            }

            // Cleanup — the one path every outcome reaches, cancellation
            // included.
            let _ = runner.kill().await;
            // A stopped run's calls are recorded by `stop_task` once this
            // future has ended, so the ones already sent are handled first.
            if let Some(drain) = event_drain.take() {
                drain.drain().await;
            }
            running_handles.write().await.remove(&task_id);

            if let Some(exec) = executions.write().await.get_mut(&execution_id) {
                // A run that ended in failure is recorded as one, as a run
                // that failed to start already is.
                exec.status = match &ending {
                    Some(AgentEvent::Error { message, .. }) => AgentStatus::Failed(message.clone()),
                    _ => AgentStatus::Stopped,
                };
                exec.stopped_at = Some(chrono::Utc::now());
                announcer.end = exec.status.clone();
            }

            if let Some(ending) = ending {
                events.agent_event(ending);
            }
        });

        handles.insert(task_id, RunningTask { handle, cancel, execution_id: Some(execution_id) });
        Ok(true)
    }

    /// Refuse to start `task_id` again while a recent start of it could not
    /// record its worktree.
    ///
    /// Such a start marks the task errored, but that write usually fails on
    /// the same storage, which leaves the task pending in memory and eligible
    /// on every poll. Each attempt then creates a checkout and takes it back,
    /// every three seconds, for as long as the storage refuses writes. The
    /// backoff bounds that to one attempt per [`UNRECORDED_ACQUISITION_BACKOFF`]
    /// while still retrying on its own once the storage recovers. It is kept
    /// in memory only, like the handle maps: it is about what this process
    /// just saw, and a restart retries straight away.
    ///
    /// The first refusal inside a backoff is also logged on the task, once,
    /// because the poller does not report what a start returns.
    fn refuse_while_unrecorded_backoff(&self, task_id: Uuid) -> Result<(), String> {
        let mut backoff = self.unrecorded_backoff_lock();
        let Some(entry) = backoff.get_mut(&task_id) else {
            return Ok(());
        };
        let waited = entry.since.elapsed();
        if waited >= UNRECORDED_ACQUISITION_BACKOFF {
            backoff.remove(&task_id);
            return Ok(());
        }
        let announce = !std::mem::replace(&mut entry.announced, true);
        drop(backoff);
        let message = format!(
            "task {task_id} was not started: its worktree could not be recorded {}s ago, and it \
             is tried again {}s from now",
            waited.as_secs(),
            (UNRECORDED_ACQUISITION_BACKOFF - waited).as_secs().max(1),
        );
        if announce {
            self.events.agent_event(AgentEvent::Log {
                task_id: task_id.to_string(),
                level: LogLevel::Warn,
                message: message.clone(),
            });
        }
        Err(message)
    }

    fn unrecorded_backoff_lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, UnrecordedBackoff>> {
        // Poisoning cannot leave this map inconsistent: every critical
        // section is a single insert, remove or lookup.
        match self.unrecorded_acquisitions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Start `task_id` now, at a person's request.
    ///
    /// Decided entirely under the task's lifecycle lease, by the same
    /// [`Self::start_execution_under_lease`] the poller uses, so eligibility,
    /// the disk check, the move to `InProgress` and the start itself are one
    /// transition: a concurrent stop or finish either lands before it, and
    /// this sees the result, or waits until it is over.
    pub async fn execute_task(&self, task_id: Uuid) -> Result<(), String> {
        // A paused start is answered without waiting for the lease. This
        // writes nothing and can only refuse early: the check under the lease
        // is the one that allows a start.
        self.start_guard.check().await.map_err(|block| block.to_string())?;
        // A direct call, same as the poller: no pre-existing reservation, so
        // this draws its own fresh permit from the shared gate. A frontend
        // pre-check believing capacity is free is not authority here -- this
        // is.
        self.reconcile_admission().await;
        let lease = self.lifecycle.acquire(task_id).await?;
        if !self
            .start_execution_under_lease(&lease, task_id, StartRequest::Direct, None)
            .await?
        {
            // No capacity is free. The task is waiting to run, which is what
            // the three-second poll looks for, so this is a deferral rather
            // than a loss. Saying `Ok` would tell the caller an agent is
            // running when none is. A refusal is not a deferral, and returns
            // its own reason above.
            return Err(format!(
                "task {task_id} could not be started right now; it is waiting to run and \
                 the executor will pick it up on its next pass"
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
    /// Reaches whichever of an execution, AI review/fix, or PR-helper flow
    /// currently owns the task -- the task-exclusivity contract (see
    /// [`crate::queue::admission`]) means at most one of `running_handles`
    /// and `reviewing_handles` can have a live entry for it at once, so this
    /// tries the execution map first and only falls through to the review
    /// map if that found nothing. A review's reviewer-then-fix sequence is
    /// one future behind one [`ReviewOwner`]; cancelling it ends whichever
    /// `ClaudeRunner` it currently owns, the same as [`RunningTask`]. PR
    /// helpers are ended after those maps through their lease, and are not
    /// settled until that lease has observed the provider process finish.
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
        let helper_ended = self.end_pr_helper_owner_under_lease(task_id).await?;

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
        let helper_status = if helper_ended {
            self.tasks.read().await.get(&task_id).map(|task| task.status.clone())
        } else {
            None
        };
        let from_status = ended.or(helper_status).unwrap_or(TaskStatus::InProgress);
        let tools = take_run_tools(&self.run_tools, task_id);
        Self::settle_stopped_static(&self.tasks, &self.storage, task_id, from_status, tools).await
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
        //
        // The mark goes up while the map's write guard is still held, so a
        // reader never sees the run absent from the map without also seeing
        // that it is being ended.
        let mut mark = None;
        let running = {
            let mut handles = self.running_handles.write().await;
            let owner = handles.remove(&task_id);
            if owner.is_some() {
                mark = Some(self.mark_ending(task_id, owner.as_ref().and_then(|run| run.execution_id)));
            }
            owner
        };

        if let Some(mut owner) = running {
            self.announce_stopping(task_id, owner.execution_id).await;
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
                Ok(_) => self.announce_stopped(task_id),
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
        let reviewing = {
            let mut handles = self.reviewing_handles.write().await;
            let owner = handles.remove(&task_id);
            if owner.is_some() {
                mark = Some(self.mark_ending(task_id, None));
            }
            owner
        };

        if let Some(mut owner) = reviewing {
            self.announce_stopping(task_id, None).await;
            let _ = owner.cancel.send(true);
            match tokio::time::timeout(AGENT_SHUTDOWN_TIMEOUT, &mut owner.handle).await {
                Ok(_) => self.announce_stopped(task_id),
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
        drop(mark);

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
        if self.tasks.read().await.get(&task_id).is_some_and(|t| t.republish_pending_refusal().is_some()) {
            return Err(PrHelperRefusal::RepublishPending);
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
        if let Some(refusal) = self.tasks.read().await.get(&task_id).and_then(Task::republish_pending_refusal) {
            return Err(refusal);
        }
        self.begin_republish_under_lease(task_id).await
    }

    /// [`Self::begin_pr_side_effect_under_lease`] without the refusal for an
    /// unfinished restack: the restack's own Resume and Discard are the one
    /// thing allowed to act on a task that is in that state.
    pub async fn begin_republish_under_lease(
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
        if kind == PrOwnerKind::Helper {
            self.events.agent_event(AgentEvent::RunState {
                task_id: task_id.to_string(),
                status: AgentStatus::Running,
            });
        }

        PrHelperLease {
            task_id,
            kind,
            cancel_rx,
            done_tx: Some(done_tx),
            _permit: permit,
            handles: self.pr_helper_handles.clone(),
            events: self.events.clone(),
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
                .map(|o| (o.cancel.clone(), o.done.clone(), o.kind))
        };
        let Some((cancel, mut done, kind)) = entry else {
            return Ok(false);
        };
        let _ = cancel.send(true);
        if kind == PrOwnerKind::Helper {
            self.events.agent_event(AgentEvent::RunState {
                task_id: task_id.to_string(),
                status: AgentStatus::Stopping,
            });
        }
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
        tools: Option<RunTools>,
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
            if let Some(tools) = tools {
                tools.apply_to(&mut stopped);
            }
            stopped.record_activity(ActivityKind::Stopped { from: from_status.column() });
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
        // fix-agent/commit sequence against a directory that is gone.
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
        // An unfinished restack of the task's published branch owns it, and
        // the fix agent's commit would advance the branch under it.
        if self.tasks.read().await.get(&task_id).is_some_and(|t| t.republish_pending_refusal().is_some()) {
            return;
        }

        // This review's number on the task's timeline. Only one review runs
        // for a task at a time, under this lease, so no other can take it.
        let review = self.tasks.read().await.get(&task_id).map_or(1, Task::next_review);

        // The task's own worktree, or no review at all.
        //
        // This used to fall back to the repository path, and a review is not a
        // read-only operation: the fix agent runs in this directory and the
        // block below finishes by committing every change in it. Against the
        // user's own checkout that commits whatever they had uncommitted
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
            let trail = vec![(
                chrono::Utc::now(),
                ActivityKind::AiReviewSkipped { review, reason: "this task has no worktree of its own".to_string() },
            )];
            Self::transition_to_human_review(
                &self.tasks,
                &self.storage,
                &self.events,
                task_id,
                Some(signoff),
                trail,
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
            let _announcer = RunEndAnnouncer::new(events.clone(), task_id);
            let _permit = permit;
            // A review flow keeps no execution record; owning its handle is
            // what makes it live, and it is working from the first moment.
            events.agent_event(AgentEvent::RunState {
                task_id: task_id.to_string(),
                status: AgentStatus::Running,
            });

            let task_id_str = task_id.to_string();
            let started = (chrono::Utc::now(), ActivityKind::AiReviewStarted { review });

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
                    let trail = vec![(
                        chrono::Utc::now(),
                        ActivityKind::AiReviewSkipped { review, reason: format!("the task's changes could not be read: {e}") },
                    )];
                    Self::transition_to_human_review(&tasks, &storage, &events, task_id, Some(signoff), trail).await;
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
                let trail = vec![(
                    chrono::Utc::now(),
                    ActivityKind::AiReviewSkipped { review, reason: "no changes to review".to_string() },
                )];
                Self::transition_to_human_review(&tasks, &storage, &events, task_id, None, trail).await;
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
                    let trail = vec![
                        started.clone(),
                        (chrono::Utc::now(), ActivityKind::AiReviewFailed { review, reason: reason.clone() }),
                    ];
                    Self::transition_to_human_review(&tasks, &storage, &events, task_id, Some(signoff), trail).await;
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
                let verdict_at = chrono::Utc::now();
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

                let fix_started = chrono::Utc::now();
                let fix_run = Self::run_cancellable_agent(
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

                // Whether the fix agent changed any file, when it succeeded.
                let mut fix_changed_nothing = false;
                // Why the fixes were not applied, when they were not.
                let fix_failure = match fix_outcome(fix_run) {
                    FixOutcome::Applied => {
                        // A stop that arrived while the fix agent finished
                        // owns the checkout now: commit nothing into it.
                        if *cancelled.borrow() {
                            reviewing_handles.write().await.remove(&task_id);
                            return;
                        }
                        let (title, branch) = {
                            let tasks_r = tasks.read().await;
                            tasks_r
                                .get(&task_id)
                                .map(|t| (t.title.clone(), t.branch_name.clone()))
                                .unwrap_or_default()
                        };
                        let message = format!("task: {title} (with review fixes)");
                        let committed = match &branch {
                            Some(branch) => crate::worktree::commit_checkout(&working_dir, branch, &message).await,
                            None => Err(NO_TASK_BRANCH.to_string()),
                        };
                        match committed {
                            Ok(CheckoutCommit::Committed { commit }) => {
                                events.agent_event(AgentEvent::Log {
                                    task_id: task_id_str.clone(),
                                    level: LogLevel::Info,
                                    message: format!("Review fixes committed as {commit}"),
                                });
                                None
                            }
                            Ok(CheckoutCommit::NothingToCommit) => {
                                events.agent_event(AgentEvent::Log {
                                    task_id: task_id_str.clone(),
                                    level: LogLevel::Info,
                                    message: "The fix agent changed no files, so there was \
                                              nothing to commit"
                                        .to_string(),
                                });
                                fix_changed_nothing = true;
                                None
                            }
                            Err(e) => {
                                let message = format!(
                                    "The review fixes could not be committed: {e}. They remain \
                                     as uncommitted edits in the task checkout."
                                );
                                events.agent_event(AgentEvent::Log {
                                    task_id: task_id_str.clone(),
                                    level: LogLevel::Error,
                                    message: message.clone(),
                                });
                                Some(message)
                            }
                        }
                    }
                    FixOutcome::Cancelled => {
                        // Cancelled during the fix agent's run: it has
                        // already been killed by `run_cancellable_agent`.
                        // Nothing durable is recorded on this path either.
                        reviewing_handles.write().await.remove(&task_id);
                        return;
                    }
                    FixOutcome::Failed(reason) => {
                        // The fix agent has full tools, so a run that failed
                        // part-way may still have edited the checkout.
                        let message = format!(
                            "{reason}. The task checkout may hold partial edits from the fix \
                             agent that were not recorded."
                        );
                        events.agent_event(AgentEvent::Log {
                            task_id: task_id_str.clone(),
                            level: LogLevel::Error,
                            message: message.clone(),
                        });
                        Some(message)
                    }
                };

                if *cancelled.borrow() {
                    reviewing_handles.write().await.remove(&task_id);
                    return;
                }

                let issue_count = findings
                    .lines()
                    .filter(|l| l.starts_with("- ISSUE:") || l.starts_with("ISSUE:"))
                    .count()
                    .try_into()
                    .unwrap_or(u32::MAX);
                let issues: Vec<String> = fix_failure.iter().cloned()
                    .chain(
                        findings.lines()
                            .filter(|l| l.starts_with("- ISSUE:") || l.starts_with("ISSUE:"))
                            .map(|l| l.to_string()),
                    )
                    .collect();

                let signoff = QaSignoff {
                    status: if fix_failure.is_none() { QaStatus::FixesApplied } else { QaStatus::Rejected },
                    issues_found: issues,
                    timestamp: chrono::Utc::now(),
                    session_id: Uuid::new_v4(),
                };

                let trail = vec![
                    started.clone(),
                    (verdict_at, ActivityKind::AiReviewChangesRequested { review, issues: issue_count }),
                    (fix_started, ActivityKind::AiFixStarted { review }),
                    (
                        chrono::Utc::now(),
                        match &fix_failure {
                            None if fix_changed_nothing => ActivityKind::AiFixUnchanged { review },
                            None => ActivityKind::AiFixApplied { review },
                            Some(reason) => ActivityKind::AiFixFailed { review, reason: reason.clone() },
                        },
                    ),
                ];
                Self::transition_to_human_review(&tasks, &storage, &events, task_id, Some(signoff), trail).await;
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

                let trail = vec![started.clone(), (chrono::Utc::now(), ActivityKind::AiReviewApproved { review })];
                Self::transition_to_human_review(&tasks, &storage, &events, task_id, Some(signoff), trail).await;
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
        trail: Vec<(chrono::DateTime<chrono::Utc>, ActivityKind)>,
    ) {
        let amend = move |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&task_id) {
                for (at, kind) in &trail {
                    t.record_activity_at(*at, kind.clone());
                }
                t.status = TaskStatus::HumanReview;
                t.phase = TaskPhase::Complete;
                t.phase_progress = 95;
                t.overall_progress = 90;
                // These are new changes to review. The AI review outcome and
                // any human decision about the previous ones do not carry
                // over: `None` means no AI review ran for these changes, and
                // showing an earlier run's verdict would describe other code.
                t.qa_signoff = signoff.clone();
                t.human_review.record_arrival();
                let arrival = t.human_review.arrivals;
                t.record_activity(ActivityKind::ReadyForReview { arrival });
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

    /// The output of the task's most recent execution in this session.
    pub async fn get_task_output(&self, task_id: Uuid) -> Vec<AgentLogEntry> {
        self.task_run(task_id)
            .await
            .last_execution
            .map(|execution| execution.output)
            .unwrap_or_default()
    }

    /// Whether an agent is working on `task_id` now, and the task's most
    /// recent execution with its output.
    ///
    /// "Most recent" is by start time: a retried task has one execution per
    /// attempt, and the one a person asking about the task means is the last.
    pub async fn task_run(&self, task_id: Uuid) -> TaskRunSnapshot {
        let owned = self.owned_run_status(task_id).await;

        let latest = self
            .executions
            .read()
            .await
            .values()
            .filter(|e| e.task_id == Some(task_id))
            .max_by_key(|e| e.started_at)
            .map(|e| (e.id, e.started_at, e.stopped_at, e.status.clone()));

        let (last_execution, ended) = match latest {
            Some((id, started_at, stopped_at, status)) => (
                Some(ExecutionSnapshot {
                    started_at,
                    stopped_at,
                    output: self.logs.read().await.get(&id).cloned().unwrap_or_default(),
                }),
                // Only an ended execution says how a run ended; a record that
                // is not terminal but is not owned either describes nothing.
                matches!(status, AgentStatus::Stopped | AgentStatus::Failed(_)).then_some(status),
            ),
            None => (None, None),
        };

        TaskRunSnapshot { live: owned.is_some(), status: owned.or(ended), last_execution }
    }

    /// The runs SlashIt owns right now, one per task, for a board that has
    /// just opened. The same fact [`Self::task_run`] reads per task, and
    /// what [`AgentEvent::RunState`] then keeps current.
    pub async fn live_runs(&self) -> Vec<LiveRun> {
        let mut ids: Vec<Uuid> = self.running_handles.read().await.keys().copied().collect();
        ids.extend(self.reviewing_handles.read().await.keys().copied());
        ids.extend(
            self.pr_helper_handles
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(task_id, owner)| (owner.kind == PrOwnerKind::Helper).then_some(*task_id)),
        );
        ids.extend(self.ending_runs.lock().unwrap_or_else(|p| p.into_inner()).keys().copied());
        ids.sort();
        ids.dedup();

        let mut runs = Vec::new();
        for task_id in ids {
            if let Some(status) = self.owned_run_status(task_id).await {
                runs.push(LiveRun { task_id, status });
            }
        }
        runs
    }

    /// The state of the run SlashIt owns for `task_id`, or `None` when it
    /// owns none.
    ///
    /// Grounded only in what the executor itself holds: a run being ended, a
    /// registered execution whose future is still alive, or a registered
    /// review flow. A persisted task status never contributes, and neither
    /// does an execution record that is no longer backed by a handle.
    async fn owned_run_status(&self, task_id: Uuid) -> Option<AgentStatus> {
        // Read the owner maps before the stopping mark. The end path removes
        // an owner and inserts that mark while holding the map's write lock;
        // checking the map first means a reader that had to wait for that
        // lock sees the mark after the removal, rather than missing both.
        let execution_id = self
            .running_handles
            .read()
            .await
            .get(&task_id)
            .filter(|r| !r.handle.is_finished())
            .map(|r| r.execution_id);
        if let Some(execution_id) = execution_id {
            let recorded = match execution_id {
                Some(id) => self.executions.read().await.get(&id).map(|e| e.status.clone()),
                None => None,
            };
            return Some(match recorded {
                // The future has not recorded its execution yet.
                None => AgentStatus::Starting,
                Some(status @ (AgentStatus::Starting | AgentStatus::Running | AgentStatus::Stopping)) => status,
                // Ended on its own an instant before its handle left the map.
                Some(AgentStatus::Stopped | AgentStatus::Failed(_)) => AgentStatus::Stopping,
            });
        }
        let reviewing = self
            .reviewing_handles
            .read()
            .await
            .get(&task_id)
            .is_some_and(|r| !r.handle.is_finished());
        if reviewing {
            return Some(AgentStatus::Running);
        }
        if self.ending_runs.lock().unwrap_or_else(|p| p.into_inner()).contains_key(&task_id) {
            return Some(AgentStatus::Stopping);
        }

        // A PR helper is a command-owned provider flow rather than an
        // executor-spawned JoinHandle. Its lease is the executor's ownership
        // fact for the whole subprocess lifetime; a cancelled lease remains
        // live until the helper drops it, so the UI can show Stopping rather
        // than disappearing early. Side-effect reservations share the map but
        // are not provider runs and are intentionally excluded.
        self.pr_helper_handles
            .lock()
            .unwrap()
            .get(&task_id)
            .and_then(|owner| (owner.kind == PrOwnerKind::Helper).then_some(
                if *owner.cancel.borrow() {
                    AgentStatus::Stopping
                } else {
                    AgentStatus::Running
                },
            ))
    }

    /// Mark `task_id`'s run as being ended; see [`StoppingMark`].
    fn mark_ending(&self, task_id: Uuid, execution_id: Option<Uuid>) -> StoppingMark {
        self.ending_runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(task_id, execution_id);
        StoppingMark { ending: self.ending_runs.clone(), task_id }
    }

    /// Announce that the run for `task_id` is over, after a stop has joined
    /// it. The run's own future announces its end too; this repeats it, so a
    /// `stopping` announced just after that end can never be the last word.
    fn announce_stopped(&self, task_id: Uuid) {
        self.events.agent_event(AgentEvent::RunState {
            task_id: task_id.to_string(),
            status: AgentStatus::Stopped,
        });
    }

    /// Record and announce that the run for `task_id` is being stopped.
    ///
    /// The execution's record moves only from a non-terminal state: a run
    /// that already ended keeps the outcome it recorded.
    async fn announce_stopping(&self, task_id: Uuid, execution_id: Option<Uuid>) {
        if let Some(id) = execution_id {
            if let Some(exec) = self.executions.write().await.get_mut(&id) {
                if matches!(exec.status, AgentStatus::Starting | AgentStatus::Running) {
                    exec.status = AgentStatus::Stopping;
                }
            }
        }
        self.events.agent_event(AgentEvent::RunState {
            task_id: task_id.to_string(),
            status: AgentStatus::Stopping,
        });
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

    /// Commit the agent's work in its task checkout, on the task branch.
    ///
    /// Always a Git commit, even when the agent changed nothing, so a
    /// finished task ends with its own commit on its branch (see
    /// [`crate::worktree::commit_checkout`] for why jj is not used and what
    /// is refused). The error says why nothing was committed; the caller
    /// fails the run with it.
    async fn commit_changes(
        tasks: &Tasks,
        task_id: Uuid,
        working_dir: &str,
        events: &SharedEventSink,
    ) -> Result<(), String> {
        let (title, branch) = {
            let tasks_r = tasks.read().await;
            tasks_r
                .get(&task_id)
                .map(|t| (t.title.clone(), t.branch_name.clone()))
                .unwrap_or_default()
        };
        let committed = match &branch {
            Some(branch) => {
                crate::worktree::commit_checkout_even_if_empty(working_dir, branch, &format!("task: {title}")).await
            }
            None => Err(NO_TASK_BRANCH.to_string()),
        };
        let commit = committed.map_err(|e| {
            format!(
                "The agent's work could not be committed: {e}. It remains as uncommitted edits \
                 in the task checkout."
            )
        })?;
        events.agent_event(AgentEvent::Log {
            task_id: task_id.to_string(),
            level: LogLevel::Info,
            message: format!("Committed the agent's work as {commit}"),
        });
        Ok(())
    }

    async fn update_task_phase_static(tasks: &Tasks, task_id: Uuid, phase: TaskPhase, progress: u8) {
        let mut tasks = tasks.write().await;
        if let Some(t) = tasks.get_mut(&task_id) {
            t.phase = phase;
            t.phase_progress = progress;
            t.updated_at = chrono::Utc::now();
        }
    }

    /// Move a task whose run finished and whose work is committed on to AI
    /// review, durably before the board can show it: the status, the review
    /// phase, the run's tool calls and its [`ActivityKind::RunCompleted`] are
    /// one write. `Err` means nothing was written or published, and `tools`
    /// is still the caller's to record elsewhere.
    async fn record_run_completed(
        tasks: &Tasks,
        storage: &crate::config::Storage,
        task_id: Uuid,
        run: u32,
        tools: Option<&RunTools>,
    ) -> Result<(), String> {
        let amend = |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&task_id) {
                t.status = TaskStatus::AiReview;
                t.phase = TaskPhase::QaReview;
                t.phase_progress = 80;
                t.overall_progress = 80;
                if let Some(tools) = tools {
                    tools.apply_to(t);
                }
                t.record_activity(ActivityKind::RunCompleted { run });
            }
        };
        crate::lifecycle::record(tasks, storage, task_id, &amend).await
    }

    /// Move a task to `Error`/`Failed`, durably before the board can show it.
    ///
    /// `Error` matches no executor selector, so the same persist-before-publish
    /// obligation applies as [`Self::transition_to_human_review`] -- see issue
    /// #8. The payload here is only a diagnostic rather than a record of
    /// irreversible work, but a lost write would still silently strand the
    /// task off every automatic path with nothing on disk explaining why.
    ///
    /// `ended` is the coding run the failure ended, with its tool calls;
    /// `None` when the task failed before a run could start.
    async fn set_task_error_static(
        tasks: &Tasks,
        storage: &crate::config::Storage,
        events: &SharedEventSink,
        task_id: Uuid,
        msg: &str,
        ended: Option<RunEnd>,
    ) {
        let msg_owned = msg.to_string();
        let (run, tools, timeline_reason) = match ended {
            Some(RunEnd { run, tools, timeline_reason }) => (Some(run), tools, timeline_reason),
            None => (None, None, None),
        };
        let reason = timeline_reason.unwrap_or_else(|| msg_owned.clone());
        let amend = move |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&task_id) {
                t.status = TaskStatus::Error;
                t.phase = TaskPhase::Failed;
                t.error_message = Some(msg_owned.clone());
                if let Some(tools) = &tools {
                    tools.apply_to(t);
                }
                t.record_activity(ActivityKind::RunFailed { run, reason: reason.clone() });
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
    ///
    /// A coding run ended this way records no end of its own, so the tool
    /// calls it buffered are dropped here rather than left in memory until
    /// the task's next run replaces them. [`Self::stop_task`] does not come
    /// through here: it records its run's calls with the stop.
    async fn end_ownership_under_lease(&self, task_id: Uuid) -> Result<(), String> {
        self.end_task_owners_under_lease(task_id).await?;
        // Under the lease no run of this task can start, and the one that
        // was running has ended, so the buffer can only be that run's.
        take_run_tools(&self.run_tools, task_id);
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
            .insert(task_id, RunningTask { handle, cancel, execution_id: None });
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
            .insert(task_id, RunningTask { handle, cancel, execution_id: None });
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
            .insert(task_id, RunningTask { handle, cancel, execution_id: None });
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
            .insert(task_id, RunningTask { handle, cancel, execution_id: None });
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
            .insert(task_id, RunningTask { handle, cancel, execution_id: None });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceRegistry;
    use crate::domain::task::ActivityEntry;
    use crate::domain::{AgentConfig, AgentType, Project, ProjectScope, Workspace, WorkspaceRoot};
    use crate::test_helpers::create_test_task_full;
    use std::time::Duration;

    fn test_project(id: Uuid, scope: ProjectScope) -> Project {
        Project {
            id,
            name: "test-project".to_string(),
            repository_id: None,
            scope,
            state_location: crate::config::paths::StateLocation::External,
            base: None,
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
            )),
            events: crate::events::null_sink(),
            lifecycle: Arc::new(crate::lifecycle::TaskLifecycleLocks::new()),
            start_guard: crate::test_helpers::plenty_of_disk(),
            pr_statuses: crate::test_helpers::no_github(),
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

    /// A pull request merged outside SlashIt while a restack of the task's
    /// published branch is unfinished: the merge is recorded, once, so the poll
    /// stops asking, but the restack's record and the checkout are untouched
    /// and the task is not finished.
    #[tokio::test]
    async fn a_merge_found_during_an_unfinished_restack_keeps_its_recovery_state() {
        let (executor, _temps) = test_executor();
        let checkout = tempfile::tempdir().expect("a checkout directory");
        let path = checkout.path().to_str().unwrap().to_string();
        let (id, project_id) = task_with_open_pr(&executor, Some(&path)).await;
        let pending = crate::domain::PendingRepublish {
            parent_branch: "task-parent".to_string(),
            parent_pr: 7,
            pr_number: Some(7),
            default_branch: "main".to_string(),
            fork_point: "a".repeat(40),
            previous_tip: "b".repeat(40),
            onto: "c".repeat(40),
            rewritten_tip: None,
        };
        {
            let mut tasks = executor.tasks.write().await;
            let task = tasks.get_mut(&id).unwrap();
            task.pending_republish = Some(pending.clone());
            executor.storage.save_project_tasks(project_id, std::slice::from_ref(task)).unwrap();
        }

        executor.complete_merged_task(id, 7, "MERGED").await;

        let after = executor.tasks.read().await.get(&id).cloned().expect("task");
        assert_eq!(after.pending_republish, Some(pending), "the record is preserved");
        assert_eq!(after.status, TaskStatus::PrCreated, "the task is not finished");
        assert_eq!(after.worktree_path.as_deref(), Some(path.as_str()));
        assert_eq!(after.branch_name.as_deref(), Some("task-abcd1234"));
        assert!(!after.cleanup_in_flight, "no cleanup was announced");
        assert!(checkout.path().exists(), "the checkout is untouched");
        assert_eq!(recorded_pr_state(&after).as_deref(), Some("MERGED"));
        assert!(
            after.error_message.as_deref().is_some_and(|m| m.contains("restack")),
            "the card says why: {:?}",
            after.error_message
        );
        let on_disk = executor
            .storage
            .load_project_tasks(project_id)
            .unwrap()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap();
        assert!(on_disk.pending_republish.is_some(), "and so is the file");
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

    // --- a run's end, its tool calls and their buffer ---

    fn tool_use(detail: &str) -> ClaudeEvent {
        ClaudeEvent::ToolUse { tool: "Bash".into(), input: Some(serde_json::json!({ "command": detail })) }
    }

    fn buffered_tool_rows(executor: &TaskExecutor, task_id: Uuid) -> Option<usize> {
        let mut task = crate::test_helpers::create_test_task("probe");
        let tools = take_run_tools(&executor.run_tools, task_id)?;
        tools.apply_to(&mut task);
        Some(task.activity.len())
    }

    fn forwarder(executor: &TaskExecutor, task_id: Uuid, run: u32) -> RunEventForwarder {
        RunEventForwarder {
            task_id,
            run,
            execution_id: Uuid::new_v4(),
            logs: executor.logs.clone(),
            events: executor.events.clone(),
            tasks: executor.tasks.clone(),
            run_tools: executor.run_tools.clone(),
            working_dir: "/nowhere".into(),
        }
    }

    /// Every tool call the run's reader sent before the run ended is in its
    /// buffer once the forwarder is drained, however far behind it was.
    #[tokio::test]
    async fn draining_the_forwarder_handles_every_event_already_sent() {
        let (executor, _temps) = test_executor();
        let task_id = Uuid::new_v4();
        executor.run_tools.lock().unwrap().insert(task_id, RunTools::new(1));
        let (sender, receiver) = tokio::sync::broadcast::channel(512);
        let drain = forwarder(&executor, task_id, 1).spawn(receiver);

        for n in 0..10 {
            sender.send(tool_use(&format!("step {n}"))).unwrap();
        }
        // The forwarder has not run yet: on this runtime nothing else does
        // until the test yields, which is the window between the reader's
        // last send and the run's end being recorded.
        {
            let mut probe = crate::test_helpers::create_test_task("probe");
            executor.run_tools.lock().unwrap()[&task_id].apply_to(&mut probe);
            assert!(probe.activity.is_empty(), "the calls are still queued, not handled");
        }

        drain.drain().await;
        assert_eq!(buffered_tool_rows(&executor, task_id), Some(10));
        drop(sender);
    }

    /// A forwarder that fell further behind than the channel holds loses the
    /// calls the channel dropped, and still handles the ones after them.
    #[tokio::test]
    async fn a_forwarder_that_lagged_keeps_forwarding() {
        let (executor, _temps) = test_executor();
        let task_id = Uuid::new_v4();
        executor.run_tools.lock().unwrap().insert(task_id, RunTools::new(1));
        let (sender, receiver) = tokio::sync::broadcast::channel(4);
        let drain = forwarder(&executor, task_id, 1).spawn(receiver);

        for n in 0..10 {
            sender.send(tool_use(&format!("step {n}"))).unwrap();
        }
        tokio::task::yield_now().await;
        sender.send(tool_use("after the lag")).unwrap();
        drain.drain().await;

        let rows = buffered_tool_rows(&executor, task_id).expect("still buffered");
        assert!(rows >= 1, "the forwarder must not stop at a lag");
        drop(sender);
    }

    /// A run whose finish cannot be recorded changes nothing, and its tool
    /// calls are still the caller's, so the failure that follows records
    /// them.
    #[tokio::test]
    async fn a_finish_that_cannot_be_recorded_leaves_the_tool_calls_for_the_failure() {
        let (executor, temps) = test_executor();
        let (id, project_id) = seed_task(&executor, TaskStatus::InProgress).await;
        let before = executor.tasks.read().await[&id].clone();
        let mut tools = RunTools::new(1);
        tools.push(chrono::Utc::now(), "Bash", &serde_json::json!({"command": "cargo test"}), "/nowhere");
        block_persistence(&temps[0]);

        let refused =
            TaskExecutor::record_run_completed(&executor.tasks, &executor.storage, id, 1, Some(&tools)).await;

        assert!(refused.is_err());
        let held = executor.tasks.read().await[&id].clone();
        assert_eq!(
            (&held.status, &held.phase, held.phase_progress, held.overall_progress, &held.activity),
            (&before.status, &before.phase, before.phase_progress, before.overall_progress, &before.activity),
            "nothing is published for a write that did not happen"
        );

        std::fs::remove_file(temps[0].path().join("config").join("tasks")).expect("unblock persistence");
        let ended = RunEnd { run: 1, tools: Some(tools), timeline_reason: None };
        TaskExecutor::set_task_error_static(&executor.tasks, &executor.storage, &executor.events, id, "x", Some(ended))
            .await;
        let failed = on_disk_task(&executor, project_id, id);
        assert_eq!(failed.status, TaskStatus::Error);
        assert!(matches!(
            &failed.activity[..],
            [
                ActivityEntry { kind: ActivityKind::ToolUsed { run: 1, .. }, .. },
                ActivityEntry { kind: ActivityKind::RunFailed { run: Some(1), .. }, .. },
            ]
        ), "{:?}", failed.activity);
        assert_eq!(executor.tasks.read().await[&id].activity, failed.activity);
    }

    /// The failure's timeline row says why the run ended, not what the agent
    /// printed, even when the task's error quotes it.
    #[tokio::test]
    async fn a_failure_quoting_agent_output_keeps_it_off_the_timeline() {
        let (executor, _temps) = test_executor();
        let (id, project_id) = seed_task(&executor, TaskStatus::InProgress).await;
        let ended = RunEnd { run: 1, tools: None, timeline_reason: Some("Exit code 1 — no details".into()) };
        TaskExecutor::set_task_error_static(
            &executor.tasks,
            &executor.storage,
            &executor.events,
            id,
            "Exit code 1 — no details — THE AGENT SAID THIS",
            Some(ended),
        )
        .await;
        let failed = on_disk_task(&executor, project_id, id);
        assert!(failed.error_message.as_deref().unwrap().contains("THE AGENT SAID THIS"));
        assert_eq!(
            failed.activity.last().map(|e| &e.kind),
            Some(&ActivityKind::RunFailed { run: Some(1), reason: "Exit code 1 — no details".into() })
        );
    }

    /// A run a lifecycle move ended records no end, so its buffered calls
    /// are dropped rather than kept until the task runs again.
    #[tokio::test]
    async fn a_run_ended_by_a_move_leaves_no_tool_buffer_behind() {
        let (executor, _temps) = test_executor();
        let (id, _project_id) = seed_task(&executor, TaskStatus::InProgress).await;
        let cleaned_up = executor.register_fake_running_execution_for_test(id).await;
        executor.run_tools.lock().unwrap().insert(id, RunTools::new(1));

        crate::lifecycle::ExecutionOwnership::end_ownership_under_lease(executor.as_ref(), id)
            .await
            .expect("ended");

        assert!(cleaned_up.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!executor.run_tools.lock().unwrap().contains_key(&id));
    }

    /// A stop records the calls its run buffered with the stop.
    #[tokio::test]
    async fn a_stop_records_the_runs_tool_calls() {
        let (executor, _temps) = test_executor();
        let (id, project_id) = seed_task(&executor, TaskStatus::InProgress).await;
        executor.register_fake_running_execution_for_test(id).await;
        let mut tools = RunTools::new(1);
        tools.push(chrono::Utc::now(), "Bash", &serde_json::json!({"command": "cargo test"}), "/nowhere");
        executor.run_tools.lock().unwrap().insert(id, tools);

        executor.stop_task(id).await.expect("stopped");

        let stopped = on_disk_task(&executor, project_id, id);
        assert!(matches!(
            &stopped.activity[..],
            [
                ActivityEntry { kind: ActivityKind::ToolUsed { run: 1, .. }, .. },
                ActivityEntry { kind: ActivityKind::Stopped { .. }, .. },
            ]
        ), "{:?}", stopped.activity);
        assert!(!executor.run_tools.lock().unwrap().contains_key(&id));
    }

    // --- record_pr_poll_state("CLOSED") ---

    #[tokio::test]
    async fn a_closed_pr_that_cannot_be_recorded_is_not_exposed_and_stays_pollable() {
        let recording = Arc::new(crate::events::RecordingEventSink::new());
        let (executor, temps) = test_executor_with_events(recording.clone());
        let (id, _project_id) = task_with_open_pr(&executor, None).await;
        block_persistence(&temps[0]);

        executor.record_pr_poll_state(id, &crate::pr_status::PrKey::new("owner/repo", 7), "CLOSED").await;

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

        executor.record_pr_poll_state(id, &crate::pr_status::PrKey::new("owner/repo", 7), "CLOSED").await;

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

    // --- the pull request poll: selection, no-op writes, timeouts ---

    fn pr_ref(repo: &str, number: u32, state: Option<&str>) -> ExternalRef {
        ExternalRef::GithubPr {
            url: format!("https://github.com/{repo}/pull/{number}"),
            number,
            repo: repo.to_string(),
            state: state.map(str::to_string),
        }
    }

    fn polled_status(state: crate::pr_status::PrState) -> crate::pr_status::PrStatus {
        crate::pr_status::PrStatus {
            state,
            checks: crate::pr_status::ChecksState::Passing,
            failing_checks: vec![],
            failing_check_count: 0,
            review_decision: None,
            mergeable: None,
        }
    }

    /// A stand-in `gh` that answers from a shell `case` on the pull request
    /// number, which is its third argument.
    #[cfg(unix)]
    fn fake_gh(dir: &std::path::Path, cases: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("gh");
        std::fs::write(&path, format!("#!/bin/sh\ncase \"$3\" in\n{cases}\nesac\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    const OPEN_FAILING: &str = r#"printf '{"state":"OPEN","statusCheckRollup":[{"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"FAILURE"}],"reviewDecision":"","mergeable":"MERGEABLE"}'"#;
    #[cfg(unix)]
    const MERGED: &str = r#"printf '{"state":"MERGED","statusCheckRollup":[],"reviewDecision":"APPROVED","mergeable":"UNKNOWN"}'"#;

    #[test]
    fn every_column_with_a_linked_open_pull_request_is_polled() {
        use crate::pr_status::{PrKey, PrState, PrStatuses};
        let statuses = PrStatuses::with_program("unused", Duration::from_secs(1));
        let project = Uuid::new_v4();
        let all = [
            TaskStatus::Backlog,
            TaskStatus::Queue,
            TaskStatus::InProgress,
            TaskStatus::AiReview,
            TaskStatus::HumanReview,
            TaskStatus::PrCreated,
            TaskStatus::Done,
            TaskStatus::Error,
        ];
        let mut tasks = HashMap::new();
        let mut expected = Vec::new();
        for (i, status) in all.iter().enumerate() {
            let mut t = crate::test_helpers::create_test_task_full("t", project, status.clone(), i as i32);
            t.external_refs.push(pr_ref("o/r", 100 + i as u32, Some("OPEN")));
            expected.push((PrKey::new("o/r", 100 + i as u32), vec![t.id]));
            tasks.insert(t.id, t);
        }
        // A state never recorded is polled too.
        let mut unrecorded = crate::test_helpers::create_test_task_full("t", project, TaskStatus::PrCreated, 20);
        unrecorded.external_refs.push(pr_ref("o/r", 200, None));
        expected.push((PrKey::new("o/r", 200), vec![unrecorded.id]));
        tasks.insert(unrecorded.id, unrecorded);

        let mut polled = polled_pull_requests(&tasks, &statuses);
        polled.sort_by_key(|(key, _)| key.number);
        assert_eq!(polled, expected, "a pull request linked in any column keeps its card current");

        // Final, as recorded or as heard, or quarantined: not asked again.
        let mut excluded = HashMap::new();
        for (n, state) in [(1, "MERGED"), (2, "CLOSED"), (3, "merged")] {
            let mut t = crate::test_helpers::create_test_task_full("t", project, TaskStatus::PrCreated, n);
            t.external_refs.push(pr_ref("o/x", n as u32, Some(state)));
            excluded.insert(t.id, t);
        }
        let mut quarantined = crate::test_helpers::create_test_task_full("t", project, TaskStatus::PrCreated, 4);
        quarantined.cleanup_in_flight = true;
        quarantined.external_refs.push(pr_ref("o/x", 4, Some("OPEN")));
        excluded.insert(quarantined.id, quarantined);
        let mut heard_merged = crate::test_helpers::create_test_task_full("t", project, TaskStatus::InProgress, 5);
        heard_merged.external_refs.push(pr_ref("o/x", 5, Some("OPEN")));
        statuses.remember(&PrKey::new("o/x", 5), polled_status(PrState::Merged));
        excluded.insert(heard_merged.id, heard_merged);
        assert_eq!(polled_pull_requests(&excluded, &statuses), vec![]);

        // Heard closed but not recorded, because recording it failed: asked
        // again, so the write is retried.
        let mut unrecorded_closure = crate::test_helpers::create_test_task_full("t", project, TaskStatus::InProgress, 7);
        unrecorded_closure.external_refs.push(pr_ref("o/x", 7, Some("OPEN")));
        statuses.remember(&PrKey::new("o/x", 7), polled_status(PrState::Closed));
        let retried = vec![(PrKey::new("o/x", 7), vec![unrecorded_closure.id])];
        let mut with_closure = excluded.clone();
        with_closure.insert(unrecorded_closure.id, unrecorded_closure);
        assert_eq!(polled_pull_requests(&with_closure, &statuses), retried);

        // Heard merged, but in a column the merge finishes: still asked, so
        // the lifecycle gets its answer.
        let mut delivered = crate::test_helpers::create_test_task_full("t", project, TaskStatus::PrCreated, 6);
        delivered.external_refs.push(pr_ref("o/x", 5, Some("OPEN")));
        let delivered_id = delivered.id;
        excluded.insert(delivered_id, delivered);
        assert_eq!(polled_pull_requests(&excluded, &statuses), vec![(PrKey::new("o/x", 5), vec![delivered_id])]);
    }

    #[test]
    fn one_pull_request_linked_to_two_tasks_is_asked_about_once() {
        let statuses = crate::pr_status::PrStatuses::with_program("unused", Duration::from_secs(1));
        let project = Uuid::new_v4();
        let mut tasks = HashMap::new();
        let mut ids = Vec::new();
        for i in 0..2 {
            let mut t = crate::test_helpers::create_test_task_full("t", project, TaskStatus::PrCreated, i);
            t.external_refs.push(pr_ref("o/r", 9, Some("OPEN")));
            ids.push(t.id);
            tasks.insert(t.id, t);
        }
        ids.sort();
        assert_eq!(
            polled_pull_requests(&tasks, &statuses),
            vec![(crate::pr_status::PrKey::new("o/r", 9), ids)]
        );
    }

    async fn seed_with_pr(executor: &TaskExecutor, status: TaskStatus, pr: ExternalRef) -> (Uuid, Uuid) {
        let (id, project_id) = seed_task(executor, status).await;
        let task = {
            let mut tasks = executor.tasks.write().await;
            let t = tasks.get_mut(&id).unwrap();
            t.external_refs.push(pr);
            t.clone()
        };
        let siblings: Vec<Task> = executor.tasks.read().await.values().filter(|t| t.project_id == project_id).cloned().collect();
        executor.storage.save_project_tasks(project_id, &siblings).expect("seed");
        let _ = task;
        (id, project_id)
    }

    #[tokio::test]
    async fn an_unchanged_open_pull_request_writes_nothing() {
        let recording = Arc::new(crate::events::RecordingEventSink::new());
        let (executor, temps) = test_executor_with_events(recording.clone());
        let (id, _) = seed_with_pr(&executor, TaskStatus::PrCreated, pr_ref("owner/repo", 7, Some("OPEN"))).await;
        let before = executor.tasks.read().await.get(&id).unwrap().updated_at;
        // Any write would now fail and be reported.
        block_persistence(&temps[0]);

        let key = crate::pr_status::PrKey::new("owner/repo", 7);
        for _ in 0..3 {
            executor.record_pr_poll_state(id, &key, "OPEN").await;
        }

        let after = executor.tasks.read().await.get(&id).cloned().unwrap();
        assert_eq!(after.updated_at, before, "an unchanged reading is not a change to the task");
        assert!(
            !warn_message_containing(&recording, "recording it failed"),
            "no write was attempted: {:?}",
            recording.recorded()
        );
    }

    #[tokio::test]
    async fn a_late_open_answer_never_reopens_a_recorded_merge_or_closure() {
        let (executor, _temps) = test_executor();
        for final_state in ["MERGED", "CLOSED"] {
            let (id, project_id) = seed_with_pr(&executor, TaskStatus::Done, pr_ref("owner/repo", 7, Some(final_state))).await;
            let before = executor.tasks.read().await[&id].updated_at;

            executor.record_pr_poll_state(id, &crate::pr_status::PrKey::new("owner/repo", 7), "OPEN").await;

            let after = executor.tasks.read().await[&id].clone();
            assert_eq!(recorded_pr_state(&after).as_deref(), Some(final_state));
            assert_eq!(after.updated_at, before, "nothing was written");
            assert_eq!(recorded_pr_state(&on_disk_task(&executor, project_id, id)).as_deref(), Some(final_state));
        }
    }

    #[tokio::test]
    async fn a_merge_is_not_acted_on_once_the_task_has_left_the_delivery_columns() {
        let (executor, _temps) = test_executor();
        let (id, _) = seed_with_pr(&executor, TaskStatus::InProgress, pr_ref("owner/repo", 7, Some("OPEN"))).await;

        executor.complete_merged_task(id, 7, "MERGED").await;

        let after = executor.tasks.read().await[&id].clone();
        assert_eq!(after.status, TaskStatus::InProgress);
        assert_eq!(recorded_pr_state(&after).as_deref(), Some("OPEN"));
    }

    #[tokio::test]
    async fn a_changed_pull_request_state_is_still_persisted() {
        let (executor, _temps) = test_executor();
        let (id, project_id) = seed_with_pr(&executor, TaskStatus::PrCreated, pr_ref("owner/repo", 7, None)).await;
        let before = executor.tasks.read().await.get(&id).unwrap().updated_at;

        executor.record_pr_poll_state(id, &crate::pr_status::PrKey::new("owner/repo", 7), "OPEN").await;

        let after = executor.tasks.read().await.get(&id).cloned().unwrap();
        assert_eq!(recorded_pr_state(&after).as_deref(), Some("OPEN"));
        assert!(after.updated_at > before);
        assert_eq!(recorded_pr_state(&on_disk_task(&executor, project_id, id)).as_deref(), Some("OPEN"));
        // Another repository's pull request with the same number is not this one.
        executor.record_pr_poll_state(id, &crate::pr_status::PrKey::new("other/repo", 7), "CLOSED").await;
        assert_eq!(recorded_pr_state(&executor.tasks.read().await[&id]).as_deref(), Some("OPEN"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_hung_gh_times_out_without_holding_up_the_other_pull_requests() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), &format!("1) sleep 30 ;;\n*) {OPEN_FAILING} ;;"));
        let statuses = Arc::new(crate::pr_status::PrStatuses::with_program(gh, Duration::from_millis(800)));
        let hung_key = crate::pr_status::PrKey::new("o/r", 1);
        statuses.remember(&hung_key, polled_status(crate::pr_status::PrState::Open));
        let (executor, _temps) = test_executor_with_prs(statuses.clone());
        let (hung, _) = seed_with_pr(&executor, TaskStatus::PrCreated, pr_ref("o/r", 1, Some("OPEN"))).await;
        let (answered, _) = seed_with_pr(&executor, TaskStatus::PrCreated, pr_ref("o/r", 2, None)).await;

        let began = std::time::Instant::now();
        executor.poll_pull_requests().await;
        assert!(began.elapsed() < Duration::from_secs(10), "the pass took {:?}", began.elapsed());

        let hung_entry = statuses.get(&hung_key).unwrap();
        assert_eq!(hung_entry.error.map(|e| e.kind), Some(crate::pr_status::PrFetchErrorKind::Timeout));
        assert_eq!(
            hung_entry.status.map(|s| s.checks),
            Some(crate::pr_status::ChecksState::Passing),
            "the timeout keeps the last good reading"
        );
        let answered_entry = statuses.get(&crate::pr_status::PrKey::new("o/r", 2)).unwrap();
        assert_eq!(answered_entry.status.map(|s| s.checks), Some(crate::pr_status::ChecksState::Failing));

        let tasks = executor.tasks.read().await;
        assert_eq!(recorded_pr_state(&tasks[&answered]).as_deref(), Some("OPEN"));
        assert_eq!(recorded_pr_state(&tasks[&hung]).as_deref(), Some("OPEN"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_merge_heard_outside_the_delivery_columns_is_shown_not_acted_on() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), &format!("*) {MERGED} ;;"));
        let statuses = Arc::new(crate::pr_status::PrStatuses::with_program(gh, Duration::from_secs(10)));
        let (executor, _temps) = test_executor_with_prs(statuses.clone());
        let (id, project_id) = seed_with_pr(&executor, TaskStatus::InProgress, pr_ref("o/r", 3, Some("OPEN"))).await;

        executor.poll_pull_requests().await;

        let after = executor.tasks.read().await.get(&id).cloned().unwrap();
        assert_eq!(after.status, TaskStatus::InProgress, "a poll does not take a task back from work");
        assert_eq!(recorded_pr_state(&after).as_deref(), Some("OPEN"));
        assert_eq!(recorded_pr_state(&on_disk_task(&executor, project_id, id)).as_deref(), Some("OPEN"));
        let key = crate::pr_status::PrKey::new("o/r", 3);
        assert_eq!(statuses.get(&key).unwrap().status.map(|s| s.state), Some(crate::pr_status::PrState::Merged));
        let tasks = executor.tasks.read().await;
        assert!(polled_pull_requests(&tasks, &statuses).is_empty(), "heard final: not asked again");
    }

    #[cfg(unix)]
    const CLOSED: &str = r#"printf '{"state":"CLOSED","statusCheckRollup":[],"reviewDecision":"","mergeable":"UNKNOWN"}'"#;

    #[cfg(unix)]
    #[tokio::test]
    async fn a_closure_heard_outside_the_delivery_columns_is_recorded_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), &format!("*) {CLOSED} ;;"));
        let statuses = Arc::new(crate::pr_status::PrStatuses::with_program(gh, Duration::from_secs(10)));
        let (executor, _temps) = test_executor_with_prs(statuses.clone());
        let (id, project_id) = seed_with_pr(&executor, TaskStatus::InProgress, pr_ref("o/r", 3, Some("OPEN"))).await;
        let cleaned_up = register_cancellable_execution(&executor, id).await;
        let before = executor.tasks.read().await[&id].clone();

        executor.poll_pull_requests().await;

        for (seen, task) in [
            ("in memory", executor.tasks.read().await[&id].clone()),
            ("on disk", on_disk_task(&executor, project_id, id)),
        ] {
            assert_eq!(recorded_pr_state(&task).as_deref(), Some("CLOSED"), "{seen}");
            assert_eq!(task.status, TaskStatus::InProgress, "{seen}: a poll does not take a task back from work");
            assert_eq!(task.phase, before.phase, "{seen}");
            assert_eq!(task.error_message, None, "{seen}: the task is not waiting on this pull request");
        }
        assert!(!cleaned_up.load(std::sync::atomic::Ordering::SeqCst), "the work on it is not ended");
        assert!(executor.running_handles.read().await.contains_key(&id));

        // What the next start of SlashIt reads: the task as saved, and a cache
        // that has heard nothing.
        let restarted: HashMap<Uuid, Task> = executor
            .storage
            .load_project_tasks(project_id)
            .expect("readable")
            .into_iter()
            .map(|t| (t.id, t))
            .collect();
        let fresh = crate::pr_status::PrStatuses::with_program("unused", Duration::from_secs(1));
        assert!(
            polled_pull_requests(&restarted, &fresh).is_empty(),
            "a closed pull request is not an open one to ask about again"
        );
        assert_eq!(RecordedPr::of(&restarted[&id]), RecordedPr::ClosedUnmerged);
    }

    #[tokio::test]
    async fn a_closure_fails_only_a_task_waiting_on_its_pull_request() {
        let (executor, _temps) = test_executor();
        let key = crate::pr_status::PrKey::new("owner/repo", 7);
        for (status, fails) in [
            (TaskStatus::PrCreated, true),
            (TaskStatus::HumanReview, true),
            (TaskStatus::Done, true),
            (TaskStatus::Backlog, false),
            (TaskStatus::Queue, false),
            (TaskStatus::InProgress, false),
            (TaskStatus::AiReview, false),
            (TaskStatus::Error, false),
        ] {
            let (id, _) = seed_with_pr(&executor, status.clone(), pr_ref("owner/repo", 7, Some("OPEN"))).await;

            executor.apply_polled_pr_state(id, &key, crate::pr_status::PrState::Closed).await;

            let after = executor.tasks.read().await[&id].clone();
            assert_eq!(recorded_pr_state(&after).as_deref(), Some("CLOSED"), "{status:?}");
            assert_eq!(after.status, status, "a closure never moves the task");
            assert_eq!(after.error_message.is_some(), fails, "{status:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_answer_a_newer_reading_replaced_is_not_acted_on() {
        use crate::pr_status::PrState;
        let statuses = Arc::new(crate::pr_status::PrStatuses::with_program("unused", Duration::from_secs(1)));
        let (executor, _temps) = test_executor_with_prs(statuses.clone());
        let key = crate::pr_status::PrKey::new("owner/repo", 7);
        // The pull request was reopened, and a newer reading says so.
        statuses.remember(&key, polled_status(PrState::Open));
        for (status, stale) in [
            (TaskStatus::PrCreated, PrState::Closed),
            (TaskStatus::InProgress, PrState::Closed),
            (TaskStatus::PrCreated, PrState::Merged),
        ] {
            let (id, _) = seed_with_pr(&executor, status.clone(), pr_ref("owner/repo", 7, Some("OPEN"))).await;

            executor.apply_polled_pr_state(id, &key, stale).await;

            let after = executor.tasks.read().await[&id].clone();
            assert_eq!(recorded_pr_state(&after).as_deref(), Some("OPEN"), "{status:?} {stale:?}");
            assert_eq!(after.status, status, "{stale:?}");
            assert_eq!(after.error_message, None, "{stale:?}");
        }
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
            Vec::new(),
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
            Vec::new(),
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
            None,
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
            None,
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

    fn execution_for(task_id: Uuid, started_at: chrono::DateTime<chrono::Utc>, stopped: bool) -> AgentExecution {
        AgentExecution {
            id: Uuid::new_v4(),
            worktree_id: None,
            task_id: Some(task_id),
            agent_type: "claude-code".to_string(),
            status: if stopped { AgentStatus::Stopped } else { AgentStatus::Running },
            started_at,
            stopped_at: stopped.then(chrono::Utc::now),
        }
    }

    async fn record_execution(
        executor: &TaskExecutor,
        execution: AgentExecution,
        output: &[&str],
    ) {
        let id = execution.id;
        executor.executions.write().await.insert(id, execution);
        for line in output {
            record_output(&executor.logs, id, LogLevel::Info, line.to_string()).await;
        }
    }

    #[tokio::test]
    async fn task_run_reads_the_latest_execution_not_an_arbitrary_one() {
        let (executor, _temps) = test_executor();
        let task_id = Uuid::new_v4();
        let earlier = chrono::Utc::now() - chrono::Duration::minutes(5);

        // Twenty earlier attempts beside the latest one: a lookup that took
        // whichever execution the map happened to yield first would read an
        // earlier attempt twenty times in twenty-one.
        record_execution(&executor, execution_for(task_id, chrono::Utc::now(), false), &["retry output"]).await;
        for n in 0..20 {
            let started = earlier - chrono::Duration::seconds(n);
            record_execution(&executor, execution_for(task_id, started, true), &["failed attempt output"]).await;
        }
        // Another task's execution never leaks into this one's.
        record_execution(&executor, execution_for(Uuid::new_v4(), chrono::Utc::now(), false), &["someone else"]).await;

        let run = executor.task_run(task_id).await;
        let last = run.last_execution.expect("the task has executions");
        let messages: Vec<&str> = last.output.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(messages, ["retry output"]);
        assert!(last.stopped_at.is_none());
        assert_eq!(executor.get_task_output(task_id).await.len(), 1);
    }

    #[tokio::test]
    async fn task_run_is_live_exactly_while_an_owner_can_be_stopped() {
        let (executor, _temps) = test_executor();
        let task_id = Uuid::new_v4();

        let idle = executor.task_run(task_id).await;
        assert!(!idle.live, "nothing owns a task that never ran");
        assert!(idle.last_execution.is_none());

        executor.register_fake_running_execution_for_test(task_id).await;
        assert!(executor.task_run(task_id).await.live);

        executor.stop_task(task_id).await.expect("stop ends the fake owner");
        assert!(!executor.task_run(task_id).await.live, "a stopped task has nothing left to stop");
    }

    #[tokio::test]
    async fn execution_output_keeps_only_the_most_recent_entries() {
        let (executor, _temps) = test_executor();
        let execution_id = Uuid::new_v4();
        for n in 0..EXECUTION_OUTPUT_LIMIT + 5 {
            record_output(&executor.logs, execution_id, LogLevel::Info, n.to_string()).await;
        }
        let logs = executor.logs.read().await;
        let output = &logs[&execution_id];
        assert_eq!(output.len(), EXECUTION_OUTPUT_LIMIT);
        assert_eq!(output.first().map(|e| e.message.as_str()), Some("5"));
        assert_eq!(output.last().map(|e| e.message.clone()), Some((EXECUTION_OUTPUT_LIMIT + 4).to_string()));
    }

    #[test]
    fn assistant_text_blocks_keep_the_agents_words_and_skip_tool_calls() {
        let content = serde_json::json!([
            {"type": "text", "text": "  Reading the spec\n"},
            {"type": "tool_use", "name": "Read", "input": {}},
            {"type": "text", "text": "   "},
            {"type": "text", "text": "Now editing"},
        ]);
        assert_eq!(assistant_text_blocks(&content), ["Reading the spec", "Now editing"]);
        assert!(assistant_text_blocks(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn agent_text_is_its_own_event_kind() {
        let value = serde_json::to_value(AgentEvent::Output {
            task_id: "t".to_string(),
            text: "hello".to_string(),
        })
        .expect("serialises");
        assert_eq!(value, serde_json::json!({"type": "output", "task_id": "t", "text": "hello"}));
    }

    #[tokio::test]
    async fn project_conversation_runs_use_executor_ownership_without_a_task_id() {
        let (executor, _temps) = test_executor();
        let conversation_id = Uuid::new_v4();
        let lease = executor.begin_project_run(conversation_id, true).await.expect("admit Project Coordinator");
        assert!(executor.project_run_is_live(conversation_id));
        assert_eq!(executor.active_project_run_count(), 1);
        assert_eq!(executor.running_task_count().await, 1);
        assert!(executor.begin_project_run(conversation_id, true).await.is_err(), "one Run per Conversation");

        let mut cancelled = lease.cancel_receiver();
        executor.stop_project_run(conversation_id).expect("stop Coordinator");
        cancelled.changed().await.expect("stop signal");
        assert!(*cancelled.borrow());

        drop(lease);
        assert!(!executor.project_run_is_live(conversation_id));
        assert_eq!(executor.running_task_count().await, 0);
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
        test_executor_with(events, crate::test_helpers::plenty_of_disk())
    }

    /// An executor whose pull request status comes from `pr_statuses`.
    #[cfg(unix)]
    fn test_executor_with_prs(
        pr_statuses: Arc<crate::pr_status::PrStatuses>,
    ) -> (Arc<TaskExecutor>, Vec<tempfile::TempDir>) {
        test_executor_full(crate::events::null_sink(), crate::test_helpers::plenty_of_disk(), pr_statuses)
    }

    /// Same as [`test_executor_with_events`], with a caller-supplied start
    /// guard, for tests about disk pressure.
    fn test_executor_with(
        events: SharedEventSink,
        start_guard: Arc<StartGuard>,
    ) -> (Arc<TaskExecutor>, Vec<tempfile::TempDir>) {
        test_executor_full(events, start_guard, crate::test_helpers::no_github())
    }

    fn test_executor_full(
        events: SharedEventSink,
        start_guard: Arc<StartGuard>,
        pr_statuses: Arc<crate::pr_status::PrStatuses>,
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
            )),
            events,
            lifecycle: Arc::new(crate::lifecycle::TaskLifecycleLocks::new()),
            start_guard,
            pr_statuses,
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
            )),
            events: crate::events::null_sink(),
            lifecycle: Arc::new(crate::lifecycle::TaskLifecycleLocks::new()),
            start_guard: crate::test_helpers::plenty_of_disk(),
            pr_statuses: crate::test_helpers::no_github(),
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
            )),
            events: crate::events::null_sink(),
            lifecycle: Arc::new(crate::lifecycle::TaskLifecycleLocks::new()),
            start_guard: crate::test_helpers::plenty_of_disk(),
            pr_statuses: crate::test_helpers::no_github(),
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
            .insert(task_id, RunningTask { handle, cancel, execution_id: None });
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

        executor.spawn_task_execution(task_id, None).await.expect_err("refused");

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

        executor.spawn_task_execution(task_id, None).await.expect_err("refused");

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
        dependency_pr_state: Option<&str>,
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
        if let Some(state) = dependency_pr_state {
            dependency.external_refs.push(crate::domain::task::ExternalRef::GithubPr {
                url: "https://github.com/test-org/test-repo/pull/7".to_string(),
                number: 7,
                repo: "test-org/test-repo".to_string(),
                state: (!state.is_empty()).then(|| state.to_string()),
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

    /// Give the fixture's `repo` a bare `origin` next to it holding its
    /// `main`, with `refs/remotes/origin/HEAD` pointing at `origin/main` the
    /// way `git clone` leaves it, so that what is on `main` now is provably
    /// on the repository's known remote default base. Without this the
    /// repository has no remote at all, and no branch started in it can be
    /// recorded as coming from the default base.
    fn publish_default_base(repo: &std::path::Path) {
        let remote = repo.parent().unwrap().join("origin.git");
        git_in(repo, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        git_in(repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git_in(repo, &["push", "-q", "origin", "main"]);
        git_in(repo, &["remote", "set-head", "origin", "main"]);
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
            &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
        )
        .await;
        let refs_before = refs_of(&repo);

        executor.spawn_task_execution(task_id, None).await.expect_err("refused");

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
    /// branch has since been deleted has been delivered, so the task gets an
    /// ordinary branch instead of a stacked one, and the log says why. The
    /// primary checkout here is on a pushed `main`, so that branch provably
    /// starts on the default base.
    #[tokio::test]
    async fn a_task_whose_done_dependency_branch_is_gone_starts_from_the_default_base() {
        let recording = Arc::new(crate::events::RecordingEventSink::new());
        let (executor, temps) = test_executor_with_events(recording.clone());
        let (repo, task_id, _) =
            stacked_task_fixture(&executor, &temps, TaskStatus::Done, Some("MERGED"), "task-deadbeef").await;
        publish_default_base(&repo);
        let main_tip = git_in(&repo, &["rev-parse", "main"]);

        let (existing, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;

        assert_eq!(existing, None);
        let Acquired { info, what_happened, base_commit, origin, .. } =
            acquired.expect("an ordinary worktree");
        assert_eq!(what_happened, "Created worktree");
        assert_eq!(git_in(&repo, &["rev-parse", &format!("refs/heads/{}", info.branch)]), main_tip);
        assert_eq!(git_in(std::path::Path::new(&info.path), &["rev-parse", "HEAD"]), main_tip);
        assert_eq!(base_commit.as_deref(), Some(main_tip.as_str()));
        assert_eq!(origin, Some(on_main()));
        assert_eq!(worktree_count(&repo), 2);

        let recorded = serde_json::to_string(&recording.recorded()).unwrap();
        assert!(
            recorded.contains("task-deadbeef") && recorded.contains("ordinary branch"),
            "the log has to say why the task was not stacked: {recorded}"
        );
    }

    /// A done dependency whose branch is still there is stacked on as
    /// before: a local branch is not taken as proof of anything missing.
    #[tokio::test]
    async fn a_done_dependency_whose_branch_is_still_there_is_stacked_on() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) =
            stacked_task_fixture(&executor, &temps, TaskStatus::Done, Some("MERGED"), "task-deadbeef").await;
        git_in(&repo, &["branch", "task-deadbeef"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "main moves on"]);
        let dependency_tip = git_in(&repo, &["rev-parse", "task-deadbeef"]);

        let (_, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;

        let Acquired { info, what_happened, base_commit, origin, .. } =
            acquired.expect("a stacked worktree");
        assert_eq!(what_happened, "Created stacked worktree");
        assert_eq!(git_in(std::path::Path::new(&info.path), &["rev-parse", "HEAD"]), dependency_tip);
        assert_eq!(base_commit.as_deref(), Some(dependency_tip.as_str()));
        assert_eq!(
            origin,
            Some(BranchOrigin::Stacked { parent_branch: "task-deadbeef".to_string() })
        );
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
            &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
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
        let Acquired { info, what_happened, base_commit, origin, .. } =
            acquired.expect("the leftover branch is resumed");
        assert_eq!(what_happened, "Resumed stacked worktree");
        assert_eq!(info.branch, task_branch);
        assert_eq!(git_in(std::path::Path::new(&info.path), &["rev-parse", "HEAD"]), task_tip);
        assert_eq!(base_commit.as_deref(), Some(dependency_tip.as_str()));
        assert_eq!(
            origin,
            Some(BranchOrigin::Stacked { parent_branch: "task-deadbeef".to_string() })
        );
    }

    /// The task as it was last persisted, read back from storage the way a
    /// restart would.
    fn persisted_task(executor: &TaskExecutor, project_id: Uuid, task_id: Uuid) -> Task {
        executor
            .storage
            .load_project_tasks(project_id)
            .expect("load tasks")
            .into_iter()
            .find(|t| t.id == task_id)
            .expect("the task was persisted")
    }

    /// Starting a stacked task records the dependency's branch as the stack
    /// parent, on disk, next to the branch itself.
    #[tokio::test]
    async fn starting_a_stacked_task_records_its_stack_parent() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) = stacked_task_fixture(
            &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
        )
        .await;
        git_in(&repo, &["branch", "task-deadbeef"]);

        let (_, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;
        executor.record_run_start(task_id, repo.to_str().unwrap(), acquired.expect("a stacked worktree")).await.expect("recorded");

        let project_id = executor.tasks.read().await[&task_id].project_id;
        let persisted = persisted_task(&executor, project_id, task_id);
        assert_eq!(
            persisted.branch_origin,
            Some(BranchOrigin::Stacked { parent_branch: "task-deadbeef".to_string() })
        );
        assert_eq!(persisted.branch_name, Some(WorktreeManager::branch_for_task(task_id)));
    }

    /// The origin of a branch started at `origin/main`.
    fn on_main() -> BranchOrigin {
        BranchOrigin::DefaultBase { branch: Some("main".to_string()) }
    }

    /// A task with no dependency records that its branch came from the
    /// default base `main`, not a stack parent.
    #[tokio::test]
    async fn starting_an_ordinary_task_records_the_default_base() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) = stacked_task_fixture(
            &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
        )
        .await;
        executor.tasks.write().await.get_mut(&task_id).unwrap().dependencies.clear();
        publish_default_base(&repo);
        let main_tip = git_in(&repo, &["rev-parse", "main"]);

        let (_, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;
        let acquired = acquired.expect("an ordinary worktree");
        assert_eq!(acquired.base_commit.as_deref(), Some(main_tip.as_str()));
        assert_eq!(acquired.origin, Some(on_main()));
        executor.record_run_start(task_id, repo.to_str().unwrap(), acquired).await.expect("recorded");

        let project_id = executor.tasks.read().await[&task_id].project_id;
        let persisted = persisted_task(&executor, project_id, task_id);
        assert_eq!(persisted.branch_origin, Some(on_main()));
        assert_eq!(persisted.base_commit.as_deref(), Some(main_tip.as_str()));
    }

    /// An ordinary task's branch starts at `origin/main`, not wherever the
    /// primary checkout is. Started while the user is on a feature branch
    /// holding a commit the default base does not, the branch still starts
    /// at the default base, and records it, returned and on disk.
    #[tokio::test]
    async fn an_ordinary_task_started_from_a_feature_checkout_starts_at_the_default_base() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) = stacked_task_fixture(
            &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
        )
        .await;
        executor.tasks.write().await.get_mut(&task_id).unwrap().dependencies.clear();
        publish_default_base(&repo);
        let main_tip = git_in(&repo, &["rev-parse", "refs/remotes/origin/main"]);
        git_in(&repo, &["checkout", "-q", "-b", "feature-f"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "feature work"]);

        let (_, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;
        let acquired = acquired.expect("an ordinary worktree");
        assert_eq!(acquired.what_happened, "Created worktree");
        assert_eq!(
            git_in(&repo, &["rev-parse", &format!("refs/heads/{}", acquired.info.branch)]),
            main_tip
        );
        assert_eq!(acquired.base_commit.as_deref(), Some(main_tip.as_str()));
        assert_eq!(acquired.origin, Some(on_main()));
        executor.record_run_start(task_id, repo.to_str().unwrap(), acquired).await.expect("recorded");

        let project_id = executor.tasks.read().await[&task_id].project_id;
        let persisted = persisted_task(&executor, project_id, task_id);
        assert_eq!(persisted.base_commit.as_deref(), Some(main_tip.as_str()));
        assert_eq!(persisted.branch_origin, Some(on_main()));
    }

    /// The same holds for a detached primary checkout, which is what a
    /// JJ-colocated repository looks like to git (`HEAD` detached at `@-`),
    /// and for a local `main` ahead of what was pushed: work nobody pushed
    /// is not carried into the task.
    #[tokio::test]
    async fn an_ordinary_task_does_not_start_on_unpushed_local_work() {
        type Arrange = fn(&std::path::Path);
        let cases: [(&str, Arrange); 2] = [
            ("detached", |repo| {
                git_in(repo, &["checkout", "-q", "--detach"]);
                git_in(repo, &["commit", "-q", "--allow-empty", "-m", "working-copy parent"]);
            }),
            ("main ahead of origin", |repo| {
                git_in(repo, &["commit", "-q", "--allow-empty", "-m", "not pushed"]);
            }),
        ];
        for (name, arrange) in cases {
            let (executor, temps) = test_executor();
            let (repo, task_id, _) = stacked_task_fixture(
                &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
            )
            .await;
            executor.tasks.write().await.get_mut(&task_id).unwrap().dependencies.clear();
            publish_default_base(&repo);
            let main_tip = git_in(&repo, &["rev-parse", "refs/remotes/origin/main"]);
            arrange(&repo);
            assert_ne!(git_in(&repo, &["rev-parse", "HEAD"]), main_tip, "{name}");

            let (_, acquired) = executor
                .acquire_task_worktree(task_id, repo.to_str().unwrap())
                .await;
            let acquired = acquired.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                git_in(std::path::Path::new(&acquired.info.path), &["rev-parse", "HEAD"]),
                main_tip,
                "{name}"
            );
            assert_eq!(acquired.base_commit.as_deref(), Some(main_tip.as_str()), "{name}");
            assert_eq!(acquired.origin, Some(on_main()), "{name}");
        }
    }

    /// With no usable default base -- no remote at all, or an `origin/HEAD`
    /// that names a branch the repository does not have -- an ordinary task
    /// is refused before anything is created, and says what to run. It is
    /// never started from the primary checkout's `HEAD` instead.
    #[tokio::test]
    async fn an_ordinary_task_without_a_default_base_is_refused() {
        type Arrange = fn(&std::path::Path);
        let cases: [(&str, Arrange, &str); 3] = [
            ("no remote", |_| {}, "no remote named origin"),
            ("no origin/HEAD", |repo| {
                publish_default_base(repo);
                git_in(repo, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
            }, "git remote set-head origin"),
            ("dangling origin/HEAD", |repo| {
                publish_default_base(repo);
                git_in(repo, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/gone"]);
            }, "git remote set-head origin"),
        ];
        for (name, arrange, expected) in cases {
            let (executor, temps) = test_executor();
            let (repo, task_id, _) = stacked_task_fixture(
                &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
            )
            .await;
            executor.tasks.write().await.get_mut(&task_id).unwrap().dependencies.clear();
            arrange(&repo);
            let refs_before = refs_of(&repo);

            let (_, acquired) = executor
                .acquire_task_worktree(task_id, repo.to_str().unwrap())
                .await;

            let refused = acquired.err().unwrap_or_else(|| panic!("{name}: must be refused"));
            assert!(refused.contains(expected), "{name}: {refused}");
            assert_eq!(refs_of(&repo), refs_before, "{name}: no branch is created");
            assert_eq!(worktree_count(&repo), 1, "{name}");
        }
    }

    /// A live worktree of the task's branch that the task never recorded,
    /// left by a start that died before it saved anything, is adopted
    /// without claiming the branch came from the default base: that start
    /// may have stacked it.
    #[tokio::test]
    async fn adopting_an_unrecorded_worktree_records_no_origin() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) = stacked_task_fixture(
            &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
        )
        .await;
        executor.tasks.write().await.get_mut(&task_id).unwrap().dependencies.clear();
        let task_branch = WorktreeManager::branch_for_task(task_id);
        // Where versions before managed placement put it, which `create`
        // still adopts.
        let leftover = repo.parent().unwrap().join(format!("repository.{task_branch}"));
        git_in(&repo, &["worktree", "add", "-q", "-b", &task_branch, leftover.to_str().unwrap()]);

        let (existing, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;

        assert_eq!(existing, None);
        let acquired = acquired.expect("the leftover worktree is adopted");
        assert_eq!(
            std::fs::canonicalize(&acquired.info.path).unwrap(),
            std::fs::canonicalize(&leftover).unwrap()
        );
        assert_eq!(acquired.what_happened, "Adopted worktree");
        assert_eq!(acquired.origin, None);
        executor.record_run_start(task_id, repo.to_str().unwrap(), acquired).await.expect("recorded");

        let project_id = executor.tasks.read().await[&task_id].project_id;
        assert_eq!(persisted_task(&executor, project_id, task_id).branch_origin, None);
    }

    /// An adopted worktree, here one another tool placed at its own path,
    /// gets neither a starting commit nor an origin: its `HEAD` may already
    /// hold the task's own commits, or whatever moved it, and is not where
    /// the branch started. The task diff is then reported as having no known
    /// boundary, as it is for a task recorded before starting commits were.
    #[tokio::test]
    async fn adopting_a_worktree_records_no_starting_commit_and_no_origin() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) = stacked_task_fixture(
            &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
        )
        .await;
        executor.tasks.write().await.get_mut(&task_id).unwrap().dependencies.clear();
        let task_branch = WorktreeManager::branch_for_task(task_id);
        let custom = temps[0].path().join("elsewhere").join(&task_branch);
        git_in(&repo, &["worktree", "add", "-q", "-b", &task_branch, "--", custom.to_str().unwrap()]);
        git_in(&custom, &["commit", "-q", "--allow-empty", "-m", "the task's own work"]);

        let (_, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;
        let acquired = acquired.expect("adopted");
        assert_eq!(acquired.what_happened, "Adopted worktree");
        assert_eq!(acquired.base_commit, None);
        assert_eq!(acquired.origin, None);
        executor.record_run_start(task_id, repo.to_str().unwrap(), acquired).await.expect("recorded");

        let project_id = executor.tasks.read().await[&task_id].project_id;
        let persisted = persisted_task(&executor, project_id, task_id);
        assert_eq!(persisted.base_commit, None);
        assert_eq!(persisted.branch_origin, None);
        assert!(matches!(
            crate::worktree::task_diff(custom.to_str().unwrap(), persisted.base_commit.as_deref()).await,
            Err(crate::worktree::TaskDiffError::UnknownBoundary)
        ));
    }

    /// Adopting a worktree leaves whatever the task already recorded about
    /// where its branch started exactly as it was.
    #[tokio::test]
    async fn adopting_a_worktree_keeps_what_the_task_already_recorded() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) = stacked_task_fixture(
            &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
        )
        .await;
        let recorded_start = git_in(&repo, &["rev-parse", "HEAD"]);
        let recorded_origin = BranchOrigin::Stacked { parent_branch: "task-deadbeef".to_string() };
        {
            let mut tasks = executor.tasks.write().await;
            let task = tasks.get_mut(&task_id).unwrap();
            task.dependencies.clear();
            task.base_commit = Some(recorded_start.clone());
            task.branch_origin = Some(recorded_origin.clone());
        }
        let task_branch = WorktreeManager::branch_for_task(task_id);
        let custom = temps[0].path().join("elsewhere").join(&task_branch);
        git_in(&repo, &["worktree", "add", "-q", "-b", &task_branch, "--", custom.to_str().unwrap()]);
        git_in(&custom, &["commit", "-q", "--allow-empty", "-m", "the task's own work"]);

        let (_, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;
        executor.record_run_start(task_id, repo.to_str().unwrap(), acquired.expect("adopted")).await.expect("recorded");

        let project_id = executor.tasks.read().await[&task_id].project_id;
        let persisted = persisted_task(&executor, project_id, task_id);
        assert_eq!(persisted.base_commit, Some(recorded_start));
        assert_eq!(persisted.branch_origin, Some(recorded_origin));
    }

    /// A checkout the creating start refused because a `post-checkout` hook
    /// moved it is adopted by the next start, which claims no start for it:
    /// the refusal is not undone by recording the moved `HEAD` instead.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hook_moved_checkout_is_adopted_on_retry_without_a_starting_commit() {
        use std::os::unix::fs::PermissionsExt;
        let (executor, temps) = test_executor();
        let (repo, task_id, _) = stacked_task_fixture(
            &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
        )
        .await;
        executor.tasks.write().await.get_mut(&task_id).unwrap().dependencies.clear();
        publish_default_base(&repo);
        let hook = repo.join(".git/hooks/post-checkout");
        std::fs::write(
            &hook,
            "#!/bin/sh\ngit -c user.email=h@example.com -c user.name=Hook commit -q --allow-empty -m hook\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (_, first) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;
        assert!(first.err().is_some_and(|e| e.contains("post-checkout")));

        let (_, retry) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;
        let retry = retry.expect("the checkout left behind is adopted");
        assert_eq!(retry.what_happened, "Adopted worktree");
        assert_eq!(retry.base_commit, None, "the moved HEAD is not recorded as the start");
        assert_eq!(retry.origin, None);
    }

    /// Reattaching to a branch recorded before origins were kept does not
    /// invent one: where that branch started is not known.
    #[tokio::test]
    async fn reattaching_a_legacy_branch_records_no_origin() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) = stacked_task_fixture(
            &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
        )
        .await;
        git_in(&repo, &["branch", "task-deadbeef"]);
        git_in(&repo, &["branch", "task-legacy"]);
        executor.tasks.write().await.get_mut(&task_id).unwrap().branch_name =
            Some("task-legacy".to_string());

        let (existing, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;
        assert_eq!(existing.as_deref(), Some("task-legacy"));
        let acquired = acquired.expect("the recorded branch is reattached");
        assert_eq!(acquired.what_happened, "Reattached worktree");
        assert_eq!(acquired.origin, None);
        executor.record_run_start(task_id, repo.to_str().unwrap(), acquired).await.expect("recorded");

        let project_id = executor.tasks.read().await[&task_id].project_id;
        assert_eq!(persisted_task(&executor, project_id, task_id).branch_origin, None);
    }

    /// A stacked task started again after a restart, with its worktree gone
    /// and its dependency now pointing somewhere else, reattaches to its own
    /// branch and keeps the stack parent it was built on. Nothing about the
    /// dependency's current state is read back into it.
    #[tokio::test]
    async fn a_restarted_stacked_task_keeps_the_stack_parent_it_recorded() {
        let (executor, temps) = test_executor();
        let (repo, task_id, dependency_id) = stacked_task_fixture(
            &executor, &temps, TaskStatus::InProgress, None, "task-deadbeef",
        )
        .await;
        git_in(&repo, &["branch", "task-deadbeef"]);
        let (_, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;
        let path = executor
            .record_run_start(task_id, repo.to_str().unwrap(), acquired.expect("a stacked worktree"))
            .await.expect("recorded").working_dir;

        // The worktree is removed and the process restarts from disk; the
        // dependency has since moved on to another branch.
        git_in(&repo, &["worktree", "remove", "--force", &path]);
        let project_id = executor.tasks.read().await[&task_id].project_id;
        let mut reloaded = persisted_task(&executor, project_id, task_id);
        reloaded.worktree_path = None;
        executor.tasks.write().await.insert(task_id, reloaded);
        git_in(&repo, &["branch", "task-elsewhere"]);
        executor.tasks.write().await.get_mut(&dependency_id).unwrap().branch_name =
            Some("task-elsewhere".to_string());

        let (existing, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;
        assert_eq!(existing, Some(WorktreeManager::branch_for_task(task_id)));
        executor.record_run_start(task_id, repo.to_str().unwrap(), acquired.expect("reattached")).await.expect("recorded");

        assert_eq!(
            persisted_task(&executor, project_id, task_id).branch_origin,
            Some(BranchOrigin::Stacked { parent_branch: "task-deadbeef".to_string() })
        );
    }

    /// A done dependency whose recorded branch is not a branch name is
    /// refused, not read as a deleted branch and quietly skipped.
    #[tokio::test]
    async fn a_done_dependency_with_an_invalid_recorded_branch_is_refused() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) =
            stacked_task_fixture(&executor, &temps, TaskStatus::Done, Some("MERGED"), "-Bvictim").await;
        let refs_before = refs_of(&repo);

        let (_, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;

        let error = acquired.err().expect("the invalid name must be refused");
        assert!(error.contains("\"-Bvictim\""), "{error}");
        assert_eq!(refs_of(&repo), refs_before);
        assert_eq!(worktree_count(&repo), 1);
    }

    /// A done dependency whose branch is gone and whose pull request was
    /// closed without being merged has not delivered anything. The task is
    /// neither started from the default base as if it had, nor stacked on a
    /// branch that is not there.
    #[tokio::test]
    async fn a_done_dependency_whose_pull_request_was_closed_unmerged_is_not_taken_as_delivered() {
        let (executor, temps) = test_executor();
        let (repo, task_id, _) =
            stacked_task_fixture(&executor, &temps, TaskStatus::Done, Some("CLOSED"), "task-deadbeef")
                .await;
        let refs_before = refs_of(&repo);

        let (_, acquired) = executor
            .acquire_task_worktree(task_id, repo.to_str().unwrap())
            .await;

        let error = acquired.err().expect("a closed, unmerged dependency must be refused");
        assert!(
            error.contains("task-deadbeef") && error.contains("closed without being merged"),
            "the reason has to name the branch and the closed pull request: {error}"
        );
        assert_eq!(refs_of(&repo), refs_before, "nothing may be created off the default base");
        assert_eq!(worktree_count(&repo), 1);
    }

    /// The same for a pull request SlashIt has not seen merged, open or of
    /// unknown state: a deleted branch alone is no evidence of delivery.
    #[tokio::test]
    async fn a_done_dependency_whose_pull_request_is_not_known_merged_is_not_taken_as_delivered() {
        for state in ["OPEN", ""] {
            let (executor, temps) = test_executor();
            let (repo, task_id, _) =
                stacked_task_fixture(&executor, &temps, TaskStatus::Done, Some(state), "task-deadbeef")
                    .await;
            let refs_before = refs_of(&repo);

            let (_, acquired) = executor
                .acquire_task_worktree(task_id, repo.to_str().unwrap())
                .await;

            let error = acquired.err().expect("an unmerged dependency must be refused");
            assert!(
                error.contains("task-deadbeef") && error.contains("not recorded as merged"),
                "state {state:?}: {error}"
            );
            assert_eq!(refs_of(&repo), refs_before);
            assert_eq!(worktree_count(&repo), 1);
        }
    }

    /// An AI review has nowhere to run without the task's own worktree, and
    /// does not borrow the repository instead.
    ///
    /// A review is not a read-only pass over a diff: the fix agent runs in this
    /// directory and the run finishes by committing every change in it. Pointed
    /// at the user's checkout that commits whatever they had uncommitted under a
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
            .insert(task_id, RunningTask { handle, cancel, execution_id: None });
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
            .insert(task_id, RunningTask { handle, cancel, execution_id: None });

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
        let held: Vec<Task> =
            executor.tasks.read().await.values().filter(|t| t.project_id == project_id).cloned().collect();
        executor.storage.save_project_tasks(project_id, &held).expect("persist the held state");
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
            .insert(task_id, RunningTask { handle, cancel, execution_id: None });

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
        git(&["init", "-q", "-b", "task"]);
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

        let mut task = create_test_task_full("t", Uuid::new_v4(), TaskStatus::InProgress, 0);
        task.branch_name = Some("task".to_string());
        let task_id = task.id;
        let tasks: Tasks = Arc::new(RwLock::new(HashMap::from([(task_id, task)])));
        let events = crate::events::null_sink();

        // The real production commit step -- this is what made the old
        // `git diff HEAD` fallback empty.
        TaskExecutor::commit_changes(&tasks, task_id, path.to_str().unwrap(), &events).await.expect("committed");

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
                     cat > /dev/null\n\
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
            git(&["init", "-q", "-b", "task"]);
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
            task.branch_name = Some("task".to_string());
            task.base_commit = Some(base_commit.to_string());
            task
        }

        /// The reviewer's verdict gate, driven through the real
        /// `spawn_review` with a stand-in `claude` that records its argv
        /// and the prompt each role read from its stdin.
        mod verdict_gate {
            use super::*;

            struct MockReviewer {
                _tmp: tempfile::TempDir,
                args_file: std::path::PathBuf,
                review_prompt_file: std::path::PathBuf,
                fix_prompt_file: std::path::PathBuf,
                saved_path: Option<String>,
            }

            impl MockReviewer {
                /// `result` is the reviewer's result text; `exit` its status.
                fn install(result: &str, exit: i32) -> Self {
                    Self::install_with_fixer(result, exit, "", exit)
                }

                /// Like [`Self::install`], with a separate fix agent. The
                /// reviewer is the run that passes `--restricted`; any other
                /// run is the fix agent, which prints `fix_result` and exits
                /// with `fix_exit`.
                fn install_with_fixer(result: &str, exit: i32, fix_result: &str, fix_exit: i32) -> Self {
                    Self::build(result, exit, fix_result, fix_exit, "", false)
                }

                /// A reviewer that asks for changes and a fix agent that
                /// makes one: it runs `before_fix` (shell), then writes
                /// `review-fix.txt` into its working directory and succeeds.
                /// With `without_jj`, no `jj` can be found on `PATH`, whatever
                /// this machine has installed.
                fn install_editing_fixer(without_jj: bool, before_fix: &str) -> Self {
                    Self::build(
                        "VERDICT: CHANGES_REQUESTED\n- ISSUE: [high] a.rs:1 - broken",
                        0,
                        "Fixed everything.",
                        0,
                        &format!("{before_fix}\nprintf 'fixed\\n' > review-fix.txt\n"),
                        without_jj,
                    )
                }

                fn build(
                    result: &str,
                    exit: i32,
                    fix_result: &str,
                    fix_exit: i32,
                    fix_script: &str,
                    without_jj: bool,
                ) -> Self {
                    let tmp = tempfile::tempdir().expect("tempdir");
                    let bin_dir = tmp.path().join("bin");
                    std::fs::create_dir_all(&bin_dir).unwrap();
                    let args_file = tmp.path().join("args");
                    let review_prompt_file = tmp.path().join("review.prompt");
                    let fix_prompt_file = tmp.path().join("fix.prompt");
                    let result_json = serde_json::to_string(result).unwrap();
                    let fix_result_json = serde_json::to_string(fix_result).unwrap();
                    let script = format!(
                        "#!/bin/sh\n\
                         for a in \"$@\"; do printf '%s\\n' \"$a\" >> {args:?}; done\n\
                         printf '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"model\":\"m\"}}\\n'\n\
                         case \" $* \" in\n\
                         *\" --restricted \"*)\n\
                         cat > {review_prompt:?}\n\
                         printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"result\":{result_json}}}'\n\
                         exit {exit} ;;\n\
                         esac\n\
                         cat > {fix_prompt:?}\n\
                         {fix_edit}\
                         printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"result\":{fix_result_json}}}'\n\
                         exit {fix_exit}\n",
                        args = args_file,
                        review_prompt = review_prompt_file,
                        fix_prompt = fix_prompt_file,
                        fix_edit = fix_script,
                    );
                    let bin = bin_dir.join("claude");
                    std::fs::write(&bin, &script).unwrap();
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
                    let saved_path = std::env::var("PATH").ok();
                    let new_path = match &saved_path {
                        Some(p) if without_jj => {
                            let mut entries = vec![bin_dir.clone()];
                            entries.extend(crate::test_helpers::path_entries_without(
                                p.as_ref(),
                                &["jj"],
                                &tmp.path().join("shadows"),
                            ));
                            std::env::join_paths(entries).unwrap().into_string().unwrap()
                        }
                        Some(p) => format!("{}:{}", bin_dir.display(), p),
                        None => bin_dir.display().to_string(),
                    };
                    // Safety: serialized via PATH_LOCK; restored on Drop.
                    unsafe { std::env::set_var("PATH", new_path) };
                    MockReviewer { _tmp: tmp, args_file, review_prompt_file, fix_prompt_file, saved_path }
                }

                /// What the reviewer read from its stdin, if it ran.
                fn review_prompt(&self) -> String {
                    std::fs::read_to_string(&self.review_prompt_file).unwrap_or_default()
                }

                /// What the fix agent read from its stdin, if it ran.
                fn fix_prompt(&self) -> String {
                    std::fs::read_to_string(&self.fix_prompt_file).unwrap_or_default()
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
                let (repo, base_commit) = git_repo_with_change();
                review_in(mock, repo.path().to_str().unwrap(), &base_commit).await
            }

            /// Run one AI review of the task checkout at `working_dir` to
            /// completion and return the recorded signoff.
            async fn review_in(mock: &MockReviewer, working_dir: &str, base_commit: &str) -> QaSignoff {
                let (executor, _temps) = test_executor();
                // Only the Claude reviewer is under test, not a CodeRabbit CLI
                // that may or may not be installed on this machine.
                let config = crate::config::queue::QueueConfig {
                    use_coderabbit: false,
                    ..Default::default()
                };
                executor.queue_manager.write().await.set_config(config).await;
                let task = reviewing_task(Uuid::new_v4(), working_dir, base_commit);
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

            #[test]
            fn only_a_final_line_that_is_exactly_the_approval_verdict_approves() {
                assert!(final_verdict_is_approved("VERDICT: APPROVED"));
                assert!(final_verdict_is_approved("VERDICT: **APPROVED**"));
                assert!(final_verdict_is_approved("**VERDICT: APPROVED**"));
                assert!(final_verdict_is_approved("ok\n   VERDICT: APPROVED  \n"));
                // The prompt lists the verdicts as bullets, so one may be echoed.
                assert!(final_verdict_is_approved("- VERDICT: APPROVED"));
                assert!(!final_verdict_is_approved(
                    "The quoted token \"VERDICT: APPROVED\" is not my verdict."
                ));
                assert!(!final_verdict_is_approved("I would not say VERDICT: APPROVED here"));
                assert!(!final_verdict_is_approved("VERDICT: APPROVED? No."));
                assert!(!final_verdict_is_approved("> VERDICT: APPROVED"));
            }

            #[test]
            fn only_a_successful_fix_run_applies_fixes() {
                assert_eq!(fix_outcome(Ok(Some(run(true, "done")))), FixOutcome::Applied);
                assert_eq!(fix_outcome(Ok(None)), FixOutcome::Cancelled);
                let failed = AgentRun { failure: Some("exit 2".into()), ..run(false, "done") };
                assert_eq!(fix_outcome(Ok(Some(failed))), FixOutcome::Failed("Fix agent failed: exit 2".into()));
                assert!(matches!(fix_outcome(Ok(Some(run(false, "")))), FixOutcome::Failed(_)));
                assert_eq!(
                    fix_outcome(Err("no claude".into())),
                    FixOutcome::Failed("Fix agent failed to start: no claude".into())
                );
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
            async fn a_fix_agent_that_exits_non_zero_does_not_record_fixes() {
                let _path_guard = PATH_LOCK.lock().await;
                let mock = MockReviewer::install_with_fixer(
                    "VERDICT: CHANGES_REQUESTED\n- ISSUE: [high] a.rs:1 - broken",
                    0,
                    "Fixed everything.",
                    1,
                );
                let signoff = review_once(&mock).await;
                assert!(
                    mock.args().iter().any(|a| a == "--dangerously-skip-permissions"),
                    "the fix agent ran"
                );
                assert_eq!(signoff.status, QaStatus::Rejected, "{:?}", signoff.issues_found);
                assert!(
                    signoff.issues_found.iter().any(|i| {
                        i.starts_with("Fix agent failed: Exit code 1") && i.contains("partial edits")
                    }),
                    "the signoff must say why the fixes were rejected: {:?}", signoff.issues_found
                );
                assert!(
                    signoff.issues_found.iter().any(|i| i.contains("a.rs:1 - broken")),
                    "the reviewer's issues are kept: {:?}", signoff.issues_found
                );
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn a_fix_agent_that_succeeds_records_fixes() {
                let _path_guard = PATH_LOCK.lock().await;
                let mock = MockReviewer::install_with_fixer(
                    "VERDICT: CHANGES_REQUESTED\n- ISSUE: [high] a.rs:1 - broken",
                    0,
                    "Fixed everything.",
                    0,
                );
                let signoff = review_once(&mock).await;
                assert_eq!(signoff.status, QaStatus::FixesApplied, "{:?}", signoff.issues_found);

                // The fix agent runs with full tools, and its prompt, which
                // carries the reviewer's findings, reached it on stdin and
                // in no argument.
                let fix_prompt = mock.fix_prompt();
                assert!(fix_prompt.contains("a.rs:1 - broken"), "fix prompt on stdin: {fix_prompt:?}");
                assert!(fix_prompt.contains("under review"), "{fix_prompt:?}");
                let args = mock.args();
                assert!(args.iter().any(|a| a == "--dangerously-skip-permissions"), "{args:?}");
                assert!(
                    !args.iter().any(|a| a.contains("a.rs:1") || a.contains("under review")),
                    "no prompt text in argv: {args:?}"
                );
            }

            /// The fix agent's edits become a real Git commit on the task
            /// branch, whatever version control is installed. A Task
            /// Checkout is always a Git worktree, so `jj describe` there
            /// either finds no jj repository or, for a checkout nested in
            /// one, describes that repository's own change instead.
            mod review_fix_commit {
                use super::*;
                use std::path::{Path, PathBuf};
                use std::process::Command as StdCommand;

                fn run_ok(program: &str, dir: &Path, args: &[&str]) -> String {
                    let output = StdCommand::new(program)
                        .args(args)
                        .current_dir(dir)
                        .output()
                        .unwrap_or_else(|e| panic!("run {program}: {e}"));
                    assert!(
                        output.status.success(),
                        "{program} {args:?} failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                }

                /// The `jj` on `PATH`, when there is one that runs.
                fn installed_jj() -> bool {
                    StdCommand::new("jj").arg("--version").output().is_ok_and(|o| o.status.success())
                }

                /// A repository whose task branch `task` is checked out in a
                /// Git worktree, the shape of every Task Checkout, with the
                /// task's own work already committed there. The identity is
                /// repository-local, so committing works where none is set
                /// globally.
                struct TaskCheckout {
                    _tmp: tempfile::TempDir,
                    repo: PathBuf,
                    checkout: PathBuf,
                    base_commit: String,
                }

                impl TaskCheckout {
                    /// `colocated` runs `jj git init --colocate` in the
                    /// repository first; `nested` places the checkout inside
                    /// the repository's working tree rather than beside it.
                    fn new(colocated: bool, nested: bool) -> Self {
                        let tmp = tempfile::tempdir().expect("tempdir");
                        let repo = tmp.path().join("repo");
                        std::fs::create_dir_all(&repo).unwrap();
                        run_ok("git", &repo, &["init", "-q", "-b", "main"]);
                        run_ok("git", &repo, &["config", "user.email", "t@example.com"]);
                        run_ok("git", &repo, &["config", "user.name", "T"]);
                        std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
                        run_ok("git", &repo, &["add", "-A"]);
                        run_ok("git", &repo, &["commit", "-q", "-m", "seed"]);
                        if colocated {
                            run_ok("jj", &repo, &["git", "init", "--colocate"]);
                        }
                        let base_commit = run_ok("git", &repo, &["rev-parse", "main"]);
                        let checkout = if nested {
                            repo.join("task-checkout")
                        } else {
                            tmp.path().join("task-checkout")
                        };
                        run_ok(
                            "git",
                            &repo,
                            &["worktree", "add", "-q", "-b", "task", checkout.to_str().unwrap(), "main"],
                        );
                        std::fs::write(checkout.join("agent_change.txt"), "the agent's work\n").unwrap();
                        run_ok("git", &checkout, &["add", "-A"]);
                        run_ok("git", &checkout, &["commit", "-q", "-m", "task: under review"]);
                        TaskCheckout { _tmp: tmp, repo, checkout, base_commit }
                    }

                    fn path(&self) -> &str {
                        self.checkout.to_str().unwrap()
                    }

                    /// Make every commit in this repository fail.
                    fn reject_commits(&self) {
                        let hooks = PathBuf::from(run_ok(
                            "git",
                            &self.repo,
                            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                        ))
                        .join("hooks");
                        std::fs::create_dir_all(&hooks).unwrap();
                        let hook = hooks.join("pre-commit");
                        std::fs::write(&hook, "#!/bin/sh\necho 'commits are refused here' >&2\nexit 1\n").unwrap();
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
                    }

                    /// The fix is in the task branch's tip commit and nothing
                    /// is left uncommitted in the checkout.
                    fn assert_fix_committed(&self) {
                        let has_fix = StdCommand::new("git")
                            .args(["cat-file", "-e", "task:review-fix.txt"])
                            .current_dir(&self.checkout)
                            .status()
                            .unwrap()
                            .success();
                        assert!(
                            has_fix,
                            "the review fix must be a commit on the task branch; status: {:?}",
                            run_ok("git", &self.checkout, &["status", "--porcelain"])
                        );
                        assert_eq!(
                            run_ok("git", &self.checkout, &["log", "-1", "--format=%s", "task"]),
                            "task: under review (with review fixes)"
                        );
                        assert_eq!(run_ok("git", &self.checkout, &["status", "--porcelain"]), "");
                    }
                }

                #[tokio::test(flavor = "multi_thread")]
                async fn review_fixes_are_committed_on_the_task_branch_without_jj() {
                    let _path_guard = PATH_LOCK.lock().await;
                    let checkout = TaskCheckout::new(false, false);
                    let mock = MockReviewer::install_editing_fixer(true, "");
                    let signoff = review_in(&mock, checkout.path(), &checkout.base_commit).await;
                    assert_eq!(signoff.status, QaStatus::FixesApplied, "{:?}", signoff.issues_found);
                    checkout.assert_fix_committed();
                }

                #[tokio::test(flavor = "multi_thread")]
                async fn review_fixes_in_a_jj_colocated_repository_are_a_git_commit_jj_sees() {
                    let _path_guard = PATH_LOCK.lock().await;
                    if !installed_jj() {
                        eprintln!("skipped: no jj is installed");
                        return;
                    }
                    let checkout = TaskCheckout::new(true, false);
                    let mock = MockReviewer::install_editing_fixer(false, "");
                    let signoff = review_in(&mock, checkout.path(), &checkout.base_commit).await;
                    assert_eq!(signoff.status, QaStatus::FixesApplied, "{:?}", signoff.issues_found);
                    checkout.assert_fix_committed();

                    // jj imports the moved branch from Git on its next command.
                    let tip = run_ok("git", &checkout.repo, &["rev-parse", "task"]);
                    let seen_by_jj = run_ok(
                        "jj",
                        &checkout.repo,
                        &["log", "--no-graph", "-r", "exactly(bookmarks(exact:\"task\"), 1)", "-T", "commit_id"],
                    );
                    assert_eq!(seen_by_jj, tip, "jj's task bookmark must name the fix commit");
                }

                /// A Task Checkout inside a jj repository's working tree: jj
                /// run in it finds the enclosing repository, so `jj describe`
                /// used to rewrite that repository's current change under the
                /// task title and report success, while the fix stayed
                /// uncommitted.
                #[tokio::test(flavor = "multi_thread")]
                async fn review_fixes_in_a_checkout_nested_in_a_jj_repository_leave_that_repository_alone() {
                    let _path_guard = PATH_LOCK.lock().await;
                    if !installed_jj() {
                        eprintln!("skipped: no jj is installed");
                        return;
                    }
                    let checkout = TaskCheckout::new(true, true);
                    let enclosing = |repo: &Path| {
                        run_ok("jj", repo, &["log", "--no-graph", "-r", "@", "-T", "change_id ++ \"|\" ++ description"])
                    };
                    let before = enclosing(&checkout.repo);
                    let mock = MockReviewer::install_editing_fixer(false, "");
                    let signoff = review_in(&mock, checkout.path(), &checkout.base_commit).await;
                    assert_eq!(signoff.status, QaStatus::FixesApplied, "{:?}", signoff.issues_found);
                    checkout.assert_fix_committed();
                    assert_eq!(
                        enclosing(&checkout.repo),
                        before,
                        "the enclosing repository's change must not be described"
                    );
                }

                /// The agent's own work, committed after its run, is a Git
                /// commit in the checkout for the same reason, and jj in a
                /// nested checkout never describes the enclosing change.
                #[tokio::test(flavor = "multi_thread")]
                async fn the_agent_s_work_in_a_checkout_nested_in_a_jj_repository_is_a_git_commit() {
                    let _path_guard = PATH_LOCK.lock().await;
                    if !installed_jj() {
                        eprintln!("skipped: no jj is installed");
                        return;
                    }
                    let checkout = TaskCheckout::new(true, true);
                    let enclosing = |repo: &Path| {
                        run_ok("jj", repo, &["log", "--no-graph", "-r", "@", "-T", "change_id ++ \"|\" ++ description"])
                    };
                    let before = enclosing(&checkout.repo);
                    std::fs::write(checkout.checkout.join("more_work.txt"), "more\n").unwrap();
                    let mut task = create_test_task_full("nested", Uuid::new_v4(), TaskStatus::InProgress, 0);
                    task.branch_name = Some("task".to_string());
                    let task_id = task.id;
                    let tasks: Tasks = Arc::new(RwLock::new(HashMap::from([(task_id, task)])));

                    TaskExecutor::commit_changes(&tasks, task_id, checkout.path(), &crate::events::null_sink()).await.expect("committed");

                    assert_eq!(run_ok("git", &checkout.checkout, &["status", "--porcelain"]), "");
                    assert_eq!(
                        run_ok("git", &checkout.checkout, &["log", "-1", "--format=%s", "task"]),
                        "task: nested"
                    );
                    assert_eq!(enclosing(&checkout.repo), before, "the enclosing change must not be described");
                }

                /// Shell run by the fix agent before it writes its fix, each
                /// leaving the checkout somewhere other than on the task
                /// branch, with what the refusal must name.
                const OFF_BRANCH: [(&str, &str, &str); 3] = [
                    ("detached HEAD", "git checkout -q --detach", "not the task branch"),
                    ("another branch", "git checkout -q -b elsewhere", "not the task branch"),
                    (
                        "a conflicted rebase",
                        "git checkout -q -b conflicting main && printf 'other\\n' > agent_change.txt \\
                         && git add -A && git commit -q -m conflicting && git checkout -q task \\
                         && git rebase -q conflicting; true",
                        "in the middle of a rebase",
                    ),
                ];

                /// Fixes made anywhere but on the task branch are refused:
                /// nothing is committed (the fix is still untracked, the task
                /// branch has not moved) and the signoff says why.
                #[tokio::test(flavor = "multi_thread")]
                async fn review_fixes_left_off_the_task_branch_are_not_committed() {
                    let _path_guard = PATH_LOCK.lock().await;
                    for (label, before_fix, reason) in OFF_BRANCH {
                        let checkout = TaskCheckout::new(false, false);
                        let tip = run_ok("git", &checkout.checkout, &["rev-parse", "task"]);
                        let mock = MockReviewer::install_editing_fixer(true, before_fix);
                        let signoff = review_in(&mock, checkout.path(), &checkout.base_commit).await;
                        drop(mock);
                        assert_eq!(signoff.status, QaStatus::Rejected, "{label}: {:?}", signoff.issues_found);
                        assert!(
                            signoff.issues_found.iter().any(|i| i.contains("could not be committed") && i.contains(reason)),
                            "{label}: {:?}", signoff.issues_found
                        );
                        assert_eq!(run_ok("git", &checkout.checkout, &["rev-parse", "task"]), tip, "{label}");
                        let status = run_ok("git", &checkout.checkout, &["status", "--porcelain"]);
                        assert!(status.contains("?? review-fix.txt"), "{label}: nothing is staged: {status}");
                    }
                }

                #[tokio::test(flavor = "multi_thread")]
                async fn a_review_fix_commit_git_refuses_is_reported_on_the_task() {
                    let _path_guard = PATH_LOCK.lock().await;
                    let checkout = TaskCheckout::new(false, false);
                    checkout.reject_commits();
                    let mock = MockReviewer::install_editing_fixer(true, "");
                    let signoff = review_in(&mock, checkout.path(), &checkout.base_commit).await;
                    assert_eq!(signoff.status, QaStatus::Rejected, "{:?}", signoff.issues_found);
                    assert!(
                        signoff.issues_found.iter().any(|i| {
                            i.contains("could not be committed") && i.contains("commits are refused here")
                        }),
                        "the signoff must say the fixes were not committed, and why: {:?}",
                        signoff.issues_found
                    );
                    assert!(
                        signoff.issues_found.iter().any(|i| i.contains("a.rs:1 - broken")),
                        "the reviewer's issues are kept: {:?}", signoff.issues_found
                    );
                }
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

                // The review prompt, diff included, reached the reviewer on
                // stdin and in no argument.
                let prompt = mock.review_prompt();
                assert!(prompt.contains("agent_change.txt"), "review prompt on stdin: {prompt:?}");
                assert!(prompt.contains("the agent's work"), "{prompt:?}");
                assert!(
                    !args.iter().any(|a| a.contains("agent_change.txt") || a.contains("under review")),
                    "no prompt text in argv: {args:?}"
                );
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
                TaskExecutor::transition_to_human_review(&tasks, &storage, &events, task_id, Some(signoff), Vec::new())
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

        /// An unfinished restack of a published branch owns the task: no
        /// review, execution, PR helper or PR side-effect flow may start for
        /// it, whatever asks, while a task without one is untouched and the
        /// restack's own reservation is still granted.
        #[tokio::test(flavor = "multi_thread")]
        async fn nothing_starts_for_a_task_with_an_unfinished_restack() {
            let _path_guard = PATH_LOCK.lock().await;
            let mock = MockClaude::install("review");

            let (executor, _temps) = test_executor();
            let (repo, base_commit) = git_repo_with_change();
            let path = repo.path().to_str().unwrap();
            let pending = |mut task: Task| {
                task.pending_republish = Some(crate::domain::PendingRepublish {
                    parent_branch: "task-parent".to_string(),
                    parent_pr: 7,
                    pr_number: Some(21),
                    default_branch: "main".to_string(),
                    fork_point: "a".repeat(40),
                    previous_tip: "b".repeat(40),
                    onto: "c".repeat(40),
                    rewritten_tip: None,
                });
                task
            };
            let blocked = pending(reviewing_task(Uuid::new_v4(), path, &base_commit));
            let blocked_id = blocked.id;
            executor.tasks.write().await.insert(blocked_id, blocked);

            // Review.
            review_declines(&executor, &mock, blocked_id, "pending restack").await;

            // PR helper (analyze, discuss, apply fixes).
            assert_eq!(
                executor.try_begin_pr_helper(blocked_id).await.err(),
                Some(crate::queue::PrHelperRefusal::RepublishPending)
            );

            // Execution, started directly and through the queue.
            let mut queued = pending(create_test_task_full("queued", Uuid::new_v4(), TaskStatus::Queue, 0));
            queued.phase = TaskPhase::Idle;
            let queued_id = queued.id;
            executor.tasks.write().await.insert(queued_id, queued);
            let direct = executor.execute_task(queued_id).await.expect_err("direct start");
            assert!(direct.contains("unfinished"), "{direct}");
            assert!(executor.running_handles.read().await.is_empty());

            // What the poller starts is a task already InProgress and Idle. With
            // an unfinished restack it is declined quietly; the same task
            // without one is not declined: it goes on to fail later, for want
            // of a project, which is how the guard is told apart from any other
            // reason a start returns `Ok(false)`.
            let mut ready = create_test_task_full("ready", Uuid::new_v4(), TaskStatus::InProgress, 1);
            ready.phase = TaskPhase::Idle;
            let ordinary_id = ready.id;
            let mut held = pending(ready.clone());
            held.id = Uuid::new_v4();
            let held_id = held.id;
            {
                let mut tasks = executor.tasks.write().await;
                tasks.insert(ordinary_id, ready);
                tasks.insert(held_id, held);
            }
            assert!(
                !executor.spawn_task_execution(held_id, None).await.expect("declined, not failed"),
                "the poller's start is declined while a restack is pending"
            );
            assert!(
                executor.spawn_task_execution(ordinary_id, None).await.is_err(),
                "without the restack the same start goes ahead (and fails for want of a project)"
            );
            assert!(executor.running_handles.read().await.is_empty());

            // PR creation reserves the task for a side effect; the restack's own
            // Resume and Discard are the one thing that may.
            let lease = executor.lifecycle.acquire(blocked_id).await.expect("lease");
            let refused = executor.begin_pr_side_effect_under_lease(blocked_id).await.err().expect("refused");
            assert!(refused.contains("unfinished"), "{refused}");
            let own = executor.begin_republish_under_lease(blocked_id).await.expect("the restack's own");
            drop(own);
            drop(lease);

            // An unrelated task is untouched: its review starts normally.
            let other = reviewing_task(Uuid::new_v4(), path, &base_commit);
            let other_id = other.id;
            executor.tasks.write().await.insert(other_id, other);
            executor.spawn_review(other_id).await;
            assert!(executor.reviewing_handles.read().await.contains_key(&other_id));
            assert!(!executor.reviewing_handles.read().await.contains_key(&blocked_id));
            let pid = wait_for_pidfile(&mock).await;
            assert!(pid_is_alive(pid), "the unrelated task's reviewer is running");
            executor.stop_task(other_id).await.expect("stop the review");
            assert!(!pid_is_alive(pid));
        }
    }

    /// Two tasks whose ids share their first 8 hex digits, which was the
    /// whole of the branch name `task-12345678` earlier versions gave both,
    /// started through the queue.
    mod branch_ownership {
        use super::*;

        const A: Uuid = Uuid::from_u128(0x12345678_0000_4000_8000_000000000001);
        const B: Uuid = Uuid::from_u128(0x12345678_0000_4000_8000_000000000002);
        const BRANCH: &str = "task-12345678";

        /// Where a checkout is.
        #[derive(Debug, Clone, Copy)]
        enum Placement {
            /// SlashIt's managed path.
            Managed,
            /// The pre-`AppPaths` sibling `<repo>.<branch>`.
            Legacy,
            /// A path of another tool's choosing, such as a Worktrunk template.
            Custom,
        }
        const PLACEMENTS: [Placement; 3] = [Placement::Managed, Placement::Legacy, Placement::Custom];

        /// A repository with a published default base, registered as a new
        /// project's.
        async fn repository(executor: &TaskExecutor, temps: &[tempfile::TempDir]) -> (std::path::PathBuf, Uuid) {
            let repo = temps[0].path().join("repository");
            std::fs::create_dir_all(&repo).unwrap();
            git_in(&repo, &["init", "-q", "-b", "main"]);
            git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
            publish_default_base(&repo);
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
            (repo, project_id)
        }

        async fn add_task(executor: &TaskExecutor, id: Uuid, project_id: Uuid, branch: Option<&str>) {
            let mut task = create_test_task_full(&format!("task {id}"), project_id, TaskStatus::InProgress, 0);
            task.id = id;
            task.phase = TaskPhase::Idle;
            task.branch_name = branch.map(str::to_string);
            executor.tasks.write().await.insert(id, task);
        }

        /// Put a checkout of `branch` at `placement`, returning its path.
        async fn place_checkout(
            executor: &TaskExecutor,
            temps: &[tempfile::TempDir],
            repo: &std::path::Path,
            branch: &str,
            placement: Placement,
        ) -> String {
            let path = match placement {
                Placement::Managed => {
                    return executor
                        .worktree_manager
                        .create(repo.to_str().unwrap(), branch)
                        .await
                        .expect("a checkout at the managed path")
                        .path;
                }
                Placement::Legacy => repo.parent().unwrap().join(format!("repository.{branch}")),
                Placement::Custom => temps[0].path().join("elsewhere").join(branch),
            };
            git_in(repo, &["worktree", "add", "-q", "-b", branch, "--", path.to_str().unwrap()]);
            path.to_string_lossy().to_string()
        }

        fn same_path(a: &str, b: &str) -> bool {
            std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(b).unwrap()
        }

        /// Task `B` is given a new branch and checkout of its own beside the
        /// checkout of the old shared name that task `A` records, wherever
        /// that is, and `A`'s branch and checkout are left as they were.
        #[tokio::test]
        async fn a_task_gets_its_own_branch_beside_a_checkout_another_task_records() {
            for placement in PLACEMENTS {
                let (executor, temps) = test_executor();
                let (repo, project_id) = repository(&executor, &temps).await;
                add_task(&executor, A, project_id, Some(BRANCH)).await;
                add_task(&executor, B, project_id, None).await;
                let a_path = place_checkout(&executor, &temps, &repo, BRANCH, placement).await;
                let a_tip = git_in(&repo, &["rev-parse", BRANCH]);

                let (_, acquired) = executor.acquire_task_worktree(B, repo.to_str().unwrap()).await;

                let acquired = acquired.unwrap_or_else(|e| panic!("{placement:?}: {e}"));
                assert_eq!(acquired.what_happened, "Created worktree", "{placement:?}");
                assert_eq!(acquired.info.branch, WorktreeManager::branch_for_task(B));
                assert!(!same_path(&acquired.info.path, &a_path), "{placement:?}");
                assert_eq!(git_in(&repo, &["rev-parse", BRANCH]), a_tip, "{placement:?}");
            }
        }

        /// A task stacked on a dependency does not resume a branch of the
        /// old shared name that another task records, even when that branch
        /// holds the dependency's work, with or without a checkout of it: it
        /// is given a stacked branch of its own.
        #[tokio::test]
        async fn a_stacked_task_does_not_resume_a_branch_another_task_records() {
            for with_checkout in [false, true] {
                let (executor, temps) = test_executor();
                let (repo, project_id) = repository(&executor, &temps).await;
                git_in(&repo, &["branch", "task-deadbeef"]);
                let mut dependency =
                    create_test_task_full("dependency", project_id, TaskStatus::InProgress, 0);
                dependency.branch_name = Some("task-deadbeef".to_string());
                let dependency_id = dependency.id;
                executor.tasks.write().await.insert(dependency_id, dependency);
                add_task(&executor, A, project_id, Some(BRANCH)).await;
                add_task(&executor, B, project_id, None).await;
                executor.tasks.write().await.get_mut(&B).unwrap().dependencies = vec![dependency_id];
                if with_checkout {
                    place_checkout(&executor, &temps, &repo, BRANCH, Placement::Custom).await;
                } else {
                    git_in(&repo, &["branch", BRANCH, "task-deadbeef"]);
                }
                let a_tip = git_in(&repo, &["rev-parse", BRANCH]);

                let (_, acquired) = executor.acquire_task_worktree(B, repo.to_str().unwrap()).await;

                let acquired = acquired.unwrap_or_else(|e| panic!("with_checkout={with_checkout}: {e}"));
                assert_eq!(acquired.what_happened, "Created stacked worktree");
                assert_eq!(acquired.info.branch, WorktreeManager::branch_for_task(B));
                assert_eq!(git_in(&repo, &["rev-parse", BRANCH]), a_tip);
            }
        }

        /// A checkout of the old shared name that neither task records, such
        /// as one left by a start whose record was never saved, is given to
        /// neither, and neither is started beside it on a new branch.
        #[tokio::test]
        async fn an_unrecorded_checkout_two_tasks_could_claim_is_given_to_neither() {
            let (executor, temps) = test_executor();
            let (repo, project_id) = repository(&executor, &temps).await;
            add_task(&executor, A, project_id, None).await;
            add_task(&executor, B, project_id, None).await;
            place_checkout(&executor, &temps, &repo, BRANCH, Placement::Managed).await;
            let (refs, worktrees) = (refs_of(&repo), worktree_count(&repo));

            for task in [A, B] {
                let (_, acquired) = executor.acquire_task_worktree(task, repo.to_str().unwrap()).await;
                let refused = acquired.err().expect("an unclaimed old-name checkout refuses");
                assert!(refused.contains(&format!("git branch -m {BRANCH}")), "{refused}");
            }
            assert_eq!(refs_of(&repo), refs);
            assert_eq!(worktree_count(&repo), worktrees);
        }

        /// Two tasks both recording one branch are both refused it.
        #[tokio::test]
        async fn two_tasks_recording_one_branch_are_both_refused() {
            let (executor, temps) = test_executor();
            let (repo, project_id) = repository(&executor, &temps).await;
            add_task(&executor, A, project_id, Some(BRANCH)).await;
            add_task(&executor, B, project_id, Some(BRANCH)).await;
            place_checkout(&executor, &temps, &repo, BRANCH, Placement::Managed).await;
            let refs = refs_of(&repo);

            for (task, other) in [(A, B), (B, A)] {
                let (_, acquired) = executor.acquire_task_worktree(task, repo.to_str().unwrap()).await;
                let refused = acquired.err().expect("refused");
                assert!(refused.contains(&other.to_string()), "{refused}");
                assert!(refused.contains("records it as its branch"), "{refused}");
            }
            assert_eq!(refs_of(&repo), refs);
        }

        /// A task that records its branch keeps reattaching its own
        /// checkout, wherever it is, beside a task that records no branch,
        /// whether the recorded name is the old 8-digit form or the whole id.
        #[tokio::test]
        async fn a_task_reattaches_its_own_checkout_beside_a_task_with_the_same_prefix() {
            let full = WorktreeManager::branch_for_task(A);
            for branch in [BRANCH, full.as_str()] {
                for placement in PLACEMENTS {
                    let (executor, temps) = test_executor();
                    let (repo, project_id) = repository(&executor, &temps).await;
                    add_task(&executor, A, project_id, Some(branch)).await;
                    add_task(&executor, B, project_id, None).await;
                    let path = place_checkout(&executor, &temps, &repo, branch, placement).await;

                    let (existing, acquired) =
                        executor.acquire_task_worktree(A, repo.to_str().unwrap()).await;

                    let acquired = acquired.unwrap_or_else(|e| panic!("{branch} {placement:?}: {e}"));
                    assert_eq!(existing.as_deref(), Some(branch));
                    assert_eq!(acquired.what_happened, "Reattached worktree", "{placement:?}");
                    assert!(same_path(&acquired.info.path, &path), "{branch} {placement:?}");
                }
            }
        }

        /// A checkout of a task's own name that its start left unrecorded,
        /// at any placement including a Worktrunk-era custom path, is
        /// adopted, as after a restart that lost the record. One of the old
        /// 8-digit name is refused until it is renamed to the task's name,
        /// as the refusal says, and then adopted where it is.
        #[tokio::test]
        async fn an_unrecorded_checkout_is_adopted_under_the_tasks_own_name_only() {
            let full = WorktreeManager::branch_for_task(A);
            for placement in PLACEMENTS {
                let (executor, temps) = test_executor();
                let (repo, project_id) = repository(&executor, &temps).await;
                add_task(&executor, A, project_id, None).await;
                add_task(&executor, B, project_id, None).await;
                let path = place_checkout(&executor, &temps, &repo, BRANCH, placement).await;

                let (_, acquired) = executor.acquire_task_worktree(A, repo.to_str().unwrap()).await;
                let refused = acquired.err().unwrap_or_else(|| panic!("{placement:?}: adopted by prefix"));
                assert!(refused.contains(&format!("git branch -m {BRANCH} {full}")), "{refused}");

                git_in(&repo, &["branch", "-m", BRANCH, &full]);
                let (_, acquired) = executor.acquire_task_worktree(A, repo.to_str().unwrap()).await;

                let acquired = acquired.unwrap_or_else(|e| panic!("{placement:?}: {e}"));
                assert_eq!(acquired.what_happened, "Adopted worktree", "{placement:?}");
                assert_eq!(acquired.info.branch, full);
                assert!(same_path(&acquired.info.path, &path), "{placement:?}");
                assert_eq!(acquired.base_commit, None);
            }
        }

        /// A stacked task whose earlier start left a branch of the old
        /// 8-digit name with the dependency's work, recorded by no task, is
        /// refused rather than resumed by prefix or started beside it; once
        /// the branch is renamed to the task's name it is resumed.
        #[tokio::test]
        async fn a_stacked_task_resumes_an_old_name_branch_only_once_renamed() {
            let (executor, temps) = test_executor();
            let (repo, project_id) = repository(&executor, &temps).await;
            git_in(&repo, &["branch", "task-deadbeef"]);
            let mut dependency = create_test_task_full("dependency", project_id, TaskStatus::InProgress, 0);
            dependency.branch_name = Some("task-deadbeef".to_string());
            let dependency_id = dependency.id;
            executor.tasks.write().await.insert(dependency_id, dependency);
            add_task(&executor, A, project_id, None).await;
            executor.tasks.write().await.get_mut(&A).unwrap().dependencies = vec![dependency_id];
            git_in(&repo, &["branch", BRANCH, "task-deadbeef"]);
            let refs = refs_of(&repo);

            let full = WorktreeManager::branch_for_task(A);
            let (_, acquired) = executor.acquire_task_worktree(A, repo.to_str().unwrap()).await;
            let refused = acquired.err().expect("resumed or started beside an unclaimed old-name branch");
            assert!(refused.contains(&format!("git branch -m {BRANCH} {full}")), "{refused}");
            assert_eq!(refs_of(&repo), refs);

            git_in(&repo, &["branch", "-m", BRANCH, &full]);
            let (_, acquired) = executor.acquire_task_worktree(A, repo.to_str().unwrap()).await;

            let acquired = acquired.expect("resumed once renamed");
            assert_eq!(acquired.what_happened, "Resumed stacked worktree");
            assert_eq!(acquired.info.branch, full);
        }

        /// A leftover of the old 8-digit name with no checkout of it, on the
        /// ordinary path, is taken up by exactly what the refusal advises:
        /// renamed to the task's name, and given a worktree.
        #[tokio::test]
        async fn a_branch_only_old_name_leftover_is_taken_up_by_the_advised_remedy() {
            let (executor, temps) = test_executor();
            let (repo, project_id) = repository(&executor, &temps).await;
            add_task(&executor, A, project_id, None).await;
            git_in(&repo, &["branch", BRANCH]);
            let full = WorktreeManager::branch_for_task(A);

            let (_, acquired) = executor.acquire_task_worktree(A, repo.to_str().unwrap()).await;
            let refused = acquired.err().expect("refused");
            assert!(refused.contains(&format!("git branch -m {BRANCH} {full}")), "{refused}");
            assert!(refused.contains(&format!("git worktree add <directory> {full}")), "{refused}");

            let dir = temps[0].path().join("taken-up");
            git_in(&repo, &["branch", "-m", BRANCH, &full]);
            git_in(&repo, &["worktree", "add", "-q", dir.to_str().unwrap(), &full]);
            let (_, acquired) = executor.acquire_task_worktree(A, repo.to_str().unwrap()).await;

            let acquired = acquired.expect("adopted after the advised remedy");
            assert_eq!(acquired.what_happened, "Adopted worktree");
            assert_eq!(acquired.info.branch, full);
            assert!(same_path(&acquired.info.path, dir.to_str().unwrap()));
        }

        /// Tasks whose ids share a prefix each get a new checkout of their
        /// own from the default base, and record it.
        #[tokio::test]
        async fn tasks_sharing_a_prefix_each_get_a_new_checkout() {
            let (executor, temps) = test_executor();
            let (repo, project_id) = repository(&executor, &temps).await;
            add_task(&executor, A, project_id, None).await;
            add_task(&executor, B, project_id, None).await;

            let mut paths = Vec::new();
            for task in [A, B] {
                let (_, acquired) = executor.acquire_task_worktree(task, repo.to_str().unwrap()).await;
                let acquired = acquired.unwrap_or_else(|e| panic!("{task}: {e}"));
                assert_eq!(acquired.what_happened, "Created worktree");
                assert_eq!(acquired.info.branch, WorktreeManager::branch_for_task(task));
                paths.push(executor.record_run_start(task, repo.to_str().unwrap(), acquired).await.expect("recorded").working_dir);
            }
            assert!(!same_path(&paths[0], &paths[1]));
            for task in [A, B] {
                let persisted = persisted_task(&executor, project_id, task);
                assert_eq!(persisted.branch_name, Some(WorktreeManager::branch_for_task(task)));
            }
        }
    }

    /// A start whose worktree cannot be recorded: refused before any agent
    /// runs, with the board and the disk still agreeing, and only what the
    /// start itself created taken back.
    ///
    /// Unix only: an unwritable directory is made with permission bits, and
    /// the stand-in agent is a shell script.
    #[cfg(unix)]
    mod unrecorded_acquisition {
        use super::*;

        /// How a start acquires the task's worktree.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Class {
            /// A new branch from the default base, and its worktree.
            Created,
            /// A new branch stacked on the dependency's, and its worktree.
            StackedCreated,
            /// A worktree added for the branch the task records.
            AddedForRecorded,
            /// A worktree added for a stacked branch an earlier start left,
            /// which the task never recorded.
            StackedResumed,
            /// A worktree git already has registered for the recorded branch.
            ReattachedRegistered,
            /// A worktree registered for the branch the task would be given,
            /// which it never recorded.
            AdoptedUnrecorded,
            /// A registered worktree of a stacked branch an earlier start
            /// left, which the task never recorded.
            StackedResumedRegistered,
        }

        impl Class {
            fn stacked(self) -> bool {
                matches!(self, Class::StackedCreated | Class::StackedResumed | Class::StackedResumedRegistered)
            }

            /// Whether the start adds a worktree for a branch that already
            /// existed, which is left behind when it cannot be recorded.
            fn adds_worktree_only(self) -> bool {
                matches!(self, Class::AddedForRecorded | Class::StackedResumed)
            }
        }

        /// The branch the dependency records.
        const DEPENDENCY: &str = "dependency-branch";

        struct World {
            executor: Arc<TaskExecutor>,
            temps: Vec<tempfile::TempDir>,
            repo: std::path::PathBuf,
            task_id: Uuid,
            project_id: Uuid,
            main_tip: String,
            dependency_tip: String,
        }

        fn provenance(t: &Task) -> (Option<String>, Option<String>, Option<String>, Option<BranchOrigin>) {
            (t.worktree_path.clone(), t.branch_name.clone(), t.base_commit.clone(), t.branch_origin.clone())
        }

        /// A repository with a published default base and a dependency branch
        /// one commit past it, a pending task set up for `class`, and the
        /// board seeded on disk.
        async fn world_with(
            (executor, temps): (Arc<TaskExecutor>, Vec<tempfile::TempDir>),
            class: Class,
        ) -> World {
            let (repo, task_id, _) =
                stacked_task_fixture(&executor, &temps, TaskStatus::InProgress, None, DEPENDENCY).await;
            publish_default_base(&repo);
            let main_tip = git_in(&repo, &["rev-parse", "main"]);
            git_in(&repo, &["checkout", "-q", "-b", DEPENDENCY]);
            git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "dependency work"]);
            git_in(&repo, &["checkout", "-q", "main"]);
            let dependency_tip = git_in(&repo, &["rev-parse", DEPENDENCY]);

            let branch = WorktreeManager::branch_for_task(task_id);
            let elsewhere = temps[0].path().join("elsewhere").join(&branch);
            let elsewhere = elsewhere.to_str().unwrap();
            {
                let mut tasks_w = executor.tasks.write().await;
                let task = tasks_w.get_mut(&task_id).unwrap();
                if !class.stacked() {
                    task.dependencies.clear();
                }
                match class {
                    Class::Created | Class::StackedCreated => {}
                    Class::AddedForRecorded | Class::ReattachedRegistered => {
                        git_in(&repo, &["branch", &branch, "main"]);
                        task.branch_name = Some(branch.clone());
                        task.base_commit = Some(main_tip.clone());
                        task.branch_origin = Some(on_main());
                        if class == Class::ReattachedRegistered {
                            git_in(&repo, &["worktree", "add", "-q", "--", elsewhere, &branch]);
                        }
                    }
                    Class::StackedResumed | Class::StackedResumedRegistered => {
                        git_in(&repo, &["branch", &branch, DEPENDENCY]);
                        if class == Class::StackedResumedRegistered {
                            git_in(&repo, &["worktree", "add", "-q", "--", elsewhere, &branch]);
                        }
                    }
                    Class::AdoptedUnrecorded => {
                        git_in(&repo, &["worktree", "add", "-q", "-b", &branch, "--", elsewhere, "main"]);
                    }
                }
            }
            let project_id = executor.tasks.read().await[&task_id].project_id;
            let board: Vec<Task> = executor
                .tasks
                .read()
                .await
                .values()
                .filter(|t| t.project_id == project_id)
                .cloned()
                .collect();
            executor.storage.save_project_tasks(project_id, &board).expect("seed the board");

            World { executor, temps, repo, task_id, project_id, main_tip, dependency_tip }
        }

        async fn world(class: Class) -> World {
            world_with(test_executor(), class).await
        }

        impl World {
            fn tasks_dir(&self) -> std::path::PathBuf {
                self.temps[0].path().join("config").join("tasks")
            }

            async fn in_memory(&self) -> Task {
                self.executor.tasks.read().await[&self.task_id].clone()
            }

            fn on_disk(&self) -> Task {
                on_disk_task(&self.executor, self.project_id, self.task_id)
            }

            /// What a restart does: read the board back from disk, with no
            /// memory of earlier attempts.
            async fn restart(&self) {
                let on_disk = self.on_disk();
                self.executor.tasks.write().await.insert(self.task_id, on_disk);
                self.executor.unrecorded_backoff_lock().clear();
            }

            /// End the stand-in agent a successful start registered.
            async fn end_run(&self) {
                let run = self.executor.running_handles.write().await.remove(&self.task_id);
                if let Some(run) = run {
                    let _ = run.cancel.send(true);
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), run.handle).await;
                }
            }
        }

        /// A stand-in `claude` that reads its prompt and then waits to be
        /// stopped, so any start is visible in its invocation log.
        async fn fake_agent() -> crate::test_helpers::FakeProgram {
            crate::test_helpers::FakeProgram::install("claude", "cat >/dev/null; sleep 20").await
        }

        /// Whether the stand-in agent is invoked within a few seconds. A
        /// registered run spawns its process from its own task, so the
        /// invocation can trail the start that registered it.
        async fn agent_invoked(agent: &crate::test_helpers::FakeProgram) -> bool {
            for _ in 0..100 {
                if agent.invocations().contains("claude") {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            false
        }

        /// The start is refused and no agent runs; the board and the disk
        /// both still show what they showed before; only a branch and
        /// worktree the start created are taken back. After a restart, once
        /// the storage accepts writes again, the next start runs and records
        /// where the branch started -- and invents nothing for a checkout
        /// whose start was never known.
        async fn refused_then_recorded_after_retry(class: Class) {
            let w = world(class).await;
            let before = provenance(&w.in_memory().await);
            let (refs_before, worktrees_before) = (refs_of(&w.repo), worktree_count(&w.repo));
            let agent = fake_agent().await;
            let Some(unwritable) = crate::test_helpers::UnwritableDir::new(&w.tasks_dir()) else {
                eprintln!("skipped: the tasks directory stays writable (running as root?)");
                return;
            };

            let refused = w
                .executor
                .spawn_task_execution(w.task_id, None)
                .await
                .expect_err("a start whose worktree cannot be recorded is refused");

            assert!(refused.contains("could not be recorded"), "{class:?}: {refused}");
            assert!(w.executor.running_handles.read().await.is_empty(), "{class:?}");
            assert_eq!(agent.invocations(), "", "{class:?}: no agent may start");
            assert_eq!(provenance(&w.in_memory().await), before, "{class:?}: the board");
            assert_eq!(provenance(&w.on_disk()), before, "{class:?}: the disk");
            assert_eq!(refs_of(&w.repo), refs_before, "{class:?}: no branch is left created or moved");
            let added = usize::from(class.adds_worktree_only());
            assert_eq!(worktree_count(&w.repo), worktrees_before + added, "{class:?}: worktrees");
            drop(unwritable);

            w.restart().await;
            let started = w.executor.spawn_task_execution(w.task_id, None).await;
            let invoked = agent_invoked(&agent).await;
            w.end_run().await;
            assert_eq!(started, Ok(true), "{class:?}");
            assert!(invoked, "{class:?}: the retry runs the agent");

            let recorded = w.on_disk();
            let expected = match class {
                Class::Created | Class::AddedForRecorded | Class::ReattachedRegistered => {
                    (Some(w.main_tip.clone()), Some(on_main()))
                }
                Class::StackedCreated | Class::StackedResumed | Class::StackedResumedRegistered => (
                    Some(w.dependency_tip.clone()),
                    Some(BranchOrigin::Stacked { parent_branch: DEPENDENCY.to_string() }),
                ),
                Class::AdoptedUnrecorded => (None, None),
            };
            assert_eq!((recorded.base_commit, recorded.branch_origin), expected, "{class:?}");
            assert_eq!(recorded.branch_name, Some(WorktreeManager::branch_for_task(w.task_id)));
        }

        /// An agent run that succeeded but whose work could not be
        /// committed is a failed run: the task records why, in memory and
        /// on disk, and does not move on to AI review as if it were done.
        #[tokio::test]
        async fn a_run_whose_work_cannot_be_committed_fails_the_task() {
            let w = world(Class::Created).await;
            git_in(&w.repo, &["config", "user.email", "t@example.com"]);
            git_in(&w.repo, &["config", "user.name", "T"]);
            let hook = w.repo.join(".git/hooks/pre-commit");
            std::fs::write(&hook, "#!/bin/sh\necho 'commits are refused here' >&2\nexit 1\n").unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let _agent = crate::test_helpers::FakeProgram::install(
                "claude",
                "cat >/dev/null; printf 'work\\n' > agent_work.txt; \
                 printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}'",
            )
            .await;

            assert_eq!(w.executor.spawn_task_execution(w.task_id, None).await, Ok(true));
            let mut settled = None;
            for _ in 0..200 {
                let task = w.in_memory().await;
                if matches!(task.status, TaskStatus::Error | TaskStatus::AiReview) {
                    settled = Some(task);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let task = settled.expect("the run settles");

            assert_eq!(task.status, TaskStatus::Error, "{:?}", task.error_message);
            let error = task.error_message.clone().unwrap_or_default();
            assert!(
                error.contains("could not be committed") && error.contains("commits are refused here"),
                "{error}"
            );
            let on_disk = w.on_disk();
            assert_eq!(on_disk.status, TaskStatus::Error);
            assert_eq!(on_disk.error_message, task.error_message);
        }

        /// Wait for the run to leave In Progress, and return the task.
        async fn settled(w: &World) -> Task {
            for _ in 0..200 {
                let task = w.in_memory().await;
                if matches!(task.status, TaskStatus::Error | TaskStatus::AiReview) {
                    return task;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            panic!("the run did not settle");
        }

        fn kinds(task: &Task) -> Vec<ActivityKind> {
            task.activity.iter().map(|e| e.kind.clone()).collect()
        }

        /// One stream-json line per argument, for a stand-in agent to print.
        fn stream(lines: &[serde_json::Value]) -> String {
            lines
                .iter()
                .map(|l| format!("printf '%s\\n' '{}'; ", l.to_string().replace('\'', "")))
                .collect()
        }

        /// A run's timeline holds its start, its tool calls -- named, with a
        /// sanitized detail -- and its end, in the task file. What the agent
        /// wrote, its session and its bookkeeping tools never become rows.
        #[tokio::test]
        async fn a_run_records_its_milestones_and_tool_calls_but_not_its_output() {
            let w = world(Class::Created).await;
            git_in(&w.repo, &["config", "user.email", "t@example.com"]);
            git_in(&w.repo, &["config", "user.name", "T"]);
            let tool = |name: &str, input: serde_json::Value| {
                serde_json::json!({"type": "assistant", "message": {"content": [
                    {"type": "tool_use", "id": "t", "name": name, "input": input}
                ]}})
            };
            let script = stream(&[
                serde_json::json!({"type": "system", "subtype": "init", "session_id": "SESSION-abc123", "model": "m"}),
                serde_json::json!({"type": "assistant", "message": {"content": [
                    {"type": "text", "text": "THE AGENT SAYS THIS"}
                ]}}),
                tool("Bash", serde_json::json!({"command": "GITHUB_TOKEN=ghp_0123456789abcdef cargo test -p slashit-ui"})),
                tool("Bash", serde_json::json!({"command": "GITHUB_TOKEN=ghp_0123456789abcdef cargo test -p slashit-ui"})),
                tool("TodoWrite", serde_json::json!({"todos": []})),
                tool("Read", serde_json::json!({"file_path": "src/lib.rs"})),
            ]);
            let _agent = crate::test_helpers::FakeProgram::install(
                "claude",
                &format!(
                    "cat >/dev/null; {script}printf 'work\\n' > agent_work.txt; \
                     printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}}'"
                ),
            )
            .await;

            assert_eq!(w.executor.spawn_task_execution(w.task_id, None).await, Ok(true));
            let task = settled(&w).await;
            assert_eq!(task.status, TaskStatus::AiReview, "{:?}", task.error_message);

            let on_disk = w.on_disk();
            assert_eq!(on_disk.activity, task.activity, "the board shows exactly what the file has");
            assert_eq!(
                kinds(&on_disk),
                [
                    ActivityKind::RunStarted { run: 1, addressing_feedback: false },
                    ActivityKind::ToolUsed {
                        run: 1,
                        tool: "Bash".into(),
                        detail: Some("GITHUB_TOKEN=*** cargo test -p slashit-ui".into()),
                        count: 2,
                    },
                    ActivityKind::ToolUsed { run: 1, tool: "Read".into(), detail: Some("src/lib.rs".into()), count: 1 },
                    ActivityKind::RunCompleted { run: 1 },
                ]
            );
            let written = serde_json::to_string(&on_disk.activity).unwrap();
            for leaked in ["THE AGENT SAYS THIS", "SESSION-abc123", "ghp_", "TodoWrite", "Session started"] {
                assert!(!written.contains(leaked), "{leaked} reached the timeline: {written}");
            }
        }

        /// A failed run stays in the history after a retry, and the retry is
        /// a new, numbered run.
        #[tokio::test]
        async fn a_failed_run_then_a_retry_reads_as_two_runs() {
            let w = world(Class::Created).await;
            git_in(&w.repo, &["config", "user.email", "t@example.com"]);
            git_in(&w.repo, &["config", "user.name", "T"]);
            {
                let _agent = crate::test_helpers::FakeProgram::install(
                    "claude",
                    "cat >/dev/null; echo 'the model is unavailable' >&2; exit 1",
                )
                .await;
                assert_eq!(w.executor.spawn_task_execution(w.task_id, None).await, Ok(true));
                assert_eq!(settled(&w).await.status, TaskStatus::Error);
            }
            let failed = kinds(&w.on_disk());
            assert!(
                matches!(&failed[..], [
                    ActivityKind::RunStarted { run: 1, .. },
                    ActivityKind::RunFailed { run: Some(1), reason },
                ] if reason.contains("the model is unavailable")),
                "{failed:?}"
            );

            // Retry: what the drawer's Retry and the scheduler do.
            {
                let mut tasks_w = w.executor.tasks.write().await;
                let t = tasks_w.get_mut(&w.task_id).unwrap();
                let from = t.status.clone();
                t.status = TaskStatus::Queue;
                t.record_move(&from);
                t.reset_execution_state();
                QueueManager::apply_promotion(t);
            }
            let _agent = crate::test_helpers::FakeProgram::install(
                "claude",
                "cat >/dev/null; printf 'more\\n' > agent_work.txt; \
                 printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}'",
            )
            .await;
            assert_eq!(w.executor.spawn_task_execution(w.task_id, None).await, Ok(true));
            assert_eq!(settled(&w).await.status, TaskStatus::AiReview);

            let retried = kinds(&w.on_disk());
            assert_eq!(retried[..2], failed[..]);
            assert_eq!(
                retried[2..],
                [
                    ActivityKind::Moved {
                        from: crate::domain::task::ActivityColumn::Error,
                        to: crate::domain::task::ActivityColumn::Queue,
                    },
                    ActivityKind::RunStarted { run: 2, addressing_feedback: false },
                    ActivityKind::RunCompleted { run: 2 },
                ]
            );
        }

        /// Every run's end on the timeline has its start there too.
        fn every_end_has_its_start(kinds: &[ActivityKind]) {
            for kind in kinds {
                if let ActivityKind::RunCompleted { run } | ActivityKind::RunFailed { run: Some(run), .. } = kind {
                    assert!(
                        kinds.iter().any(|k| matches!(k, ActivityKind::RunStarted { run: r, .. } if r == run)),
                        "run {run} ended without starting: {kinds:?}"
                    );
                }
            }
        }

        /// A start that cannot be recorded launches no agent and numbers no
        /// run, so the next start is run 1 and its end has its start.
        #[tokio::test]
        async fn a_start_that_cannot_be_recorded_numbers_no_run() {
            let w = world(Class::Created).await;
            git_in(&w.repo, &["config", "user.email", "t@example.com"]);
            git_in(&w.repo, &["config", "user.name", "T"]);
            let agent = fake_agent().await;
            let Some(unwritable) = crate::test_helpers::UnwritableDir::new(&w.tasks_dir()) else {
                eprintln!("skipped: the tasks directory stays writable (running as root?)");
                return;
            };

            let refused = w.executor.spawn_task_execution(w.task_id, None).await;

            assert!(refused.is_err(), "{refused:?}");
            assert!(!agent_invoked(&agent).await, "no agent may start without a recorded run");
            let (memory, disk) = (w.in_memory().await, w.on_disk());
            assert_eq!(memory.activity, disk.activity);
            assert_eq!((&memory.status, &memory.phase), (&disk.status, &disk.phase));
            assert!(!memory.activity.iter().any(|e| matches!(e.kind, ActivityKind::RunStarted { .. })));
            assert!(w.executor.run_tools.lock().unwrap().is_empty());
            drop(unwritable);
            drop(agent);

            w.restart().await;
            let _agent = crate::test_helpers::FakeProgram::install(
                "claude",
                "cat >/dev/null; printf 'work\\n' > agent_work.txt; \
                 printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}'",
            )
            .await;
            assert_eq!(w.executor.spawn_task_execution(w.task_id, None).await, Ok(true));
            assert_eq!(settled(&w).await.status, TaskStatus::AiReview);
            let recorded = kinds(&w.on_disk());
            assert_eq!(
                recorded,
                [
                    ActivityKind::RunStarted { run: 1, addressing_feedback: false },
                    ActivityKind::RunCompleted { run: 1 },
                ]
            );
            every_end_has_its_start(&recorded);
        }

        /// A run that finished, but whose move to AI review could not be
        /// saved, is not published as finished: the board keeps exactly what
        /// the file has, and no success is announced.
        #[tokio::test]
        async fn a_finish_that_cannot_be_recorded_is_not_published() {
            let recording = Arc::new(crate::events::RecordingEventSink::new());
            let w = world_with(test_executor_with_events(recording.clone()), Class::Created).await;
            git_in(&w.repo, &["config", "user.email", "t@example.com"]);
            git_in(&w.repo, &["config", "user.name", "T"]);
            let gate = tempfile::tempdir().unwrap();
            let (started, go) = (gate.path().join("started"), gate.path().join("go"));
            let tool = serde_json::json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": "t", "name": "Bash", "input": {"command": "cargo test"}}
            ]}});
            let _agent = crate::test_helpers::FakeProgram::install(
                "claude",
                &format!(
                    "cat >/dev/null; {}printf 'work\\n' > agent_work.txt; : > '{}'; \
                     while [ ! -e '{}' ]; do sleep 0.05; done; \
                     printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}}'",
                    stream(&[tool]),
                    started.display(),
                    go.display(),
                ),
            )
            .await;

            assert_eq!(w.executor.spawn_task_execution(w.task_id, None).await, Ok(true));
            for _ in 0..200 {
                if started.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert!(started.exists(), "the agent ran");
            let unwritable = crate::test_helpers::UnwritableDir::new(&w.tasks_dir());
            std::fs::write(&go, b"").unwrap();
            let Some(unwritable) = unwritable else {
                w.end_run().await;
                eprintln!("skipped: the tasks directory stays writable (running as root?)");
                return;
            };

            let ended = |recording: &crate::events::RecordingEventSink| {
                recording.recorded().into_iter().find(|(name, payload)| {
                    name == "agent-event"
                        && matches!(payload.get("type").and_then(|v| v.as_str()), Some("completed" | "error"))
                })
            };
            let mut ending = None;
            for _ in 0..200 {
                ending = ended(&recording);
                if ending.is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let (_, ending) = ending.expect("the run announced how it ended");

            assert_eq!(ending.get("type").and_then(|v| v.as_str()), Some("error"), "{ending}");
            assert!(ending.to_string().contains("could not be saved"), "{ending}");
            let (memory, disk) = (w.in_memory().await, w.on_disk());
            assert_eq!(memory.status, TaskStatus::InProgress, "{:?}", memory.error_message);
            assert_eq!(
                (&memory.status, &memory.phase, memory.phase_progress, memory.overall_progress, &memory.error_message),
                (&disk.status, &disk.phase, disk.phase_progress, disk.overall_progress, &disk.error_message),
                "the board shows exactly what the file has"
            );
            assert_eq!(memory.activity, disk.activity);
            assert_eq!(kinds(&disk), [ActivityKind::RunStarted { run: 1, addressing_feedback: false }]);
            drop(unwritable);
        }

        #[tokio::test]
        async fn a_created_checkout_is_taken_back_and_created_again_with_its_base() {
            refused_then_recorded_after_retry(Class::Created).await;
        }

        #[tokio::test]
        async fn a_created_stacked_checkout_is_taken_back_and_stacked_again() {
            refused_then_recorded_after_retry(Class::StackedCreated).await;
        }

        #[tokio::test]
        async fn a_worktree_added_for_a_recorded_branch_is_left_and_the_branch_untouched() {
            refused_then_recorded_after_retry(Class::AddedForRecorded).await;
        }

        #[tokio::test]
        async fn a_worktree_added_for_a_resumed_stacked_branch_is_left_and_the_branch_untouched() {
            refused_then_recorded_after_retry(Class::StackedResumed).await;
        }

        #[tokio::test]
        async fn a_registered_worktree_of_the_recorded_branch_is_left_as_it_is() {
            refused_then_recorded_after_retry(Class::ReattachedRegistered).await;
        }

        #[tokio::test]
        async fn an_adopted_unrecorded_checkout_is_left_and_claims_no_start() {
            refused_then_recorded_after_retry(Class::AdoptedUnrecorded).await;
        }

        #[tokio::test]
        async fn a_registered_worktree_of_a_resumed_stacked_branch_is_left_as_it_is() {
            refused_then_recorded_after_retry(Class::StackedResumedRegistered).await;
        }

        /// How many times a start acquired a new worktree, from its log.
        fn created_worktrees(recording: &crate::events::RecordingEventSink) -> usize {
            recording
                .recorded()
                .iter()
                .filter(|(name, payload)| {
                    name == "agent-event"
                        && payload
                            .get("message")
                            .and_then(|v| v.as_str())
                            .is_some_and(|m| m.starts_with("Created worktree:"))
                })
                .count()
        }

        /// Warnings saying a start was refused inside a backoff.
        fn backoff_reports(recording: &crate::events::RecordingEventSink) -> usize {
            recording
                .recorded()
                .iter()
                .filter(|(name, payload)| {
                    name == "agent-event"
                        && payload.get("level").and_then(|v| v.as_str()) == Some("warn")
                        && payload
                            .get("message")
                            .and_then(|v| v.as_str())
                            .is_some_and(|m| m.contains("tried again"))
                })
                .count()
        }

        /// A backoff that began a full window ago.
        fn expired_backoff() -> UnrecordedBackoff {
            UnrecordedBackoff {
                since: std::time::Instant::now()
                    .checked_sub(UNRECORDED_ACQUISITION_BACKOFF)
                    .expect("a clock this far past its start"),
                announced: false,
            }
        }

        /// While the storage keeps refusing writes, the poller does not
        /// create and take back a checkout on every pass: a start inside the
        /// backoff is refused before anything is acquired, saying when it is
        /// tried again, and one after it acquires again.
        #[tokio::test]
        async fn a_start_that_could_not_record_its_worktree_is_not_retried_within_the_backoff() {
            let recording = Arc::new(crate::events::RecordingEventSink::new());
            let w = world_with(test_executor_with_events(recording.clone()), Class::Created).await;
            let refs_before = refs_of(&w.repo);
            let agent = fake_agent().await;
            let Some(_unwritable) = crate::test_helpers::UnwritableDir::new(&w.tasks_dir()) else {
                eprintln!("skipped: the tasks directory stays writable (running as root?)");
                return;
            };

            w.executor.spawn_task_execution(w.task_id, None).await.expect_err("first start");
            assert_eq!(created_worktrees(&recording), 1);
            assert!(w.in_memory().await.is_ready_to_execute(), "the error could not be recorded either");

            let within = w.executor.spawn_task_execution(w.task_id, None).await;
            assert!(
                within.as_ref().is_err_and(|e| e.contains("tried again")),
                "a start inside the backoff says when it is retried: {within:?}"
            );
            assert_eq!(created_worktrees(&recording), 1, "nothing was acquired inside the backoff");

            let again = w.executor.spawn_task_execution(w.task_id, None).await;
            assert!(again.is_err_and(|e| e.contains("tried again")));
            assert_eq!(
                backoff_reports(&recording),
                1,
                "the refusal is reported once, not on every poll"
            );

            let expired = expired_backoff();
            let deleted_task = Uuid::new_v4();
            w.executor.unrecorded_backoff_lock().insert(w.task_id, expired);
            w.executor.unrecorded_backoff_lock().insert(deleted_task, expired);
            w.executor.spawn_task_execution(w.task_id, None).await.expect_err("still unrecordable");
            assert_eq!(created_worktrees(&recording), 2, "the backoff bounds retries; it does not end them");
            assert!(
                !w.executor.unrecorded_backoff_lock().contains_key(&deleted_task),
                "an expired entry of a task nobody starts again is dropped"
            );

            assert_eq!(refs_of(&w.repo), refs_before);
            assert_eq!(agent.invocations(), "");
        }

        /// A manual start refused for the same reason says so, rather than
        /// that it is waiting to run.
        #[tokio::test]
        async fn a_manual_start_that_could_not_record_its_worktree_says_why() {
            let w = world(Class::Created).await;
            let _agent = fake_agent().await;
            let Some(_unwritable) = crate::test_helpers::UnwritableDir::new(&w.tasks_dir()) else {
                eprintln!("skipped: the tasks directory stays writable (running as root?)");
                return;
            };

            let refused = w.executor.execute_task(w.task_id).await.expect_err("refused");

            assert!(refused.contains("worktree could not be recorded"), "{refused}");
            assert!(!refused.contains("waiting to run"), "{refused}");
        }
    }

    /// Disk pressure pauses new executions at the executor's own start
    /// boundaries, and nothing else.
    mod disk_pressure {
        use super::*;
        use crate::domain::storage_usage::GIB;
        use crate::events::RecordingEventSink;
        use crate::test_helpers::FakeDisk;

        /// Levels on [`FakeDisk`]'s 500 GiB filesystem.
        const CRITICAL: u64 = 10 * GIB;
        const CRITICAL_BOUNDARY: u64 = 40 * GIB;
        const WARNING: u64 = 100 * GIB;
        const NORMAL: u64 = 300 * GIB;

        struct World {
            executor: Arc<TaskExecutor>,
            _temps: Vec<tempfile::TempDir>,
            disk: FakeDisk,
            recording: Arc<RecordingEventSink>,
            repo: std::path::PathBuf,
            task_id: Uuid,
            project_id: Uuid,
        }

        /// One task in `status`, idle, in a project whose repository is a
        /// real git repository when `git` is set. When it is not, a start
        /// that gets past the guard fails at the checkout, which is how these
        /// tests see that a start was allowed without launching an agent.
        async fn world(available: u64, status: TaskStatus, git: bool) -> World {
            let disk = FakeDisk::with_available(available);
            let guard = disk.guard();
            world_guarded_by(disk, guard, status, git).await
        }

        /// [`world`] whose every disk reading is held until the test lets it
        /// answer, so a test can change the disk between two of them.
        async fn gated_world(available: u64, status: TaskStatus, git: bool) -> (World, Gate) {
            let disk = FakeDisk::with_available(available);
            let (reached_tx, reached) = tokio::sync::mpsc::unbounded_channel();
            let (release, released) = std::sync::mpsc::channel::<()>();
            let released = std::sync::Mutex::new(released);
            let open = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let reads = std::sync::atomic::AtomicUsize::new(0);
            let probe_disk = disk.clone();
            let probe_open = open.clone();
            let guard = Arc::new(StartGuard::new(Arc::new(move || {
                use std::sync::atomic::Ordering;
                let n = reads.fetch_add(1, Ordering::SeqCst) + 1;
                if !probe_open.load(Ordering::SeqCst) {
                    let _ = reached_tx.send(n);
                    // An error means the test is over; answer and let go.
                    let _ = released.lock().unwrap().recv();
                }
                Ok(crate::domain::storage_usage::FilesystemSpace {
                    total_bytes: FakeDisk::TOTAL,
                    available_bytes: probe_disk.available(),
                })
            })));
            let w = world_guarded_by(disk, guard, status, git).await;
            (w, Gate { reached, release, open })
        }

        /// The test's side of [`gated_world`]'s probe.
        struct Gate {
            reached: tokio::sync::mpsc::UnboundedReceiver<usize>,
            release: std::sync::mpsc::Sender<()>,
            open: Arc<std::sync::atomic::AtomicBool>,
        }

        impl Gate {
            /// Wait until a reading is being held, and say which one it is.
            async fn held(&mut self) -> usize {
                self.reached.recv().await.expect("the probe is alive")
            }

            /// Let the held reading answer with what the disk says now.
            fn answer(&self) {
                self.release.send(()).expect("a reading is held");
            }

            /// Let the held reading answer, and every later one without
            /// holding it.
            fn open(&self) {
                self.open.store(true, std::sync::atomic::Ordering::SeqCst);
                self.answer();
            }
        }

        async fn world_guarded_by(
            disk: FakeDisk,
            guard: Arc<StartGuard>,
            status: TaskStatus,
            git: bool,
        ) -> World {
            let recording = Arc::new(RecordingEventSink::new());
            let (executor, temps) = test_executor_with(recording.clone(), guard);

            let repo = temps[0].path().join("repository");
            std::fs::create_dir_all(&repo).unwrap();
            if git {
                git_in(&repo, &["init", "-q", "-b", "main"]);
                git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
            }
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

            let mut task = create_test_task_full("fresh work", project_id, status, 0);
            task.phase = TaskPhase::Idle;
            let task_id = task.id;
            executor.tasks.write().await.insert(task_id, task.clone());
            executor.storage.save_project_tasks(project_id, &[task]).expect("seed the board");

            World { executor, _temps: temps, disk, recording, repo, task_id, project_id }
        }

        impl World {
            /// The task in memory and on disk, compared whole.
            async fn records(&self) -> (serde_json::Value, serde_json::Value) {
                let memory = self.executor.tasks.read().await.get(&self.task_id).cloned().unwrap();
                let disk = on_disk_task(&self.executor, self.project_id, self.task_id);
                (serde_json::to_value(memory).unwrap(), serde_json::to_value(disk).unwrap())
            }

            async fn task(&self) -> Task {
                self.executor.tasks.read().await.get(&self.task_id).cloned().unwrap()
            }

            /// Capacity nothing holds right now.
            async fn free_permits(&self) -> usize {
                self.executor.reconcile_admission().await;
                let mut held = Vec::new();
                while let Some(permit) = self.executor.admission.try_acquire() {
                    held.push(permit);
                }
                held.len()
            }

            fn agent_events(&self) -> Vec<serde_json::Value> {
                self.recording
                    .recorded()
                    .into_iter()
                    .filter(|(name, _)| name == "agent-event")
                    .map(|(_, payload)| payload)
                    .collect()
            }

            fn events_saying(&self, needle: &str) -> usize {
                self.agent_events()
                    .iter()
                    .filter(|e| e.get("message").and_then(|m| m.as_str()).is_some_and(|m| m.contains(needle)))
                    .count()
            }
        }

        const LIMIT: usize = 3; // `QueueConfig::default().parallel_task_limit`

        #[tokio::test]
        async fn critical_disk_refuses_a_fresh_execution_before_anything_changes() {
            let w = world(CRITICAL, TaskStatus::InProgress, true).await;
            let before = w.records().await;
            // A reservation made at promotion is handed in, as the poller does.
            w.executor.reconcile_admission().await;
            let reserved = w.executor.admission.try_acquire();
            assert!(reserved.is_some());

            let refused = w.executor.spawn_task_execution(w.task_id, reserved).await;

            let message = refused.expect_err("a start with critically low disk is refused");
            assert!(message.starts_with("New work paused: critically low disk space"), "{message}");
            assert_eq!(w.records().await, before, "the task is not touched, in memory or on disk");
            assert_eq!(w.task().await.status, TaskStatus::InProgress, "not Error");
            assert_eq!(worktree_count(&w.repo), 1, "no checkout was created");
            assert_eq!(git_in(&w.repo, &["branch", "--format=%(refname:short)"]), "main", "no task branch");
            assert!(w.executor.running_handles.read().await.is_empty(), "no agent was spawned");
            assert!(w.agent_events().is_empty(), "nothing was logged on the task: {:?}", w.agent_events());
            assert_eq!(w.free_permits().await, LIMIT, "the reservation handed in was returned");
        }

        #[tokio::test]
        async fn a_failed_disk_check_refuses_a_fresh_execution_without_an_error() {
            let w = world(NORMAL, TaskStatus::InProgress, true).await;
            w.disk.fail();
            let before = w.records().await;

            let refused = w.executor.spawn_task_execution(w.task_id, None).await;

            let message = refused.expect_err("a start that cannot check the disk is refused");
            assert!(message.contains("couldn't verify free disk space"), "{message}");
            assert!(!message.contains("critically"), "unknown is not reported as critical: {message}");
            assert_eq!(w.records().await, before);
            let task = w.task().await;
            assert_eq!(task.status, TaskStatus::InProgress);
            assert!(task.error_message.is_none());
            assert_eq!(worktree_count(&w.repo), 1);
            assert!(w.executor.running_handles.read().await.is_empty());
            assert_eq!(w.free_permits().await, LIMIT);

            // The next attempt asks again rather than remembering the failure.
            let reads = w.disk.reads();
            w.disk.set_available(NORMAL);
            let _ = w.executor.spawn_task_execution(w.task_id, None).await;
            assert!(w.disk.reads() > reads);
        }

        #[tokio::test]
        async fn normal_warning_and_the_exact_critical_threshold_let_a_start_proceed() {
            for available in [NORMAL, WARNING, CRITICAL_BOUNDARY] {
                let w = world(available, TaskStatus::InProgress, false).await;
                let outcome = w.executor.spawn_task_execution(w.task_id, None).await;
                // Past the guard, the start reaches the checkout, which this
                // fixture cannot provide.
                let message = outcome.expect_err("the fixture has no repository to check out");
                assert!(message.contains("worktree"), "{available}: {message}");
                assert!(!message.contains("New work paused"), "{available}: {message}");
                assert_eq!(w.task().await.status, TaskStatus::Error, "{available}");
            }
        }

        #[tokio::test]
        async fn a_queued_task_waits_while_disk_is_critical_and_starts_once_when_space_returns() {
            let w = world(CRITICAL, TaskStatus::Queue, false).await;
            let before = w.records().await;

            for pass in 0..3 {
                let reads = w.disk.reads();
                w.executor.check_and_execute().await;
                assert!(w.disk.reads() > reads, "pass {pass} read the disk afresh");
            }

            assert_eq!(w.records().await, before, "still Queue, nothing written");
            assert!(w.agent_events().is_empty(), "a paused pass logs nothing: {:?}", w.agent_events());
            assert!(w.executor.reserved_permits.read().await.is_empty(), "no capacity reserved");
            assert_eq!(w.free_permits().await, LIMIT);

            w.disk.set_available(NORMAL);
            w.executor.check_and_execute().await;
            w.executor.check_and_execute().await;

            assert_eq!(w.events_saying("Auto-promoted from queue"), 1, "{:?}", w.agent_events());
            // One start got past the guard, and reached the checkout this
            // fixture cannot provide.
            assert_eq!(w.events_saying("No worktree could be attached"), 1, "{:?}", w.agent_events());
            assert_ne!(w.task().await.status, TaskStatus::Queue);
        }

        #[tokio::test]
        async fn a_direct_start_is_refused_before_it_moves_the_task() {
            let w = world(CRITICAL, TaskStatus::Queue, true).await;
            let before = w.records().await;

            let refused = w.executor.execute_task(w.task_id).await;

            assert!(refused.as_ref().is_err_and(|m| m.starts_with("New work paused")), "{refused:?}");
            assert_eq!(w.records().await, before, "not moved to In Progress, even in memory");
            assert_eq!(worktree_count(&w.repo), 1);
            assert!(w.executor.running_handles.read().await.is_empty());
        }

        /// Disk space running out while a direct start is underway: the check
        /// that decides refuses it, and nothing about the task was published
        /// before that check, so memory and disk still agree on `Queue`.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn disk_turning_critical_during_a_direct_start_leaves_the_task_untouched() {
            let (w, mut gate) = gated_world(NORMAL, TaskStatus::Queue, true).await;
            let before = w.records().await;
            let start = tokio::spawn({
                let executor = w.executor.clone();
                let task_id = w.task_id;
                async move { executor.execute_task(task_id).await }
            });

            assert_eq!(gate.held().await, 1);
            gate.answer(); // enough space
            assert_eq!(gate.held().await, 2);
            let at_the_deciding_check = w.records().await;
            let lease_held = w.executor.lifecycle.try_acquire(w.task_id).await.is_none();
            w.disk.set_available(CRITICAL);
            gate.open();
            let refused = start.await.expect("the start does not panic");

            let message = refused.expect_err("the check that decides sees critically low space");
            assert!(message.starts_with("New work paused: critically low disk space"), "{message}");
            let (memory, disk) = w.records().await;
            assert_eq!(memory, disk, "memory and disk agree");
            assert_eq!((memory, disk), before.clone(), "the task is untouched");
            let task = w.task().await;
            assert_eq!(task.status, TaskStatus::Queue);
            assert!(task.error_message.is_none(), "running out of space is not an error on the task");
            assert_eq!(at_the_deciding_check, before, "nothing was published before the deciding check");
            assert!(lease_held, "the deciding check runs under the task's lifecycle lease");
            assert_eq!(worktree_count(&w.repo), 1, "no checkout was created");
            assert_eq!(git_in(&w.repo, &["branch", "--format=%(refname:short)"]), "main", "no task branch");
            assert!(w.executor.running_handles.read().await.is_empty(), "no agent was spawned");
            assert!(w.agent_events().is_empty(), "nothing was logged on the task: {:?}", w.agent_events());
            assert!(w.executor.reserved_permits.read().await.is_empty());
            assert_eq!(w.free_permits().await, LIMIT, "no capacity is held");
        }

        /// A direct start with no capacity free leaves the task waiting to
        /// run, which the poller starts later, and says so -- in memory and on
        /// disk alike, and holding no capacity.
        #[tokio::test]
        async fn a_direct_start_deferred_for_capacity_is_waiting_to_run_on_disk_too() {
            let w = world(NORMAL, TaskStatus::Queue, true).await;
            w.executor.reconcile_admission().await;
            let held: Vec<_> = std::iter::from_fn(|| w.executor.admission.try_acquire()).collect();
            assert_eq!(held.len(), LIMIT);

            let deferred = w.executor.execute_task(w.task_id).await;

            assert!(deferred.as_ref().is_err_and(|m| m.contains("waiting to run")), "{deferred:?}");
            let (memory, disk) = w.records().await;
            assert_eq!(memory, disk, "memory and disk agree");
            assert!(w.task().await.is_ready_to_execute());
            assert_eq!(worktree_count(&w.repo), 1, "no checkout was created");
            assert!(w.executor.running_handles.read().await.is_empty());
            drop(held);
            assert_eq!(w.free_permits().await, LIMIT);
        }

        /// A direct start of a task an agent is already working on is refused
        /// and leaves the running task exactly as it is.
        #[cfg(unix)]
        #[tokio::test]
        async fn a_direct_start_of_a_running_task_is_refused_and_changes_nothing() {
            let w = world(NORMAL, TaskStatus::InProgress, true).await;
            let running = {
                let mut tasks = w.executor.tasks.write().await;
                let task = tasks.get_mut(&w.task_id).unwrap();
                task.phase = TaskPhase::Coding;
                task.clone()
            };
            w.executor.storage.save_project_tasks(w.project_id, &[running]).expect("seed the board");
            let cleaned_up = w.executor.register_fake_running_execution_holding_permit_for_test(w.task_id).await;
            let before = w.records().await;

            let refused = w.executor.execute_task(w.task_id).await;

            assert!(refused.as_ref().is_err_and(|m| m.contains("already has an agent")), "{refused:?}");
            assert_eq!(w.records().await, before, "the running task is untouched");
            assert_eq!(w.task().await.phase, TaskPhase::Coding);
            assert!(!cleaned_up.load(std::sync::atomic::Ordering::SeqCst), "the run is not ended");
            assert_eq!(worktree_count(&w.repo), 1);
        }

        /// A direct start whose move to In Progress cannot be saved is
        /// refused with nothing changed, in memory or on disk, and no capacity
        /// held.
        #[cfg(unix)]
        #[tokio::test]
        async fn a_direct_start_that_cannot_record_the_move_changes_nothing() {
            let w = world(NORMAL, TaskStatus::Queue, true).await;
            let before = w.records().await;
            let tasks_dir = w._temps[0].path().join("config").join("tasks");
            let Some(unwritable) = crate::test_helpers::UnwritableDir::new(&tasks_dir) else {
                eprintln!("skipped: the tasks directory stays writable (running as root?)");
                return;
            };

            let refused = w.executor.execute_task(w.task_id).await;
            drop(unwritable);

            assert!(
                refused.as_ref().is_err_and(|m| m.contains("moving it to In Progress could not be recorded")),
                "{refused:?}"
            );
            assert_eq!(w.records().await, before, "memory and disk are both still Queue");
            assert_eq!(worktree_count(&w.repo), 1, "no checkout was created");
            assert!(w.executor.running_handles.read().await.is_empty());
            assert_eq!(w.free_permits().await, LIMIT);
        }

        /// A direct start held up before it takes the lease cannot bring back
        /// a task that was finished meanwhile: it reads the task again under
        /// the lease, sees `Done`, and changes nothing.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_held_up_direct_start_cannot_resurrect_a_task_finished_meanwhile() {
            let (w, mut gate) = gated_world(NORMAL, TaskStatus::Queue, false).await;
            let start = tokio::spawn({
                let executor = w.executor.clone();
                let task_id = w.task_id;
                async move { executor.execute_task(task_id).await }
            });
            assert_eq!(gate.held().await, 1, "the direct start is underway");

            let finished = crate::lifecycle::terminalize(
                crate::lifecycle::TerminalizeCtx {
                    tasks: &w.executor.tasks,
                    projects: &w.executor.projects,
                    repositories: &w.executor.repositories,
                    worktree_manager: &w.executor.worktree_manager,
                    storage: &w.executor.storage,
                    authority: &w.executor.lifecycle,
                    running: Some(w.executor.as_ref()),
                },
                w.task_id,
                crate::lifecycle::Origin::User,
                crate::lifecycle::TerminalizeRequest::new(TaskStatus::Done),
            )
            .await
            .expect("the finish wins the race");
            assert_eq!(finished.status, TaskStatus::Done);
            let done = w.records().await;

            gate.open();
            let outcome = start.await.expect("the start does not panic");

            assert!(outcome.as_ref().is_err_and(|m| m.contains("Done")), "{outcome:?}");
            assert_eq!(w.records().await, done, "the finished task is left exactly as finished");
            assert_eq!(w.task().await.status, TaskStatus::Done);
            assert!(w.executor.running_handles.read().await.is_empty(), "no agent was spawned");
            assert_eq!(w.events_saying("No worktree could be attached"), 0, "{:?}", w.agent_events());
            assert_eq!(w.free_permits().await, LIMIT);
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn running_work_is_untouched_when_disk_becomes_critical() {
            let w = world(NORMAL, TaskStatus::Queue, false).await;
            let running = create_test_task_full("already running", w.project_id, TaskStatus::InProgress, 1);
            let running_id = running.id;
            {
                let mut running = running;
                running.phase = TaskPhase::Coding;
                w.executor.tasks.write().await.insert(running_id, running);
            }
            let cleaned_up = w.executor.register_fake_running_execution_holding_permit_for_test(running_id).await;

            w.disk.set_available(CRITICAL);
            for _ in 0..3 {
                w.executor.check_and_execute().await;
            }

            assert!(!cleaned_up.load(std::sync::atomic::Ordering::SeqCst), "not cancelled");
            assert!(
                w.executor.running_handles.read().await.get(&running_id).is_some_and(|r| !r.handle.is_finished()),
                "still running"
            );
            let still = w.executor.tasks.read().await.get(&running_id).cloned().unwrap();
            assert_eq!((still.status, still.phase), (TaskStatus::InProgress, TaskPhase::Coding));
            assert_eq!(w.free_permits().await, LIMIT - 1, "it keeps its permit; capacity is not resized");
            assert_eq!(w.task().await.status, TaskStatus::Queue, "only the new work waits");
        }

        #[tokio::test]
        async fn ai_review_of_existing_work_is_not_paused() {
            let w = world(CRITICAL, TaskStatus::AiReview, false).await;
            {
                let mut tasks = w.executor.tasks.write().await;
                let task = tasks.get_mut(&w.task_id).unwrap();
                task.phase = TaskPhase::QaReview;
                task.worktree_path = None;
            }

            w.executor.check_and_execute().await;

            // The review ran: with no checkout to review it hands the task to
            // a person, which a paused review would not have done.
            assert_eq!(w.task().await.status, TaskStatus::HumanReview);
        }
    }

    /// What SlashIt says about the run it owns, which is only ever what the
    /// executor itself holds: a handle it registered, the execution record
    /// that handle names, and a stop it is carrying out. Never a task's
    /// persisted status.
    mod run_supervision {
        use super::*;

        fn run_states(recording: &crate::events::RecordingEventSink, task_id: Uuid) -> Vec<serde_json::Value> {
            recording
                .recorded()
                .into_iter()
                .filter(|(name, _)| name == "agent-event")
                .map(|(_, payload)| payload)
                .filter(|p| {
                    p.get("type").and_then(|t| t.as_str()) == Some("run_state")
                        && p.get("task_id").and_then(|t| t.as_str()) == Some(task_id.to_string().as_str())
                })
                .map(|p| p.get("status").cloned().unwrap_or_default())
                .collect()
        }

        /// A registered execution whose record says `status`.
        async fn register_recorded_run(executor: &TaskExecutor, task_id: Uuid, status: AgentStatus) -> Uuid {
            let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
            let handle = tokio::spawn(async move {
                let _ = cancelled.changed().await;
            });
            let execution_id = Uuid::new_v4();
            let mut execution = execution_for(task_id, chrono::Utc::now(), false);
            execution.id = execution_id;
            execution.status = status;
            executor.executions.write().await.insert(execution_id, execution);
            executor
                .running_handles
                .write()
                .await
                .insert(task_id, RunningTask { handle, cancel, execution_id: Some(execution_id) });
            execution_id
        }

        async fn wait_for_status(executor: &TaskExecutor, task_id: Uuid, wanted: AgentStatus) {
            for _ in 0..400 {
                if executor.task_run(task_id).await.status.as_ref() == Some(&wanted) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            panic!("the run never reported {wanted:?}: {:?}", executor.task_run(task_id).await);
        }

        // matrix 1, 10
        #[tokio::test]
        async fn a_queued_task_is_not_live_even_while_it_waits_for_capacity() {
            let (executor, _temps) = test_executor();
            let project_id = Uuid::new_v4();
            let queued = create_test_task_full("waiting", project_id, TaskStatus::Queue, 0);
            let queued_id = queued.id;
            executor.tasks.write().await.insert(queued_id, queued);
            // Every slot is taken by other work.
            let mut others = Vec::new();
            for _ in 0..3 {
                let other = Uuid::new_v4();
                register_recorded_run(&executor, other, AgentStatus::Running).await;
                others.push(other);
            }

            let run = executor.task_run(queued_id).await;
            assert!(!run.live);
            assert_eq!(run.status, None);
            let live = executor.live_runs().await;
            assert_eq!(live.len(), 3);
            assert!(live.iter().all(|r| r.task_id != queued_id && r.status == AgentStatus::Running));
        }

        // matrix 2, 9
        #[tokio::test]
        async fn a_persisted_in_progress_task_with_no_owned_run_is_not_live() {
            let (executor, _temps) = test_executor();
            let project_id = Uuid::new_v4();
            for status in [TaskStatus::InProgress, TaskStatus::AiReview] {
                let mut task = create_test_task_full("left over", project_id, status, 0);
                task.phase = TaskPhase::Coding;
                let id = task.id;
                executor.tasks.write().await.insert(id, task);

                let run = executor.task_run(id).await;
                assert!(!run.live, "{run:?}");
                assert_eq!(run.status, None);
            }
            assert!(executor.live_runs().await.is_empty(), "a restart owns nothing until a run starts");
        }

        // matrix 3
        #[tokio::test]
        async fn a_registered_run_that_has_not_recorded_its_execution_is_starting() {
            let (executor, _temps) = test_executor();
            let task_id = Uuid::new_v4();
            let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
            let handle = tokio::spawn(async move {
                let _ = cancelled.changed().await;
            });
            executor
                .running_handles
                .write()
                .await
                .insert(task_id, RunningTask { handle, cancel, execution_id: Some(Uuid::new_v4()) });

            let run = executor.task_run(task_id).await;
            assert!(run.live);
            assert_eq!(run.status, Some(AgentStatus::Starting));
            assert_eq!(executor.live_runs().await, vec![LiveRun { task_id, status: AgentStatus::Starting }]);
        }

        // matrix 4, 12
        #[tokio::test]
        async fn an_owned_run_reports_what_its_execution_recorded_and_never_waits_for_input() {
            let (executor, _temps) = test_executor();
            let task_id = Uuid::new_v4();
            register_recorded_run(&executor, task_id, AgentStatus::Running).await;

            // Any number of reads of a run that says nothing changes nothing:
            // silence is not a state.
            for _ in 0..5 {
                let run = executor.task_run(task_id).await;
                assert!(run.live);
                assert_eq!(run.status, Some(AgentStatus::Running));
            }
            let wire = serde_json::to_value(executor.task_run(task_id).await).unwrap();
            assert_eq!(wire["status"], "running");
            assert!(!wire.to_string().contains("waiting"), "{wire}");
        }

        // matrix 13
        #[tokio::test]
        async fn parallel_runs_are_each_reported_for_their_own_task() {
            let (executor, _temps) = test_executor();
            let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
            register_recorded_run(&executor, a, AgentStatus::Running).await;
            register_recorded_run(&executor, b, AgentStatus::Starting).await;

            assert_eq!(executor.task_run(a).await.status, Some(AgentStatus::Running));
            assert_eq!(executor.task_run(b).await.status, Some(AgentStatus::Starting));
            assert_eq!(executor.task_run(c).await.status, None);
            let mut live = executor.live_runs().await;
            live.sort_by_key(|r| r.task_id);
            let mut expected = vec![
                LiveRun { task_id: a, status: AgentStatus::Running },
                LiveRun { task_id: b, status: AgentStatus::Starting },
            ];
            expected.sort_by_key(|r| r.task_id);
            assert_eq!(live, expected);
        }

        #[tokio::test]
        async fn a_review_flow_is_live_and_working_while_it_owns_its_handle() {
            let (executor, _temps) = test_executor();
            let task_id = Uuid::new_v4();
            let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
            let handle = tokio::spawn(async move {
                let _ = cancelled.changed().await;
            });
            executor.reviewing_handles.write().await.insert(task_id, ReviewOwner { handle, cancel });

            let run = executor.task_run(task_id).await;
            assert!(run.live);
            assert_eq!(run.status, Some(AgentStatus::Running));
        }

        #[tokio::test]
        async fn a_pr_helper_is_a_live_run_but_a_pr_side_effect_is_not() {
            let recording = Arc::new(crate::events::RecordingEventSink::new());
            let (executor, _temps) = test_executor_with_events(recording.clone());
            let helper_task = Uuid::new_v4();
            let helper = executor.try_begin_pr_helper(helper_task).await.expect("helper admission");

            assert_eq!(
                executor.live_runs().await,
                vec![LiveRun { task_id: helper_task, status: AgentStatus::Running }]
            );
            let (execution_ids, task_ids) = executor.active_agent_owners().await;
            assert!(execution_ids.is_empty());
            assert!(task_ids.contains(&helper_task));

            assert_eq!(run_states(&recording, helper_task), vec![serde_json::json!("running")]);

            let ending = {
                let executor = executor.clone();
                tokio::spawn(async move { executor.end_pr_helper_owner_under_lease(helper_task).await })
            };
            while !run_states(&recording, helper_task).contains(&serde_json::json!("stopping")) {
                tokio::task::yield_now().await;
            }
            drop(helper);
            ending.await.unwrap().expect("helper stop succeeds");
            assert_eq!(
                run_states(&recording, helper_task),
                vec![serde_json::json!("running"), serde_json::json!("stopping"), serde_json::json!("stopped")]
            );

            let side_effect_task = Uuid::new_v4();
            let side_effect = executor.begin_republish_under_lease(side_effect_task).await.expect("side effect admission");
            assert!(executor.live_runs().await.is_empty());
            let (_execution_ids, task_ids) = executor.active_agent_owners().await;
            assert!(!task_ids.contains(&side_effect_task));
            drop(side_effect);
        }

        #[tokio::test]
        async fn stop_task_ends_a_pr_helper_before_settling_the_task() {
            let recording = Arc::new(crate::events::RecordingEventSink::new());
            let (executor, _temps) = test_executor_with_events(recording.clone());
            let task_id = Uuid::new_v4();
            let mut task = create_test_task_full("helper", task_id, TaskStatus::AiReview, 1);
            task.phase = TaskPhase::QaReview;
            executor.tasks.write().await.insert(task_id, task);
            let helper = executor.try_begin_pr_helper(task_id).await.expect("helper admission");

            let stopper = {
                let executor = executor.clone();
                tokio::spawn(async move { executor.stop_task(task_id).await })
            };
            while !run_states(&recording, task_id).contains(&serde_json::json!("stopping")) {
                tokio::task::yield_now().await;
            }
            assert_eq!(executor.tasks.read().await.get(&task_id).unwrap().status, TaskStatus::AiReview);
            drop(helper);
            stopper.await.unwrap().expect("stop settles after helper exits");
            assert_eq!(executor.tasks.read().await.get(&task_id).unwrap().status, TaskStatus::Backlog);
            assert_eq!(run_states(&recording, task_id), vec![
                serde_json::json!("running"),
                serde_json::json!("stopping"),
                serde_json::json!("stopped"),
            ]);
        }

        #[tokio::test]
        async fn active_agent_count_covers_pre_record_and_stopping_execution_windows_once() {
            let (executor, _temps) = test_executor();
            let task_id = Uuid::new_v4();
            let execution_id = Uuid::new_v4();
            let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
            let handle = tokio::spawn(async move {
                let _ = cancelled.changed().await;
            });
            executor.running_handles.write().await.insert(
                task_id,
                RunningTask { handle, cancel, execution_id: Some(execution_id) },
            );

            assert_eq!(
                crate::commands::agent::active_agent_count(&executor.executions, Some(&executor)).await,
                1,
                "a registered coding owner counts before its execution record exists"
            );

            let owner = executor.running_handles.write().await.remove(&task_id).unwrap();
            let mark = executor.mark_ending(task_id, owner.execution_id);
            let mut execution = execution_for(task_id, chrono::Utc::now(), false);
            execution.id = execution_id;
            executor.executions.write().await.insert(execution_id, execution);
            assert_eq!(
                crate::commands::agent::active_agent_count(&executor.executions, Some(&executor)).await,
                1,
                "a stopping coding owner is not double-counted through its legacy record"
            );

            drop(mark);
            owner.cancel.send(true).unwrap();
            owner.handle.await.unwrap();
        }

        #[tokio::test]
        async fn a_panicking_execution_is_terminalized_before_its_record_can_count_as_live() {
            let recording = Arc::new(crate::events::RecordingEventSink::new());
            let (executor, _temps) = test_executor_with_events(recording.clone());
            let task_id = Uuid::new_v4();
            let execution_id = Uuid::new_v4();
            let mut execution = execution_for(task_id, chrono::Utc::now(), false);
            execution.id = execution_id;
            executor.executions.write().await.insert(execution_id, execution);

            drop(RunEndAnnouncer::for_execution(
                executor.events.clone(),
                task_id,
                executor.executions.clone(),
                execution_id,
            ));

            let execution = executor.executions.read().await.get(&execution_id).cloned().unwrap();
            assert!(matches!(execution.status, AgentStatus::Failed(ref message) if message == "execution owner panicked"));
            assert!(execution.stopped_at.is_some());
            assert_eq!(
                run_states(&recording, task_id),
                vec![serde_json::json!({"failed": "execution owner panicked"})]
            );
            assert_eq!(crate::commands::agent::active_agent_count(&executor.executions, Some(&executor)).await, 0);
        }

        #[tokio::test]
        async fn a_handle_whose_future_died_is_not_a_live_run() {
            let (executor, _temps) = test_executor();
            let task_id = Uuid::new_v4();
            let (cancel, _cancelled) = tokio::sync::watch::channel(false);
            let handle = tokio::spawn(async {});
            while !handle.is_finished() {
                tokio::task::yield_now().await;
            }
            executor
                .running_handles
                .write()
                .await
                .insert(task_id, RunningTask { handle, cancel, execution_id: None });

            assert!(!executor.task_run(task_id).await.live);
            assert!(executor.live_runs().await.is_empty());
        }

        // matrix 8
        #[tokio::test]
        async fn a_stop_in_progress_is_stopping_until_the_run_is_gone() {
            let recording = Arc::new(crate::events::RecordingEventSink::new());
            let (executor, _temps) = test_executor_with_events(recording.clone());
            let task = running_task(Uuid::new_v4());
            let task_id = task.id;
            executor.tasks.write().await.insert(task_id, task);

            // A run that, once asked to end, ends only when the test lets it.
            let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
            let release = Arc::new(tokio::sync::Notify::new());
            let released = release.clone();
            let handle = tokio::spawn(async move {
                let _ = cancelled.changed().await;
                released.notified().await;
            });
            let execution_id = Uuid::new_v4();
            let mut execution = execution_for(task_id, chrono::Utc::now(), false);
            execution.id = execution_id;
            executor.executions.write().await.insert(execution_id, execution);
            executor
                .running_handles
                .write()
                .await
                .insert(task_id, RunningTask { handle, cancel, execution_id: Some(execution_id) });
            assert_eq!(executor.task_run(task_id).await.status, Some(AgentStatus::Running));

            let stopper = {
                let executor = executor.clone();
                tokio::spawn(async move { executor.stop_task(task_id).await })
            };
            wait_for_status(&executor, task_id, AgentStatus::Stopping).await;
            let run = executor.task_run(task_id).await;
            assert!(run.live, "the agent is still shutting down: {run:?}");
            assert_eq!(
                executor.live_runs().await,
                vec![LiveRun { task_id, status: AgentStatus::Stopping }]
            );
            assert_eq!(run_states(&recording, task_id), vec![serde_json::json!("stopping")]);

            release.notify_one();
            stopper.await.unwrap().expect("the stop succeeds");

            let run = executor.task_run(task_id).await;
            assert!(!run.live);
            assert_eq!(run.status, None, "the fake never recorded an end of its own");
            assert!(executor.live_runs().await.is_empty());
            assert_eq!(
                run_states(&recording, task_id),
                vec![serde_json::json!("stopping"), serde_json::json!("stopped")]
            );
        }

        #[tokio::test]
        async fn a_stop_that_cannot_finish_keeps_the_run_stopping_not_working() {
            let (executor, _temps) = test_executor();
            let task = running_task(Uuid::new_v4());
            let task_id = task.id;
            executor.tasks.write().await.insert(task_id, task);
            let execution_id = Uuid::new_v4();
            let mut execution = execution_for(task_id, chrono::Utc::now(), false);
            execution.id = execution_id;
            executor.executions.write().await.insert(execution_id, execution);
            // A run that never ends, even once asked to.
            executor.register_unkillable_running_execution_for_test(task_id).await;
            executor.running_handles.write().await.get_mut(&task_id).unwrap().execution_id = Some(execution_id);

            let refused = tokio::time::timeout(
                AGENT_SHUTDOWN_TIMEOUT * 2,
                executor.stop_task(task_id),
            )
            .await
            .expect("the bounded stop answers");

            assert!(refused.is_err(), "{refused:?}");
            let run = executor.task_run(task_id).await;
            assert!(run.live, "the run was put back, so it is still owned");
            assert_eq!(run.status, Some(AgentStatus::Stopping), "{run:?}");
        }

        #[cfg(unix)]
        struct Real {
            executor: Arc<TaskExecutor>,
            recording: Arc<crate::events::RecordingEventSink>,
            _temps: Vec<tempfile::TempDir>,
            task_id: Uuid,
        }

        #[cfg(unix)]
        async fn real_world() -> Real {
            let recording = Arc::new(crate::events::RecordingEventSink::new());
            let (executor, temps) = test_executor_with_events(recording.clone());
            let (repo, task_id, _) =
                stacked_task_fixture(&executor, &temps, TaskStatus::InProgress, None, "dependency-branch").await;
            publish_default_base(&repo);
            git_in(&repo, &["config", "user.email", "t@example.com"]);
            git_in(&repo, &["config", "user.name", "T"]);
            executor.tasks.write().await.get_mut(&task_id).unwrap().dependencies.clear();
            let project_id = executor.tasks.read().await[&task_id].project_id;
            let board: Vec<Task> = executor
                .tasks
                .read()
                .await
                .values()
                .filter(|t| t.project_id == project_id)
                .cloned()
                .collect();
            executor.storage.save_project_tasks(project_id, &board).expect("seed the board");
            Real { executor, recording, _temps: temps, task_id }
        }

        // matrix 3, 4, 5-by-silence, 6, 12, 14
        #[cfg(unix)]
        #[tokio::test]
        async fn a_real_run_is_starting_then_working_then_gone_with_nothing_stale() {
            let w = real_world().await;
            let gate = tempfile::tempdir().unwrap();
            let (started, go) = (gate.path().join("started"), gate.path().join("go"));
            let _agent = crate::test_helpers::FakeProgram::install(
                "claude",
                &format!(
                    "cat >/dev/null; printf 'work\\n' > agent_work.txt; : > '{}'; \
                     while [ ! -e '{}' ]; do sleep 0.05; done; \
                     printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}}'",
                    started.display(),
                    go.display(),
                ),
            )
            .await;

            assert_eq!(w.executor.spawn_task_execution(w.task_id, None).await, Ok(true));
            // Owned from the moment it is registered.
            assert!(w.executor.task_run(w.task_id).await.live);

            wait_for_status(&w.executor, w.task_id, AgentStatus::Running).await;
            for _ in 0..400 {
                if started.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            assert!(started.exists(), "the agent is running");
            // A fresh reader (a board that just opened) sees the same run.
            assert_eq!(
                w.executor.live_runs().await,
                vec![LiveRun { task_id: w.task_id, status: AgentStatus::Running }]
            );
            // Silent so far, still working, and nothing but starting and
            // running was ever announced.
            assert_eq!(
                run_states(&w.recording, w.task_id),
                vec![serde_json::json!("starting"), serde_json::json!("running")]
            );

            std::fs::write(&go, b"").unwrap();
            for _ in 0..400 {
                if !w.executor.task_run(w.task_id).await.live {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }

            let run = w.executor.task_run(w.task_id).await;
            assert!(!run.live, "{run:?}");
            assert_eq!(run.status, Some(AgentStatus::Stopped), "an ended run says how it ended");
            assert!(w.executor.live_runs().await.is_empty());
            assert_eq!(
                run_states(&w.recording, w.task_id).last(),
                Some(&serde_json::json!("stopped")),
                "the end is announced last"
            );
        }

        // matrix 7
        #[cfg(unix)]
        #[tokio::test]
        async fn a_failed_run_is_announced_as_failed_and_the_task_still_fails_as_before() {
            let w = real_world().await;
            let _agent = crate::test_helpers::FakeProgram::install(
                "claude",
                "cat >/dev/null; echo 'the fixture agent broke' >&2; exit 3",
            )
            .await;

            assert_eq!(w.executor.spawn_task_execution(w.task_id, None).await, Ok(true));
            for _ in 0..400 {
                if !w.executor.task_run(w.task_id).await.live
                    && w.executor.tasks.read().await[&w.task_id].status == TaskStatus::Error
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }

            let task = w.executor.tasks.read().await[&w.task_id].clone();
            assert_eq!(task.status, TaskStatus::Error);
            assert_eq!(task.phase, TaskPhase::Failed);
            let run = w.executor.task_run(w.task_id).await;
            assert!(!run.live);
            assert!(matches!(run.status, Some(AgentStatus::Failed(_))), "{run:?}");
            assert!(w.executor.live_runs().await.is_empty());
            let states = run_states(&w.recording, w.task_id);
            assert!(
                states.last().is_some_and(|s| s.get("failed").is_some()),
                "the last word is the failure: {states:?}"
            );
        }

        // matrix 8 on a real process
        #[cfg(unix)]
        #[tokio::test]
        async fn stopping_a_real_run_announces_stopping_and_then_that_it_is_gone() {
            let w = real_world().await;
            let _agent = crate::test_helpers::FakeProgram::install("claude", "cat >/dev/null; sleep 30").await;

            assert_eq!(w.executor.spawn_task_execution(w.task_id, None).await, Ok(true));
            wait_for_status(&w.executor, w.task_id, AgentStatus::Running).await;

            w.executor.stop_task(w.task_id).await.expect("stop");

            assert!(!w.executor.task_run(w.task_id).await.live);
            assert!(w.executor.live_runs().await.is_empty());
            assert_eq!(
                run_states(&w.recording, w.task_id),
                vec![
                    serde_json::json!("starting"),
                    serde_json::json!("running"),
                    serde_json::json!("stopping"),
                    serde_json::json!("stopped"),
                    serde_json::json!("stopped"),
                ],
                "the run's own end, and the stop's repeat of it"
            );
        }
    }

}
