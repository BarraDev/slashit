// SPDX-License-Identifier: MPL-2.0
//! The Project Rail after a selection. The rail expands while the pointer is
//! over it and collapses once the pointer leaves, including after a switch
//! from one selected Project to another.
//!
//! The pointer movement matters. A single WebDriver jump straight from a
//! just-clicked rail item to the board produced no `mousemove` in the WebView,
//! so the rail stayed expanded. A real pointer always reports motion, and a
//! motion over the item before leaving reproduces that. The journey therefore
//! moves within the item before it leaves the rail, as a hand does.

use super::*;

const MAIN: &str = "main";
const RAIL: &str = "[data-testid=\"project-rail\"]";

fn rail_item(project_id: &str) -> String {
    format!("[data-testid=\"rail-project-{project_id}\"]")
}

async fn create_project(driver: &WebDriver, name: &str) -> Result<String> {
    created_id(ui::invoke(driver, "create_project", json!({
        "name": name, "repositoryId": Value::Null, "agentType": "claude_code"
    })).await?, "create_project")
}

/// Whether the rail's root element is in the expanded state right now.
async fn rail_expanded(driver: &WebDriver) -> Result<bool> {
    Ok(page(driver, &format!("const rail = document.querySelector('{RAIL}'); return !!rail && rail.classList.contains('expanded');"), vec![]).await? == Value::Bool(true))
}

