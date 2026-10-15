//! S1 Worker handoff recovery through the real desktop application.
//!
//! The committed case seeds the exact Task-owned crash record after a real
//! Worker commit, then lets the real startup poller validate Git and finish
//! the lifecycle handoff. The uncertain case stops a real fake provider while
//! it is blocked after writing into its registered checkout, then restarts.

use super::task_runs::{branch_name_of, create_task, register_project, run_to_review};
use super::*;
use anyhow::{Context, bail};

#[tokio::test(flavor = "multi_thread")]
async fn a_committed_handoff_recovers_on_restart_without_a_second_coding_worker() {
    let context = TestContext::new("worker_recovery_committed").expect("harness setup");
    let result = committed_handoff_journey(&context).await;
    context.finish(result);
}

async fn committed_handoff_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_agent::WRITE_FILE_VAR, "recovery-proof.txt");
    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("worker-commit").await?;
    let (project_id, task_id, branch, commit) = async {
        let project_id = register_project(session.driver(), &repository).await?;
        let (task_id, title) =
            create_task(session.driver(), &project_id, "Committed handoff", &[]).await?;
        let task = run_to_review(session.driver(), &project_id, &task_id, &title).await?;
        let listed = ui::invoke(
            session.driver(),
            "list_tasks",
            json!({ "projectId": project_id }),
        )
        .await?;
        let found = find_task(&listed, &task_id).context("task disappeared")?;
        let branch = branch_name_of(&found)?;
        let commit = repository
            .branch_tip(&branch)?
            .context("the Worker branch has no tip")?;
        if repository
            .file_at(&branch, "recovery-proof.txt")?
            .as_deref()
            != Some(fake_agent::WORK_CONTENT)
        {
            bail!("the recorded branch tip does not contain the fake Worker's output");
        }
        if !repository.registers_worktree(&task.worktree_path)? {
            bail!("the reported Task Checkout is not registered with Git");
        }
        Ok::<_, anyhow::Error>((project_id, task_id, branch, commit))
    }
    .await?;
    // The provider is still live here. Use the product's ownership path so it
    // is joined and killed before harness shutdown; its edits remain
    // uncommitted and the durable handoff becomes RecoveryRequired.
    ui::invoke(
        session.driver(),
        "stop_task_execution",
        json!({ "taskId": task_id }),
    )
    .await?;
    if !agent.blocked_pids()?.is_empty() {
        bail!("the product returned from Stop while the provider was still alive");
    }
    let closed = Ok(());
    context
        .close_session(session, "worker-commit", &closed)
        .await?;

    // Model the persisted point after exact commit evidence but before the
    // AI Review lifecycle write. This is the real production schema; the
    // executor/storage test covers the commit/evidence crash boundary itself.
    mutate_task_record(&root, &task_id, |record| {
        record.insert("status".into(), toml::Value::String("in_progress".into()));
        record.insert("phase".into(), toml::Value::String("coding".into()));
        record.remove("error_message");
        let handoff = record
            .get_mut("worker_handoff")
            .and_then(toml::Value::as_table_mut)
            .context("real Worker run did not persist handoff evidence")?;
        let mut stage = toml::map::Map::new();
        stage.insert(
            "state".into(),
            toml::Value::String("committed_awaiting_review".into()),
        );
        stage.insert("commit".into(), toml::Value::String(commit.clone()));
        handoff.insert("stage".into(), toml::Value::Table(stage));
        Ok(())
    })?;

    let session = context.start_session("committed-recovery").await?;
    let task = await_status(
        session.driver(),
        &project_id,
        &task_id,
        &["human_review", "error"],
    )
    .await?;
    if status_of(&task) != Some("human_review") {
        bail!("exact committed handoff did not complete review after restart: {task}");
    }
    assert_recovered_record(&root, &task_id, &commit)?;
    context
        .close_session(session, "committed-recovery", &closed)
        .await?;

    // A second process restart is the idempotency check: it must preserve the
    // downstream lifecycle state and must not append another RunCompleted or
    // implementation invocation.
    let session = context.start_session("committed-recovery-again").await?;
    let task = await_status(
        session.driver(),
        &project_id,
        &task_id,
        &["human_review", "error"],
    )
    .await?;
    if status_of(&task) != Some("human_review") {
        bail!("repeated restart regressed the recovered task: {task}");
    }
    assert_recovered_record(&root, &task_id, &commit)?;
    let runs = agent_runs(&agent)?;
    let coding = runs
        .iter()
        .filter(|run| {
            run.prompt.is_some()
                && run
                    .flag("--allowedTools")
                    .is_some_and(|tools| tools.to_string_lossy().contains("Bash"))
        })
        .count();
    if coding != 1 {
        bail!("recovery launched {coding} coding Workers; expected exactly the original one");
    }
    if repository.branch_tip(&branch)?.as_deref() != Some(commit.as_str()) {
        bail!("recovery changed the exact committed branch tip");
    }
    context
        .close_session(session, "committed-recovery-again", &closed)
        .await
}

