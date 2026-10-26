// SPDX-License-Identifier: MPL-2.0
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
//! Permission policy: [`OPENCODE_POLICY_CONTENT`] denies every tool except
//! reads, searches and edits, and it refuses reads, writes, searches and
//! globs whose path is outside the Task Checkout. A denied tool is removed from
//! the model's tool list, so it issues no request. OpenCode is started under
//! [`policy_env`], which also points its global configuration at an empty
//! directory that this run owns, and turns off the repository's own
//! configuration. The user's global configuration would otherwise override the
//! policy: its agent permissions win over the inline policy, and its plugins
//! load inside the Worker.
//!
//! The policy was checked against OpenCode 2.0.32 with a scripted model and no
//! paid provider, for the tools the model was offered (read, glob, grep, edit,
//! write, shell, execute, webfetch, subagent) and for the scenarios named
//! there. Known gaps: a symbolic link inside the checkout is followed by `read`,
//! because the path check does not see its target, and that is not closed here.
//! Global commands and the `question`, `skill` and `websearch` tools were not
//! exercised individually.
//!
//! SlashIt has no interactive approval path yet, so nothing is approved. A
//! request that arrives anyway is refused, and a request with no reject option
//! is cancelled. The connection itself cancels a request that gets no answer in
//! time, and nothing is ever approved by a timeout, a dropped answer or a closed
//! connection. Each refusal is recorded on the run
//! ([`OpenCodeRunner::refused_permissions`]). A turn that still ends with
//! `end_turn` is `Ok`, so the executor decides from the checkout whether the
//! Task was finished.

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
    config_option_value, PermissionDecision, PermissionKind, PermissionOption, SessionUpdate,
    StopReason,
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

/// The OpenCode permission policy every run is started under. Denied tools are
/// removed from the model's list, so a denied call is never offered. The
/// allowed tools are read, glob, grep and edit (OpenCode's `edit` permission
/// also covers `write`). `external_directory` refuses paths outside the Task
/// Checkout, including a relative `../` path, for the file tools that were
/// checked. Shell, network fetch, subagents and the code runtime (`execute`) are
/// not offered, because they are denied by default. An inline policy outranks
/// the project's `opencode.json`, and [`policy_env`] turns that file off as well.
pub const OPENCODE_POLICY_CONTENT: &str = r#"{"permission":{"*":"deny","read":"allow","glob":"allow","grep":"allow","edit":"allow","external_directory":"deny"}}"#;

/// The variables that pin the OpenCode policy for one process. The Task
/// Checkout is written by a model, so a repository's `opencode.json` must not
/// decide the policy. With project configuration off, that file is not read,
/// and neither are the MCP servers it declares, nor the project's plugins and
/// commands. Probes showed the MCP server and the project plugin starting
/// without any prompt, and a project command's shell substitution running when
/// the prompt named it. `config_dir` replaces the user's global configuration
/// directory, so the user's global permissions, agent overrides and plugins are
/// not read either. It must be empty and owned by this run.
pub fn policy_env(config_dir: &str) -> Vec<(String, String)> {
    vec![
        ("OPENCODE_DISABLE_PROJECT_CONFIG".to_string(), "1".to_string()),
        ("OPENCODE_CONFIG_DIR".to_string(), config_dir.to_string()),
        ("OPENCODE_CONFIG_CONTENT".to_string(), OPENCODE_POLICY_CONTENT.to_string()),
    ]
}

/// The environment one OpenCode process is started with: the provider
/// allowlist from `parent`, plus [`policy_env`]. The parent's own `OPENCODE_*`
/// variables are never passed, so a user's setting cannot replace the policy.
pub fn opencode_spawn_env(
    parent: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    config_dir: &str,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = provider_env(parent)
        .into_iter()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    env.extend(policy_env(config_dir));
    env
}

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
    /// The model the run must use, as `provider/model`. Required: a run
    /// without one would use OpenCode's own default, which SlashIt does not
    /// choose and may bill, so [`OpenCodeRunner::start`] refuses it.
    pub model: Option<String>,
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
    /// One entry per permission request this run refused, in arrival order.
    refusals: Refusals,
    /// The configuration directory OpenCode was started with, for the tests
    /// that check when it is removed. The process supervisor owns the
    /// directory and removes it only after the process has been reaped.
    #[cfg(test)]
    config_path: std::path::PathBuf,
}

