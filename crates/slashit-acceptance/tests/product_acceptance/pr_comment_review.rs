// SPDX-License-Identifier: MPL-2.0
//! A public reply to a PR review comment is posted only when the person selects
//! it, and only with the exact text the dialog showed them.
//!
//! A real task is approved, so its branch is delivered and its pull request
//! exists. Its persisted review plan is then written onto its board: two
//! threaded replies, a fix with no public reply, a PR-level reply whose text
//! names its comment, and a PR-level reply an earlier attempt may already have
//! posted. Its approvals are the ones Apply records, the only path that persists
//! a person's approval of a fix. The journey then uses the rendered PR Comment
//! Review: it checks the replies offered for approved fixes, sends a tampered
//! approval through the command boundary, posts one selected reply, posts the
//! uncertain reply only after accepting its duplicate risk, presses Apply, and
//! reopens the dialog after a restart. Every public write is read back from the
//! fake `gh`, which holds the exact body it was given.

use super::human_review::{approve_and_open_the_pull_request, click, install_fakes, open_card_menu};
use super::*;
use slashit_acceptance::fake_gh::FakeGh;

const MODAL: &str = "[data-testid=\"pr-review-modal\"]";
const MENU_ANALYZE: &str = "[data-testid=\"task-menu-analyze-pr\"]";
const ITEM: &str = "[data-testid=\"pr-review-item\"]";
const ITEM_APPROVE: &str = "[data-testid=\"pr-review-item-approve\"]";
const REPLY: &str = "[data-testid=\"pr-review-reply\"]";
const REPLY_SELECT: &str = "[data-testid=\"pr-review-reply-select\"]";
const REPLY_TEXT: &str = "[data-testid=\"pr-review-reply-text\"]";
const DUPLICATE: &str = "[data-testid=\"pr-review-reply-duplicate\"]";
const NO_REPLY: &str = "[data-testid=\"pr-review-no-reply\"]";
const NO_REPLY_ITEM: &str = "[data-testid=\"pr-review-no-reply-item\"]";
const POST: &str = "[data-testid=\"pr-review-post-replies\"]";
const APPLY: &str = "[data-testid=\"pr-review-apply\"]";
const ERROR: &str = "[data-testid=\"pr-review-error\"]";
const CLOSE: &str = "[data-testid=\"pr-review-close\"]";

/// The five comments of the seeded review. Inline comments are answered in
/// their own thread; the others are answered at PR level.
const THREAD_RENAME: u64 = 1001;
const THREAD_TEST: u64 = 1002;
const THREAD_MESSAGE: u64 = 1003;
const PR_CHANGELOG: u64 = 900;
const PR_DOCS: u64 = 901;

const RENAME_REPLY: &str = "Renamed the helper to say what it does.";
const TEST_REPLY: &str = "Added the test for empty input.";
const CHANGELOG_REPLY: &str = "Added a changelog entry under Unreleased.";
const DOCS_REPLY: &str = "Updated the docs for the new flag.";
const MESSAGE_SUMMARY: &str = "Clarify the error message";
/// Internal reasoning for the fix that has no public reply. It must never be
/// shown as a reply or posted.
const MESSAGE_REASONING: &str = "INTERNAL-REASONING-MESSAGE: the message is already clear enough.";

/// The pull request the approval opened, as the fake `gh` numbers it.
const PR_NUMBER: &str = "1";
const REPOSITORY: &str = "slashit-acceptance/fixture";

#[tokio::test(flavor = "multi_thread")]
async fn a_public_reply_is_posted_only_as_the_person_approved_its_exact_text() {
    let context = TestContext::new("pr_comment_review").expect("harness setup");
    let outcome = journey(&context).await;
    context.finish(outcome);
}

