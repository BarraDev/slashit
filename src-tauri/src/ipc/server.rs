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
const ACCEPT_BACKOFF: Duration = if cfg!(test) {
    Duration::from_millis(1)
} else {
    Duration::from_millis(100)
};

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

    /// The Project lifecycle gate, shared with `AppState.task.project_gate`:
    /// Task creation from the CLI takes it shared, exactly as the desktop's
    /// own creation paths do.
    pub project_gate: crate::lifecycle::ProjectLifecycleGate,

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

    /// A CLI-aware diagnostics snapshot, frozen at daemon startup.
    ///
    /// `features` above is a plain `FeatureFlags` — resolved once, but with no
    /// memory of which layer decided each value. The daemon has no live
    /// `SetFeature`-style request that changes flags after startup, so a
    /// snapshot taken once, from the full resolution that still knows about
    /// the CLI layer, stays correct for the daemon's whole lifetime. The
    /// desktop GUI has no CLI layer and *can* change `features` live (the
    /// settings toggle), so it leaves this `None` and `handle_features` falls
    /// back to deriving diagnostics from the live value instead.
    pub feature_diagnostics: Option<Vec<slashit_ipc::FeatureFlagInfo>>,

    /// The resolved application directories. The credentials file lives here,
    /// which is where the TCP bearer token is minted and read.
    pub paths: Arc<crate::config::paths::AppPaths>,

    /// Whether there is disk space to begin new task executions; the same
    /// guard the desktop commands and the executor use.
    pub start_guard: Arc<crate::queue::start_guard::StartGuard>,

    /// The handles a lifecycle transition needs.
    ///
    /// `slashit move <task> done` is the same terminal claim as dragging the
    /// card, so it goes through the same
    /// [`crate::lifecycle::terminalize`] and needs the same things it does:
    /// somewhere to resolve the repository from, something to remove the
    /// worktree with, and the lease that serializes it against the desktop app
    /// and the queue. Before this, the move handler had none of them and
    /// simply assigned the status, which is how a task could reach `Done` over
    /// IPC with its worktree untouched.
    pub repositories: Arc<RwLock<HashMap<Uuid, crate::domain::Repository>>>,
    pub worktree_manager: Arc<crate::worktree::WorktreeManager>,
    pub task_lifecycle_locks: Arc<crate::lifecycle::TaskLifecycleLocks>,
    /// The live queue, when this process runs one, so a terminalization can
    /// tell whether an agent currently owns the task.
    pub executor: Arc<tokio::sync::OnceCell<Arc<crate::queue::TaskExecutor>>>,
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
    listeners: Vec<ListenerOwner>,
    /// The bearer token, present only when some endpoint needs one. Shared
    /// rather than copied per connection so it exists once in memory.
    token: Option<Arc<AuthToken>>,
}

/// Endpoints this process has claimed, held before any application state
/// exists.
///
/// Owning the OS-authenticated local endpoint is what makes a process *the*
/// SlashIt instance, so it has to be established before the process touches
/// anything shared — before state is hydrated, before startup reconciliation
/// rewrites task records, and before the executor can promote a task and
/// spawn an agent. Claiming therefore takes only `AppPaths`, which is pure
/// path resolution, rather than an [`IpcContext`] that can only be built from
/// hydrated state.
///
/// [`attach`](Self::attach) supplies the context afterwards, once there is
/// one, turning the claim into a servable [`IpcServer`].
pub struct BoundIpc {
    listeners: Vec<ListenerOwner>,
    token: Option<Arc<AuthToken>>,
}

/// One bound listener, shared between the accept task that uses it and the
/// supervisor that owns it.
///
/// The OS resource that makes an instance exclusive lives inside the
/// [`IpcListener`]: the `flock` lease and socket inode on Unix, the
/// `first_pipe_instance` named-pipe handle on Windows. Accept tasks only
/// borrow the listener through this shared reference, so an accept task that
/// returns, errors or panics cannot release it. It is released when the last
/// reference is dropped, and the supervisor and the caller's
/// [`IpcOwnership`] drop theirs only after the shutdown drain.
#[derive(Clone)]
struct ListenerOwner {
    endpoint: Endpoint,
    /// `None` only after an optional listener was retired; the OS-verified
    /// listener is never taken out, whatever its accept task does.
    listener: Arc<tokio::sync::Mutex<Option<IpcListener>>>,
    #[cfg(test)]
    fault: Arc<tests::AcceptFault>,
}

impl ListenerOwner {
    fn new(listener: IpcListener) -> Self {
        Self {
            endpoint: listener.endpoint(),
            listener: Arc::new(tokio::sync::Mutex::new(Some(listener))),
            #[cfg(test)]
            fault: Arc::default(),
        }
    }
}

/// The process-level claim on the instance's endpoints.
///
/// Held by whoever supervises the process (the desktop `run` closure or the
/// daemon's `main` path), not by the server task, so ownership outlives the
/// server task even when that task panics. Dropping it releases the
/// exclusive local resource, so the holder drops it only once the executor is
/// quiescent: see [`IpcServer::serve_until`] and [`drain_execution`].
pub struct IpcOwnership {
    _listeners: Vec<ListenerOwner>,
}

impl IpcOwnership {
    /// The OS-verified listeners are the exclusive resource. An optional
    /// listener is held here only when it is all there is.
    fn of(listeners: &[ListenerOwner]) -> Self {
        let exclusive: Vec<_> = listeners
            .iter()
            .filter(|owner| owner.endpoint.is_os_authenticated())
            .cloned()
            .collect();
        Self {
            _listeners: if exclusive.is_empty() {
                listeners.to_vec()
            } else {
                exclusive
            },
        }
    }
}

impl BoundIpc {
    /// Claim what the configuration asks for.
    pub async fn bind(
        paths: &crate::config::paths::AppPaths,
        config: &IpcConfig,
    ) -> anyhow::Result<Self> {
        let options = BindOptions {
            allow_remote: config.tcp.allow_remote,
        };
        Self::bind_endpoints(paths, &config.listen_endpoints(), &options).await
    }

    /// The endpoints actually bound, with any OS-chosen port resolved.
    pub fn endpoints(&self) -> Vec<Endpoint> {
        self.listeners.iter().map(|owner| owner.endpoint.clone()).collect()
    }

    /// A second reference to the claimed listeners, to be released only after
    /// the shutdown drain. Take it before [`attach`](Self::attach).
    pub fn ownership(&self) -> IpcOwnership {
        IpcOwnership::of(&self.listeners)
    }

    /// Hand the claimed listeners to the handlers that will serve them.
    pub fn attach(self, ctx: Arc<IpcContext>) -> IpcServer {
        IpcServer {
            ctx,
            listeners: self.listeners,
            token: self.token,
        }
    }

