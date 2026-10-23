//! The ACP v1 client connection.
//!
//! A `Connection` is a cheap handle. Its reader task owns stdout and routes
//! every frame: responses to waiting requests, agent requests to the
//! permission path, and notifications to the event stream. Writes go through
//! the writer task, so a request waiting for a slow response never blocks a
//! cancel, a permission answer, or a kill.
//!
//! Fail-closed rules enforced here:
//! - A protocol version other than 1 closes the connection.
//! - A malformed frame, an oversized frame, or an event backlog past
//!   `MAX_QUEUED_EVENTS` closes the connection and kills the child.
//! - A permission request is answered only by an explicit decision naming an
//!   offered option of the matching kind. Otherwise it is `cancelled`. A
//!   timer answers `cancelled` when no decision arrives in time.
//! - Agent requests this client does not implement (filesystem, terminal,
//!   elicitation) are answered with "method not found".

use super::frame::{self, FrameError, Message};
use super::process::{self, Control, LineRead, SpawnSpec, StderrTail};
use super::types::{
    self, AgentCapabilities, PermissionDecision, PermissionKind, PermissionOption,
    PermissionOutcome, SessionUpdate, StopReason, PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::process::ChildStdout;
use tokio::sync::{mpsc, oneshot, watch};

/// Events queued for the consumer. Past this, the connection fails closed
/// rather than growing without bound.
pub const MAX_QUEUED_EVENTS: usize = 1024;

const OUTBOUND_CAPACITY: usize = 64;
const REAP_TIMEOUT: Duration = Duration::from_secs(10);
const METHOD_NOT_FOUND: i64 = -32601;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AcpError {
    #[error("could not start the agent process: {0}")]
    Spawn(String),
    #[error("the working directory for the agent is not usable: {0}")]
    InvalidWorkingDirectory(String),
    #[error("{method} did not complete within the time limit")]
    Timeout { method: &'static str },
    #[error("the agent speaks protocol version {agent}, and this client speaks version 1")]
    UnsupportedProtocolVersion { agent: String },
    #[error("initialize has not completed on this connection")]
    NotInitialized,
    #[error("the agent did not return a usable session identifier")]
    InvalidSessionId,
    #[error("{method} failed with JSON-RPC error {code}: {message}")]
    Rpc {
        method: &'static str,
        code: i64,
        message: String,
    },
    #[error("the agent sent a frame that is not valid ACP: {0}")]
    Malformed(String),
    #[error("the agent sent a frame larger than the size limit")]
    FrameTooLarge,
    #[error("the agent process closed its output. Recent stderr: {stderr}")]
    ProcessExited { stderr: String },
    #[error("the connection is closed: {0}")]
    Closed(String),
    #[error("the agent sent an unexpected stop reason: {0}")]
    UnexpectedStopReason(String),
    #[error("no pending permission request matches this identifier")]
    PermissionNotPending,
    #[error(
        "the decision does not match an option the agent offered, so the request was cancelled"
    )]
    InvalidPermissionDecision,
    #[error("the agent process was not reaped within the time limit")]
    ReapTimeout,
}

/// Identifies one permission request. It is a counter, not the agent's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PermissionId(u64);

#[derive(Debug, Clone, PartialEq)]
pub enum ResolutionReason {
    /// The owner answered through `answer_permission`.
    Decision,
    /// `cancel` was called for the session.
    SessionCancelled,
    /// No decision arrived before the permission timeout.
    NoDecision,
}

/// Something the agent reported. None of these is an action. The consumer
/// decides what, if anything, to show or record.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Update {
        session_id: String,
        update: SessionUpdate,
    },
    PermissionRequested {
        id: PermissionId,
        session_id: String,
        tool_call_id: String,
        options: Vec<PermissionOption>,
    },
    PermissionResolved {
        id: PermissionId,
        outcome: PermissionOutcome,
        reason: ResolutionReason,
    },
    /// An agent request or notification this client does not handle.
    /// `method` is bounded and redacted.
    Unhandled {
        method: String,
    },
    /// A request that was malformed, answered `cancelled`, and dropped.
    InvalidPermissionRequest,
    Closed {
        reason: String,
    },
}

