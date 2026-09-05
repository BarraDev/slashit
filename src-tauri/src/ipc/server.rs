//! Bind, accept, authenticate, authorize, dispatch.
//!
//! Everything about *how* bytes arrive lives in [`slashit_ipc::transport`].
//! What is left here is the decision that must not be re-derived per platform:
//! what a given peer is allowed to ask for. That decision has essentially one
//! input — whether the operating system already established who the peer is —
//! because the command set contains a code-execution primitive. `CreateTask`
//! followed by `MoveTask(in_progress)` reaches the queue executor, which
//! creates a worktree and runs an agent with full tool access over an
//! attacker-controlled prompt. See `docs/architecture/ipc-security.md`.
//!
//! | Peer | Read-only verbs | Other mutating verbs | Agent-spawning verbs |
//! |---|---|---|---|
//! | OS-verified owner (Unix socket, named pipe) | yes | yes | yes |
//! | Bearer-token holder (loopback TCP) | yes | yes | **no** |
//! | Anyone else | no | no | no |
//!
//! An OS-verified peer is granted everything on purpose: reaching a socket
//! inside a 0700 directory, or a named pipe with an owner-only DACL, already
//! required being the user, and that user can run the same commands at a
//! shell. A token holder is *not* the same thing — nothing about a loopback
//! TCP connection identifies the process at the other end — so the verbs that
//! amount to remote code execution are refused there regardless of the token.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWrite;
use tokio::sync::RwLock;
use uuid::Uuid;

use slashit_ipc::auth::{AuthToken, Credentials};
use slashit_ipc::endpoint::Endpoint;
use slashit_ipc::framing::{read_frame, write_json_frame, Frame};
use slashit_ipc::transport::{Accepted, BindOptions, IpcListener, TransportAuth};
use slashit_ipc::{
    IpcConfig, IpcEnvelope, IpcRequest, IpcResponse, MAX_REQUEST_BYTES, PROTOCOL_VERSION,
};

use crate::domain::{AgentExecution, Project, Task};

use super::handlers::{self, Dispatch};

/// How long a peer may take to send its request.
///
/// Without a bound, a client that connects and then stalls holds a task, a file
/// descriptor and — for the local endpoint — a slot in whatever the operator
/// is debugging, indefinitely. The size cap in [`slashit_ipc::framing`] bounds
/// how much a peer may send; this bounds how long it may take not to.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long writing the response may take.
///
/// Shorter than the read budget because by this point the work is done: a peer
/// that has stopped reading is no longer owed anything.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Pause after a failed `accept`, so a permanently broken listener cannot spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Consecutive `accept` failures before a listener is abandoned.
///
/// A transient failure — a peer that vanished between connecting and being
/// accepted, a momentary descriptor shortage — must not kill the listener. A
/// listener that fails every time is broken for good and retrying it forever
/// only hides that.
const MAX_CONSECUTIVE_ACCEPT_FAILURES: u32 = 16;

/// Everything an IPC handler is allowed to touch.
///
/// Deliberately *not* an `AppHandle` and not the whole `AppState`: the same
/// context is built by the desktop app and by the headless daemon, so anything
/// that only exists in one of them is reached through a trait
/// ([`crate::events::EventSink`], [`crate::instance::InstanceControl`]) rather
/// than named directly.
pub struct IpcContext {
    pub tasks: Arc<RwLock<HashMap<Uuid, Task>>>,
    pub projects: Arc<RwLock<HashMap<Uuid, Project>>>,
    pub executions: Arc<RwLock<HashMap<Uuid, AgentExecution>>>,
    pub pty: crate::pty::PtyState,
    pub queue_manager: Arc<RwLock<crate::queue::QueueManager>>,
    pub storage: crate::config::Storage,

    /// Where a handler reports to when it has something to say.
    ///
    /// Held rather than reached for: a handler that needed to notify the UI
    /// would otherwise have to carry a Tauri app handle, which is exactly what
    /// stops this module compiling in the daemon. The current command set is
    /// pure request/response, so nothing emits yet.
    pub events: crate::events::SharedEventSink,

    /// Showing a window and quitting — the two things a daemon cannot do the
    /// way the desktop app does.
    pub control: crate::instance::SharedInstanceControl,

    /// Runtime feature flags, shared with the settings UI so `slashit features`
    /// and the toggle in the app can never disagree.
    pub features: Arc<RwLock<crate::config::features::FeatureFlags>>,

    /// The resolved application directories. The credentials file lives here,
    /// which is where the TCP bearer token is minted and read.
    pub paths: Arc<crate::config::paths::AppPaths>,
}

