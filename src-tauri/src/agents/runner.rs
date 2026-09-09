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
    /// Extra directories to expose to Claude via repeated `--add-dir` flags.
    /// Used when running from a meta-workspace cwd so the agent can also
    /// read/write a specific project tree.
    pub additional_dirs: Vec<std::path::PathBuf>,
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
    /// Handle to the stdout reader task. `wait()` joins this before returning so
    /// callers can read the full accumulated output without racing the reader.
    reader_handle: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

impl ClaudeRunner {
    /// Start a Claude Code CLI run with the given config.
    pub async fn start(config: ClaudeRunConfig) -> Result<Self, String> {
        Self::start_program("claude", config).await
    }

    /// Start `program` with the Claude Code CLI argument set.
    ///
    /// Split out from [`start`] only so the tests can point the runner at a
    /// stand-in process that follows the same stdout/exit contract. Production
    /// has exactly one caller and it passes `"claude"`.
    async fn start_program(
        program: impl AsRef<std::ffi::OsStr>,
        config: ClaudeRunConfig,
    ) -> Result<Self, String> {
        let mut cmd = Command::new(program);

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

        for dir in &config.additional_dirs {
            if !dir.is_dir() {
                eprintln!(
                    "[claude-runner] skipping --add-dir for missing directory: {}",
                    dir.display()
                );
                continue;
            }
            cmd.arg("--add-dir").arg(dir);
        }

        cmd.current_dir(&config.working_dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // A backstop, not the way runs are ended. Cancellation is cooperative
        // and kills explicitly, because only the code that owns the run can
        // also record what happened to it. But nothing here is dropped while
        // its process is meant to carry on — every caller waits and then kills
        // — so a `ClaudeRunner` that goes away with a live child means
        // something skipped its own cleanup: a panic unwinding past it, a
        // future dropped by a path added later, the runtime shutting down.
        // Without this, the default is to leave that agent running with
        // nothing pointing at it.
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn()
            .map_err(|e| format!("Failed to spawn claude: {}. Is claude CLI installed?", e))?;
        eprintln!("[claude-runner] spawned pid={:?}", child.id());

        // Take stdout here, while the child is still ours alone, and hand it
        // straight to the reader. Draining stdout then depends on nothing but
        // the pipe itself: not on the child mutex, and so not on whoever is
        // currently waiting for the process to exit.
        let stdout = child.stdout.take();

        let (event_tx, _) = broadcast::channel(512);

        let runner = Self {
            child: Arc::new(Mutex::new(child)),
            event_tx,
            session_id: Arc::new(Mutex::new(config.session_id)),
            accumulated_output: Arc::new(RwLock::new(String::new())),
            result_error: Arc::new(RwLock::new(None)),
            reader_handle: Mutex::new(None),
        };

        let handle = runner.start_reader(stdout);
        *runner.reader_handle.lock().await = Some(handle);

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

        // The child has exited and nothing below needs it, so release it rather
        // than hold it through the join. Draining can outlast the process when a
        // descendant inherited the pipe; that join is still unbounded, and
        // bounding it belongs with cancellation rather than here, but at least
        // `kill()` is no longer queued behind a caller stuck waiting on output.
        drop(child);

        // Wait for the stdout reader to drain before callers read accumulated_output.
        // Without this, get_output() can race the reader and return partial/empty text.
        if let Some(handle) = self.reader_handle.lock().await.take() {
            let _ = handle.await;
        }

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

    /// Spawn the task that turns the child's stdout into events.
    ///
    /// It is given the pipe rather than the child on purpose. A reader that had
    /// to reach through the child mutex to find its own stdout could not run
    /// while `wait()` held that mutex, which is both a deadlock (`wait()` joins
    /// this task) and, for a talkative child, a stalled pipe.
    fn start_reader(
        &self,
        stdout: Option<tokio::process::ChildStdout>,
    ) -> tauri::async_runtime::JoinHandle<()> {
        let event_tx = self.event_tx.clone();
        let session_id = self.session_id.clone();
        let accumulated_output = self.accumulated_output.clone();
        let result_error = self.result_error.clone();

        tauri::async_runtime::spawn(async move {
            // Read stdout (NDJSON stream)
            if let Some(stdout) = stdout {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();

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
                                    ClaudeEvent::AssistantMessage { content } => {
                                        if let Some(arr) = content.as_array() {
                                            for block in arr {
                                                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                                                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                                        accumulated_output.write().await.push_str(text);
                                                    }
                                                }
                                            }
                                        }
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
        })
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

// The fixtures below are shell scripts, so these only make sense where there is
// a shell. The Windows and macOS CI jobs are compile-only, and the runtime job
// is Linux.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    /// How long a journey is allowed to take before we call it hung.
    ///
    /// This is a failure bound, not a synchronisation device: every fixture
    /// below exits on its own in well under a second, so a timeout here means
    /// the runner stopped making progress rather than that it needed longer.
    const DEADLINE: Duration = Duration::from_secs(15);

    /// A stand-in for the Claude Code CLI.
    ///
    /// It models the parts of the CLI the runner actually depends on -- argv,
    /// working directory, NDJSON on stdout, text on stderr, and an exit status
    /// -- and nothing else. It is deliberately not an emulator of the agent.
    struct Fixture {
        dir: TempDir,
        program: std::path::PathBuf,
    }

    impl Fixture {
        /// Locate one of the checked-in fixture scripts, and give it a
        /// directory of its own to run in.
        ///
        /// The script is a file that already exists rather than one written
        /// here and then run. `execve` refuses a file that any process has
        /// open for writing, and a process that has forked but has not yet
        /// reached its own `exec` still holds a copy of every descriptor its
        /// parent had open at the moment of the fork -- so a test that writes
        /// its own executable can have that write carried past the point where
        /// it runs the file, by an entirely unrelated concurrent spawn in the
        /// same test binary, and get `ETXTBSY`. A file nobody ever opens for
        /// writing cannot be caught that way.
        fn new(fixture: &str) -> Self {
            let dir = TempDir::new().expect("temp dir");
            let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/claude-cli")
                .join(fixture);
            assert!(
                program.is_file(),
                "the fixture script {} is missing",
                program.display()
            );
            Self { dir, program }
        }

        fn config(&self) -> ClaudeRunConfig {
            ClaudeRunConfig {
                prompt: "do the thing".to_string(),
                working_dir: self.dir.path().to_string_lossy().into_owned(),
                allowed_tools: vec!["Read".to_string(), "Edit".to_string()],
                max_turns: Some(50),
                max_budget_usd: None,
                session_id: Some("session-under-test".to_string()),
                resume_session: None,
                model: None,
                system_prompt: None,
                permission_mode: None,
                disable_mcp: false,
                additional_dirs: Vec::new(),
            }
        }

        async fn start(&self) -> ClaudeRunner {
            ClaudeRunner::start_program(&self.program, self.config())
                .await
                .expect("fixture should spawn")
        }
    }

    /// Emits a valid exchange and exits immediately, before the caller has any
    /// realistic chance to reach `wait()`.
    const FAST: &str = "fast";

    /// Delivers its events incrementally, after a moment spent starting up.
    ///
    /// The sleeps model a CLI that takes time to boot and then to think between
    /// messages. The leading one matters to the test as well as to the model:
    /// `subscribe()` can only be called once the runner exists, and the
    /// broadcast channel drops anything sent before a receiver joins, so a
    /// fixture that spoke instantly would make the first event a coin toss.
    /// Note that the tests which prove the ownership fix are the ones with no
    /// sleeps at all.
    const STREAMING: &str = "streaming";

    /// Writes far more than a pipe can hold before exiting.
    ///
    /// This is the same ownership problem as the fast case with a wider blast
    /// radius: a reader that cannot reach its own stdout leaves the pipe to
    /// fill, and the child then blocks in `write()` before it can exit, so the
    /// `wait()` holding the reader out is waiting for an exit it is preventing.
    const CHATTY: &str = "chatty";

    /// The `pad` in the `chatty` fixture quadruples three times from 16 bytes.
    const CHATTY_PAD: usize = 16 * 4 * 4 * 4;
    const CHATTY_LINES: usize = 512;

    const CRASHING: &str = "crashing";

    /// Announces itself and then stays alive until something ends it.
    ///
    /// `exec` on purpose: the process that waits is the same process the
    /// runner spawned, so the pid recorded here is the one the runner owns and
    /// the fixture has no descendant of its own to confuse the question.
    const BLOCKING: &str = "blocking";

    const CLAUDE_LEVEL_ERROR: &str = "claude-level-error";

    async fn bounded(label: &str, runner: &ClaudeRunner) -> Result<bool, String> {
        match tokio::time::timeout(DEADLINE, runner.wait()).await {
            Ok(result) => result,
            Err(_) => panic!(
                "{label}: wait() did not return within {DEADLINE:?}; the fixture exits on its own, \
                 so the runner stopped making progress"
            ),
        }
    }

    /// The case the product acceptance journey hit: a process that is already
    /// gone by the time anyone waits for it.
    ///
    /// Repeated, because a single pass proves nothing about a race. Each
    /// iteration is a fresh process, so scheduling differs between them.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_process_that_exits_immediately_is_still_waited_on_successfully() {
        let fixture = Fixture::new(FAST);
        for attempt in 0..40 {
            let runner = fixture.start().await;
            let ok = bounded(&format!("attempt {attempt}"), &runner).await;
            assert_eq!(ok, Ok(true), "attempt {attempt} should succeed");
            assert_eq!(
                runner.get_output().await,
                "done",
                "attempt {attempt} should have the result text, which only the \
                 stdout reader can produce"
            );
        }
    }

    /// The same case in the shape production uses.
    ///
    /// In the desktop app the queue loop is itself started with
    /// `tauri::async_runtime::spawn`, so starting the runner, waiting for it and
    /// reading its stdout all happen on Tauri's runtime. Doing only the wait
    /// there would be a configuration the product never has.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_immediate_exit_is_handled_on_the_runtime_production_uses() {
        let fixture = Fixture::new(FAST);
        for attempt in 0..40 {
            let program = fixture.program.clone();
            let config = fixture.config();
            let handle = tauri::async_runtime::spawn(async move {
                let runner = ClaudeRunner::start_program(&program, config)
                    .await
                    .map_err(|e| format!("fixture should spawn: {e}"))?;
                tokio::time::timeout(DEADLINE, runner.wait())
                    .await
                    .map_err(|_| "wait() never returned".to_string())??;
                Ok::<_, String>(runner.get_output().await)
            });
            let output = handle
                .await
                .expect("the waiting task should not panic")
                .unwrap_or_else(|e| panic!("attempt {attempt} failed: {e}"));
            assert_eq!(output, "done", "attempt {attempt} lost the result text");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn events_arriving_over_time_are_all_observed_before_wait_returns() {
        let fixture = Fixture::new(STREAMING);
        for attempt in 0..3 {
            streaming_round(&fixture, attempt).await;
        }
    }

    async fn streaming_round(fixture: &Fixture, attempt: usize) {
        let runner = fixture.start().await;
        let mut events = runner.subscribe();

        let collector = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Ok(event) = events.recv().await {
                seen.push(event);
            }
            seen
        });

        assert_eq!(
            bounded(&format!("streaming {attempt}"), &runner).await,
            Ok(true)
        );

        // The sender lives in the runner, so the collector only ends once the
        // runner is dropped.
        let output = runner.get_output().await;
        drop(runner);
        let seen = collector.await.expect("collector should not panic");

        assert_eq!(
            output, "alphabeta\ngamma",
            "accumulated output should hold the assistant text, the delta and \
             the result, in arrival order"
        );

        let kinds = seen.iter().map(describe).collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec!["system_init", "assistant_message", "text_delta", "result"],
            "every event should have been broadcast, in order"
        );

        match seen.first() {
            Some(ClaudeEvent::SystemInit {
                session_id, model, ..
            }) => {
                assert_eq!(session_id, "s-stream");
                assert_eq!(model.as_deref(), Some("fixture-model"));
            }
            other => panic!("expected a system init event first, got {other:?}"),
        }
    }

