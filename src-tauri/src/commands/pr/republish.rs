//! Restacking a stacked task whose branch is already published, after its
//! parent landed on the default branch.
//!
//! A child whose pull request was opened against its parent while the parent
//! was open is published by the time the parent lands. A squash or rebase
//! merge gives the parent's work new commits on the default branch, so the
//! child's branch still carries the parent's old ones and its pull request
//! lists them again. Retargeting the pull request does not change that: only
//! replaying the child's own commits onto what landed does, which rewrites the
//! published branch. That needs the user's approval and a guarded update, so
//! nothing here runs on its own: [`status`] only observes, [`restack`] is one
//! explicit action, and [`discard`] is the way back.
//!
//! Where the parent landed with a merge commit, its commits are on the default
//! branch as they are and the branch already differs from it by exactly the
//! child's own commits. Nothing is rewritten then; the pull request is only
//! retargeted.
//!
//! The replay itself is the unpublished restack's
//! ([`crate::worktree::restack::Restack`]): `git rebase --onto <onto> <fork
//! point> <branch>` from the commit the task recorded when its branch was
//! created, with a backup of the old tip, a patch-id check of the result and a
//! rollback. What differs is everything around it, because the old tip is on
//! the remote and a retry must find its way without guessing.
//!
//! # Phases and what survives a crash
//!
//! One durable record, [`PendingRepublish`] on the task, is written before
//! anything changes and says what the user approved: the tip the remote must
//! still have (`previous_tip`) and the commit to replay onto (`onto`). Every
//! retry starts from it and from what is actually in the repository, never from
//! a fresh look at the remote that could pick up someone else's work.
//!
//! 1. Plan: the record is written. Nothing else has changed. A crash here
//!    leaves a record, the branch at `previous_tip` and no backup.
//! 2. Backup: `refs/slashit/republish-backup/<task>` is created at
//!    `previous_tip`, create-only. It is deleted only at the very end, after
//!    the remote holds the rewritten branch, the pull request is retargeted
//!    and the pending record is cleared.
//! 3. Replay: the branch moves to a new tip, verified by patch-id.
//! 4. Record: one compare-and-set task write sets `rewritten_tip`, moves
//!    `base_commit` to `onto` and the origin to the default branch. The
//!    record then says the local branch is ahead of the remote on purpose.
//! 5. Push: `git push --force-with-lease=refs/heads/<b>:<previous_tip>`. A
//!    retry asks the remote only to tell three states apart: it is at the
//!    rewritten tip (done), at `previous_tip` (push, with the same lease), or
//!    elsewhere (someone else pushed: refuse, change nothing, hand back).
//! 6. Retarget: the pull request's base is read and converged to the default
//!    branch, an edit only when it differs, so GitHub having already moved it
//!    is the same as having done it. Then the record is cleared.
//!
//! A crash after phase 3 and before phase 4 leaves a branch that is not at
//! `previous_tip` and a record without `rewritten_tip`: the branch is adopted
//! only when its commits are the replay of the recorded range onto `onto`:
//! the same changes by patch-id, and the same full message and author for each
//! (a rebase changes neither), and refused otherwise. A rebase left stopped by
//! a crash is never touched here; the user finishes or aborts it, then retries
//! or discards.
//!
//! The local branch is never claimed to be published: while the record exists,
//! the remote may be behind it.

use super::*;
use crate::domain::PendingRepublish;
use crate::worktree::restack;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What the pull request surface shows for a stacked task whose branch is
/// published.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepublishStatus {
    /// The parent landed on the default branch, and the task can be moved
    /// onto it. `rewrites` is false when only the pull request's base needs
    /// to change.
    NeedsRestack {
        parent_branch: String,
        parent_pr: u64,
        default_branch: String,
        pr_number: u64,
        rewrites: bool,
    },
    /// The parent landed, but a restack is not possible as things are, and
    /// `reason` says what to change first.
    Blocked { reason: String },
    /// A restack was started and did not finish. Running it again resumes it.
    /// While it is, nothing else may change the task's branch or checkout.
    /// `discard_blocked` says why Discard would be refused, when origin
    /// already has the rewritten branch.
    Interrupted { rewritten: bool, detail: String, discard_blocked: Option<String> },
}

/// What a finished restack did.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepublishOutcome {
    /// Whether the branch was rewritten (false for a retarget only).
    pub rewritten: bool,
    /// The branch the pull request now targets.
    pub base: String,
    /// The branch's tip before the restack.
    pub previous_tip: String,
    /// The branch's tip now, which the remote has.
    pub new_tip: String,
}

/// Where a test makes the process "die": the function returns early without
/// any cleanup, leaving exactly what a crash at that point would.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    Planned,
    BackedUp,
    Replayed,
    Recorded,
    Pushed,
}

#[cfg(test)]
pub(super) static CRASH_AFTER: std::sync::Mutex<Option<Stage>> = std::sync::Mutex::new(None);
/// Run once, in tests, right before the guarded push, to move the remote.
#[cfg(test)]
pub(super) static BEFORE_PUSH: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn crash_after(stage: Stage) -> Result<(), String> {
    if *CRASH_AFTER.lock().unwrap() == Some(stage) {
        return Err(format!("injected crash after {stage:?}"));
    }
    Ok(())
}

