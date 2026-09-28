//! Human Review as a decision point: approving (with and without a pull
//! request), requesting changes, and closing a task without merging it.
//!
//! Every decision is made by clicking the drawer or the board, the way a
//! person makes it. What each journey then holds the product to is read from
//! three places that cannot vouch for each other: the product's own
//! `list_tasks`, the task file on disk (and again after a restart), and the
//! external programs and repositories it acted on -- the prompt the fake agent
//! received, the pull requests the fake `gh` opened, and the refs in git.

use super::*;
use slashit_acceptance::fake_gh::{self, FakeGh};

const HR_PANEL: &str = "[data-testid=\"human-review\"]";
const HR_APPROVE: &str = "[data-testid=\"human-review-approve\"]";
const HR_REQUEST_CHANGES: &str = "[data-testid=\"human-review-request-changes\"]";
const HR_FEEDBACK: &str = "[data-testid=\"human-review-feedback\"]";
const HR_SEND_FEEDBACK: &str = "[data-testid=\"human-review-send-feedback\"]";
const HR_APPROVED: &str = "[data-testid=\"human-review-approved\"]";
const HR_DELIVERY: &str = "[data-testid=\"human-review-delivery\"]";
const HR_PR_ERROR: &str = "[data-testid=\"human-review-pr-error\"]";
const HR_RETRY_PR: &str = "[data-testid=\"human-review-retry-pr\"]";
const HR_PR_LINK: &str = "[data-testid=\"human-review-pr-link\"]";
const HR_ITERATION: &str = "[data-testid=\"human-review-iteration\"]";
const HR_HISTORY_ENTRY: &str = "[data-testid=\"human-review-history-entry\"]";
const DRAWER_REVIEW_VERDICT: &str = "[data-testid=\"task-drawer-review-verdict\"]";
const CLOSE_DIALOG: &str = "[data-testid=\"close-without-merge-dialog\"]";
const CLOSE_CONSEQUENCES: &str = "[data-testid=\"close-without-merge-consequences\"]";
const CLOSE_CANCEL: &str = "[data-testid=\"close-without-merge-cancel\"]";
const CLOSE_CONFIRM: &str = "[data-testid=\"close-without-merge-confirm\"]";
const CLOSE_ERROR: &str = "[data-testid=\"close-without-merge-error\"]";
const PR_CREATED_COLUMN: &str = "[data-testid=\"column-prcreated\"]";

/// The lines the coding prompt fences each piece of feedback with
/// (`queue::prompt` in the backend).
const FEEDBACK_BEGIN: &str = "----- BEGIN HUMAN REVIEW FEEDBACK -----";
const FEEDBACK_END: &str = "----- END HUMAN REVIEW FEEDBACK -----";

/// Lower case on purpose: WebDriver's typed text must come out exactly as
/// written, and nothing here depends on a modifier key.
const FEEDBACK: &str = "count blank lines separately and keep the output format (review-feedback-7f3a)";

impl GitFixture {
    /// Make `origin` read as a GitHub repository, to `git remote get-url` and
    /// to `gh`, while every push still lands in the local bare repository
    /// beside the fixture. Nothing ever fetches from `origin`.
    fn point_origin_at_github(&self) -> Result<()> {
        let push_url = self.origin_path();
        let push_url = push_url.to_str().context("the origin path is not UTF-8")?;
        git(&self.path, &["remote", "set-url", "origin", fake_gh::REPOSITORY_URL])?;
        git(&self.path, &["remote", "set-url", "--push", "origin", push_url])
    }

    fn origin_path(&self) -> PathBuf {
        self.state_root.join("fixture-origin.git")
    }

    /// What `branch` points at in the bare `origin`, if it is there at all.
    fn origin_tip(&self, branch: &str) -> Result<Option<String>> {
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(self.origin_path())
            .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .with_context(|| format!("could not resolve {branch} in origin"))?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()))
    }
}

/// The fake agent and the fake `gh`, both on the application's `PATH`.
fn install_fakes(context: &TestContext, root: &Path) -> Result<(FakeAgent, FakeGh)> {
    let agent = FakeAgent::install(root)?;
    let gh = FakeGh::install(root, &agent)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_gh::DIR_VAR, gh.dir());
    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    Ok((agent, gh))
}

async fn read_task(driver: &WebDriver, executed: &ExecutedTask) -> Result<Value> {
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": executed.project_id })).await?;
    find_task(&listed, &executed.id).with_context(|| format!("the product no longer lists {}", executed.id))
}

fn branch_of(task: &Value) -> Result<String> {
    Ok(task
        .get("branch_name")
        .and_then(Value::as_str)
        .context("the task records no branch")?
        .to_string())
}