/// A session created by `session/new`. The id is opaque: it is stored and
/// sent back unchanged, and it is never parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    id: String,
    cwd: PathBuf,
}

impl SessionRef {
    pub fn id(&self) -> &str {
        &self.id
    }
    #[cfg(test)]
    pub(crate) fn for_tests(id: &str, cwd: &Path) -> Self {
        Self {
            id: id.to_string(),
            cwd: cwd.to_path_buf(),
        }
    }
}

/// The result of `session/prompt`, with the stop reason the spec defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnOutcome {
    pub stop_reason: StopReason,
}

type Reply = oneshot::Sender<Result<Value, AcpError>>;

struct PendingRequest {
    method: &'static str,
    reply: Reply,
}

struct PendingPermission {
    session_id: String,
    request_id: Value,
    options: Vec<PermissionOption>,
}

#[derive(Default)]
struct State {
    failure: Option<AcpError>,
    /// Set once `Closed` has been sent. A connection can fail twice, for
    /// example when `shutdown` records a failure before the reader sees the
    /// process exit, and consumers wait for `Closed` to know the stream is
    /// over. So it is sent once, after the first failure, whichever comes first.
    closed_sent: bool,
    capabilities: Option<AgentCapabilities>,
    pending: HashMap<u64, PendingRequest>,
    permissions: HashMap<u64, PendingPermission>,
}

struct Shared {
    outbound: mpsc::Sender<Option<Vec<u8>>>,
    control: mpsc::UnboundedSender<Control>,
    exit: watch::Receiver<process::ExitReport>,
    events: mpsc::UnboundedSender<AgentEvent>,
    event_backlog: AtomicUsize,
    event_rx: StdMutex<Option<mpsc::UnboundedReceiver<AgentEvent>>>,
    stderr: StderrTail,
    next_request_id: AtomicU64,
    next_permission_id: AtomicU64,
    request_timeout: Duration,
    permission_timeout: Duration,
    state: StdMutex<State>,
}

/// Kills the child when the last `Connection` handle goes away.
struct Owner {
    shared: Arc<Shared>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.shared.control.send(Control::Kill);
    }
}

#[derive(Clone)]
pub struct Connection {
    owner: Arc<Owner>,
}

/// The single consumer of agent events. Dropping it does not stop the agent.
pub struct EventStream {
    shared: Arc<Shared>,
    rx: mpsc::UnboundedReceiver<AgentEvent>,
}

impl EventStream {
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        let event = self.rx.recv().await;
        if event.is_some() {
            self.shared.event_backlog.fetch_sub(1, Ordering::SeqCst);
        }
        event
    }
}

