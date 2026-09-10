//! One function per command.
//!
//! The handlers know nothing about transports, credentials or windows. Access
//! control has already happened by the time [`dispatch`] is called (see
//! [`super::server`]), and the two operations that differ between the desktop
//! app and the daemon — showing a window, quitting — go through
//! [`crate::instance::InstanceControl`]. That is why this module names no Tauri
//! type at all: it is the same code in both processes.

use slashit_ipc::{
    AppStatus, InstanceInfo, IpcRequest, IpcResponse, ProjectSummary, QueueStatusInfo, TaskSummary,
    TerminalSummary, PROTOCOL_VERSION,
};
use uuid::Uuid;

use crate::domain::{AgentStatus, TaskPriority, TaskStatus};

use super::server::{IpcContext, PeerContext};

/// The outcome of one request.
///
/// A plain [`IpcResponse`] is not quite enough, because `Quit` has to happen
/// *after* its own reply has been written: asking a daemon to stop trips the
/// shutdown signal, and a client that asked politely should not be answered
/// with a closed connection.
pub struct Dispatch {
    pub response: IpcResponse,

    /// The instance was asked to stop. The server calls
    /// [`crate::instance::InstanceControl::request_quit`] once the response is
    /// on the wire.
    pub then_quit: bool,
}

impl Dispatch {
    /// An ordinary answer, with nothing deferred.
    pub fn reply(response: IpcResponse) -> Self {
        Self {
            response,
            then_quit: false,
        }
    }
}

