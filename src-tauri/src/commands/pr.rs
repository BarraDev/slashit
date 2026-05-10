use crate::domain::{Task, TaskStatus};
use crate::domain::task::{
    ExternalRef, PrCommentKind, PrReviewApplyResult, PrReviewComment, PrReviewDecision,
    PrReviewItem, PrReviewPlan,
};
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
) -> Result<PrReviewPlan, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let working_dir = resolve_working_dir(&state, task_uuid).await?;
    let task = {
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_uuid).cloned().ok_or("Task not found")?
    };
    let pr_url = pr_url_for_task(&task)?;

    eprintln!("[pr-review] analyze {} for task {}", pr_url, task_uuid);
    let (review_decision, comments) = fetch_pr_review_data(&pr_url).await?;
    eprintln!(
        "[pr-review] {} comments fetched (decision={:?})",
        comments.len(), review_decision,
    );
    if comments.is_empty() {
        // Don't cache empty plans — reviewers can still leave comments later
        // and the user shouldn't have to remember to hit Re-analyze.
        return Ok(PrReviewPlan {
            generated_at: chrono::Utc::now(),
            pr_url,
            review_decision,
            comments,
            items: Vec::new(),
            raw_plan: String::new(),
            last_apply: None,
        });
    }

    let prompt = build_review_analysis_prompt(&task, &pr_url, &comments);
    let raw_output = run_claude_pr_helper(prompt, working_dir, false).await?;
    eprintln!(
        "[pr-review] triage output: {} chars",
        raw_output.len(),
    );
    if raw_output.trim().is_empty() {
        return Err(format!(
            "Triage helper finished without producing output. \
             The Claude CLI exited before writing a result \
             (max-turns hit, MCP startup stall, or no transcript captured). \
             PR: {} | comments: {}.",
            pr_url, comments.len()
        ));
    }
    let items = parse_review_items(&raw_output, &comments);
    eprintln!("[pr-review] parsed {} items", items.len());

    let plan = PrReviewPlan {
        generated_at: chrono::Utc::now(),
        pr_url,
        review_decision,
        comments,
        items,
        raw_plan: raw_output,
        last_apply: None,
    };
    save_review_plan_on_task(&state, task_uuid, plan.clone()).await;
    Ok(plan)
}

/// Re-discuss any items currently flagged Question that have a non-empty
/// `user_note`. The agent receives only those items (with the user's note as
/// guidance) and returns updated decision/reasoning/proposed_change for each.
/// Other items are left untouched. Returns the merged plan.
#[tauri::command]
pub async fn discuss_pr_review_questions(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    plan: PrReviewPlan,
) -> Result<PrReviewPlan, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let working_dir = resolve_working_dir(&state, task_uuid).await?;
    let task = {
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_uuid).cloned().ok_or("Task not found")?
    };

    let merged = discuss_pr_review_questions_inner(task, working_dir, plan).await?;
    save_review_plan_on_task(&state, task_uuid, merged.clone()).await;
    Ok(merged)
}

/// Core logic of `discuss_pr_review_questions` extracted for testability. Owns
/// no `AppState`; the caller resolves the task + working directory and persists
/// the returned plan.
pub async fn discuss_pr_review_questions_inner(
    task: Task,
    working_dir: String,
    plan: PrReviewPlan,
) -> Result<PrReviewPlan, String> {
    let pending: Vec<&PrReviewItem> = plan.items.iter()
        .filter(|i| matches!(i.decision, PrReviewDecision::Question) && !i.user_note.trim().is_empty())
        .collect();
    if pending.is_empty() {
        return Err("No Question items with notes to discuss".to_string());
    }
    eprintln!("[pr-review] discussing {} question items", pending.len());

    let prompt = build_discuss_prompt(&task, &plan.pr_url, &plan.comments, &pending);
    let raw_output = run_claude_pr_helper(prompt, working_dir, false).await?;
    eprintln!("[pr-review] discuss output: {} chars", raw_output.len());
    if raw_output.trim().is_empty() {
        return Err("Discuss helper finished without producing output.".to_string());
    }

    let updates = parse_review_items(&raw_output, &plan.comments);
    if updates.is_empty() {
        return Err("Discuss helper output did not parse as JSON items.".to_string());
    }

    let mut merged = plan;
    for update in updates {
        let Some(target_id) = update.comment_id else { continue; };
        if let Some(existing) = merged.items.iter_mut().find(|i| i.comment_id == Some(target_id)) {
            existing.decision = update.decision;
            existing.reasoning = update.reasoning;
            existing.proposed_change = update.proposed_change;
            existing.approved = update.approved;
            existing.summary = update.summary;
            existing.user_note.clear();
        }
    }

    Ok(merged)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AddressPrReviewOptions {
    pub auto_push: bool,
    pub auto_reply: bool,
    /// When true, the agent runs read-only and describes what it would do, but
    /// no edits, jj describe, push, or PR replies are performed. Result is
    /// saved on the task with `pushed=false`, `replies_posted=0`, and
    /// `agent_summary` containing the dry-run report.
    #[serde(default)]
    pub dry_run: bool,
}

/// Progress event emitted during a per-item apply so the UI can update a
/// status badge next to each item as the agent works through them.
/// `kind` is one of: `item_started`, `item_succeeded`, `item_failed`,
/// `push_started`, `push_done`, `push_failed`, `reply_started`,
/// `reply_done`, `reply_failed`, `all_done`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PrReviewProgress {
    pub task_id: String,
    pub kind: String,
    pub current: Option<usize>,
    pub total: Option<usize>,
    pub comment_id: Option<u64>,
    pub message: Option<String>,
}

/// Callback the inner apply invokes for every progress event. Production code
/// passes one that re-emits via Tauri; tests pass a collector that pushes into
/// a Vec.
pub type ProgressSink = std::sync::Arc<dyn Fn(PrReviewProgress) + Send + Sync>;

pub fn no_progress() -> ProgressSink {
    std::sync::Arc::new(|_| {})
}

