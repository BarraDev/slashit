// SPDX-License-Identifier: MPL-2.0
//! Publish one reviewed commit to a Task's existing pull request.
//!
//! The person chooses Publish to PR after seeing the commit, the branch and
//! the pull request. This command revalidates that choice against the
//! repository and GitHub, records it durably, pushes exactly that commit
//! through the pinned push, reads the remote branch back, and records the
//! result. The decisions are in [`crate::domain::publication`]; this module
//! observes, asks those rules, and writes.
//!
//! What it never does: approve the Task, change its Human Review record,
//! force-push, create a pull request, or publish any commit other than the one
//! named in the request. A refusal before the push leaves the remote as it
//! was. A push whose outcome cannot be read back is recorded as unknown, and
//! the next request reads the remote again before it may push.

//! Residual limits. The pull request is read again after the record is
//! written and before the push, but GitHub can still merge or close it in the
//! moments between that read and the push, and the push then reaches the
//! branch. A repeated request for a commit already recorded as published is
//! answered from the record, without reading the remote again. A run that
//! starts after the live-run check and before the reservation is not refused.

use std::sync::atomic::{AtomicBool, Ordering};

use uuid::Uuid;

use crate::commands::gui_admission::GuiMutation;
use crate::commands::pr::{
    own_pr_url_for_branch, pr_url_for_task, push_branch_pinned, refuse_if_pr_operation_cancelled,
    reserve_task_for_pr_side_effect, resolve_repository_dir, run_cmd, task_branch_tip,
};
use crate::domain::publication::{
    classify_outcome, reconcile, replay, start_refusal, PrPublication, PublicationState, PullRequestObservation,
    Replay, StartObservation,
};
use crate::domain::{Task, TaskStatus};
use crate::lifecycle::{self, RecordWriteError};
use crate::worktree::delivery::RemoteBranch;
use crate::worktree::restack;

/// Publish `commit` to the task's pull request. Returns the durable record of
/// the request, whatever its state: the caller shows `Published`, `Refused`
/// and `Unknown` alike, and only for the commit it asked for.
///
/// Authorization. A person's confirmation covers the commit, branch and pull
/// request it showed. The request checks the branch and the pull request when it
/// starts, and pushes the commit by object ID, so it never pushes a different
/// commit. The Task's status and its Human Review approval are not part of that
/// consent: publishing does not approve the Task. A move that ends the request's
/// owner is honored at the request's checkpoints: before the pending record is
/// written, after GitHub is read again, and immediately before the push. After
/// the last checkpoint the push is not interrupted. A move that arrives while the
/// push is being prepared is not seen, and the push completes with the confirmed
/// commit. That window is a known limitation.
///
/// `Err` means this call returned no record. It covers four cases: the request
/// could not start (a running task, a pull request that is not this task's, an
/// unreadable repository); a record write did not land before any push; an
/// earlier publication of a different commit or pull request was still pending
/// or unknown; and a push ran but its outcome could not be recorded, in which
/// case the error text says whether the push succeeded. In the third case that
/// earlier record is reconciled against the remote and its settled state is
/// written, so the stored record can change even though the request returns
/// `Err`. A request pushes at most the commit it was given, and only after its
/// pending record was written.
#[tauri::command]
pub async fn publish_task_to_pr(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    branch: String,
    commit: String,
    pr_url: String,
) -> Result<PrPublication, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    publish(&state, task_id, &branch, &commit, &pr_url).await
}

/// Whether `commit` has the shape of a full object ID. A short ID is not
/// something a person was shown to confirm.
fn is_full_object_id(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64) && commit.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The remote branch head as a publication outcome reads it: `Ok(None)` when
/// the branch is absent, `Err` when the remote could not be asked.
fn remote_head_of(remote: &RemoteBranch) -> Result<Option<String>, String> {
    if let Some(tip) = remote.tip() {
        return Ok(Some(tip.to_string()));
    }
    if remote.is_absent() {
        return Ok(None);
    }
    Err(remote.unavailable().unwrap_or("the remote branch could not be read").to_string())
}

