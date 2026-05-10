use crate::domain::Task;

/// Build a structured prompt from task metadata for the Claude Code agent.
pub fn build_task_prompt(task: &Task, project_path: Option<&str>) -> String {
    let mut parts = Vec::new();

    // Task header
    parts.push(format!("# Task: {}", task.title));

    // Description
    if let Some(desc) = &task.description {
        if !desc.is_empty() {
            parts.push(format!("\n## Description\n{}", desc));
        }
    }

    // Metadata context
    let mut meta = Vec::new();
    meta.push(format!("- Category: {:?}", task.category));
    meta.push(format!("- Priority: {:?}", task.priority));
    meta.push(format!("- Complexity: {:?}", task.complexity));
    if task.model != "default" && !task.model.is_empty() {
        meta.push(format!("- Model: {}", task.model));
    }
    parts.push(format!("\n## Context\n{}", meta.join("\n")));

    // Working directory
    if let Some(path) = project_path {
        parts.push(format!("\n## Working Directory\n{}", path));
    }

    // Subtasks as checklist
    if !task.subtasks.is_empty() {
        let subtask_list: Vec<String> = task.subtasks.iter().map(|s| {
            let check = if s.completed { "x" } else { " " };
            format!("- [{}] {}", check, s.title)
        }).collect();
        parts.push(format!("\n## Subtasks\n{}", subtask_list.join("\n")));
    }

    // External references (prefer structured external_refs, fall back to legacy fields)
    if !task.external_refs.is_empty() {
        let refs: Vec<String> = task.external_refs.iter()
            .map(|r| {
                if let Some(url) = r.url() {
                    format!("- {}: {}", r.label(), url)
                } else {
                    format!("- {}", r.label())
                }
            })
            .collect();
        parts.push(format!("\n## References\n{}", refs.join("\n")));
    } else {
        // Fallback to legacy fields if external_refs is empty
        let mut refs = Vec::new();
        if let Some(url) = &task.github_issue_url {
            refs.push(format!("- GitHub Issue: {}", url));
        }
        if let Some(url) = &task.pr_url {
            refs.push(format!("- PR: {}", url));
        }
        if let Some(id) = &task.linear_ticket_id {
            refs.push(format!("- Linear: {}", id));
        }
        if !refs.is_empty() {
            parts.push(format!("\n## References\n{}", refs.join("\n")));
        }
    }

    // Instructions
    parts.push("\n## Instructions\nImplement this task. Follow existing code patterns and conventions. Write tests if applicable. Keep changes minimal and focused.".to_string());

    parts.join("\n")
}

/// Build a prompt for the AI code review pass.
pub fn build_review_prompt(task: &Task, diff: &str) -> String {
    let desc = task.description.as_deref().unwrap_or("(no description)");

    format!(
        r#"Review this code change for the task: {}

## Task Description
{}

## Code Changes (diff)
```diff
{}
```

## Instructions
Review for: correctness, security issues, code quality, and test coverage.

End your review with exactly one of:
- VERDICT: APPROVED
- VERDICT: CHANGES_REQUESTED

If changes requested, list each issue as:
- ISSUE: [severity] file:line - description

Where severity is one of: critical, high, medium, low"#,
        task.title, desc, diff
    )
}

