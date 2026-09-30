//! Pull request status: what GitHub says about a task's pull request, on its
//! card and in its drawer.
//!
//! The fake `gh` stands in for GitHub; the journey scripts what `pr view`
//! answers and follows the answer to the screen through the product's own
//! refreshes: opening the board, the drawer's Refresh and the card's.

use super::human_review::{click, install_fakes};
use super::*;
use slashit_acceptance::fake_gh::FakeGh;

const PR_URL: &str = "https://github.com/slashit-acceptance/fixture/pull/1";
const DRAWER_PR: &str = "[data-testid=\"task-drawer-pr\"]";
const DRAWER_PR_STATE: &str = "[data-testid=\"task-drawer-pr-state\"]";
const DRAWER_PR_CHECKS: &str = "[data-testid=\"task-drawer-pr-checks\"]";
const DRAWER_PR_REFRESH: &str = "[data-testid=\"task-drawer-pr-refresh\"]";
const DRAWER_FAILING_CHECK: &str = "[data-testid=\"task-drawer-pr-failing-check\"]";
/// How slow GitHub is made to answer while a Refresh is watched waiting.
const SLOW_VIEW_SECONDS: u32 = 3;
/// How soon a pressed Refresh must reach `gh`. Far inside the background
/// poll's thirty seconds, so a poll landing in this window is unlikely; the
/// drawer's Refresh above is proven without that assumption.
const PRESS_TO_GH: Duration = Duration::from_secs(3);

fn answer(rollup: Value) -> Value {
    json!({
        "state": "OPEN",
        "statusCheckRollup": rollup,
        "reviewDecision": "REVIEW_REQUIRED",
        "mergeable": "MERGEABLE",
    })
}

fn running() -> Value {
    answer(json!([{ "__typename": "CheckRun", "name": "build", "status": "IN_PROGRESS", "conclusion": "" }]))
}

fn failing() -> Value {
    answer(json!([
        { "__typename": "CheckRun", "name": "unit tests", "status": "COMPLETED", "conclusion": "FAILURE" },
        { "__typename": "StatusContext", "context": "lint", "state": "SUCCESS" },
    ]))
}

fn passing() -> Value {
    answer(json!([
        { "__typename": "CheckRun", "name": "unit tests", "status": "COMPLETED", "conclusion": "SUCCESS" },
        { "__typename": "StatusContext", "context": "lint", "state": "SUCCESS" },
    ]))
}

/// A task with an open pull request shows its CI on the card and in the
/// drawer, and each refresh shows what GitHub says now.
#[tokio::test(flavor = "multi_thread")]
async fn a_pull_requests_ci_status_shows_on_its_card_and_in_its_drawer() {
    let context = TestContext::new("pr_status").expect("harness setup");
    let outcome = pr_status_journey(&context).await;
    context.finish(outcome);
}

async fn pr_status_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (_agent, gh) = install_fakes(context, &root)?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    gh.script_pr_view(&running())?;

    let session = context.start_session("pr-status").await?;
    let outcome = follow_the_checks(session.driver(), &gh, &repository).await;
    context.close_session(session, "pr-status", &outcome).await?;
    outcome
}

