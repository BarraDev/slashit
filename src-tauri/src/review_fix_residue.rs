// SPDX-License-Identifier: MPL-2.0
//! Durable record of AI Review fix attempts that may have left edits in a Task
//! Checkout.
//!
//! The lifecycle: [`begin_attempt`] persists the record before the fix agent may
//! edit anything. [`settle`] then clears it when the attempt left no edits, or
//! keeps it with the failure reason. An applied fix over a clean checkout is
//! instead authorized for its own commit, and [`Ledger::commit_fix`] clears the
//! record only after that commit is made. A crash leaves the record in place, so
//! a restart finds it and treats the checkout as unresolved; nothing infers
//! "clean" from its absence after an interruption. [`resolve`] is the Human exit.
//!
//! Serialization: writes of the record and admissions of a commit run under the
//! record gate of the Task ([`record_gate`]), not the Task's lifecycle lease.
//! `stop_task` holds the lease while it waits for a running review to finish,
//! and that review must still record its outcome, so the gate can never be the
//! lease. Human [`resolve`] takes the lease and then the gate. A record belongs
//! to one Task and one checkout, so one Task's gate never orders another's.
//!
//! Not a sandbox: a process running as the same user can still edit the
//! checkout or the app data directly. The record stops SlashIt's own commits and
//! removals, and a person's explicit resolution, not the agent's own tools.

use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;

use crate::config::Storage;
use crate::domain::task::{ReviewFixResidue, Task};
use crate::lifecycle::{self, TaskLifecycleLocks, Tasks};
use crate::worktree::CheckoutCommit;

// shortcut: one entry per Task ever touched stays in this map for the process lifetime, upgrade to evicting idle entries if the board grows large.
static RECORD_GATES: std::sync::LazyLock<
    std::sync::Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(Default::default);

/// Proof that the caller holds the record gate of one Task, from [`record_gate`].
pub type RecordGate = tokio::sync::OwnedMutexGuard<()>;

/// The record gate of `task_id`, held for one record write or one commit
/// admission and never while waiting for a lifecycle lease.
pub(crate) async fn record_gate(task_id: Uuid) -> RecordGate {
    let gate = RECORD_GATES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(task_id)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    gate.lock_owned().await
}

/// Persists `residue` before a fix agent starts. Refused, and nothing starts,
/// when an earlier attempt is still unresolved or the record cannot be
/// published durably.
pub async fn begin(
    _gate: &RecordGate,
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    residue: ReviewFixResidue,
) -> Result<(), String> {
    if let Some(existing) = tasks
        .read()
        .await
        .get(&task_id)
        .and_then(|t| t.review_fix_residue.clone())
    {
        return Err(format!(
            "an earlier AI Review fix on this Task is unresolved ({}), so no new fix was started",
            existing.reason
        ));
    }
    lifecycle::record_typed(tasks, storage, task_id, &|staged: &mut HashMap<
        Uuid,
        Task,
    >| {
        if let Some(task) = staged.get_mut(&task_id) {
            task.review_fix_residue = Some(residue.clone());
        }
    })
    .await
    .map_err(|e| format!("the fix attempt could not be recorded, so no fix agent was started: {e}"))
}

/// The reason an attempt is recorded with before it starts. It is replaced
/// only by [`settle`], so a crash leaves exactly this text behind.
pub const STARTED_REASON: &str = "an AI Review fix was started and did not record its outcome";

/// Records a new attempt in the Task Checkout at `working_dir` and returns its
/// identity. Refused when the Task has no branch, the checkout's HEAD cannot be
/// read, or an earlier attempt is unresolved. Callers settle it after the fix
/// agent's run, on every path that follows.
pub async fn begin_attempt(
    gate: &RecordGate,
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    working_dir: &str,
) -> Result<Uuid, String> {
    let branch = tasks
        .read()
        .await
        .get(&task_id)
        .and_then(|t| t.branch_name.clone())
        .ok_or_else(|| "this Task records no branch, so no fix agent was started".to_string())?;
    let base_head = crate::worktree::checkout_head_commit(working_dir)
        .await
        .map_err(|e| {
            format!("the checkout's HEAD could not be read, so no fix agent was started: {e}")
        })?;
    let attempt_id = Uuid::new_v4();
    begin(
        gate,
        tasks,
        storage,
        task_id,
        ReviewFixResidue {
            attempt_id,
            branch,
            checkout_path: working_dir.to_string(),
            base_head,
            reason: STARTED_REASON.to_string(),
            commit_authorized: false,
            apply_scope: false,
        },
    )
    .await?;
    Ok(attempt_id)
}

