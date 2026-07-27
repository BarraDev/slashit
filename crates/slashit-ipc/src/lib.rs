//! The SlashIt control channel, shared by the desktop app, the daemon and the
//! CLI.
//!
//! The crate is layered so that adding a transport never means touching a
//! command, and changing a command never means touching a platform:
//!
//! - [`protocol`] — the command set, the envelope, the response.
//! - [`framing`] — how one message is delimited and bounded.
//! - [`transport`] — how bytes move, and what the OS proves about the peer.
//! - [`auth`] — bearer tokens, for transports the OS does not access-control.
//! - [`config`] — which listeners exist and where clients look.
//! - [`endpoint`] — platform-native addresses and the bind-address policy.
//! - [`client`] — the request/response round trip, used by the CLI.
//!
//! **The channel is a code-execution channel.** `CreateTask` followed by
//! `MoveTask(in_progress)` reaches the queue executor, which creates a
//! worktree and spawns an agent with full tool access. Every access-control
//! decision in this crate follows from that; see
//! `docs/architecture/ipc-security.md`.

pub mod auth;
pub mod client;
pub mod config;
pub mod endpoint;
pub mod framing;
pub mod protocol;
pub mod transport;

/// The application identity used for per-user runtime paths.
///
/// Deliberately `slashit-app` rather than the binary name `slashit-ui`: it
/// must match `config::paths::APPLICATION` in the desktop crate, which is
/// frozen because changing it would move `~/.config/slashit-app` and orphan
/// every existing user's projects, tasks and terminal history.
pub const APPLICATION: &str = "slashit-app";

pub use auth::{AuthToken, Credentials};
pub use config::{IpcConfig, TcpConfig};
pub use endpoint::{socket_path, Endpoint};
pub use protocol::{
    AppStatus, FeatureFlagInfo, InstanceInfo, IpcEnvelope, IpcRequest, IpcResponse, ProjectSummary,
    QueueStatusInfo, TaskSummary, TerminalSummary, MAX_REQUEST_BYTES, PROTOCOL_VERSION,
};
pub use transport::{Accepted, BindOptions, IpcListener, IpcStream, TransportAuth};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_application_identity_is_frozen() {
        // A rename here silently relocates every user's configuration and
        // orphans the running socket. See docs/architecture/state-locations.md.
        assert_eq!(APPLICATION, "slashit-app");
    }
}