/// Why a restack is not on offer.
enum Refusal {
    /// Whether the task needs a restack could not be worked out, for example
    /// because GitHub could not be asked.
    Unknown(String),
    /// The task needs one, and cannot have it as things are.
    Blocked(String),
}

impl Refusal {
    fn into_message(self) -> String {
        match self {
            Refusal::Unknown(m) | Refusal::Blocked(m) => m,
        }
    }
}

/// Everything a restack acts on, observed together, with nothing written.
struct Plan {
    working_dir: String,
    branch: String,
    parent_branch: String,
    parent_pr: u64,
    default_branch: String,
    pr_number: u64,
    fork_point: String,
    /// The branch's tip, which is also the remote's.
    tip: String,
    onto: String,
    worktree: Option<PathBuf>,
    rewrites: bool,
}

fn blocked<T>(message: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal::Blocked(message.into()))
}

/// The task's branch, worktree and stacking, as far as a restack is
/// concerned.
struct Stacked {
    branch: String,
    parent_branch: String,
    fork_point: Option<String>,
    worktree: Option<PathBuf>,
    linked: bool,
}

/// `None` when the task is not stacked or has no branch.
async fn stacked_task(state: &crate::AppState, task_uuid: Uuid) -> Result<Option<Stacked>, String> {
    let tasks = state.task.tasks.read().await;
    let task = tasks.get(&task_uuid).ok_or("Task not found")?;
    let (Some(BranchOrigin::Stacked { parent_branch }), Some(branch)) =
        (&task.branch_origin, &task.branch_name)
    else {
        return Ok(None);
    };
    Ok(Some(Stacked {
        branch: branch.clone(),
        parent_branch: parent_branch.clone(),
        fork_point: task.base_commit.clone(),
        worktree: task.worktree_path.as_ref().map(PathBuf::from).filter(|p| p.is_dir()),
        linked: task.pr_url.is_some() || task.external_refs.iter().any(crate::domain::ExternalRef::is_pr),
    }))
}