/// `(decision, feedback)` for every review entry, oldest first.
fn review_entries(task: &Value) -> Vec<(String, Option<String>)> {
    task.pointer("/human_review/entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|e| {
                    (
                        e.get("decision").and_then(Value::as_str).unwrap_or_default().to_string(),
                        e.get("feedback").and_then(Value::as_str).map(str::to_string),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What the task file holds for one task's review.
struct PersistedReview {
    status: String,
    description: Option<String>,
    /// `(decision, feedback)`, oldest first.
    entries: Vec<(String, Option<String>)>,
    pr_error: Option<String>,
}

/// The same record, as the task file holds it.
fn persisted_review(root: &Path, task_id: &str) -> Result<PersistedReview> {
    for file in toml_files(root)? {
        let Ok(contents) = std::fs::read_to_string(&file) else { continue };
        let Ok(document) = contents.parse::<toml::Value>() else { continue };
        let Some(record) = task_record(&document, task_id) else { continue };
        let status = record.get("status").and_then(toml::Value::as_str).unwrap_or_default().to_string();
        let description = record.get("description").and_then(toml::Value::as_str).map(str::to_string);
        let review = record.get("human_review");
        let entries = review
            .and_then(|r| r.get("entries"))
            .and_then(toml::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| {
                        (
                            e.get("decision").and_then(toml::Value::as_str).unwrap_or_default().to_string(),
                            e.get("feedback").and_then(toml::Value::as_str).map(str::to_string),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let pr_error = review
            .and_then(|r| r.get("pr_error"))
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        return Ok(PersistedReview { status, description, entries, pr_error });
    }
    bail!("no task file under {} holds task {task_id}", root.display())
}

/// Wait until the product reports the task with `accept` true of it.
async fn await_task(
    driver: &WebDriver,
    executed: &ExecutedTask,
    what: &str,
    accept: impl Fn(&Value) -> bool,
) -> Result<Value> {
    let started = Instant::now();
    loop {
        let task = read_task(driver, executed).await?;
        if accept(&task) {
            return Ok(task);
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("the task never reached {what}; it is now {task}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn attribute(driver: &WebDriver, selector: &str, name: &str) -> Result<Option<String>> {
    Ok(ui::visible(driver, selector).await?.attr(name).await?)
}

async fn click(driver: &WebDriver, selector: &str, what: &str) -> Result<()> {
    ui::visible(driver, selector)
        .await
        .with_context(|| format!("{what} is not offered"))?
        .click()
        .await
        .with_context(|| format!("could not press {what}"))
}

async fn await_gone(driver: &WebDriver, selector: &str, what: &str) -> Result<()> {
    let started = Instant::now();
    while count(driver, selector).await? != 0 {
        if started.elapsed() > RENDER_DEADLINE {
            bail!("{what} is still shown");
        }
        tokio::time::sleep(POLL).await;
    }
    Ok(())
}

async fn open_reviewed_drawer(driver: &WebDriver, executed: &ExecutedTask) -> Result<()> {
    open_drawer(driver, &executed.id, &executed.title).await?;
    ui::visible(driver, HR_PANEL).await.context("the drawer shows no Human Review section")?;
    Ok(())
}

// --- Journey A ----------------------------------------------------------------

/// Approve & Create PR: the approval is recorded, the existing pull request
/// flow opens the pull request for the task's own branch, the task reaches
/// PR Created, and after a restart the drawer still says both.
#[tokio::test(flavor = "multi_thread")]
async fn approving_a_task_opens_its_pull_request_and_the_decision_survives_a_restart() {
    let context = TestContext::new("human_review_approve").expect("harness setup");
    let outcome = approve_journey(&context).await;
    context.finish(outcome);
}

async fn approve_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = install_fakes(context, &root)?;
    // Real work, so a real AI review runs and the drawer has a summary.
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    repository.point_origin_at_github()?;

    let session = context.start_session("approve").await?;
    let outcome = approve_and_open_the_pull_request(session.driver(), &agent, &gh, &repository).await;
    context.close_session(session, "approve", &outcome).await?;
    let (executed, pr_url) = outcome?;

    assert_persisted(&root, &executed, "pr_created")?;
    let persisted = persisted_review(&root, &executed.id)?;
    if persisted.entries != vec![("approved".to_string(), None)] || persisted.pr_error.is_some() {
        bail!(
            "the task file holds review {:?} with pull request error {:?}",
            persisted.entries,
            persisted.pr_error
        );
    }

    let session = context.start_session("approve-restart").await?;
    let outcome = still_approved_after_restart(session.driver(), &executed, &pr_url).await;
    context.close_session(session, "approve-restart", &outcome).await?;
    outcome
}

async fn approve_and_open_the_pull_request(
    driver: &WebDriver,
    agent: &FakeAgent,
    gh: &FakeGh,
    repository: &GitFixture,
) -> Result<(ExecutedTask, String)> {
    let executed = execute_one_task(driver, agent, repository).await?;
    let task = read_task(driver, &executed).await?;
    let branch = branch_of(&task)?;
    let branch_tip = repository.branch_tip(&branch)?.context("the task branch is missing")?;
    let main_before = repository.branch_tip("main")?;
    let origin_main_before = repository.origin_tip("main")?;
    if !review_entries(&task).is_empty() {
        bail!("a task that was never reviewed already has decisions: {task}");
    }

    open_reviewed_drawer(driver, &executed).await?;
    // Decision-ready: the AI review's outcome in words, then the choices.
    let verdict = await_text(driver, DRAWER_REVIEW_VERDICT, |t| !t.is_empty(), "the AI review").await?;
    if verdict.contains("Rejected") || verdict.contains("FixesApplied") {
        bail!("the AI review is shown as a raw status: {verdict:?}");
    }
    let approve_label = await_text(driver, HR_APPROVE, |t| t != "Checking…", "the approve action").await?;
    if approve_label != "Approve & Create PR" || attribute(driver, HR_APPROVE, "data-creates-pr").await?.as_deref() != Some("true") {
        bail!("a GitHub project should offer Approve & Create PR, the drawer offers {approve_label:?}");
    }
    ui::visible(driver, HR_REQUEST_CHANGES).await.context("Request Changes is not offered")?;
    if gh.create_attempts()? != 0 {
        bail!("a pull request was attempted before anyone approved");
    }

    // --- The action under test --------------------------------------------
    click(driver, HR_APPROVE, "Approve & Create PR").await?;

    let settled = await_task(driver, &executed, "PR Created", |t| status_of(t) == Some("pr_created")).await?;
    let pr_url = settled
        .get("pr_url")
        .and_then(Value::as_str)
        .context("a task in PR Created records no pull request")?
        .to_string();
    if gh.pr_for(&branch)?.as_deref() != Some(pr_url.as_str()) {
        bail!("the task records {pr_url}, but gh opened {:?} for {branch}", gh.pr_for(&branch)?);
    }
    if gh.create_attempts()? != 1 {
        bail!("gh pr create ran {} times for one approval", gh.create_attempts()?);
    }
    if review_entries(&settled) != vec![("approved".to_string(), None)] {
        bail!("the approval is recorded as {:?}", review_entries(&settled));
    }
    // The branch was delivered as it is; nothing was merged anywhere.
    if repository.origin_tip(&branch)?.as_deref() != Some(branch_tip.as_str()) {
        bail!("origin does not hold {branch} at the reviewed commit {branch_tip}");
    }
    if repository.branch_tip("main")? != main_before || repository.origin_tip("main")? != origin_main_before {
        bail!("approving changed main");
    }

    ui::visible(driver, HR_APPROVED).await.context("the drawer does not say Approved")?;
    if attribute(driver, HR_DELIVERY, "data-delivery").await?.as_deref() != Some("created") {
        bail!("the drawer does not show the pull request as created");
    }
    for (control, what) in [(HR_APPROVE, "Approve, once approved"), (HR_REQUEST_CHANGES, "Request Changes, once approved")] {
        assert_not_offered(driver, control, what).await?;
    }
    close_drawer(driver).await?;
    assert_card_in_column(driver, PR_CREATED_COLUMN, &executed.title).await?;
    Ok((executed, pr_url))
}

async fn still_approved_after_restart(driver: &WebDriver, executed: &ExecutedTask, pr_url: &str) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    show_on_board(driver, &executed.project_id, PR_CREATED_COLUMN, &executed.title).await?;
    let task = read_task(driver, executed).await?;
    if status_of(&task) != Some("pr_created") || review_entries(&task) != vec![("approved".to_string(), None)] {
        bail!("after a restart the task is {task}");
    }
    open_reviewed_drawer(driver, executed).await?;
    ui::visible(driver, HR_APPROVED).await.context("after a restart the drawer does not say Approved")?;
    let link = await_text(driver, HR_PR_LINK, |t| !t.is_empty(), "the pull request").await?;
    if link != pr_url {
        bail!("after a restart the drawer links {link:?}, not {pr_url:?}");
    }
    assert_not_offered(driver, HR_APPROVE, "Approve, after a restart").await
}

// --- Journey B ----------------------------------------------------------------

/// A pull request that cannot be opened keeps the approval, says why in the
/// drawer (still after a restart), and Retry Create PR opens it without a
/// second approval.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_pull_request_keeps_the_approval_and_retrying_needs_no_second_approval() {
    let context = TestContext::new("human_review_pr_failure").expect("harness setup");
    let outcome = pr_failure_journey(&context).await;
    context.finish(outcome);
}

async fn pr_failure_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = install_fakes(context, &root)?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    repository.point_origin_at_github()?;
    gh.fail_pr_creation(true)?;

    let session = context.start_session("pr-failure").await?;
    let outcome = approve_while_github_fails(session.driver(), &agent, &gh, &repository).await;
    context.close_session(session, "pr-failure", &outcome).await?;
    let executed = outcome?;

    let persisted = persisted_review(&root, &executed.id)?;
    if persisted.status != "human_review" || persisted.entries != vec![("approved".to_string(), None)] {
        bail!("the task file holds {} with review {:?}", persisted.status, persisted.entries);
    }
    if !persisted.pr_error.as_deref().is_some_and(|e| e.contains(fake_gh::CREATE_FAILURE)) {
        bail!("the task file does not keep why the pull request failed: {:?}", persisted.pr_error);
    }

    gh.fail_pr_creation(false)?;
    let session = context.start_session("pr-retry").await?;
    let outcome = retry_after_restart(session.driver(), &gh, &executed).await;
    context.close_session(session, "pr-retry", &outcome).await?;
    outcome?;

    assert_persisted(&root, &executed, "pr_created")?;
    let persisted = persisted_review(&root, &executed.id)?;
    if persisted.entries != vec![("approved".to_string(), None)] || persisted.pr_error.is_some() {
        bail!(
            "after the retry the task file holds review {:?}, error {:?}",
            persisted.entries,
            persisted.pr_error
        );
    }
    Ok(())
}

async fn approve_while_github_fails(
    driver: &WebDriver,
    agent: &FakeAgent,
    gh: &FakeGh,
    repository: &GitFixture,
) -> Result<ExecutedTask> {
    let executed = execute_one_task(driver, agent, repository).await?;
    let main_before = repository.branch_tip("main")?;
    open_reviewed_drawer(driver, &executed).await?;
    await_text(driver, HR_APPROVE, |t| t == "Approve & Create PR", "Approve & Create PR").await?;

    click(driver, HR_APPROVE, "Approve & Create PR").await?;

    let task = await_task(driver, &executed, "a recorded pull request failure", |t| {
        t.pointer("/human_review/pr_error").is_some_and(|e| !e.is_null())
    })
    .await?;
    if status_of(&task) != Some("human_review") {
        bail!("a failed pull request moved the task to {:?}", status_of(&task));
    }
    if review_entries(&task) != vec![("approved".to_string(), None)] {
        bail!("the failure changed the decision: {:?}", review_entries(&task));
    }
    if gh.create_attempts()? != 1 {
        bail!("gh pr create ran {} times", gh.create_attempts()?);
    }

    // Inline, in the drawer: approved, and why there is no pull request.
    ui::visible(driver, HR_APPROVED).await.context("a failed pull request hid the approval")?;
    if attribute(driver, HR_DELIVERY, "data-delivery").await?.as_deref() != Some("failed") {
        bail!("the drawer does not show the pull request as failed");
    }
    let shown = await_text(driver, HR_PR_ERROR, |t| !t.is_empty(), "the failure").await?;
    if !shown.contains(fake_gh::CREATE_FAILURE) {
        bail!("the drawer shows {shown:?}, not what gh reported");
    }
    ui::visible(driver, HR_RETRY_PR).await.context("Retry Create PR is not offered")?;
    for (control, what) in [(HR_APPROVE, "Approve, a second time"), (HR_REQUEST_CHANGES, "Request Changes, once approved")] {
        assert_not_offered(driver, control, what).await?;
    }
    if repository.branch_tip("main")? != main_before {
        bail!("approving changed main");
    }
    Ok(executed)
}

async fn retry_after_restart(driver: &WebDriver, gh: &FakeGh, executed: &ExecutedTask) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    show_on_board(driver, &executed.project_id, HUMAN_REVIEW_COLUMN, &executed.title).await?;
    open_reviewed_drawer(driver, executed).await?;
    ui::visible(driver, HR_APPROVED).await.context("after a restart the approval is gone")?;
    let shown = await_text(driver, HR_PR_ERROR, |t| !t.is_empty(), "the failure").await?;
    if !shown.contains(fake_gh::CREATE_FAILURE) {
        bail!("after a restart the drawer shows {shown:?}");
    }

    click(driver, HR_RETRY_PR, "Retry Create PR").await?;

    let task = await_task(driver, executed, "PR Created", |t| status_of(t) == Some("pr_created")).await?;
    if review_entries(&task) != vec![("approved".to_string(), None)] {
        bail!("retrying recorded another decision: {:?}", review_entries(&task));
    }
    if gh.create_attempts()? != 2 {
        bail!("expected one failed and one successful gh pr create, saw {}", gh.create_attempts()?);
    }
    let branch = branch_of(&task)?;
    if gh.pr_for(&branch)?.as_deref() != task.get("pr_url").and_then(Value::as_str) {
        bail!("the task does not record the pull request gh opened");
    }
    if attribute(driver, HR_DELIVERY, "data-delivery").await?.as_deref() != Some("created") {
        bail!("after the retry the drawer does not show the pull request");
    }
    Ok(())
}

// --- Approval without GitHub ----------------------------------------------------

/// A project whose `origin` is not on GitHub can still be approved. Nothing
/// is pushed, merged or finished, and the drawer says why there is no pull
/// request.
#[tokio::test(flavor = "multi_thread")]
async fn approving_without_a_github_remote_records_the_decision_and_merges_nothing() {
    let context = TestContext::new("human_review_no_github").expect("harness setup");
    let outcome = no_github_journey(&context).await;
    context.finish(outcome);
}

async fn no_github_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = install_fakes(context, &root)?;
    // `origin` is the fixture's local bare repository.
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("no-github").await?;
    let outcome = approve_without_github(session.driver(), &agent, &gh, &repository).await;
    context.close_session(session, "no-github", &outcome).await?;
    let executed = outcome?;

    let persisted = persisted_review(&root, &executed.id)?;
    if persisted.status != "human_review" || persisted.entries != vec![("approved".to_string(), None)] {
        bail!("the task file holds {} with review {:?}", persisted.status, persisted.entries);
    }
    Ok(())
}

async fn approve_without_github(
    driver: &WebDriver,
    agent: &FakeAgent,
    gh: &FakeGh,
    repository: &GitFixture,
) -> Result<ExecutedTask> {
    let executed = execute_one_task(driver, agent, repository).await?;
    let branch = branch_of(&read_task(driver, &executed).await?)?;
    let branch_tip = repository.branch_tip(&branch)?;
    let main_before = repository.branch_tip("main")?;
    let origin_main_before = repository.origin_tip("main")?;

    open_reviewed_drawer(driver, &executed).await?;
    let label = await_text(driver, HR_APPROVE, |t| t != "Checking…", "the approve action").await?;
    if label != "Approve" || attribute(driver, HR_APPROVE, "data-creates-pr").await?.as_deref() != Some("false") {
        bail!("without GitHub the drawer should offer a plain Approve, it offers {label:?}");
    }

    click(driver, HR_APPROVE, "Approve").await?;

    ui::visible(driver, HR_APPROVED).await.context("the drawer does not say Approved")?;
    if attribute(driver, HR_DELIVERY, "data-delivery").await?.as_deref() != Some("unavailable") {
        bail!("the drawer does not explain that no pull request can be created");
    }
    let explained = await_text(driver, HR_DELIVERY, |t| !t.is_empty(), "the explanation").await?;
    for expected in ["cannot create a GitHub pull request", "not a GitHub repository", "Nothing was merged"] {
        if !explained.contains(expected) {
            bail!("the explanation {explained:?} does not say {expected:?}");
        }
    }
    let task = read_task(driver, &executed).await?;
    if status_of(&task) != Some("human_review") || task.get("pr_url").is_some_and(|u| !u.is_null()) {
        bail!("approving without GitHub moved or delivered the task: {task}");
    }
    if review_entries(&task) != vec![("approved".to_string(), None)] {
        bail!("the approval is recorded as {:?}", review_entries(&task));
    }
    if !gh.invocations()?.is_empty() {
        bail!("gh was run for a project with no GitHub remote: {:?}", gh.invocations()?);
    }
    if repository.origin_tip(&branch)?.is_some() {
        bail!("{branch} was pushed although no pull request can be created");
    }
    if repository.branch_tip(&branch)? != branch_tip {
        bail!("approving moved the task branch");
    }
    if repository.branch_tip("main")? != main_before || repository.origin_tip("main")? != origin_main_before {
        bail!("approving merged something into main");
    }
    if !executed.worktree_path.is_dir() {
        bail!("approving removed the Task Checkout");
    }
    Ok(executed)
}

// --- Journey C ----------------------------------------------------------------

/// Request Changes: the feedback is kept apart from the description, reaches
/// the next run's prompt in its own section, and the drawer shows the later
/// review with the earlier feedback, also after a restart.
#[tokio::test(flavor = "multi_thread")]
async fn requested_changes_reach_the_next_run_and_the_review_history_survives_a_restart() {
    let context = TestContext::new("human_review_request_changes").expect("harness setup");
    let outcome = request_changes_journey(&context).await;
    context.finish(outcome);
}

async fn request_changes_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, _gh) = install_fakes(context, &root)?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("request-changes").await?;
    let outcome = send_back_with_feedback(session.driver(), &agent, &repository).await;
    context.close_session(session, "request-changes", &outcome).await?;
    let (executed, description) = outcome?;

    let persisted = persisted_review(&root, &executed.id)?;
    if persisted.status != "human_review" {
        bail!("the task file holds status {}", persisted.status);
    }
    if persisted.description.as_deref() != Some(description.as_str()) {
        bail!("the task file's description changed: {:?}", persisted.description);
    }
    if persisted.entries != vec![("changes_requested".to_string(), Some(FEEDBACK.to_string()))] {
        bail!("the task file holds review history {:?}", persisted.entries);
    }

    let session = context.start_session("request-changes-restart").await?;
    let outcome = history_after_restart(session.driver(), &executed).await;
    context.close_session(session, "request-changes-restart", &outcome).await?;
    outcome
}

async fn send_back_with_feedback(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
) -> Result<(ExecutedTask, String)> {
    let executed = execute_one_task(driver, agent, repository).await?;
    let description = read_task(driver, &executed)
        .await?
        .get("description")
        .and_then(Value::as_str)
        .context("the task has no description")?
        .to_string();

    open_reviewed_drawer(driver, &executed).await?;
    click(driver, HR_REQUEST_CHANGES, "Request Changes").await?;
    let send = ui::visible(driver, HR_SEND_FEEDBACK).await.context("no way to send feedback")?;
    if send.prop("disabled").await?.as_deref() != Some("true") {
        bail!("feedback can be sent before anything is written");
    }
    ui::visible(driver, HR_FEEDBACK).await?.send_keys(FEEDBACK).await.context("could not type the feedback")?;

    click(driver, HR_SEND_FEEDBACK, "Send back to the agent").await?;

    // Out of Human Review and run again, from the queue, by the executor.
    let runs = await_agent_runs(agent, 2).await?;
    let returned = await_task(driver, &executed, "Human Review after the rerun", |t| {
        status_of(t) == Some("human_review") && t.pointer("/human_review/arrivals").and_then(Value::as_u64) == Some(2)
    })
    .await?;

    // The prompt the agent actually received.
    let first = runs[0].prompt.as_deref().context("the first run read no prompt")?;
    let second = runs[1].prompt.as_deref().context("the rerun read no prompt")?;
    if first.contains("Human Review Feedback") || first.contains(FEEDBACK) {
        bail!("the first run was given feedback nobody had written yet");
    }
    let fenced = format!("{FEEDBACK_BEGIN}\n{FEEDBACK}\n{FEEDBACK_END}");
    if !second.contains("## Human Review Feedback") || !second.contains(&fenced) {
        bail!("the rerun's prompt does not carry the feedback in its own section:\n{second}");
    }
    let section = &second[second.find("## Human Review Feedback").unwrap()..];
    if section.contains(&description) {
        bail!("the feedback section repeats the description");
    }
    if !second.contains(&format!("## Description\n{description}")) {
        bail!("the rerun's prompt lost the task description:\n{second}");
    }
    if resolve(&runs[1].working_dir) != resolve(&executed.worktree_path) {
        bail!("the rerun did not continue in the task's checkout");
    }

    // The definition of the task is exactly what it was.
    if returned.get("description").and_then(Value::as_str) != Some(description.as_str()) {
        bail!("the description changed: {:?}", returned.get("description"));
    }
    if review_entries(&returned) != vec![("changes_requested".to_string(), Some(FEEDBACK.to_string()))] {
        bail!("the review history is {:?}", review_entries(&returned));
    }

    // Still in the open drawer: a later review, with the earlier feedback,
    // undecided again.
    assert_later_review_shown(driver).await?;
    Ok((executed, description))
}

async fn assert_later_review_shown(driver: &WebDriver) -> Result<()> {
    let iteration = ui::visible(driver, HR_ITERATION).await.context("the drawer does not show this is a later review")?;
    if iteration.attr("data-iteration").await?.as_deref() != Some("2") {
        bail!("the drawer numbers this review {:?}", iteration.attr("data-iteration").await?);
    }
    let earlier = await_text(driver, HR_HISTORY_ENTRY, |t| !t.is_empty(), "the earlier feedback").await?;
    if !earlier.contains(FEEDBACK) {
        bail!("the drawer shows earlier feedback {earlier:?}");
    }
    ui::visible(driver, HR_APPROVE).await.context("the later review cannot be approved")?;
    ui::visible(driver, HR_REQUEST_CHANGES).await.context("the later review cannot be sent back")?;
    Ok(())
}

async fn history_after_restart(driver: &WebDriver, executed: &ExecutedTask) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    show_on_board(driver, &executed.project_id, HUMAN_REVIEW_COLUMN, &executed.title).await?;
    let task = read_task(driver, executed).await?;
    if review_entries(&task) != vec![("changes_requested".to_string(), Some(FEEDBACK.to_string()))] {
        bail!("after a restart the review history is {:?}", review_entries(&task));
    }
    open_reviewed_drawer(driver, executed).await?;
    assert_later_review_shown(driver).await
}

