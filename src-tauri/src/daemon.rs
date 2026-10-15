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

    // Ownership first, before a single byte of shared state is read.
    //
    // Hydration is not passive: it requeues tasks left `InProgress` by what it
    // assumes was a crash, reconciles worktree references, and writes both
    // back to disk. Doing that while another instance is live corrupts the
    // running instance's board rather than merely duplicating work, and
    // starting the executor afterwards would spawn a second agent on the same
    // branch. Claiming the endpoint here means a loser exits having touched
    // nothing shared at all.
    let paths = crate::config::paths::AppPaths::new()
        .map_err(|e| anyhow::anyhow!("cannot resolve application paths: {e}"))?;
    let ipc_config = slashit_ipc::IpcConfig::load(&paths.ipc_config_file());
    let bound = crate::ipc::BoundIpc::bind(&paths, &ipc_config).await?;

    let (state, report) = build_state().await?;
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
            "slashitd: loaded {} repositories, {} projects, {} tasks",
            report.repositories, report.projects, report.tasks
        );
    } else {
        eprintln!(
            "slashitd: startup incomplete: loaded {} repositories, {} projects, {} tasks; {} cleanup recovery issue(s) need attention",
            report.repositories,
            report.projects,
            report.tasks,
            cleanup_failures.len()
        );
        for failure in &cleanup_failures {
            eprintln!("slashitd: pending cleanup: {failure}");
        }
    }
    if report.migrated_projects > 0 {
        println!(
            "slashitd: migrated tasks in {} project(s); adopted {} worktree(s), cleared {}",
            report.migrated_projects, report.adopted_worktrees, report.cleared_worktrees
        );
        if report.unsaved_migrated_projects > 0 {
            println!(
                "slashitd: {} project(s) could not be saved to disk yet and will be retried automatically",
                report.unsaved_migrated_projects
            );
        }
        if report.unconfirmed_migrated_projects > 0 {
            println!(
                "slashitd: {} project(s) were saved but SlashIt could not confirm that reached stable storage (a restart repeats the same reconciliation either way)",
                report.unconfirmed_migrated_projects
            );
        }
    }

    // CLI overrides sit above the environment and the config file, so the full
    // stack is re-resolved rather than the top layer being poked into the
    // already-resolved set. Nothing is written back: an override is for this
    // run, not a change to the operator's configuration.
    //
    // `feature_diagnostics` keeps the full resolution — including which flags
    // the CLI decided — for `handle_features` below. `state.features` alone
    // cannot answer that later: it is a plain `FeatureFlags`, so a value the
    // CLI chose is indistinguishable from one the config file happened to
    // already hold, and `FeatureFlags::diagnostics()` would re-attribute it to
    // whichever of `config`/`env` matches instead.
    let feature_diagnostics = if options.feature_overrides.is_empty() {
        None
    } else {
        let persisted = crate::config::features::FeatureFlags::load(&state.paths);
        let resolved = crate::config::features::FeatureResolver::new()
            .with_config(persisted)
            .with_process_env()
            .with_cli_args(&options.feature_overrides)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .resolve_and_report();

        let info = resolved.to_flag_info();
        for flag in &info {
            if flag.source == "cli" {
                println!(
                    "slashitd: feature `{}` = {} (command line)",
                    flag.name, flag.value
                );
            }
        }

        let mut flags = state.features.write().await;
        resolved.apply_to(&mut flags);
        drop(flags);

        Some(info)
    };

    let pid_file = PidFile::write(state.paths.pid_file())?;

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

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let polling_handle = tokio::spawn(executor.polling_loop(Some(shutdown_tx.subscribe())));
    println!("slashitd: queue executor started");

    let control = Arc::new(DaemonControl {
        shutdown: shutdown_tx.clone(),
    });

    let ctx = Arc::new(crate::ipc::IpcContext {
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
        feature_diagnostics,
        paths: state.paths.clone(),
        start_guard: state.start_guard.clone(),
    });

    // Serving the endpoints claimed at the top of this function. Nothing is
    // bound here, so there is no second chance for another instance to have
    // taken them in the meantime.
    //
    // This function, not the server task, holds the claim on those endpoints
    // (`ownership`). The server task drains the executor before it lets go of
    // its own reference, and this function lets go of the last one only after
    // the executor is quiescent, so even a server task that dies without
    // draining cannot release the instance under a running agent.
    let ownership = bound.ownership();
    let poller = crate::ipc::poller_join(polling_handle);
    let handlers = crate::ipc::HandlerRescue::default();
    let supervision = crate::ipc::Supervision {
        shutdown: shutdown_tx.subscribe(),
        poller: Some(poller.clone()),
        handlers: handlers.clone(),
        finish_grace: SHUTDOWN_GRACE,
    };
    let mut server = tokio::spawn({
        let ctx = ctx.clone();
        async move { bound.attach(ctx).serve_until(supervision).await }
    });

    println!("slashitd: ready (pid {})", std::process::id());

    // Whichever comes first: an operator signal, a client asking to quit, or
    // the control channel dying.
    //
    // That last arm matters. A daemon whose IPC server has stopped is a
    // process nobody can talk to: it still runs queued agents, but no CLI can
    // reach it and no operator can stop it short of a signal. Logging and
    // carrying on would leave exactly that, so it is fatal. The server drains
    // before it reports that, so this arm never races a release of ownership.
    let mut server_result = None;
    let outcome = tokio::select! {
        reason = wait_for_signal() => signal_outcome(reason),
        _ = shutdown_rx.changed() => {
            println!("slashitd: quit requested over IPC, shutting down");
            Ok(())
        }
        result = &mut server => {
            let outcome = match &result {
                Ok(Ok(())) => Err(anyhow::anyhow!("the IPC server stopped on its own")),
                Ok(Err(e)) => Err(anyhow::anyhow!("the IPC server failed: {e}")),
                Err(e) => Err(anyhow::anyhow!("the IPC server task panicked: {e}")),
            };
            server_result = Some(result);
            outcome
        },
    };

    // Intentional shutdown: tell the server (and through it the poller) to
    // stop, then wait for it. It returns only after handlers are joined, the
    // poller has returned and every execution owner is reaped.
    let _ = shutdown_tx.send(true);
    let server_result = match server_result {
        Some(result) => result,
        None => (&mut server).await,
    };
    let drained = match server_result {
        Ok(Ok(())) => Ok(()),
        // Drained, but the cause was a failure: not a clean stop.
        Ok(Err(error)) => Err(error.context("the IPC server failed closed")),
        // The supervisor itself died, possibly before draining. Ownership is
        // still ours, so finish the drain here before releasing it.
        Err(error) => {
            eprintln!("slashitd: the IPC supervisor task failed ({error}); draining here");
            crate::ipc::quiesce_after_supervisor_failure(Some(&executor), Some(poller), &handlers)
                .await;
            Err(anyhow::anyhow!("the IPC supervisor task failed: {error}"))
        }
    };

    // The last reference to the exclusive local resource. Nothing here can
    // still start or finish work.
    // The PID file goes first: once the endpoint is released a successor may
    // write its own, and ours must not delete it.
    drop(pid_file);
    drop(ownership);
    println!("slashitd: stopped");
    drained.and(outcome)
}

