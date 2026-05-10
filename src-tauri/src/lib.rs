pub mod domain;
pub mod commands;

pub mod test_helpers;
mod acp;
mod jj;
mod agents;
mod config;
mod session;
mod queue;
mod pty;
mod worktree;
mod ipc;

use commands::*;
use config::Storage;
use pty::PtyState;
use std::sync::Arc;
use tauri::{Emitter, Manager};

#[derive(Clone)]
pub struct AppState {
    pub repository: commands::repository::RepositoryState,
    pub project: commands::project::ProjectState,
    pub workspace: commands::workspace::WorkspaceState,
    pub task: commands::task::TaskState,
    pub agent: commands::agent::AgentState,
    pub session: commands::session::SessionState,
    pub jj: commands::jj::JjState,
    pub queue: commands::queue::QueueState,
    pub roadmap: commands::roadmap::RoadmapState,
    pub file: commands::file::FileState,
    pub github: commands::github::GithubState,
    pub changelog: commands::changelog::ChangelogState,
    pub mcp: commands::mcp::McpState,
    pub memory: commands::memory::MemoryState,
    pub appearance: commands::appearance::AppearanceState,
    pub pty: PtyState,
    pub storage: Storage,
    pub worktree_manager: Arc<worktree::WorktreeManager>,
    pub executor: Arc<tokio::sync::OnceCell<Arc<queue::TaskExecutor>>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let storage = Storage::new().expect("Failed to initialize storage");

    let task_state = commands::task::TaskState::new();
    let queue_state = commands::queue::QueueState::new(task_state.tasks.clone());
    let app_state = AppState {
        repository: commands::repository::RepositoryState::new(),
        project: commands::project::ProjectState::new(),
        workspace: commands::workspace::WorkspaceState::new().expect("Failed to initialize workspace state"),
        task: task_state,
        agent: commands::agent::AgentState::new(),
        session: commands::session::SessionState::new(),
        jj: commands::jj::JjState::new(),
        queue: queue_state,
        roadmap: commands::roadmap::RoadmapState::new(),
        file: commands::file::FileState::new(),
        github: commands::github::GithubState::new(),
        changelog: commands::changelog::ChangelogState::new(),
        mcp: commands::mcp::McpState::new(),
        memory: commands::memory::MemoryState::new(),
        appearance: commands::appearance::AppearanceState::new(),
        pty: PtyState::new(),
        storage,
        worktree_manager: Arc::new(worktree::WorktreeManager::new()),
        executor: Arc::new(tokio::sync::OnceCell::new()),
    };

    // Load persisted config from disk (synchronous at startup)
    // Load repositories FIRST, then projects (which reference repository_id), then tasks (which reference project_id)
    let loaded_config = app_state.storage.load_config().unwrap_or_else(|e| {
        eprintln!("Warning: Failed to load config from disk: {}", e);
        config::storage::AppConfig::default()
    });
    
    // Insert loaded repositories into repository state
    {
        let repositories_map = app_state.repository.repositories.clone();
        let mut repositories = repositories_map.blocking_write();
        for (id_str, mut repository) in loaded_config.repositories {
            if let Ok(id) = uuid::Uuid::parse_str(&id_str) {
                // Ensure the repository's id field matches the key
                repository.id = id;
                repositories.insert(id, repository);
            } else {
                eprintln!("Warning: Invalid repository UUID in config: {}", id_str);
            }
        }
        println!("SlashIt: Loaded {} repositories from disk", repositories.len());
    }
    
    // Insert loaded projects into project state
    {
        let projects_map = app_state.project.projects.clone();
        let mut projects = projects_map.blocking_write();
        for (id_str, mut project) in loaded_config.projects {
            if let Ok(id) = uuid::Uuid::parse_str(&id_str) {
                // Ensure the project's id field matches the key
                project.id = id;
                projects.insert(id, project);
            } else {
                eprintln!("Warning: Invalid project UUID in config: {}", id_str);
            }
        }
        println!("SlashIt: Loaded {} projects from disk", projects.len());
    }

    // Load persisted tasks from disk (synchronous at startup)
    let loaded_tasks = app_state.storage.load_all_tasks().unwrap_or_else(|e| {
        eprintln!("Warning: Failed to load tasks from disk: {}", e);
        Vec::new()
    });
    
