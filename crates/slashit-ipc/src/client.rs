//! The client half of the round trip: connect, send one request, read one
//! response.
//!
//! Lives here rather than in the CLI so the daemon supervisor, the CLI and any
//! test can share the same connection semantics and, importantly, the same
//! diagnostics. A client that cannot connect should say why in terms the user
//! can act on.

use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

use crate::config::{client_endpoint, TOKEN_ENV};
use crate::endpoint::Endpoint;
use crate::framing::{read_frame, write_json_frame, Frame};
use crate::protocol::{IpcEnvelope, IpcRequest, IpcResponse, MAX_REQUEST_BYTES};
use crate::transport;

/// How long to wait for the instance to answer a request it has already
/// accepted. Every current request handler is expected to return in well
/// under a second — agent spawning happens on the queue executor's own
/// background polling loop, decoupled from the request/response cycle, so a
/// handler that runs long here is a bug, not routine slow work. This mirrors
/// the server's own `REQUEST_TIMEOUT`, keeping both ends of the protocol
/// symmetric.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything that can go wrong on the client side.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    NotRunning(String),
    #[error("{0}")]
    Connect(String),
    #[error("SlashIt accepted the connection on {0} but did not answer in time")]
    NoResponse(Endpoint),
    #[error("SlashIt closed the connection without answering — it may have crashed")]
    EmptyResponse,
    #[error("SlashIt sent a response larger than the {MAX_REQUEST_BYTES} byte limit")]
    ResponseTooLarge,
    #[error("could not understand SlashIt's response: {0}")]
    Decode(String),
    #[error("a bearer token is required for {0}. Set {TOKEN_ENV}, or use the local endpoint")]
    TokenRequired(Endpoint),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// How to reach the instance.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub endpoint: Endpoint,
    /// Bearer token. Required for TCP, ignored by the local transports.
    pub token: Option<String>,
    /// Poll for the endpoint to appear rather than failing immediately.
    pub wait: Duration,
}

impl ClientOptions {
    /// Resolve from the environment: `SLASHIT_IPC_ENDPOINT`, `SLASHIT_IPC_TOKEN`,
    /// otherwise the platform-native local endpoint and no token.
    pub fn from_env() -> Result<Self, ClientError> {
        let endpoint = client_endpoint().map_err(ClientError::Connect)?;
        let token = std::env::var(TOKEN_ENV)
            .ok()
            .filter(|t| !t.trim().is_empty());
        Ok(Self {
            endpoint,
            token,
            wait: Duration::ZERO,
        })
    }

    pub fn with_wait(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }
}

/// Send one request and return the response.
pub async fn send(
    request: &IpcRequest,
    options: &ClientOptions,
) -> Result<IpcResponse, ClientError> {
    // Fail before connecting when the credential is structurally missing, so
    // the message names the cause rather than reporting an auth error from
    // the far end.
    if !options.endpoint.is_os_authenticated() && options.token.is_none() {
        return Err(ClientError::TokenRequired(options.endpoint.clone()));
    }

    let stream = if options.wait.is_zero() {
        transport::connect(&options.endpoint)
            .await
            .map_err(|e| describe_connect_failure(&options.endpoint, e))?
    } else {
        wait_for_endpoint(&options.endpoint, options.wait).await?
    };

    let envelope = match &options.token {
        Some(token) => IpcEnvelope::with_auth(request.clone(), token.clone()),
        None => IpcEnvelope::new(request.clone()),
    };

    let (reader, mut writer) = stream.into_halves();
    write_json_frame(&mut writer, &envelope).await?;
    // Half-close so the server's read completes without waiting for a further
    // newline. The response arrives on the read half, which stays open.
    writer.shutdown().await?;

    let frame = tokio::time::timeout(RESPONSE_TIMEOUT, read_frame(reader, MAX_REQUEST_BYTES))
        .await
        .map_err(|_| ClientError::NoResponse(options.endpoint.clone()))??;
    match frame {
        Frame::Line(line) => {
            serde_json::from_str(&line).map_err(|e| ClientError::Decode(e.to_string()))
        }
        Frame::Empty => Err(ClientError::EmptyResponse),
        Frame::TooLarge => Err(ClientError::ResponseTooLarge),
    }
}