/// Run one already-authorized request.
///
/// `peer` is carried this far only so `Ping` can report the endpoint the caller
/// actually reached; no handler makes an access-control decision of its own,
/// because a rule that lived in two places would eventually be applied in one.
pub async fn dispatch(req: IpcRequest, ctx: &IpcContext, peer: &PeerContext) -> Dispatch {
    match req {
        IpcRequest::Status => Dispatch::reply(handle_status(ctx).await),
        IpcRequest::ListProjects => Dispatch::reply(handle_list_projects(ctx).await),
        IpcRequest::ListTasks { project_id } => {
            Dispatch::reply(handle_list_tasks(ctx, project_id).await)
        }
        IpcRequest::CreateTask {
            project_id,
            title,
            description,
            priority,
        } => Dispatch::reply(handle_create_task(ctx, project_id, title, description, priority).await),
        IpcRequest::MoveTask { task_id, status } => {
            Dispatch::reply(handle_move_task(ctx, task_id, status).await)
        }
        IpcRequest::EditTask {
            task_id,
            title,
            description,
            priority,
        } => Dispatch::reply(handle_edit_task(ctx, task_id, title, description, priority).await),
        IpcRequest::DeleteTask { task_id } => {
            Dispatch::reply(handle_delete_task(ctx, task_id).await)
        }
        IpcRequest::QueueStatus => Dispatch::reply(handle_queue_status(ctx).await),
        IpcRequest::EnqueueTask { task_id } => {
            Dispatch::reply(handle_enqueue_task(ctx, task_id).await)
        }
        IpcRequest::ListTerminals => Dispatch::reply(handle_list_terminals(ctx).await),
        IpcRequest::Show => Dispatch::reply(handle_show(ctx)),
        IpcRequest::Quit => handle_quit(),
        IpcRequest::Features => Dispatch::reply(handle_features(ctx).await),
        IpcRequest::Ping => Dispatch::reply(handle_ping(ctx, peer)),
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
            let name = projects
                .get(&t.project_id)
                .map(|p| p.name.as_str())
                .unwrap_or("?");
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
        Err(_) => return IpcResponse::error(format!("Invalid project_id: {project_id}")),
    };

    // Verify project exists and get its name
    let project_name = {
        let projects = ctx.projects.read().await;
        match projects.get(&project_uuid) {
            Some(p) => p.name.clone(),
            None => return IpcResponse::error(format!("Project {project_id} not found")),
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
        worktree_id: None,
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
        cleanup_in_flight: false,
        pr_review_plan: None,
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

/// Move a task to another column from the CLI.
///
/// A move into `Done` is the same terminal claim the desktop app makes, and
/// takes the same route: [`crate::lifecycle::terminalize`] removes the worktree
/// first and commits the status only if that succeeded. This handler used to
/// assign the status directly and had no worktree manager at all, so
/// `slashit move <task> done` finished tasks on the board and left their
/// checkouts behind for a periodic sweep to notice.
///
/// Every other move takes the task's lifecycle lease, so it cannot land in the
/// middle of a cleanup and be overwritten by the commit that cleanup is about
/// to make.
async fn handle_move_task(ctx: &IpcContext, task_id: String, status: String) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
    };

    let new_status = match parse_status(&status) {
        Some(s) => s,
        None => return IpcResponse::error(format!("Invalid status: {status}")),
    };

    if matches!(new_status, crate::domain::TaskStatus::Done) {
        return match crate::lifecycle::terminalize(
            terminalize_ctx(ctx),
            task_uuid,
            crate::lifecycle::Origin::User,
            crate::lifecycle::TerminalizeRequest::new(new_status),
        )
        .await
        {
            Ok(task) => {
                let projects = ctx.projects.read().await;
                let pname = projects
                    .get(&task.project_id)
                    .map(|p| p.name.as_str())
                    .unwrap_or("?");
                let summary = task_to_summary(&task, pname);
                IpcResponse::success(serde_json::to_value(summary).unwrap_or_default())
            }
            Err(crate::lifecycle::TerminalizeRefusal::TaskNotFound) => {
                IpcResponse::error(format!("Task {task_id} not found"))
            }
            Err(refusal) => IpcResponse::error(refusal.to_string()),
        };
    }

    let _lease = match ctx.task_lifecycle_locks.acquire(task_uuid).await {
        Ok(lease) => lease,
        Err(e) => return IpcResponse::error(e),
    };

    let projects = ctx.projects.read().await;
    let mut tasks = ctx.tasks.write().await;
    if let Some(task) = tasks.get_mut(&task_uuid) {
        task.status = new_status;
        task.updated_at = chrono::Utc::now();
        let project_id = task.project_id;
        let pname = projects
            .get(&project_id)
            .map(|p| p.name.as_str())
            .unwrap_or("?");
        let summary = task_to_summary(task, pname);
        persist_project_tasks(&tasks, project_id, &ctx.storage);
        IpcResponse::success(serde_json::to_value(summary).unwrap_or_default())
    } else {
        IpcResponse::error(format!("Task {task_id} not found"))
    }
}

/// The handles [`crate::lifecycle::terminalize`] needs, out of an
/// [`IpcContext`].
fn terminalize_ctx(ctx: &IpcContext) -> crate::lifecycle::TerminalizeCtx<'_> {
    crate::lifecycle::TerminalizeCtx {
        tasks: &ctx.tasks,
        projects: &ctx.projects,
        repositories: &ctx.repositories,
        worktree_manager: &ctx.worktree_manager,
        storage: &ctx.storage,
        authority: &ctx.task_lifecycle_locks,
        running: ctx
            .executor
            .get()
            .map(|e| e.as_ref() as &dyn crate::lifecycle::ExecutionOwnership),
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
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
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
        let pname = projects
            .get(&project_id)
            .map(|p| p.name.as_str())
            .unwrap_or("?");
        let summary = task_to_summary(task, pname);
        persist_project_tasks(&tasks, project_id, &ctx.storage);
        IpcResponse::success(serde_json::to_value(summary).unwrap_or_default())
    } else {
        IpcResponse::error(format!("Task {task_id} not found"))
    }
}

