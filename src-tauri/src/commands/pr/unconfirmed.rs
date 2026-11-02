// SPDX-License-Identifier: MPL-2.0
//! A person's recovery from a pull request request whose outcome SlashIt
//! never learned (`Task::pr_create_unconfirmed`).

use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::commands::gui_admission::GuiMutation;
use crate::commands::pr::{
    find_existing_pr_for_branch, github_repository_of_remote_url, is_pull_request_url,
    jj_git_dir_arg, link_pr_to_task_reserved, pr_link_error, program, redact_remote_url,
    redact_urls_in, refuse_if_pr_operation_cancelled, reserve_task_admitted_as,
    resolve_repository_dir, run_cmd, PrSideEffectAdmission,
};
use crate::domain::task::{ActivityKind, UnconfirmedPrCreate};
use crate::domain::{Task, TaskStatus};
use crate::lifecycle::{self, RecordWriteError};

/// The attempt a person was shown, exactly as the task record held it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnconfirmedToken {
    pub branch: String,
    pub commit: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnconfirmedResolution {
    LinkExisting { pr_url: String },
    ConfirmNoPullRequest { acknowledged: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum UnconfirmedOutcome {
    Linked { url: String },
    Discarded { durable: bool },
    PullRequestFound { url: String },
    NotThisPullRequest { reason: String },
    Stale { reason: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolveOutcome {
    pub task: Option<Task>,
    pub result: UnconfirmedOutcome,
}

#[tauri::command]
pub async fn resolve_unconfirmed_pr_create(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    token: UnconfirmedToken,
    resolution: UnconfirmedResolution,
) -> Result<ResolveOutcome, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    resolve(&state, task_id, token, resolution).await
}

/// Settle the unanswered request `token` names, as the person chose.
///
/// Neither resolution pushes a branch, runs `gh pr create` or makes any other
/// remote change. Linking reads the pull request the person named and links it
/// only when it is verified to be this branch's own (see [`verify_pull_request`]).
/// Confirming that none exists is the person's decision, not GitHub's: the one
/// lookup made first can only refuse (it found a pull request), and an empty
/// answer is never taken as proof.
///
/// Both are bound to the exact record the person was shown: if the task no
/// longer holds it (settled, replaced by a newer attempt, or never the task's),
/// the answer is [`UnconfirmedOutcome::Stale`] and nothing changes. The task is
/// reserved as for any pull request side effect, so a running task, or another
/// pull request operation holding it, refuses the recovery, and a lifecycle
/// transition that ends it before the write stops it.
pub(super) async fn resolve(
    state: &crate::AppState,
    task_id: Uuid,
    token: UnconfirmedToken,
    resolution: UnconfirmedResolution,
) -> Result<ResolveOutcome, String> {
    if matches!(
        resolution,
        UnconfirmedResolution::ConfirmNoPullRequest {
            acknowledged: false
        }
    ) {
        return Err(
            "Nothing was discarded: confirm that you checked GitHub yourself and understand that \
             a second pull request may be created."
                .to_string(),
        );
    }
    let running = state
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .map(|task| {
            matches!(
                task.status,
                TaskStatus::Queue | TaskStatus::InProgress | TaskStatus::AiReview
            )
        })
        .ok_or("Task not found")?;
    if running {
        return Err(
            "The task is running or waiting to run, so its pull request cannot be recovered now. \
             Stop it first."
                .to_string(),
        );
    }
    let reservation =
        reserve_task_admitted_as(state, task_id, PrSideEffectAdmission::UnconfirmedRecovery)
            .await?;

    // Read under the reservation: nothing else can settle or replace the
    // record from here on.
    let task = state
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .ok_or("Task not found")?;
    let attempt = match &task.pr_create_unconfirmed {
        None => {
            return Ok(stale(
                state,
                task_id,
                "This task has no unanswered request to open a pull request any more. It was \
                 settled, or this recovery already ran.",
            )
            .await)
        }
        Some(record)
            if (
                record.branch.as_str(),
                record.commit.as_str(),
                record.started_at,
            ) != (
                token.branch.as_str(),
                token.commit.as_str(),
                token.started_at,
            ) =>
        {
            return Ok(stale(
                state,
                task_id,
                "The unanswered request changed since you were shown it. Review it as it is now.",
            )
            .await)
        }
        Some(record) => record.clone(),
    };
    let working_dir = resolve_repository_dir(state, task_id).await?;

    match resolution {
        UnconfirmedResolution::LinkExisting { pr_url } => {
            link_existing(state, &task, &attempt, &working_dir, &pr_url, reservation).await
        }
        UnconfirmedResolution::ConfirmNoPullRequest { .. } => {
            confirm_none(state, &task, &attempt, &working_dir, reservation).await
        }
    }
}

async fn stale(state: &crate::AppState, task_id: Uuid, reason: &str) -> ResolveOutcome {
    ResolveOutcome {
        task: state.task.tasks.read().await.get(&task_id).cloned(),
        result: UnconfirmedOutcome::Stale {
            reason: reason.to_string(),
        },
    }
}

async fn current(state: &crate::AppState, task_id: Uuid) -> Option<Task> {
    state.task.tasks.read().await.get(&task_id).cloned()
}

async fn link_existing(
    state: &crate::AppState,
    task: &Task,
    attempt: &UnconfirmedPrCreate,
    working_dir: &str,
    pr_url: &str,
    reservation: Option<crate::queue::PrHelperLease>,
) -> Result<ResolveOutcome, String> {
    let url = match verify_pull_request(working_dir, task, attempt, pr_url).await? {
        Verification::Refused(reason) => {
            return Ok(ResolveOutcome {
                task: current(state, task.id).await,
                result: UnconfirmedOutcome::NotThisPullRequest { reason },
            })
        }
        Verification::Verified(url) => url,
    };
    refuse_if_pr_operation_cancelled(&reservation, "linking the pull request")?;
    // The existing link: it records the pull request, settles the unanswered
    // request in the same write, and ends the reservation under the lease.
    if let Err(failure) = link_pr_to_task_reserved(state, task.id, &url, reservation).await {
        if let Some(message) = pr_link_error(&failure, &url) {
            return Err(message);
        }
    }
    Ok(ResolveOutcome {
        task: current(state, task.id).await,
        result: UnconfirmedOutcome::Linked { url },
    })
}

async fn confirm_none(
    state: &crate::AppState,
    task: &Task,
    attempt: &UnconfirmedPrCreate,
    working_dir: &str,
    reservation: Option<crate::queue::PrHelperLease>,
) -> Result<ResolveOutcome, String> {
    // One read-only lookup, to refuse and offer the link when GitHub lists the
    // branch's pull request. An empty answer is not proof that none exists, and
    // is not taken as one: the discard rests on the person's confirmation. An
    // answer that cannot be had refuses too, since the lookup is what could
    // have found the pull request.
    match find_existing_pr_for_branch(working_dir, &attempt.branch).await {
        Ok(Some(url)) => {
            return Ok(ResolveOutcome {
                task: current(state, task.id).await,
                result: UnconfirmedOutcome::PullRequestFound { url },
            })
        }
        Ok(None) => {}
        Err(e) => return Err(format!("Nothing was discarded. {}", redact_urls_in(&e))),
    }
    #[cfg(test)]
    after_lookup_hook::run(working_dir, &reservation);
    refuse_if_pr_operation_cancelled(&reservation, "discarding the record")?;

    let applied = AtomicBool::new(false);
    let task_id = task.id;
    let discarded = attempt.clone();
    let apply = |staged: &mut std::collections::HashMap<Uuid, Task>| {
        let Some(task) = staged.get_mut(&task_id) else {
            return;
        };
        if task.pr_create_unconfirmed.as_ref() != Some(&discarded) {
            return;
        }
        task.pr_create_unconfirmed = None;
        task.record_activity(ActivityKind::PrCreateUnconfirmedDiscarded {
            branch: discarded.branch.clone(),
            commit: discarded.commit.clone(),
            started_at: discarded.started_at,
        });
        // The reason a failed delivery showed still tells the person to try
        // again after an unanswered request; after this decision it says what
        // they decided.
        if task.human_review.pr_error.is_some() {
            task.human_review.pr_error = Some(format!(
                "You confirmed that no pull request exists for {} and discarded the unanswered \
                 request. Nothing was sent to GitHub. Retry to open one: SlashIt looks for an \
                 existing pull request first, and the usual approval still applies.",
                discarded.branch
            ));
        }
        applied.store(true, Ordering::SeqCst);
    };
    let durable =
        match lifecycle::record_typed(&state.task.tasks, &state.storage, task_id, &apply).await {
            Ok(()) => true,
            Err(RecordWriteError::NotPublished(e)) => {
                return Err(format!(
                    "SlashIt could not save your confirmation, so nothing was changed and the \
                 unanswered request is still recorded: {e}"
                ))
            }
            Err(RecordWriteError::PublishedNotDurable(_)) => false,
        };
    drop(reservation);
    if !applied.load(Ordering::SeqCst) {
        return Ok(stale(
            state,
            task_id,
            "The unanswered request changed while the confirmation was being saved, so it was \
             not discarded.",
        )
        .await);
    }
    Ok(ResolveOutcome {
        task: current(state, task_id).await,
        result: UnconfirmedOutcome::Discarded { durable },
    })
}

/// What reading the pull request a person named established.
enum Verification {
    /// It is this branch's own pull request, in this repository, at this
    /// work. Carries its canonical URL.
    Verified(String),
    /// It is not provably so, and why.
    Refused(String),
}

fn owner_and_repo_of(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let mut parts = rest.split('/');
    let (_host, owner, repo) = (parts.next()?, parts.next()?, parts.next()?);
    Some((owner.to_ascii_lowercase(), repo.to_ascii_lowercase()))
}

/// Check that `pr_url` is the pull request the unanswered `attempt` may have
/// opened, before anything is linked.
///
/// It must be a pull request URL, in the repository `gh` works in and origin
/// pushes to; GitHub must answer for exactly that pull request; its head
/// branch must be the task's branch, in this repository and not a fork's; and
/// its head commit must be the attempted commit or contain it. A URL that
/// merely looks valid is never enough. `Err` is a failure to ask (`gh` or
/// `git` did not answer); `Refused` is an answer that says it is not this
/// branch's pull request, or that SlashIt cannot establish that it is.
async fn verify_pull_request(
    working_dir: &str,
    task: &Task,
    attempt: &UnconfirmedPrCreate,
    pr_url: &str,
) -> Result<Verification, String> {
    let refused = |reason: String| Ok(Verification::Refused(reason));
    let url = pr_url.trim().trim_end_matches('/');
    if !is_pull_request_url(url) {
        return refused(format!(
            "{} is not a pull request URL of the form https://host/owner/repo/pull/number.",
            redact_remote_url(pr_url)
        ));
    }
    let Some(branch) = task.branch_name.as_deref() else {
        return refused(
            "The task has no branch recorded, so no pull request can be its own.".into(),
        );
    };
    let (owner, repo) = owner_and_repo_of(url).ok_or("the pull request URL names no repository")?;

    // The repository gh works in.
    let viewed = run_cmd(
        "gh",
        &["repo", "view", "--json", "nameWithOwner"],
        working_dir,
    )
    .await
    .map_err(|e| format!("gh could not name its repository: {}", redact_urls_in(&e)))?;
    let named = serde_json::from_str::<serde_json::Value>(&viewed)
        .ok()
        .and_then(|json| json.get("nameWithOwner")?.as_str().map(str::to_string));
    let Some((gh_owner, gh_repo)) = named
        .as_deref()
        .and_then(|name| name.split_once('/'))
        .map(|(o, r)| (o.to_ascii_lowercase(), r.to_ascii_lowercase()))
    else {
        return refused(
            "SlashIt cannot check which repository gh works in, so it cannot tell that the \
             pull request is this repository's."
                .into(),
        );
    };
    if (owner.as_str(), repo.as_str()) != (gh_owner.as_str(), gh_repo.as_str()) {
        return refused(format!(
            "The pull request is in {owner}/{repo}, but gh works in {gh_owner}/{gh_repo}, so it \
             is not in the repository this branch is delivered to."
        ));
    }

    // The repository origin pushes to.
    let git_dir = jj_git_dir_arg(working_dir).await?;
    let mut get_url: Vec<&str> = git_dir.iter().map(String::as_str).collect();
    get_url.extend(["remote", "get-url", "--push", "origin"]);
    let push_url = run_cmd("git", &get_url, working_dir).await.map_err(|e| {
        format!(
            "origin's push URL could not be read: {}",
            redact_urls_in(&e)
        )
    })?;
    match github_repository_of_remote_url(&push_url) {
        None => {
            return refused(
                "SlashIt cannot check which repository origin pushes to (its push URL is not an \
                 owner/repository URL), so it cannot tell that the pull request is from the \
                 branch it pushed."
                    .into(),
            )
        }
        Some(pushed_to) if pushed_to != (gh_owner.clone(), gh_repo.clone()) => {
            return refused(format!(
                "Origin pushes to {}/{}, but the pull request is in {owner}/{repo}, so it is not \
                 from the branch SlashIt pushed.",
                pushed_to.0, pushed_to.1
            ))
        }
        Some(_) => {}
    }

    // What GitHub says about exactly that pull request.
    let answer = run_cmd(
        "gh",
        &[
            "pr",
            "view",
            url,
            "--json",
            "number,url,state,headRefName,headRefOid,baseRefName,isCrossRepository",
        ],
        working_dir,
    )
    .await
    .map_err(|e| {
        format!(
            "GitHub could not be asked about {url}: {}",
            redact_urls_in(&e)
        )
    })?;
    let json: serde_json::Value = serde_json::from_str(&answer)
        .map_err(|e| format!("gh's answer about {url} could not be read: {e}"))?;
    let text = |name: &str| json.get(name).and_then(|v| v.as_str());
    match text("url").map(|u| u.trim_end_matches('/')) {
        Some(answered) if answered.eq_ignore_ascii_case(url) => {}
        _ => {
            return refused(format!(
                "gh answered with a different pull request than {url}, so nothing was linked."
            ))
        }
    }
    match text("headRefName") {
        Some(head) if head == branch => {}
        Some(head) => {
            return refused(format!(
                "The pull request's head branch is {head}, not this task's branch {branch}."
            ))
        }
        None => return refused("gh did not report the pull request's head branch.".into()),
    }
    match json.get("isCrossRepository").and_then(|v| v.as_bool()) {
        Some(false) => {}
        Some(true) => {
            return refused(
                "The pull request is from a fork or another repository, not from this \
                 repository's branch."
                    .into(),
            )
        }
        None => {
            return refused(
                "gh did not say whether the pull request is from another repository, so \
                 SlashIt cannot tell that it is from this repository's branch."
                    .into(),
            )
        }
    }
    let Some(head_commit) =
        text("headRefOid").filter(|oid| crate::worktree::restack::is_full_object_id(oid))
    else {
        return refused("gh did not report the pull request's head commit.".into());
    };
    if head_commit != attempt.commit {
        match commit_is_ancestor(working_dir, &attempt.commit, head_commit).await {
            Some(true) => {}
            Some(false) => {
                return refused(format!(
                    "The pull request's head commit {} does not contain the commit this request \
                     was made for ({}).",
                    short(head_commit),
                    short(&attempt.commit)
                ))
            }
            None => {
                return refused(format!(
                    "SlashIt cannot establish whether the pull request's head commit {} contains \
                     the commit this request was made for ({}): it is not in this repository.",
                    short(head_commit),
                    short(&attempt.commit)
                ))
            }
        }
    }
    Ok(Verification::Verified(url.to_string()))
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// Whether `ancestor` is `descendant` or an ancestor of it, as the repository
/// knows them; `None` when either commit is not there to compare.
async fn commit_is_ancestor(working_dir: &str, ancestor: &str, descendant: &str) -> Option<bool> {
    let mut command = crate::host_process::tokio_command(program("git"));
    if let Ok(Some(git_dir)) = jj_git_dir_arg(working_dir).await {
        command.arg(git_dir);
    }
    let status = command
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(working_dir)
        .output()
        .await
        .ok()?
        .status;
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// Test-only seam: one hook, armed for one working directory, runs once the
/// lookup that precedes a discard has answered and before the record is
/// written. It is given the operation's reservation, so a test can end the
/// operation in that window deterministically.
#[cfg(test)]
pub(super) mod after_lookup_hook {
    use std::sync::Mutex;

    type Hook = Box<dyn FnOnce(&Option<crate::queue::PrHelperLease>) + Send>;
    static HOOK: Mutex<Option<(String, Hook)>> = Mutex::new(None);

    pub(in crate::commands::pr) fn arm(
        working_dir: &str,
        hook: impl FnOnce(&Option<crate::queue::PrHelperLease>) + Send + 'static,
    ) {
        *HOOK.lock().unwrap() = Some((working_dir.to_string(), Box::new(hook)));
    }

    pub(super) fn run(working_dir: &str, reservation: &Option<crate::queue::PrHelperLease>) {
        let due = {
            let mut slot = HOOK.lock().unwrap();
            match slot.as_ref() {
                Some((dir, _)) if dir == working_dir => slot.take().map(|(_, hook)| hook),
                _ => None,
            }
        };
        if let Some(hook) = due {
            hook(reservation);
        }
    }
}
