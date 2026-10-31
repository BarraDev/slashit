// SPDX-License-Identifier: MPL-2.0
//! The provider-neutral boundary between `TaskExecutor` and a coding agent run.
//!
//! The executor starts a run, subscribes to its events, waits for it, reads
//! its result and kills it through [`CodingRunner`]. It never names a provider
//! type. Claude Code is the only provider in this module; another provider
//! becomes another variant here, with the same events and the same run
//! operations.
//!
//! Building the run's configuration is still provider-specific
//! ([`ClaudeRunConfig`]). Moving that behind this boundary belongs to the next
//! provider integration, once a second provider states what it needs.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::opencode_run::{OpenCodeRunConfig, OpenCodeRunner};
use super::provider_lease::ProviderLease;
use super::runner::{ClaudeRunConfig, ClaudeRunner, LaunchError};

/// Events a coding run emits while it executes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CodingRunEvent {
    /// Session initialized
    #[serde(rename = "system_init")]
    SystemInit { session_id: String, model: Option<String>, message: Option<String> },
    /// Partial text output (streaming)
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    /// Agent is using a tool
    #[serde(rename = "tool_use")]
    ToolUse { tool: String, input: Option<serde_json::Value> },
    /// Full assistant message received
    #[serde(rename = "assistant_message")]
    AssistantMessage { content: serde_json::Value },
    /// Final result
    #[serde(rename = "result")]
    Result { session_id: String, text: String, is_error: bool },
    /// Error during execution
    #[serde(rename = "error")]
    Error { message: String },
}

/// A running coding agent, whichever provider runs it.
///
/// Each method forwards to the provider's run with the same meaning. See
/// [`ClaudeRunner`] for what each one guarantees.
pub enum CodingRunner {
    Claude(ClaudeRunner),
    OpenCode(OpenCodeRunner),
}

impl CodingRunner {
    /// Start a Claude Code run. Every run that can edit a Task Checkout passes
    /// its provider lease. See [`ClaudeRunner::start_leased`].
    pub async fn start_claude(
        config: ClaudeRunConfig,
        lease: Option<ProviderLease>,
    ) -> Result<Self, LaunchError> {
        ClaudeRunner::start_leased(config, lease).await.map(Self::Claude)
    }

    /// Start an OpenCode run over ACP. See [`OpenCodeRunner::start`].
    pub async fn start_opencode(
        config: OpenCodeRunConfig,
        lease: Option<ProviderLease>,
    ) -> Result<Self, String> {
        OpenCodeRunner::start(config, lease).await.map(Self::OpenCode)
    }

    /// Subscribe to the run's events. See [`ClaudeRunner::subscribe`].
    pub fn subscribe(&self) -> broadcast::Receiver<CodingRunEvent> {
        match self {
            Self::Claude(runner) => runner.subscribe(),
            Self::OpenCode(runner) => runner.subscribe(),
        }
    }

    /// Wait for the run to finish. See [`ClaudeRunner::wait`].
    pub async fn wait(&self) -> Result<bool, String> {
        match self {
            Self::Claude(runner) => runner.wait().await,
            Self::OpenCode(runner) => runner.wait().await,
        }
    }

    /// The run's output so far. See [`ClaudeRunner::get_output`].
    pub async fn get_output(&self) -> String {
        match self {
            Self::Claude(runner) => runner.get_output().await,
            Self::OpenCode(runner) => runner.get_output().await,
        }
    }

    /// The permission requests this run refused. See
    /// [`OpenCodeRunner::refused_permissions`]. A Claude run decides its own
    /// permissions in `auto` mode and reports no refusals here.
    pub fn refused_permissions(&self) -> Vec<String> {
        match self {
            Self::Claude(_) => Vec::new(),
            Self::OpenCode(runner) => runner.refused_permissions(),
        }
    }

    /// The run's final result text. See [`ClaudeRunner::final_result_text`].
    pub async fn final_result_text(&self) -> Result<String, String> {
        match self {
            Self::Claude(runner) => runner.final_result_text().await,
            Self::OpenCode(runner) => runner.final_result_text().await,
        }
    }

    /// Stop the run and reap it. See [`ClaudeRunner::kill`].
    pub async fn kill(&self) -> Result<(), String> {
        match self {
            Self::Claude(runner) => runner.kill().await,
            Self::OpenCode(runner) => runner.kill().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant keeps the JSON shape it had as `ClaudeEvent`. These are
    /// serialized by anything that persists or forwards a run's events, so a
    /// provider integration must not change them.
    #[test]
    fn events_keep_their_wire_shape() {
        let cases = [
            (
                CodingRunEvent::SystemInit {
                    session_id: "s".into(),
                    model: Some("m".into()),
                    message: None,
                },
                serde_json::json!({
                    "type": "system_init", "session_id": "s", "model": "m", "message": null
                }),
            ),
            (
                CodingRunEvent::TextDelta { text: "hi".into() },
                serde_json::json!({ "type": "text_delta", "text": "hi" }),
            ),
            (
                CodingRunEvent::ToolUse { tool: "Bash".into(), input: None },
                serde_json::json!({ "type": "tool_use", "tool": "Bash", "input": null }),
            ),
            (
                CodingRunEvent::AssistantMessage { content: serde_json::json!([]) },
                serde_json::json!({ "type": "assistant_message", "content": [] }),
            ),
            (
                CodingRunEvent::Result { session_id: "s".into(), text: "t".into(), is_error: false },
                serde_json::json!({
                    "type": "result", "session_id": "s", "text": "t", "is_error": false
                }),
            ),
            (
                CodingRunEvent::Error { message: "boom".into() },
                serde_json::json!({ "type": "error", "message": "boom" }),
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(serde_json::to_value(&event).unwrap(), expected);
        }
    }
}
