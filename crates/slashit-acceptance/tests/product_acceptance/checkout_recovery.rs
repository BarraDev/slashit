//! What starting the application again does to the Task Checkouts of tasks
//! that were already running, when the checkouts met three different fates
//! while it was stopped.
//!
//! Three tasks each reach Human Review with a real Task Checkout holding
//! committed work. The application is stopped, and the person who owns the
//! repository does three different things with Git, one per task:
//!
//! - nothing, beyond leaving an uncommitted note in the checkout (intact);
//! - `git worktree move`, so the checkout is elsewhere but Git still has it
//!   registered (moved);
//! - `git worktree remove`, so the directory and Git's registration are both
//!   gone (gone). Plain deletion of the directory is not used: that leaves a
//!   stale registration behind, which is a different, murkier state.
//!
//! A fourth checkout, made by hand for a branch no task owns, stands for
//! everything else in the repository. After the restart each task is held to
//! its own outcome, nothing the person kept is touched, and each task is then
//! sent back to its agent from the drawer to show it is still usable.

use super::human_review::{await_task, click, open_reviewed_drawer, HR_FEEDBACK, HR_REQUEST_CHANGES, HR_SEND_FEEDBACK};
use super::task_runs::{branch_name_of, create_task, git_out, register_project, run_to_review};
use super::*;

const NOTE: &str = "uncommitted-note.txt";
const NOTE_CONTENT: &str = "kept by the person who owns this checkout\n";

#[tokio::test(flavor = "multi_thread")]
async fn restarting_keeps_an_intact_checkout_adopts_a_moved_one_and_clears_a_gone_one() {
    let context = TestContext::new("checkout_recovery").expect("harness setup");
    let outcome = recovery_journey(&context).await;
    context.finish(outcome);
}

/// One task's checkout, as it was when the application was stopped.
struct Case {
    task: ExecutedTask,
    branch: String,
    tip: String,
    work_file: String,
    /// The feedback that sends this task back, and so identifies its rerun.
    feedback: String,
}

struct Before {
    project_id: String,
    intact: Case,
    moved: Case,
    gone: Case,
    bystander: PathBuf,
    bystander_branch: String,
    main: String,
}

async fn recovery_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_agent::WRITE_OWN_FILE_VAR, "1");
    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("recovery").await?;
    let outcome = run_three_tasks(session.driver(), &repository).await;
    context.close_session(session, "recovery", &outcome).await?;
    let before = outcome?;

    // The application is not running. What happens next is the repository
    // owner's own doing, with Git.
    let relocated = apply_the_three_fates(&repository, &before)?;

    let session = context.start_session("recovery-restart").await?;
    let outcome = recover(session.driver(), &agent, &repository, &root, &before, &relocated).await;
    context.close_session(session, "recovery-restart", &outcome).await?;
    outcome
}

async fn run_three_tasks(driver: &WebDriver, repository: &GitFixture) -> Result<Before> {
    let project_id = register_project(driver, repository).await?;
    let mut cases = Vec::new();
    for label in ["intact", "moved", "gone"] {
        let (id, title) = create_task(driver, &project_id, &format!("Checkout {label}"), &[]).await?;
        let task = run_to_review(driver, &project_id, &id, &title).await?;
        let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
        let branch = branch_name_of(&find_task(&listed, &id).context("the task is not listed")?)?;
        let tip = repository.branch_tip(&branch)?.context("the task branch is missing")?;
        let directory = task.worktree_path.file_name().and_then(OsStr::to_str).context("the checkout has no name")?;
        let work_file = fake_agent::own_work_file(directory);
        if repository.file_at(&branch, &work_file)?.as_deref() != Some(fake_agent::WORK_CONTENT) {
            bail!("{label}: branch {branch} does not carry the agent's committed work {work_file}");
        }
        if !task.worktree_path.is_dir() || !repository.registers_worktree(&task.worktree_path)? {
            bail!("{label}: the recorded checkout {} is not a registered directory", task.worktree_path.display());
        }
        cases.push(Case { task, branch, tip, work_file, feedback: format!("keep going after the restart (recovery-{label})") });
    }
    let gone = cases.pop().context("no third task")?;
    let moved = cases.pop().context("no second task")?;
    let intact = cases.pop().context("no first task")?;

    // A checkout of a branch no task owns, made by hand beside them.
    let bystander = repository.state_root().join("hand-made-checkout");
    let bystander_branch = "hand-made".to_string();
    git_out(
        &repository.path_buf(),
        &["worktree", "add", "-q", "-b", &bystander_branch, bystander.to_str().context("path")?],
    )?;
    std::fs::write(bystander.join(NOTE), NOTE_CONTENT).context("could not write the bystander's note")?;
    let main = repository.branch_tip("main")?.context("main is missing")?;
    Ok(Before { project_id, intact, moved, gone, bystander, bystander_branch, main })
}

