// SPDX-License-Identifier: MPL-2.0
//! Run now is operated from the card's menu, with the keyboard and with a
//! double click.
//!
//! With the only run slot held by another Task, a keyboard Run now on a queued
//! Task is refused: the Task stays in the Queue, the Task that holds the slot
//! is untouched, and no agent starts. After the slot frees, a double click on
//! Run now promotes the queued Task exactly once, and its run reaches Human
//! Review. The journey only touches the page and the product's own commands.

use super::*;

const TOAST: &str = "[data-testid=\"toast\"]";
const RUN_NOW: &str = "[data-testid=\"task-menu-run-now\"]";

#[tokio::test(flavor = "multi_thread")]
async fn run_now_refuses_a_full_queue_by_keyboard_and_runs_a_queued_task_once_by_double_click() {
    let context = TestContext::new("run_now").expect("harness setup");
    let outcome = run_now_journey(&context).await;
    context.finish(outcome);
}

async fn run_now_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root)?;
    // Runs stay open until released, so the holder really occupies the slot
    // while the target is refused.
    let release = agent.block_agent_runs()?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_agent::BLOCK_DIR_VAR, &release);
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("run-now").await?;
    let outcome = run_now_from_the_menu(session.driver(), &agent, &repository).await;
    if outcome.is_err() {
        // A blocked run outlives the application; let it end so the failure
        // reported is the journey's own.
        let _ = agent.release_blocked_runs();
    }
    context.close_session(session, "run-now", &outcome).await?;
    let finished = outcome?;

    assert_persisted(&root, &finished, "human_review")?;
    // The holder's run and the target's run, and nothing else: the refused
    // Run now and the double click started no extra agent.
    assert_agent_roles(&agent, "run_now_journey", 2, 0, 0)?;
    Ok(())
}

