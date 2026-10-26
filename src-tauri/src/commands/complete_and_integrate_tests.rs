// SPDX-License-Identifier: MPL-2.0
//! Complete and Integrate from Human Review: the integration and the
//! completion are two durable facts, and every state between them has to be
//! truthful, recoverable and repeatable without repeating anything.
//!
//! Each test builds a real repository, a real Task Checkout and a Task in
//! Human Review, and asserts on Git, on the in-memory board and on the file
//! the next start reads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::commands::human_review;
use crate::commands::integration::{
    complete_and_integrate, discard, integrate, preview, reconfirm, resume, IntegrationConfirmation,
    IntegrationPreview, PreviewState,
};
use crate::domain::task::ActivityKind;
use crate::domain::{IntegrationState, IntegrationStrategy, Task, TaskStatus};
use crate::test_helpers::create_test_task_full;
use crate::worktree::integration as git;

fn run(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn ref_exists(repo: &Path, name: &str) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--verify", "-q", name])
        .current_dir(repo)
        .output()
        .unwrap()
        .status
        .success()
}

fn tip_of(repo: &Path, branch: &str) -> String {
    run(repo, &["rev-parse", &format!("refs/heads/{branch}")])
}

/// A commit on `main` while the primary checkout stays detached, so `main`
/// is free to move unless a test checks it out.
fn commit_on_main(repo: &Path, file: &str, content: &str) -> String {
    run(repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join(file), content).unwrap();
    run(repo, &["add", "."]);
    run(repo, &["commit", "-q", "-m", &format!("main: {file}")]);
    let tip = run(repo, &["rev-parse", "HEAD"]);
    run(repo, &["checkout", "-q", "--detach"]);
    tip
}

struct Fx {
    _tmp: tempfile::TempDir,
    paths: std::sync::Arc<crate::config::paths::AppPaths>,
    state: crate::AppState,
    repo: PathBuf,
    worktree: PathBuf,
    project_id: Uuid,
    task_id: Uuid,
    branch: String,
    tip: String,
}