    /// Claim an explicit list of endpoints.
    ///
    /// Used by tests, which need a socket inside a tempdir rather than the
    /// user's real runtime directory.
    pub async fn bind_endpoints(
        paths: &crate::config::paths::AppPaths,
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
            let path = paths.credentials_file();
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
                    listeners.push(ListenerOwner::new(listener));
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

        Ok(Self { listeners, token })
    }
}

impl IpcServer {
    /// Claim the configured endpoints and attach `ctx` in one step.
    ///
    /// Only appropriate where the context already exists — in practice tests
    /// and the [`serve`] convenience wrapper. Startup paths must use
    /// [`BoundIpc::bind`] instead, so ownership is settled before there is any
    /// state to hydrate.
    pub async fn bind(ctx: Arc<IpcContext>, config: &IpcConfig) -> anyhow::Result<Self> {
        let paths = ctx.paths.clone();
        Ok(BoundIpc::bind(&paths, config).await?.attach(ctx))
    }

    /// Claim an explicit list of endpoints and attach `ctx`.
    pub async fn bind_endpoints(
        ctx: Arc<IpcContext>,
        endpoints: &[Endpoint],
        options: &BindOptions,
    ) -> anyhow::Result<Self> {
        let paths = ctx.paths.clone();
        Ok(BoundIpc::bind_endpoints(&paths, endpoints, options)
            .await?
            .attach(ctx))
    }

    /// The endpoints actually bound, with any OS-chosen port resolved.
    pub fn endpoints(&self) -> Vec<Endpoint> {
        self.listeners.iter().map(|owner| owner.endpoint.clone()).collect()
    }

    /// A second reference to the bound listeners; see [`IpcOwnership`].
    pub fn ownership(&self) -> IpcOwnership {
        IpcOwnership::of(&self.listeners)
    }

    /// Serve until every listener has given up, treating that as a failure.
    ///
    /// The convenience form of [`serve_until`](Self::serve_until) for callers
    /// with no shutdown signal and no poller of their own.
    pub async fn serve(self) -> anyhow::Result<()> {
        let (_stop, shutdown) = tokio::sync::watch::channel(false);
        self.serve_until(Supervision::new(shutdown)).await
    }

    /// Serve until an intentional shutdown or a failure of the instance's
    /// control plane, then run the ownership-preserving shutdown.
    ///
    /// What counts as a failure: the mandatory local (OS-authenticated)
    /// listener giving up, any accept task panicking, a connection handler
    /// panicking, or the last listener of any kind being lost. An optional TCP
    /// listener giving up is reported and retired; the local owner is healthy
    /// and carries on.
    ///
    /// Shutdown, in this order, whatever triggered it:
    ///
    /// 1. Close executor admission. After this returns, no start, review,
    ///    PR flow or Project Run can begin, and none is half-way to
    ///    registering its owner. Mutating IPC requests are refused.
    /// 2. Stop accepting and join every connection handler, so none can
    ///    mutate state afterwards.
    /// 3. Wait for the queue poller to return.
    /// 4. Settle or cancel and reap every execution owner ([`drain_execution`]).
    /// 5. Drop the listeners, which releases the exclusive local resource.
    ///    A caller holding an [`IpcOwnership`] releases it after this returns.
    ///
    /// Steps 3 and 4 do not time out into a release: if an owner cannot be
    /// reaped, the instance keeps its ownership and keeps retrying, because a
    /// timeout is not proof that the owner stopped.
    ///
    /// `Ok(())` is an intentional shutdown that drained cleanly. `Err` is a
    /// failure that was drained cleanly (the instance is quiescent, but the
    /// caller must not report a clean stop).
    pub async fn serve_until(self, supervision: Supervision) -> anyhow::Result<()> {
        let Self {
            ctx,
            listeners,
            token,
        } = self;
        let Supervision {
            mut shutdown,
            poller,
            finish_grace,
        } = supervision;

        let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut accepting = tokio::task::JoinSet::new();
        let mut accept_endpoints = HashMap::new();
        for owner in &listeners {
            let abort = accepting.spawn(accept_loop(owner.clone(), accepted_tx.clone()));
            accept_endpoints.insert(abort.id(), owner.endpoint.clone());
        }
        drop(accepted_tx);
        let mut handlers = tokio::task::JoinSet::new();
        let mut receiving = true;

        let cause = loop {
            if *shutdown.borrow() {
                break Cause::Intentional;
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        break Cause::Intentional;
                    }
                }
                accepted = accepted_rx.recv(), if receiving => match accepted {
                    Some(connection) => {
                        handlers.spawn(handle_connection(connection, ctx.clone(), token.clone()));
                    }
                    // Every sender is gone, so every accept task has returned;
                    // the join arm below reports why.
                    None => receiving = false,
                },
                Some(joined) = accepting.join_next_with_id() => match joined {
                    Ok((id, endpoint)) => {
                        accept_endpoints.remove(&id);
                        eprintln!("[ipc] stopped listening on {endpoint}");
                        if endpoint.is_os_authenticated() {
                            break Cause::Failure(anyhow::anyhow!(
                                "the mandatory local IPC listener stopped on {endpoint}"
                            ));
                        }
                        if accepting.is_empty() {
                            break Cause::Failure(anyhow::anyhow!(
                                "every IPC listener has stopped"
                            ));
                        }
                    }
                    Err(error) => {
                        let endpoint = accept_endpoints.remove(&error.id());
                        if endpoint.as_ref().is_none_or(Endpoint::is_os_authenticated)
                            || accepting.is_empty()
                        {
                            break Cause::Failure(anyhow::anyhow!(
                                "an IPC accept loop ended abnormally on {endpoint:?}: {error}"
                            ));
                        }
                        eprintln!(
                            "[ipc] the optional accept loop on {endpoint:?} ended abnormally: \
                             {error}"
                        );
                        // Closed rather than left bound and unserved.
                        for owner in &listeners {
                            if Some(&owner.endpoint) == endpoint.as_ref() {
                                owner.listener.lock().await.take();
                            }
                        }
                    }
                },
                Some(joined) = handlers.join_next() => {
                    if let Err(error) = joined {
                        break Cause::Failure(anyhow::anyhow!(
                            "an IPC connection handler failed: {error}"
                        ));
                    }
                }
            }
        };

        let mut failure = match cause {
            Cause::Intentional => {
                println!("[ipc] shutting down: closing admission and draining");
                None
            }
            Cause::Failure(error) => {
                eprintln!(
                    "[ipc] FAILING CLOSED: {error}. Admission is closed and this instance keeps \
                     its local ownership until every execution owner has been reaped."
                );
                Some(error)
            }
        };
        // Intentional shutdown may let running work finish; a failure never
        // waits on work it can no longer be told about.
        let grace = if failure.is_none() {
            finish_grace
        } else {
            Duration::ZERO
        };

        // The fence goes up before anything is stopped. A start that read
        // "open" earlier is waited for here and has registered its owner (or
        // returned) by the time this resolves.
        let executor = ctx.executor.get().cloned();
        if let Some(executor) = &executor {
            if grace.is_zero() {
                executor.close_admission_cancelling_runs().await;
            } else {
                executor.close_admission().await;
            }
        }

        accepting.abort_all();
        while accepting.join_next().await.is_some() {}
        drop(accepted_rx);
        // An optional listener carries no ownership: close it now so its
        // clients are refused instead of queued behind a drain. The
        // OS-verified listener stays until the end.
        for owner in &listeners {
            if !owner.endpoint.is_os_authenticated() {
                owner.listener.lock().await.take();
            }
        }
        while let Some(joined) = handlers.join_next().await {
            if let Err(error) = joined {
                failure.get_or_insert_with(|| {
                    anyhow::anyhow!("an IPC connection handler failed during shutdown: {error}")
                });
            }
        }

        if let Some(poller) = poller {
            if let Err(error) = await_poller(poller).await {
                failure.get_or_insert_with(|| anyhow::anyhow!("the queue poller failed: {error}"));
            }
        }

        if let Some(executor) = &executor {
            drain_execution(executor, grace).await;
        }

        // Only now may the exclusive resource go: handlers are joined, the
        // poller has returned and no execution owner remains.
        drop(listeners);
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Why the serve loop stopped.
enum Cause {
    Intentional,
    Failure(anyhow::Error),
}