/// The current process's effective user id.
///
/// Shared by every ownership check that needs it, all of which are
/// `#[cfg(unix)]` tests exercising permission-sensitive paths — hence the same
/// gate here, or the function is dead code on every other build. No `libc`
/// dependency in this crate; declaring the one symbol needed avoids pulling
/// one in just for this.
#[cfg(all(test, unix))]
pub(crate) fn current_uid() -> u32 {
    extern "C" {
        #[link_name = "geteuid"]
        fn libc_geteuid() -> u32;
    }
    // Safety: geteuid takes no arguments and always succeeds.
    unsafe { libc_geteuid() }
}

/// What the transport established about one connection.
///
/// Passed to the handlers rather than re-derived, so `Ping` can report which
/// endpoint answered and the authorization rule has a single source of truth.
pub struct PeerContext {
    /// The endpoint that accepted the connection, for the reply and the audit
    /// record.
    pub endpoint: Endpoint,

    /// The operating system proved this peer is the owning user.
    ///
    /// True for a Unix socket in a 0700 directory and for a Windows named pipe
    /// with an owner-only descriptor; never true for TCP, whatever the token
    /// says.
    pub os_verified: bool,
}

/// Bind every configured endpoint and serve until the process stops.
///
/// The one call both entry points make. Errors only when the instance ends up
/// with no usable control channel at all.
pub async fn serve(ctx: Arc<IpcContext>, config: IpcConfig) -> anyhow::Result<()> {
    IpcServer::bind(ctx, &config).await?.serve().await
}

/// Listeners that are already bound, before anything is accepted on them.
///
/// Binding is separated from serving so a caller — in practice a test — can
/// learn the resolved endpoints first. TCP port 0 means "any free port", and
/// nobody can connect to a listener whose port they cannot discover.
pub struct IpcServer {
    ctx: Arc<IpcContext>,
    listeners: Vec<IpcListener>,
    /// The bearer token, present only when some endpoint needs one. Shared
    /// rather than copied per connection so it exists once in memory.
    token: Option<Arc<AuthToken>>,
}

impl IpcServer {
    /// Bind what the configuration asks for.
    pub async fn bind(ctx: Arc<IpcContext>, config: &IpcConfig) -> anyhow::Result<Self> {
        let options = BindOptions {
            allow_remote: config.tcp.allow_remote,
        };
        Self::bind_endpoints(ctx, &config.listen_endpoints(), &options).await
    }

    /// Bind an explicit list of endpoints.
    ///
    /// Used by tests, which need a socket inside a tempdir rather than the
    /// user's real runtime directory.
    pub async fn bind_endpoints(
        ctx: Arc<IpcContext>,
        endpoints: &[Endpoint],
        options: &BindOptions,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !endpoints.is_empty(),
            "IPC is configured with no listeners at all, so the `slashit` CLI would have \
             nothing to connect to. Set `local_enabled = true` in the IPC configuration."
        );

        // Minted before anything is bound. Nothing about a TCP connection
        // identifies the peer, so such a listener without a credential is an
        // open door; failing to obtain one must remove the listener rather than
        // be discovered by the first client that connects.
        let mut wanted: Vec<&Endpoint> = endpoints.iter().collect();
        let mut token = None;

        if wanted.iter().any(|e| !e.is_os_authenticated()) {
            let path = ctx.paths.credentials_file();
            match Credentials::ensure_token(&path) {
                Ok(minted) => {
                    // The path, never the value: a token printed into a log has
                    // outlived the 0600 file it was written to.
                    println!("[ipc] bearer token for TCP clients is in {}", path.display());
                    token = Some(Arc::new(minted));
                }
                // Failing closed on the listener that needs the credential, and
                // only on that one: the local channel is protected by the
                // filesystem and loses nothing here.
                Err(e) => {
                    eprintln!(
                        "[ipc] not binding any TCP listener: the bearer token at {} could not be \
                         created ({e}). The local control channel is unaffected.",
                        path.display()
                    );
                    wanted.retain(|e| e.is_os_authenticated());
                }
            }
        }

