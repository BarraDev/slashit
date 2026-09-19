//! End-to-end tests for the control channel, with no GUI anywhere.
//!
//! Every test binds a real listener inside a tempdir and talks to it over a
//! real socket. That matters more than it might look: the properties under test
//! — a size cap that fires before the parser, a version mismatch that is named
//! rather than reported as a JSON error, and above all the rule that a peer the
//! operating system has not identified may not spawn an agent — are properties
//! of the assembled server, not of any one function in it.
//!
//! See `docs/architecture/ipc-security.md` for why the authorization test is
//! the important one: `CreateTask` plus `MoveTask(in_progress)` reaches the
//! queue executor, which runs an agent with full tool access.
//!
//! Unix-only for now: the harness hardcodes `Endpoint::Unix`, which
//! `transport::connect`/`bind_endpoints` reject outright on Windows. Gating
//! the whole file keeps that a compile-time fact instead of 16 tests panicking
//! the first time anyone runs `cargo test` on Windows. CI already only runs
//! `cargo check`, not `cargo test`, on non-Linux platforms, so this changes
//! nothing observable today — it only stops a latent trap. Real Windows
//! named-pipe integration coverage remains a tracked gap, not something this
//! gate closes.
#![cfg(unix)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

use slashit_ipc::auth::{AuthToken, Credentials};
use slashit_ipc::client::{send, ClientOptions};
use slashit_ipc::framing::{read_frame, Frame};
use slashit_ipc::transport::{self, BindOptions};
use slashit_ipc::{
    AppStatus, Endpoint, InstanceInfo, IpcEnvelope, IpcRequest, IpcResponse, MAX_REQUEST_BYTES,
    PROTOCOL_VERSION,
};
use slashit_ui_lib::config::paths::AppPaths;
use slashit_ui_lib::ipc::IpcServer;
use slashit_ui_lib::test_helpers::ipc_test_context;

/// A running server plus everything a test needs to reach it.
///
/// The tempdir is held for the lifetime of the harness because it contains both
/// the socket and the credentials file; dropping it early would pull the
/// listener's directory out from under it.
struct Harness {
    _tmp: TempDir,
    local: Endpoint,
    tcp: Endpoint,
    token: AuthToken,
}

impl Harness {
    /// Bind a Unix socket and a loopback TCP listener, and start serving.
    async fn start() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let paths = Arc::new(AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));

        let ctx = Arc::new(ipc_test_context(paths.clone()));

        let requested = vec![
            Endpoint::Unix {
                path: tmp.path().join("slashit.sock"),
            },
            // Port 0: the OS picks, and `endpoints()` reports what it picked.
            Endpoint::Tcp {
                addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            },
        ];

        let server = IpcServer::bind_endpoints(ctx, &requested, &BindOptions::default())
            .await
            .expect("both endpoints should bind inside a tempdir");

        let bound = server.endpoints();
        let local = bound
            .iter()
            .find(|e| matches!(e, Endpoint::Unix { .. }))
            .expect("the local endpoint must be bound")
            .clone();
        let tcp = bound
            .iter()
            .find(|e| matches!(e, Endpoint::Tcp { .. }))
            .expect("the TCP endpoint must be bound")
            .clone();

        // `ensure_token` is idempotent, so this is the same token the server
        // minted while binding rather than a second one.
        let token = Credentials::ensure_token(&paths.credentials_file()).expect("token");

        tokio::spawn(server.serve());

        Self {
            _tmp: tmp,
            local,
            tcp,
            token,
        }
    }

    fn options(&self, endpoint: &Endpoint, token: Option<&AuthToken>) -> ClientOptions {
        ClientOptions {
            endpoint: endpoint.clone(),
            token: token.map(|t| t.expose().to_string()),
            wait: Duration::ZERO,
        }
    }
}

/// Send bytes that no well-behaved client would produce, and read the answer.
///
/// The typed client cannot express a malformed request, an oversized one, or a
/// pre-envelope one — which is precisely why those paths need a raw sender to
/// be tested at all.
async fn raw_roundtrip(endpoint: &Endpoint, payload: Vec<u8>) -> IpcResponse {
    let stream = transport::connect(endpoint).await.expect("connect");
    let (reader, mut writer) = stream.into_halves();

    // Written from a separate task so an oversized payload cannot deadlock:
    // the server stops reading at the cap and answers, which would block a
    // sequential write on a full socket buffer.
    let writing = tokio::spawn(async move {
        writer.write_all(&payload).await?;
        writer.shutdown().await
    });

    let frame = read_frame(reader, MAX_REQUEST_BYTES)
        .await
        .expect("the server should answer");

    // A refused oversized request may close before the last bytes are accepted,
    // so a write error here is expected rather than a failure.
    let _ = writing.await;

    match frame {
        Frame::Line(line) => serde_json::from_str(&line).expect("a well-formed JSON response"),
        other => panic!("expected a response line, got {other:?}"),
    }
}

