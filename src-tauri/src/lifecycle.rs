//! Per-task lifecycle authority and the one operation that terminalizes a task.
//!
//! A task owns at most one worktree and at most one agent execution. Every
//! operation that changes which of those a task has -- starting a run, stopping
//! it, creating or reattaching a worktree, removing one, finishing the task,
//! deleting it -- takes the same per-task lease first, so for one task those
//! decisions happen one at a time and each of them sees the result of the last.
//!
//! Deliberately free of every `tauri::` reference, like [`crate::ipc`] and
//! [`crate::queue::executor`], so `slashitd` links it without the GUI toolkit.
//! The desktop commands, the daemon's IPC handlers and the queue executor all
//! reach the same [`terminalize`] through raw handles rather than each holding
//! their own copy of the rules.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};
use uuid::Uuid;

use crate::config::Storage;
use crate::domain::{Project, Repository, Task, TaskStatus};
use crate::worktree::WorktreeManager;

pub type Tasks = Arc<RwLock<HashMap<Uuid, Task>>>;

/// How long a user-initiated command waits for a task's lifecycle lease before
/// giving up.
///
/// Bounded rather than unbounded because the lease is held across `git`
/// subprocesses, and a removal blocked on a repository hook would
/// otherwise hang a dragged card with no way out. Long enough that an ordinary
/// removal -- milliseconds on any checkout the product creates -- is never
/// interrupted by it, short enough that "the task is busy" reaches the user
/// while they still remember asking. A timeout is not a failed operation:
/// nothing was attempted, so the user can simply ask again.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// Serializes the lifecycle transitions of one task against each other.
///
/// Keyed by task id and modelled on [`crate::commands::state_location::StateLocationLocks`],
/// which solves the same problem one level up. A per-task lease rather than the
/// task map's own `RwLock` for two reasons: the map is global, so holding it
/// across a `git worktree remove` would stall every unrelated task and every
/// board read for the duration of a subprocess; and the map guard cannot be
/// held across the whole read-decide-act sequence anyway, because the act runs
/// a subprocess that needs the map released.
///
/// Owned guards rather than a task-id set: an `OwnedMutexGuard` is released by
/// unwinding, so a panic inside a lifecycle operation cannot strand a task with
/// its lease held forever -- which is exactly what the executor's previous
/// manually-released `HashSet` of in-flight cleanups did.
///
/// Entries are never evicted. One idle `Arc<Mutex<()>>` per task that has ever
/// had a lifecycle operation is a handful of bytes, bounded by the number of
/// tasks that exist, and not worth cleanup complexity -- the same trade the
/// per-project locks make.
#[derive(Default)]
pub struct TaskLifecycleLocks {
    per_task: Mutex<HashMap<Uuid, Arc<Mutex<()>>>>,
}

/// Proof that the holder currently owns a task's lifecycle transitions.
pub type LifecycleLease = OwnedMutexGuard<()>;

impl TaskLifecycleLocks {
    pub fn new() -> Self {
        Self::default()
    }

    async fn handle(&self, task_id: Uuid) -> Arc<Mutex<()>> {
        let mut per_task = self.per_task.lock().await;
        per_task
            .entry(task_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Wait up to [`ACQUIRE_TIMEOUT`] for the lease, for an operation a person
    /// asked for and is waiting on.
    ///
    /// `Err` means nothing was attempted and nothing was changed.
    pub async fn acquire(&self, task_id: Uuid) -> Result<LifecycleLease, String> {
        let handle = self.handle(task_id).await;
        tokio::time::timeout(ACQUIRE_TIMEOUT, handle.lock_owned())
            .await
            .map_err(|_| {
                format!(
                    "another lifecycle operation is still running for task {task_id}; \
                     nothing was changed, so this can simply be asked for again"
                )
            })
    }

    /// Take the lease only if it is free, for an operation nobody is waiting
    /// on.
    ///
    /// `None` means some other transition owns the task right now. Declining
    /// costs nothing and leaves no trace, so an automatic caller that gets it
    /// must behave as though it never looked: no state written, no opportunity
    /// consumed, free to try again on a later pass.
    pub async fn try_acquire(&self, task_id: Uuid) -> Option<LifecycleLease> {
        self.handle(task_id).await.try_lock_owned().ok()
    }
}

/// Everything [`terminalize`] needs, grouped so the call sites pass handles
/// rather than a `tauri::State` none of them share.
pub struct TerminalizeCtx<'a> {
    pub tasks: &'a Tasks,
    pub projects: &'a Arc<RwLock<HashMap<Uuid, Project>>>,
    pub repositories: &'a Arc<RwLock<HashMap<Uuid, Repository>>>,
    pub worktree_manager: &'a WorktreeManager,
    pub storage: &'a Storage,
    pub authority: &'a TaskLifecycleLocks,
    /// Whatever can answer "is an agent running for this task right now", when
    /// there is one.
    ///
    /// A trait rather than the executor itself so this module stays independent
    /// of the queue -- the executor is one of its callers. `None` where no
    /// executor exists yet: during startup, and in tests that build no queue,
    /// which is also exactly when no execution can be running.
    pub running: Option<&'a dyn ExecutionOwnership>,
}

/// Who currently owns a task's agent process.
///
/// Implemented by [`crate::queue::TaskExecutor`], whose `running_handles` is
/// the live fact. Terminalization asks rather than infers, because a task's
/// persisted status says nothing about whether a process is attached to it:
/// an `InProgress` task may or may not have a running agent, and deleting the
/// checkout out from under one that does would take the agent's working
/// directory away mid-run.
#[async_trait::async_trait]
pub trait ExecutionOwnership: Send + Sync {
    async fn is_task_running(&self, task_id: Uuid) -> bool;

    /// End whatever execution or AI review/fix ownership currently exists
    /// for `task_id`, killing and reaping the underlying process before
    /// returning -- the same bounded shutdown [`crate::queue::TaskExecutor::
    /// stop_task`] performs, made reachable to a caller that is not asking
    /// for a stop but is about to durably change the task's status.
    ///
    /// The caller must already hold `task_id`'s lifecycle lease
    /// ([`TaskLifecycleLocks::acquire`]/`try_acquire`). This does not take
    /// it itself: it is called from inside the same critical section a
    /// lifecycle-changing command already opened, and taking the lease again
    /// here would deadlock against the caller's own held guard.
    ///
    /// `Ok(())` means ownership is now clear -- whether because something
    /// was owning the task and was safely ended, or because nothing was.
    /// `Err` means owned work could not be ended within the bounded shutdown
    /// window: nothing was changed, the owner is left exactly as it was, and
    /// the caller must refuse its own transition rather than publish a
    /// status the old owner might still act on.
    ///
    /// Defaulted to a no-op success for implementors -- test doubles that
    /// only ever answer `is_task_running` -- that have no ownership of their
    /// own to end.
    async fn end_ownership_under_lease(&self, _task_id: Uuid) -> Result<(), String> {
        Ok(())
    }
}

/// End active ownership before a lifecycle-changing status transition
/// commits, through whatever [`ExecutionOwnership`] handle the caller has.
///
/// Shared by every front door that can move a task's status outside
/// [`terminalize`] -- `update_task_status`, `reorder_task` and their IPC/CLI
/// equivalents, the queue's enqueue/requeue commands -- so "end the previous
/// owner before committing a new status" is one call reused everywhere,
/// rather than a conditional re-derived per command. `None` where no
/// executor exists yet (startup, or a test that builds no queue), which is
/// also exactly when nothing could be running.
pub async fn end_active_ownership(
    running: Option<&dyn ExecutionOwnership>,
    task_id: Uuid,
) -> Result<(), String> {
    match running {
        Some(owner) => owner.end_ownership_under_lease(task_id).await,
        None => Ok(()),
    }
}

/// Who asked, which decides how hard this tries and what it refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A person is waiting for the answer. Waits [`ACQUIRE_TIMEOUT`] for the
    /// lease, and will re-attempt a cleanup an earlier crash left unfinished,
    /// because an explicit request is the product's stated way back out of that
    /// state.
    User,
    /// Nobody is waiting. Declines immediately if the lease is held, and never
    /// touches a worktree a previous run was interrupted mid-removal of.
    Automatic,
}

/// Changes a caller needs made inside one of `terminalize`'s durable writes.
///
/// Takes the project's whole task set rather than the one task because a
/// terminal move is sometimes also a move within a column: `reorder_task` has
/// to renumber the task's new neighbours, and doing that in a second
/// transaction after the status commit would publish a moment where the board
/// has the card in the right column at the wrong position.
pub type Amend<'a> = &'a (dyn Fn(&mut HashMap<Uuid, Task>) + Send + Sync);

pub struct TerminalizeRequest<'a> {
    /// The status a successful terminalization commits.
    pub desired: TaskStatus,
    /// Facts the caller has already established that must be recorded whether
    /// or not the cleanup succeeds -- a PR observed merged is true regardless
    /// of what git says about the checkout. Applied in the durable write that
    /// precedes any destructive step, so a crash during removal cannot lose it.
    pub record_first: Option<Amend<'a>>,
    /// Applied only in the final commit, and only when the cleanup succeeded.
    /// Everything that would be a lie after a refusal belongs here.
    pub on_success: Option<Amend<'a>>,
}

impl<'a> TerminalizeRequest<'a> {
    pub fn new(desired: TaskStatus) -> Self {
        Self {
            desired,
            record_first: None,
            on_success: None,
        }
    }
}

const REPUBLISH_PENDING_REASON: &str =
    "a restack of this task's published branch is unfinished, so its \
     checkout and branch were kept; resume or discard the restack first";

/// What the merged-pull-request poll puts on a task whose terminalization a
/// pending restack refused.
pub fn merged_while_republish_pending_message() -> String {
    format!("The pull request is merged, but the task was not finished: {REPUBLISH_PENDING_REASON}")
}

/// Drop [`merged_while_republish_pending_message`] from `task`, once the
/// restack it names is resolved. Any other error is left alone.
pub fn clear_merged_while_republish_pending(task: &mut Task) {
    if task.error_message.as_deref() == Some(merged_while_republish_pending_message().as_str()) {
        task.error_message = None;
    }
}

/// Why a terminalization did not happen.
///
/// Every variant leaves the task exactly as it was found, including its
/// worktree, its branch and everything uncommitted inside the checkout.
#[derive(Debug, Clone)]
pub enum TerminalizeRefusal {
    /// The task is gone, or was never there.
    TaskNotFound,
    /// The lease is held by another transition. Nothing was attempted.
    Busy(String),
    /// An agent currently owns this task. Cleanup would delete the directory
    /// the agent is working in, so it is refused; ending the run is what
    /// `stop_task` is for.
    ExecutionActive,
    /// A previous process was interrupted mid-removal and the checkout is
    /// still there. Only an explicit request may act on it.
    Quarantined(String),
    /// The task's project resolves to no repository, so there is no directory
    /// to run git in.
    RepositoryUnresolved(String),
    /// Git refused to remove the checkout, and said why. Nothing was deleted.
    CleanupRefused {
        worktree_path: String,
        reason: String,
    },
    /// The removal happened but the board could not be written.
    NotRecorded(String),
    /// An unfinished restack of the task's published branch owns its checkout
    /// and branch. Finishing the task would delete the evidence that resuming
    /// or discarding that restack depends on, and nothing here can tell
    /// whether the restack succeeded.
    RepublishPending,
    /// An integration of the task's approved work into a local branch was
    /// started and not finished. It may already have moved its target, and
    /// only the person's Resume or Discard decides how it ends.
    IntegrationUnfinished,
    /// The Task Checkout is the only remaining evidence for an uncertain
    /// Worker outcome, so terminalization may not remove it.
    WorkerRecoveryRequired,
    /// The task records an unfinished local integration, in whatever status it
    /// happens to be. Only a delete asks this: the record is durable evidence
    /// of a Git operation that may already have moved its target branch, and
    /// removing the task removes the only copy of it.
    DeleteBlockedByIntegration,
}

impl std::fmt::Display for TerminalizeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TaskNotFound => write!(f, "task not found"),
            Self::Busy(m) => write!(f, "{m}"),
            Self::ExecutionActive => write!(
                f,
                "an agent is still running for this task; stop it before finishing the task"
            ),
            Self::Quarantined(path) => write!(
                f,
                "the worktree at {path} was left behind by a cleanup that was interrupted \
                 and needs attention before this task can be finished automatically"
            ),
            Self::RepositoryUnresolved(m) => write!(f, "{m}"),
            Self::CleanupRefused {
                worktree_path,
                reason,
            } => write!(f, "the worktree at {worktree_path} was kept: {reason}"),
            Self::NotRecorded(m) => write!(f, "{m}"),
            Self::RepublishPending => f.write_str(REPUBLISH_PENDING_REASON),
            Self::IntegrationUnfinished => {
                f.write_str(crate::domain::task::INTEGRATION_UNFINISHED_REFUSAL)
            }
            Self::WorkerRecoveryRequired => f.write_str(
                "Worker recovery is unresolved; the Task Checkout was kept for inspection",
            ),
            Self::DeleteBlockedByIntegration => f.write_str(DELETE_BLOCKED_BY_INTEGRATION_REASON),
        }
    }
}

impl From<TerminalizeRefusal> for String {
    fn from(refusal: TerminalizeRefusal) -> Self {
        refusal.to_string()
    }
}

/// Move a task to its terminal status, and remove the worktree it is finished
/// with -- in that order, and only in that order.
///
/// The whole point of one function is that "the task is done" and "its checkout
/// is gone" become a single decision with a single answer. The board only ever
/// says `Done` for a task whose cleanup has already succeeded, because the
/// write that says so is the write that also clears `worktree_path`, and it
/// happens after the removal rather than alongside it.
///
/// The sequence, and why each step is where it is:
///
/// 1. Take the task's lifecycle lease, so nothing else can start a run,
///    reattach a worktree or delete the task while this decides.
/// 2. Re-read the task under the lease. Whatever the caller saw before waiting
///    for the lease may no longer be true.
/// 3. Refuse if an agent owns the task. A live run's working directory is the
///    checkout this would delete.
/// 4. Resolve the repository, because `git worktree remove` needs somewhere to
///    run.
/// 5. Durably record `cleanup_in_flight`, and anything the caller asked to be
///    recorded first, *before* a destructive subprocess can exist. This is what
///    makes a crash during removal visible to the next startup.
/// 6. Remove the worktree, safely: git decides, and a refusal is an answer, not
///    an obstacle to route around.
/// 7. Commit the outcome in one durable write -- terminal status, cleared
///    worktree and cleared flag on success; cleared flag and a readable reason
///    on refusal -- persisting before publishing to shared memory, so the board
///    can never show a state the file does not have.
///
/// The branch is never touched on any path. It names the commits the task
/// produced, and removing a checkout says nothing about whether those are
/// wanted.
pub async fn terminalize(
    ctx: TerminalizeCtx<'_>,
    task_id: Uuid,
    origin: Origin,
    request: TerminalizeRequest<'_>,
) -> Result<Task, TerminalizeRefusal> {
    let _lease = match origin {
        Origin::User => ctx
            .authority
            .acquire(task_id)
            .await
            .map_err(TerminalizeRefusal::Busy)?,
        Origin::Automatic => ctx
            .authority
            .try_acquire(task_id)
            .await
            .ok_or_else(|| TerminalizeRefusal::Busy(format!("task {task_id} is busy")))?,
    };

    terminalize_leased(ctx, task_id, origin, request).await
}

