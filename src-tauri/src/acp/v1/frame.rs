//! JSON-RPC 2.0 framing for ACP over stdio.
//!
//! The ACP transport page (verified 2026-10-21, https://agentclientprotocol.com/protocol/transports)
//! says that messages are delimited by newlines, MUST NOT contain embedded
//! newlines, that the agent's stdout carries only protocol messages, and that
//! stderr is free for logging. This module enforces those rules on read and
//! on write. Errors carry at most a redacted excerpt of an offending frame.

use serde_json::{json, Value};
use thiserror::Error;

/// Upper bound on one stdio frame, in bytes. A larger line is a protocol
/// violation and fails the connection instead of being buffered.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Upper bound on the characters kept from a frame or message in an error.
pub const MAX_EXCERPT_CHARS: usize = 160;

const REDACTED: &str = "[redacted]";

/// One parsed message from the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A response to a request this client sent. `id` is the id we sent.
    Response {
        id: Value,
        result: Result<Value, RpcError>,
    },
    /// A request the agent sent to the client, which must be answered.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A notification, which must not be answered.
    Notification { method: String, params: Value },
}

/// A JSON-RPC error object. The message is already excerpted and redacted.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum FrameError {
    #[error("the frame is not valid ACP JSON-RPC ({reason}): {excerpt}")]
    Malformed {
        reason: &'static str,
        excerpt: String,
    },
    #[error("the frame is larger than the frame size limit")]
    TooLarge,
}

/// Parses one frame. The trailing newline must already be removed.
pub fn parse(line: &[u8]) -> Result<Message, FrameError> {
    let malformed = |reason: &'static str| FrameError::Malformed {
        reason,
        excerpt: excerpt(&String::from_utf8_lossy(line)),
    };
    let text = std::str::from_utf8(line).map_err(|_| malformed("not UTF-8"))?;
    let value: Value = serde_json::from_str(text).map_err(|_| malformed("not JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| malformed("not a JSON object"))?;
    if object.get("jsonrpc") != Some(&Value::String("2.0".to_string())) {
        return Err(malformed("jsonrpc is not \"2.0\""));
    }

    if let Some(method) = object.get("method") {
        let method = method
            .as_str()
            .filter(|method| !method.is_empty())
            .ok_or_else(|| malformed("method is not a non-empty string"))?
            .to_string();
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        return match object.get("id") {
            Some(id) if is_valid_id(id) => Ok(Message::Request {
                id: id.clone(),
                method,
                params,
            }),
            Some(_) => Err(malformed("request id is not a string or number")),
            None => Ok(Message::Notification { method, params }),
        };
    }

    let id = object
        .get("id")
        .filter(|id| is_valid_id(id))
        .ok_or_else(|| malformed("response has no valid id"))?;
    let result = match (object.get("result"), object.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(error)) => Err(RpcError {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .map(excerpt)
                .unwrap_or_default(),
        }),
        _ => {
            return Err(malformed(
                "response has neither or both of result and error",
            ))
        }
    };
    Ok(Message::Response {
        id: id.clone(),
        result,
    })
}

fn is_valid_id(id: &Value) -> bool {
    id.is_string() || id.is_number()
}

/// Serializes a message as one newline-terminated frame.
///
/// `serde_json` escapes newlines inside strings, so the output has exactly
/// one newline, at the end. The size limit applies to the whole frame.
pub fn encode(message: &Value) -> Result<Vec<u8>, FrameError> {
    let mut bytes = serde_json::to_vec(message).map_err(|_| FrameError::Malformed {
        reason: "not serializable",
        excerpt: String::new(),
    })?;
    if bytes.len() + 1 > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// A bounded, redacted, single-line excerpt for error messages and logs.
///
/// Control characters become spaces. Values that follow a sensitive name
/// (`token=`, `"api_key":`, `password:`) and `Bearer`/`Basic` credentials are
/// replaced. Known token prefixes are replaced as well.
pub fn excerpt(text: &str) -> String {
    let redacted = redact(text);
    let mut out = String::new();
    for (count, ch) in redacted.chars().enumerate() {
        if count == MAX_EXCERPT_CHARS {
            out.push_str("...");
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }
    out
}

fn redact(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut redact_next = false;
    for token in text.split_whitespace() {
        if redact_next {
            out.push(REDACTED.to_string());
            redact_next = false;
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if lower == "bearer" || lower == "basic" {
            out.push(token.to_string());
            redact_next = true;
            continue;
        }
        if let Some(position) = token.find(['=', ':']) {
            let name = &token[..position];
            if is_sensitive_name(name) {
                out.push(format!("{name}{}{REDACTED}", &token[position..=position]));
                continue;
            }
        }
        if looks_like_secret(token) {
            out.push(REDACTED.to_string());
            continue;
        }
        out.push(token.to_string());
    }
    out.join(" ")
}

fn is_sensitive_name(name: &str) -> bool {
    let lower = name.trim_matches('"').to_ascii_lowercase();
    [
        "key",
        "token",
        "secret",
        "password",
        "passwd",
        "authorization",
        "credential",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

fn looks_like_secret(token: &str) -> bool {
    let trimmed = token.trim_matches(|c: char| c == '"' || c == ',');
    trimmed.len() >= 16
        && ["sk-", "ghp_", "gho_", "xoxb-", "xoxp-", "AKIA"]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
}
