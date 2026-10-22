//! Tests for the ACP v1 layer. Connection tests run against the fake agent
//! `tests/fixtures/acp_fixture_agent.rs`, a package binary that `cargo test`
//! builds. No test calls a model.
//!
//! Synchronization uses channels and explicit awaits. Timeouts are upper
//! bounds on a wait that is expected to end with an observed event, so they
//! never decide an outcome.

use super::connection::{
    AcpError, AgentEvent, Connection, EventStream, ResolutionReason, SessionRef,
};
use super::frame::{self, FrameError, Message};
use super::process::{read_line_bounded, LineRead, SpawnSpec};
use super::types::{
    PermissionDecision, PermissionOutcome, SessionUpdate, StopReason, ToolCallStatus,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

const LONG: Duration = Duration::from_secs(20);

/// The fixture binary, which cargo builds beside the test binary's directory
/// on every `cargo test` run (see the `[[bin]]` entry in `Cargo.toml`). A
/// missing file is a failure, not a skip.
fn fixture_path() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let debug_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/debug directory");
    let path = debug_dir.join(format!("acp_fixture_agent{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "the fixture agent is missing at {}; it is a binary of this package",
        path.display()
    );
    path
}

fn spec(scenario: &str, request_timeout: Duration, permission_timeout: Duration) -> SpawnSpec {
    SpawnSpec {
        program: fixture_path().to_string_lossy().into_owned(),
        args: vec![scenario.to_string()],
        clear_env: false,
        cwd: std::env::temp_dir(),
        env: Vec::new(),
        request_timeout,
        permission_timeout,
    }
}

fn start(scenario: &str) -> Connection {
    Connection::spawn_leased(&spec(scenario, LONG, LONG), None).expect("spawn fixture")
}

/// Waits for the next event that satisfies `matches`, skipping the rest.
async fn next_matching<F>(events: &mut EventStream, matches: F) -> AgentEvent
where
    F: Fn(&AgentEvent) -> bool,
{
    loop {
        let event = tokio::time::timeout(LONG, events.recv())
            .await
            .expect("an event arrives")
            .expect("the event stream stays open");
        if matches(&event) {
            return event;
        }
    }
}

async fn text_update_containing(events: &mut EventStream, needle: &'static str) -> AgentEvent {
    next_matching(events, |event| match event {
        AgentEvent::Update {
            update: SessionUpdate::AgentMessageChunk { text: Some(text) },
            ..
        } => text.contains(needle),
        _ => false,
    })
    .await
}

async fn initialized(scenario: &str) -> (Connection, SessionRef) {
    let conn = start(scenario);
    conn.initialize().await.expect("initialize");
    let session = conn
        .new_session(&std::env::temp_dir())
        .await
        .expect("session/new");
    (conn, session)
}

fn exit_is_failure(status: ExitStatus) -> bool {
    !status.success()
}

// Frames

#[test]
fn frame_parses_response_notification_and_request() {
    let response = frame::parse(br#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#).unwrap();
    assert!(matches!(response, Message::Response { result: Ok(_), .. }));

    let notification =
        frame::parse(br#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#).unwrap();
    assert!(matches!(notification, Message::Notification { .. }));

    let request = frame::parse(
        br#"{"jsonrpc":"2.0","id":"abc","method":"session/request_permission","params":{}}"#,
    )
    .unwrap();
    assert!(matches!(request, Message::Request { .. }));
}

#[test]
fn frame_rejects_invalid_shapes_with_bounded_reasons() {
    for bad in [
        &b"not json"[..],
        br#"[1,2]"#,
        br#"{"jsonrpc":"1.0","method":"x"}"#,
        br#"{"jsonrpc":"2.0","method":""}"#,
        br#"{"jsonrpc":"2.0","id":true,"method":"x"}"#,
        br#"{"jsonrpc":"2.0","id":1}"#,
        br#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":1,"message":"x"}}"#,
    ] {
        assert!(
            matches!(frame::parse(bad), Err(FrameError::Malformed { .. })),
            "expected malformed: {}",
            String::from_utf8_lossy(bad)
        );
    }
}

#[test]
fn frame_excerpt_redacts_secrets_and_bounds_length() {
    let text = "error token=abc123 and \"api_key\":\"hunter2\" then Bearer xyz987 ok sk-0123456789abcdefXYZ";
    let shown = frame::excerpt(text);
    assert!(!shown.contains("abc123"), "{shown}");
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(!shown.contains("xyz987"), "{shown}");
    assert!(!shown.contains("sk-0123"), "{shown}");
    assert!(shown.contains("[redacted]"), "{shown}");

    let long = "a".repeat(10_000);
    let shown = frame::excerpt(&long);
    assert!(
        shown.chars().count() <= frame::MAX_EXCERPT_CHARS + 3,
        "{}",
        shown.len()
    );
    assert!(!shown.contains('\n'));
}

#[test]
fn frame_encoding_is_one_line_terminated_by_a_newline() {
    let message = frame::request(7, "session/prompt", json!({"text": "line one\nline two"}));
    let bytes = frame::encode(&message).unwrap();
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    assert_eq!(bytes.last(), Some(&b'\n'));
}

#[tokio::test]
async fn bounded_reader_truncates_oversized_lines_and_recovers() {
    let input: &[u8] = b"0123456789\nshort\n";
    let mut reader = tokio::io::BufReader::new(input);
    let mut line = Vec::new();

    let first = read_line_bounded(&mut reader, &mut line, 4).await.unwrap();
    assert!(matches!(first, LineRead::Line { truncated: true }));
    assert_eq!(line, b"0123");

    line.clear();
    let second = read_line_bounded(&mut reader, &mut line, 4).await.unwrap();
    assert!(matches!(second, LineRead::Line { truncated: true }));
    assert_eq!(line, b"shor");

    line.clear();
    assert!(matches!(
        read_line_bounded(&mut reader, &mut line, 4).await.unwrap(),
        LineRead::Eof
    ));
}

// Types

#[test]
fn session_update_discriminators_map_to_typed_variants_and_unknowns() {
    let tool = json!({"sessionId": "s", "update": {
        "sessionUpdate": "tool_call", "toolCallId": "t", "status": "in_progress"}});
    let (_, update) = super::types::parse_session_update(&tool).unwrap();
    assert_eq!(
        update,
        SessionUpdate::ToolCall {
            tool_call_id: "t".into(),
            title: None,
            kind: None,
            raw_input: None,
            status: Some(ToolCallStatus::InProgress)
        }
    );

    let unknown = json!({"sessionId": "s", "update": {"sessionUpdate": "delete_repo"}});
    let (_, update) = super::types::parse_session_update(&unknown).unwrap();
    assert_eq!(
        update,
        SessionUpdate::Unknown {
            kind: "delete_repo".into()
        }
    );
    assert_eq!(
        super::types::parse_stop_reason("refusal"),
        Some(StopReason::Refusal)
    );
}

// Connection: handshake and capability gates

#[tokio::test]
async fn initialize_negotiates_version_one_and_reads_capabilities() {
    let conn = start("ok");
    let caps = conn.initialize().await.expect("initialize");
    assert!(caps.load_session);
    assert!(
        !caps.resume_session,
        "the fixture does not advertise resume"
    );
    assert!(caps.close_session);
    assert_eq!(caps.auth_method_count, 0);
    let status = conn.shutdown(LONG).await.expect("shutdown");
    assert!(status.success(), "clean EOF exit: {status:?}");
}

#[tokio::test]
async fn unsupported_protocol_version_fails_closed_and_kills_the_agent() {
    let conn = start("bad-version");
    let error = conn.initialize().await.unwrap_err();
    assert!(matches!(error, AcpError::UnsupportedProtocolVersion { ref agent } if agent == "2"));
    // The connection refuses every later request.
    let later = conn.new_session(&std::env::temp_dir()).await.unwrap_err();
    assert!(matches!(
        later,
        AcpError::NotInitialized | AcpError::Closed(_)
    ));
    let status = conn.shutdown(LONG).await.expect("reap");
    assert!(exit_is_failure(status), "the agent was killed: {status:?}");
}

#[tokio::test]
async fn session_new_requires_an_absolute_working_directory() {
    let conn = start("ok");
    conn.initialize().await.unwrap();
    let relative = conn
        .new_session(Path::new("relative/dir"))
        .await
        .unwrap_err();
    assert!(matches!(relative, AcpError::InvalidWorkingDirectory(_)));
    let session = conn.new_session(&std::env::temp_dir()).await.unwrap();
    assert_eq!(session.id(), "fixture-session-1");
    conn.shutdown(LONG).await.unwrap();
}

#[tokio::test]
async fn session_new_before_initialize_is_refused() {
    let conn = start("ok");
    let error = conn.new_session(&std::env::temp_dir()).await.unwrap_err();
    assert_eq!(error, AcpError::NotInitialized);
    conn.shutdown(LONG).await.unwrap();
}

// Connection: streaming, stop reasons, and the event stream

#[tokio::test]
async fn streamed_updates_are_typed_and_unknown_events_are_not_actions() {
    let (conn, session) = initialized("ok").await;
    let mut events = conn.take_events().expect("event stream");
    let outcome = conn
        .prompt(&session, "say hello", LONG)
        .await
        .expect("prompt");
    assert_eq!(outcome.stop_reason, StopReason::EndTurn);

    let mut seen = Vec::new();
    while seen.len() < 5 {
        let event = next_matching(&mut events, |event| {
            matches!(event, AgentEvent::Update { .. })
        })
        .await;
        if let AgentEvent::Update { update, .. } = event {
            seen.push(update);
        }
    }
    assert!(
        matches!(seen[0], SessionUpdate::AgentMessageChunk { text: Some(ref t) } if t == "hello")
    );
    assert!(matches!(seen[1], SessionUpdate::AgentThoughtChunk { .. }));
    assert!(matches!(seen[2], SessionUpdate::ToolCall { .. }));
    assert!(matches!(
        seen[3],
        SessionUpdate::ToolCallUpdate {
            status: Some(ToolCallStatus::Completed),
            ..
        }
    ));
    assert!(matches!(seen[4], SessionUpdate::Unknown { ref kind } if kind == "mystery_event"));
    conn.shutdown(LONG).await.unwrap();
}

// Connection: permissions

#[tokio::test]
async fn permission_without_a_decision_is_cancelled_by_timeout() {
    let conn = Connection::spawn_leased(&spec("permission", LONG, Duration::from_millis(100)), None).unwrap();
    conn.initialize().await.unwrap();
    let session = conn.new_session(&std::env::temp_dir()).await.unwrap();
    let mut events = conn.take_events().unwrap();

    let turn = {
        let conn = conn.clone();
        let session = session.clone();
        tokio::spawn(async move { conn.prompt(&session, "edit", LONG).await })
    };

    let resolved = next_matching(&mut events, |event| {
        matches!(event, AgentEvent::PermissionResolved { .. })
    })
    .await;
    assert!(matches!(
        resolved,
        AgentEvent::PermissionResolved {
            outcome: PermissionOutcome::Cancelled,
            reason: ResolutionReason::NoDecision,
            ..
        }
    ));
    text_update_containing(&mut events, "perm-outcome=cancelled").await;
    let outcome = turn.await.unwrap().unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Cancelled);
    conn.shutdown(LONG).await.unwrap();
}

#[tokio::test]
async fn permission_decisions_must_name_an_offered_option_of_the_matching_kind() {
    let (conn, session) = {
        let conn = Connection::spawn_leased(&spec("permission", LONG, LONG), None).unwrap();
        conn.initialize().await.unwrap();
        let session = conn.new_session(&std::env::temp_dir()).await.unwrap();
        (conn, session)
    };
    let mut events = conn.take_events().unwrap();
    let turn = {
        let conn = conn.clone();
        let session = session.clone();
        tokio::spawn(async move { conn.prompt(&session, "edit", LONG).await })
    };
    let first = next_matching(&mut events, |event| {
        matches!(event, AgentEvent::PermissionRequested { .. })
    })
    .await;
    let AgentEvent::PermissionRequested { id, .. } = first else {
        unreachable!()
    };

    // An option the agent never offered is refused.
    let refused = conn.answer_permission(
        id,
        PermissionDecision::Reject {
            option_id: "made-up".into(),
        },
    );
    assert_eq!(refused, Err(AcpError::InvalidPermissionDecision));
    text_update_containing(&mut events, "perm-outcome=cancelled").await;
    assert_eq!(
        turn.await.unwrap().unwrap().stop_reason,
        StopReason::Cancelled
    );

    // The request is consumed. Answering it again is refused.
    assert_eq!(
        conn.answer_permission(id, PermissionDecision::Cancel),
        Err(AcpError::PermissionNotPending)
    );
    conn.shutdown(LONG).await.unwrap();
}

#[tokio::test]
async fn an_explicit_reject_selects_only_the_reject_option() {
    let conn = Connection::spawn_leased(&spec("permission", LONG, LONG), None).unwrap();
    conn.initialize().await.unwrap();
    let session = conn.new_session(&std::env::temp_dir()).await.unwrap();
    let mut events = conn.take_events().unwrap();
    let turn = {
        let conn = conn.clone();
        let session = session.clone();
        tokio::spawn(async move { conn.prompt(&session, "edit", LONG).await })
    };
    let AgentEvent::PermissionRequested { id, .. } = next_matching(&mut events, |event| {
        matches!(event, AgentEvent::PermissionRequested { .. })
    })
    .await
    else {
        unreachable!()
    };
    conn.answer_permission(
        id,
        PermissionDecision::Reject {
            option_id: "reject-once".into(),
        },
    )
    .expect("an offered reject option is accepted");
    text_update_containing(&mut events, "perm-outcome=selected:reject-once").await;
    assert_eq!(
        turn.await.unwrap().unwrap().stop_reason,
        StopReason::EndTurn
    );
    conn.shutdown(LONG).await.unwrap();
}

// Connection: cancellation

#[tokio::test]
async fn cancel_answers_pending_permission_and_ends_the_turn_as_cancelled() {
    let conn = Connection::spawn_leased(&spec("permission", LONG, LONG), None).unwrap();
    conn.initialize().await.unwrap();
    let session = conn.new_session(&std::env::temp_dir()).await.unwrap();
    let mut events = conn.take_events().unwrap();
    let turn = {
        let conn = conn.clone();
        let session = session.clone();
        tokio::spawn(async move { conn.prompt(&session, "edit", LONG).await })
    };
    next_matching(&mut events, |event| {
        matches!(event, AgentEvent::PermissionRequested { .. })
    })
    .await;

    conn.cancel(&session).await.expect("session/cancel");
    let resolved = next_matching(&mut events, |event| {
        matches!(event, AgentEvent::PermissionResolved { .. })
    })
    .await;
    assert!(matches!(
        resolved,
        AgentEvent::PermissionResolved {
            outcome: PermissionOutcome::Cancelled,
            reason: ResolutionReason::SessionCancelled,
            ..
        }
    ));
    text_update_containing(&mut events, "perm-outcome=cancelled").await;
    assert_eq!(
        turn.await.unwrap().unwrap().stop_reason,
        StopReason::Cancelled
    );
    conn.shutdown(LONG).await.unwrap();
}

// Connection: concurrency. A pending request must not block other traffic.

#[tokio::test]
async fn a_cancel_is_written_while_a_request_is_waiting_for_its_reply() {
    let (conn, session) = {
        let conn = start("silent");
        conn.initialize().await.unwrap();
        (conn, SessionRef::for_tests("unused", &std::env::temp_dir()))
    };
    let mut events = conn.take_events().unwrap();

    // The fixture never answers session/new, so this stays pending.
    let pending = {
        let conn = conn.clone();
        tokio::spawn(async move { conn.new_session(&std::env::temp_dir()).await })
    };

    conn.cancel(&session)
        .await
        .expect("the writer accepts the cancel");
    text_update_containing(&mut events, "cancel-seen").await;
    assert!(
        !pending.is_finished(),
        "session/new still waits for its reply"
    );

    // Shutdown releases the waiting request with an error, whatever the agent does next.
    conn.shutdown(Duration::from_millis(50))
        .await
        .expect("reap");
    assert!(pending.await.unwrap().is_err());
}

#[tokio::test]
async fn a_request_times_out_when_the_agent_is_silent() {
    let conn = Connection::spawn_leased(&spec("silent", Duration::from_millis(150), LONG), None).unwrap();
    conn.initialize().await.unwrap();
    let error = conn.new_session(&std::env::temp_dir()).await.unwrap_err();
    assert_eq!(
        error,
        AcpError::Timeout {
            method: "session/new"
        }
    );
    conn.shutdown(Duration::from_millis(50)).await.unwrap();
}

#[tokio::test]
async fn a_kill_releases_a_write_that_the_agent_never_reads() {
    let conn = start("stop-reading");
    conn.initialize().await.unwrap();
    let session = SessionRef::for_tests("unused", &std::env::temp_dir());
    // Far larger than the pipe buffer, so the writer blocks until the agent dies.
    let big = "x".repeat(2 * 1024 * 1024);
    let turn = {
        let conn = conn.clone();
        tokio::spawn(async move { conn.prompt(&session, &big, Duration::from_secs(60)).await })
    };
    let status = conn
        .shutdown(Duration::from_millis(100))
        .await
        .expect("kill and reap");
    assert!(exit_is_failure(status));
    assert!(
        turn.await.unwrap().is_err(),
        "the blocked prompt fails once the child is gone"
    );
}

#[tokio::test]
async fn an_unexpected_exit_fails_the_pending_turn_and_reports_the_status() {
    let (conn, session) = initialized("exit-on-prompt").await;
    let error = conn.prompt(&session, "boom", LONG).await.unwrap_err();
    assert!(matches!(error, AcpError::ProcessExited { .. }), "{error:?}");
    let status = conn.shutdown(LONG).await.unwrap();
    assert_eq!(status.code(), Some(3));
}

#[tokio::test]
async fn malformed_stdout_fails_the_connection_and_kills_the_agent() {
    let conn = start("bad-json");
    conn.initialize().await.unwrap();
    let error = conn.new_session(&std::env::temp_dir()).await.unwrap_err();
    assert!(matches!(error, AcpError::Malformed(_)), "{error:?}");
    let status = conn.shutdown(LONG).await.unwrap();
    assert!(exit_is_failure(status));
}

// Connection: shutdown and reaping

#[tokio::test]
async fn shutdown_kills_an_agent_that_ignores_end_of_input() {
    let conn = start("ignore-eof");
    conn.initialize().await.unwrap();
    let status = conn.shutdown(Duration::from_millis(100)).await.unwrap();
    assert!(exit_is_failure(status), "{status:?}");
}

#[tokio::test]
async fn dropping_the_last_handle_kills_the_agent() {
    let conn = start("ignore-eof");
    conn.initialize().await.unwrap();
    let mut events = conn.take_events().unwrap();
    // The event stream does not keep the child alive. Dropping the last
    // handle kills the agent. The reader then sees end of output and reports it.
    drop(conn);
    let closed = next_matching(&mut events, |event| {
        matches!(event, AgentEvent::Closed { .. })
    })
    .await;
    assert!(matches!(closed, AgentEvent::Closed { .. }));
}

/// Reproduces the suspected defect in the legacy client (`acp/client.rs`).
///
/// Its stdout reader locks the child for the whole life of the process, and
/// `initialize` must lock the same child to write its request, so the request
/// is never sent and `initialize` waits. The legacy reader also performs
/// blocking reads on a runtime worker, which is stuck for good. Dropping that
/// runtime would join the stuck worker, so the runtime is forgotten after the
/// check. Ignored by default: it leaves one blocked thread and one child
/// process, both of which end when the test binary exits.
#[test]
#[ignore = "reproduces the legacy stdout-lock defect; run with --ignored"]
fn legacy_client_initialize_is_blocked_by_the_child_lock() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let fixture = fixture_path().to_string_lossy().into_owned();
    let blocked = runtime.block_on(async move {
        let client = crate::acp::AcpClient::start(&fixture, &["ok"], &[], &std::env::temp_dir())
            .expect("start the legacy client against the fixture");
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            client.initialize("SlashIt".to_string(), "0.1.0".to_string()),
        )
        .await;
        result.is_err()
    });
    std::mem::forget(runtime);
    assert!(
        blocked,
        "the legacy initialize returned instead of blocking"
    );
}

/// Whether a process still runs. A zombie has exited, and so is not running.
fn process_is_running(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.trim_start().chars().next())
            .is_some_and(|state| state != 'Z' && state != 'X'),
        Err(_) => false,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_connection_ends_the_processes_its_agent_started() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("grandchild.pid");
    // The grandchild is in the agent's process group. The agent then writes a
    // frame that is not JSON, which fails the connection and ends the group.
    let script = format!("sleep 300 & echo $! > {pid_file:?}; echo not-json; wait");
    let spec = SpawnSpec {
        program: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), script],
        cwd: std::env::temp_dir(),
        env: Vec::new(),
        clear_env: false,
        request_timeout: LONG,
        permission_timeout: LONG,
    };
    let conn = Connection::spawn_leased(&spec, None).expect("spawn");
    assert!(conn.initialize().await.is_err(), "a malformed frame must fail");
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("the agent recorded its grandchild")
        .trim()
        .parse()
        .expect("a pid");
    for _ in 0..400 {
        if !process_is_running(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the grandchild {pid} outlived its failed connection");
}