#[tauri::command]
pub async fn address_pr_review(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    plan: PrReviewPlan,
    options: AddressPrReviewOptions,
) -> Result<PrReviewApplyResult, String> {
    use tauri::Emitter;
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let working_dir = resolve_working_dir(&state, task_uuid).await?;
    let task = {
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_uuid).cloned().ok_or("Task not found")?
    };

    let app_handle = app.clone();
    let progress: ProgressSink = std::sync::Arc::new(move |ev: PrReviewProgress| {
        if let Err(e) = app_handle.emit("pr-review-progress", &ev) {
            eprintln!("[pr-review] failed to emit progress event: {}", e);
        }
    });

    // Plans written before the lifecycle fields existed (and plans where the
    // frontend hasn't backfilled yet) need fix_done / reply_posted derived from
    // the prior last_apply so the apply loop respects what's already on disk.
    let mut plan = plan;
    plan.backfill_lifecycle_from_last_apply();
    let (result, updated_plan) = address_pr_review_inner(task, working_dir, plan, options, progress).await?;
    save_review_plan_on_task(&state, task_uuid, updated_plan).await;
    Ok(result)
}

/// Core logic of `address_pr_review` extracted for testability. Owns no
/// `AppState`; the caller is responsible for fetching the `Task` + working
/// directory and persisting the returned plan.
///
/// Each approved Fix item is sent to claude in its own invocation, so a single
/// max-turns blowout no longer wipes the whole batch. Failures are recorded
/// per-item; subsequent items still run. Push and replies only happen if at
/// least one item succeeded.
pub async fn address_pr_review_inner(
    task: Task,
    working_dir: String,
    plan: PrReviewPlan,
    options: AddressPrReviewOptions,
    progress: ProgressSink,
) -> Result<(PrReviewApplyResult, PrReviewPlan), String> {
    let pr_url = pr_url_for_task(&task)?;
    let task_id_str = task.id.to_string();

    let approved_indices: Vec<usize> = plan.items.iter().enumerate()
        .filter(|(_, i)| i.approved && matches!(i.decision, PrReviewDecision::Fix))
        .map(|(idx, _)| idx)
        .collect();
    if approved_indices.is_empty() {
        return Err("No approved fix items to apply".to_string());
    }
    let total = approved_indices.len();
    eprintln!(
        "[pr-review] applying {} approved items per-item (auto_push={}, auto_reply={}, dry_run={})",
        total, options.auto_push, options.auto_reply, options.dry_run,
    );

    let (reply_repo, reply_number) = if options.dry_run {
        (String::new(), String::new())
    } else {
        parse_pr_url(&pr_url)?
    };

    // We mutate the plan in place to record per-item lifecycle. `updated_plan`
    // is what we hand back to the caller; the in-loop snapshot of an item is
    // cloned so we don't hold a borrow across the async agent call.
    let mut updated_plan = plan;

    let mut per_item_summaries: Vec<String> = Vec::with_capacity(total);
    let mut fixed_ids: Vec<u64> = Vec::new();
    let mut failed_ids: Vec<u64> = Vec::new();
    let mut fix_errors: Vec<String> = Vec::new();
    let mut replies_posted = 0u32;
    let mut reply_errors: Vec<String> = Vec::new();

    for (loop_idx, &orig_idx) in approved_indices.iter().enumerate() {
        let item = updated_plan.items[orig_idx].clone();
        let current = loop_idx + 1;
        let label = item.comment_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "<none>".to_string());

        progress(PrReviewProgress {
            task_id: task_id_str.clone(),
            kind: "item_started".to_string(),
            current: Some(current),
            total: Some(total),
            comment_id: item.comment_id,
            message: Some(item.summary.clone()),
        });

        // --- Dry-run path: agent only, never touch plan state -----------------
        if options.dry_run {
            let single = vec![&item];
            let prompt = build_review_fix_prompt(&task, &pr_url, &updated_plan.comments, &single, true);
            match run_claude_pr_helper(prompt, working_dir.clone(), false).await {
                Ok(summary) => {
                    if let Some(id) = item.comment_id { fixed_ids.push(id); }
                    per_item_summaries.push(format!(
                        "## Item {}/{} — comment {}: {}\n\n{}",
                        current, total, label, item.summary, summary,
                    ));
                    progress(PrReviewProgress {
                        task_id: task_id_str.clone(),
                        kind: "item_succeeded".to_string(),
                        current: Some(current),
                        total: Some(total),
                        comment_id: item.comment_id,
                        message: None,
                    });
                }
                Err(e) => {
                    if let Some(id) = item.comment_id { failed_ids.push(id); }
                    fix_errors.push(format!("comment {}: {}", label, e));
                    per_item_summaries.push(format!(
                        "## Item {}/{} — comment {}: {} — FAILED\n\n{}",
                        current, total, label, item.summary, e,
                    ));
                    progress(PrReviewProgress {
                        task_id: task_id_str.clone(),
                        kind: "item_failed".to_string(),
                        current: Some(current),
                        total: Some(total),
                        comment_id: item.comment_id,
                        message: Some(e),
                    });
                }
            }
            continue;
        }

        // --- Already fully done — skip silently ------------------------------
        if item.fix_done && item.reply_posted {
            per_item_summaries.push(format!(
                "## Item {}/{} — comment {}: {} — already addressed (skipped)\n",
                current, total, label, item.summary,
            ));
            progress(PrReviewProgress {
                task_id: task_id_str.clone(),
                kind: "item_skipped".to_string(),
                current: Some(current),
                total: Some(total),
                comment_id: item.comment_id,
                message: Some("already fixed and replied".to_string()),
            });
            continue;
        }

        // --- Run agent only if the fix isn't already on disk -----------------
        let mut agent_summary_for_reply: Option<String> = item.last_agent_summary.clone();

        if !item.fix_done {
            let single = vec![&item];
            let prompt = build_review_fix_prompt(&task, &pr_url, &updated_plan.comments, &single, false);
            match run_claude_pr_helper(prompt, working_dir.clone(), true).await {
                Ok(summary) => {
                    if let Some(id) = item.comment_id { fixed_ids.push(id); }
                    agent_summary_for_reply = Some(summary.clone());
                    {
                        let p = &mut updated_plan.items[orig_idx];
                        p.fix_done = true;
                        p.last_agent_summary = Some(summary.clone());
                        p.last_error = None;
                    }
                    per_item_summaries.push(format!(
                        "## Item {}/{} — comment {}: {}\n\n{}",
                        current, total, label, item.summary, summary,
                    ));
                    progress(PrReviewProgress {
                        task_id: task_id_str.clone(),
                        kind: "item_succeeded".to_string(),
                        current: Some(current),
                        total: Some(total),
                        comment_id: item.comment_id,
                        message: None,
                    });
                }
                Err(e) => {
                    if let Some(id) = item.comment_id { failed_ids.push(id); }
                    fix_errors.push(format!("comment {}: {}", label, e));
                    updated_plan.items[orig_idx].last_error = Some(e.clone());
                    per_item_summaries.push(format!(
                        "## Item {}/{} — comment {}: {} — FAILED\n\n{}",
                        current, total, label, item.summary, e,
                    ));
                    progress(PrReviewProgress {
                        task_id: task_id_str.clone(),
                        kind: "item_failed".to_string(),
                        current: Some(current),
                        total: Some(total),
                        comment_id: item.comment_id,
                        message: Some(e),
                    });
                    // Don't even attempt reply for an item whose fix just failed.
                    continue;
                }
            }
        } else {
            // Fix was completed in a prior run; we're only here because the
            // reply is missing. Don't re-run the agent.
            per_item_summaries.push(format!(
                "## Item {}/{} — comment {}: {} — fix already on disk, posting deferred reply\n",
                current, total, label, item.summary,
            ));
            progress(PrReviewProgress {
                task_id: task_id_str.clone(),
                kind: "item_succeeded".to_string(),
                current: Some(current),
                total: Some(total),
                comment_id: item.comment_id,
                message: Some("reusing prior fix".to_string()),
            });
        }

        // --- Reply step (only if enabled and not yet posted) -----------------
        if options.auto_reply && !item.reply_posted {
            let item_for_body = &updated_plan.items[orig_idx];
            let body = build_reply_body(item_for_body, agent_summary_for_reply.as_deref());
            progress(PrReviewProgress {
                task_id: task_id_str.clone(),
                kind: "reply_started".to_string(),
                current: Some(current),
                total: Some(total),
                comment_id: item.comment_id,
                message: None,
            });
            match post_pr_reply(&reply_repo, &reply_number, &pr_url, item.comment_id, &body).await {
                Ok(()) => {
                    replies_posted += 1;
                    updated_plan.items[orig_idx].reply_posted = true;
                    progress(PrReviewProgress {
                        task_id: task_id_str.clone(),
                        kind: "reply_done".to_string(),
                        current: Some(current),
                        total: Some(total),
                        comment_id: item.comment_id,
                        message: None,
                    });
                }
                Err(e) => {
                    reply_errors.push(format!("comment {}: {}", label, e));
                    progress(PrReviewProgress {
                        task_id: task_id_str.clone(),
                        kind: "reply_failed".to_string(),
                        current: Some(current),
                        total: Some(total),
                        comment_id: item.comment_id,
                        message: Some(e),
                    });
                }
            }
        }
    }

    let agent_summary = per_item_summaries.join("\n\n---\n\n");
    let any_new_fix = !fixed_ids.is_empty();

    let mut pushed = false;
    let mut push_branch_name: Option<String> = None;
    let mut push_error: Option<String> = None;

    if !options.dry_run && any_new_fix {
        let _ = run_cmd("jj", &["describe", "-m", &format!(
            "task: {} (PR review fixes: {} of {})",
            task.title, fixed_ids.len(), total,
        )], &working_dir).await;
        let _ = run_cmd("jj", &["git", "export"], &working_dir).await;

        if options.auto_push {
            progress(PrReviewProgress {
                task_id: task_id_str.clone(),
                kind: "push_started".to_string(),
                current: None,
                total: None,
                comment_id: None,
                message: None,
            });
            let branch = task.branch_name.clone();
            match push_branch(&working_dir, branch.as_deref()).await {
                Ok(b) => {
                    pushed = true;
                    push_branch_name = Some(b.clone());
                    progress(PrReviewProgress {
                        task_id: task_id_str.clone(),
                        kind: "push_done".to_string(),
                        current: None,
                        total: None,
                        comment_id: None,
                        message: Some(b),
                    });
                }
                Err(e) => {
                    push_error = Some(e.clone());
                    progress(PrReviewProgress {
                        task_id: task_id_str.clone(),
                        kind: "push_failed".to_string(),
                        current: None,
                        total: None,
                        comment_id: None,
                        message: Some(e),
                    });
                }
            }
        }
    }

    let skipped_ids: Vec<u64> = updated_plan.items.iter()
        .filter(|i| !i.approved || matches!(i.decision, PrReviewDecision::Skip))
        .filter_map(|i| i.comment_id)
        .collect();

    let result = PrReviewApplyResult {
        applied_at: chrono::Utc::now(),
        agent_summary,
        fixed_ids,
        skipped_ids,
        pushed,
        push_branch: push_branch_name,
        replies_posted,
        reply_errors,
        dry_run: options.dry_run,
        failed_ids,
        fix_errors,
        push_error,
    };

    progress(PrReviewProgress {
        task_id: task_id_str.clone(),
        kind: "all_done".to_string(),
        current: Some(total),
        total: Some(total),
        comment_id: None,
        message: None,
    });

    // Only real applies advance the persisted `last_apply` timestamp on the
    // task. Dry-runs are session-local previews: the caller still gets the
    // `PrReviewApplyResult` to display in the modal, but the plan written
    // back to disk keeps whatever real-apply state existed before.
    if !options.dry_run {
        updated_plan.last_apply = Some(result.clone());
    }

    Ok((result, updated_plan))
}

