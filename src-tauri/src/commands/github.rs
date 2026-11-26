// SPDX-License-Identifier: MPL-2.0
use crate::commands::gui_admission::GuiMutation;
use crate::domain::github::{Assignee, GithubIssue, Label, PullRequest, Review};
use crate::domain::github_issue_link::{parse_issue_url, project_issue_refusal, IssueAddress};
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

/// The fields every Issue read asks `gh` for. `url` is the address GitHub
/// reports, which a transferred Issue answers at its new home.
const ISSUE_FIELDS: &str = "number,title,body,state,labels,assignees,comments,createdAt,updatedAt,url";

/// The `gh issue list --state` values. The state is part of the query, so a
/// closed Issue is only listed when the caller asked for closed Issues.
const ISSUE_LIST_STATES: [&str; 3] = ["open", "closed", "all"];

/// The most Issues one `gh issue list` asks for. A full page may hide more
/// Issues, so the frontend says so. Keep in step with `LISTING_LIMIT` in
/// src/services/github_issue_cache.rs.
const ISSUE_LIST_LIMIT: &str = "100";

/// Read one Issue as `gh issue list` or `gh issue view` answered it.
///
/// Labels, assignees and the comment count come from GitHub's answer, since
/// category and priority are derived from the labels. A missing or malformed
/// timestamp is an error, never the current time: the stored Task would
/// otherwise claim a history GitHub never recorded.
fn issue_from_json(v: &serde_json::Value) -> Result<GithubIssue, String> {
    let text = |key: &str| v.get(key).and_then(serde_json::Value::as_str);
    let time = |key: &str| -> Result<chrono::DateTime<chrono::Utc>, String> {
        let raw = text(key).ok_or_else(|| format!("GitHub's issue has no {key}"))?;
        chrono::DateTime::parse_from_rfc3339(raw)
            .map(|d| d.with_timezone(&chrono::Utc))
            .map_err(|e| format!("GitHub's {key} is not a timestamp: {e}"))
    };
    let number = v
        .get("number")
        .and_then(serde_json::Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or("GitHub's issue has no valid number")?;
    let state = text("state")
        .filter(|s| matches!(*s, "OPEN" | "CLOSED"))
        .ok_or_else(|| format!("GitHub did not report the state of issue #{number}"))?;
    let title = text("title").ok_or_else(|| format!("issue #{number} has no title"))?;
    let labels = v
        .get("labels")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|lbl| {
                    Some(Label {
                        name: lbl.get("name")?.as_str()?.to_string(),
                        color: lbl
                            .get("color")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let assignees = v
        .get("assignees")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|asn| {
                    Some(Assignee {
                        login: asn.get("login")?.as_str()?.to_string(),
                        avatar_url: asn
                            .get("avatarUrl")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    // `gh` answers comments as a list of comment objects, not a count.
    let comments = v
        .get("comments")
        .and_then(serde_json::Value::as_array)
        .map_or(0, |list| list.len() as i32);
    Ok(GithubIssue {
        number,
        title: title.to_string(),
        body: text("body").unwrap_or("").to_string(),
        state: state.to_string(),
        labels,
        assignees,
        comments,
        created_at: time("createdAt")?,
        updated_at: time("updatedAt")?,
    })
}

fn issues_from_json(json: &serde_json::Value) -> Result<Vec<GithubIssue>, String> {
    json.as_array()
        .ok_or("GitHub's issue list was not a list")?
        .iter()
        .map(issue_from_json)
        .collect()
}

#[tauri::command]
pub async fn get_issues(repo: String, state: String) -> Result<Vec<GithubIssue>, String> {
    if !ISSUE_LIST_STATES.contains(&state.as_str()) {
        return Err(format!("The Issue state must be open, closed or all, not {state}"));
    }
    check_gh().await?;

    let json = run_gh(&[
        "issue",
        "list",
        "--repo",
        &repo,
        "--state",
        &state,
        "--json",
        ISSUE_FIELDS,
        "--limit",
        ISSUE_LIST_LIMIT,
    ])
    .await?;
    issues_from_json(&json)
}

#[tauri::command]
pub async fn get_issue(repo: String, number: i32) -> Result<GithubIssue, String> {
    issue_from_json(&view_issue(&repo, number).await?)
}

/// `gh issue view` for one Issue, as GitHub answered it.
async fn view_issue(repo: &str, number: i32) -> Result<serde_json::Value, String> {
    check_gh().await?;
    run_gh(&["issue", "view", &number.to_string(), "--repo", repo, "--json", ISSUE_FIELDS]).await
}

/// Refuse an answer that does not name the Issue asked for. An Issue
/// transferred to another repository is answered at its new address, and it
/// may have a new number there, so the address is compared as well as the
/// number. `advice` tells the person what to do instead.
fn check_issue_address(
    json: &serde_json::Value,
    repo: &str,
    number: u32,
    advice: &str,
) -> Result<(), String> {
    let url = json.get("url").and_then(serde_json::Value::as_str).unwrap_or_default();
    let Ok(home) = parse_issue_url(url) else {
        return Err(format!("GitHub did not answer with issue #{number} in {repo}"));
    };
    if !home.repo.eq_ignore_ascii_case(repo) {
        return Err(format!(
            "GitHub reports issue #{number} in {repo} at {}. {advice}",
            home.url()
        ));
    }
    let answered = json.get("number").and_then(serde_json::Value::as_u64);
    if home.number != number || answered != Some(u64::from(number)) {
        return Err(format!("GitHub did not answer with issue #{number} in {repo}"));
    }
    Ok(())
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
    check_issue_address(&json, repo, number, "Link that address instead.")?;
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
    crate::commands::project::refuse_if_project_removed(&state.project.projects, project_id).await?;
    let address = crate::domain::github_issue_link::parse_issue_url(&format!(
        "https://github.com/{repo}/issues/{issue_number}"
    ))?;
    // Refuse before the Issue is fetched: a second Task for it would duplicate
    // the Issue. The person links the existing Task instead.
    {
        let tasks = state.task.tasks.read().await;
        if let Some(refusal) = crate::domain::github_issue_link::project_issue_refusal(
            tasks.values(),
            project_id,
            &address,
        ) {
            return Err(refusal);
        }
    }

    // The Task names the Issue as GitHub answers for it. A transferred Issue is
    // refused, as linking refuses it, rather than stored under the typed repository.
    let answer = view_issue(&repo, issue_number).await?;
    check_issue_address(
        &answer,
        &repo,
        address.number,
        "No Task was created. Load that repository to create one from it.",
    )?;
    let issue = issue_from_json(&answer)?;

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
        pr_reply_records: Vec::new(),
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

    create_issue_task(&state, task, address).await
}

/// Insert the Task for an Issue unless this Project already links that Issue.
///
/// The refusal is checked inside [`crate::lifecycle::create_checked`], under
/// the Tasks write lock that the insert also needs. Two requests for the same
/// Issue therefore serialize: the second one sees the first Task and is
/// refused, and a refusal writes nothing.
async fn create_issue_task(
    state: &crate::AppState,
    task: Task,
    address: IssueAddress,
) -> Result<Task, String> {
    let project_id = task.project_id;
    crate::lifecycle::create_checked(
        &state.task.project_gate,
        &state.task.tasks,
        &state.project.projects,
        &state.storage,
        project_id,
        move |existing| match project_issue_refusal(existing.values(), project_id, &address) {
            Some(refusal) => Err(refusal),
            None => Ok(task),
        },
    )
    .await
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

    /// Creating a Task reads the same canonical address as linking does: a
    /// transferred Issue is refused with its new address and the advice given,
    /// and the Issue at its typed address is accepted.
    #[test]
    fn a_created_task_takes_its_issue_identity_only_from_the_canonical_address() {
        let moved = serde_json::json!({
            "number": 12, "url": "https://github.com/new/repo/issues/12"
        });
        let refusal = check_issue_address(&moved, "old/repo", 7, "No Task was created.")
            .unwrap_err();
        assert!(refusal.contains("https://github.com/new/repo/issues/12"), "{refusal}");
        assert!(refusal.contains("No Task was created."), "{refusal}");

        let same = serde_json::json!({
            "number": 7, "url": "https://github.com/Old/Repo/issues/7"
        });
        assert!(check_issue_address(&same, "old/repo", 7, "unused").is_ok());

        let renumbered = serde_json::json!({
            "number": 8, "url": "https://github.com/old/repo/issues/8"
        });
        let mismatch = check_issue_address(&renumbered, "old/repo", 7, "unused").unwrap_err();
        assert!(mismatch.contains("did not answer with issue #7"), "{mismatch}");
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

#[cfg(test)]
mod issue_create_tests {
    use super::*;
    use crate::config::paths::StateLocation;
    use crate::domain::project::ProjectScope;
    use crate::domain::{AgentConfig, AgentType, Project};
    use crate::test_helpers::create_test_task_full;
    use std::collections::HashMap;

    /// A `gh issue view --json` answer in the shape GitHub returns it.
    fn gh_issue() -> serde_json::Value {
        serde_json::json!({
            "number": 42,
            "title": "Crash on empty input",
            "body": "Steps:\n\n```\nrun --empty\n```",
            "state": "OPEN",
            "labels": [
                {"id": "L1", "name": "bug", "description": "", "color": "d73a4a"},
                {"id": "L2", "name": "priority: high", "description": "", "color": "ff0000"}
            ],
            "assignees": [{"id": "U1", "login": "octocat", "name": ""}],
            "comments": [
                {"id": "C1", "author": {"login": "reviewer"}, "body": "Seen it", "createdAt": "2026-11-10T10:00:00Z"},
                {"id": "C2", "author": {"login": "octocat"}, "body": "Fixing", "createdAt": "2026-11-11T10:00:00Z"}
            ],
            "createdAt": "2026-11-01T09:30:00Z",
            "updatedAt": "2026-11-15T18:45:00Z"
        })
    }

    async fn test_state() -> (tempfile::TempDir, crate::AppState) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let (state, _report) = crate::app_core::build_state_with_paths(paths)
            .await
            .expect("state should build under a fresh tempdir");
        (tmp, state)
    }

    async fn insert_project(state: &crate::AppState) -> Uuid {
        let project_id = Uuid::new_v4();
        state.project.projects.write().await.insert(
            project_id,
            Project {
                id: project_id,
                name: "issues".to_string(),
                repository_id: None,
                scope: ProjectScope::Standalone,
                state_location: StateLocation::External,
                base: None,
                agent_type: AgentType::ClaudeCode,
                agent_config: AgentConfig {
                    agent_type: AgentType::ClaudeCode,
                    command: "claude".to_string(),
                    args: Vec::new(),
                    env: HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                removed_at: None,
                pr_review_defaults: Default::default(),
            },
        );
        project_id
    }

    /// A Task that links `repo` issue `number` as creation would have stored it.
    fn linked_task(project_id: Uuid, repo: &str, number: u32) -> Task {
        let mut task = create_test_task_full("linked", project_id, TaskStatus::Backlog, 0);
        task.external_refs = vec![ExternalRef::GithubIssue {
            url: format!("https://github.com/{repo}/issues/{number}"),
            number,
            repo: repo.to_string(),
            state: Some("OPEN".to_string()),
        }];
        task
    }

    fn address(repo: &str, number: u32) -> IssueAddress {
        IssueAddress { repo: repo.to_string(), number }
    }

    #[test]
    fn a_view_keeps_labels_assignees_comments_and_github_timestamps() {
        let issue = issue_from_json(&gh_issue()).expect("realistic answer");
        assert_eq!(issue.labels.len(), 2);
        assert_eq!(issue.labels[1].name, "priority: high");
        assert_eq!(issue.assignees[0].login, "octocat");
        assert_eq!(issue.comments, 2);
        assert_eq!(issue.created_at.to_rfc3339(), "2026-11-01T09:30:00+00:00");
        assert_eq!(issue.updated_at.to_rfc3339(), "2026-11-15T18:45:00+00:00");
    }

    #[test]
    fn the_created_task_takes_category_and_priority_from_real_labels() {
        let issue = issue_from_json(&gh_issue()).expect("realistic answer");
        assert!(matches!(map_labels_to_category(&issue.labels), TaskCategory::BugFix));
        assert!(matches!(map_labels_to_priority(&issue.labels), TaskPriority::High));
    }

    #[test]
    fn malformed_answers_are_refused_not_filled_in() {
        let mut no_created = gh_issue();
        no_created.as_object_mut().unwrap().remove("createdAt");
        assert!(issue_from_json(&no_created).unwrap_err().contains("createdAt"));

        let mut bad_time = gh_issue();
        bad_time["updatedAt"] = serde_json::json!("yesterday");
        assert!(issue_from_json(&bad_time).unwrap_err().contains("updatedAt"));

        let mut lowercase = gh_issue();
        lowercase["state"] = serde_json::json!("open");
        assert!(issue_from_json(&lowercase).is_err());

        let mut no_number = gh_issue();
        no_number.as_object_mut().unwrap().remove("number");
        assert!(issue_from_json(&no_number).is_err());

        assert!(issues_from_json(&gh_issue()).is_err(), "a single issue is not a list");
    }

    #[test]
    fn a_closed_issue_in_a_list_is_read_as_closed() {
        let mut closed = gh_issue();
        closed["state"] = serde_json::json!("CLOSED");
        let listed = issues_from_json(&serde_json::json!([closed])).expect("a list");
        assert_eq!(listed[0].state, "CLOSED");
    }

    #[test]
    fn the_issue_list_asks_gh_for_one_hundred_issues() {
        assert_eq!(ISSUE_LIST_LIMIT.parse::<usize>(), Ok(100));
    }

    #[tokio::test]
    async fn an_unknown_state_is_refused_before_gh_runs() {
        let error = get_issues("owner/repo".to_string(), "merged".to_string())
            .await
            .expect_err("merged is not an Issue state");
        assert!(error.contains("open, closed or all"), "{error}");
    }

    /// Two requests for one Issue, against real storage: exactly one Task is
    /// created, the other is refused, and the file holds only the first.
    #[tokio::test]
    async fn two_concurrent_creations_for_one_issue_create_one_task() {
        let (_tmp, state) = test_state().await;
        let project_id = insert_project(&state).await;

        let first = create_issue_task(&state, linked_task(project_id, "o/r", 7), address("o/r", 7));
        let second = create_issue_task(&state, linked_task(project_id, "O/R", 7), address("O/R", 7));
        let (a, b) = tokio::join!(first, second);

        assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(), 1, "{a:?} {b:?}");
        let refused = a.err().or(b.err()).expect("one request was refused");
        assert!(refused.contains("already linked"), "{refused}");
        assert_eq!(state.task.tasks.read().await.len(), 1);
        assert_eq!(state.storage.load_project_tasks(project_id).unwrap().len(), 1);
    }

    /// Another Project may create its own Task for the same Issue, and a
    /// refused creation writes nothing.
    #[tokio::test]
    async fn the_duplicate_policy_is_scoped_to_one_project() {
        let (_tmp, state) = test_state().await;
        let first = insert_project(&state).await;
        let second = insert_project(&state).await;

        create_issue_task(&state, linked_task(first, "o/r", 7), address("o/r", 7))
            .await
            .expect("first Project creates the Task");
        create_issue_task(&state, linked_task(second, "o/r", 7), address("o/r", 7))
            .await
            .expect("another Project is independent");
        let refused = create_issue_task(&state, linked_task(first, "o/r", 7), address("o/r", 7)).await;
        assert!(refused.is_err());

        assert_eq!(state.storage.load_project_tasks(first).unwrap().len(), 1);
        assert_eq!(state.storage.load_project_tasks(second).unwrap().len(), 1);
    }
}
