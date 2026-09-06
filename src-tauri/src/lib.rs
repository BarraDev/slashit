pub mod domain;
pub mod commands;

pub mod test_helpers;
mod acp;
mod jj;
mod agents;
pub mod config;
mod session;
mod queue;
mod pty;
mod worktree;
/// Building `AppState` once, for whichever front end wants it.
pub mod app_core;
/// Headless execution, sharing the whole stack with the GUI.
pub mod daemon;
/// Transport-neutral event emission, so the backend does not need a webview.
pub mod events;
/// What kind of process this is, and the few operations that differ.
pub mod instance;
// The control channel. No longer Unix-gated: the transport layer now provides
// a Windows named pipe alongside the Unix socket, so the module compiles and
// works on every supported platform.
pub mod ipc;

use commands::*;
use config::Storage;
use pty::PtyState;
use std::sync::Arc;
use tauri::Manager;

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
    pub updater: commands::updater::UpdaterState,
    pub pty: PtyState,
    pub storage: Storage,
    /// Where backend events go.
    ///
    /// A `OnceLock` because the sink is not knowable when the state is built:
    /// the GUI's sink needs an `AppHandle`, which only exists once Tauri has
    /// set up. The daemon installs its sink immediately. Until one is
    /// installed, emitting is a no-op rather than a panic — losing a progress
    /// event during startup is not worth aborting over.
    pub events: Arc<std::sync::OnceLock<events::SharedEventSink>>,
    /// The one resolved set of application directories. Commands must read
    /// paths from here rather than deriving their own.
    pub paths: Arc<config::paths::AppPaths>,
    /// Runtime feature flags, so unfinished work can ship dark.
    pub features: Arc<tokio::sync::RwLock<config::features::FeatureFlags>>,
    pub worktree_manager: Arc<worktree::WorktreeManager>,
    pub executor: Arc<tokio::sync::OnceCell<Arc<queue::TaskExecutor>>>,
    /// Per-project guard serializing `apply_state_migration` and
    /// `set_state_location` for the same project. See its doc comment.
    pub state_location_locks: Arc<commands::state_location::StateLocationLocks>,
}

impl AppState {
    /// The installed event sink, or a discarding one if none is installed yet.
    pub fn events(&self) -> events::SharedEventSink {
        self.events.get().cloned().unwrap_or_else(events::null_sink)
    }