/// The refused permission requests of one run, shared with its event reader.
type Refusals = Arc<std::sync::Mutex<Vec<String>>>;

/// Selects `model` for the session and checks that OpenCode now reports it as
/// the current model. OpenCode refuses a model it does not know, with an
/// error. A reply that reports another model, or no model, is refused too, so
/// a run never proceeds on a model SlashIt did not choose.
async fn select_model(
    connection: &Connection,
    session: &SessionRef,
    model: &str,
) -> Result<(), String> {
    let result = connection
        .set_session_config_option(session, "model", model)
        .await
        .map_err(|e| format!("OpenCode did not accept the model {model}: {e}"))?;
    match config_option_value(&result, "model") {
        Some(current) if current == model => Ok(()),
        Some(current) => Err(format!(
            "OpenCode reports the model {current}, not the selected model {model}"
        )),
        None => Err(format!(
            "OpenCode did not report a current model after {model} was selected"
        )),
    }
}

impl OpenCodeRunner {
    /// Starts the process, initializes the protocol, creates a session and
    /// sends the prompt. `lease`, when present, is held by the process for
    /// its whole life. An error here means no session started, and the
    /// process was ended. The configuration directory is handed to the
    /// process supervisor, which removes it once the process has been reaped,
    /// so no path, including a kill or a failed start, removes it earlier.
    pub async fn start(
        config: OpenCodeRunConfig,
        lease: Option<ProviderLease>,
    ) -> Result<Self, String> {
        // Checked before anything starts, so a run without a model never
        // spawns OpenCode.
        let model = config.model.clone().ok_or_else(|| {
            "This OpenCode run names no model, so it was not started. SlashIt does not run OpenCode's default model.".to_string()
        })?;
        // Empty and private to this run, so the user's global OpenCode
        // configuration is not read (see `policy_env`).
        let config_dir = tempfile::Builder::new()
            .prefix("slashit-opencode-config-")
            .tempdir()
            .map_err(|e| format!("The OpenCode configuration directory could not be created: {e}"))?;
        let config_path = config_dir
            .path()
            .to_str()
            .ok_or_else(|| "The OpenCode configuration directory path is not UTF-8".to_string())?
            .to_string();
        let spec = SpawnSpec {
            program: config.program.clone(),
            args: config.args.clone(),
            cwd: config.working_dir.clone(),
            env: opencode_spawn_env(std::env::vars_os(), &config_path),
            clear_env: true,
            request_timeout: REQUEST_TIMEOUT,
            permission_timeout: PERMISSION_TIMEOUT,
        };
        let connection = Connection::spawn_holding(&spec, lease, Box::new(config_dir))
            .map_err(|e| e.to_string())?;
        connection.initialize().await.map_err(|e| e.to_string())?;
        let session = connection
            .new_session(&config.working_dir)
            .await
            .map_err(|e| e.to_string())?;
        // The model is selected before the prompt is sent, and the run stops
        // here unless OpenCode reports that model as the current one.
        select_model(&connection, &session, &model).await?;
        let stream = connection
            .take_events()
            .ok_or_else(|| "the OpenCode event stream was already taken".to_string())?;

        let (events, first) = broadcast::channel(512);
        let output = Arc::new(RwLock::new(String::new()));
        let refusals: Refusals = Arc::new(std::sync::Mutex::new(Vec::new()));
        let reader = tokio::spawn(forward_events(
            stream,
            connection.clone(),
            events.clone(),
            output.clone(),
            refusals.clone(),
        ));
        let _ = events.send(CodingRunEvent::SystemInit {
            session_id: session.id().to_string(),
            model: Some(model),
            message: None,
        });

        let turn = tokio::spawn(run_turn(connection.clone(), session.clone(), config.prompt));

        Ok(Self {
            connection,
            session,
            events,
            first_subscriber: std::sync::Mutex::new(Some(first)),
            output,
            turn: Mutex::new(Some(turn)),
            reader: Mutex::new(Some(reader)),
            outcome: OnceCell::new(),
            refusals,
            #[cfg(test)]
            config_path: std::path::PathBuf::from(config_path),
        })
    }

