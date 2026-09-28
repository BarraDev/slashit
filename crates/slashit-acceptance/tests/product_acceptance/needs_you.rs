//! Needs you: which tasks cannot make progress without the user.
//!
//! Nothing about attention is stored, so every journey reads it where a
//! person does -- the card chip, the header count, the project rail -- and
//! holds it against the record the product reports and, across a restart,
//! against the task file. Tasks reach Error and Human Review either through a
//! real run or through `update_task_status`, which is what dragging a card
//! sends.

use super::human_review::{
    attribute, await_task, click, install_fakes, open_reviewed_drawer, persisted_review, read_task,
    HR_APPROVE, HR_APPROVED, HR_DELIVERY, HR_FEEDBACK, HR_REQUEST_CHANGES, HR_RETRY_PR,
    HR_SEND_FEEDBACK, FEEDBACK,
};
use super::*;
use slashit_acceptance::fake_gh::{self, FakeGh};

const HEADER_NEEDS_YOU: &str = "[data-testid=\"header-needs-you\"]";
const HR_DELIVERING: &str = "[data-testid=\"human-review-delivering\"]";
const HR_ATTEMPT: &str = "[data-testid=\"human-review-attempt\"]";
const PR_CREATED_COLUMN: &str = "[data-testid=\"column-prcreated\"]";

/// Long enough that a 250 ms poll sees the attempt running, the way a person
/// sees a real push and GitHub round trip.
const SLOW_CREATE_SECONDS: u32 = 4;

/// What one card shows, read in one pass so the parts agree with each other.
#[derive(Debug, Clone, Default)]
struct Card {
    /// `data-reason` of the "Needs you" chip, if there is one.
    reason: Option<String>,
    chip_text: Option<String>,
    chip_classes: Option<String>,
    delivering: bool,
    /// The text of the card's human decision badge, if shown.
    decision: Option<String>,
    sync_pr: bool,
    text: String,
    highlighted: bool,
    focused: bool,
    /// The card's own pixel is what is on screen at its centre.
    on_screen: bool,
}

const READ_CARD: &str = r#"
const card = document.querySelector(`[data-card-task-id="${arguments[0]}"]`);
if (!card) return null;
const chip = card.querySelector('[data-testid="task-card-attention"]');
const decision = card.querySelector('[data-testid="task-card-human-decision"]');
const r = card.getBoundingClientRect();
const x = r.left + r.width / 2, y = r.top + Math.min(r.height / 2, 24);
const hit = (x >= 0 && y >= 0 && x < innerWidth && y < innerHeight) ? document.elementFromPoint(x, y) : null;
return {
  reason: chip ? chip.getAttribute('data-reason') : null,
  chip_text: chip ? chip.textContent.trim() : null,
  chip_classes: chip ? chip.className : null,
  delivering: !!card.querySelector('[data-testid="task-card-delivering"]'),
  decision: decision ? decision.textContent.trim() : null,
  sync_pr: !!card.querySelector('[data-testid="task-card-sync-pr"]'),
  text: card.textContent,
  highlighted: card.getAttribute('data-attention-highlight') === 'true',
  focused: document.activeElement === card,
  on_screen: !!hit && card.contains(hit),
};
"#;

async fn card(driver: &WebDriver, task_id: &str) -> Result<Option<Card>> {
    let found = page(driver, READ_CARD, vec![Value::String(task_id.to_string())]).await?;
    if found.is_null() {
        return Ok(None);
    }
    let text = |key: &str| found.get(key).and_then(Value::as_str).map(str::to_string);
    let flag = |key: &str| found.get(key).and_then(Value::as_bool).unwrap_or(false);
    Ok(Some(Card {
        reason: text("reason"),
        chip_text: text("chip_text"),
        chip_classes: text("chip_classes"),
        delivering: flag("delivering"),
        decision: text("decision"),
        sync_pr: flag("sync_pr"),
        text: text("text").unwrap_or_default(),
        highlighted: flag("highlighted"),
        focused: flag("focused"),
        on_screen: flag("on_screen"),
    }))
}