        let mut listeners = Vec::with_capacity(wanted.len());
        for endpoint in wanted {
            match IpcListener::bind(endpoint, options).await {
                Ok(listener) => {
                    println!("[ipc] listening on {}", listener.endpoint());
                    listeners.push(listener);
                }
                // The local endpoint is how the CLI and the app talk to each
                // other; losing it means the instance is uncontrollable, and a
                // failure here usually means another instance already owns it.
                Err(e) if endpoint.is_os_authenticated() => {
                    return Err(anyhow::anyhow!(
                        "cannot bind the local control channel at {endpoint}: {e}"
                    ));
                }
                // An optional listener that could not bind — a port already in
                // use, most likely — must not take the local channel down with
                // it. Loud, because the operator asked for it and is not
                // getting it.
                Err(e) => {
                    eprintln!(
                        "[ipc] could not bind {endpoint}: {e}. The local control channel is \
                         unaffected."
                    );
                }
            }
        }

        anyhow::ensure!(
            !listeners.is_empty(),
            "no IPC endpoint could be bound; this instance cannot be controlled"
        );

        Ok(Self {
            ctx,
            listeners,
            token,
        })
    }

    /// The endpoints actually bound, with any OS-chosen port resolved.
    pub fn endpoints(&self) -> Vec<Endpoint> {
        self.listeners.iter().map(IpcListener::endpoint).collect()
    }

    /// Accept on every listener until they all give up.
    pub async fn serve(self) -> anyhow::Result<()> {
        let Self {
            ctx,
            listeners,
            token,
        } = self;

        let mut accepting = tokio::task::JoinSet::new();
        for listener in listeners {
            accepting.spawn(accept_loop(listener, ctx.clone(), token.clone()));
        }

        // An accept loop only finishes when its listener is broken, so the
        // server stays up as long as any one of them still accepts. Returning
        // on the first failure would let a wedged TCP listener close the local
        // channel.
        while let Some(joined) = accepting.join_next().await {
            match joined {
                Ok(endpoint) => eprintln!("[ipc] stopped listening on {endpoint}"),
                // Shutdown aborts the whole set; that is not a failure.
                Err(e) if e.is_cancelled() => return Ok(()),
                Err(e) => eprintln!("[ipc] an accept loop ended unexpectedly: {e}"),
            }
        }

        Ok(())
    }
}