/// Record facts about a task durably, without attempting any transition.
///
/// The persist-before-publish half of [`terminalize`] on its own, for a caller
/// that has established something true -- a pull request observed merged, the
/// reason a transition cannot proceed -- and must not lose it merely because
/// the transition it would normally accompany was refused. Nothing here is
/// destructive and nothing here moves a task between columns.
///
/// The order is the same contract the rest of the backend holds: the file
/// accepts the change before the board shows it. A save that was refused
/// before the replacement leaves both the shared map and the disk exactly as
/// they were; one that replaced the file and then could not confirm it durable
/// has already put the new board on disk -- [`publish`] adopts it into memory,
/// and the error it returns says the change is in place, so the caller never
/// reports a failure that published as one that published nothing.
pub async fn record(
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    amend: Amend<'_>,
) -> Result<(), String> {
    record_typed(tasks, storage, task_id, &amend)
        .await
        .map_err(|e| e.to_string())
}

/// Typed board-write result for Task Worker handoff boundaries. Callers must
/// not infer publication from rendered error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordWriteError {
    NotPublished(String),
    PublishedNotDurable(String),
}

impl std::fmt::Display for RecordWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPublished(message) | Self::PublishedNotDurable(message) => {
                f.write_str(message)
            }
        }
    }
}

/// `record` with the board's publication state retained as typed data.
pub async fn record_typed(
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    amend: Amend<'_>,
) -> Result<(), RecordWriteError> {
    let mut tasks_w = tasks.write().await;
    let Some(project_id) = tasks_w.get(&task_id).map(|t| t.project_id) else {
        return Err(RecordWriteError::NotPublished(format!(
            "task {task_id} is no longer on the board"
        )));
    };

    let mut staged = stage_project(&tasks_w, project_id);
    amend(&mut staged);
    if let Some(task) = staged.get_mut(&task_id) {
        task.updated_at = chrono::Utc::now();
    }

    publish_typed(&mut tasks_w, storage, project_id, staged)
}

/// Changes a caller may need made in [`record_if_changed`]'s write, answering
/// whether it changed anything.
pub type Revise<'a> = &'a (dyn Fn(&mut HashMap<Uuid, Task>) -> bool + Send + Sync);

/// [`record`], for an observation that is often already on the record.
///
/// `revise` answers whether it changed anything. When it did not, nothing is
/// written, `updated_at` is left alone, and this answers `Ok(false)`. A
/// repeated reading -- a pull request still open, thirty seconds later -- is
/// not a change to the task and must not rewrite its project's file.
pub async fn record_if_changed(
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    revise: Revise<'_>,
) -> Result<bool, String> {
    record_typed_if_changed(tasks, storage, task_id, revise)
        .await
        .map_err(|e| e.to_string())
}

/// [`record_if_changed`] with the board's publication state retained as typed
/// data, for a decision that must tell "refused, nothing written" apart from
/// "published but not confirmed durable".
///
/// `revise` runs under the same write guard the save commits with, so a
/// compare-and-set predicate inside it cannot be invalidated between the check
/// and the write. When it answers `false` the board, the file and `updated_at`
/// are untouched.
pub async fn record_typed_if_changed(
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    revise: Revise<'_>,
) -> Result<bool, RecordWriteError> {
    let mut tasks_w = tasks.write().await;
    let Some(project_id) = tasks_w.get(&task_id).map(|t| t.project_id) else {
        return Err(RecordWriteError::NotPublished(format!(
            "task {task_id} is no longer on the board"
        )));
    };

    let mut staged = stage_project(&tasks_w, project_id);
    if !revise(&mut staged) {
        return Ok(false);
    }
    if let Some(task) = staged.get_mut(&task_id) {
        task.updated_at = chrono::Utc::now();
    }

    publish_typed(&mut tasks_w, storage, project_id, staged).map(|()| true)
}

/// Amend a task's own fields durably, or say there is no such task.
///
/// [`record`] cannot make the distinction an ordinary command needs: every
/// command in `commands::task` and `ipc::handlers` has always answered an id
/// that is not on the board with `Ok(None)`, not an error. Checking membership
/// before staging gives that contract in the case that matters -- the id was
/// never there -- but does not extend it across a race: a delete landing
/// between the check and the write turns what should be `Ok(None)` into the
/// `Err` [`record`] returns for a task it cannot find. The message is sane and
/// nothing is written, so the failure is safe; a caller that reads "gone" as
/// "failed" for one interleaving is a smaller problem than a mechanism
/// invented to avoid saying so.
pub async fn commit_task(
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    amend: Amend<'_>,
) -> Result<Option<Task>, String> {
    if !tasks.read().await.contains_key(&task_id) {
        return Ok(None);
    }

    let recovery_refusal = tasks
        .read()
        .await
        .get(&task_id)
        .and_then(Task::worker_recovery_refusal);
    if let Some(refusal) = recovery_refusal {
        // The caller's intent is opaque, so fail closed for every command that
        // uses this lifecycle transition boundary. Same-status recovery writes
        // use `record_typed` directly.
        return Err(refusal);
    }

    record(tasks, storage, task_id, amend).await?;

    Ok(tasks.read().await.get(&task_id).cloned())
}

/// Say what a creation failure did, not only what broke.
///
/// A task id is the only dedup key, and every caller builds a fresh one per
/// attempt -- the desktop form, the CLI, the issue importers. So a save that
/// replaced the file and then could not confirm it durable
/// (`PublishedNotDurable`, which [`publish`] adopted into memory) must not
/// read as "nothing was created": a caller that retries duplicates the task,
/// or the whole imported batch. The wording follows [`delete`]'s published
/// arm: the caller is told the change is in place and only the confirmation
/// is missing. A NotPublished failure left nothing anywhere, so it passes
/// through unchanged.
fn creation_consequence(rendered: &str, what: &str) -> String {
    if crate::config::says_in_place_but_unconfirmed(rendered) {
        format!(
            "{rendered} The {what} is in place on the board and in the written file, so \
             creating it again makes a duplicate of it; if it is missing after the next \
             start, this write was lost and nothing survived."
        )
    } else {
        rendered.to_string()
    }
}

/// Insert a brand-new task durably.
///
/// The stage/persist/publish contract [`record`] gives an existing task,
/// extended to the one case `record` cannot express: there is nothing yet at
/// `task.id` for it to find. `build` sees the tasks already committed for
/// `project_id` -- under the same lock the insert commits with, so a value it
/// derives from that set (a board position, a sequence number) answers a
/// question nothing racing this call could have already invalidated -- and
/// returns the task to insert. A save that failed before the replacement
/// leaves the shared map exactly as it was, so a caller told creation failed
/// never goes on to see the task anyway -- but a save that replaced the file
/// and then could not confirm it durable nevertheless put the task there:
/// `publish` adopts that board. Because the task id is the dedup key and
/// callers build a fresh one per attempt, the returned error says the task is
/// in place and that creating it again duplicates it (see
/// [`creation_consequence`]).
pub async fn create(
    gate: &ProjectLifecycleGate,
    tasks: &Tasks,
    projects: &RwLock<HashMap<Uuid, Project>>,
    storage: &Storage,
    project_id: Uuid,
    build: impl FnOnce(&HashMap<Uuid, Task>) -> Task,
) -> Result<Task, String> {
    // The deletion barrier is held before anything is read: if a deletion is
    // underway, this creation waits for it to settle and is then refused by
    // the existence check below -- the Project went away while this creation
    // was waiting. Sharing one per-Project gate with the deletion is what
    // keeps a deletion's Task-decision frozen against a birth.
    let _gate = gate.read(project_id).await;
    let mut tasks_w = tasks.write().await;

    // Asking whether the Project exists before waiting on this lock would
    // reopen the window Project deletion closed for one of its own Tasks: a
    // create that decided a Project still exists and then waited could land
    // its insert after a deletion released it here, and the Task file of a
    // Project that no longer exists would be back on disk. Checked under the
    // lock instead, either the create lands beside the Project it names or it
    // does not happen. The small read under this write guard is the ordered
    // side of the pair -- no code knowingly holds a Project lock and then
    // waits on the Task map, which the review contract this file relies on.
    if !projects.read().await.contains_key(&project_id) {
        return Err(format!(
            "Project {project_id} does not exist or was deleted, so no Task can be created for it"
        ));
    }

    let mut staged = stage_project(&tasks_w, project_id);
    let task = build(&staged);
    staged.insert(task.id, task.clone());
    publish(&mut tasks_w, storage, project_id, staged)
        .map_err(|e| creation_consequence(&e, "task"))?;
    Ok(task)
}

/// [`create`], for several Tasks that arrive together (an import).
///
/// All of them reach the board in one write after the file accepted the whole
/// batch, so a save refused before the replacement leaves neither the board
/// nor the disk with a part of it. A batch that replaced the file and then
/// could not confirm it durable is nevertheless entirely in place -- `publish`
/// adopts that board -- so the returned error says the batch is in place and
/// that retrying the import reimports a duplicate batch (see
/// [`creation_consequence`]).
pub async fn create_batch(
    gate: &ProjectLifecycleGate,
    tasks: &Tasks,
    projects: &RwLock<HashMap<Uuid, Project>>,
    storage: &Storage,
    project_id: Uuid,
    new_tasks: &[Task],
) -> Result<(), String> {
    // The deletion barrier; see [`create`].
    let _gate = gate.read(project_id).await;
    let mut tasks_w = tasks.write().await;

    // The same under-lock existence check [`create`] does, for the same
    // reason: a Project removed while the batch was waiting must not get its
    // Task file back.
    if !projects.read().await.contains_key(&project_id) {
        return Err(format!(
            "Project {project_id} does not exist or was deleted, so no Task can be imported \
             into it"
        ));
    }

    let mut staged = stage_project(&tasks_w, project_id);
    for task in new_tasks {
        staged.insert(task.id, task.clone());
    }
    publish(&mut tasks_w, storage, project_id, staged).map_err(|e| {
        creation_consequence(
            &e,
            if new_tasks.len() > 1 {
                "batch of tasks"
            } else {
                "task"
            },
        )
    })
}

/// Renumber `target_status`'s column so `task_id` sits at `new_position` and
/// every card in that column has a distinct, gapless position.
///
/// Takes the map it should mutate rather than reading the live one, so the
/// terminal path can run it inside
/// [`terminalize`]'s final
/// write. Positions computed before a cleanup subprocess ran are stale by the
/// time it finishes -- another card may have been dragged into the same column
/// meanwhile -- and committing them would publish the card at a position that
/// was correct a second ago.
pub(crate) fn renumber_column(
    staged: &mut HashMap<Uuid, Task>,
    project_id: Uuid,
    task_id: Uuid,
    target_status: &TaskStatus,
    new_position: i32,
) {
    let mut column: Vec<(Uuid, i32)> = staged
        .values()
        .filter(|t| t.project_id == project_id && t.status == *target_status && t.id != task_id)
        .map(|t| (t.id, t.position))
        .collect();
    column.sort_by_key(|(_, pos)| *pos);

    let clamped = new_position.max(0).min(column.len() as i32) as usize;
    column.insert(clamped, (task_id, 0));

    let now = chrono::Utc::now();
    for (idx, (tid, _)) in column.iter().enumerate() {
        if let Some(task) = staged.get_mut(tid) {
            task.position = idx as i32;
            task.updated_at = now;
        }
    }
}

/// What [`enqueue_leased`] found, and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueVerdict {
    /// The Task was in the column the caller saw and is now at the top of
    /// Queue. Carries its title.
    Queued(String),
    /// The Task was already in Queue, so nothing was written. Which front door
    /// put it there cannot be told from the record, and this does not claim to.
    AlreadyQueued(String),
    /// The Task is in another column than the caller saw; nothing was changed.
    Moved(TaskStatus),
    /// The Task's coding input is no longer the one the caller approved;
    /// nothing was changed.
    InputChanged,
    Missing,
    OtherProject,
    /// The Task cannot be run until something else is resolved, so queueing it
    /// would only park it in Queue; nothing was changed.
    Blocked(String),
}

/// Put a Task at the top of Queue provided it is still in `seen_in`, the
/// column the caller saw it in.
///
/// The same transition the board's single-Task enqueue performs through
/// `reorder_task` with `expected_status` (the drawer's Start and Retry and the
/// card menu's "Add to Queue"): status, the activity record, the reset of
/// execution state and the top-of-column position all land in one write. It
/// only queues. Admission, the Task Checkout, the branch and the agent stay
/// with the executor's scheduler.
///
/// The check and the change share the Task store's write, so the Task cannot
/// move between them. The caller holds the Task's lifecycle lease. No owner is
/// ended: a Task that is not in Queue or beyond has none, and any other
/// `seen_in` is the caller's responsibility to refuse.
///
/// `approved_input`, when given, is the `task_input_digest` a person approved.
/// It is compared in the same write, so a Task edited since is refused as
/// [`EnqueueVerdict::InputChanged`] instead of being queued with input nobody
/// approved. The board passes none: a person starting a Task there is looking
/// at its current definition.
pub async fn enqueue_leased(
    tasks: &Tasks,
    storage: &Storage,
    project_id: Uuid,
    task_id: Uuid,
    seen_in: &TaskStatus,
    approved_input: Option<&str>,
) -> Result<EnqueueVerdict, String> {
    let verdict = std::sync::Mutex::new(EnqueueVerdict::Missing);
    let revise = |staged: &mut HashMap<Uuid, Task>| -> bool {
        let (found, changed) = match staged.get_mut(&task_id) {
            None => (EnqueueVerdict::Missing, false),
            Some(task) if task.project_id != project_id => (EnqueueVerdict::OtherProject, false),
            Some(task) if task.status == TaskStatus::Queue => {
                (EnqueueVerdict::AlreadyQueued(task.title.clone()), false)
            }
            Some(task) if task.status != *seen_in => (EnqueueVerdict::Moved(task.status.clone()), false),
            Some(task)
                if approved_input.is_some_and(|digest| crate::queue::prompt::task_input_digest(task) != digest) =>
            {
                (EnqueueVerdict::InputChanged, false)
            }
            Some(task) if task.cleanup_in_flight => (
                EnqueueVerdict::Blocked(
                    "An earlier cleanup of this Task's checkout is unfinished, so it cannot run yet".into(),
                ),
                false,
            ),
            Some(task) if task.republish_pending_refusal().is_some() => (
                EnqueueVerdict::Blocked(
                    "A restack of this Task's published branch is unfinished, so it cannot run yet".into(),
                ),
                false,
            ),
            Some(task) if task.worker_recovery_refusal().is_some() => (
                EnqueueVerdict::Blocked(task.worker_recovery_refusal().unwrap_or_default()),
                false,
            ),
            Some(task) if task.integration_unfinished_refusal().is_some() => (
                EnqueueVerdict::Blocked(
                    "An integration of this Task into a local branch is unfinished, so it cannot run yet".into(),
                ),
                false,
            ),
            Some(task) => {
                let from = task.status.clone();
                task.status = TaskStatus::Queue;
                task.record_move(&from);
                if classify_status_transition(&from, &TaskStatus::Queue)
                    == StatusTransitionEffect::ResetExecutionState
                {
                    task.reset_execution_state();
                }
                let title = task.title.clone();
                renumber_column(staged, project_id, task_id, &TaskStatus::Queue, 0);
                (EnqueueVerdict::Queued(title), true)
            }
        };
        *verdict.lock().unwrap() = found;
        changed
    };
    match record_if_changed(tasks, storage, task_id, &revise).await {
        Ok(_) => Ok(verdict.into_inner().unwrap()),
        Err(_) if !tasks.read().await.contains_key(&task_id) => Ok(EnqueueVerdict::Missing),
        Err(error) => Err(error),
    }
}

