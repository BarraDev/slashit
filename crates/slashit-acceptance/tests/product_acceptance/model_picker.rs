// SPDX-License-Identifier: MPL-2.0
//! The Task model picker, driven from the keyboard, through the Task editor.
//!
//! The picker is a combobox: arrows move a highlight, Enter chooses only a row
//! the person moved to, Escape closes it and keeps focus in the field, and a
//! chosen value survives save and reopen. The saved model starts as `sonnet`,
//! which is not the first row, so an Enter that chose the first row would show.

use super::human_review::{click, install_fakes};
use super::*;
use thirtyfour::components::SelectElement;

const MODEL_INPUT_ID: &str = "task-ai-model";
const EDIT_BUTTON: &str = "[data-testid=\"task-drawer-edit\"]";
const SAVE_BUTTON: &str = "//button[normalize-space(.)='Save Changes']";
const WORKER_SELECT: &str = "//label[normalize-space(.)='Coding Worker']/following::select[1]";
const SAVED_CLAUDE_LABEL: &str = "Claude Sonnet (latest alias)";
const HAIKU_LABEL: &str = "Claude Haiku (latest alias)";
const OPENCODE_MODEL: &str = "ollama/llama3.2:3b";
/// A full Claude model ID that no catalog lists, typed by hand.
const MANUAL_CLAUDE_MODEL: &str = "claude-opus-4-1-20250805";

/// The picker's accessibility state, read from the page.
const PICKER_STATE: &str = r#"
const input = document.getElementById('task-ai-model');
if (!input) { return null; }
const rows = [...document.querySelectorAll('#task-ai-model-listbox [role="option"]')];
return {
  role: input.getAttribute('role'),
  open: input.getAttribute('aria-expanded') === 'true',
  controls: input.getAttribute('aria-controls'),
  listbox: !!document.getElementById('task-ai-model-listbox'),
  active: input.getAttribute('aria-activedescendant') || null,
  selected: rows.filter(r => r.getAttribute('aria-selected') === 'true').map(r => r.id),
  rowCount: rows.length,
  rowTexts: rows.map(r => r.textContent.trim()),
  focused: document.activeElement === input,
  label: input.labels && input.labels[0] ? input.labels[0].textContent.trim() : null,
  value: input.value,
  refresh: [...document.querySelectorAll('button')].some(b => b.textContent.trim() === 'Refresh')
};
"#;

#[tokio::test(flavor = "multi_thread")]
async fn the_model_picker_is_driven_from_the_keyboard() {
    let context = TestContext::new("model_picker_keyboard").expect("harness setup");
    let outcome = keyboard_journey(&context).await;
    context.finish(outcome);
}

async fn keyboard_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (_agent, _gh) = install_fakes(context, &root)?;
    let repository = GitFixture::create_local_only(&root.join("fixture-repo"))?;

    let session = context.start_session("model-picker").await?;
    let outcome = keyboard(session.driver(), &repository).await;
    context
        .close_session(session, "model-picker", &outcome)
        .await?;
    outcome
}

