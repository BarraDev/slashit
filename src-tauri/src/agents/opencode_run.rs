//! The OpenCode Worker: one `opencode acp` process per run, driven over ACP v1.
//!
//! [`super::coding_run::CodingRunner`] exposes this run through the same
//! operations as the Claude run, so `TaskExecutor` never names OpenCode. Here
//! stay the ACP lifecycle, the mapping of ACP updates to
//! [`CodingRunEvent`]s, and the permission policy.
//!
//! Lifecycle: spawn (holding the provider lease), initialize, create a
//! session, send the Task prompt, forward updates, then end the process. A
//! turn that does not end with `end_turn` is a failure, and so is a turn that
//! times out.
//!
//! Permission policy: every permission request is refused. OpenCode asks
//! before tools it does not auto-approve, and SlashIt has no interactive
//! approval path for that yet, so nothing is approved by default. A request
//! with no reject option is cancelled. The connection itself cancels a
//! request that gets no answer in time, and nothing is ever approved by a
//! timeout, a dropped answer or a closed connection.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, Mutex, OnceCell, RwLock};
use tokio::task::JoinHandle;

use super::coding_run::CodingRunEvent;
use super::provider_lease::ProviderLease;
use super::runner::provider_env;
use crate::acp::v1::connection::{AgentEvent, Connection, EventStream, SessionRef};
use crate::acp::v1::process::SpawnSpec;
use crate::acp::v1::types::{
    PermissionDecision, PermissionKind, PermissionOption, SessionUpdate, StopReason,
};

/// The program a run starts. A test passes its fake agent instead.
pub const OPENCODE_PROGRAM: &str = "opencode";

/// Bounds each `initialize` and `session/new`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounds one prompt turn. A turn that runs longer is cancelled.
const TURN_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
/// How long a finished run waits for OpenCode to exit on its own after its
/// input closes, before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(5);
/// How long a cancelled run waits for its process before it is killed.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Permission requests are answered at once, so this is only the connection's
/// upper bound on an answer that never comes.
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What a run needs to start. The prompt is written to the agent's session
/// and never passed as an argument.
pub struct OpenCodeRunConfig {
    pub prompt: String,
    /// Absolute. The session's working directory and the agent's cwd.
    pub working_dir: std::path::PathBuf,
    /// The program to start: [`OPENCODE_PROGRAM`] in production.
    pub program: String,
    /// The program's arguments: `["acp"]` in production.
    pub args: Vec<String>,
}

/// A running OpenCode session. Dropping it ends the process and its group.
pub struct OpenCodeRunner {
    connection: Connection,
    session: SessionRef,
    events: broadcast::Sender<CodingRunEvent>,
    /// The receiver created with the channel, handed to the first
    /// [`subscribe`](Self::subscribe), so the first events are not missed.
    first_subscriber: std::sync::Mutex<Option<broadcast::Receiver<CodingRunEvent>>>,
    output: Arc<RwLock<String>>,
    /// The prompt turn. Taken once, by the first `wait`.
    turn: Mutex<Option<JoinHandle<Result<(), String>>>>,
    /// The event reader, joined by `wait` so every event of the run is sent
    /// before `wait` returns.
    reader: Mutex<Option<JoinHandle<()>>>,
    /// The run's outcome, set by the first `wait`. Later waits read it.
    outcome: OnceCell<Result<(), String>>,
}