/// The caller-supplied half of [`IpcServer::serve_until`]: what ends the
/// server intentionally, and what the supervisor must wait for before it may
/// release ownership.
pub struct Supervision {
    /// Flipped to `true` for an intentional shutdown. A dropped sender is
    /// treated the same way.
    pub shutdown: tokio::sync::watch::Receiver<bool>,
    /// Resolves once the queue poller has returned, when this process runs one.
    pub poller: Option<PollerJoin>,
    /// How long an *intentional* shutdown lets running work finish on its own
    /// before cancelling it.
    pub finish_grace: Duration,
}

/// The queue poller's join, as a future the server can wait on.
///
/// Cloneable on purpose: the server task waits on one copy, and the task that
/// supervises the server keeps another. If the server task dies, its copy is
/// gone with it, and the supervisor must still be able to prove the poller has
/// returned before it releases ownership. Closing the admission fence does not
/// prove that: a poll pass that began earlier keeps running until it returns.
pub type PollerJoin = futures::future::Shared<
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>,
>;

/// Wrap the poller's task handle as a [`PollerJoin`]. A panic in the poller
/// resolves it to an error rather than being lost.
pub fn poller_join<H, E>(handle: H) -> PollerJoin
where
    H: std::future::Future<Output = Result<(), E>> + Send + 'static,
    E: std::fmt::Display,
{
    use futures::FutureExt;
    let joined: std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> =
        Box::pin(async move { handle.await.map_err(|error| error.to_string()) });
    joined.shared()
}

/// Wait, as long as it takes, for the poller to return. A bounded wait would
/// be a guess: the poller may be partway through a pass that changes a Task.
async fn await_poller(poller: PollerJoin) -> Result<(), String> {
    let mut poller = poller;
    loop {
        match tokio::time::timeout(POLLER_STOP_REPORT, &mut poller).await {
            Ok(result) => return result,
            Err(_) => eprintln!(
                "[ipc] the queue poller has not returned after {POLLER_STOP_REPORT:?}; still \
                 holding local ownership"
            ),
        }
    }
}

/// Finish the shutdown when the task running [`IpcServer::serve_until`] died
/// (panicked or was cancelled) before it could do so.
///
/// The caller still holds the ownership claim. This does what the server
/// would have done, in the same order: close the admission fence, wait for the
/// poller to return (a pass that began before the fence is not stopped by it),
/// then reap every execution owner. Only when this returns may the caller
/// release ownership.
pub async fn quiesce_after_supervisor_failure(
    executor: Option<&crate::queue::TaskExecutor>,
    poller: Option<PollerJoin>,
) {
    if let Some(executor) = executor {
        executor.close_admission_cancelling_runs().await;
    }
    if let Some(poller) = poller {
        if let Err(error) = await_poller(poller).await {
            eprintln!("[ipc] the queue poller failed: {error}");
        }
    }
    if let Some(executor) = executor {
        drain_execution(executor, Duration::ZERO).await;
    }
}

impl Supervision {
    /// Supervision with no poller and no grace for running work.
    pub fn new(shutdown: tokio::sync::watch::Receiver<bool>) -> Self {
        Self {
            shutdown,
            poller: None,
            finish_grace: Duration::ZERO,
        }
    }
}

/// How often a poller that has not yet returned is reported.
const POLLER_STOP_REPORT: Duration = Duration::from_secs(30);

/// Pause between attempts to reap owners that did not stop in time.
const DRAIN_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(10)
} else {
    Duration::from_secs(1)
};

/// Close admission and do not return until the executor holds no owner.
///
/// Optionally lets running work finish on its own for up to `grace` first.
/// Anything left is cancelled and reaped through the executor's existing
/// owner-ending paths, which leave Task status, Task Checkouts, branches,
/// commits and recovery records alone: a Task that was `in_progress` stays
/// `in_progress`, which is exactly what startup recovery requeues.
///
/// Never returns while an owner cannot be reaped. A bounded wait is not proof
/// that a provider process has stopped, and returning would let the caller
/// release exclusive ownership under a process that can still act. The
/// instance stays alive, loud, and holding ownership; an operator who forces
/// it down accepts that an orphaned provider process may survive.
pub async fn drain_execution(executor: &crate::queue::TaskExecutor, grace: Duration) {
    if grace.is_zero() {
        executor.close_admission_cancelling_runs().await;
    } else {
        executor.close_admission().await;
    }
    if !grace.is_zero() {
        let deadline = tokio::time::Instant::now() + grace;
        let live = || async {
            executor.running_task_count().await + executor.active_project_run_count()
        };
        let running = live().await;
        if running > 0 {
            println!("[ipc] waiting up to {grace:?} for {running} running owner(s) to finish");
        }
        while live().await > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100).min(grace)).await;
        }
    }
    let mut attempts = 0u32;
    loop {
        match executor.quiesce_for_shutdown().await {
            Ok(()) => return,
            Err(error) => {
                attempts += 1;
                if attempts == 1 || attempts.is_multiple_of(30) {
                    eprintln!(
                        "[ipc] an execution owner could not be reaped yet: {error}. Local \
                         ownership is retained and the shutdown is NOT clean; retrying."
                    );
                }
                tokio::time::sleep(DRAIN_RETRY).await;
            }
        }
    }
}

