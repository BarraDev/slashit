// SPDX-License-Identifier: MPL-2.0
//! The AI Reviewer policy, through the product's own surfaces.
//!
//! A Task created in the editor keeps the reviewer and review mode the person
//! chose. A reviewer the command refuses changes nothing. An automatic Task
//! with no reviewer does not borrow one: its coding run ends in Human Review,
//! and the screen says that no AI review ran, why, and that nothing was
//! approved by AI.

use super::human_review::{click, install_fakes, read_task, HR_PANEL};
use super::task_runs::{register_project, run_to_review};
use super::*;
use thirtyfour::components::SelectElement;

const NEW_TASK_BUTTON: &str = "[data-testid=\"new-task-button\"]";
const TITLE_INPUT: &str = "input[placeholder=\"Task title...\"]";
const CREATE_BUTTON: &str = "//button[normalize-space(.)='Create Task']";
const REVIEWER_SELECT: &str = "//select[option[contains(., 'Not chosen: AI Review will not run')]]";
const CARD_NO_VERDICT: &str = "[data-testid=\"task-card-no-verdict\"]";
const DRAWER_NO_AI_REVIEW: &str = "[data-testid=\"task-drawer-no-ai-review\"]";
const HR_AI_STATUS: &str = "[data-testid=\"human-review-ai-status\"]";
const CREATE_DEADLINE: Duration = Duration::from_secs(20);

// --- Creating in the editor -------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_task_created_in_the_editor_keeps_the_reviewer_chosen() {
    let context = TestContext::new("reviewer_policy_create").expect("harness setup");
    let outcome = create_in_editor_journey(&context).await;
    context.finish(outcome);
}

async fn create_in_editor_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    // The reviewer control lists Claude models only when the Claude CLI is on
    // PATH, and the harness puts its fake agent there.
    let (_agent, _gh) = install_fakes(context, &root)?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("reviewer-create").await?;
    let outcome = create_in_editor(session.driver(), &repository).await;
    context
        .close_session(session, "reviewer-create", &outcome)
        .await?;
    outcome
}

async fn create_in_editor(driver: &WebDriver, repository: &GitFixture) -> Result<()> {
    let project_id = register_project(driver, repository).await?;
    open_board(driver, &project_id).await?;

    let title = unique_title("Reviewed in the editor");
    click(driver, NEW_TASK_BUTTON, "the New Task button").await?;
    ui::visible(driver, TITLE_INPUT)
        .await
        .context("the Create Task form shows no title field")?
        .send_keys(&title)
        .await?;
    choose(
        driver,
        REVIEWER_SELECT,
        "Claude Sonnet",
        "the AI Reviewer control",
    )
    .await?;
    driver
        .find(By::XPath(CREATE_BUTTON))
        .await
        .context("the Create Task form shows no Create Task button")?
        .click()
        .await?;

    let created = await_task_titled(driver, &project_id, &title).await?;
    if created.get("reviewer_model").and_then(Value::as_str) != Some("sonnet") {
        bail!("the editor's reviewer choice was not kept: {created}");
    }
    // The Create form offers no Worker or review mode: it creates Claude with
    // Automatic review, as before, and says so in its own comments.
    // The defaults are omitted from a stored Task, so an absent key is the default.
    if created
        .get("provider")
        .is_some_and(|value| value.as_str() != Some("claude"))
    {
        bail!("a Task created in the form is not Claude: {created}");
    }
    if created
        .get("review_mode")
        .is_some_and(|value| value.as_str() != Some("automatic"))
    {
        bail!("a Task created in the form is not Automatic: {created}");
    }
    Ok(())
}

// --- A refused reviewer -----------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_reviewer_through_the_command_changes_no_task() {
    let context = TestContext::new("reviewer_policy_refused").expect("harness setup");
    let outcome = refused_reviewer_journey(&context).await;
    context.finish(outcome);
}