fn assert_recovered_record(root: &Path, task_id: &str, commit: &str) -> Result<()> {
    let record = read_task_record(root, task_id)?;
    if record.get("status").and_then(toml::Value::as_str) != Some("human_review") {
        bail!("the durable Task record is not in Human Review");
    }
    let activity = record
        .get("activity")
        .and_then(toml::Value::as_array)
        .context("activity timeline is missing")?;
    let completions = activity
        .iter()
        .filter(|entry| {
            entry
                .get("kind")
                .and_then(|kind| kind.get("type"))
                .and_then(toml::Value::as_str)
                == Some("run_completed")
        })
        .count();
    if completions != 1 {
        bail!("expected one RunCompleted activity for this task, found {completions}");
    }
    let stage = record
        .get("worker_handoff")
        .and_then(|handoff| handoff.get("stage"))
        .context("the durable handoff evidence disappeared")?;
    if stage.get("state").and_then(toml::Value::as_str) != Some("resolved_to_ai_review")
        || stage.get("commit").and_then(toml::Value::as_str) != Some(commit)
    {
        bail!("the durable resolved handoff does not retain exact commit {commit}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_worker_is_visible_and_cannot_be_restarted_or_moved() {
    let context = TestContext::new("worker_recovery_ambiguous").expect("harness setup");
    let result = ambiguous_handoff_journey(&context).await;
    context.finish(result);
}

async fn ambiguous_handoff_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root)?;
    let block_dir = agent.block_agent_runs()?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_agent::WRITE_FILE_VAR, "preserved-uncommitted.txt");
    context.set_child_env(fake_agent::BLOCK_DIR_VAR, block_dir.as_os_str());
    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("interrupted-worker").await?;
    let (project_id, task_id, title, worktree, branch, provider_pid) = async {
        let project_id = register_project(session.driver(), &repository).await?;
        let (task_id, title) =
            create_task(session.driver(), &project_id, "Ambiguous worker", &[]).await?;
        ui::invoke(
            session.driver(),
            "update_task_status",
            json!({ "taskId": task_id, "status": "in_progress" }),
        )
        .await?;
        let _ = await_status(session.driver(), &project_id, &task_id, &["in_progress"]).await?;
        for _ in 0..EXECUTION_DEADLINE.as_secs() {
            if !agent.blocked_pids()?.is_empty() {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        if agent.blocked_pids()?.is_empty() {
            bail!("the fake Worker never reached its deterministic blocked point");
        }
        let provider_pid = agent.blocked_pids()?[0];
        let task = ui::invoke(
            session.driver(),
            "list_tasks",
            json!({ "projectId": project_id }),
        )
        .await?;
        let task = find_task(&task, &task_id).context("interrupted Task disappeared")?;
        let worktree = task
            .get("worktree_path")
            .and_then(Value::as_str)
            .context("Task Checkout was not recorded")?
            .to_string();
        let branch = branch_name_of(&task)?;
        if std::fs::read_to_string(Path::new(&worktree).join("preserved-uncommitted.txt"))
            .ok()
            .as_deref()
            != Some(fake_agent::WORK_CONTENT)
        {
            bail!("the fake Worker did not leave its expected uncommitted output");
        }
        Ok::<_, anyhow::Error>((project_id, task_id, title, worktree, branch, provider_pid))
    }
    .await?;
    ui::invoke(
        session.driver(),
        "stop_task_execution",
        json!({ "taskId": task_id }),
    )
    .await?;
    let stopped_at = Instant::now();
    while agent.is_running(provider_pid) && stopped_at.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(POLL).await;
    }
    if agent.is_running(provider_pid) {
        bail!("the product's Stop returned but fake provider {provider_pid} remained alive");
    }
    let closed = Ok(());
    context
        .close_session(session, "interrupted-worker", &closed)
        .await?;

    let before_tip = repository
        .branch_tip(&branch)?
        .context("Task branch disappeared on shutdown")?;
    if repository
        .file_at(&branch, "preserved-uncommitted.txt")?
        .is_some()
    {
        bail!("the interrupted Worker's output was unexpectedly committed");
    }
    if !repository.registers_worktree(Path::new(&worktree))?
        || std::fs::read_to_string(Path::new(&worktree).join("preserved-uncommitted.txt"))
            .ok()
            .as_deref()
            != Some(fake_agent::WORK_CONTENT)
    {
        bail!("shutdown removed or changed the ambiguous Task Checkout");
    }

    let session = context.start_session("recovery-required").await?;
    let task = await_status(
        session.driver(),
        &project_id,
        &task_id,
        &["backlog", "error", "in_progress"],
    )
    .await?;
    let handoff = task
        .get("worker_handoff")
        .context("startup did not retain Worker recovery evidence")?;
    let stage = handoff
        .get("stage")
        .context("Worker handoff has no stage")?;
    if stage.get("state").and_then(Value::as_str) != Some("recovery_required") {
        bail!("ambiguous prior effects were not classified RecoveryRequired: {handoff}");
    }
    let needs_you = ui::visible(session.driver(), "[data-testid=\"header-needs-you\"]").await?;
    if needs_you
        .attr("data-count")
        .await?
        .as_deref()
        .is_none_or(|count| count.parse::<u32>().unwrap_or(0) < 1)
    {
        bail!("Needs You does not count the ambiguous recovery Task");
    }
    super::open_drawer(session.driver(), &task_id, &title).await?;
    let guidance = ui::visible(
        session.driver(),
        "[data-testid=\"task-drawer-recovery-guidance\"]",
    )
    .await?;
    let guidance = guidance.prop("textContent").await?.unwrap_or_default();
    for fact in ["preserved", "No new Worker can start", &branch, &worktree] {
        if !guidance.contains(fact) {
            bail!("recovery guidance omits {fact:?}: {guidance}");
        }
    }
    ui::assert_absent(session.driver(), "[data-testid=\"task-drawer-retry\"]").await?;
    ui::assert_absent(session.driver(), "[data-testid=\"task-drawer-start\"]").await?;
    driver_click(
        session.driver(),
        "[data-testid=\"task-drawer-tab-changes\"]",
    )
    .await?;
    let changes = ui::visible(session.driver(), "[data-testid=\"task-drawer-changes\"]").await?;
    let changes_text = changes.prop("textContent").await?.unwrap_or_default();
    if !changes_text.contains("preserved-uncommitted.txt") {
        bail!("the preserved checkout is not inspectable in Changes: {changes_text}");
    }

    if ui::invoke(
        session.driver(),
        "execute_task",
        json!({ "taskId": task_id }),
    )
    .await
    .is_ok()
    {
        bail!("direct execute_task bypassed the recovery gate");
    }
    if ui::invoke(
        session.driver(),
        "update_task_status",
        json!({ "taskId": task_id, "status": "queue" }),
    )
    .await
    .is_ok()
    {
        bail!("ordinary status movement bypassed the recovery gate");
    }
    let runs = agent_runs(&agent)?
        .iter()
        .filter(|run| {
            run.prompt.is_some()
                && run
                    .flag("--allowedTools")
                    .is_some_and(|tools| tools.to_string_lossy().contains("Bash"))
        })
        .count();
    if runs != 1 {
        bail!("recovery attempted to launch another coding Worker; observed {runs} invocations");
    }
    if repository.branch_tip(&branch)?.as_deref() != Some(before_tip.as_str())
        || !repository.registers_worktree(Path::new(&worktree))?
        || std::fs::read_to_string(Path::new(&worktree).join("preserved-uncommitted.txt"))
            .ok()
            .as_deref()
            != Some(fake_agent::WORK_CONTENT)
    {
        bail!("recovery changed the branch or preserved checkout");
    }
    let final_task = ui::invoke(
        session.driver(),
        "list_tasks",
        json!({ "projectId": project_id }),
    )
    .await?;
    let final_task = find_task(&final_task, &task_id).context("Task disappeared after refusals")?;
    if final_task
        .get("worker_handoff")
        .and_then(|e| e.get("stage"))
        .and_then(|e| e.get("state"))
        .and_then(Value::as_str)
        != Some("recovery_required")
    {
        bail!("refused commands changed the durable recovery state");
    }
    context
        .close_session(session, "recovery-required", &closed)
        .await
}

