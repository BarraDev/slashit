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
/// Bounded rather than unbounded because the lease is held across `git` and
/// `wt` subprocesses, and a removal blocked on a repository hook would
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
    CleanupRefused { worktree_path: String, reason: String },
    /// The removal happened but the board could not be written.
    NotRecorded(String),
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
            } => write!(
                f,
                "the worktree at {worktree_path} was kept: {reason}"
            ),
            Self::NotRecorded(m) => write!(f, "{m}"),
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
/// accepts the change before the board shows it, so a failure leaves both the
/// shared map and the disk exactly as they were and the caller is told so.
pub async fn record(
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    amend: Amend<'_>,
) -> Result<(), String> {
    let mut tasks_w = tasks.write().await;
    let Some(project_id) = tasks_w.get(&task_id).map(|t| t.project_id) else {
        return Err(format!("task {task_id} is no longer on the board"));
    };

    let mut staged = stage_project(&tasks_w, project_id);
    amend(&mut staged);
    if let Some(task) = staged.get_mut(&task_id) {
        task.updated_at = chrono::Utc::now();
    }

    publish(&mut tasks_w, storage, project_id, staged)
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
            Some(task) => (task.project_id, task.status.clone()),
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
    storage.save_project_tasks(project_id, &list).map_err(|e| {
        // The record stays, and it stays truthful: the terminalization above
        // already committed `worktree_path = None` and `cleanup_in_flight =
        // false`, so what survives says the cleanup succeeded, which it did.
        // Asking again is then an ordinary delete with nothing left to clean
        // up.
        TerminalizeRefusal::NotRecorded(format!(
            "the worktree for task {task_id} was cleaned up, but removing the task from the \
             board failed, so the task is still there and can simply be deleted again: {e}"
        ))
    })?;

    tasks_w.remove(&task_id);
    Ok(true)
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
    let (project_id, worktree_path, quarantined) = {
        let tasks = ctx.tasks.read().await;
        let task = tasks.get(&task_id).ok_or(TerminalizeRefusal::TaskNotFound)?;
        (
            task.project_id,
            task.worktree_path.clone(),
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
        .remove(&worktree_path, &repo_path)
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
    Kept { worktree_path: String, reason: String },
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
        return Err(format!("task {task_id} vanished before its cleanup was recorded"));
    };
    task.cleanup_in_flight = true;
    task.updated_at = chrono::Utc::now();

    publish(&mut tasks_w, storage, project_id, staged).map_err(|e| {
        format!(
            "the worktree cleanup for task {task_id} was not started, because recording that \
             it had begun failed and an interrupted removal would then be invisible: {e}"
        )
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
                task.status = request.desired.clone();
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
        TerminalizeRefusal::NotRecorded(match &kept {
            None => format!(
                "the worktree was removed, but recording task {task_id} as finished failed, so \
                 the board still shows its previous state and still names a checkout that is \
                 gone; the next start reconciles it: {e}"
            ),
            Some(_) => format!(
                "the worktree was kept, and recording why failed: {e}"
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
    let list: Vec<Task> = staged.values().cloned().collect();
    storage
        .save_project_tasks(project_id, &list)
        .map_err(|e| e.to_string())?;
    for (id, task) in staged {
        tasks_w.insert(id, task);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::paths::{AppPaths, StateLocation, WorktreePlacement};
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
            // `Managed` forces the git-native path regardless of whether `wt`
            // happens to be installed on the machine running this test.
            worktree_manager: WorktreeManager::new(
                Arc::new(app_paths.clone()),
                WorktreePlacement::Managed,
            ),
            storage: Storage::with_paths(app_paths.clone()),
            authority: TaskLifecycleLocks::new(),
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
        assert!(!std::path::Path::new(&wt).exists(), "the checkout must be gone");

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
        std::fs::write(std::path::Path::new(&wt).join("unsaved.txt"), "not committed\n").unwrap();
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
        assert_eq!(after.status, TaskStatus::InProgress, "the status must not move");
        assert_eq!(after.worktree_path.as_deref(), Some(wt.as_str()));
        assert_eq!(after.branch_name.as_deref(), Some("task-abcd1234"));
        assert!(!after.cleanup_in_flight, "the attempt concluded, so nothing is in flight");
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

        assert!(matches!(refusal, TerminalizeRefusal::RepositoryUnresolved(_)));
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

        let refusal = terminalize(ctx, id, Origin::User, TerminalizeRequest::new(TaskStatus::Done))
            .await
            .expect_err("a live agent's working directory must not be deleted under it");

        assert!(matches!(refusal, TerminalizeRefusal::ExecutionActive));
        assert!(std::path::Path::new(&wt).exists());
        let after = world.task(id).await;
        assert_eq!(after.status, TaskStatus::InProgress);
        assert!(!after.cleanup_in_flight);
    }

    // -----------------------------------------------------------------------
    // Quarantine
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn an_automatic_terminalization_will_not_touch_an_interrupted_cleanup() {
        let world = world(true);
        let wt = worktree_with_committed_work(&world, "task-abcd1234").await;
        let (id, _) = seed(&world, TaskStatus::InProgress, Some(&wt)).await;
        world.tasks.write().await.get_mut(&id).unwrap().cleanup_in_flight = true;

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
        world.tasks.write().await.get_mut(&id).unwrap().cleanup_in_flight = true;

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
        assert_eq!(after.status, TaskStatus::PrCreated, "and the task is not Done");
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
        assert_eq!(after.pr_url.as_deref(), Some("https://example.invalid/pull/1"));
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
        assert_eq!(after.pr_url, None, "nothing may be published that the file did not accept");
        assert_eq!(after.status, TaskStatus::InProgress, "including the status it implied");
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
        assert_eq!(after.pr_url.as_deref(), Some("https://example.invalid/pr/1"));
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
        assert_ne!(after.status, TaskStatus::Done, "and nothing may claim it finished");
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
        std::fs::write(std::path::Path::new(&wt).join("unsaved.txt"), "not committed\n").unwrap();
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

        assert!(delete(world.ctx(), id).await.expect("the delete must succeed"));

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
        assert!(!std::path::Path::new(&wt).exists(), "the checkout goes with it");
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
        assert!(delete(world.ctx(), id).await.expect("the first delete succeeds"));
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

    #[tokio::test]
    async fn a_delete_of_a_task_that_is_not_there_is_not_a_failure() {
        let world = world(true);
        assert!(
            !delete(world.ctx(), Uuid::new_v4())
                .await
                .expect("asking for something already gone is not an error"),
        );
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
}