async fn refused_reviewer_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (_agent, _gh) = install_fakes(context, &root)?;
    let repository = GitFixture::create_local_only(&root.join("fixture-repo"))?;

    let session = context.start_session("reviewer-refused").await?;
    let outcome = refuse_then_change(session.driver(), &repository).await;
    context
        .close_session(session, "reviewer-refused", &outcome)
        .await?;
    outcome
}

async fn refuse_then_change(driver: &WebDriver, repository: &GitFixture) -> Result<()> {
    let prerequisites = create_prerequisites(
        driver,
        repository,
        "Reviewer refused",
        "Exercises an invalid reviewer through the update command.",
    )
    .await?;
    let project_id = prerequisites.project_id;
    let task_id = prerequisites.task_id;
    let before = listed_task(driver, &project_id, &task_id).await?;

    // The provider default is not a reviewer choice; the command refuses it
    // with its own reason, and the Task is written as it was.
    for refused in ["default", "ollama/llama3.2:3b"] {
        let reason = ui::invoke_expecting_refusal(
            driver,
            "update_task",
            json!({ "params": { "taskId": task_id, "reviewerModel": refused } }),
        )
        .await?;
        if reason.trim().is_empty() {
            bail!("the refusal of {refused:?} gave no reason");
        }
        let after = listed_task(driver, &project_id, &task_id).await?;
        if after != before {
            bail!("refusing {refused:?} changed the Task: before {before}, after {after}");
        }
    }

    // A valid choice is written, and an empty one clears it.
    ui::invoke(
        driver,
        "update_task",
        json!({ "params": { "taskId": task_id, "reviewerModel": "opus" } }),
    )
    .await?;
    let chosen = listed_task(driver, &project_id, &task_id).await?;
    if chosen.get("reviewer_model").and_then(Value::as_str) != Some("opus") {
        bail!("a valid reviewer was not written: {chosen}");
    }
    ui::invoke(
        driver,
        "update_task",
        json!({ "params": { "taskId": task_id, "reviewerModel": "" } }),
    )
    .await?;
    let cleared = listed_task(driver, &project_id, &task_id).await?;
    if cleared
        .get("reviewer_model")
        .is_some_and(|value| !value.is_null())
    {
        bail!("an empty reviewer did not clear the Task's reviewer: {cleared}");
    }
    Ok(())
}

// --- No reviewer -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn an_automatic_task_without_a_reviewer_stops_at_human_review_and_says_why() {
    let context = TestContext::new("reviewer_policy_skip").expect("harness setup");
    let outcome = skipped_review_journey(&context).await;
    context.finish(outcome);
}

async fn skipped_review_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = install_fakes(context, &root)?;
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);
    let repository = GitFixture::create_local_only(&root.join("fixture-repo"))?;

    let session = context.start_session("reviewer-skip").await?;
    let outcome = skip_and_show(session.driver(), &repository).await;
    context
        .close_session(session, "reviewer-skip", &outcome)
        .await?;
    let executed = outcome?;

    // One coding run and no reviewer: the reviewer is never launched.
    assert_agent_roles(&agent, "reviewer_policy_skip_journey", 1, 0, 0)?;
    if !gh.invocations()?.is_empty() {
        bail!(
            "gh was run for a Task with no reviewer: {:?}",
            gh.invocations()?
        );
    }
    let persisted = super::human_review::persisted_review(&root, &executed.id)?;
    if persisted.status != "human_review" {
        bail!("the Task is {} instead of human_review", persisted.status);
    }
    Ok(())
}

