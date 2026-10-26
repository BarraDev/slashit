// SPDX-License-Identifier: MPL-2.0
//! Typed ACP v1 values: negotiated capabilities, streamed session updates,
//! stop reasons, and permission options.
//!
//! Shapes follow the ACP pages verified on 2026-10-21 (initialization,
//! session-setup, prompt-turn, tool-calls). Anything not listed there becomes
//! `SessionUpdate::Unknown` or an error. Nothing in this module turns a
//! provider event into an action. The variants only describe what the agent
//! said.

use super::frame::excerpt;
use serde_json::Value;

/// The only protocol major version this client speaks. The spec says the
/// `protocolVersion` is a single integer that changes only for breaking changes.
pub const PROTOCOL_VERSION: u64 = 1;

/// The capabilities the agent advertised in its `initialize` result.
///
/// The spec says that omitted capabilities are unsupported, so every field
/// defaults to `false`. `resume_session` is true when
/// `sessionCapabilities.resume` is present and not `false`/`null`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentCapabilities {
    pub load_session: bool,
    pub resume_session: bool,
    pub close_session: bool,
    pub auth_method_count: usize,
}

/// Reads the protocol version and capabilities from an `initialize` result.
///
/// Returns the raw version value on failure so the caller can report it.
pub fn parse_initialize_result(result: &Value) -> Result<(u64, AgentCapabilities), String> {
    let version = result
        .get("protocolVersion")
        .ok_or_else(|| "the initialize result has no protocolVersion".to_string())?
        .as_u64()
        .ok_or_else(|| "the protocolVersion is not an unsigned integer".to_string())?;

    let advertised = result.get("agentCapabilities");
    let session = advertised.and_then(|caps| caps.get("sessionCapabilities"));
    let capabilities = AgentCapabilities {
        load_session: advertised
            .and_then(|caps| caps.get("loadSession"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        resume_session: session
            .and_then(|caps| caps.get("resume"))
            .is_some_and(|resume| !resume.is_null() && resume.as_bool() != Some(false)),
        close_session: session
            .and_then(|caps| caps.get("close"))
            .is_some_and(|close| !close.is_null() && close.as_bool() != Some(false)),
        auth_method_count: result
            .get("authMethods")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
    };
    Ok((version, capabilities))
}

/// The stop reasons the spec defines for `session/prompt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

/// The value the agent reports as current for the configuration option
/// `config_id`, in a `session/new` or `session/set_config_option` result.
/// `None` when the option is absent or its current value is not a string.
pub fn config_option_value(result: &Value, config_id: &str) -> Option<String> {
    result
        .get("configOptions")?
        .as_array()?
        .iter()
        .find(|option| option.get("id").and_then(Value::as_str) == Some(config_id))?
        .get("currentValue")?
        .as_str()
        .map(str::to_string)
}

pub fn parse_stop_reason(value: &str) -> Option<StopReason> {
    match value {
        "end_turn" => Some(StopReason::EndTurn),
        "max_tokens" => Some(StopReason::MaxTokens),
        "max_turn_requests" => Some(StopReason::MaxTurnRequests),
        "refusal" => Some(StopReason::Refusal),
        "cancelled" => Some(StopReason::Cancelled),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

fn parse_tool_call_status(value: Option<&Value>) -> Option<ToolCallStatus> {
    match value.and_then(Value::as_str)? {
        "pending" => Some(ToolCallStatus::Pending),
        "in_progress" => Some(ToolCallStatus::InProgress),
        "completed" => Some(ToolCallStatus::Completed),
        "failed" => Some(ToolCallStatus::Failed),
        _ => None,
    }
}

/// One `session/update` notification, typed by its `sessionUpdate` discriminator.
///
/// `Unknown` keeps the discriminator (bounded and redacted) as evidence. It is
/// never an action, and SlashIt must not map it to one.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionUpdate {
    AgentMessageChunk {
        text: Option<String>,
    },
    AgentThoughtChunk {
        text: Option<String>,
    },
    UserMessageChunk {
        text: Option<String>,
    },
    ToolCall {
        tool_call_id: String,
        /// The human-readable title the agent gave the call, if any.
        title: Option<String>,
        /// The tool's kind as the agent names it (`read`, `edit`, ...), if any.
        kind: Option<String>,
        /// The input the agent sent for the call, verbatim, if any.
        raw_input: Option<Value>,
        status: Option<ToolCallStatus>,
    },
    ToolCallUpdate {
        tool_call_id: String,
        status: Option<ToolCallStatus>,
    },
    Plan,
    /// A documented update that carries no action for SlashIt, such as
    /// `current_mode_update` or `usage_update`.
    Informational {
        kind: String,
    },
    Unknown {
        kind: String,
    },
}

fn content_text(update: &Value) -> Option<String> {
    let content = update.get("content")?;
    (content.get("type")?.as_str()? == "text")
        .then(|| {
            content
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .flatten()
}

/// Parses `session/update` params. Returns `None` when `sessionId` or the
/// discriminator is missing, so the caller can record an unknown event.
pub fn parse_session_update(params: &Value) -> Option<(String, SessionUpdate)> {
    let session_id = params.get("sessionId")?.as_str()?.to_string();
    let update = params.get("update")?;
    let kind = update.get("sessionUpdate")?.as_str()?;
    let parsed = match kind {
        "agent_message_chunk" => SessionUpdate::AgentMessageChunk {
            text: content_text(update),
        },
        "agent_thought_chunk" => SessionUpdate::AgentThoughtChunk {
            text: content_text(update),
        },
        "user_message_chunk" => SessionUpdate::UserMessageChunk {
            text: content_text(update),
        },
        "tool_call" | "tool_call_update" => {
            let Some(tool_call_id) = update.get("toolCallId").and_then(Value::as_str) else {
                return Some((session_id, unknown(kind)));
            };
            let status = parse_tool_call_status(update.get("status"));
            let tool_call_id = tool_call_id.to_string();
            if kind == "tool_call" {
                SessionUpdate::ToolCall {
                    tool_call_id,
                    title: update.get("title").and_then(Value::as_str).map(str::to_string),
                    kind: update.get("kind").and_then(Value::as_str).map(str::to_string),
                    raw_input: update.get("rawInput").cloned(),
                    status,
                }
            } else {
                SessionUpdate::ToolCallUpdate {
                    tool_call_id,
                    status,
                }
            }
        }
        "plan" => SessionUpdate::Plan,
        "available_commands_update"
        | "current_mode_update"
        | "config_option_update"
        | "session_info_update"
        | "usage_update"
        | "notice"
        | "compaction_update"
        | "compaction_summary_chunk" => SessionUpdate::Informational {
            kind: kind.to_string(),
        },
        _ => unknown(kind),
    };
    Some((session_id, parsed))
}

fn unknown(kind: &str) -> SessionUpdate {
    SessionUpdate::Unknown {
        kind: excerpt(kind).chars().take(64).collect(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl PermissionKind {
    pub fn is_allow(self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub session_id: String,
    pub tool_call_id: String,
    pub options: Vec<PermissionOption>,
}

/// Parses `session/request_permission` params. Returns `None` for anything the
/// spec does not describe, including duplicate option ids and an empty option list.
/// The caller then answers `cancelled`.
pub fn parse_permission_request(params: &Value) -> Option<PermissionRequest> {
    let session_id = params.get("sessionId")?.as_str()?.to_string();
    let tool_call_id = params
        .get("toolCall")?
        .get("toolCallId")?
        .as_str()?
        .to_string();
    let raw_options = params.get("options")?.as_array()?;
    let mut options: Vec<PermissionOption> = Vec::with_capacity(raw_options.len());
    for raw in raw_options {
        let option_id = raw.get("optionId")?.as_str()?.to_string();
        let name = raw
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let kind = match raw.get("kind")?.as_str()? {
            "allow_once" => PermissionKind::AllowOnce,
            "allow_always" => PermissionKind::AllowAlways,
            "reject_once" => PermissionKind::RejectOnce,
            "reject_always" => PermissionKind::RejectAlways,
            _ => return None,
        };
        if options
            .iter()
            .any(|existing| existing.option_id == option_id)
        {
            return None;
        }
        options.push(PermissionOption {
            option_id,
            name,
            kind,
        });
    }
    if options.is_empty() || session_id.is_empty() {
        return None;
    }
    Some(PermissionRequest {
        session_id,
        tool_call_id,
        options,
    })
}

/// What the owner decided for one permission request.
///
/// There is deliberately no variant that picks an option by kind or by
/// position. The `option_id` must be one the agent offered, or the decision is
/// refused and the request is cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Reject { option_id: String },
    Cancel,
}

/// The outcome sent back to the agent for a permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    Selected { option_id: String },
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn initialize_result_defaults_unadvertised_capabilities_to_unsupported() {
        let (version, caps) = parse_initialize_result(&json!({"protocolVersion": 1})).unwrap();
        assert_eq!(version, 1);
        assert_eq!(caps, AgentCapabilities::default());
    }

    #[test]
    fn initialize_result_reads_resume_and_load_flags() {
        let result = json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "sessionCapabilities": {"resume": {}, "close": {}}
            },
            "authMethods": [{"id": "login"}]
        });
        let (_, caps) = parse_initialize_result(&result).unwrap();
        assert!(caps.load_session);
        assert!(caps.resume_session);
        assert!(caps.close_session);
        assert_eq!(caps.auth_method_count, 1);
    }

    #[test]
    fn initialize_result_without_a_version_is_refused() {
        assert!(parse_initialize_result(&json!({"agentCapabilities": {}})).is_err());
        assert!(parse_initialize_result(&json!({"protocolVersion": "1"})).is_err());
    }

    #[test]
    fn session_update_parses_known_and_unknown_discriminators() {
        let text = json!({"sessionId": "s", "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hi"}
        }});
        assert_eq!(
            parse_session_update(&text),
            Some((
                "s".to_string(),
                SessionUpdate::AgentMessageChunk {
                    text: Some("hi".to_string())
                }
            ))
        );

        let tool = json!({"sessionId": "s", "update": {
            "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed"
        }});
        assert_eq!(
            parse_session_update(&tool).map(|(_, update)| update),
            Some(SessionUpdate::ToolCallUpdate {
                tool_call_id: "t1".to_string(),
                status: Some(ToolCallStatus::Failed),
            })
        );

        let mystery = json!({"sessionId": "s", "update": {"sessionUpdate": "rm_everything"}});
        assert_eq!(
            parse_session_update(&mystery).map(|(_, update)| update),
            Some(SessionUpdate::Unknown {
                kind: "rm_everything".to_string()
            })
        );
    }

    #[test]
    fn session_update_without_a_session_id_is_not_parsed() {
        let params = json!({"update": {"sessionUpdate": "plan"}});
        assert_eq!(parse_session_update(&params), None);
    }

    #[test]
    fn permission_request_requires_well_formed_options() {
        let valid = json!({
            "sessionId": "s",
            "toolCall": {"toolCallId": "t1"},
            "options": [
                {"optionId": "a", "name": "Allow", "kind": "allow_once"},
                {"optionId": "r", "name": "Reject", "kind": "reject_once"}
            ]
        });
        let request = parse_permission_request(&valid).unwrap();
        assert_eq!(request.options.len(), 2);
        assert!(request.options[0].kind.is_allow());

        let unknown_kind = json!({"sessionId": "s", "toolCall": {"toolCallId": "t1"},
            "options": [{"optionId": "a", "name": "x", "kind": "allow_forever"}]});
        assert_eq!(parse_permission_request(&unknown_kind), None);

        let duplicate = json!({"sessionId": "s", "toolCall": {"toolCallId": "t1"},
        "options": [
            {"optionId": "a", "name": "x", "kind": "allow_once"},
            {"optionId": "a", "name": "y", "kind": "reject_once"}
        ]});
        assert_eq!(parse_permission_request(&duplicate), None);

        let empty = json!({"sessionId": "s", "toolCall": {"toolCallId": "t1"}, "options": []});
        assert_eq!(parse_permission_request(&empty), None);
    }

    #[test]
    fn config_option_value_reads_only_the_named_option() {
        let result = json!({"configOptions": [
            {"id": "mode", "currentValue": "auto"},
            {"id": "model", "currentValue": "ollama/llama3.2:3b"},
        ]});
        assert_eq!(
            config_option_value(&result, "model").as_deref(),
            Some("ollama/llama3.2:3b")
        );
        assert_eq!(config_option_value(&result, "thought_level"), None);
        assert_eq!(config_option_value(&json!({"configOptions": []}), "model"), None);
        assert_eq!(config_option_value(&json!({}), "model"), None);
        let not_a_string = json!({"configOptions": [{"id": "model", "currentValue": 3}]});
        assert_eq!(config_option_value(&not_a_string, "model"), None);
    }

    #[test]
    fn stop_reason_accepts_only_the_spec_values() {
        assert_eq!(parse_stop_reason("cancelled"), Some(StopReason::Cancelled));
        assert_eq!(parse_stop_reason("end_turn"), Some(StopReason::EndTurn));
        assert_eq!(parse_stop_reason("finished"), None);
    }
}
