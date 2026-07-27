//! Headless execution: the IPC server and the queue executor, with no window.
//!
//! The daemon is not a second implementation of SlashIt. It calls the same
//! [`crate::app_core::build_state`] the desktop app calls, constructs the same
//! `TaskExecutor`, and serves the same IPC command set. The only differences
//! are the [`crate::events::EventSink`] (stderr instead of a webview) and the
//! [`crate::instance::InstanceControl`] (no window to show).

use std::sync::Arc;

use crate::app_core::build_state;
use crate::events::{headless_sink, SharedEventSink};
use crate::instance::{InstanceControl, InstanceMode};
use crate::queue;

/// How the daemon was asked to run.
#[derive(Debug, Clone, Default)]
pub struct DaemonOptions {
    /// Log every event, including per-chunk agent output.
    pub verbose: bool,
    /// Raw `name=value` feature overrides, highest precedence.
    ///
    /// Left unparsed so the one parser in `config::features` validates them —
    /// a second parser here would be a second place for the accepted spellings
    /// to drift.
    pub feature_overrides: Vec<String>,
}

/// Lets the IPC layer ask the daemon to stop without knowing what a daemon is.
struct DaemonControl {
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl InstanceControl for DaemonControl {
    fn mode(&self) -> InstanceMode {
        InstanceMode::Daemon
    }

    fn show_window(&self) -> Result<(), String> {
        // Reporting success here would be a lie that a `slashit show` user
        // cannot detect. There is no window.
        Err("this instance is a headless daemon and has no window".to_string())
    }

    fn request_quit(&self) {
        // The receiver side finishes the in-flight response before the process
        // exits, so a client always gets its answer to `slashit quit`.
        let _ = self.shutdown.send(true);
    }
}

/// Records this process as the daemon, and cleans up on the way out.
///
/// The PID file is informational: the authoritative single-instance guarantee
/// is endpoint ownership in the transport layer, which refuses to bind an
/// endpoint a live instance already holds. A PID file cannot provide that —
/// it goes stale on a hard kill, and acting on a stale one would let a second
/// daemon start and silently steal the first one's clients.
struct PidFile {
    path: std::path::PathBuf,
}

impl PidFile {
    fn write(path: std::path::PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(Self { path })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        // Best effort: a leftover PID file is harmless because nothing trusts
        // it for exclusion.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Run until a signal arrives or a client asks to quit.
pub async fn run(options: DaemonOptions) -> anyhow::Result<()> {
    let events: SharedEventSink = headless_sink(options.verbose);

    let (state, report) = build_state().await?;
    println!(
        "slashitd: loaded {} repositories, {} projects, {} tasks",
        report.repositories, report.projects, report.tasks
    );
    if report.migrated_projects > 0 {
        println!(
            "slashitd: migrated tasks in {} project(s); adopted {} worktree(s), cleared {}",
            report.migrated_projects, report.adopted_worktrees, report.cleared_worktrees
        );
    }

    // CLI overrides sit above the environment and the config file, so the full
    // stack is re-resolved rather than the top layer being poked into the
    // already-resolved set. Nothing is written back: an override is for this
    // run, not a change to the operator's configuration.
    if !options.feature_overrides.is_empty() {
        let persisted = crate::config::features::FeatureFlags::load(&state.paths);
        let resolved = crate::config::features::FeatureResolver::new()
            .with_config(persisted)
            .with_process_env()
            .with_cli_args(&options.feature_overrides)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .resolve_and_report();

        for flag in resolved.to_flag_info() {
            if flag.source == "cli" {
                println!(
                    "slashitd: feature `{}` = {} (command line)",
                    flag.name, flag.value
                );
            }
        }

        let mut flags = state.features.write().await;
        resolved.apply_to(&mut flags);
    }

    let _pid_file = PidFile::write(state.paths.pid_file())?;

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
    executor.start_polling();
    println!("slashitd: queue executor started");

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let control = Arc::new(DaemonControl {
        shutdown: shutdown_tx,
    });

    let ipc_config = slashit_ipc::IpcConfig::load(&state.paths.ipc_config_file());

    let ctx = Arc::new(crate::ipc::IpcContext {
        tasks: state.task.tasks.clone(),
        projects: state.project.projects.clone(),
        executions: state.agent.executions.clone(),
        pty: state.pty.clone(),
        queue_manager: state.queue.manager.clone(),
        storage: state.storage.clone(),
        events: events.clone(),
        control: control.clone(),
        features: state.features.clone(),
        paths: state.paths.clone(),
    });

    let mut server = tokio::spawn({
        let ctx = ctx.clone();
        let ipc_config = ipc_config.clone();
        async move { crate::ipc::serve(ctx, ipc_config).await }
    });

    println!("slashitd: ready (pid {})", std::process::id());

    // Whichever comes first: an operator signal, a client asking to quit, or
    // the control channel dying.
    //
    // That last arm matters. A daemon whose IPC server has stopped is a
    // process nobody can talk to: it still runs queued agents, but no CLI can
    // reach it and no operator can stop it short of a signal. Logging and
    // carrying on would leave exactly that, so it is fatal.
    let outcome = tokio::select! {
        reason = wait_for_signal() => {
            println!("slashitd: {reason}, shutting down");
            Ok(())
        }
        _ = shutdown_rx.changed() => {
            println!("slashitd: quit requested over IPC, shutting down");
            Ok(())
        }
        result = &mut server => match result {
            Ok(Ok(())) => Err(anyhow::anyhow!("the IPC server stopped on its own")),
            Ok(Err(e)) => Err(anyhow::anyhow!("the IPC server failed: {e}")),
            Err(e) => Err(anyhow::anyhow!("the IPC server task panicked: {e}")),
        },
    };

    // Stop accepting new work before waiting on what is running, so the
    // in-flight set cannot grow while we drain it.
    server.abort();

    shutdown(&executor).await;
    println!("slashitd: stopped");
    outcome
}

/// Let running agents finish, within a bound.
///
/// A hard abort would leave a task marked `in_progress` with no process behind
/// it — recoverable, because startup returns orphaned tasks to the queue, but
/// it discards work the agent had already done.
async fn shutdown(executor: &Arc<queue::TaskExecutor>) {
    const GRACE: std::time::Duration = std::time::Duration::from_secs(30);

    let running = executor.running_task_count().await;
    if running == 0 {
        return;
    }

    println!("slashitd: waiting up to {GRACE:?} for {running} running task(s)");
    let deadline = tokio::time::Instant::now() + GRACE;
    while tokio::time::Instant::now() < deadline {
        if executor.running_task_count().await == 0 {
            println!("slashitd: all tasks finished");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let remaining = executor.running_task_count().await;
    // Said plainly rather than silently: these tasks return to the queue on
    // the next start, and the operator should know why.
    println!(
        "slashitd: {remaining} task(s) still running after the grace period; \
         they will be requeued on the next start"
    );
}

#[cfg(unix)]
async fn wait_for_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};

    // A daemon under systemd gets SIGTERM; one started from a shell gets
    // SIGINT. Both mean stop.
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("slashitd: cannot listen for SIGTERM: {e}");
            return "signal handling unavailable";
        }
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("slashitd: cannot listen for SIGINT: {e}");
            return "signal handling unavailable";
        }
    };

