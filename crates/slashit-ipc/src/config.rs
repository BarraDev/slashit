//! Which listeners the instance opens, and where clients look.
//!
//! Stored as `ipc.toml` beside the other configuration. Kept out of
//! `config.toml` because the CLI has to read it without pulling in the whole
//! desktop configuration model.

use serde::{Deserialize, Serialize};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;

use crate::endpoint::Endpoint;

/// Default TCP port when the transport is enabled. Registered to nothing;
/// picked to sit outside the ephemeral range on Linux and macOS.
pub const DEFAULT_TCP_PORT: u16 = 8731;

/// Environment variable that overrides where a client connects.
///
/// Accepts the same forms `Endpoint` displays: `unix:/path`, `pipe:\\.\pipe\x`,
/// or `tcp:127.0.0.1:8731`. A bare path is treated as a Unix socket so the
/// common case stays short.
pub const ENDPOINT_ENV: &str = "SLASHIT_IPC_ENDPOINT";

/// Environment variable carrying the bearer token for TCP clients.
pub const TOKEN_ENV: &str = "SLASHIT_IPC_TOKEN";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IpcConfig {
    /// The platform-native local listener. Always on: it is how the CLI and
    /// the GUI talk to each other, and it is access-controlled by the OS.
    pub local_enabled: bool,

    pub tcp: TcpConfig,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            local_enabled: true,
            tcp: TcpConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TcpConfig {
    /// Off by default. The command set can spawn an agent, so a network
    /// listener is opt-in even on loopback.
    pub enabled: bool,

    /// Address to bind. Loopback unless `allow_remote` is set, and refused
    /// even then — the option exists so the refusal can name the missing
    /// controls rather than the missing option.
    pub address: IpAddr,

    pub port: u16,

    /// Permit attempting a non-loopback bind. See
    /// `docs/architecture/ipc-security.md`.
    pub allow_remote: bool,
}

impl Default for TcpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_TCP_PORT,
            allow_remote: false,
        }
    }
}

impl TcpConfig {
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.port)
    }
}

impl IpcConfig {
    /// Read configuration, falling back to defaults on any problem.
    ///
    /// Forgiving in the same way feature flags are: refusing to start because
    /// `ipc.toml` has a typo would be worse than starting with the safe
    /// defaults, and the defaults here are the *restrictive* ones — local
    /// only, TCP off.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("[ipc] ignoring {}: {e}. Using defaults.", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }

    /// Every endpoint this configuration asks the server to listen on.
    pub fn listen_endpoints(&self) -> Vec<Endpoint> {
        let mut endpoints = Vec::new();
        if self.local_enabled {
            endpoints.push(Endpoint::local_default());
        }
        if self.tcp.enabled {
            endpoints.push(Endpoint::Tcp {
                addr: self.tcp.socket_addr(),
            });
        }
        endpoints
    }
}

/// Parse an endpoint from its displayed form.
///
/// A bare path with no scheme is a Unix socket, because that is what a user
/// pasting a socket path means.
pub fn parse_endpoint(value: &str) -> Result<Endpoint, String> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("unix:") {
        return Ok(Endpoint::Unix { path: rest.into() });
    }
    if let Some(rest) = value.strip_prefix("pipe:") {
        return Ok(Endpoint::NamedPipe {
            name: rest.to_string(),
        });
    }
    if let Some(rest) = value.strip_prefix("tcp:") {
        let addr: SocketAddr = rest
            .parse()
            .map_err(|e| format!("`{rest}` is not a host:port address: {e}"))?;
        return Ok(Endpoint::Tcp { addr });
    }
    if value.starts_with(r"\\") {
        return Ok(Endpoint::NamedPipe {
            name: value.to_string(),
        });
    }
    if value.contains('/') || value.contains('\\') {
        return Ok(Endpoint::Unix { path: value.into() });
    }
    Err(format!(
        "`{value}` is not an endpoint. Use unix:/path, pipe:\\\\.\\pipe\\name, \
         or tcp:127.0.0.1:{DEFAULT_TCP_PORT}"
    ))
}