/// Send one envelope as JSON, bypassing the client's own checks.
async fn envelope_roundtrip(endpoint: &Endpoint, envelope: &serde_json::Value) -> IpcResponse {
    let mut line = serde_json::to_vec(envelope).expect("serialise");
    line.push(b'\n');
    raw_roundtrip(endpoint, line).await
}

fn error_of(response: &IpcResponse) -> String {
    assert!(!response.ok, "expected a refusal, got {response:?}");
    response
        .error
        .clone()
        .expect("a refusal must carry a message")
}

// --- Round trip -------------------------------------------------------------

#[tokio::test]
async fn a_status_request_round_trips_over_a_unix_socket() {
    let h = Harness::start().await;

    let response = send(&IpcRequest::Status, &h.options(&h.local, None))
        .await
        .expect("the local endpoint should answer");

    assert!(response.ok, "{response:?}");
    let status: AppStatus = serde_json::from_value(response.data).expect("an AppStatus");
    assert_eq!(status.active_terminals, 0);
    assert_eq!(status.running_agents, 0);
    assert_eq!(status.queued_tasks, 0);
}

#[tokio::test]
async fn ping_names_the_instance_and_the_endpoint_that_answered() {
    let h = Harness::start().await;

    let response = send(&IpcRequest::Ping, &h.options(&h.local, None))
        .await
        .expect("ping should answer");
    assert!(response.ok, "{response:?}");

    let info: InstanceInfo = serde_json::from_value(response.data).expect("an InstanceInfo");
    assert_eq!(info.protocol_version, PROTOCOL_VERSION);
    assert_eq!(info.pid, std::process::id());
    // With several listeners bound, the useful answer is the one the caller
    // actually reached.
    assert_eq!(info.endpoint, h.local.to_string());
    assert!(!info.version.is_empty());
}

#[tokio::test]
async fn features_are_reported_with_the_layer_that_decided_them() {
    let h = Harness::start().await;

    let response = send(&IpcRequest::Features, &h.options(&h.local, None))
        .await
        .expect("features should answer");
    assert!(response.ok, "{response:?}");

    let flags: Vec<slashit_ipc::FeatureFlagInfo> =
        serde_json::from_value(response.data).expect("a flag list");
    assert!(!flags.is_empty(), "this build defines feature flags");
    for flag in &flags {
        assert!(!flag.name.is_empty());
        assert!(!flag.source.is_empty(), "{} has no source", flag.name);
    }
}

#[tokio::test]
async fn a_windowless_instance_refuses_to_show_rather_than_pretending() {
    let h = Harness::start().await;

    let response = send(&IpcRequest::Show, &h.options(&h.local, None))
        .await
        .expect("show should answer");

    // The test context installs an inert control, which is what a daemon looks
    // like: reporting success would be a lie the caller cannot detect.
    assert!(error_of(&response).contains("no window"));
}

#[tokio::test]
async fn quit_is_acknowledged_before_anything_stops() {
    let h = Harness::start().await;

    let response = send(&IpcRequest::Quit, &h.options(&h.local, None))
        .await
        .expect("quit should be acknowledged");

    assert!(response.ok, "{response:?}");
    assert_eq!(response.data["quit"], true);
}

// --- Framing and protocol ---------------------------------------------------

#[tokio::test]
async fn an_oversized_request_is_rejected_by_size_not_by_the_parser() {
    let h = Harness::start().await;

    // Valid JSON would still be over the cap; the point is that the answer
    // names the limit rather than reporting a syntax error from a truncated
    // prefix.
    let payload = vec![b'x'; (MAX_REQUEST_BYTES + 64) as usize];
    let response = raw_roundtrip(&h.local, payload).await;

    let error = error_of(&response);
    assert!(
        error.contains(&MAX_REQUEST_BYTES.to_string()) && error.contains("limit"),
        "expected a size-limit message, got {error}"
    );
    assert!(
        !error.contains("Invalid request"),
        "an oversized request must not be reported as a parse failure: {error}"
    );
}