impl Connection {
    /// Starts the agent process for a provider that owns a lease, or for a
    /// test with none. The lease is marked, inherited and recorded around the
    /// spawn, and it is held by the child until the process has exited. `held`
    /// is dropped only after the child has been reaped, on a kill, a shutdown
    /// and a start that fails after the spawn alike. A run puts there the
    /// configuration directory the child was started with. See `process::spawn`.
    pub fn spawn_holding(
        spec: &SpawnSpec,
        lease: Option<crate::agents::provider_lease::ProviderLease>,
        held: Box<dyn Send>,
    ) -> Result<Self, AcpError> {
        let owned = process::spawn(spec, lease, held)?;
        let (outbound, outbound_rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let (events, events_rx) = mpsc::unbounded_channel();
        process::spawn_writer(owned.stdin, outbound_rx);
        let stderr = process::spawn_stderr_capture(owned.stderr);
        let shared = Arc::new(Shared {
            outbound,
            control: owned.control,
            exit: owned.exit,
            events,
            event_backlog: AtomicUsize::new(0),
            event_rx: StdMutex::new(Some(events_rx)),
            stderr,
            next_request_id: AtomicU64::new(1),
            next_permission_id: AtomicU64::new(1),
            request_timeout: spec.request_timeout,
            permission_timeout: spec.permission_timeout,
            state: StdMutex::new(State::default()),
        });
        tokio::spawn(read_loop(shared.clone(), owned.stdout));
        Ok(Self {
            owner: Arc::new(Owner { shared }),
        })
    }

    fn shared(&self) -> &Arc<Shared> {
        &self.owner.shared
    }

    /// Takes the event stream. Returns `None` after the first call.
    pub fn take_events(&self) -> Option<EventStream> {
        let rx = process::lock(&self.shared().event_rx).take()?;
        Some(EventStream {
            shared: self.shared().clone(),
            rx,
        })
    }

    /// Negotiates the protocol version and returns the advertised capabilities.
    ///
    /// The client advertises no client capabilities. It implements no
    /// filesystem or terminal methods, so it must not claim them. A version
    /// other than 1 closes the connection and kills the child.
    pub async fn initialize(&self) -> Result<AgentCapabilities, AcpError> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {},
            "clientInfo": {
                "name": "SlashIt",
                "title": "SlashIt",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let result = self.request("initialize", params).await?;
        let (version, capabilities) =
            types::parse_initialize_result(&result).map_err(AcpError::Malformed)?;
        if version != PROTOCOL_VERSION {
            let error = AcpError::UnsupportedProtocolVersion {
                agent: version.to_string(),
            };
            fail(self.shared(), error.clone(), true);
            return Err(error);
        }
        process::lock(&self.shared().state).capabilities = Some(capabilities.clone());
        Ok(capabilities)
    }

    /// Creates a session. The working directory must be absolute, and the
    /// agent's session identifier is stored without interpretation.
    pub async fn new_session(&self, cwd: &Path) -> Result<SessionRef, AcpError> {
        self.require_initialized()?;
        let cwd_text = absolute_cwd(cwd)?;
        let result = self
            .request("session/new", json!({"cwd": cwd_text, "mcpServers": []}))
            .await?;
        let id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(AcpError::InvalidSessionId)?;
        Ok(SessionRef {
            id: id.to_string(),
            cwd: cwd.to_path_buf(),
        })
    }

    /// Runs one prompt turn with a single text block and waits for its stop reason.
    ///
    /// On timeout the turn is cancelled with `session/cancel`, and the
    /// timeout error is returned.
    pub async fn prompt(
        &self,
        session: &SessionRef,
        text: &str,
        timeout: Duration,
    ) -> Result<TurnOutcome, AcpError> {
        self.require_initialized()?;
        let params = json!({
            "sessionId": session.id(),
            "prompt": [{"type": "text", "text": text}],
        });
        match self
            .request_with_timeout("session/prompt", params, timeout)
            .await
        {
            Ok(result) => {
                let reason = result
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AcpError::Malformed("session/prompt has no stopReason".into())
                    })?;
                let stop_reason = types::parse_stop_reason(reason).ok_or_else(|| {
                    AcpError::UnexpectedStopReason(
                        frame::excerpt(reason).chars().take(64).collect(),
                    )
                })?;
                Ok(TurnOutcome { stop_reason })
            }
            Err(error @ AcpError::Timeout { .. }) => {
                let _ = self.cancel(session).await;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    /// Sends `session/cancel`, a notification. The agent then answers the
    /// pending prompt with `stopReason: cancelled`. Every pending permission
    /// request for the session is answered `cancelled` now, as the spec requires.
    pub async fn cancel(&self, session: &SessionRef) -> Result<(), AcpError> {
        self.send_frame(
            frame::notification("session/cancel", json!({"sessionId": session.id()})),
            self.shared().request_timeout,
        )
        .await?;
        let shared = self.shared().clone();
        let resolved = take_permissions(&shared, |pending| pending.session_id == session.id());
        for (id, pending) in resolved {
            answer_permission_frame(&shared, &pending.request_id, None)?;
            emit(
                &shared,
                AgentEvent::PermissionResolved {
                    id: PermissionId(id),
                    outcome: PermissionOutcome::Cancelled,
                    reason: ResolutionReason::SessionCancelled,
                },
            );
        }
        Ok(())
    }

    /// Answers a permission request. The decision must name an option the
    /// agent offered, with the matching kind. Anything else is cancelled and
    /// returns `InvalidPermissionDecision`.
    pub fn answer_permission(
        &self,
        id: PermissionId,
        decision: PermissionDecision,
    ) -> Result<(), AcpError> {
        let shared = self.shared().clone();
        let pending = take_permission(&shared, id.0).ok_or(AcpError::PermissionNotPending)?;

        let selected = match &decision {
            PermissionDecision::Reject { option_id } => {
                select_option(&pending.options, option_id, |kind| !kind.is_allow())
            }
            PermissionDecision::Cancel => None,
        };
        let refused = selected.is_none() && decision != PermissionDecision::Cancel;
        answer_permission_frame(&shared, &pending.request_id, selected.clone())?;
        emit(
            &shared,
            AgentEvent::PermissionResolved {
                id,
                outcome: selected
                    .clone()
                    .map_or(PermissionOutcome::Cancelled, |option_id| {
                        PermissionOutcome::Selected { option_id }
                    }),
                reason: ResolutionReason::Decision,
            },
        );
        if refused {
            return Err(AcpError::InvalidPermissionDecision);
        }
        Ok(())
    }

    /// Asks the supervisor to kill the child's process group now, without the
    /// protocol shutdown. Clones of a connection keep it alive, so dropping the
    /// last one does not reach this on its own.
    pub fn force_kill(&self) {
        let _ = self.shared().control.send(Control::Kill);
    }

    /// Closes stdin, waits up to `grace` for the agent to exit, kills it if
    /// it has not, and reaps it. Returns the exit status.
    pub async fn shutdown(&self, grace: Duration) -> Result<ExitStatus, AcpError> {
        let shared = self.shared().clone();
        {
            let mut state = process::lock(&shared.state);
            if state.failure.is_none() {
                state.failure = Some(AcpError::Closed("the connection was shut down".into()));
            }
        }
        let closed = AcpError::Closed("the connection was shut down".into());
        fail_pending(&shared, &closed);
        let _ = shared.outbound.try_send(None);

        let mut exit = shared.exit.clone();
        if tokio::time::timeout(grace, wait_for_exit(&mut exit))
            .await
            .is_err()
        {
            let _ = shared.control.send(Control::Kill);
        }
        match tokio::time::timeout(REAP_TIMEOUT, wait_for_exit(&mut exit)).await {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(reason)) => Err(AcpError::Closed(reason)),
            Err(_) => Err(AcpError::ReapTimeout),
        }
    }

    fn require_initialized(&self) -> Result<AgentCapabilities, AcpError> {
        process::lock(&self.shared().state)
            .capabilities
            .clone()
            .ok_or(AcpError::NotInitialized)
    }
    async fn request(&self, method: &'static str, params: Value) -> Result<Value, AcpError> {
        let timeout = self.shared().request_timeout;
        self.request_with_timeout(method, params, timeout).await
    }

    async fn request_with_timeout(
        &self,
        method: &'static str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, AcpError> {
        let shared = self.shared();
        let id = shared.next_request_id.fetch_add(1, Ordering::SeqCst);
        let (reply, response) = oneshot::channel();
        {
            let mut state = process::lock(&shared.state);
            if let Some(failure) = &state.failure {
                return Err(failure.clone());
            }
            state.pending.insert(id, PendingRequest { method, reply });
        }
        let message = frame::request(id, method, params);
        let outcome = tokio::time::timeout(timeout, async {
            self.send_frame(message, timeout).await?;
            response
                .await
                .map_err(|_| AcpError::Closed("the reply channel was dropped".into()))?
        })
        .await;
        match outcome {
            Ok(result) => result,
            Err(_) => {
                process::lock(&shared.state).pending.remove(&id);
                Err(AcpError::Timeout { method })
            }
        }
    }

    async fn send_frame(&self, message: Value, timeout: Duration) -> Result<(), AcpError> {
        let bytes = frame::encode(&message).map_err(frame_error)?;
        match tokio::time::timeout(timeout, self.shared().outbound.send(Some(bytes))).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(AcpError::Closed("the writer has stopped".into())),
            Err(_) => Err(AcpError::Closed(
                "the writer did not accept the frame in time".into(),
            )),
        }
    }
}

