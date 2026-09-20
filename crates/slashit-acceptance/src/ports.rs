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
}
