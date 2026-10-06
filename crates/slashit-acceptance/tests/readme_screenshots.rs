//! Regenerates the README's screenshots from synthetic data.
//!
//! This is documentation tooling, not a product journey. It lives in its own
//! test target so it is outside `product_acceptance`, outside the CI shard
//! manifest, and never runs unless asked for by name:
//!
//! ```text
//! scripts/capture-readme-screenshots.sh
//! ```
//!
//! Everything on screen comes from the dataset below. The run uses the
//! harness's private state root and fake `claude` and `gh`, so no real
//! project, path, account or reviewer can appear, and nothing is written to
//! the repository: images go to `target/readme-screenshots/` and the wrapper
//! script copies them into `docs/assets/screenshots/` only after a clean run.
//!
//! How the data gets there:
//!
//! 1. Projects and tasks are created through the product's own commands.
//! 2. The application is stopped. Its task store (`tasks.toml`) is then edited
//!    to attach the PR review plan and one failure message, which no command
//!    can set. The store is plain TOML with a `version` field; only the fields
//!    named below are touched.
//! 3. The application is started again, so it loads the seeded store the way
//!    it loads any other. Before anything can run, the queue is told to start
//!    nothing (limit 0, no auto-promotion); only then are the tasks moved into
//!    their columns through `update_task_status`. Seeding statuses in the
//!    store instead would not work: startup puts a task nothing is driving
//!    back on the queue, and the executor would start or fail the rest.
//!
//! The window is pinned to [`WIDTH`] x [`HEIGHT`] CSS pixels and the run
//! fails if the page does not report that size at a device pixel ratio of 1.
//! The app has one theme (dark); there is nothing to pin.

#![cfg(target_os = "linux")]

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use slashit_acceptance::fake_agent::{self, FakeAgent};
use slashit_acceptance::fake_gh::FakeGh;
use slashit_acceptance::{ui, TestContext};
use std::path::{Path, PathBuf};
use thirtyfour::prelude::*;

/// Where the window starts. The app caps its content at 1800px and eight
/// columns need about 2030px, so the board always scrolls sideways. The
/// capture shows Queue through Done, so [`fit_window_to_the_columns`] then
/// adjusts the width until the board's row is exactly as wide as those six
/// columns. Starting from a fixed size and adjusting by measurement gives the
/// same result on every run.
const WIDTH: u32 = 1862;
const HEIGHT: u32 = 1000;

/// Where finished images go, unless `SLASHIT_README_SCREENSHOT_DIR` says
/// otherwise. Untracked.
fn output_dir() -> PathBuf {
    match std::env::var_os("SLASHIT_README_SCREENSHOT_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("the crate sits two levels below the workspace root")
            .join("target/readme-screenshots"),
    }
}

// ---- the synthetic dataset -------------------------------------------------

const PROJECTS: [&str; 3] = ["harbor-api", "harbor-web", "tidepool-docs"];

/// The project whose board is shown. Everything below belongs to it.
const BOARD_PROJECT: &str = "harbor-api";

/// (title, status as stored, category, priority, overall progress)
const TASKS: [(&str, &str, &str, &str, u32); 13] = [
    ("Add rate-limit headers to public endpoints", "backlog", "feature", "medium", 0),
    ("Document webhook retry policy", "backlog", "documentation", "low", 0),
    ("Paginate the audit log endpoint", "backlog", "performance", "medium", 0),
    ("Reconcile ledger totals nightly", "error", "bug_fix", "high", 0),
    ("Add idempotency keys to refunds", "pr_created", "feature", "high", 100),
    ("Paginate the invoice export", "queue", "performance", "medium", 0),
    ("Return 429 with Retry-After", "queue", "feature", "medium", 0),
    ("Cache the plan catalog lookup", "in_progress", "performance", "high", 40),
    ("Validate webhook signatures", "ai_review", "security", "urgent", 100),
    ("Rename tenant_id in billing views", "human_review", "refactoring", "medium", 100),
    ("Fix timezone drift in usage reports", "done", "bug_fix", "medium", 100),
    ("Add health endpoint for the worker", "done", "infrastructure", "low", 100),
    ("Tidy the error enum", "done", "refactoring", "low", 100),
];

