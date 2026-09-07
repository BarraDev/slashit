//! The two journeys that prove the harness itself.
//!
//! They are deliberately few. Their job is not product coverage — it is to
//! establish that a Rust test can start the real SlashIt desktop application,
//! reach its frontend and its backend, restart it, and clean up after itself.
//! Product acceptance coverage belongs in later suites built on these
//! primitives.
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
use slashit_acceptance::{ui, TestContext};
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