/// A repository on `main` with no remote, a Task Checkout holding one
/// committed change on its branch, and a Task in Human Review that owns both
/// and has been approved at that commit. The primary checkout is detached
/// unless `detach` is false.
async fn fixture_with(detach: bool, remote: Option<&str>) -> Fx {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let paths = std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
        tmp.path().join("config"),
        tmp.path().join("data"),
        tmp.path().join("cache"),
        tmp.path().join("runtime"),
    ));
    let (state, _) = crate::app_core::build_state_with_paths(paths.clone()).await.expect("state");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "test@example.com"]);
    run(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("README.md"), "base\n").unwrap();
    run(&repo, &["add", "."]);
    run(&repo, &["commit", "-q", "-m", "init"]);
    if let Some(url) = remote {
        run(&repo, &["remote", "add", "origin", url]);
    }

    let repository_id = Uuid::new_v4();
    let project_id = Uuid::new_v4();
    state.repository.repositories.write().await.insert(
        repository_id,
        crate::domain::Repository {
            id: repository_id,
            local_path: repo.to_str().unwrap().to_string(),
            remote_url: remote.map(str::to_string),
            remote_type: None,
            created_at: chrono::Utc::now(),
        },
    );
    let base = crate::domain::project::ProjectBase::LocalBranch { branch: "main".to_string() };
    state.project.projects.write().await.insert(
        project_id,
        crate::domain::Project {
            id: project_id,
            name: "complete and integrate".to_string(),
            repository_id: Some(repository_id),
            scope: crate::domain::project::ProjectScope::Standalone,
            state_location: crate::config::paths::StateLocation::External,
            base: Some(base.clone()),
            agent_type: crate::domain::AgentType::ClaudeCode,
            agent_config: crate::domain::AgentConfig {
                agent_type: crate::domain::AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
    );

    let branch = "task-one".to_string();
    let (info, _) = state
        .worktree_manager
        .create_or_adopt(repo.to_str().unwrap(), &branch, Some(&base))
        .await
        .expect("a Task Checkout");
    let worktree = PathBuf::from(&info.path);
    run(&worktree, &["config", "user.email", "test@example.com"]);
    run(&worktree, &["config", "user.name", "Test"]);
    std::fs::write(worktree.join("work.txt"), "work\n").unwrap();
    run(&worktree, &["add", "."]);
    run(&worktree, &["commit", "-q", "-m", "task work"]);
    let tip = run(&worktree, &["rev-parse", "HEAD"]);

    let mut task = create_test_task_full("complete and integrate", project_id, TaskStatus::HumanReview, 0);
    task.branch_name = Some(branch.clone());
    task.worktree_path = Some(info.path.clone());
    task.overall_progress = 90;
    task.human_review.record_arrival();
    let task_id = task.id;
    state.task.tasks.write().await.insert(task_id, task.clone());
    state.storage.save_project_tasks(project_id, &[task]).expect("seed the board");
    if detach {
        run(&repo, &["checkout", "-q", "--detach"]);
    }
    let fx = Fx { _tmp: tmp, paths, state, repo, worktree, project_id, task_id, branch, tip };
    human_review::approve_shown(&fx.state, fx.task_id, false).await.expect("approved");
    assert_eq!(current(&fx).await.human_review.approved_tip(), Some(fx.tip.as_str()), "the approval records the commit");
    fx
}

async fn fixture() -> Fx {
    fixture_with(true, None).await
}

async fn current(fx: &Fx) -> Task {
    fx.state.task.tasks.read().await.get(&fx.task_id).cloned().expect("the task")
}

fn on_disk(fx: &Fx) -> Task {
    fx.state
        .storage
        .load_project_tasks(fx.project_id)
        .unwrap()
        .into_iter()
        .find(|t| t.id == fx.task_id)
        .expect("on disk")
}

fn confirmation(preview: &IntegrationPreview) -> IntegrationConfirmation {
    IntegrationConfirmation {
        target_branch: preview.target_branch.clone().expect("a target"),
        target_tip: preview.target_tip.clone().expect("a target tip"),
        approved_tip: preview.approved_tip.clone().expect("an approved tip"),
        checkout_path: preview.checked_out_at.clone(),
    }
}

async fn shown(fx: &Fx) -> IntegrationPreview {
    preview(&fx.state, fx.task_id).await.expect("a preview")
}

fn count(task: &Task, wanted: fn(&ActivityKind) -> bool) -> usize {
    task.activity.iter().filter(|e| wanted(&e.kind)).count()
}

fn integrations(task: &Task) -> usize {
    count(task, |k| matches!(k, ActivityKind::IntegratedLocally { .. }))
}

fn completions(task: &Task) -> usize {
    count(task, |k| matches!(k, ActivityKind::CompletedLocally {}))
}

/// Nothing happened: the target, the Task and its file are as they were.
async fn assert_untouched(fx: &Fx, main_before: &str) {
    assert_eq!(tip_of(&fx.repo, "main"), main_before, "the destination did not move");
    let task = current(fx).await;
    assert_eq!(task.status, TaskStatus::HumanReview);
    assert!(task.local_integration.is_none(), "no record");
    assert_eq!(integrations(&task) + completions(&task), 0, "no activity");
    assert!(fx.worktree.exists(), "the Task Checkout is kept");
    assert!(!ref_exists(&fx.repo, &git::pin_ref(fx.task_id)), "no pin is left");
    assert_eq!(on_disk(fx).local_integration, None);
}

fn assert_done_and_integrated(fx: &Fx, done: &Task, strategy: IntegrationStrategy) {
    assert_eq!(done.status, TaskStatus::Done);
    assert_eq!(done.phase, crate::domain::task::TaskPhase::Complete);
    assert_eq!(done.overall_progress, 100);
    assert!(done.completed_locally());
    assert!(!done.cleanup_in_flight);
    assert!(done.worktree_path.is_none());
    assert!(!fx.worktree.exists(), "the Task Checkout is removed");
    assert_eq!(tip_of(&fx.repo, &fx.branch), fx.tip, "the Task branch and its commits are kept");
    let record = done.local_integration.clone().expect("an integration record");
    assert_eq!(record.state, IntegrationState::Integrated);
    assert_eq!(record.strategy, strategy);
    assert_eq!(record.approved_tip, fx.tip, "the record names the approved commit");
    assert!(record.completes_task);
    assert_eq!(record.result_oid, tip_of(&fx.repo, "main"), "the record names the result the destination holds");
    assert_eq!(integrations(done), 1, "integration is on the timeline once");
    assert_eq!(completions(done), 1, "completion is on the timeline once");
    let order: Vec<_> = done
        .activity
        .iter()
        .filter_map(|e| match &e.kind {
            ActivityKind::IntegratedLocally { .. } => Some("integrated"),
            ActivityKind::CompletedLocally {} => Some("completed"),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["integrated", "completed"]);
    assert!(done.pr_url.is_none() && done.external_refs.iter().all(|r| !r.is_pr()), "no pull request");
    assert!(!ref_exists(&fx.repo, &git::pin_ref(fx.task_id)), "the pin is released");
    let disk = on_disk(fx);
    assert_eq!(disk.status, TaskStatus::Done);
    assert_eq!(disk.local_integration, Some(record), "the file the next start reads says the same");
    assert_eq!(disk.activity, done.activity);
}

// ---- Strategies ----

#[tokio::test]
async fn an_approved_task_is_fast_forwarded_and_completed_in_one_action() {
    let fx = fixture().await;
    let before = tip_of(&fx.repo, "main");
    let history = current(&fx).await.human_review.clone();

    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::Ready);
    assert_eq!(p.strategy, Some(IntegrationStrategy::FastForward));
    assert_eq!(p.approved_tip.as_deref(), Some(fx.tip.as_str()));
    assert_eq!(p.target_tip.as_deref(), Some(before.as_str()));
    assert!(!p.has_remote);
    assert_eq!(tip_of(&fx.repo, "main"), before, "a preview moves nothing");
    assert!(fx.worktree.exists(), "and removes nothing");

    let done = complete_and_integrate(&fx.state, fx.task_id, confirmation(&p)).await.expect("completed");
    assert_eq!(tip_of(&fx.repo, "main"), fx.tip);
    assert_done_and_integrated(&fx, &done, IntegrationStrategy::FastForward);
    assert_eq!(done.human_review, history, "the approval history is preserved");
    assert!(!ref_exists(&fx.repo, "refs/remotes/origin/main"), "nothing was fetched or pushed");
    assert_eq!(shown(&fx).await.state, PreviewState::Integrated);
}

#[tokio::test]
async fn a_diverged_destination_gets_a_merge_commit_and_the_task_completes() {
    let fx = fixture().await;
    let moved = commit_on_main(&fx.repo, "other.txt", "other\n");
    let p = shown(&fx).await;
    assert_eq!(p.strategy, Some(IntegrationStrategy::MergeCommit));

    let done = complete_and_integrate(&fx.state, fx.task_id, confirmation(&p)).await.expect("completed");
    assert_done_and_integrated(&fx, &done, IntegrationStrategy::MergeCommit);
    let merged = tip_of(&fx.repo, "main");
    let parents = run(&fx.repo, &["rev-list", "--parents", "-n", "1", &merged]);
    let parents: Vec<_> = parents.split(' ').skip(1).collect();
    assert_eq!(parents, [moved.as_str(), fx.tip.as_str()], "both lines are kept, neither rewritten");
}

#[tokio::test]
async fn a_destination_that_already_holds_the_work_completes_without_moving() {
    let fx = fixture().await;
    run(&fx.repo, &["branch", "-f", "main", &fx.tip]);
    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::AlreadyContained);
    let done = complete_and_integrate(&fx.state, fx.task_id, confirmation(&p)).await.expect("completed");
    assert_done_and_integrated(&fx, &done, IntegrationStrategy::AlreadyContained);
    assert_eq!(tip_of(&fx.repo, "main"), fx.tip);
}