async fn run_now_from_the_menu(
    driver: &WebDriver,
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
        "Run now target",
        "Run now from its card menu while another task holds the only slot.",
    )
    .await?;
    let holder = created_id(
        ui::invoke(
            driver,
            "create_task",
            json!({
                "params": {
                    "projectId": project_id,
                    "title": format!("Run now holder for {title}"),
                    "description": "Holds the only execution slot until it is released.",
                    "model": "default",
                    "planningMode": false,
                    "dependencies": [],
                }
            }),
        )
        .await?,
        "create_task",
    )?;

    // One slot, and nothing promotes on its own: every start in this journey
    // is an explicit Run now or the one the holder is given.
    ui::invoke(
        driver,
        "update_queue_config",
        json!({ "parallelTaskLimit": 1, "autoPromote": false }),
    )
    .await?;
    for id in [&holder, &task_id] {
        ui::invoke(
            driver,
            "reorder_task",
            json!({
                "taskId": id,
                "newStatus": "queue",
                "newPosition": 0,
                "expectedStatus": "backlog",
            }),
        )
        .await?;
    }
    ui::invoke(
        driver,
        "run_queued_task",
        json!({ "taskId": holder, "expectedStatus": "queue" }),
    )
    .await?;
    await_agent_runs(agent, 1).await?;
    await_status(driver, &project_id, &holder, &["in_progress"]).await?;

    open_board(driver, &project_id).await?;
    open_card_menu(driver, &title).await?;

    // --- Keyboard: a refused Run now leaves the Task queued -------------------
    ui::visible(driver, RUN_NOW)
        .await
        .context("a queued Task offers Run now in its menu")?;
    focus(driver, RUN_NOW).await?;
    driver
        .active_element()
        .await
        .context("nothing has keyboard focus")?
        .send_keys(Key::Enter)
        .await
        .context("could not press Run now with the keyboard")?;
    let reason = await_text(
        driver,
        TOAST,
        |text| text.contains("run slots are in use"),
        "the refusal for a full Queue",
    )
    .await?;
    if !reason.contains(&title) {
        bail!("the refusal names {reason:?} instead of the targeted Task {title:?}");
    }

    let target = await_status(driver, &project_id, &task_id, &["queue"]).await?;
    let holding = await_status(driver, &project_id, &holder, &["in_progress"]).await?;
    if target.get("title").and_then(Value::as_str) != Some(title.as_str()) {
        bail!("the Run now request reached a Task other than {title:?}");
    }
    if status_of(&holding) != Some("in_progress") {
        bail!(
            "the Task holding the slot changed to {:?}",
            status_of(&holding)
        );
    }
    if agent_runs(agent)?.len() != 1 {
        bail!(
            "{} agent runs started while the only slot was taken, expected just the holder's",
            agent_runs(agent)?.len()
        );
    }

    // --- Release the slot; a double click promotes the target exactly once --
    let released = agent.release_blocked_runs()?;
    if released != 1 {
        bail!("the release reached {released} blocked runs, expected the holder's");
    }
    release_until(driver, agent, &project_id, &holder, "human_review").await?;
    open_board(driver, &project_id).await?;
    open_card_menu(driver, &title).await?;
    ui::visible(driver, RUN_NOW)
        .await
        .context("the queued Task offers Run now once the slot is free")?;
    page(
        driver,
        r#"
        const button = document.querySelector('[data-testid="task-menu-run-now"]');
        button.click();
        button.click();
        return true;
        "#,
        vec![],
    )
    .await?;

    await_agent_runs(agent, 2).await?;
    // A moment for a duplicate request to reach the executor, if one were sent.
    tokio::time::sleep(POLL).await;
    if agent_runs(agent)?.len() != 2 {
        bail!(
            "the double click started {} runs, expected the holder's and the target's",
            agent_runs(agent)?.len()
        );
    }

    let finished = release_until(driver, agent, &project_id, &task_id, "human_review").await?;
    if agent_runs(agent)?.len() != 2 {
        bail!(
            "the target's run changed the run count to {}",
            agent_runs(agent)?.len()
        );
    }

    let worktree_path = finished
        .get("worktree_path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context("the finished Task records no worktree")?;
    Ok(ExecutedTask {
        id: task_id,
        project_id,
        title,
        worktree_path,
    })
}

/// Release the blocked runs one step at a time until the Task shows `status`.
/// A run blocks again at each step, so a single release is not enough.
async fn release_until(
    driver: &WebDriver,
    agent: &FakeAgent,
    project_id: &str,
    task_id: &str,
    status: &str,
) -> Result<Value> {
    let started = Instant::now();
    loop {
        agent.release_blocked_runs()?;
        let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
        if let Some(task) = find_task(&listed, task_id) {
            match status_of(&task) {
                Some(current) if current == status => return Ok(task),
                Some("error") => bail!("the task failed: {task}"),
                _ => {}
            }
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("the task never reached {status}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn focus(driver: &WebDriver, selector: &str) -> Result<()> {
    page(
        driver,
        "document.querySelector(arguments[0]).focus(); return true;",
        vec![Value::String(selector.to_string())],
    )
    .await?;
    Ok(())
}

async fn open_card_menu(driver: &WebDriver, title: &str) -> Result<()> {
    let opened = page(
        driver,
        r#"
        const [title] = arguments;
        for (const card of document.querySelectorAll('[data-testid="task-card"]')) {
            const t = card.querySelector('[data-testid="task-title"]');
            if (t && t.textContent === title) {
                card.parentElement.querySelector('[data-testid="task-card-menu"]').click();
                return true;
            }
        }
        return false;
        "#,
        vec![Value::String(title.to_string())],
    )
    .await?;
    if opened != Value::Bool(true) {
        bail!("no card titled {title:?} to open a menu on");
    }
    ui::visible(driver, "[data-testid=\"task-menu-move-to\"]")
        .await
        .context("the card menu did not open")?;
    Ok(())
}