#[tokio::test]
async fn malformed_json_is_rejected_with_a_clear_message() {
    let h = Harness::start().await;

    let response = raw_roundtrip(&h.local, b"{ this is not json\n".to_vec()).await;

    let error = error_of(&response);
    assert!(
        error.starts_with("Invalid request"),
        "expected a parse message, got {error}"
    );
}

#[tokio::test]
async fn a_wrong_protocol_version_names_both_versions() {
    let h = Harness::start().await;

    let response = envelope_roundtrip(
        &h.local,
        &serde_json::json!({ "version": 999, "request": { "cmd": "status" } }),
    )
    .await;

    let error = error_of(&response);
    assert!(
        error.contains("999") && error.contains(&PROTOCOL_VERSION.to_string()),
        "the mismatch should name both versions, got {error}"
    );
}

#[tokio::test]
async fn a_bare_legacy_request_gets_a_version_message_not_a_serde_error() {
    let h = Harness::start().await;

    // The pre-envelope wire format. A serde error here would send the user
    // hunting for a JSON bug that is really a skew between two of their own
    // binaries.
    let response = envelope_roundtrip(&h.local, &serde_json::json!({ "cmd": "status" })).await;

    let error = error_of(&response);
    assert!(
        error.contains("protocol"),
        "expected a protocol-version message, got {error}"
    );
    assert!(
        !error.starts_with("Invalid request"),
        "a recognisable legacy request must not be reported as malformed: {error}"
    );
}

// --- Authentication ---------------------------------------------------------

#[tokio::test]
async fn tcp_without_a_token_is_denied() {
    let h = Harness::start().await;

    let response = envelope_roundtrip(
        &h.tcp,
        &serde_json::to_value(IpcEnvelope::new(IpcRequest::Status)).unwrap(),
    )
    .await;

    let error = error_of(&response);
    assert!(
        error.to_lowercase().contains("authentication"),
        "expected an authentication refusal, got {error}"
    );
}

#[tokio::test]
async fn tcp_with_the_wrong_token_is_denied() {
    let h = Harness::start().await;

    let response = envelope_roundtrip(
        &h.tcp,
        &serde_json::to_value(IpcEnvelope::with_auth(
            IpcRequest::Status,
            "0000000000000000000000000000000000000000000000000000000000000000",
        ))
        .unwrap(),
    )
    .await;

    let error = error_of(&response);
    assert!(
        error.to_lowercase().contains("authentication"),
        "expected an authentication refusal, got {error}"
    );
    // The refusal must not reveal whether a token is configured at all.
    assert!(
        !error.contains(h.token.expose()),
        "the refusal leaked the expected token"
    );
}

#[tokio::test]
async fn tcp_with_the_right_token_may_read() {
    let h = Harness::start().await;

    let response = send(&IpcRequest::Status, &h.options(&h.tcp, Some(&h.token)))
        .await
        .expect("an authenticated TCP client should be answered");

    assert!(response.ok, "{response:?}");
    serde_json::from_value::<AppStatus>(response.data).expect("an AppStatus");
}

// --- Authorization ----------------------------------------------------------

/// The verbs that reach the queue executor, which creates a worktree and runs
/// an agent with the task description as its prompt.
fn agent_spawning_requests() -> Vec<IpcRequest> {
    vec![
        IpcRequest::CreateTask {
            project_id: uuid::Uuid::new_v4().to_string(),
            title: "from a remote peer".to_string(),
            description: Some("rm -rf".to_string()),
            priority: None,
        },
        IpcRequest::MoveTask {
            task_id: uuid::Uuid::new_v4().to_string(),
            status: "in_progress".to_string(),
        },
        IpcRequest::EnqueueTask {
            task_id: uuid::Uuid::new_v4().to_string(),
        },
    ]
}

#[tokio::test]
async fn a_valid_token_still_cannot_spawn_an_agent() {
    let h = Harness::start().await;

    for request in agent_spawning_requests() {
        let response = send(&request, &h.options(&h.tcp, Some(&h.token)))
            .await
            .expect("the request should be answered, not dropped");

        let error = error_of(&response);
        assert!(
            error.contains("not available to clients"),
            "`{}` must be refused for a non-local peer, got {error}",
            request.verb()
        );
        // The refusal must be the authorization one, not the handler happening
        // to fail on a made-up id: those look alike from the outside and only
        // one of them is a security property.
        assert!(
            !error.contains("not found"),
            "`{}` reached the handler over TCP: {error}",
            request.verb()
        );
    }
}