/// Build a prompt for the validation + fix pass after review findings.
pub fn build_fix_prompt(task: &Task, review_findings: &str) -> String {
    format!(
        r#"The following issues were found during code review for task: {}

## Review Findings
{}

## Instructions
For EACH issue listed above:
1. Read the relevant code to verify the issue actually exists
2. If the issue is REAL: fix it
3. If the issue is a FALSE POSITIVE: skip it

Do NOT blindly fix everything. Only fix issues you have verified are real problems in the code.

After processing all issues, output a summary:
- FIXED: [issue description]
- FALSE_POSITIVE: [issue description] - [why it's not a real issue]
- SKIPPED: [issue description] - [reason]"#,
        task.title, review_findings
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task::{ExternalRef, Subtask};
    use crate::test_helpers::create_test_task;
    use uuid::Uuid;

    // ──────────────────────────────────────────────
    // build_task_prompt tests
    // ──────────────────────────────────────────────

    #[test]
    fn build_task_prompt_basic_contains_title_category_priority() {
        let task = create_test_task("Implement login page");
        let prompt = build_task_prompt(&task, None);

        assert!(prompt.contains("# Task: Implement login page"));
        assert!(prompt.contains("Category: Feature"));
        assert!(prompt.contains("Priority: Medium"));
    }

    #[test]
    fn build_task_prompt_with_description() {
        let mut task = create_test_task("Add caching");
        task.description = Some("Add Redis-based caching layer for API responses".to_string());
        let prompt = build_task_prompt(&task, None);

        assert!(prompt.contains("## Description"));
        assert!(prompt.contains("Add Redis-based caching layer for API responses"));
    }

    #[test]
    fn build_task_prompt_empty_description_no_section() {
        let mut task = create_test_task("Refactor module");
        task.description = Some(String::new());
        let prompt = build_task_prompt(&task, None);

        assert!(!prompt.contains("## Description"));
    }

    #[test]
    fn build_task_prompt_with_subtasks() {
        let mut task = create_test_task("Build dashboard");
        task.subtasks = vec![
            Subtask {
                id: Uuid::new_v4(),
                title: "Create layout".to_string(),
                completed: true,
            },
            Subtask {
                id: Uuid::new_v4(),
                title: "Add charts".to_string(),
                completed: false,
            },
        ];
        let prompt = build_task_prompt(&task, None);

        assert!(prompt.contains("## Subtasks"));
        assert!(prompt.contains("- [x] Create layout"));
        assert!(prompt.contains("- [ ] Add charts"));
    }

    #[test]
    fn build_task_prompt_with_external_refs_github_issue_and_jira() {
        let mut task = create_test_task("Fix bug");
        task.external_refs = vec![
            ExternalRef::GithubIssue {
                url: "https://github.com/org/repo/issues/42".to_string(),
                number: 42,
                repo: "org/repo".to_string(),
                state: Some("OPEN".to_string()),
            },
            ExternalRef::JiraTicket {
                key: "PLAT-123".to_string(),
                project: "PLAT".to_string(),
            },
        ];
        let prompt = build_task_prompt(&task, None);

        assert!(prompt.contains("## References"));
        assert!(prompt.contains("#42"));
        assert!(prompt.contains("https://github.com/org/repo/issues/42"));
        assert!(prompt.contains("PLAT-123"));
    }

    #[test]
    fn build_task_prompt_legacy_fields_fallback() {
        let mut task = create_test_task("Legacy task");
        // No external_refs, but legacy fields set
        task.github_issue_url = Some("https://github.com/org/repo/issues/99".to_string());
        task.pr_url = Some("https://github.com/org/repo/pull/100".to_string());
        task.linear_ticket_id = Some("LIN-456".to_string());
        let prompt = build_task_prompt(&task, None);

        assert!(prompt.contains("## References"));
        assert!(prompt.contains("GitHub Issue: https://github.com/org/repo/issues/99"));
        assert!(prompt.contains("PR: https://github.com/org/repo/pull/100"));
        assert!(prompt.contains("Linear: LIN-456"));
    }

    #[test]
    fn build_task_prompt_no_refs_no_legacy_no_references_section() {
        let task = create_test_task("Simple task");
        let prompt = build_task_prompt(&task, None);

        assert!(!prompt.contains("## References"));
    }

    #[test]
    fn build_task_prompt_with_project_path() {
        let task = create_test_task("Path task");
        let prompt = build_task_prompt(&task, Some("/home/user/project"));

        assert!(prompt.contains("## Working Directory"));
        assert!(prompt.contains("/home/user/project"));
    }

    #[test]
    fn build_task_prompt_no_project_path() {
        let task = create_test_task("No path task");
        let prompt = build_task_prompt(&task, None);

        assert!(!prompt.contains("## Working Directory"));
    }

    #[test]
    fn build_task_prompt_non_default_model_in_context() {
        let mut task = create_test_task("Model task");
        task.model = "claude-opus-4-0-20250514".to_string();
        let prompt = build_task_prompt(&task, None);

        assert!(prompt.contains("Model: claude-opus-4-0-20250514"));
    }

    #[test]
    fn build_task_prompt_default_model_not_in_context() {
        let mut task = create_test_task("Default model task");
        task.model = "default".to_string();
        let prompt = build_task_prompt(&task, None);

        assert!(!prompt.contains("Model:"));
    }

    #[test]
    fn build_task_prompt_all_fields_populated() {
        let mut task = create_test_task("Full task");
        task.description = Some("A comprehensive task with everything".to_string());
        task.model = "sonnet".to_string();
        task.subtasks = vec![
            Subtask {
                id: Uuid::new_v4(),
                title: "Step 1".to_string(),
                completed: true,
            },
            Subtask {
                id: Uuid::new_v4(),
                title: "Step 2".to_string(),
                completed: false,
            },
        ];
        task.external_refs = vec![
            ExternalRef::GithubIssue {
                url: "https://github.com/org/repo/issues/1".to_string(),
                number: 1,
                repo: "org/repo".to_string(),
                state: None,
            },
            ExternalRef::JiraTicket {
                key: "PROJ-10".to_string(),
                project: "PROJ".to_string(),
            },
        ];

        let prompt = build_task_prompt(&task, Some("/tmp/project"));

        assert!(prompt.contains("# Task: Full task"));
        assert!(prompt.contains("## Description"));
        assert!(prompt.contains("## Context"));
        assert!(prompt.contains("Model: sonnet"));
        assert!(prompt.contains("## Working Directory"));
        assert!(prompt.contains("/tmp/project"));
        assert!(prompt.contains("## Subtasks"));
        assert!(prompt.contains("- [x] Step 1"));
        assert!(prompt.contains("- [ ] Step 2"));
        assert!(prompt.contains("## References"));
        assert!(prompt.contains("#1"));
        assert!(prompt.contains("PROJ-10"));
        assert!(prompt.contains("## Instructions"));
    }

    #[test]
    fn build_task_prompt_very_long_description() {
        let mut task = create_test_task("Long desc task");
        let long_desc = "x".repeat(1500);
        task.description = Some(long_desc.clone());
        let prompt = build_task_prompt(&task, None);

        assert!(prompt.contains("## Description"));
        assert!(prompt.contains(&long_desc));
    }

    #[test]
    fn build_task_prompt_description_with_markdown_special_chars() {
        let mut task = create_test_task("Markdown task");
        task.description = Some("Use `Vec<String>` and **bold** text. See [link](http://example.com). # Not a heading\n\n```rust\nfn main() {}\n```".to_string());
        let prompt = build_task_prompt(&task, None);

        assert!(prompt.contains("`Vec<String>`"));
        assert!(prompt.contains("**bold**"));
        assert!(prompt.contains("```rust"));
    }

    #[test]
    fn build_task_prompt_all_five_external_ref_types() {
        let mut task = create_test_task("All refs task");
        task.external_refs = vec![
            ExternalRef::GithubIssue {
                url: "https://github.com/org/repo/issues/10".to_string(),
                number: 10,
                repo: "org/repo".to_string(),
                state: Some("OPEN".to_string()),
            },
            ExternalRef::GithubPr {
                url: "https://github.com/org/repo/pull/20".to_string(),
                number: 20,
                repo: "org/repo".to_string(),
                state: Some("OPEN".to_string()),
            },
            ExternalRef::GitlabIssue {
                url: "https://gitlab.com/org/repo/-/issues/30".to_string(),
            },
            ExternalRef::JiraTicket {
                key: "PLAT-40".to_string(),
                project: "PLAT".to_string(),
            },
            ExternalRef::LinearTicket {
                id: "LIN-50".to_string(),
            },
        ];
        let prompt = build_task_prompt(&task, None);

        assert!(prompt.contains("## References"));
        assert!(prompt.contains("#10"));
        assert!(prompt.contains("PR #20"));
        assert!(prompt.contains("#30"));
        assert!(prompt.contains("PLAT-40"));
        assert!(prompt.contains("LIN-50"));
    }

    #[test]
    fn build_task_prompt_empty_model_not_in_context() {
        let mut task = create_test_task("Empty model task");
        task.model = String::new();
        let prompt = build_task_prompt(&task, None);

        assert!(!prompt.contains("Model:"));
    }

    // ──────────────────────────────────────────────
    // build_review_prompt tests
    // ──────────────────────────────────────────────

    #[test]
    fn build_review_prompt_basic() {
        let mut task = create_test_task("Fix auth bug");
        task.description = Some("Authentication fails on expired tokens".to_string());
        let diff = "+fn validate_token(t: &str) -> bool {\n+    !t.is_empty()\n+}";
        let prompt = build_review_prompt(&task, diff);

        assert!(prompt.contains("Fix auth bug"));
        assert!(prompt.contains("Authentication fails on expired tokens"));
        assert!(prompt.contains("+fn validate_token"));
        assert!(prompt.contains("VERDICT: APPROVED"));
        assert!(prompt.contains("VERDICT: CHANGES_REQUESTED"));
    }

    #[test]
    fn build_review_prompt_no_description_shows_placeholder() {
        let task = create_test_task("No desc task");
        let prompt = build_review_prompt(&task, "some diff");

        assert!(prompt.contains("(no description)"));
    }

    #[test]
    fn build_review_prompt_empty_diff() {
        let task = create_test_task("Empty diff task");
        let prompt = build_review_prompt(&task, "");

        assert!(prompt.contains("```diff\n\n```"));
        assert!(prompt.contains("Empty diff task"));
    }

    // ──────────────────────────────────────────────
    // build_fix_prompt tests
    // ──────────────────────────────────────────────

    #[test]
    fn build_fix_prompt_contains_title_and_findings() {
        let task = create_test_task("Refactor parser");
        let findings = "ISSUE: [high] src/parser.rs:42 - Potential panic on unwrap";
        let prompt = build_fix_prompt(&task, findings);

        assert!(prompt.contains("Refactor parser"));
        assert!(prompt.contains("ISSUE: [high] src/parser.rs:42 - Potential panic on unwrap"));
        assert!(prompt.contains("FIXED:"));
        assert!(prompt.contains("FALSE_POSITIVE:"));
    }
}
