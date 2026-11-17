// SPDX-License-Identifier: MPL-2.0
//! The Issues page: browse GitHub Issues by state, keep a listing across a
//! visit elsewhere, keep the selected Issue's title in view, link an existing
//! Task, refuse a second Task for a linked Issue, open that Task on its board,
//! and recover from a failed listing.
//!
//! The fake `gh` answers `issue list` and `issue view` from files this journey
//! scripts, so what the page shows is what the product asked GitHub for. The
//! journey checks those asks. Nothing reaches GitHub.

use super::human_review::{click, install_fakes};
use super::*;
use slashit_acceptance::fake_gh::FakeGh;
use thirtyfour::components::SelectElement;

const REPOSITORY: &str = "slashit-acceptance/fixture";
const MOVED_REPOSITORY: &str = "slashit-acceptance/moved";
const REPOSITORY_INPUT: &str = "#issues-repo";
const ROW: &str = "[data-testid=\"issue-row\"]";
const COUNT: &str = "[data-testid=\"issues-count\"]";
const REFRESH: &str = "[data-testid=\"issues-refresh\"]";
const LOAD: &str = "form button[type=\"submit\"]";
const ALERT: &str = "[role=\"alert\"]";
const DETAIL: &str = "[data-testid=\"issue-detail\"]";
const DETAIL_BODY: &str = "[data-testid=\"issue-detail-body\"]";
const LINKED: &str = "[data-testid=\"issue-linked-tasks\"]";
const OPEN_TASK: &str = "[data-testid=\"issue-open-task\"]";
const DRAWER: &str = "[data-testid=\"task-drawer\"]";
const TASK_SELECT: &str = "[data-testid=\"issue-task-select\"]";
const MODE_EXISTING: &str = "[data-testid=\"issue-mode-existing\"]";
const MODE_NEW: &str = "[data-testid=\"issue-mode-new\"]";
const LINK_EXISTING: &str = "[data-testid=\"issue-link-existing\"]";
const CREATE: &str = "[data-testid=\"issue-create-task\"]";
const NOTICE: &str = "[data-testid=\"issue-notice\"]";

/// The window size the owner reviewed, and a smaller desktop size.
const REVIEWED_WINDOW: (u32, u32) = (1280, 800);
const SMALL_WINDOW: (u32, u32) = (1024, 700);

#[tokio::test(flavor = "multi_thread")]
async fn the_issues_page_filters_keeps_its_listing_and_links_or_creates_once() {
    let context = TestContext::new("issues_page").expect("harness setup");
    let outcome = issues_journey(&context).await;
    context.finish(outcome);
}

async fn issues_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (_agent, gh) = install_fakes(context, &root)?;
    script_issues(&gh)?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("issues-page").await?;
    let outcome = browse_link_create(session.driver(), &gh, &repository).await;
    context
        .close_session(session, "issues-page", &outcome)
        .await?;
    outcome
}

/// What GitHub answers: two open Issues, one closed, and the full view of each
/// that a Task reads. Issue 7 has Markdown with a code block and a link. Issue
/// 9 was transferred: GitHub answers for it at another repository.
fn script_issues(gh: &FakeGh) -> Result<()> {
    let dir = gh.dir();
    let url = |n: u32| format!("https://github.com/{REPOSITORY}/issues/{n}");
    let open_seven = json!({
        "number": 7, "url": url(7), "title": "Crash on empty input",
        "body": "Steps:\n\n```\nrun --empty\n```\n\nSee [the report](https://example.com/report).",
        "state": "OPEN", "labels": [{"name": "bug", "color": "d73a4a"}],
        "assignees": [], "comments": [],
        "createdAt": "2026-11-01T09:30:00Z", "updatedAt": "2026-11-15T18:45:00Z"
    });
    let open_eight = json!({
        "number": 8, "url": url(8), "title": "Add dark theme", "body": "Plain text.",
        "state": "OPEN", "labels": [{"name": "enhancement", "color": "a2eeef"}],
        "assignees": [], "comments": [],
        "createdAt": "2026-11-02T09:30:00Z", "updatedAt": "2026-11-03T09:30:00Z"
    });
    let closed_three = json!({
        "number": 3, "url": url(3), "title": "Old typo in README", "body": "Closed long ago.",
        "state": "CLOSED", "labels": [{"name": "bug", "color": "d73a4a"}],
        "assignees": [], "comments": [],
        "createdAt": "2026-10-01T09:30:00Z", "updatedAt": "2026-10-05T09:30:00Z"
    });
    let transferred_nine = json!({
        "number": 9, "url": format!("https://github.com/{MOVED_REPOSITORY}/issues/9"),
        "title": "Moved elsewhere", "body": "", "state": "OPEN", "labels": [],
        "assignees": [], "comments": [],
        "createdAt": "2026-11-03T09:30:00Z", "updatedAt": "2026-11-03T09:30:00Z"
    });
    std::fs::write(dir.join("issue-list-open.json"), json!([open_seven, open_eight]).to_string())?;
    std::fs::write(dir.join("issue-list-closed.json"), json!([closed_three]).to_string())?;
    std::fs::write(dir.join("issue-view-7.json"), open_seven.to_string())?;
    std::fs::write(dir.join("issue-view-3.json"), closed_three.to_string())?;
    std::fs::write(dir.join("issue-view-9.json"), transferred_nine.to_string())?;
    Ok(())
}

