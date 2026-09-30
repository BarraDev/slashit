//! The journeys that prove the harness itself.
//!
//! They are deliberately few. Their job is not product coverage — it is to
//! establish that a Rust test can start the real SlashIt desktop application,
//! reach its frontend and its backend, restart it, and clean up after itself.
//! Product acceptance coverage belongs in later suites built on these
//! primitives.
//!
//! One more group, below the two application journeys, proves a narrower
//! thing: that [`Provider::spawn`] actually waits for the native WebDriver to
//! be ready rather than racing it. Those use a stand-in native driver
//! (`src/bin/fake-native-driver.rs`) instead of the real application, because
//! the race under test is between `tauri-driver` and its native driver, one
//! layer below anything the application can influence.
//!
//! Run with:
//!
//! ```text
//! cargo tauri build --debug --no-bundle
//! cargo test -p slashit-acceptance --features run-acceptance
//! ```
//!
//! Linux only. The harness has no macOS or Windows runtime yet, so on any
//! other target this file compiles to nothing rather than to a suite that
//! looks portable and then fails for reasons nobody can act on.

#![cfg(target_os = "linux")]

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use slashit_acceptance::driver::Provider;
use slashit_acceptance::{ui, TestContext};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use thirtyfour::prelude::*;

/// The button that opens the project wizard, and the wizard it opens.
const ADD_PROJECT: &str = "[data-testid=\"rail-add-project\"]";
const CREATE_PROJECT_MODAL: &str = "[data-testid=\"create-project-modal\"]";
const PROJECT_RAIL: &str = "[data-testid=\"project-rail\"]";

/// Prove the application under test is the real thing: the packaged frontend
/// rendering real Leptos components, reacting to a real click, and talking to
/// a real backend — not a surviving process, a dev-server error page, or an
/// empty window.
#[tokio::test(flavor = "multi_thread")]
async fn app_boots_and_frontend_is_real() {
    let context = TestContext::new("app_boots_and_frontend_is_real").expect("harness setup");
    let outcome = boot_journey(&context).await;
    context.finish(outcome);
}

async fn boot_journey(context: &TestContext) -> Result<()> {
    let session = context.start_session("boot").await?;
    let result = boot_assertions(session.driver()).await;
    context.close_session(session, "boot", &result).await?;
    result
}

async fn boot_assertions(driver: &WebDriver) -> Result<()> {
    // The packaged frontend, not `devUrl` and not an error page.
    ui::assert_frontend_is_real(driver).await?;

    // A second real component, so the proof does not rest on the single
    // element the harness already waits for as its readiness signal.
    ui::visible(driver, PROJECT_RAIL).await?;

    // A real interaction with a consequence that could not already be true:
    // the wizard is absent, the click creates it, and it is visible.
    ui::assert_absent(driver, CREATE_PROJECT_MODAL).await?;
    ui::visible(driver, ADD_PROJECT)
        .await?
        .click()
        .await
        .context("clicking the add-project button failed")?;
    ui::visible(driver, CREATE_PROJECT_MODAL)
        .await
        .context("clicking the add-project button did not open the project wizard")?;

    // A real backend round trip, while that rendered UI is alive. An empty
    // list is the correct answer for a fresh state root and is therefore also
    // the isolation proof: the developer's real projects are not visible here.
    let projects = ui::invoke(driver, "list_projects", json!({})).await?;
    let projects = projects
        .as_array()
        .with_context(|| format!("list_projects should return an array, got {projects}"))?;
    if !projects.is_empty() {
        bail!(
            "a fresh state root should contain no projects, found {} — state isolation is leaking",
            projects.len()
        );
    }

    Ok(())
}

/// Prove state written by one application process is still there for the
/// next one: a full restart, not a page reload and not a re-read of a value
/// that never left memory.
#[tokio::test(flavor = "multi_thread")]
async fn state_survives_restart() {
    let context = TestContext::new("state_survives_restart").expect("harness setup");
    let outcome = restart_journey(&context).await;
    context.finish(outcome);
}

