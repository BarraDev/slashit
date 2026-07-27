//! Where the control channel lives, per platform.
//!
//! This module is the single source of truth for the runtime directory and
//! the default endpoint. `AppPaths` in the desktop crate defers to
//! [`runtime_dir`] rather than deriving its own, because the two disagreeing
//! is not a cosmetic problem: the server binds one path while the PID file and
//! any diagnostics point at another.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

/// Directory holding runtime state: the Unix socket and the PID file.
///
/// `$XDG_RUNTIME_DIR` is already per-user and mode 0700. When it is unset the
/// fallback is `<tmp>/slashit-<uid>` rather than a fixed name directly in the
/// temp directory: `/tmp` is world-writable and shared between users, so a
/// predictable unqualified path there can be pre-created or squatted by
/// another local user. The caller is responsible for creating this directory
/// with owner-only permissions before binding.
///
/// macOS and Windows have no `XDG_RUNTIME_DIR`, so the qualified temp fallback
/// is the normal path there, not an edge case.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join(crate::APPLICATION);
        }
    }
    std::env::temp_dir().join(format!("slashit-{}", current_user_id()))
}

/// Default Unix socket path.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("slashit.sock")
}

/// Default Windows named pipe name.
///
/// Named pipes live in a machine-global namespace, so the name is qualified
/// with the user to keep two accounts on the same machine from colliding.
pub fn pipe_name() -> String {
    format!(r"\\.\pipe\slashit-{}", current_user_id())
}

/// Where the control channel is reachable.
///
/// Local variants are access-controlled by the operating system: a Unix socket
/// by the 0700 directory that contains it, a named pipe by its security
/// descriptor. TCP is not, which is why it carries a token and is off by
/// default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Endpoint {
    /// Unix domain socket. Linux and macOS.
    Unix { path: PathBuf },
    /// Windows named pipe.
    NamedPipe { name: String },
    /// TCP. Requires a bearer token; loopback unless explicitly configured.
    Tcp { addr: SocketAddr },
}

impl Endpoint {
    /// The platform-native local endpoint.
    pub fn local_default() -> Self {
        #[cfg(windows)]
        {
            Self::NamedPipe { name: pipe_name() }
        }
        #[cfg(not(windows))]
        {
            Self::Unix {
                path: socket_path(),
            }
        }
    }

    /// Whether the operating system, rather than a credential, decides who may
    /// connect.
    ///
    /// This drives authorization: a peer that arrived over an OS-controlled
    /// local endpoint is already the owning user and may do anything the user
    /// could do at a shell. A TCP peer is not.
    pub fn is_os_authenticated(&self) -> bool {
        match self {
            Self::Unix { .. } | Self::NamedPipe { .. } => true,
            Self::Tcp { .. } => false,
        }
    }

    /// Whether this endpoint is reachable from another machine.
    pub fn is_remote_capable(&self) -> bool {
        match self {
            Self::Unix { .. } | Self::NamedPipe { .. } => false,
            Self::Tcp { addr } => !addr.ip().is_loopback(),
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix { path } => write!(f, "unix:{}", path.display()),
            Self::NamedPipe { name } => write!(f, "pipe:{name}"),
            Self::Tcp { addr } => write!(f, "tcp:{addr}"),
        }
    }
}

/// Why a requested listener was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointRejection {
    /// A non-loopback bind without the explicit opt-in.
    RemoteNotPermitted { addr: IpAddr },
    /// A non-loopback bind with the opt-in but without the controls that make
    /// it safe. See `docs/architecture/ipc-security.md`.
    RemoteNotImplemented { addr: IpAddr },
}

impl fmt::Display for EndpointRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RemoteNotPermitted { addr } => write!(
                f,
                "refusing to bind IPC to {addr}: it is reachable from other machines. \
                 Binding off-loopback requires setting `tcp.allow_remote = true` in the \
                 IPC configuration, and that option is itself gated — see \
                 docs/architecture/ipc-security.md"
            ),
            Self::RemoteNotImplemented { addr } => write!(
                f,
                "refusing to bind IPC to {addr}: remote access is not implemented. \
                 The IPC command set can spawn an agent with full tool access, so a \
                 network listener requires mutual TLS, per-identity authorization, \
                 rate limiting and an audit log first. See \
                 docs/architecture/ipc-security.md"
            ),
        }
    }
}

