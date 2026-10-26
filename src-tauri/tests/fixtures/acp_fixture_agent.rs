// SPDX-License-Identifier: MPL-2.0
//! A fake ACP agent for fixture tests. It never calls a model.
//!
//! Usage: `acp_fixture_agent <scenario>`. Scenarios:
//! - `ok`: initialize (version 1), session/new, streamed updates, end_turn.
//! - `bad-version`: answers initialize with protocolVersion 2.
//! - `permission`: a prompt asks for permission, echoes the decision as a
//!   text update, then answers with the stop reason. A cancel forces `cancelled`.
//! - `permission-then-edit`: as `permission`, and after a refusal it writes
//!   `permitted-change.txt` in its working directory before it ends the turn.
//! - `silent`: never answers session/new. Echoes a `cancel-seen` update on session/cancel.
//! - `bad-json`: writes a non-JSON line on stdout in answer to session/new.
//! - `exit-on-prompt`: exits with status 3 when it receives session/prompt.
//! - `ignore-eof`: keeps running after stdin closes, so the client must kill it.
//! - `stop-reading`: answers initialize, then never reads stdin again.
//! - `model-echo`: answers session/set_config_option with the value it was
//!   given, then, in the prompt turn, echoes the model it recorded as `model=<id>`.
//! - `model-refuse`: answers session/set_config_option with an error, as
//!   OpenCode does for a model it does not know.
//! - `model-mismatch`: answers session/set_config_option reporting another model.
//!
//! Every other scenario accepts session/set_config_option and reports the value.
//!
//! Set `ACP_FIXTURE_TRACE=1` to copy every frame it receives to stderr.

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::thread;
use std::time::Duration;

struct Io {
    lines: io::Lines<io::StdinLock<'static>>,
    trace: bool,
}

impl Io {
    fn new() -> Self {
        Self {
            lines: io::stdin().lock().lines(),
            trace: std::env::var_os("ACP_FIXTURE_TRACE").is_some(),
        }
    }

    /// The next JSON message from the client, or `None` at end of input.
    fn recv(&mut self) -> Option<Value> {
        loop {
            let line = self.lines.next()?.ok()?;
            if self.trace {
                eprintln!("fixture <- {line}");
            }
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                return Some(value);
            }
        }
    }

    fn send(&self, value: &Value) {
        self.write_raw(&format!("{value}\n"));
    }

    fn write_raw(&self, line: &str) {
        if self.trace {
            eprint!("fixture -> {line}");
        }
        let mut out = io::stdout().lock();
        out.write_all(line.as_bytes()).expect("write to stdout");
        out.flush().expect("flush stdout");
    }

    fn update(&self, session: &Value, update: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session, "update": update},
        }));
    }

    fn text_update(&self, session: &Value, text: &str) {
        self.update(
            session,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
        );
    }
}

fn respond(io: &Io, id: &Value, result: Value) {
    io.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn main() {
    let scenario = std::env::args().nth(1).unwrap_or_else(|| "ok".to_string());
    let mut io = Io::new();

    if let Some(init) = io.recv() {
        let version = if scenario == "bad-version" { 2 } else { 1 };
        respond(
            &io,
            &init["id"],
            json!({
                "protocolVersion": version,
                "agentCapabilities": {
                    "loadSession": true,
                    "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false},
                    "mcpCapabilities": {"http": false, "sse": false},
                    "sessionCapabilities": {"close": {}}
                },
                "authMethods": [],
                "agentInfo": {"name": "acp-fixture", "version": "0.0.0"}
            }),
        );
    }

    if scenario == "stop-reading" {
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }

    while let Some(message) = io.recv() {
        dispatch(&mut io, &scenario, &message);
    }

    if scenario == "ignore-eof" {
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }
    if scenario == "notify-after-eof" {
        // A frame that arrives only after the client has closed its input.
        io.send(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": "fixture-session-1", "update": {"sessionUpdate": "mystery_event"}}
        }));
    }
}

