//! TCP transport. Off by default, loopback only, always authenticated.
//!
//! TCP exists because a containerised or remote daemon will eventually need
//! it. It is not a drop-in alternative to the local transports: nothing about
//! a TCP connection identifies the peer, so a bearer token is mandatory even
//! on loopback — any local process, including one running as a different user,
//! can connect to `127.0.0.1`.
//!
//! Binding off-loopback is refused. See [`crate::endpoint::check_bind_address`]
//! and `docs/architecture/ipc-security.md` for the controls that would have to
//! exist first.

use std::io;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};

use super::{Accepted, IpcStream, TransportAuth};
use crate::endpoint::{check_bind_address, Endpoint};

#[derive(Debug)]
pub struct TcpListenerTransport {
    listener: TcpListener,
}

impl TcpListenerTransport {
    pub async fn bind(addr: SocketAddr, allow_remote: bool) -> io::Result<Self> {
        // Checked before the bind, so a refused address is never briefly
        // listening.
        check_bind_address(addr, allow_remote)
            .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))?;

        let listener = TcpListener::bind(addr).await?;
        Ok(Self { listener })
    }

    pub async fn accept(&mut self) -> io::Result<Accepted> {
        let (stream, peer) = self.listener.accept().await?;

        // Defence in depth: the bind check should make this unreachable, but
        // a listener bound to a loopback interface must never serve a peer
        // that is not itself on loopback.
        if !peer.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refusing a non-loopback peer {peer}"),
            ));
        }

        // Disable Nagle: the protocol is a single small request and a single
        // small response, so waiting to coalesce only adds latency.
        stream.set_nodelay(true)?;

        Ok(Accepted {
            stream: IpcStream::new(stream),
            transport_auth: TransportAuth::RequiresToken,
            endpoint: self.endpoint(),
        })
    }

    pub fn endpoint(&self) -> Endpoint {
        // Report the resolved address: a port of 0 in the configuration means
        // the OS picked one, and diagnostics need the real value.
        match self.listener.local_addr() {
            Ok(addr) => Endpoint::Tcp { addr },
            Err(_) => Endpoint::Tcp {
                addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            },
        }
    }
}

pub async fn connect(addr: SocketAddr) -> io::Result<IpcStream> {
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    Ok(IpcStream::new(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn loopback_any_port() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
    }

    #[tokio::test]
    async fn loopback_binds_and_reports_its_resolved_port() {
        let listener = TcpListenerTransport::bind(loopback_any_port(), false)
            .await
            .unwrap();
        match listener.endpoint() {
            Endpoint::Tcp { addr } => {
                assert!(addr.ip().is_loopback());
                assert_ne!(addr.port(), 0, "port 0 must resolve to the OS choice");
            }
            other => panic!("expected a TCP endpoint, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_non_loopback_bind_is_refused() {
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
        let err = TcpListenerTransport::bind(addr, false)
            .await
            .expect_err("0.0.0.0 must be refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            err.to_string().contains("reachable from other machines"),
            "refusal should explain why: {err}"
        );
    }

    #[tokio::test]
    async fn a_non_loopback_bind_with_the_opt_in_is_still_refused() {
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
        let err = TcpListenerTransport::bind(addr, true)
            .await
            .expect_err("remote access is not implemented");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            err.to_string().contains("not implemented"),
            "the opt-in should change the message to name the missing controls: {err}"
        );
    }

    #[tokio::test]
    async fn a_tcp_peer_is_never_os_verified() {
        let mut listener = TcpListenerTransport::bind(loopback_any_port(), false)
            .await
            .unwrap();
        let Endpoint::Tcp { addr } = listener.endpoint() else {
            unreachable!()
        };

        let client = tokio::spawn(async move { connect(addr).await });
        let accepted = listener.accept().await.unwrap();

        assert_eq!(
            accepted.transport_auth,
            TransportAuth::RequiresToken,
            "TCP must always demand a credential, even on loopback"
        );
        client.await.unwrap().unwrap();
    }
}