/// The task that has a pull request, and that pull request. Display only:
/// nothing here is ever contacted.
const PR_TASK: &str = "Add idempotency keys to refunds";
const PR_URL: &str = "https://github.com/slashit-demo/harbor-api/pull/42";
const PR_BRANCH: &str = "feat/refund-idempotency";

/// The task shown in the Error column, and why it failed.
const FAILED_TASK: &str = "Reconcile ledger totals nightly";
const FAILURE: &str = "Checks failed: 3 of 12 tests did not pass";

/// The comment an earlier apply already fixed and answered. It is older than
/// the last apply, so "Only new since last apply" hides exactly this one.
const PREVIOUS_ROUND: u64 = 9000;

/// What the reviewers said and what the assistant recommended for each.
/// (id, author, association, kind, path, line, comment, summary, decision,
/// reasoning, proposed change)
type Review = (
    u64,
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
    Option<i64>,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

const REVIEW: [Review; 6] = [
    (
        9001,
        "maya-ortiz-demo",
        "OWNER",
        "inline",
        Some("src/refunds/mod.rs"),
        Some(88),
        "This `unwrap()` will panic if the client omits the idempotency key. Please return an error instead.",
        "Unchecked unwrap on the idempotency key",
        "fix",
        "Confirmed: `key.unwrap()` on line 88 of `src/refunds/mod.rs` panics when the header is missing, and the handler has no guard before it. I will return a typed error so the client gets a 400 instead of a dropped connection.",
        "Replace `key.unwrap()` with `key.ok_or(RefundError::MissingKey)?` in src/refunds/mod.rs and map `MissingKey` to HTTP 400.",
    ),
    (
        9003,
        "sam-lindqvist-demo",
        "COLLABORATOR",
        "inline",
        Some("tests/refunds.rs"),
        Some(14),
        "Could these four cases be one table-driven test?",
        "Suggest a table-driven test",
        "skip",
        "Reasonable, but the four cases share no setup, and merging them would make this PR touch every refund test. Better as a follow-up, so I will leave it out of this change.",
        "No change in this PR. Offer to open a follow-up task for the test refactor.",
    ),
    (
        9004,
        "sam-lindqvist-demo",
        "COLLABORATOR",
        "inline",
        Some("src/refunds/mod.rs"),
        Some(120),
        "Why do we not retry when the payment provider answers 409?",
        "Question about retrying on 409",
        "question",
        "A 409 here means the provider already saw this key, so a retry would be a duplicate. I am not sure whether the team wants the first response replayed instead. This needs a product decision.",
        "Hold off on code changes until the retry-or-replay behaviour is decided.",
    ),
    (
        9002,
        "review-bot-demo",
        "NONE",
        "inline",
        Some("src/refunds/keys.rs"),
        Some(31),
        "Comment says keys are 16 bytes but the code takes 12.",
        "Misleading key-length comment",
        "fix",
        "Confirmed: `KEY_LEN` is 12, and the doc comment above it says 16. The test in `tests/refunds.rs` asserts 12, so the comment is the part that is wrong.",
        "Change the comment in src/refunds/keys.rs:29 to `// 12 random bytes, hex-encoded to 24 characters`.",
    ),
    (
        9005,
        "maya-ortiz-demo",
        "OWNER",
        "inline",
        Some("README.md"),
        Some(57),
        "Typo: \"idempotancy\".",
        "Typo in the refunds section",
        "fix",
        "Confirmed: line 57 of README.md spells it \"idempotancy\".",
        "Replace \"idempotancy\" with \"idempotency\" in README.md.",
    ),
    (
        9000,
        "maya-ortiz-demo",
        "OWNER",
        "inline",
        Some("src/refunds/mod.rs"),
        Some(41),
        "Please document what `RefundKey` guarantees.",
        "Missing docs on RefundKey",
        "fix",
        "Done in the previous round: added a doc comment stating that a key is unique per refund request.",
        "Add a doc comment to `RefundKey` in src/refunds/mod.rs.",
    ),
];

// ---- the journey -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn capture_the_readme_screenshots() {
    let context = TestContext::new("readme_screenshots").expect("harness setup");
    let outcome = journey(&context).await;
    context.finish(outcome);
}

