//! Test helper utilities
//! Common test fixtures and utilities for backend testing

use crate::domain::{Task, TaskStatus, TaskCategory, TaskPriority, TaskComplexity, TaskImpact, SecuritySeverity, TaskPhase};
pub use uuid::Uuid;
use chrono::Utc;

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
        paths,
    }
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
