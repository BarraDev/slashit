//! Domain model tests
//! Run with: cargo test -p slashit-app

use super::*;

#[cfg(test)]
mod task_tests {
    use super::*;

    #[test]
    fn test_task_category_default() {
        let category = TaskCategory::default();
        assert!(matches!(category, TaskCategory::Feature));
    }

    #[test]
    fn test_task_priority_default() {
        let priority = TaskPriority::default();
        assert!(matches!(priority, TaskPriority::Medium));
    }

    #[test]
    fn test_task_complexity_default() {
        let complexity = TaskComplexity::default();
        assert!(matches!(complexity, TaskComplexity::Moderate));
    }

    #[test]
    fn test_task_category_variants() {
        let feature = TaskCategory::Feature;
        let bugfix = TaskCategory::BugFix;
        let refactoring = TaskCategory::Refactoring;
        
        assert!(matches!(feature, TaskCategory::Feature));
        assert!(matches!(bugfix, TaskCategory::BugFix));
        assert!(matches!(refactoring, TaskCategory::Refactoring));
    }

    #[test]
    fn test_task_status_variants() {
        assert!(matches!(TaskStatus::Backlog, TaskStatus::Backlog));
        assert!(matches!(TaskStatus::Queue, TaskStatus::Queue));
        assert!(matches!(TaskStatus::InProgress, TaskStatus::InProgress));
        assert!(matches!(TaskStatus::Done, TaskStatus::Done));
    }

    #[test]
    fn test_task_priority_variants() {
        assert!(matches!(TaskPriority::Urgent, TaskPriority::Urgent));
        assert!(matches!(TaskPriority::High, TaskPriority::High));
        assert!(matches!(TaskPriority::Medium, TaskPriority::Medium));
        assert!(matches!(TaskPriority::Low, TaskPriority::Low));
    }
}

#[cfg(test)]
mod project_tests {
    use super::*;
    use uuid::Uuid;
    use std::collections::HashMap;