/// The smallest domain object SlashIt persists on its own: a project needs
/// only a name, is written straight into `config.toml`, is reloaded at
/// startup, and renders in the rail with an identifying `data-testid`.
struct CreatedProject {
    id: String,
    name: String,
}

async fn restart_journey(context: &TestContext) -> Result<()> {
    // Process A: create the state.
    let session = context.start_session("before-restart").await?;
    let created = create_project(session.driver()).await;
    context
        .close_session(session, "before-restart", &created)
        .await?;
    let created = created?;

    // Corroborate against the file the application actually wrote, so a
    // restart that somehow served the value from elsewhere would still fail.
    let config_file = context.state().config_file();
    let config = std::fs::read_to_string(&config_file).with_context(|| {
        format!(
            "the application wrote no config at {} — nothing could have been persisted",
            config_file.display()
        )
    })?;
    if !config.contains(&created.id) || !config.contains(&created.name) {
        bail!(
            "{} does not contain the created project ({} / {})",
            config_file.display(),
            created.id,
            created.name
        );
    }

    // Process B: a brand new application process and a brand new WebDriver
    // session, pointed at the same state root.
    let session = context.start_session("after-restart").await?;
    let verified = verify_persisted(session.driver(), &created).await;
    context
        .close_session(session, "after-restart", &verified)
        .await?;
    verified
}

async fn create_project(driver: &WebDriver) -> Result<CreatedProject> {
    ui::assert_frontend_is_real(driver).await?;

    // Unique per run, so a leaked state root could never make this pass.
    let name = format!(
        "Acceptance {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );

    // The same command, with the same argument shape, that
    // `src/services/project_service.rs` sends when the wizard is submitted.
    let project = ui::invoke(
        driver,
        "create_project",
        json!({ "name": name, "repositoryId": Value::Null, "agentType": "claude_code" }),
    )
    .await?;

    let id = project
        .get("id")
        .and_then(Value::as_str)
        .with_context(|| format!("create_project returned no id: {project}"))?
        .to_string();

    // Present before the restart, so the restart is what is under test rather
    // than creation itself.
    let listed = ui::invoke(driver, "list_projects", json!({})).await?;
    if !mentions_project(&listed, &id) {
        bail!("the project was created but list_projects does not return it: {listed}");
    }

    Ok(CreatedProject { id, name })
}

async fn verify_persisted(driver: &WebDriver, created: &CreatedProject) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;

    // The strongest available evidence: the rail renders one button per
    // project, keyed by id, from a list the freshly started frontend fetched
    // from a freshly started backend that read it off disk. Nothing in this
    // process saw the value before.
    let selector = format!("[data-testid=\"rail-project-{}\"]", created.id);
    let button = ui::visible(driver, &selector).await.with_context(|| {
        format!(
            "the restarted application does not render the project created before the restart \
             ({})",
            created.id
        )
    })?;

    // The rail puts the project name in the button's title, so this confirms
    // the same object came back rather than merely some project.
    let title = button
        .attr("title")
        .await
        .context("could not read the rail button's title")?
        .unwrap_or_default();
    if title != created.name {
        bail!(
            "the restored project has the wrong name: expected {:?}, found {:?}",
            created.name,
            title
        );
    }

    // And the backend agrees, with exactly the one project this test made.
    let listed = ui::invoke(driver, "list_projects", json!({})).await?;
    let projects = listed
        .as_array()
        .with_context(|| format!("list_projects should return an array, got {listed}"))?;
    if projects.len() != 1 {
        bail!(
            "expected exactly the one persisted project after restart, found {}",
            projects.len()
        );
    }
    if !mentions_project(&listed, &created.id) {
        bail!("list_projects after restart does not contain {}", created.id);
    }

    Ok(())
}

fn mentions_project(listed: &Value, id: &str) -> bool {
    listed
        .as_array()
        .is_some_and(|projects| {
            projects
                .iter()
                .any(|project| project.get("id").and_then(Value::as_str) == Some(id))
        })
}

