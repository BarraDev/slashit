use crate::domain::jira::{JiraIssue, JiraProject};
use crate::domain::{Task, TaskStatus, TaskCategory, TaskPriority, TaskComplexity, TaskImpact, SecuritySeverity, TaskPhase};
use crate::domain::task::ExternalRef;
use tokio::process::Command;
use uuid::Uuid;

/// Check if `acli` (Atlassian CLI) is available.
#[tauri::command]
pub async fn check_acli_available() -> Result<bool, String> {
    let output = Command::new("acli")
        .args(["--version"])
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => Ok(true),
        Ok(_) => Ok(false),
        Err(_) => Ok(false),
    }
}

/// Run an acli command and return raw JSON output.
async fn run_acli(args: &[&str]) -> Result<serde_json::Value, String> {
    let output = Command::new("acli")
        .args(args)
        .output()
        .await
        .map_err(|e| format!("Failed to run acli: {}. Install from: https://bobswift.atlassian.net/wiki/spaces/ACLI", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("acli command failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).map_err(|e| format!("Failed to parse acli output: {}", e))
}

/// List available Jira projects.
#[tauri::command]
pub async fn list_jira_projects() -> Result<Vec<JiraProject>, String> {
    let json = run_acli(&[
        "jira", "--action", "getProjectList", "--outputFormat", "json",
    ]).await?;

    let projects: Vec<JiraProject> = json.as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|v| {
            Some(JiraProject {
                key: v.get("key")?.as_str()?.to_string(),
                name: v.get("name")?.as_str()?.to_string(),
                lead: v.get("lead").and_then(|l| l.as_str()).map(|s| s.to_string()),
            })
        })
        .collect();

    Ok(projects)
}

/// Import Jira issues as tasks.
#[tauri::command]
pub async fn import_jira_issues(
    state: tauri::State<'_, crate::AppState>,
    project_key: String,
    jql_filter: Option<String>,
    target_project_id: String,
) -> Result<Vec<Task>, String> {
    let project_id = Uuid::parse_str(&target_project_id).map_err(|e| e.to_string())?;

    let jql = jql_filter.unwrap_or_else(|| format!("project = {} AND status != Done ORDER BY created DESC", project_key));

    let json = run_acli(&[
        "jira", "--action", "getIssueList",
        "--jql", &jql,
        "--outputFormat", "json",
        "--limit", "100",
    ]).await?;

    let issues: Vec<JiraIssue> = json.as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|v| {
            Some(JiraIssue {
                key: v.get("key")?.as_str()?.to_string(),
                summary: v.get("summary").or(v.get("fields").and_then(|f| f.get("summary"))).and_then(|s| s.as_str())?.to_string(),
                description: v.get("description").or(v.get("fields").and_then(|f| f.get("description"))).and_then(|d| d.as_str()).map(|s| s.to_string()),
                status: v.get("status").and_then(|s| s.as_str()).unwrap_or("To Do").to_string(),
                issue_type: v.get("issuetype").or(v.get("type")).and_then(|t| t.as_str()).unwrap_or("Task").to_string(),
                priority: v.get("priority").and_then(|p| p.as_str()).map(|s| s.to_string()),
                assignee: v.get("assignee").and_then(|a| a.as_str()).map(|s| s.to_string()),
                reporter: v.get("reporter").and_then(|r| r.as_str()).map(|s| s.to_string()),
                labels: v.get("labels").and_then(|l| l.as_array()).map(|arr| {
                    arr.iter().filter_map(|l| l.as_str().map(|s| s.to_string())).collect()
                }).unwrap_or_default(),
                story_points: v.get("storyPoints").and_then(|sp| sp.as_f64()),
                created: chrono::Utc::now(),
                updated: chrono::Utc::now(),
            })
        })
        .collect();

    let now = chrono::Utc::now();
    let mut imported = Vec::new();

    for issue in issues {
        let category = map_jira_type_to_category(&issue.issue_type);
        let priority = map_jira_priority(issue.priority.as_deref());
        let complexity = map_story_points_to_complexity(issue.story_points);

        let task = Task {
            id: Uuid::new_v4(),
            project_id,
            title: format!("[{}] {}", issue.key, issue.summary),
            description: issue.description,
            status: TaskStatus::Backlog,
            model: "default".to_string(),
            planning_mode: false,
            dependencies: Vec::new(),
            workspace_id: None,
            jj_change_id: None,
            category,
            priority,
            complexity,
            impact: TaskImpact::Medium,
            security_severity: SecuritySeverity::None,
            phase: TaskPhase::Idle,
            phase_progress: 0,
            overall_progress: 0,
            subtasks: Vec::new(),
            sequence_number: 0,
            position: imported.len() as i32,
            github_issue_url: None,
            gitlab_issue_url: None,
            linear_ticket_id: None,
            jira_issue_key: Some(issue.key.clone()),
            pr_url: None,
            external_refs: vec![ExternalRef::JiraTicket {
                project: issue.key.split('-').next().unwrap_or("").to_string(),
                key: issue.key,
            }],
            qa_signoff: None,
            human_review: None,
            stuck_since: None,
            error_message: None,
            worktree_path: None,
            branch_name: None,
            created_at: now,
            updated_at: now,
        };

        state.task.tasks.write().await.insert(task.id, task.clone());
        imported.push(task);
    }

    // Persist
    let tasks_r = state.task.tasks.read().await;
    let project_tasks: Vec<Task> = tasks_r.values()
        .filter(|t| t.project_id == project_id)
        .cloned()
        .collect();
    let _ = state.storage.save_project_tasks(project_id, &project_tasks);

    Ok(imported)
}

fn map_jira_type_to_category(issue_type: &str) -> TaskCategory {
    match issue_type.to_lowercase().as_str() {
        "bug" => TaskCategory::BugFix,
        "story" | "feature" => TaskCategory::Feature,
        "improvement" | "enhancement" => TaskCategory::Refactoring,
        "documentation" => TaskCategory::Documentation,
        "security" => TaskCategory::Security,
        _ => TaskCategory::Feature,
    }
}

fn map_jira_priority(priority: Option<&str>) -> TaskPriority {
    match priority.map(|p| p.to_lowercase()).as_deref() {
        Some("blocker") | Some("critical") | Some("highest") => TaskPriority::Urgent,
        Some("high") => TaskPriority::High,
        Some("low") | Some("lowest") => TaskPriority::Low,
        _ => TaskPriority::Medium,
    }
}

fn map_story_points_to_complexity(points: Option<f64>) -> TaskComplexity {
    match points {
        Some(p) if p <= 2.0 => TaskComplexity::Minimal,
        Some(p) if p <= 5.0 => TaskComplexity::Moderate,
        Some(p) if p <= 8.0 => TaskComplexity::Complex,
        Some(_) => TaskComplexity::Advanced,
        None => TaskComplexity::Moderate,
    }
}