// --- Journey D ----------------------------------------------------------------

/// Done from Human Review asks first, says what it does, changes nothing
/// when cancelled, reports a close Git refused as not done, and when
/// confirmed over a clean checkout closes the task without merging: the
/// checkout is removed and the work stays on its branch.
#[tokio::test(flavor = "multi_thread")]
async fn closing_a_reviewed_task_without_merging_is_confirmed_first_and_keeps_its_work() {
    let context = TestContext::new("human_review_close").expect("harness setup");
    let outcome = close_journey(&context).await;
    context.finish(outcome);
}

async fn close_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = install_fakes(context, &root)?;
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("close").await?;
    let outcome = close_without_merging(session.driver(), &agent, &gh, &repository).await;
    context.close_session(session, "close", &outcome).await?;
    let (executed, work) = outcome?;

    assert_persisted(&root, &executed, "done")?;
    if executed.worktree_path.exists() {
        bail!("the Task Checkout outlived the application that removed it");
    }
    assert_work_survives(&repository, &work, "closing the task without merging")
}

async fn close_without_merging(
    driver: &WebDriver,
    agent: &FakeAgent,
    gh: &FakeGh,
    repository: &GitFixture,
) -> Result<(ExecutedTask, EstablishedWork)> {
    let executed = execute_one_task(driver, agent, repository).await?;
    let work = establish_work(driver, repository, &executed).await?;
    let main_before = repository.branch_tip("main")?;

    // The backend itself refuses an unconfirmed close.
    let refusal = ui::invoke_expecting_refusal(
        driver,
        "reorder_task",
        json!({ "taskId": executed.id, "newStatus": "done", "newPosition": 0 }),
    )
    .await?;
    if !refusal.contains("closes it without merging") {
        bail!("an unconfirmed close was refused for another reason: {refusal}");
    }

    // --- First attempt: the card menu, Move to, Done; then Cancel ------------
    move_to_done_from_the_menu(driver, &executed.title).await?;
    assert_consequences_explained(driver, &executed, &work).await?;
    click(driver, CLOSE_CANCEL, "Cancel").await?;
    await_gone(driver, CLOSE_DIALOG, "the confirmation, after Cancel").await?;

    let task = read_task(driver, &executed).await?;
    if status_of(&task) != Some("human_review") || !executed.worktree_path.is_dir() {
        bail!("cancelling changed the task: {task}");
    }
    if repository.branch_tip(&work.branch)?.as_deref() != Some(work.commit.as_str()) {
        bail!("cancelling moved the task branch");
    }
    assert_card_in_column(driver, HUMAN_REVIEW_COLUMN, &executed.title).await?;

    // --- Second attempt: a drop on Done, confirmed over a dirty checkout ---
    //
    // Git refuses to remove a checkout holding uncommitted work, so nothing
    // is closed, and the dialog has to say so rather than report success.
    let stray = executed.worktree_path.join("uncommitted-note.txt");
    std::fs::write(&stray, "not committed\n").context("could not dirty the checkout")?;
    drag_card_to(driver, &executed.title, DONE_COLUMN).await?;
    assert_consequences_explained(driver, &executed, &work).await?;
    click(driver, CLOSE_CONFIRM, "Close without merging").await?;
    let refused = await_text(driver, CLOSE_ERROR, |t| !t.is_empty(), "why the task was not closed").await?;
    if !refused.contains("was not closed") || !refused.contains("modified or untracked files") {
        bail!("a refused close was reported as {refused:?}");
    }
    ui::visible(driver, CLOSE_DIALOG).await.context("a refused close dismissed the dialog")?;
    let task = read_task(driver, &executed).await?;
    if status_of(&task) != Some("human_review") || !stray.is_file() {
        bail!("a refused close changed the task or its checkout: {task}");
    }
    click(driver, CLOSE_CANCEL, "Cancel").await?;
    await_gone(driver, CLOSE_DIALOG, "the confirmation, after Cancel").await?;
    std::fs::remove_file(&stray).context("could not clean the checkout")?;

    // --- Third attempt: the menu again, confirmed over a clean checkout ----
    move_to_done_from_the_menu(driver, &executed.title).await?;
    assert_consequences_explained(driver, &executed, &work).await?;
    click(driver, CLOSE_CONFIRM, "Close without merging").await?;
    await_gone(driver, CLOSE_DIALOG, "the confirmation, after closing").await?;

    let settled = await_status(driver, &executed.project_id, &executed.id, &["done"]).await?;
    await_worktree_removal(repository, &executed.worktree_path).await?;
    if let Some(still) = await_worktree_path_cleared(driver, &executed).await? {
        bail!("the closed task still records its checkout at {still}");
    }
    if settled.get("pr_url").is_some_and(|u| !u.is_null()) || !gh.invocations()?.is_empty() {
        bail!("closing without merging created a pull request");
    }
    if repository.branch_tip(&work.branch)?.as_deref() != Some(work.commit.as_str()) {
        bail!("the branch no longer holds the work the dialog said it would keep");
    }
    if repository.branch_tip("main")? != main_before {
        bail!("closing without merging changed main");
    }
    assert_work_survives(repository, &work, "closing the task without merging")?;
    assert_card_in_column(driver, DONE_COLUMN, &executed.title).await?;
    Ok((executed, work))
}

