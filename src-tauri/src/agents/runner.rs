use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, Mutex, RwLock};

/// Configuration for a Claude Code CLI run.
#[derive(Debug, Clone)]
pub struct ClaudeRunConfig {
    pub prompt: String,
    pub working_dir: String,
    pub allowed_tools: Vec<String>,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub session_id: Option<String>,
    pub resume_session: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub permission_mode: Option<String>,
    /// When true, pass --strict-mcp-config without any --mcp-config files,
    /// effectively disabling all MCP servers (project + user) for this run.
    pub disable_mcp: bool,
}

/// Events emitted by the Claude runner during execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClaudeEvent {
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

/// Runs Claude Code CLI and streams events.
pub struct ClaudeRunner {
    child: Arc<Mutex<Child>>,
    event_tx: broadcast::Sender<ClaudeEvent>,
    session_id: Arc<Mutex<Option<String>>>,
    accumulated_output: Arc<RwLock<String>>,
    /// Set to the error text if the Result event has is_error: true
    result_error: Arc<RwLock<Option<String>>>,
}

impl ClaudeRunner {
    /// Start a Claude Code CLI run with the given config.
    pub async fn start(config: ClaudeRunConfig) -> Result<Self, String> {
        let mut cmd = Command::new("claude");

        cmd.arg("-p").arg(&config.prompt);
        cmd.arg("--verbose");
        cmd.arg("--output-format").arg("stream-json");

        if !config.allowed_tools.is_empty() {
            cmd.arg("--allowedTools").arg(config.allowed_tools.join(","));
        }

        if let Some(ref mode) = config.permission_mode {
            cmd.arg("--permission-mode").arg(mode);
        } else {
            cmd.arg("--dangerously-skip-permissions");
        }

        if let Some(turns) = config.max_turns {
            cmd.arg("--max-turns").arg(turns.to_string());
        }
        if let Some(budget) = config.max_budget_usd {
            cmd.arg("--max-budget-usd").arg(budget.to_string());
        }
        if let Some(ref sid) = config.session_id {
            cmd.arg("--session-id").arg(sid);
        }
        if let Some(ref resume) = config.resume_session {
            cmd.arg("--resume").arg(resume);
        }
        if let Some(ref model) = config.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(ref sys) = config.system_prompt {
            cmd.arg("--system-prompt").arg(sys);
        }

        if config.disable_mcp {
            cmd.arg("--strict-mcp-config");
        }

        cmd.current_dir(&config.working_dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = cmd.spawn()
            .map_err(|e| format!("Failed to spawn claude: {}. Is claude CLI installed?", e))?;

        let (event_tx, _) = broadcast::channel(512);

        let runner = Self {
            child: Arc::new(Mutex::new(child)),
            event_tx,
            session_id: Arc::new(Mutex::new(config.session_id)),
            accumulated_output: Arc::new(RwLock::new(String::new())),
            result_error: Arc::new(RwLock::new(None)),
        };

        runner.start_reader();

        Ok(runner)
    }

    /// Subscribe to events.
    pub fn subscribe(&self) -> broadcast::Receiver<ClaudeEvent> {
        self.event_tx.subscribe()
    }

    /// Get the accumulated text output from all TextDelta and Result events.
    pub async fn get_output(&self) -> String {
        self.accumulated_output.read().await.clone()
    }

    /// Kill the running process.
    pub async fn kill(&self) -> Result<(), String> {
        let mut child = self.child.lock().await;
        child.kill().await.map_err(|e| format!("Failed to kill claude: {}", e))
    }

    /// Wait for the process to complete and return exit status.
    /// On failure, includes stderr in the error message.
    pub async fn wait(&self) -> Result<bool, String> {
        let mut child = self.child.lock().await;

        // Take stderr handle and read it concurrently with wait
        let stderr_handle = child.stderr.take().map(|stderr| {
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stderr);
                let mut buf = String::new();
                let _ = tokio::io::AsyncReadExt::read_to_string(&mut reader, &mut buf).await;
                buf
            })
        });

        let status = child.wait().await.map_err(|e| format!("Wait failed: {}", e))?;

        // Collect stderr now that process has exited
        let stderr_text = match stderr_handle {
            Some(handle) => handle.await.unwrap_or_default(),
            None => String::new(),
        };

        if !status.success() {
            let code = status.code().map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_string());
            let stderr_summary = stderr_text.trim()
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("no details")
                .to_string();
            if !stderr_text.is_empty() {
                self.accumulated_output.write().await.push_str(&format!("\n--- STDERR ---\n{}", stderr_text));
            }
            return Err(format!("Exit code {} — {}", code, stderr_summary));
        }

        // Check for Claude-level errors (exit code 0 but is_error: true in result)
        if let Some(err_text) = self.result_error.read().await.as_ref() {
            return Err(err_text.clone());
        }

        Ok(true)
    }

    fn start_reader(&self) {
        let child = self.child.clone();
        let event_tx = self.event_tx.clone();
        let session_id = self.session_id.clone();
        let accumulated_output = self.accumulated_output.clone();
        let result_error = self.result_error.clone();

        tauri::async_runtime::spawn(async move {
            let mut child_guard = child.lock().await;

            // Read stdout (NDJSON stream)
            if let Some(stdout) = child_guard.stdout.take() {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();

                // Drop child guard so other operations can proceed
                drop(child_guard);

                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }

                    let parsed: Result<serde_json::Value, _> = serde_json::from_str(&line);
                    match parsed {
                        Ok(json) => {
                            let event = parse_claude_event(&json, &session_id).await;
                            if let Some(evt) = event {
                                // Accumulate text from TextDelta and Result events
                                match &evt {
                                    ClaudeEvent::TextDelta { text } => {
                                        accumulated_output.write().await.push_str(text);
                                    }
                                    ClaudeEvent::Result { text, is_error, .. } => {
                                        let mut output = accumulated_output.write().await;
                                        if !output.is_empty() {
                                            output.push('\n');
                                        }
                                        output.push_str(text);
                                        if *is_error {
                                            *result_error.write().await = Some(text.clone());
                                        }
                                    }
                                    _ => {}
                                }
                                let _ = event_tx.send(evt);
                            }
                        }
                        Err(_) => {
                            // Non-JSON line — treat as raw text
                            accumulated_output.write().await.push_str(&line);
                            let _ = event_tx.send(ClaudeEvent::TextDelta {
                                text: line,
                            });
                        }
                    }
                }
            }
        });
    }
}