fn dispatch(io: &mut Io, scenario: &str, message: &Value) {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let id = message.get("id").cloned();
    match (method, id) {
        ("session/new", Some(id)) => match scenario {
            "silent" => {}
            "bad-json" => io.write_raw("this line is not JSON and must fail the connection\n"),
            _ => respond(
                io,
                &id,
                json!({"sessionId": "fixture-session-1", "configOptions": []}),
            ),
        },
        ("session/load" | "session/resume", Some(id)) => respond(io, &id, json!({})),
        ("session/set_config_option", Some(id)) => {
            let value = message["params"]["value"].as_str().unwrap_or("");
            match scenario {
                "model-refuse" => io.send(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32602, "message": format!("model not found: {value}")},
                })),
                "model-mismatch" => respond(
                    io,
                    &id,
                    json!({"configOptions": [{"id": "model", "currentValue": "opencode/flint-40b-free"}]}),
                ),
                _ => {
                    *SELECTED_MODEL.lock().unwrap_or_else(|p| p.into_inner()) = Some(value.to_string());
                    respond(
                        io,
                        &id,
                        json!({"configOptions": [{"id": "model", "currentValue": value}]}),
                    )
                }
            }
        }
        ("session/prompt", Some(id)) => {
            let session = message["params"]["sessionId"].clone();
            match scenario {
                "exit-on-prompt" => std::process::exit(3),
                "permission" | "permission-then-edit" => {
                    permission_turn(io, &id, &session, scenario)
                }
                "model-echo" => echo_model_turn(io, &id, &session),
                _ => stream_turn(io, &id, &session),
            }
        }
        ("session/cancel", None) if scenario == "silent" => {
            let session = message["params"]["sessionId"].clone();
            io.text_update(&session, "cancel-seen");
        }
        (_, Some(id)) => io.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "method not supported by the fixture"},
        })),
        _ => {}
    }
}

/// The model the last session/set_config_option selected in this process.
static SELECTED_MODEL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn echo_model_turn(io: &mut Io, id: &Value, session: &Value) {
    let model = SELECTED_MODEL
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .unwrap_or_else(|| "none".to_string());
    io.text_update(session, &format!("model={model}"));
    respond(io, id, json!({"stopReason": "end_turn"}));
}

fn stream_turn(io: &mut Io, id: &Value, session: &Value) {
    io.text_update(session, "hello");
    io.update(
        session,
        json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "thinking"}}),
    );
    io.update(
        session,
        json!({"sessionUpdate": "tool_call", "toolCallId": "tc-1", "title": "read file", "status": "pending"}),
    );
    io.update(
        session,
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "tc-1", "status": "completed"}),
    );
    io.update(session, json!({"sessionUpdate": "mystery_event"}));
    respond(io, id, json!({"stopReason": "end_turn"}));
}

fn permission_turn(io: &mut Io, id: &Value, session: &Value, scenario: &str) {
    io.text_update(session, "working");
    io.send(&json!({
        "jsonrpc": "2.0",
        "id": 1000,
        "method": "session/request_permission",
        "params": {
            "sessionId": session,
            "toolCall": {"toolCallId": "tc-1", "title": "write file"},
            "options": [
                {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "reject-once", "name": "Reject once", "kind": "reject_once"}
            ]
        }
    }));
    let mut cancelled = false;
    while let Some(message) = io.recv() {
        let method = message.get("method").and_then(Value::as_str);
        if method == Some("session/cancel") {
            cancelled = true;
            continue;
        }
        if method.is_none() && message.get("id") == Some(&json!(1000)) {
            let outcome = &message["result"]["outcome"];
            let kind = outcome["outcome"].as_str().unwrap_or("invalid").to_string();
            let text = match outcome["optionId"].as_str() {
                Some(option) => format!("perm-outcome={kind}:{option}"),
                None => format!("perm-outcome={kind}"),
            };
            io.text_update(session, &text);
            // After the refusal, the fixture finishes the change through an
            // operation it was allowed to run: the Task's work still lands.
            if scenario == "permission-then-edit" {
                std::fs::write("permitted-change.txt", "written after a refusal\n")
                    .expect("the fixture writes in its working directory");
            }
            let stop = if cancelled || kind != "selected" {
                "cancelled"
            } else {
                "end_turn"
            };
            respond(io, id, json!({"stopReason": stop}));
            return;
        }
    }
    std::process::exit(0);
}