async fn journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let out = output_dir();
    std::fs::create_dir_all(&out).with_context(|| format!("could not create {}", out.display()))?;

    // `claude` and `gh` resolve to the harness's fixtures, so nothing on this
    // machine can be reached, and a task that happened to start would run the
    // fake agent rather than a real one.
    let agent = FakeAgent::install(&root)?;
    let _gh = FakeGh::install(&root, &agent)?;
    present_a_plausible_claude_version(&agent)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    // One device pixel per CSS pixel whatever the host's desktop scaling is.
    context.set_child_env("GDK_SCALE", "1");
    context.set_child_env("GDK_DPI_SCALE", "1");

    // 1. Create the data through the product.
    let session = context.start_session("seed").await?;
    let created = create_data(session.driver()).await;
    context.close_session(session, "seed", &created).await?;
    let board_project = created?;

    // 2. With the application stopped, attach what no command can set.
    seed_task_store(&root)?;

    // 3. Start it again on the seeded store, place the tasks, take the pictures.
    let session = context.start_session("capture").await?;
    let shots = place_tasks_and_capture(session.driver(), &board_project, &out).await;
    context.close_session(session, "capture", &shots).await?;
    shots
}

/// The sidebar footer shows what `claude --version` prints, and the fixture
/// answers every call, that one included, with an agent event. Put a small
/// wrapper in front of it that answers only `--version` and hands everything
/// else to the fixture unchanged.
fn present_a_plausible_claude_version(agent: &FakeAgent) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let link = agent.executable();
    let fixture = std::fs::read_link(link).context("the fake agent is not a link to its fixture")?;
    std::fs::remove_file(link)?;
    std::fs::write(
        link,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '2.1.0 (Claude Code)'; exit 0; fi\nexec '{}' \"$@\"\n",
            fixture.display()
        ),
    )?;
    std::fs::set_permissions(link, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

async fn create_data(driver: &WebDriver) -> Result<String> {
    let mut board = None;
    for name in PROJECTS {
        let project = ui::invoke(
            driver,
            "create_project",
            json!({ "name": name, "repositoryId": Value::Null, "agentType": "claude_code" }),
        )
        .await?;
        let id = id_of(&project, "create_project")?;
        if name == BOARD_PROJECT {
            board = Some(id);
        }
    }
    let board = board.context("the board project was not created")?;

    for (title, _, category, priority, _) in TASKS {
        let task = ui::invoke(
            driver,
            "create_task",
            json!({ "params": {
                "projectId": board,
                "title": title,
                "description": Value::Null,
                "model": "default",
                "planningMode": false,
                "dependencies": [],
                "category": category,
                "priority": priority,
                "complexity": Value::Null,
                "impact": Value::Null,
                "securitySeverity": Value::Null,
            }}),
        )
        .await?;
        if title == PR_TASK {
            ui::invoke(
                driver,
                "link_pr",
                json!({ "taskId": id_of(&task, "create_task")?, "prUrl": PR_URL }),
            )
            .await?;
        }
    }
    Ok(board)
}

fn id_of(value: &Value, command: &str) -> Result<String> {
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("{command} returned no id: {value}"))
}

// ---- the task store --------------------------------------------------------

