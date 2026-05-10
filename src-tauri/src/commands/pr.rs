use crate::domain::{Task, TaskStatus};
use crate::domain::task::ExternalRef;
use uuid::Uuid;

/// Parse a GitHub PR URL into an ExternalRef::GithubPr
fn parse_pr_url_to_ref(pr_url: &str) -> Option<ExternalRef> {
    let parts: Vec<&str> = pr_url.trim_end_matches('/').split('/').collect();
    let pull_idx = parts.iter().position(|&p| p == "pull")?;
    let gh_idx = parts.iter().position(|&p| p == "github.com")?;
    if gh_idx + 2 >= pull_idx { return None; }
    let number: u32 = parts.get(pull_idx + 1)?.parse().ok()?;
    let repo = format!("{}/{}", parts[gh_idx + 1], parts[gh_idx + 2]);
    Some(ExternalRef::GithubPr {
        url: pr_url.to_string(),
        number,
        repo,
        state: Some("OPEN".to_string()),
    })
}

/// Resolve working directory for a task via project → repository chain.
/// Prefers worktree_path if set on the task, otherwise falls back to repo root.
async fn resolve_working_dir(
    state: &crate::AppState,
    task_id: Uuid,
) -> Result<String, String> {
    let tasks = state.task.tasks.read().await;
    let task = tasks.get(&task_id).ok_or("Task not found")?;

    // Use worktree path if available
    if let Some(ref wt_path) = task.worktree_path {
        return Ok(wt_path.clone());
    }

    // Fall back to repository path
    let project_id = task.project_id;
    drop(tasks);

    let projects = state.project.projects.read().await;
    let project = projects.get(&project_id).ok_or("Project not found")?;
    let repo_id = project.repository_id.ok_or("No repository linked to project")?;
    drop(projects);

    let repos = state.repository.repositories.read().await;
    let repo = repos.get(&repo_id).ok_or("Repository not found")?;
    Ok(repo.local_path.clone())
}

