// SPDX-License-Identifier: MPL-2.0
//! Organize Projects: rename a Project's displayed name and remove a Project
//! from SlashIt, then bring the removed Project back by opening its folder.
//!
//! Renaming changes only the name SlashIt shows. The folder is not touched.
//! Removal is refused, with its reason, while a Task is queued, and succeeds
//! once the queue is clear. A removed Project leaves the rail and every active
//! listing, and the backend refuses new work for it. Opening its folder again
//! returns the same Project with its id, name and Tasks. Renames and removals
//! survive a restart.
//!
//! The journey drives the real rail, organizer and keyboard. Only the folders
//! are created by the harness; everything else goes through the application.

use super::*;
use std::fs;

const RAIL_ORGANIZE: &str = "[data-testid=\"rail-organize-projects\"]";
const ORGANIZER: &str = "[data-testid=\"project-organizer\"]";
const ORGANIZER_ROW: &str = "[data-testid=\"project-organizer-row\"]";
const ORGANIZER_NAME: &str = "[data-testid=\"project-organizer-name\"]";
const ORGANIZER_SAVE: &str = "[data-testid=\"project-organizer-save\"]";
const ORGANIZER_REMOVE: &str = "[data-testid=\"project-organizer-remove\"]";
const ORGANIZER_CONFIRM: &str = "[data-testid=\"project-organizer-confirm-remove\"]";
const ORGANIZER_ERROR: &str = "[data-testid=\"project-organizer-error\"]";
const WEBDRIVER_ESCAPE: &str = "\u{E00C}";
/// Focusable, inside the dialog: the Escape key is sent to it and bubbles up.
const ORGANIZER_CLOSE: &str = "[data-testid=\"project-organizer\"] button[aria-label=\"Close\"]";

fn rail_item(project_id: &str) -> String {
    format!("[data-testid=\"rail-project-{project_id}\"]")
}

/// Opens a folder the way the Open Folder card does: a repository for the
/// folder, then a Project on it.
async fn open_folder(driver: &WebDriver, folder: &Path, name: &str) -> Result<String> {
    let location = folder.to_str().context("the folder path is not UTF-8")?;
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
        json!({ "name": name, "repositoryId": repository_id, "agentType": "claude_code" }),
    )
    .await?;
    created_id(project, "create_project")
}

async fn create_task(driver: &WebDriver, project_id: &str, title: &str) -> Result<String> {
    created_id(
        ui::invoke(
            driver,
            "create_task",
            json!({ "params": {
                "projectId": project_id,
                "title": title,
                "description": "Part of the Organize Projects journey.",
                "model": "default",
                "planningMode": false,
                "dependencies": [],
            } }),
        )
        .await?,
        "create_task",
    )
}

/// The rail item's accessible name, which carries the Project's name.
async fn rail_label(driver: &WebDriver, project_id: &str) -> Result<Option<String>> {
    let value = page(
        driver,
        &format!(
            "const item = document.querySelector('{}'); return item ? item.getAttribute('aria-label') : null;",
            rail_item(project_id)
        ),
        vec![],
    )
    .await?;
    Ok(value.as_str().map(str::to_owned))
}

async fn rail_has(driver: &WebDriver, project_id: &str) -> Result<bool> {
    Ok(page(
        driver,
        &format!("return !!document.querySelector('{}');", rail_item(project_id)),
        vec![],
    )
    .await?
        == Value::Bool(true))
}

async fn rail_is_selected(driver: &WebDriver, project_id: &str) -> Result<bool> {
    Ok(page(
        driver,
        &format!(
            "const item = document.querySelector('{}'); return !!item && item.classList.contains('active');",
            rail_item(project_id)
        ),
        vec![],
    )
    .await?
        == Value::Bool(true))
}