fn seed_task_store(root: &Path) -> Result<()> {
    let store = find_task_store(root)?;
    let text = std::fs::read_to_string(&store)
        .with_context(|| format!("could not read {}", store.display()))?;
    let mut document: toml::Table = text
        .parse()
        .with_context(|| format!("{} is not valid TOML", store.display()))?;
    if document.get("version").and_then(toml::Value::as_integer) != Some(1) {
        bail!(
            "{} has a task-store version this seeder was not written for; check ProjectTasksFile \
             in src-tauri/src/config/storage.rs",
            store.display()
        );
    }
    let tasks = document
        .get_mut("tasks")
        .and_then(toml::Value::as_array_mut)
        .context("the task store has no [[tasks]]")?;

    for (title, ..) in TASKS {
        let task = tasks
            .iter_mut()
            .filter_map(toml::Value::as_table_mut)
            .find(|t| t.get("title").and_then(toml::Value::as_str) == Some(title))
            .with_context(|| format!("the store has no task titled {title:?}"))?;
        if title == PR_TASK {
            task.insert("branch_name".into(), PR_BRANCH.into());
            task.insert("pr_review_plan".into(), review_plan()?);
        }
        if title == FAILED_TASK {
            task.insert("error_message".into(), FAILURE.into());
        }
    }

    // Written whole. The application is stopped, so nothing races this.
    std::fs::write(&store, toml::to_string_pretty(&document)?)
        .with_context(|| format!("could not write {}", store.display()))
}

/// The one `tasks.toml` that holds the board project's tasks.
fn find_task_store(root: &Path) -> Result<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e == "toml")
                && std::fs::read_to_string(&path).is_ok_and(|t| t.contains(PR_TASK))
            {
                found.push(path);
            }
        }
    }
    match found.as_slice() {
        [one] => Ok(one.clone()),
        other => bail!("expected one task store naming {PR_TASK:?}, found {other:?}"),
    }
}

/// `PrReviewPlan`, as `src-tauri/src/domain/task.rs` serializes it.
///
/// Times are fixed, so the plan is the same on every run.
fn review_plan() -> Result<toml::Value> {
    let comments: Vec<Value> = REVIEW
        .iter()
        .map(|r| {
            // The comment from the previous round predates the last apply.
            let created = if r.0 == PREVIOUS_ROUND { "2026-03-09T15:00:00Z" } else { "2026-03-10T09:30:00Z" };
            json!({
                "id": r.0, "kind": r.3, "author": r.1, "author_association": r.2,
                "body": r.6, "path": r.4, "line": r.5,
                "url": format!("{PR_URL}#discussion_r{}", r.0),
                "created_at": created, "updated_at": created,
            })
        })
        .collect();
    let items: Vec<Value> = REVIEW
        .iter()
        .map(|r| {
            json!({
                "comment_id": r.0, "summary": r.7, "decision": r.8, "reasoning": r.9,
                "proposed_change": r.10,
                // Only a Fix from a collaborator starts approved, as in the product.
                "approved": r.8 == "fix" && r.2 == "OWNER" && r.0 != PREVIOUS_ROUND,
                "fix_done": r.0 == PREVIOUS_ROUND,
                "reply_posted": r.0 == PREVIOUS_ROUND,
            })
        })
        .collect();
    let plan = json!({
        "generated_at": "2026-03-10T10:00:00Z",
        "pr_url": PR_URL,
        "review_decision": "CHANGES_REQUESTED",
        "comments": comments,
        "items": items,
        "raw_plan": "",
        "last_apply": {
            "applied_at": "2026-03-09T16:00:00Z",
            "agent_summary": "Added the missing doc comment and replied on the thread.",
            "fixed_ids": [PREVIOUS_ROUND], "skipped_ids": [],
            "pushed": true, "push_branch": PR_BRANCH,
            "replies_posted": 1, "auto_reply": true,
        },
    });
    // JSON null has no TOML form; a missing key reads back as `None`.
    Ok(toml::Value::try_from(strip_nulls(plan))?)
}

fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

// ---- the pictures ----------------------------------------------------------

