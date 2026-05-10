use slashit_ipc::*;
use uuid::Uuid;

use crate::domain::{AgentStatus, TaskPriority, TaskStatus};

use super::server::IpcContext;

pub async fn dispatch(req: IpcRequest, ctx: &IpcContext) -> IpcResponse {
    match req {
        IpcRequest::Status => handle_status(ctx).await,
        IpcRequest::ListProjects => handle_list_projects(ctx).await,
        IpcRequest::ListTasks { project_id } => handle_list_tasks(ctx, project_id).await,
        IpcRequest::CreateTask {
            project_id,
            title,
            description,
            priority,
        } => handle_create_task(ctx, project_id, title, description, priority).await,
        IpcRequest::MoveTask { task_id, status } => handle_move_task(ctx, task_id, status).await,
        IpcRequest::EditTask {
            task_id,
            title,
            description,
            priority,
        } => handle_edit_task(ctx, task_id, title, description, priority).await,
        IpcRequest::DeleteTask { task_id } => handle_delete_task(ctx, task_id).await,
        IpcRequest::QueueStatus => handle_queue_status(ctx).await,
        IpcRequest::EnqueueTask { task_id } => handle_enqueue_task(ctx, task_id).await,
        IpcRequest::ListTerminals => handle_list_terminals(ctx).await,
        IpcRequest::Show => handle_show(ctx),
        IpcRequest::Quit => handle_quit(ctx),
    }
}

async fn handle_status(ctx: &IpcContext) -> IpcResponse {
    let active_terminals = ctx.pty.sessions.lock().await.len();

    let running_agents = {
        let execs = ctx.executions.read().await;
        execs
            .values()
            .filter(|e| matches!(e.status, AgentStatus::Running | AgentStatus::Starting))
            .count()
    };

    let queue_mgr = ctx.queue_manager.read().await;
    let queued_tasks = queue_mgr.get_queued_tasks().await.len();
    let in_progress_tasks = queue_mgr.get_in_progress_count().await;

    let status = AppStatus {
        active_terminals,
        running_agents,
        queued_tasks,
        in_progress_tasks,
    };

    IpcResponse::success(serde_json::to_value(status).unwrap_or_default())
}

async fn handle_list_projects(ctx: &IpcContext) -> IpcResponse {
    let projects = ctx.projects.read().await;
    let summaries: Vec<ProjectSummary> = projects
        .values()
        .map(|p| ProjectSummary {
            id: p.id.to_string(),
            name: p.name.clone(),
            path: None,
        })
        .collect();

    IpcResponse::success(serde_json::to_value(summaries).unwrap_or_default())
}

async fn handle_list_tasks(ctx: &IpcContext, project_id: Option<String>) -> IpcResponse {
    let filter_id = project_id.as_deref().and_then(|s| Uuid::parse_str(s).ok());

    let tasks = ctx.tasks.read().await;
    let projects = ctx.projects.read().await;
    let summaries: Vec<TaskSummary> = tasks
        .values()
        .filter(|t| match filter_id {
            Some(pid) => t.project_id == pid,
            None => true,
        })
        .map(|t| {
            let name = projects.get(&t.project_id).map(|p| p.name.as_str()).unwrap_or("?");
            task_to_summary(t, name)
        })
        .collect();

    IpcResponse::success(serde_json::to_value(summaries).unwrap_or_default())
}