async fn keyboard(driver: &WebDriver, repository: &GitFixture) -> Result<()> {
    let Prerequisites {
        project_id,
        task_id,
        title,
    } = create_prerequisites(
        driver,
        repository,
        "Model picker",
        "Chooses its model from the keyboard.",
    )
    .await?;
    ui::invoke(
        driver,
        "update_task",
        json!({ "params": { "taskId": task_id, "model": "sonnet" } }),
    )
    .await?;
    open_board(driver, &project_id).await?;

    // Enter with no highlight must not choose the first row, and Escape closes
    // the list with focus still in the field.
    open_editor(driver, &task_id, &title).await?;
    let idle = picker_state(driver).await?;
    expect(&idle, "label", &json!("AI Model"), "the field is labelled")?;
    expect(&idle, "value", &json!(SAVED_CLAUDE_LABEL), "the saved model is shown when closed")?;
    expect(&idle, "controls", &json!(null), "a closed picker names no listbox")?;
    expect(&idle, "refresh", &json!(false), "a Claude picker offers no Refresh, its aliases are fixed")?;
    focus_field(driver).await?;
    let opened = picker_state(driver).await?;
    expect(&opened, "role", &json!("combobox"), "the field has the combobox role")?;
    expect(&opened, "open", &json!(true), "focusing the field opens the list")?;
    expect(&opened, "controls", &json!("task-ai-model-listbox"), "the field controls its listbox")?;
    expect(&opened, "listbox", &json!(true), "the listbox is rendered while open")?;
    expect(&opened, "active", &json!(null), "no row is highlighted on open")?;
    expect(&opened, "rowCount", &json!(4), "the Claude aliases are listed")?;
    send(driver, Key::Enter).await?;
    let after_enter = picker_state(driver).await?;
    expect(&after_enter, "open", &json!(true), "Enter with no highlight leaves the list open")?;
    expect(&after_enter, "active", &json!(null), "Enter with no highlight highlights nothing")?;
    expect(&after_enter, "value", &json!(""), "Enter with no highlight chooses nothing")?;
    send(driver, Key::Escape).await?;
    let closed = picker_state(driver).await?;
    expect(&closed, "open", &json!(false), "Escape closes the list")?;
    expect(&closed, "listbox", &json!(false), "the closed list is removed")?;
    expect(&closed, "controls", &json!(null), "the closed list is no longer referenced")?;
    expect(&closed, "focused", &json!(true), "Escape keeps focus in the field")?;
    save(driver, &project_id, &task_id, "sonnet").await?;

    // Reopen: the saved value is shown, and the arrows move a highlight with
    // wrap-around, so ArrowUp from no highlight reaches the last row.
    open_editor(driver, &task_id, &title).await?;
    expect(&picker_state(driver).await?, "value", &json!(SAVED_CLAUDE_LABEL), "the saved model survives reopen")?;
    focus_field(driver).await?;
    send(driver, Key::Up).await?;
    expect(&picker_state(driver).await?, "active", &json!("task-ai-model-option-haiku"), "ArrowUp from no highlight reaches the last row")?;
    send(driver, Key::Down).await?;
    expect(&picker_state(driver).await?, "active", &json!("task-ai-model-option-default"), "ArrowDown wraps to the first row")?;
    send(driver, Key::Up).await?;
    send(driver, Key::Up).await?;
    let highlighted = picker_state(driver).await?;
    expect(&highlighted, "active", &json!("task-ai-model-option-opus"), "ArrowUp moves the highlight")?;
    expect(&highlighted, "selected", &json!(["task-ai-model-option-opus"]), "the highlighted row is aria-selected")?;
    send(driver, Key::Down).await?;
    send(driver, Key::Enter).await?;
    let chosen = picker_state(driver).await?;
    expect(&chosen, "open", &json!(false), "choosing a row closes the list")?;
    expect(&chosen, "focused", &json!(true), "choosing a row keeps focus in the field")?;
    expect(&chosen, "value", &json!(HAIKU_LABEL), "the highlighted row was chosen")?;
    save(driver, &project_id, &task_id, "haiku").await?;

    // An OpenCode provider/model typed into the field and chosen from the
    // keyboard keeps its exact identifier through save and reopen.
    open_editor(driver, &task_id, &title).await?;
    SelectElement::new(
        &driver
            .find(By::XPath(WORKER_SELECT))
            .await
            .context("the editor shows no Coding Worker select")?,
    )
    .await?
    .select_by_value("opencode")
    .await
    .context("could not switch the Coding Worker to OpenCode")?;
    focus_field(driver).await?;
    driver
        .find(By::Id(MODEL_INPUT_ID))
        .await?
        .send_keys(OPENCODE_MODEL)
        .await?;
    send(driver, Key::Down).await?;
    let typed = picker_state(driver).await?;
    expect(&typed, "active", &json!("task-ai-model-option-_typed"), "the typed entry is the first row")?;
    expect(&typed, "refresh", &json!(true), "an OpenCode picker offers Refresh, its list is discovered")?;
    send(driver, Key::Enter).await?;
    save(driver, &project_id, &task_id, OPENCODE_MODEL).await?;
    let saved = listed(driver, &project_id, &task_id).await?;
    if saved.get("provider").and_then(Value::as_str) != Some("opencode") {
        bail!("the Task is not an OpenCode Task after saving: {saved}");
    }
    open_editor(driver, &task_id, &title).await?;
    expect(&picker_state(driver).await?, "value", &json!(OPENCODE_MODEL), "the exact OpenCode identifier survives reopen")?;

    // A typed Claude identifier the catalog does not list is kept when the field
    // loses focus by keyboard, and it is saved and reopened exactly. Switching
    // the Worker starts each runtime from its own model, never from the default.
    select_worker(driver, "claude").await?;
    expect(&picker_state(driver).await?, "value", &json!(""), "switching to Claude starts empty, without the provider default")?;
    focus_field(driver).await?;
    driver
        .find(By::Id(MODEL_INPUT_ID))
        .await?
        .send_keys(MANUAL_CLAUDE_MODEL)
        .await?;
    send(driver, Key::Tab).await?;
    let left = picker_state(driver).await?;
    expect(&left, "value", &json!(MANUAL_CLAUDE_MODEL), "a typed identifier is kept when the field loses focus")?;
    // Tab moves focus into the AI Reviewer field. Save must be clickable here,
    // with no keystroke to dismiss anything first.
    save(driver, &project_id, &task_id, MANUAL_CLAUDE_MODEL).await?;
    let blurred_saved = listed(driver, &project_id, &task_id).await?;
    // The default provider is omitted from a stored Task, so no key means Claude.
    if blurred_saved.get("provider").and_then(Value::as_str).unwrap_or("claude") != "claude" {
        bail!("the Task is not a Claude Task after saving the typed identifier: {blurred_saved}");
    }
    open_editor(driver, &task_id, &title).await?;
    expect(&picker_state(driver).await?, "value", &json!(MANUAL_CLAUDE_MODEL), "the exact Claude identifier survives reopen")?;
    select_worker(driver, "opencode").await?;
    expect(&picker_state(driver).await?, "value", &json!(""), "switching to OpenCode does not show the Claude model")?;
    select_worker(driver, "claude").await?;
    expect(&picker_state(driver).await?, "value", &json!(MANUAL_CLAUDE_MODEL), "switching back restores the Claude model")?;
    Ok(())
}

