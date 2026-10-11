//! Integrating locally completed work into a local branch, from the drawer.
//!
//! Every action is a click on the drawer or on Settings > Repository, the way
//! a person makes it. What each journey holds the product to is read from
//! places that cannot vouch for each other: what the window shows, the
//! product's own `list_tasks`, the task file, and Git itself -- the branch,
//! the index and the files of the checkout that has `main`, the parents of a
//! merge commit, and the bare `origin`, which must never be pushed to.

use super::human_review::{
    attribute, await_task, click, install_fakes, open_reviewed_drawer, read_task, HR_APPROVE, HR_APPROVED, HR_DELIVERY,
    HR_RETRY_PR,
};
use super::task_runs::{branch_name_of, create_task, git_is_ancestor, git_out, register_project, run_to_review};
use super::*;
use slashit_acceptance::fake_gh::FakeGh;

const HR_COMPLETE_LOCALLY: &str = "[data-testid=\"human-review-complete-locally\"]";
const HR_COMPLETE_CONFIRM: &str = "[data-testid=\"human-review-complete-locally-confirm-button\"]";
const HR_APPROVE_LOCAL: &str = "[data-testid=\"human-review-approve-local\"]";
const HR_LOCAL_CHOICE: &str = "[data-testid=\"human-review-local-choice\"]";

const LI_READY: &str = "[data-testid=\"local-integration-ready\"]";
const LI_START: &str = "[data-testid=\"local-integration-start\"]";
const LI_CONFIRM_BOX: &str = "[data-testid=\"local-integration-confirm\"]";
const LI_FACTS: &str = "[data-testid=\"local-integration-facts\"]";
const LI_STRATEGY: &str = "[data-testid=\"local-integration-strategy\"]";
const LI_REMOTE: &str = "[data-testid=\"local-integration-remote\"]";
const LI_CONFIRM: &str = "[data-testid=\"local-integration-confirm-button\"]";
const LI_CANCEL: &str = "[data-testid=\"local-integration-cancel\"]";
const LI_RESULT: &str = "[data-testid=\"local-integration-result\"]";
const LI_PENDING: &str = "[data-testid=\"local-integration-pending\"]";
const LI_PENDING_NOTE: &str = "[data-testid=\"local-integration-pending-note\"]";
const LI_RESUME: &str = "[data-testid=\"local-integration-resume\"]";
const LI_DISCARD: &str = "[data-testid=\"local-integration-discard\"]";
const LI_REFUSED: &str = "[data-testid=\"local-integration-refused\"]";
const LI_REASON: &str = "[data-testid=\"local-integration-reason\"]";
const LI_CONFLICTS: &str = "[data-testid=\"local-integration-conflicts\"]";
const LI_ERROR: &str = "[data-testid=\"local-integration-error\"]";
const LI_ERROR_DISMISS: &str = "[data-testid=\"local-integration-error-dismiss\"]";
const LI_COMPLETION: &str = "[data-testid=\"local-integration-completion\"]";

const SETTINGS_REPOSITORY_TAB: &str = "[data-testid=\"settings-tab-repository\"]";
const REPOSITORY_SOURCE: &str = "[data-testid=\"repository-source\"]";
const REPOSITORY_SOURCE_LOCAL: &str = "[data-testid=\"repository-source-local\"]";
const REPOSITORY_SOURCE_STANDING: &str = "[data-testid=\"repository-source-standing\"]";
const REPOSITORY_BLOCKED: &str = "[data-testid=\"repository-blocked\"]";
const REPOSITORY_READY: &str = "[data-testid=\"repository-ready\"]";

impl GitFixture {
    /// What `branch` is in the bare `origin`, if it is there at all. Nothing
    /// in these journeys may ever change it.
    fn pushed_tip(&self, branch: &str) -> Result<Option<String>> {
        let origin = self.origin.as_deref().context("this fixture was created without an origin")?;
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(origin)
            .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .with_context(|| format!("could not resolve {branch} in origin"))?;
        Ok(output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string()))
    }

    /// Every branch the bare `origin` has, to prove none was added.
    fn pushed_branches(&self) -> Result<String> {
        let origin = self.origin.as_deref().context("this fixture was created without an origin")?;
        git_out(origin, &["for-each-ref", "--format=%(refname) %(objectname)", "refs/heads"])
    }
}

/// The file the fake agent left in `task`'s checkout, named after the
/// checkout directory.
fn own_file_of(task: &ExecutedTask) -> Result<String> {
    let directory = task
        .worktree_path
        .file_name()
        .and_then(OsStr::to_str)
        .with_context(|| format!("the checkout {} has no name", task.worktree_path.display()))?;
    Ok(fake_agent::own_work_file(directory))
}

