pub mod domain;
pub mod commands;

pub mod test_helpers;
mod acp;
mod agents;
pub mod config;
mod session;
mod queue;
mod pty;
mod worktree;
mod main_window;
mod storage_accounting;
/// What GitHub last said about the pull requests linked to tasks.
pub mod pr_status;
/// Building `AppState` once, for whichever front end wants it.
pub mod lifecycle;
pub mod conversation_actions;
pub mod conversation_executions;
pub mod coordinator_reads;
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

/// Set by the updater when the application should come back up after the
/// ownership-preserving shutdown instead of staying down.
pub struct RestartRequested(pub Arc<std::sync::atomic::AtomicBool>);

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
    pub conversation: commands::conversation::ConversationState,
    pub agent: commands::agent::AgentState,
    pub session: commands::session::SessionState,
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
    /// Per-task guard serializing one task's lifecycle transitions. Shared with
    /// the queue executor and the IPC handlers, so a card dragged in the app, a
    /// `slashit` command and an agent starting all queue behind the same lease.
    pub task_lifecycle_locks: Arc<lifecycle::TaskLifecycleLocks>,
    /// How much disk SlashIt uses. Measured only when asked, one
    /// measurement at a time.
    pub storage_accounting: Arc<storage_accounting::StorageAccounting>,
    /// Whether there is disk space to begin new task executions. Shared with
    /// the executor and the IPC handlers, so every way of starting a task
    /// asks the same question.
    pub start_guard: Arc<queue::start_guard::StartGuard>,
    /// The one owner of pull request status: the executor's background poll
    /// and the refresh commands all ask GitHub through it, and the board
    /// reads its cache.
    pub pr_statuses: Arc<pr_status::PrStatuses>,
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
    // Ownership first, before a single byte of shared state is read.
    //
    // The GUI and `slashitd` are separate processes, so the in-process guard
    // that serialises `config.toml` writes cannot exclude them from each
    // other; the OS-authenticated control channel is the only thing that can.
    // It has to be claimed here rather than after the window is built, because
    // hydration is not passive: it requeues tasks left `InProgress` — which,
    // next to a live daemon, means tasks that are running right now — and
    // writes that back to disk. Losing the race after doing so would corrupt
    // the running instance's board and then start a second executor against
    // it.
    let paths = match config::paths::AppPaths::new() {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("SlashIt: cannot resolve application paths: {e}");
            std::process::exit(1);
        }
    };
    let ipc_config = slashit_ipc::IpcConfig::load(&paths.ipc_config_file());
    let bound = match tauri::async_runtime::block_on(ipc::BoundIpc::bind(&paths, &ipc_config)) {
        Ok(bound) => bound,
        // Refusing to start is the whole point. Previously this failure was
        // printed to a terminal a desktop user never sees, and the app carried
        // on with its own executor and no control channel.
        Err(e) => {
            eprintln!(
                "SlashIt: another SlashIt instance (desktop app or slashitd) is already running \
                 and owns the control channel, so this one is exiting rather than operating on \
                 the same data: {e}"
            );
            std::process::exit(1);
        }
    };

    // Hydration is async because it takes tokio locks, and a blocking
    // acquisition panics on a runtime thread. `block_on` here is safe: the
    // Tauri event loop has not started, so nothing is waiting on this thread.
    let (app_state, report) = tauri::async_runtime::block_on(app_core::build_state())
        .expect("Failed to build application state");

    // Desktop and daemon share the same cooperative poller-stop signal. The
    // desktop intercepts Tauri's exit request and lets the IPC owner release
    // that signal only after its drain completes.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let restart_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The exclusive endpoint claim, parked here after the drain and dropped
    // only at `RunEvent::Exit`, so no in-flight command can still be writing
    // when a successor becomes the owner.
    let ownership_slot: Arc<std::sync::Mutex<Option<ipc::IpcOwnership>>> =
        Arc::new(std::sync::Mutex::new(None));

    let cleanup_failures = report
        .project_conversation_cleanup_failures
        .iter()
        .map(|failure| format!("Project Conversation cleanup: {failure}"))
        .chain(
            report
                .project_tasks_cleanup_failures
                .iter()
                .map(|failure| format!("Project Task cleanup: {failure}")),
        )
        .collect::<Vec<_>>();
    if cleanup_failures.is_empty() {
        println!(
            "SlashIt: Loaded {} repositories, {} projects, {} tasks from disk",
            report.repositories, report.projects, report.tasks
        );
    } else {
        eprintln!(
            "SlashIt: Startup incomplete: loaded {} repositories, {} projects, {} tasks; {} cleanup recovery issue(s) need attention",
            report.repositories,
            report.projects,
            report.tasks,
            cleanup_failures.len()
        );
        for failure in &cleanup_failures {
            eprintln!("SlashIt: Pending cleanup: {failure}");
        }
    }
    if report.migrated_projects > 0 {
        if report.unsaved_migrated_projects > 0 {
            println!(
                "SlashIt: Migrated tasks in {} project(s); {} could not be saved to disk yet and will be retried automatically",
                report.migrated_projects, report.unsaved_migrated_projects
            );
        }
        if report.unconfirmed_migrated_projects > 0 {
            println!(
                "SlashIt: Migrated tasks in {} project(s); {} were saved but SlashIt could not confirm that reached stable storage (a restart repeats the same reconciliation either way)",
                report.migrated_projects, report.unconfirmed_migrated_projects
            );
        }
        if report.unsaved_migrated_projects == 0 && report.unconfirmed_migrated_projects == 0 {
            println!(
                "SlashIt: Migrated tasks in {} project(s) and saved to disk",
                report.migrated_projects
            );
        }
    }

    // Acceptance builds only. The plugin embeds an HTTP server able to drive
    // this webview, so it is compiled out entirely unless the crate is built
    // with `--features acceptance-webdriver` — which no normal or release
    // build does. Two whole bindings rather than one `mut` and a `cfg` block,
    // so neither configuration needs a lint exception.
    #[cfg(feature = "acceptance-webdriver")]
    let builder = tauri::Builder::default().plugin(tauri_plugin_wdio_webdriver::init());
    #[cfg(not(feature = "acceptance-webdriver"))]
    let builder = tauri::Builder::default();

    builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(app_state)
        .manage(RestartRequested(restart_requested.clone()))
        .setup({
            let shutdown_rx = shutdown_rx.clone();
            let shutdown_complete = shutdown_complete.clone();
            let restart_requested = restart_requested.clone();
            let ownership_slot = ownership_slot.clone();
            move |app| {
            // The main window first, so nothing below runs before its boundary exists.
            main_window::create(app)?;
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
                    lifecycle: state.task_lifecycle_locks.clone(),
                    start_guard: state.start_guard.clone(),
                    pr_statuses: state.pr_statuses.clone(),
                },
            ));
            let _ = state.executor.set(executor.clone());
            // Tauri's runtime, not `tokio::spawn`: this closure runs on the
            // main thread before the event loop starts, with no reactor in
            // thread-local scope. Same idiom as the IPC server below.
            let polling_handle = tauri::async_runtime::spawn(
                executor.polling_loop(Some(shutdown_rx.clone())),
            );
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
                    project_gate: state.task.project_gate.clone(),
                    events: events.clone(),
                    control: control.clone(),
                    features: state.features.clone(),
                    repositories: state.repository.repositories.clone(),
                    worktree_manager: state.worktree_manager.clone(),
                    task_lifecycle_locks: state.task_lifecycle_locks.clone(),
                    executor: state.executor.clone(),
                    feature_diagnostics: None,
                    paths: state.paths.clone(),
                    start_guard: state.start_guard.clone(),
                });
                // Serving the endpoints claimed before hydration. Nothing is
                // bound here, so this can no longer fail because another
                // instance took them in the meantime — that race was decided
                // at the top of `run()`, while there was still nothing to
                // corrupt.
                let app_handle = app.handle().clone();
                // The claim on the endpoints stays with this supervising task,
                // not with the server task inside it, and is released only
                // after the executor is quiescent.
                let ownership = bound.ownership();
                let poller = ipc::poller_join(polling_handle);
                let handlers = ipc::HandlerRescue::default();
                let supervision = ipc::Supervision {
                    shutdown: shutdown_rx.clone(),
                    poller: Some(poller.clone()),
                    handlers: handlers.clone(),
                    // The user already confirmed quitting with work running
                    // (`request_quit`), so running agents are cancelled and
                    // reaped, not waited for.
                    finish_grace: std::time::Duration::ZERO,
                };
                let executor_cell = state.executor.clone();
                let agent_clients = state.agent.running_clients.clone();
                tauri::async_runtime::spawn(async move {
                    let server = tauri::async_runtime::spawn(async move {
                        bound.attach(ipc_ctx).serve_until(supervision).await
                    });
                    let result = match server.await {
                        Ok(result) => result,
                        Err(error) => {
                            eprintln!("SlashIt: the IPC supervisor task failed: {error}");
                            ipc::quiesce_after_supervisor_failure(
                                executor_cell.get().map(|executor| executor.as_ref()),
                                Some(poller),
                                &handlers,
                            )
                            .await;
                            Err(anyhow::anyhow!("the IPC supervisor task failed: {error}"))
                        }
                    };
                    if let Err(error) = &result {
                        eprintln!("SlashIt: the instance failed closed and is exiting: {error}");
                    }
                    // Legacy ACP agents belong to the app state, not the
                    // executor; the fence is closed, so none can be added.
                    let clients: Vec<_> = agent_clients.write().await.drain().collect();
                    for (_, client) in clients {
                        let _ = client.kill().await;
                    }
                    *ownership_slot.lock().unwrap() = Some(ownership);
                    shutdown_complete.store(true, std::sync::atomic::Ordering::Release);
                    // An update restart waits for the same drain as a quit.
                    if result.is_ok()
                        && restart_requested.load(std::sync::atomic::Ordering::SeqCst)
                    {
                        // Windows installs here, after the drain: the
                        // installer step ends the process.
                        if let Err(error) =
                            commands::updater::install_staged_after_drain(&app_handle).await
                        {
                            eprintln!("SlashIt: the staged update could not be installed: {error}");
                        }
                        app_handle.restart();
                    }
                    app_handle.exit(if result.is_ok() { 0 } else { 1 });
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
            }
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
            get_project_readiness,
            set_project_base,
            set_project_base_source,
            detect_remote_default_branch,
            preview_vcs_initialization,
            initialize_project_vcs,
            create_project,
            list_projects,
            get_project_conversation,
            get_project_conversation_executions,
            open_project_conversation,
            send_project_message,
            retry_project_conversation_continuation,
            act_on_project_conversation,
            stop_project_conversation,
            get_project,
            delete_project,
            update_project,
            get_project_path,
            attach_project_to_workspace,
            detach_project_from_workspace,
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
            get_task_diff,
            get_task_diff_stat,
            get_queue_config,
            update_queue_config,
            add_to_queue,
            bulk_add_to_queue,
            get_queue_position,
            promote_next_task,
            run_queued_task,
            get_queue_capacity,
            get_in_progress_count,
            requeue_task,
            submit_qa_review,
            get_qa_history,
            check_recurring_issues,
            approve_task,
            complete_task_locally,
            approve_task_again,
            get_approval_status,
            get_worker_recovery_evidence,
            accept_worker_result,
            get_new_attempt_evidence,
            authorize_new_attempt,
            start_new_attempt,
            get_integration_preview,
            integrate_task_locally,
            complete_and_integrate_task,
            reconfirm_approved_tip,
            resume_local_integration,
            discard_local_integration,
            create_approved_task_pr,
            request_task_changes,
            get_attention_summary,
            get_storage_usage,
            refresh_storage_usage,
            get_new_work_pause,
            scan_orphans,
            list_task_board_diagnostics,
            reclaim_orphan_checkout,
            reclaim_orphan_branch,
            create_worktree,
            cleanup_worktree,
            check_worktree_exists,
            create_pr,
            get_published_restack_status,
            restack_published_task,
            discard_published_restack,
            get_pr_availability,
            bulk_create_prs,
            sync_existing_pr,
            find_pr_candidates,
            get_pr_push_recovery,
            recover_private_email_and_create_pr,
            list_pr_statuses,
            refresh_pr_status,
            analyze_pr_comments,
            address_pr_review,
            sync_pr_review_replies,
            discuss_pr_review_questions,
            refresh_task_pr_state,
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
            get_task_run,
            get_live_runs,
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
        .run(move |app_handle, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                ownership_slot.lock().unwrap().take();
            }
            if let tauri::RunEvent::ExitRequested { api, .. } = &event {
                if !shutdown_complete.load(std::sync::atomic::Ordering::Acquire) {
                    api.prevent_exit();
                    let _ = shutdown_tx.send(true);
                }
            }
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
        let executor = state.executor.get().map(|executor| executor.as_ref());
        let agent_count =
            commands::agent::active_agent_count(&state.agent.executions, executor).await;
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
