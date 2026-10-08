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
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

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
                 scripts/build-acceptance-app.sh\n\
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
    started: Instant,
    session_logs: RefCell<Vec<PathBuf>>,
    result_recorded: Cell<bool>,
    /// Extra variables for the application process tree, on top of the ones
    /// the state root sets.
    ///
    /// Deliberately a list of pairs rather than anything cleverer: the only
    /// thing a journey has needed so far is a `PATH` whose `claude` is a
    /// fixture instead of the developer's real one, and a mechanism that can
    /// only add named variables to one command cannot grow into a way of
    /// reaching into the application.
    child_env: RefCell<Vec<(OsString, OsString)>>,
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
            started: Instant::now(),
            session_logs: RefCell::new(Vec::new()),
            result_recorded: Cell::new(false),
            child_env: RefCell::new(Vec::new()),
        })
    }

    pub fn state(&self) -> &StateRoot {
        &self.state
    }

    /// Set a variable every session started after this call launches the
    /// application with.
    ///
    /// Sessions already running keep the environment they were started with,
    /// which is what makes a restart journey able to change one.
    pub fn set_child_env(&self, key: impl Into<OsString>, value: impl Into<OsString>) {
        let key = key.into();
        let mut env = self.child_env.borrow_mut();
        env.retain(|(existing, _)| existing != &key);
        env.push((key, value.into()));
    }

    /// Launch the application against this context's state root.
    ///
    /// Calling it twice is a real restart: a new application process and a new
    /// WebDriver session, pointed at the state the previous one left behind.
    pub async fn start_session(&self, label: &str) -> Result<Session> {
        let index = self.sessions_started.get();
        self.sessions_started.set(index + 1);
        let log = self.artifacts.join(format!("{index}-{label}-provider.log"));
        self.session_logs.borrow_mut().push(log.clone());
        // Copied out before the await: holding the borrow across it would let
        // a `set_child_env` from another task panic the whole run.
        let child_env = self.child_env.borrow().clone();
        Session::start(&self.environment, &self.state, log, &child_env)
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
            self.record_result(
                "failed",
                Some(diagnostics::classify_failure(&format!("{error:#}"))),
            );
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
        if !leaks.is_empty() {
            let message = leaks.join("\n  ");
            self.record_result("failed", Some(diagnostics::classify_failure(&message)));
            panic!(
                "{} passed but could not clean up after itself:\n  {}",
                self.name, message
            );
        }
        self.record_result("passed", None);
    }

    fn record_result(
        &self,
        outcome: &'static str,
        failure_class: Option<diagnostics::FailureClass>,
    ) {
        if self.result_recorded.replace(true) {
            return;
        }
        let session_logs = self
            .session_logs
            .borrow()
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let elapsed = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let result = diagnostics::JourneyResult {
            schema_version: 1,
            journey: &self.name,
            outcome,
            duration_ms: elapsed,
            failure_class,
            session_logs: &session_logs,
        };
        let index = std::env::var_os("SLASHIT_ACCEPTANCE_TIMING_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                self.environment
                    .artifacts_root
                    .join("journey-timings.jsonl")
            });
        if let Err(error) = diagnostics::write_journey_result(&self.artifacts, &index, &result) {
            eprintln!(
                "[acceptance] could not write timing diagnostics for {}: {error:#}",
                self.name
            );
        } else if let Ok(json) = serde_json::to_string(&result) {
            println!("{}", context_result_line(&json));
        }
    }
}

/// Keep the in-test diagnostic distinct from the runner's canonical result
/// line, which it replays from the timing index after libtest exits. On a
/// failing test libtest also prints captured stdout, so sharing that prefix
/// would emit the same structured record twice.
fn context_result_line(json: &str) -> String {
    format!("ACCEPTANCE_CONTEXT_RESULT_JSON={json}")
}

#[cfg(test)]
mod tests {
    use super::context_result_line;

    #[test]
    fn context_diagnostic_does_not_use_the_runner_result_prefix() {
        let line = context_result_line(r#"{"journey":"example","outcome":"failed"}"#);

        assert!(line.starts_with("ACCEPTANCE_CONTEXT_RESULT_JSON="));
        assert!(!line.starts_with("ACCEPTANCE_RESULT_JSON="));
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        if self.result_recorded.get() {
            return;
        }
        let panicking = std::thread::panicking();
        if panicking {
            // Preserve state and the provider-tree log for an unexpected
            // panic that bypassed finish(outcome).
            self.state.keep();
        }
        self.record_result(
            if panicking { "panicked" } else { "abandoned" },
            Some(diagnostics::FailureClass::HarnessPanic),
        );
    }
}
