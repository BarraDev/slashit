// SPDX-License-Identifier: MPL-2.0
//! The Project Rail after a selection. The rail expands while the pointer is
//! over it and collapses once the pointer leaves, including after a switch
//! from one selected Project to another.
//!
//! The pointer movement matters. A single WebDriver jump straight from a
//! just-clicked rail item to the board produced no `mousemove` in the WebView,
//! so the rail stayed expanded. A real pointer always reports motion, and a
//! motion over the item before leaving reproduces that. The journey therefore
//! moves within the item before it leaves the rail, as a hand does.

use super::*;

const MAIN: &str = "main";
const RAIL: &str = "[data-testid=\"project-rail\"]";

fn rail_item(project_id: &str) -> String {
    format!("[data-testid=\"rail-project-{project_id}\"]")
}

async fn create_project(driver: &WebDriver, name: &str) -> Result<String> {
    created_id(ui::invoke(driver, "create_project", json!({
        "name": name, "repositoryId": Value::Null, "agentType": "claude_code"
    })).await?, "create_project")
}

/// Whether the rail's root element is in the expanded state right now.
async fn rail_expanded(driver: &WebDriver) -> Result<bool> {
    Ok(page(driver, &format!("const rail = document.querySelector('{RAIL}'); return !!rail && rail.classList.contains('expanded');"), vec![]).await? == Value::Bool(true))
}

