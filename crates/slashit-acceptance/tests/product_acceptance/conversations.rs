//! Desktop journeys for Project-owned conversation, including the zero-Task
//! path and a separately approved Task Worker delegation.

use super::*;
use slashit_acceptance::fake_agent::{self, FakeAgent};

const PANEL: &str = "[data-testid=\"project-conversation\"]";
const INPUT: &str = "[data-testid=\"conversation-message-input\"]";
const SEND: &str = "[data-testid=\"conversation-send\"]";

#[tokio::test(flavor = "multi_thread")]
async fn zero_task_project_conversation_replies_and_survives_restart() {
    let context = TestContext::new("project_conversation_zero_tasks").expect("harness setup");
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root).expect("install fake Claude");
    context.set_child_env("PATH", agent.path_value().to_os_string());
    context.set_child_env(
        fake_agent::MARKER_DIR_VAR,
        agent.marker_dir().as_os_str().to_os_string(),
    );
    context.set_child_env(
        fake_agent::COORDINATOR_OUTPUT_VAR,
        r#"{"type":"reply","text":"Fake Coordinator reply."}"#,
    );

    let outcome = async {
        let session = context.start_session("zero-tasks").await?;
        let project_id = created_id(ui::invoke(session.driver(), "create_project", json!({
            "name":"Zero Task Conversation", "repositoryId":Value::Null, "agentType":"claude_code"
        })).await?, "create_project")?;
        open_board(session.driver(), &project_id).await?;
        ui::visible(session.driver(), PANEL).await?;
        let tasks: Vec<Value> = serde_json::from_value(ui::invoke(session.driver(), "list_tasks", json!({"projectId":project_id})).await?)?;
        if !tasks.is_empty() { bail!("the no-Task fixture unexpectedly has {} Tasks", tasks.len()); }

        send_message(session.driver(), "What is this Project?", "Fake Coordinator reply.").await?;
        submit_message(session.driver(), "Can we keep discussing?").await?;
        let second_turn = await_conversation_idle(session.driver(), &project_id, 4).await?;
        let second_reply = second_turn["conversation"]["entries"].as_array().context("Conversation entries missing after second turn")?.iter()
            .filter_map(|entry| entry["kind"]["text"].as_str())
            .filter(|text| *text == "Fake Coordinator reply.")
            .count();
        if second_reply != 2 { bail!("second Coordinator turn did not persist its reply: {second_turn}"); }
        let first = ui::invoke(session.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        let conversation_id = first["conversation"]["id"].as_str().context("Conversation id missing")?.to_string();
        if conversation_id == project_id { bail!("Conversation identity was encoded as Project identity"); }
        if first["coordinator_live"] != false || first["worker_live"] != false { bail!("a completed Coordinator was reported live"); }
        let entries = first["conversation"]["entries"].as_array().context("Conversation entries missing")?;
        if entries.len() < 4 { bail!("expected two Human messages and two replies, got {} entries", entries.len()); }
        if entries.iter().any(|entry| entry.to_string().contains("action_proposed")) { bail!("an ordinary reply unexpectedly created an action"); }

        let invocations = agent.invocations()?;
        let runs = invocations.iter().filter(|invocation| invocation.prompt.is_some()).collect::<Vec<_>>();
        if runs.len() != 2 { bail!("expected two fresh Coordinator runs, observed {}", runs.len()); }
        for run in &runs {
            let prompt = run.prompt.as_deref().unwrap_or_default();
            if !prompt.contains("\"tasks\":[]") { bail!("Coordinator projection did not prove an empty Task index: {prompt}"); }
            if run.working_dir.starts_with(context.state().data_home().join("slashit-app")) { bail!("Coordinator without a repository root was given access to SlashIt's private data directory: {}", run.working_dir.display()); }
            if run.args.iter().any(|arg| arg == "--dangerously-skip-permissions") { bail!("Project Coordinator received mutating tool access"); }
            if !run.args.iter().any(|arg| arg == "Read,Glob,Grep") { bail!("Project Coordinator was not restricted to read-only tools"); }
            if run.args.iter().any(|arg| arg == "--resume" || arg == "--session-id") { bail!("Coordinator run depended on provider session continuity"); }
        }
        let second_prompt = runs[1].prompt.as_deref().unwrap_or_default();
        let projection_json = second_prompt
            .split_once("Project context projection (JSON):\n")
            .and_then(|(_, rest)| rest.split_once("\n\nReturn exactly one JSON object").map(|(json, _)| json))
            .context("second Coordinator projection JSON missing")?;
        let projection: Value = serde_json::from_str(projection_json)?;
        if projection["current_message"] != "Can we keep discussing?" { bail!("second turn current_message was wrong: {projection}"); }
        if projection["recent_history"].to_string().contains("Can we keep discussing?") { bail!("current Human message was duplicated into recent history: {projection}"); }
        context.close_session(session, "zero-tasks", &Ok(())).await?;

        let restarted = context.start_session("zero-tasks-restart").await?;
        open_board(restarted.driver(), &project_id).await?;
        ui::visible(restarted.driver(), PANEL).await?;
        let restored = ui::invoke(restarted.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        if restored["conversation"]["id"] != conversation_id { bail!("restart changed the primary Conversation identity"); }
        if restored["coordinator_live"] != false || restored["worker_live"] != false { bail!("restart fabricated a live Run"); }
        let history = text_of(restarted.driver(), "[data-testid=\"conversation-history\"]").await?.unwrap_or_default();
        if !history.contains("What is this Project?") || !history.contains("Can we keep discussing?") || !history.contains("Fake Coordinator reply.") { bail!("reopened Conversation did not render its persisted history: {history}"); }
        context.close_session(restarted, "zero-tasks-restart", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    context.finish(outcome);
}

#[tokio::test(flavor = "multi_thread")]
async fn project_conversation_human_gates_a_worker_and_mediates_its_result() {
    let context = TestContext::new("project_conversation_delegation").expect("harness setup");
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root).expect("install fake Claude");
    context.set_child_env("PATH", agent.path_value().to_os_string());
    context.set_child_env(
        fake_agent::MARKER_DIR_VAR,
        agent.marker_dir().as_os_str().to_os_string(),
    );
    context.set_child_env(fake_agent::COORDINATOR_OUTPUT_VAR, "delegate_to_task");
    context.set_child_env(fake_agent::COORDINATOR_DELEGATE_POSITION_VAR, "3");
    let blocked_runs = agent
        .block_agent_runs()
        .expect("create deterministic run gates");
    context.set_child_env(fake_agent::BLOCK_DIR_VAR, blocked_runs.into_os_string());
    // Fail the first Coordinator run after the Worker result. The saved result
    // must remain visible and an explicit fresh retry must not rerun the Worker.
    context.set_child_env(fake_agent::COORDINATOR_FAIL_AFTER_WORKER_VAR, "1");

    let outcome = async {
        let repository = GitFixture::create(&root.join("fixture-repo"))?;
        let session = context.start_session("delegation").await?;
        let repository_id = created_id(ui::invoke(session.driver(), "create_repository", json!({"localPath":repository.path(),"remoteUrl":Value::Null,"initialize":Value::Null})).await?, "create_repository")?;
        let project_id = created_id(ui::invoke(session.driver(), "create_project", json!({"name":"Delegated Project","repositoryId":repository_id,"agentType":"claude_code"})).await?, "create_project")?;
        let task = ui::invoke(session.driver(), "create_task", json!({"params":{"projectId":project_id,"title":"Existing Task","description":"A bounded implementation task","model":"sonnet","planningMode":false,"dependencies":[]}})).await?;
        let task_id = created_id(task.clone(), "create_task")?;
        ui::invoke(session.driver(), "create_worktree", json!({"taskId":task_id})).await?;
        let target_config = agent.marker_dir().join(".coordinator-target");
        std::fs::create_dir_all(&target_config)?;
        let stale_target = ui::invoke(session.driver(), "create_task", json!({"params":{"projectId":project_id,"title":"Task to remove before rejection","description":"A stale proposal target","model":"sonnet","planningMode":false,"dependencies":[]}})).await?;
        let stale_target_id = created_id(stale_target, "create stale proposal target")?;
        std::fs::write(target_config.join("id"), &task_id)?;
        let unrelated = ui::invoke(session.driver(), "create_task", json!({"params":{"projectId":project_id,"title":"Unrelated private Task sentinel","description":"Must not enter the Worker projection","model":"sonnet","planningMode":false,"dependencies":[]}})).await?;
        let unrelated_task_id = created_id(unrelated, "create_task")?;
        open_board(session.driver(), &project_id).await?;
        ui::visible(session.driver(), PANEL).await?;

        // No proposal may start the Worker. The fixture's invocation count
        // stays at the Coordinator until the exact payload is approved.
        submit_message(session.driver(), "Please make a small change in Existing Task.").await?;
        await_and_release_provider_run(&agent).await?;
        await_text(session.driver(), PANEL, "Coordinator proposes Task work").await?;
        let proposal = ui::visible(session.driver(), "[data-testid=\"conversation-action-proposal\"]").await?;
        let proposal_text = proposal.text().await?;
        if !proposal_text.contains("Existing Task") || !proposal_text.contains(&task_id) || !proposal_text.contains("right execution boundary") { bail!("the exact target and explanation are not visible: {proposal_text}"); }
        ui::visible(session.driver(), "[data-testid=\"conversation-action-request\"]").await?;
        if field_value(session.driver(), "[data-testid=\"conversation-action-request\"]").await? != "Add the approved marker." { bail!("the exact proposed request is not visible in its editable field"); }
        if agent.invocations()?.iter().filter(|invocation| invocation.prompt.is_some()).count() != 1 { bail!("Worker ran before approval"); }

        // A normal rejection is durable and must never start a Worker.
        ui::visible(session.driver(), "[data-testid=\"conversation-action-reject\"]").await?.click().await?;
        let rejected = ui::invoke(session.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        if rejected["conversation"]["actions"].as_array().and_then(|actions| actions.first()).is_none_or(|action| action["status"] != "rejected") { bail!("rejection was not persisted"); }
        if agent.invocations()?.iter().filter(|invocation| invocation.prompt.is_some()).count() != 1 { bail!("reject started a Worker"); }

        // A stale proposal remains rejectable after its target disappears.
        std::fs::write(target_config.join("id"), &stale_target_id)?;
        submit_message(session.driver(), "Please propose the Task work again.").await?;
        await_and_release_provider_run(&agent).await?;
        await_text(session.driver(), PANEL, "Coordinator proposes Task work").await?;
        let proposal = ui::visible(session.driver(), "[data-testid=\"conversation-action-proposal\"]").await?;
        let proposal_text = proposal.text().await?;
        if !proposal_text.contains("Task to remove before rejection") || !proposal_text.contains(&stale_target_id) { bail!("the stale proposal target is not visible: {proposal_text}"); }
        ui::invoke(session.driver(), "delete_task", json!({"taskId":stale_target_id})).await?;
        ui::visible(session.driver(), "[data-testid=\"conversation-action-reject\"]").await?.click().await?;
        let stale_rejected = ui::invoke(session.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        if stale_rejected["conversation"]["actions"].as_array().is_none_or(|actions| !actions.iter().any(|action| action["target_task_id"] == stale_target_id && action["status"] == "rejected")) { bail!("deleted proposal target could not be rejected: {stale_rejected}"); }
        if agent.invocations()?.iter().filter(|invocation| invocation.prompt.is_some()).count() != 2 { bail!("stale-target rejection started a Worker"); }

        std::fs::write(target_config.join("id"), &task_id)?;
        submit_message(session.driver(), "Please propose the Task work a third time.").await?;
        await_and_release_provider_run(&agent).await?;
        await_text(session.driver(), PANEL, "Coordinator proposes Task work").await?;
        let proposal = ui::visible(session.driver(), "[data-testid=\"conversation-action-proposal\"]").await?;
        let proposal_text = proposal.text().await?;
        if !proposal_text.contains("Existing Task") || !proposal_text.contains(&task_id) { bail!("the third exact proposal target is not visible: {proposal_text}"); }
        ui::visible(session.driver(), "[data-testid=\"conversation-action-request\"]").await?;
        if field_value(session.driver(), "[data-testid=\"conversation-action-request\"]").await? != "Add the approved marker." { bail!("the third exact proposed request is not visible in its editable field"); }
        if agent.invocations()?.iter().filter(|invocation| invocation.prompt.is_some()).count() != 3 { bail!("third proposal started a Worker before approval"); }

        let request = ui::visible(session.driver(), "[data-testid=\"conversation-action-request\"]").await?;
        request.clear().await?;
        request.send_keys("Edited authoritative request").await?;
        ui::visible(session.driver(), "[data-testid=\"conversation-action-edit-approve\"]").await?.click().await?;
        await_and_release_provider_run(&agent).await?; // Worker run
        await_returned_worker_result(session.driver(), &project_id).await?;
        await_blocked_provider_run(&agent).await?; // Coordinator after durable Worker result
        let result_while_mediating = ui::invoke(session.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        if result_while_mediating["conversation"]["actions"].as_array().is_none_or(|actions| !actions.iter().any(|action| action["status"] == "returned" && action["worker_result"].as_str().is_some())) { bail!("Worker result was not durably visible while Coordinator mediation was blocked: {result_while_mediating}"); }
        agent.release_blocked_runs()?; // First follow-up fails by fixture instruction.
        await_text(session.driver(), "[data-testid=\"conversation-worker-result\"]", "fake Worker completed approved work").await?;
        ui::visible(session.driver(), "[data-testid=\"conversation-continuation-error\"]").await?;
        if ui::visible(session.driver(), SEND).await?.is_enabled().await? { bail!("Human send remained enabled while a saved Worker result awaited Coordinator mediation"); }
        if !super::text_of(session.driver(), "[data-testid=\"conversation-history\"]").await?.unwrap_or_default().contains("fake Worker completed approved work") {
            bail!("a failed Coordinator follow-up hid the durable Worker result");
        }
        let before_explicit_retry = agent.invocations()?.iter().filter(|invocation| invocation.prompt.is_some()).count();
        for _ in 0..3 {
            let pending = ui::invoke(session.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
            if pending["conversation"]["actions"].as_array().is_none_or(|actions| !actions.iter().any(|action| action["status"] == "returned" && action["coordinator_replied"] == false)) { bail!("GET did not preserve the visible unmediated result: {pending}"); }
        }
        let after_reads = agent.invocations()?.iter().filter(|invocation| invocation.prompt.is_some()).count();
        if after_reads != before_explicit_retry { bail!("GET unexpectedly launched provider work ({before_explicit_retry} -> {after_reads})"); }
        ui::visible(session.driver(), "[data-testid=\"conversation-retry-coordinator\"]").await?.click().await?;
        await_blocked_provider_run(&agent).await?;
        ui::visible(session.driver(), "[data-testid=\"conversation-stop\"]").await?.click().await?;
        let stopped = await_conversation_idle(session.driver(), &project_id, 1).await?;
        if stopped["conversation"]["actions"].as_array().is_none_or(|actions| !actions.iter().any(|action| action["status"] == "returned" && action["coordinator_replied"] == false)) {
            bail!("stopping Coordinator mediation did not preserve the returned Worker result: {stopped}");
        }
        await_enabled(session.driver(), "[data-testid=\"conversation-retry-coordinator\"]").await?.click().await?;
        await_and_release_provider_run(&agent).await?;
        await_text(session.driver(), "[data-testid=\"conversation-history\"]", "Coordinator reviewed the Worker result.").await?;
        let worker_runs = agent.invocations()?.iter().filter(|run| run.prompt.as_deref().is_some_and(|prompt| prompt.contains("Execute only approved_request in this Task Checkout"))).count();
        if worker_runs != 1 { bail!("Coordinator retry started the Worker {worker_runs} times"); }


        let conversation = ui::invoke(session.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        let conversation_id = conversation["conversation"]["id"].as_str().context("Conversation id missing")?.to_string();
        let actions = conversation["conversation"]["actions"].as_array().context("Delegation actions missing")?;
        if actions.iter().any(|action| action["target_task_id"] == unrelated_task_id) { bail!("the unrelated Task was targeted"); }
        let action = actions.iter().find(|action| action["status"] == "returned").context("Returned delegation action missing")?;
        if action["approved_request"] != "Edited authoritative request" || action["status"] != "returned" || action["coordinator_replied"] != true { bail!("approved payload/result state was not durably mediated: {action}"); }
        if conversation["coordinator_live"] != false || conversation["worker_live"] != false { bail!("completed delegation is still live"); }

        let tasks: Vec<Value> = serde_json::from_value(ui::invoke(session.driver(), "list_tasks", json!({"projectId":project_id})).await?)?;
        let persisted_task = tasks.iter().find(|task| task["id"] == task_id).context("Task disappeared")?;
        if persisted_task["status"] != "backlog" { bail!("TaskStatus was changed to represent Worker liveness: {}", persisted_task["status"]); }
        let invocations = agent.invocations()?;
        let runs = invocations.iter().filter(|invocation| invocation.prompt.is_some()).collect::<Vec<_>>();
        if runs.len() != 7 { bail!("expected three proposal Coordinators, Worker, failed Coordinator, stopped Coordinator and successful retry; observed {}", runs.len()); }
        let worker = runs[3];
        if !worker.working_dir.ends_with(persisted_task["worktree_path"].as_str().unwrap_or_default()) && worker.working_dir != Path::new(persisted_task["worktree_path"].as_str().unwrap_or_default()) { bail!("Worker did not run in target Task Checkout: {}", worker.working_dir.display()); }
        let worker_prompt = worker.prompt.as_deref().unwrap_or_default();
        if !worker_prompt.contains("Edited authoritative request") || worker_prompt.contains("Please make a small change") || worker_prompt.contains("right execution boundary") || worker_prompt.contains("Unrelated private Task sentinel") || worker_prompt.contains("Must not enter the Worker projection") { bail!("Worker context contains the wrong payload, hidden Coordinator history, or unrelated Task data: {worker_prompt}"); }
        if runs.iter().any(|run| run.args.iter().any(|arg| arg == "--resume" || arg == "--session-id")) { bail!("a fresh Run depended on provider continuity"); }
        if !runs[6].prompt.as_deref().unwrap_or_default().contains("fake Worker completed approved work") { bail!("fresh Coordinator retry did not receive the persisted Worker result"); }

        submit_message(session.driver(), "What should we do next?").await?;
        await_provider_invocation_count(&agent, 8).await?;
        let final_run_count = agent
            .invocations()?
            .iter()
            .filter(|invocation| invocation.prompt.is_some())
            .count();
        if final_run_count != 8 {
            bail!("the continued Human turn started an unexpected number of provider Runs: {final_run_count}");
        }
        let continued = await_conversation_idle(session.driver(), &project_id, 8).await?;
        let entries = continued["conversation"]["entries"].as_array().context("continued Conversation entries missing")?;
        let mediator_replies = entries.iter().filter(|entry| entry["kind"]["text"] == "Coordinator reviewed the Worker result.").count();
        let ordinary_replies = entries.iter().filter(|entry| entry["kind"]["text"] == "Fake Coordinator reply.").count();
        if mediator_replies != 1 || ordinary_replies != 1 {
            bail!("Conversation did not persist both the mediated reply and the later ordinary reply: {continued}");
        }
        await_text(session.driver(), "[data-testid=\"conversation-history\"]", "Fake Coordinator reply.").await?;
        if continued["conversation"]["id"] != conversation_id { bail!("continued chat changed Conversation identity"); }
        if entries.len() < 8 { bail!("Conversation did not remain open after Worker mediation"); }
        context.close_session(session, "delegation", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    context.finish(outcome);
}

async fn send_message(driver: &WebDriver, message: &str, expected: &str) -> Result<()> {
    submit_message(driver, message).await?;
    await_text(driver, "[data-testid=\"project-conversation\"]", expected).await
}

async fn submit_message(driver: &WebDriver, message: &str) -> Result<()> {
    let input = ui::visible(driver, INPUT).await?;
    input.send_keys(message).await?;
    ui::visible(driver, SEND).await?.click().await?;
    Ok(())
}

async fn await_blocked_provider_run(agent: &FakeAgent) -> Result<()> {
    let started = Instant::now();
    loop {
        if agent
            .blocked_pids()?
            .into_iter()
            .any(|pid| agent.is_running(pid))
        {
            return Ok(());
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("fake provider did not reach its deterministic blocking gate");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn await_provider_invocation_count(agent: &FakeAgent, expected: usize) -> Result<()> {
    let started = Instant::now();
    loop {
        let count = agent
            .invocations()?
            .iter()
            .filter(|invocation| invocation.prompt.is_some())
            .count();
        if count >= expected {
            return Ok(());
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("expected {expected} provider invocations, observed {count}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn await_enabled(driver: &WebDriver, selector: &str) -> Result<WebElement> {
    let started = Instant::now();
    loop {
        let element = ui::visible(driver, selector).await?;
        if element.is_enabled().await? {
            return Ok(element);
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("{selector} stayed disabled past the acceptance deadline");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn await_returned_worker_result(driver: &WebDriver, project_id: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let snapshot = ui::invoke(
            driver,
            "get_project_conversation",
            json!({"projectId":project_id}),
        )
        .await?;
        let returned = snapshot["conversation"]["actions"]
            .as_array()
            .is_some_and(|actions| {
                actions.iter().any(|action| {
                    action["status"] == "returned"
                        && action["worker_result"].as_str().is_some()
                })
            });
        if returned {
            return Ok(());
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("Worker did not durably return a result before Coordinator mediation");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn await_and_release_provider_run(agent: &FakeAgent) -> Result<()> {
    await_blocked_provider_run(agent).await?;
    let released = agent.release_blocked_runs()?;
    if released != 1 {
        bail!("expected one serial provider run at its gate, released {released}");
    }
    Ok(())
}

async fn await_conversation_idle(
    driver: &WebDriver,
    project_id: &str,
    minimum_entries: usize,
) -> Result<Value> {
    let started = Instant::now();
    loop {
        let snapshot = ui::invoke(
            driver,
            "get_project_conversation",
            json!({"projectId":project_id}),
        )
        .await?;
        let entries = snapshot["conversation"]["entries"]
            .as_array()
            .map_or(0, Vec::len);
        if entries >= minimum_entries
            && snapshot["coordinator_live"] == false
            && snapshot["worker_live"] == false
        {
            return Ok(snapshot);
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("Conversation did not persist {minimum_entries} entries and become idle: {snapshot}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn field_value(driver: &WebDriver, selector: &str) -> Result<String> {
    Ok(driver
        .execute(
            "return document.querySelector(arguments[0]).value;",
            vec![Value::String(selector.to_owned())],
        )
        .await?
        .json()
        .as_str()
        .context("editable field value missing")?
        .to_owned())
}

async fn await_text(driver: &WebDriver, selector: &str, text: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let matches = super::page(
            driver,
            "return Array.from(document.querySelectorAll(arguments[0]), element => element.textContent || '');",
            vec![Value::String(selector.to_owned())],
        ).await?;
        let shown = matches.as_array().is_some_and(|elements| {
            elements
                .iter()
                .filter_map(Value::as_str)
                .any(|element| element.contains(text))
        });
        if shown {
            return Ok(());
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("{selector} never displayed {text:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}