async fn follow_the_checks(driver: &WebDriver, gh: &FakeGh, repository: &GitFixture) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let Prerequisites { project_id, task_id, title } =
        create_prerequisites(driver, repository, "PR status", "Never runs.").await?;
    ui::invoke(driver, "link_pr", json!({ "taskId": task_id, "prUrl": PR_URL })).await?;
    ui::invoke(driver, "update_task_status", json!({ "taskId": task_id, "status": "pr_created" })).await?;

    // Opening the board shows the first reading. Either the board's own
    // check or the background poll may be what asked; the refreshes below
    // are each proven to ask themselves.
    open_board(driver, &project_id).await?;
    await_badge(driver, &task_id, "ci_running", "CI running").await?;

    // GitHub now reports a failed check. The drawer's Refresh asks again.
    gh.script_pr_view(&failing())?;
    open_drawer(driver, &task_id, &title).await?;
    ui::visible(driver, DRAWER_PR).await.context("the drawer has no Pull request section")?;
    await_text(driver, DRAWER_PR_STATE, |t| t == "Open", "the pull request as Open").await?;
    // GitHub answers slowly, so the drawer is seen waiting on this press's
    // own request: the button holds "Refreshing…" until `gh` answers, which
    // no background poll can make it do.
    gh.delay_pr_view(Some(SLOW_VIEW_SECONDS))?;
    // Timed from before the click, so a slow click only lengthens it: a
    // Refresh that never waited on `gh` still cannot reach the delay.
    let pressed = Instant::now();
    click(driver, DRAWER_PR_REFRESH, "the pull request's Refresh").await?;
    await_text(driver, DRAWER_PR_REFRESH, |t| t == "Refreshing…", "Refreshing… on the pressed button").await?;
    await_text(driver, DRAWER_PR_REFRESH, |t| t == "Refresh", "Refresh again once GitHub answered").await?;
    if pressed.elapsed() < Duration::from_secs(SLOW_VIEW_SECONDS.into()) {
        bail!("the drawer's Refresh finished in {:?}, before GitHub could have answered", pressed.elapsed());
    }
    gh.delay_pr_view(None)?;
    await_badge(driver, &task_id, "ci_failing", "CI failing").await?;
    await_text(driver, DRAWER_PR_CHECKS, |t| t.starts_with("Failing"), "Checks: Failing").await?;
    let names = texts(driver, DRAWER_FAILING_CHECK).await?;
    if names != ["unit tests"] {
        bail!("the drawer names the failing checks {names:?}, expected only \"unit tests\"");
    }

    // The checks pass on a new push. The card's own Refresh shows it, from
    // a column that is not waiting on the pull request: Refresh observes, and
    // must not move the task to PR Created or otherwise take it back.
    gh.script_pr_view(&passing())?;
    close_drawer(driver).await?;
    ui::invoke(driver, "update_task_status", json!({ "taskId": task_id, "status": "backlog" })).await?;
    let asked = gh.view_count()?;
    click(
        driver,
        // Found in the Backlog column, so the board has redrawn the card
        // there before it is pressed.
        &format!("{BACKLOG_COLUMN} [data-card-task-id=\"{task_id}\"] [data-testid=\"task-card-pr-refresh\"]"),
        "the card's Refresh",
    )
    .await?;
    await_asked_since(gh, asked).await?;
    await_badge(driver, &task_id, "ci_passed", "CI passed").await?;
    open_drawer(driver, &task_id, &title).await?;
    await_text(driver, DRAWER_PR_CHECKS, |t| t == "Passing", "Checks: Passing").await?;
    if !texts(driver, DRAWER_FAILING_CHECK).await?.is_empty() {
        bail!("the drawer still names a failing check after the checks passed");
    }

    // Refreshing is not a change to the task: its column is what it was.
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    let task = find_task(&listed, &task_id).context("the task disappeared")?;
    if status_of(&task) != Some("backlog") {
        bail!("the card's Refresh moved the task to {:?}", status_of(&task));
    }
    Ok(())
}

/// Wait until the card's pull request badge shows `signal`, labelled `label`.
async fn await_badge(driver: &WebDriver, task_id: &str, signal: &str, label: &str) -> Result<()> {
    const READ: &str = r#"
const badge = document.querySelector(`[data-card-task-id="${arguments[0]}"] [data-testid="task-card-pr"]`);
if (!badge) return null;
const shown = badge.querySelector('[data-testid="task-card-pr-signal"]');
return { signal: badge.getAttribute('data-signal'), stale: badge.getAttribute('data-stale'), text: shown ? shown.textContent.trim() : null };
"#;
    let started = Instant::now();
    loop {
        let shown = page(driver, READ, vec![Value::String(task_id.to_string())]).await?;
        let text = shown.get("text").and_then(Value::as_str).unwrap_or("");
        if shown.get("signal").and_then(Value::as_str) == Some(signal)
            && text.ends_with(label)
            && shown.get("stale").and_then(Value::as_str) == Some("false")
        {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the card never showed {label:?}; its pull request badge shows {shown}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn texts(driver: &WebDriver, selector: &str) -> Result<Vec<String>> {
    let found = page(
        driver,
        "return [...document.querySelectorAll(arguments[0])].map(e => e.textContent.trim());",
        vec![Value::String(selector.to_string())],
    )
    .await?;
    Ok(found
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default())
}

/// Wait, briefly, for a `gh pr view` beyond the `before` already counted.
async fn await_asked_since(gh: &FakeGh, before: usize) -> Result<()> {
    let started = Instant::now();
    while gh.view_count()? <= before {
        if started.elapsed() > PRESS_TO_GH {
            bail!("pressing the card's Refresh did not ask GitHub within {PRESS_TO_GH:?}");
        }
        tokio::time::sleep(POLL).await;
    }
    Ok(())
}