/// Poll until the endpoint accepts a connection or the budget runs out.
async fn wait_for_endpoint(
    endpoint: &Endpoint,
    budget: Duration,
) -> Result<crate::IpcStream, ClientError> {
    let started = Instant::now();
    let poll = Duration::from_millis(250);
    let mut announced = false;

    loop {
        match transport::connect(endpoint).await {
            Ok(stream) => {
                if announced {
                    eprintln!("Connected.");
                }
                return Ok(stream);
            }
            Err(e) => {
                if started.elapsed() >= budget {
                    return Err(ClientError::NotRunning(format!(
                        "timed out after {}s waiting for SlashIt on {endpoint} ({e})",
                        budget.as_secs()
                    )));
                }
                if !announced {
                    eprintln!(
                        "Waiting for SlashIt on {endpoint} (timeout: {}s)...",
                        budget.as_secs()
                    );
                    announced = true;
                }
                tokio::time::sleep(poll).await;
            }
        }
    }
}

/// Turn a connect failure into something the user can act on.
fn describe_connect_failure(endpoint: &Endpoint, e: std::io::Error) -> ClientError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => ClientError::NotRunning(format!(
            "SlashIt is not running (nothing at {endpoint}).\n\
             Start the app or the daemon first, or pass --wait."
        )),
        ErrorKind::ConnectionRefused => ClientError::NotRunning(format!(
            "{endpoint} exists but refused the connection — SlashIt may be shutting down.\n\
             Try again, or pass --wait."
        )),
        ErrorKind::PermissionDenied => ClientError::Connect(format!(
            "permission denied connecting to {endpoint}.\n\
             The endpoint may belong to a different user."
        )),
        _ => ClientError::Connect(format!("cannot connect to {endpoint}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    #[tokio::test]
    async fn a_tcp_request_without_a_token_fails_before_connecting() {
        // No listener exists on this address. If the check were done after
        // connecting, the error would be "not running" instead.
        let options = ClientOptions {
            endpoint: Endpoint::Tcp {
                addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 1)),
            },
            token: None,
            wait: Duration::ZERO,
        };

        let err = send(&IpcRequest::Status, &options).await.unwrap_err();
        assert!(
            matches!(err, ClientError::TokenRequired(_)),
            "expected a token error, got {err}"
        );
    }

    // `Endpoint::Unix` is OS-authenticated, so `send` reaches
    // `transport::connect` unconditionally. On a non-Unix target that returns
    // `ErrorKind::Unsupported`, which `describe_connect_failure` maps to
    // `ClientError::Connect` rather than `NotRunning` — a real answer for that
    // platform, just not the one this test expects for a genuinely-absent
    // endpoint.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_missing_local_endpoint_reports_not_running() {
        let options = ClientOptions {
            endpoint: Endpoint::Unix {
                path: "/nonexistent/slashit-test/absent.sock".into(),
            },
            token: None,
            wait: Duration::ZERO,
        };

        let err = send(&IpcRequest::Status, &options).await.unwrap_err();
        match err {
            ClientError::NotRunning(msg) => assert!(msg.contains("not running"), "{msg}"),
            other => panic!("expected NotRunning, got {other}"),
        }
    }

    // `Endpoint::Unix` is OS-authenticated, so `send` reaches
    // `transport::connect` unconditionally. On a non-Unix target that returns
    // `ErrorKind::Unsupported`, which `describe_connect_failure` maps to
    // `ClientError::Connect`, not the `NotRunning` timeout message this test
    // expects — see the identical reasoning on
    // `a_missing_local_endpoint_reports_not_running` above.
    #[cfg(unix)]
    #[tokio::test]
    async fn waiting_gives_up_with_a_timeout_message() {
        let options = ClientOptions {
            endpoint: Endpoint::Unix {
                path: "/nonexistent/slashit-test/absent.sock".into(),
            },
            token: None,
            wait: Duration::from_millis(300),
        };

        let err = send(&IpcRequest::Status, &options).await.unwrap_err();
        match err {
            ClientError::NotRunning(msg) => assert!(msg.contains("timed out"), "{msg}"),
            other => panic!("expected a timeout, got {other}"),
        }
    }
}