#[tokio::test]
async fn the_local_endpoint_may_use_the_agent_spawning_verbs() {
    let h = Harness::start().await;

    for request in agent_spawning_requests() {
        let verb = request.verb();
        let response = send(&request, &h.options(&h.local, None))
            .await
            .expect("the local endpoint should answer");

        // The ids are invented, so every one of these fails — but it fails
        // inside the handler, which is exactly what proves authorization let it
        // through.
        let error = error_of(&response);
        assert!(
            !error.contains("not available to clients"),
            "`{verb}` must not be refused on the local endpoint: {error}"
        );
        assert!(
            !error.to_lowercase().contains("authentication"),
            "the local endpoint must not ask for a credential: {error}"
        );
        assert!(
            error.contains("not found") || error.contains("Invalid"),
            "`{verb}` should have reached its handler, got {error}"
        );
    }
}

#[tokio::test]
async fn a_read_only_verb_is_allowed_from_both_endpoints() {
    let h = Harness::start().await;

    for (name, options) in [
        ("local", h.options(&h.local, None)),
        ("tcp", h.options(&h.tcp, Some(&h.token))),
    ] {
        let response = send(&IpcRequest::ListProjects, &options)
            .await
            .unwrap_or_else(|e| panic!("{name} endpoint should answer: {e}"));
        assert!(response.ok, "{name}: {response:?}");
    }
}

// ---------------------------------------------------------------------------
// Single-instance ownership
//
// The property under test is an *ordering*, not merely "a second bind fails".
// The GUI and `slashitd` are separate processes, so the in-process mutex that
// serialises `config.toml` writes cannot exclude them from each other. What
// makes a process *the* SlashIt instance is owning the OS-authenticated
// control channel — and it has to own it before it hydrates, because
// hydration requeues tasks left `InProgress` (next to a live instance: tasks
// running right now) and writes that back to disk.
//
// These tests are deterministic. Nothing sleeps and nothing races on wall
// clock: a `Barrier` releases both candidates together, and the loser is
// identified by the kernel refusing it the lease, not by timing.
// ---------------------------------------------------------------------------

/// Paths rooted in `tmp`, matching what a real instance resolves at startup.
fn instance_paths(tmp: &TempDir) -> AppPaths {
    AppPaths::with_roots(
        tmp.path().join("config"),
        tmp.path().join("data"),
        tmp.path().join("cache"),
        tmp.path().join("runtime"),
    )
}

fn local_endpoint(tmp: &TempDir) -> Endpoint {
    Endpoint::Unix {
        path: tmp.path().join("runtime").join("slashit.sock"),
    }
}

fn bind_opts() -> BindOptions {
    BindOptions {
        allow_remote: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exactly_one_of_two_concurrent_starts_owns_the_instance() {
    use slashit_ui_lib::ipc::BoundIpc;

    let tmp = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("runtime")).unwrap();
    let paths = Arc::new(instance_paths(&tmp));
    let endpoint = local_endpoint(&tmp);

    // Both candidates are released from the same barrier, so neither can win
    // merely by having started earlier.
    let gate = Arc::new(tokio::sync::Barrier::new(2));

    let mut candidates = Vec::new();
    for _ in 0..2 {
        let paths = paths.clone();
        let endpoint = endpoint.clone();
        let gate = gate.clone();
        candidates.push(tokio::spawn(async move {
            gate.wait().await;
            BoundIpc::bind_endpoints(&paths, std::slice::from_ref(&endpoint), &bind_opts()).await
        }));
    }

    let mut winners = Vec::new();
    let mut losers = Vec::new();
    for c in candidates {
        match c.await.expect("candidate panicked") {
            Ok(bound) => winners.push(bound),
            Err(e) => losers.push(e),
        }
    }

    assert_eq!(
        winners.len(),
        1,
        "exactly one instance may own the control channel, got {} winners and {} losers",
        winners.len(),
        losers.len()
    );
    assert_eq!(losers.len(), 1, "the other candidate must be refused");

    let refusal = format!("{:#}", losers[0]);
    assert!(
        refusal.contains("cannot bind the local control channel"),
        "the loser must be told it lost the local control channel, got: {refusal}"
    );

    // The winner is genuinely usable, not merely holding a lock.
    assert_eq!(
        winners[0].endpoints().len(),
        1,
        "the winner must hold the endpoint it claimed"
    );
}

#[tokio::test]
async fn the_loser_of_the_race_never_touches_shared_state() {
    use slashit_ui_lib::ipc::BoundIpc;

    // The regression this guards is the original startup ordering, where both
    // the GUI and the daemon ran `build_state()` — hydrating, requeueing
    // in-flight tasks and persisting the result — and only afterwards tried to
    // bind. `BoundIpc::bind_endpoints` takes `&AppPaths` and nothing else
    // precisely so that losing is decided while there is still nothing to
    // corrupt. Proven here by taking a full snapshot of the config and data
    // trees and requiring it to be untouched.
    let tmp = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("runtime")).unwrap();
    let paths = Arc::new(instance_paths(&tmp));
    let endpoint = local_endpoint(&tmp);

    let _winner = BoundIpc::bind_endpoints(&paths, std::slice::from_ref(&endpoint), &bind_opts())
        .await
        .expect("the first instance must win");

    fn snapshot(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    out.push((path, bytes));
                }
            }
        }
        out.sort();
        out
    }

    let before_config = snapshot(&tmp.path().join("config"));
    let before_data = snapshot(&tmp.path().join("data"));

    let refused = BoundIpc::bind_endpoints(&paths, std::slice::from_ref(&endpoint), &bind_opts()).await;
    assert!(refused.is_err(), "the second instance must be refused");

    assert_eq!(
        before_config,
        snapshot(&tmp.path().join("config")),
        "a refused instance must not have written to the config tree"
    );
    assert_eq!(
        before_data,
        snapshot(&tmp.path().join("data")),
        "a refused instance must not have written to the data tree — no task may be \
         requeued or persisted by a process that does not own the instance"
    );
}