    // Insert loaded tasks into task state synchronously at startup
    let mut migrated_projects = std::collections::HashSet::new();
    {
        let tasks_map = app_state.task.tasks.clone();
        let mut tasks = tasks_map.blocking_write();
        for mut task in loaded_tasks {
            let mut changed = false;

            // Migrate old model names to "default"
            if task.model.starts_with("claude-3") || task.model.starts_with("claude-2") {
                task.model = "default".to_string();
                changed = true;
            }

            // Reset orphaned InProgress/AiReview tasks back to Queue
            if matches!(task.status, domain::TaskStatus::InProgress | domain::TaskStatus::AiReview) {
                println!("SlashIt: Resetting orphaned task '{}' from {:?} back to Queue", task.title, task.status);
                task.status = domain::TaskStatus::Queue;
                changed = true;
            }

            // Fix inconsistent phase (e.g. status=Queue but phase=Failed)
            if matches!(task.status, domain::TaskStatus::Backlog | domain::TaskStatus::Queue)
                && task.phase != domain::TaskPhase::Idle
            {
                task.phase = domain::TaskPhase::Idle;
                task.phase_progress = 0;
                task.overall_progress = 0;
                task.error_message = None;
                changed = true;
            }

            if changed {
                migrated_projects.insert(task.project_id);
            }
            tasks.insert(task.id, task);
        }

        // Verify worktree paths still exist on disk (mark stale ones)
        for task in tasks.values_mut() {
            if let Some(wt_path) = task.worktree_path.as_ref() {
                if !std::path::Path::new(wt_path).exists() {
                    // Worktree dir was deleted externally — clear the reference
                    println!("SlashIt: Worktree dir missing for task '{}', clearing reference", task.title);
                    task.worktree_path = None;
                    migrated_projects.insert(task.project_id);
                }
            }
        }

        println!("SlashIt: Loaded {} tasks from disk", tasks.len());

        // Persist migrated tasks back to disk
        for project_id in &migrated_projects {
            let project_tasks: Vec<domain::Task> = tasks.values()
                .filter(|t| &t.project_id == project_id)
                .cloned()
                .collect();
            if let Err(e) = app_state.storage.save_project_tasks(*project_id, &project_tasks) {
                eprintln!("Warning: Failed to persist migrated tasks for project {}: {}", project_id, e);
            }
        }
        if !migrated_projects.is_empty() {
            println!("SlashIt: Migrated tasks in {} project(s) and saved to disk", migrated_projects.len());
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(app_state)
        .setup(|app| {
            let state: tauri::State<AppState> = app.state();
            let executor = Arc::new(queue::TaskExecutor::new(
                queue::executor::TaskExecutorConfig {
                    tasks: state.task.tasks.clone(),
                    queue_manager: state.queue.manager.clone(),
                    executions: state.agent.executions.clone(),
                    logs: state.agent.logs.clone(),
                    projects: state.project.projects.clone(),
                    repositories: state.repository.repositories.clone(),
                    storage: state.storage.clone(),
                    worktree_manager: state.worktree_manager.clone(),
                    app_handle: app.handle().clone(),
                },
            ));
            let _ = state.executor.set(executor.clone());
            executor.start_polling();
            println!("SlashIt: Task executor started");

            // IPC Unix socket server
            let ipc_ctx = ipc::IpcContext {
                tasks: state.task.tasks.clone(),
                projects: state.project.projects.clone(),
                executions: state.agent.executions.clone(),
                pty: state.pty.clone(),
                queue_manager: state.queue.manager.clone(),
                storage: state.storage.clone(),
                app_handle: app.handle().clone(),
            };
            tauri::async_runtime::spawn(async move {
                if let Err(e) = ipc::server::run(ipc_ctx).await {
                    eprintln!("SlashIt: IPC server error: {}", e);
                }
            });
            println!("SlashIt: IPC server starting");

            // System tray
            {
                use tauri::menu::{MenuBuilder, MenuItem};
                use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

                let toggle_i = MenuItem::with_id(app, "toggle", "Show/Hide SlashIt", true, None::<&str>)?;
                let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
                let menu = MenuBuilder::new(app).items(&[&toggle_i, &quit_i]).build()?;
                let tray_icon = tauri::include_image!("icons/32x32.png");

                let tray = TrayIconBuilder::new()
                    .icon(tray_icon)
                    .menu(&menu)
                    .tooltip("SlashIt")
                    .on_menu_event(move |app, event| match event.id.as_ref() {
                        "toggle" => toggle_window(app),
                        "quit" => request_quit(app),
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, event| {
                        if let TrayIconEvent::Click { button, button_state, .. } = event {
                            match (button, button_state) {
                                (MouseButton::Left, MouseButtonState::Up) => toggle_window(tray.app_handle()),
                                _ => {}
                            }
                        }
                    });

                #[cfg(not(target_os = "linux"))]
                let tray = tray.show_menu_on_left_click(false);

                let _tray = tray.build(app)?;

                println!("SlashIt: System tray initialized");
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            greet,
            create_repository,
            list_repositories,
            get_repository,
            create_project,
            list_projects,
            get_project,
            delete_project,
            update_project,
            get_project_path,
            create_workspace,
            list_workspaces,
            remove_workspace,
            get_workspace_status,
            create_task,
            list_tasks,
            update_task_status,
            set_task_dependencies,
            update_task_metadata,
            update_task_progress,
            add_subtask,
            toggle_subtask,
            link_github_issue,
            link_pr,
            add_external_ref,
            remove_external_ref,
            mark_task_stuck,
            unstick_task,
            update_task,
            delete_task,
            reorder_task,
            start_agent,
            stop_agent,
            get_agent_status,
            get_agent_logs,
            list_available_models,
            check_claude_cli,
            create_session,
            send_message,
            get_session_history,
            list_sessions,
            new_change,
            describe_change,
            abandon_change,
            jj_get_workspace_status,
            git_export,
            get_task_diff,
            get_task_diff_stat,
            get_queue_config,
            update_queue_config,
            add_to_queue,
            bulk_add_to_queue,
            get_queue_position,
            promote_next_task,
            get_queue_capacity,
            get_in_progress_count,
            requeue_task,
            submit_qa_review,
            get_qa_history,
            check_recurring_issues,
            submit_review,
            check_review_valid,
            get_review,
            create_worktree,
            cleanup_worktree,
            check_worktree_exists,
            create_pr,
            bulk_create_prs,
            sync_existing_pr,
            find_pr_candidates,
            get_pr_push_recovery,
            recover_private_email_and_create_pr,
            get_pr_status,
            analyze_pr_comments,
            address_pr_review,
            sync_pr_review_replies,
            discuss_pr_review_questions,
            refresh_task_pr_state,
            submit_stack,
            create_roadmap_feature,
            update_roadmap_feature,
            delete_roadmap_feature,
            list_roadmap_features,
            get_roadmap_feature,
            link_task_to_feature,
            unlink_task_from_feature,
            list_files,
            read_file,
            write_file,
            search_files,
            get_file_info,
            get_issues,
            get_issue,
            create_task_from_issue,
            import_github_issues,
            get_prs,
            get_pr,
            generate_changelog,
            get_git_history,
            compare_branches,
            list_mcp_servers,
            toggle_mcp_server,
            get_mcp_agents,
            configure_agent,
            search_memories,
            get_graph_status,
            store_memory,
            delete_memory,
            get_theme,
            set_theme,
            list_themes,
            get_appearance_mode,
            set_appearance_mode,
            get_use_project_rail,
            set_use_project_rail,
            pick_folder,
            check_is_git_repo,
            pty::spawn_pty,
            pty::write_pty,
            pty::resize_pty,
            pty::kill_pty,
            pty::list_pty_sessions,
            pty::get_pty_scrollback,
            pty::attach_pty_session,
            pty::write_to_all_ptys,
            execute_task,
            stop_task_execution,
            get_execution_status,
            get_task_output,
            commands::workflow::get_workflow_config,
            commands::workflow::update_workflow_config,
            commands::workflow::list_workflows,
            commands::workflow::get_workflow,
            commands::workflow::get_workflow_logs,
            commands::jira::check_acli_available,
            commands::jira::list_jira_projects,
            commands::jira::import_jira_issues,
            force_quit,
            get_active_process_count,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::CloseRequested { api, .. },
                ..
            } = &event
            {
                if label == "main" {
                    api.prevent_close();
                    if let Some(w) = app_handle.get_webview_window("main") {
                        let _ = w.hide();
                    }
                }
            }
        });
}

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

fn show_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn toggle_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
}

fn request_quit(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let pty_count = state.pty.sessions.blocking_lock().len();
    let agent_count = {
        let execs = state.agent.executions.blocking_read();
        execs
            .values()
            .filter(|e| {
                matches!(
                    e.status,
                    crate::domain::AgentStatus::Running | crate::domain::AgentStatus::Starting
                )
            })
            .count()
    };

    if pty_count == 0 && agent_count == 0 {
        app.exit(0);
        return;
    }

    show_window(app);
    let _ = app.emit(
        "quit-requested",
        serde_json::json!({
            "pty_count": pty_count,
            "agent_count": agent_count,
        }),
    );
}