async fn place_tasks_and_capture(driver: &WebDriver, board_project: &str, out: &Path) -> Result<()> {
    // Nothing may start. Every task is still in Backlog, so there is nothing
    // for the executor to act on in the moment before this lands.
    ui::invoke(
        driver,
        "update_queue_config",
        json!({ "parallelTaskLimit": 0, "autoPromote": false, "useCoderabbit": false }),
    )
    .await?;

    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": board_project })).await?;
    for (title, status, _, _, progress) in TASKS {
        let id = listed
            .as_array()
            .and_then(|all| all.iter().find(|t| t["title"] == title))
            .and_then(|t| t["id"].as_str())
            .with_context(|| format!("no task titled {title:?}"))?
            .to_string();
        if status != "backlog" {
            ui::invoke(driver, "update_task_status", json!({ "taskId": id, "status": status, "closeWithoutMerge": Value::Null }))
                .await
                .with_context(|| format!("could not move {title:?} to {status}"))?;
        }
        if progress > 0 {
            ui::invoke(
                driver,
                "update_task_progress",
                json!({ "taskId": id, "phase": if status == "in_progress" { "coding" } else { "complete" }, "phaseProgress": progress, "overallProgress": progress, "sequenceNumber": 0 }),
            )
            .await?;
        }
    }
    capture(driver, board_project, out).await
}