/// Read the pull request's state and head branch from GitHub, by its URL.
/// Reads only.
async fn pull_request_state(working_dir: &str, pr_url: &str) -> Result<(bool, String), String> {
    let listed = run_cmd("gh", &["pr", "view", pr_url, "--json", "state,headRefName"], working_dir)
        .await
        .map_err(|e| format!("could not read the pull request from GitHub: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&listed).map_err(|e| format!("could not read gh's answer: {e}"))?;
    let open = json.get("state").and_then(|s| s.as_str()) == Some("OPEN");
    let head = json
        .get("headRefName")
        .and_then(|h| h.as_str())
        .ok_or("gh's answer has no head branch")?
        .to_string();
    Ok((open, head))
}

/// What GitHub says about the pull request: whether it is open, its head
/// branch, and whether it is the branch's own pull request. Reads only.
///
/// In a unit test a value set by [`test_seam::fix_observation`] stands in for
/// `gh`, which cannot be asked offline. The override is thread-local, so
/// parallel tests cannot see each other's answers.
async fn observe_pull_request(
    working_dir: &str,
    branch: &str,
    pr_url: &str,
) -> Result<(bool, String, bool), String> {
    #[cfg(test)]
    if let Some(fixed) = test_seam::fixed() {
        test_seam::run_hook();
        // Lets whatever a hook started run before the request resumes.
        tokio::task::yield_now().await;
        return Ok(fixed);
    }
    let (open, head_branch) = pull_request_state(working_dir, pr_url).await?;
    let own = own_pr_url_for_branch(working_dir, branch).await?.as_deref() == Some(pr_url);
    Ok((open, head_branch, own))
}

#[cfg(test)]
mod test_seam {
    use std::cell::RefCell;

    thread_local! {
        static FIXED: RefCell<Option<(bool, String, bool)>> = const { RefCell::new(None) };
        /// How many observations to let through before the hook runs, and the hook.
        static HOOK: RefCell<Option<(u32, Box<dyn FnOnce()>)>> = const { RefCell::new(None) };
    }

    /// Answer as if GitHub said: open or not, head branch, own or not.
    pub(super) fn fix_observation(open: bool, head_branch: &str, own: bool) {
        FIXED.with(|f| *f.borrow_mut() = Some((open, head_branch.to_string(), own)));
    }

    pub(super) fn fixed() -> Option<(bool, String, bool)> {
        FIXED.with(|f| f.borrow().clone())
    }

    /// Run `hook` at the observation that follows `skip` earlier ones. The
    /// request observes twice: once at its start and once after its pending
    /// record is durable, so `skip = 1` puts the hook in that window.
    pub(super) fn hook_observation_after(skip: u32, hook: Box<dyn FnOnce()>) {
        HOOK.with(|h| *h.borrow_mut() = Some((skip, hook)));
    }

    /// Run the armed hook when its observation is reached.
    pub(super) fn run_hook() {
        let due = HOOK.with(|h| {
            let mut slot = h.borrow_mut();
            let (remaining, _) = slot.as_mut()?;
            if *remaining > 0 {
                *remaining -= 1;
                return None;
            }
            slot.take().map(|(_, hook)| hook)
        });
        if let Some(hook) = due {
            hook();
        }
    }
}

/// Write `record` as the task's publication, but only while the task still
/// holds `expected` (compare-and-set). `Err` says whether the write landed:
/// `NotPublished` means the board is unchanged, including when `expected` no
/// longer matches; `PublishedNotDurable` means the new record is in place but
/// was not confirmed on disk.
async fn write_publication(
    state: &crate::AppState,
    task_id: Uuid,
    expected: Option<&PrPublication>,
    record: &PrPublication,
) -> Result<(), RecordWriteError> {
    let recorded = AtomicBool::new(false);
    let apply = |staged: &mut std::collections::HashMap<Uuid, Task>| {
        if let Some(task) = staged.get_mut(&task_id) {
            if task.pr_publication.as_ref() == expected {
                task.pr_publication = Some(record.clone());
                recorded.store(true, Ordering::SeqCst);
            }
        }
    };
    let written = lifecycle::record_typed(&state.task.tasks, &state.storage, task_id, &apply).await;
    // The compare-and-set decides first: when `expected` no longer matches, the
    // record was not written whatever the save reported.
    if !recorded.load(Ordering::SeqCst) {
        return Err(RecordWriteError::NotPublished(
            "the task's publication record changed while this request was running".to_string(),
        ));
    }
    written
}

/// The message for a publication record that did not land.
fn record_write_failure(error: RecordWriteError) -> String {
    format!("the publication could not be recorded: {error}")
}

/// Refuse while the task is queued, being worked on, in AI review, or while
/// an agent run is live. Checked before the reservation, because the
/// reservation ends an existing run's ownership and would cancel it.
async fn refuse_while_running(state: &crate::AppState, task_id: Uuid) -> Result<(), String> {
    const REFUSAL: &str = "This task is queued, being worked on, or in AI review. Publish to PR once its run \
                           has finished, so the commit you publish is the one that run left.";
    let status = state.task.tasks.read().await.get(&task_id).map(|t| t.status.clone());
    if matches!(status, Some(TaskStatus::Queue | TaskStatus::InProgress | TaskStatus::AiReview)) {
        return Err(REFUSAL.to_string());
    }
    if let Some(executor) = state.executor.get() {
        if executor.task_run(task_id).await.live {
            return Err(REFUSAL.to_string());
        }
    }
    Ok(())
}

/// Whether `commit` is on the remote: the remote head itself, or reachable from
/// it. `None` when Git could not say.
async fn commit_on_remote(working_dir: &str, commit: &str, head: &Option<String>) -> Option<bool> {
    let dir = std::path::Path::new(working_dir);
    match head {
        None => Some(false),
        Some(h) if h == commit => Some(true),
        Some(h) => restack::is_ancestor(dir, commit, h).await.ok(),
    }
}

async fn publish(
    state: &crate::AppState,
    task_id: Uuid,
    confirmed_branch: &str,
    commit: &str,
    pr_url: &str,
) -> Result<PrPublication, String> {
    if !is_full_object_id(commit) {
        return Err("The commit to publish must be a full object ID.".to_string());
    }
    refuse_while_running(state, task_id).await?;
    // Serialized with every other lifecycle change and PR side effect for this
    // task. The reservation also refuses an unfinished restack, integration or
    // worker recovery.
    let lease = reserve_task_for_pr_side_effect(state, task_id).await?;

    let task = state
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .ok_or("Task not found")?;
    if task.branch_name.as_deref() != Some(confirmed_branch) {
        return Err(
            "The task's branch is no longer the branch you confirmed, so nothing was published. \
             Review the commit and confirm again."
                .to_string(),
        );
    }
    let branch = task
        .branch_name
        .clone()
        .ok_or("This task has no branch recorded, so there is nothing to publish.")?;
    if pr_url_for_task(&task)? != pr_url {
        return Err(
            "The pull request you confirmed is not this task's pull request, so nothing was published."
                .to_string(),
        );
    }
    let working_dir = resolve_repository_dir(state, task_id).await?;
    let existing = task.pr_publication.clone();

    match replay(existing.as_ref(), commit, pr_url) {
        Replay::AlreadyPublished => return Ok(existing.expect("replay names an existing record")),
        Replay::Start => {}
        Replay::MustReconcile => {
            let record = existing.expect("replay names an existing record");
            let remote = RemoteBranch::refresh(&working_dir, Some(record.branch.as_str())).await;
            let remote_now = remote_head_of(&remote);
            let on_remote = match &remote_now {
                Ok(head) => commit_on_remote(&working_dir, &record.commit, head).await,
                Err(_) => None,
            };
            let Some(state_now) = reconcile(&record, &remote_now, on_remote) else {
                return Err(
                    "An earlier publication's outcome is still unknown, because the remote branch could not \
                     confirm it. Nothing new was pushed. Try again once GitHub can be reached."
                        .to_string(),
                );
            };
            let updated = PrPublication { state: state_now, updated_at: chrono::Utc::now(), ..record.clone() };
            write_publication(state, task_id, Some(&record), &updated)
                .await
                .map_err(record_write_failure)?;
            if updated.commit != commit || updated.pr_url != pr_url {
                // The stored record was for another commit or another pull request.
                // Its outcome is now settled, but this request was not pushed, and
                // reconciling must not start a push for it.
                return Err(format!(
                    "An earlier publication of {} to {} was settled against the remote. The commit you confirmed, \
                     {}, was not published, and nothing was pushed for it. Confirm the publication again to publish it.",
                    short_commit(&record.commit),
                    record.pr_url,
                    short_commit(commit),
                ));
            }
            return Ok(updated);
        }
    }

    // Observe everything the start rules need. Reads only.
    refuse_if_pr_operation_cancelled(&lease, "reading the pull request")?;
    let local_tip = task_branch_tip(state, task_id).await?;
    let (open, head_branch, own) = observe_pull_request(&working_dir, &branch, pr_url).await?;
    let remote = RemoteBranch::refresh(&working_dir, Some(branch.as_str())).await;
    let remote_head = remote_head_of(&remote)?;
    let remote_head_is_ancestor = match &remote_head {
        Some(head) => matches!(restack::is_ancestor(std::path::Path::new(&working_dir), head, commit).await, Ok(true)),
        None => false,
    };
    let observed = StartObservation {
        local_tip,
        remote_head: remote_head.clone(),
        remote_head_is_ancestor,
        pull_request: PullRequestObservation { open, head_branch, is_branch_pull_request: own },
    };

    let now = chrono::Utc::now();
    let mut record = PrPublication::pending(
        Uuid::new_v4(),
        commit.to_string(),
        branch.clone(),
        pr_url.to_string(),
        remote_head.clone(),
        now,
    );
    if let Some(reason) = start_refusal(commit, &branch, &observed) {
        // Nothing reached the remote. The refusal is recorded so the outcome
        // stays visible, and a new request may start afterwards.
        record.state = PublicationState::Refused { reason };
        write_publication(state, task_id, existing.as_ref(), &record).await.map_err(record_write_failure)?;
        return Ok(record);
    }

    // The record is durable before the push. If it cannot be written, nothing
    // is pushed.
    refuse_if_pr_operation_cancelled(&lease, "publishing the commit")?;
    write_publication(state, task_id, existing.as_ref(), &record)
        .await
        .map_err(|e| format!("{}. Nothing was pushed.", record_write_failure(e)))?;

    let pending = record.clone();
    // The pull request is read again once the record is durable, so that a
    // merge or close since the first read is not pushed into. This narrows the
    // window to one GitHub read; it does not close it (see the module docs).
    let recheck = match observe_pull_request(&working_dir, &branch, pr_url).await {
        Ok((open, head_branch, own)) => StartObservation {
            pull_request: PullRequestObservation { open, head_branch, is_branch_pull_request: own },
            ..observed.clone()
        },
        Err(error) => {
            record.state = PublicationState::Refused {
                reason: format!("the pull request could not be read again before the push, so nothing was pushed: {error}"),
            };
            write_publication(state, task_id, Some(&pending), &record).await.map_err(record_write_failure)?;
            return Ok(record);
        }
    };
    if let Some(reason) = start_refusal(commit, &branch, &recheck) {
        record.state = PublicationState::Refused { reason };
        write_publication(state, task_id, Some(&pending), &record).await.map_err(record_write_failure)?;
        return Ok(record);
    }
    // The owner may have been ended while the pending record was written and
    // GitHub was read again. Nothing has been pushed yet, so that is recorded
    // as a refusal rather than pushed over the move.
    if refuse_if_pr_operation_cancelled(&lease, "pushing the branch").is_err() {
        // The reason is about this request, not the Task: an owner can be ended
        // by a move that was then refused, or by a stop that timed out.
        record.state = PublicationState::Refused {
            reason: "This publication was stopped before the push, because the task's pull request \
                     operation was ended while it was being prepared. Nothing was pushed. Publish \
                     again to retry."
                .to_string(),
        };
        write_publication(state, task_id, Some(&pending), &record).await.map_err(record_write_failure)?;
        return Ok(record);
    }

    let push = push_branch_pinned(&working_dir, &branch, Some(commit)).await.map(|_| ());
    let remote_after = remote_head_of(&RemoteBranch::refresh(&working_dir, Some(branch.as_str())).await);
    let settled = classify_outcome(commit, remote_head.as_deref(), &remote_after, &push);

    let outcome = if push.is_ok() { "succeeded" } else { "failed" };
    let finished = PrPublication { state: settled, updated_at: chrono::Utc::now(), ..record };
    write_publication(state, task_id, Some(&pending), &finished).await.map_err(|e| match e {
        // Nothing changed on disk or in memory: the record still says pending.
        RecordWriteError::NotPublished(_) => format!(
            "The push {outcome}, but its outcome could not be recorded: {e}. The record still says pending, so \
             the next request reads the remote again before it publishes."
        ),
        // The settled record is in memory and was written, but was not confirmed
        // durable. A restart may bring back the pending record, which the next
        // request reconciles against the remote before it publishes.
        RecordWriteError::PublishedNotDurable(_) => format!(
            "The push {outcome}, and its outcome is recorded, but the record could not be confirmed on disk: {e}. \
             If the application restarts first, the next request reads the remote again before it publishes."
        ),
    })?;
    Ok(finished)
}

/// The first twelve characters of a commit, for messages a person reads.
fn short_commit(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

#[cfg(test)]
mod tests {
    use super::is_full_object_id;

    #[test]
    fn only_full_lowercase_object_ids_are_accepted_as_a_publication_commit() {
        assert!(is_full_object_id(&"a".repeat(40)));
        assert!(is_full_object_id(&"0".repeat(64)));
        assert!(!is_full_object_id("abc"));
        assert!(!is_full_object_id(&"A".repeat(40)));
        assert!(!is_full_object_id(&format!("{}g", "a".repeat(39))));
        assert!(!is_full_object_id(&format!("{} ", "a".repeat(39))));
    }
}

#[cfg(test)]
mod orchestration {
    //! The publish command against a real bare remote. GitHub's answer is the
    //! only stand-in ([`super::test_seam`]); every Git effect is real.

    use super::{publish, test_seam};
    use crate::domain::publication::{PrPublication, PublicationState};
    use crate::domain::task::HumanReviewRecord;
    use crate::domain::{AgentConfig, AgentType, Project, ProjectScope, Repository, Task, TaskStatus};
    use crate::test_helpers::create_test_task_full;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    const PR: &str = "https://github.com/BarraDev/slashit/pull/9";
    const BRANCH: &str = "task-branch";

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(["-c", "user.email=t@example.com", "-c", "user.name=T"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        state: crate::AppState,
        task_id: Uuid,
        project_id: Uuid,
        remote: PathBuf,
        checkout: PathBuf,
        config: PathBuf,
        base: String,
    }

    async fn fixture() -> Fx {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = tmp.path().join("config");
        let paths = std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
            config.clone(),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let (state, _) = crate::app_core::build_state_with_paths(paths).await.unwrap();

        let remote = tmp.path().join("remote.git");
        let checkout = tmp.path().join("checkout");
        git(tmp.path(), &["init", "-q", "--bare", remote.to_str().unwrap()]);
        git(tmp.path(), &["init", "-q", "-b", "main", checkout.to_str().unwrap()]);
        git(&checkout, &["commit", "-q", "--allow-empty", "-m", "base"]);
        git(&checkout, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git(&checkout, &["push", "-q", "-u", "origin", "main"]);
        git(&checkout, &["checkout", "-q", "-b", BRANCH]);
        git(&checkout, &["push", "-q", "-u", "origin", BRANCH]);
        let base = git(&checkout, &["rev-parse", "HEAD"]);

        let repository_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        state.repository.repositories.write().await.insert(
            repository_id,
            Repository {
                id: repository_id,
                local_path: checkout.to_string_lossy().to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        state.project.projects.write().await.insert(
            project_id,
            Project {
                id: project_id,
                name: "p".into(),
                repository_id: Some(repository_id),
                scope: ProjectScope::default(),
                state_location: crate::config::paths::StateLocation::External,
                base: None,
                agent_type: AgentType::ClaudeCode,
                agent_config: AgentConfig {
                    agent_type: AgentType::ClaudeCode,
                    command: "claude".into(),
                    args: Vec::new(),
                    env: HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );

        let mut task = create_test_task_full("publish", project_id, TaskStatus::HumanReview, 0);
        task.branch_name = Some(BRANCH.to_string());
        task.pr_url = Some(PR.to_string());
        let task_id = task.id;
        state.task.tasks.write().await.insert(task_id, task.clone());
        state.storage.save_project_tasks(project_id, &[task]).unwrap();

        Fx { _tmp: tmp, state, task_id, project_id, remote, checkout, config, base }
    }

    impl Fx {
        /// Make every save of the board fail: a regular file where the legacy
        /// tasks directory has to be, so the atomic write cannot create it.
        /// Behaves the same for every user, root included.
        fn block_persistence(&self) {
            let dir = self.config.join("tasks");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::write(&dir, b"not a directory").expect("place persistence blocker");
        }

        fn unblock_persistence(&self) {
            let dir = self.config.join("tasks");
            let _ = std::fs::remove_file(&dir);
            std::fs::create_dir_all(&dir).expect("restore the tasks directory");
        }

        /// Make the next push to the bare remote block persistence after the
        /// ref has moved: a `post-receive` hook replaces the tasks directory.
        /// The push itself succeeds, so the settled record write is the one that fails.
        #[cfg(unix)]
        fn block_persistence_after_next_push(&self) {
            use std::os::unix::fs::PermissionsExt;
            let dir = self.config.join("tasks");
            let hook = self.remote.join("hooks").join("post-receive");
            let script = format!(
                "#!/bin/sh\nrm -rf '{d}'\nprintf 'not a directory' > '{d}'\n",
                d = dir.display()
            );
            std::fs::write(&hook, script).expect("write post-receive hook");
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("make hook executable");
        }

        #[cfg(unix)]
        fn remove_post_receive_hook(&self) {
            let _ = std::fs::remove_file(self.remote.join("hooks").join("post-receive"));
        }

        /// Commit one more change on the task branch and return its ID.
        fn commit(&self, name: &str) -> String {
            git(&self.checkout, &["checkout", "-q", BRANCH]);
            std::fs::write(self.checkout.join(name), name).unwrap();
            git(&self.checkout, &["add", name]);
            git(&self.checkout, &["commit", "-q", "-m", name]);
            git(&self.checkout, &["rev-parse", "HEAD"])
        }

        /// The branch head on the remote, read from the bare repository.
        fn remote_head(&self) -> Option<String> {
            let listed = git(&self.remote, &["for-each-ref", "--format=%(objectname)", &format!("refs/heads/{BRANCH}")]);
            (!listed.is_empty()).then_some(listed)
        }

        async fn task(&self) -> Task {
            self.state.task.tasks.read().await.get(&self.task_id).cloned().unwrap()
        }

        async fn record(&self) -> Option<PrPublication> {
            self.task().await.pr_publication
        }

        /// Write a publication record as a crash would have left it.
        async fn leave_record(&self, record: PrPublication) {
            let mut tasks = self.state.task.tasks.write().await;
            let task = tasks.get_mut(&self.task_id).unwrap();
            task.pr_publication = Some(record);
            let board: Vec<Task> = tasks.values().cloned().collect();
            self.state.storage.save_project_tasks(self.project_id, &board).unwrap();
        }
    }

    fn pending_record(commit: &str, remote_before: &str) -> PrPublication {
        PrPublication::pending(
            Uuid::new_v4(),
            commit.to_string(),
            BRANCH.to_string(),
            PR.to_string(),
            Some(remote_before.to_string()),
            chrono::Utc::now(),
        )
    }

    #[tokio::test]
    async fn publishes_exactly_the_confirmed_commit_and_does_not_approve_the_task() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, true);

        let record = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();

        assert_eq!(record.state, PublicationState::Published {});
        assert_eq!(fx.remote_head().as_deref(), Some(c1.as_str()));
        let task = fx.task().await;
        assert_eq!(task.human_review, HumanReviewRecord::default(), "publication must not record a decision");
        assert_eq!(task.status, TaskStatus::HumanReview);
    }

    #[tokio::test]
    async fn a_commit_the_branch_no_longer_holds_is_refused_without_touching_the_remote() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        let _c2 = fx.commit("two");
        test_seam::fix_observation(true, BRANCH, true);

        let record = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();

        assert!(matches!(&record.state, PublicationState::Refused { reason } if reason.contains("no longer at the commit")));
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()), "a refusal must not push");
    }

    #[tokio::test]
    async fn a_non_fast_forward_is_refused_and_the_remote_branch_is_left_alone() {
        let fx = fixture().await;
        // Someone else pushes a commit to the branch; the local branch is reset
        // behind it and diverges.
        let x = fx.commit("remote-only");
        git(&fx.checkout, &["push", "-q", "origin", &format!("{x}:refs/heads/{BRANCH}")]);
        git(&fx.checkout, &["reset", "-q", "--hard", &fx.base]);
        let c1 = fx.commit("local");
        test_seam::fix_observation(true, BRANCH, true);

        let record = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();

        assert!(matches!(&record.state, PublicationState::Refused { reason } if reason.contains("fast-forward")));
        assert_eq!(fx.remote_head().as_deref(), Some(x.as_str()), "the remote commit must survive");
    }

    #[tokio::test]
    async fn a_closed_or_merged_pull_request_is_refused_without_a_push() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(false, BRANCH, true);

        let record = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();

        assert!(matches!(&record.state, PublicationState::Refused { reason } if reason.contains("closed or merged")));
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));
    }

    #[tokio::test]
    async fn a_pull_request_that_is_not_the_branch_own_is_refused_without_a_push() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, false);

        let record = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();

        assert!(matches!(record.state, PublicationState::Refused { .. }));
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));
    }

    #[tokio::test]
    async fn a_repeated_request_for_a_published_commit_is_answered_by_the_existing_record() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, true);
        let first = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();

        let again = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();

        assert_eq!(again, first, "the same request id means no new publication was started");
        assert_eq!(fx.remote_head().as_deref(), Some(c1.as_str()));
    }

    #[tokio::test]
    async fn a_later_commit_needs_its_own_publication_and_an_older_one_is_refused() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, true);
        publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();
        let c2 = fx.commit("two");

        let first_id = fx.record().await.unwrap().request_id;
        let older = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();
        // c1 is the record already published to the remote: the same request is
        // answered by that record, and nothing is pushed.
        assert_eq!(older.request_id, first_id, "the existing record answers a repeat of c1");
        assert_eq!(older.state, PublicationState::Published {});
        assert_eq!(fx.remote_head().as_deref(), Some(c1.as_str()));

        let newer = publish(&fx.state, fx.task_id, BRANCH, &c2, PR).await.unwrap();
        assert_eq!(newer.state, PublicationState::Published {});
        assert_eq!(fx.remote_head().as_deref(), Some(c2.as_str()));
    }

    #[tokio::test]
    async fn a_push_that_landed_before_its_record_settled_is_reconciled_without_pushing_again() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        // The push landed; the record still says Pending.
        git(&fx.checkout, &["push", "-q", "origin", &format!("{c1}:refs/heads/{BRANCH}")]);
        fx.leave_record(pending_record(&c1, &fx.base)).await;
        test_seam::fix_observation(true, BRANCH, true);

        let record = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();

        assert_eq!(record.state, PublicationState::Published {});
        assert_eq!(fx.record().await.unwrap().state, PublicationState::Published {});
    }

    #[tokio::test]
    async fn a_pending_record_whose_push_never_landed_is_refused_first_and_can_then_publish() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        fx.leave_record(pending_record(&c1, &fx.base)).await;
        test_seam::fix_observation(true, BRANCH, true);

        let settled = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();
        assert!(matches!(settled.state, PublicationState::Refused { .. }));
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));

        let retried = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();
        assert_eq!(retried.state, PublicationState::Published {});
        assert_eq!(fx.remote_head().as_deref(), Some(c1.as_str()));
    }

    #[tokio::test]
    async fn an_unknown_outcome_blocks_a_new_push_until_the_remote_can_be_read() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        fx.leave_record(PrPublication {
            state: PublicationState::Unknown { reason: "test".into() },
            ..pending_record(&c1, &fx.base)
        })
        .await;
        test_seam::fix_observation(true, BRANCH, true);
        // Make the remote unreadable for the reconciliation.
        let good = git(&fx.checkout, &["remote", "get-url", "origin"]);
        git(&fx.checkout, &["remote", "set-url", "origin", "/nonexistent/remote.git"]);

        let blocked = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await;
        assert!(blocked.unwrap_err().contains("still unknown"));
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()), "nothing may be pushed while unknown");

        git(&fx.checkout, &["remote", "set-url", "origin", &good]);
        // The remote is back and still holds the old head: the unknown outcome settles as refused.
        let settled = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();
        assert!(matches!(settled.state, PublicationState::Refused { .. }));
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));

        let retried = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();
        assert_eq!(retried.state, PublicationState::Published {});
    }

    #[tokio::test]
    async fn a_commit_for_another_pull_request_is_refused_before_anything_is_recorded() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, true);

        let err = publish(&fx.state, fx.task_id, BRANCH, &c1, "https://github.com/BarraDev/slashit/pull/10").await.unwrap_err();

        assert!(err.contains("not this task's pull request"));
        assert!(fx.record().await.is_none());
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));
    }

    #[tokio::test]
    async fn a_record_whose_commit_is_still_on_the_remote_under_later_commits_settles_as_published() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        git(&fx.checkout, &["push", "-q", "origin", &format!("{c1}:refs/heads/{BRANCH}")]);
        // Someone else pushes on top of it; the record is still pending.
        let c2 = fx.commit("two");
        git(&fx.checkout, &["push", "-q", "origin", &format!("{c2}:refs/heads/{BRANCH}")]);
        fx.leave_record(pending_record(&c1, &fx.base)).await;
        test_seam::fix_observation(true, BRANCH, true);

        let settled = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();

        assert_eq!(settled.state, PublicationState::Published {});
        assert_eq!(fx.remote_head().as_deref(), Some(c2.as_str()), "reconciling must not push");
    }

    #[tokio::test]
    async fn a_record_whose_remote_moved_without_its_commit_settles_so_the_task_is_not_stuck() {
        let fx = fixture().await;
        // The remote moved to a commit that does not contain c1.
        let x = fx.commit("remote-only");
        git(&fx.checkout, &["push", "-q", "origin", &format!("{x}:refs/heads/{BRANCH}")]);
        git(&fx.checkout, &["reset", "-q", "--hard", &fx.base]);
        let c1 = fx.commit("local");
        fx.leave_record(PrPublication {
            state: PublicationState::Unknown { reason: "test".into() },
            ..pending_record(&c1, &fx.base)
        })
        .await;
        test_seam::fix_observation(true, BRANCH, true);

        // The first request settles the record as refused, not as an error.
        let settled = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();
        assert!(matches!(settled.state, PublicationState::Refused { .. }));

        // A new request now runs the start rules and refuses honestly: the
        // remote holds a commit this branch does not contain. Nothing is forced.
        let retried = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap();
        assert!(matches!(&retried.state, PublicationState::Refused { reason } if reason.contains("fast-forward")));
        assert_eq!(fx.remote_head().as_deref(), Some(x.as_str()));
    }

    #[tokio::test]
    async fn a_request_for_another_commit_is_not_reported_as_published_after_an_earlier_record_settles() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        // c1 reached the remote, but its record still says pending.
        git(&fx.checkout, &["push", "-q", "origin", &format!("{c1}:refs/heads/{BRANCH}")]);
        fx.leave_record(pending_record(&c1, &fx.base)).await;
        let c2 = fx.commit("two");
        test_seam::fix_observation(true, BRANCH, true);

        let err = publish(&fx.state, fx.task_id, BRANCH, &c2, PR).await.unwrap_err();

        assert!(err.contains("was not published"), "{err}");
        assert_eq!(fx.remote_head().as_deref(), Some(c1.as_str()), "reconciling c1 must not push c2");
        let settled = fx.record().await.unwrap();
        assert_eq!(settled.commit, c1, "the stored record is c1's, settled");
        assert_eq!(settled.state, PublicationState::Published {});
    }

    #[tokio::test]
    async fn a_settled_record_for_the_same_commit_on_another_pull_request_is_not_reported_as_published() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        // c1 reached the remote for an earlier pull request; its record is still pending.
        git(&fx.checkout, &["push", "-q", "origin", &format!("{c1}:refs/heads/{BRANCH}")]);
        let earlier_pr = "https://github.com/BarraDev/slashit/pull/8";
        fx.leave_record(PrPublication { pr_url: earlier_pr.to_string(), ..pending_record(&c1, &fx.base) }).await;
        test_seam::fix_observation(true, BRANCH, true);

        let err = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap_err();

        assert!(err.contains("was not published"), "{err}");
        let settled = fx.record().await.unwrap();
        assert_eq!((settled.pr_url.as_str(), settled.state), (earlier_pr, PublicationState::Published {}));
        assert_eq!(fx.remote_head().as_deref(), Some(c1.as_str()));
    }

    #[tokio::test]
    async fn a_failed_pending_write_pushes_nothing() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, true);
        fx.block_persistence();

        let err = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap_err();

        assert!(err.contains("Nothing was pushed"), "{err}");
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()), "no push without a durable pending record");
        assert!(fx.record().await.is_none(), "the board is unchanged");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_settled_write_after_a_push_leaves_the_record_pending_and_the_next_request_reconciles() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, true);
        fx.block_persistence_after_next_push();

        let err = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap_err();

        assert!(err.contains("still says pending"), "{err}");
        assert_eq!(fx.remote_head().as_deref(), Some(c1.as_str()), "the push itself landed");
        assert_eq!(fx.record().await.unwrap().state, PublicationState::Pending {});

        fx.remove_post_receive_hook();
        fx.unblock_persistence();
        // The next request for c2 must reconcile c1 against the remote first and must not push c2.
        let c2 = fx.commit("two");
        let next = publish(&fx.state, fx.task_id, BRANCH, &c2, PR).await.unwrap_err();

        assert!(next.contains("was not published"), "{next}");
        assert_eq!(fx.remote_head().as_deref(), Some(c1.as_str()));
        let settled = fx.record().await.unwrap();
        assert_eq!((settled.commit.as_str(), settled.state), (c1.as_str(), PublicationState::Published {}));
    }

    #[tokio::test]
    async fn concurrent_requests_for_different_commits_leave_a_settled_record_that_matches_the_remote() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        let c2 = fx.commit("two");
        test_seam::fix_observation(true, BRANCH, true);

        // This fixture has no executor, so there is no PR owner registration and
        // the two requests are not serialized here. What keeps them apart is the
        // compare-and-set on the stored record. The executor-backed test below
        // covers the registration. c1 is no longer the tip, so only c2 may reach
        // the remote, in whichever order the two requests run. Their results are
        // not asserted individually.
        let (_first, _second) = tokio::join!(
            publish(&fx.state, fx.task_id, BRANCH, &c1, PR),
            publish(&fx.state, fx.task_id, BRANCH, &c2, PR),
        );

        let remote = fx.remote_head();
        assert!(
            remote.as_deref() == Some(fx.base.as_str()) || remote.as_deref() == Some(c2.as_str()),
            "only the current tip may reach the remote, got {remote:?}"
        );
        let stored = fx.record().await.expect("a request leaves a record");
        assert!(!stored.needs_reconciliation(), "no request may leave a pending or unknown record");
        if remote.as_deref() == Some(c2.as_str()) {
            // c2 reached the remote, so the stored record must say so.
            assert_eq!(stored.commit, c2, "a pushed commit must be the stored record's commit");
            assert_eq!(stored.state, PublicationState::Published {}, "a pushed commit must be recorded as published");
        }
    }

    #[tokio::test]
    async fn a_stale_expected_record_does_not_overwrite_the_stored_publication() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        let stored = PrPublication { state: PublicationState::Unknown { reason: "x".into() }, ..pending_record(&c1, &fx.base) };
        fx.leave_record(stored.clone()).await;
        let newer = pending_record(&c1, &fx.base);

        // The caller believes there is no record; the stored one must survive.
        let result = super::write_publication(&fx.state, fx.task_id, None, &newer).await;

        assert!(matches!(result, Err(crate::lifecycle::RecordWriteError::NotPublished(_))));
        assert_eq!(fx.record().await, Some(stored));
    }

    #[tokio::test]
    async fn a_task_with_a_queued_or_running_run_is_refused_before_anything_is_reserved_or_pushed() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        for status in [TaskStatus::Queue, TaskStatus::InProgress, TaskStatus::AiReview] {
            fx.state.task.tasks.write().await.get_mut(&fx.task_id).unwrap().status = status.clone();
            let err = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap_err();
            assert!(err.contains("queued, being worked on, or in AI review"), "{status:?}: {err}");
        }
        assert!(fx.record().await.is_none(), "a refused request records nothing");
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));
    }

    /// The executor's PR owner registration is held for the whole publication.
    /// While the first request holds it, a second request for the same task is
    /// refused before anything is recorded or pushed, and it proceeds once the
    /// first has ended. The held reservation stands in for an in-flight request.
    #[tokio::test]
    async fn a_second_request_is_refused_while_a_publication_holds_the_task() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        let executor = crate::test_helpers::attach_test_executor(&fx.state);
        test_seam::fix_observation(true, BRANCH, true);
        // The contract for the reservation: the caller holds the task's lifecycle lease.
        let lifecycle = fx.state.task_lifecycle_locks.acquire(fx.task_id).await.expect("the lifecycle lease");
        let in_flight = executor
            .begin_pr_side_effect_under_lease(fx.task_id)
            .await
            .expect("the first request starts");
        drop(lifecycle);

        let refused = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap_err();
        assert!(refused.contains("already in progress"), "{refused}");
        assert!(fx.record().await.is_none(), "a refused request records nothing");
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));

        drop(in_flight);
        let published = publish(&fx.state, fx.task_id, BRANCH, &c1, PR)
            .await
            .expect("the request proceeds once the first has ended");
        assert_eq!(published.state, PublicationState::Published {});
        assert_eq!(fx.remote_head().as_deref(), Some(c1.as_str()));
    }

    /// A lifecycle move that ends the request's owner while the request re-reads
    /// GitHub, after its pending record is durable, must stop the push. The
    /// request records a refusal, nothing reaches the remote, and the move
    /// completes.
    #[tokio::test]
    async fn a_move_that_ends_the_owner_during_the_pull_request_reread_stops_the_push() {
        use crate::lifecycle::ExecutionOwnership;

        let fx = fixture().await;
        let c1 = fx.commit("one");
        let executor = crate::test_helpers::attach_test_executor(&fx.state);
        test_seam::fix_observation(true, BRANCH, true);
        let task_id = fx.task_id;
        let mover = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot = mover.clone();
        let for_move = executor.clone();
        // The second observation is the re-read, after the pending record is durable.
        test_seam::hook_observation_after(
            1,
            Box::new(move || {
                let handle = tokio::spawn(async move {
                    ExecutionOwnership::end_ownership_under_lease(for_move.as_ref(), task_id).await
                });
                *slot.lock().unwrap() = Some(handle);
            }),
        );

        let record = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.expect("the refusal is recorded");

        assert!(matches!(record.state, PublicationState::Refused { .. }), "{:?}", record.state);
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()), "nothing was pushed");
        assert_eq!(fx.record().await, Some(record), "the stored record is the refusal");
        let moved = mover.lock().unwrap().take().expect("the move ran during the re-read");
        moved.await.expect("the move finishes").expect("the move ends the owner");
    }

    /// A settled write that replaced the board but was not confirmed on disk is
    /// reported as such, and memory and the file both hold the settled record.
    /// The request is not left pending, so the next request is not held up.
    #[tokio::test]
    async fn a_settled_write_published_but_not_durable_is_adopted_and_reported() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, true);
        // The pending write syncs the board's directory once and passes. The
        // settled write's sync fails after its replacement.
        let fault = slashit_durable_fs::fault::fail_after(
            slashit_durable_fs::Stage::SyncDirectory,
            std::io::ErrorKind::Other,
            1,
        );

        let err = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap_err();
        drop(fault);

        assert!(err.contains("The push succeeded, and its outcome is recorded"), "{err}");
        assert!(err.contains("could not be confirmed on disk"), "{err}");
        assert_eq!(fx.remote_head().as_deref(), Some(c1.as_str()), "the push landed");
        let adopted = fx.record().await.expect("the settled record is in memory");
        assert_eq!(adopted.commit, c1);
        assert_eq!(adopted.state, PublicationState::Published {});
        let on_disk = fx.state.storage.load_project_tasks(fx.project_id).unwrap();
        let persisted = on_disk.iter().find(|t| t.id == fx.task_id).and_then(|t| t.pr_publication.clone());
        assert_eq!(persisted, Some(adopted), "the file holds the record memory adopted");

        let c2 = fx.commit("two");
        let next = publish(&fx.state, fx.task_id, BRANCH, &c2, PR).await.expect("the next request publishes");
        assert_eq!(next.state, PublicationState::Published {});
        assert_eq!(fx.remote_head().as_deref(), Some(c2.as_str()));
    }

    /// The person confirmed a branch. When the Task has since moved to another
    /// branch, the request is refused before anything is recorded or pushed.
    #[tokio::test]
    async fn a_confirmation_for_a_branch_the_task_no_longer_has_is_refused_before_anything_is_pushed() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, true);
        fx.state.task.tasks.write().await.get_mut(&fx.task_id).unwrap().branch_name = Some("renamed-branch".into());

        let err = publish(&fx.state, fx.task_id, BRANCH, &c1, PR).await.unwrap_err();

        assert!(err.contains("no longer the branch you confirmed"), "{err}");
        assert!(fx.record().await.is_none(), "a refused request records nothing");
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));
    }
}
