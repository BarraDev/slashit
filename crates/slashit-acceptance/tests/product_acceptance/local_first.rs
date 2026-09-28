//! Local-first projects: a remote is optional, version control is not.
//!
//! A repository with commits and no remote runs a task end to end -- its
//! own branch and checkout, the agent, the commit, AI Review and the Human
//! Review decision -- and the pull request it cannot have is explained, not
//! attempted. A folder with no version control is created through the real
//! Create Project form, which says what initializing will do and uses the
//! branch Git itself names; a folder opened without it is refused at Start
//! with what to do, never initialized behind the person's back, and becomes
//! usable through Settings > Repository.

use super::human_review::{
    attribute, click, install_fakes, open_reviewed_drawer, read_task, HR_APPROVE, HR_APPROVED,
    HR_DELIVERY,
};
use super::*;
use thirtyfour::Key;

/// The branch the local-only fixture commits on. Deliberately not a name
/// anything in SlashIt could assume.
const LOCAL_BRANCH: &str = "trunk-local";
/// The branch `git init` picks for the application in the setup journey,
/// from the Git configuration this journey gives it.
const GIT_DEFAULT_BRANCH: &str = "trunk-xyz";

const ADD_PROJECT: &str = "[data-testid=\"rail-add-project\"]";
const WIZARD_CREATE: &str = "[data-testid=\"wizard-create-project\"]";
const PROJECT_NAME: &str = "[data-testid=\"create-project-name\"]";
const PROJECT_LOCATION: &str = "[data-testid=\"create-project-location\"]";
const PROJECT_SUBMIT: &str = "[data-testid=\"create-project-submit\"]";
const VCS_ACTION: &str = "[data-testid=\"vcs-init-action\"]";
const VCS_TOGGLE: &str = "[data-testid=\"vcs-init-toggle\"]";
const SETTINGS_REPOSITORY_TAB: &str = "[data-testid=\"settings-tab-repository\"]";
const REPOSITORY_BLOCKED: &str = "[data-testid=\"repository-blocked\"]";
const REPOSITORY_READY: &str = "[data-testid=\"repository-ready\"]";
const REPOSITORY_INIT_ACTION: &str = "[data-testid=\"repository-initialize-action\"]";
const REPOSITORY_INIT_GIT: &str = "[data-testid=\"repository-initialize-git\"]";
const REPOSITORY_REMOTE_NONE: &str = "[data-testid=\"repository-remote-none\"]";

impl GitFixture {
    /// [`GitFixture::create`] without any remote: one commit on
    /// [`LOCAL_BRANCH`], and nothing else.
    fn create_local_only(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)
            .with_context(|| format!("could not create {}", path.display()))?;
        git(
            path,
            &[
                "init",
                "--quiet",
                &format!("--initial-branch={LOCAL_BRANCH}"),
            ],
        )?;
        git(path, &["config", "user.name", "SlashIt Acceptance"])?;
        git(path, &["config", "user.email", "acceptance@localhost"])?;
        git(path, &["config", "commit.gpgsign", "false"])?;
        std::fs::write(path.join("README.md"), "Local-only fixture repository.\n")?;
        git(path, &["add", "README.md"])?;
        git(path, &["commit", "--quiet", "-m", "initial commit"])?;
        let state_root = path
            .parent()
            .context("the fixture has no parent directory")?
            .to_path_buf();
        Ok(Self {
            path: path.to_path_buf(),
            state_root,
        })
    }
}

// --- A repository with no remote ------------------------------------------------

/// Git with commits and no remote: the task gets its own branch from the
/// project's local base and its own checkout, the agent runs, its work is
/// committed, AI Review runs, and approving in Human Review records the
/// decision while explaining that no pull request can be opened.
#[tokio::test(flavor = "multi_thread")]
async fn a_repository_without_a_remote_runs_a_task_through_review() {
    let context = TestContext::new("local_only_task").expect("harness setup");
    let outcome = local_only_journey(&context).await;
    context.finish(outcome);
}

