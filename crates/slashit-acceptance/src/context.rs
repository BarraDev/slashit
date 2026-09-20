//! What one acceptance test owns: where the application is, which private
//! state root it runs against, and where its evidence goes.
//!
//! Linux-only, and deliberately so — see the platform note in the crate
//! documentation. Everything here composes [`crate::driver`],
//! [`crate::process`] and [`crate::state`], none of which has a macOS or
//! Windows implementation yet.

use crate::diagnostics;
use crate::driver::Session;
use crate::process;
use crate::state::{claim_unique, StateRoot};
use anyhow::{bail, Context, Result};
use std::cell::Cell;
use std::path::{Path, PathBuf};

/// Where the harness finds the things it does not build itself.
pub struct Environment {
    app_binary: PathBuf,
    native_driver: Option<PathBuf>,
    artifacts_root: PathBuf,
}

impl Environment {
    /// Locate the application under test and the optional native driver.
    pub fn discover() -> Result<Self> {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .context("could not walk up to the workspace root")?
            .to_path_buf();

        let app_binary = match std::env::var_os("SLASHIT_ACCEPTANCE_BIN") {
            Some(explicit) => PathBuf::from(explicit),
            None => workspace_root.join("target/debug/slashit-ui"),
        };
        if !app_binary.is_file() {
            bail!(
                "no application binary at {}. Build one first:\n    \
                 cargo tauri build --debug --no-bundle\n\
                 or point SLASHIT_ACCEPTANCE_BIN at an existing custom-protocol build.",
                app_binary.display()
            );
        }

        let native_driver = match std::env::var_os("SLASHIT_ACCEPTANCE_NATIVE_DRIVER") {
            Some(explicit) => {
                let path = PathBuf::from(explicit);
                if !path.is_file() {
                    bail!(
                        "SLASHIT_ACCEPTANCE_NATIVE_DRIVER points at {}, which is not a file",
                        path.display()
                    );
                }
                Some(path)
            }
            None => None,
        };

        Ok(Self {
            app_binary,
            native_driver,
            artifacts_root: workspace_root.join("target/acceptance"),
        })
    }

    pub fn app_binary(&self) -> &Path {
        &self.app_binary
    }

    pub fn native_driver(&self) -> Option<&Path> {
        self.native_driver.as_deref()
    }
}

/// One acceptance test: its isolated state, its artifact directory and the
/// sessions it opens against them.
///
/// The state root outlives individual sessions on purpose — that is what
/// makes a restart journey possible without any product-level reload.
pub struct TestContext {
    name: String,
    environment: Environment,
    state: StateRoot,
    artifacts: PathBuf,
    sessions_started: Cell<usize>,
}

impl TestContext {
    pub fn new(name: &str) -> Result<Self> {
        let environment = Environment::discover()?;

        // Bound what previous failures left behind before adding to it.
        diagnostics::prune(&environment.artifacts_root)?;

        // Claimed exclusively, for the same reason the state root is: a name
        // built from the test's own name and a second-resolution clock is one
        // two contexts can agree on, and `create_dir_all` hands both of them
        // the same directory rather than telling either that it is taken.
        // Whoever finished first would then delete the other's evidence.
        let artifacts = claim_unique(&environment.artifacts_root, name)?;

        // Deliberately under the system temp directory rather than inside
        // `target/`: the application binds a Unix socket under
        // `XDG_RUNTIME_DIR`, and socket paths are limited to about 108 bytes.
        let state = StateRoot::create(&std::env::temp_dir(), "slashit-acc")?;

        Ok(Self {
            name: name.to_string(),
            environment,
            state,
            artifacts,
            sessions_started: Cell::new(0),
        })
    }

    pub fn state(&self) -> &StateRoot {
        &self.state
    }

    /// Launch the application against this context's state root.
    ///
    /// Calling it twice is a real restart: a new application process and a new
    /// WebDriver session, pointed at the state the previous one left behind.
    pub async fn start_session(&self, label: &str) -> Result<Session> {
        let index = self.sessions_started.get();
        self.sessions_started.set(index + 1);
        let log = self
            .artifacts
            .join(format!("{index}-{label}-provider.log"));
        Session::start(&self.environment, &self.state, log)
            .await
            .with_context(|| format!("could not start session {index} ({label})"))
    }

    /// Close a session, capturing evidence first if the journey failed.
    ///
    /// Takes the outcome rather than being called from an error branch so the
    /// application is still alive when its screenshot is taken.
    pub async fn close_session<T>(
        &self,
        session: Session,
        label: &str,
        outcome: &Result<T>,
    ) -> Result<()> {
        if outcome.is_err() {
            let notes = diagnostics::capture(session.driver(), &self.artifacts, label).await;
            eprintln!(
                "[{}] {label} failed; captured:\n  {notes}\n  provider log: {}",
                self.name,
                session.log_path().display()
            );
        }

        session.shutdown().await?;
        // The session's own shutdown above is the lifecycle guarantee: it owns
        // the process group and returns only once that group is gone. This is
        // the second look, for anything that escaped the group.
        process::assert_no_visible_survivors(&self.state.config_home())
    }

    /// Report the test's outcome, cleaning up or preserving accordingly.
    ///
    /// Panics on failure, because that is how a `#[tokio::test]` fails, and
    /// prints exactly where the evidence is.
    pub fn finish<T>(mut self, outcome: Result<T>) {
        // The journey's own failure comes first and alone: cleanup runs no
        // further here, so nothing this function does can overwrite the
        // evidence or the report.
        if let Err(error) = outcome {
            self.state.keep();
            panic!(
                "{} failed:\n{error:?}\n\nartifacts: {}\nstate root (preserved): {}",
                self.name,
                self.artifacts.display(),
                self.state.path().display()
            );
        }

        // Nothing to explain, so leave nothing behind — and prove it. A green
        // journey that quietly leaves a state root under `/tmp` is how a
        // machine fills up over a week of green runs, and the harness would
        // have reported success every time. Both removals are attempted even
        // when the first fails: one directory that will not go is no reason to
        // abandon the other.
        let mut leaks = Vec::new();
        if let Err(error) = std::fs::remove_dir_all(&self.artifacts) {
            leaks.push(format!(
                "artifact directory {}: {error}",
                self.artifacts.display()
            ));
        }
        if let Err(error) = self.state.cleanup() {
            leaks.push(format!("{error:#}"));
        }
        assert!(
            leaks.is_empty(),
            "{} passed but could not clean up after itself:\n  {}",
            self.name,
            leaks.join("\n  ")
        );
    }
}
