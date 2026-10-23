//! Manual review mode: a Task whose person chose Manual is not reviewed by an
//! AI.
//!
//! The coding Worker runs once, its work is committed and recorded, and the
//! Task waits in Human Review. No reviewer or review-fix agent is launched, no
//! QA verdict is recorded, and the timeline says that AI Review was not run.

use super::human_review::{install_fakes, read_task};
use super::task_runs::run_to_review;
use super::*;

// --- Manual review ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_manual_task_reaches_human_review_without_any_ai_review() {
    let context = TestContext::new("manual_review").expect("harness setup");
    let outcome = manual_review_journey(&context).await;
    context.finish(outcome);
}

async fn manual_review_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = install_fakes(context, &root)?;
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);
    let repository = GitFixture::create_local_only(&root.join("fixture-repo"))?;

    let session = context.start_session("manual-review").await?;
    let outcome = run_manually(session.driver(), &repository).await;
    context
        .close_session(session, "manual-review", &outcome)
        .await?;
    let executed = outcome?;

    // One coding run, and no review of any kind: the reviewer and the
    // review-fix agent are both absent.
    assert_agent_roles(&agent, "manual_review_journey", 1, 0, 0)?;
    if !gh.invocations()?.is_empty() {
        bail!("gh was run for a manual review Task: {:?}", gh.invocations()?);
    }

    let persisted = super::human_review::persisted_review(&root, &executed.id)?;
    if persisted.status != "human_review" || !persisted.entries.is_empty() {
        bail!(
            "the task file holds {} with review {:?}; expected human_review with no decision",
            persisted.status,
            persisted.entries
        );
    }
    Ok(())
}

async fn run_manually(driver: &WebDriver, repository: &GitFixture) -> Result<ExecutedTask> {
    ui::assert_frontend_is_real(driver).await?;
    let Prerequisites {
        project_id,
        task_id,
        title,
    } = create_prerequisites(
        driver,
        repository,
        "Manual review",
        "Its coding run is followed by a person, not by an AI review.",
    )
    .await?;

    // The person's choice, made before the run starts.
    ui::invoke(
        driver,
        "update_task",
        json!({ "params": { "taskId": task_id, "reviewMode": "manual" } }),
    )
    .await?;

    let executed = run_to_review(driver, &project_id, &task_id, &title).await?;

    let task = read_task(driver, &executed).await?;
    if task.get("review_mode").and_then(Value::as_str) != Some("manual") {
        bail!("the Task does not record Manual review mode: {task}");
    }
    if !task.get("qa_signoff").is_some_and(Value::is_null) {
        bail!("Manual review invented an AI verdict: {task}");
    }
    let skipped = task
        .get("activity")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry.pointer("/kind/type").and_then(Value::as_str) == Some("ai_review_skipped")
        })
        .count();
    if skipped != 1 {
        bail!("expected one AI review skipped entry on the timeline, found {skipped}: {task}");
    }
    let started = task
        .get("activity")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|entry| entry.pointer("/kind/type").and_then(Value::as_str) == Some("ai_review_started"));
    if started {
        bail!("a reviewer was admitted for a Manual Task: {task}");
    }
    Ok(executed)
}