/// Polls until the rail's expanded state equals `expected`, or fails with `message`.
async fn await_expanded(driver: &WebDriver, expected: bool, message: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        if rail_expanded(driver).await? == expected {
            return Ok(());
        }
        if started.elapsed() > ui::ELEMENT_TIMEOUT {
            bail!("{message}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Selects a Project in the rail with the pointer, moves within the item as a
/// hand does, then moves the pointer onto the board. The rail must collapse.
async fn select_and_leave(driver: &WebDriver, project_id: &str) -> Result<()> {
    let item = ui::visible(driver, &rail_item(project_id)).await?;
    item.click().await.context("could not select the project in the rail")?;
    await_expanded(driver, true, "the rail did not expand under the pointer").await?;
    let selected = page(driver, &format!("const item = document.querySelector('{}'); return !!item && item.classList.contains('active');", rail_item(project_id)), vec![]).await?;
    if selected != Value::Bool(true) {
        bail!("the Project is not selected after the click");
    }

    driver.action_chain().move_to_element_center(&item).move_by_offset(3, 3).perform().await
        .context("could not move the pointer within the project rail")?;

    let main = ui::visible(driver, MAIN).await?;
    driver.action_chain().move_to_element_center(&main).perform().await
        .context("could not move the pointer off the project rail")?;
    // The collapse takes 0.2 s. A rail still expanded after the settle delay is the defect.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    if rail_expanded(driver).await? {
        bail!("the rail stayed expanded after the pointer left it, following a Project selection");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn selecting_a_project_collapses_the_rail_once_the_pointer_leaves_it() {
    let context = TestContext::new("project_rail_collapse").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-collapse").await?;
        let driver = session.driver();
        let alpha = create_project(driver, "Alpha Rail").await?;
        let beta = create_project(driver, "Beta Rail").await?;
        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;

        // The first selection, then a switch to the other Project.
        select_and_leave(driver, &alpha).await?;
        select_and_leave(driver, &beta).await?;

        context.close_session(session, "rail-collapse", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

const RAIL_ORGANIZE: &str = "[data-testid=\"rail-organize-projects\"]";
const WORKSPACE_NAME: &str = "[data-testid=\"workspace-name-input\"]";
const WORKSPACE_CREATE: &str = "[data-testid=\"workspace-create\"]";
const ORGANIZER_CLOSE: &str = "[data-testid=\"project-organizer\"] button[aria-label=\"Close\"]";
const WEBDRIVER_ESCAPE: &str = "\u{E00C}";
const RAIL_ADD: &str = "[data-testid=\"rail-add-project\"]";

/// Geometry and state of a Workspace's select control in the rail, found by its
/// accessible name. Null when the rail has no such control.
async fn rail_workspace(driver: &WebDriver, name: &str) -> Result<Value> {
    page(
        driver,
        &format!(
            "const el = [...document.querySelectorAll('[data-testid^=\"rail-workspace-select-\"]')] \
             .find(e => e.getAttribute('aria-label') === 'Workspace {name}'); \
             if (!el) return null; \
             const r = el.getBoundingClientRect(); \
             const badge = el.querySelector('.rail-badge').getBoundingClientRect(); \
             const rail = document.querySelector('[data-testid=\"project-rail\"]').getBoundingClientRect(); \
             return {{ width: r.width, height: r.height, badgeWidth: badge.width, badgeHeight: badge.height, \
                       badgeLeft: badge.left, badgeRight: badge.right, railRight: rail.right, \
                       active: el.classList.contains('active') }};"
        ),
        vec![],
    )
    .await
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

/// A Workspace is created in Organize Projects, which has no Projects yet. The
/// dialog closes and the rail must still list the Workspace, visibly and
/// selectably, before any member Project exists.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_without_projects_stays_visible_in_the_rail() {
    let context = TestContext::new("project_rail_workspace").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-workspace").await?;
        let driver = session.driver();
        ui::assert_frontend_is_real(driver).await?;

        ui::visible(driver, RAIL_ORGANIZE).await?.click().await.context("could not open Organize Projects")?;
        ui::visible(driver, WORKSPACE_NAME).await?.send_keys("Field Workspace").await?;
        ui::visible(driver, WORKSPACE_CREATE).await?.click().await.context("could not press Create workspace")?;
        wait_until("the new Workspace to appear in the rail", || async {
            Ok(rail_workspace(driver, "Field Workspace").await? != Value::Null)
        })
        .await?;
        // The folder is the managed one SlashIt created, never "no folder".
        wait_until("the Workspace folder to be shown", || async {
            Ok(page(
                driver,
                "const el = document.querySelector('[data-testid=\"workspace-root-label\"]'); \
                 return !!el && el.innerText.includes('/workspaces/');",
                vec![],
            )
            .await?
                == Value::Bool(true))
        })
        .await?;
        ui::visible(driver, ORGANIZER_CLOSE).await?.send_keys(WEBDRIVER_ESCAPE).await?;
        let main = ui::visible(driver, MAIN).await?;
        driver.action_chain().move_to_element_center(&main).perform().await?;
        tokio::time::sleep(Duration::from_millis(1500)).await;

        let state = rail_workspace(driver, "Field Workspace").await?;
        driver.screenshot(&context.artifacts_dir().join("rail-empty-workspace.png")).await?;
        if state["width"].as_f64().unwrap_or(0.0) <= 0.0 || state["badgeHeight"].as_f64().unwrap_or(0.0) <= 0.0 {
            bail!("the empty Workspace is in the rail but not visible: {state}");
        }
        if state["badgeRight"].as_f64().unwrap_or(f64::MAX) > state["railRight"].as_f64().unwrap_or(0.0) {
            bail!("the Workspace badge is clipped by the collapsed rail: {state}");
        }

        ui::visible(driver, "[data-testid^=\"rail-workspace-select-\"]").await?.click().await?;
        wait_until("the Workspace to become the active selection", || async {
            Ok(rail_workspace(driver, "Field Workspace").await?["active"] == Value::Bool(true))
        })
        .await?;
        ui::visible(driver, "[data-testid=\"workspace-home\"]").await?;

        context.close_session(session, "rail-workspace", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

/// A Recent Repository whose folder is already open as a Project is refused
/// from Get Started: the refusal is shown, the dialog stays open, and no second
/// Project for the folder appears.
#[tokio::test(flavor = "multi_thread")]
async fn a_recent_folder_that_is_already_open_is_refused_without_a_second_project() {
    let context = TestContext::new("project_rail_duplicate").expect("harness setup");

    let outcome = async {
        let folder = context.state().path().join("folders").join("alpha");
        std::fs::create_dir_all(&folder).context("could not create the fixture folder")?;
        let location = folder.to_str().context("the folder path is not UTF-8")?.to_string();

        let session = context.start_session("rail-duplicate").await?;
        let driver = session.driver();
        let repository = ui::invoke(driver, "create_repository", json!({
            "localPath": location, "remoteUrl": Value::Null, "initialize": Value::Null
        }))
        .await?;
        let repository_id = created_id(repository, "create_repository")?;
        created_id(ui::invoke(driver, "create_project", json!({
            "name": "Alpha", "repositoryId": repository_id, "agentType": "claude_code"
        }))
        .await?, "create_project")?;

        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;
        ui::visible(driver, RAIL_ADD).await?.click().await.context("could not open Get Started")?;
        ui::visible(driver, "[data-testid=\"wizard-create-project\"]").await?;

        let tagged = page(
            driver,
            "const row = [...document.querySelectorAll('button')].find(b => b.textContent.includes(arguments[0])); \
             if (!row) return false; row.setAttribute('data-acceptance-recent', '1'); return true;",
            vec![json!(location)],
        )
        .await?;
        if tagged != Value::Bool(true) {
            bail!("the Recent Repository for the open folder is not listed in Get Started");
        }
        ui::visible(driver, "[data-acceptance-recent=\"1\"]").await?
            .click()
            .await
            .context("could not press the Recent Repository")?;
        wait_until("the refusal to be shown", || async {
            Ok(page(
                driver,
                "return document.body.innerText.includes(\"already open as Project 'Alpha'\");",
                vec![],
            )
            .await?
                == Value::Bool(true))
        })
        .await?;

        let listed = ui::invoke(driver, "list_projects", json!({})).await?;
        let count = listed.as_array().map_or(0, |items| items.len());
        if count != 1 {
            bail!("the refused open still left {count} Projects for one folder");
        }
        context.close_session(session, "rail-duplicate", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

/// The sidebar entry marked as the active view, as its `nav-*` test id, or null.
/// The active entry is the one with the `border-l-2` accent class, which both the
/// normal and the stub variant carry.
async fn active_view(driver: &WebDriver) -> Result<Value> {
    page(
        driver,
        "const active = [...document.querySelectorAll('[data-testid^=\"nav-\"]')] \
         .find(b => b.className.split(' ').includes('border-l-2')); \
         return active ? active.dataset.testid : null;",
        vec![],
    )
    .await
}

/// The chevron of a Workspace group hides and shows its member Projects, and
/// the Workspace's own control stays separate from it. Neither the Workspace
/// control nor the chevron changes the active view.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_chevron_collapses_and_expands_its_member_projects() {
    let context = TestContext::new("project_rail_group").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-group").await?;
        let driver = session.driver();
        let alpha = create_project(driver, "Alpha Group").await?;
        let beta = create_project(driver, "Beta Group").await?;
        let workspace = created_id(
            ui::invoke(driver, "create_workspace", json!({ "name": "Grouped", "rootPath": Value::Null })).await?,
            "create_workspace",
        )?;
        for project in [&alpha, &beta] {
            ui::invoke(driver, "attach_project_to_workspace", json!({ "projectId": project, "workspaceId": workspace })).await?;
        }

        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;
        let members = format!(
            "return document.querySelectorAll('[data-testid=\"rail-workspace-{workspace}\"] [data-testid^=\"rail-project-\"]').length;"
        );
        wait_until("both members under the Workspace", || async {
            Ok(page(driver, &members, vec![]).await?.as_u64() == Some(2))
        })
        .await?;
        let view_before = active_view(driver).await?;
        if !view_before.is_string() {
            bail!("no sidebar view is marked active before the chevron is used: {view_before}");
        }

        let toggle = format!("[data-testid=\"rail-workspace-toggle-{workspace}\"]");
        ui::visible(driver, &toggle).await?.click().await.context("could not collapse the Workspace")?;
        wait_until("the chevron to hide the members", || async {
            Ok(page(driver, &members, vec![]).await?.as_u64() == Some(0))
        })
        .await?;
        let view_collapsed = active_view(driver).await?;
        if view_collapsed != view_before {
            bail!("collapsing the Workspace changed the active view from {view_before} to {view_collapsed}");
        }

        ui::visible(driver, &toggle).await?.click().await.context("could not expand the Workspace")?;
        wait_until("the chevron to show the members again", || async {
            Ok(page(driver, &members, vec![]).await?.as_u64() == Some(2))
        })
        .await?;
        let view_expanded = active_view(driver).await?;
        if view_expanded != view_before {
            bail!("expanding the Workspace changed the active view from {view_before} to {view_expanded}");
        }

        context.close_session(session, "rail-group", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

/// Clicking a Workspace's name in the rail while Settings is open leaves
/// Settings for that Workspace's home, and its exit returns to Projects.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_name_in_the_rail_opens_its_home_from_settings_and_exits() {
    let context = TestContext::new("project_rail_settings_home").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-settings-home").await?;
        let driver = session.driver();
        let workspace = created_id(
            ui::invoke(driver, "create_workspace", json!({ "name": "Settings Home", "rootPath": Value::Null })).await?,
            "create_workspace",
        )?;
        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;

        ui::visible(driver, "[data-testid=\"nav-settings\"]").await?.click().await.context("could not open Settings")?;
        ui::visible(driver, &format!("[data-testid=\"rail-workspace-select-{workspace}\"]"))
            .await?
            .click()
            .await
            .context("could not click the Workspace in the rail")?;
        wait_until("Workspace Home to open for the clicked Workspace", || async {
            Ok(page(
                driver,
                "const el = document.querySelector('[data-testid=\"workspace-home-name\"]'); \
                 return !!el && el.textContent.trim() === 'Settings Home';",
                vec![],
            )
            .await?
                == Value::Bool(true))
        })
        .await?;

        ui::visible(driver, "[data-testid=\"workspace-home-exit\"]")
            .await?
            .click()
            .await
            .context("could not press Back to Projects")?;
        wait_until("Workspace Home to close after the exit", || async {
            Ok(page(driver, "return !document.querySelector('[data-testid=\"workspace-home\"]');", vec![]).await?
                == Value::Bool(true))
        })
        .await?;

        context.close_session(session, "rail-settings-home", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

/// The organizer's focused name field keeps keyboard focus while the rail
/// reloads and shows the rename. This proves the organizer field's focus only.
/// It does not check focus on a rail element.
#[tokio::test(flavor = "multi_thread")]
async fn organizer_name_keeps_focus_while_the_rail_shows_the_rename() {
    let context = TestContext::new("project_rail_focus").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-focus").await?;
        let driver = session.driver();
        create_project(driver, "Focus Alpha").await?;
        let beta = create_project(driver, "Focus Beta").await?;
        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;

        ui::visible(driver, RAIL_ORGANIZE).await?.click().await.context("could not open Organize Projects")?;
        ui::visible(driver, "[data-testid=\"project-organizer\"]").await?;
        page(
            driver,
            "const row = [...document.querySelectorAll('[data-testid=\"project-organizer-row\"]')] \
             .find(r => r.querySelector('[data-testid=\"project-organizer-name\"]').value === 'Focus Beta'); \
             if (!row) return false; \
             row.querySelector('[data-testid=\"project-organizer-name\"]').dataset.focusProbe = '1'; return true;",
            vec![],
        )
        .await?;
        let name = ui::visible(driver, "[data-testid=\"project-organizer-row\"] [data-testid=\"project-organizer-name\"][data-focus-probe=\"1\"]").await?;
        name.clear().await.context("could not clear the name field")?;
        name.send_keys("Focus Beta Renamed").await.context("could not type the new name")?;
        if page(driver, "return document.activeElement?.dataset.focusProbe === '1';", vec![]).await? != Value::Bool(true) {
            bail!("the name field did not hold keyboard focus before the rail update");
        }

        // Save with a script click, so the pointer and keyboard focus do not move.
        page(
            driver,
            "const row = [...document.querySelectorAll('[data-testid=\"project-organizer-row\"]')] \
             .find(r => r.querySelector('[data-testid=\"project-organizer-name\"]').dataset.focusProbe === '1'); \
             row.querySelector('[data-testid=\"project-organizer-save\"]').click(); return true;",
            vec![],
        )
        .await?;
        let label = format!("[data-testid=\"rail-project-{beta}\"]");
        wait_until("the rail to show the renamed Project", || async {
            Ok(page(
                driver,
                &format!("const el = document.querySelector('{label}'); return !!el && (el.getAttribute('aria-label') || '').includes('Focus Beta Renamed');"),
                vec![],
            )
            .await?
                == Value::Bool(true))
        })
        .await?;
        if page(driver, "return document.activeElement?.dataset.focusProbe === '1';", vec![]).await? != Value::Bool(true) {
            bail!("keyboard focus left the organizer name field when the rail updated");
        }

        context.close_session(session, "rail-focus", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

/// A member row whose membership another client already removed cannot detach
/// again. The refusal reaches the user, and the other member is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_member_row_refuses_a_second_detach_and_leaves_other_members_alone() {
    let context = TestContext::new("project_rail_stale_detach").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-stale-detach").await?;
        let driver = session.driver();
        let workspace = created_id(
            ui::invoke(driver, "create_workspace", json!({ "name": "Stale Row", "rootPath": Value::Null })).await?,
            "create_workspace",
        )?;
        let alpha = create_project(driver, "Stale Alpha").await?;
        let beta = create_project(driver, "Stale Beta").await?;
        for project in [&alpha, &beta] {
            ui::invoke(driver, "attach_project_to_workspace", json!({ "projectId": project, "workspaceId": workspace })).await?;
        }
        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;

        ui::visible(driver, RAIL_ORGANIZE).await?.click().await.context("could not open Organize Projects")?;
        ui::visible(driver, &format!("[data-testid=\"workspace-{workspace}\"] [data-testid=\"workspace-toggle\"]"))
            .await?
            .click()
            .await
            .context("could not expand the Workspace")?;
        let alpha_row = format!("[data-testid=\"workspace-{workspace}\"] [data-testid=\"workspace-member-{alpha}\"]");
        ui::visible(driver, &alpha_row).await?;

        // Another client detaches Alpha while this page still lists it.
        ui::invoke(
            driver,
            "detach_project_from_workspace",
            json!({ "projectId": alpha, "expectedWorkspaceId": workspace }),
        )
        .await?;
        ui::visible(driver, &format!("{alpha_row} [data-testid=\"workspace-detach\"]"))
            .await?
            .click()
            .await
            .context("could not press Detach on the stale row")?;
        wait_until("the refusal of the stale Detach", || async {
            Ok(page(
                driver,
                "return [...document.querySelectorAll('[data-testid=\"toast\"][data-variant=\"error\"]')] \
                 .some(t => t.textContent.includes('Project is not attached to a workspace'));",
                vec![],
            )
            .await?
                == Value::Bool(true))
        })
        .await?;

        assert_backend_scope(driver, &alpha, None).await?;
        assert_backend_scope(driver, &beta, Some(&workspace)).await?;

        context.close_session(session, "rail-stale-detach", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

const ATTACH_PICKER: &str = "[data-testid=\"workspace-attach\"] button[aria-haspopup=\"listbox\"]";
const ATTACH_SUBMIT: &str = "[data-testid=\"workspace-attach-submit\"]";

fn workspace_select(workspace: &str) -> String {
    format!("[data-testid=\"rail-workspace-select-{workspace}\"]")
}

/// The name Workspace Home shows, or the empty string when it shows none.
async fn home_name(driver: &WebDriver) -> Result<String> {
    Ok(page(
        driver,
        "const el = document.querySelector('[data-testid=\"workspace-home-name\"]'); \
         return el ? el.textContent.trim() : null;",
        vec![],
    )
    .await?
    .as_str()
    .unwrap_or_default()
    .to_string())
}

/// Opens Organize Projects and expands one Workspace in it.
async fn expand_workspace_in_organizer(driver: &WebDriver, workspace: &str) -> Result<()> {
    ui::visible(driver, RAIL_ORGANIZE).await?.click().await.context("could not open Organize Projects")?;
    ui::visible(driver, &format!("[data-testid=\"workspace-{workspace}\"] [data-testid=\"workspace-toggle\"]"))
        .await?
        .click()
        .await
        .context("could not expand the Workspace")?;
    ui::visible(driver, &format!("[data-testid=\"workspace-{workspace}\"] {ATTACH_PICKER}")).await?;
    Ok(())
}

/// Picks a standalone Project in the Attach picker and presses Attach.
async fn attach_from_picker(driver: &WebDriver, project_name: &str) -> Result<()> {
    ui::visible(driver, ATTACH_PICKER).await?.click().await.context("could not open the Attach picker")?;
    wait_until(&format!("the Attach picker to offer {project_name}"), || async {
        Ok(page(
            driver,
            "const option = [...document.querySelectorAll('[role=\"option\"]')] \
             .find(o => o.textContent.includes(arguments[0])); \
             if (!option) return false; option.click(); return true;",
            vec![json!(project_name)],
        )
        .await?
            == Value::Bool(true))
    })
    .await?;
    // Toasts from earlier actions sit over the bottom-right corner, where Attach is.
    dismiss_toasts(driver).await?;
    let submit = ui::visible(driver, ATTACH_SUBMIT).await?;
    if let Err(error) = submit.click().await {
        // Name the element that took the pointer, so the failure says what covers Attach.
        let state = page(driver, ATTACH_DIAGNOSTIC, vec![])
            .await
            .map_or_else(|e| format!("unreadable: {e:#}"), |v| v.to_string());
        bail!("could not press Attach: {error}\nAttach page state: {state}");
    }
    Ok(())
}

/// Dismisses every visible toast through its own dismiss control, as a user would,
/// and waits until none is shown.
async fn dismiss_toasts(driver: &WebDriver) -> Result<()> {
    // Re-query after each click: dismissing one toast re-renders the others, which
    // stales any element found before. The bound stops a toast that never leaves.
    for _ in 0..10 {
        let Some(button) = driver.find_all(By::Css("[data-testid=\"toast-dismiss\"]")).await?.into_iter().next() else {
            break;
        };
        button.click().await.context("could not dismiss a toast")?;
    }
    wait_until("the toasts to clear", || async {
        Ok(page(driver, "return document.querySelectorAll('[data-testid=\"toast\"]').length === 0;", vec![]).await?
            == Value::Bool(true))
    })
    .await
}

/// What covers the Attach button at its centre, and the picker's state.
const ATTACH_DIAGNOSTIC: &str = "\
    const submit = document.querySelector('[data-testid=\"workspace-attach-submit\"]');\
    const picker = document.querySelector('[data-testid=\"workspace-attach\"] button[aria-haspopup=\"listbox\"]');\
    const r = submit ? submit.getBoundingClientRect() : null;\
    const hit = r ? document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2) : null;\
    return {\
        submitDisabled: submit ? submit.disabled : null,\
        submitRect: r ? [r.left, r.top, r.width, r.height] : null,\
        pickerText: picker ? picker.textContent.trim() : null,\
        pickerExpanded: picker ? picker.getAttribute('aria-expanded') : null,\
        listboxOpen: !!document.querySelector('[role=\"listbox\"]'),\
        toasts: [...document.querySelectorAll('[data-testid=\"toast\"]')].map(t => t.textContent.trim().slice(0, 160)),\
        hitTag: hit ? hit.tagName : null,\
        hitTestId: hit ? (hit.closest('[data-testid]') || {}).dataset?.testid || null : null,\
        hitClass: hit ? String(hit.className).slice(0, 160) : null,\
        hitText: hit ? (hit.textContent || '').trim().slice(0, 80) : null,\
        viewport: [window.innerWidth, window.innerHeight],\
    };";

/// Scenario: Workspace Home's own pending Project read (its `list_projects` on
/// load) must not replace the Workspace the user selected afterwards. This is
/// not the organizer's fallback read after a Project removal; that path is not
/// covered here.
#[tokio::test(flavor = "multi_thread")]
async fn a_delayed_project_read_does_not_replace_the_selected_workspace() {
    let context = TestContext::new("project_rail_stale_home").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-stale-home").await?;
        let driver = session.driver();
        let first = created_id(
            ui::invoke(driver, "create_workspace", json!({ "name": "Delayed Alpha", "rootPath": Value::Null })).await?,
            "create_workspace",
        )?;
        let second = created_id(
            ui::invoke(driver, "create_workspace", json!({ "name": "Delayed Beta", "rootPath": Value::Null })).await?,
            "create_workspace",
        )?;
        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;

        // The first Project read the next selection makes is Workspace A's, from its Home load.
        ui::arm_invoke_fault(driver, "list_projects", ui::InvokeFault::Hold).await?;
        ui::visible(driver, &workspace_select(&first)).await?.click().await.context("could not select Delayed Alpha")?;
        wait_until("Workspace A's Project read to be issued", || async {
            Ok(ui::invoke_fault_state(driver).await?.0)
        })
        .await?;

        ui::visible(driver, &workspace_select(&second)).await?.click().await.context("could not select Delayed Beta")?;
        wait_until("Workspace Home to show Delayed Beta", || async {
            Ok(home_name(driver).await? == "Delayed Beta")
        })
        .await?;

        ui::release_invoke_hold(driver).await?;
        wait_until("the held Project read to reach the application", || async {
            Ok(ui::invoke_fault_state(driver).await?.1)
        })
        .await?;
        let shown = home_name(driver).await?;
        if shown != "Delayed Beta" {
            bail!("the late Project read replaced the selected Workspace: Workspace Home shows {shown:?}");
        }

        context.close_session(session, "rail-stale-home", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

/// Scenario: a membership reload that fails leaves no permanent busy state, and
/// the next Attach works once the organizer reads the Projects again.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_membership_reload_clears_busy_and_the_next_attach_works() {
    let context = TestContext::new("project_rail_reload_failure").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-reload-failure").await?;
        let driver = session.driver();
        let workspace = created_id(
            ui::invoke(driver, "create_workspace", json!({ "name": "Reload Failure", "rootPath": Value::Null })).await?,
            "create_workspace",
        )?;
        let alpha = create_project(driver, "Reload Alpha").await?;
        create_project(driver, "Reload Beta").await?;
        create_project(driver, "Reload Gamma").await?;
        ui::invoke(driver, "attach_project_to_workspace", json!({ "projectId": alpha, "workspaceId": workspace })).await?;
        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;

        expand_workspace_in_organizer(driver, &workspace).await?;
        // The next Project read answers with a payload that is not a Project list.
        ui::arm_invoke_fault(driver, "list_projects", ui::InvokeFault::BadPayload(json!("not a project list"))).await?;
        attach_from_picker(driver, "Reload Beta").await?;
        wait_until("the failed membership reload to be issued", || async {
            Ok(ui::invoke_fault_state(driver).await?.0)
        })
        .await?;

        let busy_control = format!("[data-testid=\"workspace-{workspace}\"] [data-testid=\"workspace-rename-save\"]");
        wait_until("the organizer to leave its busy state", || async {
            Ok(page(
                driver,
                "const el = document.querySelector(arguments[0]); return !!el && el.disabled === false;",
                vec![json!(busy_control)],
            )
            .await?
                == Value::Bool(true))
        })
        .await?;

        // The injected reply reached the application. Its error toast is not awaited: toasts expire after 4 seconds.
        wait_until("the injected reply to reach the application", || async {
            Ok(ui::invoke_fault_state(driver).await?.1)
        })
        .await?;

        // The failed read empties the Attach picker, so reopen the organizer to read the Projects again.
        ui::visible(driver, ORGANIZER_CLOSE).await?.click().await.context("could not close the organizer")?;
        expand_workspace_in_organizer(driver, &workspace).await?;
        attach_from_picker(driver, "Reload Gamma").await?;
        ui::visible(driver, &format!("[data-testid=\"workspace-{workspace}\"] [data-testid=\"workspace-member-{alpha}\"]")).await?;
        wait_until("the second Attach to list Reload Gamma", || async {
            Ok(page(
                driver,
                "return [...document.querySelectorAll('[data-testid^=\"workspace-member-\"]')] \
                 .some(r => r.textContent.includes('Reload Gamma'));",
                vec![],
            )
            .await?
                == Value::Bool(true))
        })
        .await?;

        context.close_session(session, "rail-reload-failure", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

/// Scenario: closing the organizer while an Attach is pending does not break the
/// application when the reply arrives afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn closing_the_organizer_while_an_attach_is_pending_leaves_the_app_responsive() {
    let context = TestContext::new("project_rail_close_pending").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-close-pending").await?;
        let driver = session.driver();
        let workspace = created_id(
            ui::invoke(driver, "create_workspace", json!({ "name": "Closing Race", "rootPath": Value::Null })).await?,
            "create_workspace",
        )?;
        let alpha = create_project(driver, "Closing Alpha").await?;
        create_project(driver, "Closing Beta").await?;
        ui::invoke(driver, "attach_project_to_workspace", json!({ "projectId": alpha, "workspaceId": workspace })).await?;
        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;

        expand_workspace_in_organizer(driver, &workspace).await?;
        ui::arm_invoke_fault(driver, "attach_project_to_workspace", ui::InvokeFault::Hold).await?;
        attach_from_picker(driver, "Closing Beta").await?;
        wait_until("the Attach to be issued", || async {
            Ok(ui::invoke_fault_state(driver).await?.0)
        })
        .await?;

        ui::visible(driver, ORGANIZER_CLOSE).await?.click().await.context("could not close the organizer")?;
        wait_until("the organizer to close while the Attach is pending", || async {
            Ok(page(driver, "return !document.querySelector('[data-testid=\"project-organizer\"]');", vec![]).await?
                == Value::Bool(true))
        })
        .await?;

        ui::release_invoke_hold(driver).await?;
        wait_until("the held Attach reply to reach the application", || async {
            Ok(ui::invoke_fault_state(driver).await?.1)
        })
        .await?;

        // The application must still respond: the rail is up, and the organizer opens again.
        ui::visible(driver, RAIL).await?;
        ui::visible(driver, RAIL_ORGANIZE).await?.click().await.context("could not reopen Organize Projects")?;
        ui::visible(driver, "[data-testid=\"project-organizer\"]").await?;

        context.close_session(session, "rail-close-pending", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

/// Scenario: text typed into the Workspace name while its rename is in flight
/// is kept when the reply arrives. The reply confirms the name that was
/// submitted, and it does not replace the newer text.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_name_typed_during_its_rename_survives_the_reply() {
    let context = TestContext::new("project_rail_rename_inflight").expect("harness setup");

    let outcome = async {
        let session = context.start_session("rail-rename-inflight").await?;
        let driver = session.driver();
        let workspace = created_id(
            ui::invoke(driver, "create_workspace", json!({ "name": "Rename Alpha", "rootPath": Value::Null })).await?,
            "create_workspace",
        )?;
        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;

        expand_workspace_in_organizer(driver, &workspace).await?;

        let field = ui::visible(driver, "[data-testid=\"workspace-rename-input\"]").await?;
        field.clear().await.context("could not clear the name field")?;
        field.send_keys("Renamed Once").await.context("could not type the new name")?;

        ui::arm_invoke_fault(driver, "rename_workspace", ui::InvokeFault::Hold).await?;
        ui::visible(driver, "[data-testid=\"workspace-rename-save\"]")
            .await?
            .click()
            .await
            .context("could not save the name")?;
        wait_until("the rename to be issued", || async { Ok(ui::invoke_fault_state(driver).await?.0) }).await?;

        // Typed while the save is still in flight.
        ui::visible(driver, "[data-testid=\"workspace-rename-input\"]")
            .await?
            .send_keys(" and more")
            .await
            .context("could not type while the rename is in flight")?;

        ui::release_invoke_hold(driver).await?;
        wait_until("the held rename reply to reach the application", || async {
            Ok(ui::invoke_fault_state(driver).await?.1)
        })
        .await?;
        wait_until("the rename success to be reported", || async {
            Ok(page(
                driver,
                "return [...document.querySelectorAll('[data-testid=\"toast\"]')].some(t => t.textContent.includes('Workspace renamed'));",
                vec![],
            )
            .await?
                == Value::Bool(true))
        })
        .await?;

        let draft = page(
            driver,
            "const el = document.querySelector('[data-testid=\"workspace-rename-input\"]'); return el ? el.value : null;",
            vec![],
        )
        .await?;
        if draft != json!("Renamed Once and more") {
            bail!("the rename reply replaced text typed during the save: the name field shows {draft}");
        }

        context.close_session(session, "rail-rename-inflight", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

/// Scenario: text typed into the Workspace root field while its save is in
/// flight is kept when the reply arrives, as with the name.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_root_typed_during_its_save_survives_the_reply() {
    let context = TestContext::new("project_rail_root_inflight").expect("harness setup");

    let outcome = async {
        let folder = context.state().path().join("folders").join("root-inflight");
        std::fs::create_dir_all(&folder).context("could not create the fixture folder")?;
        let location = folder.to_str().context("the folder path is not UTF-8")?.to_string();

        let session = context.start_session("rail-root-inflight").await?;
        let driver = session.driver();
        let workspace = created_id(
            ui::invoke(driver, "create_workspace", json!({ "name": "Root Alpha", "rootPath": Value::Null })).await?,
            "create_workspace",
        )?;
        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;

        expand_workspace_in_organizer(driver, &workspace).await?;

        let field = ui::visible(driver, "[data-testid=\"workspace-root-input\"]").await?;
        field.clear().await.context("could not clear the root field")?;
        field.send_keys(&location).await.context("could not type the root folder")?;

        ui::arm_invoke_fault(driver, "set_workspace_root", ui::InvokeFault::Hold).await?;
        ui::visible(driver, "[data-testid=\"workspace-root-save\"]")
            .await?
            .click()
            .await
            .context("could not save the root")?;
        wait_until("the root save to be issued", || async { Ok(ui::invoke_fault_state(driver).await?.0) }).await?;

        ui::visible(driver, "[data-testid=\"workspace-root-input\"]")
            .await?
            .send_keys("-later")
            .await
            .context("could not type while the root save is in flight")?;

        ui::release_invoke_hold(driver).await?;
        wait_until("the held root reply to reach the application", || async {
            Ok(ui::invoke_fault_state(driver).await?.1)
        })
        .await?;

        wait_until("the root save success to be reported", || async {
            Ok(page(
                driver,
                "return [...document.querySelectorAll('[data-testid=\"toast\"]')].some(t => t.textContent.includes('Workspace root saved'));",
                vec![],
            )
            .await?
                == Value::Bool(true))
        })
        .await?;

        let draft = page(
            driver,
            "const el = document.querySelector('[data-testid=\"workspace-root-input\"]'); return el ? el.value : null;",
            vec![],
        )
        .await?;
        let expected = format!("{location}-later");
        if draft != json!(expected) {
            bail!("the root reply replaced text typed during the save: the root field shows {draft}");
        }

        context.close_session(session, "rail-root-inflight", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}
