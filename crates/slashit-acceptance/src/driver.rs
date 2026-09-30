//! The Linux WebDriver provider and one live application session.
//!
//! The chain is `thirtyfour -> tauri-driver -> WebKitWebDriver -> SlashIt`.
//! Only the first hop is ours: `tauri-driver` starts the native driver, which
//! starts the application, so the harness owns exactly one process — and,
//! through its process group, everything below it.
//!
//! Readiness is never a sleep. Each stage waits on the condition that stage
//! actually establishes: the provider accepting connections, the native
//! driver reporting itself ready, the W3C session existing, and the Leptos
//! root having mounted.
//!
//! The second of those exists because `tauri-driver` does not wait for its
//! own point: it spawns `WebKitWebDriver` and immediately starts forwarding
//! requests to it (`crates/tauri-driver/src/main.rs` upstream, both in the
//! pinned `^2` line and at `2.1.0`, unchanged in that respect since `2.0.0`).
//! A request that arrives before the native driver is listening gets
//! forwarded into a refused connection, which `tauri-driver` turns into a
//! broken connection back to us -- this is exactly the failure a hosted run
//! hit once, session creation itself, before this wait existed. Waiting on
//! [`ports::wait_until_http_ready`] against `WebKitWebDriver`'s own `/status`
//! turns that race into an ordinary, bounded wait.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use thirtyfour::prelude::*;

use crate::context::Environment;
use crate::developer_tools;
use crate::ports::{self, ReservedPort};
use crate::process::OwnedProcess;
use crate::state::StateRoot;
use crate::{DEV_URL_PREFIX, ROOT_ELEMENT};

// The three constants below are not independent per-stage caps: they only
// name how much each stage would get in the ordinary case where the ones
// before it were fast. [`Session::create`] adds them into one deadline,
// computed once, and every stage gets whatever of it the earlier stages
// left -- so the worst case this crate will ever wait for a session, in any
// order of slowness, is bounded by their sum (150s today), never by one of
// them multiplied by a retry count. A slow first stage can leave a later one
// far less than its own constant would suggest; that is intentional, not a
// bug to fix by giving a stage its own fresh timer.

/// How long `tauri-driver` gets to bind its client port, in the ordinary
/// case.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `WebKitWebDriver` gets to report itself ready over `/status`,
/// once `tauri-driver`'s own client port is already open, in the ordinary
/// case.
const NATIVE_DRIVER_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the actual W3C `New Session` call gets, once the native driver
/// is ready, in the ordinary case.
const SESSION_TIMEOUT: Duration = Duration::from_secs(90);
/// How long the Leptos application gets to mount its root element.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(60);
/// Polling interval for element readiness.
const POLL: Duration = Duration::from_millis(200);

/// One running application plus the WebDriver session driving it.
pub struct Session {
    driver: WebDriver,
    provider: Provider,
    log_path: PathBuf,
}

impl Session {
    /// Start the provider, launch the application against `state`, and return
    /// only once the frontend has mounted.
    pub async fn start(
        env: &Environment,
        state: &StateRoot,
        log_path: PathBuf,
        child_env: &[(OsString, OsString)],
    ) -> Result<Self> {
        let started = Self::create(env, state, &log_path, child_env).await;
        let (driver, provider) = started.map_err(|e| {
            // The provider log is the only place the native driver's own
            // complaint appears, so name it in the failure.
            e.context(format!(
                "provider log: {} (application: {})",
                log_path.display(),
                env.app_binary().display()
            ))
        })?;

        let session = Self {
            driver,
            provider,
            log_path,
        };

        session.assert_custom_protocol_build().await?;
        session.wait_until_mounted().await?;
        Ok(session)
    }