async fn driver_click(driver: &WebDriver, selector: &str) -> Result<()> {
    ui::visible(driver, selector).await?.click().await?;
    Ok(())
}

fn read_task_record(root: &Path, task_id: &str) -> Result<toml::value::Table> {
    for file in toml_files(root)? {
        let raw = std::fs::read_to_string(&file)?;
        let Ok(document) = raw.parse::<toml::Value>() else {
            continue;
        };
        if let Some(record) = task_record(&document, task_id) {
            return Ok(record.clone());
        }
    }
    bail!("no persisted Task record for {task_id}")
}

fn mutate_task_record(
    root: &Path,
    task_id: &str,
    mutate: impl FnOnce(&mut toml::value::Table) -> Result<()>,
) -> Result<()> {
    for file in toml_files(root)? {
        let raw = std::fs::read_to_string(&file)?;
        let Ok(mut document) = raw.parse::<toml::Value>() else {
            continue;
        };
        let Some(record) = document
            .get_mut("tasks")
            .and_then(toml::Value::as_array_mut)
            .and_then(|tasks| {
                tasks
                    .iter_mut()
                    .find(|task| task.get("id").and_then(toml::Value::as_str) == Some(task_id))
            })
            .and_then(toml::Value::as_table_mut)
        else {
            continue;
        };
        mutate(record)?;
        std::fs::write(&file, toml::to_string(&document)?)?;
        return Ok(());
    }
    bail!("no persisted Task record for {task_id}")
}
