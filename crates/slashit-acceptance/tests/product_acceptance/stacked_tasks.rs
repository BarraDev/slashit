//! A task stacked on another, through the parent's merge to the child's own
//! pull request.
//!
//! The parent and the child are run by the real executor, the parent's pull
//! request is opened by approving it, and the parent's merge is learned the
//! way a user's board learns it: GitHub (the fake `gh`) reports the pull
//! request as merged and the drawer's Refresh asks. The squash commit GitHub
//! would have made is made on `origin` by a scratch clone, since that is what
//! a merge on GitHub does and nothing SlashIt does. The child's pull request
//! is then opened by approving the child, which is the one point at which
//! SlashIt restacks an unpublished child onto the default branch.
//!
//! What each step is held to is read from places that cannot vouch for each
//! other: the product's own `list_tasks`, the task file, git (the fixture and
//! its bare `origin`), and the arguments `gh` was called with.

use super::human_review::{await_task, click, install_fakes, open_reviewed_drawer, read_task, HR_APPROVE};
use super::task_runs::{branch_name_of, create_task, git_out, git_succeeds, register_project, run_to_review};
use super::*;
use slashit_acceptance::fake_gh::FakeGh;

const DRAWER_PR_REFRESH: &str = "[data-testid=\"task-drawer-pr-refresh\"]";
const PR_CREATED_COLUMN: &str = "[data-testid=\"column-prcreated\"]";
const DONE_COLUMN_SELECTOR: &str = "[data-testid=\"column-done\"]";
const GITHUB_ADDRESS: &str = "git@github.com:slashit-acceptance/fixture.git";

impl GitFixture {
    /// Make `origin` read as the GitHub repository the fake `gh` stands for,
    /// while everything git does with it still reaches the bare repository
    /// beside the fixture: pushes through the push URL, and fetches and
    /// listings through an `ssh` stand-in that serves that repository (see
    /// `fixtures/ssh-to-local-origin`). Nothing can reach a real host.
    fn serve_origin_as_github(&self) -> Result<()> {
        let origin = self.origin.as_deref().context("this fixture was created without an origin")?;
        let origin = origin.to_str().context("the origin path is not UTF-8")?;
        let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join("ssh-to-local-origin");
        let shim = shim.to_str().context("the ssh stand-in's path is not UTF-8")?;
        git(&self.path, &["remote", "set-url", "origin", GITHUB_ADDRESS])?;
        git(&self.path, &["remote", "set-url", "--push", "origin", origin])?;
        git(&self.path, &["config", "core.sshCommand", &format!("'{shim}' '{origin}'")])
    }

    /// The commit `branch` is at in the bare `origin`, if it is there.
    fn remote_tip(&self, branch: &str) -> Result<Option<String>> {
        let origin = self.origin.as_deref().context("this fixture was created without an origin")?;
        let refname = format!("refs/heads/{branch}");
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(origin)
            .args(["rev-parse", "--verify", "--quiet", &refname])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .with_context(|| format!("could not resolve {refname} in origin"))?;
        Ok(output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string()))
    }

    /// What GitHub does when a pull request is squash-merged: one new commit
    /// on `origin`'s `main` holding the branch's changes, none of the
    /// branch's own commits. Done in a scratch clone, which is removed again,
    /// and returns the new commit.
    fn squash_merge_on_origin(&self, branch: &str, title: &str) -> Result<String> {
        let origin = self.origin.as_deref().context("this fixture was created without an origin")?;
        let origin = origin.to_str().context("the origin path is not UTF-8")?;
        let clone = self.state_root.join("github-side-clone");
        git_out(&self.state_root, &["clone", "--quiet", "--branch", "main", origin, clone.to_str().context("clone path")?])?;
        let landed = (|| -> Result<String> {
            git(&clone, &["config", "user.name", "GitHub"])?;
            git(&clone, &["config", "user.email", "noreply@localhost"])?;
            git(&clone, &["config", "commit.gpgsign", "false"])?;
            git(&clone, &["merge", "--squash", "--quiet", &format!("origin/{branch}")])?;
            git(&clone, &["commit", "--quiet", "-m", title])?;
            git(&clone, &["push", "--quiet", "origin", "main"])?;
            git_out(&clone, &["rev-parse", "HEAD"])
        })();
        let _ = std::fs::remove_dir_all(&clone);
        landed
    }
}

/// A parent task with a pull request, a child stacked on it, the parent's
/// merge, and the child's pull request: the child is restacked onto the
/// default branch the parent landed on, exactly once, and opened against it.
#[tokio::test(flavor = "multi_thread")]
async fn a_task_stacked_on_a_parent_is_restacked_onto_the_default_branch_once_the_parent_lands() {
    let context = TestContext::new("stacked_task_lifecycle").expect("harness setup");
    let outcome = stacked_journey(&context).await;
    context.finish(outcome);
}