// Below here: the native-driver readiness regression.
//
// A hosted run of PR #93 hit this for real once: `tauri-driver`'s own port
// was open, `New Session` was sent, and `WebKitWebDriver` had not started
// listening yet, so `tauri-driver`'s forward was refused. These tests use
// `fake-native-driver` (`src/bin/fake-native-driver.rs`) — a stand-in that
// can be told to delay listening, or to answer `POST /session` with a real
// error — through the same `--native-driver` override the harness already
// gives `tauri-driver` (`Provider::spawn`). Nothing here touches the real
// application.

/// Where this test's provider log goes. A directory under the system temp
/// root, named for the test and unique enough that two runs of the whole
/// suite never collide, kept only on failure.
fn scratch_log(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "slashit-acc-driver-startup-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).expect("create a scratch directory for the provider log");
    dir.join("provider.log")
}

/// Capabilities `fake-native-driver` accepts. It never launches anything —
/// `application` only needs the shape `tauri-driver`'s capability rewrite
/// expects, never a real, existing path.
fn fake_capabilities() -> Capabilities {
    let mut capabilities = Capabilities::new();
    capabilities
        .set(
            "tauri:options",
            json!({ "application": "/nonexistent/fake-app", "args": [] }),
        )
        .expect("set tauri:options");
    capabilities
}

/// A native driver that starts late must still be waited for.
///
/// RED against the code before this test was added: `Provider::spawn` waited
/// only for `tauri-driver`'s own client port, so it returned as soon as that
/// was open — while `fake-native-driver` was still three seconds from
/// listening — and the `New Session` call that followed hit exactly the
/// connection-refused forward the hosted run did.
#[tokio::test(flavor = "multi_thread")]
async fn a_native_driver_that_starts_late_is_still_waited_for() {
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake-native-driver"));
    let log_path = scratch_log("starts-late");
    let deadline = Instant::now() + Duration::from_secs(20);

    let mut provider = Provider::spawn(Some(&fake), &log_path, deadline, |command| {
        command.env("FAKE_NATIVE_DRIVER_DELAY_MS", "3000");
        Ok(())
    })
    .await
    .expect("a native driver that starts 3s late must still be waited for, not raced");

    let driver = provider
        .create_session(
            fake_capabilities(),
            deadline.saturating_duration_since(Instant::now()),
        )
        .await
        .expect("session creation must succeed once the native driver becomes ready");

    let _ = driver.quit().await;
    provider
        .shutdown()
        .expect("the provider tree must tear down cleanly");
    let _ = std::fs::remove_dir_all(log_path.parent().expect("log has a parent directory"));
}

/// The corrected half of the readiness contract: a native driver that is
/// listening and answering 2xx, but has not yet reported `ready:true`, must
/// still be waited for -- not raced the moment its first response arrives.
///
/// RED against the code before this fix: `wait_until_http_ready` treated any
/// 2xx as readiness, so it returned on the very first `ready:false` poll, and
/// `New Session` followed immediately -- before `fake-native-driver` ever
/// reported itself actually ready. Against that code, the assertions below
/// fail: no `ready=true` line ever appears in the log, because the harness
/// stops polling after its first (still-`ready:false`) response.
#[tokio::test(flavor = "multi_thread")]
async fn a_native_driver_reporting_not_ready_is_waited_for_rather_than_raced() {
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake-native-driver"));
    let log_path = scratch_log("not-ready-polls");
    let deadline = Instant::now() + Duration::from_secs(20);

    let mut provider = Provider::spawn(Some(&fake), &log_path, deadline, |command| {
        // A handful of not-ready polls at the wait's 100ms poll interval:
        // enough to prove the wait actually polled more than once, small
        // enough to keep the test fast.
        command.env("FAKE_NATIVE_DRIVER_NOT_READY_POLLS", "5");
        Ok(())
    })
    .await
    .expect("a native driver that reports ready:false at first must still be waited for");

    let driver = provider
        .create_session(
            fake_capabilities(),
            deadline.saturating_duration_since(Instant::now()),
        )
        .await
        .expect("session creation must succeed once the native driver reports ready:true");

    let _ = driver.quit().await;
    provider
        .shutdown()
        .expect("the provider tree must tear down cleanly");

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.matches("ready=false").count() >= 1,
        "the wait must have observed at least one not-ready poll: log was:\n{log}"
    );
    assert!(
        log.contains("ready=true"),
        "the wait must have observed an explicit ready=true poll before creating a session: log was:\n{log}"
    );
    let ready_true_pos = log.find("ready=true").expect("checked above");
    let session_pos = log
        .find("POST /session")
        .unwrap_or_else(|| panic!("a session must have been created: log was:\n{log}"));
    assert!(
        ready_true_pos < session_pos,
        "New Session must not be sent until the native driver reports ready=true: log was:\n{log}"
    );
    assert_eq!(
        log.matches("POST /session").count(),
        1,
        "session creation must be attempted exactly once: log was:\n{log}"
    );

    let _ = std::fs::remove_dir_all(log_path.parent().expect("log has a parent directory"));
}

