// SPDX-License-Identifier: MPL-2.0
//! Moving between Human Review Tasks from the drawer without closing it.
//!
//! The drawer's previous and next controls walk the Human Review column in
//! board order. A move that would discard an unsent GitHub issue address is
//! refused: the address stays in its field and the open Task does not change.
//! Moving decides nothing: no Task is approved, moved or published by it.
//! Across each move the drawer keeps its order: Output, then Changes, then
//! Activity.

use super::github_issue_link::{type_address, INPUT, ISSUE_URL};
use super::human_review::click;
use super::*;
use thirtyfour::Key;

const STEP_PREVIOUS: &str = "[data-testid=\"task-drawer-review-previous\"]";
const STEP_NEXT: &str = "[data-testid=\"task-drawer-review-next\"]";
const STEP_POSITION: &str = "[data-testid=\"task-drawer-review-position\"]";
const STEP_BLOCKED: &str = "[data-testid=\"task-drawer-review-unsaved\"]";

#[tokio::test(flavor = "multi_thread")]
async fn a_drawer_steps_through_human_review_and_refuses_to_drop_an_unsent_issue_address() {
    let context = TestContext::new("review_navigation").expect("harness setup");
    let outcome = navigation_journey(&context).await;
    context.finish(outcome);
}

async fn navigation_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    let session = context.start_session("review-navigation").await?;
    let outcome = step_through_the_column(session.driver(), &repository).await;
    context
        .close_session(session, "review-navigation", &outcome)
        .await?;
    outcome
}

async fn step_through_the_column(driver: &WebDriver, repository: &GitFixture) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let Prerequisites {
        project_id,
        task_id: first,
        title: first_title,
    } = create_prerequisites(driver, repository, "Step first", "Never runs.").await?;
    let (second, second_title) = create_task_in(driver, &project_id, "Step second").await?;
    // No agent runs here: Human Review is entered directly, because the
    // stepping under test does not depend on how a Task got there.
    for id in [&first, &second] {
        ui::invoke(
            driver,
            "update_task_status",
            json!({ "taskId": id, "status": "human_review" }),
        )
        .await?;
    }

    // The column in the order the backend keeps it, which is the order the
    // board shows; the journey takes its two Tasks from that order.
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    let mut column: Vec<&Value> = listed
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| status_of(t) == Some("human_review"))
        .collect();
    column.sort_by_key(|t| t.get("position").and_then(Value::as_i64).unwrap_or_default());
    let ids: Vec<&str> = column
        .iter()
        .filter_map(|t| t.get("id").and_then(Value::as_str))
        .collect();
    let title_of = |id: &str| {
        if id == first {
            first_title.clone()
        } else {
            second_title.clone()
        }
    };
    if ids.len() != 2 || !ids.contains(&first.as_str()) || !ids.contains(&second.as_str()) {
        bail!("the Human Review column holds {ids:?}, not the two Tasks moved into it");
    }
    let (current, current_title) = (ids[0].to_string(), title_of(ids[0]));
    let (other, other_title) = (ids[1].to_string(), title_of(ids[1]));

    open_board(driver, &project_id).await?;
    open_drawer(driver, &current, &current_title).await?;
    await_position(driver, "1 of 2 in Human Review").await?;
    assert_drawer_order(driver).await?;
    if !disabled(driver, STEP_PREVIOUS).await? {
        bail!("Previous is offered on the first Task of the column");
    }
    if disabled(driver, STEP_NEXT).await? {
        bail!("Next is not offered although a Task follows in the column");
    }

    // An unsent issue address is the drawer's draft. Moving away from this
    // Task must be refused, and the draft and the Task must both remain.
    type_address(driver, ISSUE_URL).await?;
    click(driver, STEP_NEXT, "the Next Human Review task control").await?;
    ui::visible(driver, STEP_BLOCKED)
        .await
        .context("moving away from an unsent issue address was not refused")?;
    expect_open(driver, &current, &current_title).await?;
    let typed = ui::visible(driver, INPUT).await?.prop("value").await?;
    if typed.as_deref() != Some(ISSUE_URL) {
        bail!("the unsent issue address changed to {typed:?} after the refused move");
    }

    // Once the draft is cleared, the same control moves to the next Task.
    // Backspace, not `clear()`: the draft is read from the field's input
    // events, and a cleared value set without them would still count as text.
    let field = ui::visible(driver, INPUT).await?;
    for _ in 0..ISSUE_URL.len() {
        field.send_keys(Key::Backspace).await?;
    }
    click(driver, STEP_NEXT, "the Next Human Review task control").await?;
    ui::visible(driver, &drawer_of(&other))
        .await
        .context("Next did not open the following Task of the column")?;
    await_position(driver, "2 of 2 in Human Review").await?;
    expect_open(driver, &other, &other_title).await?;
    if !disabled(driver, STEP_NEXT).await? {
        bail!("Next is offered on the last Task of the column");
    }
    assert_drawer_order(driver).await?;

    // And Previous walks back to the Task the journey started on.
    click(driver, STEP_PREVIOUS, "the Previous Human Review task control").await?;
    ui::visible(driver, &drawer_of(&current))
        .await
        .context("Previous did not open the preceding Task of the column")?;
    await_position(driver, "1 of 2 in Human Review").await?;
    expect_open(driver, &current, &current_title).await?;

    // Stepping changed no decision and no status.
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    for id in [&first, &second] {
        let task = find_task(&listed, id).context("the product no longer lists a Task")?;
        if status_of(&task) != Some("human_review") {
            bail!("stepping moved a Task to {:?}", status_of(&task));
        }
        let entries = task
            .pointer("/human_review/entries")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if entries != 0 {
            bail!("stepping recorded {entries} review decision(s) on {id}");
        }
    }
    Ok(())
}

/// The drawer reads Output, then Changes, then Activity.
async fn assert_drawer_order(driver: &WebDriver) -> Result<()> {
    let order = page(
        driver,
        "const ids = ['task-drawer-tab-output', 'task-drawer-tab-changes', 'task-drawer-timeline']; \
         const els = ids.map(id => document.querySelector(`[data-testid=\"task-drawer\"] [data-testid=\"${id}\"]`)); \
         if (els.some(e => !e)) { return ids.filter((_, i) => !els[i]); } \
         return els.every((e, i) => i === 0 || (els[i - 1].compareDocumentPosition(e) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0);",
        vec![],
    )
    .await?;
    if order != Value::Bool(true) {
        bail!("the drawer does not read Output, Changes, Activity in that order (missing or out of order: {order})");
    }
    Ok(())
}

fn drawer_of(task_id: &str) -> String {
    format!("{TASK_DRAWER}[data-task-id=\"{task_id}\"]")
}

/// The drawer shows this Task, and not another.
async fn expect_open(driver: &WebDriver, task_id: &str, title: &str) -> Result<()> {
    ui::visible(driver, &drawer_of(task_id))
        .await
        .with_context(|| format!("the drawer does not show the task {task_id}"))?;
    let text = drawer_text(driver).await?;
    if !text.contains(title) {
        bail!("the drawer for {task_id} does not read {title:?}");
    }
    Ok(())
}

async fn await_position(driver: &WebDriver, expected: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let shown = text_of(driver, STEP_POSITION).await?.unwrap_or_default();
        if shown == expected {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the position reads {shown:?}, not {expected:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}