/// Where a client should connect, honouring the environment override.
pub fn client_endpoint() -> Result<Endpoint, String> {
    match std::env::var(ENDPOINT_ENV) {
        Ok(v) if !v.trim().is_empty() => parse_endpoint(&v),
        _ => Ok(Endpoint::local_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_are_local_only_with_tcp_off() {
        let cfg = IpcConfig::default();
        assert!(cfg.local_enabled);
        assert!(!cfg.tcp.enabled);
        assert!(!cfg.tcp.allow_remote);
        assert!(cfg.tcp.address.is_loopback());
        assert_eq!(cfg.listen_endpoints(), vec![Endpoint::local_default()]);
    }

    #[test]
    fn a_missing_file_yields_the_restrictive_defaults() {
        let tmp = TempDir::new().unwrap();
        let cfg = IpcConfig::load(&tmp.path().join("absent.toml"));
        assert_eq!(cfg, IpcConfig::default());
    }

    #[test]
    fn a_malformed_file_yields_the_restrictive_defaults() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ipc.toml");
        std::fs::write(&path, "not = = toml").unwrap();
        let cfg = IpcConfig::load(&path);
        assert!(!cfg.tcp.enabled, "a broken file must not enable a listener");
        assert_eq!(cfg, IpcConfig::default());
    }

    #[test]
    fn a_partial_file_keeps_the_defaults_for_absent_keys() {
        // Guards against a config that enables TCP also silently clearing
        // `allow_remote`'s default or the local listener.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ipc.toml");
        std::fs::write(&path, "[tcp]\nenabled = true\n").unwrap();

        let cfg = IpcConfig::load(&path);
        assert!(cfg.tcp.enabled);
        assert!(cfg.local_enabled, "local listener stays on");
        assert!(!cfg.tcp.allow_remote, "remote stays off");
        assert_eq!(cfg.tcp.port, DEFAULT_TCP_PORT);
    }

    #[test]
    fn config_roundtrips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ipc.toml");

        let mut cfg = IpcConfig::default();
        cfg.tcp.enabled = true;
        cfg.tcp.port = 9999;
        cfg.save(&path).unwrap();

        assert_eq!(IpcConfig::load(&path), cfg);
    }

    #[test]
    fn enabling_tcp_adds_a_second_endpoint() {
        let mut cfg = IpcConfig::default();
        cfg.tcp.enabled = true;
        let endpoints = cfg.listen_endpoints();
        assert_eq!(endpoints.len(), 2);
        assert!(endpoints.iter().any(|e| matches!(e, Endpoint::Tcp { .. })));
    }

    #[test]
    fn endpoints_parse_from_their_displayed_form() {
        let unix = Endpoint::Unix {
            path: "/run/x.sock".into(),
        };
        assert_eq!(parse_endpoint(&unix.to_string()).unwrap(), unix);

        let tcp = Endpoint::Tcp {
            addr: "127.0.0.1:8731".parse().unwrap(),
        };
        assert_eq!(parse_endpoint(&tcp.to_string()).unwrap(), tcp);

        let pipe = Endpoint::NamedPipe {
            name: r"\\.\pipe\slashit-1000".into(),
        };
        assert_eq!(parse_endpoint(&pipe.to_string()).unwrap(), pipe);
    }

    #[test]
    fn a_bare_path_is_a_unix_socket() {
        assert_eq!(
            parse_endpoint("/run/user/1000/slashit-app/slashit.sock").unwrap(),
            Endpoint::Unix {
                path: "/run/user/1000/slashit-app/slashit.sock".into()
            }
        );
    }

    #[test]
    fn a_bare_unc_path_is_a_named_pipe() {
        assert_eq!(
            parse_endpoint(r"\\.\pipe\slashit-rui").unwrap(),
            Endpoint::NamedPipe {
                name: r"\\.\pipe\slashit-rui".into()
            }
        );
    }

    #[test]
    fn nonsense_is_rejected_with_guidance() {
        let err = parse_endpoint("banana").unwrap_err();
        assert!(
            err.contains("unix:"),
            "error should show the accepted forms: {err}"
        );

        assert!(parse_endpoint("tcp:not-an-address").is_err());
    }
}
