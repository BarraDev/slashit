// SPDX-License-Identifier: MPL-2.0
//! A GitHub Issue linked to a Task, through the drawer: link it, see it, open
//! it, unlink it, reload, and refuse what GitHub does not confirm.
//!
//! The fake `gh` answers `issue view`, the check the product makes against
//! GitHub. A stand-in `xdg-open` first on the application's `PATH` records the
//! address the product opens, so the opened URL is observed, not assumed.
//! Nothing reaches GitHub, and the journey fails if a worker runs or a pull
//! request is opened.

use super::human_review::{click, install_fakes};
use super::*;
use slashit_acceptance::fake_agent::FakeAgent;
use slashit_acceptance::fake_gh::FakeGh;

const ISSUE_URL: &str = "https://github.com/slashit-acceptance/fixture/issues/7";
const REFUSED_URL: &str = "https://github.com/slashit-acceptance/fixture/issues/9";
const INPUT: &str = "[data-testid=\"task-drawer-issue-input\"]";
const LINK: &str = "[data-testid=\"task-drawer-issue-link\"]";
const SECTION: &str = "[data-testid=\"task-drawer-issues\"]";
const ROW: &str = "[data-testid=\"task-drawer-issue\"]";
const ROW_STATE: &str = "[data-testid=\"task-drawer-issue-state\"]";
const OPEN: &str = "[data-testid=\"task-drawer-issue-open\"]";
const UNLINK: &str = "[data-testid=\"task-drawer-issue-unlink\"]";
const ERROR: &str = "[data-testid=\"task-drawer-issue-error\"]";

#[tokio::test(flavor = "multi_thread")]
async fn a_github_issue_links_opens_and_unlinks_from_a_task_without_starting_work() {
    let context = TestContext::new("github_issue_link").expect("harness setup");
    let outcome = issue_link_journey(&context).await;
    context.finish(outcome);
}

async fn issue_link_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = install_fakes(context, &root)?;
    let opened = install_url_opener(context, &root, &agent)?;
    // GitHub answers for issue 9 with a pull request's address. The product
    // must refuse that answer.
    std::fs::write(
        gh.dir().join("issue-view-9.json"),
        r#"{"number":9,"state":"OPEN","url":"https://github.com/slashit-acceptance/fixture/pull/9"}"#,
    )?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("github-issue-link").await?;
    let outcome = link_open_unlink(session.driver(), &gh, &agent, &opened, &repository).await;
    context
        .close_session(session, "github-issue-link", &outcome)
        .await?;
    outcome
}

/// A stand-in `xdg-open`, first on the application's `PATH`. It appends each
/// address it is given to a log, which is what the product's open action
/// reaches on Linux.
fn install_url_opener(context: &TestContext, root: &Path, agent: &FakeAgent) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("opener-bin");
    std::fs::create_dir_all(&bin)?;
    let log = root.join("opened-urls.txt");
    let script = bin.join("xdg-open");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\n", log.display()),
    )?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(agent.path_value())),
    )?;
    context.set_child_env("PATH", path);
    Ok(log)
}

