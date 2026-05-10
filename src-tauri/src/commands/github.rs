use crate::domain::github::{GithubIssue, PullRequest, Label, Assignee, Review};
use crate::domain::{Task, TaskStatus, TaskCategory, TaskPriority, TaskComplexity, TaskImpact, SecuritySeverity, TaskPhase};
use crate::domain::task::ExternalRef;
use tokio::process::Command;
use uuid::Uuid;

#[derive(Clone)]
pub struct GithubState;

impl GithubState {
    pub fn new() -> Self { Self }
}

impl Default for GithubState {
    fn default() -> Self { Self::new() }
}

/// Check if `gh` CLI is available and authenticated.
async fn check_gh() -> Result<(), String> {
    let output = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .await
        .map_err(|e| format!("gh CLI not found: {}. Install with: sudo pacman -S github-cli", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh not authenticated: {}. Run: gh auth login", stderr));
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
        "issue", "list",
        "--repo", &repo,
        "--json", "number,title,body,state,labels,assignees,comments,createdAt,updatedAt",
        "--limit", "50",
    ]).await?;

    let issues: Vec<GithubIssue> = json.as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|v| {
            Some(GithubIssue {
                number: v.get("number")?.as_i64()? as i32,
                title: v.get("title")?.as_str()?.to_string(),
                body: v.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string(),
                state: v.get("state").and_then(|s| s.as_str()).unwrap_or("OPEN").to_string(),
                labels: v.get("labels").and_then(|l| l.as_array()).map(|arr| {
                    arr.iter().filter_map(|lbl| {
                        Some(Label {
                            name: lbl.get("name")?.as_str()?.to_string(),
                            color: lbl.get("color").and_then(|c| c.as_str()).unwrap_or("").to_string(),
                        })
                    }).collect()
                }).unwrap_or_default(),
                assignees: v.get("assignees").and_then(|a| a.as_array()).map(|arr| {
                    arr.iter().filter_map(|asn| {
                        Some(Assignee {
                            login: asn.get("login")?.as_str()?.to_string(),
                            avatar_url: asn.get("avatarUrl").and_then(|u| u.as_str()).map(|s| s.to_string()),
                        })
                    }).collect()
                }).unwrap_or_default(),
                comments: v.get("comments").and_then(|c| c.as_i64()).unwrap_or(0) as i32,
                created_at: v.get("createdAt").and_then(|d| d.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(chrono::Utc::now),
                updated_at: v.get("updatedAt").and_then(|d| d.as_str())
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
        "issue", "view",
        &number.to_string(),
        "--repo", &repo,
        "--json", "number,title,body,state,labels,assignees,comments,createdAt,updatedAt",
    ]).await?;

    Ok(GithubIssue {
        number: json.get("number").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
        title: json.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        body: json.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string(),
        state: json.get("state").and_then(|s| s.as_str()).unwrap_or("OPEN").to_string(),
        labels: Vec::new(),
        assignees: Vec::new(),
        comments: json.get("comments").and_then(|c| c.as_i64()).unwrap_or(0) as i32,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    })
}

#[tauri::command]
pub async fn create_task_from_issue(
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
        id: Uuid::new_v4(),
        project_id,
        title: issue.title,
        description: Some(issue.body),
        status: TaskStatus::Backlog,
        model: "default".to_string(),
        planning_mode: false,
        dependencies: Vec::new(),
        workspace_id: None,
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
        github_issue_url: Some(format!("https://github.com/{}/issues/{}", repo, issue_number)),
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
        human_review: None,
        stuck_since: None,
        error_message: None,
        worktree_path: None,
        branch_name: None,
        pr_review_plan: None,
        created_at: now,
        updated_at: now,
    };

    // Store in task state
    state.task.tasks.write().await.insert(task.id, task.clone());

    // Persist
    let tasks_r = state.task.tasks.read().await;
    let project_tasks: Vec<Task> = tasks_r.values()
        .filter(|t| t.project_id == project_id)
        .cloned()
        .collect();
    let _ = state.storage.save_project_tasks(project_id, &project_tasks);

    Ok(task)
}

#[tauri::command]
pub async fn import_github_issues(
    state: tauri::State<'_, crate::AppState>,
    repo: String,
    project_id: String,
    filter: Option<String>,
) -> Result<Vec<Task>, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    check_gh().await?;

    let mut args = vec![
        "issue", "list",
        "--repo", &repo,
        "--json", "number,title,body,state,labels,assignees,comments,createdAt,updatedAt",
        "--limit", "100",
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
    let issues: Vec<GithubIssue> = serde_json::from_value(json)
        .map_err(|e| format!("Parse error: {}", e))?;

    let now = chrono::Utc::now();
    let mut imported = Vec::new();

    for issue in issues {
        let category = map_labels_to_category(&issue.labels);
        let priority = map_labels_to_priority(&issue.labels);

        let task = Task {
            id: Uuid::new_v4(),
            project_id,
            title: issue.title,
            description: Some(issue.body),
            status: TaskStatus::Backlog,
            model: "default".to_string(),
            planning_mode: false,
            dependencies: Vec::new(),
            workspace_id: None,
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
            github_issue_url: Some(format!("https://github.com/{}/issues/{}", repo, issue.number)),
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
            human_review: None,
            stuck_since: None,
            error_message: None,
            worktree_path: None,
            branch_name: None,
            pr_review_plan: None,
            created_at: now,
            updated_at: now,
        };

        state.task.tasks.write().await.insert(task.id, task.clone());
        imported.push(task);
    }

    // Persist all
    let tasks_r = state.task.tasks.read().await;
    let project_tasks: Vec<Task> = tasks_r.values()
        .filter(|t| t.project_id == project_id)
        .cloned()
        .collect();
    let _ = state.storage.save_project_tasks(project_id, &project_tasks);

    Ok(imported)
}

#[tauri::command]
pub async fn get_prs(repo: String) -> Result<Vec<PullRequest>, String> {
    check_gh().await?;

    let json = run_gh(&[
        "pr", "list",
        "--repo", &repo,
        "--json", "number,title,body,state,author,createdAt,updatedAt,reviews,additions,deletions",
        "--limit", "50",
    ]).await?;

    let prs: Vec<PullRequest> = json.as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|v| {
            Some(PullRequest {
                number: v.get("number")?.as_i64()? as i32,
                title: v.get("title")?.as_str()?.to_string(),
                body: v.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string(),
                state: v.get("state").and_then(|s| s.as_str()).unwrap_or("OPEN").to_string(),
                author: v.get("author").and_then(|a| a.get("login")).and_then(|l| l.as_str()).unwrap_or("").to_string(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                reviews: v.get("reviews").and_then(|r| r.as_array()).map(|arr| {
                    arr.iter().filter_map(|rev| {
                        Some(Review {
                            user: rev.get("author").and_then(|a| a.get("login")).and_then(|l| l.as_str())?.to_string(),
                            state: rev.get("state").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                            submitted_at: None,
                        })
                    }).collect()
                }).unwrap_or_default(),
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
        "pr", "view",
        &number.to_string(),
        "--repo", &repo,
        "--json", "number,title,body,state,author,createdAt,updatedAt,reviews,additions,deletions",
    ]).await?;

    Ok(PullRequest {
        number: json.get("number").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
        title: json.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        body: json.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string(),
        state: json.get("state").and_then(|s| s.as_str()).unwrap_or("OPEN").to_string(),
        author: json.get("author").and_then(|a| a.get("login")).and_then(|l| l.as_str()).unwrap_or("").to_string(),
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
        if name.contains("bug") || name.contains("fix") { return TaskCategory::BugFix; }
        if name.contains("feature") || name.contains("enhancement") { return TaskCategory::Feature; }
        if name.contains("doc") { return TaskCategory::Documentation; }
        if name.contains("security") { return TaskCategory::Security; }
        if name.contains("perf") { return TaskCategory::Performance; }
        if name.contains("ui") || name.contains("ux") { return TaskCategory::UiUx; }
        if name.contains("test") { return TaskCategory::Testing; }
        if name.contains("infra") || name.contains("ci") { return TaskCategory::Infrastructure; }
        if name.contains("refactor") { return TaskCategory::Refactoring; }
    }
    TaskCategory::Feature
}

fn map_labels_to_priority(labels: &[Label]) -> TaskPriority {
    for label in labels {
        let name = label.name.to_lowercase();
        if name.contains("critical") || name.contains("urgent") || name.contains("p0") { return TaskPriority::Urgent; }
        if name.contains("high") || name.contains("p1") { return TaskPriority::High; }
        if name.contains("low") || name.contains("p3") { return TaskPriority::Low; }
    }
    TaskPriority::Medium
}
