//! Turning this program's global flags into connection options.
//!
//! The connection itself — every transport, the retry policy, and the "SlashIt
//! is not running" diagnostics — lives in [`slashit_ipc::client`], shared with
//! anything else that needs to reach a running instance. This file used to
//! carry its own `UnixStream` code and its own error messages, which meant the
//! CLI was the only client that worked and the only one whose failures were
//! explained.

use anyhow::{anyhow, Result};
use slashit_ipc::client::ClientOptions;
use std::time::Duration;

/// Resolve where to connect, with what credential, and how patiently.
///
/// `--endpoint` wins over `SLASHIT_IPC_ENDPOINT`: a flag typed on this command
/// line is more specific than a variable exported for the whole shell.
pub fn options(endpoint: Option<&str>, wait: bool, timeout_secs: u64) -> Result<ClientOptions> {
    let mut options = ClientOptions::from_env()?;

    if let Some(raw) = endpoint {
        options.endpoint = slashit_ipc::config::parse_endpoint(raw).map_err(|e| anyhow!(e))?;
    }

    if wait {
        options = options.with_wait(Duration::from_secs(timeout_secs));
    }

    Ok(options)
}
