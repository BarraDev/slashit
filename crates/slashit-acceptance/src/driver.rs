//! The Linux WebDriver provider and one live application session.
//!
//! The chain is `thirtyfour -> tauri-driver -> WebKitWebDriver -> SlashIt`.
//! Only the first hop is ours: `tauri-driver` starts the native driver, which
//! starts the application, so the harness owns exactly one process — and,
//! through its process group, everything below it.
//!
//! Readiness is never a sleep. Each stage waits on the condition that stage
//! actually establishes: the provider accepting connections, the W3C session
//! existing, and the Leptos root having mounted.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use thirtyfour::prelude::*;

use crate::context::Environment;
use crate::ports::{self, ReservedPort};
use crate::process::OwnedProcess;
use crate::state::StateRoot;
use crate::{DEV_URL_PREFIX, ROOT_ELEMENT};

/// How long `tauri-driver` gets to bind its client port.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the whole "create a session and launch the application" step gets.
const SESSION_TIMEOUT: Duration = Duration::from_secs(90);
/// How long the Leptos application gets to mount its root element.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(60);
/// Polling interval for element readiness.
const POLL: Duration = Duration::from_millis(200);

/// One running application plus the WebDriver session driving it.
pub struct Session {
    driver: WebDriver,
    provider: OwnedProcess,
    client_port: u16,
    native_port: u16,
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
        let mut client = ReservedPort::reserve()?;
        let mut native = ReservedPort::reserve()?;
        let (client_port, native_port) = (client.port(), native.port());

        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("could not create the provider log {}", log_path.display()))?;

        let mut command = Command::new("tauri-driver");
        command
            .arg("--port")
            .arg(client_port.to_string())
            .arg("--native-port")
            .arg(native_port.to_string());
        if let Some(native_driver) = env.native_driver() {
            command.arg("--native-driver").arg(native_driver);
        }
        // The journey's own variables go on first so the state root's go on
        // last and win. Isolation is not something a test should be able to
        // switch off by asking for one more variable.
        for (key, value) in child_env {
            command.env(key, value);
        }
        // The application is spawned by the native driver, which inherits this
        // environment. Setting it here is what keeps the run off the
        // developer's real SlashIt state.
        state.apply_to(&mut command);
        command
            .stdout(Stdio::from(log.try_clone().context("clone the log handle")?))
            .stderr(Stdio::from(log));

        // Hand both ports over only now, and refuse to start if anything
        // grabbed one in the meantime.
        client.release();
        native.release();
        ports::assert_free(client_port, "the provider's client port")?;
        ports::assert_free(native_port, "the provider's native-driver port")?;

        let provider = OwnedProcess::spawn("tauri-driver", &mut command)
            .context("could not start tauri-driver — is it installed and on PATH?")?;

        ports::wait_until_listening(client_port, PROVIDER_TIMEOUT, "tauri-driver").await?;

        let driver = match Self::connect(env.app_binary(), client_port).await {
            Ok(driver) => driver,
            Err(e) => {
                // The provider log is the only place the native driver's own
                // complaint appears, so name it in the failure.
                return Err(e.context(format!(
                    "provider log: {} (application: {})",
                    log_path.display(),
                    env.app_binary().display()
                )));
            }
        };

        let session = Self {
            driver,
            provider,
            client_port,
            native_port,
            log_path,
        };

        session.assert_custom_protocol_build().await?;
        session.wait_until_mounted().await?;
        Ok(session)
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
                 cargo tauri build --debug --no-bundle\n\
                 An ordinary cargo build, test or clippy for `slashit-ui` overwrites the \
                 same path with a binary that has no frontend embedded."
            );
        }
        Ok(())
    }

    async fn connect(app_binary: &Path, client_port: u16) -> Result<WebDriver> {
        let mut capabilities = Capabilities::new();
        // `tauri-driver` reads `tauri:options` out of `capabilities.alwaysMatch`
        // and rewrites it into the native driver's own vendor namespace
        // (`webkitgtk:browserOptions` here). thirtyfour already serialises
        // exactly that envelope, so no protocol glue is needed.
        capabilities
            .set("tauri:options", json!({ "application": app_binary, "args": [] }))
            .map_err(|e| anyhow!("could not set tauri:options: {e}"))?;

        let endpoint = format!("http://127.0.0.1:{client_port}");
        tokio::time::timeout(SESSION_TIMEOUT, WebDriver::new(&endpoint, capabilities))
            .await
            .map_err(|_| {
                anyhow!(
                    "creating a WebDriver session timed out after {}s — the application never \
                     came up",
                    SESSION_TIMEOUT.as_secs()
                )
            })?
            .map_err(|e| anyhow!("the provider refused to create a session: {e}"))
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
            driver,
            mut provider,
            client_port,
            native_port,
            ..
        } = self;

        // A session that already died takes its own error path; the process
        // group teardown below is the part that must still happen.
        let quit = driver.quit().await;

        provider.terminate()?;
        ports::assert_free(client_port, "the provider's client port after shutdown")?;
        ports::assert_free(native_port, "the native driver's port after shutdown")?;

        quit.map_err(|e| anyhow!("closing the WebDriver session failed: {e}"))
    }
}
