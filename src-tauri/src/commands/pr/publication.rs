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
use crate::lifecycle;
use crate::worktree::delivery::RemoteBranch;
use crate::worktree::restack;

/// Publish `commit` to the task's pull request. Returns the durable record of
/// the request, whatever its state: the caller shows `Published`, `Refused`
/// and `Unknown` alike. `Err` means nothing could be recorded, so nothing was
/// pushed by this call.
#[tauri::command]
pub async fn publish_task_to_pr(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    commit: String,
    pr_url: String,
) -> Result<PrPublication, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    publish(&state, task_id, &commit, &pr_url).await
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
    }

    /// Answer as if GitHub said: open or not, head branch, own or not.
    pub(super) fn fix_observation(open: bool, head_branch: &str, own: bool) {
        FIXED.with(|f| *f.borrow_mut() = Some((open, head_branch.to_string(), own)));
    }

    pub(super) fn fixed() -> Option<(bool, String, bool)> {
        FIXED.with(|f| f.borrow().clone())
    }
}

/// Write `record` as the task's publication, but only while the task still
/// holds `expected`. `Err` when the write did not land, which the caller
/// must treat as "nothing recorded".
async fn write_publication(
    state: &crate::AppState,
    task_id: Uuid,
    expected: Option<&PrPublication>,
    record: &PrPublication,
) -> Result<(), String> {
    let recorded = AtomicBool::new(false);
    let apply = |staged: &mut std::collections::HashMap<Uuid, Task>| {
        if let Some(task) = staged.get_mut(&task_id) {
            if task.pr_publication.as_ref() == expected {
                task.pr_publication = Some(record.clone());
                recorded.store(true, Ordering::SeqCst);
            }
        }
    };
    lifecycle::record(&state.task.tasks, &state.storage, task_id, &apply)
        .await
        .map_err(|e| format!("the publication could not be recorded: {e}"))?;
    if recorded.load(Ordering::SeqCst) {
        Ok(())
    } else {
        Err("the task's publication record changed while this request was running".to_string())
    }
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
            return match reconcile(&record, &remote_now, on_remote) {
                Some(state_now) => {
                    let updated = PrPublication { state: state_now, updated_at: chrono::Utc::now(), ..record.clone() };
                    write_publication(state, task_id, Some(&record), &updated).await?;
                    Ok(updated)
                }
                None => Err(
                    "An earlier publication's outcome is still unknown, because the remote branch could not \
                     confirm it. Nothing new was pushed. Try again once GitHub can be reached."
                        .to_string(),
                ),
            };
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
        write_publication(state, task_id, existing.as_ref(), &record).await?;
        return Ok(record);
    }

    // The record is durable before the push. If it cannot be written, nothing
    // is pushed.
    refuse_if_pr_operation_cancelled(&lease, "publishing the commit")?;
    write_publication(state, task_id, existing.as_ref(), &record).await.map_err(|e| {
        format!("{e}. Nothing was pushed.")
    })?;

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
            write_publication(state, task_id, Some(&pending), &record).await?;
            return Ok(record);
        }
    };
    if let Some(reason) = start_refusal(commit, &branch, &recheck) {
        record.state = PublicationState::Refused { reason };
        write_publication(state, task_id, Some(&pending), &record).await?;
        return Ok(record);
    }

    let push = push_branch_pinned(&working_dir, &branch, Some(commit)).await.map(|_| ());
    let remote_after = remote_head_of(&RemoteBranch::refresh(&working_dir, Some(branch.as_str())).await);
    let settled = classify_outcome(commit, remote_head.as_deref(), &remote_after, &push);

    let finished = PrPublication { state: settled, updated_at: chrono::Utc::now(), ..record };
    write_publication(state, task_id, Some(&pending), &finished).await.map_err(|e| {
        format!(
            "The push {}, but its outcome could not be recorded: {e}. The record still says pending, so the \
             next request reads the remote again before it publishes.",
            if push.is_ok() { "succeeded" } else { "failed" },
        )
    })?;
    Ok(finished)
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
        base: String,
    }

    async fn fixture() -> Fx {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
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

        Fx { _tmp: tmp, state, task_id, project_id, remote, checkout, base }
    }

    impl Fx {
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

        let record = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();

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

        let record = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();

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

        let record = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();

        assert!(matches!(&record.state, PublicationState::Refused { reason } if reason.contains("fast-forward")));
        assert_eq!(fx.remote_head().as_deref(), Some(x.as_str()), "the remote commit must survive");
    }

    #[tokio::test]
    async fn a_closed_or_merged_pull_request_is_refused_without_a_push() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(false, BRANCH, true);

        let record = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();

        assert!(matches!(&record.state, PublicationState::Refused { reason } if reason.contains("closed or merged")));
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));
    }

    #[tokio::test]
    async fn a_pull_request_that_is_not_the_branch_own_is_refused_without_a_push() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, false);

        let record = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();

        assert!(matches!(record.state, PublicationState::Refused { .. }));
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));
    }

    #[tokio::test]
    async fn a_repeated_request_for_a_published_commit_is_answered_by_the_existing_record() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, true);
        let first = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();

        let again = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();

        assert_eq!(again, first, "the same request id means no new publication was started");
        assert_eq!(fx.remote_head().as_deref(), Some(c1.as_str()));
    }

    #[tokio::test]
    async fn a_later_commit_needs_its_own_publication_and_an_older_one_is_refused() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, true);
        publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();
        let c2 = fx.commit("two");

        let first_id = fx.record().await.unwrap().request_id;
        let older = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();
        // c1 is the record already published to the remote: the same request is
        // answered by that record, and nothing is pushed.
        assert_eq!(older.request_id, first_id, "the existing record answers a repeat of c1");
        assert_eq!(older.state, PublicationState::Published {});
        assert_eq!(fx.remote_head().as_deref(), Some(c1.as_str()));

        let newer = publish(&fx.state, fx.task_id, &c2, PR).await.unwrap();
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

        let record = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();

        assert_eq!(record.state, PublicationState::Published {});
        assert_eq!(fx.record().await.unwrap().state, PublicationState::Published {});
    }

    #[tokio::test]
    async fn a_pending_record_whose_push_never_landed_is_refused_first_and_can_then_publish() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        fx.leave_record(pending_record(&c1, &fx.base)).await;
        test_seam::fix_observation(true, BRANCH, true);

        let settled = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();
        assert!(matches!(settled.state, PublicationState::Refused { .. }));
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));

        let retried = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();
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

        let blocked = publish(&fx.state, fx.task_id, &c1, PR).await;
        assert!(blocked.unwrap_err().contains("still unknown"));
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()), "nothing may be pushed while unknown");

        git(&fx.checkout, &["remote", "set-url", "origin", &good]);
        // The remote is back and still holds the old head: the unknown outcome settles as refused.
        let settled = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();
        assert!(matches!(settled.state, PublicationState::Refused { .. }));
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));

        let retried = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();
        assert_eq!(retried.state, PublicationState::Published {});
    }

    #[tokio::test]
    async fn a_commit_for_another_pull_request_is_refused_before_anything_is_recorded() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        test_seam::fix_observation(true, BRANCH, true);

        let err = publish(&fx.state, fx.task_id, &c1, "https://github.com/BarraDev/slashit/pull/10").await.unwrap_err();

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

        let settled = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();

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
        let settled = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();
        assert!(matches!(settled.state, PublicationState::Refused { .. }));

        // A new request now runs the start rules and refuses honestly: the
        // remote holds a commit this branch does not contain. Nothing is forced.
        let retried = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap();
        assert!(matches!(&retried.state, PublicationState::Refused { reason } if reason.contains("fast-forward")));
        assert_eq!(fx.remote_head().as_deref(), Some(x.as_str()));
    }

    #[tokio::test]
    async fn a_task_with_a_queued_or_running_run_is_refused_before_anything_is_reserved_or_pushed() {
        let fx = fixture().await;
        let c1 = fx.commit("one");
        for status in [TaskStatus::Queue, TaskStatus::InProgress, TaskStatus::AiReview] {
            fx.state.task.tasks.write().await.get_mut(&fx.task_id).unwrap().status = status.clone();
            let err = publish(&fx.state, fx.task_id, &c1, PR).await.unwrap_err();
            assert!(err.contains("queued, being worked on, or in AI review"), "{status:?}: {err}");
        }
        assert!(fx.record().await.is_none(), "a refused request records nothing");
        assert_eq!(fx.remote_head().as_deref(), Some(fx.base.as_str()));
    }
}