    #[test]
    fn test_project_creation() {
        let project = Project {
            id: Uuid::new_v4(),
            name: "Test Project".to_string(),
            repository_id: None,
            agent_type: AgentType::ClaudeCode,
            agent_config: AgentConfig {
                agent_type: AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: vec![],
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        
        assert_eq!(project.name, "Test Project");
        assert!(matches!(project.agent_type, AgentType::ClaudeCode));
    }

    #[test]
    fn test_agent_type_variants() {
        assert!(matches!(AgentType::ClaudeCode, AgentType::ClaudeCode));
        assert!(matches!(AgentType::Cursor, AgentType::Cursor));
        assert!(matches!(AgentType::Cody, AgentType::Cody));
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_workspace_creation() {
        let workspace = Workspace {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            name: "main".to_string(),
            path: "/path/to/workspace".to_string(),
            base_revision: None,
            current_change_id: None,
            created_at: chrono::Utc::now(),
        };
        
        assert_eq!(workspace.name, "main");
    }

    #[test]
    fn test_workspace_status_creation() {
        let status = WorkspaceStatus {
            workspace_id: Uuid::new_v4(),
            current_change_id: Some("abc123".to_string()),
            pending_changes: true,
            conflicted: false,
        };
        
        assert!(status.pending_changes);
        assert!(!status.conflicted);
    }
}

#[cfg(test)]
mod external_ref_tests {
    use super::*;
    use serde_json;

    // --- label() tests ---

    #[test]
    fn test_github_issue_label() {
        let r = ExternalRef::GithubIssue {
            url: "https://github.com/org/repo/issues/123".to_string(),
            number: 123,
            repo: "org/repo".to_string(),
            state: Some("open".to_string()),
        };
        assert_eq!(r.label(), "#123");
    }

    #[test]
    fn test_github_pr_label() {
        let r = ExternalRef::GithubPr {
            url: "https://github.com/org/repo/pull/456".to_string(),
            number: 456,
            repo: "org/repo".to_string(),
            state: Some("open".to_string()),
        };
        assert_eq!(r.label(), "PR #456");
    }

    #[test]
    fn test_gitlab_issue_label_extracts_number() {
        let r = ExternalRef::GitlabIssue {
            url: "https://gitlab.com/org/repo/-/issues/789".to_string(),
        };
        assert_eq!(r.label(), "#789");
    }

    #[test]
    fn test_jira_ticket_label() {
        let r = ExternalRef::JiraTicket {
            key: "PLAT-42".to_string(),
            project: "PLAT".to_string(),
        };
        assert_eq!(r.label(), "PLAT-42");
    }

    #[test]
    fn test_linear_ticket_label() {
        let r = ExternalRef::LinearTicket {
            id: "ENG-100".to_string(),
        };
        assert_eq!(r.label(), "ENG-100");
    }

    // --- url() tests ---

    #[test]
    fn test_github_issue_url() {
        let r = ExternalRef::GithubIssue {
            url: "https://github.com/org/repo/issues/1".to_string(),
            number: 1,
            repo: "org/repo".to_string(),
            state: None,
        };
        assert_eq!(r.url(), Some("https://github.com/org/repo/issues/1"));
    }

    #[test]
    fn test_github_pr_url() {
        let r = ExternalRef::GithubPr {
            url: "https://github.com/org/repo/pull/2".to_string(),
            number: 2,
            repo: "org/repo".to_string(),
            state: None,
        };
        assert_eq!(r.url(), Some("https://github.com/org/repo/pull/2"));
    }

    #[test]
    fn test_gitlab_issue_url() {
        let r = ExternalRef::GitlabIssue {
            url: "https://gitlab.com/org/repo/-/issues/3".to_string(),
        };
        assert_eq!(r.url(), Some("https://gitlab.com/org/repo/-/issues/3"));
    }

    #[test]
    fn test_jira_ticket_url_is_none() {
        let r = ExternalRef::JiraTicket {
            key: "PROJ-1".to_string(),
            project: "PROJ".to_string(),
        };
        assert_eq!(r.url(), None);
    }

    #[test]
    fn test_linear_ticket_url_is_none() {
        let r = ExternalRef::LinearTicket {
            id: "LIN-1".to_string(),
        };
        assert_eq!(r.url(), None);
    }

    // --- is_pr() / is_issue() tests ---

    #[test]
    fn test_github_pr_is_pr() {
        let r = ExternalRef::GithubPr {
            url: "https://github.com/org/repo/pull/10".to_string(),
            number: 10,
            repo: "org/repo".to_string(),
            state: None,
        };
        assert!(r.is_pr());
        assert!(!r.is_issue());
    }

    #[test]
    fn test_github_issue_is_issue() {
        let r = ExternalRef::GithubIssue {
            url: "https://github.com/org/repo/issues/10".to_string(),
            number: 10,
            repo: "org/repo".to_string(),
            state: None,
        };
        assert!(r.is_issue());
        assert!(!r.is_pr());
    }

    #[test]
    fn test_gitlab_issue_is_issue() {
        let r = ExternalRef::GitlabIssue {
            url: "https://gitlab.com/org/repo/-/issues/5".to_string(),
        };
        assert!(r.is_issue());
        assert!(!r.is_pr());
    }

    #[test]
    fn test_jira_ticket_is_issue() {
        let r = ExternalRef::JiraTicket {
            key: "X-1".to_string(),
            project: "X".to_string(),
        };
        assert!(r.is_issue());
        assert!(!r.is_pr());
    }

    #[test]
    fn test_linear_ticket_is_issue() {
        let r = ExternalRef::LinearTicket {
            id: "L-1".to_string(),
        };
        assert!(r.is_issue());
        assert!(!r.is_pr());
    }

    // --- Serde round-trip tests ---

    #[test]
    fn test_serde_github_issue_roundtrip() {
        let original = ExternalRef::GithubIssue {
            url: "https://github.com/org/repo/issues/42".to_string(),
            number: 42,
            repo: "org/repo".to_string(),
            state: Some("open".to_string()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ExternalRef = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_serde_github_pr_roundtrip() {
        let original = ExternalRef::GithubPr {
            url: "https://github.com/org/repo/pull/99".to_string(),
            number: 99,
            repo: "org/repo".to_string(),
            state: Some("merged".to_string()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ExternalRef = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_serde_gitlab_issue_roundtrip() {
        let original = ExternalRef::GitlabIssue {
            url: "https://gitlab.com/group/project/-/issues/7".to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ExternalRef = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_serde_jira_ticket_roundtrip() {
        let original = ExternalRef::JiraTicket {
            key: "PLAT-100".to_string(),
            project: "PLAT".to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ExternalRef = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_serde_linear_ticket_roundtrip() {
        let original = ExternalRef::LinearTicket {
            id: "ENG-55".to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ExternalRef = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_serde_tagged_format() {
        let r = ExternalRef::GithubIssue {
            url: "https://github.com/a/b/issues/1".to_string(),
            number: 1,
            repo: "a/b".to_string(),
            state: None,
        };
        let json: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(json["ref_type"], "github_issue");
        assert_eq!(json["number"], 1);
    }

    #[test]
    fn test_serde_pr_tagged_format() {
        let r = ExternalRef::GithubPr {
            url: "https://github.com/a/b/pull/5".to_string(),
            number: 5,
            repo: "a/b".to_string(),
            state: Some("closed".to_string()),
        };
        let json: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(json["ref_type"], "github_pr");
    }

    // --- Edge case tests ---

    #[test]
    fn test_gitlab_issue_url_no_trailing_number() {
        let r = ExternalRef::GitlabIssue {
            url: "https://gitlab.com/org/repo/-/issues".to_string(),
        };
        // rsplit('/').next() gives "issues", so label is "#issues"
        assert_eq!(r.label(), "#issues");
    }

    #[test]
    fn test_gitlab_issue_empty_url() {
        let r = ExternalRef::GitlabIssue {
            url: "".to_string(),
        };
        // rsplit('/').next() on empty string gives Some(""), so "#"
        assert_eq!(r.label(), "#");
    }

    #[test]
    fn test_jira_ticket_empty_key() {
        let r = ExternalRef::JiraTicket {
            key: "".to_string(),
            project: "PROJ".to_string(),
        };
        assert_eq!(r.label(), "");
    }

    #[test]
    fn test_linear_ticket_empty_id() {
        let r = ExternalRef::LinearTicket {
            id: "".to_string(),
        };
        assert_eq!(r.label(), "");
    }

    #[test]
    fn test_github_issue_number_zero() {
        let r = ExternalRef::GithubIssue {
            url: "https://github.com/org/repo/issues/0".to_string(),
            number: 0,
            repo: "org/repo".to_string(),
            state: None,
        };
        assert_eq!(r.label(), "#0");
    }

    #[test]
    fn test_github_pr_state_merged() {
        let r = ExternalRef::GithubPr {
            url: "https://github.com/org/repo/pull/77".to_string(),
            number: 77,
            repo: "org/repo".to_string(),
            state: Some("merged".to_string()),
        };
        assert_eq!(r.label(), "PR #77");
        assert!(r.is_pr());
        // Verify state serializes correctly
        let json: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(json["state"], "merged");
    }

    #[test]
    fn test_github_pr_state_none() {
        let r = ExternalRef::GithubPr {
            url: "https://github.com/org/repo/pull/88".to_string(),
            number: 88,
            repo: "org/repo".to_string(),
            state: None,
        };
        let json: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert!(json["state"].is_null());
    }

    #[test]
    fn test_serde_unknown_ref_type_fails() {
        let json = r#"{"ref_type": "bitbucket_issue", "url": "https://example.com"}"#;
        let result = serde_json::from_str::<ExternalRef>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_serde_missing_ref_type_fails() {
        let json = r#"{"url": "https://example.com", "number": 1}"#;
        let result = serde_json::from_str::<ExternalRef>(json);
        assert!(result.is_err());
    }
}