async fn assert_consequences_explained(
    driver: &WebDriver,
    executed: &ExecutedTask,
    work: &EstablishedWork,
) -> Result<()> {
    ui::visible(driver, CLOSE_DIALOG).await.context("moving to Done did not ask first")?;
    let text = await_text(driver, CLOSE_CONSEQUENCES, |t| !t.is_empty(), "the consequences").await?;
    let worktree = executed.worktree_path.to_string_lossy();
    for expected in [
        "No pull request will be created.",
        "Nothing will be merged into main",
        "The task will be marked Done.",
        &format!("Task Checkout at {worktree} will be removed"),
        &format!("stay on the branch {}", work.branch),
    ] {
        if !text.contains(expected) {
            bail!("the confirmation does not say {expected:?}: {text:?}");
        }
    }
    Ok(())
}

/// Move to, Done, from the card's own menu.
async fn move_to_done_from_the_menu(driver: &WebDriver, title: &str) -> Result<()> {
    open_card_menu(driver, title).await?;
    // The submenu opens on hover; `mouseenter` is what its handler listens to.
    page(
        driver,
        "document.querySelector('[data-testid=\"task-menu-move-to\"]').dispatchEvent(new MouseEvent('mouseenter')); return true;",
        Vec::new(),
    )
    .await?;
    click(driver, "[data-testid=\"task-menu-move-done\"]", "Move to Done").await
}