async fn handle_create_task(
    ctx: &IpcContext,
    project_id: String,
    title: String,
    description: Option<String>,
    priority: Option<String>,
) -> IpcResponse {
    let project_uuid = match Uuid::parse_str(&project_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid project_id: {}", project_id)),
    };

    // Verify project exists and get its name
    let project_name = {
        let projects = ctx.projects.read().await;
        match projects.get(&project_uuid) {
            Some(p) => p.name.clone(),
            None => return IpcResponse::error(format!("Project {} not found", project_id)),
        }
    };

    let priority = parse_priority(priority.as_deref());
    let now = chrono::Utc::now();
    let task = crate::domain::Task {
        id: Uuid::new_v4(),
        project_id: project_uuid,
        title,
        description,
        status: TaskStatus::Backlog,
        model: "default".to_string(),
        planning_mode: false,
        dependencies: Vec::new(),
        workspace_id: None,
        jj_change_id: None,
        category: Default::default(),
        priority,
        complexity: Default::default(),
        impact: Default::default(),
        security_severity: Default::default(),
        phase: Default::default(),
        phase_progress: 0,
        overall_progress: 0,
        subtasks: Vec::new(),
        sequence_number: 0,
        position: 0,
        github_issue_url: None,
        gitlab_issue_url: None,
        linear_ticket_id: None,
        jira_issue_key: None,
        pr_url: None,
        external_refs: Vec::new(),
        qa_signoff: None,
        human_review: None,
        stuck_since: None,
        error_message: None,
        worktree_path: None,
        branch_name: None,
        created_at: now,
        updated_at: now,
    };

    let summary = task_to_summary(&task, &project_name);

    {
        let mut tasks = ctx.tasks.write().await;
        tasks.insert(task.id, task);
        persist_project_tasks(&tasks, project_uuid, &ctx.storage);
    }

    IpcResponse::success(serde_json::to_value(summary).unwrap_or_default())
}

async fn handle_move_task(ctx: &IpcContext, task_id: String, status: String) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {}", task_id)),
    };

    let new_status = match parse_status(&status) {
        Some(s) => s,
        None => return IpcResponse::error(format!("Invalid status: {}", status)),
    };

    let projects = ctx.projects.read().await;
    let mut tasks = ctx.tasks.write().await;
    if let Some(task) = tasks.get_mut(&task_uuid) {
        task.status = new_status;
        task.updated_at = chrono::Utc::now();
        let project_id = task.project_id;
        let pname = projects.get(&project_id).map(|p| p.name.as_str()).unwrap_or("?");
        let summary = task_to_summary(task, pname);
        persist_project_tasks(&tasks, project_id, &ctx.storage);
        IpcResponse::success(serde_json::to_value(summary).unwrap_or_default())
    } else {
        IpcResponse::error(format!("Task {} not found", task_id))
    }
}

async fn handle_edit_task(
    ctx: &IpcContext,
    task_id: String,
    title: Option<String>,
    description: Option<String>,
    priority: Option<String>,
) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {}", task_id)),
    };

    let projects = ctx.projects.read().await;
    let mut tasks = ctx.tasks.write().await;
    if let Some(task) = tasks.get_mut(&task_uuid) {
        if let Some(t) = title {
            task.title = t;
        }
        if let Some(d) = description {
            task.description = Some(d);
        }
        if let Some(p) = priority {
            task.priority = parse_priority(Some(&p));
        }
        task.updated_at = chrono::Utc::now();
        let project_id = task.project_id;
        let pname = projects.get(&project_id).map(|p| p.name.as_str()).unwrap_or("?");
        let summary = task_to_summary(task, pname);
        persist_project_tasks(&tasks, project_id, &ctx.storage);
        IpcResponse::success(serde_json::to_value(summary).unwrap_or_default())
    } else {
        IpcResponse::error(format!("Task {} not found", task_id))
    }
}

async fn handle_delete_task(ctx: &IpcContext, task_id: String) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {}", task_id)),
    };

    let mut tasks = ctx.tasks.write().await;
    if let Some(task) = tasks.remove(&task_uuid) {
        let project_id = task.project_id;
        persist_project_tasks(&tasks, project_id, &ctx.storage);
        IpcResponse::success(serde_json::json!({"deleted": task_id}))
    } else {
        IpcResponse::error(format!("Task {} not found", task_id))
    }
}