/// Observe whether and how the task can be restacked. `Ok(None)` means there
/// is nothing to do: the task is not a published stacked task, its parent has
/// not landed on the default branch, or its pull request already targets it
/// and needs no rewrite.
async fn observe(state: &crate::AppState, task_uuid: Uuid) -> Result<Option<Plan>, Refusal> {
    let Some(stacked) = stacked_task(state, task_uuid).await.map_err(Refusal::Unknown)? else {
        return Ok(None);
    };
    if !stacked.linked {
        return Ok(None);
    }
    let working_dir = resolve_repository_dir(state, task_uuid).await.map_err(Refusal::Unknown)?;
    let branch = checked_task_branch(&stacked.branch).map_err(Refusal::Unknown)?.to_string();

    let child = branch_pr_state(&working_dir, &branch).await.map_err(Refusal::Unknown)?;
    let (Some("OPEN"), Some(pr_number)) = (child.state.as_deref(), child.number) else {
        return Ok(None);
    };
    let child_base = child.base.clone().unwrap_or_default();

    let origin = BranchOrigin::Stacked { parent_branch: stacked.parent_branch.clone() };
    let base = pr_base_for(&working_dir, Some(&origin), stacked.fork_point.as_deref(), true)
        .await
        .map_err(Refusal::Unknown)?;
    let Some(landed) = base.landed_parent else {
        return Ok(None);
    };
    let default = checked_task_branch(&landed.default_branch).map_err(Refusal::Unknown)?.to_string();
    let context = format!(
        "This task is stacked on {}, whose pull request #{} was merged into {default}",
        landed.parent_branch, landed.number
    );
    let git_err = |e: String| Refusal::Blocked(format!("{context}, but {e}"));

    let Some(fork_point) = stacked.fork_point.filter(|c| restack::is_full_object_id(c)) else {
        return blocked(format!(
            "{context}, but the task has no recorded fork point, so SlashIt cannot tell which \
             commits are its own."
        ));
    };
    let repo = Path::new(&working_dir);
    let branch_ref = format!("refs/heads/{branch}");
    let Some(tip) = restack::exact_ref(repo, &branch_ref).await.map_err(git_err)? else {
        return blocked(format!("{context}, but the task's branch does not exist locally."));
    };
    if !restack::has_commit(repo, &fork_point).await.map_err(git_err)?
        || !restack::is_ancestor(repo, &fork_point, &tip).await.map_err(git_err)?
    {
        return blocked(format!(
            "{context}, but {branch} no longer contains the commit it was started from \
             ({fork_point}), so SlashIt cannot tell which commits are its own. Move it onto \
             {default} yourself, then retarget its pull request."
        ));
    }

    let merge_commit = merged_pull_request(&working_dir, &landed).await.map_err(|e| {
        Refusal::Unknown(format!("{context}, and SlashIt could not confirm what landed: {e}"))
    })?;
    let onto = restack::fetch_remote_branch(repo, &default)
        .await
        .map_err(|e| Refusal::Unknown(format!("{context}. {e}")))?;
    if !restack::has_commit(repo, &merge_commit).await.map_err(git_err)?
        || !restack::is_ancestor(repo, &merge_commit, &onto).await.map_err(git_err)?
    {
        return blocked(format!(
            "{context} as {merge_commit}, but {default} on origin ({onto}) does not contain that \
             commit, so SlashIt cannot tell what this task's branch should be restacked onto."
        ));
    }

    let plan = |rewrites: bool, tip: String, worktree: Option<PathBuf>| Plan {
        working_dir: working_dir.clone(),
        branch: branch.clone(),
        parent_branch: landed.parent_branch.clone(),
        parent_pr: landed.number,
        default_branch: default.clone(),
        pr_number,
        fork_point: fork_point.clone(),
        tip,
        onto: onto.clone(),
        worktree,
        rewrites,
    };

    if restack::is_ancestor(repo, &fork_point, &onto).await.map_err(git_err)? {
        // The parent's commits are on the default branch as they are, so the
        // branch already differs from it by exactly its own commits.
        return Ok((child_base != default).then(|| plan(false, tip, None)));
    }

    match remote_branch_commit(&working_dir, &branch).await {
        Err(e) => return Err(Refusal::Unknown(format!("{context}. {e}"))),
        Ok(None) => {
            return blocked(format!(
                "{context}, but {branch} is not on origin, so there is no published branch to update."
            ))
        }
        Ok(Some(remote)) if remote != tip => {
            return blocked(format!(
                "{context}, but {branch} is at {tip} here and {remote} on origin. Push or fetch it \
                 first, so that a restack never publishes or discards commits nobody approved."
            ))
        }
        Ok(Some(_)) => {}
    }
    match parent_pull_request_commits(&working_dir, landed.number).await {
        Err(e) => {
            return Err(Refusal::Unknown(format!(
                "{context}, and SlashIt could not list that pull request's commits: {e}"
            )))
        }
        Ok(commits) if commits.contains(&fork_point) => {}
        Ok(commits) if commits.len() >= GITHUB_PR_COMMIT_LIST_LIMIT => {
            return blocked(format!(
                "{context}, but GitHub lists at most {GITHUB_PR_COMMIT_LIST_LIMIT} commits of a \
                 pull request and #{} has at least that many, so SlashIt cannot prove that the \
                 commit this task started from, {fork_point}, is among them.",
                landed.number
            ))
        }
        Ok(_) => {
            return blocked(format!(
                "{context}, but the commit this task's branch was started from, {fork_point}, is \
                 not among the commits of pull request #{}: {} was rewritten after this task \
                 started from it, and a restack would drop that commit's changes from under the \
                 task.",
                landed.number, landed.parent_branch
            ))
        }
    }
    if let Some((title, id, dependent_branch)) = stacked_dependent(state, task_uuid, &branch).await {
        return blocked(format!(
            "{context}, but task \"{title}\" ({id}) is stacked on {branch}, on branch \
             {dependent_branch}. Rewriting {branch} would strand it on the old commits."
        ));
    }
    if let Some(other) = restack::branches_containing(repo, &tip)
        .await
        .map_err(git_err)?
        .into_iter()
        .find(|b| b != &branch_ref)
    {
        return blocked(format!(
            "{context}, but branch {other} is built on it (it contains {branch}'s tip {tip}). \
             Rewriting {branch} would strand it on the old commits."
        ));
    }
    if let Some(merge) = restack::merges_between(repo, &fork_point, &tip).await.map_err(git_err)?.first() {
        return blocked(format!(
            "{context}, but the branch has a merge commit of its own ({merge}), which a restack \
             would flatten. Move the branch onto {default} yourself."
        ));
    }
    let Some(worktree) = stacked.worktree else {
        return blocked(format!(
            "{context}, but this task has no worktree to restack in. Attach a worktree to the \
             task first."
        ));
    };
    check_worktree(&worktree, &branch, &tip, &context).await.map_err(Refusal::Blocked)?;
    Ok(Some(plan(true, tip, Some(worktree))))
}

/// The worktree must have `branch` checked out at `tip`, clean and in the
/// middle of nothing.
async fn check_worktree(worktree: &Path, branch: &str, tip: &str, context: &str) -> Result<(), String> {
    let shown = worktree.display();
    let checkout = restack::worktree_state(worktree).await.map_err(|e| format!("{context}, but {e}"))?;
    if let Some(operation) = checkout.in_progress {
        return Err(format!(
            "{context}, but the task's worktree at {shown} is in the middle of a {operation}. \
             Finish it or abort it there first."
        ));
    }
    if checkout.head_ref.as_deref() != Some(format!("refs/heads/{branch}").as_str()) || checkout.head != tip {
        return Err(format!(
            "{context}, but the task's worktree at {shown} does not have {branch} checked out at \
             {tip}. Check out {branch} there first."
        ));
    }
    if !checkout.uncommitted.is_empty() {
        return Err(format!(
            "{context}, but the task's worktree at {shown} has uncommitted changes ({}). SlashIt \
             does not set work aside on its own. Commit or remove them first.",
            checkout.uncommitted.join("; ")
        ));
    }
    Ok(())
}

/// Why Discard is not offered while origin cannot be asked whether it already
/// has the restacked branch.
fn remote_unavailable(error: &str) -> String {
    format!(
        "Origin could not be checked, so whether going back is safe is unknown: {error}. Resume \
         or try again once origin is reachable."
    )
}