/// What the first session establishes and the second looks for.
struct Restacked {
    project_id: String,
    child: ExecutedTask,
    landed: String,
}

async fn stacked_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (_agent, gh) = install_fakes(context, &root)?;
    // Real work, so reviews run and the parent and the child each leave
    // commits of their own, in different files.
    context.set_child_env(fake_agent::WRITE_OWN_FILE_VAR, "1");
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    repository.serve_origin_as_github()?;

    let session = context.start_session("stacked").await?;
    let outcome = stack_land_and_restack(session.driver(), &gh, &repository).await;
    context.close_session(session, "stacked", &outcome).await?;
    let restacked = outcome?;

    // The same facts, in the task file.
    let persisted = persisted_origin(&root, &restacked.child.id)?;
    if persisted != ("default_base".to_string(), Some("main".to_string()), Some(restacked.landed.clone())) {
        bail!("the task file holds branch origin and base {persisted:?}, expected the default base main at {}", restacked.landed);
    }

    let session = context.start_session("stacked-restart").await?;
    let outcome = restack_survives_a_restart(session.driver(), &repository, &restacked).await;
    context.close_session(session, "stacked-restart", &outcome).await?;
    outcome
}

/// `(origin kind, default branch if any, base commit)` as the task file holds them.
fn persisted_origin(root: &Path, task_id: &str) -> Result<(String, Option<String>, Option<String>)> {
    for file in toml_files(root)? {
        let Ok(contents) = std::fs::read_to_string(&file) else { continue };
        let Ok(document) = contents.parse::<toml::Value>() else { continue };
        let Some(record) = task_record(&document, task_id) else { continue };
        let origin = record.get("branch_origin").context("the task file records no branch origin")?;
        let text = |value: Option<&toml::Value>| value.and_then(toml::Value::as_str).map(str::to_string);
        return Ok((
            text(origin.get("kind")).context("the branch origin has no kind")?,
            text(origin.get("branch")),
            text(record.get("base_commit")),
        ));
    }
    bail!("no task file under {} holds task {task_id}", root.display())
}

