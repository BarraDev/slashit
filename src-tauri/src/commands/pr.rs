use crate::domain::{Task, TaskStatus};
use crate::domain::task::ExternalRef;
use crate::agents::runner::{ClaudeRunConfig, ClaudeRunner};
use tokio::io::AsyncWriteExt;
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

fn parse_pr_url(pr_url: &str) -> Result<(String, String), String> {
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

    Ok((format!("{}/{}", parts[repo_idx + 1], parts[repo_idx + 2]), number.to_string()))
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

fn friendly_pr_error(error: String) -> String {
    if error.contains("GH007") || error.contains("private email address") {
        return [
            "GitHub rejected the push because the task commit uses a private email address.",
            "SlashIt can fix this after you confirm: set this repo's author email to your GitHub noreply address, rewrite the task branch tip author, then retry PR creation.",
        ].join(" ");
    }

    error
}

async fn run_cmd_no_cwd(cmd: &str, args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("Failed to run {}: {}", cmd, e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{} failed: {}", cmd, stderr));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn is_jj_repo(working_dir: &str) -> bool {
    tokio::process::Command::new("jj")
        .args(["root"])
        .current_dir(working_dir)
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

async fn build_pr_push_recovery_plan(
    working_dir: &str,
    branch: &str,
) -> Result<PrPushRecoveryPlan, String> {
    let plan = if is_jj_repo(working_dir).await {
        let template = "commit_id ++ \"\\0\" ++ author.name() ++ \"\\0\" ++ author.email() ++ \"\\0\" ++ description.first_line()";
        let output = run_cmd(
            "jj",
            &[
                "--ignore-working-copy",
                "log",
                "-r", branch,
                "--no-graph",
                "-T", template,
            ],
            working_dir,
        ).await.map_err(|e| format!("Could not inspect jj bookmark `{}`: {}", branch, e))?;
        parse_recovery_plan_output(branch, &output)?
    } else {
        let rev = format!("refs/heads/{}", branch);
        run_cmd("git", &["rev-parse", "--verify", &rev], working_dir)
            .await
            .map_err(|e| format!("Task branch `{}` does not exist locally: {}", branch, e))?;

        let output = run_cmd(
            "git",
            &["show", "-s", "--format=%H%x00%an%x00%ae%x00%s", &rev],
            working_dir,
        ).await.map_err(|e| format!("Could not inspect task branch `{}`: {}", branch, e))?;
        parse_recovery_plan_output(branch, &output)?
    };

    Ok(PrPushRecoveryPlan {
        suggested_email: suggested_github_noreply_email().await,
        ..plan
    })
}

fn parse_recovery_plan_output(branch: &str, output: &str) -> Result<PrPushRecoveryPlan, String> {
    let parts: Vec<&str> = output.trim_end_matches('\n').split('\0').collect();
    if parts.len() < 4 {
        return Err("Could not parse task commit metadata".to_string());
    }

    Ok(PrPushRecoveryPlan {
        branch_name: branch.to_string(),
        commit_sha: parts[0].to_string(),
        author_name: parts[1].to_string(),
        author_email: parts[2].to_string(),
        commit_subject: parts[3].to_string(),
        suggested_email: None,
    })
}

async fn suggested_github_noreply_email() -> Option<String> {
    let output = run_cmd_no_cwd(
        "gh",
        &["api", "user", "--jq", "\"\\(.id)+\\(.login)@users.noreply.github.com\""],
    ).await.ok()?;

    let email = output.trim_matches('"').trim().to_string();
    if email.contains("@users.noreply.github.com") {
        Some(email)
    } else {
        None
    }
}

async fn rewrite_branch_tip_author(
    working_dir: &str,
    branch: &str,
    plan: &PrPushRecoveryPlan,
    new_email: &str,
) -> Result<(), String> {
    if is_jj_repo(working_dir).await {
        let author = format!("{} <{}>", plan.author_name, new_email);
        run_cmd(
            "jj",
            &["config", "set", "--repo", "user.email", new_email],
            working_dir,
        ).await.map_err(|e| format!("Failed to set repo-local jj email: {}", e))?;
        run_cmd(
            "jj",
            &["metaedit", "-r", branch, "--author", &author],
            working_dir,
        ).await.map_err(|e| format!("Failed to rewrite jj author metadata: {}", e))?;
        run_cmd("jj", &["git", "export"], working_dir)
            .await
            .map_err(|e| format!("Failed to export rewritten jj change to Git: {}", e))?;
        return Ok(());
    }

    rewrite_git_branch_tip_author(working_dir, branch, plan, new_email).await
}

async fn rewrite_git_branch_tip_author(
    working_dir: &str,
    branch: &str,
    plan: &PrPushRecoveryPlan,
    new_email: &str,
) -> Result<(), String> {
    let rev = format!("refs/heads/{}", branch);
    let current_sha = run_cmd("git", &["rev-parse", "--verify", &rev], working_dir).await?;
    if current_sha.trim() != plan.commit_sha {
        return Err(format!(
            "Task branch `{}` changed while preparing recovery. Refresh the task and try again.",
            branch
        ));
    }

    let tree = run_cmd("git", &["show", "-s", "--format=%T", &rev], working_dir).await?;
    let parent_output = run_cmd("git", &["show", "-s", "--format=%P", &rev], working_dir).await?;
    let message = run_cmd("git", &["show", "-s", "--format=%B", &rev], working_dir).await?;

    let mut args = vec!["commit-tree".to_string(), tree];
    for parent in parent_output.split_whitespace() {
        args.push("-p".to_string());
        args.push(parent.to_string());
    }

    let mut child = tokio::process::Command::new("git")
        .args(args.iter().map(|s| s.as_str()))
        .current_dir(working_dir)
        .env("GIT_AUTHOR_NAME", &plan.author_name)
        .env("GIT_AUTHOR_EMAIL", new_email)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start git commit-tree: {}", e))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(message.as_bytes())
            .await
            .map_err(|e| format!("Failed to write commit message to git commit-tree: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("Failed to finish git commit-tree: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let new_sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    run_cmd("git", &["update-ref", &rev, &new_sha, &plan.commit_sha], working_dir)
        .await
        .map_err(|e| format!("Failed to update task branch `{}`: {}", branch, e))?;

    Ok(())
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

#[tauri::command]
pub async fn sync_existing_pr(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Option<Task>, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let working_dir = resolve_working_dir(&state, task_uuid).await?;
    let branch = {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_uuid).ok_or("Task not found")?;
        task.branch_name.clone().ok_or("Task has no branch to search for a PR")?
    };

    match find_existing_pr_for_branch_strict(&working_dir, &branch).await? {
        Some(pr_url) => {
            link_pr_to_task(&state, task_uuid, &pr_url).await;
            let tasks = state.task.tasks.read().await;
            Ok(tasks.get(&task_uuid).cloned())
        }
        None => Ok(None),
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PrCandidate {
    pub url: String,
    pub number: u32,
    pub title: String,
    pub state: String,
    pub head_ref_name: String,
    pub reason: String,
}

#[tauri::command]
pub async fn find_pr_candidates(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Vec<PrCandidate>, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let working_dir = resolve_working_dir(&state, task_uuid).await?;
    let task = {
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_uuid).cloned().ok_or("Task not found")?
    };

    let repo = repo_slug_for_task(&task, &working_dir).await?;
    let branch = task.branch_name.clone().unwrap_or_default();
    let issue_numbers: Vec<u32> = task.external_refs.iter().filter_map(|r| match r {
        ExternalRef::GithubIssue { number, .. } => Some(*number),
        _ => None,
    }).collect();
    let task_title_tokens = title_tokens(&task.title);

    let output = tokio::process::Command::new("gh")
        .args([
            "pr", "list",
            "--repo", &repo,
            "--state", "all",
            "--limit", "200",
            "--json", "number,title,url,headRefName,state,closingIssuesReferences",
        ])
        .current_dir(&working_dir)
        .output()
        .await
        .map_err(|e| format!("Failed to run gh: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "gh pr list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let prs: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse gh output: {}", e))?;

    let mut candidates = Vec::new();
    for pr in prs.as_array().cloned().unwrap_or_default() {
        let url = pr.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let title = pr.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let head = pr.get("headRefName").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let state = pr.get("state").and_then(|v| v.as_str()).unwrap_or("UNKNOWN").to_string();
        let number = pr.get("number").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if url.is_empty() || number == 0 {
            continue;
        }

        let mut reasons = Vec::new();
        if !branch.is_empty() && head == branch {
            reasons.push(format!("branch {}", branch));
        }

        if let Some(refs) = pr.get("closingIssuesReferences").and_then(|v| v.as_array()) {
            let linked: Vec<u32> = refs.iter()
                .filter_map(|r| r.get("number").and_then(|n| n.as_u64()).map(|n| n as u32))
                .filter(|n| issue_numbers.contains(n))
                .collect();
            if !linked.is_empty() {
                reasons.push(format!("linked issue {}", linked.iter().map(|n| format!("#{}", n)).collect::<Vec<_>>().join(", ")));
            }
        }

        let pr_tokens = title_tokens(&title);
        let shared = task_title_tokens.iter().filter(|t| pr_tokens.contains(t)).count();
        if shared >= 2 {
            reasons.push("similar title".to_string());
        }

        if !reasons.is_empty() {
            candidates.push(PrCandidate {
                url,
                number,
                title,
                state,
                head_ref_name: head,
                reason: reasons.join(" + "),
            });
        }
    }

    candidates.sort_by_key(|c| {
        if c.reason.contains("branch ") { 0 }
        else if c.reason.contains("linked issue") { 1 }
        else { 2 }
    });
    Ok(candidates)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PrPushRecoveryPlan {
    pub branch_name: String,
    pub commit_sha: String,
    pub commit_subject: String,
    pub author_name: String,
    pub author_email: String,
    pub suggested_email: Option<String>,
}

#[tauri::command]
pub async fn get_pr_push_recovery(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<PrPushRecoveryPlan, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let working_dir = resolve_working_dir(&state, task_uuid).await?;
    let branch = {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_uuid).ok_or("Task not found")?;
        task.branch_name.clone().ok_or("Task has no branch to recover")?
    };

    build_pr_push_recovery_plan(&working_dir, &branch).await
}

#[tauri::command]
pub async fn recover_private_email_and_create_pr(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    author_email: String,
) -> Result<String, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let working_dir = resolve_working_dir(&state, task_uuid).await?;
    let branch = {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_uuid).ok_or("Task not found")?;
        task.branch_name.clone().ok_or("Task has no branch to recover")?
    };

    if let Some(existing_pr_url) = find_existing_pr_for_branch(&working_dir, &branch).await? {
        link_pr_to_task(&state, task_uuid, &existing_pr_url).await;
        return Ok(existing_pr_url);
    }

    let plan = build_pr_push_recovery_plan(&working_dir, &branch).await?;
    let email = author_email.trim();
    if email.is_empty() || !email.contains('@') {
        return Err("Recovery email is invalid".to_string());
    }
    if plan.author_email == email {
        return Err("The task commit already uses that author email".to_string());
    }

    run_cmd("git", &["config", "user.email", email], &working_dir).await
        .map_err(|e| format!("Failed to set repo-local Git email: {}", e))?;
    rewrite_branch_tip_author(&working_dir, &branch, &plan, email).await?;

    create_pr_inner(&state, &task_id).await
}

#[tauri::command]
pub async fn analyze_pr_comments(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<String, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let working_dir = resolve_working_dir(&state, task_uuid).await?;

    let task = {
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_uuid).cloned().ok_or("Task not found")?
    };

    let pr_url = task.pr_url.clone()
        .or_else(|| task.external_refs.iter().find_map(|r| match r {
            ExternalRef::GithubPr { url, .. } => Some(url.clone()),
            _ => None,
        }))
        .ok_or("Task does not have a GitHub PR")?;

    let comments = fetch_pr_comments(&pr_url).await?;
    if comments.trim().is_empty() {
        return Ok("No PR comments or requested changes found.".to_string());
    }

    let prompt = build_pr_comment_analysis_prompt(&task, &pr_url, &comments);
    let output = run_claude_pr_helper(prompt, working_dir, false).await?;
    if output.trim().is_empty() {
        return Err(format!(
            "Triage helper finished without producing a plan. \
            The Claude CLI exited before writing a result \
            (max-turns hit, MCP startup stall, or no transcript captured). \
            PR: {} | comments fetched: {} chars. Try again or paste a manual plan.",
            pr_url,
            comments.len()
        ));
    }
    Ok(output)
}

#[tauri::command]
pub async fn address_pr_comments(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    approved_plan: String,
) -> Result<String, String> {
    if approved_plan.trim().is_empty() {
        return Err("Approved plan is empty".to_string());
    }

    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let working_dir = resolve_working_dir(&state, task_uuid).await?;

    let task = {
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_uuid).cloned().ok_or("Task not found")?
    };

    let pr_url = task.pr_url.clone()
        .or_else(|| task.external_refs.iter().find_map(|r| match r {
            ExternalRef::GithubPr { url, .. } => Some(url.clone()),
            _ => None,
        }))
        .ok_or("Task does not have a GitHub PR")?;

    let comments = fetch_pr_comments(&pr_url).await?;
    let prompt = build_pr_comment_fix_prompt(&task, &pr_url, &comments, &approved_plan);
    let output = run_claude_pr_helper(prompt, working_dir.clone(), true).await?;

    let _ = run_cmd("jj", &["describe", "-m", &format!("task: {} (PR review fixes)", task.title)], &working_dir).await;
    let _ = run_cmd("jj", &["git", "export"], &working_dir).await;

    {
        let mut tasks = state.task.tasks.write().await;
        if let Some(task) = tasks.get_mut(&task_uuid) {
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

    Ok(output)
}

async fn fetch_pr_comments(pr_url: &str) -> Result<String, String> {
    let (repo, number) = parse_pr_url(pr_url)?;

    let review_json = run_cmd_no_cwd(
        "gh",
        &[
            "pr", "view", &number,
            "--repo", &repo,
            "--json", "reviews,comments,reviewDecision",
        ],
    ).await?;

    let inline_json = run_cmd_no_cwd(
        "gh",
        &[
            "api",
            &format!("repos/{}/pulls/{}/comments", repo, number),
        ],
    ).await.unwrap_or_else(|e| {
        serde_json::json!([{ "body": format!("Could not fetch inline comments: {}", e) }]).to_string()
    });

    let mut sections = Vec::new();
    sections.push(format!("PR: {}", pr_url));

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&review_json) {
        if let Some(decision) = json.get("reviewDecision").and_then(|v| v.as_str()) {
            sections.push(format!("Review decision: {}", decision));
        }

        if let Some(reviews) = json.get("reviews").and_then(|v| v.as_array()) {
            for review in reviews {
                let state = review.get("state").and_then(|v| v.as_str()).unwrap_or("UNKNOWN");
                let author = review.pointer("/author/login").and_then(|v| v.as_str()).unwrap_or("unknown");
                let body = review.get("body").and_then(|v| v.as_str()).unwrap_or("").trim();
                if !body.is_empty() || state == "CHANGES_REQUESTED" {
                    sections.push(format!("Review by {} [{}]\n{}", author, state, body));
                }
            }
        }

        if let Some(comments) = json.get("comments").and_then(|v| v.as_array()) {
            for comment in comments {
                let author = comment.pointer("/author/login").and_then(|v| v.as_str()).unwrap_or("unknown");
                let body = comment.get("body").and_then(|v| v.as_str()).unwrap_or("").trim();
                if !body.is_empty() {
                    sections.push(format!("Conversation comment by {}\n{}", author, body));
                }
            }
        }
    }

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&inline_json) {
        if let Some(comments) = json.as_array() {
            for comment in comments {
                let author = comment.pointer("/user/login").and_then(|v| v.as_str()).unwrap_or("unknown");
                let path = comment.get("path").and_then(|v| v.as_str()).unwrap_or("unknown file");
                let line = comment.get("line")
                    .or_else(|| comment.get("original_line"))
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let body = comment.get("body").and_then(|v| v.as_str()).unwrap_or("").trim();
                if !body.is_empty() {
                    sections.push(format!("Inline comment by {} at {}:{}\n{}", author, path, line, body));
                }
            }
        }
    }

    Ok(sections.join("\n\n---\n\n"))
}

fn build_pr_comment_analysis_prompt(task: &Task, pr_url: &str, comments: &str) -> String {
    format!(
        r#"# PR Review Triage

Task: {}
PR: {}

## PR Comments
{}

## Instructions
Analyze the review comments without editing files.

For each requested change, decide whether it should be fixed. Separate real issues from optional suggestions, misunderstandings, duplicates, and risky requests.

Return a concise plan using exactly these headings:

## Should Fix
- [ ] file or area: recommended fix and why

## Should Not Fix
- comment/request: why it should be skipped or discussed

## Questions For User
- any decision that needs human confirmation

## Proposed Command
One short sentence describing what you will change if the user confirms."#,
        task.title, pr_url, comments
    )
}

fn build_pr_comment_fix_prompt(task: &Task, pr_url: &str, comments: &str, approved_plan: &str) -> String {
    format!(
        r#"# Apply Approved PR Review Fixes

Task: {}
PR: {}

## Approved Plan From User
{}

## Original PR Comments
{}

## Instructions
Only implement the items explicitly approved in the plan above.
Do not implement items listed as "Should Not Fix" or unresolved questions.
Before editing each area, verify that the issue exists in the current code.
Keep the change focused and minimal.

After edits, summarize:
- FIXED: what changed
- SKIPPED: anything from the approved plan that no longer applied and why"#,
        task.title, pr_url, approved_plan, comments
    )
}

async fn run_claude_pr_helper(prompt: String, working_dir: String, can_edit: bool) -> Result<String, String> {
    let allowed_tools = if can_edit {
        vec![
            "Read".to_string(), "Edit".to_string(), "Write".to_string(),
            "Bash".to_string(), "Glob".to_string(), "Grep".to_string(),
        ]
    } else {
        vec!["Read".to_string(), "Glob".to_string(), "Grep".to_string()]
    };

    let runner = ClaudeRunner::start(ClaudeRunConfig {
        prompt,
        working_dir,
        allowed_tools,
        max_turns: Some(if can_edit { 30 } else { 12 }),
        max_budget_usd: None,
        session_id: Some(Uuid::new_v4().to_string()),
        resume_session: None,
        model: None,
        system_prompt: None,
        disable_mcp: true,
        permission_mode: None,
    }).await?;

    let wait_result = runner.wait().await;
    let output = runner.get_output().await;
    let _ = runner.kill().await;
    wait_result?;

    Ok(output)
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

    if let Some(branch) = task_branch_name.as_deref() {
        if let Some(existing_pr_url) = find_existing_pr_for_branch(&working_dir, branch).await? {
            link_pr_to_task(state, task_uuid, &existing_pr_url).await;
            return Ok(existing_pr_url);
        }
    }

    let branch = push_branch(&working_dir, task_branch_name.as_deref())
        .await
        .map_err(friendly_pr_error)?;

    if let Some(existing_pr_url) = find_existing_pr_for_branch(&working_dir, &branch).await? {
        link_pr_to_task(state, task_uuid, &existing_pr_url).await;
        return Ok(existing_pr_url);
    }

    let pr_url = run_cmd(
        "gh",
        &[
            "pr", "create",
            "--title", &pr_title,
            "--body", &pr_body,
            "--head", &branch,
        ],
        &working_dir,
    ).await.map_err(friendly_pr_error)?;

    link_pr_to_task(state, task_uuid, &pr_url).await;

    Ok(pr_url)
}

async fn find_existing_pr_for_branch(
    working_dir: &str,
    branch: &str,
) -> Result<Option<String>, String> {
    if branch.trim().is_empty() {
        return Ok(None);
    }

    let output = tokio::process::Command::new("gh")
        .args([
            "pr", "list",
            "--head", branch,
            "--state", "all",
            "--limit", "1",
            "--json", "url",
        ])
        .current_dir(working_dir)
        .output()
        .await
        .map_err(|e| format!("Failed to run gh: {}", e))?;

    if !output.status.success() {
        return Ok(None);
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse gh output: {}", e))?;

    Ok(json.as_array()
        .and_then(|items| items.first())
        .and_then(|item| item.get("url"))
        .and_then(|url| url.as_str())
        .map(|url| url.to_string()))
}

async fn find_existing_pr_for_branch_strict(
    working_dir: &str,
    branch: &str,
) -> Result<Option<String>, String> {
    if branch.trim().is_empty() {
        return Err("Task branch is empty".to_string());
    }

    let output = tokio::process::Command::new("gh")
        .args([
            "pr", "list",
            "--head", branch,
            "--state", "all",
            "--limit", "1",
            "--json", "url",
        ])
        .current_dir(working_dir)
        .output()
        .await
        .map_err(|e| format!("Failed to run gh: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "gh pr list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse gh output: {}", e))?;

    Ok(json.as_array()
        .and_then(|items| items.first())
        .and_then(|item| item.get("url"))
        .and_then(|url| url.as_str())
        .map(|url| url.to_string()))
}

async fn link_pr_to_task(
    state: &crate::AppState,
    task_uuid: Uuid,
    pr_url: &str,
) {
    let remote_state = fetch_pr_state(pr_url).await;
    {
        let mut tasks = state.task.tasks.write().await;
        if let Some(task) = tasks.get_mut(&task_uuid) {
            task.pr_url = Some(pr_url.to_string());
            if let Some(mut ref_) = parse_pr_url_to_ref(pr_url) {
                if let (ExternalRef::GithubPr { state: ref mut s, .. }, Some(remote)) = (&mut ref_, remote_state.as_ref()) {
                    *s = Some(remote.clone());
                }
                if !task.external_refs.iter().any(|r| matches!(r, ExternalRef::GithubPr { url, .. } if url == pr_url)) {
                    task.external_refs.push(ref_);
                } else if let Some(remote) = remote_state.as_ref() {
                    for r in task.external_refs.iter_mut() {
                        if let ExternalRef::GithubPr { url, state: s, .. } = r {
                            if url == pr_url {
                                *s = Some(remote.clone());
                            }
                        }
                    }
                }
            }
            task.status = if matches!(remote_state.as_deref(), Some("MERGED")) {
                TaskStatus::Done
            } else {
                TaskStatus::PrCreated
            };
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
}

/// Fetch the GitHub-reported state ("OPEN" | "CLOSED" | "MERGED") for a PR URL.
/// Returns None if `gh` fails or the response is unparseable.
async fn fetch_pr_state(pr_url: &str) -> Option<String> {
    let (repo, number) = parse_pr_url(pr_url).ok()?;
    let output = tokio::process::Command::new("gh")
        .args(["pr", "view", &number, "--repo", &repo, "--json", "state"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    json.get("state")
        .and_then(|v| v.as_str())
        .map(|s| s.to_uppercase())
}

#[tauri::command]
pub async fn refresh_task_pr_state(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Option<Task>, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;

    let pr_url = {
        let tasks = state.task.tasks.read().await;
        let task = tasks.get(&task_uuid).ok_or("Task not found")?;
        task.pr_url.clone()
            .or_else(|| task.external_refs.iter().find_map(|r| match r {
                ExternalRef::GithubPr { url, .. } => Some(url.clone()),
                _ => None,
            }))
            .ok_or("Task has no PR linked")?
    };

    let Some(remote_state) = fetch_pr_state(&pr_url).await else {
        return Err("Failed to fetch PR state from gh".to_string());
    };

    let updated = {
        let mut tasks = state.task.tasks.write().await;
        let task = tasks.get_mut(&task_uuid).ok_or("Task not found")?;
        for r in task.external_refs.iter_mut() {
            if let ExternalRef::GithubPr { url, state: s, .. } = r {
                if url == &pr_url {
                    *s = Some(remote_state.clone());
                }
            }
        }
        task.status = match remote_state.as_str() {
            "MERGED" => TaskStatus::Done,
            "CLOSED" | "OPEN" => TaskStatus::PrCreated,
            _ => task.status.clone(),
        };
        task.updated_at = chrono::Utc::now();
        task.clone()
    };

    let tasks_r = state.task.tasks.read().await;
    let project_tasks: Vec<Task> = tasks_r.values()
        .filter(|t| t.project_id == updated.project_id)
        .cloned()
        .collect();
    let _ = state.storage.save_project_tasks(updated.project_id, &project_tasks);

    Ok(Some(updated))
}

async fn repo_slug_for_task(task: &Task, working_dir: &str) -> Result<String, String> {
    if let Some(repo) = task.external_refs.iter().find_map(|r| match r {
        ExternalRef::GithubIssue { repo, .. } | ExternalRef::GithubPr { repo, .. } => Some(repo.clone()),
        _ => None,
    }) {
        return Ok(repo);
    }

    let output = tokio::process::Command::new("gh")
        .args(["repo", "view", "--json", "nameWithOwner"])
        .current_dir(working_dir)
        .output()
        .await
        .map_err(|e| format!("Failed to run gh: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "Could not determine GitHub repo: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse gh repo view output: {}", e))?;
    json.get("nameWithOwner")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or("gh repo view did not return nameWithOwner".to_string())
}

fn title_tokens(title: &str) -> Vec<String> {
    title
        .split(|c: char| !c.is_alphanumeric())
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| s.len() >= 4)
        .collect()
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
        if is_jj_repo(working_dir).await {
            run_cmd("jj", &["git", "export"], working_dir).await
                .map_err(|e| format!("jj git export failed: {}", e))?;
            if let Err(jj_err) = run_cmd("jj", &["git", "push", "--allow-new", "--bookmark", branch], working_dir).await {
                run_cmd("git", &["push", "-u", "origin", branch], working_dir).await
                    .map_err(|git_err| format!("Push failed. jj: {}. git: {}", jj_err, git_err))?;
            }
            return Ok(branch.to_string());
        }

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