/// Parse a JSON line from Claude CLI stream-json output into a ClaudeEvent.
async fn parse_claude_event(
    json: &serde_json::Value,
    session_id: &Arc<Mutex<Option<String>>>,
) -> Option<ClaudeEvent> {
    let msg_type = json.get("type")?.as_str()?;

    match msg_type {
        "system" => {
            let subtype = json.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
            let sid = json.get("session_id").and_then(|s| s.as_str()).map(|s| s.to_string());
            let model = json.get("model").and_then(|s| s.as_str()).map(|s| s.to_string());
            if let Some(ref s) = sid {
                *session_id.lock().await = Some(s.clone());
            }
            Some(ClaudeEvent::SystemInit {
                session_id: sid.unwrap_or_default(),
                model,
                message: Some(format!("system:{}", subtype)),
            })
        }

        "assistant" => {
            // Full assistant message with content blocks
            let message = json.get("message")?;
            let content = message.get("content").cloned().unwrap_or(serde_json::Value::Null);

            // Check for tool_use blocks in content
            if let Some(arr) = content.as_array() {
                for block in arr {
                    if let Some(block_type) = block.get("type").and_then(|t| t.as_str()) {
                        if block_type == "tool_use" {
                            let tool = block.get("name").and_then(|n| n.as_str()).unwrap_or("unknown").to_string();
                            let input = block.get("input").cloned();
                            return Some(ClaudeEvent::ToolUse { tool, input });
                        }
                    }
                }
            }

            Some(ClaudeEvent::AssistantMessage { content })
        }

        "result" => {
            let text = json.get("result").and_then(|r| r.as_str()).unwrap_or("").to_string();
            let is_error = json.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
            let sid = json.get("session_id").and_then(|s| s.as_str()).unwrap_or("").to_string();

            if !sid.is_empty() {
                *session_id.lock().await = Some(sid.clone());
            }

            Some(ClaudeEvent::Result { session_id: sid, text, is_error })
        }

        // Stream events (content_block_delta with text_delta)
        "content_block_delta" => {
            let delta = json.get("delta")?;
            let delta_type = delta.get("type").and_then(|t| t.as_str())?;
            if delta_type == "text_delta" {
                let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                Some(ClaudeEvent::TextDelta { text })
            } else {
                None
            }
        }

        "content_block_start" => {
            let block = json.get("content_block")?;
            let block_type = block.get("type").and_then(|t| t.as_str())?;
            if block_type == "tool_use" {
                let tool = block.get("name").and_then(|n| n.as_str()).unwrap_or("unknown").to_string();
                Some(ClaudeEvent::ToolUse { tool, input: None })
            } else {
                None
            }
        }

        _ => None,
    }
}