fn short(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

fn integration_of(task: &Value) -> Option<&Value> {
    task.get("local_integration").filter(|v| !v.is_null())
}

/// Wait until `selector` is shown with `attribute` equal to `expected`.
async fn await_attribute(driver: &WebDriver, selector: &str, attribute_name: &str, expected: &str) -> Result<()> {
    let started = Instant::now();
    let mut last = None;
    loop {
        if count(driver, selector).await? > 0 {
            last = attribute(driver, selector, attribute_name).await?;
            if last.as_deref() == Some(expected) {
                return Ok(());
            }
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("{selector} never had {attribute_name}={expected:?}; it last had {last:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn await_shown(driver: &WebDriver, selector: &str, what: &str) -> Result<()> {
    let started = Instant::now();
    while count(driver, selector).await? == 0 {
        if started.elapsed() > RENDER_DEADLINE {
            bail!("{what} is never shown");
        }
        tokio::time::sleep(POLL).await;
    }
    Ok(())
}

async fn await_hidden(driver: &WebDriver, selector: &str, what: &str) -> Result<()> {
    let started = Instant::now();
    while count(driver, selector).await? != 0 {
        if started.elapsed() > RENDER_DEADLINE {
            bail!("{what} is still shown");
        }
        tokio::time::sleep(POLL).await;
    }
    Ok(())
}

/// Approve in the drawer (`local` chooses the explicit local-delivery
/// approval of a project that could open a pull request), complete locally
/// after reading the consequences, and wait for the task to be Done with its
/// checkout gone.
async fn approve_and_complete_locally(
    driver: &WebDriver,
    repository: &GitFixture,
    task: &ExecutedTask,
    local: bool,
) -> Result<()> {
    open_board(driver, &task.project_id).await?;
    open_reviewed_drawer(driver, task).await?;
    await_text(driver, HR_APPROVE, |t| t != "Checking…", "the approve action").await?;
    if local {
        dismiss_toasts(driver).await?;
        click(driver, HR_APPROVE_LOCAL, "Approve for local delivery").await?;
    } else {
        dismiss_toasts(driver).await?;
        click(driver, HR_APPROVE, "Approve").await?;
    }
    ui::visible(driver, HR_APPROVED).await.context("the drawer does not say Approved")?;
    dismiss_toasts(driver).await?;
    click(driver, HR_COMPLETE_LOCALLY, "Complete locally").await?;
    dismiss_toasts(driver).await?;
    click(driver, HR_COMPLETE_CONFIRM, "the Complete locally confirmation").await?;
    await_status(driver, &task.project_id, &task.id, &["done"]).await?;
    await_worktree_removal(repository, &task.worktree_path).await
}

/// Reload the window and open the drawer of a task already completed locally.
async fn reopen_completed_drawer(driver: &WebDriver, task: &ExecutedTask) -> Result<()> {
    open_board(driver, &task.project_id).await?;
    open_reviewed_drawer(driver, task).await
}

/// The text of every element matching `selector`, joined.
async fn text_of_panel(driver: &WebDriver, selector: &str) -> Result<String> {
    Ok(text_of(driver, selector).await?.unwrap_or_default())
}

/// The state of the repository the fixture is, for comparing before and
/// after: where `main` is, and what the checkout's tracked files look like.
fn tracked_status(repository: &GitFixture) -> Result<String> {
    git_out(&repository.path_buf(), &["status", "--porcelain", "--untracked-files=no"])
}

fn assert_main_checked_out_and_clean(repository: &GitFixture) -> Result<()> {
    let repo = repository.path_buf();
    if git_out(&repo, &["symbolic-ref", "HEAD"])? != "refs/heads/main" {
        bail!("the fixture's primary checkout is not on main");
    }
    let dirty = tracked_status(repository)?;
    if !dirty.is_empty() {
        bail!("the fixture's checkout is not clean: {dirty}");
    }
    Ok(())
}

// --- Journey A ----------------------------------------------------------------

/// A repository whose origin is not GitHub: approved work is completed
/// locally, shown and then integrated into the `main` that is checked out
/// in the repository, which Git fast-forwards with its index and files. The
/// result survives a restart, nothing is pushed, and a task that depends on
/// the integrated one starts with its work.
#[tokio::test(flavor = "multi_thread")]
async fn local_work_is_integrated_into_the_checked_out_main_and_a_dependent_task_receives_it() {
    let context = TestContext::new("local_integration_checked_out").expect("harness setup");
    let outcome = checked_out_journey(&context).await;
    context.finish(outcome);
}

struct Integrated {
    task: ExecutedTask,
    work_file: String,
    tip: String,
}

async fn checked_out_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (_agent, gh) = install_fakes(context, &root)?;
    // Each task leaves a file of its own, so the dependent has commits too.
    context.set_child_env(fake_agent::WRITE_OWN_FILE_VAR, "1");
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("local-integration").await?;
    let outcome = integrate_into_checked_out_main(session.driver(), &repository).await;
    context.close_session(session, "local-integration", &outcome).await?;
    let integrated = outcome?;
    if !gh.invocations()?.is_empty() {
        bail!("integrating locally ran gh: {:?}", gh.invocations()?);
    }

    let session = context.start_session("local-integration-restart").await?;
    let outcome = integration_survives_and_feeds_a_dependent(session.driver(), &repository, &integrated).await;
    context.close_session(session, "local-integration-restart", &outcome).await?;
    outcome?;
    if !gh.invocations()?.is_empty() {
        bail!("starting the dependent task ran gh: {:?}", gh.invocations()?);
    }
    Ok(())
}

async fn integrate_into_checked_out_main(driver: &WebDriver, repository: &GitFixture) -> Result<Integrated> {
    let repo = repository.path_buf();
    let project_id = register_project(driver, repository).await?;
    let (task_id, title) = create_task(driver, &project_id, "Integrate journey", &[]).await?;
    let executed = run_to_review(driver, &project_id, &task_id, &title).await?;
    let branch = branch_name_of(&read_task(driver, &executed).await?)?;
    let work_file = own_file_of(&executed)?;
    let tip = repository.branch_tip(&branch)?.context("the task branch is missing")?;
    let main_before = repository.branch_tip("main")?.context("main is missing")?;
    let origin_before = repository.pushed_branches()?;
    if tip == main_before {
        bail!("the task produced no commit of its own");
    }
    assert_main_checked_out_and_clean(repository)?;

    approve_and_complete_locally(driver, repository, &executed, false).await?;
    if repository.branch_tip("main")?.as_deref() != Some(main_before.as_str()) {
        bail!("completing the task moved main");
    }

    // --- What integrating would do, read before anything is confirmed -------
    await_attribute(driver, LI_READY, "data-state", "ready_in_checkout").await?;
    dismiss_toasts(driver).await?;
    click(driver, LI_START, "Integrate locally").await?;
    ui::visible(driver, LI_CONFIRM_BOX).await.context("Integrate locally did not ask first")?;
    let facts = await_text(driver, LI_FACTS, |t| !t.is_empty(), "the facts to confirm").await?;
    for expected in [
        "Destination branch: main".to_string(),
        format!("Approved commit: {}", short(&tip)),
        format!("Current destination commit: {}", short(&main_before)),
        "Strategy: Fast-forward".to_string(),
        "Checkout affected: ".to_string(),
    ] {
        if !facts.contains(&expected) {
            bail!("the confirmation does not say {expected:?}: {facts:?}");
        }
    }
    if !facts.contains("fixture-repo") {
        bail!("the confirmation does not name the checkout that has main: {facts:?}");
    }
    let strategy = text_of_panel(driver, LI_STRATEGY).await?;
    if !strategy.contains("no history is rewritten") {
        bail!("the strategy is not explained: {strategy:?}");
    }
    let remote = text_of_panel(driver, LI_REMOTE).await?;
    if !remote.contains("Nothing will be pushed") {
        bail!("the confirmation does not say nothing is pushed: {remote:?}");
    }

    // --- Cancel changes nothing ----------------------------------------------
    dismiss_toasts(driver).await?;
    click(driver, LI_CANCEL, "Cancel").await?;
    await_hidden(driver, LI_CONFIRM_BOX, "the confirmation, after Cancel").await?;
    if repository.branch_tip("main")?.as_deref() != Some(main_before.as_str()) || repository.path_buf().join(&work_file).exists() {
        bail!("cancelling changed main or its files");
    }
    if integration_of(&read_task(driver, &executed).await?).is_some() {
        bail!("cancelling recorded an integration");
    }

    // --- Confirm ----------------------------------------------------------------
    dismiss_toasts(driver).await?;
    click(driver, LI_START, "Integrate locally").await?;
    dismiss_toasts(driver).await?;
    click(driver, LI_CONFIRM, "the Integrate locally confirmation").await?;
    let said = await_text(driver, LI_RESULT, |t| t.contains("Integrated into main"), "the integration result").await?;
    if !said.contains("fast-forward") || !said.contains(short(&tip)) || !said.contains("did not push it") {
        bail!("the result says {said:?}");
    }

    // Git: the branch, the index and the files all moved, together.
    if repository.branch_tip("main")?.as_deref() != Some(tip.as_str()) {
        bail!("main is not at the approved commit {tip}");
    }
    assert_main_checked_out_and_clean(repository)?;
    match std::fs::read_to_string(repo.join(&work_file)) {
        Ok(content) if content == fake_agent::WORK_CONTENT => {}
        other => bail!("the checkout that has main holds {other:?} as {work_file}, not the agent's work"),
    }
    if git_out(&repo, &["rev-parse", &format!("refs/heads/{branch}")])? != tip {
        bail!("the Task branch was moved");
    }
    if repository.pushed_branches()? != origin_before {
        bail!("integrating locally pushed something to origin");
    }

    // The product's record, and the timeline.
    let settled = read_task(driver, &executed).await?;
    let record = integration_of(&settled).context("the task records no integration")?;
    if record.get("state").and_then(Value::as_str) != Some("integrated")
        || record.get("strategy").and_then(Value::as_str) != Some("fast_forward")
        || record.get("target_branch").and_then(Value::as_str) != Some("main")
        || record.get("result_oid").and_then(Value::as_str) != Some(tip.as_str())
        || record.get("checkout_path").and_then(Value::as_str).is_none()
    {
        bail!("the integration is recorded as {record}");
    }
    let rows = await_timeline_reaching(driver, "integrated_locally").await?;
    let row = rows.iter().find(|(kind, _, _)| kind == "integrated_locally").context("no integration row")?;
    if row.1 != "Integrated into main" || !row.2.contains("fast forward") || !row.2.contains(short(&tip)) {
        bail!("the timeline shows {row:?}");
    }
    assert_persisted(repository.state_root(), &executed, "done")?;
    Ok(Integrated { task: executed, work_file, tip })
}

async fn integration_survives_and_feeds_a_dependent(
    driver: &WebDriver,
    repository: &GitFixture,
    integrated: &Integrated,
) -> Result<()> {
    let repo = repository.path_buf();
    ui::assert_frontend_is_real(driver).await?;
    reopen_completed_drawer(driver, &integrated.task).await?;
    let said = await_text(driver, LI_RESULT, |t| t.contains("Integrated into main"), "the result after a restart").await?;
    if !said.contains(short(&integrated.tip)) {
        bail!("after a restart the result says {said:?}");
    }
    let rows = await_timeline_reaching(driver, "integrated_locally").await?;
    if !rows.iter().any(|(k, title, _)| k == "integrated_locally" && title == "Integrated into main") {
        bail!("after a restart the timeline shows {rows:?}");
    }
    let origin_before = repository.pushed_branches()?;

    // The person chooses their local branch as where tasks start, so the
    // dependent task starts from the base that holds the approved work.
    ui::invoke(driver, "set_project_base", json!({ "projectId": &integrated.task.project_id, "branch": "main" })).await?;
    open_repository_settings(driver, &integrated.task.project_id).await?;
    dismiss_toasts(driver).await?;
    click(driver, REPOSITORY_SOURCE_LOCAL, "the local branch choice").await?;
    await_attribute(driver, REPOSITORY_SOURCE, "data-prefers-local", "true").await?;

    // A task that depends on the integrated one starts with its work.
    let (child_id, child_title) =
        create_task(driver, &integrated.task.project_id, "Dependent journey", &[integrated.task.id.as_str()]).await?;
    let child = run_to_review(driver, &integrated.task.project_id, &child_id, &child_title).await?;
    let child_task = read_task(driver, &child).await?;
    if child_task.pointer("/branch_origin/kind").and_then(Value::as_str) != Some("local_base")
        || child_task.get("base_commit").and_then(Value::as_str) != Some(integrated.tip.as_str())
    {
        bail!("the dependent task did not start from local main at {}", integrated.tip);
    }
    let child_branch = branch_name_of(&child_task)?;
    let child_tip = repository.branch_tip(&child_branch)?.context("the dependent's branch is missing")?;
    if !git_is_ancestor(&repo, &integrated.tip, &child_tip)? {
        bail!("the dependent task's branch does not contain the integrated work {}", integrated.tip);
    }
    if repository.file_at(&child_branch, &integrated.work_file)?.as_deref() != Some(fake_agent::WORK_CONTENT) {
        bail!("the dependent task's branch lacks {}", integrated.work_file);
    }
    if !child.worktree_path.join(&integrated.work_file).is_file() {
        bail!("the dependent task's checkout lacks {}", integrated.work_file);
    }
    if repository.branch_tip("main")?.as_deref() != Some(integrated.tip.as_str()) {
        bail!("starting the dependent task moved main");
    }
    if repository.pushed_branches()? != origin_before {
        bail!("starting the dependent task pushed something to origin");
    }
    Ok(())
}

// --- Journey B ----------------------------------------------------------------

/// A repository whose origin is a GitHub one: Create PR stays the default,
/// approving for local delivery opens nothing, a dirty checkout is refused
/// without touching a byte of it, a diverged `main` gets a merge commit, and
/// choosing the local branch in Settings > Repository is kept across a
/// restart and is where the next task starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_project_with_a_github_remote_delivers_locally_by_choice_and_starts_new_tasks_from_its_local_branch() {
    let context = TestContext::new("local_integration_github_remote").expect("harness setup");
    let outcome = github_remote_journey(&context).await;
    context.finish(outcome);
}

struct Chosen {
    project_id: String,
    main_tip: String,
    origin_main: String,
    inherited_files: Vec<String>,
}

async fn github_remote_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (_agent, gh) = install_fakes(context, &root)?;
    context.set_child_env(fake_agent::WRITE_OWN_FILE_VAR, "1");
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    repository.point_origin_at_github()?;

    let session = context.start_session("local-github").await?;
    let outcome = deliver_locally_in_a_github_project(session.driver(), &repository, &gh).await;
    context.close_session(session, "local-github", &outcome).await?;
    let chosen = outcome?;

    let session = context.start_session("local-github-restart").await?;
    let outcome = choice_survives_and_tasks_start_locally(session.driver(), &repository, &gh, &chosen).await;
    context.close_session(session, "local-github-restart", &outcome).await?;
    outcome
}

async fn deliver_locally_in_a_github_project(
    driver: &WebDriver,
    repository: &GitFixture,
    gh: &FakeGh,
) -> Result<Chosen> {
    let repo = repository.path_buf();
    let project_id = register_project(driver, repository).await?;
    let (task_id, title) = create_task(driver, &project_id, "Github local journey", &[]).await?;
    let executed = run_to_review(driver, &project_id, &task_id, &title).await?;
    let branch = branch_name_of(&read_task(driver, &executed).await?)?;
    let work_file = own_file_of(&executed)?;
    let tip = repository.branch_tip(&branch)?.context("the task branch is missing")?;
    let origin_main = repository.pushed_tip("main")?.context("origin has no main")?;
    let origin_before = repository.pushed_branches()?;

    // --- The default is unchanged; the local approval is a named second choice
    open_board(driver, &project_id).await?;
    open_reviewed_drawer(driver, &executed).await?;
    let label = await_text(driver, HR_APPROVE, |t| t != "Checking…", "the approve action").await?;
    if label != "Approve & Create PR" {
        bail!("the default approval of a GitHub project reads {label:?}");
    }
    let local = await_text(driver, HR_APPROVE_LOCAL, |t| !t.is_empty(), "the local approval").await?;
    if local != "Approve for local delivery" {
        bail!("the local approval reads {local:?}");
    }
    dismiss_toasts(driver).await?;
    click(driver, HR_APPROVE_LOCAL, "Approve for local delivery").await?;
    ui::visible(driver, HR_APPROVED).await.context("the drawer does not say Approved")?;
    if gh.create_attempts()? != 0 {
        bail!("approving for local delivery created a pull request");
    }
    await_attribute(driver, HR_DELIVERY, "data-delivery", "not_attempted").await?;
    ui::visible(driver, HR_LOCAL_CHOICE).await.context("the drawer does not offer to keep the work local")?;
    ui::visible(driver, HR_RETRY_PR).await.context("Create PR is no longer offered")?;
    // Local delivery in one step is offered beside it as a named choice,
    // never instead of it, and asking to see it opens nothing.
    let one_step = await_text(driver, LI_START, |t| !t.is_empty(), "the Complete and Integrate action").await?;
    if one_step != "Complete and Integrate" {
        bail!("the one-step local delivery reads {one_step:?}");
    }
    if gh.create_attempts()? != 0 {
        bail!("offering Complete and Integrate created a pull request");
    }

    dismiss_toasts(driver).await?;
    click(driver, HR_COMPLETE_LOCALLY, "Complete locally").await?;
    dismiss_toasts(driver).await?;
    click(driver, HR_COMPLETE_CONFIRM, "the Complete locally confirmation").await?;
    let done = await_status(driver, &project_id, &executed.id, &["done"]).await?;
    await_worktree_removal(repository, &executed.worktree_path).await?;
    if done.get("pr_url").is_some_and(|u| !u.is_null()) || gh.create_attempts()? != 0 || gh.pr_for(&branch)?.is_some() {
        bail!("completing locally created a pull request");
    }
    if repository.pushed_branches()? != origin_before {
        bail!("completing locally pushed something to origin");
    }

    // --- A checkout with uncommitted work is refused, byte for byte -------------
    assert_main_checked_out_and_clean(repository)?;
    let readme = repo.join("README.md");
    let original = std::fs::read(&readme)?;
    let mine = b"my own uncommitted words\n".to_vec();
    std::fs::write(&readme, &mine)?;
    let main_before = repository.branch_tip("main")?.context("main is missing")?;
    reopen_completed_drawer(driver, &executed).await?;
    await_shown(driver, LI_REFUSED, "the refusal of a dirty checkout").await?;
    let reason = await_text(driver, LI_REASON, |t| !t.is_empty(), "why it was refused").await?;
    if !reason.contains("uncommitted changes") || !reason.contains("README.md") {
        bail!("the refusal says {reason:?}");
    }
    if count(driver, LI_START).await? != 0 {
        bail!("a dirty checkout is offered the integration");
    }
    if std::fs::read(&readme)? != mine || repository.branch_tip("main")?.as_deref() != Some(main_before.as_str()) {
        bail!("refusing changed the checkout or main");
    }
    if integration_of(&read_task(driver, &executed).await?).is_some() {
        bail!("a refused integration was recorded");
    }
    std::fs::write(&readme, &original)?;

    // --- Diverged main gets a merge commit ---------------------------------------
    std::fs::write(repo.join("elsewhere.txt"), "committed on main meanwhile\n")?;
    git_out(&repo, &["add", "elsewhere.txt"])?;
    git_out(&repo, &["commit", "--quiet", "-m", "work on main"])?;
    let diverged_main = repository.branch_tip("main")?.context("main is missing")?;
    reopen_completed_drawer(driver, &executed).await?;
    await_attribute(driver, LI_READY, "data-state", "ready_in_checkout").await?;
    dismiss_toasts(driver).await?;
    click(driver, LI_START, "Integrate locally").await?;
    let facts = await_text(driver, LI_FACTS, |t| !t.is_empty(), "the facts to confirm").await?;
    if !facts.contains("Strategy: Merge commit") || !facts.contains(&format!("Current destination commit: {}", short(&diverged_main))) {
        bail!("the confirmation of a diverged main says {facts:?}");
    }
    let remote = text_of_panel(driver, LI_REMOTE).await?;
    if !remote.contains("remote named origin") || !remote.contains("Nothing will be pushed") {
        bail!("the confirmation does not say a remote exists and nothing is pushed: {remote:?}");
    }
    dismiss_toasts(driver).await?;
    click(driver, LI_CONFIRM, "the Integrate locally confirmation").await?;
    let said = await_text(driver, LI_RESULT, |t| t.contains("Integrated into main"), "the merge result").await?;
    if !said.contains("merge commit") {
        bail!("the result says {said:?}");
    }
    let merged = repository.branch_tip("main")?.context("main is missing")?;
    let parents = git_out(&repo, &["rev-list", "--parents", "-n1", &merged])?;
    let parents: Vec<&str> = parents.split_whitespace().skip(1).collect();
    if parents != [diverged_main.as_str(), tip.as_str()] {
        bail!("the merge commit has parents {parents:?}, expected main then the approved commit");
    }
    assert_main_checked_out_and_clean(repository)?;
    if !repo.join(&work_file).is_file() || !repo.join("elsewhere.txt").is_file() {
        bail!("the checkout does not hold both lines of work after the merge");
    }
    let rows = await_timeline_reaching(driver, "integrated_locally").await?;
    if !rows.iter().any(|(k, t, d)| k == "integrated_locally" && t == "Integrated into main" && d.contains("merge commit")) {
        bail!("the timeline shows {rows:?}");
    }
    if repository.pushed_branches()? != origin_before || repository.pushed_tip("main")?.as_deref() != Some(origin_main.as_str()) {
        bail!("integrating pushed something to origin");
    }
    if gh.create_attempts()? != 0 || !gh.invocations()?.iter().all(|args| !args.iter().any(|a| a == "create")) {
        bail!("integrating opened a pull request");
    }

    // --- Settings > Repository: the local branch is a choice a person makes -------
    // The choice is between origin's default branch and the project's local
    // base branch, so the project has to have one.
    let before = ui::invoke(driver, "get_project_readiness", json!({ "projectId": project_id })).await?;
    if before.get("project_base").is_none_or(Value::is_null) {
        ui::invoke(driver, "set_project_base", json!({ "projectId": project_id, "branch": "main" })).await?;
    }
    open_repository_settings(driver, &project_id).await?;
    let blocked = await_text(driver, REPOSITORY_BLOCKED, |t| !t.is_empty(), "why tasks cannot start yet").await?;
    if !blocked.contains("integrated") {
        bail!("Settings does not explain the local work origin lacks: {blocked:?}");
    }
    await_attribute(driver, REPOSITORY_SOURCE, "data-prefers-local", "false").await?;
    let standing = await_text(driver, REPOSITORY_SOURCE_STANDING, |t| !t.is_empty(), "where main stands against origin").await?;
    if !standing.contains("ahead of origin/main") {
        bail!("Settings says {standing:?}");
    }
    click(driver, REPOSITORY_SOURCE_LOCAL, "the local branch choice").await?;
    await_attribute(driver, REPOSITORY_SOURCE, "data-prefers-local", "true").await?;
    let ready = await_text(driver, REPOSITORY_READY, |t| t.contains("as you chose"), "tasks ready to start locally").await?;
    if !ready.contains(short(&merged)) {
        bail!("Settings says tasks start from {ready:?}, not from main at {merged}");
    }
    let readiness = ui::invoke(driver, "get_project_readiness", json!({ "projectId": project_id })).await?;
    if readiness.get("prefers_local").and_then(Value::as_bool) != Some(true)
        || readiness.pointer("/base/commit").and_then(Value::as_str) != Some(merged.as_str())
        || readiness.pointer("/base/source").and_then(Value::as_str) != Some("local")
        || readiness.pointer("/standing/comparison/state").and_then(Value::as_str) != Some("local_ahead")
    {
        bail!("the readiness after choosing is {readiness}");
    }
    if repository.pushed_branches()? != origin_before {
        bail!("choosing the local branch pushed something to origin");
    }
    Ok(Chosen { project_id, main_tip: merged, origin_main, inherited_files: vec![work_file, "elsewhere.txt".to_string()] })
}

async fn choice_survives_and_tasks_start_locally(
    driver: &WebDriver,
    repository: &GitFixture,
    gh: &FakeGh,
    chosen: &Chosen,
) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let origin_before = repository.pushed_branches()?;
    open_repository_settings(driver, &chosen.project_id).await?;
    await_attribute(driver, REPOSITORY_SOURCE, "data-prefers-local", "true").await?;
    await_text(driver, REPOSITORY_READY, |t| t.contains("as you chose"), "the choice after a restart").await?;

    // A new task, with no dependency, starts from the local main exactly.
    let (task_id, title) = create_task(driver, &chosen.project_id, "Starts locally", &[]).await?;
    let executed = run_to_review(driver, &chosen.project_id, &task_id, &title).await?;
    let task = read_task(driver, &executed).await?;
    let branch = branch_name_of(&task)?;
    if task.pointer("/branch_origin/kind").and_then(Value::as_str) != Some("local_base")
        || task.get("base_commit").and_then(Value::as_str) != Some(chosen.main_tip.as_str())
    {
        bail!(
            "the task records origin {:?} and fork point {:?}, expected the local base at {}",
            task.get("branch_origin"),
            task.get("base_commit"),
            chosen.main_tip
        );
    }
    if chosen.main_tip == chosen.origin_main {
        bail!("this journey proves nothing: main and origin/main are the same commit");
    }
    let repo = repository.path_buf();
    let tip = repository.branch_tip(&branch)?.context("the task branch is missing")?;
    if !git_is_ancestor(&repo, &chosen.main_tip, &tip)? {
        bail!("the task's branch does not contain the local main {}", chosen.main_tip);
    }
    for file in &chosen.inherited_files {
        if !executed.worktree_path.join(file).is_file() {
            bail!("the task's checkout lacks {file}, which is on the local main");
        }
    }
    if repository.pushed_branches()? != origin_before || repository.pushed_tip("main")?.as_deref() != Some(chosen.origin_main.as_str()) {
        bail!("starting the task pushed something to origin");
    }
    if gh.create_attempts()? != 0 {
        bail!("starting the task opened a pull request");
    }
    Ok(())
}

