// SPDX-License-Identifier: MPL-2.0
//! A request to open a pull request whose outcome SlashIt never learned, and
//! what a person can do about it.
//!
//! The journey leaves a request unanswered the way a lost connection does,
//! shows the recovery in the drawer, has the person confirm that no pull
//! request exists, and then retries normally. What it holds the product to is
//! read from places that cannot vouch for each other: the product's own
//! `list_tasks`, the task file after a restart, the invocations the fake `gh`
//! recorded, and the refs in the fixture's `origin`.

use super::human_review::{
    attribute, await_task, click, install_fakes, open_reviewed_drawer, persisted_review, read_task,
    HR_APPROVE, HR_DELIVERY, HR_RETRY_PR,
};
use super::*;
use slashit_acceptance::fake_gh::FakeGh;

const PANEL: &str = "[data-testid=\"unconfirmed-pr\"]";
const REPOSITORY: &str = "[data-testid=\"unconfirmed-pr-repository\"]";
const BRANCH: &str = "[data-testid=\"unconfirmed-pr-branch\"]";
const COMMIT: &str = "[data-testid=\"unconfirmed-pr-commit\"]";
const LINK_START: &str = "[data-testid=\"unconfirmed-pr-link-start\"]";
const DISCARD_START: &str = "[data-testid=\"unconfirmed-pr-discard-start\"]";
const DISCARD: &str = "[data-testid=\"unconfirmed-pr-discard\"]";
const ACK: &str = "[data-testid=\"unconfirmed-pr-ack\"]";
const DISCARD_CONFIRM: &str = "[data-testid=\"unconfirmed-pr-discard-confirm\"]";
const NOTE: &str = "[data-testid=\"unconfirmed-pr-note\"]";
const HR_ERROR: &str = "[data-testid=\"human-review-pr-error\"]";
const HR_HINT: &str = "[data-testid=\"human-review-unconfirmed-hint\"]";

/// A request SlashIt sent and never heard back about stays unanswered across a
/// restart, blocks a second one, and is settled only by the person: who sees
/// the repository, branch and commit, must acknowledge the risk, and whose
/// confirmation sends nothing to GitHub. A normal retry then opens exactly one
/// pull request.
#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_pull_request_request_is_settled_by_the_person_and_a_retry_opens_exactly_one() {
    let context = TestContext::new("pr_recovery").expect("harness setup");
    let outcome = recovery_journey(&context).await;
    context.finish(outcome);
}

async fn recovery_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = install_fakes(context, &root)?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    repository.point_origin_at_github()?;
    gh.fail_pr_creation_unanswered(true)?;

    let session = context.start_session("unanswered").await?;
    let outcome = leave_the_request_unanswered(session.driver(), &agent, &gh, &repository).await;
    context.close_session(session, "unanswered", &outcome).await?;
    let executed = outcome?;

    let persisted = persisted_review(&root, &executed.id)?;
    if persisted.status != "human_review" {
        bail!("the task file holds {}", persisted.status);
    }

    let session = context.start_session("recover").await?;
    let outcome = recover_and_retry(session.driver(), &gh, &repository, &executed).await;
    context.close_session(session, "recover", &outcome).await?;
    outcome?;

    assert_persisted(&root, &executed, "pr_created")?;
    let persisted = persisted_review(&root, &executed.id)?;
    if persisted.pr_error.is_some() {
        bail!("after the pull request was opened the task file keeps an error: {:?}", persisted.pr_error);
    }
    Ok(())
}

fn unconfirmed(task: &Value) -> Option<&Value> {
    task.get("pr_create_unconfirmed").filter(|v| !v.is_null())
}

async fn panel_state(driver: &WebDriver) -> Result<String> {
    attribute(driver, PANEL, "data-state")
        .await?
        .context("the recovery panel has no state")
}