async fn link_open_unlink(
    driver: &WebDriver,
    gh: &FakeGh,
    agent: &FakeAgent,
    opened: &Path,
    repository: &GitFixture,
) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let Prerequisites {
        project_id,
        task_id,
        title,
    } = create_prerequisites(driver, repository, "Issue link", "Never runs.").await?;
    open_board(driver, &project_id).await?;
    open_drawer(driver, &task_id, &title).await?;
    ui::visible(driver, SECTION)
        .await
        .context("the drawer shows no GitHub Issues section")?;

    // Text that is not an address is refused, and nothing is stored.
    type_address(driver, "not an issue").await?;
    click(driver, LINK, "the Link issue button").await?;
    ui::visible(driver, ERROR)
        .await
        .context("the invalid address was not refused")?;
    expect_issues(driver, &project_id, &task_id, &[]).await?;

    // An answer that names a pull request, not the Issue, is refused.
    type_address(driver, REFUSED_URL).await?;
    click(driver, LINK, "the Link issue button").await?;
    await_error_naming(driver, "#9").await?;
    expect_issues(driver, &project_id, &task_id, &[]).await?;

    // A valid Issue is linked, shown in the drawer, and stored.
    type_address(driver, ISSUE_URL).await?;
    click(driver, LINK, "the Link issue button").await?;
    await_count(driver, ROW, 1).await?;
    let label = text_of(driver, ROW).await?.unwrap_or_default();
    if !label.contains("slashit-acceptance/fixture #7") {
        bail!("the linked Issue row reads {label:?}, not the Issue it was linked to");
    }
    let state = text_of(driver, ROW_STATE).await?.unwrap_or_default();
    if state != "Open" {
        bail!("the linked Issue reads as {state:?}, not Open");
    }
    expect_issues(driver, &project_id, &task_id, &[(ISSUE_URL, 7, "OPEN")]).await?;

    // Open goes to the Issue's own address.
    click(driver, OPEN, "the Open button of the linked Issue").await?;
    await_opened(opened, ISSUE_URL).await?;

    // Unlink removes it from the drawer and from the record.
    click(driver, UNLINK, "the Unlink button of the linked Issue").await?;
    await_count(driver, ROW, 0).await?;
    expect_issues(driver, &project_id, &task_id, &[]).await?;

    // After a reload, nothing restores the Issue.
    driver
        .refresh()
        .await
        .context("reload the application window")?;
    ui::assert_frontend_is_real(driver).await?;
    open_board(driver, &project_id).await?;
    open_drawer(driver, &task_id, &title).await?;
    ui::visible(driver, SECTION).await?;
    expect_issues(driver, &project_id, &task_id, &[]).await?;
    await_count(driver, ROW, 0).await?;

    // Linking reads GitHub and changes nothing else.
    let invocations = gh.invocations()?;
    if !invocations
        .iter()
        .any(|args| invoked(args, &["issue", "view", "7"]))
    {
        bail!("the product never asked GitHub about issue 7: {invocations:?}");
    }
    if invocations
        .iter()
        .any(|args| invoked(args, &["pr", "create"]))
    {
        bail!("a pull request was opened by linking an Issue");
    }
    // The application probes `claude --version` for its status bar; that is
    // not a run. Any other invocation of the agent is one.
    let runs = agent
        .invocations()?
        .into_iter()
        .filter(|run| !run.has_flag("--version"))
        .count();
    if runs != 0 {
        bail!("linking an Issue started {runs} agent run(s)");
    }
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    let task = find_task(&listed, &task_id).context("the product no longer lists the task")?;
    if status_of(&task) != Some("backlog") {
        bail!("the task left the backlog: {:?}", status_of(&task));
    }
    Ok(())
}

/// Whether `args` begins with `expected`, the way gh was invoked.
fn invoked(args: &[String], expected: &[&str]) -> bool {
    args.len() >= expected.len() && args.iter().zip(expected).all(|(arg, want)| arg == want)
}

async fn type_address(driver: &WebDriver, text: &str) -> Result<()> {
    let field = ui::visible(driver, INPUT)
        .await
        .context("the Issue address field is not shown")?;
    field.clear().await?;
    field.send_keys(text).await?;
    Ok(())
}

async fn await_count(driver: &WebDriver, selector: &str, expected: usize) -> Result<()> {
    let started = Instant::now();
    loop {
        let found = count(driver, selector).await?;
        if found == expected {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("{selector}: expected {expected} on screen, found {found}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn await_error_naming(driver: &WebDriver, needle: &str) -> Result<String> {
    let started = Instant::now();
    loop {
        if let Some(text) = text_of(driver, ERROR).await? {
            if text.contains(needle) {
                return Ok(text);
            }
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("no refusal naming {needle} was shown");
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Wait for the opener's log to name `url`, then require it to name nothing else.
async fn await_opened(log: &Path, url: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let opened = std::fs::read_to_string(log).unwrap_or_default();
        if opened.lines().any(|line| line == url) {
            let all: Vec<&str> = opened.lines().collect();
            if all != [url] {
                bail!("the product opened {all:?}, not only {url}");
            }
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the product did not open {url}");
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The GitHub Issue references the backend holds for the task, compared as
/// `(url, number, state)`, in stored order.
async fn expect_issues(
    driver: &WebDriver,
    project_id: &str,
    task_id: &str,
    expected: &[(&str, u64, &str)],
) -> Result<()> {
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    let task = find_task(&listed, task_id).context("the product no longer lists the task")?;
    let stored: Vec<(String, u64, String)> = task
        .get("external_refs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r.get("ref_type").and_then(Value::as_str) == Some("github_issue"))
        .map(|r| {
            (
                r.get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                r.get("number").and_then(Value::as_u64).unwrap_or_default(),
                r.get("state")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect();
    let wanted: Vec<(String, u64, String)> = expected
        .iter()
        .map(|(url, number, state)| (url.to_string(), *number, state.to_string()))
        .collect();
    if stored != wanted {
        bail!("the stored Issue references are {stored:?}, expected {wanted:?}");
    }
    Ok(())
}