fn absolute_cwd(cwd: &Path) -> Result<&str, AcpError> {
    if !cwd.is_absolute() {
        return Err(AcpError::InvalidWorkingDirectory(
            "the session working directory must be absolute".into(),
        ));
    }
    cwd.to_str().ok_or_else(|| {
        AcpError::InvalidWorkingDirectory("the session working directory is not UTF-8".into())
    })
}

fn frame_error(error: FrameError) -> AcpError {
    match error {
        FrameError::TooLarge => AcpError::FrameTooLarge,
        FrameError::Malformed { reason, excerpt } => {
            AcpError::Malformed(format!("{reason}: {excerpt}"))
        }
    }
}

fn select_option(
    options: &[PermissionOption],
    option_id: &str,
    kind_matches: impl Fn(PermissionKind) -> bool,
) -> Option<String> {
    options
        .iter()
        .find(|option| option.option_id == option_id && kind_matches(option.kind))
        .map(|option| option.option_id.clone())
}

/// Removes and returns the pending permissions that match `keep`, in key order.
fn take_permissions(
    shared: &Arc<Shared>,
    keep: impl Fn(&PendingPermission) -> bool,
) -> Vec<(u64, PendingPermission)> {
    let mut state = process::lock(&shared.state);
    let mut keys: Vec<u64> = state
        .permissions
        .iter()
        .filter(|(_, pending)| keep(pending))
        .map(|(key, _)| *key)
        .collect();
    keys.sort_unstable();
    keys.into_iter()
        .filter_map(|key| state.permissions.remove(&key).map(|pending| (key, pending)))
        .collect()
}

