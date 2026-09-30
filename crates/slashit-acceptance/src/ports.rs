//! Port allocation, and the preflight that makes a stale provider loud.
//!
//! Fixed ports are the reason a leftover WebDriver can silently hijack a run:
//! the new provider binds the client port, forwards to a process left over
//! from the previous run, and the application launches with *that* run's
//! environment — including its state root. Isolation then fails open, and the
//! run passes against the wrong state. Every port here is handed out by the
//! kernel instead, and every one is checked before use and after teardown.

use anyhow::{bail, Context, Result};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// A loopback port the kernel has confirmed is free, held open until the
/// child that wants it is about to be spawned.
///
/// Binding to port 0 and closing immediately leaves a window in which
/// something else can take the number. Keeping the listener alive until
/// [`ReservedPort::release`] narrows that window to the gap between two
/// statements.
pub struct ReservedPort {
    port: u16,
    listener: Option<TcpListener>,
}

impl ReservedPort {
    /// Ask the kernel for an unused loopback port and hold it.
    pub fn reserve() -> Result<Self> {
        let listener =
            TcpListener::bind("127.0.0.1:0").context("could not reserve a free loopback port")?;
        let port = listener
            .local_addr()
            .context("reserved socket has no local address")?
            .port();
        Ok(Self {
            port,
            listener: Some(listener),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Give the port up so the child process can bind it.
    pub fn release(&mut self) {
        self.listener = None;
    }
}

/// Refuse to continue if something is already holding `port`.
///
/// Called immediately after releasing a reservation and again after teardown.
/// The first call catches a racing binder; the second proves the provider
/// really let go rather than merely having been sent a signal.
///
/// The probe binds rather than connects. A loopback `connect` to an
/// unoccupied port in the ephemeral range can succeed against itself when the
/// kernel happens to pick the same number as the source port — TCP
/// simultaneous open — which would report a free port as busy. Binding asks
/// the question directly and has no such failure mode.
pub fn assert_free(port: u16, context: &str) -> Result<()> {
    if TcpListener::bind(("127.0.0.1", port)).is_err() {
        bail!(
            "port {port} is in use ({context}). Refusing to continue: a run that talks to \
             a process it did not start inherits that process's environment, and would \
             silently test the wrong application state."
        );
    }
    Ok(())
}

/// Block until `port` accepts a connection, or fail with a useful timeout.
pub async fn wait_until_listening(port: u16, timeout: Duration, what: &str) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "{what} never accepted a connection on 127.0.0.1:{port} within {}s",
        timeout.as_secs()
    )
}

/// Block until an HTTP GET on `port`/`path` returns a successful status, or
/// fail with a useful timeout.
///
/// This is not a generic HTTP client wait: it exists for exactly one
/// contract, `WebKitWebDriver`'s `GET /status` (the W3C readiness endpoint,
/// `{"value":{"ready":true,...}}` once the server can accept a session).
/// `tauri-driver` spawns that process and starts forwarding requests to it
/// immediately, without waiting for it to be listening
/// (`crates/tauri-driver/src/main.rs` upstream), so a connection refused here
/// is the expected, retryable shape of "not up yet" -- it is not evidence of
/// a permanent problem until the deadline passes. Any other failure while
/// reading the response (a reset mid-response, a malformed status line) is
/// treated the same way: retried, because during startup those are at least
/// as likely to be transient as a refused connection, and the deadline is
/// what turns "not ready yet" into a real failure either way.
pub async fn wait_until_http_ready(
    port: u16,
    path: &str,
    timeout: Duration,
    what: &str,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_error: Option<String> = None;
    while Instant::now() < deadline {
        match http_get_status(port, path).await {
            Ok(status) if (200..300).contains(&status) => return Ok(()),
            Ok(status) => last_error = Some(format!("HTTP {status}")),
            Err(e) => last_error = Some(e.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "{what} never answered {path} on 127.0.0.1:{port} with success within {}s{}",
        timeout.as_secs(),
        last_error
            .map(|e| format!(" (last attempt: {e})"))
            .unwrap_or_default()
    )
}

/// One GET, no retry, no connection reuse: the status code if a complete
/// HTTP response was read, or the error that stopped it (most commonly
/// "connection refused" while the server is still starting).
async fn http_get_status(port: u16, path: &str) -> Result<u16> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .context("connect")?;
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .context("write request")?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .await
        .context("read status line")?;

    // "HTTP/1.1 200 OK" -- the code is the second whitespace-separated field.
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .with_context(|| format!("not an HTTP status line: {status_line:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a reservation promises is that *this* process holds the port
    /// until it says otherwise, and then stops holding it. It cannot promise
    /// the number is still unused afterwards: releasing is precisely the act
    /// of giving it back to the machine, and anything -- another test in this
    /// binary, another job on the runner -- may legitimately take it in the
    /// gap before the next statement. Asserting the port reads as free after
    /// release tested the machine's luck rather than the harness, and hosted
    /// CI duly failed on it. So the held half is checked through the
    /// preflight, which is real behaviour, and the released half is checked
    /// against the reservation's own state, which is the part it owns.
    #[test]
    fn a_reservation_holds_its_port_until_it_is_released() {
        let mut reserved = ReservedPort::reserve().expect("reserve a port");
        let port = reserved.port();
        assert!(
            reserved.listener.is_some(),
            "a fresh reservation must be holding the socket it reserved"
        );

        // While held, the preflight must see it as taken -- that is exactly
        // the signal it exists to raise.
        assert!(
            assert_free(port, "held reservation").is_err(),
            "a held reservation must read as in use"
        );

        reserved.release();
        assert!(
            reserved.listener.is_none(),
            "release must drop the listener, because the child cannot bind a port we still hold"
        );
        assert_eq!(
            reserved.port(),
            port,
            "the number must survive release: it is what the child is told to use"
        );
    }

    /// The success path, which is deterministic precisely because the listener
    /// is held for the whole wait: nothing else can take the port while this
    /// test owns it, so the answer cannot depend on what else is running.
    #[tokio::test]
    async fn a_port_something_really_listens_on_is_recognised() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a listener");
        let port = listener.local_addr().expect("local address").port();

        wait_until_listening(port, Duration::from_secs(5), "a provider")
            .await
            .expect("a genuine listener must be reported as listening");
    }

    /// The success path for [`wait_until_http_ready`]: a real HTTP response
    /// with a 2xx status is recognised as readiness.
    #[tokio::test]
    async fn an_http_endpoint_answering_200_is_recognised() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a listener");
        let port = listener.local_addr().expect("local address").port();
        tokio::spawn(respond_once(listener, "HTTP/1.1 200 OK\r\n\r\n"));

        wait_until_http_ready(port, "/status", Duration::from_secs(5), "a native driver")
            .await
            .expect("a 200 response must be recognised as ready");
    }

    /// The one thing this wait exists for: a connection refused while the
    /// server has not started listening yet must be retried, not treated as
    /// the final answer, right up until it actually comes up.
    #[tokio::test]
    async fn a_late_http_endpoint_is_waited_for_rather_than_raced() {
        let port = {
            // Reserve a real port, then let it go: nothing listens on it
            // until the delayed task below binds it, which is exactly the
            // "spawned but not yet listening" gap this wait exists to cross.
            let probe = TcpListener::bind("127.0.0.1:0").expect("bind a probe listener");
            probe.local_addr().expect("local address").port()
        };

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let listener =
                tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind late");
            respond_once_tokio(listener, "HTTP/1.1 200 OK\r\n\r\n").await;
        });

        let started = Instant::now();
        wait_until_http_ready(port, "/status", Duration::from_secs(5), "a native driver")
            .await
            .expect("a server that starts late must still be waited for");
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "the wait returned before the server could plausibly have started: {:?}",
            started.elapsed()
        );
    }

    /// Respond to exactly one connection with `response`, from a std
    /// listener handed off to a blocking task (used where the listener must
    /// be bound before the task starts, to close the reservation gap).
    async fn respond_once(listener: TcpListener, response: &'static str) {
        tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().expect("accept a connection");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf); // drain the request; content unused
            let _ = stream.write_all(response.as_bytes());
        })
        .await
        .expect("responder task must not panic");
    }

    /// Same as [`respond_once`], for a listener that was bound with tokio.
    async fn respond_once_tokio(listener: tokio::net::TcpListener, response: &'static str) {
        use tokio::io::AsyncWriteExt;
        let (mut stream, _) = listener.accept().await.expect("accept a connection");
        let mut buf = [0u8; 1024];
        let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
        let _ = stream.write_all(response.as_bytes()).await;
    }

    /// When the wait runs out, its message is often the only evidence a CI log
    /// carries about what failed, so it has to name both the port and what was
    /// expected on it.
    ///
    /// The deadline is spent rather than merely short, and no port is touched
    /// at all. This test used to reserve a port, release it and then require
    /// nothing to answer on it -- the same assumption of ownership after
    /// release that made its sibling flaky, and for the same reason: another
    /// test in this binary may be handed that number the moment it is given
    /// back, and it holds a listener while it works. Measured at roughly two
    /// runs in twenty-five of the whole suite.
    #[tokio::test]
    async fn a_wait_that_runs_out_of_time_names_the_port_and_what_it_wanted() {
        let err = wait_until_listening(65000, Duration::ZERO, "a provider")
            .await
            .expect_err("a deadline that has already passed must fail");
        let message = err.to_string();
        assert!(message.contains("65000"), "message: {message}");
        assert!(message.contains("a provider"), "message: {message}");
    }

    /// Same proof as its `wait_until_listening` sibling above, for the HTTP
    /// variant: nothing ever answers, so the deadline is the only thing that
    /// can end the wait, and its message has to say enough to act on.
    #[tokio::test]
    async fn an_http_wait_that_runs_out_of_time_names_the_port_and_the_path() {
        let err = wait_until_http_ready(65000, "/status", Duration::ZERO, "a native driver")
            .await
            .expect_err("a deadline that has already passed must fail");
        let message = err.to_string();
        assert!(message.contains("65000"), "message: {message}");
        assert!(message.contains("/status"), "message: {message}");
        assert!(message.contains("a native driver"), "message: {message}");
    }
}