/// Delete a task through the same lifecycle-aware operation the desktop uses.
///
/// This used to be a bare `HashMap::remove` plus a save: no lease, no execution
/// check, no cleanup. `slashit delete` on a task with a live checkout removed
/// the only record naming that directory and that branch, left the directory on
/// disk, and reported success -- and, holding no lease, it could do it while a
/// cleanup was mid-removal, overwriting the `cleanup_in_flight` stamp that the
/// next start relies on. There is one delete now, and both front doors call it.
async fn handle_delete_task(ctx: &IpcContext, task_id: String) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
    };

    match crate::lifecycle::delete(terminalize_ctx(ctx), task_uuid).await {
        Ok(true) => IpcResponse::success(serde_json::json!({"deleted": task_id})),
        Ok(false) => IpcResponse::error(format!("Task {task_id} not found")),
        Err(refusal) => IpcResponse::error(refusal.to_string()),
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
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
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

/// Bring the window forward, if this instance has one.
///
/// A daemon reports the failure instead of quietly succeeding: `slashit show`
/// against a headless instance should say there is no window, not claim it
/// raised one.
fn handle_show(ctx: &IpcContext) -> IpcResponse {
    match ctx.control.show_window() {
        Ok(()) => IpcResponse::success(serde_json::json!({"shown": true})),
        Err(e) => IpcResponse::error(e),
    }
}

/// Ask the instance to stop.
///
/// The quit itself is deferred to the server, which performs it only after this
/// response has been written; see [`Dispatch::then_quit`].
fn handle_quit() -> Dispatch {
    Dispatch {
        response: IpcResponse::success(serde_json::json!({"quit": true})),
        then_quit: true,
    }
}

/// Every feature flag with the value in force and the layer that decided it.
///
/// A daemon started with `--feature` overrides serves its startup snapshot,
/// which is the only place the CLI layer is still visible — see
/// [`IpcContext::feature_diagnostics`]. Otherwise this resolves live through
/// the same code the settings UI calls, so `slashit features` and the toggle
/// in the app cannot disagree about which layer won.
async fn handle_features(ctx: &IpcContext) -> IpcResponse {
    let flags = match &ctx.feature_diagnostics {
        Some(cached) => cached.clone(),
        None => ctx.features.read().await.diagnostics(),
    };
    IpcResponse::success(serde_json::to_value(flags).unwrap_or_default())
}

/// Report what kind of instance answered, and where.
///
/// The endpoint comes from the connection rather than from configuration: with
/// several listeners bound, the useful answer is the one the caller actually
/// reached.
fn handle_ping(ctx: &IpcContext, peer: &PeerContext) -> IpcResponse {
    let info = InstanceInfo {
        mode: ctx.control.mode().as_str().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: PROTOCOL_VERSION,
        pid: std::process::id(),
        endpoint: peer.endpoint.to_string(),
    };
    IpcResponse::success(serde_json::to_value(info).unwrap_or_default())
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
        eprintln!("Warning: Failed to persist tasks: {e}");
    }
}

#[cfg(test)]
mod tests {
    //! The delete handler's own policy, proven against the same conditions the
    //! desktop delete is held to in `crate::lifecycle::tests`.
    //!
    //! Driven through [`dispatch`] rather than over a socket: the transport is
    //! already covered by `tests/ipc_integration.rs`, and what needs proving
    //! here is that the daemon's front door reaches the one shared operation
    //! rather than a second copy of the rules. A delete that is safe through
    //! the desktop and unsafe through the CLI is not a safe delete.

    use super::*;
    use crate::domain::{Project, Repository};
    use crate::test_helpers::{create_test_task_full, ipc_test_context};
    use slashit_ipc::Endpoint;
    use std::path::Path;
    use std::sync::Arc;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn peer() -> PeerContext {
        PeerContext {
            endpoint: Endpoint::Unix {
                path: std::path::PathBuf::from("/nonexistent-for-tests"),
            },
            os_verified: true,
        }
    }