/// What the repository owner does to each checkout while the application is
/// stopped. Returns where the moved checkout now is.
fn apply_the_three_fates(repository: &GitFixture, before: &Before) -> Result<PathBuf> {
    let repo = repository.path_buf();

    std::fs::write(before.intact.task.worktree_path.join(NOTE), NOTE_CONTENT).context("could not leave a note")?;

    let moved_from = &before.moved.task.worktree_path;
    std::fs::write(moved_from.join(NOTE), NOTE_CONTENT).context("could not leave a note")?;
    let parent = repository.state_root().join("relocated-checkouts");
    std::fs::create_dir_all(&parent).context("could not create the relocation directory")?;
    let moved_to = parent.join(&before.moved.branch);
    git_out(&repo, &["worktree", "move", moved_from.to_str().context("path")?, moved_to.to_str().context("path")?])?;

    // Not forced: Git refuses a checkout with uncommitted work, so this also
    // shows the task's checkout held nothing that was not committed.
    git_out(&repo, &["worktree", "remove", before.gone.task.worktree_path.to_str().context("path")?])?;

    // Three distinct states, established before the application starts.
    if !before.intact.task.worktree_path.is_dir() || !repository.registers_worktree(&before.intact.task.worktree_path)? {
        bail!("the intact checkout was disturbed");
    }
    if moved_from.exists() || repository.registers_worktree(moved_from)? || !repository.registers_worktree(&moved_to)? {
        bail!("the moved checkout is not simply elsewhere and still registered");
    }
    if before.gone.task.worktree_path.exists() || repository.registers_worktree(&before.gone.task.worktree_path)? {
        bail!("the gone checkout is still there or still registered");
    }
    Ok(moved_to)
}

async fn recover(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
    root: &Path,
    before: &Before,
    relocated: &Path,
) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    open_board(driver, &before.project_id).await?;
    for case in [&before.intact, &before.moved, &before.gone] {
        assert_card_in_column(driver, HUMAN_REVIEW_COLUMN, &case.task.title).await?;
    }

    // --- Intact: the same checkout, untouched ---------------------------------
    let task = listed(driver, &before.intact).await?;
    if recorded_checkout(&task) != Some(resolve(&before.intact.task.worktree_path)) {
        bail!("the intact checkout is now recorded as {:?}", task.get("worktree_path"));
    }
    let checkout = &before.intact.task.worktree_path;
    if !repository.registers_worktree(checkout)?
        || std::fs::read_to_string(checkout.join(NOTE)).ok().as_deref() != Some(NOTE_CONTENT)
        || std::fs::read_to_string(checkout.join(&before.intact.work_file)).ok().as_deref() != Some(fake_agent::WORK_CONTENT)
        || git_out(checkout, &["status", "--porcelain"])? != format!("?? {NOTE}")
        || git_out(checkout, &["rev-parse", "HEAD"])? != before.intact.tip
    {
        bail!("the intact checkout at {} was changed by the restart", checkout.display());
    }

    // --- Moved: the task follows Git to where the checkout now is ---------------
    let task = listed(driver, &before.moved).await?;
    if recorded_checkout(&task) != Some(resolve(relocated)) {
        bail!("the moved checkout is recorded as {:?}, Git has it at {}", task.get("worktree_path"), relocated.display());
    }
    if before.moved.task.worktree_path.exists() || repository.registers_worktree(&before.moved.task.worktree_path)? {
        bail!("the old location of the moved checkout came back");
    }
    if !repository.registers_worktree(relocated)?
        || std::fs::read_to_string(relocated.join(NOTE)).ok().as_deref() != Some(NOTE_CONTENT)
        || std::fs::read_to_string(relocated.join(&before.moved.work_file)).ok().as_deref() != Some(fake_agent::WORK_CONTENT)
        || git_out(relocated, &["status", "--porcelain"])? != format!("?? {NOTE}")
        || git_out(relocated, &["rev-parse", "HEAD"])? != before.moved.tip
    {
        bail!("the moved checkout at {} was changed by the restart", relocated.display());
    }
    // Written to the task file by the restart itself, not merely held in memory.
    if persisted_checkout(root, &before.moved.task.id)?.map(|p| resolve(Path::new(&p))) != Some(resolve(relocated)) {
        bail!("the task file does not record the moved checkout's new location");
    }

    // --- Gone: the stale reference is cleared and nothing else ----------------
    let task = listed(driver, &before.gone).await?;
    if !task.get("worktree_path").is_none_or(Value::is_null) {
        bail!("the gone checkout is still recorded as {:?}", task.get("worktree_path"));
    }
    if status_of(&task) != Some("human_review") || !task.get("error_message").is_none_or(Value::is_null) {
        bail!("clearing the reference changed the task: {task}");
    }
    if let Some(path) = persisted_checkout(root, &before.gone.task.id)? {
        bail!("the task file still records the gone checkout as {path}");
    }
    if before.gone.task.worktree_path.exists() || repository.registers_worktree(&before.gone.task.worktree_path)? {
        bail!("the restart brought the gone checkout back");
    }
    // The work is on its branch, which is still there.
    if repository.branch_tip(&before.gone.branch)?.as_deref() != Some(before.gone.tip.as_str())
        || repository.file_at(&before.gone.branch, &before.gone.work_file)?.as_deref() != Some(fake_agent::WORK_CONTENT)
    {
        bail!("the gone checkout's branch {} lost its work", before.gone.branch);
    }

    // --- Nothing unrelated was removed ---------------------------------------
    for case in [&before.intact, &before.moved, &before.gone] {
        if repository.branch_tip(&case.branch)?.as_deref() != Some(case.tip.as_str()) {
            bail!("branch {} is not where the task left it", case.branch);
        }
    }
    if !repository.registers_worktree(&before.bystander)?
        || std::fs::read_to_string(before.bystander.join(NOTE)).ok().as_deref() != Some(NOTE_CONTENT)
        || !repository.has_branch(&before.bystander_branch)?
    {
        bail!("the hand-made checkout {} was disturbed by the restart", before.bystander.display());
    }
    if repository.branch_tip("main")?.as_deref() != Some(before.main.as_str()) {
        bail!("the restart moved main");
    }

    // --- Each task is still usable: sent back, it works in the right place ----
    let rerun = send_back(driver, agent, &before.intact).await?;
    if resolve(&rerun) != resolve(&before.intact.task.worktree_path) {
        bail!("the intact task's rerun ran in {}", rerun.display());
    }
    let rerun = send_back(driver, agent, &before.moved).await?;
    if resolve(&rerun) != resolve(relocated) {
        bail!("the moved task's rerun ran in {}, not where its checkout now is", rerun.display());
    }
    let rerun = send_back(driver, agent, &before.gone).await?;
    let task = listed(driver, &before.gone).await?;
    if recorded_checkout(&task) != Some(resolve(&rerun)) || !rerun.is_dir() || !repository.registers_worktree(&rerun)? {
        bail!("the gone task's rerun ran in {}, which is not the checkout it now records", rerun.display());
    }
    if git_out(&rerun, &["symbolic-ref", "HEAD"])? != format!("refs/heads/{}", before.gone.branch)
        || std::fs::read_to_string(rerun.join(&before.gone.work_file)).ok().as_deref() != Some(fake_agent::WORK_CONTENT)
    {
        bail!("the gone task's new checkout is not its branch with its earlier work");
    }
    Ok(())
}