/// How long an intentional shutdown lets running agents finish on their own
/// before they are cancelled and reaped.
///
/// A cancelled agent's task stays `in_progress`, which startup recovery
/// returns to the queue; letting it finish keeps the work it had already done.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Turn a `wait_for_signal` result into the daemon's overall outcome.
///
/// Pulled out of the `tokio::select!` arm so the one decision that matters --
/// a registration failure must never read as a normal shutdown -- is testable
/// on its own, without needing to actually exhaust the OS's signal-handler
/// slots to exercise it.
fn signal_outcome(reason: Result<&'static str, &'static str>) -> anyhow::Result<()> {
    match reason {
        Ok(reason) => {
            println!("slashitd: {reason}, shutting down");
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!("slashitd: {e}, cannot be stopped cleanly")),
    }
}

/// Wait for the operator to ask the daemon to stop.
///
/// A registration failure is not a stop request, and treating it as one
/// silently shut the daemon down seconds after start: the caller's
/// `tokio::select!` arm printed whatever string came back and returned
/// `Ok(())`, so a supervisor that only restarts on failure saw a clean exit
/// and never tried again. `Err` here means the opposite of "asked to
/// stop" -- it means stopping was never something an operator could ask
/// for -- and the caller now propagates it as a real startup failure
/// instead.
#[cfg(unix)]
async fn wait_for_signal() -> Result<&'static str, &'static str> {
    use tokio::signal::unix::{signal, SignalKind};

    // A daemon under systemd gets SIGTERM; one started from a shell gets
    // SIGINT. Both mean stop. One failing to register is not fatal on its
    // own -- the other still works -- so registration failure and "no signal
    // can ever be waited on" are kept distinct below.
    let mut term = signal(SignalKind::terminate())
        .inspect_err(|e| eprintln!("slashitd: cannot listen for SIGTERM: {e}"))
        .ok();
    let mut int = signal(SignalKind::interrupt())
        .inspect_err(|e| eprintln!("slashitd: cannot listen for SIGINT: {e}"))
        .ok();

    if term.is_none() && int.is_none() {
        return Err("neither SIGTERM nor SIGINT could be registered");
    }

    // A stream that never yields keeps its arm pending rather than ready, so
    // whichever registration failed cannot masquerade as an operator asking
    // the daemon to stop.
    tokio::select! {
        Some(_) = async { match term.as_mut() {
            Some(s) => s.recv().await,
            None => std::future::pending().await,
        } } => Ok("received SIGTERM"),
        Some(_) = async { match int.as_mut() {
            Some(s) => s.recv().await,
            None => std::future::pending().await,
        } } => Ok("received SIGINT"),
        else => Err("both signal streams ended without delivering a signal"),
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() -> Result<&'static str, &'static str> {
    match tokio::signal::ctrl_c().await {
        Ok(()) => Ok("received Ctrl-C"),
        Err(e) => {
            eprintln!("slashitd: cannot listen for Ctrl-C: {e}");
            Err("Ctrl-C could not be registered")
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

    #[test]
    fn a_genuine_signal_is_a_clean_shutdown() {
        assert!(signal_outcome(Ok("received SIGTERM")).is_ok());
        assert!(signal_outcome(Ok("received SIGINT")).is_ok());
    }

    #[test]
    fn a_registration_failure_cannot_masquerade_as_a_clean_shutdown() {
        let err = signal_outcome(Err("neither SIGTERM nor SIGINT could be registered"))
            .expect_err("registration failure must not collapse into Ok(())");
        let message = err.to_string();
        assert!(
            message.contains("cannot be stopped cleanly"),
            "error should say the daemon could not be stopped cleanly, got: {message}"
        );
        assert!(
            message.contains("neither SIGTERM nor SIGINT could be registered"),
            "error should carry the original reason, got: {message}"
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