    async fn create(
        env: &Environment,
        state: &StateRoot,
        log_path: &Path,
        child_env: &[(OsString, OsString)],
    ) -> Result<(WebDriver, Provider)> {
        // One deadline for the whole "get a session" step, shared across the
        // three waits inside it. See the constants above for why: without a
        // shared deadline, a retry or an extra wait multiplies the total
        // instead of bounding it.
        let deadline = Instant::now() + PROVIDER_TIMEOUT + NATIVE_DRIVER_TIMEOUT + SESSION_TIMEOUT;

        let mut provider = Provider::spawn(env.native_driver(), log_path, deadline, |command| {
            // The journey's own variables go on first so the state root's go
            // on last and win. Isolation is not something a test should be
            // able to switch off by asking for one more variable.
            for (key, value) in child_env {
                command.env(key, value);
            }
            // The application is spawned by the native driver, which inherits
            // this environment. Setting it here is what keeps the run off the
            // developer's real SlashIt state.
            state.apply_to(command);
            // Last, for the same reason: whatever PATH the journey asked for,
            // the application must not find a review tool the developer
            // installed.
            let path = developer_tools::requested_path(child_env, std::env::var_os("PATH"));
            command.env(
                "PATH",
                developer_tools::isolate(&path, &state.path().join("isolated-path"))?,
            );
            Ok(())
        })
        .await?;
        let driver = provider
            .create_session(
                application_capabilities(env.app_binary())?,
                deadline.saturating_duration_since(Instant::now()),
            )
            .await?;
        Ok((driver, provider))
    }

    /// Fail immediately if the binary under test is a development build.
    ///
    /// The two builds are indistinguishable on disk — same name, same
    /// strings — and a development build still produces a window, a session
    /// and a URL. It simply never mounts, so without this the run spends the
    /// whole mount timeout discovering nothing useful.
    ///
    /// The usual cause is mundane: `cargo build`, `cargo test` or `cargo
    /// clippy` for `slashit-ui` rebuilds `target/debug/slashit-ui` without
    /// `custom-protocol` and silently replaces the acceptance binary.
    async fn assert_custom_protocol_build(&self) -> Result<()> {
        let url = self
            .driver
            .current_url()
            .await
            .context("could not read the application's URL")?
            .to_string();
        if url.starts_with(DEV_URL_PREFIX) {
            bail!(
                "the application under test is a development build: it is loading {url} \
                 instead of the embedded frontend. Rebuild it with\n    \
                 scripts/build-acceptance-app.sh\n\
                 An ordinary cargo build, test or clippy for `slashit-ui` overwrites the \
                 same path with a binary that has no frontend embedded."
            );
        }
        Ok(())
    }

    /// Wait for the Leptos root to exist.
    ///
    /// A Tauri window appears long before WASM has mounted anything, so
    /// "the process is alive" is not readiness. This is.
    async fn wait_until_mounted(&self) -> Result<()> {
        self.driver
            .query(By::Css(ROOT_ELEMENT))
            .wait(MOUNT_TIMEOUT, POLL)
            .exists()
            .await
            .with_context(|| {
                format!(
                    "the Leptos application never mounted: {ROOT_ELEMENT} did not appear \
                     within {}s",
                    MOUNT_TIMEOUT.as_secs()
                )
            })?;
        Ok(())
    }

    pub fn driver(&self) -> &WebDriver {
        &self.driver
    }

    /// Where this session's provider and native-driver output went.
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// End the session, terminate the provider tree, and verify both.
    ///
    /// Closing the WebDriver session is what asks the application to exit;
    /// terminating the process group is what guarantees it. Neither is
    /// trusted on its own.
    pub async fn shutdown(self) -> Result<()> {
        let Session {
            driver, provider, ..
        } = self;

        // A session that already died takes its own error path; the process
        // group teardown below is the part that must still happen.
        let quit = driver.quit().await;

        provider.shutdown()?;

        quit.map_err(|e| anyhow!("closing the WebDriver session failed: {e}"))
    }
}

/// The capabilities that ask the provider to launch `app_binary`.
fn application_capabilities(app_binary: &Path) -> Result<Capabilities> {
    let mut capabilities = Capabilities::new();
    // `tauri-driver` reads `tauri:options` out of `capabilities.alwaysMatch`
    // and rewrites it into the native driver's own vendor namespace
    // (`webkitgtk:browserOptions` here). thirtyfour already serialises
    // exactly that envelope, so no protocol glue is needed.
    capabilities
        .set("tauri:options", json!({ "application": app_binary, "args": [] }))
        .map_err(|e| anyhow!("could not set tauri:options: {e}"))?;
    Ok(capabilities)
}