/// Run an async command and return stdout on success, or Err with stderr.
async fn run_cmd(cmd: &str, args: &[&str], cwd: &str) -> Result<String, String> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| format!("Failed to run {}: {}", cmd, e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{} failed: {}", cmd, stderr));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[tauri::command]
pub async fn create_pr(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<String, String> {
    create_pr_inner(&state, &task_id).await
}

#[tauri::command]
pub async fn bulk_create_prs(
    state: tauri::State<'_, crate::AppState>,
    task_ids: Vec<String>,
) -> Result<Vec<String>, String> {
    let mut results = Vec::new();

    for task_id in task_ids {
        match create_pr_inner(&state, &task_id).await {
            Ok(url) => results.push(format!("Created PR for {}: {}", task_id, url)),
            Err(e) => results.push(format!("Failed for {}: {}", task_id, e)),
        }
    }

    Ok(results)
}

async fn create_pr_inner(
    state: &crate::AppState,
    task_id: &str,
) -> Result<String, String> {
    let task_uuid = Uuid::parse_str(task_id).map_err(|e| e.to_string())?;
    let working_dir = resolve_working_dir(state, task_uuid).await?;

    let (pr_title, pr_body, task_branch_name) = {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_uuid).ok_or("Task not found")?;
        (task.title.clone(), build_pr_body(task), task.branch_name.clone())
    };

    let branch = push_branch(&working_dir, task_branch_name.as_deref()).await?;

    let pr_url = run_cmd(
        "gh",
        &[
            "pr", "create",
            "--title", &pr_title,
            "--body", &pr_body,
            "--head", &branch,
        ],
        &working_dir,
    ).await?;

    {
        let mut tasks = state.task.tasks.write().await;
        if let Some(task) = tasks.get_mut(&task_uuid) {
            task.pr_url = Some(pr_url.clone());
            if let Some(ref_) = parse_pr_url_to_ref(&pr_url) {
                if !task.external_refs.iter().any(|r| matches!(r, ExternalRef::GithubPr { url, .. } if url == &pr_url)) {
                    task.external_refs.push(ref_);
                }
            }
            task.status = TaskStatus::PrCreated;
            task.updated_at = chrono::Utc::now();
        }
    }

    let tasks_r = state.task.tasks.read().await;
    if let Some(task) = tasks_r.get(&task_uuid) {
        let project_id = task.project_id;
        let project_tasks: Vec<Task> = tasks_r.values()
            .filter(|t| t.project_id == project_id)
            .cloned()
            .collect();
        let _ = state.storage.save_project_tasks(project_id, &project_tasks);
    }

    Ok(pr_url)
}

fn build_pr_body(task: &Task) -> String {
    let mut body = task.description.clone().unwrap_or_default();

    let fixes: Vec<String> = task.external_refs.iter()
        .filter_map(|r| match r {
            ExternalRef::GithubIssue { number, .. } => Some(format!("Fixes #{}", number)),
            _ => None,
        })
        .collect();

    if !fixes.is_empty() {
        if !body.is_empty() { body.push_str("\n\n"); }
        body.push_str(&fixes.join("\n"));
    } else if let Some(ref issue_url) = task.github_issue_url {
        if let Some(number) = issue_url.rsplit('/').next().and_then(|n| n.parse::<u32>().ok()) {
            if !body.is_empty() { body.push_str("\n\n"); }
            body.push_str(&format!("Fixes #{}", number));
        }
    }

    body
}

async fn push_branch(working_dir: &str, known_branch: Option<&str>) -> Result<String, String> {
    if let Some(branch) = known_branch {
        run_cmd("git", &["push", "-u", "origin", branch], working_dir).await
            .map_err(|e| format!("git push failed: {}", e))?;
        return Ok(branch.to_string());
    }

    run_cmd("jj", &["git", "export"], working_dir).await
        .map_err(|e| format!("jj git export failed: {}", e))?;

    let branch = run_cmd("git", &["branch", "--show-current"], working_dir).await
        .map_err(|e| format!("Could not determine branch: {}", e))?;

    if branch.is_empty() {
        return Err("No branch name found. Create a jj bookmark first.".to_string());
    }

    if let Err(jj_err) = run_cmd("jj", &["git", "push", "--allow-new"], working_dir).await {
        run_cmd("git", &["push", "-u", "origin", &branch], working_dir).await
            .map_err(|git_err| format!("Push failed. jj: {}. git: {}", jj_err, git_err))?;
    }

    Ok(branch)
}

#[tauri::command]
pub async fn submit_stack(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Vec<String>, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let working_dir = resolve_working_dir(&state, task_uuid).await?;

    if !state.worktree_manager.gs_available {
        return Err("git-spice not available".to_string());
    }

    // Submit the entire stack
    let output = tokio::process::Command::new("git-spice")
        .args(["stack", "submit"])
        .current_dir(&working_dir)
        .output()
        .await
        .map_err(|e| format!("git-spice stack submit failed: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "git-spice stack submit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(vec![stdout])
}

/// PR status from GitHub
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrStatus {
    pub state: PrState,
    pub checks_passing: Option<bool>,
    pub review_decision: Option<ReviewDecision>,
    pub mergeable: Option<Mergeability>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    Merged,
    Unknown,
}

impl PrState {
    fn from_gh(s: &str) -> Self {
        match s {
            "OPEN" => Self::Open,
            "CLOSED" => Self::Closed,
            "MERGED" => Self::Merged,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

impl ReviewDecision {
    fn from_gh(s: &str) -> Option<Self> {
        match s {
            "APPROVED" => Some(Self::Approved),
            "CHANGES_REQUESTED" => Some(Self::ChangesRequested),
            "REVIEW_REQUIRED" => Some(Self::ReviewRequired),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mergeability {
    Mergeable,
    Conflicting,
    Unknown,
}

impl Mergeability {
    fn from_gh(s: &str) -> Option<Self> {
        match s {
            "MERGEABLE" => Some(Self::Mergeable),
            "CONFLICTING" => Some(Self::Conflicting),
            "UNKNOWN" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[tauri::command]
pub async fn get_pr_status(
    pr_url: String,
) -> Result<PrStatus, String> {
    // Parse PR URL: https://github.com/{owner}/{repo}/pull/{number}
    let parts: Vec<&str> = pr_url.trim_end_matches('/').split('/').collect();

    let pull_idx = parts.iter().position(|&p| p == "pull")
        .ok_or("Not a GitHub PR URL (missing /pull/ segment)")?;

    if pull_idx + 1 >= parts.len() {
        return Err("PR URL missing number after /pull/".to_string());
    }

    let number = parts[pull_idx + 1];
    if !number.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("Invalid PR number: {}", number));
    }

    let repo_idx = parts.iter().position(|&p| p == "github.com")
        .ok_or("Not a GitHub URL")?;

    if repo_idx + 2 >= pull_idx {
        return Err("Invalid GitHub PR URL format".to_string());
    }

    let owner = parts[repo_idx + 1];
    let repo = parts[repo_idx + 2];

    let output = tokio::process::Command::new("gh")
        .args([
            "pr", "view", number,
            "--repo", &format!("{}/{}", owner, repo),
            "--json", "state,statusCheckRollup,reviewDecision,mergeable",
        ])
        .output()
        .await
        .map_err(|e| format!("Failed to run gh: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh pr view failed: {}", stderr));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse gh output: {}", e))?;

    let state = PrState::from_gh(json["state"].as_str().unwrap_or("UNKNOWN"));
    let review_decision = json["reviewDecision"].as_str().and_then(ReviewDecision::from_gh);
    let mergeable = json["mergeable"].as_str().and_then(Mergeability::from_gh);

    // Check passes if all conclusions are SUCCESS or NEUTRAL (SKIPPED is also OK)
    let checks_passing = json["statusCheckRollup"].as_array().map(|checks| {
        if checks.is_empty() {
            return true;
        }
        checks.iter().all(|c| {
            let conclusion = c["conclusion"].as_str().unwrap_or("");
            matches!(conclusion, "SUCCESS" | "NEUTRAL" | "SKIPPED")
        })
    });

    Ok(PrStatus {
        state,
        checks_passing,
        review_decision,
        mergeable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task::ExternalRef;
    use crate::test_helpers::create_test_task;

    use super::build_pr_body;

    // ──────────────────────────────────────────────
    // PR body "Fixes #N" generation tests
    // ──────────────────────────────────────────────

    #[test]
    fn pr_body_one_github_issue_ref() {
        let mut task = create_test_task("Fix login bug");
        task.description = Some("Login fails on Safari".to_string());
        task.external_refs = vec![
            ExternalRef::GithubIssue {
                url: "https://github.com/org/repo/issues/123".to_string(),
                number: 123,
                repo: "org/repo".to_string(),
                state: Some("OPEN".to_string()),
            },
        ];

        let body = build_pr_body(&task);
        assert!(body.contains("Fixes #123"));
        assert!(body.contains("Login fails on Safari"));
    }

    #[test]
    fn pr_body_multiple_github_issue_refs() {
        let mut task = create_test_task("Fix multiple bugs");
        task.description = Some("Addresses several issues".to_string());
        task.external_refs = vec![
            ExternalRef::GithubIssue {
                url: "https://github.com/org/repo/issues/10".to_string(),
                number: 10,
                repo: "org/repo".to_string(),
                state: None,
            },
            ExternalRef::GithubIssue {
                url: "https://github.com/org/repo/issues/20".to_string(),
                number: 20,
                repo: "org/repo".to_string(),
                state: None,
            },
            ExternalRef::GithubIssue {
                url: "https://github.com/org/repo/issues/30".to_string(),
                number: 30,
                repo: "org/repo".to_string(),
                state: None,
            },
        ];

        let body = build_pr_body(&task);
        assert!(body.contains("Fixes #10"));
        assert!(body.contains("Fixes #20"));
        assert!(body.contains("Fixes #30"));
    }

    #[test]
    fn pr_body_github_pr_refs_only_no_fixes() {
        let mut task = create_test_task("Follow-up PR");
        task.description = Some("Follow-up changes".to_string());
        task.external_refs = vec![
            ExternalRef::GithubPr {
                url: "https://github.com/org/repo/pull/50".to_string(),
                number: 50,
                repo: "org/repo".to_string(),
                state: Some("MERGED".to_string()),
            },
        ];

        let body = build_pr_body(&task);
        assert!(!body.contains("Fixes"));
    }

    #[test]
    fn pr_body_no_refs_no_fixes() {
        let mut task = create_test_task("New feature");
        task.description = Some("Brand new feature".to_string());

        let body = build_pr_body(&task);
        assert!(!body.contains("Fixes"));
        assert_eq!(body, "Brand new feature");
    }

    #[test]
    fn pr_body_mixed_refs_only_github_issues_get_fixes() {
        let mut task = create_test_task("Mixed refs task");
        task.description = Some("Mixed references".to_string());
        task.external_refs = vec![
            ExternalRef::GithubIssue {
                url: "https://github.com/org/repo/issues/7".to_string(),
                number: 7,
                repo: "org/repo".to_string(),
                state: None,
            },
            ExternalRef::JiraTicket {
                key: "PLAT-99".to_string(),
                project: "PLAT".to_string(),
            },
            ExternalRef::GithubPr {
                url: "https://github.com/org/repo/pull/8".to_string(),
                number: 8,
                repo: "org/repo".to_string(),
                state: None,
            },
            ExternalRef::LinearTicket {
                id: "LIN-1".to_string(),
            },
            ExternalRef::GitlabIssue {
                url: "https://gitlab.com/org/repo/-/issues/9".to_string(),
            },
        ];

        let body = build_pr_body(&task);
        assert!(body.contains("Fixes #7"));
        // Only GithubIssue produces Fixes lines
        assert!(!body.contains("Fixes #8")); // PR, not issue
        assert!(!body.contains("PLAT-99"));
        assert!(!body.contains("LIN-1"));
    }

    #[test]
    fn pr_body_legacy_github_issue_url_fallback() {
        let mut task = create_test_task("Legacy task");
        task.description = Some("Uses legacy field".to_string());
        // No external_refs, so falls back to github_issue_url
        task.github_issue_url = Some("https://github.com/org/repo/issues/55".to_string());

        let body = build_pr_body(&task);
        assert!(body.contains("Fixes #55"));
    }

    #[test]
    fn pr_body_no_description_with_issue_ref() {
        let mut task = create_test_task("No desc fix");
        task.external_refs = vec![
            ExternalRef::GithubIssue {
                url: "https://github.com/org/repo/issues/1".to_string(),
                number: 1,
                repo: "org/repo".to_string(),
                state: None,
            },
        ];

        let body = build_pr_body(&task);
        // No description, so body should start directly with Fixes
        assert_eq!(body, "Fixes #1");
    }

    #[test]
    fn pr_body_description_separated_from_fixes_by_blank_line() {
        let mut task = create_test_task("Separator check");
        task.description = Some("Some description".to_string());
        task.external_refs = vec![
            ExternalRef::GithubIssue {
                url: "https://github.com/org/repo/issues/5".to_string(),
                number: 5,
                repo: "org/repo".to_string(),
                state: None,
            },
        ];

        let body = build_pr_body(&task);
        assert!(body.contains("Some description\n\nFixes #5"));
    }

    // ──────────────────────────────────────────────
    // parse_pr_url_to_ref tests
    // ──────────────────────────────────────────────

    #[test]
    fn parse_pr_url_valid() {
        let result = parse_pr_url_to_ref("https://github.com/acme/widgets/pull/42");
        assert!(result.is_some());
        let r = result.unwrap();
        match r {
            ExternalRef::GithubPr { url, number, repo, state } => {
                assert_eq!(url, "https://github.com/acme/widgets/pull/42");
                assert_eq!(number, 42);
                assert_eq!(repo, "acme/widgets");
                assert_eq!(state, Some("OPEN".to_string()));
            }
            _ => panic!("Expected GithubPr variant"),
        }
    }

    #[test]
    fn parse_pr_url_trailing_slash() {
        let result = parse_pr_url_to_ref("https://github.com/org/repo/pull/7/");
        assert!(result.is_some());
        match result.unwrap() {
            ExternalRef::GithubPr { number, .. } => assert_eq!(number, 7),
            _ => panic!("Expected GithubPr"),
        }
    }

    #[test]
    fn parse_pr_url_invalid_no_pull() {
        let result = parse_pr_url_to_ref("https://github.com/org/repo/issues/42");
        assert!(result.is_none());
    }

    #[test]
    fn parse_pr_url_invalid_no_number() {
        let result = parse_pr_url_to_ref("https://github.com/org/repo/pull/");
        assert!(result.is_none());
    }

    // ──────────────────────────────────────────────
    // PrState / ReviewDecision / Mergeability parsing
    // ──────────────────────────────────────────────

    #[test]
    fn pr_state_from_gh_known_values() {
        assert_eq!(PrState::from_gh("OPEN"), PrState::Open);
        assert_eq!(PrState::from_gh("CLOSED"), PrState::Closed);
        assert_eq!(PrState::from_gh("MERGED"), PrState::Merged);
        assert_eq!(PrState::from_gh("garbage"), PrState::Unknown);
    }

    #[test]
    fn review_decision_from_gh_known_values() {
        assert_eq!(ReviewDecision::from_gh("APPROVED"), Some(ReviewDecision::Approved));
        assert_eq!(ReviewDecision::from_gh("CHANGES_REQUESTED"), Some(ReviewDecision::ChangesRequested));
        assert_eq!(ReviewDecision::from_gh("REVIEW_REQUIRED"), Some(ReviewDecision::ReviewRequired));
        assert_eq!(ReviewDecision::from_gh("OTHER"), None);
    }

    #[test]
    fn mergeability_from_gh_known_values() {
        assert_eq!(Mergeability::from_gh("MERGEABLE"), Some(Mergeability::Mergeable));
        assert_eq!(Mergeability::from_gh("CONFLICTING"), Some(Mergeability::Conflicting));
        assert_eq!(Mergeability::from_gh("UNKNOWN"), Some(Mergeability::Unknown));
        assert_eq!(Mergeability::from_gh("other"), None);
    }
}
