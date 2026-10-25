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