impl std::error::Error for EndpointRejection {}

/// Reject any bind address that would be reachable from another machine.
///
/// Called before every TCP bind. The check is on the resolved address rather
/// than on a configuration string, so `0.0.0.0`, `::`, and a concrete LAN
/// address are all caught by the same rule.
pub fn check_bind_address(addr: SocketAddr, allow_remote: bool) -> Result<(), EndpointRejection> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    if allow_remote {
        // The opt-in exists so the refusal can name the missing controls
        // instead of pretending the option does not exist.
        return Err(EndpointRejection::RemoteNotImplemented { addr: addr.ip() });
    }
    Err(EndpointRejection::RemoteNotPermitted { addr: addr.ip() })
}

#[cfg(unix)]
fn current_user_id() -> String {
    // Safety: getuid is always successful and has no preconditions.
    unsafe { libc::getuid() }.to_string()
}

#[cfg(windows)]
fn current_user_id() -> String {
    // Named pipes are machine-global; the username is what separates two
    // accounts on the same host. USERNAME is set for interactive and service
    // logons alike; the fallback keeps the path total rather than panicking.
    std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string())
}

#[cfg(not(any(unix, windows)))]
fn current_user_id() -> String {
    "default".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn loopback_binds_are_allowed() {
        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, 8731));
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, 8731));
        assert!(check_bind_address(v4, false).is_ok());
        assert!(check_bind_address(v6, false).is_ok());
    }

    #[test]
    fn wildcard_bind_is_refused_even_with_the_opt_in() {
        let any = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 8731));
        assert_eq!(
            check_bind_address(any, false),
            Err(EndpointRejection::RemoteNotPermitted {
                addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            })
        );
        // With the opt-in the answer is still a refusal, but it names the
        // missing controls rather than the missing option.
        assert_eq!(
            check_bind_address(any, true),
            Err(EndpointRejection::RemoteNotImplemented {
                addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            })
        );
    }

    #[test]
    fn ipv6_wildcard_is_refused() {
        let any = SocketAddr::from((Ipv6Addr::UNSPECIFIED, 8731));
        assert!(check_bind_address(any, false).is_err());
        assert!(check_bind_address(any, true).is_err());
    }

    #[test]
    fn a_concrete_lan_address_is_refused() {
        let lan = SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 8731));
        assert!(check_bind_address(lan, false).is_err());
        assert!(check_bind_address(lan, true).is_err());
    }

    #[test]
    fn local_endpoints_are_os_authenticated_and_tcp_is_not() {
        assert!(Endpoint::Unix {
            path: PathBuf::from("/tmp/x.sock")
        }
        .is_os_authenticated());
        assert!(Endpoint::NamedPipe {
            name: r"\\.\pipe\x".into()
        }
        .is_os_authenticated());
        assert!(!Endpoint::Tcp {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 1))
        }
        .is_os_authenticated());
    }

    #[test]
    fn loopback_tcp_is_not_remote_capable() {
        assert!(!Endpoint::Tcp {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 1))
        }
        .is_remote_capable());
        assert!(Endpoint::Tcp {
            addr: SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 1))
        }
        .is_remote_capable());
    }

    #[test]
    fn the_temp_fallback_is_qualified_per_user() {
        // Guards the squatting hazard: a fixed name in world-writable /tmp can
        // be pre-created by another local user.
        let previous = std::env::var("XDG_RUNTIME_DIR").ok();
        // SAFETY: single-threaded test; restored below.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "") };

        let dir = runtime_dir();
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("slashit-") && name.len() > "slashit-".len(),
            "temp fallback must be user-qualified, got {name}"
        );

        match previous {
            // SAFETY: single-threaded test.
            Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
    }

    #[test]
    fn endpoints_display_with_their_scheme() {
        assert_eq!(
            Endpoint::Unix {
                path: PathBuf::from("/run/user/1000/slashit-app/slashit.sock")
            }
            .to_string(),
            "unix:/run/user/1000/slashit-app/slashit.sock"
        );
        assert_eq!(
            Endpoint::Tcp {
                addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 8731))
            }
            .to_string(),
            "tcp:127.0.0.1:8731"
        );
    }
}
