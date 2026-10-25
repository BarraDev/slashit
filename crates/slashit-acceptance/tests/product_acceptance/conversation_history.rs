//! Desktop journeys for a Project Conversation with a long saved history: how it
//! renders, when entries were saved, whether it follows the newest entry, what a
//! Project switch shows, which reads write, and which keyboard shortcuts the
//! running WebView handles. The histories are seeded on disk, and no journey here
//! sends a message. The fake Claude stays first on PATH as a guard.

use super::conversations::{await_rail, await_text, open_conversations, open_nav};
use super::*;
use slashit_acceptance::fake_agent::{self, FakeAgent};
use std::fs;

const PANEL: &str = "[data-testid=\"project-conversation\"]";
const HISTORY: &str = "[data-testid=\"conversation-history\"]";
const INPUT: &str = "[data-testid=\"conversation-message-input\"]";
const MESSAGE: &str = "[data-testid=\"conversation-message\"]";
const EVENT: &str = "[data-testid=\"conversation-action-event\"]";
const TIME: &str = "[data-testid=\"conversation-entry-time\"]";
const JUMP: &str = "[data-testid=\"conversation-jump-latest\"]";
const SETTINGS_HEADING: &str = "Configure your application preferences";
const CONTEXT_HEADING: &str = "Context Files";

/// Completed question-and-answer turns in the long history. Each turn is two
/// entries, and the seeded history adds one orphaned entry and one final reply.
const TURNS: usize = 240;
const ORPHAN_AT_TURN: usize = 100;
const FINAL_REPLY: &str = "Latest reply: seed-final.";
/// About 560 characters, so the history is a realistic size on disk and on screen.
const FILLER: &str = "The Project keeps its Tasks in its own folder, and every Coordinator reply cites the Task index it read. Nothing here changes a Task until a person approves a proposal, so a long reply is only context for the next question. ";

fn entry_id(index: usize) -> String {
    format!("{index:08x}-e0e0-4000-8000-000000000002")
}

fn conversation_id(salt: u32) -> String {
    format!("{salt:08x}-c0c0-4000-8000-000000000001")
}

/// Saved times are minutes after midnight UTC on one day, so the minute of each
/// entry is known and the displayed minute can be checked.
fn saved_at(index: usize) -> String {
    format!("2026-10-20T{:02}:{:02}:00Z", index / 60, index % 60)
}

fn entry(index: usize, role: &str, kind: Value) -> Value {
    json!({ "id": entry_id(index), "role": role, "kind": kind, "created_at": saved_at(index) })
}

fn human(index: usize, text: &str) -> Value {
    entry(index, "human", json!({ "kind": "human_message", "text": text }))
}

fn coordinator(index: usize, text: &str) -> Value {
    entry(index, "coordinator", json!({ "kind": "coordinator_reply", "text": text }))
}

fn long_history() -> Vec<Value> {
    let mut entries = Vec::new();
    for turn in 0..TURNS {
        let index = entries.len();
        entries.push(human(index, &format!("Turn {turn}: what is in this Project?")));
        let index = entries.len();
        entries.push(coordinator(index, &format!("Turn {turn} reply. {FILLER}")));
        if turn == ORPHAN_AT_TURN {
            // An event whose proposal record is not in the Conversation's actions.
            let index = entries.len();
            entries.push(entry(index, "coordinator", json!({ "kind": "action_proposed", "action_id": "0000dead-0000-4000-8000-00000000beef" })));
        }
    }
    let index = entries.len();
    entries.push(coordinator(index, FINAL_REPLY));
    entries
}

/// Writes a Conversation the way the application stores one, so the application
/// reads it as its own. The primary pointer names it, as for any Project.
fn seed_conversation(data_home: &Path, project_id: &str, salt: u32, entries: Vec<Value>) -> Result<(PathBuf, usize)> {
    let dir = data_home.join("slashit-app").join("conversations");
    fs::create_dir_all(&dir).context("could not create the conversations directory")?;
    let id = conversation_id(salt);
    let total = entries.len();
    let conversation = json!({
        "id": id,
        "project_id": project_id,
        "revision": total,
        "created_at": saved_at(0),
        "updated_at": saved_at(total),
        "entries": entries,
        "actions": [],
        "project_actions": [],
    });
    let file = dir.join(format!("{id}.json"));
    fs::write(&file, serde_json::to_vec(&conversation)?).context("could not write the seeded Conversation")?;
    fs::write(dir.join(format!("{project_id}.primary")), &id).context("could not write the primary pointer")?;
    Ok((file, total))
}