    tokio::select! {
        _ = term.recv() => "received SIGTERM",
        _ = int.recv() => "received SIGINT",
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() -> &'static str {
    match tokio::signal::ctrl_c().await {
        Ok(()) => "received Ctrl-C",
        Err(e) => {
            eprintln!("slashitd: cannot listen for Ctrl-C: {e}");
            "signal handling unavailable"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daemon_reports_its_mode_and_refuses_to_show_a_window() {
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let control = DaemonControl { shutdown: tx };

        assert_eq!(control.mode(), InstanceMode::Daemon);
        let err = control.show_window().expect_err("a daemon has no window");
        assert!(
            err.contains("headless"),
            "message should explain why: {err}"
        );
    }

    #[tokio::test]
    async fn requesting_quit_trips_the_shutdown_signal() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let control = DaemonControl { shutdown: tx };

        assert!(!*rx.borrow());
        control.request_quit();
        rx.changed().await.unwrap();
        assert!(*rx.borrow());
    }

    #[test]
    fn the_pid_file_holds_this_process_and_is_removed_on_drop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("run").join("slashitd.pid");

        {
            let _pid = PidFile::write(path.clone()).unwrap();
            let written = std::fs::read_to_string(&path).unwrap();
            assert_eq!(written, std::process::id().to_string());
        }

        assert!(
            !path.exists(),
            "a stopped daemon should not leave its PID file behind"
        );
    }
}
