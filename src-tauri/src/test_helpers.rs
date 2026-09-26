//! Test helper utilities
//! Common test fixtures and utilities for backend testing

use crate::domain::{Task, TaskStatus, TaskCategory, TaskPriority, TaskComplexity, TaskImpact, SecuritySeverity, TaskPhase};
pub use uuid::Uuid;
use chrono::Utc;

/// The one process-global lock for every `--lib` unit test that mutates
/// process-wide `PATH` to install a fake command shim (a fixture `claude` or
/// `gh` binary at the front of `PATH`).
///
/// `PATH` belongs to the whole test *process*, not to any one module. Before
/// this lock existed, `queue::executor`'s review-lifecycle tests and
/// `commands::pr`'s repo-level PR-creation tests each guarded their own PATH
/// mutation with a private, module-local `static PATH_LOCK` of the same
/// name. Two distinct `Mutex`es do not serialize against each other, so
/// under default `cargo test` thread parallelism a thread running one
/// module's fixture install/restore could interleave with the other
/// module's, corrupting whichever one restored `PATH` last.
///
/// Concretely reproduced (see the Unit 5C2 corrective report): with both
/// suites running in the same default-parallel `cargo test -p slashit-ui
/// --lib` process,
/// `commands::pr::tests::repo_level_pr_creation::bulk_create_prs_applies_the_same_contract_as_create_pr`
/// escaped its own `MockGh` and reached the developer's real `gh` binary
/// (observed failure: `gh failed: none of the git remotes configured for
/// this repository point to a known GitHub host`), and
/// `queue::executor::tests::review_lifecycle::a_running_fix_agent_can_be_stopped_safely`
/// spawned something other than its own fixture script and never observed
/// its pidfile being written (`fixture never wrote its pid file; the
/// blocking role was never reached`).
///
/// Every test in this crate that mutates process-global `PATH` (or removes
/// it) to install a fake command must acquire this lock -- not a private
/// one -- before saving/mutating `PATH`, and must not release it until
/// `PATH` has been fully restored to what it was before the mutation
/// (typically by holding the guard for the test's whole body, so an owning
/// fixture's `Drop` impl runs and restores `PATH` before the lock itself is
/// released).
///
/// `#[cfg(test)]`: this is compiled only when building the crate's own unit
/// test binary (`cargo test -p slashit-ui --lib`), never into the release
/// artifact -- unlike the rest of this module, which stays uncfg'd so the
/// separate integration-test binaries under `tests/` can link it too.
#[cfg(test)]
pub static PATH_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// The IPC server, reachable from the integration tests.
///
/// `tests/ipc_integration.rs` is a separate crate, so it can only name items
/// that are public *and* reachable from the crate root. Re-exporting here means
/// the tests keep working whatever `lib.rs` decides about the visibility of the
/// `ipc` module itself.
pub use crate::ipc::{serve, IpcContext, IpcServer};

/// An [`IpcContext`] with empty state, rooted at explicit directories.
///
/// Everything the IPC server does — framing, protocol versioning,
/// authentication, authorization — is worth testing without a window, a webview
/// or the developer's real configuration. So the event sink discards, the
/// instance control is inert, and every path lands in whatever tempdir the
/// caller passed.
pub fn ipc_test_context(paths: std::sync::Arc<crate::config::paths::AppPaths>) -> IpcContext {
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{Mutex, RwLock};

    let tasks = Arc::new(RwLock::new(HashMap::new()));

    // Built field by field rather than through `PtyState::new`, which resolves
    // the real OS directories and rewrites the developer's live terminal
    // session file as a side effect.
    let pty = crate::pty::PtyState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        scrollback: crate::pty::store::ScrollbackManager::new(),
        store: Arc::new(
            crate::pty::store::SessionStore::with_paths(&paths)
                .expect("a session store under a tempdir should always be creatable"),
        ),
    };

    IpcContext {
        tasks: tasks.clone(),
        projects: Arc::new(RwLock::new(HashMap::new())),
        executions: Arc::new(RwLock::new(HashMap::new())),
        pty,
        queue_manager: Arc::new(RwLock::new(crate::queue::QueueManager::with_config(tasks))),
        storage: crate::config::Storage::with_paths((*paths).clone()),
        events: crate::events::null_sink(),
        control: Arc::new(crate::instance::InertControl),
        features: Arc::new(RwLock::new(
            crate::config::features::FeatureFlags::default(),
        )),
        repositories: Arc::new(RwLock::new(HashMap::new())),
        worktree_manager: Arc::new(crate::worktree::WorktreeManager::new(
            paths.clone(),
            crate::config::paths::WorktreePlacement::default(),
        )),
        task_lifecycle_locks: Arc::new(crate::lifecycle::TaskLifecycleLocks::new()),
        executor: Arc::new(tokio::sync::OnceCell::new()),
        feature_diagnostics: None,
        paths,
    }
}