// ---- The destination checkout ----

#[tokio::test]
async fn a_checked_out_destination_is_advanced_only_in_the_checkout_that_was_shown() {
    let fx = fixture_with(false, None).await;
    let before = tip_of(&fx.repo, "main");
    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::ReadyInCheckout);
    let shown_at = p.checked_out_at.clone().expect("the checkout is named");

    // Without naming that checkout, nothing is touched.
    let mut unnamed = confirmation(&p);
    unnamed.checkout_path = None;
    let refused = complete_and_integrate(&fx.state, fx.task_id, unnamed).await.expect_err("not confirmed");
    assert!(refused.contains("checked out"), "{refused}");
    assert_untouched(&fx, &before).await;

    let done = complete_and_integrate(&fx.state, fx.task_id, confirmation(&p)).await.expect("completed");
    assert_done_and_integrated(&fx, &done, IntegrationStrategy::FastForward);
    assert_eq!(done.local_integration.unwrap().checkout_path.as_deref(), Some(shown_at.as_str()));
    assert_eq!(std::fs::read_to_string(fx.repo.join("work.txt")).unwrap(), "work\n", "the checkout moved with the branch");
    assert_eq!(run(&fx.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn a_destination_checkout_that_became_dirty_after_the_preview_refuses_and_changes_nothing() {
    let fx = fixture_with(false, None).await;
    let before = tip_of(&fx.repo, "main");
    let p = shown(&fx).await;
    let confirmed = confirmation(&p);
    std::fs::write(fx.repo.join("README.md"), "edited by a person\n").unwrap();

    let refused = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect_err("dirty");
    assert!(refused.contains("uncommitted changes"), "{refused}");
    assert_untouched(&fx, &before).await;
    assert_eq!(std::fs::read_to_string(fx.repo.join("README.md")).unwrap(), "edited by a person\n", "their work is intact");

    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::Refused, "and the preview says why");
    assert!(p.checkout_problems.iter().any(|m| m.contains("uncommitted changes")));
}

#[tokio::test]
async fn conflicts_refuse_before_anything_moves() {
    let fx = fixture().await;
    // The task and the destination both change the same file.
    std::fs::write(fx.worktree.join("README.md"), "task version\n").unwrap();
    run(&fx.worktree, &["add", "."]);
    run(&fx.worktree, &["commit", "-q", "-m", "task edits README"]);
    // A new commit means a new approval of the exact tip.
    let tip = run(&fx.worktree, &["rev-parse", "HEAD"]);
    {
        let mut tasks = fx.state.task.tasks.write().await;
        let t = tasks.get_mut(&fx.task_id).unwrap();
        t.human_review.record_arrival();
        t.human_review.approve(Some(tip), chrono::Utc::now());
    }
    let main_before = commit_on_main_readme(&fx.repo, "main version\n");

    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::Refused);
    assert_eq!(p.conflicts, ["README.md"]);

    let guess = IntegrationConfirmation {
        target_branch: "main".into(),
        target_tip: main_before.clone(),
        approved_tip: p.approved_tip.clone().expect("the approved commit is shown even when refused"),
        checkout_path: None,
    };
    let refused = complete_and_integrate(&fx.state, fx.task_id, guess).await.expect_err("conflicts");
    assert!(refused.to_lowercase().contains("conflict"), "{refused}");
    assert_untouched(&fx, &main_before).await;
}

fn commit_on_main_readme(repo: &Path, content: &str) -> String {
    commit_on_main(repo, "README.md", content)
}

// ---- Stale confirmations ----

#[tokio::test]
async fn a_destination_that_moved_after_the_preview_refuses_and_changes_nothing() {
    let fx = fixture().await;
    let p = shown(&fx).await;
    let confirmed = confirmation(&p);
    let moved = commit_on_main(&fx.repo, "late.txt", "late\n");

    let refused = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect_err("stale");
    assert!(refused.contains("moved after"), "{refused}");
    assert_untouched(&fx, &moved).await;
}

#[tokio::test]
async fn an_approval_that_is_not_the_one_shown_refuses_and_changes_nothing() {
    let fx = fixture().await;
    let before = tip_of(&fx.repo, "main");
    let p = shown(&fx).await;
    let mut confirmed = confirmation(&p);
    confirmed.approved_tip = "0".repeat(40);

    let refused = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect_err("another commit");
    assert!(refused.contains("not the one that was shown"), "{refused}");
    assert_untouched(&fx, &before).await;
}