async fn browse_link_create(driver: &WebDriver, gh: &FakeGh, repository: &GitFixture) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let Prerequisites { project_id, task_id, title } =
        create_prerequisites(driver, repository, "Issues page", "Never runs.").await?;

    // The Project was created behind the window's back, so reload it: the window
    // lists Projects only as it mounts. This is also what a person would see after a restart.
    driver.refresh().await.context("could not reload the application window")?;
    ui::assert_frontend_is_real(driver).await?;
    let (width, height) = REVIEWED_WINDOW;
    driver
        .set_window_rect(0, 0, width, height)
        .await
        .context("could not size the window")?;

    // Open Issues, load the repository: the open listing is what GitHub is asked for.
    ui::visible(driver, "[data-testid=\"nav-github_issues\"]").await?.click().await?;
    // Enter in the repository field submits the page's own form, as a person does.
    ui::visible(driver, REPOSITORY_INPUT)
        .await?
        .send_keys(format!("{REPOSITORY}\u{E007}"))
        .await?;
    await_count(driver, ROW, 2).await?;
    await_text(driver, COUNT, "2 open Issues").await?;
    if list_asks(gh, "open")? != 1 {
        bail!("loading the page should ask GitHub once for open Issues: {:?}", gh.invocations()?);
    }
    screenshot(driver, "issues-open.png").await?;

    // Closed is its own query: GitHub is asked for closed Issues, and only those are shown.
    click(driver, "[data-testid=\"issues-filter-closed\"]", "the Closed filter").await?;
    await_count(driver, ROW, 1).await?;
    await_text(driver, COUNT, "1 closed Issues").await?;
    if list_asks(gh, "closed")? != 1 {
        bail!("the Closed filter did not ask GitHub for closed Issues: {:?}", gh.invocations()?);
    }

    // Back to Open is answered from the session's listing, without a new ask.
    click(driver, "[data-testid=\"issues-filter-open\"]", "the Open filter").await?;
    await_count(driver, ROW, 2).await?;
    if list_asks(gh, "open")? != 1 {
        bail!("a filter already loaded was asked of GitHub again");
    }

    // Refresh asks GitHub again for the selected state.
    click(driver, REFRESH, "the Refresh button").await?;
    let started = Instant::now();
    while list_asks(gh, "open")? < 2 {
        if started.elapsed() > RENDER_DEADLINE {
            bail!("Refresh did not ask GitHub for open Issues again");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Leave for another page and come back: the listing is shown at once, from
    // the cache, and GitHub is not asked again.
    ui::visible(driver, "[data-testid=\"nav-agent\"]").await?.click().await?;
    ui::visible(driver, "[data-testid=\"nav-github_issues\"]").await?.click().await?;
    await_count(driver, ROW, 2).await?;
    if list_asks(gh, "open")? != 2 {
        bail!("returning to the Issues page asked GitHub again: {:?}", gh.invocations()?);
    }

    // The title of each selected Issue is on screen at once, whatever the
    // detail panel was scrolled to: the header is not part of the scrolling body.
    click_row(driver, 0, "the first Issue").await?;
    ui::visible(driver, DETAIL).await?;
    await_title_in_view(driver, "Crash on empty input").await?;
    scroll_detail_body(driver, 200).await?;
    click_row(driver, 1, "the second Issue").await?;
    await_title_in_view(driver, "Add dark theme").await?;
    scroll_detail_body(driver, 200).await?;
    click_row(driver, 0, "the first Issue").await?;
    await_title_in_view(driver, "Crash on empty input").await?;

    // A filter change clears the selection; the next Issue's title is in view too.
    click(driver, "[data-testid=\"issues-filter-closed\"]", "the Closed filter").await?;
    await_count(driver, ROW, 1).await?;
    click_row(driver, 0, "the closed Issue").await?;
    await_title_in_view(driver, "Old typo in README").await?;
    click(driver, "[data-testid=\"issues-filter-open\"]", "the Open filter").await?;
    await_count(driver, ROW, 2).await?;
    click_row(driver, 0, "the first Issue").await?;
    await_title_in_view(driver, "Crash on empty input").await?;

    // The smaller desktop size: the same title stays in view after scrolling.
    let (width, height) = SMALL_WINDOW;
    driver
        .set_window_rect(0, 0, width, height)
        .await
        .context("could not resize the window to the smaller desktop size")?;
    await_title_in_view(driver, "Crash on empty input").await?;
    scroll_detail_body(driver, 200).await?;
    await_title_in_view(driver, "Crash on empty input").await?;
    let (width, height) = REVIEWED_WINDOW;
    driver
        .set_window_rect(0, 0, width, height)
        .await
        .context("could not restore the reviewed window size")?;
    await_title_in_view(driver, "Crash on empty input").await?;

    // Link the existing Task to Issue 7, without creating anything. The
    // linking area belongs to the selected Issue; its Project starts unchosen.
    let project_option = format!("[data-testid=\"issue-project-select\"] option[value=\"{project_id}\"]");
    await_count(driver, &project_option, 1).await?;
    let project_select = driver
        .find(By::Css("[data-testid=\"issue-project-select\"]"))
        .await
        .context("the Project choice is not shown")?;
    SelectElement::new(&project_select).await?.select_by_value(&project_id).await?;

    click(driver, MODE_EXISTING, "the link-to-an-existing-Task choice").await?;
    let select = driver
        .find(By::Css(TASK_SELECT))
        .await
        .context("the Task choice is not shown")?;
    SelectElement::new(&select).await?.select_by_value(&task_id).await?;
    click(driver, LINK_EXISTING, "the Link to the existing Task button").await?;
    await_text_containing(driver, LINKED, &title).await?;
    expect_task_count(driver, &project_id, 1).await?;
    // The linked Task is shown with its action, and creating a second Task is not offered.
    await_count(driver, OPEN_TASK, 1).await?;
    await_count(driver, MODE_NEW, 0).await?;
    screenshot(driver, "issues-linked.png").await?;

    // The backend refuses the same Issue again, both as a new Task and as a second link.
    let refusal = ui::invoke_expecting_refusal(
        driver,
        "create_task_from_issue",
        json!({ "repo": REPOSITORY, "issueNumber": 7, "projectId": project_id }),
    )
    .await?;
    if !refusal.contains("already linked") {
        bail!("the backend refusal does not say the Issue is linked: {refusal:?}");
    }
    let refusal = ui::invoke_expecting_refusal(
        driver,
        "link_github_issue",
        json!({ "taskId": task_id, "issueUrl": format!("https://github.com/{REPOSITORY}/issues/7") }),
    )
    .await?;
    if !refusal.contains("already links") {
        bail!("the backend refusal does not say the Task links the Issue: {refusal:?}");
    }
    expect_task_count(driver, &project_id, 1).await?;

    // A transferred Issue is refused, naming its new address, and no Task is written.
    let refusal = ui::invoke_expecting_refusal(
        driver,
        "create_task_from_issue",
        json!({ "repo": REPOSITORY, "issueNumber": 9, "projectId": project_id }),
    )
    .await?;
    let moved_address = format!("https://github.com/{MOVED_REPOSITORY}/issues/9");
    if !refusal.contains(&moved_address) || !refusal.contains("No Task was created") {
        bail!("the transferred Issue was not refused with its new address: {refusal:?}");
    }
    expect_task_count(driver, &project_id, 1).await?;

    // Issue 3 is closed and unlinked: creating a Task from it works once.
    click(driver, "[data-testid=\"issues-filter-closed\"]", "the Closed filter").await?;
    await_count(driver, ROW, 1).await?;
    click_row(driver, 0, "the closed Issue").await?;
    await_title_in_view(driver, "Old typo in README").await?;
    click(driver, MODE_NEW, "the create-a-new-Task choice").await?;
    click(driver, CREATE, "the Create Task button").await?;
    await_text_containing(driver, NOTICE, "Nothing was started.").await?;
    expect_task_count(driver, &project_id, 2).await?;
    screenshot(driver, "issues-created.png").await?;

    // A failed listing: the loading state ends, the error is shown, the listing
    // already on screen stays, no Task changes, and Refresh recovers.
    std::fs::write(gh.dir().join("fail-issue-list-closed"), "")?;
    click(driver, REFRESH, "the Refresh button").await?;
    await_text_containing(driver, ALERT, "503").await?;
    await_text(driver, LOAD, "Load Issues").await?;
    await_count(driver, ROW, 1).await?;
    expect_task_count(driver, &project_id, 2).await?;
    screenshot(driver, "issues-listing-failed.png").await?;
    std::fs::remove_file(gh.dir().join("fail-issue-list-closed"))?;
    click(driver, REFRESH, "the Refresh button").await?;
    await_count(driver, ALERT, 0).await?;
    await_count(driver, ROW, 1).await?;
    // The alert clears when Refresh starts, before the fake `gh` records its call.
    let started = Instant::now();
    while list_asks(gh, "closed")? < 3 {
        if started.elapsed() > RENDER_DEADLINE {
            bail!("Refresh after the failure did not ask GitHub again: {:?}", gh.invocations()?);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Open the linked Task from its Issue: its board opens the Task's drawer.
    ui::visible(driver, "[data-testid=\"nav-agent\"]").await?.click().await?;
    ui::visible(driver, "[data-testid=\"nav-github_issues\"]").await?.click().await?;
    await_count(driver, ROW, 2).await?;
    click_row(driver, 0, "the first Issue").await?;
    await_text_containing(driver, LINKED, &title).await?;
    click(driver, OPEN_TASK, "the Open Task button").await?;
    await_text_containing(driver, DRAWER, &title).await?;
    screenshot(driver, "issues-open-task.png").await?;

    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    let created = listed
        .as_array()
        .context("list_tasks did not answer with a list")?
        .iter()
        .find(|t| t["title"] == "Old typo in README")
        .context("the Task created from Issue 3 is not listed")?;
    if status_of(created) != Some("backlog") {
        bail!("the created Task is not in the backlog: {created}");
    }
    let linked_to_three = created["external_refs"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|r| r["number"] == json!(3) && r["repo"] == json!(REPOSITORY));
    if !linked_to_three {
        bail!("the created Task does not link Issue 3: {created}");
    }

    open_task_in_other_project(driver, &project_id).await
}

/// A Task in another Project is linked from the Issues page and opened there.
/// The page switches the selected Project and the drawer opens on that board.
async fn open_task_in_other_project(driver: &WebDriver, project_id: &str) -> Result<()> {
    let listed = ui::invoke(driver, "list_projects", json!({})).await?;
    let repository_id = listed
        .as_array()
        .context("list_projects did not answer with a list")?
        .iter()
        .find(|p| p["id"] == json!(project_id))
        .and_then(|p| p["repository_id"].as_str())
        .context("the first Project has no repository")?
        .to_string();
    let other_project = created_id(
        ui::invoke(
            driver,
            "create_project",
            json!({ "name": "Acceptance Other Project", "repositoryId": repository_id, "agentType": "claude_code" }),
        )
        .await?,
        "create_project",
    )?;
    let title = "Task in the other Project";
    let other_task = created_id(
        ui::invoke(
            driver,
            "create_task",
            json!({ "params": {
                "projectId": other_project,
                "title": title,
                "description": "Never runs.",
                "model": "default",
                "reviewerModel": "sonnet",
                "planningMode": false,
                "dependencies": [],
                "category": Value::Null,
                "priority": Value::Null,
                "complexity": Value::Null,
                "impact": Value::Null,
                "securitySeverity": Value::Null,
                "githubIssueUrl": Value::Null,
                "gitlabIssueUrl": Value::Null,
                "linearTicketId": Value::Null,
            } }),
        )
        .await?,
        "create_task",
    )?;

    // The drawer of the first Project's Task covers the navigation; close it first.
    click(driver, "[data-testid=\"task-drawer-close\"]", "the Task drawer's close button").await?;
    await_count(driver, DRAWER, 0).await?;

    // Back on the Issues page the first Project is selected. Choose the other
    // Project as the destination, then link its Task to Issue 7.
    ui::visible(driver, "[data-testid=\"nav-agent\"]").await?.click().await?;
    ui::visible(driver, "[data-testid=\"nav-github_issues\"]").await?.click().await?;
    await_count(driver, ROW, 2).await?;
    click_row(driver, 0, "the first Issue").await?;
    let other_option = format!("[data-testid=\"issue-project-select\"] option[value=\"{other_project}\"]");
    await_count(driver, &other_option, 1).await?;
    let project_select = driver
        .find(By::Css("[data-testid=\"issue-project-select\"]"))
        .await
        .context("the Project choice is not shown")?;
    SelectElement::new(&project_select).await?.select_by_value(&other_project).await?;
    click(driver, MODE_EXISTING, "the link-to-an-existing-Task choice").await?;
    let task_option = format!("{TASK_SELECT} option[value=\"{other_task}\"]");
    await_count(driver, &task_option, 1).await?;
    let select = driver
        .find(By::Css(TASK_SELECT))
        .await
        .context("the Task choice is not shown")?;
    SelectElement::new(&select).await?.select_by_value(&other_task).await?;
    click(driver, LINK_EXISTING, "the Link to the existing Task button").await?;
    await_text_containing(driver, LINKED, title).await?;
    expect_task_count(driver, &other_project, 1).await?;

    // The other Project's board opens the drawer of its Task.
    click(driver, OPEN_TASK, "the Open Task button").await?;
    await_text_containing(driver, DRAWER, title).await?;
    screenshot(driver, "issues-open-task-other-project.png").await?;
    Ok(())
}

/// Click the Issue row at `index` in the list, as a person would.
async fn click_row(driver: &WebDriver, index: usize, what: &str) -> Result<()> {
    let rows = driver
        .find_all(By::Css(ROW))
        .await
        .with_context(|| format!("could not list the Issue rows for {what}"))?;
    let row = rows
        .get(index)
        .with_context(|| format!("there is no row {index} for {what}"))?;
    row.click().await.with_context(|| format!("could not click {what}"))
}

/// Scroll the selected Issue's body, the part below its header, down by `pixels`.
async fn scroll_detail_body(driver: &WebDriver, pixels: u32) -> Result<()> {
    let script = format!(
        "const body = document.querySelector('{DETAIL_BODY}'); if (body) {{ body.scrollTop = {pixels}; }} return !!body;"
    );
    let found = driver.execute(&script, vec![]).await?.json().clone();
    if found != json!(true) {
        bail!("the Issue body is not shown, so it cannot be scrolled");
    }
    Ok(())
}

/// Wait until the selected Issue's title is the one named and lies fully
/// inside the detail panel and the window.
async fn await_title_in_view(driver: &WebDriver, title: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let shown = text_of(driver, "[data-testid=\"issue-detail\"] h2").await?.unwrap_or_default();
        if shown.trim() == title && title_in_view(driver).await? {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the title {title:?} is not fully in view (the title reads {shown:?})");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn title_in_view(driver: &WebDriver) -> Result<bool> {
    let script = format!(
        "const panel = document.querySelector('{DETAIL}');
         const title = panel && panel.querySelector('h2');
         if (!panel || !title) {{ return false; }}
         const p = panel.getBoundingClientRect();
         const t = title.getBoundingClientRect();
         return t.top >= p.top && t.bottom <= p.bottom && t.top >= 0 && t.bottom <= window.innerHeight;"
    );
    let value = driver.execute(&script, vec![]).await?.json().clone();
    Ok(value == json!(true))
}

/// Save a screenshot when `SLASHIT_ACCEPTANCE_SCREENSHOTS` names a directory,
/// so a run can be inspected by eye. Without it, nothing is written.
async fn screenshot(driver: &WebDriver, name: &str) -> Result<()> {
    let Some(dir) = std::env::var_os("SLASHIT_ACCEPTANCE_SCREENSHOTS") else {
        return Ok(());
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir)?;
    driver
        .screenshot(&dir.join(name))
        .await
        .with_context(|| format!("could not save the screenshot {name}"))
}

/// How many `gh issue list --state <state>` calls the product has made.
fn list_asks(gh: &FakeGh, state: &str) -> Result<usize> {
    Ok(gh
        .invocations()?
        .iter()
        .filter(|args| {
            args.len() >= 2
                && args[0] == "issue"
                && args[1] == "list"
                && args.windows(2).any(|pair| pair[0] == "--state" && pair[1] == state)
        })
        .count())
}

async fn expect_task_count(driver: &WebDriver, project_id: &str, expected: usize) -> Result<()> {
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    let found = listed.as_array().map_or(0, Vec::len);
    if found != expected {
        bail!("the Project holds {found} Task(s), expected {expected}");
    }
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
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn await_text(driver: &WebDriver, selector: &str, expected: &str) -> Result<()> {
    await_text_where(driver, selector, |text| text.trim() == expected).await
}

async fn await_text_containing(driver: &WebDriver, selector: &str, expected: &str) -> Result<()> {
    await_text_where(driver, selector, |text| text.contains(expected)).await
}

async fn await_text_where(
    driver: &WebDriver,
    selector: &str,
    accept: impl Fn(&str) -> bool,
) -> Result<()> {
    let started = Instant::now();
    loop {
        let text = text_of(driver, selector).await?.unwrap_or_default();
        if accept(&text) {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("{selector} reads {text:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