async fn await_panel_state(driver: &WebDriver, expected: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        if count(driver, PANEL).await? > 0 && panel_state(driver).await? == expected {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the recovery panel never showed {expected}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn leave_the_request_unanswered(
    driver: &WebDriver,
    agent: &FakeAgent,
    gh: &FakeGh,
    repository: &GitFixture,
) -> Result<ExecutedTask> {
    let executed = execute_one_task(driver, agent, repository).await?;
    open_reviewed_drawer(driver, &executed).await?;
    click(driver, HR_APPROVE, "Approve & Create PR").await?;

    let task = await_task(driver, &executed, "an unanswered request", |t| unconfirmed(t).is_some()).await?;
    if gh.create_attempts()? != 1 {
        bail!("gh pr create ran {} times", gh.create_attempts()?);
    }
    if gh.pr_for(task["branch_name"].as_str().unwrap_or_default())?.is_some() {
        bail!("the fake opened a pull request, so the request was not left unanswered");
    }

    // Shown in the drawer, with what the person needs to judge it.
    await_panel_state(driver, "unknown").await?;
    let shown = await_text(driver, REPOSITORY, |t| !t.is_empty() && t != "not known", "the repository").await?;
    if shown != "slashit-acceptance/fixture" {
        bail!("the panel names the repository {shown:?}");
    }
    let branch = task["branch_name"].as_str().context("no branch")?;
    let commit = unconfirmed(&task).and_then(|u| u["commit"].as_str()).context("no commit")?;
    if text_of(driver, BRANCH).await?.as_deref() != Some(branch) || text_of(driver, COMMIT).await?.as_deref() != Some(commit) {
        bail!("the panel does not show the branch {branch} at {commit}");
    }
    ui::visible(driver, HR_HINT).await.context("the failed delivery does not point at the recovery")?;

    // GitHub would now answer, but SlashIt must not ask again: a second pull
    // request could duplicate the one that may exist.
    gh.fail_pr_creation_unanswered(false)?;
    click(driver, HR_RETRY_PR, "Retry Create PR").await?;
    let refused = await_text(driver, HR_ERROR, |t| t.contains("did not open another"), "the refusal").await?;
    if !refused.contains("recovery") {
        bail!("the refusal does not point at the recovery: {refused}");
    }
    if gh.create_attempts()? != 1 {
        bail!("a retry while the request was unanswered ran gh pr create");
    }
    Ok(executed)
}

async fn recover_and_retry(
    driver: &WebDriver,
    gh: &FakeGh,
    repository: &GitFixture,
    executed: &ExecutedTask,
) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    show_on_board(driver, &executed.project_id, HUMAN_REVIEW_COLUMN, &executed.title).await?;
    open_reviewed_drawer(driver, executed).await?;

    // After a restart the request is still unanswered, from the task file.
    await_panel_state(driver, "unknown").await?;
    let task = read_task(driver, executed).await?;
    let branch = task["branch_name"].as_str().context("no branch")?.to_string();
    let origin_before = repository.origin_tip(&branch)?;
    let attempts_before = gh.create_attempts()?;

    // Not a single click: the confirmation needs an explicit acknowledgement.
    click(driver, DISCARD_START, "No pull request exists...").await?;
    ui::visible(driver, DISCARD).await.context("the confirmation did not open")?;
    if !disabled(driver, DISCARD_CONFIRM).await? {
        bail!("the discard is offered before the risk is acknowledged");
    }
    click(driver, ACK, "the acknowledgement").await?;
    if disabled(driver, DISCARD_CONFIRM).await? {
        bail!("the discard is still refused after the acknowledgement");
    }
    click(driver, DISCARD_CONFIRM, "the confirmation").await?;

    let task = await_task(driver, executed, "the request discarded", |t| unconfirmed(t).is_none()).await?;
    await_panel_state(driver, "resolved_by_person").await?;
    let note = await_text(driver, NOTE, |t| t.contains("You confirmed"), "the person's decision").await?;
    if !note.contains("GitHub did not confirm") {
        bail!("the decision is not worded as the person's own: {note}");
    }
    let recorded = task["activity"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|e| e.pointer("/kind/type").and_then(Value::as_str) == Some("pr_create_unconfirmed_discarded"));
    if !recorded {
        bail!("the decision is not on the task's timeline: {}", task["activity"]);
    }
    if !task
        .pointer("/human_review/pr_error")
        .and_then(Value::as_str)
        .is_some_and(|e| e.contains("You confirmed"))
    {
        bail!("the failure the drawer keeps still tells the person to try again: {:?}", task.pointer("/human_review/pr_error"));
    }

    // The confirmation sent nothing.
    if gh.create_attempts()? != attempts_before {
        bail!("recovering ran gh pr create");
    }
    if gh.pr_for(&branch)?.is_some() {
        bail!("recovering opened a pull request");
    }
    if repository.origin_tip(&branch)? != origin_before {
        bail!("recovering pushed to origin");
    }
    if status_of(&task) != Some("human_review") {
        bail!("recovering moved the task to {:?}", status_of(&task));
    }

    // A normal retry, under the approval that still stands, opens exactly one.
    click(driver, HR_RETRY_PR, "Retry Create PR").await?;
    let task = await_task(driver, executed, "PR Created", |t| status_of(t) == Some("pr_created")).await?;
    if gh.create_attempts()? != attempts_before + 1 {
        bail!("expected exactly one more gh pr create, saw {}", gh.create_attempts()? - attempts_before);
    }
    if gh.pr_for(&branch)?.as_deref() != task.get("pr_url").and_then(Value::as_str) {
        bail!("the task does not record the pull request gh opened");
    }
    if unconfirmed(&task).is_some() {
        bail!("the opened pull request left the request unanswered");
    }
    if attribute(driver, HR_DELIVERY, "data-delivery").await?.as_deref() != Some("created") {
        bail!("the drawer does not show the pull request");
    }
    // Not offered any more: nothing is unanswered, and the pull request is linked.
    let started = Instant::now();
    while count(driver, PANEL).await? != 0 {
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the recovery panel is still shown for a linked pull request");
        }
        tokio::time::sleep(POLL).await;
    }
    for control in [LINK_START, DISCARD_START] {
        assert_not_offered(driver, control, "a recovery action for a linked pull request").await?;
    }
    Ok(())
}
