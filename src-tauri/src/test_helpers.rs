//! Test helper utilities
//! Common test fixtures and utilities for backend testing

use crate::domain::{Task, TaskStatus, TaskCategory, TaskPriority, TaskComplexity, TaskImpact, SecuritySeverity, TaskPhase};
pub use uuid::Uuid;
use chrono::Utc;

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
        workspace_id: None,
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