/// Accept connections on one listener until it fails for good.
///
/// Returns the endpoint it gave up on, so the caller can report which one.
/// The listener stays owned by `owner`; returning or panicking here releases
/// nothing.
async fn accept_loop(
    owner: ListenerOwner,
    accepted: tokio::sync::mpsc::UnboundedSender<Accepted>,
) -> Endpoint {
    let endpoint = owner.endpoint.clone();
    let mut consecutive_failures = 0u32;

    loop {
        match next_connection(&owner).await {
            Ok(connection) => {
                consecutive_failures = 0;
                if accepted.send(connection).is_err() {
                    return endpoint;
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                eprintln!("[ipc] accept failed on {endpoint}: {e}");
                if consecutive_failures >= MAX_CONSECUTIVE_ACCEPT_FAILURES {
                    eprintln!(
                        "[ipc] giving up on {endpoint} after {consecutive_failures} consecutive \
                         failures"
                    );
                    // An optional listener that is given up on is closed, so
                    // its clients are refused rather than left queued. The
                    // OS-verified one is kept: it is the ownership.
                    if !endpoint.is_os_authenticated() {
                        owner.listener.lock().await.take();
                    }
                    return endpoint;
                }
                // Backoff rather than an immediate retry: a listener that fails
                // synchronously would otherwise saturate a core while it did.
                tokio::time::sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

#[cfg(not(test))]
async fn next_connection(owner: &ListenerOwner) -> std::io::Result<Accepted> {
    match owner.listener.lock().await.as_mut() {
        Some(listener) => listener.accept().await,
        None => Err(std::io::Error::other("the listener was retired")),
    }
}

/// Production accept, with the test-only fault seam in front of it: the
/// failure and the panic come out of the real accept loop and the real
/// supervisor, not out of a stand-in for them.
#[cfg(test)]
async fn next_connection(owner: &ListenerOwner) -> std::io::Result<Accepted> {
    owner.fault.check()?;
    let mut guard = owner.listener.lock().await;
    let Some(listener) = guard.as_mut() else {
        return Err(std::io::Error::other("the listener was retired"));
    };
    tokio::select! {
        accepted = listener.accept() => accepted,
        _ = owner.fault.woken() => {
            owner.fault.check()?;
            Err(std::io::Error::other("spurious accept wake"))
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
        if ctx
            .executor
            .get()
            .is_some_and(|executor| !executor.is_accepting_work())
        {
            audit(verb, peer, "denied-shutting-down");
            return Dispatch::reply(IpcResponse::error(
                "This SlashIt instance is shutting down and no longer accepts state changes.",
            ));
        }
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

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
    use std::time::Duration;

    use tempfile::TempDir;
    use uuid::Uuid;

    use slashit_ipc::client::{send, ClientOptions};
    use slashit_ipc::transport;
    use slashit_ipc::{IpcRequest, IpcResponse};

    use super::*;
    use crate::commands::gui_admission::GuiMutation;
    use crate::config::paths::AppPaths;
    use crate::domain::task::{TaskStatus, WorkerHandoff, WorkerHandoffStage};
    use crate::instance::{InstanceControl, InstanceMode, SharedInstanceControl};
    use crate::test_helpers::{attach_test_executor_ipc, create_test_task_full, ipc_test_context};

    /// What the next accept on one listener should do instead of accepting.
    #[derive(Default)]
    pub(crate) struct AcceptFault {
        mode: AtomicU8,
        wake: tokio::sync::Notify,
    }

    const NONE: u8 = 0;
    const FAIL: u8 = 1;
    const PANIC: u8 = 2;

    impl AcceptFault {
        pub(crate) fn fail(&self) {
            self.mode.store(FAIL, Ordering::SeqCst);
            self.wake.notify_one();
        }

        pub(crate) fn panic(&self) {
            self.mode.store(PANIC, Ordering::SeqCst);
            self.wake.notify_one();
        }

        pub(super) fn check(&self) -> std::io::Result<()> {
            match self.mode.load(Ordering::SeqCst) {
                NONE => Ok(()),
                FAIL => Err(std::io::Error::other("injected accept failure")),
                _ => panic!("injected accept-loop panic"),
            }
        }

        pub(super) async fn woken(&self) {
            self.wake.notified().await;
        }
    }

    /// A control whose quit request panics inside the connection handler: a
    /// handler failure that arrives over the real transport.
    struct PanickingControl;

    impl InstanceControl for PanickingControl {
        fn mode(&self) -> InstanceMode {
            InstanceMode::Daemon
        }
        fn show_window(&self) -> Result<(), String> {
            Err("no window".into())
        }
        fn request_quit(&self) {
            panic!("injected connection-handler panic");
        }
    }

    /// The local endpoint: a Unix socket in the test's tempdir, or a uniquely
    /// named pipe on Windows (the pipe namespace is machine-global).
    ///
    /// Derived from the root alone, so a second process given the same root
    /// names the same endpoint.
    fn local_endpoint(root: &std::path::Path) -> Endpoint {
        #[cfg(unix)]
        {
            std::fs::create_dir_all(root.join("runtime")).unwrap();
            Endpoint::Unix {
                path: root.join("runtime/slashit.sock"),
            }
        }
        #[cfg(windows)]
        {
            let tag = root.file_name().unwrap().to_string_lossy().replace('.', "-");
            Endpoint::NamedPipe {
                name: format!(r"\\.\pipe\slashit-shutdown-test-{tag}"),
            }
        }
    }

    fn tcp_endpoint() -> Endpoint {
        Endpoint::Tcp {
            addr: std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
        }
    }

    struct Rig {
        _tmp: Option<TempDir>,
        root: std::path::PathBuf,
        paths: Arc<AppPaths>,
        ctx: Arc<IpcContext>,
        executor: Arc<crate::queue::TaskExecutor>,
        local: Endpoint,
        tcp: Option<Endpoint>,
        local_fault: Option<Arc<AcceptFault>>,
        tcp_fault: Option<Arc<AcceptFault>>,
        stop: tokio::sync::watch::Sender<bool>,
        ownership: Option<IpcOwnership>,
        server: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    }

    struct Layout {
        /// Use this directory instead of a fresh tempdir (a second process
        /// names the same endpoint by sharing it).
        root: Option<std::path::PathBuf>,
        local: bool,
        tcp: bool,
        control: Option<SharedInstanceControl>,
        grace: Duration,
    }

    impl Layout {
        fn local_only() -> Self {
            Self { root: None, local: true, tcp: false, control: None, grace: Duration::ZERO }
        }
        fn local_and_tcp() -> Self {
            Self { root: None, local: true, tcp: true, control: None, grace: Duration::ZERO }
        }
        fn tcp_only() -> Self {
            Self { root: None, local: false, tcp: true, control: None, grace: Duration::ZERO }
        }
    }

    impl Rig {
        async fn start(layout: Layout) -> Self {
            let (tmp, root) = match layout.root {
                Some(root) => (None, root),
                None => {
                    let tmp = TempDir::new().expect("tempdir");
                    let root = tmp.path().to_path_buf();
                    (Some(tmp), root)
                }
            };
            let paths = Arc::new(AppPaths::with_roots(
                root.join("config"),
                root.join("data"),
                root.join("cache"),
                root.join("runtime"),
            ));
            let mut context = ipc_test_context(paths.clone());
            if let Some(control) = layout.control {
                context.control = control;
            }
            let ctx = Arc::new(context);
            let executor = attach_test_executor_ipc(&ctx).await;

            let local = local_endpoint(&root);
            let mut wanted = Vec::new();
            if layout.local {
                wanted.push(local.clone());
            }
            if layout.tcp {
                wanted.push(tcp_endpoint());
            }
            let bound = BoundIpc::bind_endpoints(&paths, &wanted, &BindOptions::default())
                .await
                .expect("bind inside a tempdir");
            let endpoints = bound.endpoints();
            let tcp = endpoints.iter().find(|e| !e.is_os_authenticated()).cloned();
            let fault_of = |wanted_local: bool| {
                bound
                    .listeners
                    .iter()
                    .find(|owner| owner.endpoint.is_os_authenticated() == wanted_local)
                    .map(|owner| owner.fault.clone())
            };
            let local_fault = fault_of(true);
            let tcp_fault = fault_of(false);
            let ownership = bound.ownership();

            let (stop, shutdown) = tokio::sync::watch::channel(false);
            let supervision = Supervision {
                shutdown,
                poller: None,
                finish_grace: layout.grace,
            };
            let server = tokio::spawn({
                let ctx = ctx.clone();
                async move { bound.attach(ctx).serve_until(supervision).await }
            });
            Self {
                _tmp: tmp,
                root,
                paths,
                ctx,
                executor,
                local,
                tcp,
                local_fault,
                tcp_fault,
                stop,
                ownership: Some(ownership),
                server: Some(server),
            }
        }

        /// A second SlashIt instance trying to become the owner.
        async fn second_instance(&self) -> anyhow::Result<BoundIpc> {
            BoundIpc::bind_endpoints(&self.paths, std::slice::from_ref(&self.local), &BindOptions::default())
                .await
        }

        fn server_finished(&self) -> bool {
            self.server.as_ref().is_some_and(|server| server.is_finished())
        }

        async fn finish(&mut self) -> anyhow::Result<()> {
            self.server
                .take()
                .expect("the server is still held")
                .await
                .expect("the server task must not die: the supervisor catches every failure")
        }

        fn options(&self, endpoint: &Endpoint) -> ClientOptions {
            let token = (!endpoint.is_os_authenticated()).then(|| {
                slashit_ipc::auth::Credentials::ensure_token(&self.paths.credentials_file())
                    .expect("token")
                    .expose()
                    .to_string()
            });
            ClientOptions {
                endpoint: endpoint.clone(),
                token,
                wait: Duration::ZERO,
            }
        }

        async fn ping(&self, endpoint: &Endpoint) -> anyhow::Result<IpcResponse> {
            send(&IpcRequest::Ping, &self.options(endpoint))
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))
        }
    }

    /// Acquire the endpoint after a release, tolerating the instant in which a
    /// concurrently forking test still holds a copy of the lease descriptor
    /// between `fork` and `exec`. A lease that is genuinely held never clears.
    async fn acquire_after_release<F, Fut>(mut attempt: F) -> BoundIpc
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<BoundIpc>>,
    {
        let mut last = None;
        for _ in 0..1000 {
            match attempt().await {
                Ok(bound) => return bound,
                Err(error) => last = Some(error),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("ownership was never released: {:?}", last);
    }

    async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for: {what}"));
    }

    /// Give a pending future every chance to complete, to make a "still
    /// waiting" assertion mean something.
    async fn settle() {
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn git(repo: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(["-c", "user.email=t@example.com", "-c", "user.name=T"])
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    // --- 1, 9, 10, 11, 12, 13, 14: the real failure scenario -----------------

    /// The confirmed defect: the mandatory local listener fails while TCP is
    /// not even configured. The instance must refuse work, cancel and reap the
    /// Worker that is still running, keep the local ownership (a second
    /// instance is refused) for the whole time, and release it only once the
    /// Worker has actually stopped. The Task, its recovery evidence, its
    /// checkout and its Git refs are untouched throughout.
    #[tokio::test]
    async fn local_listener_failure_holds_ownership_until_the_worker_is_reaped() {
        let mut rig = Rig::start(Layout::local_only()).await;

        // A Task whose Worker is running, with S1 recovery evidence on it, a
        // real checkout and a real branch.
        let repo = rig.root.join("repository");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        git(&repo, &["branch", "task-branch"]);
        let checkout = rig.root.join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("work.txt"), "uncommitted work").unwrap();

        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("running", project_id, TaskStatus::InProgress, 0);
        task.worktree_path = Some(checkout.to_string_lossy().to_string());
        task.branch_name = Some("task-branch".into());
        task.worker_handoff = Some(WorkerHandoff {
            task_id: task.id,
            project_id,
            repository_id: None,
            run: Some(1),
            branch: Some("task-branch".into()),
            start_head: None,
            stage: WorkerHandoffStage::RecoveryRequired {
                reason: "evidence that must survive".into(),
                observed_commit: None,
            },
        });
        let task_id = task.id;
        rig.ctx.storage.save_project_tasks(project_id, std::slice::from_ref(&task)).unwrap();
        rig.ctx.tasks.write().await.insert(task_id, task);
        let task_json = |tasks: &HashMap<Uuid, Task>| serde_json::to_string(&tasks[&task_id]).unwrap();
        let storage = rig.ctx.storage.clone();
        let stored_json = move || {
            serde_json::to_string(&storage.load_project_tasks(project_id).unwrap()).unwrap()
        };
        let task_before = task_json(&*rig.ctx.tasks.read().await);
        let stored_before = stored_json();
        let refs_before = git(&repo, &["for-each-ref", "--format=%(refname) %(objectname)"]);

        let worker = rig.executor.register_gated_running_execution_for_test(task_id).await;
        assert!(rig.ping(&rig.local).await.unwrap().ok, "serving before the failure");

        rig.local_fault.as_ref().expect("local listener").fail();

        eventually("admission to close", || !rig.executor.is_accepting_work()).await;
        eventually("the Worker to be told to stop", || {
            worker.cancel_seen.load(Ordering::SeqCst)
        })
        .await;
        settle().await;

        // The Worker has been told to stop and has not stopped: the instance
        // can still be doing conflicting work, so nobody else may own it.
        assert!(!worker.finished.load(Ordering::SeqCst));
        assert!(!rig.server_finished(), "the drain waits for the Worker");
        assert!(
            rig.second_instance().await.is_err(),
            "a second instance must not acquire ownership while the Worker is alive"
        );

        worker.release.send(true).unwrap();
        let error = rig.finish().await.expect_err("a failure is never reported as a clean stop");
        assert!(error.to_string().contains("mandatory local IPC listener"), "{error}");
        assert!(worker.finished.load(Ordering::SeqCst));
        assert_eq!(rig.executor.running_task_count().await, 0);

        // The supervisor still holds the claim after the server task is gone.
        assert!(
            rig.second_instance().await.is_err(),
            "ownership is held by the supervisor, not by the server task"
        );
        drop(rig.ownership.take());
        acquire_after_release(|| rig.second_instance()).await;

        assert_eq!(task_json(&*rig.ctx.tasks.read().await), task_before, "Task and evidence");
        assert_eq!(stored_json(), stored_before, "persisted Task");
        assert_eq!(
            git(&repo, &["for-each-ref", "--format=%(refname) %(objectname)"]),
            refs_before
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("work.txt")).unwrap(),
            "uncommitted work"
        );
    }

    // --- 2: local fails, TCP keeps accepting ---------------------------------

    #[tokio::test]
    async fn local_listener_failure_fails_closed_even_though_tcp_is_healthy() {
        let mut rig = Rig::start(Layout::local_and_tcp()).await;
        let tcp = rig.tcp.clone().expect("tcp bound");
        assert!(rig.ping(&tcp).await.unwrap().ok, "TCP serves before the failure");

        rig.local_fault.as_ref().unwrap().fail();

        let error = rig.finish().await.expect_err("fail closed");
        assert!(error.to_string().contains("mandatory local IPC listener"), "{error}");
        assert!(!rig.executor.is_accepting_work());
        assert!(
            transport::connect(&tcp).await.is_err(),
            "the surviving TCP listener is closed with the rest of the instance"
        );
        drop(rig.ownership.take());
        acquire_after_release(|| rig.second_instance()).await;
    }

    // --- 3: TCP fails, local stays the owner ---------------------------------

    #[tokio::test]
    async fn tcp_listener_failure_leaves_the_local_owner_serving() {
        let mut rig = Rig::start(Layout::local_and_tcp()).await;
        let tcp = rig.tcp.clone().expect("tcp bound");

        rig.tcp_fault.as_ref().unwrap().fail();

        tokio::time::timeout(Duration::from_secs(10), async {
            while transport::connect(&tcp).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the failed optional listener is closed, so its clients are refused");
        assert!(!rig.server_finished(), "TCP is optional: the server carries on");
        assert!(rig.executor.is_accepting_work());
        assert!(rig.ping(&rig.local).await.unwrap().ok, "the local owner still answers");
        assert!(rig.second_instance().await.is_err(), "and still owns the endpoint");

        rig.stop.send(true).unwrap();
        rig.finish().await.expect("a later intentional shutdown is clean");
    }

    // --- 4: abnormal accept-loop completion and panic -------------------------

    #[tokio::test]
    async fn a_panic_in_the_local_accept_loop_fails_closed() {
        let mut rig = Rig::start(Layout::local_and_tcp()).await;
        rig.local_fault.as_ref().unwrap().panic();

        let error = rig.finish().await.expect_err("fail closed");
        assert!(error.to_string().contains("ended abnormally"), "{error}");
        assert!(!rig.executor.is_accepting_work());
        drop(rig.ownership.take());
        acquire_after_release(|| rig.second_instance()).await;
    }

    #[tokio::test]
    async fn a_panic_in_an_optional_accept_loop_is_not_fatal() {
        let mut rig = Rig::start(Layout::local_and_tcp()).await;
        rig.tcp_fault.as_ref().unwrap().panic();
        settle().await;

        assert!(!rig.server_finished());
        assert!(rig.executor.is_accepting_work());
        assert!(rig.ping(&rig.local).await.unwrap().ok);
        rig.stop.send(true).unwrap();
        rig.finish().await.expect("clean");
    }

    #[tokio::test]
    async fn losing_the_only_listener_fails_closed() {
        let mut rig = Rig::start(Layout::tcp_only()).await;
        rig.tcp_fault.as_ref().unwrap().fail();

        let error = rig.finish().await.expect_err("fail closed");
        assert!(error.to_string().contains("every IPC listener"), "{error}");
        assert!(!rig.executor.is_accepting_work());
    }

    // --- 4/5: handler panic ---------------------------------------------------

    #[tokio::test]
    async fn a_panicking_connection_handler_fails_closed() {
        let mut rig = Rig::start(Layout {
            control: Some(Arc::new(PanickingControl)),
            ..Layout::local_only()
        })
        .await;

        // The handler answers, then asks to quit, and the quit panics.
        let _ = send(&IpcRequest::Quit, &rig.options(&rig.local)).await;

        let error = rig.finish().await.expect_err("fail closed");
        assert!(error.to_string().contains("connection handler failed"), "{error}");
        assert!(!rig.executor.is_accepting_work());
    }

    // --- 5: intentional shutdown ----------------------------------------------

    #[tokio::test]
    async fn an_intentional_shutdown_drains_then_releases_and_reports_clean() {
        let mut rig = Rig::start(Layout::local_and_tcp()).await;
        let task_id = Uuid::new_v4();
        let worker = rig.executor.register_gated_running_execution_for_test(task_id).await;

        rig.stop.send(true).unwrap();
        eventually("the Worker to be told to stop", || {
            worker.cancel_seen.load(Ordering::SeqCst)
        })
        .await;
        settle().await;
        assert!(!rig.executor.is_accepting_work());
        assert!(!rig.server_finished());
        assert!(rig.second_instance().await.is_err(), "still the owner while draining");

        worker.release.send(true).unwrap();
        rig.finish().await.expect("an intentional shutdown that drained is clean");
        drop(rig.ownership.take());
        acquire_after_release(|| rig.second_instance()).await;
    }

    /// Intentional shutdown may let running work finish on its own first; a
    /// failure never does.
    #[tokio::test]
    async fn drain_gives_running_work_its_grace_before_cancelling_it() {
        let (executor, _temps) = {
            let tmp = TempDir::new().unwrap();
            let paths = Arc::new(AppPaths::with_roots(
                tmp.path().join("config"),
                tmp.path().join("data"),
                tmp.path().join("cache"),
                tmp.path().join("runtime"),
            ));
            let ctx = ipc_test_context(paths);
            (attach_test_executor_ipc(&ctx).await, (tmp, ctx))
        };
        let lease = executor.begin_project_run(Uuid::new_v4(), false).await.unwrap();
        let mut cancel = lease.cancel_receiver();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            drop(lease);
        });

        drain_execution(&executor, Duration::from_secs(30)).await;

        assert!(!*cancel.borrow_and_update(), "it finished by itself and was never cancelled");
        assert_eq!(executor.running_task_count().await, 0);
    }

    // --- 6: in-flight handler during fencing ----------------------------------

    #[tokio::test]
    async fn an_in_flight_handler_is_joined_and_cannot_mutate_after_the_fence() {
        let mut rig = Rig::start(Layout::local_only()).await;

        // A connection whose handler is admitted and waiting for its request.
        let stream = transport::connect(&rig.local).await.expect("connect");
        let (reader, mut writer) = stream.into_halves();
        settle().await;

        rig.local_fault.as_ref().unwrap().fail();
        eventually("admission to close", || !rig.executor.is_accepting_work()).await;
        settle().await;
        assert!(!rig.server_finished(), "the admitted handler is still accounted for");
        assert!(rig.second_instance().await.is_err());

        // The handler now receives a state-changing request. It is refused.
        let request = slashit_ipc::IpcEnvelope::new(IpcRequest::DeleteTask {
            task_id: Uuid::new_v4().to_string(),
        });
        slashit_ipc::framing::write_json_frame(&mut writer, &request).await.unwrap();
        let frame = slashit_ipc::framing::read_frame(reader, MAX_REQUEST_BYTES)
            .await
            .expect("answered");
        let slashit_ipc::framing::Frame::Line(line) = frame else { panic!("no answer") };
        let response: IpcResponse = serde_json::from_str(&line).unwrap();
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("shutting down"));

        let error = rig.finish().await.expect_err("fail closed");
        assert!(error.to_string().contains("mandatory local"), "{error}");
        assert!(rig.executor.running_task_count().await == 0);
    }

    // --- supervisor panic ---------------------------------------------------

    /// The task that runs the server dies. The supervisor above it holds the
    /// ownership claim, and the poller's join, so a poll pass that began
    /// before the fence closed is still waited for: closing the fence is not
    /// proof that it has stopped.
    #[tokio::test]
    async fn a_supervisor_panic_waits_for_the_running_poll_pass_before_ownership_can_go() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let paths = Arc::new(AppPaths::with_roots(
            root.join("config"),
            root.join("data"),
            root.join("cache"),
            root.join("runtime"),
        ));
        let ctx = Arc::new(ipc_test_context(paths.clone()));
        let executor = attach_test_executor_ipc(&ctx).await;
        let local = local_endpoint(&root);
        let bound = BoundIpc::bind_endpoints(&paths, std::slice::from_ref(&local), &BindOptions::default())
            .await
            .expect("bind");
        let ownership = bound.ownership();
        let second = || async {
            BoundIpc::bind_endpoints(&paths, std::slice::from_ref(&local), &BindOptions::default())
                .await
        };

        // A poll pass held mid-way at a barrier. When it is let go it changes
        // "state", standing for a Task update.
        let (release, mut gate) = tokio::sync::watch::channel(false);
        let in_pass = Arc::new(AtomicBool::new(false));
        let pass_finished = Arc::new(AtomicBool::new(false));
        let poller = poller_join(tokio::spawn({
            let (in_pass, pass_finished) = (in_pass.clone(), pass_finished.clone());
            async move {
                in_pass.store(true, Ordering::SeqCst);
                let _ = gate.wait_for(|released| *released).await;
                pass_finished.store(true, Ordering::SeqCst);
            }
        }));
        eventually("the poll pass to start", || in_pass.load(Ordering::SeqCst)).await;

        // The supervisor task holds the listeners and dies without draining.
        let server = tokio::spawn(async move {
            let _listeners = bound;
            if true {
                panic!("the supervisor failed");
            }
            Ok::<(), anyhow::Error>(())
        });
        assert!(server.await.expect_err("it panicked").is_panic());

        let quiesced = tokio::spawn({
            let executor = executor.clone();
            async move { quiesce_after_supervisor_failure(Some(&executor), Some(poller)).await }
        });
        eventually("admission to close", || !executor.is_accepting_work()).await;
        settle().await;
        assert!(!quiesced.is_finished(), "closing the fence is not the poller returning");
        assert!(!pass_finished.load(Ordering::SeqCst));
        assert!(second().await.is_err(), "ownership is held while the pass is running");

        release.send(true).unwrap();
        quiesced.await.expect("quiescence completes");
        assert!(pass_finished.load(Ordering::SeqCst), "the pass finished before quiescence did");

        drop(ownership);
        acquire_after_release(second).await;
    }

    // --- 12, 13: a second OS process ---------------------------------------

    const CHILD_ROOT: &str = "SLASHIT_SHUTDOWN_CHILD_ROOT";

    /// The first instance, as a separate OS process.
    ///
    /// Re-executed by [`a_second_process_cannot_own_the_endpoint_while_the_first_drains`]
    /// with the root in the environment; a no-op in an ordinary test run. It
    /// runs the production supervisor over a real listener, loses its local
    /// listener, and holds a Worker that has been told to stop but has not
    /// stopped, until the parent says to let it go.
    #[tokio::test]
    async fn shutdown_child_process() {
        let Ok(root) = std::env::var(CHILD_ROOT) else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let mut rig = Rig::start(Layout {
            root: Some(root.clone()),
            ..Layout::local_only()
        })
        .await;
        let worker = rig
            .executor
            .register_gated_running_execution_for_test(Uuid::new_v4())
            .await;
        std::fs::write(root.join("serving"), b"").unwrap();

        rig.local_fault.as_ref().unwrap().fail();
        eventually("the Worker to be told to stop", || {
            worker.cancel_seen.load(Ordering::SeqCst)
        })
        .await;
        std::fs::write(root.join("draining"), b"").unwrap();

        // Held until the parent has made its observations.
        let go = root.join("go");
        eventually("the parent's go-ahead", || go.exists()).await;
        worker.release.send(true).unwrap();

        let error = rig.finish().await.expect_err("fail closed");
        assert!(error.to_string().contains("mandatory local"), "{error}");
        drop(rig.ownership.take());
    }

    fn spawn_child(root: &std::path::Path) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["ipc::server::tests::shutdown_child_process", "--exact", "--nocapture"])
            .env(CHILD_ROOT, root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("re-execute the test binary as the first instance")
    }

    async fn wait_for_file(path: &std::path::Path, child: &mut std::process::Child) {
        tokio::time::timeout(Duration::from_secs(120), async {
            while !path.exists() {
                if let Some(status) = child.try_wait().unwrap() {
                    panic!("the first instance exited early: {status}");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {}", path.display()));
    }

    fn second_instance_at(root: &std::path::Path) -> impl std::future::Future<Output = anyhow::Result<BoundIpc>> {
        let paths = AppPaths::with_roots(
            root.join("parent-config"),
            root.join("parent-data"),
            root.join("parent-cache"),
            root.join("runtime"),
        );
        let endpoint = local_endpoint(root);
        async move {
            BoundIpc::bind_endpoints(&paths, &[endpoint], &BindOptions::default()).await
        }
    }

    /// The real boundary: two OS processes, the production supervisor, the
    /// real listener and its kernel-held lease. While the first process is
    /// failing closed with a Worker still stopping, the second cannot become
    /// the owner. Once the first has quiesced and exited, it can.
    #[tokio::test]
    async fn a_second_process_cannot_own_the_endpoint_while_the_first_drains() {
        let root = TempDir::new().unwrap();
        let mut child = spawn_child(root.path());

        wait_for_file(&root.path().join("serving"), &mut child).await;
        wait_for_file(&root.path().join("draining"), &mut child).await;
        assert!(
            second_instance_at(root.path()).await.is_err(),
            "the first process is draining a live Worker: no one else may own the endpoint"
        );
        assert!(child.try_wait().unwrap().is_none(), "and it has not exited");

        std::fs::write(root.path().join("go"), b"").unwrap();
        let status = tokio::task::spawn_blocking(move || child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "the first instance failed closed and exited: {status}");

        acquire_after_release(|| second_instance_at(root.path())).await;
    }

    // --- GUI commands -----------------------------------------------------

    /// What a desktop mutation does in this test: record that it started,
    /// wait at a barrier, then change "state" (a flag). It stands for any
    /// command that saves a Task or moves a Git reference.
    struct GuiGate {
        started: Arc<AtomicUsize>,
        performed: Arc<AtomicBool>,
        release: tokio::sync::watch::Receiver<bool>,
    }

    #[tauri::command]
    async fn gated_gui_mutation(
        _gui: GuiMutation,
        gate: tauri::State<'_, GuiGate>,
    ) -> Result<(), String> {
        gate.started.fetch_add(1, Ordering::SeqCst);
        let mut release = gate.release.clone();
        let _ = release.wait_for(|released| *released).await;
        gate.performed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn invoke_gated(
        webview: &tauri::WebviewWindow<tauri::test::MockRuntime>,
    ) -> Result<tauri::ipc::InvokeResponseBody, serde_json::Value> {
        tauri::test::get_ipc_response(
            webview,
            tauri::webview::InvokeRequest {
                cmd: "gated_gui_mutation".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: "tauri://localhost".parse().unwrap(),
                body: tauri::ipc::InvokeBody::default(),
                headers: Default::default(),
                invoke_key: tauri::test::INVOKE_KEY.to_string(),
            },
        )
    }

    /// The desktop's version of the confirmed defect. A GUI command is already
    /// running when the local listener fails. Ownership must outlive it: a
    /// second acquirer is refused until the command has finished its mutation,
    /// a command dispatched after the fence is refused without running, and
    /// only then is the endpoint released.
    #[tokio::test]
    async fn an_in_flight_gui_mutation_finishes_before_ownership_is_released() {
        let mut rig = Rig::start(Layout::local_only()).await;

        let (state, _report) = crate::app_core::build_state_with_paths(rig.paths.clone())
            .await
            .expect("build state");
        state.executor.set(rig.executor.clone()).ok().expect("one executor");

        let started = Arc::new(AtomicUsize::new(0));
        let performed = Arc::new(AtomicBool::new(false));
        let (release, released) = tokio::sync::watch::channel(false);
        let app = tauri::test::mock_builder()
            .invoke_handler(tauri::generate_handler![gated_gui_mutation])
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("mock app");
        {
            use tauri::Manager;
            app.manage(state);
            app.manage(GuiGate {
                started: started.clone(),
                performed: performed.clone(),
                release: released,
            });
        }
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("webview");

        // The command is admitted and parked at the barrier.
        let in_flight = tokio::task::spawn_blocking({
            let webview = webview.clone();
            move || invoke_gated(&webview)
        });
        let mut in_flight = in_flight;
        tokio::select! {
            result = &mut in_flight => panic!("the command returned early: {:?}", result),
            _ = eventually("the command to be in flight", || started.load(Ordering::SeqCst) == 1) => {}
        }

        rig.local_fault.as_ref().unwrap().fail();
        eventually("admission to close", || !rig.executor.is_accepting_work()).await;
        settle().await;
        assert!(!rig.server_finished(), "the supervisor waits for the admitted command");
        assert!(rig.second_instance().await.is_err(), "ownership is still held");
        assert!(!performed.load(Ordering::SeqCst));

        // A command dispatched now is refused before its body runs.
        let refused = tokio::task::spawn_blocking({
            let webview = webview.clone();
            move || invoke_gated(&webview)
        })
        .await
        .unwrap()
        .expect_err("refused after the fence");
        assert!(refused.to_string().contains("shutting down"), "{refused}");
        assert_eq!(started.load(Ordering::SeqCst), 1, "the refused command never started");

        // Let the admitted command finish. Only after it has mutated may the
        // supervisor release the endpoint.
        release.send(true).unwrap();
        in_flight.await.unwrap().expect("the admitted command completes");
        assert!(performed.load(Ordering::SeqCst));

        let error = rig.finish().await.expect_err("fail closed");
        assert!(error.to_string().contains("mandatory local"), "{error}");
        drop(rig.ownership.take());
        acquire_after_release(|| rig.second_instance()).await;
        assert!(performed.load(Ordering::SeqCst), "nothing ran after release");
    }

    /// What is *not* promised, pinned so it stays honest: killing the owner
    /// outright (SIGKILL, like power loss) releases the kernel lease at once,
    /// whatever the owner was in the middle of. No supervisor can run in that
    /// case, so a surviving provider process is outside this contract.
    #[cfg(unix)]
    #[tokio::test]
    async fn killing_the_owner_outright_releases_ownership_immediately() {
        let root = TempDir::new().unwrap();
        let mut child = spawn_child(root.path());
        wait_for_file(&root.path().join("draining"), &mut child).await;
        assert!(second_instance_at(root.path()).await.is_err());

        child.kill().expect("SIGKILL");
        child.wait().unwrap();

        acquire_after_release(|| second_instance_at(root.path())).await;
    }
}