#[tokio::test]
async fn an_approval_withdrawn_after_the_preview_refuses() {
    let fx = fixture().await;
    let before = tip_of(&fx.repo, "main");
    let confirmed = confirmation(&shown(&fx).await);
    {
        let mut tasks = fx.state.task.tasks.write().await;
        tasks.get_mut(&fx.task_id).unwrap().human_review.withdraw_approval();
    }
    let refused = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect_err("not approved");
    assert!(refused.contains("no approval"), "{refused}");
    assert_untouched(&fx, &before).await;
}

#[tokio::test]
async fn a_task_branch_rewritten_after_the_approval_is_not_integrated() {
    let fx = fixture().await;
    let before = tip_of(&fx.repo, "main");
    let confirmed = confirmation(&shown(&fx).await);
    run(&fx.worktree, &["commit", "-q", "--amend", "-m", "rewritten"]);

    let refused = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect_err("rewritten");
    assert!(refused.contains("no longer on the task's branch"), "{refused}");
    assert_untouched(&fx, &before).await;
}

#[tokio::test]
async fn commits_after_the_approval_are_reported_and_not_integrated() {
    let fx = fixture().await;
    std::fs::write(fx.worktree.join("more.txt"), "more\n").unwrap();
    run(&fx.worktree, &["add", "."]);
    run(&fx.worktree, &["commit", "-q", "-m", "unapproved"]);
    let p = shown(&fx).await;
    assert_eq!(p.unapproved_commits, 1);

    let done = complete_and_integrate(&fx.state, fx.task_id, confirmation(&p)).await.expect("completed");
    assert_eq!(tip_of(&fx.repo, "main"), fx.tip, "only the approved commit reached the destination");
    assert_ne!(tip_of(&fx.repo, &fx.branch), fx.tip, "the later commit is still on the Task branch");
    assert_eq!(done.local_integration.unwrap().approved_tip, fx.tip);
}

// ---- Approvals that did not record a commit ----

#[tokio::test]
async fn a_legacy_approval_has_to_be_reconfirmed_before_it_can_be_completed() {
    let fx = fixture().await;
    let before = tip_of(&fx.repo, "main");
    {
        let mut tasks = fx.state.task.tasks.write().await;
        let t = tasks.get_mut(&fx.task_id).unwrap();
        t.human_review.entries.last_mut().unwrap().approved_tip = None;
    }
    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::NeedsReconfirmation);
    let shown_tip = p.reconfirm.clone().expect("the commit to confirm").branch_tip;
    assert_eq!(shown_tip, fx.tip);

    let guess = IntegrationConfirmation {
        target_branch: "main".into(),
        target_tip: before.clone(),
        approved_tip: fx.tip.clone(),
        checkout_path: None,
    };
    let refused = complete_and_integrate(&fx.state, fx.task_id, guess).await.expect_err("no commit on record");
    assert!(refused.contains("did not record which commit"), "{refused}");
    assert_untouched(&fx, &before).await;

    // Confirming a commit that is not the branch tip is refused.
    assert!(reconfirm(&fx.state, fx.task_id, &"1".repeat(40)).await.is_err());
    let confirmed = reconfirm(&fx.state, fx.task_id, &shown_tip).await.expect("confirmed");
    assert_eq!(confirmed.status, TaskStatus::HumanReview, "confirming completes nothing");
    assert!(confirmed.human_review.entries.last().unwrap().tip_reconfirmed);

    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::Ready);
    let done = complete_and_integrate(&fx.state, fx.task_id, confirmation(&p)).await.expect("completed");
    assert_done_and_integrated(&fx, &done, IntegrationStrategy::FastForward);
}

// ---- Eligibility ----

#[tokio::test]
async fn only_an_approved_task_in_human_review_can_be_completed_and_integrated() {
    let fx = fixture().await;
    let before = tip_of(&fx.repo, "main");
    let confirmed = confirmation(&shown(&fx).await);

    // An integration after completion is the other action, and refuses a
    // task that is still in review.
    let refused = integrate(&fx.state, fx.task_id, confirmed.clone()).await.expect_err("still in review");
    assert!(refused.contains("completed locally"), "{refused}");
    assert_untouched(&fx, &before).await;

    // Undecided.
    {
        let mut tasks = fx.state.task.tasks.write().await;
        tasks.get_mut(&fx.task_id).unwrap().human_review.withdraw_approval();
    }
    assert_eq!(shown(&fx).await.state, PreviewState::Refused);
    assert!(complete_and_integrate(&fx.state, fx.task_id, confirmed.clone()).await.is_err());

    // Completed without integration: the other action integrates it.
    human_review::approve_shown(&fx.state, fx.task_id, false).await.unwrap();
    human_review::complete_locally(&fx.state, fx.task_id, true).await.expect("completed locally");
    let refused = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect_err("already completed");
    assert!(refused.contains("without its approved work being integrated"), "{refused}");
}

#[tokio::test]
async fn a_task_with_a_pull_request_or_one_being_opened_is_not_integrated_locally() {
    let fx = fixture().await;
    let before = tip_of(&fx.repo, "main");
    let confirmed = confirmation(&shown(&fx).await);
    {
        let _mark = fx.state.task.deliveries.start(fx.task_id);
        assert_eq!(shown(&fx).await.state, PreviewState::Refused);
        let refused = complete_and_integrate(&fx.state, fx.task_id, confirmed.clone()).await.expect_err("being opened");
        assert!(refused.contains("pull request"), "{refused}");
    }
    {
        let mut tasks = fx.state.task.tasks.write().await;
        tasks.get_mut(&fx.task_id).unwrap().pr_url = Some("https://github.com/o/r/pull/1".into());
    }
    let refused = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect_err("has one");
    assert!(refused.contains("pull request"), "{refused}");
    assert_eq!(tip_of(&fx.repo, "main"), before);
    assert!(current(&fx).await.local_integration.is_none());
}