async fn stack_land_and_restack(
    driver: &WebDriver,
    gh: &FakeGh,
    repository: &GitFixture,
) -> Result<Restacked> {
    let project_id = register_project(driver, repository).await?;

    // --- The parent, with its own pull request --------------------------------
    let (parent_id, parent_title) = create_task(driver, &project_id, "Stack parent", &[]).await?;
    let parent = run_to_review(driver, &project_id, &parent_id, &parent_title).await?;
    let parent_branch = branch_name_of(&read_task(driver, &parent).await?)?;
    let parent_tip = repository.branch_tip(&parent_branch)?.context("the parent branch is missing")?;
    approve_and_open_the_pull_request(driver, &parent, gh).await?;
    let parent_pr = gh.pr_for(&parent_branch)?.context("gh opened no pull request for the parent")?;
    let parent_number: u64 = parent_pr
        .rsplit('/')
        .next()
        .and_then(|n| n.parse().ok())
        .with_context(|| format!("cannot read a pull request number from {parent_pr}"))?;
    if repository.remote_tip(&parent_branch)?.as_deref() != Some(parent_tip.as_str()) {
        bail!("origin does not hold the parent's branch at its reviewed tip {parent_tip}");
    }

    // --- The child, created depending on the parent and run -------------------
    let (child_id, child_title) = create_task(driver, &project_id, "Stack child", &[parent_id.as_str()]).await?;
    let child = run_to_review(driver, &project_id, &child_id, &child_title).await?;
    let child_task = read_task(driver, &child).await?;
    let child_branch = branch_name_of(&child_task)?;
    if child_branch == parent_branch {
        bail!("the child runs on its parent's branch {parent_branch}");
    }
    let child_tip_before = repository.branch_tip(&child_branch)?.context("the child branch is missing")?;

    // The stack, as the product recorded it when it created the child's branch.
    if child_task.pointer("/branch_origin/kind").and_then(Value::as_str) != Some("stacked")
        || child_task.pointer("/branch_origin/parent_branch").and_then(Value::as_str) != Some(parent_branch.as_str())
    {
        bail!("the child's branch origin is {:?}, not stacked on {parent_branch}", child_task.get("branch_origin"));
    }
    if child_task.get("base_commit").and_then(Value::as_str) != Some(parent_tip.as_str()) {
        bail!("the child's fork point is {:?}, not the parent's tip {parent_tip}", child_task.get("base_commit"));
    }
    // And as git holds it: the child starts at the parent's tip and adds
    // exactly one commit of its own, in its own file.
    let repo = repository.path_buf();
    if !git_succeeds(&repo, &["merge-base", "--is-ancestor", &parent_tip, &child_tip_before])? {
        bail!("the child's branch does not contain the parent's tip {parent_tip}");
    }
    let own = git_out(&repo, &["rev-list", &format!("{parent_tip}..{child_tip_before}")])?;
    if own.lines().count() != 1 {
        bail!("the child holds {} commits beyond the parent's tip, expected its one: {own}", own.lines().count());
    }
    let child_file = fake_agent::own_work_file(&child_branch);
    if repository.file_at(&child_branch, &child_file)?.as_deref() != Some(fake_agent::WORK_CONTENT)
        || repository.file_at(&parent_branch, &child_file)?.is_some()
    {
        bail!("{child_file} is not the child's own work");
    }
    if repository.remote_tip(&child_branch)?.is_some() || gh.pr_for(&child_branch)?.is_some() {
        bail!("the child was published before anyone approved it");
    }
    let main_before = repository.branch_tip("main")?;

    // --- The parent lands on GitHub --------------------------------------------
    let pr_commits = git_out(&repo, &["rev-list", "--reverse", &format!("main..{parent_tip}")])?;
    let landed = repository.squash_merge_on_origin(&parent_branch, &format!("{parent_title} (#{parent_number})"))?;
    gh.script_pr_list_of(
        &parent_branch,
        &json!([{ "number": parent_number, "state": "MERGED", "baseRefName": "main", "isCrossRepository": false, "url": parent_pr }]),
    )?;
    gh.script_pr_view_of(
        parent_number,
        &json!({
            "number": parent_number, "state": "MERGED", "baseRefName": "main", "headRefName": parent_branch,
            "isCrossRepository": false, "mergeCommit": { "oid": landed },
            "mergeable": "UNKNOWN", "reviewDecision": "", "statusCheckRollup": [],
        }),
    )?;
    gh.script_pr_commits(parent_number, &pr_commits.lines().map(str::to_string).collect::<Vec<_>>())?;

    // SlashIt learns of it from the parent's Refresh and finishes the parent.
    open_board(driver, &project_id).await?;
    open_reviewed_drawer_of_a_delivered_task(driver, &parent).await?;
    click(driver, DRAWER_PR_REFRESH, "the parent's pull request Refresh").await?;
    await_task(driver, &parent, "Done after its pull request merged", |t| status_of(t) == Some("done")).await?;
    await_worktree_removal(repository, &parent.worktree_path).await?;
    if repository.branch_tip(&parent_branch)?.as_deref() != Some(parent_tip.as_str()) {
        bail!("finishing the merged parent moved or removed its branch");
    }
    // The child is still stacked: nothing has restacked it yet.
    let child_task = read_task(driver, &child).await?;
    if child_task.pointer("/branch_origin/kind").and_then(Value::as_str) != Some("stacked")
        || repository.branch_tip(&child_branch)?.as_deref() != Some(child_tip_before.as_str())
    {
        bail!("the parent landing already changed the child: {child_task}");
    }

    // --- The child's pull request ------------------------------------------------
    approve_and_open_the_pull_request(driver, &child, gh).await?;
    let settled = read_task(driver, &child).await?;
    if gh.create_attempts()? != 2 {
        bail!("gh pr create ran {} times for two approvals", gh.create_attempts()?);
    }
    let created = gh
        .invocations()?
        .into_iter()
        .find(|args| args.iter().any(|a| a == "create") && args.windows(2).any(|w| w[0] == "--head" && w[1] == child_branch))
        .context("gh was never asked to open the child's pull request")?;
    if !created.windows(2).any(|w| w[0] == "--base" && w[1] == "main") {
        bail!("the child's pull request was not opened against main: {created:?}");
    }

    // The task now says it starts at what landed.
    if settled.pointer("/branch_origin/kind").and_then(Value::as_str) != Some("default_base")
        || settled.pointer("/branch_origin/branch").and_then(Value::as_str) != Some("main")
        || settled.get("base_commit").and_then(Value::as_str) != Some(landed.as_str())
    {
        bail!(
            "after the restack the child records origin {:?} and fork point {:?}, expected the default base main at {landed}",
            settled.get("branch_origin"),
            settled.get("base_commit")
        );
    }
    // And git agrees: the branch was replayed, not merely relabelled.
    let child_tip = repository.branch_tip(&child_branch)?.context("the child branch is gone")?;
    if child_tip == child_tip_before {
        bail!("the child's branch was not rewritten");
    }
    if !git_succeeds(&repo, &["merge-base", "--is-ancestor", &landed, &child_tip])? {
        bail!("the child's branch does not sit on what landed ({landed})");
    }
    if git_succeeds(&repo, &["merge-base", "--is-ancestor", &parent_tip, &child_tip])? {
        bail!("the child's branch still carries the parent's original commit {parent_tip}");
    }
    let own = git_out(&repo, &["rev-list", &format!("{landed}..{child_tip}")])?;
    if own.lines().count() != 1 || repository.file_at(&child_branch, &child_file)?.as_deref() != Some(fake_agent::WORK_CONTENT) {
        bail!("the restacked child does not hold exactly its own work: {own:?}");
    }
    if repository.file_at(&child_branch, &fake_agent::own_work_file(&parent_branch))?.as_deref() != Some(fake_agent::WORK_CONTENT) {
        bail!("the restacked child lost the parent's work, which landed on main");
    }
    if repository.remote_tip(&child_branch)?.as_deref() != Some(child_tip.as_str()) {
        bail!("origin does not hold the restacked child at {child_tip}");
    }
    // Nothing else moved: the parent's branch, local main, and the old work.
    if repository.branch_tip(&parent_branch)?.as_deref() != Some(parent_tip.as_str()) || repository.branch_tip("main")? != main_before {
        bail!("restacking the child moved the parent's branch or the local main");
    }
    if !git_succeeds(&repo, &["cat-file", "-e", &format!("{child_tip_before}^{{commit}}")])? {
        bail!("the child's previous tip {child_tip_before} no longer exists");
    }
    let backups = git_out(&repo, &["for-each-ref", "refs/slashit/restack-backup"])?;
    if !backups.is_empty() {
        bail!("a restack backup ref was left behind: {backups}");
    }
    // The checkout follows its branch and holds nothing uncommitted.
    if git_out(&child.worktree_path, &["rev-parse", "HEAD"])? != child_tip
        || !git_out(&child.worktree_path, &["status", "--porcelain"])?.is_empty()
    {
        bail!("the child's checkout is not clean at the restacked tip");
    }

    open_board(driver, &project_id).await?;
    assert_card_in_column(driver, PR_CREATED_COLUMN, &child.title).await?;
    assert_card_in_column(driver, DONE_COLUMN_SELECTOR, &parent.title).await?;
    Ok(Restacked { project_id, child, landed })
}