/// A native driver that never starts must still fail inside its own bound,
/// not hang, and not be retried against SESSION_TIMEOUT's full length.
#[tokio::test(flavor = "multi_thread")]
async fn a_native_driver_that_never_starts_fails_within_the_deadline() {
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake-native-driver"));
    let log_path = scratch_log("never-starts");
    let budget = Duration::from_secs(3);
    let deadline = Instant::now() + budget;

    let started = Instant::now();
    let result = Provider::spawn(Some(&fake), &log_path, deadline, |command| {
        // Comfortably longer than any deadline this test uses.
        command.env("FAKE_NATIVE_DRIVER_DELAY_MS", "600000");
        Ok(())
    })
    .await;
    let elapsed = started.elapsed();

    let error = result.expect_err("a native driver that never listens must not wait forever");
    assert!(
        elapsed < budget + Duration::from_secs(2),
        "the deadline ({budget:?}) was not honoured: spawn took {elapsed:?}"
    );
    let message = format!("{error:#}");
    assert!(
        message.contains("native"),
        "the failure should name the native driver, not just time out silently: {message}"
    );
    let _ = std::fs::remove_dir_all(log_path.parent().expect("log has a parent directory"));
}

/// A real WebDriver protocol error from `POST /session` must fail promptly
/// and must not be retried by anything this crate added: this is a readiness
/// wait, not a session-creation retry, and the two must not be confused.
///
/// `fake-native-driver` answers with HTTP 400 rather than 500 so this stays
/// true to that scope: `thirtyfour`'s own `start_session` retries a bare 500
/// once, on the theory that some WebDriver servers report a real, momentary
/// startup failure that way (`session/create.rs`). This test is not a claim
/// about that; it is a claim that nothing *this crate* added retries a
/// genuine 4xx protocol error.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_session_creation_error_is_not_retried() {
    let fake = PathBuf::from(env!("CARGO_BIN_EXE_fake-native-driver"));
    let log_path = scratch_log("session-rejected");
    // Generous: proves the failure is fast on its own merits, not merely
    // faster than some arbitrary short budget.
    let deadline = Instant::now() + Duration::from_secs(30);

    let mut provider = Provider::spawn(Some(&fake), &log_path, deadline, |command| {
        command.env("FAKE_NATIVE_DRIVER_REJECT_SESSION", "1");
        Ok(())
    })
    .await
    .expect("a native driver with no artificial delay must be ready almost immediately");

    let started = Instant::now();
    let result = provider
        .create_session(
            fake_capabilities(),
            deadline.saturating_duration_since(Instant::now()),
        )
        .await;
    let elapsed = started.elapsed();

    result.expect_err("fake-native-driver was told to reject every session");
    assert!(
        elapsed < Duration::from_secs(5),
        "a real protocol error must fail promptly, not after a retry/backoff loop: took {elapsed:?}"
    );

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let session_attempts = log.matches("POST /session").count();
    assert_eq!(
        session_attempts, 1,
        "session creation must be attempted exactly once, never retried: log was:\n{log}"
    );

    provider
        .shutdown()
        .expect("the provider tree must tear down cleanly even after a rejected session");
    let _ = std::fs::remove_dir_all(log_path.parent().expect("log has a parent directory"));
}
