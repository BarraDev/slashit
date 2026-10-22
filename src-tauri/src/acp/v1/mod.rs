//! ACP v1 over stdio: the spec-conformant transport and protocol layer.
//!
//! Status: milestone 1 of the modern ACP work. This module is compiled only
//! for tests (see `acp/mod.rs`), so it is not reachable from any Tauri command
//! or from `TaskExecutor`. It is verified against the fixture agent
//! (`tests/fixtures/acp_fixture_agent.rs`) and the tests in `tests.rs`.
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
