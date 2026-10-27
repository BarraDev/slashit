// SPDX-License-Identifier: MPL-2.0
//! Desktop journeys for Project-owned conversation, including the zero-Task
//! path, the refusal of retired direct delegation, and approved Task execution.

use super::*;
use super::task_runs::{create_task, register_project};
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
        open_conversations(session.driver(), &project_id).await?;
        open_nav(session.driver(), "dashboard", KANBAN_BOARD).await?;
        if text_of(session.driver(), PANEL).await?.is_some() { bail!("the Project conversation is still rendered on the Kanban page"); }
        open_nav(session.driver(), "conversations", PANEL).await?;
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
        open_conversations(restarted.driver(), &project_id).await?;
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
async fn project_conversation_refuses_retired_direct_delegation_and_stays_usable() {
    let context = TestContext::new("project_conversation_retired_delegation").expect("harness setup");
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
        let repository = GitFixture::create(&root.join("fixture-repo"))?;
        let session = context.start_session("retired-delegation").await?;
        let repository_id = created_id(ui::invoke(session.driver(), "create_repository", json!({"localPath":repository.path(),"remoteUrl":Value::Null,"initialize":Value::Null})).await?, "create_repository")?;
        let project_id = created_id(ui::invoke(session.driver(), "create_project", json!({"name":"Retired Delegation Project","repositoryId":repository_id,"agentType":"claude_code"})).await?, "create_project")?;
        let task_id = created_id(ui::invoke(session.driver(), "create_task", json!({"params":{"projectId":project_id,"title":"Existing Task","description":"A bounded implementation task","model":"sonnet","planningMode":false,"dependencies":[]}})).await?, "create_task")?;
        ui::invoke(session.driver(), "create_worktree", json!({"taskId":task_id})).await?;
        // The first Coordinator turn still emits the retired delegation. The
        // fixture substitutes the Task id that the delegation names.
        let scripted = agent.marker_dir().join(".coordinator-output");
        std::fs::create_dir_all(&scripted)?;
        std::fs::write(scripted.join("1"), "delegate_to_task")?;
        let target_config = agent.marker_dir().join(".coordinator-target");
        std::fs::create_dir_all(&target_config)?;
        std::fs::write(target_config.join("id"), &task_id)?;
        open_conversations(session.driver(), &project_id).await?;

        submit_message(session.driver(), "Please make a small change in Existing Task.").await?;
        await_text(session.driver(), "[data-testid=\"conversation-run-failed\"]", "Direct delegation to a Task Worker is retired").await?;
        if text_of(session.driver(), "[data-testid=\"conversation-action-proposal\"]").await?.is_some() {
            bail!("the refused delegation still offered a proposal card");
        }
        let refused = ui::invoke(session.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        if refused["conversation"]["actions"].as_array().is_some_and(|actions| !actions.is_empty()) { bail!("a refused delegation recorded an action: {refused}"); }
        if refused["coordinator_live"] != false || refused["worker_live"] != false { bail!("the refusal left a Run live"); }

        // The Conversation stays usable after the refusal.
        send_message(session.driver(), "What can we do instead?", "Fake Coordinator reply.").await?;
        context.close_session(session, "retired-delegation", &Ok(())).await?;

        let restarted = context.start_session("retired-delegation-restart").await?;
        open_conversations(restarted.driver(), &project_id).await?;
        let restored = ui::invoke(restarted.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        if restored["conversation"]["actions"].as_array().is_some_and(|actions| !actions.is_empty()) { bail!("restart created a legacy action: {restored}"); }
        let history = text_of(restarted.driver(), "[data-testid=\"conversation-history\"]").await?.unwrap_or_default();
        if !history.contains("Direct delegation to a Task Worker is retired") || !history.contains("What can we do instead?") { bail!("the refusal did not survive restart: {history}"); }

        // No run ever executed the Task Worker prompt, in any Task Checkout.
        let worker_runs = agent.invocations()?.iter()
            .filter(|run| run.prompt.as_deref().is_some_and(|prompt| prompt.contains("Execute only approved_request in this Task Checkout")))
            .count();
        if worker_runs != 0 { bail!("a retired delegation started {worker_runs} Task Worker runs"); }
        let tasks: Vec<Value> = serde_json::from_value(ui::invoke(restarted.driver(), "list_tasks", json!({"projectId":project_id})).await?)?;
        let persisted_task = tasks.iter().find(|task| task["id"] == task_id).context("Task disappeared")?;
        if persisted_task["status"] != "backlog" { bail!("the refused delegation changed the Task status: {}", persisted_task["status"]); }
        context.close_session(restarted, "retired-delegation-restart", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    context.finish(outcome);
}

#[tokio::test(flavor = "multi_thread")]
async fn coordinator_creates_and_edits_tasks_only_after_human_approval() {
    let context = TestContext::new("project_conversation_task_mutations").expect("harness setup");
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root).expect("install fake Claude");
    context.set_child_env("PATH", agent.path_value().to_os_string());
    context.set_child_env(
        fake_agent::MARKER_DIR_VAR,
        agent.marker_dir().as_os_str().to_os_string(),
    );
    // Each Coordinator run answers with the output scripted for its position.
    let scripted = agent.marker_dir().join(".coordinator-output");
    std::fs::create_dir_all(&scripted).expect("script directory");
    std::fs::write(scripted.join("1"), r#"{"type":"create_task","text":"This looks worth tracking.","title":"Rejected idea","description":"Nobody asked for this."}"#).unwrap();
    std::fs::write(scripted.join("2"), r#"{"type":"create_task","text":"Let us track the dark theme.","title":"Add dark mode","description":"Support a dark theme","priority":"high","category":"ui_ux"}"#).unwrap();

    let outcome = async {
        let session = context.start_session("task-mutations").await?;
        let project_id = created_id(ui::invoke(session.driver(), "create_project", json!({
            "name":"Coordinator Task Mutations", "repositoryId":Value::Null, "agentType":"claude_code"
        })).await?, "create_project")?;
        open_conversations(session.driver(), &project_id).await?;
        let list_tasks = || async { anyhow::Ok(serde_json::from_value::<Vec<Value>>(ui::invoke(session.driver(), "list_tasks", json!({"projectId":project_id})).await?)?) };
        let proposal = "[data-testid=\"project-action-proposal\"]";

        // 1. A rejected proposal changes nothing, and the Human saw what it would do.
        submit_message(session.driver(), "We should capture some ideas.").await?;
        await_text(session.driver(), proposal, "Rejected idea").await?;
        let shown = ui::visible(session.driver(), proposal).await?.text().await?;
        // The action label is upper-cased by CSS, and WebDriver returns rendered text.
        if !shown.to_lowercase().contains("create task") || !shown.contains("Backlog") || !shown.contains("Nobody asked for this.") { bail!("the exact creation is not visible: {shown}"); }
        if !list_tasks().await?.is_empty() { bail!("a proposal created a Task before approval"); }
        ui::visible(session.driver(), "[data-testid=\"project-action-reject\"]").await?.click().await?;
        await_text(session.driver(), "[data-testid=\"conversation-history\"]", "You rejected the proposal.").await?;
        if !list_tasks().await?.is_empty() { bail!("rejecting a proposal created a Task"); }

        // 2. An approved creation yields one ordinary Backlog Task on the board.
        submit_message(session.driver(), "Then add dark mode.").await?;
        await_text(session.driver(), proposal, "Add dark mode").await?;
        let shown = ui::visible(session.driver(), proposal).await?.text().await?;
        if !shown.contains("high") || !shown.contains("ui ux") { bail!("proposed metadata is not visible: {shown}"); }
        ui::visible(session.driver(), "[data-testid=\"project-action-approve\"]").await?.click().await?;
        await_text(session.driver(), "[data-testid=\"conversation-history\"]", "Task created in Backlog: Add dark mode").await?;
        await_board_task_title(session.driver(), "Add dark mode").await?;
        let tasks = list_tasks().await?;
        if tasks.len() != 1 { bail!("expected exactly one created Task, found {}", tasks.len()); }
        let created = tasks[0].clone();
        let task_id = created["id"].as_str().context("created Task id")?.to_string();
        if created["status"] != "backlog" || created["priority"] != "high" || created["category"] != "ui_ux" || created["project_id"] != project_id.as_str() { bail!("the created Task is not the approved Backlog Task: {created}"); }
        if !created["worktree_path"].is_null() || !created["branch_name"].is_null() || created["phase"] != "idle" { bail!("creating a Task started execution state: {created}"); }

        // 3. An approved edit changes exactly the proposed fields.
        std::fs::write(scripted.join("3"), format!(r#"{{"type":"edit_task","text":"Sharpen the wording.","target_task_id":"{task_id}","title":"Add a dark mode toggle","priority":"urgent"}}"#))?;
        submit_message(session.driver(), "Make that more precise and urgent.").await?;
        await_text(session.driver(), proposal, "Add a dark mode toggle").await?;
        let shown = ui::visible(session.driver(), proposal).await?.text().await?;
        // The action label is upper-cased by CSS too, so compare it case-insensitively.
        let lowered = shown.to_lowercase();
        if !lowered.contains("edit task") || !shown.contains("Add dark mode → Add a dark mode toggle") || !shown.contains("high → urgent") || lowered.contains("description") { bail!("the exact edit is not visible: {shown}"); }
        ui::visible(session.driver(), "[data-testid=\"project-action-approve\"]").await?.click().await?;
        await_text(session.driver(), "[data-testid=\"conversation-history\"]", "Task updated: Add dark mode").await?;
        await_board_task_title(session.driver(), "Add a dark mode toggle").await?;
        let edited = list_tasks().await?.into_iter().next().context("edited Task missing")?;
        if edited["title"] != "Add a dark mode toggle" || edited["priority"] != "urgent" || edited["category"] != "ui_ux" || edited["description"] != created["description"] || edited["status"] != "backlog" { bail!("the edit changed more or less than approved: {edited}"); }

        // Nothing above may have started anything but the three Coordinator runs.
        let invocations = agent.invocations()?;
        let runs = invocations.iter().filter(|run| run.prompt.is_some()).collect::<Vec<_>>();
        if runs.len() != 3 || runs.iter().any(|run| !run.prompt.as_deref().unwrap_or_default().contains("\"current_message\"")) { bail!("a Task operation started a Worker or agent Run: {} runs", runs.len()); }
        context.close_session(session, "task-mutations", &Ok(())).await?;

        // 4. After a restart the outcomes are durable and a replayed approval is inert.
        let restarted = context.start_session("task-mutations-restart").await?;
        open_conversations(restarted.driver(), &project_id).await?;
        let snapshot = ui::invoke(restarted.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        let statuses = snapshot["conversation"]["project_actions"].as_array().context("project actions missing")?.iter().map(|action| action["status"].as_str().unwrap_or_default().to_string()).collect::<Vec<_>>();
        if statuses != ["rejected", "applied", "applied"] { bail!("project action outcomes did not survive restart: {statuses:?}"); }
        await_text(restarted.driver(), "[data-testid=\"conversation-history\"]", "Task created in Backlog: Add dark mode").await?;
        let controls = super::page(restarted.driver(), "return document.querySelectorAll('[data-testid=project-action-approve], [data-testid=project-action-reject]').length;", vec![]).await?;
        if controls != 0 { bail!("a finished action still offers controls after restart"); }
        let applied = snapshot["conversation"]["project_actions"][1]["id"].as_str().context("applied action id")?;
        ui::invoke(restarted.driver(), "act_on_project_conversation", json!({
            "projectId":project_id, "conversationId":snapshot["conversation"]["id"], "revision":snapshot["conversation"]["revision"],
            "actionId":applied, "action":{"action":"approve","request":Value::Null}
        })).await?;
        let after = serde_json::from_value::<Vec<Value>>(ui::invoke(restarted.driver(), "list_tasks", json!({"projectId":project_id})).await?)?;
        if after.len() != 1 { bail!("a replayed approval changed the Task count to {}", after.len()); }
        context.close_session(restarted, "task-mutations-restart", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    context.finish(outcome);
}

/// A Coordinator proposal to queue a Backlog Task, from rejection through
/// approval and the scheduler's own promotion to a restart.
///
/// Automatic promotion is switched off while the approval is made, so the
/// journey can hold the Task in Queue and show that approving only queues it:
/// no checkout, no branch and no Worker exist until the scheduler is allowed
/// to promote. The Task is then run to Human Review by the real executor.
#[tokio::test(flavor = "multi_thread")]
async fn coordinator_queues_a_backlog_task_only_after_human_approval() {
    let context = TestContext::new("project_conversation_enqueue").expect("harness setup");
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root).expect("install fake Claude");
    context.set_child_env("PATH", agent.path_value().to_os_string());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir().as_os_str().to_os_string());
    let scripted = agent.marker_dir().join(".coordinator-output");
    std::fs::create_dir_all(&scripted).expect("script directory");

    let outcome = async {
        super::start_on_the_legacy_auto_placement(&context.state().config_file())?;
        let repository = GitFixture::create(&root.join("fixture-repo"))?;
        let session = context.start_session("enqueue").await?;
        let driver = session.driver();
        let project_id = register_project(driver, &repository).await?;
        let (task_id, title) = create_task(driver, &project_id, "Queue me", &[]).await?;
        // Held in Queue until the journey chooses otherwise.
        ui::invoke(driver, "update_queue_config", json!({ "autoPromote": false })).await?;
        let proposal = "[data-testid=\"project-action-proposal\"]";
        let history = "[data-testid=\"conversation-history\"]";
        let task_record = || async {
            let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
            super::find_task(&listed, &task_id).context("the Task is no longer listed")
        };
        let worker_runs = || anyhow::Ok(agent.invocations()?.iter().filter(|run| {
            run.prompt.as_deref().is_some_and(|prompt| !prompt.contains("\"current_message\""))
        }).count());
        let enqueue = format!(r#"{{"type":"enqueue_task","text":"This is ready to build.","target_task_id":"{task_id}"}}"#);
        std::fs::write(scripted.join("1"), &enqueue)?;
        std::fs::write(scripted.join("2"), &enqueue)?;
        open_conversations(driver, &project_id).await?;

        // 1. A rejected proposal changes nothing, and the Human saw what it would do.
        submit_message(driver, "Please get this started.").await?;
        await_text(driver, proposal, &title).await?;
        let shown = ui::visible(driver, proposal).await?.text().await?;
        let lowered = shown.to_lowercase();
        if !lowered.contains("queue task") || !lowered.contains("backlog → queue") || !lowered.contains("only queues the task") { bail!("the exact enqueue is not visible: {shown}"); }
        if task_record().await?["status"] != "backlog" { bail!("a proposal moved the Task before approval"); }
        ui::visible(driver, "[data-testid=\"project-action-reject\"]").await?.click().await?;
        await_text(driver, history, "You rejected the proposal.").await?;
        let rejected = task_record().await?;
        if rejected["status"] != "backlog" || !rejected["worktree_path"].is_null() { bail!("rejecting a proposal changed the Task: {rejected}"); }

        // 2. Approval queues the Task and nothing more.
        submit_message(driver, "Yes, please queue it.").await?;
        await_text(driver, proposal, &title).await?;
        ui::visible(driver, "[data-testid=\"project-action-approve\"]").await?.click().await?;
        await_text(driver, history, &format!("Task queued: {title}")).await?;
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(8) {
            let queued = task_record().await?;
            if queued["status"] != "queue" || !queued["worktree_path"].is_null() || !queued["branch_name"].is_null() || queued["phase"] != "idle" {
                bail!("approving started more than the queue: {queued}");
            }
            if worker_runs()? != 0 { bail!("approving started a Worker before the scheduler promoted the Task"); }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // 3. The scheduler's normal promotion runs it, once.
        ui::invoke(driver, "update_queue_config", json!({ "autoPromote": true })).await?;
        let settled = super::await_status(driver, &project_id, &task_id, &["human_review", "error"]).await?;
        if settled["status"] != "human_review" || settled["worktree_path"].is_null() || settled["branch_name"].is_null() {
            bail!("the queued Task did not run through the executor to Human Review: {settled}");
        }
        if worker_runs()? != 1 { bail!("expected exactly one Task run, found {}", worker_runs()?); }
        context.close_session(session, "enqueue", &Ok(())).await?;

        // 4. After a restart the outcomes are durable and nothing is replayed.
        let restarted = context.start_session("enqueue-restart").await?;
        open_conversations(restarted.driver(), &project_id).await?;
        let snapshot = ui::invoke(restarted.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        let statuses = snapshot["conversation"]["project_actions"].as_array().context("project actions missing")?.iter().map(|action| action["status"].as_str().unwrap_or_default().to_string()).collect::<Vec<_>>();
        if statuses != ["rejected", "applied"] { bail!("project action outcomes did not survive restart: {statuses:?}"); }
        let controls = super::page(restarted.driver(), "return document.querySelectorAll('[data-testid=project-action-approve], [data-testid=project-action-reject]').length;", vec![]).await?;
        if controls != 0 { bail!("a finished action still offers controls after restart"); }
        let applied = snapshot["conversation"]["project_actions"][1]["id"].as_str().context("applied action id")?;
        ui::invoke(restarted.driver(), "act_on_project_conversation", json!({
            "projectId":project_id, "conversationId":snapshot["conversation"]["id"], "revision":snapshot["conversation"]["revision"],
            "actionId":applied, "action":{"action":"approve","request":Value::Null}
        })).await?;
        let listed = ui::invoke(restarted.driver(), "list_tasks", json!({ "projectId": project_id })).await?;
        let after = super::find_task(&listed, &task_id).context("the Task is no longer listed")?;
        if after["status"] != "human_review" { bail!("a replayed approval or the restart moved the Task: {after}"); }
        tokio::time::sleep(Duration::from_secs(4)).await;
        if worker_runs()? != 1 { bail!("a restart or replayed approval ran the Task again: {} runs", worker_runs()?); }
        context.close_session(restarted, "enqueue-restart", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    context.finish(outcome);
}

/// A Human-approved queue shows its Task run in the Conversation: queued under
/// the approval first, then completed with the coding result. The association
/// survives a restart because it is rebuilt from the Task record, and the
/// Conversation starts no provider run of its own.
#[tokio::test(flavor = "multi_thread")]
async fn project_conversation_shows_the_run_an_approval_started() {
    let context = TestContext::new("project_conversation_execution").expect("harness setup");
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root).expect("install fake Claude");
    context.set_child_env("PATH", agent.path_value().to_os_string());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir().as_os_str().to_os_string());
    let scripted = agent.marker_dir().join(".coordinator-output");
    std::fs::create_dir_all(&scripted).expect("script directory");

    let outcome = async {
        super::start_on_the_legacy_auto_placement(&context.state().config_file())?;
        let repository = GitFixture::create(&root.join("fixture-repo"))?;
        let session = context.start_session("execution").await?;
        let driver = session.driver();
        let project_id = register_project(driver, &repository).await?;
        let (task_id, title) = create_task(driver, &project_id, "Show my run", &[]).await?;
        // Held in Queue until the journey lets the scheduler promote it.
        ui::invoke(driver, "update_queue_config", json!({ "autoPromote": false })).await?;
        let worker_runs = || anyhow::Ok(agent.invocations()?.iter().filter(|run| {
            run.prompt.as_deref().is_some_and(|prompt| !prompt.contains("\"current_message\""))
        }).count());
        let enqueue = format!(r#"{{"type":"enqueue_task","text":"This is ready to build.","target_task_id":"{task_id}"}}"#);
        std::fs::write(scripted.join("1"), &enqueue)?;
        open_conversations(driver, &project_id).await?;
        let history = "[data-testid=\"conversation-history\"]";
        let card = "[data-testid=\"conversation-execution\"]";

        // 1. Approval queues the Task, and the card names the approval's queued run.
        submit_message(driver, "Please get this started.").await?;
        await_text(driver, "[data-testid=\"project-action-proposal\"]", &title).await?;
        ui::visible(driver, "[data-testid=\"project-action-approve\"]").await?.click().await?;
        await_text(driver, history, &format!("Task queued: {title}")).await?;
        await_text(driver, &format!("{card}[data-state=\"queued\"] [data-testid=\"conversation-execution-task\"]"), &title).await?;
        await_text(driver, &format!("{card}[data-state=\"queued\"] [data-testid=\"conversation-execution-run\"]"), "Run not established yet").await?;
        if worker_runs()? != 0 { bail!("approving started a run before the scheduler promoted the Task"); }

        // 2. The executor runs it. The card settles to the coding result, and the Conversation starts nothing.
        ui::invoke(driver, "update_queue_config", json!({ "autoPromote": true })).await?;
        let settled = super::await_status(driver, &project_id, &task_id, &["human_review", "error"]).await?;
        if settled["status"] != "human_review" || settled["worktree_path"].is_null() {
            bail!("the queued Task did not run through the executor to Human Review: {settled}");
        }
        await_text(driver, &format!("{card}[data-state=\"completed\"] [data-testid=\"conversation-execution-run\"]"), "Run 1").await?;
        await_text(driver, &format!("{card}[data-state=\"completed\"] [data-testid=\"conversation-execution-result\"]"), fake_agent::REPORTED_RESULT).await?;
        if worker_runs()? != 1 { bail!("expected exactly one Task run, found {}", worker_runs()?); }
        context.close_session(session, "execution", &Ok(())).await?;

        // 3. After a restart the card is rebuilt from the Task record, and no run is replayed.
        let restarted = context.start_session("execution-restart").await?;
        open_conversations(restarted.driver(), &project_id).await?;
        await_text(restarted.driver(), &format!("{card}[data-state=\"completed\"] [data-testid=\"conversation-execution-result\"]"), fake_agent::REPORTED_RESULT).await?;
        tokio::time::sleep(Duration::from_secs(4)).await;
        if worker_runs()? != 1 { bail!("a restart or the Conversation ran the Task again: {} runs", worker_runs()?); }
        context.close_session(restarted, "execution-restart", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    context.finish(outcome);
}

/// A Coordinator run whose answer breaks the strict JSON contract is recorded
/// as a failed turn at once. The message stays saved and visible, the failure
/// survives a restart, and one explicit Retry answers that same message without
/// saving it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_coordinator_turn_survives_restart_and_is_retried_once() {
    const MESSAGE: &str = "Summarize the Project.";
    const RETRY: &str = "[data-testid=\"conversation-retry-turn\"]";
    const RETRY_PANEL: &str = "[data-testid=\"conversation-turn-retry\"]";
    let context = TestContext::new("project_conversation_failed_turn_retry").expect("harness setup");
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root).expect("install fake Claude");
    context.set_child_env("PATH", agent.path_value().to_os_string());
    context.set_child_env(
        fake_agent::MARKER_DIR_VAR,
        agent.marker_dir().as_os_str().to_os_string(),
    );
    // The first Coordinator run answers with text that is not the JSON contract.
    // The fixture answers every later run with a valid reply.
    let scripted = agent.marker_dir().join(".coordinator-output");
    std::fs::create_dir_all(&scripted).expect("create the scripted output directory");
    std::fs::write(scripted.join("1"), "this is not the JSON contract").expect("script the malformed run");

    let outcome = async {
        let session = context.start_session("failed-turn").await?;
        let project_id = created_id(ui::invoke(session.driver(), "create_project", json!({
            "name":"Failed Turn Conversation", "repositoryId":Value::Null, "agentType":"claude_code"
        })).await?, "create_project")?;
        open_conversations(session.driver(), &project_id).await?;

        submit_message(session.driver(), MESSAGE).await?;
        await_text(session.driver(), "[data-testid=\"conversation-run-failed\"]", "Invalid Coordinator JSON").await?;
        await_text(session.driver(), RETRY_PANEL, "Your last message has no reply yet").await?;
        let failed = text_of(session.driver(), "[data-testid=\"conversation-history\"]").await?.unwrap_or_default();
        if failed.matches(MESSAGE).count() != 1 { bail!("the failed message is not shown exactly once: {failed}"); }
        let failed_snapshot = ui::invoke(session.driver(), "get_project_conversation", json!({"projectId":project_id})).await?;
        if failed_snapshot["coordinator_live"] != false { bail!("a failed run was reported live: {failed_snapshot}"); }
        context.close_session(session, "failed-turn", &Ok(())).await?;

        // The failure was saved when it happened, so a restarted application shows it and offers Retry.
        let restarted = context.start_session("failed-turn-restart").await?;
        open_conversations(restarted.driver(), &project_id).await?;
        await_text(restarted.driver(), RETRY_PANEL, "Your last message has no reply yet").await?;
        ui::visible(restarted.driver(), RETRY).await?.click().await?;
        await_text(restarted.driver(), "[data-testid=\"conversation-message\"]", "Fake Coordinator reply.").await?;

        let answered = await_conversation_idle(restarted.driver(), &project_id, 3).await?;
        let kinds = answered["conversation"]["entries"].as_array().context("Conversation entries missing")?
            .iter()
            .map(|entry| entry["kind"]["kind"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        if kinds != ["human_message", "run_failed", "coordinator_reply"] { bail!("the retry changed the saved turn order: {kinds:?}"); }
        let saved_messages = answered["conversation"]["entries"].as_array().context("Conversation entries missing")?
            .iter()
            .filter(|entry| entry["kind"]["kind"] == "human_message" && entry["kind"]["text"] == MESSAGE)
            .count();
        if saved_messages != 1 { bail!("the message was saved {saved_messages} times"); }
        if text_of(restarted.driver(), RETRY).await?.is_some() { bail!("Retry is still offered after the turn was answered"); }

        let invocations = agent.invocations()?;
        let runs = invocations.iter().filter(|invocation| invocation.prompt.is_some()).collect::<Vec<_>>();
        if runs.len() != 2 { bail!("expected exactly one failed run and one retry, observed {}", runs.len()); }
        let retried = runs[1].prompt.as_deref().unwrap_or_default();
        if !retried.contains(r#""current_message":"Summarize the Project.""#) { bail!("the retry did not ask the saved message: {retried}"); }
        context.close_session(restarted, "failed-turn-restart", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    context.finish(outcome);
}

/// Open the Conversations page from the Project navigation.
///
/// The last page is restored on reload, so a restarted session may already be
/// on this page and must not be expected to land on the board.
pub(super) async fn open_conversations(driver: &WebDriver, project_id: &str) -> Result<()> {
    driver.refresh().await.context("could not reload the application window")?;
    ui::assert_frontend_is_real(driver).await?;
    ui::visible(driver, &format!("[data-testid=\"rail-project-{project_id}\"]")).await?
        .click().await.context("could not select the project in the rail")?;
    // Hold the formerly racy state: the pointer on a fully expanded rail, whose
    // overlay covers the navigation item. `open_nav` must still get through.
    await_rail(driver, "return rail.classList.contains('expanded') && rail.getBoundingClientRect().width >= 180;").await?;
    open_nav(driver, "conversations", PANEL).await
}

/// Show a Task title on the board, then return to the conversation.
async fn await_board_task_title(driver: &WebDriver, title: &str) -> Result<()> {
    open_nav(driver, "dashboard", KANBAN_BOARD).await?;
    await_text(driver, "[data-testid=\"task-title\"]", title).await?;
    open_nav(driver, "conversations", PANEL).await
}

/// Click a Project navigation item and wait for the page it opens.
///
/// Selecting a Project leaves the pointer on the Project Rail, which expands
/// on hover into a 180px overlay over the Sidebar. That overlay covers the
/// centre of every navigation item, so clicking before it has collapsed is
/// intercepted, or lands on the rail instead. Park the pointer on the page
/// and wait for the rail to be back at its collapsed width first.
pub(super) async fn open_nav(driver: &WebDriver, page: &str, landmark: &str) -> Result<()> {
    release_rail(driver).await?;
    ui::visible(driver, &format!("[data-testid=\"nav-{page}\"]")).await?.click().await
        .with_context(|| format!("could not open the {page} page"))?;
    ui::visible(driver, landmark).await.with_context(|| format!("the {page} page did not render {landmark}"))?;
    Ok(())
}

/// Move the pointer off the Project Rail and wait until it has collapsed.
async fn release_rail(driver: &WebDriver) -> Result<()> {
    let main = ui::visible(driver, "main").await?;
    driver.action_chain().move_to_element_center(&main).perform().await
        .context("could not move the pointer off the project rail")?;
    await_rail(driver, "return !rail.classList.contains('expanded') && rail.getBoundingClientRect().width <= 48;").await
}

/// Wait until the Project Rail satisfies a predicate over `rail`.
pub(super) async fn await_rail(driver: &WebDriver, predicate: &str) -> Result<()> {
    let script = format!("const rail = document.querySelector('[data-testid=\"project-rail\"]'); return !!rail && (() => {{ {predicate} }})();");
    let started = Instant::now();
    loop {
        if super::page(driver, &script, vec![]).await? == Value::Bool(true) { return Ok(()); }
        if started.elapsed() > ui::ELEMENT_TIMEOUT {
            bail!("the project rail never reached the expected state: {predicate}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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

pub(super) async fn await_text(driver: &WebDriver, selector: &str, text: &str) -> Result<()> {
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