/// Select, amend and durably commit a task the caller does not yet have the id
/// of, under one write-lock hold.
///
/// [`commit_task`] serves every ordinary mutation, which already knows which
/// task it means. Queue promotion does not: "which task becomes `InProgress`
/// next" is itself the decision, made from capacity and ordering rules that
/// have to see the same map the mutation then commits, or a second caller
/// racing the first could select the very same task. `select` and `amend`
/// therefore share the write guard `create` already holds across staging and
/// publish, rather than `select` taking its own read lock first and handing
/// back an id that may no longer be the right answer by the time `amend`
/// runs.
///
/// `None` from `select` means there was nothing to do, which is not a failure
/// -- an empty queue is not an error, just like [`commit_task`] finding no
/// such task is not one.
pub async fn commit_selected(
    tasks: &Tasks,
    storage: &Storage,
    select: impl FnOnce(&HashMap<Uuid, Task>) -> Option<Uuid>,
    amend: impl FnOnce(&mut HashMap<Uuid, Task>, Uuid),
) -> Result<Option<Task>, String> {
    let mut tasks_w = tasks.write().await;
    let Some(task_id) = select(&tasks_w) else {
        return Ok(None);
    };
    let project_id = tasks_w
        .get(&task_id)
        .map(|t| t.project_id)
        .expect("select must return an id present in the map it was given");

    let mut staged = stage_project(&tasks_w, project_id);
    amend(&mut staged, task_id);
    if let Some(task) = staged.get_mut(&task_id) {
        task.updated_at = chrono::Utc::now();
    }

    publish(&mut tasks_w, storage, project_id, staged)?;
    Ok(tasks_w.get(&task_id).cloned())
}

/// How a status transition affects a task's execution state and its worktree.
///
/// Resetting execution state and destroying a worktree are different
/// operations, and these variants keep them apart. One variant used to do
/// both, which made re-queuing a failed task also delete the work that task
/// had produced. Worse, a task's branch and worktree path are derived from its
/// id, which a retry does not change, so that asynchronous deletion raced the
/// retry recreating them at the very same path: the cleanup could remove the
/// *successful* second attempt's directory, branch and commits, then clear
/// `worktree_path`, leaving a task reporting completion with nothing on disk
/// behind it.
///
/// Every transition maps to exactly one variant, so no call site can spawn
/// cleanup twice by evaluating overlapping conditions in the wrong order.
///
/// Shared by the desktop commands and the daemon's IPC handlers -- see
/// [`classify_status_transition`] -- so a card dragged through the CLI and one
/// dragged in the app resolve the same transition the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusTransitionEffect {
    /// Moving back into the workflow: out of `Error`, or back to an early
    /// column. Clears phase, progress and the error message, which is what
    /// makes the task eligible to be picked up and run again.
    ///
    /// Deliberately preserves `worktree_path` and `branch_name`: the next
    /// execution reattaches to that branch and continues from the work already
    /// there. Discarding a worktree is an explicit destructive action, not a
    /// side effect of moving a card back into a column.
    ResetExecutionState,
    /// Moving to `Done`: the task is finished with its worktree, so remove it.
    /// Everything else — `branch_name` included, for PR creation — is left
    /// untouched.
    CleanUpWorktree,
    /// No effect on execution state or worktree.
    None,
}

/// `Done` is tested first so a task finishing *out of* `Error` still gets its
/// terminal cleanup. Calling a task done is an explicit statement that its
/// worktree is no longer needed; re-queuing that same task is the opposite.
///
/// The one classifier both front doors call. A front door that assigned
/// `task.status` directly used to reach this task's board through a status
/// write with none of the consequences above -- a `Failed -> InProgress` move
/// that kept a stale `phase = Failed` and `error_message`, which then failed
/// every readiness check even though the status said the task was running.
pub(crate) fn classify_status_transition(
    old_status: &TaskStatus,
    new_status: &TaskStatus,
) -> StatusTransitionEffect {
    if matches!(new_status, TaskStatus::Done) {
        StatusTransitionEffect::CleanUpWorktree
    } else if *old_status == TaskStatus::Error
        || matches!(
            new_status,
            TaskStatus::Backlog | TaskStatus::Queue | TaskStatus::InProgress
        )
    {
        StatusTransitionEffect::ResetExecutionState
    } else {
        StatusTransitionEffect::None
    }
}

/// Why deleting a Task is refused while it records an unfinished integration.
pub const DELETE_BLOCKED_BY_INTEGRATION_REASON: &str =
    "This Task records an unfinished integration of its approved work into a local branch. \
     Deleting it would remove the only record of that operation, which may already have moved \
     the target branch, so the Task was kept. Resume or discard the integration first (its \
     controls are in the Task's Human Review panel). A record found in a status that offers no \
     such panel cannot be deleted until that is handled.";

/// Why this Task may not be deleted for reasons that no cleanup can settle, or
/// `None`.
///
/// Deliberately narrower than [`project_deletion_refusal`]. That predicate also
/// refuses a Task that still has a checkout or an interrupted cleanup, because
/// a Project deletion cannot clean them; a Task deletion is exactly the
/// operation that does, and refusing there would remove the retry path for an
/// interrupted cleanup. What is refused here is only what deleting the record
/// would destroy: an unfinished local integration (wherever the Task's status
/// put it, which is not the same set as
/// [`Task::integration_unfinished_refusal`] answers for a status move) and an
/// unfinished restack of its published branch.
pub fn task_deletion_refusal(task: &Task) -> Option<TerminalizeRefusal> {
    if task
        .local_integration
        .as_ref()
        .is_some_and(|r| r.is_pending())
    {
        return Some(TerminalizeRefusal::DeleteBlockedByIntegration);
    }
    if task.republish_pending_refusal().is_some() {
        return Some(TerminalizeRefusal::RepublishPending);
    }
    None
}

/// Remove a task's record, once and only once nothing is owed on its behalf.
///
/// Deleting is not a status change, so nothing here moves a task to a terminal
/// column. What it shares with terminalization is the obligation: the record is
/// the only thing that names a checkout, so removing one while its worktree is
/// still there strands the directory with nothing left pointing at it and no
/// later operation able to find it. The cleanup therefore runs first, under the
/// same lease, and a refusal ends the delete instead of being logged past --
/// the task, its `worktree_path`, its branch and every uncommitted byte
/// survive, and the user can deal with the checkout and ask again.
///
/// `Ok(false)` means there was no such task, which is not a failure: the caller
/// asked for it to be gone and it is.
///
/// One operation for both front doors. The desktop command and the daemon's IPC
/// handler call this rather than each holding a copy of the policy, because a
/// delete that is safe through one and unsafe through the other is not a safe
/// delete.
pub async fn delete(ctx: TerminalizeCtx<'_>, task_id: Uuid) -> Result<bool, TerminalizeRefusal> {
    // Held across the cleanup *and* the record removal, so nothing can start a
    // transition against a task that is on its way out, and the removal cannot
    // land between a cleanup starting and that cleanup recording its outcome.
    let _lease = ctx
        .authority
        .acquire(task_id)
        .await
        .map_err(TerminalizeRefusal::Busy)?;

    // Kept before `ctx` is handed to the terminalization, which consumes it.
    let tasks = ctx.tasks;
    let storage = ctx.storage;

    // Re-read under the lease: anything sampled before waiting for it is a
    // guess about the past.
    let (project_id, current_status) = {
        let tasks_r = tasks.read().await;
        match tasks_r.get(&task_id) {
            Some(task) => {
                // Before the terminalization below, which only recognizes an
                // unfinished integration in the two statuses that offer a
                // Resume. Nothing has been touched when this answers.
                if let Some(refusal) = task_deletion_refusal(task) {
                    return Err(refusal);
                }
                (task.project_id, task.status.clone())
            }
            None => return Ok(false),
        }
    };

    // `desired` is the status the task already has: a delete is not a status
    // change, and what is wanted here is the safe removal together with its
    // durable bookkeeping, which is one operation. `terminalize_leased` because
    // the lease is already held; releasing and retaking it around this would
    // open exactly the window the lease exists to close.
    match terminalize_leased(
        ctx,
        task_id,
        Origin::User,
        TerminalizeRequest::new(current_status),
    )
    .await
    {
        Ok(_) => {}
        // It went while this was working. Nothing is owed on a record that is
        // not there.
        Err(TerminalizeRefusal::TaskNotFound) => return Ok(false),
        // Every other refusal ends the delete. `CleanupRefused` is the expected
        // one -- git will not take a checkout that holds work no commit names
        // -- but `ExecutionActive` matters just as much: an agent is running
        // and that checkout is its working directory. None of them may be
        // reported as a successful delete, because for all of them the cleanup
        // obligation the record carries is still outstanding.
        Err(refusal) => return Err(refusal),
    }

    // The checkout is gone and that fact is already durable, so what is left is
    // one write. The file accepts the board without the task before the shared
    // map loses it: a delete the file did not accept is one the next start
    // undoes, and reporting it as done would leave the user looking at a task
    // that comes back.
    let mut tasks_w = tasks.write().await;
    let mut staged = stage_project(&tasks_w, project_id);
    if staged.remove(&task_id).is_none() {
        return Ok(false);
    }

    let list: Vec<Task> = staged.values().cloned().collect();
    if let Err(e) = storage.save_project_tasks(project_id, &list) {
        // The record stays, and it stays truthful: the terminalization above
        // already committed `worktree_path = None` and `cleanup_in_flight =
        // false`, so what survives says the cleanup succeeded, which it did.
        // Asking again is then an ordinary delete with nothing left to clean
        // up.
        if !crate::config::board_write_published(&e) {
            return Err(TerminalizeRefusal::NotRecorded(format!(
                "the worktree for task {task_id} was cleaned up, but removing the task from the \
                 board failed, so the task is still there and can simply be deleted again: {e}"
            )));
        }
        // The file already lacks the task; only its durability is unconfirmed.
        // Keeping it in memory would let the next save of any sibling put it
        // back, undoing a delete the user was told about.
        tasks_w.remove(&task_id);
        return Err(TerminalizeRefusal::NotRecorded(format!(
            "the worktree for task {task_id} was cleaned up and the task was removed from the \
             board, but SlashIt could not confirm that this reached stable storage, so the task \
             may reappear if the computer stops before the next successful save: {e}"
        )));
    }

    tasks_w.remove(&task_id);
    Ok(true)
}

/// Per-Project barrier serializing Project deletion against Task creation.
///
/// Project deletion takes this gate **exclusively** before its lease snapshot
/// and holds it for the whole transaction; every Task creation takes it
/// **shared** from before its board insert until its project-existence check
/// has answered. The two checks of a deletion (the durable Task records and
/// the board) therefore look at a Task set no concurrent creation can extend,
/// and a creation that arrives mid-transaction waits for the deletion to
/// settle and is then refused by the existence check those functions already
/// run -- a refusal before anything was written, exactly the guarantee a
/// deletion refusal itself promises.
///
/// The gate is deliberately *inside* [`create`] and [`create_batch`]: they
/// are the authoritative Task-creation boundary, so no later front door
/// (desktop, CLI, import) can bypass it by forgetting its own call.
///
/// One-process guarantee: a separate SlashIt process (desktop and daemon can
/// both run against the same storage) is not excluded by an in-process
/// barrier; the pre-existing, unsupported cross-process contract of the
/// storage layer applies unchanged. See `docs/architecture/daemon.md`.
#[derive(Clone, Default)]
pub struct ProjectLifecycleGate {
    inner: std::sync::Arc<PerProjectGate>,
}

#[derive(Default)]
struct PerProjectGate {
    per_project: tokio::sync::Mutex<HashMap<Uuid, Arc<tokio::sync::RwLock<()>>>>,
}

pub type ProjectLifecycleRead = tokio::sync::OwnedRwLockReadGuard<()>;
pub type ProjectLifecycleWrite = tokio::sync::OwnedRwLockWriteGuard<()>;

impl ProjectLifecycleGate {
    pub fn new() -> Self {
        Self::default()
    }

    async fn per_project(&self, project_id: Uuid) -> Arc<tokio::sync::RwLock<()>> {
        let mut per_project = self.inner.per_project.lock().await;
        per_project
            .entry(project_id)
            .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
            .clone()
    }

    /// A Task creation's shared hold, from its first board read to its insert.
    pub async fn read(&self, project_id: Uuid) -> ProjectLifecycleRead {
        self.per_project(project_id).await.read_owned().await
    }

    /// A Project deletion's exclusive hold, for its whole transaction.
    pub async fn write(&self, project_id: Uuid) -> ProjectLifecycleWrite {
        self.per_project(project_id).await.write_owned().await
    }
}

/// Why a Task blocks the deletion of the Project it belongs to, or `None`.
///
/// [`Task::integration_unfinished_refusal`] deliberately answers only where a
/// Resume exists, because a status move should not pin a record the person has
/// no way to finish. Deleting a whole Project is different: the record is
/// durable evidence of a Git operation that may already have moved a branch,
/// and removing the Tasks file deletes the only copy of it — with no control
/// and no target branch, whatever it proves would be stranded. Deletion is
/// therefore refused while the record is pending, wherever the Task's status
/// put it, which fulfills both sides: a status move asks
/// [`Self::integration_unfinished_refusal`], and a Project deletion asks this.
pub fn project_deletion_refusal(task: &Task) -> Option<String> {
    let title = task.title.as_str();
    let id = task.id;
    if task
        .local_integration
        .as_ref()
        .is_some_and(|r| r.is_pending())
    {
        return Some(format!(
            "task {title:?} ({id}) records an unfinished integration of its approved work; \
             resume or discard that integration before deleting the Project"
        ));
    }
    if task.republish_pending_refusal().is_some() {
        return Some(format!(
            "task {title:?} ({id}) has an unfinished restack of its published branch; \
             resume or discard that restack before deleting the Project"
        ));
    }
    if task.cleanup_in_flight {
        return Some(format!(
            "the cleanup of task {title:?}'s ({id}) checkout is unfinished, because an earlier \
             removal was interrupted; delete the Task again before deleting the Project"
        ));
    }
    if let Some(worktree_path) = task.worktree_path.as_deref() {
        return Some(format!(
            "task {title:?} ({id}) still has a checkout at {worktree_path}; delete the Task \
             (its deletion removes the checkout safely and keeps its Git recovery refs) \
             before deleting the Project"
        ));
    }
    None
}

/// Acquire every Project-task lifecycle lease at once, releasing again if any
/// of them cannot be taken.
///
/// Project deletion must not open a Task's checkout, start a run against it or
/// ask a Task mutation to persist while one of the Project's Tasks is mid
/// transition. Every transition that matters -- starting a run, crafting a
/// worktree, terminalizing, deleting -- takes this same lease first, so
/// holding all of a Project's leases across the deletion makes its remaining
/// Tasks momentarily inaccessible to exactly those transitions and nothing
/// else. A read of the board, and an ordinary edit of a Task's text or of its
/// dependencies, do not take the lease and are unaffected.
///
/// The leases are dropped (`Err` et al. unwind them) when any acquisition
/// times out, so a refusal leaves every Task as found -- the same contract as
/// [`TaskLifecycleLocks::acquire`], extended to the set.
pub async fn hold_project_task_leases(
    authority: &TaskLifecycleLocks,
    tasks: &Tasks,
    project_id: Uuid,
) -> Result<Vec<LifecycleLease>, String> {
    let ids = {
        let tasks_r = tasks.read().await;
        project_task_ids(&tasks_r, project_id)
    };
    let mut leases = Vec::with_capacity(ids.len());
    for id in &ids {
        match authority.acquire(*id).await {
            Ok(lease) => leases.push(lease),
            Err(message) => return Err(message),
        }
    }
    Ok(leases)
}

/// The ids of a Project's Tasks currently on the board.
pub fn project_task_ids(tasks: &HashMap<Uuid, Task>, project_id: Uuid) -> Vec<Uuid> {
    tasks
        .values()
        .filter(|t| t.project_id == project_id)
        .map(|t| t.id)
        .collect()
}

