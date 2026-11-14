// SPDX-License-Identifier: MPL-2.0
use crate::commands::gui_admission::GuiMutation;
use crate::domain::github::{Assignee, GithubIssue, Label, PullRequest, Review};
use crate::domain::github_issue_link::parse_issue_url;
use crate::domain::task::ExternalRef;
use crate::domain::{
    SecuritySeverity, Task, TaskCategory, TaskComplexity, TaskImpact, TaskPhase, TaskPriority,
    TaskStatus,
};
use std::ffi::OsStr;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use uuid::Uuid;

/// How long one read of a linked Issue may take. The read happens once, when
/// the Issue is linked, so a slow answer only delays that request.
const ISSUE_READ_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct GithubState;

impl GithubState {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GithubState {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if `gh` CLI is available and authenticated.
async fn check_gh() -> Result<(), String> {
    let output = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .await
        .map_err(|e| {
            format!(
                "gh CLI not found: {}. Install with: sudo pacman -S github-cli",
                e
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "gh not authenticated: {}. Run: gh auth login",
            stderr
        ));
    }
    Ok(())
}

/// Run a gh command and parse JSON output.
async fn run_gh(args: &[&str]) -> Result<serde_json::Value, String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .await
        .map_err(|e| format!("Failed to run gh: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh command failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).map_err(|e| format!("Failed to parse gh output: {}", e))
}

#[tauri::command]
pub async fn get_issues(repo: String) -> Result<Vec<GithubIssue>, String> {
    check_gh().await?;

    let json = run_gh(&[
        "issue",
        "list",
        "--repo",
        &repo,
        "--json",
        "number,title,body,state,labels,assignees,comments,createdAt,updatedAt",
        "--limit",
        "50",
    ])
    .await?;

    let issues: Vec<GithubIssue> = json
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|v| {
            Some(GithubIssue {
                number: v.get("number")?.as_i64()? as i32,
                title: v.get("title")?.as_str()?.to_string(),
                body: v
                    .get("body")
                    .and_then(|b| b.as_str())
                    .unwrap_or("")
                    .to_string(),
                state: v
                    .get("state")
                    .and_then(|s| s.as_str())
                    .unwrap_or("OPEN")
                    .to_string(),
                labels: v
                    .get("labels")
                    .and_then(|l| l.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|lbl| {
                                Some(Label {
                                    name: lbl.get("name")?.as_str()?.to_string(),
                                    color: lbl
                                        .get("color")
                                        .and_then(|c| c.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                assignees: v
                    .get("assignees")
                    .and_then(|a| a.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|asn| {
                                Some(Assignee {
                                    login: asn.get("login")?.as_str()?.to_string(),
                                    avatar_url: asn
                                        .get("avatarUrl")
                                        .and_then(|u| u.as_str())
                                        .map(|s| s.to_string()),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                comments: v.get("comments").and_then(|c| c.as_i64()).unwrap_or(0) as i32,
                created_at: v
                    .get("createdAt")
                    .and_then(|d| d.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(chrono::Utc::now),
                updated_at: v
                    .get("updatedAt")
                    .and_then(|d| d.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(chrono::Utc::now),
            })
        })
        .collect();

    Ok(issues)
}

#[tauri::command]
pub async fn get_issue(repo: String, number: i32) -> Result<GithubIssue, String> {
    check_gh().await?;

    let json = run_gh(&[
        "issue",
        "view",
        &number.to_string(),
        "--repo",
        &repo,
        "--json",
        "number,title,body,state,labels,assignees,comments,createdAt,updatedAt",
    ])
    .await?;

    Ok(GithubIssue {
        number: json.get("number").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
        title: json
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        body: json
            .get("body")
            .and_then(|b| b.as_str())
            .unwrap_or("")
            .to_string(),
        state: json
            .get("state")
            .and_then(|s| s.as_str())
            .unwrap_or("OPEN")
            .to_string(),
        labels: Vec::new(),
        assignees: Vec::new(),
        comments: json.get("comments").and_then(|c| c.as_i64()).unwrap_or(0) as i32,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    })
}

/// Read one Issue from GitHub through `program` (the `gh` on `PATH` in
/// production) and return its state, `OPEN` or `CLOSED`. `gh issue view`
/// changes nothing on GitHub. An answer for another number or address, such
/// as a pull request, is refused.
pub(crate) async fn read_issue_state(
    program: &OsStr,
    repo: &str,
    number: u32,
) -> Result<String, String> {
    read_issue_state_within(program, repo, number, ISSUE_READ_TIMEOUT).await
}

/// [`read_issue_state`] with the bound injected, so the timeout path can be
/// exercised without waiting for the production limit.
async fn read_issue_state_within(
    program: &OsStr,
    repo: &str,
    number: u32,
    limit: Duration,
) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args([
            "issue",
            "view",
            &number.to_string(),
            "--repo",
            repo,
            "--json",
            "number,state,url",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(limit, command.output())
        .await
        .map_err(|_| {
            format!(
                "GitHub did not answer within {} seconds",
                limit.as_secs()
            )
        })?
        .map_err(|e| {
            format!("Could not run gh ({e}). Install the GitHub CLI and run gh auth login.")
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "GitHub could not return issue #{number} in {repo}: {}",
            stderr.trim()
        ));
    }
    parse_issue_state(&output.stdout, repo, number)
}

fn parse_issue_state(stdout: &[u8], repo: &str, number: u32) -> Result<String, String> {
    let json: serde_json::Value = serde_json::from_slice(stdout)
        .map_err(|e| format!("GitHub's answer could not be read: {e}"))?;
    let answered_number = json.get("number").and_then(serde_json::Value::as_u64);
    let url = json
        .get("url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let Ok(home) = parse_issue_url(url) else {
        return Err(format!(
            "GitHub did not answer with issue #{number} in {repo}"
        ));
    };
    if answered_number != Some(u64::from(number)) {
        return Err(format!(
            "GitHub did not answer with issue #{number} in {repo}"
        ));
    }
    // An Issue transferred to another repository is answered at its new
    // address. Storing the typed address would name the wrong repository, so
    // the caller is told the address GitHub reports instead.
    if home.number != number || !home.repo.eq_ignore_ascii_case(repo) {
        return Err(format!(
            "GitHub reports issue #{number} in {repo} at {}. Link that address instead.",
            home.url()
        ));
    }
    match json.get("state").and_then(serde_json::Value::as_str) {
        Some(state @ ("OPEN" | "CLOSED")) => Ok(state.to_string()),
        _ => Err(format!(
            "GitHub did not report the state of issue #{number} in {repo}"
        )),
    }
}

#[tauri::command]
pub async fn create_task_from_issue(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    repo: String,
    issue_number: i32,
    project_id: String,
) -> Result<Task, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;

    // Fetch issue
    let issue = get_issue(repo.clone(), issue_number).await?;

    // Map labels to category/priority
    let category = map_labels_to_category(&issue.labels);
    let priority = map_labels_to_priority(&issue.labels);

    let now = chrono::Utc::now();
    let task = Task {
        review_mode: Default::default(),
        id: Uuid::new_v4(),
        project_id,
        title: issue.title,
        description: Some(issue.body),
        status: TaskStatus::Backlog,
        model: "default".to_string(),
        planning_mode: false,
        provider: Default::default(),
        dependencies: Vec::new(),
        worktree_id: None,
        checkout_admission: None,
        jj_change_id: None,
        category,
        priority,
        complexity: TaskComplexity::Moderate,
        impact: TaskImpact::Medium,
        security_severity: SecuritySeverity::None,
        phase: TaskPhase::Idle,
        phase_progress: 0,
        overall_progress: 0,
        subtasks: Vec::new(),
        sequence_number: 0,
        position: 0,
        github_issue_url: Some(format!(
            "https://github.com/{}/issues/{}",
            repo, issue_number
        )),
        gitlab_issue_url: None,
        linear_ticket_id: None,
        jira_issue_key: None,
        pr_url: None,
        external_refs: vec![ExternalRef::GithubIssue {
            url: format!("https://github.com/{}/issues/{}", repo, issue_number),
            number: issue_number as u32,
            repo: repo.clone(),
            state: Some(issue.state.clone()),
        }],
        qa_signoff: None,
        human_review: Default::default(),
        stuck_since: None,
        error_message: None,
        worktree_path: None,
        branch_name: None,
        base_commit: None,
        branch_origin: None,
        pending_republish: None,
        pr_create_unconfirmed: None,
        pr_publication: None,
        local_integration: None,
        cleanup_in_flight: false,
        pr_review_plan: None,
        activity: Vec::new(),
        worker_handoff: None,
        worker_handoff_confirmed: false,
        review_fix_residue: None,
        last_observed_model: None,
        reviewer_model: None,
        new_attempt: None,
        new_attempt_confirmed: false,
        approved_execution: None,
        executed_approvals: Vec::new(),
        created_at: now,
        updated_at: now,
    };

    // The board shows the task only once the file has accepted it.
    crate::lifecycle::create_batch(
        &state.task.project_gate,
        &state.task.tasks,
        &state.project.projects,
        &state.storage,
        project_id,
        std::slice::from_ref(&task),
    )
    .await?;

    Ok(task)
}

#[tauri::command]
pub async fn import_github_issues(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    repo: String,
    project_id: String,
    filter: Option<String>,
) -> Result<Vec<Task>, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    check_gh().await?;

    let mut args = vec![
        "issue",
        "list",
        "--repo",
        &repo,
        "--json",
        "number,title,body,state,labels,assignees,comments,createdAt,updatedAt",
        "--limit",
        "100",
    ];

    let filter_str;
    if let Some(ref f) = filter {
        if !f.is_empty() {
            filter_str = f.clone();
            args.push("--search");
            args.push(&filter_str);
        }
    }

    let json = run_gh(&args).await?;
    let issues: Vec<GithubIssue> =
        serde_json::from_value(json).map_err(|e| format!("Parse error: {}", e))?;

    let now = chrono::Utc::now();
    let mut imported = Vec::new();

    for issue in issues {
        let category = map_labels_to_category(&issue.labels);
        let priority = map_labels_to_priority(&issue.labels);

        let task = Task {
            review_mode: Default::default(),
            id: Uuid::new_v4(),
            project_id,
            title: issue.title,
            description: Some(issue.body),
            status: TaskStatus::Backlog,
            model: "default".to_string(),
            planning_mode: false,
            provider: Default::default(),
            dependencies: Vec::new(),
            worktree_id: None,
            jj_change_id: None,
            category,
            priority,
            complexity: TaskComplexity::Moderate,
            impact: TaskImpact::Medium,
            security_severity: SecuritySeverity::None,
            phase: TaskPhase::Idle,
            phase_progress: 0,
            overall_progress: 0,
            subtasks: Vec::new(),
            sequence_number: 0,
            position: imported.len() as i32,
            github_issue_url: Some(format!(
                "https://github.com/{}/issues/{}",
                repo, issue.number
            )),
            gitlab_issue_url: None,
            linear_ticket_id: None,
            jira_issue_key: None,
            pr_url: None,
            external_refs: vec![ExternalRef::GithubIssue {
                url: format!("https://github.com/{}/issues/{}", repo, issue.number),
                number: issue.number as u32,
                repo: repo.clone(),
                state: Some(issue.state.clone()),
            }],
            qa_signoff: None,
            human_review: Default::default(),
            stuck_since: None,
            error_message: None,
            worktree_path: None,
            branch_name: None,
            checkout_admission: None,
            base_commit: None,
            branch_origin: None,
            pending_republish: None,
            pr_create_unconfirmed: None,
            pr_publication: None,
            local_integration: None,
            cleanup_in_flight: false,
            pr_review_plan: None,
            activity: Vec::new(),
            worker_handoff: None,
            worker_handoff_confirmed: false,
            review_fix_residue: None,
            last_observed_model: None,
            reviewer_model: None,
            new_attempt: None,
            new_attempt_confirmed: false,
            approved_execution: None,
            executed_approvals: Vec::new(),
            created_at: now,
            updated_at: now,
        };

        imported.push(task);
    }

    // The board shows the import only once the file has accepted it.
    crate::lifecycle::create_batch(
        &state.task.project_gate,
        &state.task.tasks,
        &state.project.projects,
        &state.storage,
        project_id,
        &imported,
    )
    .await?;

    Ok(imported)
}

#[tauri::command]
pub async fn get_prs(repo: String) -> Result<Vec<PullRequest>, String> {
    check_gh().await?;

    let json = run_gh(&[
        "pr",
        "list",
        "--repo",
        &repo,
        "--json",
        "number,title,body,state,author,createdAt,updatedAt,reviews,additions,deletions",
        "--limit",
        "50",
    ])
    .await?;

    let prs: Vec<PullRequest> = json
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|v| {
            Some(PullRequest {
                number: v.get("number")?.as_i64()? as i32,
                title: v.get("title")?.as_str()?.to_string(),
                body: v
                    .get("body")
                    .and_then(|b| b.as_str())
                    .unwrap_or("")
                    .to_string(),
                state: v
                    .get("state")
                    .and_then(|s| s.as_str())
                    .unwrap_or("OPEN")
                    .to_string(),
                author: v
                    .get("author")
                    .and_then(|a| a.get("login"))
                    .and_then(|l| l.as_str())
                    .unwrap_or("")
                    .to_string(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                reviews: v
                    .get("reviews")
                    .and_then(|r| r.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|rev| {
                                Some(Review {
                                    user: rev
                                        .get("author")
                                        .and_then(|a| a.get("login"))
                                        .and_then(|l| l.as_str())?
                                        .to_string(),
                                    state: rev
                                        .get("state")
                                        .and_then(|s| s.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    submitted_at: None,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                additions: v.get("additions").and_then(|a| a.as_i64()).unwrap_or(0) as i32,
                deletions: v.get("deletions").and_then(|d| d.as_i64()).unwrap_or(0) as i32,
            })
        })
        .collect();

    Ok(prs)
}

#[tauri::command]
pub async fn get_pr(repo: String, number: i32) -> Result<PullRequest, String> {
    check_gh().await?;

    let json = run_gh(&[
        "pr",
        "view",
        &number.to_string(),
        "--repo",
        &repo,
        "--json",
        "number,title,body,state,author,createdAt,updatedAt,reviews,additions,deletions",
    ])
    .await?;

    Ok(PullRequest {
        number: json.get("number").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
        title: json
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        body: json
            .get("body")
            .and_then(|b| b.as_str())
            .unwrap_or("")
            .to_string(),
        state: json
            .get("state")
            .and_then(|s| s.as_str())
            .unwrap_or("OPEN")
            .to_string(),
        author: json
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(|l| l.as_str())
            .unwrap_or("")
            .to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        reviews: Vec::new(),
        additions: json.get("additions").and_then(|a| a.as_i64()).unwrap_or(0) as i32,
        deletions: json.get("deletions").and_then(|d| d.as_i64()).unwrap_or(0) as i32,
    })
}

// --- Label mapping helpers ---

fn map_labels_to_category(labels: &[Label]) -> TaskCategory {
    for label in labels {
        let name = label.name.to_lowercase();
        if name.contains("bug") || name.contains("fix") {
            return TaskCategory::BugFix;
        }
        if name.contains("feature") || name.contains("enhancement") {
            return TaskCategory::Feature;
        }
        if name.contains("doc") {
            return TaskCategory::Documentation;
        }
        if name.contains("security") {
            return TaskCategory::Security;
        }
        if name.contains("perf") {
            return TaskCategory::Performance;
        }
        if name.contains("ui") || name.contains("ux") {
            return TaskCategory::UiUx;
        }
        if name.contains("test") {
            return TaskCategory::Testing;
        }
        if name.contains("infra") || name.contains("ci") {
            return TaskCategory::Infrastructure;
        }
        if name.contains("refactor") {
            return TaskCategory::Refactoring;
        }
    }
    TaskCategory::Feature
}

fn map_labels_to_priority(labels: &[Label]) -> TaskPriority {
    for label in labels {
        let name = label.name.to_lowercase();
        if name.contains("critical") || name.contains("urgent") || name.contains("p0") {
            return TaskPriority::Urgent;
        }
        if name.contains("high") || name.contains("p1") {
            return TaskPriority::High;
        }
        if name.contains("low") || name.contains("p3") {
            return TaskPriority::Low;
        }
    }
    TaskPriority::Medium
}

#[cfg(test)]
mod issue_read_tests {
    use super::*;

    #[test]
    fn an_answer_for_this_issue_gives_its_state() {
        let open = br#"{"number":7,"state":"OPEN","url":"https://github.com/o/r/issues/7"}"#;
        assert_eq!(parse_issue_state(open, "o/r", 7).unwrap(), "OPEN");
        let closed = br#"{"number":7,"state":"CLOSED","url":"https://github.com/o/r/issues/7"}"#;
        assert_eq!(parse_issue_state(closed, "o/r", 7).unwrap(), "CLOSED");
    }

    #[test]
    fn an_answer_for_another_number_or_a_pull_request_is_refused() {
        let other = br#"{"number":8,"state":"OPEN","url":"https://github.com/o/r/issues/8"}"#;
        assert!(parse_issue_state(other, "o/r", 7).is_err());
        let pull = br#"{"number":7,"state":"OPEN","url":"https://github.com/o/r/pull/7"}"#;
        assert!(parse_issue_state(pull, "o/r", 7).is_err());
    }

    /// An Issue transferred to another repository must not be stored under the
    /// address that was typed: the answer names the repository that holds it.
    #[test]
    fn an_answer_naming_the_issue_in_another_repository_is_refused() {
        let moved = br#"{"number":7,"state":"OPEN","url":"https://github.com/new/repo/issues/7"}"#;
        let refusal = parse_issue_state(moved, "old/repo", 7).unwrap_err();
        assert!(
            refusal.contains("https://github.com/new/repo/issues/7"),
            "{refusal}"
        );
    }

    #[test]
    fn an_answer_that_differs_only_in_the_case_of_the_repository_is_accepted() {
        let answer = br#"{"number":7,"state":"OPEN","url":"https://github.com/Old/Repo/issues/7"}"#;
        assert_eq!(parse_issue_state(answer, "old/repo", 7).unwrap(), "OPEN");
    }

    #[test]
    fn an_answer_without_a_known_state_or_readable_json_is_refused() {
        let merged = br#"{"number":7,"state":"MERGED","url":"https://github.com/o/r/issues/7"}"#;
        let refusal = parse_issue_state(merged, "o/r", 7).unwrap_err();
        assert!(refusal.contains("did not report the state"), "{refusal}");
        let unreadable = parse_issue_state(b"not json", "o/r", 7).unwrap_err();
        assert!(unreadable.contains("could not be read"), "{unreadable}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_gh_is_reported_with_its_refusal() {
        // `false` exits 1 whatever it is given, so no network is involved.
        let error = read_issue_state(OsStr::new("false"), "o/r", 7)
            .await
            .unwrap_err();
        assert!(error.contains("could not return issue #7 in o/r"), "{error}");
    }

    /// A `gh` that never answers is abandoned at the bound, and its process is
    /// killed rather than left running. `exec` makes the script itself the
    /// sleeping process, so killing it is enough.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_gh_that_never_answers_is_abandoned_at_the_bound() {
        let dir = tempfile::TempDir::new().unwrap();
        let gh = dir.path().join("gh");
        crate::test_helpers::write_executable(&gh, "#!/bin/sh\nexec sleep 30\n");

        let started = std::time::Instant::now();
        let error = read_issue_state_within(gh.as_os_str(), "o/r", 7, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(error.contains("did not answer within"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_missing_gh_is_reported_as_missing() {
        let error = read_issue_state(OsStr::new("/nonexistent/slashit-gh-for-test"), "o/r", 7)
            .await
            .unwrap_err();
        assert!(error.contains("Could not run gh"), "{error}");
    }
}