/// Result of `sync_pr_review_replies` — how many GitHub replies were posted
/// in this catch-up pass and any per-item errors.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncPrRepliesResult {
    pub replied: u32,
    pub errors: Vec<String>,
    /// Number of items that already had `reply_posted=true` and were left
    /// untouched. Useful for the UI's confirmation toast.
    pub already_done: u32,
    /// Number of approved Fix items still missing a fix on disk
    /// (`fix_done=false`). These are NOT replied to — the user must run Apply
    /// for them. Carried back so the UI can warn instead of silently dropping.
    pub fix_pending: u32,
}

/// Catch-up reply pass: post replies for items where `fix_done=true` but
/// `reply_posted=false`, without invoking the agent and without pushing.
///
/// This is the recovery path for partial runs: when the agent fixed something
/// but the GitHub reply step failed (rate limit, transient API error, the user
/// closed the modal mid-run, etc.), the user can click "Sync replies" to walk
/// the plan and post only the deferred replies.
#[tauri::command]
pub async fn sync_pr_review_replies(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<SyncPrRepliesResult, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let task = {
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_uuid).cloned().ok_or("Task not found")?
    };
    let mut plan = task.pr_review_plan.clone()
        .ok_or_else(|| "Task has no PR review plan to sync".to_string())?;
    plan.backfill_lifecycle_from_last_apply();

    let (result, updated_plan) = sync_pr_review_replies_inner(task, plan).await?;
    save_review_plan_on_task(&state, task_uuid, updated_plan).await;
    Ok(result)
}

