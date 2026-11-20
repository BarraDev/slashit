// SPDX-License-Identifier: MPL-2.0
//! Desktop journeys for the Project Conversation composer: the send key, the
//! draft that survives a page change and a restart, and the send-key setting.
//!
//! The Coordinator is the fixture's fake Claude, so no paid agent runs.

use super::*;
use super::conversations::{await_text, open_conversations, open_nav};
use slashit_acceptance::fake_agent::{self, FakeAgent};

const PANEL: &str = "[data-testid=\"project-conversation\"]";
const INPUT: &str = "[data-testid=\"conversation-message-input\"]";
const HINT: &str = "[data-testid=\"conversation-send-hint\"]";
const ENTER_MODE: &str = "[data-testid=\"conversation-send-enter\"]";
const SETTINGS_SECTION: &str = "[data-testid=\"conversation-send-settings\"]";
/// WebDriver key codes for the keys the journeys press.
const ENTER: &str = "\u{E007}";
const SHIFT: &str = "\u{E008}";
const CONTROL: &str = "\u{E009}";

/// Start a Coordinator fixture that answers every turn with `reply`.
fn coordinator_fixture(context: &TestContext, reply: &str) -> Result<FakeAgent> {
    let root = context.state().path().to_path_buf();
    let agent = FakeAgent::install(&root).context("install fake Claude")?;
    context.set_child_env("PATH", agent.path_value().to_os_string());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir().as_os_str().to_os_string());
    context.set_child_env(fake_agent::COORDINATOR_OUTPUT_VAR, format!(r#"{{"type":"reply","text":"{reply}"}}"#));
    Ok(agent)
}

async fn create_zero_task_project(driver: &WebDriver, name: &str) -> Result<String> {
    created_id(ui::invoke(driver, "create_project", json!({
        "name": name, "repositoryId": Value::Null, "agentType": "claude_code"
    })).await?, "create_project")
}

/// The composer's current text, as the page holds it.
async fn composer_text(driver: &WebDriver) -> Result<String> {
    let value = page(driver, "const el = document.querySelector('[data-testid=\"conversation-message-input\"]'); return el ? el.value : null;", vec![]).await?;
    value.as_str().map(str::to_owned).context("the message box is not on the page")
}

async fn await_composer_text(driver: &WebDriver, expected: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        if composer_text(driver).await? == expected { return Ok(()); }
        if started.elapsed() > ui::ELEMENT_TIMEOUT {
            bail!("the message box never held {expected:?}; it holds {:?}", composer_text(driver).await?);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The saved Human messages of a Project's Conversation, in order.
async fn human_messages(driver: &WebDriver, project_id: &str) -> Result<Vec<String>> {
    let snapshot = ui::invoke(driver, "get_project_conversation", json!({"projectId": project_id})).await?;
    Ok(snapshot["conversation"]["entries"].as_array().context("Conversation entries missing")?
        .iter()
        .filter(|entry| entry["role"] == "human")
        .filter_map(|entry| entry["kind"]["text"].as_str().map(str::to_owned))
        .collect())
}

/// Wait until the Coordinator has answered and the Conversation is idle again.
async fn await_idle_with_human_count(driver: &WebDriver, project_id: &str, expected: usize) -> Result<()> {
    let started = Instant::now();
    loop {
        let snapshot = ui::invoke(driver, "get_project_conversation", json!({"projectId": project_id})).await?;
        let idle = snapshot["coordinator_live"] == false && snapshot["worker_live"] == false;
        if idle && human_messages(driver, project_id).await?.len() == expected { return Ok(()); }
        if started.elapsed() > ui::ELEMENT_TIMEOUT {
            bail!("the Conversation never reached {expected} Human message(s) and idle: {snapshot}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn composer_sends_once_with_ctrl_enter_and_keeps_an_unsent_draft_across_pages_and_restart() {
    let context = TestContext::new("conversation_composer_keys_and_draft").expect("harness setup");
    let _agent = coordinator_fixture(&context, "Composer reply.").expect("fake Claude");

    let outcome = async {
        let session = context.start_session("composer-keys").await?;
        let project_id = create_zero_task_project(session.driver(), "Composer Keys").await?;
        open_conversations(session.driver(), &project_id).await?;
        let input = ui::visible(session.driver(), INPUT).await?;
        await_text(session.driver(), HINT, "Ctrl+Enter sends.").await?;

        // Default mode: Enter inserts a newline and does not send.
        input.click().await?;
        input.send_keys("First line").await?;
        input.send_keys(ENTER).await?;
        input.send_keys("Second line").await?;
        await_composer_text(session.driver(), "First line\nSecond line").await?;
        if !human_messages(session.driver(), &project_id).await?.is_empty() {
            bail!("Enter sent a message in the default mode");
        }

        // Ctrl+Enter sends once. The message keeps its newline, and the draft is cleared
        // only after the history shows the Human message.
        input.send_keys(&format!("{CONTROL}{ENTER}")).await?;
        await_idle_with_human_count(session.driver(), &project_id, 1).await?;
        let sent = human_messages(session.driver(), &project_id).await?;
        if sent != ["First line\nSecond line"] { bail!("Ctrl+Enter saved the wrong message: {sent:?}"); }
        await_composer_text(session.driver(), "").await?;

        // An unsent draft survives a page change and returns with the Project.
        input.send_keys("Unsent draft").await?;
        tokio::time::sleep(Duration::from_millis(600)).await;
        open_nav(session.driver(), "dashboard", "[data-testid=\"kanban-board\"]").await?;
        open_nav(session.driver(), "conversations", PANEL).await?;
        await_composer_text(session.driver(), "Unsent draft").await?;

        // And it survives an application restart.
        context.close_session(session, "composer-keys", &Ok(())).await?;
        let restarted = context.start_session("composer-keys-restart").await?;
        open_conversations(restarted.driver(), &project_id).await?;
        await_composer_text(restarted.driver(), "Unsent draft").await?;
        if human_messages(restarted.driver(), &project_id).await?.len() != 1 {
            bail!("restart changed the saved messages");
        }
        context.close_session(restarted, "composer-keys-restart", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    context.finish(outcome);
}

async fn open_settings(driver: &WebDriver) -> Result<()> {
    page(driver, "localStorage.setItem('slashit_current_page', 'settings'); return true;", vec![]).await?;
    driver.refresh().await?;
    ui::assert_frontend_is_real(driver).await?;
    ui::visible(driver, SETTINGS_SECTION).await.map(|_| ())
}

#[tokio::test(flavor = "multi_thread")]
async fn enter_sends_when_chosen_in_settings_and_the_choice_survives_restart() {
    let context = TestContext::new("conversation_composer_enter_mode").expect("harness setup");
    let _agent = coordinator_fixture(&context, "Enter reply.").expect("fake Claude");

    let outcome = async {
        let session = context.start_session("enter-mode").await?;
        let project_id = create_zero_task_project(session.driver(), "Enter Mode").await?;
        // Choose Enter-sends in General settings. The Settings page is opened the way the
        // other Settings journeys open it, which does not depend on the Project rail.
        open_settings(session.driver()).await?;
        ui::visible(session.driver(), ENTER_MODE).await?.click().await?;

        open_conversations(session.driver(), &project_id).await?;
        await_text(session.driver(), HINT, "Enter sends. Shift+Enter adds a new line.").await?;
        let input = ui::visible(session.driver(), INPUT).await?;

        // Shift+Enter inserts a newline; Enter sends exactly one message.
        input.click().await?;
        input.send_keys("Hello").await?;
        input.send_keys(&format!("{SHIFT}{ENTER}")).await?;
        input.send_keys("world").await?;
        await_composer_text(session.driver(), "Hello\nworld").await?;
        if !human_messages(session.driver(), &project_id).await?.is_empty() {
            bail!("Shift+Enter sent a message in the Enter-sends mode");
        }
        input.send_keys(ENTER).await?;
        await_idle_with_human_count(session.driver(), &project_id, 1).await?;
        if human_messages(session.driver(), &project_id).await? != ["Hello\nworld"] {
            bail!("Enter saved the wrong message: {:?}", human_messages(session.driver(), &project_id).await?);
        }
        await_composer_text(session.driver(), "").await?;

        // The choice is part of the saved configuration, so a restart keeps it.
        context.close_session(session, "enter-mode", &Ok(())).await?;
        let restarted = context.start_session("enter-mode-restart").await?;
        open_conversations(restarted.driver(), &project_id).await?;
        await_text(restarted.driver(), HINT, "Enter sends. Shift+Enter adds a new line.").await?;
        context.close_session(restarted, "enter-mode-restart", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    context.finish(outcome);
}

const CONFLICT_PANEL: &str = "[data-testid=\"conversation-draft-conflict\"]";
const CONFLICT_STORED: &str = "[data-testid=\"conversation-draft-conflict-stored\"]";
const KEEP_LOCAL: &str = "[data-testid=\"conversation-keep-local-draft\"]";
const SEND: &str = "[data-testid=\"conversation-send\"]";

/// Wait until `selector` is absent from the page, as a state change may take a moment to render.
async fn await_absent(driver: &WebDriver, selector: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let present = page(driver, "return document.querySelector(arguments[0]) !== null;", vec![Value::String(selector.to_string())]).await?;
        if present != Value::Bool(true) { return Ok(()); }
        if started.elapsed() > ui::ELEMENT_TIMEOUT {
            bail!("{selector} is still on the page");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_draft_saved_elsewhere_is_offered_for_choice_and_typed_text_is_never_overwritten() {
    let context = TestContext::new("conversation_draft_conflict").expect("harness setup");
    let _agent = coordinator_fixture(&context, "Unused.").expect("fake Claude");

    let outcome = async {
        let session = context.start_session("draft-conflict").await?;
        let project_id = create_zero_task_project(session.driver(), "Draft Conflict").await?;
        open_conversations(session.driver(), &project_id).await?;
        let input = ui::visible(session.driver(), INPUT).await?;

        // The first save of this session is sequence 1.
        input.click().await?;
        input.send_keys("Typed here").await?;
        await_composer_text(session.driver(), "Typed here").await?;
        tokio::time::sleep(Duration::from_millis(600)).await;

        // Another writer stores a newer draft for this Project.
        ui::invoke(session.driver(), "save_conversation_draft", json!({
            "projectId": project_id,
            "draft": { "seq": 5, "text": "Saved elsewhere" }
        })).await?;

        // The next save is refused, and the stored draft is offered next to the typed text.
        input.send_keys(" more").await?;
        ui::visible(session.driver(), CONFLICT_PANEL).await.context("the draft conflict was not shown")?;
        let stored = ui::visible(session.driver(), CONFLICT_STORED).await?.text().await?;
        if !stored.contains("Saved elsewhere") { bail!("the conflict shows the wrong stored draft: {stored:?}"); }
        await_composer_text(session.driver(), "Typed here more").await?;
        let send_disabled = page(session.driver(), "return document.querySelector(arguments[0]).disabled;", vec![Value::String(SEND.to_string())]).await?;
        if send_disabled != Value::Bool(true) { bail!("sending was not blocked during the conflict"); }

        // Keeping the typed text writes it at the shown sequence and clears the conflict.
        ui::visible(session.driver(), KEEP_LOCAL).await?.click().await?;
        await_absent(session.driver(), CONFLICT_PANEL).await?;
        await_composer_text(session.driver(), "Typed here more").await?;
        let saved = ui::invoke(session.driver(), "get_conversation_draft", json!({ "projectId": project_id })).await?;
        if saved["text"] != "Typed here more" { bail!("the typed text was not saved: {saved}"); }
        Ok::<_, anyhow::Error>(())
    }.await;
    context.finish(outcome);
}