/// Polls until the rail's expanded state equals `expected`, or fails with `message`.
async fn await_expanded(driver: &WebDriver, expected: bool, message: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        if rail_expanded(driver).await? == expected {
            return Ok(());
        }
        if started.elapsed() > ui::ELEMENT_TIMEOUT {
            bail!("{message}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Selects a Project in the rail with the pointer, moves within the item as a
/// hand does, then moves the pointer onto the board. The rail must collapse.
async fn select_and_leave(driver: &WebDriver, project_id: &str) -> Result<()> {
    let item = ui::visible(driver, &rail_item(project_id)).await?;
    item.click().await.context("could not select the project in the rail")?;
    await_expanded(driver, true, "the rail did not expand under the pointer").await?;
    let selected = page(driver, &format!("const item = document.querySelector('{}'); return !!item && item.classList.contains('active');", rail_item(project_id)), vec![]).await?;
    if selected != Value::Bool(true) {
        bail!("the Project is not selected after the click");
    }

    driver.action_chain().move_to_element_center(&item).move_by_offset(3, 3).perform().await
        .context("could not move the pointer within the project rail")?;

    let main = ui::visible(driver, MAIN).await?;
    driver.action_chain().move_to_element_center(&main).perform().await
        .context("could not move the pointer off the project rail")?;
    // The collapse takes 0.2 s. A rail still expanded after the settle delay is the defect.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    if rail_expanded(driver).await? {
        bail!("the rail stayed expanded after the pointer left it, following a Project selection");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn selecting_a_project_collapses_the_rail_once_the_pointer_leaves_it() {
    let context = TestContext::new("project_rail_collapse").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-collapse").await?;
        let driver = session.driver();
        let alpha = create_project(driver, "Alpha Rail").await?;
        let beta = create_project(driver, "Beta Rail").await?;
        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;

        // The first selection, then a switch to the other Project.
        select_and_leave(driver, &alpha).await?;
        select_and_leave(driver, &beta).await?;

        context.close_session(session, "rail-collapse", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

const RAIL_ORGANIZE: &str = "[data-testid=\"rail-organize-projects\"]";
const WORKSPACE_NAME: &str = "[data-testid=\"workspace-name-input\"]";
const WORKSPACE_CREATE: &str = "[data-testid=\"workspace-create\"]";
const ORGANIZER_CLOSE: &str = "[data-testid=\"project-organizer\"] button[aria-label=\"Close\"]";
const WEBDRIVER_ESCAPE: &str = "\u{E00C}";
const RAIL_ADD: &str = "[data-testid=\"rail-add-project\"]";

/// Geometry and state of a Workspace's select control in the rail, found by its
/// accessible name. Null when the rail has no such control.
async fn rail_workspace(driver: &WebDriver, name: &str) -> Result<Value> {
    page(
        driver,
        &format!(
            "const el = [...document.querySelectorAll('[data-testid^=\"rail-workspace-select-\"]')] \
             .find(e => e.getAttribute('aria-label') === 'Workspace {name}'); \
             if (!el) return null; \
             const r = el.getBoundingClientRect(); \
             const badge = el.querySelector('.rail-badge').getBoundingClientRect(); \
             const rail = document.querySelector('[data-testid=\"project-rail\"]').getBoundingClientRect(); \
             return {{ width: r.width, height: r.height, badgeWidth: badge.width, badgeHeight: badge.height, \
                       badgeLeft: badge.left, badgeRight: badge.right, railRight: rail.right, \
                       active: el.classList.contains('active') }};"
        ),
        vec![],
    )
    .await
}

async fn wait_until<F, Fut>(what: &str, mut check: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let started = Instant::now();
    loop {
        if check().await? {
            return Ok(());
        }
        if started.elapsed() > ui::ELEMENT_TIMEOUT {
            bail!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A Workspace is created in Organize Projects, which has no Projects yet. The
/// dialog closes and the rail must still list the Workspace, visibly and
/// selectably, before any member Project exists.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_without_projects_stays_visible_in_the_rail() {
    let context = TestContext::new("project_rail_workspace").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-workspace").await?;
        let driver = session.driver();
        ui::assert_frontend_is_real(driver).await?;

        ui::visible(driver, RAIL_ORGANIZE).await?.click().await.context("could not open Organize Projects")?;
        ui::visible(driver, WORKSPACE_NAME).await?.send_keys("Field Workspace").await?;
        ui::visible(driver, WORKSPACE_CREATE).await?.click().await.context("could not press Create workspace")?;
        wait_until("the new Workspace to appear in the rail", || async {
            Ok(rail_workspace(driver, "Field Workspace").await? != Value::Null)
        })
        .await?;
        // The folder is the managed one SlashIt created, never "no folder".
        wait_until("the Workspace folder to be shown", || async {
            Ok(page(
                driver,
                "const el = document.querySelector('[data-testid=\"workspace-root-label\"]'); \
                 return !!el && el.innerText.includes('/workspaces/');",
                vec![],
            )
            .await?
                == Value::Bool(true))
        })
        .await?;
        ui::visible(driver, ORGANIZER_CLOSE).await?.send_keys(WEBDRIVER_ESCAPE).await?;
        let main = ui::visible(driver, MAIN).await?;
        driver.action_chain().move_to_element_center(&main).perform().await?;
        tokio::time::sleep(Duration::from_millis(1500)).await;

        let state = rail_workspace(driver, "Field Workspace").await?;
        driver.screenshot(&context.artifacts_dir().join("rail-empty-workspace.png")).await?;
        if state["width"].as_f64().unwrap_or(0.0) <= 0.0 || state["badgeHeight"].as_f64().unwrap_or(0.0) <= 0.0 {
            bail!("the empty Workspace is in the rail but not visible: {state}");
        }
        if state["badgeRight"].as_f64().unwrap_or(f64::MAX) > state["railRight"].as_f64().unwrap_or(0.0) {
            bail!("the Workspace badge is clipped by the collapsed rail: {state}");
        }

        ui::visible(driver, "[data-testid^=\"rail-workspace-select-\"]").await?.click().await?;
        wait_until("the Workspace to become the active selection", || async {
            Ok(rail_workspace(driver, "Field Workspace").await?["active"] == Value::Bool(true))
        })
        .await?;
        ui::visible(driver, "[data-testid=\"workspace-home\"]").await?;

        context.close_session(session, "rail-workspace", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

/// A Recent Repository whose folder is already open as a Project is refused
/// from Get Started: the refusal is shown, the dialog stays open, and no second
/// Project for the folder appears.
#[tokio::test(flavor = "multi_thread")]
async fn a_recent_folder_that_is_already_open_is_refused_without_a_second_project() {
    let context = TestContext::new("project_rail_duplicate").expect("harness setup");

    let outcome = async {
        let folder = context.state().path().join("folders").join("alpha");
        std::fs::create_dir_all(&folder).context("could not create the fixture folder")?;
        let location = folder.to_str().context("the folder path is not UTF-8")?.to_string();

        let session = context.start_session("rail-duplicate").await?;
        let driver = session.driver();
        let repository = ui::invoke(driver, "create_repository", json!({
            "localPath": location, "remoteUrl": Value::Null, "initialize": Value::Null
        }))
        .await?;
        let repository_id = created_id(repository, "create_repository")?;
        created_id(ui::invoke(driver, "create_project", json!({
            "name": "Alpha", "repositoryId": repository_id, "agentType": "claude_code"
        }))
        .await?, "create_project")?;

        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;
        ui::visible(driver, RAIL_ADD).await?.click().await.context("could not open Get Started")?;
        ui::visible(driver, "[data-testid=\"wizard-create-project\"]").await?;

        let tagged = page(
            driver,
            "const row = [...document.querySelectorAll('button')].find(b => b.textContent.includes(arguments[0])); \
             if (!row) return false; row.setAttribute('data-acceptance-recent', '1'); return true;",
            vec![json!(location)],
        )
        .await?;
        if tagged != Value::Bool(true) {
            bail!("the Recent Repository for the open folder is not listed in Get Started");
        }
        ui::visible(driver, "[data-acceptance-recent=\"1\"]").await?
            .click()
            .await
            .context("could not press the Recent Repository")?;
        wait_until("the refusal to be shown", || async {
            Ok(page(
                driver,
                "return document.body.innerText.includes(\"already open as Project 'Alpha'\");",
                vec![],
            )
            .await?
                == Value::Bool(true))
        })
        .await?;

        let listed = ui::invoke(driver, "list_projects", json!({})).await?;
        let count = listed.as_array().map_or(0, |items| items.len());
        if count != 1 {
            bail!("the refused open still left {count} Projects for one folder");
        }
        context.close_session(session, "rail-duplicate", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

/// The chevron of a Workspace group hides and shows its member Projects, and
/// the Workspace's own control stays separate from it.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_chevron_collapses_and_expands_its_member_projects() {
    let context = TestContext::new("project_rail_group").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-group").await?;
        let driver = session.driver();
        let alpha = create_project(driver, "Alpha Group").await?;
        let beta = create_project(driver, "Beta Group").await?;
        let workspace = created_id(
            ui::invoke(driver, "create_workspace", json!({ "name": "Grouped", "rootPath": Value::Null })).await?,
            "create_workspace",
        )?;
        for project in [&alpha, &beta] {
            ui::invoke(driver, "attach_project_to_workspace", json!({ "projectId": project, "workspaceId": workspace })).await?;
        }

        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;
        let members = format!(
            "return document.querySelectorAll('[data-testid=\"rail-workspace-{workspace}\"] [data-testid^=\"rail-project-\"]').length;"
        );
        wait_until("both members under the Workspace", || async {
            Ok(page(driver, &members, vec![]).await?.as_u64() == Some(2))
        })
        .await?;

        let toggle = format!("[data-testid=\"rail-workspace-toggle-{workspace}\"]");
        ui::visible(driver, &toggle).await?.click().await.context("could not collapse the Workspace")?;
        wait_until("the chevron to hide the members", || async {
            Ok(page(driver, &members, vec![]).await?.as_u64() == Some(0))
        })
        .await?;

        ui::visible(driver, &toggle).await?.click().await.context("could not expand the Workspace")?;
        wait_until("the chevron to show the members again", || async {
            Ok(page(driver, &members, vec![]).await?.as_u64() == Some(2))
        })
        .await?;

        context.close_session(session, "rail-group", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}