/// What the pull request surface should offer for the task, by observation
/// only: nothing is written, and the only network use is asking GitHub.
pub(super) async fn status(
    state: &crate::AppState,
    task_uuid: Uuid,
) -> Result<Option<RepublishStatus>, String> {
    let pending = state.task.tasks.read().await.get(&task_uuid).and_then(|t| t.pending_republish.clone());
    if let Some(pending) = pending {
        let rewritten = pending.rewritten_tip.is_some();
        // Discard itself fails closed when origin cannot be asked, so a failed
        // query is reported as unavailable rather than as "allowed".
        let discard_blocked = match &pending.rewritten_tip {
            Some(tip) => match Context::of(state, task_uuid).await {
                Ok(ctx) => match remote_branch_commit(&ctx.working_dir, &ctx.branch).await {
                    Ok(Some(remote)) if remote == *tip => Some(
                        "Origin already has the restacked branch, so going back would leave this \
                         branch behind it. Resume to finish."
                            .to_string(),
                    ),
                    Ok(_) => None,
                    Err(e) => Some(remote_unavailable(&e)),
                },
                Err(e) => Some(remote_unavailable(&e)),
            },
            None => None,
        };
        let detail = if rewritten {
            "The branch was rewritten here, and origin may not have it yet. Until this is resumed \
             or discarded, agent runs, reviews and pull request actions for this task are refused. \
             Resuming updates origin only if it still has the tip you approved, then retargets the \
             pull request."
        } else {
            "A restack was started and the branch has not been rewritten, or not recorded, yet. \
             Until this is resumed or discarded, agent runs, reviews and pull request actions for \
             this task are refused. Resuming continues it from where it stopped."
        };
        return Ok(Some(RepublishStatus::Interrupted {
            rewritten,
            detail: detail.to_string(),
            discard_blocked,
        }));
    }
    match observe(state, task_uuid).await {
        Ok(None) => Ok(None),
        Ok(Some(plan)) => Ok(Some(RepublishStatus::NeedsRestack {
            parent_branch: plan.parent_branch,
            parent_pr: plan.parent_pr,
            default_branch: plan.default_branch,
            pr_number: plan.pr_number,
            rewrites: plan.rewrites,
        })),
        Err(Refusal::Blocked(reason)) => Ok(Some(RepublishStatus::Blocked { reason })),
        Err(Refusal::Unknown(e)) => Err(e),
    }
}

/// Restack the task's published branch onto the default branch its parent
/// landed on and retarget its pull request, or finish doing so after an
/// interruption. Only ever called for a user's explicit request.
pub(super) async fn restack(state: &crate::AppState, task_uuid: Uuid) -> Result<RepublishOutcome, String> {
    let reservation = reserve_task_for_republish(state, task_uuid).await?;
    restack_reserved(state, task_uuid, &reservation).await
}

async fn restack_reserved(
    state: &crate::AppState,
    task_uuid: Uuid,
    reservation: &Option<crate::queue::PrHelperLease>,
) -> Result<RepublishOutcome, String> {
    let pending = state.task.tasks.read().await.get(&task_uuid).and_then(|t| t.pending_republish.clone());
    if let Some(pending) = pending {
        let ctx = Context::of(state, task_uuid).await?;
        return advance(state, task_uuid, &ctx, pending, reservation).await;
    }
    let Some(plan) = observe(state, task_uuid).await.map_err(Refusal::into_message)? else {
        return Err("There is nothing to restack: this task's parent has not landed on the default \
                    branch, or its pull request already targets it."
            .to_string());
    };
    if !plan.rewrites {
        refuse_if_pr_operation_cancelled(reservation, "retargeting the pull request")?;
        converge_pr_base(&plan.working_dir, plan.pr_number, &plan.branch, &plan.default_branch).await?;
        return Ok(RepublishOutcome {
            rewritten: false,
            base: plan.default_branch,
            previous_tip: plan.tip.clone(),
            new_tip: plan.tip,
        });
    }

    let pending = PendingRepublish {
        parent_branch: plan.parent_branch.clone(),
        parent_pr: plan.parent_pr,
        pr_number: plan.pr_number,
        default_branch: plan.default_branch.clone(),
        fork_point: plan.fork_point.clone(),
        previous_tip: plan.tip.clone(),
        onto: plan.onto.clone(),
        rewritten_tip: None,
    };
    refuse_if_pr_operation_cancelled(reservation, "restacking the branch")?;
    let recorded = std::sync::atomic::AtomicBool::new(false);
    let stacked = BranchOrigin::Stacked { parent_branch: plan.parent_branch.clone() };
    let apply = |staged: &mut std::collections::HashMap<Uuid, Task>| {
        if let Some(task) = staged.get_mut(&task_uuid) {
            if task.branch_name.as_deref() == Some(plan.branch.as_str())
                && task.base_commit.as_deref() == Some(plan.fork_point.as_str())
                && task.branch_origin.as_ref() == Some(&stacked)
                && task.pending_republish.is_none()
            {
                task.pending_republish = Some(pending.clone());
                recorded.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    };
    crate::lifecycle::record(&state.task.tasks, &state.storage, task_uuid, &apply)
        .await
        .map_err(|e| format!("The restack could not be recorded, so nothing was changed: {e}"))?;
    if !recorded.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("The task changed while its restack was being planned, so nothing was changed.".to_string());
    }
    #[cfg(test)]
    crash_after(Stage::Planned)?;

    let ctx = Context { working_dir: plan.working_dir, branch: plan.branch, worktree: plan.worktree };
    advance(state, task_uuid, &ctx, pending, reservation).await
}

/// Where the task's branch lives.
struct Context {
    working_dir: String,
    branch: String,
    worktree: Option<PathBuf>,
}

impl Context {
    async fn of(state: &crate::AppState, task_uuid: Uuid) -> Result<Self, String> {
        let working_dir = resolve_repository_dir(state, task_uuid).await?;
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_uuid).ok_or("Task not found")?;
        let branch = task.branch_name.clone().ok_or("This task has no branch recorded.")?;
        Ok(Context {
            working_dir,
            branch: checked_task_branch(&branch)?.to_string(),
            worktree: task.worktree_path.as_ref().map(PathBuf::from).filter(|p| p.is_dir()),
        })
    }
}

