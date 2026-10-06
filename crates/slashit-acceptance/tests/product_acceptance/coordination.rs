use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn human_gates_delegation_and_finishes_a_persisted_round() {
    let context = TestContext::new("coordination_round").expect("harness setup");
    let outcome = async {
        let root = context.state().path();
        let agent = FakeAgent::install(root)?;
        context.set_child_env("PATH", agent.path_value());
        context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
        context.set_child_env("SLASHIT_FAKE_COORDINATION", "1");
        start_on_the_legacy_auto_placement(&context.state().config_file())?;
        let repository = GitFixture::create(&root.join("repo"))?;
        let session = context.start_session("coordination").await?;
        let outcome = journey(session.driver(), &repository, &agent).await;
        context
            .close_session(session, "coordination", &outcome)
            .await?;
        let (task, expected) = outcome?;
        let restarted = context.start_session("coordination-restart").await?;
        let outcome = async {
            let saved = ui::invoke(
                restarted.driver(),
                "get_coordination",
                json!({"taskId":task}),
            )
            .await?;
            if saved["conversation"] != expected || saved["live"] != false {
                bail!("restart changed coordination or invented a live Run: {saved}");
            }
            Ok(())
        }
        .await;
        context
            .close_session(restarted, "coordination-restart", &outcome)
            .await?;
        outcome
    }
    .await;
    context.finish(outcome);
}

async fn journey(
    driver: &WebDriver,
    repo: &GitFixture,
    agent: &FakeAgent,
) -> Result<(String, Value)> {
    let project = task_runs::register_project(driver, repo).await?;
    let (task, title) = task_runs::create_task(driver, &project, "Coordination", &[]).await?;
    let executed = task_runs::run_to_review(driver, &project, &task, &title).await?;
    show_on_board(driver, &project, HUMAN_REVIEW_COLUMN, &title).await?;
    open_drawer(driver, &task, &title).await?;
    ui::visible(driver, "[data-testid=coordination-open]")
        .await?
        .click()
        .await?;
    ui::visible(driver, "[data-testid=coordination-goal]")
        .await?
        .send_keys("Create one bounded file change")
        .await?;
    driver
        .query(By::Css("[data-testid=coordination-submit]"))
        .and_enabled()
        .first()
        .await?
        .click()
        .await?;
    wait_text(
        driver,
        "coordination-proposed",
        "Create coordination-work.txt in the Task Checkout",
    )
    .await?;
    let before = ui::invoke(driver, "get_coordination", json!({"taskId":task})).await?;
    if before["conversation"]["rounds"][0]["delegation"]["status"] != "Proposed" {
        bail!("proposal was not persisted before approval");
    }
    if executed
        .worktree_path
        .join("coordination-work.txt")
        .exists()
    {
        bail!("Worker started before human approval");
    }
    driver
        .query(By::Css("[data-testid=coordination-approve]"))
        .and_enabled()
        .first()
        .await?
        .click()
        .await?;
    wait_text(
        driver,
        "coordination-result",
        "Worker created coordination-work.txt",
    )
    .await?;
    wait_text(
        driver,
        "coordination-summary",
        "Coordinator recommends approving the completed bounded change",
    )
    .await?;
    driver
        .query(By::Css("[data-testid=coordination-finish]"))
        .and_enabled()
        .first()
        .await?
        .click()
        .await?;
    wait_text(driver, "coordination-decision", "Finish").await?;
    let saved = ui::invoke(driver, "get_coordination", json!({"taskId":task})).await?;
    let round = &saved["conversation"]["rounds"][0];
    if round["delegation"]["status"] != "Returned" || round["decision"] != "Finish" {
        bail!("persisted state differs from UI: {saved}");
    }
    let calls = agent.invocations()?;
    let coordination: Vec<_> = calls
        .iter()
        .filter(|c| c.prompt.as_ref().is_some_and(|p| p.contains("\"stage\":")))
        .collect();
    if coordination.len() != 3 {
        bail!(
            "expected exactly three fresh coordination Runs, got {}",
            coordination.len()
        );
    }
    for call in &coordination {
        if call.has_flag("--resume") || call.working_dir != executed.worktree_path {
            bail!("Run resumed provider state or used the wrong checkout");
        }
    }
    for call in &coordination[1..] {
        if call
            .prompt
            .as_ref()
            .unwrap()
            .contains("PRIVATE_COORDINATOR_HISTORY")
        {
            bail!("Coordinator history leaked into projected context");
        }
    }
    if std::fs::read_to_string(executed.worktree_path.join("coordination-work.txt"))?
        != "approved delegated work\n"
    {
        bail!("Worker did not operate in Task Checkout");
    }
    Ok((task, saved["conversation"].clone()))
}

async fn wait_text(driver: &WebDriver, testid: &str, expected: &str) -> Result<()> {
    let selector = format!("[data-testid={testid}]");
    let started = std::time::Instant::now();
    loop {
        let text = text_of(driver, &selector).await?.unwrap_or_default();
        if text.trim() == expected {
            return Ok(());
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("{selector} never showed {expected:?}; it last showed {text:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}