/// [`terminalize`] for a caller that already holds the task's lease.
///
/// Split out so an operation which must take the lease for its own reasons --
/// `reorder_task` computing column positions, `delete_task` removing the record
/// afterwards -- does not have to release and retake it around the
/// terminalization, which would open exactly the window the lease exists to
/// close.
pub async fn terminalize_leased(
    ctx: TerminalizeCtx<'_>,
    task_id: Uuid,
    origin: Origin,
    request: TerminalizeRequest<'_>,
) -> Result<Task, TerminalizeRefusal> {
    // Re-read under the lease: anything sampled before waiting for it is a
    // guess about the past.
    let (project_id, worktree_path, branch_name, quarantined) = {
        let tasks = ctx.tasks.read().await;
        let task = tasks
            .get(&task_id)
            .ok_or(TerminalizeRefusal::TaskNotFound)?;
        // Before anything else, whatever the origin: a merged pull request, a
        // card dragged to Done, a delete and an explicit cleanup all end here,
        // and every one of them would remove the checkout and branch that the
        // pending restack's recovery needs. Nothing is written, so the record
        // and its backup are exactly as found.
        if task.republish_pending_refusal().is_some() {
            return Err(TerminalizeRefusal::RepublishPending);
        }
        if task.worker_recovery_refusal().is_some() {
            return Err(TerminalizeRefusal::WorkerRecoveryRequired);
        }
        // Likewise for an unfinished local integration: finishing the task
        // here would remove the checkout and withdraw the state its Resume
        // re-proves. An integration that completes the task does so only
        // after recording itself as integrated, so it is never asked this.
        if task.integration_unfinished_refusal().is_some() {
            return Err(TerminalizeRefusal::IntegrationUnfinished);
        }
        (
            task.project_id,
            task.worktree_path.clone(),
            task.branch_name.clone(),
            task.cleanup_in_flight,
        )
    };

    if let Some(running) = ctx.running {
        if running.is_task_running(task_id).await {
            return Err(TerminalizeRefusal::ExecutionActive);
        }
    }

    // A checkout a previous process was interrupted mid-removal of is not
    // something to quietly try again: nothing on disk says how far that removal
    // got, and an automatic caller retrying it on a timer is the loop this
    // design exists to remove. A person asking explicitly is the stated way
    // back out, and the removal they trigger is the same safe one, so it either
    // succeeds or reports why not.
    //
    // Deliberately not conditioned on the checkout still being there. A
    // quarantine whose directory has since gone is safe to finish, but startup
    // already reconciles exactly that case, so acting on it here would buy no
    // liveness at all and would cost the one rule worth having: while a task is
    // quarantined, nothing automatic touches it.
    if quarantined && origin == Origin::Automatic {
        return Err(TerminalizeRefusal::Quarantined(
            worktree_path.unwrap_or_else(|| format!("(no path recorded for task {task_id})")),
        ));
    }

    // No worktree: nothing destructive to do, so the terminal state is simply
    // committed. The flag is written as `false` either way, which is what
    // reconciles a task whose quarantined checkout has since gone.
    let Some(worktree_path) = worktree_path else {
        return commit(
            ctx.tasks,
            ctx.storage,
            task_id,
            project_id,
            Outcome::Cleaned,
            &request,
        )
        .await;
    };

    let repo_path = resolve_repo_path(ctx.projects, ctx.repositories, project_id)
        .await
        .map_err(TerminalizeRefusal::RepositoryUnresolved)?;

    // Durable before destructive. Ordering, not defence in depth: once the
    // subprocess exists, a crash is indistinguishable from a healthy task
    // unless this write already reached the disk.
    stamp_cleanup_in_flight(ctx.tasks, ctx.storage, task_id, project_id, &request)
        .await
        .map_err(TerminalizeRefusal::NotRecorded)?;

    let removal = ctx
        .worktree_manager
        // The branch too: a checkout whose directory is gone is looked up by
        // both, so that one git registers under a path spelled differently
        // from the recorded one is still found. See `CheckoutState`.
        .remove(&worktree_path, &repo_path, branch_name.as_deref())
        .await;

    let outcome = match removal {
        Ok(()) => Outcome::Cleaned,
        Err(reason) => Outcome::Kept {
            worktree_path: worktree_path.clone(),
            reason,
        },
    };

    commit(
        ctx.tasks,
        ctx.storage,
        task_id,
        project_id,
        outcome,
        &request,
    )
    .await
}

enum Outcome {
    Cleaned,
    Kept {
        worktree_path: String,
        reason: String,
    },
}

/// Resolve a project's repository local path.
async fn resolve_repo_path(
    projects: &Arc<RwLock<HashMap<Uuid, Project>>>,
    repositories: &Arc<RwLock<HashMap<Uuid, Repository>>>,
    project_id: Uuid,
) -> Result<String, String> {
    let projects_r = projects.read().await;
    let project = projects_r.get(&project_id).ok_or("Project not found")?;
    let repo_id = project.repository_id.ok_or("No repository linked")?;
    drop(projects_r);

    let repos = repositories.read().await;
    let repo = repos.get(&repo_id).ok_or("Repository not found")?;
    Ok(repo.local_path.clone())
}

/// Write `cleanup_in_flight = true`, plus whatever the caller needs recorded
/// before anything destructive runs, and wait for it to reach the disk.
///
/// An `Err` here aborts the terminalization before a subprocess exists, which
/// is the safe direction: a cleanup that cannot be announced is a cleanup that
/// must not happen, because its interruption would be invisible.
async fn stamp_cleanup_in_flight(
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    project_id: Uuid,
    request: &TerminalizeRequest<'_>,
) -> Result<(), String> {
    let mut tasks_w = tasks.write().await;
    let mut staged = stage_project(&tasks_w, project_id);

    if let Some(amend) = request.record_first {
        amend(&mut staged);
    }
    let Some(task) = staged.get_mut(&task_id) else {
        return Err(format!(
            "task {task_id} vanished before its cleanup was recorded"
        ));
    };
    task.cleanup_in_flight = true;
    task.updated_at = chrono::Utc::now();

    publish(&mut tasks_w, storage, project_id, staged).map_err(|e| {
        if crate::config::says_in_place_but_unconfirmed(&e) {
            // The record is in the file and on the board: the task is flagged
            // as having a cleanup in flight, which holds it out of automatic
            // terminalization until that is resolved.
            format!(
                "the worktree cleanup for task {task_id} was not started, but the record that it \
                 had begun is already saved and the task is flagged as having a cleanup in \
                 flight; SlashIt could not confirm that reached stable storage: {e}"
            )
        } else {
            format!(
                "the worktree cleanup for task {task_id} was not started, because recording that \
                 it had begun failed and an interrupted removal would then be invisible: {e}"
            )
        }
    })
}

/// The one durable write that ends a terminalization.
async fn commit(
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    project_id: Uuid,
    outcome: Outcome,
    request: &TerminalizeRequest<'_>,
) -> Result<Task, TerminalizeRefusal> {
    let mut tasks_w = tasks.write().await;
    let mut staged = stage_project(&tasks_w, project_id);

    // A `record_first` that never ran because there was no worktree still has
    // to reach the disk; applying it again where it did run is harmless,
    // because these amendments set facts rather than accumulate them.
    if let Some(amend) = request.record_first {
        amend(&mut staged);
    }

    let kept = match &outcome {
        Outcome::Cleaned => None,
        Outcome::Kept {
            worktree_path,
            reason,
        } => Some((worktree_path.clone(), reason.clone())),
    };

    {
        let Some(task) = staged.get_mut(&task_id) else {
            return Err(TerminalizeRefusal::TaskNotFound);
        };
        task.cleanup_in_flight = false;
        task.updated_at = chrono::Utc::now();

        match &kept {
            None => {
                let from = task.status.clone();
                task.status = request.desired.clone();
                task.record_move(&from);
                // Cleared together, in the same write, because either alone is
                // a lie: a `Done` task holding a `worktree_path` claims a
                // checkout that is gone, and a cleared path on a non-terminal
                // task loses the only handle back to one that is not.
                task.worktree_path = None;
                task.error_message = None;
            }
            Some((worktree_path, reason)) => {
                // Status, worktree, branch and every uncommitted byte stay
                // exactly as they were. The only change is that the task now
                // says why.
                task.error_message = Some(format!(
                    "The worktree at {worktree_path} was kept: {reason}"
                ));
            }
        }
    }

    if kept.is_none() {
        if let Some(amend) = request.on_success {
            amend(&mut staged);
        }
    }

    let committed = staged
        .get(&task_id)
        .cloned()
        .ok_or(TerminalizeRefusal::TaskNotFound)?;

    publish(&mut tasks_w, storage, project_id, staged).map_err(|e| {
        let in_place = crate::config::says_in_place_but_unconfirmed(&e);
        TerminalizeRefusal::NotRecorded(match (&kept, in_place) {
            (None, false) => format!(
                "the worktree was removed, but recording task {task_id} as finished failed, so \
                 the board still shows its previous state and still names a checkout that is \
                 gone; the next start reconciles it: {e}"
            ),
            (None, true) => format!(
                "the worktree was removed and task {task_id} is recorded as finished, but \
                 SlashIt could not confirm that reached stable storage, so after a power loss \
                 the board may still name a checkout that is gone; the next start reconciles \
                 it: {e}"
            ),
            (Some(_), false) => format!("the worktree was kept, and recording why failed: {e}"),
            (Some(_), true) => format!(
                "the worktree was kept, and the record of why is saved but not confirmed on \
                 stable storage: {e}"
            ),
        })
    })?;

    match kept {
        None => Ok(committed),
        Some((worktree_path, reason)) => Err(TerminalizeRefusal::CleanupRefused {
            worktree_path,
            reason,
        }),
    }
}

/// A working copy of one project's tasks, to be mutated and then committed as a
/// whole.
fn stage_project(tasks: &HashMap<Uuid, Task>, project_id: Uuid) -> HashMap<Uuid, Task> {
    tasks
        .values()
        .filter(|t| t.project_id == project_id)
        .map(|t| (t.id, t.clone()))
        .collect()
}

/// Persist first, then publish -- and publish exactly what reached the disk.
///
/// The order is the contract the rest of the backend already holds: the board
/// must never show a state the file does not have, because the file is what the
/// next start reads and a memory-only change is one a restart silently undoes.
/// Running the whole transaction under one write guard is what stops a sibling
/// save landing between the two halves.
fn publish(
    tasks_w: &mut HashMap<Uuid, Task>,
    storage: &Storage,
    project_id: Uuid,
    staged: HashMap<Uuid, Task>,
) -> Result<(), String> {
    publish_typed(tasks_w, storage, project_id, staged).map_err(|e| e.to_string())
}