/// Open a card's menu through its options button.
///
/// Clicked from script: the button only becomes opaque on hover, and
/// WebDriver refuses to click an element it considers invisible. The click
/// still goes through the button's own handler.
async fn open_card_menu(driver: &WebDriver, title: &str) -> Result<()> {
    let opened = page(
        driver,
        r#"
        const [title] = arguments;
        for (const card of document.querySelectorAll('[data-testid="task-card"]')) {
            const t = card.querySelector('[data-testid="task-title"]');
            if (t && t.textContent === title) {
                card.parentElement.querySelector('[data-testid="task-card-menu"]').click();
                return true;
            }
        }
        return false;
        "#,
        vec![Value::String(title.to_string())],
    )
    .await?;
    if opened != Value::Bool(true) {
        bail!("no card titled {title:?} to open a menu on");
    }
    ui::visible(driver, "[data-testid=\"task-menu-move-to\"]").await.context("the card menu did not open")?;
    Ok(())
}

/// Drag a card onto a column with the events a real drag delivers, in the
/// order the board's own handlers expect them.
///
/// Dispatched from script: WebDriver's pointer actions do not start an HTML5
/// drag in WebKitGTK. Every handler that runs is the board's own.
async fn drag_card_to(driver: &WebDriver, title: &str, column: &str) -> Result<()> {
    let dropped = page(
        driver,
        r#"
        const [title, columnSelector] = arguments;
        const column = document.querySelector(columnSelector);
        let card = null;
        for (const c of document.querySelectorAll('[data-testid="task-card"]')) {
            const t = c.querySelector('[data-testid="task-title"]');
            if (t && t.textContent === title) { card = c; }
        }
        if (!card || !column) { return false; }
        const dataTransfer = new DataTransfer();
        const fire = (target, type) => target.dispatchEvent(
            new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer })
        );
        fire(card, 'dragstart');
        fire(column, 'dragover');
        fire(column, 'drop');
        fire(card, 'dragend');
        return true;
        "#,
        vec![Value::String(title.to_string()), Value::String(column.to_string())],
    )
    .await?;
    if dropped != Value::Bool(true) {
        bail!("could not drag {title:?} to {column}");
    }
    Ok(())
}
