//! Transport: the bytes, and what the operating system can prove about who
//! sent them.
//!
//! Everything above this module — the command set, the framing, the handlers —
//! is identical on every platform. Only this layer differs, and it differs in
//! exactly two ways: how a connection is established, and whether the OS has
//! already established the peer's identity.
//!
//! | Platform | Local transport | OS-authenticated |
//! |---|---|---|
//! | Linux, macOS | Unix domain socket in a 0700 directory | yes |
//! | Windows | named pipe, owner-only DACL, remote clients rejected | yes |
//! | any | loopback TCP, off by default | no — bearer token required |

use std::io;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::endpoint::Endpoint;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

mod tcp;

/// A connected peer, split into halves so the read side can be size-capped
/// independently of the write side.
///
/// Boxed rather than an enum with per-platform variants: the protocol issues
/// one request and one response per connection, so a virtual call per message
/// is free, and the alternative is an `AsyncRead`/`AsyncWrite` impl per variant
/// per platform that exists only to delegate.
pub struct IpcStream {
    reader: Box<dyn AsyncRead + Send + Unpin>,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
}

impl IpcStream {
    /// Wrap any bidirectional stream.
    pub fn new<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        Self {
            reader: Box::new(reader),
            writer: Box::new(writer),
        }
    }

    /// Split into the halves the framing layer works with.
    pub fn into_halves(
        self,
    ) -> (
        Box<dyn AsyncRead + Send + Unpin>,
        Box<dyn AsyncWrite + Send + Unpin>,
    ) {
        (self.reader, self.writer)
    }
}

/// What the transport itself established about the peer, before any credential
/// carried in the request envelope is examined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportAuth {
    /// Reaching this endpoint required being the owning user: a 0700 directory
    /// on Unix, an owner-only pipe DACL on Windows. No further credential adds
    /// anything, because the peer could equally run the command at a shell.
    OsVerifiedOwner,
    /// The transport proves nothing about the peer. A valid bearer token in
    /// the envelope is required before any command is dispatched.
    RequiresToken,
}

/// An accepted connection and what is known about its origin.
pub struct Accepted {
    pub stream: IpcStream,
    pub transport_auth: TransportAuth,
    /// The endpoint that accepted it, for logs and audit records.
    pub endpoint: Endpoint,
}

/// Options that apply to binding a listener.
#[derive(Debug, Clone, Default)]
pub struct BindOptions {
    /// Permit binding a TCP listener to a non-loopback address.
    ///
    /// Set from configuration. It does not make remote access work — the bind
    /// is still refused — but it changes the refusal to name the missing
    /// security controls rather than the missing option.
    pub allow_remote: bool,
}

/// A bound listener, whatever carries it.
pub enum IpcListener {
    #[cfg(unix)]
    Unix(unix::UnixListenerTransport),
    #[cfg(windows)]
    NamedPipe(windows::NamedPipeListenerTransport),
    Tcp(tcp::TcpListenerTransport),
}

impl IpcListener {
    /// Bind the given endpoint.
    ///
    /// Refuses rather than steals: a local endpoint already owned by a live
    /// instance makes this fail, so a daemon started next to a running GUI
    /// does not silently capture its clients.
    pub async fn bind(endpoint: &Endpoint, options: &BindOptions) -> io::Result<Self> {
        match endpoint {
            #[cfg(unix)]
            Endpoint::Unix { path } => {
                Ok(Self::Unix(unix::UnixListenerTransport::bind(path).await?))
            }
            #[cfg(not(unix))]
            Endpoint::Unix { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unix domain sockets are not available on this platform; \
                 use a named pipe or loopback TCP",
            )),

            #[cfg(windows)]
            Endpoint::NamedPipe { name } => Ok(Self::NamedPipe(
                windows::NamedPipeListenerTransport::bind(name)?,
            )),
            #[cfg(not(windows))]
            Endpoint::NamedPipe { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "named pipes are only available on Windows; use a Unix domain \
                 socket or loopback TCP",
            )),

            Endpoint::Tcp { addr } => Ok(Self::Tcp(
                tcp::TcpListenerTransport::bind(*addr, options.allow_remote).await?,
            )),
        }
    }

    /// Accept the next connection.
    pub async fn accept(&mut self) -> io::Result<Accepted> {
        match self {
            #[cfg(unix)]
            Self::Unix(l) => l.accept().await,
            #[cfg(windows)]
            Self::NamedPipe(l) => l.accept().await,
            Self::Tcp(l) => l.accept().await,
        }
    }

    /// The endpoint actually bound. For TCP with port 0 this reports the port
    /// the OS chose, which is what tests and diagnostics need.
    pub fn endpoint(&self) -> Endpoint {
        match self {
            #[cfg(unix)]
            Self::Unix(l) => l.endpoint(),
            #[cfg(windows)]
            Self::NamedPipe(l) => l.endpoint(),
            Self::Tcp(l) => l.endpoint(),
        }
    }
}

/// Connect to an endpoint as a client.
pub async fn connect(endpoint: &Endpoint) -> io::Result<IpcStream> {
    match endpoint {
        #[cfg(unix)]
        Endpoint::Unix { path } => unix::connect(path).await,
        #[cfg(not(unix))]
        Endpoint::Unix { .. } => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix domain sockets are not available on this platform",
        )),

        #[cfg(windows)]
        Endpoint::NamedPipe { name } => windows::connect(name).await,
        #[cfg(not(windows))]
        Endpoint::NamedPipe { .. } => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "named pipes are only available on Windows",
        )),

        Endpoint::Tcp { addr } => tcp::connect(*addr).await,
    }
}

/// Whether something is already listening on this endpoint.
///
/// Connecting is the only reliable liveness test: a bound socket accepts, an
/// orphaned inode refuses. Used both to refuse stealing an endpoint and to
/// decide whether a leftover file may be removed.
pub async fn is_endpoint_live(endpoint: &Endpoint) -> bool {
    connect(endpoint).await.is_ok()
}