#[tokio::test]
async fn ownership_is_released_on_shutdown_and_can_be_reacquired() {
    use slashit_ui_lib::ipc::BoundIpc;

    let tmp = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("runtime")).unwrap();
    let paths = Arc::new(instance_paths(&tmp));
    let endpoint = local_endpoint(&tmp);

    let first = BoundIpc::bind_endpoints(&paths, std::slice::from_ref(&endpoint), &bind_opts())
        .await
        .expect("the first instance must win");

    assert!(
        BoundIpc::bind_endpoints(&paths, std::slice::from_ref(&endpoint), &bind_opts())
            .await
            .is_err(),
        "ownership must be exclusive while the owner is alive"
    );

    // Graceful shutdown: the listeners drop, which closes the socket and
    // releases the kernel-held lease.
    drop(first);

    BoundIpc::bind_endpoints(&paths, std::slice::from_ref(&endpoint), &bind_opts())
        .await
        .expect("a later instance must be able to acquire ownership once released");
}

#[tokio::test]
async fn a_stale_socket_left_by_a_crash_does_not_lock_the_app_out() {
    use slashit_ui_lib::ipc::BoundIpc;

    // A hard kill runs no destructor, so the socket file and the lock file are
    // both still on disk afterwards. Neither may be read as "an instance is
    // running": the kernel closes every descriptor when a process dies, which
    // releases the lease, and that release is the only signal that means
    // anything. A file that merely exists proves nothing.
    //
    // The post-`SIGKILL` state is reproduced on disk directly rather than by
    // leaking a live owner. `std::mem::forget` would leave this very process
    // holding the descriptor — and therefore still holding the lease — which
    // is the opposite of a crash. `std::os::unix::net::UnixListener` does not
    // unlink its path on drop, so binding and dropping one leaves exactly the
    // orphaned socket file a killed instance leaves behind.
    let tmp = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("runtime")).unwrap();
    let paths = Arc::new(instance_paths(&tmp));
    let endpoint = local_endpoint(&tmp);
    let socket_path = tmp.path().join("runtime").join("slashit.sock");
    let lock_path = tmp.path().join("runtime").join("slashit.lock");

    {
        let orphaned = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("bind a socket that will be orphaned");
        drop(orphaned);
        // The lock file outlives the crash too; only the flock on it died.
        std::fs::File::create(&lock_path).expect("leave a lock file behind");
    }

    assert!(
        socket_path.exists(),
        "precondition: a crash leaves the socket file behind"
    );
    assert!(
        lock_path.exists(),
        "precondition: a crash leaves the lock file behind"
    );

    BoundIpc::bind_endpoints(&paths, std::slice::from_ref(&endpoint), &bind_opts())
        .await
        .expect("stale artifacts must not permanently lock the app out");
}