/// Core logic of `sync_pr_review_replies` extracted for testability. Walks
/// the plan's approved Fix items, posts a reply for each one with
/// `fix_done=true && reply_posted=false`, and returns the updated plan with
/// `reply_posted` flipped on whatever succeeded.
pub async fn sync_pr_review_replies_inner(
    task: Task,
    plan: PrReviewPlan,
) -> Result<(SyncPrRepliesResult, PrReviewPlan), String> {
    let pr_url = pr_url_for_task(&task)?;
    let (repo, number) = parse_pr_url(&pr_url)?;
    let mut updated_plan = plan;

    let mut replied = 0u32;
    let mut errors: Vec<String> = Vec::new();
    let mut already_done = 0u32;
    let mut fix_pending = 0u32;

    let approved_indices: Vec<usize> = updated_plan.items.iter().enumerate()
        .filter(|(_, i)| i.approved && matches!(i.decision, PrReviewDecision::Fix))
        .map(|(idx, _)| idx)
        .collect();

    for orig_idx in approved_indices {
        let item = updated_plan.items[orig_idx].clone();
        if item.reply_posted {
            already_done += 1;
            continue;
        }
        if !item.fix_done {
            fix_pending += 1;
            continue;
        }
        let label = item.comment_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "<none>".to_string());
        let body = build_reply_body(&item, item.last_agent_summary.as_deref());
        match post_pr_reply(&repo, &number, &pr_url, item.comment_id, &body).await {
            Ok(()) => {
                replied += 1;
                updated_plan.items[orig_idx].reply_posted = true;
            }
            Err(e) => {
                errors.push(format!("comment {}: {}", label, e));
            }
        }
    }

    Ok((SyncPrRepliesResult { replied, errors, already_done, fix_pending }, updated_plan))
}

fn pr_url_for_task(task: &Task) -> Result<String, String> {
    task.pr_url.clone()
        .or_else(|| task.external_refs.iter().find_map(|r| match r {
            ExternalRef::GithubPr { url, .. } => Some(url.clone()),
            _ => None,
        }))
        .ok_or_else(|| "Task does not have a GitHub PR".to_string())
}

async fn save_review_plan_on_task(state: &crate::AppState, task_id: Uuid, plan: PrReviewPlan) {
    let project_id = {
        let mut tasks = state.task.tasks.write().await;
        let Some(t) = tasks.get_mut(&task_id) else { return; };
        t.pr_review_plan = Some(plan);
        t.updated_at = chrono::Utc::now();
        t.project_id
    };
    let tasks_r = state.task.tasks.read().await;
    let project_tasks: Vec<Task> = tasks_r.values()
        .filter(|t| t.project_id == project_id)
        .cloned()
        .collect();
    let _ = state.storage.save_project_tasks(project_id, &project_tasks);
}

