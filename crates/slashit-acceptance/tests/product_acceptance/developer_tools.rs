//! A review tool the developer installed never reaches the application.
//!
//! The product runs CodeRabbit during AI Review when `coderabbit` is on its
//! `PATH`. Hosted CI has none; a developer machine often does. This journey
//! puts a stand-in for one ahead of everything else on the `PATH` the
//! application would otherwise inherit, runs a task through AI Review and
//! Human Review, and proves the stand-in never ran -- while the fake agent,
//! `git` and the rest of the journey behave exactly as they do in CI, and the
//! developer's own shell still finds the tool afterwards.

use super::local_first::run_and_approve_locally;
use super::*;

/// Where the stand-in writes when it runs: next to itself.
const MARKER: &str = "coderabbit-invoked";

#[tokio::test(flavor = "multi_thread")]
async fn a_developer_installed_coderabbit_is_never_run() {
    let context = TestContext::new("developer_coderabbit").expect("harness setup");
    let outcome = developer_coderabbit_journey(&context).await;
    context.finish(outcome);
}

async fn developer_coderabbit_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = super::human_review::install_fakes(context, &root)?;
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);

    // A developer's own bin directory, first on PATH, holding CodeRabbit.
    let developer = root.join("developer-bin");
    std::fs::create_dir_all(&developer)?;
    let tool = developer.join("coderabbit");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("coderabbit");
    std::os::unix::fs::symlink(&fixture, &tool)
        .with_context(|| format!("could not link {}", tool.display()))?;
    let marker = developer.join(MARKER);

    // The stand-in really does leave evidence when it runs; otherwise its
    // absence below would prove nothing.
    let direct = std::process::Command::new(&tool).arg("--version").output()?;
    if !direct.status.success() || !marker.is_file() {
        bail!("the stand-in did not record a direct run ({})", direct.status);
    }
    std::fs::remove_file(&marker)?;

    let mut path = developer.clone().into_os_string();
    path.push(":");
    path.push(agent.path_value());
    if shell_resolves_coderabbit(&path)? != tool {
        bail!("a shell with the developer's PATH does not find the stand-in first");
    }
    // The product finds CodeRabbit with `which`; without it every lookup
    // fails whatever the PATH, and this journey would prove nothing.
    if resolve_on_path_via_shell("which", &path)?.is_none() {
        bail!("no `which` on PATH, so the product could never find coderabbit either way");
    }
    let harness_path = std::env::var_os("PATH");
    context.set_child_env("PATH", &path);

    let repository = GitFixture::create_local_only(&root.join("fixture-repo"))?;
    let base = repository
        .branch_tip(super::local_first::LOCAL_BRANCH)?
        .context("the fixture has no base commit")?;

    let session = context.start_session("developer-coderabbit").await?;
    let outcome = async {
        // With CodeRabbit turned off the product never looks, and the marker
        // would stay absent for the wrong reason.
        let config = ui::invoke(session.driver(), "get_queue_config", json!({})).await?;
        if config.get("use_coderabbit") != Some(&json!(true)) {
            bail!("CodeRabbit is not enabled in the queue configuration: {config}");
        }
        run_and_approve_locally(session.driver(), &agent, &repository, &base).await
    }
    .await;
    context
        .close_session(session, "developer-coderabbit", &outcome)
        .await?;
    outcome?;

    if marker.exists() {
        bail!(
            "the application ran the developer's coderabbit: {}",
            std::fs::read_to_string(&marker).unwrap_or_default()
        );
    }
    // AI Review really ran, with the fake agent as its reviewer.
    assert_agent_roles(&agent, "developer_coderabbit_journey", 1, 1, 0)?;
    if !gh.invocations()?.is_empty() {
        bail!("gh was run for a project with no remote: {:?}", gh.invocations()?);
    }

    // Nothing outside the application changed: the tool is still installed,
    // a shell with the same PATH still finds it, and the harness's own PATH
    // is what it was.
    if !tool.is_file() || shell_resolves_coderabbit(&path)? != tool {
        bail!("isolation touched the developer's installation");
    }
    if std::env::var_os("PATH") != harness_path {
        bail!("isolation changed the harness's own PATH");
    }
    Ok(())
}

/// What an ordinary `sh` with `path` runs for `name`, if anything.
fn resolve_on_path_via_shell(name: &str, path: &OsStr) -> Result<Option<PathBuf>> {
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", &format!("command -v {name}")])
        .env("PATH", path)
        .output()
        .context("could not run /bin/sh")?;
    Ok(output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())))
}

/// What an ordinary `sh` with `path` runs for `coderabbit`.
fn shell_resolves_coderabbit(path: &OsStr) -> Result<PathBuf> {
    resolve_on_path_via_shell("coderabbit", path)?
        .context("a shell with this PATH does not find coderabbit")
}