async fn listed(driver: &WebDriver, case: &Case) -> Result<Value> {
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": case.task.project_id })).await?;
    find_task(&listed, &case.task.id).with_context(|| format!("the product no longer lists {}", case.task.title))
}

fn recorded_checkout(task: &Value) -> Option<PathBuf> {
    task.get("worktree_path").and_then(Value::as_str).map(|p| resolve(Path::new(p)))
}

/// The checkout the task file records for a task, if it records one.
fn persisted_checkout(root: &Path, task_id: &str) -> Result<Option<String>> {
    for file in toml_files(root)? {
        let Ok(contents) = std::fs::read_to_string(&file) else { continue };
        let Ok(document) = contents.parse::<toml::Value>() else { continue };
        if let Some(record) = task_record(&document, task_id) {
            return Ok(record.get("worktree_path").and_then(toml::Value::as_str).map(str::to_string));
        }
    }
    bail!("no task file under {} holds task {task_id}", root.display())
}

/// Send the task back from its drawer with its own feedback, wait for it to
/// reach Human Review again, and return the directory its coding run was
/// started in.
async fn send_back(driver: &WebDriver, agent: &FakeAgent, case: &Case) -> Result<PathBuf> {
    open_board(driver, &case.task.project_id).await?;
    open_reviewed_drawer(driver, &case.task).await?;
    click(driver, HR_REQUEST_CHANGES, "Request Changes").await?;
    ui::visible(driver, HR_FEEDBACK).await?.send_keys(case.feedback.as_str()).await.context("could not type the feedback")?;
    click(driver, HR_SEND_FEEDBACK, "Send back to the agent").await?;
    await_task(driver, &case.task, "Human Review after the rerun", |t| {
        status_of(t) == Some("human_review") && t.pointer("/human_review/arrivals").and_then(Value::as_u64) == Some(2)
    })
    .await?;
    let reruns: Vec<_> = agent_runs(agent)?
        .into_iter()
        .filter(|run| run.prompt.as_deref().is_some_and(|p| p.contains(&case.feedback)))
        .collect();
    match reruns.as_slice() {
        [only] if agent_role(only)? == AgentRole::Execution => Ok(only.working_dir.clone()),
        other => bail!("expected one coding run carrying {:?}, found {}", case.feedback, other.len()),
    }
}