async fn local_only_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = install_fakes(context, &root)?;
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);
    let repository = GitFixture::create_local_only(&root.join("fixture-repo"))?;
    let base = repository
        .branch_tip(LOCAL_BRANCH)?
        .context("the fixture has no base commit")?;

    let session = context.start_session("local-only").await?;
    let outcome = run_and_approve_locally(session.driver(), &agent, &repository, &base).await;
    context
        .close_session(session, "local-only", &outcome)
        .await?;
    let executed = outcome?;

    assert_agent_roles(&agent, "local_only_journey", 1, 1, 0)?;
    if !gh.invocations()?.is_empty() {
        bail!(
            "gh was run for a project with no remote: {:?}",
            gh.invocations()?
        );
    }
    let remotes = std::process::Command::new("git")
        .arg("remote")
        .current_dir(repository.path_buf())
        .output()
        .context("could not list the fixture's remotes")?;
    if !remotes.stdout.is_empty() {
        bail!(
            "a remote appeared: {}",
            String::from_utf8_lossy(&remotes.stdout)
        );
    }
    let persisted = super::human_review::persisted_review(&root, &executed.id)?;
    if persisted.status != "human_review"
        || persisted.entries != vec![("approved".to_string(), None)]
    {
        bail!(
            "the task file holds {} with review {:?}",
            persisted.status,
            persisted.entries
        );
    }
    Ok(())
}

async fn run_and_approve_locally(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
    base: &str,
) -> Result<ExecutedTask> {
    let executed = execute_one_task(driver, agent, repository).await?;

    // Its own branch, started exactly at the local base, recorded as such.
    let task = read_task(driver, &executed).await?;
    if task.get("base_commit").and_then(Value::as_str) != Some(base) {
        bail!("the task did not start at {LOCAL_BRANCH} ({base}): {task}");
    }
    if task.get("branch_origin") != Some(&json!({ "kind": "local_base", "branch": LOCAL_BRANCH })) {
        bail!(
            "the task does not record its local base: {:?}",
            task.get("branch_origin")
        );
    }
    // The agent's work is committed on that branch, in a real Git worktree.
    let work = establish_work(driver, repository, &executed).await?;
    if repository.branch_tip(LOCAL_BRANCH)?.as_deref() != Some(base) {
        bail!("running the task moved {LOCAL_BRANCH}");
    }

    open_reviewed_drawer(driver, &executed).await?;
    let label = await_text(
        driver,
        HR_APPROVE,
        |t| t != "Checking…",
        "the approve action",
    )
    .await?;
    if label != "Approve"
        || attribute(driver, HR_APPROVE, "data-creates-pr")
            .await?
            .as_deref()
            != Some("false")
    {
        bail!("without a remote the drawer should offer a plain Approve, it offers {label:?}");
    }
    click(driver, HR_APPROVE, "Approve").await?;
    ui::visible(driver, HR_APPROVED)
        .await
        .context("the drawer does not say Approved")?;
    if attribute(driver, HR_DELIVERY, "data-delivery")
        .await?
        .as_deref()
        != Some("unavailable")
    {
        bail!("the drawer does not explain that no pull request can be created");
    }
    let explained = await_text(driver, HR_DELIVERY, |t| !t.is_empty(), "the explanation").await?;
    for expected in [
        "no `origin` remote",
        "reviewed locally",
        "Nothing was merged",
    ] {
        if !explained.contains(expected) {
            bail!("the explanation {explained:?} does not say {expected:?}");
        }
    }
    if repository.branch_tip(&work.branch)?.as_deref() != Some(work.commit.as_str()) {
        bail!("approving moved the task branch");
    }
    Ok(executed)
}

// --- A folder with no version control ---------------------------------------------

/// Create Project in a folder with files and no version control says what
/// initializing will do, does it only when ticked, commits what the ignore
/// rules keep on the branch Git names, and the project runs tasks from it.
/// A folder opened without version control is refused at Start, is not
/// initialized by it, and is set up from Settings > Repository.
#[tokio::test(flavor = "multi_thread")]
async fn projects_without_version_control_are_set_up_explicitly() {
    let context = TestContext::new("vcs_setup").expect("harness setup");
    let outcome = setup_journey(&context).await;
    context.finish(outcome);
}