/// A real [`crate::queue::TaskExecutor`], wired to an already-built
/// [`crate::AppState`]'s own handles the same way `lib.rs`'s `setup()` wires
/// the production one -- sharing its `tasks`, `storage`, `task_lifecycle_locks`
/// and friends rather than building fresh ones -- and installed into
/// `state.executor`.
///
/// For a lifecycle-front-door test that needs a live execution/review owner
/// registered (see `TaskExecutor::register_fake_running_execution_for_test`
/// and its `reviewing_handles` sibling) to prove a command like
/// `update_task_status` or `reorder_task` ends that owner before committing
/// its own status write. A command reached through `tauri::State` sees
/// exactly this executor via `state.executor.get()`, the same as it would in
/// the running app.
pub fn attach_test_executor(state: &crate::AppState) -> std::sync::Arc<crate::queue::TaskExecutor> {
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let executor = Arc::new(crate::queue::TaskExecutor::new(
        crate::queue::executor::TaskExecutorConfig {
            tasks: state.task.tasks.clone(),
            queue_manager: state.queue.manager.clone(),
            executions: Arc::new(RwLock::new(HashMap::new())),
            logs: Arc::new(RwLock::new(HashMap::new())),
            projects: state.project.projects.clone(),
            repositories: state.repository.repositories.clone(),
            workspace_registry: state.workspace.registry.clone(),
            storage: state.storage.clone(),
            worktree_manager: state.worktree_manager.clone(),
            events: state.events(),
            lifecycle: state.task_lifecycle_locks.clone(),
        },
    ));
    let _ = state.executor.set(executor.clone());
    executor
}

/// Same as [`attach_test_executor`], for an [`IpcContext`] rather than an
/// [`crate::AppState`] -- the daemon's own handles, so an IPC handler test
/// can prove the same thing through `ctx.executor` that the desktop test
/// proves through `state.executor`.
pub async fn attach_test_executor_ipc(ctx: &IpcContext) -> std::sync::Arc<crate::queue::TaskExecutor> {
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let executor = Arc::new(crate::queue::TaskExecutor::new(
        crate::queue::executor::TaskExecutorConfig {
            tasks: ctx.tasks.clone(),
            queue_manager: ctx.queue_manager.clone(),
            executions: Arc::new(RwLock::new(HashMap::new())),
            logs: Arc::new(RwLock::new(HashMap::new())),
            projects: ctx.projects.clone(),
            repositories: ctx.repositories.clone(),
            workspace_registry: Arc::new(RwLock::new(
                crate::config::WorkspaceRegistry::load_from(
                    ctx.paths.config_dir().join("workspaces.toml"),
                )
                .expect("a nonexistent workspaces file loads as an empty registry"),
            )),
            storage: ctx.storage.clone(),
            worktree_manager: ctx.worktree_manager.clone(),
            events: ctx.events.clone(),
            lifecycle: ctx.task_lifecycle_locks.clone(),
        },
    ));
    let _ = ctx.executor.set(executor.clone());
    executor
}