async fn skip_and_show(driver: &WebDriver, repository: &GitFixture) -> Result<ExecutedTask> {
    ui::assert_frontend_is_real(driver).await?;
    let Prerequisites {
        project_id,
        task_id,
        title,
    } = create_prerequisites(
        driver,
        repository,
        "Unreviewed",
        "Its coding run has no reviewer, so no AI review may run for it.",
    )
    .await?;
    // The person chose no reviewer. The product must not choose one for them.
    ui::invoke(
        driver,
        "update_task",
        json!({ "params": { "taskId": task_id, "reviewerModel": "" } }),
    )
    .await?;

    let executed = run_to_review(driver, &project_id, &task_id, &title).await?;
    // The board keeps the cards it loaded, so reload it to show the finished run.
    open_board(driver, &project_id).await?;

    let task = read_task(driver, &executed).await?;
    if !task.get("qa_signoff").is_some_and(Value::is_null) {
        bail!("a Task with no reviewer carries an AI verdict: {task}");
    }
    let activity = task
        .get("activity")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let skipped = activity
        .iter()
        .filter(|entry| {
            entry.pointer("/kind/type").and_then(Value::as_str) == Some("ai_review_skipped")
        })
        .filter_map(|entry| entry.pointer("/kind/reason").and_then(Value::as_str))
        .any(|reason| reason.contains("No AI reviewer model was configured"));
    if !skipped {
        bail!("the skip and its reason are not on the timeline: {task}");
    }
    if activity.iter().any(|entry| {
        entry.pointer("/kind/type").and_then(Value::as_str) == Some("ai_review_started")
    }) {
        bail!("a reviewer was admitted for a Task with no reviewer: {task}");
    }

    // The board card names the skip, and the drawer gives the reason.
    await_text(
        driver,
        CARD_NO_VERDICT,
        |text| text.contains("AI review skipped"),
        "the card's account of the skipped review",
    )
    .await?;
    open_drawer(driver, &executed.id, &title).await?;
    await_text(
        driver,
        DRAWER_NO_AI_REVIEW,
        |text| {
            text.contains("AI review was skipped")
                && text.contains("No AI reviewer model was configured")
                && text.contains("Nothing has been approved by AI")
        },
        "the drawer's account of the skipped review",
    )
    .await?;

    // The Human Review panel carries the same reason, and is where the
    // person decides.
    ui::visible(driver, HR_PANEL)
        .await
        .context("the Human Review panel is not shown for the Task")?;
    await_text(
        driver,
        HR_AI_STATUS,
        |text| text.contains("AI review was skipped"),
        "the Human Review panel's account of the skipped review",
    )
    .await?;
    Ok(executed)
}

// --- Helpers -----------------------------------------------------------------

/// Pick the option whose text starts with `text` in the select `xpath` finds.
async fn choose(driver: &WebDriver, xpath: &str, text: &str, what: &str) -> Result<()> {
    let select = driver
        .find(By::XPath(xpath))
        .await
        .with_context(|| format!("the Create Task form shows no {what}"))?;
    SelectElement::new(&select)
        .await?
        .select_by_partial_text(text)
        .await
        .with_context(|| format!("{what} does not offer {text:?}"))?;
    Ok(())
}

/// The product's own record of one Task, as the board lists it.
async fn listed_task(driver: &WebDriver, project_id: &str, task_id: &str) -> Result<Value> {
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    listed
        .as_array()
        .and_then(|tasks| {
            tasks
                .iter()
                .find(|task| task.get("id").and_then(Value::as_str) == Some(task_id))
        })
        .cloned()
        .with_context(|| format!("the product no longer lists Task {task_id}"))
}

/// A title no other journey or earlier run has used.
fn unique_title(prefix: &str) -> String {
    format!(
        "{prefix} {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    )
}

/// The product's own record of the Task with this title, once it is listed.
async fn await_task_titled(driver: &WebDriver, project_id: &str, title: &str) -> Result<Value> {
    let started = Instant::now();
    loop {
        let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
        let found = listed.as_array().and_then(|tasks| {
            tasks
                .iter()
                .find(|task| task.get("title").and_then(Value::as_str) == Some(title))
        });
        if let Some(task) = found {
            return Ok(task.clone());
        }
        if started.elapsed() > CREATE_DEADLINE {
            bail!("the product never listed a Task titled {title}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