async fn open_repository_settings(driver: &WebDriver, project_id: &str) -> Result<()> {
    page(
        driver,
        "localStorage.setItem('slashit_current_page', 'settings'); \
         localStorage.setItem('slashit_selected_project', arguments[0]); return true;",
        vec![json!(project_id)],
    )
    .await?;
    driver.refresh().await?;
    ui::assert_frontend_is_real(driver).await?;
    click(driver, SETTINGS_REPOSITORY_TAB, "the Repository settings").await
}

// --- Journey C ----------------------------------------------------------------

/// Conflicts and unfinished integrations. A change that conflicts with `main`
/// is refused naming the path, and nothing changes. An integration a crash
/// left pending can be discarded, which moves nothing, or resumed, which
/// finishes it even when Git had already updated the checkout's files and
/// stopped before moving the branch.
#[tokio::test(flavor = "multi_thread")]
async fn a_conflict_is_refused_and_an_unfinished_integration_is_resumed_or_discarded() {
    let context = TestContext::new("local_integration_recovery").expect("harness setup");
    let outcome = recovery_journey(&context).await;
    context.finish(outcome);
}

struct Completed {
    conflicting: ExecutedTask,
    resumable: ExecutedTask,
    discardable: ExecutedTask,
}

async fn recovery_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (_agent, gh) = install_fakes(context, &root)?;
    context.set_child_env(fake_agent::WRITE_OWN_FILE_VAR, "1");
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("local-recovery").await?;
    let outcome = complete_three_tasks_locally(session.driver(), &repository).await;
    context.close_session(session, "local-recovery", &outcome).await?;
    let completed = outcome?;

    // What a crash leaves: two integrations recorded as pending, with the
    // branch not yet moved. The application is not running while the task
    // file is written, exactly as it is not after a crash.
    let main_tip = repository.branch_tip("main")?.context("main is missing")?;
    let checkout = git_out(&repository.path_buf(), &["rev-parse", "--show-toplevel"])?;
    let resumable_tip = approved_tip_in_file(&root, &completed.resumable.id)?;
    let discardable_tip = approved_tip_in_file(&root, &completed.discardable.id)?;
    for (task, tip) in [(&completed.resumable, &resumable_tip), (&completed.discardable, &discardable_tip)] {
        seed_pending_integration(&root, &task.id, &main_tip, tip, &checkout)?;
    }

    let session = context.start_session("local-recovery-restart").await?;
    let outcome = refuse_resume_and_discard(session.driver(), &repository, &completed, &main_tip, &resumable_tip).await;
    context.close_session(session, "local-recovery-restart", &outcome).await?;
    outcome?;
    if !gh.invocations()?.is_empty() {
        bail!("recovering integrations ran gh: {:?}", gh.invocations()?);
    }
    Ok(())
}

