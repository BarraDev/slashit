//! When disk space is critically low, a started task waits in the queue, the
//! board says why, and the task runs by itself once space returns.
//!
//! The application reads free space from a file the harness controls (see
//! `StateRoot::set_disk_space`), so the journey never depends on how full the
//! host's disk is.

use super::human_review::attribute;
use super::*;
use slashit_acceptance::state::PLENTY_OF_DISK;

const QUEUE_NOTICE: &str = "[data-testid=\"new-work-paused\"]";
const DRAWER_NOTICE: &str = "[data-testid=\"task-drawer-new-work-paused\"]";

const GIB: u64 = 1024 * 1024 * 1024;
/// 500 GiB with 10 GiB free: below the 40 GiB critical floor.
const CRITICAL_DISK: (u64, u64) = (500 * GIB, 10 * GIB);

#[tokio::test(flavor = "multi_thread")]
async fn a_critically_full_disk_pauses_new_work_until_space_returns() {
    let context = TestContext::new("disk_pressure").expect("harness setup");
    let outcome = disk_pressure_journey(&context).await;
    context.finish(outcome);
}

async fn disk_pressure_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();

    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());

    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    context.state().set_disk_space(CRITICAL_DISK.0, CRITICAL_DISK.1)?;

    let session = context.start_session("disk-pressure").await?;
    let outcome = wait_for_space(session.driver(), context, &agent, &repository).await;
    context.close_session(session, "disk-pressure", &outcome).await?;
    let executed = outcome?;

    assert_persisted(&root, &executed, "human_review")?;
    // One run: waiting in the queue never started a second one.
    assert_agent_roles(&agent, "disk_pressure_journey", 1, 0, 0)?;
    Ok(())
}

async fn wait_for_space(
    driver: &WebDriver,
    context: &TestContext,
    agent: &FakeAgent,
    repository: &GitFixture,
) -> Result<ExecutedTask> {
    ui::assert_frontend_is_real(driver).await?;
    let Prerequisites {
        project_id,
        task_id,
        title,
    } = create_prerequisites(
        driver,
        repository,
        "Disk pressure journey",
        "Started while the disk is critically full.",
    )
    .await?;

    open_board(driver, &project_id).await?;
    open_drawer(driver, &task_id, &title).await?;
    await_drawer_status(driver, "backlog").await?;

    // --- Start while the disk is critically full ------------------------------
    ui::visible(driver, DRAWER_START)
        .await?
        .click()
        .await
        .context("could not press Start in the drawer")?;
    await_drawer_status(driver, "queue").await?;
    let notice = await_text(
        driver,
        DRAWER_NOTICE,
        |t| t.contains("New work paused: critically low disk space"),
        "why the queued task is not starting",
    )
    .await?;
    for expected in ["10.0 GiB free", "40.0 GiB", "Settings > Storage"] {
        if !notice.contains(expected) {
            bail!("the drawer explains the pause as {notice:?}, without {expected:?}");
        }
    }
    await_text(
        driver,
        QUEUE_NOTICE,
        |t| t.contains("critically low disk space"),
        "the pause on the Queue column",
    )
    .await?;
    assert_only_status(driver).await?;

    // Several scheduler passes: the task waits, and nothing runs or fails.
    tokio::time::sleep(RESTART_WINDOW).await;
    let runs = agent_runs(agent)?.len();
    if runs != 0 {
        bail!("{runs} agent runs started while the disk was critically full");
    }
    let waiting = await_status(driver, &project_id, &task_id, &["queue"]).await?;
    if waiting.get("error_message").is_some_and(|m| !m.is_null()) {
        bail!("a paused task was given an error: {waiting}");
    }

    // A direct move into In Progress is refused, and says why.
    let refused = ui::invoke_expecting_refusal(
        driver,
        "update_task_status",
        json!({ "taskId": task_id, "status": "in_progress" }),
    )
    .await?;
    if !refused.contains("New work paused") {
        bail!("moving the task into In Progress was refused as {refused:?}");
    }
    await_status(driver, &project_id, &task_id, &["queue"]).await?;

    // --- A check that cannot be made pauses new work too ----------------------
    context.state().fail_disk_space_check()?;
    await_text(
        driver,
        DRAWER_NOTICE,
        |t| t.contains("couldn't verify free disk space"),
        "that free space could not be checked",
    )
    .await?;
    if !agent_runs(agent)?.is_empty() {
        bail!("an agent run started while free space could not be checked");
    }
    await_status(driver, &project_id, &task_id, &["queue"]).await?;

    // --- Space returns: the queue starts the task by itself -------------------
    context
        .state()
        .set_disk_space(PLENTY_OF_DISK.0, PLENTY_OF_DISK.1)?;
    await_absent(driver, QUEUE_NOTICE).await?;
    await_agent_runs(agent, 1).await?;
    let finished = await_status(driver, &project_id, &task_id, &["human_review"]).await?;
    let worktree_path = finished
        .get("worktree_path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context("the finished task records no worktree")?;

    Ok(ExecutedTask {
        id: task_id,
        project_id,
        title,
        worktree_path,
    })
}

/// The pause is status, not a prompt: nothing in either notice can be
/// pressed, and it is announced as status.
async fn assert_only_status(driver: &WebDriver) -> Result<()> {
    for notice in [QUEUE_NOTICE, DRAWER_NOTICE] {
        let controls = page(
            driver,
            "return document.querySelectorAll(arguments[0] + ' button, ' + arguments[0] + ' a').length;",
            vec![json!(notice)],
        )
        .await?;
        if controls != json!(0) {
            bail!("{notice} offers {controls} controls");
        }
        let role = attribute(driver, notice, "role").await?;
        if role.as_deref() != Some("status") {
            bail!("{notice} has role {role:?}");
        }
    }
    Ok(())
}
