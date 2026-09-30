//! A stand-in for `WebKitWebDriver`, used only by the readiness regression in
//! `tests/acceptance.rs`.
//!
//! `tauri-driver` spawns the native driver with exactly `--port=<N>
//! --host=<H>` ([`tauri-driver`'s `webdriver::native`][native]) and does not
//! wait for it to become ready before it starts forwarding requests to that
//! port. This binary models that native driver at the one point in its
//! lifecycle the regression cares about: the gap between being spawned and
//! actually listening.
//!
//! [native]: https://github.com/tauri-apps/tauri/blob/tauri-driver-v2.1.0/crates/tauri-driver/src/webdriver.rs
//!
//! ```text
//! fake-native-driver --port=4445 --host=127.0.0.1
//! ```
//!
//! Environment:
//! - `FAKE_NATIVE_DRIVER_DELAY_MS`: milliseconds to sleep *before* binding
//!   the port, i.e. before this process looks alive to anything checking
//!   whether the port is open. Defaults to 0.
//! - `FAKE_NATIVE_DRIVER_REJECT_SESSION`: if set, `POST /session` always
//!   returns a real (non-transient) WebDriver protocol error instead of
//!   creating a session, and the port never stops accepting.
//! - `FAKE_NATIVE_DRIVER_NOT_READY_POLLS`: how many `GET /status` requests to
//!   answer with a well-formed, successful `{"value":{"ready":false,...}}`
//!   before switching (permanently) to `ready:true`. Defaults to 0, i.e.
//!   ready from the first poll. Models the other half of the readiness
//!   contract: a process that is listening and answering 2xx, but that has
//!   not yet said it can accept a session.
//!
//! Protocol coverage is deliberately minimal: `GET /status` (the readiness
//! endpoint this regression exists to wait for), `POST /session` (the one
//! call under test), and a generic success envelope for whatever the client
//! sends next (`SetTimeouts`, `DeleteSession`, ...). Nothing here is a real
//! WebDriver implementation.

use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How many `GET /status` requests have been answered so far, across every
/// connection this process serves. `/status` polls are always sequential --
/// one client, one poll at a time -- so a plain counter is enough to model
/// "not ready for the first N polls, then ready".
static STATUS_POLLS: AtomicUsize = AtomicUsize::new(0);

fn main() -> ExitCode {
    let mut port: Option<u16> = None;
    let mut host = String::from("127.0.0.1");
    for arg in env::args().skip(1) {
        if let Some(value) = arg.strip_prefix("--port=") {
            port = value.parse().ok();
        } else if let Some(value) = arg.strip_prefix("--host=") {
            host = value.to_string();
        }
    }
    let Some(port) = port else {
        eprintln!("fake-native-driver: --port=<N> is required");
        return ExitCode::FAILURE;
    };

    let delay_ms: u64 = env::var("FAKE_NATIVE_DRIVER_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let reject_session = env::var_os("FAKE_NATIVE_DRIVER_REJECT_SESSION").is_some();
    let not_ready_polls: usize = env::var("FAKE_NATIVE_DRIVER_NOT_READY_POLLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // The exact condition under test: this process exists (it is past
    // argument parsing and could already have been waited on by a process
    // table check) but is not yet listening.
    std::thread::sleep(Duration::from_millis(delay_ms));

    let listener = match TcpListener::bind((host.as_str(), port)) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("fake-native-driver: could not bind {host}:{port}: {e}");
            return ExitCode::FAILURE;
        }
    };

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        serve_connection(stream, reject_session, not_ready_polls);
    }
    ExitCode::SUCCESS
}

/// Serve every request on one keep-alive connection until the client closes
/// it. Every `POST /session` this process ever answers is printed to stderr
/// (the provider log), so a test that cares whether one was retried can grep
/// for it instead of trusting an in-process count the harness never sees.
/// Every `GET /status` poll's outcome is printed the same way, so a test can
/// prove the order readiness was observed in relative to `POST /session`.
fn serve_connection(mut stream: TcpStream, reject_session: bool, not_ready_polls: usize) {
    let peer = stream.try_clone().expect("clone stream for reading");
    let mut reader = BufReader::new(peer);

    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return; // client closed the connection
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();

        let mut content_length = 0usize;
        let mut keep_alive = true;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break; // end of headers
            }
            if let Some(value) = header
                .to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::trim)
                .and_then(|v| v.parse().ok())
            {
                content_length = value;
            }
            if header.to_ascii_lowercase().starts_with("connection:")
                && header.to_ascii_lowercase().contains("close")
            {
                keep_alive = false;
            }
        }
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }

        let result = match (method.as_str(), path.as_str()) {
            ("GET", "/status") => {
                let poll = STATUS_POLLS.fetch_add(1, Ordering::SeqCst);
                let ready = poll >= not_ready_polls;
                eprintln!("fake-native-driver: GET /status ready={ready}");
                let body = format!(r#"{{"value":{{"ready":{ready},"message":"fake"}}}}"#);
                respond(&mut stream, 200, &body)
            }
            ("POST", "/session") => {
                eprintln!("fake-native-driver: POST /session");
                if reject_session {
                    respond(
                        &mut stream,
                        400,
                        r#"{"value":{"error":"invalid argument","message":"fake-native-driver: session rejected"}}"#,
                    )
                } else {
                    respond(
                        &mut stream,
                        200,
                        r#"{"value":{"sessionId":"fake-session","capabilities":{}}}"#,
                    )
                }
            }
            // Whatever the client sends next against the session (SetTimeouts,
            // DeleteSession, ...): a generic success envelope is enough.
            _ => respond(&mut stream, 200, r#"{"value":null}"#),
        };
        if result.is_err() || !keep_alive {
            return;
        }
    }
}

fn respond(stream: &mut TcpStream, status: u16, json: &str) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         \r\n\
         {json}",
        len = json.len()
    )
}
