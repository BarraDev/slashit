//! ACP v1 over stdio: the spec-conformant transport and protocol layer.
//!
//! The OpenCode Worker runs through this module (see `agents::opencode_run`).
//! `TaskExecutor` reaches it only through `CodingRunner`, never directly. The
//! fixture agent (`tests/fixtures/acp_fixture_agent.rs`) is a binary of this
//! package that `cargo test` builds; no product path runs it, and the tests in
//! `tests.rs` drive it.
//!
//! Spec pages used, verified 2026-10-21:
//! - https://agentclientprotocol.com/protocol/transports (newline framing, stdout/stderr rules)
//! - https://agentclientprotocol.com/protocol/initialization (version, capabilities)
//! - https://agentclientprotocol.com/protocol/session-setup (session/new, load, resume)
//! - https://agentclientprotocol.com/protocol/prompt-turn (prompt, cancel, session/update)
//! - https://agentclientprotocol.com/protocol/tool-calls (session/request_permission)
//!
//! Modules:
//! - `frame`: JSON-RPC 2.0 parsing and encoding, size limits, redaction.
//! - `types`: negotiated capabilities, session updates, stop reasons, permissions.
//! - `process`: the owned child process (supervisor, writer, bounded reads, stderr tail).
//! - `connection`: the client API and the reader that routes every frame.

pub mod connection;
pub mod frame;
pub mod process;
pub mod types;

#[cfg(test)]
mod tests;
