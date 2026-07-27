//! What kind of process is running, and the few operations that differ
//! between a windowed app and a headless daemon.
//!
//! Almost all of the backend is identical in both modes — the same `AppState`,
//! the same queue executor, the same IPC command handlers. Only three things
//! genuinely differ: showing a window, quitting, and how events are reported
//! (see [`crate::events`]). Those are isolated here so the rest of the code
//! never asks which mode it is in.

use std::sync::Arc;

/// How this process is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceMode {
    /// A desktop window, possibly hidden to the tray.
    Gui,
    /// No window at all: an IPC server plus the queue executor.
    Daemon,
}

impl InstanceMode {
    /// Stable identifier for the wire and for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gui => "gui",
            Self::Daemon => "daemon",
        }
    }
}

/// The operations that only some instances can perform.
///
/// A daemon has no window, so `show_window` is a legitimate failure rather
/// than a no-op that lies to the caller: `slashit show` against a daemon
/// should say there is no window, not report success.
pub trait InstanceControl: Send + Sync {
    fn mode(&self) -> InstanceMode;

    /// Bring the window to the front.
    ///
    /// `Err` when this instance has no window.
    fn show_window(&self) -> Result<(), String>;

    /// Begin a graceful shutdown.
    ///
    /// In the GUI this asks the frontend to confirm when work is running; in
    /// the daemon it trips the shutdown signal. Either way it returns
    /// immediately — the caller still needs its response written before the
    /// process goes away.
    fn request_quit(&self);
}

/// Shared handle to whichever control this process installed.
pub type SharedInstanceControl = Arc<dyn InstanceControl>;

/// A control for tests and for any path that must not touch the process.
pub struct InertControl;

impl InstanceControl for InertControl {
    fn mode(&self) -> InstanceMode {
        InstanceMode::Daemon
    }

    fn show_window(&self) -> Result<(), String> {
        Err("this instance has no window".to_string())
    }

    fn request_quit(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_have_stable_wire_names() {
        // These strings go over IPC and into logs; renaming one silently
        // breaks `slashit ping` output and any operator's grep.
        assert_eq!(InstanceMode::Gui.as_str(), "gui");
        assert_eq!(InstanceMode::Daemon.as_str(), "daemon");
    }

    #[test]
    fn a_windowless_instance_refuses_to_show_rather_than_pretending() {
        let control = InertControl;
        assert!(control.show_window().is_err());
    }
}