async fn capture(driver: &WebDriver, board_project: &str, out: &Path) -> Result<()> {
    driver.set_window_rect(0, 0, WIDTH, HEIGHT).await.context("could not size the window")?;
    ui::assert_frontend_is_real(driver).await?;
    assert_pinned_display(driver).await?;

    ui::visible(driver, &format!("[data-testid=\"rail-project-{board_project}\"]"))
        .await?
        .click()
        .await
        .context("could not select the board project")?;
    ui::visible(driver, "[data-testid=\"kanban-board\"]").await?;
    // A click that lands before the page has attached its handler does
    // nothing, so click until the sidebar reports itself collapsed.
    let mut collapsed = false;
    for _ in 0..20 {
        let toggle = ui::visible(driver, "button[aria-label=\"Collapse sidebar\"], button[aria-label=\"Expand sidebar\"]").await?;
        if toggle.attr("aria-expanded").await?.as_deref() == Some("false") {
            collapsed = true;
            break;
        }
        toggle.click().await.context("could not collapse the sidebar")?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    if !collapsed {
        bail!("the sidebar did not collapse");
    }
    wait_for_cards(driver, TASKS.len()).await?;

    fit_window_to_the_columns(driver).await?;
    ui::visible(driver, "[data-testid=\"column-done\"]").await?;

    // Park the pointer, so no card shows a hover state.
    let inner = driver.execute("return window.innerWidth;", vec![]).await?.json().as_i64().context("no viewport width")?;
    if inner != 1680 {
        bail!("expected a 1680px capture viewport, got {inner}px");
    }
    driver.action_chain().move_to(inner - 2, HEIGHT as i64 - 2).perform().await?;
    settle().await;
    write_png(out, "dashboard-kanban.png", &driver.screenshot_as_png().await?)?;

    open_pr_review(driver).await?;
    let modal = modal_element(driver).await?;
    settle().await;
    let rect = modal.rect().await?;
    let shot = modal.screenshot_as_png().await?;
    write_png(out, "pr-comment-review.png", &crop_to(&shot, rect.width as u32, rect.height as u32)?)?;
    Ok(())
}

/// Measure, in CSS pixels: the board row's width, and the span from Queue's
/// left edge to Done's right edge.
const MEASURE: &str = r#"
const row = document.querySelector('[data-testid="kanban-board"] .snap-x');
const queue = document.querySelector('[data-testid="column-queue"]');
const done = document.querySelector('[data-testid="column-done"]');
if (!row || !queue || !done) { return null; }
row.style.scrollBehavior = 'auto';
row.style.scrollSnapType = 'none';
row.scrollLeft += queue.getBoundingClientRect().left - row.getBoundingClientRect().left;
const r = row.getBoundingClientRect();
return [r.width, done.getBoundingClientRect().right - queue.getBoundingClientRect().left,
        queue.getBoundingClientRect().left - r.left, r.right - done.getBoundingClientRect().right];
"#;

/// Size the window so the board shows Queue through Done and nothing else,
/// then scroll to Queue. Fails if it cannot get within a pixel.
async fn fit_window_to_the_columns(driver: &WebDriver) -> Result<()> {
    let mut width = WIDTH;
    for _ in 0..6 {
        let seen = driver.execute(MEASURE, vec![]).await?.json().clone();
        let [row, span, left, right] = seen
            .as_array()
            .and_then(|v| v.iter().map(Value::as_f64).collect::<Option<Vec<_>>>())
            .and_then(|v| <[f64; 4]>::try_from(v).ok())
            .with_context(|| format!("the board has no Queue or Done column to fit to: {seen}"))?;
        if (row - span).abs() <= 1.0 && left.abs() <= 1.0 && right >= -1.0 {
            return Ok(());
        }
        width = (f64::from(width) + (span - row)).round() as u32;
        driver.set_window_rect(0, 0, width, HEIGHT).await?;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    bail!("could not size the window so the board shows exactly Queue through Done (last width {width})")
}

/// The window must be exactly the pinned size at one device pixel per CSS
/// pixel, or the images would differ from run to run and host to host.
async fn assert_pinned_display(driver: &WebDriver) -> Result<()> {
    let seen = driver
        .execute("return [window.innerWidth, window.innerHeight, window.devicePixelRatio];", vec![])
        .await?
        .json()
        .clone();
    if seen != json!([WIDTH, HEIGHT, 1]) {
        bail!("expected a {WIDTH}x{HEIGHT} viewport at device pixel ratio 1, the page reports {seen}");
    }
    Ok(())
}

async fn wait_for_cards(driver: &WebDriver, expected: usize) -> Result<()> {
    for _ in 0..100 {
        let n = driver.find_all(By::Css("[data-testid=\"task-card\"]")).await?.len();
        if n == expected {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    bail!("the board never showed all {expected} seeded cards");
}

/// Open the context menu of the PR task and choose "Review PR comments".
async fn open_pr_review(driver: &WebDriver) -> Result<()> {
    let clicked = driver
        .execute(
            r#"
            const [title] = arguments;
            const card = [...document.querySelectorAll('[data-testid="task-card"]')]
              .find((c) => (c.querySelector('[data-testid="task-title"]') || {}).textContent === title);
            if (!card) { return 'no card'; }
            card.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: 200, clientY: 200 }));
            return 'ok';
            "#,
            vec![json!(PR_TASK)],
        )
        .await?
        .json()
        .clone();
    if clicked != json!("ok") {
        bail!("could not open the card menu for {PR_TASK:?}: {clicked}");
    }
    let item = driver
        .query(By::XPath("//button[contains(normalize-space(.), 'Review PR comments')]"))
        .wait(ui::ELEMENT_TIMEOUT, std::time::Duration::from_millis(150))
        .and_displayed()
        .first()
        .await
        .context("the card menu has no 'Review PR comments' entry")?;
    item.click().await.context("could not choose 'Review PR comments'")
}

async fn modal_element(driver: &WebDriver) -> Result<WebElement> {
    let heading = driver
        .query(By::XPath("//h2[normalize-space(.)='PR Comment Review']"))
        .wait(ui::ELEMENT_TIMEOUT, std::time::Duration::from_millis(150))
        .and_displayed()
        .first()
        .await
        .context("the PR Comment Review modal did not open")?;
    heading
        .find(By::XPath("ancestor::div[contains(@class,'max-w-6xl')][1]"))
        .await
        .context("could not find the modal panel around its heading")
}

/// Let transitions and fonts finish before a frame is taken.
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
}

/// WebKitWebDriver returns an element screenshot taller than the element, with
/// the extra rows black at the bottom (the modal sits 50px below the top of
/// the window and the image is 50px too tall). Keep the element's own box.
fn crop_to(png: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .context("could not decode the element screenshot")?;
    if image.width() < width || image.height() < height {
        bail!(
            "the element screenshot is {}x{}, smaller than the {width}x{height} element",
            image.width(),
            image.height()
        );
    }
    let mut cropped = Vec::new();
    image
        .crop_imm(0, 0, width, height)
        .write_to(&mut std::io::Cursor::new(&mut cropped), image::ImageFormat::Png)
        .context("could not encode the cropped screenshot")?;
    Ok(cropped)
}

fn write_png(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = dir.join(name);
    std::fs::write(&path, bytes).with_context(|| format!("could not write {}", path.display()))?;
    println!("wrote {} ({} bytes)", path.display(), bytes.len());
    Ok(())
}