#[tokio::test]
async fn uncommitted_work_in_the_task_checkout_refuses_before_anything_moves() {
    let fx = fixture().await;
    let before = tip_of(&fx.repo, "main");
    let confirmed = confirmation(&shown(&fx).await);
    std::fs::write(fx.worktree.join("unsaved.txt"), "not committed\n").unwrap();

    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::Refused);
    assert!(p.reason.unwrap().contains("uncommitted changes"));
    let refused = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect_err("uncommitted");
    assert!(refused.contains("unsaved.txt"), "{refused}");
    assert_untouched(&fx, &before).await;
    assert!(fx.worktree.join("unsaved.txt").exists(), "their work is intact");
}

#[tokio::test]
async fn an_agent_still_running_for_the_task_refuses_before_anything_moves() {
    let fx = fixture().await;
    let before = tip_of(&fx.repo, "main");
    let confirmed = confirmation(&shown(&fx).await);
    let executor = crate::test_helpers::attach_test_executor(&fx.state);
    executor.register_fake_reviewing_owner_for_test(fx.task_id).await;

    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::Refused);
    let refused = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect_err("an agent owns the task");
    assert!(refused.contains("still running"), "{refused}");
    assert_untouched(&fx, &before).await;
}

// ---- Concurrency and repetition ----

#[tokio::test]
async fn two_requests_at_once_integrate_and_complete_once() {
    let fx = fixture().await;
    let confirmed = confirmation(&shown(&fx).await);
    let (a, b) = tokio::join!(
        complete_and_integrate(&fx.state, fx.task_id, confirmed.clone()),
        complete_and_integrate(&fx.state, fx.task_id, confirmed.clone()),
    );
    let (a, b) = (a.expect("one completes"), b.expect("the other sees it done"));
    assert_eq!(a.status, TaskStatus::Done);
    assert_eq!(b.status, TaskStatus::Done);
    let task = current(&fx).await;
    assert_done_and_integrated(&fx, &task, IntegrationStrategy::FastForward);
}

#[tokio::test]
async fn asking_again_once_it_is_done_changes_nothing() {
    let fx = fixture().await;
    let confirmed = confirmation(&shown(&fx).await);
    let first = complete_and_integrate(&fx.state, fx.task_id, confirmed.clone()).await.unwrap();
    let on_disk_first = serde_json::to_value(on_disk(&fx)).unwrap();
    let main_after = tip_of(&fx.repo, "main");

    for _ in 0..2 {
        let again = complete_and_integrate(&fx.state, fx.task_id, confirmed.clone()).await.expect("idempotent");
        assert_eq!(again.activity, first.activity);
        assert_eq!(again.updated_at, first.updated_at, "nothing was written");
    }
    assert_eq!(serde_json::to_value(on_disk(&fx)).unwrap(), on_disk_first);
    assert_eq!(tip_of(&fx.repo, "main"), main_after);
}

#[tokio::test]
async fn a_second_task_cannot_interleave_into_the_same_destination() {
    let fx = fixture().await;
    let confirmed = confirmation(&shown(&fx).await);
    // A second task, approved, whose branch is another line from main.
    run(&fx.repo, &["branch", "task-two", "main"]);
    run(&fx.repo, &["checkout", "-q", "task-two"]);
    std::fs::write(fx.repo.join("two.txt"), "two\n").unwrap();
    run(&fx.repo, &["add", "."]);
    run(&fx.repo, &["commit", "-q", "-m", "two"]);
    let two_tip = run(&fx.repo, &["rev-parse", "HEAD"]);
    run(&fx.repo, &["checkout", "-q", "--detach"]);
    let mut second = create_test_task_full("second", fx.project_id, TaskStatus::HumanReview, 1);
    second.branch_name = Some("task-two".into());
    second.human_review.record_arrival();
    second.human_review.approve(Some(two_tip), chrono::Utc::now());
    let second_id = second.id;
    fx.state.task.tasks.write().await.insert(second_id, second);

    let p2 = preview(&fx.state, second_id).await.unwrap();
    let c2 = confirmation(&p2);
    let (a, b) = tokio::join!(
        complete_and_integrate(&fx.state, fx.task_id, confirmed),
        complete_and_integrate(&fx.state, second_id, c2),
    );
    // Both asked against the same destination commit, so exactly one plan
    // can hold; the other is told to look again and changed nothing.
    assert!(a.is_ok() ^ b.is_ok(), "exactly one integrates: {a:?} / {b:?}");
    let loser = a.err().or(b.err()).unwrap();
    assert!(loser.contains("moved after"), "{loser}");
}

// ---- Interruptions ----

/// A pending record, as a crash right after it was persisted leaves it.
async fn pending_unmoved(fx: &Fx) -> git::Plan {
    let c = confirmation(&shown(fx).await);
    let plan = git::plan(&fx.repo, &c.target_branch, &c.approved_tip, "m", true).await.unwrap();
    git::pin(&fx.repo, fx.task_id, &plan.result).await.unwrap();
    let record = crate::domain::LocalIntegration {
        state: IntegrationState::Pending,
        target_branch: plan.target_branch.clone(),
        target_old_oid: plan.target_old.clone(),
        approved_tip: plan.approved_tip.clone(),
        result_oid: plan.result.clone(),
        strategy: plan.strategy,
        checkout_path: None,
        started_at: chrono::Utc::now(),
        completed_at: None,
        completes_task: true,
    };
    let id = fx.task_id;
    let amend = move |s: &mut HashMap<Uuid, Task>| s.get_mut(&id).unwrap().local_integration = Some(record.clone());
    crate::lifecycle::record(&fx.state.task.tasks, &fx.state.storage, id, &amend).await.unwrap();
    plan
}