async fn complete_three_tasks_locally(driver: &WebDriver, repository: &GitFixture) -> Result<Completed> {
    let project_id = register_project(driver, repository).await?;
    let mut done = Vec::new();
    for prefix in ["Conflicting", "Resumable", "Discardable"] {
        let (id, title) = create_task(driver, &project_id, prefix, &[]).await?;
        let executed = run_to_review(driver, &project_id, &id, &title).await?;
        approve_and_complete_locally(driver, repository, &executed, false).await?;
        done.push(executed);
    }
    let discardable = done.pop().context("three tasks")?;
    let resumable = done.pop().context("three tasks")?;
    let conflicting = done.pop().context("three tasks")?;
    Ok(Completed { conflicting, resumable, discardable })
}

/// The commit the persisted approval of `task_id` was made on.
fn approved_tip_in_file(root: &Path, task_id: &str) -> Result<String> {
    for file in toml_files(root)? {
        let Ok(contents) = std::fs::read_to_string(&file) else { continue };
        let Ok(document) = contents.parse::<toml::Value>() else { continue };
        let Some(record) = task_record(&document, task_id) else { continue };
        let entries = record.get("human_review").and_then(|r| r.get("entries")).and_then(toml::Value::as_array);
        if let Some(tip) = entries
            .into_iter()
            .flatten()
            .rev()
            .find_map(|e| e.get("approved_tip").and_then(toml::Value::as_str))
        {
            return Ok(tip.to_string());
        }
    }
    bail!("no task file records the approval of {task_id} with a commit")
}

