//! The Queue settings are operated in Settings > Queue and survive a restart.
//!
//! A Human turns automatic promotion off in the form, queues a Task, and the
//! Task stays queued. After a real restart the form still shows promotion off
//! and the Task still does not start; turning promotion back on in the form is
//! what lets the Queue run it. Nothing here uses the WebView inspector or an
//! IPC command to change a setting: the only way in is the page.

use super::human_review::click;
use super::*;

const SETTINGS_QUEUE_TAB: &str = "[data-testid=\"settings-tab-queue\"]";
const SUMMARY: &str = "[data-testid=\"queue-settings-summary\"]";
const CURRENT_LIMIT: &str = "[data-testid=\"queue-settings-current-limit\"]";
const CURRENT_AUTO_PROMOTE: &str = "[data-testid=\"queue-settings-current-auto-promote\"]";
const EDIT: &str = "[data-testid=\"queue-settings-edit\"]";
const MODAL: &str = "[data-testid=\"queue-settings-modal\"]";
const LIMIT_INPUT: &str = "[data-testid=\"queue-settings-parallel-limit\"]";
const AUTO_PROMOTE_INPUT: &str = "[data-testid=\"queue-settings-auto-promote\"]";
const SAVE: &str = "[data-testid=\"queue-settings-save\"]";

#[tokio::test(flavor = "multi_thread")]
async fn queue_settings_are_edited_in_settings_and_keep_a_queued_task_waiting_across_a_restart() {
    let context = TestContext::new("queue_settings").expect("harness setup");
    let outcome = queue_settings_journey(&context).await;
    context.finish(outcome);
}

async fn queue_settings_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    // --- First run: change the settings in the form, queue a Task -------------
    let session = context.start_session("queue-settings").await?;
    let outcome = pause_promotion_and_queue_a_task(session.driver(), &agent, &repository).await;
    context.close_session(session, "queue-settings", &outcome).await?;
    let queued = outcome?;

    // The saved file, not only the screen: the section is on disk.
    let saved = std::fs::read_to_string(context.state().config_file())
        .context("the application left no config file")?;
    let saved: toml::Value = toml::from_str(&saved)?;
    let queue = saved.get("queue").context("no [queue] section was saved")?;
    if queue.get("auto_promote").and_then(|v| v.as_bool()) != Some(false)
        || queue.get("parallel_task_limit").and_then(|v| v.as_integer()) != Some(2)
    {
        bail!("the saved Queue settings are {queue}");
    }

    // --- Restart: the form still shows them, the Task still waits ------------
    let restarted = context.start_session("queue-settings-restart").await?;
    let outcome = resume_after_restart(restarted.driver(), &agent, &queued).await;
    context
        .close_session(restarted, "queue-settings-restart", &outcome)
        .await?;
    outcome
}

async fn pause_promotion_and_queue_a_task(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
) -> Result<Prerequisites> {
    open_queue_settings(driver).await?;

    // What the Queue is running under is shown before anything is edited.
    let summary = await_text(driver, SUMMARY, |t| !t.is_empty(), "the Queue summary").await?;
    if !summary.contains("start automatically") || !summary.contains("up to 3") {
        bail!("a fresh installation shows the Queue as {summary:?}");
    }

    click(driver, EDIT, "Edit Queue settings").await?;
    ui::visible(driver, MODAL).await?;
    if checked(driver, AUTO_PROMOTE_INPUT).await? != Some(true) {
        bail!("the form does not show automatic promotion on");
    }
    let limit = ui::visible(driver, LIMIT_INPUT).await?;
    limit.clear().await?;
    limit.send_keys("2").await.context("could not type the limit")?;
    click(driver, AUTO_PROMOTE_INPUT, "the auto-promote checkbox").await?;
    click(driver, SAVE, "Save").await?;
    await_absent(driver, MODAL).await?;

    await_text(driver, CURRENT_LIMIT, |t| t == "2", "the saved limit").await?;
    await_text(driver, CURRENT_AUTO_PROMOTE, |t| t == "Off", "promotion shown as off").await?;

    // The running Queue agrees with the page.
    let running = ui::invoke(driver, "get_queue_config", json!({})).await?;
    if running["auto_promote"] != false || running["parallel_task_limit"] != 2 {
        bail!("the backend runs the Queue under {running}");
    }

    let prerequisites = create_prerequisites(
        driver,
        repository,
        "Queue settings journey",
        "Queued while automatic promotion is off.",
    )
    .await?;
    open_board(driver, &prerequisites.project_id).await?;
    open_drawer(driver, &prerequisites.task_id, &prerequisites.title).await?;
    await_drawer_status(driver, "backlog").await?;
    click(driver, DRAWER_START, "Start in the drawer").await?;
    await_drawer_status(driver, "queue").await?;

    tokio::time::sleep(RESTART_WINDOW).await;
    if !agent_runs(agent)?.is_empty() {
        bail!("a queued Task started while automatic promotion was off");
    }
    await_status(driver, &prerequisites.project_id, &prerequisites.task_id, &["queue"]).await?;
    Ok(prerequisites)
}

async fn resume_after_restart(
    driver: &WebDriver,
    agent: &FakeAgent,
    queued: &Prerequisites,
) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    open_queue_settings(driver).await?;

    let summary = await_text(driver, SUMMARY, |t| !t.is_empty(), "the Queue summary").await?;
    if !summary.contains("wait until promotion is turned on") || !summary.contains("up to 2") {
        bail!("after a restart the Queue is shown as {summary:?}");
    }
    await_text(driver, CURRENT_AUTO_PROMOTE, |t| t == "Off", "promotion still off").await?;

    // Several scheduler passes after the restart: the Task has not moved.
    tokio::time::sleep(RESTART_WINDOW).await;
    if !agent_runs(agent)?.is_empty() {
        bail!("a restart started a queued Task although promotion was off");
    }
    await_status(driver, &queued.project_id, &queued.task_id, &["queue"]).await?;

    // The form opens on the saved values, and turning promotion on again is
    // what releases the Task.
    click(driver, EDIT, "Edit Queue settings").await?;
    ui::visible(driver, MODAL).await?;
    if checked(driver, AUTO_PROMOTE_INPUT).await? != Some(false) {
        bail!("the form opened with automatic promotion on");
    }
    click(driver, AUTO_PROMOTE_INPUT, "the auto-promote checkbox").await?;
    click(driver, SAVE, "Save").await?;
    await_absent(driver, MODAL).await?;
    await_text(driver, CURRENT_AUTO_PROMOTE, |t| t == "On", "promotion shown as on").await?;

    await_status(
        driver,
        &queued.project_id,
        &queued.task_id,
        &["human_review"],
    )
    .await?;
    if agent_runs(agent)?.is_empty() {
        bail!("the Task left the Queue without an agent run");
    }
    Ok(())
}

async fn open_queue_settings(driver: &WebDriver) -> Result<()> {
    page(
        driver,
        "localStorage.setItem('slashit_current_page', 'settings'); return true;",
        vec![],
    )
    .await?;
    driver.refresh().await?;
    ui::assert_frontend_is_real(driver).await?;
    click(driver, SETTINGS_QUEUE_TAB, "the Queue settings tab").await
}

async fn checked(driver: &WebDriver, selector: &str) -> Result<Option<bool>> {
    Ok(page(
        driver,
        "const e = document.querySelector(arguments[0]); return e ? e.checked : null;",
        vec![Value::String(selector.to_string())],
    )
    .await?
    .as_bool())
}
