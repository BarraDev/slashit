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
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir().as_os_str().to_os_string());
    context.set_child_env(fake_agent::COORDINATOR_OUTPUT_VAR, r#"{"type":"reply","text":"Fake Coordinator reply."}"#);

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
        send_message(session.driver(), "Can we keep discussing?", "Fake Coordinator reply.").await?;
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
            if !run.working_dir.starts_with(context.state().data_home().join("slashit-app")) { bail!("no-Task Coordinator ran outside SlashIt's data area: {}", run.working_dir.display()); }
            if run.args.iter().any(|arg| arg == "--dangerously-skip-permissions") { bail!("Project Coordinator received mutating tool access"); }
            if !run.args.iter().any(|arg| arg == "Read,Glob,Grep") { bail!("Project Coordinator was not restricted to read-only tools"); }
            if run.args.iter().any(|arg| arg == "--resume" || arg == "--session-id") { bail!("Coordinator run depended on provider session continuity"); }
        }
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
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir().as_os_str().to_os_string());
    context.set_child_env(fake_agent::COORDINATOR_OUTPUT_VAR, "delegate_to_task");

    let outcome = async {
        let repository = GitFixture::create(&root.join("fixture-repo"))?;
        let session = context.start_session("delegation").await?;
        let repository_id = created_id(ui::invoke(session.driver(), "create_repository", json!({"localPath":repository.path(),"remoteUrl":Value::Null,"initialize":Value::Null})).await?, "create_repository")?;
        let project_id = created_id(ui::invoke(session.driver(), "create_project", json!({"name":"Delegated Project","repositoryId":repository_id,"agentType":"claude_code"})).await?, "create_project")?;
        let task = ui::invoke(session.driver(), "create_task", json!({"params":{"projectId":project_id,"title":"Existing Task","description":"A bounded implementation task","model":"sonnet","planningMode":false,"dependencies":[]}})).await?;
        let task_id = created_id(task.clone(), "create_task")?;
        ui::invoke(session.driver(), "create_worktree", json!({"taskId":task_id})).await?;
        open_board(session.driver(), &project_id).await?;
        ui::visible(session.driver(), PANEL).await?;

        // No proposal may start the Worker. The fixture's invocation count
        // stays at the Coordinator until the exact payload is approved.
        send_message(session.driver(), "Please make a small change in Existing Task.", "Coordinator proposes Task work").await?;
        let proposal = ui::visible(session.driver(), "[data-testid=\"conversation-action-proposal\"]").await?;
        let proposal_text = proposal.text().await?;
        if !proposal_text.contains("Existing Task") || !proposal_text.contains("right execution boundary") || !proposal_text.contains("Add the approved marker.") { bail!("the exact proposal is not visible: {proposal_text}"); }
        if agent.invocations()?.iter().filter(|invocation| invocation.prompt.is_some()).count() != 1 { bail!("Worker ran before approval"); }

        let request = ui::visible(session.driver(), "[data-testid=\"conversation-action-request\"]").await?;
        request.clear().await?;
        request.send_keys("Edited authoritative request").await?;
        ui::visible(session.driver(), "[data-testid=\"conversation-action-edit-approve\"]").await?.click().await?;
        await_text(session.driver(), "[data-testid=\"conversation-worker-result\"]", "fake Worker completed approved work").await?;
        await_text(session.driver(), "[data-testid=\"conversation-message\"]", "Coordinator reviewed the Worker result.").await?;

        let conversation = ui::invoke(session.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        let conversation_id = conversation["conversation"]["id"].as_str().context("Conversation id missing")?.to_string();
        let action = conversation["conversation"]["actions"].as_array().and_then(|actions| actions.first()).context("Delegation action missing")?;
        if action["approved_request"] != "Edited authoritative request" || action["status"] != "returned" || action["coordinator_replied"] != true { bail!("approved payload/result state was not durably mediated: {action}"); }
        if conversation["coordinator_live"] != false || conversation["worker_live"] != false { bail!("completed delegation is still live"); }

        let tasks: Vec<Value> = serde_json::from_value(ui::invoke(session.driver(), "list_tasks", json!({"projectId":project_id})).await?)?;
        let persisted_task = tasks.iter().find(|task| task["id"] == task_id).context("Task disappeared")?;
        if persisted_task["status"] != "backlog" { bail!("TaskStatus was changed to represent Worker liveness: {}", persisted_task["status"]); }
        let invocations = agent.invocations()?;
        let runs = invocations.iter().filter(|invocation| invocation.prompt.is_some()).collect::<Vec<_>>();
        if runs.len() != 3 { bail!("expected Coordinator, Worker, fresh Coordinator; observed {}", runs.len()); }
        let worker = runs[1];
        if !worker.working_dir.ends_with(persisted_task["worktree_path"].as_str().unwrap_or_default()) && worker.working_dir != Path::new(persisted_task["worktree_path"].as_str().unwrap_or_default()) { bail!("Worker did not run in target Task Checkout: {}", worker.working_dir.display()); }
        let worker_prompt = worker.prompt.as_deref().unwrap_or_default();
        if !worker_prompt.contains("Edited authoritative request") || worker_prompt.contains("Please make a small change") || worker_prompt.contains("right execution boundary") { bail!("Worker context contains the wrong payload or hidden Coordinator history: {worker_prompt}"); }
        if runs.iter().any(|run| run.args.iter().any(|arg| arg == "--resume" || arg == "--session-id")) { bail!("a fresh Run depended on provider continuity"); }
        if !runs[2].prompt.as_deref().unwrap_or_default().contains("fake Worker completed approved work") { bail!("fresh Coordinator did not receive the mediated Worker result"); }

        send_message(session.driver(), "What should we do next?", "Fake Coordinator reply.").await?;
        let continued = ui::invoke(session.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        if continued["conversation"]["id"] != conversation_id { bail!("continued chat changed Conversation identity"); }
        if continued["conversation"]["entries"].as_array().map_or(0, Vec::len) < 8 { bail!("Conversation did not remain open after Worker mediation"); }
        context.close_session(session, "delegation", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    context.finish(outcome);
}

async fn send_message(driver: &WebDriver, message: &str, expected: &str) -> Result<()> {
    let input = ui::visible(driver, INPUT).await?;
    input.send_keys(message).await?;
    ui::visible(driver, SEND).await?.click().await?;
    await_text(driver, "[data-testid=\"project-conversation\"]", expected).await
}

async fn await_text(driver: &WebDriver, selector: &str, text: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        if super::text_of(driver, selector).await?.is_some_and(|actual| actual.contains(text)) { return Ok(()); }
        if started.elapsed() > EXECUTION_DEADLINE { bail!("{selector} never displayed {text:?}"); }
        tokio::time::sleep(POLL).await;
    }
}