async fn wait_until<F, Fut>(what: &str, mut check: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let started = Instant::now();
    loop {
        if check().await? {
            return Ok(());
        }
        if started.elapsed() > ui::ELEMENT_TIMEOUT {
            bail!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Clicks the rail item for a Project with the pointer, then leaves it, so the
/// rail collapses the way it does for a person.
async fn select_project(driver: &WebDriver, project_id: &str) -> Result<()> {
    let item = ui::visible(driver, &rail_item(project_id)).await?;
    item.click().await.context("could not select the Project in the rail")?;
    wait_until("the selected Project to be marked active", || rail_is_selected(driver, project_id)).await
}

async fn open_organizer(driver: &WebDriver) -> Result<()> {
    ui::visible(driver, RAIL_ORGANIZE).await?.click().await.context("could not open Organize Projects")?;
    ui::visible(driver, ORGANIZER).await?;
    Ok(())
}

/// The names currently shown by the organizer, in row order.
async fn organizer_names(driver: &WebDriver) -> Result<Vec<String>> {
    let value = page(
        driver,
        &format!(
            "return [...document.querySelectorAll('{ORGANIZER_ROW}')].map(r => r.querySelector('{ORGANIZER_NAME}').value);"
        ),
        vec![],
    )
    .await?;
    Ok(value
        .as_array()
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default())
}

/// The organizer row whose name field shows `name`.
async fn organizer_row(driver: &WebDriver, name: &str) -> Result<WebElement> {
    let names = organizer_names(driver).await?;
    let index = names
        .iter()
        .position(|n| n == name)
        .with_context(|| format!("the organizer does not list {name:?}; it lists {names:?}"))?;
    let rows = driver.find_all(By::Css(ORGANIZER_ROW)).await?;
    rows.into_iter()
        .nth(index)
        .context("the organizer rows changed while it was being read")
}

async fn organizer_has_name(driver: &WebDriver, name: &str) -> Result<bool> {
    Ok(organizer_names(driver).await?.iter().any(|n| n == name))
}

/// Types a new name into a row and saves it with the row's own button.
async fn rename_in_organizer(driver: &WebDriver, current: &str, new_name: &str) -> Result<()> {
    let row = organizer_row(driver, current).await?;
    let input = row.find(By::Css(ORGANIZER_NAME)).await?;
    input.clear().await?;
    input.send_keys(new_name).await.context("could not type the new name")?;
    row.find(By::Css(ORGANIZER_SAVE))
        .await?
        .click()
        .await
        .context("could not press Save name")?;
    wait_until(&format!("the organizer to show {new_name:?}"), || organizer_has_name(driver, new_name)).await
}

#[tokio::test(flavor = "multi_thread")]
async fn organizing_projects_renames_removes_and_reopens_them_without_losing_data() {
    let context = TestContext::new("project_removal").expect("harness setup");
    let outcome = journey(&context).await;
    context.finish(outcome);
}

async fn journey(context: &TestContext) -> Result<()> {
    let folders = context.state().path().join("folders");
    let alpha_folder = folders.join("alpha");
    let beta_folder = folders.join("beta");
    for folder in [&alpha_folder, &beta_folder] {
        fs::create_dir_all(folder).context("could not create the fixture folder")?;
        fs::write(folder.join("README.md"), format!("{}\n", folder.display()))
            .context("could not write the fixture file")?;
    }
    let beta_readme_before = fs::read(beta_folder.join("README.md"))?;

    // Session one: set up, rename, refuse a removal, then remove and reopen.
    let session = context.start_session("organize").await?;
    let artifacts = context.artifacts_dir().to_path_buf();
    let outcome = organize(session.driver(), &alpha_folder, &beta_folder, &beta_readme_before, &artifacts).await;
    context.close_session(session, "organize", &outcome).await?;
    let (alpha_id, beta_id) = outcome?;

    // Session two: the same state, a new application process.
    let session = context.start_session("restarted").await?;
    let outcome = after_restart(session.driver(), &alpha_folder, &beta_folder, &alpha_id, &beta_id).await;
    context.close_session(session, "restarted", &outcome).await?;
    outcome
}

/// Returns the ids of Alpha and Beta, which later sessions must find again.
async fn organize(
    driver: &WebDriver,
    alpha_folder: &Path,
    beta_folder: &Path,
    beta_readme_before: &[u8],
    artifacts: &Path,
) -> Result<(String, String)> {
    let alpha_id = open_folder(driver, alpha_folder, "Alpha").await?;
    let beta_id = open_folder(driver, beta_folder, "Beta").await?;

    // Alpha holds a queued Task, so removing it must be refused. Beta holds a
    // Backlog Task, which must survive its removal.
    ui::invoke(driver, "update_queue_config", json!({ "parallelTaskLimit": 1, "autoPromote": false })).await?;
    // Open Alpha's Conversation now, so there is one to keep through the removal.
    ui::invoke(driver, "open_project_conversation", json!({ "projectId": alpha_id })).await?;
    let alpha_task = create_task(driver, &alpha_id, "Queued in Alpha").await?;
    ui::invoke(driver, "update_task_status", json!({ "taskId": alpha_task, "status": "queue" })).await?;
    let beta_task = create_task(driver, &beta_id, "Backlog in Beta").await?;

    driver.refresh().await.context("could not reload the application window")?;
    ui::assert_frontend_is_real(driver).await?;
    wait_until("both Projects in the rail", || async {
        Ok(rail_has(driver, &alpha_id).await? && rail_has(driver, &beta_id).await?)
    })
    .await?;

    // Rename Beta with the keyboard and the mouse. The name shows in the rail
    // at once, and the folder keeps its files.
    open_organizer(driver).await?;
    driver.screenshot(&artifacts.join("organizer-open.png")).await.context("could not capture the organizer")?;
    rename_in_organizer(driver, "Beta", "Beta Renamed").await?;
    wait_until("the rail to show the new name", || async {
        Ok(rail_label(driver, &beta_id).await?.as_deref() == Some("Beta Renamed"))
    })
    .await?;
    if fs::read(beta_folder.join("README.md"))? != beta_readme_before {
        bail!("renaming the Project changed a file in its folder");
    }

    // Removing Alpha is refused while its Task is queued, and the refusal says why.
    organizer_row(driver, "Alpha").await?.find(By::Css(ORGANIZER_REMOVE)).await?.click().await?;
    organizer_row(driver, "Alpha").await?.find(By::Css(ORGANIZER_CONFIRM)).await?.click().await?;
    let refusal = ui::visible(driver, ORGANIZER_ERROR).await?.text().await?;
    if !refusal.contains("in Queue") || !refusal.contains("Nothing was changed") {
        bail!("the refusal did not say why removal is blocked: {refusal:?}");
    }
    driver.screenshot(&artifacts.join("organizer-refusal.png")).await.context("could not capture the refusal")?;
    if !organizer_has_name(driver, "Alpha").await? || !rail_has(driver, &alpha_id).await? {
        bail!("a refused removal still took Alpha out of the organizer or the rail");
    }

    // The Escape key closes the organizer, as the dialog promises.
    ui::visible(driver, ORGANIZER_CLOSE).await?.send_keys(WEBDRIVER_ESCAPE).await?;
    wait_until("the organizer to close on Escape", || async {
        Ok(page(driver, &format!("return !document.querySelector('{ORGANIZER}');"), vec![]).await? == Value::Bool(true))
    })
    .await?;

    // Clear the queue, select Alpha, and remove it. Its Tasks stay, and the
    // selection moves to the first Project still active.
    ui::invoke(driver, "update_task_status", json!({ "taskId": alpha_task, "status": "backlog" })).await?;
    select_project(driver, &alpha_id).await?;
    open_organizer(driver).await?;
    organizer_row(driver, "Alpha").await?.find(By::Css(ORGANIZER_REMOVE)).await?.click().await?;
    organizer_row(driver, "Alpha").await?.find(By::Css(ORGANIZER_CONFIRM)).await?.click().await?;
    wait_until("Alpha to leave the organizer", || async { Ok(!organizer_has_name(driver, "Alpha").await?) }).await?;
    wait_until("Alpha to leave the rail", || async { Ok(!rail_has(driver, &alpha_id).await?) }).await?;
    wait_until("the selection to move to Beta", || rail_is_selected(driver, &beta_id)).await?;
    ui::visible(driver, ORGANIZER_CLOSE).await?.send_keys(WEBDRIVER_ESCAPE).await?;

    // A removed Project starts no work, and the backend says so.
    let listed = ui::invoke(driver, "list_projects", json!({})).await?;
    let names: Vec<String> = listed
        .as_array()
        .map(|items| items.iter().filter_map(|p| p["name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    if names.contains(&"Alpha".to_string()) {
        bail!("a removed Project is still listed as active: {names:?}");
    }
    let refused = ui::invoke_expecting_refusal(
        driver,
        "create_task",
        json!({ "params": {
            "projectId": alpha_id, "title": "Must not start", "description": "",
            "model": "default", "planningMode": false, "dependencies": [],
        } }),
    )
    .await?;
    if !refused.contains("removed") {
        bail!("creating a Task in a removed Project was refused without saying why: {refused}");
    }
    let conversation = ui::invoke_expecting_refusal(
        driver,
        "get_project_conversation",
        json!({ "projectId": alpha_id }),
    )
    .await?;
    if !conversation.contains("removed") {
        bail!("a removed Project's Conversation was not refused: {conversation}");
    }
    let renamed = ui::invoke_expecting_refusal(
        driver,
        "update_project",
        json!({ "id": alpha_id, "name": "Changed while removed", "repositoryId": Value::Null, "agentType": Value::Null }),
    )
    .await?;
    if !renamed.contains("removed") {
        bail!("renaming a removed Project was not refused: {renamed}");
    }

    // Beta is untouched apart from its name: its Backlog Task is still there.
    let beta_tasks = ui::invoke(driver, "list_tasks", json!({ "projectId": beta_id })).await?;
    if !beta_tasks.to_string().contains(&beta_task) {
        bail!("Beta lost its Backlog Task when its neighbour was removed");
    }

    Ok((alpha_id, beta_id))
}

async fn after_restart(
    driver: &WebDriver,
    alpha_folder: &Path,
    beta_folder: &Path,
    alpha_id: &str,
    beta_id: &str,
) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    wait_until("the restarted rail to load", || async { rail_has(driver, beta_id).await }).await?;
    if rail_has(driver, alpha_id).await? {
        bail!("the removal did not survive the restart: Alpha is back in the rail");
    }
    if rail_label(driver, beta_id).await?.as_deref() != Some("Beta Renamed") {
        bail!("the rename did not survive the restart");
    }

    // Opening the folder again returns the same Project, not a copy.
    let reopened = open_folder(driver, alpha_folder, "Alpha again").await?;
    if reopened != alpha_id {
        bail!("opening the removed folder created a new Project {reopened} instead of reopening {alpha_id}");
    }
    let listed = ui::invoke(driver, "list_projects", json!({})).await?;
    let count = listed.as_array().map(Vec::len).unwrap_or(0);
    if count != 2 {
        bail!("reopening left {count} active Projects; expected Alpha and Beta");
    }
    let alpha = ui::invoke(driver, "get_project", json!({ "id": alpha_id })).await?;
    if alpha["name"] != "Alpha" {
        bail!("reopening changed Alpha's name to {}", alpha["name"]);
    }
    let alpha_tasks = ui::invoke(driver, "list_tasks", json!({ "projectId": alpha_id })).await?;
    if !alpha_tasks.to_string().contains("Queued in Alpha") {
        bail!("reopening Alpha lost its Task");
    }
    let conversation = ui::invoke(driver, "get_project_conversation", json!({ "projectId": alpha_id })).await?;
    if conversation.is_null() {
        bail!("reopening Alpha lost its Conversation");
    }

    driver.refresh().await.context("could not reload the application window")?;
    wait_until("Alpha back in the rail after reopening", || async { rail_has(driver, alpha_id).await }).await?;
    if fs::read(alpha_folder.join("README.md")).is_err() || fs::read(beta_folder.join("README.md")).is_err() {
        bail!("a folder lost its files during removal or reopening");
    }
    Ok(())
}

const WEBDRIVER_TAB: &str = "\u{E004}";
/// The first button of the removal confirmation, which is Cancel.
const ORGANIZER_CANCEL: &str = "[data-testid=\"project-organizer-confirm\"] button";

/// Where keyboard focus is: `inside` the Organizer, `body`, or `outside:<element>`.
async fn focus_location(driver: &WebDriver) -> Result<String> {
    let value = page(
        driver,
        "const a = document.activeElement; \
         const d = document.querySelector('[data-testid=\"project-organizer\"]'); \
         if (!a || a === document.body) return 'body'; \
         if (d && d.contains(a)) return 'inside'; \
         return 'outside:' + a.tagName + ' ' + (a.getAttribute('data-testid') || a.getAttribute('aria-label') || '');",
        vec![],
    )
    .await?;
    Ok(value.as_str().unwrap_or_default().to_string())
}

async fn organizer_is_open(driver: &WebDriver) -> Result<bool> {
    Ok(page(driver, "return !!document.querySelector('[data-testid=\"project-organizer\"]');", vec![]).await?
        == Value::Bool(true))
}

async fn rail_organize_has_focus(driver: &WebDriver) -> Result<bool> {
    Ok(page(
        driver,
        "return document.activeElement === document.querySelector('[data-testid=\"rail-organize-projects\"]');",
        vec![],
    )
    .await?
        == Value::Bool(true))
}

#[tokio::test(flavor = "multi_thread")]
async fn organizer_keeps_keyboard_focus_inside_while_open() {
    let context = TestContext::new("project_organizer_focus").expect("harness setup");
    let folder = context.state().path().join("folders").join("gamma");
    let outcome = async {
        fs::create_dir_all(&folder).context("could not create the fixture folder")?;
        fs::write(folder.join("README.md"), "gamma\n").context("could not write the fixture file")?;
        let session = context.start_session("focus").await?;
        let outcome = organizer_focus(session.driver(), &folder).await;
        context.close_session(session, "focus", &outcome).await?;
        outcome
    }
    .await;
    context.finish(outcome);
}

/// Focus enters the Organizer on open, Tab never leaves it, Escape and the Close button
/// close it, and focus returns to the Organize Projects control that opened it.
async fn organizer_focus(driver: &WebDriver, folder: &Path) -> Result<()> {
    open_folder(driver, folder, "Gamma").await?;
    driver.refresh().await.context("could not reload the application window")?;

    ui::visible(driver, RAIL_ORGANIZE).await?.click().await.context("could not open Organize Projects")?;
    ui::visible(driver, ORGANIZER).await?;
    wait_until("keyboard focus to enter the Organizer", || async {
        Ok(focus_location(driver).await? == "inside")
    })
    .await?;

    // Forty Tab presses cover the dialog several times over; any press that leaves it fails.
    for step in 0..40 {
        driver.active_element().await?.send_keys(WEBDRIVER_TAB).await.context("could not press Tab")?;
        let location = focus_location(driver).await?;
        if location != "inside" {
            bail!("Tab press {step} moved keyboard focus out of the open Organizer to {location}");
        }
    }

    // Cancelling a removal removes its buttons; focus must stay inside the dialog.
    ui::visible(driver, ORGANIZER_REMOVE).await?.click().await.context("could not press Remove")?;
    ui::visible(driver, ORGANIZER_CANCEL).await?.click().await.context("could not press Cancel")?;
    let location = focus_location(driver).await?;
    if location != "inside" {
        bail!("cancelling a removal left keyboard focus at {location}, outside the Organizer");
    }

    driver.active_element().await?.send_keys(WEBDRIVER_ESCAPE).await.context("could not press Escape")?;
    wait_until("Escape to close the Organizer", || async { Ok(!organizer_is_open(driver).await?) }).await?;
    wait_until("focus to return to Organize Projects after Escape", || rail_organize_has_focus(driver)).await?;

    ui::visible(driver, RAIL_ORGANIZE).await?.click().await.context("could not reopen Organize Projects")?;
    ui::visible(driver, ORGANIZER_CLOSE).await?.click().await.context("could not press Close")?;
    wait_until("the Close button to close the Organizer", || async { Ok(!organizer_is_open(driver).await?) }).await?;
    wait_until("focus to return to Organize Projects after Close", || rail_organize_has_focus(driver)).await?;
    Ok(())
}