/// Chooses the Coding Worker by its select value, `claude` or `opencode`.
async fn select_worker(driver: &WebDriver, value: &str) -> Result<()> {
    SelectElement::new(
        &driver
            .find(By::XPath(WORKER_SELECT))
            .await
            .context("the editor shows no Coding Worker select")?,
    )
    .await?
    .select_by_value(value)
    .await
    .with_context(|| format!("could not switch the Coding Worker to {value}"))
}

async fn open_editor(driver: &WebDriver, task_id: &str, title: &str) -> Result<()> {
    open_drawer(driver, task_id, title).await?;
    click(driver, EDIT_BUTTON, "the drawer's Edit button").await?;
    ui::visible(driver, &format!("#{MODEL_INPUT_ID}"))
        .await
        .context("the Task editor shows no AI Model field")?;
    Ok(())
}

async fn focus_field(driver: &WebDriver) -> Result<()> {
    // The field sits below the editor's fold; scroll it clear of the footer first.
    driver
        .execute(
            "document.getElementById(arguments[0])?.scrollIntoView({ block: 'center' });",
            vec![json!(MODEL_INPUT_ID)],
        )
        .await
        .context("could not scroll the AI Model field into view")?;
    driver
        .find(By::Id(MODEL_INPUT_ID))
        .await
        .context("the AI Model field is missing")?
        .click()
        .await
        .context("could not focus the AI Model field")
}

async fn send(driver: &WebDriver, key: Key) -> Result<()> {
    driver
        .active_element()
        .await?
        .send_keys(key)
        .await
        .context("could not send a key to the focused element")
}