impl OpenCodeRunner {
    /// Starts the process, initializes the protocol, creates a session and
    /// sends the prompt. `lease`, when present, is held by the process for
    /// its whole life. An error here means no session started, and the
    /// process was ended.
    pub async fn start(
        config: OpenCodeRunConfig,
        lease: Option<ProviderLease>,
    ) -> Result<Self, String> {
        let spec = SpawnSpec {
            program: config.program.clone(),
            args: config.args.clone(),
            cwd: config.working_dir.clone(),
            env: provider_env(std::env::vars_os())
                .into_iter()
                .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
                .collect(),
            clear_env: true,
            request_timeout: REQUEST_TIMEOUT,
            permission_timeout: PERMISSION_TIMEOUT,
        };
        let connection = Connection::spawn_leased(&spec, lease).map_err(|e| e.to_string())?;
        connection.initialize().await.map_err(|e| e.to_string())?;
        let session = connection
            .new_session(&config.working_dir)
            .await
            .map_err(|e| e.to_string())?;
        let stream = connection
            .take_events()
            .ok_or_else(|| "the OpenCode event stream was already taken".to_string())?;

        let (events, first) = broadcast::channel(512);
        let output = Arc::new(RwLock::new(String::new()));
        let reader = tokio::spawn(forward_events(
            stream,
            connection.clone(),
            events.clone(),
            output.clone(),
        ));
        let _ = events.send(CodingRunEvent::SystemInit {
            session_id: session.id().to_string(),
            model: None,
            message: None,
        });

        let turn = tokio::spawn(run_turn(
            connection.clone(),
            session.clone(),
            config.prompt,
            events.clone(),
            output.clone(),
        ));

        Ok(Self {
            connection,
            session,
            events,
            first_subscriber: std::sync::Mutex::new(Some(first)),
            output,
            turn: Mutex::new(Some(turn)),
            reader: Mutex::new(Some(reader)),
            outcome: OnceCell::new(),
        })
    }

    /// Subscribe to the run's events. See [`CodingRunEvent`].
    pub fn subscribe(&self) -> broadcast::Receiver<CodingRunEvent> {
        let mut first = self.first_subscriber.lock().unwrap_or_else(|p| p.into_inner());
        first.take().unwrap_or_else(|| self.events.subscribe())
    }

    /// Waits for the turn to end, then ends the process and joins the event
    /// reader. `Ok(true)` is a turn that ended with `end_turn`. Any other end
    /// is an `Err` that says why, and so is a run that was cancelled.
    pub async fn wait(&self) -> Result<bool, String> {
        let outcome = self
            .outcome
            .get_or_init(|| async {
                let turn = self.turn.lock().await.take();
                let result = match turn {
                    Some(handle) => handle
                        .await
                        .unwrap_or_else(|e| Err(format!("the OpenCode turn task failed: {e}"))),
                    None => Err("the OpenCode turn was already taken".to_string()),
                };
                // The turn is over, so OpenCode has nothing more to say. Its
                // input closes, and it is given a grace period to exit.
                let _ = self.connection.shutdown(EXIT_GRACE).await;
                if let Some(reader) = self.reader.lock().await.take() {
                    let _ = reader.await;
                }
                result
            })
            .await;
        outcome.clone().map(|()| true)
    }

    /// The text OpenCode has produced so far.
    pub async fn get_output(&self) -> String {
        self.output.read().await.clone()
    }

    /// The final text of the run. OpenCode's final answer is the text it
    /// streamed, so this is the output.
    pub async fn final_result_text(&self) -> Result<String, String> {
        Ok(self.get_output().await)
    }