/// Where the approved tip can be found, from what the repository holds now:
/// the backup ref is named only if it resolves to `previous_tip`. A restack
/// that stopped before its backup was created has only the commit itself.
async fn approved_tip_note(repo: &Path, backup: &str, previous_tip: &str) -> String {
    match restack::exact_ref(repo, backup).await {
        Ok(Some(saved)) if saved == previous_tip => {
            format!("The approved tip {previous_tip} is kept in {backup}")
        }
        Ok(_) => format!("The approved tip is {previous_tip}; no backup ref holds it"),
        Err(e) => {
            format!("The approved tip is {previous_tip}; the backup ref {backup} could not be read ({e})")
        }
    }
}

/// Carry a pending restack as far as it can go from what is actually in the
/// repository, and finish it.
async fn advance(
    state: &crate::AppState,
    task_uuid: Uuid,
    ctx: &Context,
    mut pending: PendingRepublish,
    reservation: &Option<crate::queue::PrHelperLease>,
) -> Result<RepublishOutcome, String> {
    let repo = Path::new(&ctx.working_dir);
    let branch = ctx.branch.as_str();
    let branch_ref = format!("refs/heads/{branch}");
    let backup = restack::republish_backup_ref(task_uuid);
    let context = format!(
        "This task is stacked on {}, whose pull request #{} was merged into {}, and its branch \
         {branch} is being restacked",
        pending.parent_branch, pending.parent_pr, pending.default_branch
    );
    let tip = restack::exact_ref(repo, &branch_ref)
        .await?
        .ok_or_else(|| format!("{context}, but {branch_ref} does not exist."))?;

    if pending.rewritten_tip.is_none() {
        let worktree = ctx.worktree.as_deref().ok_or_else(|| {
            format!("{context}, but this task has no worktree. Attach one, or discard the restack.")
        })?;
        if tip == pending.previous_tip {
            let remote = remote_branch_commit(&ctx.working_dir, branch).await?;
            if remote.as_deref() != Some(pending.previous_tip.as_str()) {
                return Err(format!(
                    "{context}, but origin's {branch} is no longer at {} (it is at {}). Someone \
                     else changed it, so nothing was rewritten. Discard this restack.",
                    pending.previous_tip,
                    remote.as_deref().unwrap_or("nothing")
                ));
            }
            check_worktree(worktree, branch, &tip, &context).await?;
            match restack::exact_ref(repo, &backup).await?.as_deref() {
                None => {
                    refuse_if_pr_operation_cancelled(reservation, "restacking the branch")?;
                    restack::create_backup(repo, &backup, &tip).await?;
                }
                Some(saved) if saved == tip => {}
                Some(saved) => {
                    return Err(format!(
                        "{context}, but {backup} holds {saved}, not the branch's tip {tip}. \
                         SlashIt will not overwrite it. Discard this restack, or inspect {backup}."
                    ))
                }
            }
            #[cfg(test)]
            crash_after(Stage::BackedUp)?;

            refuse_if_pr_operation_cancelled(reservation, "rewriting the branch")?;
            let replay = restack::Restack::new(
                worktree,
                branch,
                &pending.fork_point,
                &pending.previous_tip,
                &pending.onto,
                &backup,
            );
            let new_tip = match replay.replay().await {
                Ok(new_tip) => new_tip,
                Err(failure) => return Err(replay_failure(state, task_uuid, &pending, &replay, failure).await),
            };
            #[cfg(test)]
            crash_after(Stage::Replayed)?;
            pending = match record_rewrite(state, task_uuid, branch, &pending, &new_tip).await {
                Ok(pending) => pending,
                Err(what) => {
                    let failure = replay.roll_back(what).await;
                    return Err(replay_failure(state, task_uuid, &pending, &replay, failure).await);
                }
            };
        } else {
            // Not at the approved tip, with no record of a rewrite: either a
            // rewrite whose result was not recorded, or someone else's move.
            if let Some(operation) = restack::worktree_state(worktree).await?.in_progress {
                return Err(format!(
                    "{context}, but the task's worktree is in the middle of a {operation}, \
                     probably the restack's own, interrupted. Finish it or abort it there \
                     (`git rebase --abort`), then run the restack again or discard it."
                ));
            }
            if let Err(e) = restack::replay_matches_exactly(
                worktree,
                &pending.fork_point,
                &pending.previous_tip,
                &pending.onto,
                &tip,
            )
            .await
            {
                let approved = approved_tip_note(repo, &backup, &pending.previous_tip).await;
                return Err(format!(
                    "{context}, but {branch} is at {tip}, which is not the approved branch \
                     ({}) replayed onto {} ({e}). Someone else changed it. {approved}. Bring \
                     {branch} back to it by hand, then discard this restack.",
                    pending.previous_tip, pending.onto
                ));
            }
            check_worktree(worktree, branch, &tip, &context).await?;
            pending = record_rewrite(state, task_uuid, branch, &pending, &tip).await?;
        }
        #[cfg(test)]
        crash_after(Stage::Recorded)?;
    }

    let Some(rewritten) = pending.rewritten_tip.clone() else {
        return Err("The restack lost its record of the rewritten branch.".to_string());
    };
    let tip = restack::exact_ref(repo, &branch_ref).await?.unwrap_or_default();
    if tip != rewritten {
        let approved = approved_tip_note(repo, &backup, &pending.previous_tip).await;
        return Err(format!(
            "{context}, but {branch} is at {tip} here, not at {rewritten}, which the restack \
             produced. Something outside SlashIt changed it, and SlashIt will not adopt, publish \
             or discard commits it did not make. {approved}, and the restack's record is kept: \
             bring {branch} back to {rewritten} (or to {}, then discard the restack) by hand, and \
             try again.",
            pending.previous_tip
        ));
    }

    let remote = remote_branch_commit(&ctx.working_dir, branch).await?;
    match remote.as_deref() {
        Some(remote) if remote == rewritten => {}
        Some(remote) if remote == pending.previous_tip => {
            let (state, _) = read_pr(&ctx.working_dir, pending.pr_number, branch).await?;
            if !state.eq_ignore_ascii_case("OPEN") {
                return Err(format!(
                    "{context}, but pull request #{} is {state} now, not open, so there is nothing \
                     left to republish it for. Nothing was pushed. Discard this restack.",
                    pending.pr_number
                ));
            }
            refuse_if_pr_operation_cancelled(reservation, "updating the branch on origin")?;
            #[cfg(test)]
            {
                let hook = BEFORE_PUSH.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook();
                }
            }
            push_with_lease(&ctx.working_dir, branch, &pending.previous_tip).await?;
        }
        other => {
            let approved = approved_tip_note(repo, &backup, &pending.previous_tip).await;
            return Err(format!(
                "{context}, but origin's {branch} is at {} now, which is neither the tip that \
                 was approved ({}) nor the restacked one ({rewritten}). Someone else changed it, \
                 so SlashIt did not overwrite it. Nothing was pushed. Your rewrite is kept here. \
                 {approved}; discard the restack to go back to it.",
                other.unwrap_or("nothing (the branch is gone)"),
                pending.previous_tip
            ))
        }
    }
    #[cfg(test)]
    crash_after(Stage::Pushed)?;

    refuse_if_pr_operation_cancelled(reservation, "retargeting the pull request")?;
    converge_pr_base(&ctx.working_dir, pending.pr_number, branch, &pending.default_branch).await?;
    clear_pending(state, task_uuid, &pending).await?;
    // The backup has held the approved tip through every step that could still
    // need it: the remote update, the retarget and the cleared record. Only now
    // is it dropped. A crash or failure right here leaves a harmless stale ref
    // that nothing reads again, because the record that named it is gone.
    if let Err(e) = restack::retire_backup(repo, &backup, &pending.previous_tip).await {
        eprintln!(
            "[pr] {branch} was restacked and its pull request retargeted, but its backup {backup} \
             (holding {}) could not be deleted: {e}. It is harmless; to delete it, run `git \
             update-ref -d {backup}` in {}.",
            pending.previous_tip,
            repo.display()
        );
    }
    Ok(RepublishOutcome {
        rewritten: true,
        base: pending.default_branch.clone(),
        previous_tip: pending.previous_tip.clone(),
        new_tip: rewritten,
    })
}