/// Approve & Create PR from the drawer, and wait for PR Created.
async fn approve_and_open_the_pull_request(driver: &WebDriver, task: &ExecutedTask, gh: &FakeGh) -> Result<()> {
    let before = gh.create_attempts()?;
    open_board(driver, &task.project_id).await?;
    open_reviewed_drawer(driver, task).await?;
    let label = await_text(driver, HR_APPROVE, |t| t != "Checking…", "the approve action").await?;
    if label != "Approve & Create PR" {
        bail!("a GitHub project should offer Approve & Create PR, the drawer offers {label:?}");
    }
    click(driver, HR_APPROVE, "Approve & Create PR").await?;
    await_task(driver, task, "PR Created", |t| status_of(t) == Some("pr_created")).await?;
    if gh.create_attempts()? != before + 1 {
        bail!("approving {} did not open exactly one pull request", task.title);
    }
    Ok(())
}

/// Open the drawer of a task whose pull request is open.
async fn open_reviewed_drawer_of_a_delivered_task(driver: &WebDriver, task: &ExecutedTask) -> Result<()> {
    open_drawer(driver, &task.id, &task.title).await?;
    ui::visible(driver, DRAWER_PR_REFRESH).await.context("the drawer offers no pull request Refresh")?;
    Ok(())
}

async fn restack_survives_a_restart(driver: &WebDriver, repository: &GitFixture, restacked: &Restacked) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    show_on_board(driver, &restacked.project_id, PR_CREATED_COLUMN, &restacked.child.title).await?;
    let task = read_task(driver, &restacked.child).await?;
    if status_of(&task) != Some("pr_created")
        || task.pointer("/branch_origin/kind").and_then(Value::as_str) != Some("default_base")
        || task.get("base_commit").and_then(Value::as_str) != Some(restacked.landed.as_str())
    {
        bail!("after a restart the child is {task}");
    }
    let branch = branch_name_of(&task)?;
    if repository.remote_tip(&branch)? != repository.branch_tip(&branch)? {
        bail!("after a restart the child's branch and origin's disagree");
    }
    Ok(())
}