fn publish_typed(
    tasks_w: &mut HashMap<Uuid, Task>,
    storage: &Storage,
    project_id: Uuid,
    staged: HashMap<Uuid, Task>,
) -> Result<(), RecordWriteError> {
    let list: Vec<Task> = staged.values().cloned().collect();
    let saved = storage.save_project_tasks(project_id, &list);
    // A board the file already holds but could not confirm durable is still
    // the file's board: memory follows it, or the next save of any sibling
    // would write the previous board back over it. The error is still
    // returned, and says the change is in place.
    if saved
        .as_ref()
        .err()
        .is_none_or(crate::config::board_write_published)
    {
        for (id, task) in staged {
            tasks_w.insert(id, task);
        }
    }
    saved.map_err(|e| {
        if crate::config::board_write_published(&e) {
            RecordWriteError::PublishedNotDurable(e.to_string())
        } else {
            RecordWriteError::NotPublished(e.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::paths::{AppPaths, StateLocation};
    use crate::domain::{AgentConfig, AgentType, ProjectScope, TaskPhase};
    use crate::test_helpers::create_test_task_full;
    use crate::worktree::WorktreeManager;

    /// Everything a terminalization touches, under one tempdir.
    struct World {
        _tmp: tempfile::TempDir,
        tasks: Tasks,
        projects: Arc<RwLock<HashMap<Uuid, Project>>>,
        repositories: Arc<RwLock<HashMap<Uuid, Repository>>>,
        worktree_manager: WorktreeManager,
        storage: Storage,
        authority: TaskLifecycleLocks,
        gate: ProjectLifecycleGate,
        project_id: Uuid,
        storage_paths: AppPaths,
    }

    impl World {
        fn ctx(&self) -> TerminalizeCtx<'_> {
            TerminalizeCtx {
                tasks: &self.tasks,
                projects: &self.projects,
                repositories: &self.repositories,
                worktree_manager: &self.worktree_manager,
                storage: &self.storage,
                authority: &self.authority,
                running: None,
            }
        }

        async fn task(&self, id: Uuid) -> Task {
            self.tasks.read().await.get(&id).cloned().expect("task")
        }

        /// The task as it is on disk, which is the only version a restart sees.
        fn persisted(&self, id: Uuid) -> Task {
            self.storage
                .load_project_tasks(self.project_id)
                .expect("the board must be readable")
                .into_iter()
                .find(|t| t.id == id)
                .expect("the task must be on disk")
        }

        /// Make every `save_project_tasks` fail deterministically by putting a
        /// regular file where the tasks directory has to be, so the
        /// `create_dir_all` inside the atomic write fails with `AlreadyExists`.
        /// No permission bits, so this behaves identically for every user
        /// including root.
        fn block_persistence(&self) {
            let blocker = self.storage_paths.config_dir().join("tasks");
            let _ = std::fs::remove_dir_all(&blocker);
            std::fs::write(&blocker, b"not a directory").expect("place persistence blocker");
        }
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be spawnable");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A world whose project resolves to a real git repository.
    ///
    /// `repo_path` of `None` gives a project with no repository at all, which
    /// is what "the repository cannot be resolved" looks like from here.
    fn world(with_repo: bool) -> World {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        let app_paths = AppPaths::with_roots(
            root.join("config"),
            root.join("data"),
            root.join("cache"),
            root.join("runtime"),
        );

        let project_id = Uuid::new_v4();
        let repository_id = Uuid::new_v4();

        let mut repositories = HashMap::new();
        let repo_link = if with_repo {
            let repo_dir = root.join("repo");
            std::fs::create_dir_all(&repo_dir).unwrap();
            git(&repo_dir, &["init", "-q", "-b", "main"]);
            git(&repo_dir, &["config", "user.email", "test@example.com"]);
            git(&repo_dir, &["config", "user.name", "Test"]);
            std::fs::write(repo_dir.join("README.md"), "base\n").unwrap();
            git(&repo_dir, &["add", "."]);
            git(&repo_dir, &["commit", "-q", "-m", "init"]);
            // A bare `origin` whose `HEAD` is recorded locally, so that a
            // task checkout has a default base to start from.
            let origin = root.join("origin.git");
            git(
                &repo_dir,
                &["init", "-q", "--bare", origin.to_str().unwrap()],
            );
            git(
                &repo_dir,
                &["remote", "add", "origin", origin.to_str().unwrap()],
            );
            git(&repo_dir, &["push", "-q", "origin", "main"]);
            git(&repo_dir, &["remote", "set-head", "origin", "main"]);
            repositories.insert(
                repository_id,
                Repository {
                    id: repository_id,
                    local_path: repo_dir.to_str().unwrap().to_string(),
                    remote_url: None,
                    remote_type: None,
                    created_at: chrono::Utc::now(),
                },
            );
            Some(repository_id)
        } else {
            None
        };

        let project = Project {
            id: project_id,
            name: "lifecycle-test".to_string(),
            repository_id: repo_link,
            scope: ProjectScope::Standalone,
            state_location: StateLocation::External,
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
        };

        World {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            projects: Arc::new(RwLock::new(HashMap::from([(project_id, project)]))),
            repositories: Arc::new(RwLock::new(repositories)),
            worktree_manager: WorktreeManager::new(Arc::new(app_paths.clone())),
            storage: Storage::with_paths(app_paths.clone()),
            authority: TaskLifecycleLocks::new(),
            gate: ProjectLifecycleGate::new(),
            project_id,
            storage_paths: app_paths,
            _tmp: tmp,
        }
    }

    fn repo_path(world: &World) -> String {
        world._tmp.path().join("repo").to_str().unwrap().to_string()
    }

    /// Seed a task, plus a sibling in the same project that every whole-file
    /// rewrite must carry through untouched.
    async fn seed(world: &World, status: TaskStatus, worktree: Option<&str>) -> (Uuid, Uuid) {
        let mut task = create_test_task_full("subject", world.project_id, status, 0);
        task.phase = TaskPhase::Coding;
        task.worktree_path = worktree.map(|s| s.to_string());
        task.branch_name = Some("task-abcd1234".to_string());
        let sibling = create_test_task_full("sibling", world.project_id, TaskStatus::Backlog, 1);
        let (id, sibling_id) = (task.id, sibling.id);
        {
            let mut w = world.tasks.write().await;
            w.insert(id, task.clone());
            w.insert(sibling_id, sibling.clone());
        }
        world
            .storage
            .save_project_tasks(world.project_id, &[task, sibling])
            .expect("seed the board");
        (id, sibling_id)
    }

    /// A real worktree of `world`'s repository, with real committed work in it,
    /// so an ordinary `git worktree remove` succeeds.
    async fn worktree_with_committed_work(world: &World, branch: &str) -> String {
        let info = world
            .worktree_manager
            .create(&repo_path(world), branch)
            .await
            .expect("worktree creation");
        let dir = std::path::PathBuf::from(&info.path);
        std::fs::write(dir.join("work.txt"), "work\n").unwrap();
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test"]);
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "task work"]);
        info.path
    }

    // -----------------------------------------------------------------------
    // Success
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_successful_terminalization_removes_the_worktree_and_clears_its_path_together() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, sibling_id) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;

        let done = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect("a clean worktree must be removable");

        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(done.worktree_path, None);
        assert!(!done.cleanup_in_flight);
        assert_eq!(
            done.branch_name.as_deref(),
            Some("task-abcd1234"),
            "the branch names the commits the task produced and is never touched"
        );
        assert!(
            !std::path::Path::new(&wt).exists(),
            "the checkout must be gone"
        );

        // The same three facts on disk, because that is what a restart reads.
        let persisted = world.persisted(id);
        assert_eq!(persisted.status, TaskStatus::Done);
        assert_eq!(persisted.worktree_path, None);
        assert!(!persisted.cleanup_in_flight);

        assert!(
            world.tasks.read().await.contains_key(&sibling_id),
            "a whole-project rewrite must not drop the project's other tasks"
        );
        assert_eq!(
            world.persisted(sibling_id).title,
            "sibling",
            "and must not drop them on disk either"
        );
    }

    // -----------------------------------------------------------------------
    // Refusal
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_refused_removal_keeps_the_status_the_worktree_and_the_branch() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        // Uncommitted, non-ignored work: git refuses to remove this checkout,
        // through the ordinary production path.
        std::fs::write(
            std::path::Path::new(&wt).join("unsaved.txt"),
            "not committed\n",
        )
        .unwrap();
        let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("git must refuse a checkout holding uncommitted work");

        let TerminalizeRefusal::CleanupRefused { reason, .. } = &refusal else {
            panic!("expected a cleanup refusal, got {refusal:?}");
        };
        assert!(
            !reason.is_empty(),
            "the refusal must carry git's own reason, not an invented one"
        );

        let after = world.task(id).await;
        assert_eq!(
            after.status,
            TaskStatus::InProgress,
            "the status must not move"
        );
        assert_eq!(after.worktree_path.as_deref(), Some(wt.as_str()));
        assert_eq!(after.branch_name.as_deref(), Some("task-abcd1234"));
        assert!(
            !after.cleanup_in_flight,
            "the attempt concluded, so nothing is in flight"
        );
        assert!(
            after.error_message.is_some(),
            "a refusal the user has to act on has to be visible on the task"
        );

        assert!(
            std::path::Path::new(&wt).join("unsaved.txt").exists(),
            "the uncommitted work must survive"
        );
        assert!(
            std::path::Path::new(&wt).join("work.txt").exists(),
            "and so must everything else in the checkout"
        );

        let persisted = world.persisted(id);
        assert_eq!(persisted.status, TaskStatus::InProgress);
        assert_eq!(persisted.worktree_path.as_deref(), Some(wt.as_str()));
        assert!(!persisted.cleanup_in_flight);
    }

    #[tokio::test]
    async fn a_project_that_resolves_to_no_repository_keeps_its_worktree_reference() {
        let world = world(false);
        let (id, _) = seed(&world, TaskStatus::InProgress, Some("/tmp/does-not-matter")).await;

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("with no repository there is nowhere to run git");

        assert!(matches!(
            refusal,
            TerminalizeRefusal::RepositoryUnresolved(_)
        ));
        let after = world.task(id).await;
        assert_eq!(after.worktree_path.as_deref(), Some("/tmp/does-not-matter"));
        assert_eq!(after.status, TaskStatus::InProgress);
        assert!(
            !after.cleanup_in_flight,
            "nothing destructive was started, so nothing may be recorded as in flight"
        );
    }

    // -----------------------------------------------------------------------
    // Durable before destructive
    // -----------------------------------------------------------------------

    /// The intent to clean up reaches the disk before the removal runs, and the
    /// order is what this proves rather than asserts.
    ///
    /// Persistence is blocked, so the write that announces the cleanup fails.
    /// If that write happens first, as it must, the removal never runs and the
    /// checkout is still there afterwards. An implementation that removed first
    /// and recorded afterwards would pass every other test in this file and
    /// fail this one with an empty directory: after a crash mid-removal it
    /// would leave a half-deleted checkout that nothing on disk distinguishes
    /// from a healthy one.
    #[tokio::test]
    async fn nothing_destructive_runs_until_the_cleanup_has_been_recorded() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;
        world.block_persistence();

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("a cleanup that cannot be announced must not happen");

        assert!(matches!(refusal, TerminalizeRefusal::NotRecorded(_)));
        assert!(
            std::path::Path::new(&wt).exists(),
            "the removal ran before the record reached disk, which is the ordering \
             that makes an interrupted cleanup invisible to the next startup"
        );
        assert_eq!(world.task(id).await.status, TaskStatus::InProgress);
    }

    /// A terminal state that could not be written is reported, not published.
    #[tokio::test]
    async fn a_terminal_state_that_cannot_be_saved_is_never_published_as_success() {
        let world = world(true);
        let (id, _) = seed(&world, TaskStatus::InProgress, None).await;
        world.block_persistence();

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("an unwritable board cannot produce a successful terminalization");

        assert!(matches!(refusal, TerminalizeRefusal::NotRecorded(_)));
        assert_eq!(
            world.task(id).await.status,
            TaskStatus::InProgress,
            "shared memory must not show a state the file does not have"
        );
    }

    // -----------------------------------------------------------------------
    // Exclusion
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn an_automatic_terminalization_declines_a_busy_task_without_writing_anything() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;

        let held = world.authority.acquire(id).await.expect("free lease");

        // Stands for the merged-PR fact the executor asks to have recorded. It
        // is what makes declining consequential if it is written anyway: the
        // poll excludes any ref already marked `MERGED`, so persisting it on a
        // pass that did nothing would spend the task's one automatic
        // completion.
        let record_first = |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&id) {
                t.pr_url = Some("https://example.invalid/pull/7".to_string());
            }
        };
        let mut request = TerminalizeRequest::new(TaskStatus::Done);
        request.record_first = Some(&record_first);

        let refusal = terminalize(world.ctx(), id, Origin::Automatic, request)
            .await
            .expect_err("the lease is held elsewhere");

        assert!(matches!(refusal, TerminalizeRefusal::Busy(_)));
        assert!(
            std::path::Path::new(&wt).exists(),
            "declining must run no command at all"
        );
        let after = world.task(id).await;
        assert_eq!(after.status, TaskStatus::InProgress);
        assert!(
            !after.cleanup_in_flight,
            "declining must leave no trace, so a later pass finds the task exactly as \
             eligible as it was"
        );
        assert_eq!(
            after.pr_url, None,
            "and must not record the fact it was going to act on, which is what would \
             make the next pass skip this task forever"
        );
        assert_eq!(world.persisted(id).pr_url, None, "nor on disk");
        drop(held);

        // And once the lease is free, the very same automatic attempt works --
        // which is what makes declining harmless rather than a lost chance.
        let mut retry = TerminalizeRequest::new(TaskStatus::Done);
        retry.record_first = Some(&record_first);
        terminalize(world.ctx(), id, Origin::Automatic, retry)
            .await
            .expect("a free lease lets the same attempt through");
        assert_eq!(
            world.persisted(id).pr_url.as_deref(),
            Some("https://example.invalid/pull/7")
        );
    }

    struct AgentIsRunning;

    #[async_trait::async_trait]
    impl ExecutionOwnership for AgentIsRunning {
        async fn is_task_running(&self, _task_id: Uuid) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn terminalization_refuses_while_an_agent_owns_the_task_and_removes_nothing() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;

        let owner = AgentIsRunning;
        let mut ctx = world.ctx();
        ctx.running = Some(&owner);

        let refusal = terminalize(
            ctx,
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("a live agent's working directory must not be deleted under it");

        assert!(matches!(refusal, TerminalizeRefusal::ExecutionActive));
        assert!(std::path::Path::new(&wt).exists());
        let after = world.task(id).await;
        assert_eq!(after.status, TaskStatus::InProgress);
        assert!(!after.cleanup_in_flight);
    }

    // -----------------------------------------------------------------------
    // An unfinished restack of a published branch
    // -----------------------------------------------------------------------

    fn pending_restack() -> crate::domain::PendingRepublish {
        crate::domain::PendingRepublish {
            parent_branch: "task-parent".to_string(),
            parent_pr: 7,
            pr_number: Some(21),
            default_branch: "main".to_string(),
            fork_point: "a".repeat(40),
            previous_tip: "b".repeat(40),
            onto: "c".repeat(40),
            rewritten_tip: Some("d".repeat(40)),
        }
    }

    /// Every way into the destructive cleanup -- a merged pull request, a card
    /// moved to Done, an explicit cleanup, a delete -- is refused for a task
    /// whose restack is unfinished, and nothing is touched: the record, the
    /// checkout, the branch and the board are exactly as found.
    #[tokio::test]
    async fn a_task_with_an_unfinished_restack_is_never_terminalized_or_cleaned_up() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::PrCreated, Some(&wt)).await;
        let pending = pending_restack();
        {
            let mut tasks = world.tasks.write().await;
            tasks.get_mut(&id).unwrap().pending_republish = Some(pending.clone());
            let board: Vec<Task> = tasks.values().cloned().collect();
            world
                .storage
                .save_project_tasks(world.project_id, &board)
                .expect("persist the record");
        }
        let before = world.task(id).await;

        for origin in [Origin::Automatic, Origin::User] {
            let refusal = terminalize(
                world.ctx(),
                id,
                origin,
                TerminalizeRequest::new(TaskStatus::Done),
            )
            .await
            .expect_err("the restack's recovery needs this checkout and branch");
            assert!(
                matches!(refusal, TerminalizeRefusal::RepublishPending),
                "{origin:?}: {refusal}"
            );
        }
        let refusal = delete(world.ctx(), id)
            .await
            .expect_err("a delete removes the checkout too");
        assert!(matches!(refusal, TerminalizeRefusal::RepublishPending));

        assert!(std::path::Path::new(&wt).exists(), "the checkout survives");
        let after = world.task(id).await;
        assert_eq!(after.pending_republish, Some(pending));
        assert_eq!(after.status, before.status);
        assert_eq!(after.worktree_path, before.worktree_path);
        assert_eq!(after.branch_name, before.branch_name);
        assert!(!after.cleanup_in_flight, "no cleanup was announced");
        assert_eq!(
            world.persisted(id).pending_republish,
            after.pending_republish
        );
    }

    /// The guard is the task's own: a task with no restack pending, on the same
    /// board, terminalizes and is cleaned up as before.
    #[tokio::test]
    async fn an_ordinary_task_is_still_terminalized_and_cleaned_up() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::PrCreated, Some(&wt)).await;

        terminalize(
            world.ctx(),
            id,
            Origin::Automatic,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect("nothing is pending");

        assert!(!std::path::Path::new(&wt).exists());
        let after = world.task(id).await;
        assert_eq!(after.status, TaskStatus::Done);
        assert_eq!(after.worktree_path, None);
    }

    // -----------------------------------------------------------------------
    // Quarantine
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn an_automatic_terminalization_will_not_touch_an_interrupted_cleanup() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;
        world
            .tasks
            .write()
            .await
            .get_mut(&id)
            .unwrap()
            .cleanup_in_flight = true;

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::Automatic,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("nothing on disk says how far the interrupted removal got");

        assert!(matches!(refusal, TerminalizeRefusal::Quarantined(_)));
        assert!(std::path::Path::new(&wt).exists());
        assert!(
            world.task(id).await.cleanup_in_flight,
            "the quarantine survives an automatic attempt, so it cannot be cleared by one"
        );
    }

    /// An explicit request is the stated way back out of a quarantine, and what
    /// it triggers is the same safe removal as any other: it either succeeds or
    /// says why not.
    #[tokio::test]
    async fn an_explicit_request_may_resolve_an_interrupted_cleanup() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;
        world
            .tasks
            .write()
            .await
            .get_mut(&id)
            .unwrap()
            .cleanup_in_flight = true;

        let done = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect("an explicit request runs the ordinary safe removal");

        assert_eq!(done.status, TaskStatus::Done);
        assert!(!done.cleanup_in_flight);
        assert!(!std::path::Path::new(&wt).exists());
    }

    // -----------------------------------------------------------------------
    // Amendments
    // -----------------------------------------------------------------------

    /// What the caller has already established survives a refusal; what would
    /// only be true of a finished task does not.
    #[tokio::test]
    async fn a_refusal_keeps_what_was_recorded_first_and_withholds_what_success_would_add() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        std::fs::write(std::path::Path::new(&wt).join("unsaved.txt"), "x\n").unwrap();
        let (id, _) = seed(&world, TaskStatus::PrCreated, Some(&wt)).await;

        let record_first = |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&id) {
                t.pr_url = Some("https://example.invalid/pull/1".to_string());
            }
        };
        let on_success = |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&id) {
                t.overall_progress = 100;
                t.phase = TaskPhase::Complete;
            }
        };
        let mut request = TerminalizeRequest::new(TaskStatus::Done);
        request.record_first = Some(&record_first);
        request.on_success = Some(&on_success);

        terminalize(world.ctx(), id, Origin::Automatic, request)
            .await
            .expect_err("the checkout holds uncommitted work");

        let after = world.persisted(id);
        assert_eq!(
            after.pr_url.as_deref(),
            Some("https://example.invalid/pull/1"),
            "a fact established before the cleanup is true whatever git decided"
        );
        assert_eq!(
            after.status,
            TaskStatus::PrCreated,
            "and the task is not Done"
        );
        assert_eq!(after.overall_progress, 0);
        assert_ne!(after.phase, TaskPhase::Complete);
    }

    #[tokio::test]
    async fn success_applies_both_amendments_in_the_one_committed_write() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::PrCreated, Some(&wt)).await;

        let record_first = |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&id) {
                t.pr_url = Some("https://example.invalid/pull/1".to_string());
            }
        };
        let on_success = |staged: &mut HashMap<Uuid, Task>| {
            if let Some(t) = staged.get_mut(&id) {
                t.overall_progress = 100;
                t.phase = TaskPhase::Complete;
            }
        };
        let mut request = TerminalizeRequest::new(TaskStatus::Done);
        request.record_first = Some(&record_first);
        request.on_success = Some(&on_success);

        terminalize(world.ctx(), id, Origin::Automatic, request)
            .await
            .expect("a clean checkout is removable");

        let after = world.persisted(id);
        assert_eq!(after.status, TaskStatus::Done);
        assert_eq!(after.worktree_path, None);
        assert_eq!(after.overall_progress, 100);
        assert_eq!(after.phase, TaskPhase::Complete);
        assert_eq!(
            after.pr_url.as_deref(),
            Some("https://example.invalid/pull/1")
        );
    }

    // -----------------------------------------------------------------------
    // The lease itself
    // -----------------------------------------------------------------------

    // ===== record =====

    #[tokio::test]
    async fn a_record_the_file_refuses_is_not_published_to_the_board() {
        let world = world(true);
        let (id, _) = seed(&world, TaskStatus::InProgress, None).await;
        world.block_persistence();

        let note = |staged: &mut HashMap<Uuid, Task>| {
            if let Some(task) = staged.get_mut(&id) {
                task.pr_url = Some("https://example.invalid/pr/1".to_string());
                task.status = TaskStatus::PrCreated;
            }
        };
        let failure = record(&world.tasks, &world.storage, id, &note)
            .await
            .expect_err("a write the file refused is not a record");
        assert!(!failure.is_empty(), "and the caller has to be told why");

        // This is the whole point of the ordering. Publishing first left
        // `pr_url` and a moved status visible on the board and absent from the
        // file, which is the one state a restart silently undoes.
        let after = world.task(id).await;
        assert_eq!(
            after.pr_url, None,
            "nothing may be published that the file did not accept"
        );
        assert_eq!(
            after.status,
            TaskStatus::InProgress,
            "including the status it implied"
        );
    }

    /// A decision that finds the task no longer as it was shown makes no
    /// change, so it must not be a write: no file, no `updated_at`, and no
    /// failure even when the file could not be written at all.
    #[tokio::test]
    async fn a_conditional_record_that_declines_writes_nothing_and_reports_no_failure() {
        let world = world(true);
        let (id, sibling_id) = seed(&world, TaskStatus::InProgress, None).await;
        let before = (world.task(id).await, world.persisted(id));
        let before_sibling = world.persisted(sibling_id);

        let decline = |_: &mut HashMap<Uuid, Task>| false;
        let outcome = record_typed_if_changed(&world.tasks, &world.storage, id, &decline).await;

        assert_eq!(outcome, Ok(false));
        let after = world.task(id).await;
        assert_eq!(after.updated_at, before.0.updated_at);
        assert_eq!(
            serde_json::to_string(&after).unwrap(),
            serde_json::to_string(&before.0).unwrap()
        );
        assert_eq!(world.persisted(id).updated_at, before.1.updated_at);
        assert_eq!(
            world.persisted(sibling_id).updated_at,
            before_sibling.updated_at
        );

        // With the file unwritable, a decline is still not a failure: nothing
        // was attempted, so there is nothing to refuse.
        world.block_persistence();
        assert_eq!(
            record_typed_if_changed(&world.tasks, &world.storage, id, &decline).await,
            Ok(false),
            "a write was attempted although nothing changed"
        );
    }

    /// The typed result survives the conditional path: a save the file
    /// refused is `NotPublished` and leaves the board exactly as it was.
    #[tokio::test]
    async fn a_conditional_record_keeps_the_typed_write_result() {
        let world = world(true);
        let (id, _) = seed(&world, TaskStatus::InProgress, None).await;
        let before = world.task(id).await;

        let note = |staged: &mut HashMap<Uuid, Task>| {
            let Some(task) = staged.get_mut(&id) else {
                return false;
            };
            task.pr_url = Some("https://example.invalid/pr/1".to_string());
            true
        };
        assert_eq!(
            record_typed_if_changed(&world.tasks, &world.storage, id, &note).await,
            Ok(true)
        );
        assert!(world.task(id).await.updated_at > before.updated_at);
        assert_eq!(
            world.persisted(id).pr_url.as_deref(),
            Some("https://example.invalid/pr/1")
        );

        let settled = world.task(id).await;
        world.block_persistence();
        let failure = record_typed_if_changed(&world.tasks, &world.storage, id, &note).await;
        assert!(
            matches!(failure, Err(RecordWriteError::NotPublished(_))),
            "{failure:?}"
        );
        assert_eq!(
            serde_json::to_string(&world.task(id).await).unwrap(),
            serde_json::to_string(&settled).unwrap()
        );
    }

    #[tokio::test]
    async fn a_record_publishes_exactly_what_reached_the_disk() {
        let world = world(true);
        let (id, sibling_id) = seed(&world, TaskStatus::InProgress, None).await;

        let note = |staged: &mut HashMap<Uuid, Task>| {
            if let Some(task) = staged.get_mut(&id) {
                task.pr_url = Some("https://example.invalid/pr/1".to_string());
                task.status = TaskStatus::PrCreated;
            }
        };
        record(&world.tasks, &world.storage, id, &note)
            .await
            .expect("an ordinary record must succeed");

        let after = world.task(id).await;
        assert_eq!(
            after.pr_url.as_deref(),
            Some("https://example.invalid/pr/1")
        );
        assert_eq!(after.status, TaskStatus::PrCreated);
        assert_eq!(
            world.persisted(id).pr_url.as_deref(),
            Some("https://example.invalid/pr/1"),
            "the board and the file have to say the same thing"
        );
        assert_eq!(
            world.persisted(sibling_id).status,
            TaskStatus::Backlog,
            "and the rest of the project is written back unchanged rather than lost"
        );
    }

    /// A pull request already on disk is not lost when the terminalization that
    /// follows it cannot write its own result.
    ///
    /// The sequence the PR commands run: record the pull request, then
    /// terminalize under the same lease. Blocking persistence from the start
    /// only re-tests the record, which already reports its own failure. What
    /// this pins is the second half -- the record succeeded and is durable, and
    /// the write the terminalization needs is the one that fails.
    ///
    /// The stamp is what fails here, so nothing destructive ran and the
    /// checkout is still there. Asking again is an ordinary retry.
    #[tokio::test]
    async fn a_recorded_pull_request_survives_a_terminalization_that_cannot_be_stamped() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;

        let link = |staged: &mut HashMap<Uuid, Task>| {
            if let Some(task) = staged.get_mut(&id) {
                task.pr_url = Some("https://example.invalid/pull/7".to_string());
            }
        };
        record(&world.tasks, &world.storage, id, &link)
            .await
            .expect("the pull request itself records normally");
        assert_eq!(
            world.persisted(id).pr_url.as_deref(),
            Some("https://example.invalid/pull/7"),
            "the pull request has to be durable before the interesting part begins"
        );

        // Only now does the storage break, so the failure belongs to the
        // terminalization rather than to the record.
        world.block_persistence();

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("a terminal write that failed is not a terminalization");
        assert!(
            matches!(refusal, TerminalizeRefusal::NotRecorded(_)),
            "a failed write has to be reported as one, not as a refusal to write: {refusal:?}"
        );

        let after = world.task(id).await;
        assert_ne!(
            after.status,
            TaskStatus::Done,
            "and nothing may claim it finished"
        );
        assert_eq!(
            after.pr_url.as_deref(),
            Some("https://example.invalid/pull/7"),
            "the durable pull request is not rolled back by a later failure"
        );
        assert!(
            !after.cleanup_in_flight,
            "a cleanup that was never announced was never begun"
        );
        assert_eq!(
            after.worktree_path.as_deref(),
            Some(wt.as_str()),
            "the checkout keeps the only handle back to it"
        );
        assert!(
            std::path::Path::new(&wt).exists(),
            "and nothing destructive ran, so asking again is an ordinary retry"
        );
    }

    /// The same guarantee where the failing write is the final one rather than
    /// the stamp.
    ///
    /// A task with no checkout has nothing to remove, so `terminalize_leased`
    /// goes straight to the commit. That is the other write that can fail after
    /// the pull request is already durable, and it must not report success
    /// either.
    #[tokio::test]
    async fn a_recorded_pull_request_survives_a_terminal_commit_that_cannot_be_saved() {
        let world = world(true);
        let (id, sibling_id) = seed(&world, TaskStatus::InProgress, None).await;

        let link = |staged: &mut HashMap<Uuid, Task>| {
            if let Some(task) = staged.get_mut(&id) {
                task.pr_url = Some("https://example.invalid/pull/9".to_string());
            }
        };
        record(&world.tasks, &world.storage, id, &link)
            .await
            .expect("the pull request itself records normally");

        world.block_persistence();

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("a terminal write that failed is not a terminalization");
        assert!(
            matches!(refusal, TerminalizeRefusal::NotRecorded(_)),
            "the commit is a write, and a failed write is reported as one: {refusal:?}"
        );

        let after = world.task(id).await;
        assert_ne!(after.status, TaskStatus::Done);
        assert_eq!(
            after.pr_url.as_deref(),
            Some("https://example.invalid/pull/9"),
            "the durable pull request survives the failure that followed it"
        );
        assert_eq!(
            world.task(sibling_id).await.status,
            TaskStatus::Backlog,
            "and the rest of the project was never republished by the failed attempt"
        );
    }

    // ===== delete =====

    #[tokio::test]
    async fn a_delete_whose_cleanup_git_refuses_keeps_the_task_and_everything_it_names() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        // Uncommitted, non-ignored work: git refuses this checkout through the
        // ordinary production path, and refusing is the whole point.
        std::fs::write(
            std::path::Path::new(&wt).join("unsaved.txt"),
            "not committed\n",
        )
        .unwrap();
        let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;

        let refusal = delete(world.ctx(), id)
            .await
            .expect_err("a delete whose cleanup was refused is not a delete");
        assert!(
            matches!(refusal, TerminalizeRefusal::CleanupRefused { .. }),
            "the refusal has to be git's, carried through: {refusal:?}"
        );

        // The record is the only thing that names this checkout and this
        // branch. Deleting it after a refusal is what strands the directory
        // with nothing able to find it again.
        let after = world.task(id).await;
        assert_eq!(after.worktree_path.as_deref(), Some(wt.as_str()));
        assert_eq!(after.branch_name.as_deref(), Some("task-abcd1234"));
        assert_eq!(
            world.persisted(id).worktree_path.as_deref(),
            Some(wt.as_str()),
            "and the file has to agree, because the file is what the next start reads"
        );
        assert!(
            std::path::Path::new(&wt).join("unsaved.txt").exists(),
            "the work the refusal was protecting must survive"
        );
    }

    /// A checkout whose directory is gone while git still holds a locked
    /// registration of it is not cleaned up: finishing or deleting the task
    /// keeps it, and its reference, until the user unlocks it. Then both
    /// converge.
    #[tokio::test]
    async fn a_missing_checkout_git_holds_locked_is_kept_until_it_is_unlocked() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let reason = "on a drive that is not mounted";
        git(
            std::path::Path::new(&repo_path(&world)),
            &["worktree", "lock", "--reason", reason, &wt],
        );
        std::fs::remove_dir_all(&wt).unwrap();
        let (id, _) = seed(&world, TaskStatus::HumanReview, Some(&wt)).await;

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("git still holds the branch in the locked registration");
        let TerminalizeRefusal::CleanupRefused { reason: why, .. } = &refusal else {
            panic!("expected a cleanup refusal, got {refusal:?}");
        };
        assert!(why.contains(reason), "the lock is named: {why}");
        let refusal = delete(world.ctx(), id)
            .await
            .expect_err("nor is it a delete");
        assert!(matches!(refusal, TerminalizeRefusal::CleanupRefused { .. }));

        let after = world.task(id).await;
        assert_eq!(
            after.status,
            TaskStatus::HumanReview,
            "the status must not move"
        );
        assert_eq!(after.worktree_path.as_deref(), Some(wt.as_str()));
        assert!(after
            .error_message
            .as_deref()
            .unwrap_or_default()
            .contains(reason));
        assert_eq!(
            world.persisted(id).worktree_path.as_deref(),
            Some(wt.as_str())
        );

        git(
            std::path::Path::new(&repo_path(&world)),
            &["worktree", "unlock", &wt],
        );
        let done = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect("once unlocked, the missing checkout's registration is cleared");
        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(world.persisted(id).worktree_path, None);
    }

    /// A checkout of `world`'s repository detached at a commit no ref names,
    /// and that commit.
    async fn detached_checkout_with_unique_commit(world: &World) -> (String, String) {
        let wt = worktree_with_committed_work(world, "task-abcd1234").await;
        let dir = std::path::PathBuf::from(&wt);
        git(&dir, &["checkout", "-q", "--detach"]);
        std::fs::write(
            dir.join("detached.txt"),
            "only the detached HEAD names this\n",
        )
        .unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "detached work"]);
        let commit = git_output(&dir, &["rev-parse", "HEAD"]);
        let refs = git_output(
            std::path::Path::new(&repo_path(world)),
            &["for-each-ref", "--contains", &commit],
        );
        assert!(refs.is_empty(), "no ref names it yet: {refs}");
        (wt, commit)
    }

    fn git_output(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be spawnable");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// What finishing or deleting a task whose checkout is detached at a
    /// commit only that `HEAD` names must leave: the task, its status, its
    /// checkout reference, on the board and on disk, an error naming the
    /// commit, and the commit still reachable through the registration.
    async fn assert_detached_work_kept(world: &World, id: Uuid, wt: &str, commit: &str) {
        let after = world.task(id).await;
        assert_eq!(
            after.status,
            TaskStatus::HumanReview,
            "the status must not move"
        );
        assert_eq!(after.worktree_path.as_deref(), Some(wt));
        assert!(after.branch_name.is_some(), "the branch is still recorded");
        assert!(!after.cleanup_in_flight, "the cleanup lease is released");
        assert!(
            after
                .error_message
                .as_deref()
                .unwrap_or_default()
                .contains(commit),
            "the task says which commit: {:?}",
            after.error_message
        );
        assert_eq!(world.persisted(id).worktree_path.as_deref(), Some(wt));
        let repo = std::path::PathBuf::from(repo_path(world));
        assert!(
            git_output(&repo, &["worktree", "list", "--porcelain"])
                .contains(&format!("HEAD {commit}")),
            "the registration, and the HEAD it keeps, are left alone"
        );
        git(
            &repo,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("{commit}^{{commit}}"),
            ],
        );
        assert!(!git_output(&repo, &["fsck", "--unreachable", "--no-reflogs"]).contains(commit));
    }

    #[tokio::test]
    async fn finishing_and_deleting_a_task_with_a_present_detached_unique_commit_keep_it() {
        let world = world(true);
        let (wt, commit) = detached_checkout_with_unique_commit(&world).await;
        let (id, _) = seed(&world, TaskStatus::HumanReview, Some(&wt)).await;

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("removing the checkout would make its only commit unreachable");
        assert!(
            matches!(refusal, TerminalizeRefusal::CleanupRefused { .. }),
            "{refusal:?}"
        );
        assert!(std::path::Path::new(&wt).is_dir());
        assert_detached_work_kept(&world, id, &wt, &commit).await;

        let refusal = delete(world.ctx(), id)
            .await
            .expect_err("nor is deleting the task");
        assert!(
            matches!(refusal, TerminalizeRefusal::CleanupRefused { .. }),
            "{refusal:?}"
        );
        assert!(
            world.tasks.read().await.contains_key(&id),
            "the task is not deleted"
        );
        assert_eq!(world.persisted(id).id, id, "and neither is its record");
        assert_detached_work_kept(&world, id, &wt, &commit).await;

        git(
            std::path::Path::new(&repo_path(&world)),
            &["branch", "kept-work", &commit],
        );
        let done = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect("once the commit is named, finishing goes through");
        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(world.persisted(id).worktree_path, None);
    }

    #[tokio::test]
    async fn finishing_and_deleting_a_task_with_a_missing_detached_unique_commit_keep_it() {
        let world = world(true);
        let (wt, commit) = detached_checkout_with_unique_commit(&world).await;
        std::fs::remove_dir_all(&wt).unwrap();
        let (id, _) = seed(&world, TaskStatus::HumanReview, Some(&wt)).await;

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("clearing the registration would make its only commit unreachable");
        assert!(
            matches!(refusal, TerminalizeRefusal::CleanupRefused { .. }),
            "{refusal:?}"
        );
        assert_detached_work_kept(&world, id, &wt, &commit).await;

        let refusal = delete(world.ctx(), id)
            .await
            .expect_err("nor is deleting the task");
        assert!(
            matches!(refusal, TerminalizeRefusal::CleanupRefused { .. }),
            "{refusal:?}"
        );
        assert!(
            world.tasks.read().await.contains_key(&id),
            "the task is not deleted"
        );
        assert_detached_work_kept(&world, id, &wt, &commit).await;

        git(
            std::path::Path::new(&repo_path(&world)),
            &["tag", "kept-work", &commit],
        );
        assert!(delete(world.ctx(), id)
            .await
            .expect("once named, the delete goes through"));
        assert!(!world.tasks.read().await.contains_key(&id));
    }

    #[tokio::test]
    async fn finishing_and_deleting_a_task_whose_checkout_reflog_alone_reaches_a_commit_keep_it() {
        let world = world(true);
        let (wt, first) = detached_checkout_with_unique_commit(&world).await;
        let repo = std::path::PathBuf::from(repo_path(&world));
        // Move the checkout onto a commit a branch names: the commit it made
        // is now reached only by the checkout's HEAD reflog.
        git(&repo, &["branch", "elsewhere", "HEAD"]);
        git(
            std::path::Path::new(&wt),
            &["checkout", "-q", "--detach", "elsewhere"],
        );
        let (id, _) = seed(&world, TaskStatus::HumanReview, Some(&wt)).await;

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("removing the checkout would delete the reflog that reaches the commit");
        assert!(
            matches!(refusal, TerminalizeRefusal::CleanupRefused { .. }),
            "{refusal:?}"
        );
        let refusal = delete(world.ctx(), id)
            .await
            .expect_err("nor is deleting the task");
        assert!(
            matches!(refusal, TerminalizeRefusal::CleanupRefused { .. }),
            "{refusal:?}"
        );
        assert!(
            world.tasks.read().await.contains_key(&id),
            "the task is not deleted"
        );
        assert!(std::path::Path::new(&wt).is_dir());
        assert_eq!(
            world.persisted(id).worktree_path.as_deref(),
            Some(wt.as_str())
        );
        assert!(!git_output(&repo, &["fsck", "--unreachable"]).contains(&first));
    }

    #[tokio::test]
    async fn finishing_and_deleting_a_task_in_a_reftable_repository_keep_it() {
        let world = world(true);
        let repo = std::path::PathBuf::from(repo_path(&world));
        let migrated = std::process::Command::new("git")
            .args(["refs", "migrate", "--ref-format=reftable"])
            .current_dir(&repo)
            .output()
            .is_ok_and(|o| o.status.success());
        if !migrated {
            eprintln!("skipped: this git cannot use the reftable backend");
            return;
        }
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::HumanReview, Some(&wt)).await;

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("the checkout's reflog cannot be inspected under reftable");
        assert!(
            matches!(refusal, TerminalizeRefusal::CleanupRefused { .. }),
            "{refusal:?}"
        );
        let refusal = delete(world.ctx(), id)
            .await
            .expect_err("nor is deleting the task");
        assert!(
            matches!(refusal, TerminalizeRefusal::CleanupRefused { .. }),
            "{refusal:?}"
        );
        let after = world.task(id).await;
        assert_eq!(after.status, TaskStatus::HumanReview);
        assert!(world.tasks.read().await.contains_key(&id));
        assert!(!after.cleanup_in_flight);
        assert!(std::path::Path::new(&wt).is_dir());
        assert_eq!(
            world.persisted(id).worktree_path.as_deref(),
            Some(wt.as_str())
        );
        assert!(
            after
                .error_message
                .as_deref()
                .unwrap_or_default()
                .contains("reftable"),
            "{:?}",
            after.error_message
        );
        assert!(git_output(&repo, &["worktree", "list", "--porcelain"]).contains(&wt));
    }

    #[tokio::test]
    async fn finishing_and_deleting_a_task_whose_checkout_git_cannot_read_keep_it() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        git(std::path::Path::new(&wt), &["checkout", "-q", "--detach"]);
        std::fs::write(
            std::path::Path::new(&wt).join(".git"),
            "not a gitdir pointer\n",
        )
        .unwrap();
        let (id, _) = seed(&world, TaskStatus::HumanReview, Some(&wt)).await;

        let refusal = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect_err("safety could not be established");
        assert!(
            matches!(refusal, TerminalizeRefusal::CleanupRefused { .. }),
            "{refusal:?}"
        );
        let refusal = delete(world.ctx(), id)
            .await
            .expect_err("nor is deleting the task");
        assert!(
            matches!(refusal, TerminalizeRefusal::CleanupRefused { .. }),
            "{refusal:?}"
        );
        let after = world.task(id).await;
        assert_eq!(after.status, TaskStatus::HumanReview);
        assert!(world.tasks.read().await.contains_key(&id));
        assert!(!after.cleanup_in_flight);
        assert!(std::path::Path::new(&wt).is_dir());
        assert_eq!(
            world.persisted(id).worktree_path.as_deref(),
            Some(wt.as_str())
        );
        assert!(
            after
                .error_message
                .as_deref()
                .unwrap_or_default()
                .contains("could not be read"),
            "the underlying reason is given: {:?}",
            after.error_message
        );
    }

    #[tokio::test]
    async fn a_detached_checkout_whose_commit_a_branch_names_is_finished_as_usual() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        git(std::path::Path::new(&wt), &["checkout", "-q", "--detach"]);
        let (id, _) = seed(&world, TaskStatus::HumanReview, Some(&wt)).await;

        let done = terminalize(
            world.ctx(),
            id,
            Origin::User,
            TerminalizeRequest::new(TaskStatus::Done),
        )
        .await
        .expect("the task branch names the commit, so nothing is lost");
        assert_eq!(done.status, TaskStatus::Done);
        assert!(!std::path::Path::new(&wt).exists());
    }

    #[tokio::test]
    async fn a_task_whose_autosquash_rebase_completed_is_finished_and_deleted_as_usual() {
        for delete_it in [false, true] {
            let world = world(true);
            let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
            let dir = std::path::PathBuf::from(&wt);
            for (file, fixup) in [("work.txt", "HEAD"), ("other.txt", "HEAD")] {
                std::fs::write(dir.join(file), "changed\n").unwrap();
                git(&dir, &["add", "."]);
                git(&dir, &["commit", "-q", "--fixup", fixup]);
            }
            let rebased = std::process::Command::new("git")
                .args(["rebase", "-q", "-i", "--autosquash", "main"])
                .env("GIT_SEQUENCE_EDITOR", "true")
                .env("GIT_EDITOR", "true")
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(
                rebased.status.success(),
                "{}",
                String::from_utf8_lossy(&rebased.stderr)
            );
            let (id, _) = seed(&world, TaskStatus::HumanReview, Some(&wt)).await;

            if delete_it {
                delete(world.ctx(), id)
                    .await
                    .expect("only a finished rebase's steps are left");
                assert!(!world.tasks.read().await.contains_key(&id));
            } else {
                let done = terminalize(
                    world.ctx(),
                    id,
                    Origin::User,
                    TerminalizeRequest::new(TaskStatus::Done),
                )
                .await
                .expect("only a finished rebase's steps are left");
                assert_eq!(done.status, TaskStatus::Done);
            }
            assert!(!std::path::Path::new(&wt).exists());
        }
    }

    #[tokio::test]
    async fn a_delete_refuses_while_an_agent_owns_the_task_and_removes_nothing() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;

        let running = AgentIsRunning;
        let mut ctx = world.ctx();
        ctx.running = Some(&running);

        let refusal = delete(ctx, id)
            .await
            .expect_err("the checkout is the agent's working directory");
        assert!(
            matches!(refusal, TerminalizeRefusal::ExecutionActive),
            "expected an execution refusal, got {refusal:?}"
        );

        assert!(
            world.tasks.read().await.contains_key(&id),
            "the record of a task an agent is running may not be removed out from under it"
        );
        assert!(
            std::path::Path::new(&wt).exists(),
            "and nothing may be deleted from the directory it is working in"
        );
    }

    #[tokio::test]
    async fn a_delete_whose_cleanup_succeeded_removes_the_task_from_memory_and_disk() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, sibling_id) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;

        assert!(delete(world.ctx(), id)
            .await
            .expect("the delete must succeed"));

        assert!(!world.tasks.read().await.contains_key(&id));
        assert!(
            !world
                .storage
                .load_project_tasks(world.project_id)
                .expect("the board must be readable")
                .iter()
                .any(|t| t.id == id),
            "a delete the file did not accept is one the next start undoes"
        );
        assert!(
            !std::path::Path::new(&wt).exists(),
            "the checkout goes with it"
        );
        let branches = std::process::Command::new("git")
            .args(["branch", "--list", "task-abcd1234"])
            .current_dir(repo_path(&world))
            .output()
            .expect("git branch --list");
        assert!(
            !String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "but not the branch, so what the task committed stays reachable"
        );
        assert!(
            world.tasks.read().await.contains_key(&sibling_id),
            "and nothing else in the project moves"
        );
    }

    #[tokio::test]
    async fn a_delete_whose_last_write_fails_is_not_reported_as_done_and_can_be_retried() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;

        // The cleanup succeeds and its outcome is recorded; only the write that
        // takes the task off the board is made to fail. Blocking persistence
        // any earlier would refuse before anything destructive ran, which is a
        // different contract and already covered.
        assert!(delete(world.ctx(), id)
            .await
            .expect("the first delete succeeds"));
        let (id2, _) = seed(&world, TaskStatus::InProgress, None).await;
        world.block_persistence();

        let refusal = delete(world.ctx(), id2)
            .await
            .expect_err("a delete the file did not accept is not a delete");
        assert!(
            matches!(refusal, TerminalizeRefusal::NotRecorded(_)),
            "expected the board write to be reported, got {refusal:?}"
        );
        assert!(
            world.tasks.read().await.contains_key(&id2),
            "the record has to survive, because a task the user is still shown is a task they \
             can delete again"
        );
    }

    /// The board file already holds a change whose directory sync failed. Memory
    /// must follow the file: the next save, of any Task in the Project, writes
    /// the whole board from memory and would otherwise undo the change.
    #[tokio::test]
    async fn a_board_published_but_not_durable_is_adopted_so_the_next_save_cannot_undo_it() {
        use slashit_durable_fs::{fault, Stage};
        let world = world(false);
        let (id, sibling_id) = seed(&world, TaskStatus::Backlog, None).await;

        let fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
        let error = record(&world.tasks, &world.storage, id, &|staged| {
            staged.get_mut(&id).unwrap().title = "renamed".into();
        })
        .await
        .expect_err("the caller is told the durability is unconfirmed");
        drop(fault);
        assert!(error.contains("not confirmed durable"), "{error}");
        assert_eq!(
            world.tasks.read().await[&id].title,
            "renamed",
            "memory follows the visible file"
        );
        let on_disk = world.storage.load_project_tasks(world.project_id).unwrap();
        assert_eq!(
            on_disk.iter().find(|t| t.id == id).unwrap().title,
            "renamed"
        );

        // An unrelated save rewrites the whole board from memory.
        record(&world.tasks, &world.storage, sibling_id, &|staged| {
            staged.get_mut(&sibling_id).unwrap().title = "sibling renamed".into();
        })
        .await
        .expect("the next save completes");
        let on_disk = world.storage.load_project_tasks(world.project_id).unwrap();
        assert_eq!(
            on_disk.iter().find(|t| t.id == id).unwrap().title,
            "renamed",
            "the earlier change survives"
        );
        assert_eq!(
            on_disk.iter().find(|t| t.id == sibling_id).unwrap().title,
            "sibling renamed"
        );
    }

    /// A create whose directory sync failed has still put the task on the
    /// board and in the file: the error must say so, or a caller that retries
    /// with the fresh id it builds per attempt duplicates the task. It also
    /// covers a NotPublished failure, which must stay silent about a board it
    /// never reached.
    #[tokio::test]
    async fn a_create_that_published_but_could_not_confirm_durable_tells_the_caller_so() {
        use slashit_durable_fs::{fault, Stage};
        let world = world(false);
        let mut created_id: Option<Uuid> = None;

        let fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
        let error = create(
            &world.gate,
            &world.tasks,
            &world.projects,
            &world.storage,
            world.project_id,
            |_| {
                let mut t = crate::test_helpers::create_test_task("Created");
                t.project_id = world.project_id;
                created_id = Some(t.id);
                t
            },
        )
        .await
        .expect_err("the caller is told the durability is unconfirmed");
        drop(fault);
        assert!(
            error.contains("in place but not confirmed durable"),
            "{error}"
        );
        assert!(error.contains("makes a duplicate"), "{error}");
        assert!(error.contains("nothing survived"), "{error}");
        let id = created_id.expect("the created id");
        assert_eq!(
            world.tasks.read().await[&id].title,
            "Created",
            "the task is on the board"
        );
        assert_eq!(world.tasks.read().await.len(), 1);
        let on_disk = world.storage.load_project_tasks(world.project_id).unwrap();
        assert_eq!(
            on_disk.len(),
            1,
            "the whole board is the one published task"
        );
        assert_eq!(on_disk[0].id, id, "the file holds the task");

        // A NotPublished failure leaves the map and the file as they were, and
        // says nothing about a task in place.
        world.block_persistence();
        let error = create(
            &world.gate,
            &world.tasks,
            &world.projects,
            &world.storage,
            world.project_id,
            |_| {
                let mut t = crate::test_helpers::create_test_task("Interrupted");
                t.project_id = world.project_id;
                t
            },
        )
        .await
        .expect_err("nothing could be written");
        assert!(error.contains("cannot be used as a board"), "{error}");
        assert!(!error.contains("makes a duplicate"), "{error}");
        assert_eq!(
            world.tasks.read().await.len(),
            1,
            "the refused create left the board as it was"
        );
    }

    /// Same for a batch import: the whole batch is in place after a
    /// published-but-unconfirmed save, so the error tells the caller that a
    /// retry reimports it as a duplicate batch.
    #[tokio::test]
    async fn a_create_batch_that_published_but_could_not_confirm_durable_tells_the_caller_so() {
        use slashit_durable_fs::{fault, Stage};
        let world = world(false);
        let batch: Vec<Task> = ["One", "Two"]
            .iter()
            .map(|title| {
                let mut t = crate::test_helpers::create_test_task(title);
                t.project_id = world.project_id;
                t
            })
            .collect();

        let fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
        let error = create_batch(
            &world.gate,
            &world.tasks,
            &world.projects,
            &world.storage,
            world.project_id,
            &batch,
        )
        .await
        .expect_err("the caller is told the durability is unconfirmed");
        drop(fault);
        assert!(
            error.contains("in place but not confirmed durable"),
            "{error}"
        );
        assert!(error.contains("batch of tasks is in place"), "{error}");
        {
            let board = world.tasks.read().await;
            assert_eq!(board.len(), batch.len(), "every batch task is on the board");
            for task in &batch {
                assert!(&board.contains_key(&task.id));
            }
        }
        let on_disk = world.storage.load_project_tasks(world.project_id).unwrap();
        assert_eq!(on_disk.len(), batch.len(), "the file holds the batch");
    }

    /// The message names what actually happened to the task's cleanup flag.
    #[tokio::test]
    async fn an_unconfirmed_cleanup_flag_is_reported_as_saved_not_as_not_started() {
        use slashit_durable_fs::{fault, Stage};
        let world = world(false);
        let (id, _) = seed(&world, TaskStatus::Backlog, None).await;
        let _fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
        let request = TerminalizeRequest::new(TaskStatus::Backlog);
        let error =
            stamp_cleanup_in_flight(&world.tasks, &world.storage, id, world.project_id, &request)
                .await
                .unwrap_err();
        assert!(
            error.contains("flagged as having a cleanup in flight"),
            "{error}"
        );
        assert!(
            world.tasks.read().await[&id].cleanup_in_flight,
            "memory follows the file"
        );
    }

    /// A write that did not replace the file leaves memory and file as they were.
    #[tokio::test]
    async fn a_board_write_that_never_published_changes_nothing_in_memory() {
        use slashit_durable_fs::{fault, Stage};
        let world = world(false);
        let (id, _) = seed(&world, TaskStatus::Backlog, None).await;
        let before = world.tasks.read().await[&id].title.clone();

        let _fault = fault::fail_at(Stage::Replace, std::io::ErrorKind::Other);
        record(&world.tasks, &world.storage, id, &|staged| {
            staged.get_mut(&id).unwrap().title = "renamed".into();
        })
        .await
        .expect_err("nothing was written");
        assert_eq!(world.tasks.read().await[&id].title, before);
        let on_disk = world.storage.load_project_tasks(world.project_id).unwrap();
        assert_eq!(on_disk.iter().find(|t| t.id == id).unwrap().title, before);
    }

    /// The delete reached the file; only the sync failed. The Task must not come
    /// back from memory on the next save, and the message must not claim the
    /// Task is "still there".
    #[tokio::test]
    async fn a_delete_the_file_accepted_but_could_not_confirm_is_not_undone_by_the_next_save() {
        use slashit_durable_fs::{fault, Stage};
        let world = world(false);
        let (id, sibling_id) = seed(&world, TaskStatus::Backlog, None).await;

        // A delete makes two board writes: the terminalization record, then the
        // removal. Let the first through and fail the second one's sync.
        let fault = fault::fail_after(Stage::SyncDirectory, std::io::ErrorKind::Other, 1);
        let refusal = delete(world.ctx(), id)
            .await
            .expect_err("an unconfirmed delete is reported");
        drop(fault);
        let TerminalizeRefusal::NotRecorded(message) = refusal else {
            panic!("{refusal:?}")
        };
        assert!(message.contains("may reappear"), "{message}");
        assert!(!message.contains("still there"), "{message}");
        assert!(
            !world.tasks.read().await.contains_key(&id),
            "memory follows the file"
        );
        assert!(!world
            .storage
            .load_project_tasks(world.project_id)
            .unwrap()
            .iter()
            .any(|t| t.id == id));

        record(&world.tasks, &world.storage, sibling_id, &|staged| {
            staged.get_mut(&sibling_id).unwrap().title = "sibling renamed".into();
        })
        .await
        .expect("the next save completes");
        assert!(
            !world
                .storage
                .load_project_tasks(world.project_id)
                .unwrap()
                .iter()
                .any(|t| t.id == id),
            "the deleted Task is not written back"
        );
    }

    fn integration_record(
        state: crate::domain::IntegrationState,
    ) -> crate::domain::LocalIntegration {
        crate::domain::LocalIntegration {
            state,
            target_branch: "main".into(),
            target_old_oid: "a".repeat(40),
            approved_tip: "b".repeat(40),
            result_oid: "c".repeat(40),
            strategy: crate::domain::IntegrationStrategy::FastForward,
            checkout_path: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            completes_task: false,
        }
    }

    const EVERY_STATUS: [TaskStatus; 8] = [
        TaskStatus::Backlog,
        TaskStatus::Queue,
        TaskStatus::InProgress,
        TaskStatus::AiReview,
        TaskStatus::HumanReview,
        TaskStatus::Done,
        TaskStatus::PrCreated,
        TaskStatus::Error,
    ];

    async fn persist_board(world: &World) {
        let board: Vec<Task> = world.tasks.read().await.values().cloned().collect();
        world
            .storage
            .save_project_tasks(world.project_id, &board)
            .expect("persist the record");
    }

    /// A pending local integration is the only record of a Git operation that
    /// may already have moved its target. Whatever status the Task is in
    /// (including the ones where no Resume is offered), deleting it is refused
    /// before anything is touched: record, checkout and branch survive, in
    /// memory and on disk.
    #[tokio::test]
    async fn a_task_with_a_pending_integration_is_never_deleted_in_any_status() {
        for status in EVERY_STATUS {
            let world = world(true);
            let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
            let (id, sibling) = seed(&world, status.clone(), Some(&wt)).await;
            let record = integration_record(crate::domain::IntegrationState::Pending);
            world
                .tasks
                .write()
                .await
                .get_mut(&id)
                .unwrap()
                .local_integration = Some(record.clone());
            persist_board(&world).await;

            let refusal = delete(world.ctx(), id)
                .await
                .expect_err("the record must not be destroyed");

            assert!(
                matches!(refusal, TerminalizeRefusal::DeleteBlockedByIntegration),
                "{status:?}: {refusal:?}"
            );
            for after in [world.task(id).await, world.persisted(id)] {
                assert_eq!(after.local_integration, Some(record.clone()), "{status:?}");
                assert_eq!(
                    after.worktree_path.as_deref(),
                    Some(wt.as_str()),
                    "{status:?}"
                );
                assert!(
                    !after.cleanup_in_flight,
                    "{status:?}: nothing was announced"
                );
            }
            assert!(
                std::path::Path::new(&wt).exists(),
                "{status:?}: the checkout survives"
            );
            assert!(world.tasks.read().await.contains_key(&sibling));
        }
    }

    /// The same for an unfinished restack, in every status.
    #[tokio::test]
    async fn a_task_with_a_pending_restack_is_never_deleted_in_any_status() {
        for status in EVERY_STATUS {
            let world = world(true);
            let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
            let (id, _) = seed(&world, status.clone(), Some(&wt)).await;
            world
                .tasks
                .write()
                .await
                .get_mut(&id)
                .unwrap()
                .pending_republish = Some(pending_restack());
            persist_board(&world).await;

            let refusal = delete(world.ctx(), id)
                .await
                .expect_err("the restack's recovery needs it");

            assert!(
                matches!(refusal, TerminalizeRefusal::RepublishPending),
                "{status:?}: {refusal:?}"
            );
            assert!(
                world.persisted(id).pending_republish.is_some(),
                "{status:?}"
            );
            assert!(std::path::Path::new(&wt).exists(), "{status:?}");
        }
    }

    /// Only a *pending* integration blocks. One that finished holds nothing
    /// owed, and the Project-deletion predicate's other reasons (an ordinary
    /// checkout, an interrupted cleanup) are exactly what a Task delete is for.
    #[tokio::test]
    async fn a_finished_integration_an_ordinary_checkout_and_an_interrupted_cleanup_do_not_block_a_delete(
    ) {
        for (label, prepare) in [
            (
                "integrated",
                Some(integration_record(
                    crate::domain::IntegrationState::Integrated,
                )),
            ),
            ("ordinary checkout", None),
        ] {
            let world = world(true);
            let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
            let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;
            world
                .tasks
                .write()
                .await
                .get_mut(&id)
                .unwrap()
                .local_integration = prepare;
            assert!(
                delete(world.ctx(), id)
                    .await
                    .unwrap_or_else(|e| panic!("{label}: {e}")),
                "{label}"
            );
            assert!(!std::path::Path::new(&wt).exists(), "{label}");
        }

        // The retry path for a removal that was interrupted: still a delete.
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;
        world
            .tasks
            .write()
            .await
            .get_mut(&id)
            .unwrap()
            .cleanup_in_flight = true;
        assert!(delete(world.ctx(), id)
            .await
            .expect("an explicit delete retries the cleanup"));
        assert!(!std::path::Path::new(&wt).exists());
        assert!(!world.tasks.read().await.contains_key(&id));
    }

    /// Project deletion keeps its own, wider predicate: it cannot clean a
    /// checkout, so it still refuses one that Task deletion handles.
    #[test]
    fn task_and_project_deletion_share_the_recovery_reasons_but_not_the_checkout_one() {
        let mut task = create_test_task_full("t", Uuid::new_v4(), TaskStatus::InProgress, 0);
        task.worktree_path = Some("/somewhere".into());
        assert!(task_deletion_refusal(&task).is_none());
        assert!(project_deletion_refusal(&task).is_some());
        task.local_integration = Some(integration_record(crate::domain::IntegrationState::Pending));
        assert!(task_deletion_refusal(&task).is_some());
        assert!(project_deletion_refusal(&task).is_some());
    }

    #[tokio::test]
    async fn a_delete_of_a_task_that_is_not_there_is_not_a_failure() {
        let world = world(true);
        assert!(!delete(world.ctx(), Uuid::new_v4())
            .await
            .expect("asking for something already gone is not an error"),);
    }

    #[tokio::test]
    async fn the_lease_is_exclusive_per_task_and_free_between_tasks() {
        let locks = TaskLifecycleLocks::new();
        let one = Uuid::new_v4();
        let two = Uuid::new_v4();

        let held = locks.acquire(one).await.expect("first acquisition");
        assert!(
            locks.try_acquire(one).await.is_none(),
            "a second holder for the same task must be refused"
        );
        assert!(
            locks.try_acquire(two).await.is_some(),
            "an unrelated task must not be blocked by it"
        );
        drop(held);
        assert!(
            locks.try_acquire(one).await.is_some(),
            "dropping the guard must release the task"
        );
    }

    /// The guard is released by unwinding, which a manually-removed entry in a
    /// set is not. This is the whole reason for an owned guard: a panic inside a
    /// lifecycle operation used to strand the task forever.
    #[tokio::test]
    async fn a_panic_while_holding_the_lease_still_releases_it() {
        let locks = Arc::new(TaskLifecycleLocks::new());
        let task_id = Uuid::new_v4();

        let panicking = {
            let locks = locks.clone();
            tokio::spawn(async move {
                let _lease = locks.acquire(task_id).await.expect("lease");
                panic!("a lifecycle operation failing the hard way");
            })
        };
        assert!(panicking.await.is_err(), "the task must have panicked");

        assert!(
            locks.try_acquire(task_id).await.is_some(),
            "the lease must not survive the panic that held it"
        );
    }

    #[tokio::test]
    async fn a_user_acquisition_gives_up_rather_than_waiting_forever() {
        let locks = TaskLifecycleLocks::new();
        let task_id = Uuid::new_v4();
        let _held = locks.acquire(task_id).await.expect("first acquisition");

        // Time is paused, so this asserts the bound exists without spending it.
        tokio::time::pause();
        let waiting = tokio::time::timeout(
            ACQUIRE_TIMEOUT + Duration::from_secs(1),
            locks.acquire(task_id),
        )
        .await;

        match waiting {
            Ok(Err(message)) => assert!(
                message.contains("nothing was changed"),
                "a timeout must say plainly that nothing was attempted: {message}"
            ),
            other => panic!("the acquisition should have given up, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // A board file that cannot be read
    // -----------------------------------------------------------------------

    /// With the project's file damaged, the ordinary front doors (create,
    /// update, import) report failure, leave the board as it was, and leave the
    /// damaged bytes alone; a project with a healthy file is unaffected.
    #[tokio::test]
    async fn an_unreadable_board_file_refuses_create_update_and_import_without_touching_it() {
        let world = world(false);
        let existing = crate::test_helpers::create_test_task("Existing");
        let existing_id = existing.id;
        let mut existing = existing;
        existing.project_id = world.project_id;
        world
            .storage
            .save_project_tasks(world.project_id, &[existing.clone()])
            .unwrap();
        world.tasks.write().await.insert(existing_id, existing);

        let file = world
            .storage_paths
            .legacy_tasks_dir()
            .join(format!("{}.toml", world.project_id));
        let damaged = b"tasks = [ not toml".to_vec();
        std::fs::write(&file, &damaged).unwrap();

        let created = create(
            &world.gate,
            &world.tasks,
            &world.projects,
            &world.storage,
            world.project_id,
            |_| {
                let mut t = crate::test_helpers::create_test_task("New");
                t.project_id = world.project_id;
                t
            },
        )
        .await;
        let message = created.expect_err("create must not report success");
        assert!(message.contains("will not overwrite"), "{message}");

        let amended = record(&world.tasks, &world.storage, existing_id, &|staged| {
            if let Some(t) = staged.get_mut(&existing_id) {
                t.title = "Renamed".into();
            }
        })
        .await;
        assert!(amended.is_err());

        let mut imported = crate::test_helpers::create_test_task("Imported");
        imported.project_id = world.project_id;
        assert!(create_batch(
            &world.gate,
            &world.tasks,
            &world.projects,
            &world.storage,
            world.project_id,
            &[imported]
        )
        .await
        .is_err());

        let board = world.tasks.read().await;
        assert_eq!(board.len(), 1, "no refused change may appear on the board");
        assert_eq!(board[&existing_id].title, "Existing");
        assert_eq!(std::fs::read(&file).unwrap(), damaged);
        drop(board);

        // Another project keeps working — the project exists, so creation
        // for it is still allowed.
        let other = Uuid::new_v4();
        world.projects.write().await.insert(
            other,
            Project {
                id: other,
                name: "Other".into(),
                repository_id: None,
                scope: ProjectScope::Standalone,
                state_location: StateLocation::External,
                base: None,
                agent_type: crate::domain::AgentType::ClaudeCode,
                agent_config: AgentConfig {
                    agent_type: crate::domain::AgentType::ClaudeCode,
                    command: "claude".into(),
                    args: vec!["--stdio".into()],
                    env: HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );
        let ok = create(
            &world.gate,
            &world.tasks,
            &world.projects,
            &world.storage,
            other,
            |_| {
                let mut t = crate::test_helpers::create_test_task("Elsewhere");
                t.project_id = other;
                t
            },
        )
        .await;
        assert!(ok.is_ok());
    }

    /// An import lands as one write: every Task is on disk and on the board
    /// together, the project's existing Tasks are kept, and the result reads
    /// back after a restart.
    #[tokio::test]
    async fn a_batch_reaches_the_board_and_the_file_together() {
        let world = world(false);
        let mut existing = crate::test_helpers::create_test_task("Existing");
        existing.project_id = world.project_id;
        let existing_id = existing.id;
        create_batch(
            &world.gate,
            &world.tasks,
            &world.projects,
            &world.storage,
            world.project_id,
            &[existing],
        )
        .await
        .unwrap();

        let batch: Vec<Task> = ["One", "Two", "Three"]
            .iter()
            .enumerate()
            .map(|(i, title)| {
                let mut t = crate::test_helpers::create_test_task(title);
                t.project_id = world.project_id;
                t.position = i as i32;
                t
            })
            .collect();
        create_batch(
            &world.gate,
            &world.tasks,
            &world.projects,
            &world.storage,
            world.project_id,
            &batch,
        )
        .await
        .unwrap();

        assert_eq!(world.tasks.read().await.len(), 4);
        let on_disk = world.storage.load_project_tasks(world.project_id).unwrap();
        assert_eq!(on_disk.len(), 4);
        assert!(on_disk.iter().any(|t| t.id == existing_id));
        for task in &batch {
            assert_eq!(world.persisted(task.id).position, task.position);
        }
    }
}
