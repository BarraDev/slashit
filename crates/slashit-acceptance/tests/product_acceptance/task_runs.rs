//! What the stacked-task and checkout-recovery journeys share: several tasks
//! in one project, each carried to Human Review by the real executor, and
//! reading git for what the product left behind.
//!
//! Tasks are created through the product's own command surface, as every
//! journey does (the dialogs are covered elsewhere), and started the way the
//! board starts one: a move into In Progress that the queue then notices.

use super::*;

pub(super) use super::git_support::{git_commit_exists, git_is_ancestor, git_out};

/// Register an existing fixture repository and a project on it.
pub(super) async fn register_project(driver: &WebDriver, repository: &GitFixture) -> Result<String> {
    ui::assert_frontend_is_real(driver).await?;
    let repository_id = created_id(
        ui::invoke(
            driver,
            "create_repository",
            json!({ "localPath": repository.path(), "remoteUrl": Value::Null, "initialize": Value::Null }),
        )
        .await?,
        "create_repository",
    )?;
    created_id(
        ui::invoke(
            driver,
            "create_project",
            json!({ "name": "Acceptance Product Journey", "repositoryId": repository_id, "agentType": "claude_code" }),
        )
        .await?,
        "create_project",
    )
}

/// Create a backlog task, optionally depending on other tasks, and return
/// its id and its title.
pub(super) async fn create_task(
    driver: &WebDriver,
    project_id: &str,
    title_prefix: &str,
    dependencies: &[&str],
) -> Result<(String, String)> {
    let title = format!(
        "{title_prefix} {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let task = ui::invoke(
        driver,
        "create_task",
        json!({
            "params": {
                "projectId": project_id,
                "title": title,
                "description": "Exercises stacking and checkout recovery.",
                "model": "default",
                "planningMode": false,
                "dependencies": dependencies,
                "category": Value::Null,
                "priority": Value::Null,
                "complexity": Value::Null,
                "impact": Value::Null,
                "securitySeverity": Value::Null,
                "githubIssueUrl": Value::Null,
                "gitlabIssueUrl": Value::Null,
                "linearTicketId": Value::Null,
            }
        }),
    )
    .await?;
    if status_of(&task) != Some("backlog") {
        bail!("a new task should start in the backlog, got {task}");
    }
    Ok((created_id(task, "create_task")?, title))
}

/// Move a backlog task into In Progress and wait for the executor to carry it
/// to Human Review, with a Task Checkout recorded.
pub(super) async fn run_to_review(
    driver: &WebDriver,
    project_id: &str,
    task_id: &str,
    title: &str,
) -> Result<ExecutedTask> {
    ui::invoke(driver, "update_task_status", json!({ "taskId": task_id, "status": "in_progress" })).await?;
    let settled = await_status(driver, project_id, task_id, &["human_review", "error"]).await?;
    if status_of(&settled) == Some("error") {
        bail!(
            "{title} failed instead of reaching Human Review: {}",
            settled.get("error_message").and_then(Value::as_str).unwrap_or("no error message recorded")
        );
    }
    let worktree = settled
        .get("worktree_path")
        .and_then(Value::as_str)
        .with_context(|| format!("{title} reached Human Review with no Task Checkout recorded"))?;
    Ok(ExecutedTask {
        id: task_id.to_string(),
        project_id: project_id.to_string(),
        title: title.to_string(),
        worktree_path: PathBuf::from(worktree),
    })
}

/// A task's branch, from the product.
pub(super) fn branch_name_of(task: &Value) -> Result<String> {
    Ok(task
        .get("branch_name")
        .and_then(Value::as_str)
        .context("the task records no branch")?
        .to_string())
}