/// A running `tauri-driver`, the native WebDriver it started, and their ports.
#[derive(Debug)]
pub struct Provider {
    process: OwnedProcess,
    client_port: u16,
    native_port: u16,
}

impl Provider {
    /// Start `tauri-driver` on two fresh ports, wait until it accepts
    /// connections, and wait until the native driver it started reports
    /// itself ready. `configure` sets the environment the native driver, and
    /// through it the application, inherits.
    ///
    /// `deadline` bounds both waits together: whatever either one does not
    /// use is what the other gets. It is the caller's job to make sure it
    /// leaves something for the session-creation call that follows.
    pub async fn spawn(
        native_driver: Option<&Path>,
        log_path: &Path,
        deadline: Instant,
        configure: impl FnOnce(&mut Command) -> Result<()>,
    ) -> Result<Self> {
        let mut client = ReservedPort::reserve()?;
        let mut native = ReservedPort::reserve()?;
        let (client_port, native_port) = (client.port(), native.port());

        let log = std::fs::File::create(log_path)
            .with_context(|| format!("could not create the provider log {}", log_path.display()))?;

        let mut command = Command::new("tauri-driver");
        command
            .arg("--port")
            .arg(client_port.to_string())
            .arg("--native-port")
            .arg(native_port.to_string());
        if let Some(native_driver) = native_driver {
            command.arg("--native-driver").arg(native_driver);
        }
        configure(&mut command)?;
        command
            .stdout(Stdio::from(log.try_clone().context("clone the log handle")?))
            .stderr(Stdio::from(log));

        // Hand both ports over only now, and refuse to start if anything
        // grabbed one in the meantime.
        client.release();
        native.release();
        ports::assert_free(client_port, "the provider's client port")?;
        ports::assert_free(native_port, "the provider's native-driver port")?;

        let process = OwnedProcess::spawn("tauri-driver", &mut command)
            .context("could not start tauri-driver — is it installed and on PATH?")?;
        let provider = Self {
            process,
            client_port,
            native_port,
        };

        ports::wait_until_listening(
            client_port,
            deadline.saturating_duration_since(Instant::now()),
            "tauri-driver",
        )
        .await?;

        // `tauri-driver` forwards to `native_port` the moment it accepts a
        // connection on `client_port` -- it does not itself wait for the
        // native driver to be ready (see the module doc). Doing that wait
        // here, against the native driver directly, is what keeps the first
        // real request (`New Session`) from racing a driver that only just
        // started.
        ports::wait_until_http_ready(
            native_port,
            "/status",
            deadline.saturating_duration_since(Instant::now()),
            "the native WebDriver",
        )
        .await
        .with_context(|| {
            format!(
                "tauri-driver's own port opened, but its native driver on port {native_port} \
                 never did; see {} for its startup output",
                log_path.display()
            )
        })?;

        Ok(provider)
    }

    /// Where a WebDriver client reaches this provider.
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.client_port)
    }

    /// Create the W3C session, launching whatever `capabilities` name.
    pub async fn create_session(
        &mut self,
        capabilities: Capabilities,
        budget: Duration,
    ) -> Result<WebDriver> {
        tokio::time::timeout(budget, WebDriver::new(self.endpoint(), capabilities))
            .await
            .map_err(|_| {
                anyhow!(
                    "creating a WebDriver session timed out after {}s — the application never \
                     came up",
                    budget.as_secs()
                )
            })?
            .map_err(|e| anyhow!("the provider refused to create a session: {e}"))
    }

    /// Terminate the provider tree and verify both ports were let go.
    pub fn shutdown(mut self) -> Result<()> {
        self.process.terminate()?;
        ports::assert_free(self.client_port, "the provider's client port after shutdown")?;
        ports::assert_free(self.native_port, "the native driver's port after shutdown")
    }
}