    /// A context whose worktrees are SlashIt's own, plus a real repository, a
    /// project pointing at it, and one task holding a real checkout.
    ///
    /// The placement is pinned to `Managed` rather than left at the default
    /// `Auto`, which delegates to `wt` when the developer happens to have it
    /// installed. What is under test is the handler's policy, and it must not
    /// depend on which tools the machine running the test owns.
    async fn world(dirty: bool) -> (tempfile::TempDir, IpcContext, Uuid, String) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));

        let mut ctx = ipc_test_context(paths.clone());
        ctx.worktree_manager = Arc::new(crate::worktree::WorktreeManager::new(
            paths,
            crate::config::paths::WorktreePlacement::Managed,
        ));

        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        git(&repo_dir, &["init", "--initial-branch=main"]);
        git(&repo_dir, &["config", "user.email", "test@example.com"]);
        git(&repo_dir, &["config", "user.name", "Test"]);
        std::fs::write(repo_dir.join("README.md"), "seed\n").unwrap();
        git(&repo_dir, &["add", "."]);
        git(&repo_dir, &["commit", "-m", "seed"]);
        let repo_path = repo_dir.to_string_lossy().to_string();

        let repository_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        ctx.repositories.write().await.insert(
            repository_id,
            Repository {
                id: repository_id,
                local_path: repo_path.clone(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        ctx.projects.write().await.insert(
            project_id,
            Project {
                id: project_id,
                name: "ipc-delete-test".to_string(),
                repository_id: Some(repository_id),
                scope: crate::domain::project::ProjectScope::Standalone,
                state_location: crate::config::paths::StateLocation::External,
                agent_type: crate::domain::AgentType::ClaudeCode,
                agent_config: crate::domain::project::AgentConfig {
                    agent_type: crate::domain::AgentType::ClaudeCode,
                    command: "claude".to_string(),
                    args: Vec::new(),
                    env: std::collections::HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );

        let info = ctx
            .worktree_manager
            .create(&repo_path, "task-abcd1234")
            .await
            .expect("worktree creation");
        let checkout = std::path::PathBuf::from(&info.path);
        std::fs::write(checkout.join("work.txt"), "work\n").unwrap();
        git(&checkout, &["config", "user.email", "test@example.com"]);
        git(&checkout, &["config", "user.name", "Test"]);
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-m", "work"]);
        if dirty {
            // Uncommitted and not ignored: git refuses this checkout through
            // the ordinary production path, which is the refusal under test.
            std::fs::write(checkout.join("unsaved.txt"), "not committed\n").unwrap();
        }

        let mut task = create_test_task_full("subject", project_id, TaskStatus::InProgress, 0);
        task.worktree_path = Some(info.path.clone());
        task.branch_name = Some("task-abcd1234".to_string());
        let task_id = task.id;
        ctx.tasks.write().await.insert(task_id, task.clone());
        ctx.storage
            .save_project_tasks(project_id, &[task])
            .expect("seed the board");

        (tmp, ctx, task_id, info.path)
    }

    #[tokio::test]
    async fn ipc_delete_refuses_a_checkout_git_will_not_remove_and_keeps_the_record() {
        let (_tmp, ctx, task_id, checkout) = world(true).await;

        let answer = dispatch(
            IpcRequest::DeleteTask {
                task_id: task_id.to_string(),
            },
            &ctx,
            &peer(),
        )
        .await
        .response;

        assert!(
            !answer.ok,
            "the handler answered success for a delete whose cleanup git refused: {answer:?}"
        );

        // The record is the only thing naming this checkout and this branch.
        // Removing it after a refusal is what strands the directory.
        let task = ctx
            .tasks
            .read()
            .await
            .get(&task_id)
            .cloned()
            .expect("the record has to survive a refused delete");
        assert_eq!(task.worktree_path.as_deref(), Some(checkout.as_str()));
        assert!(
            Path::new(&checkout).join("unsaved.txt").exists(),
            "and the work the refusal was protecting must survive with it"
        );
    }

    #[tokio::test]
    async fn ipc_delete_removes_the_task_once_its_checkout_is_safely_gone() {
        let (_tmp, ctx, task_id, checkout) = world(false).await;

        let answer = dispatch(
            IpcRequest::DeleteTask {
                task_id: task_id.to_string(),
            },
            &ctx,
            &peer(),
        )
        .await
        .response;

        assert!(answer.ok, "a clean checkout must delete: {answer:?}");
        assert!(!ctx.tasks.read().await.contains_key(&task_id));
        assert!(
            !Path::new(&checkout).exists(),
            "the checkout goes with the record it belonged to"
        );
    }
}