/// The reason kept when a fix applied but its edits are not committed by the
/// ordinary commit.
const APPLIED_UNCOMMITTED: &str = "an AI Review fix applied, but its edits were not committed";

/// What an attempt leaves for [`settle`]. A fix that applied is the ordinary
/// fix and goes through the ordinary commit. Otherwise the record stays only if
/// the checkout may hold the attempt's edits: its content changed, or it cannot
/// be known to be unchanged.
pub fn keep_after(
    applied: bool,
    clean_before: bool,
    before: &Result<String, String>,
    after: &Result<String, String>,
) -> Option<String> {
    if applied {
        // A fix that changed a clean checkout is authorized for its own commit by
        // the caller, and a fix that changed nothing leaves nothing. Other edits
        // and an unreadable checkout are unknown, so both keep the record.
        return if clean_before && after.is_ok() {
            None
        } else {
            Some(APPLIED_UNCOMMITTED.to_string())
        };
    }
    match (before, after) {
        (Ok(before), Ok(after)) if before == after => None,
        _ => Some(
            "an AI Review fix did not finish, and the Task Checkout may hold its partial edits"
                .to_string(),
        ),
    }
}

/// Settles the attempt `attempt_id`. `keep` of `None` clears the record: the
/// attempt left the checkout as it was, or its edits are the fix itself and go
/// through the ordinary commit. `Some(reason)` keeps the checkout unresolved.
pub async fn settle(
    _gate: &RecordGate,
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    attempt_id: Uuid,
    keep: Option<String>,
) -> Result<(), String> {
    let current = open_attempt(tasks, task_id, attempt_id).await?;
    let record = keep.map(|reason| ReviewFixResidue {
        reason,
        commit_authorized: false,
        ..current
    });
    write_record(tasks, storage, task_id, record).await
}

/// The record, when it is the one `attempt_id` wrote.
pub(crate) async fn open_attempt(
    tasks: &Tasks,
    task_id: Uuid,
    attempt_id: Uuid,
) -> Result<ReviewFixResidue, String> {
    let current = tasks
        .read()
        .await
        .get(&task_id)
        .and_then(|t| t.review_fix_residue.clone())
        .ok_or_else(|| "no AI Review fix record is open for this Task".to_string())?;
    if current.attempt_id != attempt_id {
        return Err("the AI Review fix record belongs to a different attempt".to_string());
    }
    Ok(current)
}