fn install_guard(context: &TestContext) -> Result<()> {
    let agent = FakeAgent::install(context.state().path()).context("install fake Claude")?;
    context.set_child_env("PATH", agent.path_value().to_os_string());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir().as_os_str().to_os_string());
    Ok(())
}

async fn create_project(driver: &WebDriver, name: &str) -> Result<String> {
    created_id(ui::invoke(driver, "create_project", json!({
        "name": name, "repositoryId": Value::Null, "agentType": "claude_code"
    })).await?, "create_project")
}

/// The history's visible height, full height, and scroll position, in pixels.
async fn history_geometry(driver: &WebDriver) -> Result<Value> {
    page(driver, &format!(
        "const el = document.querySelector('{HISTORY}'); return {{ client: el.clientHeight, height: el.scrollHeight, top: el.scrollTop }};"
    ), vec![]).await
}

/// Whether the history is scrolled to its newest entry, within a small slack.
async fn distance_to_latest(driver: &WebDriver) -> Result<Option<f64>> {
    let value = page(driver, &format!(
        "const el = document.querySelector('{HISTORY}'); return el ? el.scrollHeight - el.scrollTop - el.clientHeight : null;"
    ), vec![]).await?;
    Ok(value.as_f64())
}

async fn await_at_latest(driver: &WebDriver) -> Result<()> {
    let started = Instant::now();
    loop {
        if distance_to_latest(driver).await?.is_some_and(|distance| distance <= 40.0) {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the history did not follow its newest entry");
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The text of every rendered element matching `selector`, in order.
async fn texts(driver: &WebDriver, selector: &str) -> Result<Vec<String>> {
    let value = page(driver, &format!(
        "return Array.from(document.querySelectorAll('{selector}'), e => e.textContent || '');"
    ), vec![]).await?;
    Ok(value.as_array().map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default())
}

/// A saved time is `HH:MM` today, `Mon D HH:MM` this year, or `YYYY-MM-DD HH:MM`.
fn is_saved_time(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 5 && bytes[bytes.len() - 5..].iter().enumerate().all(|(i, byte)| match i {
        2 => *byte == b':',
        _ => byte.is_ascii_digit(),
    })
}

/// Whether Jump to latest is on screen now. Unlike `ui::visible`, this does not wait.
async fn jump_shown(driver: &WebDriver) -> Result<bool> {
    Ok(page(driver, &format!("return !!document.querySelector('{JUMP}');"), vec![]).await? == Value::Bool(true))
}

async fn await_history(driver: &WebDriver, expected: &str) -> Result<()> {
    await_text(driver, HISTORY, expected).await
}

/// A probe the page keeps only until it reloads.
const PROBE_SET: &str = "window.__probe = 'kept'; return true;";
const PROBE_READ: &str = "return window.__probe === 'kept';";

/// Captures console errors and uncaught failures on the page, so a panic in the
/// WebView is visible even when nothing on screen shows it.
const ERROR_CAPTURE: &str = "window.__conversationErrors = []; \
    const log = console.error; \
    console.error = (...args) => { window.__conversationErrors.push(args.map(String).join(' ')); log.apply(console, args); }; \
    window.addEventListener('error', e => window.__conversationErrors.push(String(e.message))); \
    window.addEventListener('unhandledrejection', e => window.__conversationErrors.push(String(e.reason))); \
    return true;";
const ERROR_READ: &str = "return window.__conversationErrors || [];";
/// Records each Control chord the document receives, as `ctrl:key`.
const KEY_RECORD: &str = "window.__keys = []; \
    document.addEventListener('keydown', e => { if (e.ctrlKey) window.__keys.push('ctrl:' + e.key); }); \
    return true;";
const KEY_READ: &str = "return window.__keys || [];";

#[tokio::test(flavor = "multi_thread")]
async fn a_long_seeded_history_renders_saves_times_and_follows_the_latest_entry() {
    let context = TestContext::new("conversation_long_history").expect("harness setup");
    install_guard(&context).expect("install guard");

    let outcome = async {
        let session = context.start_session("long-history").await?;
        let driver = session.driver();
        let project_id = create_project(driver, "Long History").await?;
        let (file, total) = seed_conversation(&context.state().data_home(), &project_id, 1, long_history())?;
        let before = fs::read(&file)?;

        open_conversations(driver, &project_id).await?;
        await_history(driver, FINAL_REPLY).await?;

        // Ordinary reads of a clean history must not write it.
        for _ in 0..3 {
            let started = Instant::now();
            let snapshot = ui::invoke(driver, "get_project_conversation", json!({"projectId": project_id})).await?;
            let took = started.elapsed();
            let bytes = serde_json::to_vec(&snapshot)?.len();
            eprintln!("get_project_conversation: {total} entries, {bytes} bytes, {took:?}");
            if snapshot["conversation"]["entries"].as_array().map(Vec::len) != Some(total) {
                bail!("a read dropped entries from the saved history");
            }
            if snapshot["coordinator_live"] != false || snapshot["worker_live"] != false {
                bail!("a clean history was reported live");
            }
        }
        if fs::read(&file)? != before {
            bail!("ordinary reads rewrote a clean saved Conversation");
        }

        // Every saved entry has a message or an event on screen, and an orphaned
        // event says so instead of disappearing.
        let messages = texts(driver, MESSAGE).await?.len();
        if messages != TURNS * 2 + 1 {
            bail!("expected {} rendered messages, found {messages}", TURNS * 2 + 1);
        }
        let events = texts(driver, EVENT).await?;
        if events.len() != 1 || !events[0].contains("not in this Conversation") {
            bail!("the orphaned entry did not render a visible fallback: {events:?}");
        }
        let times = texts(driver, TIME).await?;
        if times.len() != total {
            bail!("expected a saved time for each of {total} entries, found {}", times.len());
        }
        if let Some(bad) = times.iter().find(|time| !is_saved_time(time)) {
            bail!("a saved time has an unexpected form: {bad:?}");
        }
        if !times[34].ends_with(":34") || !times[0].ends_with(":00") {
            bail!("saved times do not match their entries: {:?}", &times[..3]);
        }

        // Following means little unless the history can scroll at all.
        let geometry = history_geometry(driver).await?;
        eprintln!("history geometry after load: {geometry}");
        if geometry["height"].as_f64().unwrap_or(0.0) - geometry["client"].as_f64().unwrap_or(0.0) < 400.0 {
            let chain = page(driver, &format!(
                "const out = []; let n = document.querySelector('{HISTORY}'); \
                 while (n && n !== document.body) {{ const cs = getComputedStyle(n); \
                   out.push(`${{n.tagName}} [${{(n.getAttribute('class') || '').slice(0, 70)}}] client=${{n.clientHeight}} scroll=${{n.scrollHeight}} overflowY=${{cs.overflowY}} display=${{cs.display}}`); \
                   n = n.parentElement; }} return out;"
            ), vec![]).await?;
            for line in chain.as_array().into_iter().flatten() {
                eprintln!("ancestor: {line}");
            }
            bail!("the history cannot scroll, so following it cannot be checked: {geometry}");
        }

        // Following: the history opens at its newest entry.
        await_at_latest(driver).await?;
        if jump_shown(driver).await? {
            bail!("Jump to latest is shown while the history is at its newest entry");
        }

        // Reading earlier entries: scrolling up stops the follow and offers the jump.
        page(driver, &format!(
            "const el = document.querySelector('{HISTORY}'); el.scrollTop = 0; el.dispatchEvent(new Event('scroll')); return true;"
        ), vec![]).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        eprintln!("after scrolling to the top: {} (jump shown: {})", history_geometry(driver).await?, jump_shown(driver).await?);
        ui::visible(driver, JUMP).await.context("scrolling up did not offer Jump to latest")?;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        if distance_to_latest(driver).await?.is_some_and(|distance| distance <= 40.0) {
            bail!("the history followed its newest entry while the reader was above it");
        }

        ui::visible(driver, JUMP).await?.click().await.context("could not click Jump to latest")?;
        await_at_latest(driver).await?;
        if jump_shown(driver).await? {
            bail!("Jump to latest stayed on screen after it was used");
        }

        context.close_session(session, "long-history", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_projects_shows_only_the_selected_history_and_records_no_errors() {
    let context = TestContext::new("conversation_switch").expect("harness setup");
    install_guard(&context).expect("install guard");

    let outcome = async {
        let session = context.start_session("switch").await?;
        let driver = session.driver();
        let alpha = create_project(driver, "Alpha History").await?;
        let beta = create_project(driver, "Beta History").await?;
        seed_conversation(&context.state().data_home(), &alpha, 2, vec![
            human(0, "ALPHA-ONLY question"),
            coordinator(1, "ALPHA-ONLY reply"),
        ])?;
        seed_conversation(&context.state().data_home(), &beta, 3, {
            let mut entries = long_history();
            entries.push(coordinator(entries.len(), "BETA-ONLY closing reply"));
            entries
        })?;

        open_conversations(driver, &alpha).await?;
        await_history(driver, "ALPHA-ONLY reply").await?;
        page(driver, ERROR_CAPTURE, vec![]).await?;

        // Switch to Beta while Alpha's history is on screen, and wait for Beta.
        select_project(driver, &beta).await?;
        eprintln!("step: Beta selected in the rail");
        open_nav(driver, "conversations", PANEL).await?;
        eprintln!("step: Beta's Conversation open");
        await_history(driver, "BETA-ONLY closing reply").await?;
        let shown = texts(driver, MESSAGE).await?.join("\n");
        if shown.contains("ALPHA-ONLY") {
            bail!("Beta's Conversation still shows Alpha's entries");
        }

        // Switch back and away again at once, so a response for the view that
        // was left can arrive after it has been disposed.
        for round in 0..3 {
            select_project(driver, &alpha).await?;
            eprintln!("step: round {round}, Alpha selected");
            open_nav(driver, "dashboard", KANBAN_BOARD).await?;
            eprintln!("step: round {round}, Kanban open");
            open_nav(driver, "conversations", PANEL).await?;
            eprintln!("step: round {round}, Conversations open");
        }
        await_history(driver, "ALPHA-ONLY reply").await?;
        let shown = texts(driver, MESSAGE).await?.join("\n");
        if shown.contains("BETA-ONLY") {
            bail!("Alpha's Conversation shows Beta's entries after switching back");
        }

        let errors = page(driver, ERROR_READ, vec![]).await?;
        eprintln!("console errors during the switches: {errors}");
        if errors.as_array().is_some_and(|items| !items.is_empty()) {
            bail!("switching Projects reported errors: {errors}");
        }

        context.close_session(session, "switch", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_turn_is_recorded_once_and_ordinary_reads_do_not_write() {
    let context = TestContext::new("conversation_unanswered").expect("harness setup");
    install_guard(&context).expect("install guard");

    let outcome = async {
        let session = context.start_session("unanswered").await?;
        let driver = session.driver();
        let project_id = create_project(driver, "Unanswered Turn").await?;
        let (file, _) = seed_conversation(&context.state().data_home(), &project_id, 4, vec![
            human(0, "Earlier question."),
            coordinator(1, "Earlier answer."),
            human(2, "A question whose run ended before any reply was saved."),
        ])?;

        // The first read after a restart records the unanswered turn, once.
        let first = ui::invoke(driver, "get_project_conversation", json!({"projectId": project_id})).await?;
        let entries = first["conversation"]["entries"].as_array().context("entries missing")?;
        if entries.len() != 4 || entries[3]["kind"]["kind"] != "run_failed" {
            bail!("the first read did not record the unanswered turn: {first}");
        }
        let written = fs::read(&file)?;

        // Later ordinary reads find nothing to do and leave the document alone.
        for _ in 0..3 {
            let again = ui::invoke(driver, "get_project_conversation", json!({"projectId": project_id})).await?;
            if again["conversation"]["entries"].as_array().map(Vec::len) != Some(4) {
                bail!("a later read recorded the unanswered turn again: {again}");
            }
        }
        if fs::read(&file)? != written {
            bail!("later ordinary reads rewrote the Conversation");
        }

        context.close_session(session, "unanswered", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}

/// Selects a Project in the rail the way `open_conversations` does: the rail
/// expands under the pointer, and navigation waits until it has.
///
/// Then the pointer leaves the rail. In the running WebView the rail can stay
/// expanded after that, because the item under the pointer was re-rendered by
/// the selection; the journey records it and enters and leaves the rail once more.
async fn select_project(driver: &WebDriver, project_id: &str) -> Result<()> {
    let selector = format!("[data-testid=\"rail-project-{project_id}\"]");
    ui::visible(driver, &selector).await?
        .click().await.context("could not select the project in the rail")?;
    await_rail(driver, "return rail.classList.contains('expanded') && rail.getBoundingClientRect().width >= 180;").await?;

    let main = ui::visible(driver, "main").await?;
    driver.action_chain().move_to_element_center(&main).perform().await.context("could not move the pointer off the rail")?;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    if page(driver, "const rail = document.querySelector('[data-testid=\"project-rail\"]'); return !!rail && rail.classList.contains('expanded');", vec![]).await? == Value::Bool(true) {
        eprintln!("rail stayed expanded after the pointer left it; entering and leaving once more");
        let item = ui::visible(driver, &selector).await?;
        driver.action_chain().move_to_element_center(&item).perform().await.context("could not re-enter the rail")?;
        driver.action_chain().move_to_element_center(&main).perform().await.context("could not leave the rail again")?;
    }
    await_rail(driver, "return !rail.classList.contains('expanded') && rail.getBoundingClientRect().width <= 48;").await
}

/// Presses Control plus `key` in the WebView, the way a person would.
async fn press_ctrl(driver: &WebDriver, key: char) -> Result<()> {
    driver.action_chain()
        .key_down(Key::Control)
        .key_down(key)
        .key_up(key)
        .key_up(Key::Control)
        .perform()
        .await
        .context("could not send the key chord")
}

/// Records what a shortcut did, so the report says what the WebView really does.
#[tokio::test(flavor = "multi_thread")]
async fn keyboard_shortcuts_as_the_running_webview_handles_them() {
    let context = TestContext::new("conversation_keyboard").expect("harness setup");
    install_guard(&context).expect("install guard");

    let outcome = async {
        let session = context.start_session("keyboard").await?;
        let driver = session.driver();
        let project_id = create_project(driver, "Keyboard").await?;
        seed_conversation(&context.state().data_home(), &project_id, 5, vec![human(0, "Keyboard question"), coordinator(1, "Keyboard answer")])?;
        open_conversations(driver, &project_id).await?;
        await_history(driver, "Keyboard answer").await?;

        // Unsaved state: a draft in the message box, with focus in it. The key
        // recorder shows whether a chord reached the page at all.
        ui::visible(driver, INPUT).await?.send_keys("unsaved draft").await?;
        page(driver, PROBE_SET, vec![]).await?;
        page(driver, KEY_RECORD, vec![]).await?;

        press_ctrl(driver, 'k').await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let kept = page(driver, PROBE_READ, vec![]).await? == Value::Bool(true);
        eprintln!("Ctrl+K with the draft focused: page kept (no reload) = {kept}");

        press_ctrl(driver, 'r').await?;
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let kept = page(driver, PROBE_READ, vec![]).await? == Value::Bool(true);
        let draft = page(driver, &format!("return document.querySelector('{INPUT}')?.value ?? null;"), vec![]).await?;
        eprintln!("Ctrl+R with the draft focused: page kept (no reload) = {kept}; draft after = {draft}");

        // From outside the textarea, with the same draft and probe still in place.
        page(driver, PROBE_SET, vec![]).await?;
        page(driver, "document.activeElement && document.activeElement.blur(); return true;", vec![]).await?;

        press_ctrl(driver, 'k').await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        if page(driver, PROBE_READ, vec![]).await? != Value::Bool(true) {
            bail!("Ctrl+K reloaded the page");
        }

        press_ctrl(driver, 'r').await?;
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let kept = page(driver, PROBE_READ, vec![]).await? == Value::Bool(true);
        eprintln!("Ctrl+R with the body focused: page kept (no reload) = {kept}");
        let received = page(driver, KEY_READ, vec![]).await?;
        eprintln!("chords the document received so far: {received}");
        if !received.as_array().is_some_and(|keys| keys.iter().any(|key| key == "ctrl:r")) {
            bail!("Ctrl+R never reached the document, so its behavior was not observed: {received}");
        }

        press_ctrl(driver, '1').await?;
        ui::visible(driver, KANBAN_BOARD).await.context("Ctrl+1 did not open the Kanban page")?;

        press_ctrl(driver, '4').await?;
        await_text(driver, "main", CONTEXT_HEADING).await.context("Ctrl+4 did not open the Context page")?;

        press_ctrl(driver, ',').await?;
        await_text(driver, "main", SETTINGS_HEADING).await.context("Ctrl+, did not open Settings")?;

        context.close_session(session, "keyboard", &Ok(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}