/// Create a test task with default values
pub fn create_test_task(title: &str) -> Task {
    Task {
        id: Uuid::new_v4(),
        project_id: Uuid::new_v4(),
        title: title.to_string(),
        description: None,
        status: TaskStatus::Backlog,
        model: "test-model".to_string(),
        planning_mode: false,
        dependencies: Vec::new(),
        worktree_id: None,
        jj_change_id: None,
        category: TaskCategory::Feature,
        priority: TaskPriority::Medium,
        complexity: TaskComplexity::Moderate,
        impact: TaskImpact::Medium,
        security_severity: SecuritySeverity::None,
        phase: TaskPhase::Planning,
        phase_progress: 0,
        overall_progress: 0,
        subtasks: Vec::new(),
        sequence_number: 0,
        github_issue_url: None,
        gitlab_issue_url: None,
        linear_ticket_id: None,
        jira_issue_key: None,
        pr_url: None,
        external_refs: Vec::new(),
        qa_signoff: None,
        human_review: None,
        stuck_since: None,
        error_message: None,
        worktree_path: None,
        branch_name: None,
        base_commit: None,
        branch_origin: None,
        cleanup_in_flight: false,
        position: 0,
        pr_review_plan: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// Create a test task with specific status
pub fn create_test_task_with_status(title: &str, status: TaskStatus) -> Task {
    let mut task = create_test_task(title);
    task.status = status;
    task
}

/// Create a test task with specific project_id, status, and position
pub fn create_test_task_full(
    title: &str,
    project_id: Uuid,
    status: TaskStatus,
    position: i32,
) -> Task {
    let mut task = create_test_task(title);
    task.project_id = project_id;
    task.status = status;
    task.position = position;
    task
}

/// Build a Task + PrReviewPlan pair suitable for exercising
/// `address_pr_review_inner`. Default plan has 2 items: one Fix (approved,
/// inline comment) and one Skip.
pub fn create_test_pr_review_setup() -> (Task, crate::domain::task::PrReviewPlan) {
    use crate::domain::task::{
        PrReviewComment, PrReviewDecision, PrReviewItem, PrReviewPlan, PrCommentKind,
    };

    let mut task = create_test_task("Fix login bug");
    task.pr_url = Some("https://github.com/test-org/test-repo/pull/42".to_string());
    task.branch_name = Some("test-branch".to_string());

    let comments = vec![
        PrReviewComment {
            id: Some(101),
            kind: PrCommentKind::Inline,
            author: "reviewer".to_string(),
            author_association: Some("MEMBER".to_string()),
            body: "This variable is unused.".to_string(),
            path: Some("src/lib.rs".to_string()),
            line: Some(42),
            url: None,
            created_at: None,
            updated_at: None,
        },
        PrReviewComment {
            id: Some(102),
            kind: PrCommentKind::Inline,
            author: "reviewer".to_string(),
            author_association: Some("MEMBER".to_string()),
            body: "Nit: rename for clarity.".to_string(),
            path: Some("src/lib.rs".to_string()),
            line: Some(60),
            url: None,
            created_at: None,
            updated_at: None,
        },
    ];

    let items = vec![
        PrReviewItem {
            comment_id: Some(101),
            summary: "Remove unused variable".to_string(),
            decision: PrReviewDecision::Fix,
            reasoning: "Confirmed unused.".to_string(),
            proposed_change: "Delete the variable.".to_string(),
            approved: true,
            user_note: String::new(),
            fix_done: false,
            reply_posted: false,
            last_agent_summary: None,
            last_error: None,
        pr_reply_text: None,
        reply_comment_id: None,
        },
        PrReviewItem {
            comment_id: Some(102),
            summary: "Rename suggestion (skipped)".to_string(),
            decision: PrReviewDecision::Skip,
            reasoning: "Out of scope for this PR.".to_string(),
            proposed_change: String::new(),
            approved: false,
            user_note: String::new(),
            fix_done: false,
            reply_posted: false,
            last_agent_summary: None,
            last_error: None,
        pr_reply_text: None,
        reply_comment_id: None,
        },
    ];

    let plan = PrReviewPlan {
        generated_at: Utc::now(),
        pr_url: task.pr_url.clone().unwrap(),
        review_decision: None,
        comments,
        items,
        raw_plan: String::new(),
        last_apply: None,
    };

    (task, plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_test_task() {
        let task = create_test_task("Test Task");
        assert_eq!(task.title, "Test Task");
        assert!(matches!(task.status, TaskStatus::Backlog));
    }

    #[test]
    fn test_create_test_task_with_status() {
        let task = create_test_task_with_status("In Progress Task", TaskStatus::InProgress);
        assert!(matches!(task.status, TaskStatus::InProgress));
    }
}