/// Removes one pending permission. Returns `None` if it was already answered.
fn take_permission(shared: &Arc<Shared>, key: u64) -> Option<PendingPermission> {
    process::lock(&shared.state).permissions.remove(&key)
}

/// Sends the outcome for one permission request. `selected` is `None` for
/// `cancelled`. The frame is queued without waiting, so a full writer queue
/// fails the connection rather than blocking the reader.
fn answer_permission_frame(
    shared: &Arc<Shared>,
    request_id: &Value,
    selected: Option<String>,
) -> Result<(), AcpError> {
    let outcome = match selected {
        Some(option_id) => json!({"outcome": "selected", "optionId": option_id}),
        None => json!({"outcome": "cancelled"}),
    };
    let message = frame::response(request_id, json!({"outcome": outcome}));
    let bytes = frame::encode(&message).map_err(frame_error)?;
    queue_or_fail(shared, bytes)
}

fn queue_or_fail(shared: &Arc<Shared>, bytes: Vec<u8>) -> Result<(), AcpError> {
    match shared.outbound.try_send(Some(bytes)) {
        Ok(()) => Ok(()),
        Err(error) => {
            let reason = match error {
                mpsc::error::TrySendError::Full(_) => "the outbound queue is full",
                mpsc::error::TrySendError::Closed(_) => "the writer has stopped",
            };
            let failure = AcpError::Closed(reason.into());
            fail(shared, failure.clone(), true);
            Err(failure)
        }
    }
}