/// Wait until the card for `task_id` satisfies `accept`.
async fn await_card(
    driver: &WebDriver,
    task_id: &str,
    what: &str,
    accept: impl Fn(&Card) -> bool,
) -> Result<Card> {
    let started = Instant::now();
    let mut last = None;
    loop {
        if let Some(shown) = card(driver, task_id).await? {
            if accept(&shown) {
                return Ok(shown);
            }
            last = Some(shown);
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the card for {task_id} never showed {what}; it last showed {last:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn await_reason(driver: &WebDriver, task_id: &str, reason: Option<&str>) -> Result<Card> {
    await_card(driver, task_id, &format!("attention {reason:?}"), |c| {
        c.reason.as_deref() == reason && !c.delivering
    })
    .await
}

async fn header_count(driver: &WebDriver) -> Result<Option<u64>> {
    Ok(attribute(driver, HEADER_NEEDS_YOU, "data-count")
        .await?
        .and_then(|c| c.parse().ok()))
}

async fn await_header(driver: &WebDriver, expected: u64) -> Result<()> {
    let started = Instant::now();
    loop {
        let shown = header_count(driver).await?;
        if shown == Some(expected) {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the header never said Needs you: {expected}; it says {shown:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The rail's badge for a project; `None` when it shows none.
async fn rail_count(driver: &WebDriver, project_id: &str) -> Result<Option<u64>> {
    let found = page(
        driver,
        "const e = document.querySelector(arguments[0]); return e ? e.getAttribute('data-count') : null;",
        vec![Value::String(format!("[data-testid=\"rail-attention-{project_id}\"]"))],
    )
    .await?;
    Ok(found.as_str().and_then(|c| c.parse().ok()))
}

async fn await_rail(driver: &WebDriver, project_id: &str, expected: Option<u64>) -> Result<()> {
    let started = Instant::now();
    loop {
        let shown = rail_count(driver, project_id).await?;
        if shown == expected {
            return Ok(());
        }
        // Longer than a render: an unselected project is read on the rail's
        // own poll.
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the rail badge for {project_id} never showed {expected:?}; it shows {shown:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn move_to(driver: &WebDriver, task_id: &str, status: &str) -> Result<()> {
    ui::invoke(driver, "update_task_status", json!({ "taskId": task_id, "status": status })).await?;
    Ok(())
}

fn assert_chip(card: &Card, reason: &str, text: &str, colour: &str) -> Result<()> {
    if card.reason.as_deref() != Some(reason) || card.chip_text.as_deref() != Some(text) {
        bail!("expected the chip {text:?}, the card shows {:?} ({:?})", card.chip_text, card.reason);
    }
    if !card.chip_classes.as_deref().is_some_and(|c| c.contains(colour)) {
        bail!("the {text:?} chip is not {colour}: {:?}", card.chip_classes);
    }
    Ok(())
}

// --- Journey A: current-project attention ------------------------------------

/// A failed task and an undecided review need the user; a task in the backlog
/// does not. The header counts both, and pressing it walks the board to each
/// in turn, bringing a Human Review card that starts off-screen into view.
#[tokio::test(flavor = "multi_thread")]
async fn the_header_counts_what_needs_you_and_takes_you_to_it() {
    let context = TestContext::new("needs_you_current_project").expect("harness setup");
    let outcome = current_project_journey(&context).await;
    context.finish(outcome);
}

async fn current_project_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    let session = context.start_session("needs-you").await?;
    let outcome = walk_to_what_needs_you(session.driver(), &repository).await;
    context.close_session(session, "needs-you", &outcome).await?;
    outcome
}

async fn walk_to_what_needs_you(driver: &WebDriver, repository: &GitFixture) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    // Narrow enough that Human Review, the sixth column, starts off-screen.
    // Best effort: the precondition below is what the journey relies on.
    let _ = driver.set_window_rect(0, 0, 1200, 860).await;

    let Prerequisites { project_id, task_id: failed, .. } =
        create_prerequisites(driver, repository, "Needs you failed", "Never runs.").await?;
    let (review, _) = create_task_in(driver, &project_id, "Needs you review").await?;
    let (ordinary, _) = create_task_in(driver, &project_id, "Needs you ordinary").await?;
    move_to(driver, &failed, "error").await?;
    move_to(driver, &review, "human_review").await?;

    open_board(driver, &project_id).await?;
    await_header(driver, 2).await?;
    let failed_card = await_reason(driver, &failed, Some("failed")).await?;
    assert_chip(&failed_card, "failed", "Needs you \u{00B7} Failed", "red")?;
    let review_card = await_reason(driver, &review, Some("review")).await?;
    assert_chip(&review_card, "review", "Needs you \u{00B7} Review", "violet")?;
    await_reason(driver, &ordinary, None).await?;

    // Precondition: the Human Review card is not on screen yet. A window
    // manager may ignore the resize above (a wide Wayland desktop does), and
    // then every column fits; narrowing the board's own horizontal scroller
    // stands in for a narrower window. The scrolling and focusing under test
    // are still the product's.
    page(
        driver,
        "const s = document.querySelector('[data-testid=\"kanban-board\"] .overflow-x-auto'); \
         if (s && s.scrollWidth <= s.clientWidth + 300) { s.style.maxWidth = '700px'; } \
         if (s) { s.scrollLeft = 0; } return null;",
        vec![],
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    if card(driver, &review).await?.is_some_and(|c| c.on_screen) {
        bail!("the Human Review card is already on screen, so this journey cannot prove the header scrolls to it");
    }

    // First press: the first task in board order, the failure in Error.
    click(driver, HEADER_NEEDS_YOU, "Needs you").await?;
    await_card(driver, &failed, "the header's highlight", |c| c.highlighted && c.on_screen).await?;

    // Second press: the review, scrolled into view, outlined and focused.
    click(driver, HEADER_NEEDS_YOU, "Needs you, again").await?;
    let shown = await_card(driver, &review, "the header's highlight, on screen", |c| {
        c.highlighted && c.on_screen && c.focused
    })
    .await?;
    if card(driver, &failed).await?.is_some_and(|c| c.highlighted) {
        bail!("two cards are outlined at once");
    }
    // Pointing at a card changes nothing about it.
    if shown.reason.as_deref() != Some("review") {
        bail!("the highlight changed the card: {shown:?}");
    }
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    for (id, status) in [(&failed, "error"), (&review, "human_review"), (&ordinary, "backlog")] {
        let task = find_task(&listed, id).context("a task disappeared")?;
        if status_of(&task) != Some(status) {
            bail!("pressing Needs you moved {id} to {:?}", status_of(&task));
        }
    }

    // The outline is a cue, not a state.
    await_card(driver, &review, "the highlight to fade", |c| !c.highlighted).await?;
    Ok(())
}

// --- Journeys B and E: PR delivery attention, and a restart -------------------

/// Approving with a pull request that cannot be opened: while the attempt
/// runs the task waits on SlashIt, not on the user; once it fails the card
/// says "PR not created" instead of a green Approved, without the raw error.
/// A retry that fails again is visibly a new attempt. After a restart the
/// same reasons are derived from the task file alone, and a retry that
/// succeeds clears the attention and shows the pull request.
#[tokio::test(flavor = "multi_thread")]
async fn a_pull_request_that_was_not_created_needs_you_until_a_retry_opens_it() {
    let context = TestContext::new("needs_you_delivery").expect("harness setup");
    let outcome = delivery_journey(&context).await;
    context.finish(outcome);
}

async fn delivery_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (agent, gh) = install_fakes(context, &root)?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    repository.point_origin_at_github()?;
    gh.fail_pr_creation(true)?;
    gh.delay_pr_creation(Some(SLOW_CREATE_SECONDS))?;

    let session = context.start_session("delivery").await?;
    let outcome = fail_delivery_twice(session.driver(), &agent, &gh, &repository).await;
    context.close_session(session, "delivery", &outcome).await?;
    let (executed, failed, review) = outcome?;

    // Nothing about attention was written down: the task file holds the
    // decision and the failure, and nothing else is needed to derive it.
    let persisted = persisted_review(&root, &executed.id)?;
    if persisted.status != "human_review" || !persisted.pr_error.is_some() {
        bail!("the task file holds {} with pull request error {:?}", persisted.status, persisted.pr_error);
    }
    for file in toml_files(&root)? {
        let contents = std::fs::read_to_string(&file).unwrap_or_default();
        if contents.contains("needs_you") || contents.contains("attention") {
            bail!("{} persists attention state", file.display());
        }
    }

    gh.fail_pr_creation(false)?;
    let session = context.start_session("delivery-restart").await?;
    let outcome = derive_again_and_deliver(session.driver(), &gh, &executed, &failed, &review).await;
    context.close_session(session, "delivery-restart", &outcome).await?;
    outcome
}

async fn fail_delivery_twice(
    driver: &WebDriver,
    agent: &FakeAgent,
    gh: &FakeGh,
    repository: &GitFixture,
) -> Result<(ExecutedTask, String, String)> {
    let executed = execute_one_task(driver, agent, repository).await?;
    show_on_board(driver, &executed.project_id, HUMAN_REVIEW_COLUMN, &executed.title).await?;
    await_reason(driver, &executed.id, Some("review")).await?;
    await_header(driver, 1).await?;

    open_reviewed_drawer(driver, &executed).await?;
    await_text(driver, HR_APPROVE, |t| t == "Approve & Create PR", "Approve & Create PR").await?;
    click(driver, HR_APPROVE, "Approve & Create PR").await?;

    // While the attempt runs: Creating PR, and nothing asked of the user.
    let running = await_card(driver, &executed.id, "Creating PR", |c| c.delivering).await?;
    if running.reason.is_some() {
        bail!("the card asks for the user while its pull request is being opened: {running:?}");
    }
    await_header(driver, 0).await?;
    ui::visible(driver, HR_DELIVERING).await.context("the drawer does not say a pull request is being created")?;

    // Once it fails: PR not created, amber, instead of a green Approved.
    let failed_card = await_reason(driver, &executed.id, Some("pr_not_created")).await?;
    assert_chip(&failed_card, "pr_not_created", "Needs you \u{00B7} PR not created", "amber")?;
    if failed_card.decision.is_some() {
        bail!("the card still shows {:?} beside the failure", failed_card.decision);
    }
    if failed_card.text.contains(fake_gh::CREATE_FAILURE) || failed_card.text.contains("fake gh") {
        bail!("the card shows what gh printed: {:?}", failed_card.text);
    }
    await_header(driver, 1).await?;
    ui::visible(driver, HR_APPROVED).await.context("the drawer lost the approval")?;
    if attribute(driver, HR_DELIVERY, "data-delivery").await?.as_deref() != Some("failed") {
        bail!("the drawer does not show the pull request as failed");
    }
    await_attempt(driver, 1).await?;

    // A retry that fails again is visibly a new attempt, not nothing.
    click(driver, HR_RETRY_PR, "Retry Create PR").await?;
    let again = await_card(driver, &executed.id, "Creating PR on retry", |c| c.delivering).await?;
    if again.reason.is_some() {
        bail!("the card asks for the user while the retry runs: {again:?}");
    }
    await_text(driver, HR_RETRY_PR, |t| t == "Creating…", "Creating… on the retry button").await?;
    await_header(driver, 0).await?;
    await_reason(driver, &executed.id, Some("pr_not_created")).await?;
    await_attempt(driver, 2).await?;
    if gh.create_attempts()? != 2 {
        bail!("gh pr create ran {} times, expected two failed attempts", gh.create_attempts()?);
    }

    // The other two reasons, for the restart to derive again.
    let (failed, _) = create_task_in(driver, &executed.project_id, "Needs you failed").await?;
    move_to(driver, &failed, "error").await?;
    let (review, _) = create_task_in(driver, &executed.project_id, "Needs you review").await?;
    move_to(driver, &review, "human_review").await?;
    await_header(driver, 3).await?;
    Ok((executed, failed, review))
}

async fn await_attempt(driver: &WebDriver, number: u64) -> Result<()> {
    let started = Instant::now();
    loop {
        let shown = attribute(driver, HR_ATTEMPT, "data-attempt").await.ok().flatten();
        if shown.as_deref() == Some(number.to_string().as_str()) {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the drawer never numbered attempt {number}; it shows {shown:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn derive_again_and_deliver(
    driver: &WebDriver,
    gh: &FakeGh,
    executed: &ExecutedTask,
    failed: &str,
    review: &str,
) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    open_board(driver, &executed.project_id).await?;
    await_header(driver, 3).await?;
    await_reason(driver, failed, Some("failed")).await?;
    await_reason(driver, review, Some("review")).await?;
    let pr_card = await_reason(driver, &executed.id, Some("pr_not_created")).await?;
    if pr_card.decision.is_some() {
        bail!("after a restart the card shows {:?} beside the failure", pr_card.decision);
    }
    await_rail(driver, &executed.project_id, Some(3)).await?;

    open_reviewed_drawer(driver, executed).await?;
    click(driver, HR_RETRY_PR, "Retry Create PR").await?;
    await_card(driver, &executed.id, "Creating PR after the restart", |c| c.delivering && c.reason.is_none()).await?;
    await_header(driver, 2).await?;

    let task = await_task(driver, executed, "PR Created", |t| status_of(t) == Some("pr_created")).await?;
    if gh.create_attempts()? != 3 {
        bail!("expected two failed attempts and one that opened the pull request, saw {}", gh.create_attempts()?);
    }
    let url = task.get("pr_url").and_then(Value::as_str).context("the task records no pull request")?;
    if attribute(driver, HR_DELIVERY, "data-delivery").await?.as_deref() != Some("created") {
        bail!("the drawer does not show the pull request");
    }
    await_reason(driver, &executed.id, None).await?;
    await_header(driver, 2).await?;
    await_rail(driver, &executed.project_id, Some(2)).await?;
    let card = card(driver, &executed.id).await?.context("the card is gone")?;
    if !card.text.contains("PR") {
        bail!("the card does not show the linked pull request {url}: {:?}", card.text);
    }
    assert_card_in_column(driver, PR_CREATED_COLUMN, &executed.title).await
}

// --- Journey C: decision transitions ------------------------------------------

/// Human Review needs the user only while undecided: requesting changes
/// requeues the task and the attention goes with it, the next run brings it
/// back undecided, and an approval with nothing left to deliver needs nothing.
#[tokio::test(flavor = "multi_thread")]
async fn only_an_undecided_review_needs_you() {
    let context = TestContext::new("needs_you_decisions").expect("harness setup");
    let outcome = decisions_journey(&context).await;
    context.finish(outcome);
}

async fn decisions_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    // No GitHub remote: approving records the decision and delivers nothing,
    // so nothing can fail.
    let (agent, _gh) = install_fakes(context, &root)?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    let session = context.start_session("decisions").await?;
    let outcome = decide_twice(session.driver(), &agent, &repository).await;
    context.close_session(session, "decisions", &outcome).await?;
    outcome
}

async fn decide_twice(driver: &WebDriver, agent: &FakeAgent, repository: &GitFixture) -> Result<()> {
    let executed = execute_one_task(driver, agent, repository).await?;
    show_on_board(driver, &executed.project_id, HUMAN_REVIEW_COLUMN, &executed.title).await?;
    let undecided = await_reason(driver, &executed.id, Some("review")).await?;
    if undecided.sync_pr {
        bail!("Sync PR is offered for changes nobody has decided on");
    }
    await_header(driver, 1).await?;

    open_reviewed_drawer(driver, &executed).await?;
    click(driver, HR_REQUEST_CHANGES, "Request Changes").await?;
    ui::visible(driver, HR_FEEDBACK).await?.send_keys(FEEDBACK).await.context("could not type the feedback")?;
    click(driver, HR_SEND_FEEDBACK, "Send back to the agent").await?;

    // Requeued: nothing asked of the user while it waits and runs.
    await_header(driver, 0).await?;
    await_card(driver, &executed.id, "no attention once requeued", |c| c.reason.is_none()).await?;
    let started = Instant::now();
    loop {
        let task = read_task(driver, &executed).await?;
        let arrivals = task.pointer("/human_review/arrivals").and_then(Value::as_u64);
        if status_of(&task) == Some("human_review") && arrivals == Some(2) {
            break;
        }
        // Every moment between the request and the next arrival.
        if header_count(driver).await? != Some(0) {
            bail!("the task needed the user while {:?}", status_of(&task));
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("the task never came back to Human Review: {task}");
        }
        tokio::time::sleep(POLL).await;
    }

    // Back in Human Review with new changes: undecided again.
    await_reason(driver, &executed.id, Some("review")).await?;
    await_header(driver, 1).await?;

    await_text(driver, HR_APPROVE, |t| t == "Approve", "Approve").await?;
    click(driver, HR_APPROVE, "Approve").await?;
    ui::visible(driver, HR_APPROVED).await.context("the approval was not shown")?;
    await_header(driver, 0).await?;
    let approved = await_card(driver, &executed.id, "Approved without attention", |c| {
        c.reason.is_none() && c.decision.as_deref() == Some("Approved")
    })
    .await?;
    if !approved.sync_pr {
        bail!("Sync PR is not offered for approved changes with no pull request");
    }
    let task = read_task(driver, &executed).await?;
    if status_of(&task) != Some("human_review") {
        bail!("approving without a pull request moved the task to {:?}", status_of(&task));
    }
    Ok(())
}

// --- Journey D: cross-project rail --------------------------------------------

/// The rail shows every project's count, selected or not; the header counts
/// only the project on screen.
#[tokio::test(flavor = "multi_thread")]
async fn the_rail_shows_every_project_and_the_header_only_the_current_one() {
    let context = TestContext::new("needs_you_rail").expect("harness setup");
    let outcome = rail_journey(&context).await;
    context.finish(outcome);
}

async fn rail_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let first = GitFixture::create(&root.join("fixture-repo-a"))?;
    let second = GitFixture::create(&root.join("fixture-repo-b"))?;
    let session = context.start_session("rail").await?;
    let outcome = compare_projects(session.driver(), &first, &second).await;
    context.close_session(session, "rail", &outcome).await?;
    outcome
}

async fn compare_projects(driver: &WebDriver, first: &GitFixture, second: &GitFixture) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let a = create_prerequisites(driver, first, "Project A failed", "Never runs.").await?;
    let (a_review, _) = create_task_in(driver, &a.project_id, "Project A review").await?;
    let b = create_prerequisites(driver, second, "Project B failed", "Never runs.").await?;
    move_to(driver, &a.task_id, "error").await?;
    move_to(driver, &a_review, "human_review").await?;
    move_to(driver, &b.task_id, "error").await?;

    // B on screen: its header counts B alone; A's badge says 2.
    open_board(driver, &b.project_id).await?;
    await_header(driver, 1).await?;
    await_rail(driver, &a.project_id, Some(2)).await?;
    await_rail(driver, &b.project_id, Some(1)).await?;

    // Switching to A: the header follows, and B keeps its badge.
    ui::visible(driver, &format!("[data-testid=\"rail-project-{}\"]", a.project_id))
        .await?
        .click()
        .await
        .context("could not select project A")?;
    await_header(driver, 2).await?;
    await_reason(driver, &a_review, Some("review")).await?;
    await_rail(driver, &b.project_id, Some(1)).await?;
    await_rail(driver, &a.project_id, Some(2)).await?;

    // A change in the project not on screen reaches its badge.
    move_to(driver, &b.task_id, "backlog").await?;
    await_rail(driver, &b.project_id, None).await?;
    await_header(driver, 2).await
}