#[tokio::test]
async fn a_crash_before_the_destination_moved_is_finished_by_resuming_and_completes_the_task() {
    let fx = fixture().await;
    let plan = pending_unmoved(&fx).await;
    let p = shown(&fx).await;
    assert_eq!((p.state, p.observed.as_deref()), (PreviewState::Pending, Some("unmoved")));
    assert!(p.integration.unwrap().completes_task, "the pending record carries the intent to complete");
    assert_eq!(current(&fx).await.status, TaskStatus::HumanReview, "not Done while only intended");

    let done = resume(&fx.state, fx.task_id).await.expect("resumed");
    assert_eq!(tip_of(&fx.repo, "main"), plan.result);
    assert_done_and_integrated(&fx, &done, IntegrationStrategy::FastForward);
}

#[tokio::test]
async fn a_crash_after_the_destination_moved_records_it_and_completes_without_moving_again() {
    let fx = fixture().await;
    let plan = pending_unmoved(&fx).await;
    git::advance(fx.repo.to_str().unwrap(), &plan, None).await.unwrap();
    assert_eq!(shown(&fx).await.observed.as_deref(), Some("applied"));
    assert!(discard(&fx.state, fx.task_id).await.unwrap_err().contains("already holds"));

    // Asking for the action again is the same as resuming.
    let confirmed = IntegrationConfirmation {
        target_branch: "main".into(),
        target_tip: plan.target_old.clone(),
        approved_tip: fx.tip.clone(),
        checkout_path: None,
    };
    let done = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect("recorded and completed");
    assert_done_and_integrated(&fx, &done, IntegrationStrategy::FastForward);
    assert_eq!(tip_of(&fx.repo, "main"), plan.result);
}

/// Resuming to completion can finish the integration and still not complete
/// the task: the answer is `Ok` with the task in review and the reason, which
/// the drawer must not report as a completion.
#[tokio::test]
async fn resuming_an_integration_whose_checkout_cannot_be_removed_keeps_the_task_in_review() {
    let fx = fixture().await;
    let plan = pending_unmoved(&fx).await;
    run(&fx.repo, &["worktree", "lock", fx.worktree.to_str().unwrap()]);

    let kept = resume(&fx.state, fx.task_id).await.expect("integrated, not completed");
    assert_eq!(tip_of(&fx.repo, "main"), plan.result, "the destination holds the approved work");
    assert_eq!(kept.status, TaskStatus::HumanReview, "the task is not reported complete");
    assert!(!kept.completed_locally());
    assert!(kept.error_message.as_deref().is_some_and(|m| m.contains("was kept")), "{:?}", kept.error_message);
    let record = kept.local_integration.clone().expect("the integration is recorded");
    assert_eq!(record.state, IntegrationState::Integrated);
    assert_eq!((integrations(&kept), completions(&kept)), (1, 0));
    assert!(fx.worktree.exists(), "the Task Checkout is kept");

    run(&fx.repo, &["worktree", "unlock", fx.worktree.to_str().unwrap()]);
    let done = human_review::complete_locally(&fx.state, fx.task_id, true).await.expect("completed");
    assert_done_and_integrated(&fx, &done, IntegrationStrategy::FastForward);
}

#[tokio::test]
async fn a_pending_integration_dropped_before_it_moved_leaves_the_task_in_review() {
    let fx = fixture().await;
    let plan = pending_unmoved(&fx).await;
    let waiting = current(&fx).await;
    assert_eq!(waiting.status, TaskStatus::HumanReview);
    assert_eq!(waiting.needs_you(false), Some(crate::domain::task::AttentionReason::IntegrationUnfinished));
    let task = discard(&fx.state, fx.task_id).await.unwrap();
    assert!(task.local_integration.is_none());
    assert_eq!(task.needs_you(false), None);
    assert_eq!(task.status, TaskStatus::HumanReview);
    assert_eq!(tip_of(&fx.repo, "main"), plan.target_old);
    assert!(!ref_exists(&fx.repo, &git::pin_ref(fx.task_id)));
    assert!(fx.worktree.exists());
}

#[tokio::test]
async fn a_pending_integration_blocks_a_plain_completion_and_a_pull_request() {
    let fx = fixture().await;
    pending_unmoved(&fx).await;
    let refused = human_review::complete_locally(&fx.state, fx.task_id, true).await.expect_err("pending");
    assert!(refused.contains("started and not finished"), "{refused}");
    let refused = crate::commands::pr::create_pr_for_approved_task(&fx.state, fx.task_id).await.expect_err("pending");
    assert!(refused.contains("integrated into a local branch"), "{refused}");
    assert_eq!(current(&fx).await.status, TaskStatus::HumanReview);
    assert!(fx.worktree.exists());
}