    fn describe(event: &ClaudeEvent) -> &'static str {
        match event {
            ClaudeEvent::SystemInit { .. } => "system_init",
            ClaudeEvent::TextDelta { .. } => "text_delta",
            ClaudeEvent::ToolUse { .. } => "tool_use",
            ClaudeEvent::AssistantMessage { .. } => "assistant_message",
            ClaudeEvent::Result { .. } => "result",
            ClaudeEvent::Error { .. } => "error",
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_process_that_outfills_the_pipe_still_finishes() {
        let fixture = Fixture::new(CHATTY);
        for attempt in 0..5 {
            let runner = fixture.start().await;
            assert_eq!(
                bounded(&format!("chatty {attempt}"), &runner).await,
                Ok(true)
            );

            let output = runner.get_output().await;
            let expected = CHATTY_PAD * CHATTY_LINES;
            assert!(
                output.len() >= expected,
                "attempt {attempt}: expected at least {expected} bytes of drained \
                 stdout, got {}; anything less means the pipe was not consumed \
                 while the process ran",
                output.len()
            );
            assert!(
                output.ends_with("\ndone"),
                "the result should still arrive last"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_non_zero_exit_reports_the_exit_code_and_stderr() {
        let fixture = Fixture::new(CRASHING);
        for attempt in 0..10 {
            let runner = fixture.start().await;
            let error = bounded(&format!("crashing {attempt}"), &runner)
                .await
                .expect_err("a non-zero exit should be an error");
            assert!(error.contains("Exit code 3"), "got: {error}");
            assert!(
                error.contains("something went wrong in the CLI"),
                "got: {error}"
            );
            assert!(
                runner.get_output().await.contains("--- STDERR ---"),
                "stderr should also be appended to the accumulated output"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_successful_exit_carrying_an_error_result_is_still_an_error() {
        let fixture = Fixture::new(CLAUDE_LEVEL_ERROR);
        for attempt in 0..10 {
            let runner = fixture.start().await;
            let error = bounded(&format!("claude error {attempt}"), &runner)
                .await
                .expect_err("is_error should surface as an error");
            assert_eq!(error, "the model refused");
        }
    }

    /// Whether `pid` is still a running process.
    ///
    /// A process that has exited but has not been reaped yet is still a
    /// process, and reading it as one would report a terminated agent as
    /// alive -- which is the exact mistake these tests exist to catch. The
    /// state is the first field after the last `)`, because the command field
    /// is parenthesised and may itself contain spaces and parentheses.
    #[cfg(target_os = "linux")]
    fn is_alive(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            != Some("Z")
    }

    /// The pid the blocking fixture wrote, once it has written it.
    #[cfg(target_os = "linux")]
    async fn blocked_pid(fixture: &Fixture) -> u32 {
        let path = fixture.dir.path().join("blocked.pid");
        let started = std::time::Instant::now();
        loop {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(pid) = text.trim().parse() {
                    return pid;
                }
            }
            assert!(
                started.elapsed() < DEADLINE,
                "the blocking fixture never announced its process"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_wait_that_loses_a_race_releases_the_child_to_be_killed() {
        // The shape cancellation uses. `wait()` holds the child for as long as
        // it is waiting, so if losing a `select!` did not release it, the
        // `kill()` that follows would queue behind the very process it is
        // trying to end -- and wait forever, because nothing else was ever
        // going to end it.
        let fixture = Fixture::new(BLOCKING);
        let runner = fixture.start().await;
        let pid = blocked_pid(&fixture).await;
        assert!(is_alive(pid), "the fixture must still be running");

        tokio::select! {
            biased;

            _ = runner.wait() => panic!("the fixture does not exit on its own"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }

        tokio::time::timeout(DEADLINE, runner.kill())
            .await
            .expect("kill must not block on the child a dropped wait was holding")
            .expect("kill must succeed");

        assert!(
            !is_alive(pid),
            "the process is still there after the kill that owns ending it"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_runner_dropped_with_a_live_child_does_not_leave_it_running() {
        // The backstop, for the paths that skip their own cleanup: a panic
        // unwinding past the kill, a future dropped by something added later.
        // Without `kill_on_drop` the default is to leave the agent running
        // with nothing pointing at it.
        let fixture = Fixture::new(BLOCKING);
        let pid = {
            // Named so it lives to the end of the block and is dropped here,
            // which is the event under test.
            let _runner = fixture.start().await;
            let pid = blocked_pid(&fixture).await;
            assert!(is_alive(pid), "the fixture must still be running");
            pid
        };

        // The signal goes out with the drop; the reaping that follows is the
        // runtime's to schedule, so this waits for it rather than assuming it
        // has already happened.
        let started = std::time::Instant::now();
        while is_alive(pid) {
            assert!(
                started.elapsed() < DEADLINE,
                "dropping the runner left process {pid} running"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