/// Write the record a crash between "recorded" and "moved" leaves: a fast
/// forward from `main_tip` to `tip` that is pending, confirmed for `checkout`.
fn seed_pending_integration(root: &Path, task_id: &str, main_tip: &str, tip: &str, checkout: &str) -> Result<()> {
    let mut patched = 0;
    for file in toml_files(root)? {
        let Ok(contents) = std::fs::read_to_string(&file) else { continue };
        let Ok(mut document) = contents.parse::<toml::Value>() else { continue };
        let Some(tasks) = document.get_mut("tasks").and_then(toml::Value::as_array_mut) else { continue };
        let Some(record) = tasks
            .iter_mut()
            .filter_map(toml::Value::as_table_mut)
            .find(|r| r.get("id").and_then(toml::Value::as_str) == Some(task_id))
        else {
            continue;
        };
        let mut integration = toml::value::Table::new();
        for (key, value) in [
            ("state", "pending"),
            ("target_branch", "main"),
            ("target_old_oid", main_tip),
            ("approved_tip", tip),
            ("result_oid", tip),
            ("strategy", "fast_forward"),
            ("checkout_path", checkout),
            ("started_at", "2026-01-01T00:00:00Z"),
        ] {
            integration.insert(key.to_string(), toml::Value::String(value.to_string()));
        }
        record.insert("local_integration".to_string(), toml::Value::Table(integration));
        std::fs::write(&file, toml::to_string(&document).context("could not write the task file")?)
            .with_context(|| format!("could not write {}", file.display()))?;
        patched += 1;
    }
    if patched == 0 {
        bail!("no task file holds task {task_id}");
    }
    Ok(())
}

