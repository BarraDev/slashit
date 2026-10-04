//! A task's card keeps its DOM node while its task changes.
//!
//! A click is a press and a release. WebKit sends no click if the element the
//! button went down on has left the page by the time it comes up, so a card
//! the board replaces on an ordinary update (a title edit, a progress report,
//! a sibling entering or leaving the column) can swallow a click aimed at it.
//! This journey marks one rendered card and holds it against those updates.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_task_card_keeps_its_dom_node_through_updates_to_its_task_and_its_column() {
    let context = TestContext::new("board_identity").expect("harness setup");
    let outcome = identity_journey(&context).await;
    context.finish(outcome);
}

async fn identity_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("board-identity").await?;
    let outcome = keep_the_node(session.driver(), &repository).await;
    context.close_session(session, "board-identity", &outcome).await?;
    outcome
}

/// Mark the card, so a replaced node cannot pass for the same one.
const MARK: &str = r#"
const [title] = arguments;
const cards = [...document.querySelectorAll('[data-testid="task-card"]')]
  .filter((c) => (c.querySelector('[data-testid="task-title"]') || {}).textContent === title);
if (cards.length !== 1) { return 'expected one card, found ' + cards.length; }
const wrapper = cards[0].closest('[data-card-task-id]');
const draggable = cards[0].closest('[draggable="true"]');
window.__marked = { card: cards[0], wrapper, draggable, taskId: wrapper.getAttribute('data-card-task-id') };
return 'marked';
"#;

/// Whether the marked nodes are still the ones on the board, and still
/// attached.
const SAME_NODES: &str = r#"
const m = window.__marked;
const card = [...document.querySelectorAll('[data-testid="task-card"]')]
  .find((c) => c === m.card);
return {
  cardStillOnBoard: !!card,
  cardConnected: m.card.isConnected,
  wrapperConnected: m.wrapper.isConnected,
  draggableConnected: m.draggable.isConnected,
  taskIdStillMatches: m.wrapper.getAttribute('data-card-task-id') === m.taskId,
  title: (m.card.querySelector('[data-testid="task-title"]') || {}).textContent,
};
"#;

/// Tell the board a task changed, the way a running agent does, so it reads
/// the task list again at once rather than at its next poll.
const ANNOUNCE: &str = r#"
const [taskId, progress] = arguments;
await window.__TAURI__.event.emit('agent-event', {
  type: 'phase_change', task_id: taskId, phase: 'coding', progress,
});
await window.__TAURI__.event.emit('agent-event', { type: 'tool_use', task_id: taskId, tool: 'Edit' });
return true;
"#;

async fn create_backlog_task(driver: &WebDriver, project_id: &str, title: &str) -> Result<String> {
    let task = ui::invoke(
        driver,
        "create_task",
        json!({"params": {
            "projectId": project_id, "title": title, "description": "x",
            "model": "default", "planningMode": false, "dependencies": [],
            "category": Value::Null, "priority": Value::Null, "complexity": Value::Null,
            "impact": Value::Null, "securitySeverity": Value::Null, "githubIssueUrl": Value::Null,
            "gitlabIssueUrl": Value::Null, "linearTicketId": Value::Null }}),
    )
    .await?;
    created_id(task, "create_task")
}

async fn card_title(driver: &WebDriver, task_id: &str) -> Result<String> {
    let found = page(
        driver,
        "const w = document.querySelector('[data-card-task-id=\"' + arguments[0] + '\"]');
         return w ? (w.querySelector('[data-testid=\"task-title\"]') || {}).textContent || '' : null;",
        vec![json!(task_id)],
    )
    .await?;
    Ok(found.as_str().unwrap_or_default().to_string())
}

async fn wait_for_title(driver: &WebDriver, task_id: &str, title: &str) -> Result<()> {
    let started = Instant::now();
    while card_title(driver, task_id).await? != title {
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the board never showed the title {title:?} for its task");
        }
        tokio::time::sleep(POLL).await;
    }
    Ok(())
}

