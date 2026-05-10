use crate::agents::runner::{ClaudeRunner, ClaudeRunConfig, ClaudeEvent};
use crate::domain::{Task, TaskStatus, TaskPhase, AgentExecution, AgentStatus, AgentLogEntry, LogLevel, QaSignoff, QaStatus};
use crate::queue::prompt::{build_task_prompt, build_review_prompt, build_fix_prompt};
use crate::queue::QueueManager;
use crate::worktree::{WorktreeManager, WorktreeInfo};
use std::collections::HashMap;
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Event emitted to the frontend via Tauri events.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum AgentEvent {
    #[serde(rename = "log")]
    Log { task_id: String, level: LogLevel, message: String },
    #[serde(rename = "phase_change")]
    PhaseChange { task_id: String, phase: TaskPhase, progress: u8 },
    #[serde(rename = "tool_use")]
    ToolUse { task_id: String, tool: String },
    #[serde(rename = "completed")]
    Completed { task_id: String, success: bool, message: Option<String> },
    #[serde(rename = "error")]
    Error { task_id: String, message: String },
}

type Tasks = Arc<RwLock<HashMap<Uuid, Task>>>;

pub struct TaskExecutor {
    tasks: Tasks,
    queue_manager: Arc<RwLock<QueueManager>>,
    executions: Arc<RwLock<HashMap<Uuid, AgentExecution>>>,
    running_handles: Arc<RwLock<HashMap<Uuid, JoinHandle<()>>>>,
    reviewing_handles: Arc<RwLock<HashMap<Uuid, JoinHandle<()>>>>,
    logs: Arc<RwLock<HashMap<Uuid, Vec<AgentLogEntry>>>>,
    projects: Arc<RwLock<HashMap<Uuid, crate::domain::Project>>>,
    repositories: Arc<RwLock<HashMap<Uuid, crate::domain::Repository>>>,
    storage: crate::config::Storage,
    worktree_manager: Arc<WorktreeManager>,
    app_handle: tauri::AppHandle,
    pr_check_counter: std::sync::atomic::AtomicU32,
}

pub struct TaskExecutorConfig {
    pub tasks: Tasks,
    pub queue_manager: Arc<RwLock<QueueManager>>,
    pub executions: Arc<RwLock<HashMap<Uuid, AgentExecution>>>,
    pub logs: Arc<RwLock<HashMap<Uuid, Vec<AgentLogEntry>>>>,
    pub projects: Arc<RwLock<HashMap<Uuid, crate::domain::Project>>>,
    pub repositories: Arc<RwLock<HashMap<Uuid, crate::domain::Repository>>>,
    pub storage: crate::config::Storage,
    pub worktree_manager: Arc<WorktreeManager>,
    pub app_handle: tauri::AppHandle,
}

impl TaskExecutor {
    pub fn new(config: TaskExecutorConfig) -> Self {
        Self {
            tasks: config.tasks,
            queue_manager: config.queue_manager,
            executions: config.executions,
            running_handles: Arc::new(RwLock::new(HashMap::new())),
            reviewing_handles: Arc::new(RwLock::new(HashMap::new())),
            logs: config.logs,
            projects: config.projects,
            repositories: config.repositories,
            storage: config.storage,
            worktree_manager: config.worktree_manager,
            app_handle: config.app_handle,
            pr_check_counter: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Start the polling loop.
    pub fn start_polling(self: &Arc<Self>) {
        let executor = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            loop {
                executor.check_and_execute().await;
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }
        });
    }