/// Record the verified rewrite: `rewritten_tip`, the task's base and its
/// origin, in one compare-and-set write.
async fn record_rewrite(
    state: &crate::AppState,
    task_uuid: Uuid,
    branch: &str,
    pending: &PendingRepublish,
    new_tip: &str,
) -> Result<PendingRepublish, String> {
    let updated = PendingRepublish { rewritten_tip: Some(new_tip.to_string()), ..pending.clone() };
    let recorded = std::sync::atomic::AtomicBool::new(false);
    let stacked = BranchOrigin::Stacked { parent_branch: pending.parent_branch.clone() };
    let apply = |staged: &mut std::collections::HashMap<Uuid, Task>| {
        if let Some(task) = staged.get_mut(&task_uuid) {
            if task.branch_name.as_deref() == Some(branch)
                && task.base_commit.as_deref() == Some(pending.fork_point.as_str())
                && task.branch_origin.as_ref() == Some(&stacked)
                && task.pending_republish.as_ref() == Some(pending)
            {
                task.base_commit = Some(pending.onto.clone());
                task.branch_origin =
                    Some(BranchOrigin::DefaultBase { branch: Some(pending.default_branch.clone()) });
                task.pending_republish = Some(updated.clone());
                recorded.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    };
    crate::lifecycle::record(&state.task.tasks, &state.storage, task_uuid, &apply)
        .await
        .map_err(|e| format!("the task could not record the restack: {e}"))?;
    if !recorded.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(
            "the task's branch, fork point or restack changed while the branch was being restacked"
                .to_string(),
        );
    }
    Ok(updated)
}

/// Clear the pending record, only while it is still the one that finished.
async fn clear_pending(
    state: &crate::AppState,
    task_uuid: Uuid,
    pending: &PendingRepublish,
) -> Result<(), String> {
    let apply = |staged: &mut std::collections::HashMap<Uuid, Task>| {
        if let Some(task) = staged.get_mut(&task_uuid) {
            if task.pending_republish.as_ref() == Some(pending) {
                task.pending_republish = None;
                crate::lifecycle::clear_merged_while_republish_pending(task);
            }
        }
    };
    crate::lifecycle::record(&state.task.tasks, &state.storage, task_uuid, &apply).await.map_err(|e| {
        format!(
            "The restack is done, but the task could not record that: {e}. Running it again \
             finishes it without changing anything."
        )
    })
}

/// The refusal for a replay that did not produce a recorded, verified branch.
/// A branch verified back at the approved tip means nothing was changed, so the
/// pending record goes too; any other outcome keeps the record and the backup.
async fn replay_failure(
    state: &crate::AppState,
    task_uuid: Uuid,
    pending: &PendingRepublish,
    replay: &restack::Restack<'_>,
    failure: restack::RestackFailure,
) -> String {
    use restack::RestackFailure;
    let branch = replay.branch;
    match failure {
        RestackFailure::Restored(what) => {
            let note = match clear_pending(state, task_uuid, pending).await {
                Ok(()) => "Nothing was pushed, and the restack was cancelled.".to_string(),
                Err(e) => format!(
                    "Nothing was pushed, but the restack's record could not be cleared ({e}). \
                     Discard it."
                ),
            };
            format!(
                "Restacking {branch} onto {} failed: {what}. The branch is back at {}, exactly as \
                 it was. {note} Move the branch onto {} yourself, resolve what the replay stops \
                 on, push it with a lease and retarget its pull request by hand.",
                pending.default_branch, pending.previous_tip, pending.default_branch
            )
        }
        RestackFailure::NotRestored(what) | RestackFailure::BranchMoved(what) => format!(
            "Restacking {branch} onto {} failed: {what}. SlashIt could not put the branch back, \
             so it kept the restack's record and {} holding {}. Nothing was pushed. Inspect the \
             worktree, then discard the restack or run it again.",
            pending.default_branch,
            restack::republish_backup_ref(task_uuid),
            pending.previous_tip
        ),
    }
}

/// `git push --force-with-lease=refs/heads/<branch>:<expected>`: the remote
/// branch is replaced only while it is exactly `expected`. A refusal is
/// reported as it is and never retried against a newly observed tip.
async fn push_with_lease(working_dir: &str, branch: &str, expected: &str) -> Result<(), String> {
    let branch = checked_task_branch(branch)?;
    let mut git_args: Vec<String> = Vec::new();
    if is_jj_repo(working_dir).await {
        run_cmd("jj", &["--ignore-working-copy", "git", "export"], working_dir)
            .await
            .map_err(|e| format!("jj git export failed: {e}"))?;
    }
    if let Some(git_dir) = jj_git_dir_arg(working_dir).await? {
        git_args.push(git_dir);
    }
    let lease = format!("--force-with-lease=refs/heads/{branch}:{expected}");
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    git_args.extend(["push".to_string(), lease, "--".to_string(), "origin".to_string(), refspec]);
    let git_args: Vec<&str> = git_args.iter().map(String::as_str).collect();
    run_cmd("git", &git_args, working_dir).await.map(|_| ()).map_err(|e| {
        format!(
            "SlashIt did not overwrite origin's {branch}: it is no longer at {expected}, the tip \
             that was approved, or could not be updated ({e}). Nothing on origin was changed."
        )
    })
}

/// Make the pull request's base the default branch: read it, edit it only if it
/// differs, read it again. A pull request GitHub already moved is as good as
/// one SlashIt moved. A pull request that is no longer open is left alone.
async fn converge_pr_base(working_dir: &str, number: u64, branch: &str, default: &str) -> Result<(), String> {
    let (state, base) = read_pr(working_dir, number, branch).await?;
    if !state.eq_ignore_ascii_case("OPEN") || base == default {
        return Ok(());
    }
    let number = number.to_string();
    run_cmd("gh", &["pr", "edit", &number, "--base", default], working_dir).await.map_err(|e| {
        format!(
            "The branch is updated on origin, but pull request #{number} could not be retargeted \
             to {default}: {e}. Run the restack again to retry that step."
        )
    })?;
    let (_, base) = read_pr(working_dir, number.parse().unwrap_or_default(), branch).await?;
    if base != default {
        return Err(format!(
            "The branch is updated on origin, but pull request #{number} still targets {base}, \
             not {default}. Run the restack again to retry that step."
        ));
    }
    Ok(())
}

/// The pull request's state and base, after checking it is `branch`'s own.
async fn read_pr(working_dir: &str, number: u64, branch: &str) -> Result<(String, String), String> {
    let number = number.to_string();
    let out = run_cmd("gh", &["pr", "view", &number, "--json", "state,baseRefName,headRefName"], working_dir)
        .await
        .map_err(|e| format!("Could not read pull request #{number}: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("Failed to parse gh pr view output: {e}"))?;
    let text = |name: &str| json.get(name).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    if text("headRefName") != branch {
        return Err(format!(
            "Pull request #{number} is from {:?}, not from {branch}, so SlashIt will not touch it.",
            text("headRefName")
        ));
    }
    Ok((text("state"), text("baseRefName")))
}

/// Go back to the approved tip: undo a restack that was planned or done
/// locally and not yet published, and drop its record. Never touches origin.
pub(super) async fn discard(state: &crate::AppState, task_uuid: Uuid) -> Result<(), String> {
    let reservation = reserve_task_for_republish(state, task_uuid).await?;
    let pending = state
        .task
        .tasks
        .read()
        .await
        .get(&task_uuid)
        .and_then(|t| t.pending_republish.clone())
        .ok_or("There is no restack to discard.")?;
    let ctx = Context::of(state, task_uuid).await?;
    let repo = Path::new(&ctx.working_dir);
    let branch = ctx.branch.as_str();
    let backup = restack::republish_backup_ref(task_uuid);
    let tip = restack::exact_ref(repo, &format!("refs/heads/{branch}"))
        .await?
        .ok_or_else(|| format!("{branch} does not exist locally."))?;

    if let Some(rewritten) = &pending.rewritten_tip {
        let remote = remote_branch_commit(&ctx.working_dir, branch).await?;
        if remote.as_deref() == Some(rewritten.as_str()) {
            return Err(format!(
                "Origin already has the restacked {branch} ({rewritten}). Discarding would leave \
                 this branch behind origin; run the restack again to finish it instead."
            ));
        }
    }
    // A rebase left stopped by a crash keeps the branch ref where it was, so
    // it is checked whatever the tip is: discarding over it would end the
    // exclusive state while the rebase is still live in the worktree.
    if let Some(worktree) = ctx.worktree.as_deref() {
        if let Some(operation) = restack::worktree_state(worktree).await?.in_progress {
            let approved = approved_tip_note(repo, &backup, &pending.previous_tip).await;
            return Err(format!(
                "The task's worktree is in the middle of a {operation}, probably the restack's own, \
                 interrupted. Finish it or abort it there (`git rebase --abort`) first. The restack's \
                 record is kept. {approved}."
            ));
        }
    }
    if tip != pending.previous_tip {
        let worktree = ctx.worktree.as_deref().ok_or("This task has no worktree to restore the branch in.")?;
        let produced = match &pending.rewritten_tip {
            Some(rewritten) => *rewritten == tip,
            None => restack::replay_matches_exactly(
                worktree,
                &pending.fork_point,
                &pending.previous_tip,
                &pending.onto,
                &tip,
            )
            .await
            .is_ok(),
        };
        if !produced {
            let approved = approved_tip_note(repo, &backup, &pending.previous_tip).await;
            return Err(format!(
                "{branch} is at {tip}, which is not the restack's result, so SlashIt will not move \
                 it or discard what it holds. {approved}, and the restack's record is kept. \
                 Bring {branch} back to the restack's result by hand, or to {} and discard again.",
                pending.previous_tip
            ));
        }
        check_worktree(worktree, branch, &tip, "The restack is being discarded").await?;
        refuse_if_pr_operation_cancelled(&reservation, "restoring the branch")?;
        restack::move_branch_back(worktree, branch, &tip, &pending.previous_tip).await?;
    }

    let stacked = BranchOrigin::Stacked { parent_branch: pending.parent_branch.clone() };
    let apply = |staged: &mut std::collections::HashMap<Uuid, Task>| {
        if let Some(task) = staged.get_mut(&task_uuid) {
            if task.pending_republish.as_ref() == Some(&pending) {
                task.pending_republish = None;
                crate::lifecycle::clear_merged_while_republish_pending(task);
                if pending.rewritten_tip.is_some() {
                    task.base_commit = Some(pending.fork_point.clone());
                    task.branch_origin = Some(stacked.clone());
                }
            }
        }
    };
    crate::lifecycle::record(&state.task.tasks, &state.storage, task_uuid, &apply).await.map_err(|e| {
        format!(
            "The branch is back at {}, but the task could not record that: {e}. Discard again.",
            pending.previous_tip
        )
    })?;
    if let Err(e) = restack::retire_backup(repo, &backup, &pending.previous_tip).await {
        eprintln!("[pr] discarded the restack of {branch}; its backup {backup} could not be deleted: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JSON the frontend's `components::restack_published` tests read.
    #[test]
    fn the_status_serializes_as_the_frontend_reads_it() {
        let needs = RepublishStatus::NeedsRestack {
            parent_branch: "task-parent".to_string(),
            parent_pr: 7,
            default_branch: "main".to_string(),
            pr_number: 21,
            rewrites: true,
        };
        assert_eq!(
            serde_json::to_string(&needs).unwrap(),
            r#"{"kind":"needs_restack","parent_branch":"task-parent","parent_pr":7,"default_branch":"main","pr_number":21,"rewrites":true}"#
        );
        assert_eq!(
            serde_json::to_string(&RepublishStatus::Blocked { reason: "dirty".to_string() }).unwrap(),
            r#"{"kind":"blocked","reason":"dirty"}"#
        );
        assert_eq!(
            serde_json::to_string(&RepublishStatus::Interrupted {
                rewritten: true,
                detail: "d".to_string(),
                discard_blocked: Some("why".to_string()),
            })
            .unwrap(),
            r#"{"kind":"interrupted","rewritten":true,"detail":"d","discard_blocked":"why"}"#
        );
    }
}