/// The state a crash between recording the integration and finishing the
/// completion leaves: the destination holds the work, the Task is still in
/// Human Review.
#[tokio::test]
async fn a_crash_between_the_integration_and_the_completion_is_finished_without_repeating_it() {
    let fx = fixture().await;
    let plan = pending_unmoved(&fx).await;
    git::advance(fx.repo.to_str().unwrap(), &plan, None).await.unwrap();
    {
        let id = fx.task_id;
        let amend = |s: &mut HashMap<Uuid, Task>| {
            let t = s.get_mut(&id).unwrap();
            let r = t.local_integration.as_mut().unwrap();
            r.state = IntegrationState::Integrated;
            r.completed_at = Some(chrono::Utc::now());
            t.record_activity(ActivityKind::IntegratedLocally {
                target: "main".into(),
                strategy: "fast_forward".into(),
                commit: plan.result.clone(),
            });
        };
        crate::lifecycle::record(&fx.state.task.tasks, &fx.state.storage, id, &amend).await.unwrap();
    }
    git::unpin(&fx.repo, fx.task_id).await.unwrap();

    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::CompletionPending);
    assert_eq!(p.target_branch.as_deref(), Some("main"));
    assert_eq!(p.approved_tip.as_deref(), Some(fx.tip.as_str()));

    // A new integration is never planned for it, and asking is idempotent.
    let main_before = tip_of(&fx.repo, "main");
    let confirmed = IntegrationConfirmation {
        target_branch: "main".into(),
        target_tip: main_before.clone(),
        approved_tip: fx.tip.clone(),
        checkout_path: None,
    };
    let done = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect("completed");
    assert_eq!(tip_of(&fx.repo, "main"), main_before);
    assert_done_and_integrated(&fx, &done, IntegrationStrategy::FastForward);
}

/// A completion interrupted while removing the checkout leaves
/// `cleanup_in_flight` set. That refuses a new integration, never finishing
/// this one.
#[tokio::test]
async fn a_completion_interrupted_during_the_removal_is_finished_by_asking_again() {
    let fx = fixture().await;
    let confirmed = confirmation(&shown(&fx).await);
    // The completion refuses to remove a locked checkout, which stands in for
    // any removal that cannot finish; then the process "dies" mid-removal.
    run(&fx.repo, &["worktree", "lock", fx.worktree.to_str().unwrap()]);
    complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect("integrated, not completed");
    run(&fx.repo, &["worktree", "unlock", fx.worktree.to_str().unwrap()]);
    {
        let mut tasks = fx.state.task.tasks.write().await;
        tasks.get_mut(&fx.task_id).unwrap().cleanup_in_flight = true;
    }
    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::CompletionPending, "still answerable");

    let done = human_review::complete_locally(&fx.state, fx.task_id, true).await.expect("finished");
    assert_done_and_integrated(&fx, &done, IntegrationStrategy::FastForward);
}

// ---- Failures after the integration ----

#[tokio::test]
async fn a_checkout_git_will_not_remove_keeps_the_integration_and_the_task_in_review() {
    let fx = fixture().await;
    let confirmed = confirmation(&shown(&fx).await);
    run(&fx.repo, &["worktree", "lock", fx.worktree.to_str().unwrap()]);

    let kept = complete_and_integrate(&fx.state, fx.task_id, confirmed.clone()).await.expect("integrated");
    assert_eq!(tip_of(&fx.repo, "main"), fx.tip, "the destination holds the approved work");
    assert_eq!(kept.status, TaskStatus::HumanReview, "the task is not reported complete");
    assert!(!kept.completed_locally());
    assert!(kept.error_message.as_deref().is_some_and(|m| m.contains("was kept")), "{:?}", kept.error_message);
    assert!(!kept.cleanup_in_flight);
    let record = kept.local_integration.clone().expect("the integration is recorded");
    assert_eq!(record.state, IntegrationState::Integrated);
    assert_eq!((integrations(&kept), completions(&kept)), (1, 0));
    assert!(fx.worktree.exists(), "the Task Checkout is kept");
    assert_eq!(tip_of(&fx.repo, &fx.branch), fx.tip, "and so is the Task branch");
    assert_eq!(on_disk(&fx).local_integration, Some(record));

    // What the drawer shows meanwhile, and that no second delivery is offered.
    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::CompletionPending);
    assert!(p.reason.unwrap().contains("was kept"));
    assert!(crate::commands::pr::create_pr_for_approved_task(&fx.state, fx.task_id).await.is_err());

    // Asking again while it is still locked repeats nothing and reports the same.
    let again = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect("same answer");
    assert_eq!((integrations(&again), completions(&again)), (1, 0));
    assert_eq!(tip_of(&fx.repo, "main"), fx.tip);

    // Resolved, the finishing step completes it once.
    run(&fx.repo, &["worktree", "unlock", fx.worktree.to_str().unwrap()]);
    let done = human_review::complete_locally(&fx.state, fx.task_id, true).await.expect("completed");
    assert_done_and_integrated(&fx, &done, IntegrationStrategy::FastForward);
    assert!(done.error_message.is_none(), "the earlier reason is cleared");
}

#[cfg(unix)]
#[tokio::test]
async fn a_board_that_cannot_be_written_refuses_before_the_destination_moves() {
    let fx = fixture().await;
    let before = tip_of(&fx.repo, "main");
    let confirmed = confirmation(&shown(&fx).await);
    let dir = tasks_dir(&fx);
    let Some(_unwritable) = crate::test_helpers::UnwritableDir::new(&dir) else {
        return; // Root ignores permission bits; nothing could be proven.
    };

    let refused = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect_err("cannot record");
    assert!(refused.contains("because recording it failed"), "{refused}");
    assert_eq!(tip_of(&fx.repo, "main"), before, "an integration that cannot be recorded does not happen");
    assert_eq!(current(&fx).await.status, TaskStatus::HumanReview);
    assert!(current(&fx).await.local_integration.is_none());
    assert!(!ref_exists(&fx.repo, &git::pin_ref(fx.task_id)));
    assert!(fx.worktree.exists());
}