async fn setup_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    start_on_the_legacy_auto_placement(&context.state().config_file())?;

    // The application's own Git configuration: an identity, and a default
    // branch name nothing in SlashIt could have assumed.
    let git_config = root.join("app-gitconfig");
    std::fs::write(
        &git_config,
        format!(
            "[user]\n\tname = SlashIt Acceptance\n\temail = acceptance@localhost\n[init]\n\tdefaultBranch = {GIT_DEFAULT_BRANCH}\n[commit]\n\tgpgsign = false\n"
        ),
    )?;
    context.set_child_env("GIT_CONFIG_GLOBAL", &git_config);
    context.set_child_env("GIT_CONFIG_NOSYSTEM", "1");

    let created = root.join("new-project");
    std::fs::create_dir_all(created.join("scratch"))?;
    std::fs::write(created.join("idea.md"), "An idea.\n")?;
    std::fs::write(created.join(".gitignore"), "scratch/\n")?;
    std::fs::write(
        created.join("scratch/notes.txt"),
        "not for the repository\n",
    )?;
    let opened = root.join("opened-folder");
    std::fs::create_dir_all(&opened)?;
    std::fs::write(opened.join("draft.md"), "A draft.\n")?;

    let session = context.start_session("vcs-setup").await?;
    let outcome = set_up_both(session.driver(), &created, &opened).await;
    context
        .close_session(session, "vcs-setup", &outcome)
        .await?;
    outcome
}

async fn set_up_both(driver: &WebDriver, created: &Path, opened: &Path) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    create_through_the_form(driver, created).await?;
    open_and_set_up(driver, opened).await
}

async fn create_through_the_form(driver: &WebDriver, folder: &Path) -> Result<()> {
    let location = folder.to_str().context("the folder path is not UTF-8")?;
    click(driver, ADD_PROJECT, "Add project").await?;
    click(driver, WIZARD_CREATE, "Create Project").await?;
    ui::visible(driver, PROJECT_NAME)
        .await?
        .send_keys("Local First")
        .await?;
    let input = ui::visible(driver, PROJECT_LOCATION).await?;
    input.send_keys(location).await?;
    input.send_keys(Key::Tab).await?;

    // What will happen is said before it happens, and nothing has yet.
    let action = await_text(
        driver,
        VCS_ACTION,
        |t| !t.is_empty(),
        "what initializing will do",
    )
    .await?;
    for expected in [
        "Create a repository",
        "2 current file(s)",
        "ignore rules",
        "Nothing is pushed",
    ] {
        if !action.contains(expected) {
            bail!("the form says {action:?}, which does not say {expected:?}");
        }
    }
    if folder.join(".git").exists() {
        bail!("looking at the folder initialized it");
    }
    let toggle = ui::visible(driver, VCS_TOGGLE).await?;
    if toggle.is_selected().await? {
        bail!("snapshotting a folder that holds files was chosen for the person");
    }
    toggle.click().await?;
    click(driver, PROJECT_SUBMIT, "Create Project").await?;

    let project_id = await_project_for(driver, folder).await?;
    let head = git_output(folder, &["symbolic-ref", "HEAD"])?;
    if head != format!("refs/heads/{GIT_DEFAULT_BRANCH}") {
        bail!("the new repository is on {head}, not the branch Git named ({GIT_DEFAULT_BRANCH})");
    }
    let tree = git_output(folder, &["ls-tree", "-r", "--name-only", "HEAD"])?;
    if tree != ".gitignore\nidea.md" {
        bail!("the first commit holds {tree:?}");
    }
    if !folder.join("scratch/notes.txt").is_file() {
        bail!("an ignored file disappeared");
    }
    let readiness = ui::invoke(
        driver,
        "get_project_readiness",
        json!({ "projectId": project_id }),
    )
    .await?;
    if readiness.get("project_base") != Some(&json!(GIT_DEFAULT_BRANCH))
        || readiness.pointer("/base/source") != Some(&json!("local"))
    {
        bail!("the project does not start tasks from {GIT_DEFAULT_BRANCH}: {readiness}");
    }
    let (task_id, _) = create_task_in(driver, &project_id, "Created project").await?;
    ui::invoke(
        driver,
        "update_task_status",
        json!({ "taskId": task_id, "status": "in_progress" }),
    )
    .await?;
    let task = await_status(driver, &project_id, &task_id, &["human_review", "error"]).await?;
    let base = git_output(folder, &["rev-parse", "HEAD"])?;
    if status_of(&task) != Some("human_review") || task.get("base_commit") != Some(&json!(base)) {
        bail!("the task in the created project did not run from its first commit: {task}");
    }
    Ok(())
}