async fn assert_same_nodes(driver: &WebDriver, after: &str) -> Result<()> {
    let state = page(driver, SAME_NODES, vec![]).await?;
    for key in ["cardStillOnBoard", "cardConnected", "wrapperConnected", "draggableConnected", "taskIdStillMatches"] {
        if state[key] != json!(true) {
            bail!("after {after}, the marked card's node was replaced ({key} is not true): {state}");
        }
    }
    Ok(())
}

async fn keep_the_node(driver: &WebDriver, repository: &GitFixture) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let Prerequisites { project_id, task_id: first, .. } =
        create_prerequisites(driver, repository, "Identity first", "Ahead of the marked card.").await?;
    let second = create_backlog_task(driver, &project_id, "Identity second").await?;
    // Created last, so it is the last card in the column and both others are
    // ahead of it: one leaving shifts it up, which re-binds its node if cards
    // are reused by position.
    let title = "Identity marked".to_string();
    let task_id = create_backlog_task(driver, &project_id, &title).await?;
    open_board(driver, &project_id).await?;
    assert_card_in_column(driver, BACKLOG_COLUMN, &title).await?;

    let marked = page(driver, MARK, vec![json!(title)]).await?;
    if marked != json!("marked") {
        bail!("could not mark the card: {marked}");
    }

    // Updates to this task's own record: its title, and the progress and phase
    // a running agent reports, each announced as an agent event would.
    for round in 1..=3u32 {
        let renamed = format!("{title} (edit {round})");
        ui::invoke(
            driver,
            "update_task",
            json!({"params": {"taskId": task_id, "title": renamed}}),
        )
        .await?;
        ui::invoke(
            driver,
            "update_task_progress",
            json!({
                "taskId": task_id, "phase": "coding", "phaseProgress": 20 * round,
                "overallProgress": 20 * round, "sequenceNumber": round,
            }),
        )
        .await?;
        page(driver, ANNOUNCE, vec![json!(task_id), json!(20 * round)]).await?;
        wait_for_title(driver, &task_id, &renamed).await?;
        assert_same_nodes(driver, &format!("its own update {round}")).await?;
    }

    // The column changing around it: each card ahead of it leaves and comes
    // back.
    for (name, sibling) in [("the first card", &first), ("the second card", &second)] {
        for status in ["human_review", "backlog"] {
            ui::invoke(
                driver,
                "update_task_status",
                json!({"taskId": sibling, "status": status}),
            )
            .await?;
            page(driver, ANNOUNCE, vec![json!(sibling), json!(0)]).await?;
            // Give the board its refresh before looking.
            tokio::time::sleep(Duration::from_millis(750)).await;
            assert_same_nodes(driver, &format!("{name} went to {status}")).await?;
        }
    }

    // And the card still opens its own drawer. It is the last of three in a
    // column that scrolls inside the window, below the fold, so bring it into
    // view first; the click itself is still the driver's.
    page(driver, "window.__marked.card.scrollIntoView({ block: 'center' }); return true;", vec![]).await?;
    let current = card_title(driver, &task_id).await?;
    open_drawer(driver, &task_id, &current).await?;
    Ok(())
}

/// Press a card's drag, as the browser would, without moving a real pointer:
/// the board reacts to the events, not to how they came about.
const DRAG_EVENT: &str = r#"
const [selector, type] = arguments;
const node = type === 'drop' ? document.querySelector(selector)
  : window.__marked.draggable;
const transfer = new DataTransfer();
if (window.__transfer) { transfer.setData('text/plain', window.__transfer); }
const event = new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer: transfer });
node.dispatchEvent(event);
if (type === 'dragstart') { window.__transfer = transfer.getData('text/plain'); }
return true;
"#;