/// Queues an event for the consumer. The count goes up before the send, so the
/// consumer's decrement can never run first, and it comes back down when the
/// receiver is gone, so an undelivered event does not count toward the limit.
fn emit(shared: &Arc<Shared>, event: AgentEvent) {
    shared.event_backlog.fetch_add(1, Ordering::SeqCst);
    if shared.events.send(event).is_err() {
        shared.event_backlog.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Marks the connection failed, answers every waiter with the error, and
/// optionally kills the child. Only the first failure is kept.
fn fail(shared: &Arc<Shared>, error: AcpError, kill: bool) {
    let reason = {
        let mut state = process::lock(&shared.state);
        if state.failure.is_none() {
            state.failure = Some(error.clone());
        }
        state.permissions.clear();
        if std::mem::replace(&mut state.closed_sent, true) {
            return;
        }
        // The first failure is the reason, even when a later one is reported.
        state.failure.as_ref().map_or_else(|| error.to_string(), ToString::to_string)
    };
    fail_pending(shared, &error);
    if kill {
        let _ = shared.control.send(Control::Kill);
    }
    let _ = shared.events.send(AgentEvent::Closed { reason });
}

fn fail_pending(shared: &Arc<Shared>, error: &AcpError) {
    let pending: Vec<PendingRequest> = {
        let mut state = process::lock(&shared.state);
        state.pending.drain().map(|(_, pending)| pending).collect()
    };
    for request in pending {
        let _ = request.reply.send(Err(error.clone()));
    }
}

async fn wait_for_exit(
    exit: &mut watch::Receiver<process::ExitReport>,
) -> Result<ExitStatus, String> {
    match exit.wait_for(Option::is_some).await {
        Ok(report) => match report.clone() {
            Some(Ok(process::Exit { status, group: process::GroupEnd::Ended })) => Ok(status),
            Some(Ok(process::Exit { status, group: process::GroupEnd::Unconfirmed(reason) })) => {
                Err(format!(
                    "the provider exited ({status:?}), but its process group may still be running: {reason}"
                ))
            }
            Some(Err(reason)) => Err(reason),
            None => Err("the exit status is not available".to_string()),
        },
        Err(_) => Err("the supervisor stopped without a status".to_string()),
    }
}

/// The reader. It owns stdout and never waits on anything but stdout.
async fn read_loop(shared: Arc<Shared>, mut stdout: BufReader<ChildStdout>) {
    let mut line = Vec::new();
    loop {
        line.clear();
        match process::read_line_bounded(&mut stdout, &mut line, frame::MAX_FRAME_BYTES).await {
            Ok(LineRead::Line { truncated: false }) => {
                if !dispatch(&shared, &line) {
                    // The connection failed or was shut down while this frame
                    // was in flight. Report the close here too: the event
                    // stream ends on `Closed`, and nothing else sends it.
                    fail(
                        &shared,
                        AcpError::Closed("the connection was shut down".into()),
                        false,
                    );
                    return;
                }
            }
            Ok(LineRead::Line { truncated: true }) => {
                fail(&shared, AcpError::FrameTooLarge, true);
                return;
            }
            Ok(LineRead::Eof) => {
                let stderr = process::stderr_excerpt(&shared.stderr);
                fail(&shared, AcpError::ProcessExited { stderr }, true);
                return;
            }
            Err(error) => {
                fail(
                    &shared,
                    AcpError::Closed(frame::excerpt(&error.to_string())),
                    true,
                );
                return;
            }
        }
    }
}

/// Routes one frame. Returns false when the connection has failed.
fn dispatch(shared: &Arc<Shared>, bytes: &[u8]) -> bool {
    if process::lock(&shared.state).failure.is_some() {
        return false;
    }
    if shared.event_backlog.load(Ordering::SeqCst) >= MAX_QUEUED_EVENTS {
        fail(
            shared,
            AcpError::Closed("the event backlog is full".into()),
            true,
        );
        return false;
    }
    match frame::parse(bytes) {
        Err(error) => {
            fail(shared, frame_error(error), true);
            false
        }
        Ok(Message::Response { id, result }) => {
            route_response(shared, &id, result);
            true
        }
        Ok(Message::Request { id, method, params }) => {
            handle_agent_request(shared, &id, &method, params)
        }
        Ok(Message::Notification { method, params }) => {
            handle_notification(shared, &method, &params);
            true
        }
    }
}

fn route_response(shared: &Arc<Shared>, id: &Value, result: Result<Value, frame::RpcError>) {
    let Some(key) = id.as_u64() else {
        return;
    };
    let pending = process::lock(&shared.state).pending.remove(&key);
    let Some(pending) = pending else {
        // A response to a request that already timed out. Nothing waits for it.
        return;
    };
    let reply = match result {
        Ok(value) => Ok(value),
        Err(rpc) => Err(AcpError::Rpc {
            method: pending.method,
            code: rpc.code,
            message: rpc.message,
        }),
    };
    let _ = pending.reply.send(reply);
}

fn handle_agent_request(shared: &Arc<Shared>, id: &Value, method: &str, params: Value) -> bool {
    if method != "session/request_permission" {
        // This client advertises no filesystem, terminal, or elicitation
        // capability, so a well-behaved agent does not send these.
        let message =
            frame::error_response(id, METHOD_NOT_FOUND, "method not supported by this client");
        return match frame::encode(&message)
            .map_err(frame_error)
            .and_then(|bytes| queue_or_fail(shared, bytes))
        {
            Ok(()) => {
                emit(
                    shared,
                    AgentEvent::Unhandled {
                        method: frame::excerpt(method).chars().take(64).collect(),
                    },
                );
                true
            }
            Err(_) => false,
        };
    }
    open_permission(shared, id, &params)
}

/// Opens a permission request and starts its timer. A request that cannot be
/// parsed is answered `cancelled` at once.
fn open_permission(shared: &Arc<Shared>, id: &Value, params: &Value) -> bool {
    let Some(request) = types::parse_permission_request(params) else {
        if answer_permission_frame(shared, id, None).is_err() {
            return false;
        }
        emit(shared, AgentEvent::InvalidPermissionRequest);
        return true;
    };
    let key = shared.next_permission_id.fetch_add(1, Ordering::SeqCst);
    process::lock(&shared.state).permissions.insert(
        key,
        PendingPermission {
            session_id: request.session_id.clone(),
            request_id: id.clone(),
            options: request.options.clone(),
        },
    );
    emit(
        shared,
        AgentEvent::PermissionRequested {
            id: PermissionId(key),
            session_id: request.session_id,
            tool_call_id: request.tool_call_id,
            options: request.options,
        },
    );
    let timer_shared = shared.clone();
    let timeout = shared.permission_timeout;
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        expire_permission(&timer_shared, key);
    });
    true
}