async fn open_and_set_up(driver: &WebDriver, folder: &Path) -> Result<()> {
    let location = folder.to_str().context("the folder path is not UTF-8")?;
    // What Open Folder sends: the folder as it is.
    let repository = ui::invoke(
        driver,
        "create_repository",
        json!({ "localPath": location, "remoteUrl": Value::Null, "initialize": Value::Null }),
    )
    .await?;
    let repository_id = created_id(repository, "create_repository")?;
    let project = ui::invoke(
        driver,
        "create_project",
        json!({ "name": "Opened Folder", "repositoryId": repository_id, "agentType": "claude_code" }),
    )
    .await?;
    let project_id = created_id(project, "create_project")?;

    // Start is refused with what to do, and does not initialize anything.
    let (task_id, _) = create_task_in(driver, &project_id, "Opened folder").await?;
    ui::invoke(
        driver,
        "update_task_status",
        json!({ "taskId": task_id, "status": "in_progress" }),
    )
    .await?;
    let task = await_status(driver, &project_id, &task_id, &["error", "human_review"]).await?;
    let message = task
        .get("error_message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if status_of(&task) != Some("error") || !message.contains("not under version control") {
        bail!("starting a task without version control was not refused truthfully: {task}");
    }
    if folder.join(".git").exists() || folder.join(".jj").exists() {
        bail!("starting a task initialized version control");
    }

    // Settings > Repository says so, and initializes on request.
    page(
        driver,
        "localStorage.setItem('slashit_current_page', 'settings'); \
         localStorage.setItem('slashit_selected_project', arguments[0]); return true;",
        vec![json!(project_id)],
    )
    .await?;
    driver.refresh().await?;
    ui::assert_frontend_is_real(driver).await?;
    click(driver, SETTINGS_REPOSITORY_TAB, "the Repository settings").await?;
    let blocked = await_text(
        driver,
        REPOSITORY_BLOCKED,
        |t| !t.is_empty(),
        "why tasks cannot run",
    )
    .await?;
    if !blocked.contains("not under version control") {
        bail!("the settings say {blocked:?}");
    }
    let action = await_text(
        driver,
        REPOSITORY_INIT_ACTION,
        |t| !t.is_empty(),
        "what initializing will do",
    )
    .await?;
    if !action.contains("1 current file(s)") {
        bail!("the settings say {action:?}");
    }
    click(driver, REPOSITORY_INIT_GIT, "Initialize with Git").await?;
    let ready = await_text(
        driver,
        REPOSITORY_READY,
        |t| !t.is_empty(),
        "the project ready",
    )
    .await?;
    if !ready.contains(&format!("local branch {GIT_DEFAULT_BRANCH}")) {
        bail!("the settings say {ready:?}");
    }
    ui::visible(driver, REPOSITORY_REMOTE_NONE)
        .await
        .context("no-remote state not explained")?;

    // The task that was refused runs once retried.
    ui::invoke(
        driver,
        "update_task_status",
        json!({ "taskId": task_id, "status": "queue" }),
    )
    .await?;
    let task = await_status(driver, &project_id, &task_id, &["human_review"]).await?;
    let base = git_output(
        folder,
        &["rev-parse", &format!("refs/heads/{GIT_DEFAULT_BRANCH}")],
    )?;
    if task.get("base_commit") != Some(&json!(base)) {
        bail!("the retried task did not start at the first commit: {task}");
    }
    Ok(())
}

/// Wait for the project whose repository is `folder`, and return its id.
async fn await_project_for(driver: &WebDriver, folder: &Path) -> Result<String> {
    let started = Instant::now();
    loop {
        let repositories = ui::invoke(driver, "list_repositories", json!({})).await?;
        let repository_id = repositories.as_array().and_then(|all| {
            all.iter()
                .find(|r| r.get("local_path").and_then(Value::as_str) == folder.to_str())
                .and_then(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
        });
        if let Some(repository_id) = repository_id {
            let projects = ui::invoke(driver, "list_projects", json!({})).await?;
            if let Some(id) = projects.as_array().and_then(|all| {
                all.iter()
                    .find(|p| {
                        p.get("repository_id").and_then(Value::as_str)
                            == Some(repository_id.as_str())
                    })
                    .and_then(|p| p.get("id").and_then(Value::as_str).map(str::to_string))
            }) {
                return Ok(id);
            }
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("no project was created for {}", folder.display());
        }
        tokio::time::sleep(POLL).await;
    }
}

/// `git <args>` in `dir`, its trimmed stdout.
fn git_output(dir: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .with_context(|| format!("could not run git {args:?}"))?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