/// Saves the editor and waits until the product lists the expected model.
async fn save(driver: &WebDriver, project_id: &str, task_id: &str, expected: &str) -> Result<()> {
    driver
        .find(By::XPath(SAVE_BUTTON))
        .await
        .context("the editor shows no Save Changes button")?
        .click()
        .await?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let task = listed(driver, project_id, task_id).await?;
        if task.get("model").and_then(Value::as_str) == Some(expected) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("the saved model is {:?}, not {expected:?}: {task}", task.get("model"));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn picker_state(driver: &WebDriver) -> Result<Value> {
    let state = driver
        .execute(PICKER_STATE, vec![])
        .await
        .context("could not read the model picker")?
        .json()
        .clone();
    if state.is_null() {
        bail!("the AI Model field is not on the page");
    }
    Ok(state)
}

fn expect(state: &Value, field: &str, want: &Value, step: &str) -> Result<()> {
    let got = state.get(field).cloned().unwrap_or(Value::Null);
    if &got != want {
        bail!("{step}: {field} is {got}, expected {want}; picker state {state}");
    }
    Ok(())
}

async fn listed(driver: &WebDriver, project_id: &str, task_id: &str) -> Result<Value> {
    let tasks = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    tasks
        .as_array()
        .and_then(|all| all.iter().find(|task| task.get("id").and_then(Value::as_str) == Some(task_id)))
        .cloned()
        .with_context(|| format!("the product no longer lists Task {task_id}"))
}

/// At the window's minimum height the AI Reviewer list reaches the top edge of
/// Save. Save must stay the element a press lands on, so the press saves.
#[tokio::test(flavor = "multi_thread")]
async fn the_open_reviewer_list_never_stops_save() {
    let context = TestContext::new("model_picker_reviewer_save").expect("harness setup");
    let outcome = reviewer_save_journey(&context).await;
    context.finish(outcome);
}

async fn reviewer_save_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (_agent, _gh) = install_fakes(context, &root)?;
    let repository = GitFixture::create_local_only(&root.join("fixture-repo"))?;
    let session = context.start_session("model-picker-reviewer-save").await?;
    let outcome = reviewer_save(session.driver(), &repository).await;
    context
        .close_session(session, "model-picker-reviewer-save", &outcome)
        .await?;
    outcome
}

/// The point at the centre of Save's top edge, and what is element there.
const SAVE_TOP_EDGE: &str = r#"
const save = [...document.querySelectorAll('button')].find(b => b.textContent.trim() === 'Save Changes');
const r = save.getBoundingClientRect();
const x = r.left + r.width / 2;
const y = r.top + 2;
const hit = document.elementFromPoint(x, y);
return { x, y, hitIsSave: !!hit && save.contains(hit) };
"#;

async fn reviewer_save(driver: &WebDriver, repository: &GitFixture) -> Result<()> {
    let Prerequisites { project_id, task_id, title } = create_prerequisites(
        driver,
        repository,
        "Reviewer over Save",
        "Saves while the AI Reviewer list is open.",
    )
    .await?;
    open_board(driver, &project_id).await?;
    open_editor(driver, &task_id, &title).await?;
    let before = listed(driver, &project_id, &task_id).await?;

    // The minimum window height is where the list reaches Save.
    driver
        .set_window_rect(0, 0, 1280, 600)
        .await
        .context("could not set the window to its minimum height")?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    driver
        .execute(
            "document.getElementById('task-ai-reviewer-select').scrollIntoView({ block: 'center' });",
            vec![],
        )
        .await?;
    driver
        .find(By::Id("task-ai-reviewer-select"))
        .await
        .context("the editor shows no AI Reviewer field")?
        .click()
        .await
        .context("could not open the AI Reviewer list")?;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let edge = driver.execute(SAVE_TOP_EDGE, vec![]).await?.json().clone();
    if edge["hitIsSave"] != json!(true) {
        bail!("the AI Reviewer list covers the top edge of Save: {edge}");
    }
    let x = edge["x"].as_f64().unwrap_or(0.0) as i64;
    let y = edge["y"].as_f64().unwrap_or(0.0) as i64;
    driver
        .action_chain()
        .move_to(x, y)
        .click()
        .perform()
        .await
        .context("could not press Save's top edge")?;

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let open = driver
            .execute("return !!document.getElementById('task-ai-reviewer-select');", vec![])
            .await?
            .json()
            .clone();
        if open == json!(false) {
            break;
        }
        if Instant::now() >= deadline {
            bail!("pressing Save's top edge did not save the Task: the editor is still open");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let after = listed(driver, &project_id, &task_id).await?;
    if after.get("reviewer_model") != before.get("reviewer_model") {
        bail!("pressing Save chose a reviewer row: before {before}, after {after}");
    }
    Ok(())
}
