//! The control channel, server side.
//!
//! `slashit_ipc` owns the wire: the command set, the framing, the transports,
//! and what each transport proves about its peer. What is left for this module
//! is the part that is specific to a running SlashIt instance — the state a
//! command operates on, and whether a given peer is allowed to issue it.
//!
//! Nothing here is Unix-specific any more. The module used to be
//! `#[cfg(unix)]`-gated in `lib.rs` because it was written directly against
//! `UnixListener` and file mode bits; those now live behind
//! [`slashit_ipc::transport`], which picks the platform-native local transport,
//! so the same server compiles and runs on Linux, macOS and Windows.

pub mod handlers;
pub mod server;

pub use handlers::Dispatch;
pub use server::{serve, IpcContext, IpcServer, PeerContext};