/// Whether the marked card still wears the style of a card being dragged.
async fn assert_dragging_style(driver: &WebDriver, when: &str) -> Result<()> {
    let class = page(driver, "return window.__marked.draggable.className;", vec![]).await?;
    if !class.as_str().unwrap_or_default().contains("opacity-25") {
        bail!("after {when}, the dragged card lost its dragging style: {class}");
    }
    Ok(())
}

/// The task's status as the product holds it.
async fn status_in_store(driver: &WebDriver, project_id: &str, task_id: &str) -> Result<String> {
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    let task = listed
        .as_array()
        .and_then(|all| all.iter().find(|t| t["id"] == json!(task_id)))
        .with_context(|| format!("the product no longer lists {task_id}"))?;
    Ok(status_of(task).unwrap_or("?").to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_drag_in_progress_survives_an_update_to_the_dragged_task_and_still_ends_cleanly() {
    let context = TestContext::new("board_drag_update").expect("harness setup");
    let outcome = drag_journey(&context).await;
    context.finish(outcome);
}

async fn drag_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    let session = context.start_session("board-drag-update").await?;
    let outcome = drag_through_an_update(session.driver(), &repository).await;
    context.close_session(session, "board-drag-update", &outcome).await?;
    outcome
}

async fn drag_through_an_update(driver: &WebDriver, repository: &GitFixture) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let Prerequisites { project_id, task_id, title } =
        create_prerequisites(driver, repository, "Drag", "Updated while dragged.").await?;
    open_board(driver, &project_id).await?;
    assert_card_in_column(driver, BACKLOG_COLUMN, &title).await?;
    let marked = page(driver, MARK, vec![json!(title)]).await?;
    if marked != json!("marked") {
        bail!("could not mark the card: {marked}");
    }

    // A drag that ends where it started: begin, update the task mid-drag, end.
    page(driver, DRAG_EVENT, vec![json!(BACKLOG_COLUMN), json!("dragstart")]).await?;
    assert_dragging_style(driver, "the drag started").await?;
    let renamed = format!("{title} (mid-drag)");
    ui::invoke(driver, "update_task", json!({"params": {"taskId": task_id, "title": renamed}})).await?;
    page(driver, ANNOUNCE, vec![json!(task_id), json!(40)]).await?;
    wait_for_title(driver, &task_id, &renamed).await?;
    assert_same_nodes(driver, "an update during the drag").await?;
    assert_dragging_style(driver, "an update during the drag").await?;
    page(driver, DRAG_EVENT, vec![json!(BACKLOG_COLUMN), json!("dragend")]).await?;

    // The drag is over, so a drop now carries nothing: nothing moves.
    page(driver, DRAG_EVENT, vec![json!(HUMAN_REVIEW_COLUMN), json!("drop")]).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let status = status_in_store(driver, &project_id, &task_id).await?;
    if status != "backlog" {
        bail!("a drop after the drag ended moved the task to {status}: the drag state was left behind");
    }

    // A drag updated midway and then dropped still carries its task.
    page(driver, DRAG_EVENT, vec![json!(BACKLOG_COLUMN), json!("dragstart")]).await?;
    ui::invoke(
        driver,
        "update_task_progress",
        json!({"taskId": task_id, "phase": "coding", "phaseProgress": 50, "overallProgress": 50, "sequenceNumber": 2}),
    )
    .await?;
    page(driver, ANNOUNCE, vec![json!(task_id), json!(50)]).await?;
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_same_nodes(driver, "a progress report during the second drag").await?;
    page(driver, DRAG_EVENT, vec![json!(HUMAN_REVIEW_COLUMN), json!("drop")]).await?;
    let started = Instant::now();
    while status_in_store(driver, &project_id, &task_id).await? != "human_review" {
        if started.elapsed() > RENDER_DEADLINE {
            bail!("a drop after an update mid-drag did not move the dragged task");
        }
        tokio::time::sleep(POLL).await;
    }
    page(driver, DRAG_EVENT, vec![json!(BACKLOG_COLUMN), json!("dragend")]).await?;
    Ok(())
}