fn parse_gh_ts(v: Option<&serde_json::Value>) -> Option<chrono::DateTime<chrono::Utc>> {
    v.and_then(|x| x.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

async fn fetch_pr_review_data(
    pr_url: &str,
) -> Result<(Option<String>, Vec<PrReviewComment>), String> {
    let (repo, number) = parse_pr_url(pr_url)?;
    let mut comments: Vec<PrReviewComment> = Vec::new();
    let mut review_decision: Option<String> = None;

    let review_json = run_cmd_no_cwd(
        "gh",
        &["pr", "view", &number, "--repo", &repo, "--json", "reviews,comments,reviewDecision,createdAt,updatedAt"],
    ).await?;
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&review_json) {
        review_decision = json.get("reviewDecision").and_then(|v| v.as_str()).map(String::from);
        if let Some(reviews) = json.get("reviews").and_then(|v| v.as_array()) {
            for review in reviews {
                let body = review.get("body").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                let state = review.get("state").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if body.is_empty() && state != "CHANGES_REQUESTED" {
                    continue;
                }
                let id = review.get("id")
                    .or_else(|| review.get("databaseId"))
                    .and_then(|v| v.as_u64());
                let author = review.pointer("/author/login").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                let url = review.get("url").and_then(|v| v.as_str()).map(String::from);
                let display_body = if body.is_empty() { format!("[{}]", state) } else { body };
                let created_at = parse_gh_ts(review.get("submittedAt").or_else(|| review.get("createdAt")));
                comments.push(PrReviewComment {
                    id, kind: PrCommentKind::Review, author, body: display_body,
                    path: None, line: None, url,
                    created_at, updated_at: created_at,
                });
            }
        }
        if let Some(conv) = json.get("comments").and_then(|v| v.as_array()) {
            for c in conv {
                let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                if body.is_empty() { continue; }
                let id = c.get("id").or_else(|| c.get("databaseId")).and_then(|v| v.as_u64());
                let author = c.pointer("/author/login").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                let url = c.get("url").and_then(|v| v.as_str()).map(String::from);
                let created_at = parse_gh_ts(c.get("createdAt"));
                let updated_at = parse_gh_ts(c.get("updatedAt")).or(created_at);
                comments.push(PrReviewComment {
                    id, kind: PrCommentKind::Conversation, author, body,
                    path: None, line: None, url,
                    created_at, updated_at,
                });
            }
        }
    }

    let inline_json = run_cmd_no_cwd(
        "gh",
        &["api", "--paginate", &format!("repos/{}/pulls/{}/comments?per_page=100", repo, number)],
    ).await.unwrap_or_else(|e| {
        eprintln!("[pr-review] inline comments fetch failed: {}", e);
        String::new()
    });
    match serde_json::from_str::<Vec<serde_json::Value>>(&inline_json) {
        Ok(arr) => {
            for c in arr {
                let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                if body.is_empty() { continue; }
                let id = c.get("id").and_then(|v| v.as_u64());
                let author = c.pointer("/user/login").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                let path = c.get("path").and_then(|v| v.as_str()).map(String::from);
                let line = c.get("line").or_else(|| c.get("original_line")).and_then(|v| v.as_i64());
                let url = c.get("html_url").and_then(|v| v.as_str()).map(String::from);
                let created_at = parse_gh_ts(c.get("created_at"));
                let updated_at = parse_gh_ts(c.get("updated_at")).or(created_at);
                comments.push(PrReviewComment {
                    id, kind: PrCommentKind::Inline, author, body, path, line, url,
                    created_at, updated_at,
                });
            }
        }
        Err(e) if !inline_json.is_empty() => {
            eprintln!("[pr-review] inline comments parse failed: {}", e);
        }
        Err(_) => {}
    }

    Ok((review_decision, comments))
}

fn build_review_analysis_prompt(task: &Task, pr_url: &str, comments: &[PrReviewComment]) -> String {
    let comments_text = comments.iter().enumerate().map(|(i, c)| {
        let loc = match (&c.path, c.line) {
            (Some(p), Some(l)) => format!("{}:{}", p, l),
            (Some(p), None) => p.clone(),
            _ => "PR-level".to_string(),
        };
        let id_str = c.id.map(|id| id.to_string()).unwrap_or_else(|| "null".to_string());
        let kind = match c.kind {
            PrCommentKind::Inline => "inline",
            PrCommentKind::Review => "review",
            PrCommentKind::Conversation => "conversation",
        };
        format!(
            "Comment #{i} (id={id}, kind={kind}, author={author}, location={loc}):\n{body}",
            i = i, id = id_str, kind = kind, author = c.author, loc = loc, body = c.body,
        )
    }).collect::<Vec<_>>().join("\n\n---\n\n");

    format!(
        r#"# PR Review Triage

Task: {title}
PR: {pr_url}

## Comments
{comments}

## Instructions
For each comment above, decide whether the request should be applied. Read the
relevant source files (Read/Glob/Grep only) to verify the issue exists. Do not
edit files.

Return a STRICT JSON object on a single line. No markdown fences. No prose
before or after the JSON. Schema:

{{"items":[{{"comment_id":<number-or-null>,"summary":"<short title>","decision":"fix"|"skip"|"question","reasoning":"<why; will be shown to the reviewer as your reply>","proposed_change":"<concrete change you would make>"}}]}}

Use the exact `id` from each comment's header for `comment_id`. Use null only
when the comment had id=null. Make `reasoning` reply-friendly: the user can
post it back to the reviewer verbatim. If the comment is a duplicate of another
one, prefer "skip" with a reasoning that points to the canonical one.
"#,
        title = task.title, pr_url = pr_url, comments = comments_text,
    )
}

fn parse_review_items(output: &str, comments: &[PrReviewComment]) -> Vec<PrReviewItem> {
    let Some(start) = output.find('{') else { return Vec::new(); };
    let Some(end) = output.rfind('}') else { return Vec::new(); };
    if end <= start { return Vec::new(); }
    let candidate = &output[start..=end];

    #[derive(serde::Deserialize)]
    struct Raw { items: Vec<RawItem> }
    #[derive(serde::Deserialize)]
    struct RawItem {
        #[serde(default)] comment_id: Option<u64>,
        #[serde(default)] summary: String,
        #[serde(default)] decision: String,
        #[serde(default)] reasoning: String,
        #[serde(default)] proposed_change: String,
    }

    let raw: Raw = match serde_json::from_str(candidate) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    raw.items.into_iter().map(|i| {
        let decision = match i.decision.to_lowercase().as_str() {
            "fix" => PrReviewDecision::Fix,
            "skip" => PrReviewDecision::Skip,
            _ => PrReviewDecision::Question,
        };
        let approved = matches!(decision, PrReviewDecision::Fix);
        let comment_id = i.comment_id.filter(|id| comments.iter().any(|c| c.id == Some(*id)));
        PrReviewItem {
            comment_id,
            summary: i.summary,
            decision,
            reasoning: i.reasoning,
            proposed_change: i.proposed_change,
            approved,
            user_note: String::new(),
            fix_done: false,
            reply_posted: false,
            last_agent_summary: None,
            last_error: None,
        }
    }).collect()
}

fn build_discuss_prompt(
    task: &Task,
    pr_url: &str,
    comments: &[PrReviewComment],
    pending: &[&PrReviewItem],
) -> String {
    let items_text = pending.iter().enumerate().map(|(i, item)| {
        let related = item.comment_id.and_then(|id| comments.iter().find(|c| c.id == Some(id)));
        let loc = related.map(|c| match (&c.path, c.line) {
            (Some(p), Some(l)) => format!("{}:{}", p, l),
            (Some(p), None) => p.clone(),
            _ => "PR-level".to_string(),
        }).unwrap_or_else(|| "PR-level".to_string());
        let original = related.map(|c| c.body.as_str()).unwrap_or("(comment body unavailable)");
        let id_str = item.comment_id.map(|id| id.to_string()).unwrap_or_else(|| "null".to_string());
        format!(
            "Item #{i} (comment_id={id}, location: {loc}):\n\
             Original reviewer comment:\n{original}\n\n\
             Your prior reasoning: {prior}\n\
             User's note for you: {note}",
            i = i, id = id_str, loc = loc, original = original,
            prior = item.reasoning, note = item.user_note,
        )
    }).collect::<Vec<_>>().join("\n\n---\n\n");

    format!(
        r#"# PR Review Discussion

Task: {title}
PR: {pr_url}

You previously triaged the comments below as "Question" because you weren't
sure. The user has now added a note for each, telling you what they want done
or asking a follow-up. Re-evaluate each item with the user's note as guidance.
Read source files (Read/Glob/Grep only) if you need to verify. Do not edit.

## Items to re-evaluate
{items}

## Output
Return a STRICT JSON object on a single line. No markdown fences. No prose
before or after. Schema (one entry per item above, keyed by comment_id):

{{"items":[{{"comment_id":<number-or-null>,"summary":"<short title>","decision":"fix"|"skip"|"question","reasoning":"<reply to the reviewer; will be posted on the PR>","proposed_change":"<concrete change you would make if Fix>"}}]}}

Rules:
- The user's note is an instruction, not a suggestion to negotiate. If they say
  any variant of "go ahead", "fix it", "yes", "do it", "ok": return
  decision="fix" with ONE concrete proposed_change. Do NOT offer multiple
  options or ask which approach they prefer.
- If they confirm there's nothing to do or say "skip"/"ignore": return "skip".
- Only return "question" if the user's note itself raises a NEW ambiguity that
  blocks a fix decision. In that case put ONE specific follow-up in reasoning.
- proposed_change must be a single concrete edit, not a menu. If multiple
  approaches are reasonable, pick the simplest one that matches the user's note
  and the existing code style; mention the alternative in reasoning at most as
  a one-line aside, never as a numbered list of options.
"#,
        title = task.title, pr_url = pr_url, items = items_text,
    )
}

fn build_review_fix_prompt(
    task: &Task,
    pr_url: &str,
    comments: &[PrReviewComment],
    approved: &[&PrReviewItem],
    dry_run: bool,
) -> String {
    let items_text = approved.iter().enumerate().map(|(i, item)| {
        let related = item.comment_id.and_then(|id| comments.iter().find(|c| c.id == Some(id)));
        let loc = related.map(|c| match (&c.path, c.line) {
            (Some(p), Some(l)) => format!("{}:{}", p, l),
            (Some(p), None) => p.clone(),
            _ => "PR-level".to_string(),
        }).unwrap_or_else(|| "PR-level".to_string());
        format!(
            "Item #{i} (location: {loc}):\nSummary: {summary}\nReasoning: {reasoning}\nProposed change: {change}",
            i = i, loc = loc, summary = item.summary,
            reasoning = item.reasoning, change = item.proposed_change,
        )
    }).collect::<Vec<_>>().join("\n\n---\n\n");

    if dry_run {
        return format!(
            r#"# Dry-run: Plan PR Review Fixes (DO NOT EDIT)

Task: {title}
PR: {pr_url}

## Approved Items
{items}

## Instructions
This is a DRY RUN. You have read-only tools (Read, Glob, Grep). Do NOT edit
any files. For each approved item, verify the issue still exists in the
current code and write a concrete plan describing exactly what you would
change.

Output format (plain text, one section per item):

Item #N (location: <path:line>):
- Verified: yes/no — <one-line evidence from the file>
- Plan: <concrete edit you would make: which lines, what to replace with what>
- Risk: <any concern, or "none">

End with a one-line summary: "DRY RUN — N items verified, would edit M files."
"#,
            title = task.title, pr_url = pr_url, items = items_text,
        );
    }

    format!(
        r#"# Apply Approved PR Review Fixes

Task: {title}
PR: {pr_url}

## Approved Items (apply ALL of these — nothing else)
{items}

## Instructions
Implement only the approved items above. Verify each issue exists in the current
code before editing. Keep changes focused and minimal. Do not refactor unrelated
code.

After completing all edits, write a short final summary listing:
- FIXED: which items you implemented, citing the item number.
- SKIPPED: any approved item that no longer applied and why.
"#,
        title = task.title, pr_url = pr_url, items = items_text,
    )
}

fn build_reply_body(item: &PrReviewItem, agent_summary: Option<&str>) -> String {
    let status = match item.decision {
        PrReviewDecision::Fix => "Fixed",
        PrReviewDecision::Skip => "Skipped",
        PrReviewDecision::Question => "Needs discussion",
    };
    let mut body = format!("[SlashIt agent — {}]\n\n", status);
    if !item.summary.is_empty() {
        body.push_str(&item.summary);
        body.push_str("\n\n");
    }
    if !item.reasoning.is_empty() {
        body.push_str(&item.reasoning);
        body.push_str("\n\n");
    }
    if matches!(item.decision, PrReviewDecision::Fix) && !item.proposed_change.is_empty() {
        body.push_str("Change: ");
        body.push_str(&item.proposed_change);
        body.push_str("\n\n");
    }
    if let Some(summary) = agent_summary.map(str::trim).filter(|s| !s.is_empty()) {
        body.push_str("Agent notes:\n");
        body.push_str(summary);
    }
    body.trim().to_string()
}

async fn post_pr_reply(
    repo: &str,
    number: &str,
    pr_url: &str,
    comment_id: Option<u64>,
    body: &str,
) -> Result<(), String> {
    if let Some(id) = comment_id {
        let endpoint = format!("repos/{}/pulls/{}/comments/{}/replies", repo, number, id);
        let body_arg = format!("body={}", body);
        if run_cmd_no_cwd("gh", &["api", "-X", "POST", &endpoint, "-f", &body_arg]).await.is_ok() {
            return Ok(());
        }
        // Inline reply failed (e.g. comment was on a Review, not an inline thread).
        // Fall through to a global PR comment so the reply is not lost.
    }
    run_cmd_no_cwd("gh", &["pr", "comment", pr_url, "--body", body])
        .await
        .map(|_| ())
}

async fn run_claude_pr_helper(prompt: String, working_dir: String, can_edit: bool) -> Result<String, String> {
    let allowed_tools = if can_edit {
        "Read,Edit,Write,Bash,Glob,Grep"
    } else {
        "Read,Glob,Grep"
    };
    let session_id = Uuid::new_v4().to_string();

    eprintln!(
        "[pr-review] spawning claude helper (can_edit={}, prompt_chars={})",
        can_edit, prompt.len(),
    );

    let mut cmd = tokio::process::Command::new("claude");
    cmd.arg("-p").arg(&prompt)
        .arg("--output-format").arg("stream-json")
        .arg("--verbose")
        .arg("--allowedTools").arg(allowed_tools)
        .arg("--dangerously-skip-permissions")
        .arg("--max-turns").arg("30")
        .arg("--session-id").arg(&session_id)
        .arg("--strict-mcp-config")
        .current_dir(&working_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = cmd.output().await
        .map_err(|e| format!("Failed to spawn claude: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_label = output.status.code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".into());
    eprintln!(
        "[pr-review] claude exit={} stdout={}B stderr={}B",
        exit_label, stdout.len(), stderr.len(),
    );

    // Always persist stdout when it's substantial or when claude failed, so the
    // 200 KB transcript that exposes the real error isn't lost. The path is
    // surfaced in the error message and printed to stderr.
    let log_path = if !output.status.success() || stdout.len() > 4096 {
        write_pr_helper_log(&stdout, &stderr, can_edit).ok()
    } else {
        None
    };

    if !output.status.success() {
        let reason = extract_failure_reason(&stdout)
            .or_else(|| {
                let tail: Vec<&str> = stderr.lines().rev().take(5).collect();
                if tail.is_empty() {
                    None
                } else {
                    let mut joined: Vec<&str> = tail.into_iter().collect();
                    joined.reverse();
                    Some(joined.join(" | "))
                }
            })
            .unwrap_or_else(|| {
                "no error event in stream-json and no stderr — see log".to_string()
            });
        let log_hint = log_path
            .as_ref()
            .map(|p| format!(" (transcript: {})", p.display()))
            .unwrap_or_default();
        return Err(format!("claude exited {} — {}{}", exit_label, reason, log_hint));
    }

    let extracted = extract_text_from_stream_json(&stdout);
    if extracted.trim().is_empty() {
        let stderr_tail = stderr.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        eprintln!(
            "[pr-review] claude helper produced empty extracted text (can_edit={}). Stderr tail:\n{}",
            can_edit,
            if stderr_tail.is_empty() { "(empty)" } else { &stderr_tail },
        );
    }
    Ok(extracted)
}

/// Pull a human-readable failure reason out of the stream-json stdout. Prefers
/// the terminal `result` event when its `is_error` flag is set (this is where
/// the Claude CLI reports max-turns, sandbox denials, model errors, etc.).
/// Falls back to the last `error` field on any event, or the last assistant
/// text block before the truncation.
fn extract_failure_reason(stdout: &str) -> Option<String> {
    let mut last_error_text: Option<String> = None;
    let mut last_assistant_text: Option<String> = None;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue; };
        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if msg_type == "result" {
            let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
            let subtype = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
            let text = v.get("result").and_then(|r| r.as_str()).unwrap_or("").trim();
            if is_error || subtype.contains("error") || subtype.contains("max_turns") {
                let label = if subtype.is_empty() { "error".to_string() } else { format!("{}", subtype) };
                let body = if text.is_empty() { "(empty result body)".to_string() } else { text.to_string() };
                return Some(format!("{}: {}", label, truncate_one_line(&body, 400)));
            }
        }
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            last_error_text = Some(err.to_string());
        }
        if msg_type == "assistant" {
            if let Some(content) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                for block in content {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            last_assistant_text = Some(text.to_string());
                        }
                    }
                }
            }
        }
    }
    last_error_text
        .map(|e| format!("stream error: {}", truncate_one_line(&e, 400)))
        .or_else(|| last_assistant_text.map(|t| format!("last assistant text: {}", truncate_one_line(&t, 400))))
}