async fn journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = install_fakes(context, &root)?;
    // Real work, so the branch holds a commit the seeded fixes can name.
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    repository.point_origin_at_github()?;
    repository.serve_github_from_origin()?;

    // Launch one: a real approval delivers the branch and opens the pull request.
    let session = context.start_session("opened").await?;
    let outcome = approve_and_open_the_pull_request(session.driver(), &agent, &gh, &repository).await;
    context.close_session(session, "opened", &outcome).await?;
    let (executed, pr_url) = outcome?;
    seed_review_plan(&root, &repository, &executed, &pr_url)?;

    // Launch two: the dialog, with every approval and selection made by hand.
    let session = context.start_session("review").await?;
    let outcome = review(session.driver(), &gh, &executed, &root).await;
    context.close_session(session, "review", &outcome).await?;
    outcome?;

    // Launch three: nothing approved before the restart is carried over.
    let session = context.start_session("reopened").await?;
    let outcome = reopened(session.driver(), &gh, &executed, &root).await;
    context.close_session(session, "reopened", &outcome).await?;
    outcome
}

/// Writes a persisted review plan onto the Task's board, the way Analyze would
/// have saved it. Every fix names the delivered commit with its recorded
/// effect, so the delivery check can prove it on the pull request's branch.
fn seed_review_plan(root: &Path, repository: &GitFixture, executed: &ExecutedTask, pr_url: &str) -> Result<()> {
    let file = board_holding(root, &executed.id)?;
    let mut document: toml::Value = std::fs::read_to_string(&file)
        .with_context(|| format!("could not read {}", file.display()))?
        .parse()
        .context("the board is not TOML")?;
    let branch = task_record(&document, &executed.id)
        .and_then(|record| record.get("branch_name"))
        .and_then(toml::Value::as_str)
        .context("the task records no branch")?
        .to_string();
    let commit = repository
        .branch_tip(&branch)?
        .with_context(|| format!("the branch {branch} is missing"))?;
    let directory = repository.path_buf();
    let parent = rev(&directory, &format!("{commit}^"))?;
    let effect = json!({
        "before_tree": rev(&directory, &format!("{parent}^{{tree}}"))?,
        "after_tree": rev(&directory, &format!("{commit}^{{tree}}"))?,
    });
    let plan = plan_json(pr_url, &commit, &effect);
    let plan = toml::Value::try_from(plan).context("the seeded plan is not representable as TOML")?;

    let tasks = document
        .get_mut("tasks")
        .and_then(toml::Value::as_array_mut)
        .context("the board has no task list")?;
    // Written onto the last table only: the seeded task is the only one this
    // journey creates, and a plan written onto any other would be silently wrong.
    let last = tasks.last_mut().and_then(toml::Value::as_table_mut).context("the board has no task")?;
    if last.get("id").and_then(toml::Value::as_str) != Some(executed.id.as_str()) {
        bail!("the seeded task is not the last task on its board");
    }
    last.insert("pr_review_plan".to_string(), plan);
    std::fs::write(&file, toml::to_string(&document)?)
        .with_context(|| format!("could not write {}", file.display()))
}