async fn refuse_resume_and_discard(
    driver: &WebDriver,
    repository: &GitFixture,
    completed: &Completed,
    main_tip: &str,
    resumable_tip: &str,
) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let repo = repository.path_buf();
    let origin_before = repository.pushed_branches()?;

    // --- Discard: the branch has not moved, so the plan is simply dropped --------
    reopen_completed_drawer(driver, &completed.discardable).await?;
    await_attribute(driver, LI_PENDING, "data-observed", "unmoved").await?;
    let note = await_text(driver, LI_PENDING_NOTE, |t| !t.is_empty(), "what the pending integration needs").await?;
    if !note.contains("has not moved") {
        bail!("the pending integration says {note:?}");
    }
    dismiss_toasts(driver).await?;
    click(driver, LI_DISCARD, "Discard").await?;
    await_task(driver, &completed.discardable, "no pending integration", |t| integration_of(t).is_none()).await?;
    if repository.branch_tip("main")?.as_deref() != Some(main_tip) {
        bail!("discarding moved main");
    }
    assert_main_checked_out_and_clean(repository)?;

    // --- Resume, after Git had updated the checkout and stopped -------------------
    // What `git merge --ff-only` leaves if it dies between updating the files
    // and moving the branch: the index and the files hold the work, and the
    // branch still names the old commit.
    git_out(&repo, &["read-tree", "-m", "-u", main_tip, resumable_tip])?;
    if repository.branch_tip("main")?.as_deref() != Some(main_tip) {
        bail!("the crash simulation moved the branch");
    }
    reopen_completed_drawer(driver, &completed.resumable).await?;
    await_attribute(driver, LI_PENDING, "data-observed", "unmoved").await?;
    if count(driver, "[data-testid=\"local-integration-checkout-problems\"]").await? != 0 {
        bail!("a checkout Git had already updated is reported as blocked");
    }
    dismiss_toasts(driver).await?;
    click(driver, LI_RESUME, "Resume").await?;
    let said = await_text(driver, LI_RESULT, |t| t.contains("Integrated into main"), "the resumed integration").await?;
    if !said.contains(short(resumable_tip)) {
        bail!("the resumed integration says {said:?}");
    }
    if repository.branch_tip("main")?.as_deref() != Some(resumable_tip) {
        bail!("resuming did not move main to {resumable_tip}");
    }
    assert_main_checked_out_and_clean(repository)?;
    let work_file = own_file_of(&completed.resumable)?;
    if std::fs::read_to_string(repo.join(&work_file))? != fake_agent::WORK_CONTENT {
        bail!("the checkout does not hold {work_file} after resuming");
    }

    // --- A conflict is refused, naming the path, and changes nothing ----------------
    let conflicting_file = own_file_of(&completed.conflicting)?;
    std::fs::write(repo.join(&conflicting_file), "main's own, different, version\n")?;
    git_out(&repo, &["add", &conflicting_file])?;
    git_out(&repo, &["commit", "--quiet", "-m", "work on main that conflicts"])?;
    let main_now = repository.branch_tip("main")?.context("main is missing")?;
    let main_files = std::fs::read(repo.join(&conflicting_file))?;
    reopen_completed_drawer(driver, &completed.conflicting).await?;
    await_shown(driver, LI_REFUSED, "the refusal of a conflict").await?;
    let listed = await_text(driver, LI_CONFLICTS, |t| !t.is_empty(), "the conflicting paths").await?;
    if !listed.contains(&conflicting_file) {
        bail!("the conflicts list {listed:?}, not {conflicting_file}");
    }
    let reason = text_of_panel(driver, LI_REASON).await?;
    if !reason.contains("conflict") || !reason.contains("Nothing was changed") {
        bail!("the refusal says {reason:?}");
    }
    if count(driver, LI_START).await? != 0 {
        bail!("a conflicting change is offered the integration");
    }
    if repository.branch_tip("main")?.as_deref() != Some(main_now.as_str())
        || std::fs::read(repo.join(&conflicting_file))? != main_files
        || !tracked_status(repository)?.is_empty()
    {
        bail!("refusing a conflict changed main or its checkout");
    }
    let unchanged = read_task(driver, &completed.conflicting).await?;
    if integration_of(&unchanged).is_some() {
        bail!("a refused conflict was recorded");
    }
    if repository.pushed_branches()? != origin_before {
        bail!("recovering integrations pushed something to origin");
    }
    Ok(())
}

// --- Journey D ----------------------------------------------------------------

/// Complete and Integrate, from Human Review: one action integrates the
/// approved commit into the `main` that is checked out and completes the
/// task. Cancelling changes nothing; nothing is pushed and no pull request
/// is opened; the Task Checkout goes and the Task branch stays; the outcome
/// survives a restart; and a task that depends on this one starts from the
/// local base that holds the approved work.
#[tokio::test(flavor = "multi_thread")]
async fn approved_work_is_completed_and_integrated_in_one_action_and_a_dependent_task_starts_from_it() {
    let context = TestContext::new("complete_and_integrate").expect("harness setup");
    let outcome = complete_and_integrate_journey(&context).await;
    context.finish(outcome);
}

async fn complete_and_integrate_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (_agent, gh) = install_fakes(context, &root)?;
    context.set_child_env(fake_agent::WRITE_OWN_FILE_VAR, "1");
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("complete-and-integrate").await?;
    let outcome = complete_and_integrate_from_review(session.driver(), &repository).await;
    context.close_session(session, "complete-and-integrate", &outcome).await?;
    let integrated = outcome?;
    if !gh.invocations()?.is_empty() {
        bail!("completing and integrating ran gh: {:?}", gh.invocations()?);
    }

    let session = context.start_session("complete-and-integrate-restart").await?;
    let outcome = integration_survives_and_feeds_a_dependent(session.driver(), &repository, &integrated).await;
    context.close_session(session, "complete-and-integrate-restart", &outcome).await?;
    outcome?;
    if !gh.invocations()?.is_empty() {
        bail!("starting the dependent task ran gh: {:?}", gh.invocations()?);
    }
    Ok(())
}