    /// Cancels the turn and ends the process. A run that was cancelled ends
    /// with an `Err` from `wait`, and its process group is ended if it does
    /// not exit within the grace period.
    pub async fn kill(&self) -> Result<(), String> {
        let _ = self.connection.cancel(&self.session).await;
        self.connection
            .shutdown(KILL_GRACE)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Runs the single prompt turn and reports how it ended.
async fn run_turn(
    connection: Connection,
    session: SessionRef,
    prompt: String,
    events: broadcast::Sender<CodingRunEvent>,
    output: Arc<RwLock<String>>,
) -> Result<(), String> {
    let result = match connection.prompt(&session, &prompt, TURN_TIMEOUT).await {
        Ok(turn) if turn.stop_reason == StopReason::EndTurn => Ok(()),
        Ok(turn) => Err(format!(
            "OpenCode ended the turn without finishing it (stop reason: {})",
            stop_reason_name(turn.stop_reason)
        )),
        Err(error) => Err(error.to_string()),
    };
    match &result {
        Ok(()) => {
            let text = output.read().await.clone();
            let _ = events.send(CodingRunEvent::Result {
                session_id: session.id().to_string(),
                text,
                is_error: false,
            });
        }
        Err(message) => {
            let _ = events.send(CodingRunEvent::Error { message: message.clone() });
        }
    }
    result
}

/// Maps ACP events to [`CodingRunEvent`]s and answers permission requests.
/// Ends when the connection closes.
async fn forward_events(
    mut stream: EventStream,
    connection: Connection,
    events: broadcast::Sender<CodingRunEvent>,
    output: Arc<RwLock<String>>,
) {
    while let Some(event) = stream.recv().await {
        match event {
            AgentEvent::Update { update, .. } => match update {
                SessionUpdate::AgentMessageChunk { text: Some(text) } => {
                    output.write().await.push_str(&text);
                    let _ = events.send(CodingRunEvent::TextDelta { text });
                }
                SessionUpdate::ToolCall {
                    tool_call_id,
                    title,
                    kind,
                    raw_input,
                    ..
                } => {
                    let tool = title.or(kind).unwrap_or(tool_call_id);
                    let _ = events.send(CodingRunEvent::ToolUse {
                        tool,
                        input: raw_input,
                    });
                }
                _ => {}
            },
            AgentEvent::PermissionRequested {
                id,
                tool_call_id,
                options,
                ..
            } => {
                let _ = events.send(CodingRunEvent::Error {
                    message: format!(
                        "OpenCode asked permission for {tool_call_id}, and SlashIt refused it. \
                         Permission requests are not approved unattended."
                    ),
                });
                let decision = reject_decision(&options);
                let _ = connection.answer_permission(id, decision);
            }
            AgentEvent::Closed { .. } => break,
            _ => {}
        }
    }
}

/// The refusal for a permission request: the first reject option the agent
/// offered, or a cancellation when it offered none.
fn reject_decision(options: &[PermissionOption]) -> PermissionDecision {
    options
        .iter()
        .find(|option| option.kind == PermissionKind::RejectOnce)
        .or_else(|| options.iter().find(|option| !option.kind.is_allow()))
        .map(|option| PermissionDecision::Reject {
            option_id: option.option_id.clone(),
        })
        .unwrap_or(PermissionDecision::Cancel)
}

fn stop_reason_name(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(id: &str, kind: PermissionKind) -> PermissionOption {
        PermissionOption {
            option_id: id.to_string(),
            name: id.to_string(),
            kind,
        }
    }

    #[test]
    fn a_permission_request_is_refused_and_never_allowed() {
        let offered = [
            option("allow", PermissionKind::AllowOnce),
            option("always", PermissionKind::AllowAlways),
            option("no", PermissionKind::RejectOnce),
        ];
        assert_eq!(
            reject_decision(&offered),
            PermissionDecision::Reject {
                option_id: "no".into()
            }
        );
    }

    #[test]
    fn a_request_with_only_allow_options_is_cancelled() {
        let offered = [option("allow", PermissionKind::AllowOnce)];
        assert_eq!(reject_decision(&offered), PermissionDecision::Cancel);
    }

    #[test]
    fn a_request_without_options_is_cancelled() {
        assert_eq!(reject_decision(&[]), PermissionDecision::Cancel);
    }

    /// A run against the fake ACP agent, in the given scenario. No model runs.
    fn fixture(scenario: &str) -> OpenCodeRunConfig {
        let exe = std::env::current_exe().expect("test executable path");
        let debug_dir = exe
            .parent()
            .and_then(std::path::Path::parent)
            .expect("target/debug directory");
        OpenCodeRunConfig {
            prompt: "make the change".into(),
            working_dir: std::env::temp_dir(),
            program: debug_dir
                .join("acp_fixture_agent")
                .to_string_lossy()
                .into_owned(),
            args: vec![scenario.to_string()],
        }
    }

    /// Everything the run sent before it finished. `wait` joins the reader, so
    /// nothing more arrives after it returns.
    fn sent(events: &mut broadcast::Receiver<CodingRunEvent>) -> Vec<CodingRunEvent> {
        let mut out = Vec::new();
        while let Ok(event) = events.try_recv() {
            out.push(event);
        }
        out
    }

    #[tokio::test]
    async fn a_run_streams_its_text_and_tool_activity_and_ends_with_end_turn() {
        let run = OpenCodeRunner::start(fixture("ok"), None).await.expect("start");
        let mut events = run.subscribe();
        assert_eq!(run.wait().await, Ok(true));
        let seen = sent(&mut events);
        assert!(seen.iter().any(|e| matches!(e, CodingRunEvent::SystemInit { .. })));
        assert!(seen
            .iter()
            .any(|e| matches!(e, CodingRunEvent::TextDelta { text } if text == "hello")));
        assert!(seen.iter().any(|e| matches!(
            e,
            CodingRunEvent::ToolUse { tool, .. } if tool == "read file"
        )));
        assert!(seen
            .iter()
            .any(|e| matches!(e, CodingRunEvent::Result { is_error: false, .. })));
        // Thoughts are not the answer, so they are not in the output.
        assert_eq!(run.get_output().await, "hello");
        assert_eq!(run.final_result_text().await.as_deref(), Ok("hello"));
    }

    #[tokio::test]
    async fn a_permission_request_is_refused_and_the_run_does_not_approve_it() {
        let run = OpenCodeRunner::start(fixture("permission"), None)
            .await
            .expect("start");
        let mut events = run.subscribe();
        assert_eq!(run.wait().await, Ok(true));
        let seen = sent(&mut events);
        // The fake agent echoes the outcome it received.
        let output = run.get_output().await;
        assert!(output.contains("perm-outcome=selected:reject-once"), "{output}");
        assert!(!output.contains("allow-once"), "{output}");
        assert!(seen.iter().any(|e| matches!(
            e,
            CodingRunEvent::Error { message } if message.contains("refused")
        )));
    }

    #[tokio::test]
    async fn a_bad_protocol_version_is_a_startup_failure() {
        let error = OpenCodeRunner::start(fixture("bad-version"), None)
            .await
            .err()
            .expect("start must fail");
        assert!(error.contains("protocol version"), "{error}");
    }

    #[tokio::test]
    async fn a_process_that_exits_before_initialize_is_a_startup_failure() {
        let mut config = fixture("ok");
        config.program = "/bin/sh".into();
        config.args = vec!["-c".into(), "exit 0".into()];
        assert!(OpenCodeRunner::start(config, None).await.is_err());
    }

    #[tokio::test]
    async fn a_process_that_exits_during_the_turn_fails_the_run() {
        let run = OpenCodeRunner::start(fixture("exit-on-prompt"), None)
            .await
            .expect("start");
        assert!(run.wait().await.is_err());
    }

    #[tokio::test]
    async fn kill_ends_a_run_that_is_waiting_on_the_agent() {
        let run = OpenCodeRunner::start(fixture("permission"), None)
            .await
            .expect("start");
        // Whether the cancel lands before or after the turn ends, the run
        // must finish, and its process must be reaped.
        tokio::time::timeout(Duration::from_secs(30), async {
            let _ = run.kill().await;
            let _ = run.wait().await;
        })
        .await
        .expect("kill and wait must complete");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_lease_is_released_once_the_run_has_ended() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lease");
        let lease = crate::agents::provider_lease::acquire(&path)
            .expect("acquire")
            .expect("unix lease");
        let run = OpenCodeRunner::start(fixture("ok"), Some(lease))
            .await
            .expect("start");
        assert_eq!(run.wait().await, Ok(true));
        crate::agents::provider_lease::prove_absent(&path)
            .expect("the provider has ended and nothing holds the lease");
    }
}