fn truncate_one_line(s: &str, max: usize) -> String {
    let oneline: String = s.split('\n').filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().join(" / ");
    if oneline.chars().count() <= max {
        oneline
    } else {
        let truncated: String = oneline.chars().take(max).collect();
        format!("{}…", truncated)
    }
}

fn write_pr_helper_log(stdout: &str, stderr: &str, can_edit: bool) -> std::io::Result<std::path::PathBuf> {
    let dir = directories::ProjectDirs::from("com", "barradev", "slashit-app")
        .map(|d| d.data_dir().join("pr-helper-logs"))
        .ok_or_else(|| std::io::Error::other("no ProjectDirs"))?;
    std::fs::create_dir_all(&dir)?;
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let path = dir.join(format!("{}-{}.log", ts, if can_edit { "apply" } else { "readonly" }));
    let body = format!(
        "=== STDOUT ({} bytes) ===\n{}\n=== STDERR ({} bytes) ===\n{}\n",
        stdout.len(), stdout, stderr.len(), stderr,
    );
    std::fs::write(&path, body)?;
    eprintln!("[pr-review] wrote claude transcript to {}", path.display());
    Ok(path)
}

/// Pull the final text/result from a Claude CLI `--output-format stream-json` blob.
/// Prefers the terminal `result` event; falls back to concatenating text blocks
/// from assistant messages.
fn extract_text_from_stream_json(stdout: &str) -> String {
    let mut result_text: Option<String> = None;
    let mut assistant_text = String::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else { continue; };
        let msg_type = json.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match msg_type {
            "result" => {
                if let Some(text) = json.get("result").and_then(|r| r.as_str()) {
                    result_text = Some(text.to_string());
                }
            }
            "assistant" => {
                let Some(content) = json.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) else { continue; };
                for block in content {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            assistant_text.push_str(text);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    result_text.unwrap_or(assistant_text)
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

    // ──────────────────────────────────────────────
    // parse_review_items: JSON parsing, decision normalization, sort
    // ──────────────────────────────────────────────

    fn comment(id: u64) -> PrReviewComment {
        PrReviewComment {
            id: Some(id),
            kind: crate::domain::task::PrCommentKind::Inline,
            author: "reviewer".to_string(),
            body: format!("comment {id}"),
            path: None,
            line: None,
            url: None,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn parse_review_items_preserves_input_order() {
        // Order from the agent (= order from the PR) must be preserved end-to-end.
        let comments = vec![comment(1), comment(2), comment(3)];
        let raw = r#"{"items":[
            {"comment_id":1,"summary":"A","decision":"skip","reasoning":"","proposed_change":""},
            {"comment_id":2,"summary":"B","decision":"fix","reasoning":"","proposed_change":""},
            {"comment_id":3,"summary":"C","decision":"question","reasoning":"","proposed_change":""}
        ]}"#;
        let items = parse_review_items(raw, &comments);
        let summaries: Vec<_> = items.iter().map(|i| i.summary.clone()).collect();
        assert_eq!(summaries, vec!["A", "B", "C"]);
    }

    #[test]
    fn parse_review_items_unknown_decision_falls_back_to_question() {
        let comments = vec![comment(1)];
        let raw = r#"{"items":[
            {"comment_id":1,"summary":"X","decision":"maybe","reasoning":"","proposed_change":""}
        ]}"#;
        let items = parse_review_items(raw, &comments);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].decision, PrReviewDecision::Question);
        assert!(!items[0].approved);
    }

    #[test]
    fn parse_review_items_decision_is_case_insensitive() {
        let comments = vec![comment(1), comment(2)];
        let raw = r#"{"items":[
            {"comment_id":1,"summary":"u","decision":"FIX","reasoning":"","proposed_change":""},
            {"comment_id":2,"summary":"l","decision":"Skip","reasoning":"","proposed_change":""}
        ]}"#;
        let items = parse_review_items(raw, &comments);
        assert_eq!(items.len(), 2);
        // Sorted: Fix before Skip (no Question present).
        assert_eq!(items[0].decision, PrReviewDecision::Fix);
        assert_eq!(items[1].decision, PrReviewDecision::Skip);
    }

    #[test]
    fn parse_review_items_filters_unknown_comment_id() {
        // Item references comment_id=999 which is not in the comments slice.
        let comments = vec![comment(1)];
        let raw = r#"{"items":[
            {"comment_id":999,"summary":"orphan","decision":"fix","reasoning":"","proposed_change":""}
        ]}"#;
        let items = parse_review_items(raw, &comments);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].comment_id, None);
    }

    #[test]
    fn parse_review_items_fix_marks_approved() {
        let comments = vec![comment(1), comment(2), comment(3)];
        let raw = r#"{"items":[
            {"comment_id":1,"summary":"a","decision":"fix","reasoning":"","proposed_change":""},
            {"comment_id":2,"summary":"b","decision":"skip","reasoning":"","proposed_change":""},
            {"comment_id":3,"summary":"c","decision":"question","reasoning":"","proposed_change":""}
        ]}"#;
        let items = parse_review_items(raw, &comments);
        for item in &items {
            let expected = matches!(item.decision, PrReviewDecision::Fix);
            assert_eq!(item.approved, expected, "approved should mirror Fix decision");
        }
    }

    #[test]
    fn parse_review_items_extracts_json_from_surrounding_prose() {
        // Agent sometimes prefixes with chatter; parser should locate the JSON
        // between the first '{' and last '}'.
        let comments = vec![comment(1)];
        let raw = "Here is my analysis:\n```json\n{\"items\":[{\"comment_id\":1,\"summary\":\"x\",\"decision\":\"fix\",\"reasoning\":\"\",\"proposed_change\":\"\"}]}\n```\nDone.";
        let items = parse_review_items(raw, &comments);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].summary, "x");
    }

    #[test]
    fn parse_review_items_empty_input_returns_empty() {
        assert!(parse_review_items("", &[]).is_empty());
        assert!(parse_review_items("   \n\t", &[]).is_empty());
    }

    #[test]
    fn parse_review_items_no_braces_returns_empty() {
        assert!(parse_review_items("just prose, no json", &[]).is_empty());
    }

    #[test]
    fn parse_review_items_malformed_json_returns_empty() {
        let raw = r#"{"items": [not valid"#;
        assert!(parse_review_items(raw, &[]).is_empty());
    }

    #[test]
    fn parse_review_items_user_note_starts_empty() {
        // user_note is filled by the user in the UI, never by the agent.
        let comments = vec![comment(1)];
        let raw = r#"{"items":[
            {"comment_id":1,"summary":"x","decision":"question","reasoning":"","proposed_change":""}
        ]}"#;
        let items = parse_review_items(raw, &comments);
        assert_eq!(items[0].user_note, "");
    }

    // ──────────────────────────────────────────────
    // extract_text_from_stream_json: result/assistant precedence
    // ──────────────────────────────────────────────

    #[test]
    fn extract_text_prefers_terminal_result_over_assistant() {
        let stream = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"intermediate"}]}}
{"type":"result","result":"final"}"#;
        assert_eq!(extract_text_from_stream_json(stream), "final");
    }

    #[test]
    fn extract_text_falls_back_to_assistant_when_no_result() {
        let stream = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello "}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"world"}]}}"#;
        assert_eq!(extract_text_from_stream_json(stream), "hello world");
    }

    #[test]
    fn extract_text_concatenates_multiple_text_blocks_in_one_message() {
        let stream = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#;
        assert_eq!(extract_text_from_stream_json(stream), "ab");
    }

    #[test]
    fn extract_text_ignores_non_text_content_blocks() {
        // tool_use blocks should not contribute to the captured text.
        let stream = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{}},{"type":"text","text":"only this"}]}}"#;
        assert_eq!(extract_text_from_stream_json(stream), "only this");
    }

    #[test]
    fn extract_text_skips_blank_and_invalid_lines() {
        let stream = "\n\nnot json\n{\"type\":\"result\",\"result\":\"ok\"}\n   \n";
        assert_eq!(extract_text_from_stream_json(stream), "ok");
    }

    #[test]
    fn extract_text_empty_stream_returns_empty() {
        assert_eq!(extract_text_from_stream_json(""), "");
    }

    #[test]
    fn extract_text_unknown_event_types_are_ignored() {
        let stream = r#"{"type":"system","subtype":"init"}
{"type":"user","message":{"content":[]}}
{"type":"result","result":"done"}"#;
        assert_eq!(extract_text_from_stream_json(stream), "done");
    }
}
