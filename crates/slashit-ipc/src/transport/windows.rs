//! Windows named pipe transport.
//!
//! The security properties match the Unix socket rather than TCP:
//!
//! - `first_pipe_instance(true)` on the initial instance makes creation fail
//!   with `ERROR_ACCESS_DENIED` if another process already owns the name. That
//!   is the Windows analogue of the connect-first liveness check: an instance
//!   cannot silently take over another instance's clients.
//! - `reject_remote_clients(true)` refuses connections arriving over SMB, so
//!   the pipe is genuinely local rather than a network endpoint that happens
//!   to be reachable by UNC path.
//! - The default security descriptor grants the creating user and denies
//!   everyone else, which is what makes the peer OS-authenticated.
//!
//! A named pipe server handles one client per instance, so the listener keeps
//! one instance waiting and creates the replacement as soon as a client
//! arrives. Without that, a second client connecting during dispatch would be
//! refused rather than queued.

use std::io;
use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

use super::{Accepted, IpcStream, TransportAuth};
use crate::endpoint::Endpoint;

/// Windows error code for "all pipe instances are busy".
const ERROR_PIPE_BUSY: i32 = 231;

#[derive(Debug)]
pub struct NamedPipeListenerTransport {
    name: String,
    /// The instance currently waiting for a client.
    pending: tokio::net::windows::named_pipe::NamedPipeServer,
}

impl NamedPipeListenerTransport {
    pub fn bind(name: &str) -> io::Result<Self> {
        let pending = ServerOptions::new()
            // Refuse to start if another instance already owns the name,
            // rather than joining it and stealing half its connections.
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(name)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::PermissionDenied {
                    io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("another SlashIt instance is already listening on {name}"),
                    )
                } else {
                    e
                }
            })?;

        Ok(Self {
            name: name.to_string(),
            pending,
        })
    }

    pub async fn accept(&mut self) -> io::Result<Accepted> {
        // Create the replacement *before* connecting `self.pending`, not
        // after. If replacement creation failed after a successful connect,
        // `self.pending` would be left connected but never handed to a
        // caller; the next `accept()`'s own `connect()` call on that same
        // instance then returns `ERROR_PIPE_CONNECTED` immediately without
        // ever delivering that client, and repeated failures here can run out
        // the accept loop's retry budget. Creating first means a failure
        // leaves `self.pending` exactly as it was, a clean state to retry
        // `accept()` again — and the next client is still never refused.
        let next = ServerOptions::new()
            .reject_remote_clients(true)
            .create(&self.name)?;

        self.pending.connect().await?;

        let connected = std::mem::replace(&mut self.pending, next);

        Ok(Accepted {
            stream: IpcStream::new(connected),
            transport_auth: TransportAuth::OsVerifiedOwner,
            endpoint: self.endpoint(),
        })
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::NamedPipe {
            name: self.name.clone(),
        }
    }
}

pub async fn connect(name: &str) -> io::Result<IpcStream> {
    // A pipe with every instance busy is transient, not a failure: the server
    // creates the replacement instance immediately after accepting. Retry
    // briefly rather than reporting the app as not running.
    let deadline = std::time::Duration::from_secs(2);
    let started = std::time::Instant::now();

    loop {
        match ClientOptions::new().open(name) {
            Ok(client) => return Ok(IpcStream::new(client)),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if started.elapsed() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("all instances of {name} stayed busy for {deadline:?}"),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::pipe_name;

    fn unique_name(tag: &str) -> String {
        format!("{}-test-{}-{}", pipe_name(), tag, std::process::id())
    }

    #[tokio::test]
    async fn a_live_owner_cannot_be_displaced() {
        let name = unique_name("owner");
        let _first = NamedPipeListenerTransport::bind(&name).unwrap();

        let second = NamedPipeListenerTransport::bind(&name);
        let err = second.expect_err("a second bind must be refused");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn a_local_peer_is_os_verified() {
        let name = unique_name("peer");
        let mut listener = NamedPipeListenerTransport::bind(&name).unwrap();

        let client = tokio::spawn({
            let name = name.clone();
            async move { connect(&name).await }
        });

        let accepted = listener.accept().await.unwrap();
        assert_eq!(accepted.transport_auth, TransportAuth::OsVerifiedOwner);
        assert_eq!(accepted.endpoint, Endpoint::NamedPipe { name });

        client.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_second_client_is_served_rather_than_refused() {
        // Regression guard for the one-instance-per-server pitfall: without
        // creating the replacement instance on accept, this second connect
        // fails with FILE_NOT_FOUND.
        let name = unique_name("queue");
        let mut listener = NamedPipeListenerTransport::bind(&name).unwrap();

        let first = tokio::spawn({
            let name = name.clone();
            async move { connect(&name).await }
        });
        let _accepted = listener.accept().await.unwrap();
        first.await.unwrap().unwrap();

        let second = tokio::spawn({
            let name = name.clone();
            async move { connect(&name).await }
        });
        let accepted = listener.accept().await.unwrap();
        assert_eq!(accepted.transport_auth, TransportAuth::OsVerifiedOwner);
        second.await.unwrap().unwrap();
    }
}