async fn complete_and_integrate_from_review(driver: &WebDriver, repository: &GitFixture) -> Result<Integrated> {
    let repo = repository.path_buf();
    let project_id = register_project(driver, repository).await?;
    let (task_id, title) = create_task(driver, &project_id, "Complete and integrate journey", &[]).await?;
    let executed = run_to_review(driver, &project_id, &task_id, &title).await?;
    let branch = branch_name_of(&read_task(driver, &executed).await?)?;
    let work_file = own_file_of(&executed)?;
    let tip = repository.branch_tip(&branch)?.context("the task branch is missing")?;
    let main_before = repository.branch_tip("main")?.context("main is missing")?;
    let origin_before = repository.pushed_branches()?;
    if tip == main_before {
        bail!("the task produced no commit of its own");
    }
    assert_main_checked_out_and_clean(repository)?;

    // --- Approve; both local alternatives are offered side by side ------------
    open_board(driver, &project_id).await?;
    open_reviewed_drawer(driver, &executed).await?;
    await_text(driver, HR_APPROVE, |t| t != "Checking…", "the approve action").await?;
    dismiss_toasts(driver).await?;
    click(driver, HR_APPROVE, "Approve").await?;
    ui::visible(driver, HR_APPROVED).await.context("the drawer does not say Approved")?;
    ui::visible(driver, HR_COMPLETE_LOCALLY).await.context("Complete locally is no longer offered")?;
    let label = await_text(driver, LI_START, |t| !t.is_empty(), "the Complete and Integrate action").await?;
    if label != "Complete and Integrate" {
        bail!("the one-step action reads {label:?}");
    }
    await_attribute(driver, LI_READY, "data-state", "ready_in_checkout").await?;

    // --- What it would do, read before anything is confirmed ------------------
    dismiss_toasts(driver).await?;
    click(driver, LI_START, "Complete and Integrate").await?;
    ui::visible(driver, LI_CONFIRM_BOX).await.context("Complete and Integrate did not ask first")?;
    let facts = await_text(driver, LI_FACTS, |t| !t.is_empty(), "the facts to confirm").await?;
    for expected in [
        "Destination branch: main".to_string(),
        format!("Approved commit: {}", short(&tip)),
        format!("Current destination commit: {}", short(&main_before)),
        "Strategy: Fast-forward".to_string(),
        "Checkout affected: ".to_string(),
    ] {
        if !facts.contains(&expected) {
            bail!("the confirmation does not say {expected:?}: {facts:?}");
        }
    }
    let completion = text_of_panel(driver, LI_COMPLETION).await?;
    for expected in ["marked Done", "No pull request is created.", "Task Checkout at", "is removed", &branch, "are kept"] {
        if !completion.contains(expected) {
            bail!("the confirmation does not say {expected:?}: {completion:?}");
        }
    }
    let remote = text_of_panel(driver, LI_REMOTE).await?;
    if !remote.contains("Nothing will be pushed") {
        bail!("the confirmation does not say nothing is pushed: {remote:?}");
    }

    // --- Cancel changes nothing ------------------------------------------------
    dismiss_toasts(driver).await?;
    click(driver, LI_CANCEL, "Cancel").await?;
    await_hidden(driver, LI_CONFIRM_BOX, "the confirmation, after Cancel").await?;
    let still = read_task(driver, &executed).await?;
    if still.get("status").and_then(Value::as_str) != Some("human_review")
        || integration_of(&still).is_some()
        || !executed.worktree_path.exists()
        || repository.branch_tip("main")?.as_deref() != Some(main_before.as_str())
    {
        bail!("cancelling changed something: {still}");
    }

    // --- Confirm: one action ---------------------------------------------------
    dismiss_toasts(driver).await?;
    click(driver, LI_START, "Complete and Integrate").await?;
    dismiss_toasts(driver).await?;
    click(driver, LI_CONFIRM, "the Complete and Integrate confirmation").await?;
    let done = await_status(driver, &project_id, &executed.id, &["done"]).await?;
    await_worktree_removal(repository, &executed.worktree_path).await?;
    let said = await_text(driver, LI_RESULT, |t| t.contains("Integrated into main"), "the integration result").await?;
    if !said.contains("fast-forward") || !said.contains(short(&tip)) || !said.contains("did not push it") {
        bail!("the result says {said:?}");
    }
    await_attribute(driver, HR_DELIVERY, "data-delivery", "completed_locally").await?;

    // Git: the branch, the index and the files moved together; the Task
    // branch is untouched; nothing reached origin.
    if repository.branch_tip("main")?.as_deref() != Some(tip.as_str()) {
        bail!("main is not at the approved commit {tip}");
    }
    assert_main_checked_out_and_clean(repository)?;
    match std::fs::read_to_string(repo.join(&work_file)) {
        Ok(content) if content == fake_agent::WORK_CONTENT => {}
        other => bail!("the checkout that has main holds {other:?} as {work_file}, not the agent's work"),
    }
    if git_out(&repo, &["rev-parse", &format!("refs/heads/{branch}")])? != tip {
        bail!("the Task branch was moved");
    }
    if repository.pushed_branches()? != origin_before {
        bail!("completing and integrating pushed something to origin");
    }

    // The product's record: Done, complete, 100%, the approval kept, the
    // integration naming the approved commit and its result.
    let settled = read_task(driver, &executed).await?;
    if done.get("status").and_then(Value::as_str) != Some("done")
        || settled.get("phase").and_then(Value::as_str) != Some("complete")
        || settled.get("overall_progress").and_then(Value::as_u64) != Some(100)
        || settled.get("worktree_path").is_some_and(|v| !v.is_null())
        || settled.get("pr_url").is_some_and(|v| !v.is_null())
    {
        bail!("the completed task reads {settled}");
    }
    let approvals = settled
        .pointer("/human_review/entries")
        .and_then(Value::as_array)
        .context("the task has no review history")?;
    if approvals.len() != 1
        || approvals[0].get("decision").and_then(Value::as_str) != Some("approved")
        || approvals[0].get("approved_tip").and_then(Value::as_str) != Some(tip.as_str())
    {
        bail!("the approval history is {approvals:?}");
    }
    let record = integration_of(&settled).context("the task records no integration")?;
    if record.get("state").and_then(Value::as_str) != Some("integrated")
        || record.get("strategy").and_then(Value::as_str) != Some("fast_forward")
        || record.get("target_branch").and_then(Value::as_str) != Some("main")
        || record.get("approved_tip").and_then(Value::as_str) != Some(tip.as_str())
        || record.get("result_oid").and_then(Value::as_str) != Some(tip.as_str())
        || record.get("completes_task").and_then(Value::as_bool) != Some(true)
    {
        bail!("the integration is recorded as {record}");
    }

    // The timeline records each once, integration first.
    let rows = await_timeline_reaching(driver, "completed_locally").await?;
    let kinds: Vec<&str> = rows
        .iter()
        .map(|(kind, _, _)| kind.as_str())
        .filter(|kind| matches!(*kind, "integrated_locally" | "completed_locally"))
        .collect();
    if kinds != ["integrated_locally", "completed_locally"] {
        bail!("the timeline shows {kinds:?} in {rows:?}");
    }
    assert_persisted(repository.state_root(), &executed, "done")?;
    Ok(Integrated { task: executed, work_file, tip })
}

// --- Journey: a destination that moves after the confirmation opened --------------