/// The directory that holds the project's tasks file.
fn tasks_dir(fx: &Fx) -> PathBuf {
    fn find(dir: &Path, name_ends: &str, id: &str) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find(&path, name_ends, id) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|n| n.to_string_lossy().ends_with(name_ends))
                && std::fs::read_to_string(&path).is_ok_and(|c| c.contains(id))
            {
                return path.parent().map(Path::to_path_buf);
            }
        }
        None
    }
    find(fx.paths.data_dir(), ".toml", &fx.task_id.to_string())
        .or_else(|| find(fx.paths.config_dir(), ".toml", &fx.task_id.to_string()))
        .expect("the tasks file")
}

// ---- Restart ----

#[tokio::test]
async fn the_whole_outcome_survives_a_restart() {
    let fx = fixture().await;
    let confirmed = confirmation(&shown(&fx).await);
    let done = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.unwrap();

    let (restarted, _) = crate::app_core::build_state_with_paths(fx.paths.clone()).await.expect("restart");
    let reloaded = restarted.task.tasks.read().await.get(&fx.task_id).cloned().expect("the task is back");
    assert_eq!(reloaded.status, TaskStatus::Done);
    assert_eq!(reloaded.local_integration, done.local_integration);
    assert_eq!(reloaded.activity, done.activity);
    assert_eq!(reloaded.human_review, done.human_review);
    assert_eq!((integrations(&reloaded), completions(&reloaded)), (1, 1));
    assert!(reloaded.worktree_path.is_none() && !reloaded.cleanup_in_flight);
}

#[tokio::test]
async fn an_integration_waiting_for_its_completion_survives_a_restart_unchanged() {
    let fx = fixture().await;
    let confirmed = confirmation(&shown(&fx).await);
    run(&fx.repo, &["worktree", "lock", fx.worktree.to_str().unwrap()]);
    let kept = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.unwrap();

    let (restarted, _) = crate::app_core::build_state_with_paths(fx.paths.clone()).await.expect("restart");
    let reloaded = restarted.task.tasks.read().await.get(&fx.task_id).cloned().expect("the task is back");
    assert_eq!(reloaded.status, TaskStatus::HumanReview);
    assert_eq!(reloaded.local_integration, kept.local_integration);
    assert_eq!((integrations(&reloaded), completions(&reloaded)), (1, 0));
    assert!(reloaded.worktree_path.is_some());
    // Nothing completes by itself on start: a person asks.
    assert!(fx.worktree.exists());
}

// ---- Remotes ----

#[tokio::test]
async fn a_project_with_a_github_remote_integrates_locally_when_that_is_chosen_and_pushes_nothing() {
    let fx = fixture_with(true, Some("https://github.com/example/never-contacted.git")).await;
    let p = shown(&fx).await;
    assert_eq!(p.state, PreviewState::Ready);
    assert!(p.has_remote, "the preview says there is a remote");

    // A PR remains the default delivery; this is the explicit local choice,
    // and completing locally without it is still refused.
    let default_refusal = human_review::complete_locally(&fx.state, fx.task_id, false).await.expect_err("PR is the default");
    assert!(default_refusal.contains("Create PR"), "{default_refusal}");

    let done = complete_and_integrate(&fx.state, fx.task_id, confirmation(&p)).await.expect("completed");
    assert_done_and_integrated(&fx, &done, IntegrationStrategy::FastForward);
    assert!(!ref_exists(&fx.repo, "refs/remotes/origin/task-one"), "no branch was pushed");
    assert!(!ref_exists(&fx.repo, "refs/remotes/origin/main"), "nothing was fetched or pushed");
    assert_eq!(run(&fx.repo, &["remote", "get-url", "origin"]), "https://github.com/example/never-contacted.git");
}

#[tokio::test]
async fn work_that_reached_a_local_branch_is_not_also_offered_as_a_pull_request() {
    let fx = fixture_with(true, Some("https://github.com/example/never-contacted.git")).await;
    let confirmed = confirmation(&shown(&fx).await);
    run(&fx.repo, &["worktree", "lock", fx.worktree.to_str().unwrap()]);
    complete_and_integrate(&fx.state, fx.task_id, confirmed).await.expect("integrated");

    let refused = crate::commands::pr::create_pr_for_approved_task(&fx.state, fx.task_id).await.expect_err("one delivery");
    assert!(refused.contains("integrated into a local branch"), "{refused}");
    assert!(current(&fx).await.pr_url.is_none());
}

// ---- What follows ----

/// A Task started from the local base afterwards sees the integrated work,
/// by the same facts a dependent Task reads.
#[tokio::test]
async fn the_integrated_work_is_in_the_base_a_dependent_task_starts_from() {
    let fx = fixture().await;
    let confirmed = confirmation(&shown(&fx).await);
    let done = complete_and_integrate(&fx.state, fx.task_id, confirmed).await.unwrap();

    let record = done.local_integration.clone().unwrap();
    let integrated = record.state == IntegrationState::Integrated;
    assert!(
        done.completed_locally() && integrated && done.human_review.approved_tip() == Some(record.approved_tip.as_str()),
        "the facts a dependent Task's start reads"
    );
    let base = crate::worktree::resolve_default_base(
        fx.repo.to_str().unwrap(),
        Some(&crate::domain::project::ProjectBase::LocalBranch { branch: "main".into() }),
    )
    .await
    .expect("the base resolves");
    assert!(crate::worktree::restack::is_ancestor(&fx.repo, &record.approved_tip, &base.commit).await.unwrap());
}