async fn handle_queue_status(ctx: &IpcContext) -> IpcResponse {
    let queue_mgr = ctx.queue_manager.read().await;
    let queued_count = queue_mgr.get_queued_tasks().await.len();
    let in_progress_count = queue_mgr.get_in_progress_count().await;
    let config = queue_mgr.config();

    let info = QueueStatusInfo {
        queued_count,
        in_progress_count,
        parallel_limit: config.parallel_task_limit,
        auto_promote: config.auto_promote,
        fifo_ordering: config.fifo_ordering,
    };

    IpcResponse::success(serde_json::to_value(info).unwrap_or_default())
}

async fn handle_enqueue_task(ctx: &IpcContext, task_id: String) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {}", task_id)),
    };

    let queue_mgr = ctx.queue_manager.read().await;
    match queue_mgr.enqueue_task(task_uuid).await {
        Ok(()) => {
            // Persist the task status change
            let tasks = ctx.tasks.read().await;
            if let Some(task) = tasks.get(&task_uuid) {
                persist_project_tasks(&tasks, task.project_id, &ctx.storage);
            }
            IpcResponse::success(serde_json::json!({"enqueued": task_id}))
        }
        Err(e) => IpcResponse::error(e),
    }
}

async fn handle_list_terminals(ctx: &IpcContext) -> IpcResponse {
    let sessions = ctx.pty.sessions.lock().await;
    let summaries: Vec<TerminalSummary> = sessions
        .values()
        .map(|s| TerminalSummary {
            id: s.id.to_string(),
            name: s.name.clone(),
            cols: s.cols as usize,
            rows: s.rows as usize,
        })
        .collect();

    IpcResponse::success(serde_json::to_value(summaries).unwrap_or_default())
}

fn handle_show(ctx: &IpcContext) -> IpcResponse {
    use tauri::Manager;
    if let Some(window) = ctx.app_handle.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        IpcResponse::success(serde_json::json!({"shown": true}))
    } else {
        IpcResponse::error("Main window not found")
    }
}

fn handle_quit(ctx: &IpcContext) -> IpcResponse {
    use tauri::Emitter;
    let _ = ctx.app_handle.emit("quit-requested", ());
    IpcResponse::success(serde_json::json!({"quit": true}))
}

// --- Helper functions ---

fn task_to_summary(task: &crate::domain::Task, project_name: &str) -> TaskSummary {
    TaskSummary {
        id: task.id.to_string(),
        project_id: task.project_id.to_string(),
        project_name: project_name.to_string(),
        title: task.title.clone(),
        status: format!("{:?}", task.status),
        priority: format!("{:?}", task.priority),
        phase: format!("{:?}", task.phase),
        overall_progress: task.overall_progress,
        created_at: task.created_at.to_rfc3339(),
    }
}

fn parse_priority(s: Option<&str>) -> TaskPriority {
    match s {
        Some("urgent") => TaskPriority::Urgent,
        Some("high") => TaskPriority::High,
        Some("medium") => TaskPriority::Medium,
        Some("low") => TaskPriority::Low,
        _ => TaskPriority::Medium,
    }
}

fn parse_status(s: &str) -> Option<TaskStatus> {
    match s {
        "backlog" => Some(TaskStatus::Backlog),
        "queue" => Some(TaskStatus::Queue),
        "in_progress" => Some(TaskStatus::InProgress),
        "ai_review" => Some(TaskStatus::AiReview),
        "human_review" => Some(TaskStatus::HumanReview),
        "done" => Some(TaskStatus::Done),
        "pr_created" => Some(TaskStatus::PrCreated),
        "error" => Some(TaskStatus::Error),
        _ => None,
    }
}

fn persist_project_tasks(
    tasks: &std::collections::HashMap<Uuid, crate::domain::Task>,
    project_id: Uuid,
    storage: &crate::config::Storage,
) {
    let project_tasks: Vec<_> = tasks
        .values()
        .filter(|t| t.project_id == project_id)
        .cloned()
        .collect();
    if let Err(e) = storage.save_project_tasks(project_id, &project_tasks) {
        eprintln!("Warning: Failed to persist tasks: {}", e);
    }
}