pub(crate) async fn write_record(
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    record: Option<ReviewFixResidue>,
) -> Result<(), String> {
    lifecycle::record_typed(tasks, storage, task_id, &|staged: &mut HashMap<
        Uuid,
        Task,
    >| {
        if let Some(task) = staged.get_mut(&task_id) {
            task.review_fix_residue = record.clone();
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// The Human exit: clears the record when the checkout is exactly where the
/// recorded attempt left it. It needs the same attempt, the Task's own branch
/// and checkout path, HEAD on that branch, and no uncommitted change. It does
/// not approve anything, and it does not look at HEAD's history: a commit a
/// person made of the retained edits is not Task approval, and Human Review
/// still applies to it.
pub async fn resolve(
    authority: &TaskLifecycleLocks,
    tasks: &Tasks,
    storage: &Storage,
    task_id: Uuid,
    attempt_id: Uuid,
    executor: Option<&crate::queue::TaskExecutor>,
) -> Result<(), String> {
    let _lease = authority.acquire(task_id).await?;
    // Checked under the lease: a run or review registers under it, so none can
    // start between this check and the clear. A live attempt means its agent may
    // still be editing the checkout, so the record must stay.
    if let Some(executor) = executor {
        if executor.is_task_running(task_id).await {
            return Err("the Task is running an agent or review now, so the unresolved fix cannot be cleared yet".to_string());
        }
    }
    let _gate = record_gate(task_id).await;
    let (residue, branch, worktree_path) = {
        let tasks_r = tasks.read().await;
        let task = tasks_r
            .get(&task_id)
            .ok_or_else(|| format!("task {task_id} is not on the board"))?;
        (
            task.review_fix_residue.clone(),
            task.branch_name.clone(),
            task.worktree_path.clone(),
        )
    };
    let residue =
        residue.ok_or_else(|| "no AI Review fix is unresolved for this Task".to_string())?;
    if residue.attempt_id != attempt_id {
        return Err("the unresolved AI Review fix belongs to a different attempt".to_string());
    }
    // A checkout that is gone on disk holds no edits to protect. Its reference
    // may already be cleared by startup, so the absence is checked here, and the
    // record is not taken to mean the content was reviewed or approved. Only a
    // definite NotFound counts: an unreadable path (permission, unmounted drive)
    // may still hold the edits, and `exists()` would read it as gone.
    let checkout_missing = matches!(
        std::fs::symlink_metadata(&residue.checkout_path),
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
    );
    let reference_matches = match worktree_path.as_deref() {
        Some(path) => path == residue.checkout_path,
        None => checkout_missing,
    };
    if branch.as_deref() != Some(residue.branch.as_str()) || !reference_matches {
        return Err(
            "the Task's branch or checkout no longer matches the unresolved fix".to_string(),
        );
    }
    if !checkout_missing {
        if current_branch(&residue.checkout_path).await?.as_deref() != Some(residue.branch.as_str())
        {
            return Err(
                "the Task Checkout is not on the Task branch; put it back before resolving"
                    .to_string(),
            );
        }
        if crate::worktree::checkout_has_work(&residue.checkout_path, None).await? {
            return Err(
                "the Task Checkout has uncommitted changes; commit or remove them before resolving"
                    .to_string(),
            );
        }
    }
    lifecycle::record_typed(tasks, storage, task_id, &|staged: &mut HashMap<
        Uuid,
        Task,
    >| {
        if let Some(task) = staged.get_mut(&task_id) {
            task.review_fix_residue = None;
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// The branch HEAD names, or `None` when HEAD is detached.
async fn current_branch(checkout_path: &str) -> Result<Option<String>, String> {
    let output = crate::host_process::tokio_command("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(checkout_path)
        .output()
        .await
        .map_err(|e| format!("could not ask Git which branch the checkout is on: {e}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let reference = String::from_utf8_lossy(&output.stdout);
    Ok(reference
        .trim()
        .strip_prefix("refs/heads/")
        .map(str::to_string))
}

/// The board, storage and lifecycle authority the records are written through,
/// for a path that does not already hold the record gate. Each method takes the
/// gate itself, for the duration of its one check or write.
#[derive(Clone)]
pub struct Ledger {
    pub authority: std::sync::Arc<TaskLifecycleLocks>,
    pub tasks: Tasks,
    pub storage: Storage,
}

impl Ledger {
    /// The ledger over the board, storage and lease table of `state`.
    pub fn of(state: &crate::AppState) -> Self {
        Self {
            authority: state.task_lifecycle_locks.clone(),
            tasks: state.task.tasks.clone(),
            storage: state.storage.clone(),
        }
    }

    pub async fn begin_attempt(&self, task_id: Uuid, working_dir: &str) -> Result<Uuid, String> {
        let gate = record_gate(task_id).await;
        begin_attempt(&gate, &self.tasks, &self.storage, task_id, working_dir).await
    }

    pub async fn settle(
        &self,
        task_id: Uuid,
        attempt_id: Uuid,
        keep: Option<String>,
    ) -> Result<(), String> {
        let gate = record_gate(task_id).await;
        settle(&gate, &self.tasks, &self.storage, task_id, attempt_id, keep).await
    }

    /// Admits a SlashIt-controlled commit that is not an attempt's own. The
    /// returned gate must be held through the commit, so no record write can
    /// land in between. `Err` refuses the commit before anything is staged.
    pub async fn admit_commit(&self, task_id: Uuid) -> Result<RecordGate, String> {
        admit_ordinary_commit(&self.tasks, task_id).await
    }

    /// Marks the applied fix of `attempt_id` as authorized for its own commit.
    /// Written before that commit, so a crash between the two keeps the edits
    /// recorded and refuses every other commit.
    pub async fn authorize_commit(&self, task_id: Uuid, attempt_id: Uuid) -> Result<(), String> {
        let _gate = record_gate(task_id).await;
        let current = open_attempt(&self.tasks, task_id, attempt_id).await?;
        if current.apply_scope {
            return Err("a PR review apply scope is not an AI Review fix".to_string());
        }
        write_record(
            &self.tasks,
            &self.storage,
            task_id,
            Some(ReviewFixResidue {
                commit_authorized: true,
                reason: APPLIED_UNCOMMITTED.to_string(),
                ..current
            }),
        )
        .await
    }

    /// Commits the authorized fix of `attempt_id` on `branch`, holding the gate
    /// through the commit. The record is cleared once the commit is made, and
    /// kept unauthorized with the reason when it is not, so a retry cannot
    /// commit again. A record that does not authorize this commit refuses it
    /// before anything is staged.
    pub async fn commit_fix(
        &self,
        task_id: Uuid,
        attempt_id: Uuid,
        branch: Option<&str>,
        working_dir: &str,
        message: &str,
        verified_tree: &str,
    ) -> Result<CheckoutCommit, String> {
        let _gate = record_gate(task_id).await;
        let current = open_attempt(&self.tasks, task_id, attempt_id).await?;
        if current.apply_scope {
            return Err("a PR review apply scope is not an AI Review fix".to_string());
        }
        if !current.commit_authorized {
            return Err("the AI Review fix's edits are not authorized for a commit".to_string());
        }
        let outcome = match branch {
            Some(branch) => {
                crate::worktree::commit_checkout_verified(working_dir, branch, message, verified_tree)
                    .await
            }
            None => Err("this Task records no branch".to_string()),
        };
        match &outcome {
            // A commit that was made is reported as made. If clearing the record
            // fails, the record stays authorized for this attempt only, and
            // resolve reconciles it once the checkout is clean on the Task branch.
            Ok(_) => {
                let _ = write_record(&self.tasks, &self.storage, task_id, None).await;
            }
            Err(error) => {
                let reason = format!(
                    "an AI Review fix's edits could not be committed, so they remain uncommitted in the Task Checkout: {error}"
                );
                if let Err(e) = write_record(
                    &self.tasks,
                    &self.storage,
                    task_id,
                    Some(ReviewFixResidue { reason, commit_authorized: false, ..current }),
                )
                .await
                {
                    return Err(format!("{error}; the AI Review fix record could not be updated either: {e}"));
                }
            }
        }
        outcome
    }
}

/// The gate for a commit that is not an attempt's own (a Worker's, or a PR
/// apply's), refused while any attempt is unresolved. The gate is held by the
/// caller through the commit.
pub async fn admit_ordinary_commit(tasks: &Tasks, task_id: Uuid) -> Result<RecordGate, String> {
    let gate = record_gate(task_id).await;
    let refusal = tasks
        .read()
        .await
        .get(&task_id)
        .and_then(Task::review_fix_residue_refusal);
    match refusal {
        Some(refusal) => Err(refusal),
        None => Ok(gate),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskStatus;
    use std::path::{Path, PathBuf};

    /// The content the checkout holds now, as a commit must find it staged.
    async fn snapshot_of(dir: &str) -> String {
        crate::worktree::checkout_snapshot(dir).await.unwrap().content
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        state: crate::AppState,
        ledger: Ledger,
        project_id: Uuid,
        task_id: Uuid,
        checkout: PathBuf,
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A Task on `task-x` in a fresh checkout, with `a.txt` and `b.txt` committed.
    async fn fixture() -> Fx {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let (state, _report) = crate::app_core::build_state_with_paths(paths)
            .await
            .expect("build state");
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        git(&checkout, &["init", "-q", "-b", "main"]);
        git(&checkout, &["config", "user.name", "Test"]);
        git(&checkout, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(checkout.join("a.txt"), "a\n").unwrap();
        std::fs::write(checkout.join("b.txt"), "b\n").unwrap();
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-q", "-m", "base"]);
        git(&checkout, &["checkout", "-q", "-b", "task-x"]);

        let project_id = Uuid::new_v4();
        let mut task = crate::test_helpers::create_test_task("review fix task");
        task.project_id = project_id;
        task.branch_name = Some("task-x".to_string());
        task.worktree_path = Some(checkout.to_str().unwrap().to_string());
        task.status = TaskStatus::HumanReview;
        let task_id = task.id;
        state.task.tasks.write().await.insert(task_id, task);
        let ledger = Ledger::of(&state);
        Fx {
            _tmp: tmp,
            state,
            ledger,
            project_id,
            task_id,
            checkout,
        }
    }

    async fn residue(fx: &Fx) -> Option<ReviewFixResidue> {
        fx.state
            .task
            .tasks
            .read()
            .await
            .get(&fx.task_id)
            .and_then(|t| t.review_fix_residue.clone())
    }

    /// Restart: what the next process would load from the durable board.
    fn reloaded(fx: &Fx) -> Option<ReviewFixResidue> {
        fx.state
            .storage
            .load_project_tasks(fx.project_id)
            .expect("load tasks")
            .into_iter()
            .find(|t| t.id == fx.task_id)
            .and_then(|t| t.review_fix_residue)
    }

    /// Snapshot of the checkout's content, as the executor compares it.
    async fn content(fx: &Fx) -> Result<String, String> {
        crate::worktree::checkout_snapshot(fx.checkout.to_str().unwrap())
            .await
            .map(|s| s.content)
    }

    /// Edits, creates and deletes files the way a failed fix might.
    fn failed_fix_edits(checkout: &Path) {
        std::fs::write(checkout.join("a.txt"), "changed\n").unwrap();
        std::fs::write(checkout.join("new.txt"), "added\n").unwrap();
        std::fs::remove_file(checkout.join("b.txt")).unwrap();
    }

    #[test]
    fn keep_after_clears_only_when_the_checkout_is_known_unchanged() {
        let same = Ok("tree".to_string());
        let other = Ok("other".to_string());
        let unreadable = Err("no index".to_string());
        assert_eq!(
            keep_after(true, true, &same, &other),
            None,
            "an applied fix is the ordinary fix"
        );
        assert!(
            keep_after(true, false, &same, &other).is_some(),
            "an applied fix over other edits is not the ordinary commit"
        );
        assert!(
            keep_after(true, true, &same, &unreadable).is_some(),
            "an applied fix with an unknown after keeps it"
        );
        assert_eq!(
            keep_after(false, true, &same, &same),
            None,
            "an unchanged failure leaves nothing"
        );
        assert!(
            keep_after(false, true, &same, &other).is_some(),
            "a changed failure keeps the record"
        );
        assert!(
            keep_after(false, true, &unreadable, &same).is_some(),
            "an unknown before keeps it"
        );
        assert!(
            keep_after(false, true, &same, &unreadable).is_some(),
            "an unknown after keeps it"
        );
    }

    #[tokio::test]
    async fn successful_attempt_leaves_no_record_and_commits_normally() {
        let fx = fixture().await;
        let before = content(&fx).await.unwrap();
        let attempt = fx
            .ledger
            .begin_attempt(fx.task_id, fx.checkout.to_str().unwrap())
            .await
            .unwrap();
        assert!(
            residue(&fx).await.is_some(),
            "recorded before the agent runs"
        );
        std::fs::write(fx.checkout.join("a.txt"), "fixed\n").unwrap();
        let after = content(&fx).await;
        fx.ledger
            .settle(
                fx.task_id,
                attempt,
                keep_after(true, true, &Ok(before), &after),
            )
            .await
            .unwrap();
        assert!(residue(&fx).await.is_none());
        assert!(fx.ledger.admit_commit(fx.task_id).await.is_ok());
    }

    #[tokio::test]
    async fn failed_fix_with_modified_added_and_deleted_files_blocks_commits_and_survives_restart()
    {
        let fx = fixture().await;
        let before = content(&fx).await;
        let attempt = fx
            .ledger
            .begin_attempt(fx.task_id, fx.checkout.to_str().unwrap())
            .await
            .unwrap();
        failed_fix_edits(&fx.checkout);
        let after = content(&fx).await;
        fx.ledger
            .settle(
                fx.task_id,
                attempt,
                keep_after(false, true, &before, &after),
            )
            .await
            .unwrap();

        assert!(
            fx.ledger.admit_commit(fx.task_id).await.is_err(),
            "commit refused while unresolved"
        );
        assert_eq!(
            reloaded(&fx).map(|r| r.attempt_id),
            Some(attempt),
            "marker survives restart"
        );
        assert!(
            fx.checkout.join("new.txt").exists(),
            "nothing was reset or cleaned"
        );
        assert!(
            !fx.checkout.join("b.txt").exists(),
            "the deletion is preserved as found"
        );
    }

    #[tokio::test]
    async fn crash_during_or_after_the_fix_leaves_the_record_for_restart() {
        let fx = fixture().await;
        let attempt = fx
            .ledger
            .begin_attempt(fx.task_id, fx.checkout.to_str().unwrap())
            .await
            .unwrap();
        // The process stops here: nothing settles the attempt.
        assert_eq!(reloaded(&fx).map(|r| r.attempt_id), Some(attempt));
        assert_eq!(
            reloaded(&fx).map(|r| r.reason),
            Some(STARTED_REASON.to_string())
        );
    }

    #[tokio::test]
    async fn a_second_attempt_is_refused_while_one_is_unresolved() {
        let fx = fixture().await;
        let dir = fx.checkout.to_str().unwrap().to_string();
        fx.ledger.begin_attempt(fx.task_id, &dir).await.unwrap();
        assert!(fx.ledger.begin_attempt(fx.task_id, &dir).await.is_err());
    }

    #[tokio::test]
    async fn resolve_refuses_a_dirty_checkout_and_a_stale_attempt_then_clears_a_clean_one() {
        let fx = fixture().await;
        let before = content(&fx).await;
        let attempt = fx
            .ledger
            .begin_attempt(fx.task_id, fx.checkout.to_str().unwrap())
            .await
            .unwrap();
        failed_fix_edits(&fx.checkout);
        let after = content(&fx).await;
        fx.ledger
            .settle(
                fx.task_id,
                attempt,
                keep_after(false, true, &before, &after),
            )
            .await
            .unwrap();

        let stale = resolve(
            &fx.ledger.authority,
            &fx.state.task.tasks,
            &fx.state.storage,
            fx.task_id,
            Uuid::new_v4(),
            None,
        )
        .await;
        assert!(stale.is_err(), "a different attempt cannot resolve");
        let dirty = resolve(
            &fx.ledger.authority,
            &fx.state.task.tasks,
            &fx.state.storage,
            fx.task_id,
            attempt,
            None,
        )
        .await;
        assert!(dirty.is_err(), "uncommitted edits block resolution");
        assert!(residue(&fx).await.is_some());

        // A person commits the retained edits by hand. That is not Task approval,
        // and it does not change the Task's Human Review status.
        git(&fx.checkout, &["add", "-A"]);
        git(&fx.checkout, &["commit", "-q", "-m", "manual"]);
        resolve(
            &fx.ledger.authority,
            &fx.state.task.tasks,
            &fx.state.storage,
            fx.task_id,
            attempt,
            None,
        )
        .await
        .expect("a clean checkout on the Task branch resolves");
        assert!(residue(&fx).await.is_none());
        assert!(fx.ledger.admit_commit(fx.task_id).await.is_ok());
        let status = fx
            .state
            .task
            .tasks
            .read()
            .await
            .get(&fx.task_id)
            .unwrap()
            .status
            .clone();
        assert_eq!(
            status,
            TaskStatus::HumanReview,
            "a manual commit does not approve the Task"
        );
    }

    #[tokio::test]
    async fn resolve_refuses_a_checkout_on_another_branch() {
        let fx = fixture().await;
        let attempt = fx
            .ledger
            .begin_attempt(fx.task_id, fx.checkout.to_str().unwrap())
            .await
            .unwrap();
        git(&fx.checkout, &["checkout", "-q", "-b", "elsewhere"]);
        let refused = resolve(
            &fx.ledger.authority,
            &fx.state.task.tasks,
            &fx.state.storage,
            fx.task_id,
            attempt,
            None,
        )
        .await;
        assert!(refused.is_err());
        assert!(residue(&fx).await.is_some());
    }

    #[tokio::test]
    async fn a_task_gate_blocks_only_its_own_task() {
        let task = Uuid::new_v4();
        let held = record_gate(task).await;
        let same = tokio::time::timeout(std::time::Duration::from_millis(50), record_gate(task)).await;
        assert!(same.is_err(), "the Task's own writes wait for the gate");
        let other = tokio::time::timeout(std::time::Duration::from_secs(1), record_gate(Uuid::new_v4())).await;
        assert!(other.is_ok(), "another Task's writes do not wait for it");
        drop(held);
        assert!(tokio::time::timeout(std::time::Duration::from_secs(1), record_gate(task)).await.is_ok());
    }

    #[tokio::test]
    async fn an_admission_waits_for_a_writer_and_then_sees_the_settled_record() {
        let fx = fixture().await;
        let dir = fx.checkout.to_str().unwrap().to_string();
        let gate = record_gate(fx.task_id).await;
        let attempt = begin_attempt(
            &gate,
            &fx.state.task.tasks,
            &fx.state.storage,
            fx.task_id,
            &dir,
        )
        .await
        .unwrap();

        let ledger = fx.ledger.clone();
        let task_id = fx.task_id;
        let admission = tokio::spawn(async move { ledger.admit_commit(task_id).await.map(|_| ()) });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !admission.is_finished(),
            "admission cannot decide while a writer holds the gate"
        );

        settle(
            &gate,
            &fx.state.task.tasks,
            &fx.state.storage,
            fx.task_id,
            attempt,
            Some("kept".into()),
        )
        .await
        .unwrap();
        drop(gate);
        let decided = admission.await.unwrap();
        assert!(
            decided.is_err(),
            "the admission reads the record the writer left"
        );
    }

    fn commit_count(checkout: &Path) -> String {
        let out = std::process::Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(checkout)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    async fn status_of(fx: &Fx) -> TaskStatus {
        fx.state
            .task
            .tasks
            .read()
            .await
            .get(&fx.task_id)
            .unwrap()
            .status
            .clone()
    }

    #[tokio::test]
    async fn an_authorized_fix_commits_under_its_attempt_and_clears_only_after_the_commit() {
        let fx = fixture().await;
        let dir = fx.checkout.to_str().unwrap().to_string();
        let attempt = fx.ledger.begin_attempt(fx.task_id, &dir).await.unwrap();
        std::fs::write(fx.checkout.join("a.txt"), "fixed\n").unwrap();
        fx.ledger
            .authorize_commit(fx.task_id, attempt)
            .await
            .unwrap();
        assert_eq!(
            reloaded(&fx).map(|r| r.commit_authorized),
            Some(true),
            "authorization is durable before the commit"
        );

        let outcome = fx
            .ledger
            .commit_fix(fx.task_id, attempt, Some("task-x"), &dir, "task: fix", &snapshot_of(&dir).await)
            .await
            .unwrap();
        assert!(
            matches!(outcome, CheckoutCommit::Committed { .. }),
            "{outcome:?}"
        );
        assert!(residue(&fx).await.is_none());
        assert!(reloaded(&fx).is_none(), "the cleared record is durable");
        assert_eq!(commit_count(&fx.checkout), "2");
        assert!(fx.ledger.admit_commit(fx.task_id).await.is_ok());
    }

    #[tokio::test]
    async fn an_unauthorized_or_foreign_attempt_cannot_commit() {
        let fx = fixture().await;
        let dir = fx.checkout.to_str().unwrap().to_string();
        let attempt = fx.ledger.begin_attempt(fx.task_id, &dir).await.unwrap();
        std::fs::write(fx.checkout.join("a.txt"), "changed\n").unwrap();
        fx.ledger
            .settle(fx.task_id, attempt, Some("kept".into()))
            .await
            .unwrap();

        assert!(fx
            .ledger
            .commit_fix(fx.task_id, attempt, Some("task-x"), &dir, "m", &snapshot_of(&dir).await)
            .await
            .is_err());
        assert!(fx
            .ledger
            .commit_fix(fx.task_id, Uuid::new_v4(), Some("task-x"), &dir, "m", &snapshot_of(&dir).await)
            .await
            .is_err());
        assert_eq!(commit_count(&fx.checkout), "1", "no commit was made");
        assert!(residue(&fx).await.is_some(), "the record is kept");
    }

    #[tokio::test]
    async fn a_crash_after_authorization_keeps_the_edits_on_record_and_refuses_other_commits() {
        let fx = fixture().await;
        let dir = fx.checkout.to_str().unwrap().to_string();
        let attempt = fx.ledger.begin_attempt(fx.task_id, &dir).await.unwrap();
        std::fs::write(fx.checkout.join("a.txt"), "fixed\n").unwrap();
        fx.ledger
            .authorize_commit(fx.task_id, attempt)
            .await
            .unwrap();
        // The process stops before the commit. The next process loads this record.
        assert_eq!(reloaded(&fx).map(|r| r.commit_authorized), Some(true));
        assert!(
            fx.ledger.admit_commit(fx.task_id).await.is_err(),
            "ordinary commits stay refused"
        );
        assert!(
            resolve(
                &fx.ledger.authority,
                &fx.state.task.tasks,
                &fx.state.storage,
                fx.task_id,
                attempt,
                None
            )
            .await
            .is_err(),
            "uncommitted edits cannot be resolved away"
        );
        assert_eq!(
            std::fs::read_to_string(fx.checkout.join("a.txt")).unwrap(),
            "fixed\n"
        );
    }

    #[tokio::test]
    async fn a_crash_after_the_commit_is_reconciled_by_resolve_without_approving_the_task() {
        let fx = fixture().await;
        let dir = fx.checkout.to_str().unwrap().to_string();
        let attempt = fx.ledger.begin_attempt(fx.task_id, &dir).await.unwrap();
        std::fs::write(fx.checkout.join("a.txt"), "fixed\n").unwrap();
        fx.ledger
            .authorize_commit(fx.task_id, attempt)
            .await
            .unwrap();
        // The commit lands, and the process stops before the record clears.
        git(&fx.checkout, &["add", "-A"]);
        git(&fx.checkout, &["commit", "-q", "-m", "fix"]);
        assert!(reloaded(&fx).is_some(), "the record survives the crash");

        resolve(
            &fx.ledger.authority,
            &fx.state.task.tasks,
            &fx.state.storage,
            fx.task_id,
            attempt,
            None,
        )
        .await
        .expect("a clean checkout on the Task branch reconciles");
        assert!(residue(&fx).await.is_none());
        assert_eq!(
            status_of(&fx).await,
            TaskStatus::HumanReview,
            "reconciling is not approval"
        );
    }

    #[tokio::test]
    async fn a_failed_commit_keeps_the_record_and_withdraws_its_authorization() {
        let fx = fixture().await;
        let dir = fx.checkout.to_str().unwrap().to_string();
        let attempt = fx.ledger.begin_attempt(fx.task_id, &dir).await.unwrap();
        std::fs::write(fx.checkout.join("a.txt"), "fixed\n").unwrap();
        fx.ledger
            .authorize_commit(fx.task_id, attempt)
            .await
            .unwrap();
        // The checkout moved to another branch, so the commit must fail closed.
        git(&fx.checkout, &["checkout", "-q", "-b", "elsewhere"]);
        assert!(fx
            .ledger
            .commit_fix(fx.task_id, attempt, Some("task-x"), &dir, "m", &snapshot_of(&dir).await)
            .await
            .is_err());
        let kept = residue(&fx).await.expect("the record is kept");
        assert!(
            !kept.commit_authorized,
            "a failed commit is not authorized again"
        );

        git(&fx.checkout, &["checkout", "-q", "task-x"]);
        assert!(fx
            .ledger
            .commit_fix(fx.task_id, attempt, Some("task-x"), &dir, "m", &snapshot_of(&dir).await)
            .await
            .is_err());
        assert_eq!(commit_count(&fx.checkout), "1", "no commit was made");
    }

    #[tokio::test]
    async fn a_checkout_that_is_gone_resolves_only_when_its_reference_is_also_cleared() {
        let fx = fixture().await;
        let dir = fx.checkout.to_str().unwrap().to_string();
        let attempt = fx.ledger.begin_attempt(fx.task_id, &dir).await.unwrap();
        fx.ledger
            .settle(fx.task_id, attempt, Some("kept".into()))
            .await
            .unwrap();
        // The reference is cleared but the directory is still there: not a missing checkout.
        fx.state
            .task
            .tasks
            .write()
            .await
            .get_mut(&fx.task_id)
            .unwrap()
            .worktree_path = None;
        assert!(resolve(
            &fx.ledger.authority,
            &fx.state.task.tasks,
            &fx.state.storage,
            fx.task_id,
            attempt,
            None
        )
        .await
        .is_err());

        std::fs::remove_dir_all(&fx.checkout).unwrap();
        resolve(
            &fx.ledger.authority,
            &fx.state.task.tasks,
            &fx.state.storage,
            fx.task_id,
            attempt,
            None,
        )
        .await
        .expect("a checkout that is gone on disk resolves");
        assert!(residue(&fx).await.is_none());
        assert_eq!(
            status_of(&fx).await,
            TaskStatus::HumanReview,
            "no content is inferred as reviewed"
        );
    }

    /// A checkout that exists but cannot be stat'ed (its parent is unreadable) may
    /// still hold the edits, so it is not taken as gone: resolve refuses and keeps
    /// the record. Non-root only; the permission bit is what makes the stat fail.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_checkout_that_cannot_be_read_is_not_taken_as_gone() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture().await;
        let dir = fx.checkout.to_str().unwrap().to_string();
        let attempt = fx.ledger.begin_attempt(fx.task_id, &dir).await.unwrap();
        fx.ledger
            .settle(fx.task_id, attempt, Some("kept".into()))
            .await
            .unwrap();
        let parent = fx.checkout.parent().unwrap().to_path_buf();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = matches!(
            std::fs::symlink_metadata(&fx.checkout),
            Err(ref e) if e.kind() != std::io::ErrorKind::NotFound
        );
        let refused = resolve(
            &fx.ledger.authority,
            &fx.state.task.tasks,
            &fx.state.storage,
            fx.task_id,
            attempt,
            None,
        )
        .await;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !unreadable {
            // Running as root: the permission bit does not hide the path.
            return;
        }
        assert!(refused.is_err(), "an unreadable checkout must refuse: {refused:?}");
        assert!(residue(&fx).await.is_some(), "the record stays");
    }

    #[test]
    fn a_record_with_an_unknown_field_fails_to_load() {
        let json = r#"{"attempt_id":"00000000-0000-0000-0000-000000000001","branch":"b","checkout_path":"/c","base_head":"h","reason":"r","extra":1}"#;
        assert!(serde_json::from_str::<ReviewFixResidue>(json).is_err());
    }
}