/// Answers `cancelled` when no decision arrived before the timeout. Does
/// nothing if the decision already came.
fn expire_permission(shared: &Arc<Shared>, key: u64) {
    let Some(pending) = take_permission(shared, key) else {
        return;
    };
    if answer_permission_frame(shared, &pending.request_id, None).is_err() {
        return;
    }
    emit(
        shared,
        AgentEvent::PermissionResolved {
            id: PermissionId(key),
            outcome: PermissionOutcome::Cancelled,
            reason: ResolutionReason::NoDecision,
        },
    );
}

fn handle_notification(shared: &Arc<Shared>, method: &str, params: &Value) {
    if method == "session/update" {
        if let Some((session_id, update)) = types::parse_session_update(params) {
            emit(shared, AgentEvent::Update { session_id, update });
            return;
        }
    }
    emit(
        shared,
        AgentEvent::Unhandled {
            method: frame::excerpt(method).chars().take(64).collect(),
        },
    );
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    /// A reaped leader whose group could not be confirmed ended is an error,
    /// whatever the leader's own status was.
    #[tokio::test]
    async fn an_unconfirmed_group_is_an_error_even_when_the_leader_exited_cleanly() {
        let (tx, mut rx) = watch::channel::<process::ExitReport>(None);
        tx.send(Some(Ok(process::Exit {
            status: ExitStatus::from_raw(0),
            group: process::GroupEnd::Unconfirmed("killpg: Operation not permitted".into()),
        })))
        .expect("the receiver is held");
        let error = wait_for_exit(&mut rx).await.expect_err("an unconfirmed group fails");
        assert!(error.contains("killpg: Operation not permitted"), "{error}");
    }

    #[tokio::test]
    async fn an_ended_group_reports_the_leader_status() {
        let (tx, mut rx) = watch::channel::<process::ExitReport>(None);
        tx.send(Some(Ok(process::Exit {
            status: ExitStatus::from_raw(0),
            group: process::GroupEnd::Ended,
        })))
        .expect("the receiver is held");
        let status = wait_for_exit(&mut rx).await.expect("an ended group reports its status");
        assert!(status.success());
    }
}