    /// The permission requests this run refused, one entry each, in arrival
    /// order. A refusal does not end the run: OpenCode may still finish the
    /// Task another way, so the caller decides what the refusals mean.
    pub fn refused_permissions(&self) -> Vec<String> {
        self.refusals.lock().unwrap_or_else(|p| p.into_inner()).clone()
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
                // A process that is not confirmed ended fails the run, even when
                // its turn finished: its tools may still be running.
                let ended = self.connection.shutdown(EXIT_GRACE).await;
                let result = match (result, ended) {
                    (result, Ok(_)) => result,
                    (Ok(()), Err(e)) => {
                        Err(format!("the OpenCode process could not be confirmed ended: {e}"))
                    }
                    (Err(turn), Err(e)) => Err(format!(
                        "{turn}; the OpenCode process could not be confirmed ended: {e}"
                    )),
                };
                if let Some(reader) = self.reader.lock().await.take() {
                    let _ = reader.await;
                }
                // The terminal event follows the reader, so its text holds every
                // update of the turn, and no update follows it.
                match &result {
                    Ok(()) => {
                        let text = self.output.read().await.clone();
                        let _ = self.events.send(CodingRunEvent::Result {
                            session_id: self.session.id().to_string(),
                            text,
                            is_error: false,
                        });
                    }
                    Err(message) => {
                        let _ = self.events.send(CodingRunEvent::Error { message: message.clone() });
                    }
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

impl Drop for OpenCodeRunner {
    /// A run dropped while live ends its process group. The event reader holds
    /// a clone of the connection, so the connection's own drop would never run
    /// first. A run whose `wait` has finished has nothing left to kill.
    fn drop(&mut self) {
        if self.outcome.get().is_none() {
            self.connection.force_kill();
        }
    }
}

/// Runs the single prompt turn and reports how it ended. The terminal event is
/// sent by `wait`, once the event reader has delivered the turn's updates.
async fn run_turn(connection: Connection, session: SessionRef, prompt: String) -> Result<(), String> {
    match connection.prompt(&session, &prompt, TURN_TIMEOUT).await {
        Ok(turn) if turn.stop_reason == StopReason::EndTurn => Ok(()),
        Ok(turn) => Err(format!(
            "OpenCode ended the turn without finishing it (stop reason: {})",
            stop_reason_name(turn.stop_reason)
        )),
        Err(error) => Err(error.to_string()),
    }
}

/// Maps ACP events to [`CodingRunEvent`]s and answers permission requests.
/// Ends when the connection closes.
async fn forward_events(
    mut stream: EventStream,
    connection: Connection,
    events: broadcast::Sender<CodingRunEvent>,
    output: Arc<RwLock<String>>,
    refusals: Refusals,
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
                refusals
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(format!("tool call {tool_call_id}"));
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
    fn the_spawn_env_pins_the_policy_and_drops_the_users_opencode_variables() {
        use std::ffi::OsString;
        let parent = [
            ("HOME", "/home/worker"),
            ("PATH", "/usr/bin"),
            ("OPENCODE_CONFIG_CONTENT", r#"{"permission":{"*":"allow"}}"#),
            ("OPENCODE_CONFIG_DIR", "/home/worker/.config/opencode"),
            ("OPENCODE_DISABLE_PROJECT_CONFIG", "0"),
            ("OPENCODE_CONFIG", "/tmp/elsewhere.json"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let env: std::collections::HashMap<String, String> =
            opencode_spawn_env(parent, "/run/slashit-config").into_iter().collect();

        assert_eq!(env.get("HOME").map(String::as_str), Some("/home/worker"));
        assert_eq!(env.get("OPENCODE_DISABLE_PROJECT_CONFIG").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("OPENCODE_CONFIG_DIR").map(String::as_str),
            Some("/run/slashit-config"),
            "the user's global configuration directory is replaced by the run's own"
        );
        assert!(!env.contains_key("OPENCODE_CONFIG"), "the user's config file is not passed");
        assert_eq!(env["OPENCODE_CONFIG_CONTENT"], OPENCODE_POLICY_CONTENT);
    }

    #[test]
    fn the_policy_denies_by_default_and_allows_only_checkout_reads_and_edits() {
        let policy: serde_json::Value =
            serde_json::from_str(OPENCODE_POLICY_CONTENT).expect("the policy is JSON");
        assert_eq!(
            policy,
            serde_json::json!({"permission": {
                "*": "deny",
                "read": "allow",
                "glob": "allow",
                "grep": "allow",
                "edit": "allow",
                "external_directory": "deny"
            }})
        );
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
            model: Some(FIXTURE_MODEL.into()),
        }
    }

    /// The local model the fixture runs select. A `provider/model` id that a
    /// test host can name, and that no paid provider serves.
    const FIXTURE_MODEL: &str = "ollama/llama3.2:3b";

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
        assert!(run.refused_permissions().is_empty());
    }

    #[tokio::test]
    async fn the_selected_model_is_set_before_the_prompt_and_reported_at_start() {
        let run = OpenCodeRunner::start(fixture("model-echo"), None)
            .await
            .expect("start");
        let mut events = run.subscribe();
        assert_eq!(run.wait().await, Ok(true));
        let seen = sent(&mut events);
        assert!(seen.iter().any(|e| matches!(
            e,
            CodingRunEvent::SystemInit { model: Some(m), .. } if m == FIXTURE_MODEL
        )));
        // The fixture records the selection and echoes it from the prompt turn,
        // so the prompt was answered after the selection.
        assert_eq!(run.get_output().await, format!("model={FIXTURE_MODEL}"));
    }

    #[tokio::test]
    async fn a_model_the_agent_refuses_fails_the_start_and_no_prompt_is_sent() {
        let error = match OpenCodeRunner::start(fixture("model-refuse"), None).await {
            Err(error) => error,
            Ok(_) => panic!("a refused model must not start a run"),
        };
        assert!(error.contains("did not accept the model"), "{error}");
    }

    #[tokio::test]
    async fn a_model_the_agent_does_not_report_as_current_fails_the_start() {
        let error = match OpenCodeRunner::start(fixture("model-mismatch"), None).await {
            Err(error) => error,
            Ok(_) => panic!("a model the agent did not report must not start a run"),
        };
        assert!(error.contains("reports the model"), "{error}");
    }

    #[tokio::test]
    async fn a_run_without_a_model_is_refused_before_any_process_starts() {
        let mut config = fixture("ok");
        config.model = None;
        // A program that does not exist: a spawn attempt would fail with a
        // different message, so this message proves nothing was spawned.
        config.program = "/nonexistent/slashit-opencode-probe".into();
        let error = match OpenCodeRunner::start(config, None).await {
            Err(error) => error,
            Ok(_) => panic!("a run with no model must not start"),
        };
        assert!(error.contains("names no model"), "{error}");
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
        assert_eq!(run.refused_permissions(), vec!["tool call tc-1".to_string()]);
    }

    #[tokio::test]
    async fn a_refusal_is_recorded_even_when_the_turn_ends_normally() {
        // The turn ends with end_turn after the refusal, so `wait` is Ok. The
        // refusal is still on the run, for the executor to judge.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = fixture("permission-then-edit");
        config.working_dir = dir.path().to_path_buf();
        let run = OpenCodeRunner::start(config, None).await.expect("start");
        assert_eq!(run.wait().await, Ok(true));
        assert_eq!(run.refused_permissions(), vec!["tool call tc-1".to_string()]);
        assert!(dir.path().join("permitted-change.txt").exists());
    }

    #[tokio::test]
    async fn a_frame_that_arrives_after_the_input_closes_does_not_hang_wait() {
        // The turn ends normally, `wait` closes the agent's input, and the
        // agent sends one more frame before it exits. `wait` must still return.
        let run = OpenCodeRunner::start(fixture("notify-after-eof"), None)
            .await
            .expect("start");
        let outcome = tokio::time::timeout(Duration::from_secs(30), run.wait())
            .await
            .expect("wait must return once the agent has exited");
        assert_eq!(outcome, Ok(true));
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
    async fn dropping_a_live_run_ends_its_agent() {
        // `ignore-eof` keeps running after its input closes, so only a kill
        // can end it. Dropping the run must send that kill.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lease");
        let lease = crate::agents::provider_lease::acquire(&path)
            .expect("acquire")
            .expect("unix lease");
        let run = OpenCodeRunner::start(fixture("ignore-eof"), Some(lease))
            .await
            .expect("start");
        drop(run);
        let released = tokio::time::timeout(Duration::from_secs(10), async {
            while crate::agents::provider_lease::prove_absent(&path).is_err() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(released.is_ok(), "the dropped run's agent is still alive");
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

    /// Polls until `done` holds. The wait is bounded so that a missing event
    /// fails the test; it never decides an outcome by itself.
    #[cfg(unix)]
    async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}"));
    }

    #[cfg(unix)]
    fn lease_at(path: &std::path::Path) -> crate::agents::provider_lease::ProviderLease {
        crate::agents::provider_lease::acquire(path)
            .expect("acquire")
            .expect("unix lease")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_dropped_live_run_keeps_its_configuration_until_its_agent_has_ended() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lease_path = dir.path().join("lease");
        let run = OpenCodeRunner::start(fixture("ignore-eof"), Some(lease_at(&lease_path)))
            .await
            .expect("start");
        let config = run.config_path.clone();
        drop(run);
        // This test runs on a single-threaded runtime and has not yielded since
        // the kill was requested, so the agent is still running and its
        // configuration is in place.
        assert!(crate::agents::provider_lease::prove_absent(&lease_path).is_err());
        assert!(config.is_dir(), "the configuration was removed while the agent ran");
        eventually("the dropped run's agent did not end", || {
            crate::agents::provider_lease::prove_absent(&lease_path).is_ok()
        })
        .await;
        eventually("the configuration was not removed after its agent ended", || {
            !config.exists()
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_finished_run_removes_its_configuration_once_its_agent_has_ended() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lease_path = dir.path().join("lease");
        let run = OpenCodeRunner::start(fixture("ok"), Some(lease_at(&lease_path)))
            .await
            .expect("start");
        let config = run.config_path.clone();
        assert_eq!(run.wait().await, Ok(true));
        crate::agents::provider_lease::prove_absent(&lease_path)
            .expect("the agent has ended once its run has");
        eventually("the finished run's configuration was not removed", || !config.exists()).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancelled_run_removes_its_configuration_once_its_agent_has_been_reaped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lease_path = dir.path().join("lease");
        let run = OpenCodeRunner::start(fixture("permission"), Some(lease_at(&lease_path)))
            .await
            .expect("start");
        let config = run.config_path.clone();
        // `kill` returns once the agent has been reaped.
        let _ = run.kill().await;
        crate::agents::provider_lease::prove_absent(&lease_path)
            .expect("kill returned while the agent was still running");
        eventually("the cancelled run's configuration was not removed", || !config.exists()).await;
        let _ = run.wait().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_start_ends_its_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lease_path = dir.path().join("lease");
        let error = OpenCodeRunner::start(fixture("bad-version"), Some(lease_at(&lease_path)))
            .await
            .err()
            .expect("start must fail");
        assert!(error.contains("protocol version"), "{error}");
        eventually("the failed start's agent was not ended", || {
            crate::agents::provider_lease::prove_absent(&lease_path).is_ok()
        })
        .await;
    }
}