/// A person opens the confirmation for a fast-forward, and `main` moves on
/// while it is open. Confirming is refused by Git's own check and changes
/// nothing; the box closes instead of turning into a confirmation of the new
/// plan; the reason stays until it is dismissed; and the new plan (a merge
/// commit onto the moved `main`) is integrated only after it is opened and
/// confirmed again.
#[tokio::test(flavor = "multi_thread")]
async fn a_destination_that_moves_while_the_confirmation_is_open_is_refused_and_needs_a_new_confirmation() {
    let context = TestContext::new("stale_integration_confirmation").expect("harness setup");
    let outcome = stale_confirmation_journey(&context).await;
    context.finish(outcome);
}

async fn stale_confirmation_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (_agent, gh) = install_fakes(context, &root)?;
    context.set_child_env(fake_agent::WRITE_OWN_FILE_VAR, "1");
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("stale-integration-confirmation").await?;
    let outcome = stale_confirmation(session.driver(), &repository).await;
    context.close_session(session, "stale-integration-confirmation", &outcome).await?;
    outcome?;
    if !gh.invocations()?.is_empty() {
        bail!("integrating ran gh: {:?}", gh.invocations()?);
    }
    Ok(())
}

async fn stale_confirmation(driver: &WebDriver, repository: &GitFixture) -> Result<()> {
    let repo = repository.path_buf();
    let project_id = register_project(driver, repository).await?;
    let (task_id, title) = create_task(driver, &project_id, "Stale confirmation journey", &[]).await?;
    let executed = run_to_review(driver, &project_id, &task_id, &title).await?;
    let branch = branch_name_of(&read_task(driver, &executed).await?)?;
    let tip = repository.branch_tip(&branch)?.context("the task branch is missing")?;
    let main_a = repository.branch_tip("main")?.context("main is missing")?;
    let origin_before = repository.pushed_branches()?;
    assert_main_checked_out_and_clean(repository)?;

    open_board(driver, &project_id).await?;
    open_reviewed_drawer(driver, &executed).await?;
    await_text(driver, HR_APPROVE, |t| t != "Checking…", "the approve action").await?;
    dismiss_toasts(driver).await?;
    click(driver, HR_APPROVE, "Approve").await?;
    ui::visible(driver, HR_APPROVED).await.context("the drawer does not say Approved")?;
    await_attribute(driver, LI_READY, "data-state", "ready_in_checkout").await?;

    // --- The confirmation is opened for a fast-forward onto A -----------------
    dismiss_toasts(driver).await?;
    click(driver, LI_START, "Complete and Integrate").await?;
    ui::visible(driver, LI_CONFIRM_BOX).await.context("Complete and Integrate did not ask first")?;
    let facts = await_text(driver, LI_FACTS, |t| !t.is_empty(), "the facts to confirm").await?;
    for expected in [format!("Current destination commit: {}", short(&main_a)), "Strategy: Fast-forward".to_string()] {
        if !facts.contains(&expected) {
            bail!("the first confirmation does not say {expected:?}: {facts:?}");
        }
    }

    // --- main moves on while the box is open -----------------------------------
    std::fs::write(repo.join("moved-meanwhile.txt"), "committed on main while confirming\n")?;
    git_out(&repo, &["add", "moved-meanwhile.txt"])?;
    git_out(&repo, &["commit", "--quiet", "-m", "main moved while confirming"])?;
    let main_b = repository.branch_tip("main")?.context("main is missing")?;
    if main_b == main_a {
        bail!("this journey proves nothing: main did not move");
    }

    // --- One click: Git refuses, nothing changes, the reason is shown ----------
    dismiss_toasts(driver).await?;
    click(driver, LI_CONFIRM, "the stale Complete and Integrate confirmation").await?;
    let reason = await_text(driver, LI_ERROR, |t| t.contains("moved after"), "the refusal of the stale confirmation").await?;
    await_hidden(driver, LI_CONFIRM_BOX, "the stale confirmation").await?;
    assert_stale_refusal_changed_nothing(driver, repository, &executed, &main_b, &origin_before).await?;

    // The refusal outlives the refresh that follows it (the panel now holds
    // the moved destination); the box does not reopen by itself, whatever the
    // refreshed plan now is.
    await_attribute(driver, LI_READY, "data-target-tip", &main_b).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    if !text_of_panel(driver, LI_ERROR).await?.contains(&reason) {
        bail!("the reason did not stay: {:?}", text_of_panel(driver, LI_ERROR).await?);
    }
    if count(driver, LI_CONFIRM_BOX).await? != 0 {
        bail!("the confirmation reopened without being asked");
    }
    assert_stale_refusal_changed_nothing(driver, repository, &executed, &main_b, &origin_before).await?;

    // Only the person dismisses it.
    click(driver, LI_ERROR_DISMISS, "dismiss the refusal").await?;
    await_hidden(driver, LI_ERROR, "the dismissed refusal").await?;

    // --- A new confirmation shows the new plan, and confirming it works --------
    dismiss_toasts(driver).await?;
    click(driver, LI_START, "Complete and Integrate").await?;
    ui::visible(driver, LI_CONFIRM_BOX).await.context("the new plan was not offered for confirmation")?;
    let facts = await_text(driver, LI_FACTS, |t| t.contains("Strategy: "), "the facts of the new plan").await?;
    for expected in [format!("Current destination commit: {}", short(&main_b)), "Strategy: Merge commit".to_string()] {
        if !facts.contains(&expected) {
            bail!("the new confirmation does not say {expected:?}: {facts:?}");
        }
    }
    dismiss_toasts(driver).await?;
    click(driver, LI_CONFIRM, "the new Complete and Integrate confirmation").await?;
    await_status(driver, &project_id, &executed.id, &["done"]).await?;
    await_worktree_removal(repository, &executed.worktree_path).await?;
    let said = await_text(driver, LI_RESULT, |t| t.contains("Integrated into main"), "the integration result").await?;
    if !said.contains("merge commit") {
        bail!("the result says {said:?}");
    }
    let parents = git_out(&repo, &["rev-list", "--parents", "-n", "1", "main"])?;
    let parents: Vec<&str> = parents.split_whitespace().skip(1).collect();
    if parents != [main_b.as_str(), tip.as_str()] {
        bail!("the merge commit has parents {parents:?}, not [{main_b}, {tip}]");
    }
    assert_main_checked_out_and_clean(repository)?;
    if repository.pushed_branches()? != origin_before {
        bail!("integrating pushed something to origin");
    }
    Ok(())
}

/// A refused stale confirmation left no trace: `main` is where its owner put
/// it, the task is in review with no integration record and its checkout, and
/// no pin ref was left behind.
async fn assert_stale_refusal_changed_nothing(
    driver: &WebDriver,
    repository: &GitFixture,
    task: &ExecutedTask,
    main_b: &str,
    origin_before: &str,
) -> Result<()> {
    if repository.branch_tip("main")?.as_deref() != Some(main_b) {
        bail!("the refused confirmation moved main");
    }
    let settled = read_task(driver, task).await?;
    if settled.get("status").and_then(Value::as_str) != Some("human_review")
        || integration_of(&settled).is_some()
        || !task.worktree_path.exists()
    {
        bail!("the refused confirmation changed the task: {settled}");
    }
    let repo = repository.path_buf();
    let pins = git_out(&repo, &["for-each-ref", "refs/slashit/integration"])?;
    if !pins.is_empty() {
        bail!("the refused confirmation left a pin: {pins}");
    }
    if repository.pushed_branches()? != origin_before {
        bail!("the refused confirmation pushed something to origin");
    }
    Ok(())
}