fn plan_json(pr_url: &str, commit: &str, effect: &Value) -> Value {
    let comment = |id: u64, kind: &str, author: &str, association: &str, body: &str, location: Option<(&str, i64)>| {
        let mut comment = json!({
            "id": id,
            "kind": kind,
            "author": author,
            "author_association": association,
            "body": body,
            "url": format!("{pr_url}#comment-{id}"),
        });
        if let Some((path, line)) = location {
            comment["path"] = json!(path);
            comment["line"] = json!(line);
        }
        comment
    };
    let item = |comment_id: u64, summary: &str, reasoning: &str, public_reply: Option<&str>, uncertain: bool| {
        let mut item = json!({
            "comment_id": comment_id,
            "summary": summary,
            "decision": "fix",
            "reasoning": reasoning,
            "proposed_change": "Make the change the comment asks for.",
            // Recorded by Apply, which is the only path that persists a
            // person's approval of a fix. Sync posts nothing for an approval
            // the stored plan does not hold.
            "approved": true,
            "user_note": "",
            "fix_done": true,
            "fix_uncommitted": false,
            "fix_commit": commit,
            "fix_effect": effect,
            "reply_posted": false,
            "reply_uncertain": uncertain,
        });
        if let Some(reply) = public_reply {
            item["pr_reply_text"] = json!(reply);
        }
        item
    };
    json!({
        "generated_at": "2026-11-16T10:00:00Z",
        "pr_url": pr_url,
        "review_decision": "REVIEW_REQUIRED",
        "comments": [
            comment(THREAD_RENAME, "inline", "reviewer-one", "MEMBER", "Rename this helper.", Some(("src/lib.rs", 10))),
            comment(THREAD_TEST, "inline", "reviewer-one", "MEMBER", "Please add a test for empty input.", Some(("src/lib.rs", 20))),
            comment(THREAD_MESSAGE, "inline", "reviewer-two", "CONTRIBUTOR", "The error message is unclear.", Some(("src/lib.rs", 30))),
            comment(PR_CHANGELOG, "conversation", "reviewer-two", "CONTRIBUTOR", "Could you add a changelog entry?", None),
            comment(PR_DOCS, "conversation", "reviewer-two", "CONTRIBUTOR", "Please update the docs.", None),
        ],
        "items": [
            item(THREAD_RENAME, "Rename the helper", "Internal: the old name is misleading.", Some(RENAME_REPLY), false),
            item(THREAD_TEST, "Add the empty-input test", "Internal: the case is not covered yet.", Some(TEST_REPLY), false),
            item(THREAD_MESSAGE, MESSAGE_SUMMARY, MESSAGE_REASONING, None, false),
            item(PR_CHANGELOG, "Add a changelog entry", "Internal: the project keeps one.", Some(CHANGELOG_REPLY), false),
            item(PR_DOCS, "Update the docs", "Internal: the flag is documented elsewhere.", Some(DOCS_REPLY), true),
        ],
        "raw_plan": "",
    })
}

/// The one board on disk that holds the Task.
fn board_holding(root: &Path, task_id: &str) -> Result<PathBuf> {
    let mut holders = Vec::new();
    for file in toml_files(root)? {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let Ok(document) = text.parse::<toml::Value>() else { continue };
        if task_record(&document, task_id).is_some() {
            holders.push(file);
        }
    }
    match holders.as_slice() {
        [only] => Ok(only.clone()),
        other => bail!("expected one board holding task {task_id}, found {}", other.len()),
    }
}