    /// Install the sink for this process. The first call wins.
    pub fn set_event_sink(&self, sink: events::SharedEventSink) {
        if self.events.set(sink).is_err() {
            eprintln!("[events] a sink was already installed; ignoring the second one");
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Hydration is async because it takes tokio locks, and a blocking
    // acquisition panics on a runtime thread. `block_on` here is safe: the
    // Tauri event loop has not started, so nothing is waiting on this thread.
    let (app_state, report) = tauri::async_runtime::block_on(app_core::build_state())
        .expect("Failed to build application state");

    println!(
        "SlashIt: Loaded {} repositories, {} projects, {} tasks from disk",
        report.repositories, report.projects, report.tasks
    );
    if report.migrated_projects > 0 {
        if report.unsaved_migrated_projects > 0 {
            println!(
                "SlashIt: Migrated tasks in {} project(s); {} could not be saved to disk yet and will be retried automatically",
                report.migrated_projects, report.unsaved_migrated_projects
            );
        } else {
            println!(
                "SlashIt: Migrated tasks in {} project(s) and saved to disk",
                report.migrated_projects
            );
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(app_state)
        .setup(|app| {
            let state: tauri::State<AppState> = app.state();

            // The webview exists now, so the sink the rest of the backend
            // emits through can finally be installed.
            let events: events::SharedEventSink =
                Arc::new(events::TauriEventSink::new(app.handle().clone()));
            state.set_event_sink(events.clone());

            let control: instance::SharedInstanceControl = Arc::new(GuiControl {
                handle: app.handle().clone(),
            });

            let executor = Arc::new(queue::TaskExecutor::new(
                queue::executor::TaskExecutorConfig {
                    tasks: state.task.tasks.clone(),
                    queue_manager: state.queue.manager.clone(),
                    executions: state.agent.executions.clone(),
                    logs: state.agent.logs.clone(),
                    projects: state.project.projects.clone(),
                    repositories: state.repository.repositories.clone(),
                    workspace_registry: state.workspace.registry.clone(),
                    storage: state.storage.clone(),
                    worktree_manager: state.worktree_manager.clone(),
                    events: events.clone(),
                },
            ));
            let _ = state.executor.set(executor.clone());
            executor.start_polling(None);
            println!("SlashIt: Task executor started");

            // The control channel. The same server the daemon runs; only the
            // sink and the window control differ.
            {
                let ipc_ctx = Arc::new(ipc::IpcContext {
                    tasks: state.task.tasks.clone(),
                    projects: state.project.projects.clone(),
                    executions: state.agent.executions.clone(),
                    pty: state.pty.clone(),
                    queue_manager: state.queue.manager.clone(),
                    storage: state.storage.clone(),
                    events: events.clone(),
                    control: control.clone(),
                    features: state.features.clone(),
                    feature_diagnostics: None,
                    paths: state.paths.clone(),
                });
                let ipc_config = slashit_ipc::IpcConfig::load(&state.paths.ipc_config_file());

                tauri::async_runtime::spawn(async move {
                    if let Err(e) = ipc::serve(ipc_ctx, ipc_config).await {
                        eprintln!("SlashIt: IPC server error: {e}");
                    }
                });
                println!("SlashIt: IPC server starting");
            }

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
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = event
                        {
                            toggle_window(tray.app_handle());
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
            get_state_location,
            set_state_location,
            plan_state_migration,
            apply_state_migration,
            clean_legacy_state_dirs,
            get_feature_flags,
            set_feature_flag,
            describe_feature_flags,
            updater_status,
            updater_check,
            updater_download_and_install,
            updater_restart,
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
            get_workspace,
            delete_workspace,
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

/// Ask to quit, confirming first when work would be lost.
///
/// Spawns rather than blocking. The counts come from tokio locks, and this is
/// reachable both from the tray menu (the event-loop thread, where a blocking
/// acquisition is legal) and from an IPC `Quit` (a runtime worker, where the
/// same call would panic). Doing the work on the runtime is correct from
/// either caller.
fn request_quit(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        request_quit_inner(&app).await;
    });
}

async fn request_quit_inner(app: &tauri::AppHandle) {
    let (pty_count, agent_count) = {
        let state = app.state::<AppState>();
        let pty_count = state.pty.sessions.lock().await.len();
        let agent_count = state
            .agent
            .executions
            .read()
            .await
            .values()
            .filter(|e| {
                matches!(
                    e.status,
                    crate::domain::AgentStatus::Running | crate::domain::AgentStatus::Starting
                )
            })
            .count();
        (pty_count, agent_count)
    };

    if pty_count == 0 && agent_count == 0 {
        app.exit(0);
        return;
    }

    // Something is still running, so surface the window and let the frontend
    // ask before anything is killed.
    show_window(app);
    let state = app.state::<AppState>();
    state.events().emit_json(
        "quit-requested",
        serde_json::json!({
            "pty_count": pty_count,
            "agent_count": agent_count,
        }),
    );
}

/// The GUI's answer to the operations that differ between front ends.
struct GuiControl {
    handle: tauri::AppHandle,
}

impl instance::InstanceControl for GuiControl {
    fn mode(&self) -> instance::InstanceMode {
        instance::InstanceMode::Gui
    }

    fn show_window(&self) -> Result<(), String> {
        show_window(&self.handle);
        Ok(())
    }

    fn request_quit(&self) {
        request_quit(&self.handle);
    }
}