    async fn check_and_execute(&self) {
        // Auto-promote tasks from Queue → InProgress when capacity is available
        let manager = self.queue_manager.read().await;
        if manager.config().auto_promote {
            while let Some(task_id) = manager.promote_next_task().await {
                let _ = self.app_handle.emit("agent-event", AgentEvent::Log {
                    task_id: task_id.to_string(),
                    level: LogLevel::Info,
                    message: "Auto-promoted from queue".to_string(),
                });
            }
        }
        drop(manager);

        // Find InProgress tasks that haven't started execution yet
        let pending: Vec<Uuid> = {
            let tasks = self.tasks.read().await;
            tasks.values()
                .filter(|t| t.status == TaskStatus::InProgress && t.phase == TaskPhase::Idle)
                .map(|t| t.id)
                .collect()
        };

        let running = self.running_handles.read().await.len();
        let limit = {
            let mgr = self.queue_manager.read().await;
            mgr.config().parallel_task_limit as usize
        };
        let available = limit.saturating_sub(running);

        for task_id in pending.into_iter().take(available) {
            self.spawn_task_execution(task_id).await;
        }

        // Find AiReview tasks that need automated review
        let review_pending: Vec<Uuid> = {
            let tasks = self.tasks.read().await;
            let reviewing = self.reviewing_handles.read().await;
            tasks.values()
                .filter(|t| t.status == TaskStatus::AiReview && t.phase == TaskPhase::QaReview)
                .filter(|t| !reviewing.contains_key(&t.id))
                .map(|t| t.id)
                .collect()
        };

        for task_id in review_pending {
            self.spawn_review(task_id).await;
        }

        // Poll PR status every ~30s (10 cycles at 3s each)
        let counter = self.pr_check_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if counter.is_multiple_of(10) {
            use crate::domain::task::ExternalRef;

            // Find tasks with GithubPr refs that haven't been merged yet
            let pr_tasks: Vec<(Uuid, u32, String)> = {
                let tasks = self.tasks.read().await;
                tasks.values()
                    .filter(|t| matches!(t.status, TaskStatus::PrCreated | TaskStatus::HumanReview | TaskStatus::Done))
                    .flat_map(|t| {
                        t.external_refs.iter().filter_map(move |r| {
                            if let ExternalRef::GithubPr { number, repo, state, .. } = r {
                                // Only poll if not already in a terminal state
                                if state.as_deref() != Some("MERGED") && state.as_deref() != Some("CLOSED") {
                                    return Some((t.id, *number, repo.clone()));
                                }
                            }
                            None
                        })
                    })
                    .collect()
            };

            for (task_id, number, repo_slug) in pr_tasks {
                if let Ok(output) = tokio::process::Command::new("gh")
                    .args(["pr", "view", &number.to_string(), "--repo", &repo_slug, "--json", "state"])
                    .output()
                    .await
                {
                    if output.status.success() {
                        let json_str = String::from_utf8_lossy(&output.stdout);
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) {
                            let state = json.get("state").and_then(|s| s.as_str()).unwrap_or("");

                            // Update the ExternalRef state
                            {
                                let mut tasks_w = self.tasks.write().await;
                                if let Some(t) = tasks_w.get_mut(&task_id) {
                                    for r in &mut t.external_refs {
                                        if let ExternalRef::GithubPr { number: n, state: ref mut s, .. } = r {
                                            if *n == number {
                                                *s = Some(state.to_string());
                                            }
                                        }
                                    }

                                    match state {
                                        "MERGED" => {
                                            t.status = TaskStatus::Done;
                                            t.overall_progress = 100;
                                            t.phase = TaskPhase::Complete;
                                            t.updated_at = chrono::Utc::now();
                                            if let Some(wt_path) = t.worktree_path.take() {
                                                let branch = t.branch_name.clone().unwrap_or_default();
                                                let wt_mgr = self.worktree_manager.clone();
                                                tokio::spawn(async move {
                                                    let _ = wt_mgr.remove(&wt_path, &branch).await;
                                                });
                                            }
                                        }
                                        "CLOSED" => {
                                            t.error_message = Some("PR was closed without merge".to_string());
                                        }
                                        _ => {} // OPEN — update state only
                                    }
                                }
                            }

                            if state == "MERGED" {
                                Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                                let _ = self.app_handle.emit("agent-event", AgentEvent::Completed {
                                    task_id: task_id.to_string(),
                                    success: true,
                                    message: Some("PR merged — task complete".to_string()),
                                });
                            } else if state == "CLOSED" || !state.is_empty() {
                                Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                            }
                        }
                    }
                }
            }
        }
    }

    async fn spawn_task_execution(&self, task_id: Uuid) {
        // Resolve repo path from task → project → repository chain
        let repo_path = match Self::resolve_working_dir_for_task(
            &self.tasks, &self.projects, &self.repositories, task_id,
        ).await {
            Ok(dir) => dir,
            Err(e) => {
                let _ = self.app_handle.emit("agent-event", AgentEvent::Error {
                    task_id: task_id.to_string(),
                    message: format!("Cannot resolve working directory: {}", e),
                });
                Self::set_task_error_static(&self.tasks, &self.storage, task_id, &e).await;
                return;
            }
        };

        // Check if task already has a branch (re-queue after completion)
        let existing_branch = {
            let tasks_r = self.tasks.read().await;
            tasks_r.get(&task_id).and_then(|t| t.branch_name.clone())
        };

        let branch_name = existing_branch
            .clone()
            .unwrap_or_else(|| WorktreeManager::branch_for_task(task_id));

        // Check dependencies for stacked branching (only for new branches).
        // git-spice manages the stack — we just decide when to use it:
        // Stack when the dependency has a branch that hasn't been merged to main yet.
        // If merged, base on main normally (the code is already there).
        let base_branch = if existing_branch.is_none() {
            let tasks_r = self.tasks.read().await;
            let deps = tasks_r.get(&task_id)
                .map(|t| t.dependencies.clone())
                .unwrap_or_default();
            if let Some(dep_id) = deps.first() {
                tasks_r.get(dep_id).and_then(|dep_task| {
                    // Dependency must have a branch
                    let branch = dep_task.branch_name.as_ref()?;
                    // If dependency is Done and has no active PR, code is in main
                    let is_done = dep_task.status == TaskStatus::Done;
                    let has_pr = dep_task.external_refs.iter().any(|r| r.is_pr());
                    if is_done && !has_pr {
                        return None; // merged via main, no stack needed
                    }
                    Some(branch.clone())
                })
            } else {
                None
            }
        } else {
            None
        };

        // Helper closure to handle worktree success
        let handle_worktree_ok = |info: &WorktreeInfo, app: &tauri::AppHandle, msg: &str| {
            let _ = app.emit("agent-event", AgentEvent::Log {
                task_id: task_id.to_string(),
                level: LogLevel::Info,
                message: format!("{}: {}", msg, info.path),
            });
        };

        // Create or reattach to worktree for this task
        let (working_dir, _worktree_path) = if let Some(ref _existing) = existing_branch {
            // Reattach to existing branch
            match self.worktree_manager.reattach(&repo_path, &branch_name).await {
                Ok(info) => {
                    handle_worktree_ok(&info, &self.app_handle, "Reattached worktree");
                    {
                        let mut tasks_w = self.tasks.write().await;
                        if let Some(t) = tasks_w.get_mut(&task_id) {
                            t.worktree_path = Some(info.path.clone());
                            t.branch_name = Some(info.branch.clone());
                        }
                    }
                    Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                    (info.path.clone(), Some(info.path))
                }
                Err(e) => {
                    let _ = self.app_handle.emit("agent-event", AgentEvent::Log {
                        task_id: task_id.to_string(),
                        level: LogLevel::Warn,
                        message: format!("Worktree reattach failed ({}), using repo dir", e),
                    });
                    (repo_path.clone(), None)
                }
            }
        } else if let Some(ref parent_branch) = base_branch {
            // Stacked branch based on parent dependency
            match self.worktree_manager.create_stacked_branch(&repo_path, &branch_name, parent_branch).await {
                Ok(info) => {
                    handle_worktree_ok(&info, &self.app_handle, "Created stacked worktree");
                    {
                        let mut tasks_w = self.tasks.write().await;
                        if let Some(t) = tasks_w.get_mut(&task_id) {
                            t.worktree_path = Some(info.path.clone());
                            t.branch_name = Some(info.branch.clone());
                        }
                    }
                    Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                    (info.path.clone(), Some(info.path))
                }
                Err(e) => {
                    // Fallback to normal create if stacking fails
                    let _ = self.app_handle.emit("agent-event", AgentEvent::Log {
                        task_id: task_id.to_string(),
                        level: LogLevel::Warn,
                        message: format!("Stacked branch failed ({}), falling back to normal create", e),
                    });
                    match self.worktree_manager.create(&repo_path, &branch_name).await {
                        Ok(info) => {
                            handle_worktree_ok(&info, &self.app_handle, "Created worktree (fallback)");
                            {
                                let mut tasks_w = self.tasks.write().await;
                                if let Some(t) = tasks_w.get_mut(&task_id) {
                                    t.worktree_path = Some(info.path.clone());
                                    t.branch_name = Some(info.branch.clone());
                                }
                            }
                            Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                            (info.path.clone(), Some(info.path))
                        }
                        Err(e2) => {
                            let _ = self.app_handle.emit("agent-event", AgentEvent::Log {
                                task_id: task_id.to_string(),
                                level: LogLevel::Warn,
                                message: format!("Worktree creation failed ({}), using repo dir", e2),
                            });
                            (repo_path.clone(), None)
                        }
                    }
                }
            }
        } else {
            // Normal new branch
            match self.worktree_manager.create(&repo_path, &branch_name).await {
                Ok(info) => {
                    handle_worktree_ok(&info, &self.app_handle, "Created worktree");
                    {
                        let mut tasks_w = self.tasks.write().await;
                        if let Some(t) = tasks_w.get_mut(&task_id) {
                            t.worktree_path = Some(info.path.clone());
                            t.branch_name = Some(info.branch.clone());
                        }
                    }
                    Self::persist_task_static(&self.tasks, &self.storage, task_id).await;
                    (info.path.clone(), Some(info.path))
                }
                Err(e) => {
                    let _ = self.app_handle.emit("agent-event", AgentEvent::Log {
                        task_id: task_id.to_string(),
                        level: LogLevel::Warn,
                        message: format!("Worktree creation failed ({}), using repo dir", e),
                    });
                    (repo_path.clone(), None)
                }
            }
        };

        let (prompt, task_model) = {
            let tasks = self.tasks.read().await;
            match tasks.get(&task_id) {
                Some(t) => {
                    let model = if t.model.is_empty() || t.model == "default" {
                        None
                    } else {
                        Some(t.model.clone())
                    };
                    (build_task_prompt(t, None), model)
                },
                None => return,
            }
        };

        // Update phase
        self.update_task_phase(task_id, TaskPhase::Coding, 5).await;

        let tasks = self.tasks.clone();
        let executions = self.executions.clone();
        let running_handles = self.running_handles.clone();
        let logs = self.logs.clone();
        let app_handle = self.app_handle.clone();
        let storage = self.storage.clone();
        let working_dir_for_commit = working_dir.clone();

        let handle = tokio::spawn(async move {
            let execution_id = Uuid::new_v4();
            let now = chrono::Utc::now();

            // Create execution record
            let execution = AgentExecution {
                id: execution_id,
                workspace_id: Uuid::nil(),
                task_id: Some(task_id),
                agent_type: "claude-code".to_string(),
                status: AgentStatus::Starting,
                started_at: now,
                stopped_at: None,
            };
            executions.write().await.insert(execution_id, execution);
            logs.write().await.insert(execution_id, Vec::new());

            let _ = app_handle.emit("agent-event", AgentEvent::Log {
                task_id: task_id.to_string(),
                level: LogLevel::Info,
                message: format!("Starting Claude agent in {}", working_dir),
            });

            // Start Claude runner
            let runner = match ClaudeRunner::start(ClaudeRunConfig {
                prompt,
                working_dir: working_dir.clone(),
                allowed_tools: vec![
                    "Read".to_string(), "Edit".to_string(), "Write".to_string(),
                    "Bash".to_string(), "Glob".to_string(), "Grep".to_string(),
                ],
                max_turns: Some(50),
                max_budget_usd: None,
                session_id: Some(Uuid::new_v4().to_string()),
                resume_session: None,
                model: task_model,
                system_prompt: None,
                permission_mode: None, // defaults to --dangerously-skip-permissions
            }).await {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("Failed to start claude: {}", e);
                    let _ = app_handle.emit("agent-event", AgentEvent::Error {
                        task_id: task_id.to_string(),
                        message: msg.clone(),
                    });
                    Self::set_task_error_static(&tasks, &storage, task_id, &msg).await;
                    return;
                }
            };

            // Update status
            if let Some(exec) = executions.write().await.get_mut(&execution_id) {
                exec.status = AgentStatus::Running;
            }

            let _ = app_handle.emit("agent-event", AgentEvent::PhaseChange {
                task_id: task_id.to_string(),
                phase: TaskPhase::Coding,
                progress: 10,
            });

            // Subscribe to events and forward to frontend
            let mut event_rx = runner.subscribe();
            let app_handle_events = app_handle.clone();
            let task_id_str = task_id.to_string();
            let logs_events = logs.clone();
            let execution_id_events = execution_id;
            let tasks_for_events = tasks.clone();

            tokio::spawn(async move {
                while let Ok(event) = event_rx.recv().await {
                    match &event {
                        ClaudeEvent::TextDelta { text } => {
                            let _ = app_handle_events.emit("agent-event", AgentEvent::Log {
                                task_id: task_id_str.clone(),
                                level: LogLevel::Info,
                                message: text.clone(),
                            });
                        }
                        ClaudeEvent::ToolUse { tool, .. } => {
                            let _ = app_handle_events.emit("agent-event", AgentEvent::ToolUse {
                                task_id: task_id_str.clone(),
                                tool: tool.clone(),
                            });
                            let entry = AgentLogEntry {
                                timestamp: chrono::Utc::now(),
                                level: LogLevel::Info,
                                message: format!("Using tool: {}", tool),
                            };
                            logs_events.write().await.entry(execution_id_events)
                                .or_insert_with(Vec::new)
                                .push(entry);
                        }
                        ClaudeEvent::SystemInit { session_id, model, .. } => {
                            // Capture actual model on the task
                            if let Some(m) = model {
                                let mut tasks_w = tasks_for_events.write().await;
                                if let Some(t) = tasks_w.get_mut(&task_id) {
                                    t.model = m.clone();
                                }
                            }
                            let entry = AgentLogEntry {
                                timestamp: chrono::Utc::now(),
                                level: LogLevel::Info,
                                message: format!("Session started: {} (model: {})",
                                    session_id,
                                    model.as_deref().unwrap_or("unknown")),
                            };
                            logs_events.write().await.entry(execution_id_events)
                                .or_insert_with(Vec::new)
                                .push(entry);
                        }
                        ClaudeEvent::Error { message } => {
                            let _ = app_handle_events.emit("agent-event", AgentEvent::Error {
                                task_id: task_id_str.clone(),
                                message: message.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            });

            // Wait for completion
            match runner.wait().await {
                Ok(_) => {
                    // Commit changes in the worktree/working dir
                    Self::commit_changes(&tasks, task_id, &working_dir_for_commit, &app_handle).await;

                    // Move to AiReview for automated review before human review
                    Self::update_task_phase_static(&tasks, task_id, TaskPhase::QaReview, 80).await;
                    {
                        let mut tasks_w = tasks.write().await;
                        if let Some(t) = tasks_w.get_mut(&task_id) {
                            t.status = TaskStatus::AiReview;
                            t.overall_progress = 80;
                            t.updated_at = chrono::Utc::now();
                        }
                    }
                    let _ = app_handle.emit("agent-event", AgentEvent::Completed {
                        task_id: task_id.to_string(),
                        success: true,
                        message: Some("Agent completed — moving to AI review".to_string()),
                    });
                    Self::persist_task_static(&tasks, &storage, task_id).await;
                }
                Err(err_msg) => {
                    // Include accumulated stdout if stderr was empty
                    let full_msg = if err_msg.contains("no details") {
                        let stdout_output = runner.get_output().await;
                        let last_lines: String = stdout_output.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
                        if last_lines.is_empty() {
                            err_msg.clone()
                        } else {
                            format!("{} — {}", err_msg, last_lines)
                        }
                    } else {
                        err_msg.clone()
                    };
                    let _ = app_handle.emit("agent-event", AgentEvent::Error {
                        task_id: task_id.to_string(),
                        message: full_msg.clone(),
                    });
                    Self::set_task_error_static(&tasks, &storage, task_id, &full_msg).await;
                }
            }

            // Cleanup
            let _ = runner.kill().await;
            running_handles.write().await.remove(&task_id);

            if let Some(exec) = executions.write().await.get_mut(&execution_id) {
                exec.status = AgentStatus::Stopped;
                exec.stopped_at = Some(chrono::Utc::now());
            }
        });

        self.running_handles.write().await.insert(task_id, handle);
    }

    pub async fn execute_task(&self, task_id: Uuid) -> Result<(), String> {
        {
            let tasks = self.tasks.read().await;
            let task = tasks.get(&task_id).ok_or("Task not found")?;
            if task.status != TaskStatus::InProgress && task.status != TaskStatus::Queue {
                return Err(format!("Task in {:?}, expected InProgress or Queue", task.status));
            }
        }
        {
            let mut tasks = self.tasks.write().await;
            if let Some(t) = tasks.get_mut(&task_id) {
                t.status = TaskStatus::InProgress;
                t.phase = TaskPhase::Idle;
                t.updated_at = chrono::Utc::now();
            }
        }
        self.spawn_task_execution(task_id).await;
        Ok(())
    }

    pub async fn stop_task(&self, task_id: Uuid) -> Result<(), String> {
        if let Some(handle) = self.running_handles.write().await.remove(&task_id) {
            handle.abort();
        }
        {
            let mut tasks = self.tasks.write().await;
            if let Some(t) = tasks.get_mut(&task_id) {
                t.phase = TaskPhase::Idle;
                t.phase_progress = 0;
                t.updated_at = chrono::Utc::now();
            }
        }
        Ok(())
    }

    async fn spawn_review(&self, task_id: Uuid) {
        // Use worktree path if available, otherwise repo path
        let worktree_path = {
            let tasks_r = self.tasks.read().await;
            tasks_r.get(&task_id).and_then(|t| t.worktree_path.clone())
        };
        let working_dir = if let Some(wt) = worktree_path {
            wt
        } else {
            match Self::resolve_working_dir_for_task(
                &self.tasks, &self.projects, &self.repositories, task_id,
            ).await {
                Ok(dir) => dir,
                Err(e) => {
                    let _ = self.app_handle.emit("agent-event", AgentEvent::Log {
                        task_id: task_id.to_string(),
                        level: LogLevel::Warn,
                        message: format!("Cannot resolve working dir for review: {}", e),
                    });
                    let signoff = QaSignoff {
                        status: QaStatus::Rejected,
                        issues_found: vec![format!("AI review skipped: {}", e)],
                        timestamp: chrono::Utc::now(),
                        session_id: Uuid::new_v4(),
                    };
                    Self::transition_to_human_review(&self.tasks, &self.storage, task_id, Some(signoff)).await;
                    return;
                }
            }
        };

        let tasks = self.tasks.clone();
        let reviewing_handles = self.reviewing_handles.clone();
        let app_handle = self.app_handle.clone();
        let storage = self.storage.clone();
        let queue_manager = self.queue_manager.clone();

        // Mark phase as actively reviewing
        Self::update_task_phase_static(&tasks, task_id, TaskPhase::QaReview, 85).await;

        let handle = tokio::spawn(async move {
            let task_id_str = task_id.to_string();

            let _ = app_handle.emit("agent-event", AgentEvent::Log {
                task_id: task_id_str.clone(),
                level: LogLevel::Info,
                message: "Starting AI review...".to_string(),
            });

            // Get diff — try jj first, fallback to git
            let diff = match Self::get_diff(&working_dir).await {
                Some(d) => d,
                None => {
                    let _ = app_handle.emit("agent-event", AgentEvent::Log {
                        task_id: task_id_str.clone(),
                        level: LogLevel::Warn,
                        message: "Could not get diff (jj/git), skipping AI review".to_string(),
                    });
                    let signoff = QaSignoff {
                        status: QaStatus::Rejected,
                        issues_found: vec!["AI review skipped: diff failed".to_string()],
                        timestamp: chrono::Utc::now(),
                        session_id: Uuid::new_v4(),
                    };
                    Self::transition_to_human_review(&tasks, &storage, task_id, Some(signoff)).await;
                    reviewing_handles.write().await.remove(&task_id);
                    return;
                }
            };

            if diff.trim().is_empty() {
                let _ = app_handle.emit("agent-event", AgentEvent::Log {
                    task_id: task_id_str.clone(),
                    level: LogLevel::Info,
                    message: "No changes detected, skipping review".to_string(),
                });
                Self::transition_to_human_review(&tasks, &storage, task_id, None).await;
                reviewing_handles.write().await.remove(&task_id);
                return;
            }

            // Launch Claude review + CodeRabbit in parallel
            let review_prompt = {
                let tasks_r = tasks.read().await;
                match tasks_r.get(&task_id) {
                    Some(t) => build_review_prompt(t, &diff),
                    None => {
                        reviewing_handles.write().await.remove(&task_id);
                        return;
                    }
                }
            };

            // A) Claude review
            let claude_review = async {
                let runner = ClaudeRunner::start(ClaudeRunConfig {
                    prompt: review_prompt,
                    working_dir: working_dir.clone(),
                    allowed_tools: vec![
                        "Read".to_string(), "Glob".to_string(), "Grep".to_string(),
                    ],
                    max_turns: Some(10),
                    max_budget_usd: None,
                    session_id: Some(Uuid::new_v4().to_string()),
                    resume_session: None,
                    model: None,
                    system_prompt: None,
                    permission_mode: None,
                }).await;

                match runner {
                    Ok(r) => {
                        let _success = r.wait().await.unwrap_or(false);
                        let output = r.get_output().await;
                        let _ = r.kill().await;
                        output
                    }
                    Err(e) => format!("Claude review error: {}", e),
                }
            };

            // B) CodeRabbit review (if available)
            let use_coderabbit = {
                let mgr = queue_manager.read().await;
                mgr.config().use_coderabbit
            };

            let coderabbit_review = async {
                if !use_coderabbit {
                    return String::new();
                }

                // Check if coderabbit binary exists
                match tokio::process::Command::new("which")
                    .arg("coderabbit")
                    .output()
                    .await
                {
                    Ok(output) if output.status.success() => {}
                    _ => {
                        let _ = app_handle.emit("agent-event", AgentEvent::Log {
                            task_id: task_id_str.clone(),
                            level: LogLevel::Warn,
                            message: "CodeRabbit enabled but CLI not found. Install it or disable in Queue Settings.".to_string(),
                        });
                        return String::new();
                    }
                }

                let _ = app_handle.emit("agent-event", AgentEvent::Log {
                    task_id: task_id_str.clone(),
                    level: LogLevel::Info,
                    message: "Running CodeRabbit review...".to_string(),
                });

                match tokio::process::Command::new("coderabbit")
                    .args([
                        "review",
                        "--prompt-only",
                        "--type", "uncommitted",
                        "--cwd", &working_dir,
                        "--no-color",
                    ])
                    .output()
                    .await
                {
                    Ok(output) if output.status.success() => {
                        String::from_utf8_lossy(&output.stdout).to_string()
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        format!("CodeRabbit warning: {}", stderr)
                    }
                    Err(e) => {
                        format!("CodeRabbit error: {}", e)
                    }
                }
            };

            // Run both reviews in parallel
            let (claude_result, coderabbit_result) = tokio::join!(claude_review, coderabbit_review);

            // Merge findings
            let has_claude_issues = claude_result.contains("CHANGES_REQUESTED");
            let has_coderabbit_issues = !coderabbit_result.is_empty()
                && !coderabbit_result.starts_with("CodeRabbit error:")
                && !coderabbit_result.starts_with("CodeRabbit warning:");

            if has_claude_issues || has_coderabbit_issues {
                let _ = app_handle.emit("agent-event", AgentEvent::Log {
                    task_id: task_id_str.clone(),
                    level: LogLevel::Info,
                    message: "Issues found — validating and fixing...".to_string(),
                });

                // Build combined findings
                let mut findings = String::new();
                if has_claude_issues {
                    findings.push_str("### Claude Code Review\n");
                    findings.push_str(&claude_result);
                    findings.push('\n');
                }
                if has_coderabbit_issues {
                    findings.push_str("### CodeRabbit Review\n");
                    findings.push_str(&coderabbit_result);
                    findings.push('\n');
                }

                // Spawn fix agent that validates before fixing
                let fix_prompt = {
                    let tasks_r = tasks.read().await;
                    match tasks_r.get(&task_id) {
                        Some(t) => build_fix_prompt(t, &findings),
                        None => {
                            reviewing_handles.write().await.remove(&task_id);
                            return;
                        }
                    }
                };

                let fix_result = match ClaudeRunner::start(ClaudeRunConfig {
                    prompt: fix_prompt,
                    working_dir: working_dir.clone(),
                    allowed_tools: vec![
                        "Read".to_string(), "Edit".to_string(), "Write".to_string(),
                        "Glob".to_string(), "Grep".to_string(),
                    ],
                    max_turns: Some(20),
                    max_budget_usd: None,
                    session_id: Some(Uuid::new_v4().to_string()),
                    resume_session: None,
                    model: None,
                    system_prompt: None,
                    permission_mode: None,
                }).await {
                    Ok(r) => {
                        let _success = r.wait().await.unwrap_or(false);
                        let _ = r.kill().await;
                        // Re-describe in jj after fixes
                        let _ = tokio::process::Command::new("jj")
                            .args(["describe", "-m", &format!("task: {} (with review fixes)", {
                                let tasks_r = tasks.read().await;
                                tasks_r.get(&task_id).map(|t| t.title.clone()).unwrap_or_default()
                            })])
                            .current_dir(&working_dir)
                            .output()
                            .await;
                        let _ = tokio::process::Command::new("jj")
                            .args(["git", "export"])
                            .current_dir(&working_dir)
                            .output()
                            .await;
                        true
                    }
                    Err(e) => {
                        let _ = app_handle.emit("agent-event", AgentEvent::Log {
                            task_id: task_id_str.clone(),
                            level: LogLevel::Error,
                            message: format!("Fix agent failed to start: {}", e),
                        });
                        false
                    }
                };

                let issues: Vec<String> = findings.lines()
                    .filter(|l| l.starts_with("- ISSUE:") || l.starts_with("ISSUE:"))
                    .map(|l| l.to_string())
                    .collect();

                let signoff = QaSignoff {
                    status: if fix_result { QaStatus::FixesApplied } else { QaStatus::Rejected },
                    issues_found: issues,
                    timestamp: chrono::Utc::now(),
                    session_id: Uuid::new_v4(),
                };

                Self::transition_to_human_review(&tasks, &storage, task_id, Some(signoff)).await;
            } else {
                // All clear — no issues
                let _ = app_handle.emit("agent-event", AgentEvent::Log {
                    task_id: task_id_str.clone(),
                    level: LogLevel::Info,
                    message: "AI review passed — moving to human review".to_string(),
                });

                let signoff = QaSignoff {
                    status: QaStatus::Approved,
                    issues_found: Vec::new(),
                    timestamp: chrono::Utc::now(),
                    session_id: Uuid::new_v4(),
                };

                Self::transition_to_human_review(&tasks, &storage, task_id, Some(signoff)).await;
            }

            reviewing_handles.write().await.remove(&task_id);
        });

        self.reviewing_handles.write().await.insert(task_id, handle);
    }

    async fn transition_to_human_review(
        tasks: &Tasks,
        storage: &crate::config::Storage,
        task_id: Uuid,
        signoff: Option<QaSignoff>,
    ) {
        {
            let mut tasks_w = tasks.write().await;
            if let Some(t) = tasks_w.get_mut(&task_id) {
                t.status = TaskStatus::HumanReview;
                t.phase = TaskPhase::Complete;
                t.phase_progress = 95;
                t.overall_progress = 90;
                t.updated_at = chrono::Utc::now();
                if let Some(s) = signoff {
                    t.qa_signoff = Some(s);
                }
            }
        }
        Self::persist_task_static(tasks, storage, task_id).await;
    }

    pub async fn get_task_output(&self, task_id: Uuid) -> Vec<AgentLogEntry> {
        let executions = self.executions.read().await;
        let eid = executions.values()
            .find(|e| e.task_id == Some(task_id))
            .map(|e| e.id);
        if let Some(eid) = eid {
            self.logs.read().await.get(&eid).cloned().unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    // --- Helpers ---

    async fn resolve_working_dir_for_task(
        tasks: &Tasks,
        projects: &Arc<RwLock<HashMap<Uuid, crate::domain::Project>>>,
        repositories: &Arc<RwLock<HashMap<Uuid, crate::domain::Repository>>>,
        task_id: Uuid,
    ) -> Result<String, String> {
        let tasks_r = tasks.read().await;
        let task = tasks_r.get(&task_id)
            .ok_or_else(|| format!("Task {} not found", task_id))?;
        let project_id = task.project_id;
        drop(tasks_r);

        let projects_r = projects.read().await;
        let project = projects_r.get(&project_id)
            .ok_or_else(|| format!("Project {} not found", project_id))?;
        let repo_id = project.repository_id
            .ok_or_else(|| format!("Project {} has no repository linked", project_id))?;
        drop(projects_r);

        let repos = repositories.read().await;
        let repo = repos.get(&repo_id)
            .ok_or_else(|| format!("Repository {} not found", repo_id))?;
        Ok(repo.local_path.clone())
    }

    /// Commit agent changes in the worktree/working directory.
    async fn commit_changes(
        tasks: &Tasks,
        task_id: Uuid,
        working_dir: &str,
        app_handle: &tauri::AppHandle,
    ) {
        let title = {
            let tasks_r = tasks.read().await;
            tasks_r.get(&task_id).map(|t| t.title.clone()).unwrap_or_default()
        };
        let task_id_str = task_id.to_string();

        // Try jj first (for jj-managed repos)
        let jj_ok = if let Ok(output) = tokio::process::Command::new("jj")
            .args(["describe", "-m", &format!("task: {}", title)])
            .current_dir(working_dir)
            .output()
            .await
        {
            if output.status.success() {
                let _ = tokio::process::Command::new("jj")
                    .args(["git", "export"])
                    .current_dir(working_dir)
                    .output()
                    .await;
                let _ = app_handle.emit("agent-event", AgentEvent::Log {
                    task_id: task_id_str.clone(),
                    level: LogLevel::Info,
                    message: "Committed via jj".to_string(),
                });
                true
            } else {
                false
            }
        } else {
            false
        };

        // Fallback to git (for worktrees or git-only repos)
        if !jj_ok {
            let _ = tokio::process::Command::new("git")
                .args(["add", "-A"])
                .current_dir(working_dir)
                .output()
                .await;

            let commit_msg = format!("task: {}", title);
            match tokio::process::Command::new("git")
                .args(["commit", "-m", &commit_msg, "--allow-empty"])
                .current_dir(working_dir)
                .output()
                .await
            {
                Ok(output) if output.status.success() => {
                    let _ = app_handle.emit("agent-event", AgentEvent::Log {
                        task_id: task_id_str,
                        level: LogLevel::Info,
                        message: "Committed via git".to_string(),
                    });
                }
                _ => {
                    let _ = app_handle.emit("agent-event", AgentEvent::Log {
                        task_id: task_id_str,
                        level: LogLevel::Warn,
                        message: "Git commit skipped (no changes or error)".to_string(),
                    });
                }
            }
        }
    }

    async fn update_task_phase(&self, task_id: Uuid, phase: TaskPhase, progress: u8) {
        Self::update_task_phase_static(&self.tasks, task_id, phase.clone(), progress).await;
        let _ = self.app_handle.emit("agent-event", AgentEvent::PhaseChange {
            task_id: task_id.to_string(),
            phase,
            progress,
        });
    }

    async fn update_task_phase_static(tasks: &Tasks, task_id: Uuid, phase: TaskPhase, progress: u8) {
        let mut tasks = tasks.write().await;
        if let Some(t) = tasks.get_mut(&task_id) {
            t.phase = phase;
            t.phase_progress = progress;
            t.updated_at = chrono::Utc::now();
        }
    }

    async fn set_task_error_static(tasks: &Tasks, storage: &crate::config::Storage, task_id: Uuid, msg: &str) {
        {
            let mut tasks = tasks.write().await;
            if let Some(t) = tasks.get_mut(&task_id) {
                t.status = TaskStatus::Error;
                t.phase = TaskPhase::Failed;
                t.error_message = Some(msg.to_string());
                t.updated_at = chrono::Utc::now();
            }
        }
        Self::persist_task_static(tasks, storage, task_id).await;
    }

    /// Get diff from jj or git (whichever is available).
    async fn get_diff(working_dir: &str) -> Option<String> {
        // Try jj first
        if let Ok(output) = tokio::process::Command::new("jj")
            .args(["diff", "--git"])
            .current_dir(working_dir)
            .output()
            .await
        {
            if output.status.success() {
                return Some(String::from_utf8_lossy(&output.stdout).to_string());
            }
        }
        // Fallback to git
        if let Ok(output) = tokio::process::Command::new("git")
            .args(["diff", "HEAD"])
            .current_dir(working_dir)
            .output()
            .await
        {
            if output.status.success() {
                return Some(String::from_utf8_lossy(&output.stdout).to_string());
            }
        }
        None
    }

    async fn persist_task_static(tasks: &Tasks, storage: &crate::config::Storage, task_id: Uuid) {
        let tasks_r = tasks.read().await;
        if let Some(task) = tasks_r.get(&task_id) {
            let project_id = task.project_id;
            let project_tasks: Vec<Task> = tasks_r.values()
                .filter(|t| t.project_id == project_id)
                .cloned()
                .collect();
            let _ = storage.save_project_tasks(project_id, &project_tasks);
        }
    }
}