fn rev(directory: &Path, spec: &str) -> Result<String> {
    let output = super::git_support::git_command(directory, &["rev-parse", "--verify", spec])
        .output()
        .with_context(|| format!("could not resolve {spec}"))?;
    if !output.status.success() {
        bail!("git could not resolve {spec}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The dialog as a person sees it on the first launch. Nothing is approved
/// yet, so no reply is offered, and only the approvals and selections the
/// person makes move anything.
async fn review(driver: &WebDriver, gh: &FakeGh, executed: &ExecutedTask, root: &Path) -> Result<()> {
    open_dialog(driver, executed).await?;
    await_count(driver, ITEM, 5).await?;
    for comment in [THREAD_RENAME, THREAD_TEST, THREAD_MESSAGE, PR_CHANGELOG, PR_DOCS] {
        if !is_checked(driver, &item_part(comment, ITEM_APPROVE)).await? {
            bail!("the approved fix for comment {comment} is shown unapproved");
        }
    }
    // Approved fixes offer their replies, every reply unchecked. The reasoning
    // for a fix with no public reply is never offered as one.
    await_count(driver, REPLY, 4).await?;
    if !disabled(driver, POST).await? {
        bail!("Post is enabled before any reply is selected");
    }
    if !gh.posts()?.is_empty() {
        bail!("a public reply was posted before anyone selected one");
    }
    for comment in [THREAD_RENAME, THREAD_TEST, PR_CHANGELOG, PR_DOCS] {
        if is_checked(driver, &reply_part(comment, REPLY_SELECT)).await? {
            bail!("approving the fix for comment {comment} selected its reply");
        }
    }
    if count(driver, &reply_part(THREAD_MESSAGE, REPLY)).await? != 0 {
        bail!("the fix with no public reply is offered as a reply");
    }
    await_count(driver, NO_REPLY_ITEM, 1).await?;
    let no_reply = text_of(driver, NO_REPLY_ITEM).await?.unwrap_or_default();
    if !no_reply.contains(MESSAGE_SUMMARY) {
        bail!("the fix with no public reply is not named in its section: {no_reply:?}");
    }
    if count(driver, &format!("{NO_REPLY} {REPLY_SELECT}")).await? != 0 {
        bail!("a fix with no public reply can be selected");
    }
    for comment in [THREAD_RENAME, THREAD_TEST, PR_CHANGELOG, PR_DOCS] {
        let shown = reply_text(driver, comment).await?;
        if shown.contains("INTERNAL-") {
            bail!("internal reasoning is shown as the public reply to comment {comment}: {shown:?}");
        }
    }
    let renamed = reply_text(driver, THREAD_RENAME).await?;
    if renamed != RENAME_REPLY {
        bail!("the reply to the thread shows {renamed:?}, not the text the fix wrote");
    }
    let changelog = reply_text(driver, PR_CHANGELOG).await?;
    if !changelog.ends_with(CHANGELOG_REPLY) || changelog == CHANGELOG_REPLY {
        bail!("the PR-level reply does not show the line that names its comment: {changelog:?}");
    }
    let docs_displayed = reply_text(driver, PR_DOCS).await?;
    if count(driver, &reply_part(PR_DOCS, DUPLICATE)).await? != 1 {
        bail!("the uncertain PR-level reply does not show the duplicate-risk warning");
    }
    for comment in [THREAD_RENAME, THREAD_TEST, PR_CHANGELOG] {
        if count(driver, &reply_part(comment, DUPLICATE)).await? != 0 {
            bail!("comment {comment} shows a duplicate-risk warning it does not have");
        }
    }
    if !gh.posts()?.is_empty() {
        bail!("approving a reply posted it");
    }

    // The exact-text check is at the command boundary: a tampered approval is refused, nothing is written.
    let before = board_plan(root, &executed.id)?;
    let refused = ui::invoke_expecting_refusal(
        driver,
        "sync_pr_review_replies",
        json!({
            "taskId": executed.id,
            "approved": [{ "comment_id": THREAD_TEST, "body": "A reply the person never saw.", "accept_possible_duplicate": false }],
        }),
    )
    .await?;
    if !refused.contains("changed after you reviewed") {
        bail!("a tampered approval was refused for another reason: {refused}");
    }
    if !gh.posts()?.is_empty() {
        bail!("a tampered approval posted a reply");
    }
    if board_plan(root, &executed.id)? != before {
        bail!("a refused approval changed the stored review plan");
    }

    // Selecting one reply selects nothing else, and Post sends exactly that one.
    click(driver, &reply_part(THREAD_RENAME, REPLY_SELECT), "the selection of a reply").await?;
    if !is_checked(driver, &reply_part(THREAD_RENAME, REPLY_SELECT)).await? {
        bail!("the selected reply is not checked");
    }
    for comment in [THREAD_TEST, PR_CHANGELOG, PR_DOCS] {
        if is_checked(driver, &reply_part(comment, REPLY_SELECT)).await? {
            bail!("selecting the reply to comment {THREAD_RENAME} also selected comment {comment}");
        }
    }
    await_text(driver, POST, |t| t.contains("Post 1 selected"), "the count of selected replies").await?;
    click(driver, POST, "Post selected replies").await?;
    await_posts(gh, 1).await?;
    let first = gh.posts()?;
    if first[0].body != RENAME_REPLY || first[0].target != format!("POST repos/{REPOSITORY}/pulls/{PR_NUMBER}/comments/{THREAD_RENAME}/replies") {
        bail!("the first public write is {first:?}, not the one reply selected");
    }
    await_count(driver, &reply_part(THREAD_RENAME, REPLY), 0).await?;

    // Posting one reply keeps the other approvals, which the stored plan holds.
    await_count(driver, REPLY, 3).await?;
    for comment in [THREAD_TEST, PR_CHANGELOG, PR_DOCS] {
        if !is_checked(driver, &item_part(comment, ITEM_APPROVE)).await? {
            bail!("posting comment {THREAD_RENAME} dropped the approval of comment {comment}");
        }
    }
    if gh.posts()?.len() != 1 {
        bail!("posting one reply changed the public replies: {:?}", gh.posts()?);
    }

    // The uncertain reply is held: nothing is selected, so nothing is posted.
    if !disabled(driver, POST).await? {
        bail!("Post is enabled with nothing selected");
    }
    if gh.posts()?.len() != 1 {
        bail!("an uncertain reply was posted without a selection");
    }
    // Selecting it accepts the duplicate risk shown beside it, and only then posts it.
    click(driver, &reply_part(PR_DOCS, REPLY_SELECT), "the selection of the uncertain reply").await?;
    if count(driver, &reply_part(PR_DOCS, DUPLICATE)).await? != 1 {
        bail!("the uncertain reply lost its duplicate-risk warning when selected");
    }
    if reply_text(driver, PR_DOCS).await? != docs_displayed {
        bail!("the uncertain reply's text changed between the approval and the selection");
    }
    click(driver, POST, "Post selected replies").await?;
    await_posts(gh, 2).await?;
    let second = gh.posts()?;
    if second[1].body != docs_displayed || second[1].target != format!("PR-LEVEL {PR_NUMBER}") {
        bail!("the second public write {:?} is not the text shown for the uncertain reply", second[1]);
    }
    if !second[1].body.ends_with(DOCS_REPLY) || second[1].body == DOCS_REPLY {
        bail!("the PR-level reply does not carry the naming line: {:?}", second[1].body);
    }
    await_count(driver, &reply_part(PR_DOCS, REPLY), 0).await?;

    // Apply never publishes a reply, whatever happens to it. This plan has no
    // checkout provenance, so Apply refuses it before any edit or reply. It must
    // be refused, and the refusal must post nothing.
    let before_apply = gh.invocations()?.len();
    click(driver, APPLY, "Apply").await?;
    let refusal = await_text(driver, ERROR, |t| !t.is_empty(), "Apply's refusal").await?;
    if !refusal.contains("does not record the checkout state") {
        bail!("Apply was not refused for its missing provenance: {refusal:?}");
    }
    let after_apply = gh.invocations()?;
    if after_apply.len() < before_apply {
        bail!("the fake gh history shrank");
    }
    if after_apply[before_apply..].iter().any(|args| publishes(args)) {
        bail!("Apply reached GitHub with a public write: {:?}", &after_apply[before_apply..]);
    }
    if gh.posts()?.len() != 2 {
        bail!("Apply changed the public replies: {:?}", gh.posts()?);
    }

    // Closing and reopening shows the stored plan: the two replies still pending
    // are offered, unchecked, and nothing is selected by reopening.
    click(driver, CLOSE, "Close").await?;
    await_gone(driver, MODAL, "the dialog after Close").await?;
    open_dialog(driver, executed).await?;
    await_count(driver, REPLY, 2).await?;
    for comment in [THREAD_TEST, PR_CHANGELOG] {
        if is_checked(driver, &reply_part(comment, REPLY_SELECT)).await? {
            bail!("reopening the dialog selected the reply to comment {comment}");
        }
    }
    if !disabled(driver, POST).await? {
        bail!("Post is enabled after reopening with nothing selected");
    }
    if gh.posts()?.len() != 2 {
        bail!("reopening the dialog posted a reply");
    }
    Ok(())
}

/// After a restart the dialog approves nothing by itself and posts nothing on
/// open. The persisted plan records which replies were published and no others.
async fn reopened(driver: &WebDriver, gh: &FakeGh, executed: &ExecutedTask, root: &Path) -> Result<()> {
    open_dialog(driver, executed).await?;
    await_count(driver, ITEM, 5).await?;
    await_count(driver, REPLY, 2).await?;
    for comment in [THREAD_TEST, PR_CHANGELOG] {
        if is_checked(driver, &reply_part(comment, REPLY_SELECT)).await? {
            bail!("a restart selected the reply to comment {comment}");
        }
    }
    if !disabled(driver, POST).await? {
        bail!("Post is enabled after a restart with nothing selected");
    }
    if gh.posts()?.len() != 2 {
        bail!("a restart changed the public replies: {:?}", gh.posts()?);
    }
    let plan = board_plan(root, &executed.id)?;
    let posted: Vec<(u64, bool)> = plan["items"]
        .as_array()
        .context("the stored plan has no items")?
        .iter()
        .map(|item| {
            (
                item["comment_id"].as_u64().unwrap_or_default(),
                item["reply_posted"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    let expected = [
        (THREAD_RENAME, true),
        (THREAD_TEST, false),
        (THREAD_MESSAGE, false),
        (PR_CHANGELOG, false),
        (PR_DOCS, true),
    ];
    if posted != expected {
        bail!("the stored reply records are {posted:?}, expected {expected:?}");
    }
    Ok(())
}

async fn open_dialog(driver: &WebDriver, executed: &ExecutedTask) -> Result<()> {
    await_card(driver, &executed.title).await?;
    open_card_menu(driver, &executed.title).await?;
    click(driver, MENU_ANALYZE, "Analyze PR comments").await?;
    ui::visible(driver, MODAL).await.context("the PR Comment Review did not open")?;
    Ok(())
}

/// Whether a recorded `gh` call wrote to the pull request: a PR-level comment
/// or any API call that is not a read.
fn publishes(args: &[String]) -> bool {
    let is_comment = args.first().map(String::as_str) == Some("pr") && args.get(1).map(String::as_str) == Some("comment");
    let is_write = args.first().map(String::as_str) == Some("api") && args.iter().any(|a| a == "-X");
    is_comment || is_write
}

fn board_plan(root: &Path, task_id: &str) -> Result<Value> {
    let file = board_holding(root, task_id)?;
    let document: toml::Value = std::fs::read_to_string(&file)?.parse()?;
    let record = task_record(&document, task_id).context("the task is missing from its board")?;
    let plan = record.get("pr_review_plan").context("the board holds no review plan")?;
    Ok(serde_json::to_value(plan)?)
}

/// Waits until the board shows the Task's card, so the menu is not opened on a
/// board that is still rendering.
async fn await_card(driver: &WebDriver, title: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let shown = page(
            driver,
            r#"
            const [title] = arguments;
            for (const card of document.querySelectorAll('[data-testid="task-card"]')) {
                const t = card.querySelector('[data-testid="task-title"]');
                if (t && t.textContent === title) { return true; }
            }
            return false;
            "#,
            vec![json!(title)],
        )
        .await?;
        if shown == json!(true) {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the board never showed the card titled {title:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

fn item_part(comment: u64, part: &str) -> String {
    format!("{ITEM}[data-comment-id=\"{comment}\"] {part}")
}

fn reply_part(comment: u64, part: &str) -> String {
    format!("{REPLY}[data-comment-id=\"{comment}\"] {part}")
}

async fn reply_text(driver: &WebDriver, comment: u64) -> Result<String> {
    Ok(text_of(driver, &reply_part(comment, REPLY_TEXT))
        .await?
        .with_context(|| format!("comment {comment} has no reply on screen"))?
        .trim()
        .to_string())
}

async fn is_checked(driver: &WebDriver, selector: &str) -> Result<bool> {
    Ok(ui::visible(driver, selector).await?.is_selected().await?)
}

async fn disabled(driver: &WebDriver, selector: &str) -> Result<bool> {
    Ok(!ui::visible(driver, selector).await?.is_enabled().await?)
}

async fn await_count(driver: &WebDriver, selector: &str, expected: usize) -> Result<()> {
    let started = Instant::now();
    loop {
        let found = count(driver, selector).await?;
        if found == expected {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("{selector} shows {found} elements, expected {expected}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn await_gone(driver: &WebDriver, selector: &str, what: &str) -> Result<()> {
    let started = Instant::now();
    while count(driver, selector).await? > 0 {
        if started.elapsed() > RENDER_DEADLINE {
            bail!("{what} is still shown");
        }
        tokio::time::sleep(POLL).await;
    }
    Ok(())
}

async fn await_posts(gh: &FakeGh, expected: usize) -> Result<()> {
    let started = Instant::now();
    loop {
        let found = gh.posts()?.len();
        if found == expected {
            return Ok(());
        }
        if found > expected {
            bail!("{found} public replies were posted, expected {expected}");
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("{found} public replies were posted, expected {expected}");
        }
        tokio::time::sleep(POLL).await;
    }
}