/// Accept connections on one listener until it fails for good.
///
/// Returns the endpoint it gave up on, so the caller can report which one.
async fn accept_loop(
    mut listener: IpcListener,
    ctx: Arc<IpcContext>,
    token: Option<Arc<AuthToken>>,
) -> Endpoint {
    let endpoint = listener.endpoint();
    let mut consecutive_failures = 0u32;

    loop {
        match listener.accept().await {
            Ok(accepted) => {
                consecutive_failures = 0;
                tokio::spawn(handle_connection(accepted, ctx.clone(), token.clone()));
            }
            Err(e) => {
                consecutive_failures += 1;
                eprintln!("[ipc] accept failed on {endpoint}: {e}");
                if consecutive_failures >= MAX_CONSECUTIVE_ACCEPT_FAILURES {
                    eprintln!(
                        "[ipc] giving up on {endpoint} after {consecutive_failures} consecutive \
                         failures"
                    );
                    return endpoint;
                }
                // Backoff rather than an immediate retry: a listener that fails
                // synchronously would otherwise saturate a core while it did.
                tokio::time::sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

/// One request, one response, close.
async fn handle_connection(
    accepted: Accepted,
    ctx: Arc<IpcContext>,
    token: Option<Arc<AuthToken>>,
) {
    let Accepted {
        stream,
        transport_auth,
        endpoint,
    } = accepted;
    let (reader, mut writer) = stream.into_halves();

    let frame = match tokio::time::timeout(REQUEST_TIMEOUT, read_frame(reader, MAX_REQUEST_BYTES))
        .await
    {
        Ok(Ok(frame)) => frame,
        Ok(Err(e)) => {
            eprintln!("[ipc] read failed on {endpoint}: {e}");
            return;
        }
        Err(_elapsed) => {
            let seconds = REQUEST_TIMEOUT.as_secs();
            eprintln!("[ipc] no request within {seconds}s on {endpoint}; closing");
            respond(
                &mut writer,
                &IpcResponse::error(format!("No request received within {seconds} seconds")),
            )
            .await;
            return;
        }
    };

    let line = match frame {
        Frame::Line(line) => line,
        // A probe, or a client that changed its mind. Answering would be
        // writing into a socket nobody is reading.
        Frame::Empty => return,
        Frame::TooLarge => {
            respond(
                &mut writer,
                &IpcResponse::error(format!("Request exceeds the {MAX_REQUEST_BYTES} byte limit")),
            )
            .await;
            return;
        }
    };

    let peer = PeerContext {
        endpoint,
        os_verified: transport_auth == TransportAuth::OsVerifiedOwner,
    };

    let dispatch = process(&line, &peer, token.as_deref(), &ctx).await;
    respond(&mut writer, &dispatch.response).await;

    // Strictly after the response has been written and flushed: requesting a
    // quit can take the process down, and a client that asked politely should
    // see its acknowledgement rather than a closed connection.
    if dispatch.then_quit {
        ctx.control.request_quit();
    }
}

/// Decide what one line of input deserves.
///
/// The order is not arbitrary. Decoding comes first because nothing else can
/// be judged without it; the version check comes before authentication so a
/// skewed client is told about the skew rather than about its credential; and
/// authentication comes before authorization so an unauthenticated peer never
/// learns which verbs exist.
async fn process(
    line: &str,
    peer: &PeerContext,
    token: Option<&AuthToken>,
    ctx: &IpcContext,
) -> Dispatch {
    let envelope: IpcEnvelope = match serde_json::from_str(line) {
        Ok(envelope) => envelope,
        Err(e) => {
            // A pre-envelope client sends a bare request. Reporting the serde
            // error would send the user hunting for a JSON bug that is really
            // a version skew between two of their own binaries.
            if serde_json::from_str::<IpcRequest>(line).is_ok() {
                return Dispatch::reply(IpcResponse::error(format!(
                    "This client speaks the pre-envelope protocol. This instance requires \
                     protocol version {PROTOCOL_VERSION}; update the `slashit` CLI to match \
                     the app."
                )));
            }
            return Dispatch::reply(IpcResponse::error(format!("Invalid request: {e}")));
        }
    };

    if envelope.version != PROTOCOL_VERSION {
        return Dispatch::reply(IpcResponse::error(format!(
            "Protocol version mismatch: this instance speaks version {PROTOCOL_VERSION}, the \
             client sent version {}. Update whichever of the two is older.",
            envelope.version
        )));
    }

    let request = envelope.request;
    let verb = request.verb();

    if !peer.os_verified {
        // Both messages are constants. Saying "no token is configured" would
        // tell an unauthenticated peer whether guessing is worth its time.
        match (token, envelope.auth.as_deref()) {
            (Some(expected), Some(supplied)) if expected.matches(supplied) => {}
            (_, Some(_)) => {
                audit(verb, peer, "denied-bad-token");
                return Dispatch::reply(IpcResponse::error("Authentication failed."));
            }
            (_, None) => {
                audit(verb, peer, "denied-no-token");
                return Dispatch::reply(IpcResponse::error(format!(
                    "Authentication required on {}: put the bearer token from the credentials \
                     file in the request envelope, or set SLASHIT_IPC_TOKEN.",
                    peer.endpoint
                )));
            }
        }
    }

    // The core security property. A valid token proves the peer knows a secret;
    // it does not prove the peer is the user, and these verbs run code as the
    // user.
    if !peer.os_verified && request.spawns_agent() {
        audit(verb, peer, "denied-not-local");
        return Dispatch::reply(IpcResponse::error(format!(
            "`{verb}` is not available to clients the operating system has not identified. It \
             creates a worktree and runs an agent with full tool access, which makes it a \
             code-execution primitive; only the local endpoint may use it. See \
             docs/architecture/ipc-security.md."
        )));
    }

    if request.is_mutating() {
        audit(verb, peer, "dispatched");
    }

    handlers::dispatch(request, ctx, peer).await
}

/// Record one security-relevant request.
///
/// One line, fixed field order, no payload. The payload is left out on purpose:
/// a task description is user data and often the whole point of the request, so
/// an audit trail that carried it could not be shipped to an operator or a log
/// aggregator. The token is left out for the same reason its `Debug` redacts.
fn audit(verb: &str, peer: &PeerContext, outcome: &str) {
    println!(
        "[ipc-audit] verb={verb} endpoint={} os_verified={} outcome={outcome}",
        peer.endpoint, peer.os_verified
    );
}

/// Write one response frame, or give up.
///
/// Failures are logged and swallowed: the connection is finished either way,
/// and there is nobody left to return an error to. The frame is flushed by
/// [`write_json_frame`], so a successful return means the bytes have left this
/// process — which is what makes it safe to quit afterwards.
async fn respond<W>(writer: &mut W, response: &IpcResponse)
where
    W: AsyncWrite + Unpin,
{
    match tokio::time::timeout(RESPONSE_TIMEOUT, write_json_frame(writer, response)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("[ipc] could not write the response: {e}"),
        Err(_elapsed) => eprintln!(
            "[ipc] gave up writing the response after {}s; the peer stopped reading",
            RESPONSE_TIMEOUT.as_secs()
        ),
    }
}
