//! The first journey that tests SlashIt itself rather than the harness.
//!
//! One task goes all the way through the real machinery: the real window, the
//! real IPC boundary, the real task domain object, the real queue poller, the
//! real `ClaudeRunner`, a real external agent process, the real worktree and
//! commit handling, and the real persisted result. The only substitution is
//! the program on the far side of the process boundary — `claude` itself,
//! which the product looks up on `PATH` and which this suite replaces with a
//! fixture that answers in the same protocol.
//!
//! Nothing inside SlashIt is faked, stubbed or configured into a test mode.
//! The executor is not called directly; it is left to notice the task on its
//! own three-second tick, exactly as it would for a user dragging a card.
//!
//! Run with:
//!
//! ```text
//! cargo tauri build --debug --no-bundle
//! cargo test -p slashit-acceptance --features run-acceptance --test product_acceptance
//! ```
//!
//! Linux only, for the same reason as the harness journeys.

#![cfg(target_os = "linux")]

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use slashit_acceptance::fake_agent::{self, FakeAgent, REPORTED_MODEL};
use slashit_acceptance::{ui, TestContext};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thirtyfour::prelude::*;

/// How long the queue gets to notice the task and run the agent. The poller
/// ticks every three seconds and the fixture returns immediately, so this is
/// mostly headroom for a loaded hosted runner.
const EXECUTION_DEADLINE: Duration = Duration::from_secs(90);
#[path = "product_acceptance/board_identity.rs"]
mod board_identity;
#[path = "product_acceptance/disk_pressure.rs"]
mod disk_pressure;
#[path = "product_acceptance/developer_tools.rs"]
mod developer_tools;
#[path = "product_acceptance/git_fixture.rs"]
mod git_fixture;
#[path = "product_acceptance/human_review.rs"]
mod human_review;
#[path = "product_acceptance/local_first.rs"]
mod local_first;
#[path = "product_acceptance/needs_you.rs"]
mod needs_you;
#[path = "product_acceptance/pr_status.rs"]
mod pr_status;
#[path = "product_acceptance/storage.rs"]
mod storage;

/// How long the board gets to render the finished task.
const RENDER_DEADLINE: Duration = Duration::from_secs(30);
/// How long a stopped task is watched for signs of being started again.
///
/// Four ticks of the executor's own cadence: `polling_loop` sleeps three
/// seconds between passes, so this is three complete opportunities to act on
/// the task plus the one the stop itself may have landed inside.
const RESTART_WINDOW: Duration = Duration::from_secs(12);
const POLL: Duration = Duration::from_millis(250);

/// The Kanban column for a status, as the board names it: the frontend builds
/// the identifier with `format!("{:?}", status).to_lowercase()`.
const HUMAN_REVIEW_COLUMN: &str = "[data-testid=\"column-humanreview\"]";
const BACKLOG_COLUMN: &str = "[data-testid=\"column-backlog\"]";
const ERROR_COLUMN: &str = "[data-testid=\"column-error\"]";
const KANBAN_BOARD: &str = "[data-testid=\"kanban-board\"]";
/// The title of one card, within whichever column is being read.
const TASK_TITLE: &str = "[data-testid=\"task-title\"]";

/// Prove that moving a task into execution really runs an agent and really
/// carries the task to its reviewable state.
///
/// The two independent proofs are the fixture's own invocation record — which
/// only the process that actually started could have written — and the task's
/// journey through the product's state machine to the state the code says
/// follows a successful coding run.
#[tokio::test(flavor = "multi_thread")]
async fn a_task_moved_into_progress_runs_its_agent_and_reaches_human_review() {
    let context = TestContext::new("queue_agent_execution").expect("harness setup");
    let outcome = journey(&context).await;
    context.finish(outcome);
}

async fn journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();

    // The fixture goes inside the state root, so the harness's existing
    // cleanup removes it along with everything else this run owns.
    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());

    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("queue").await?;
    let executed = execute_one_task(session.driver(), &agent, &repository).await;
    context.close_session(session, "queue", &executed).await?;
    let executed = executed?;

    // The state the application reported has to also be the state on disk.
    // Everything up to here could in principle have been served from memory.
    assert_persisted(&root, &executed, "human_review")?;

    // The worktree the agent ran in is a real directory the product created,
    // and it is inside the tree this run owns — so the harness's own cleanup
    // is what removes it, and a leak would be reported rather than ignored.
    if !executed.worktree_path.is_dir() {
        bail!(
            "the product reported worktree {} but nothing is there",
            executed.worktree_path.display()
        );
    }

    // And the agent must not have been started a second time by anything that
    // happened during shutdown.
    let invocations = agent_runs(&agent)?.len();
    if invocations != 1 {
        bail!("the agent ran {invocations} times over the whole journey, expected exactly once");
    }

    Ok(())
}

/// Prove that a failed agent run is recorded as one, and that retrying the
/// task through the product's own status transition really runs the agent
/// again and carries the task to the state a successful run reaches.
///
/// The failure is a well-formed result that reports failure, with a zero exit
/// status — the agent ran and said it could not do the work. That is the
/// failure the product distinguishes in `ClaudeRunner::wait`, and it exercises
/// the whole path: the real stream parser reads `is_error`, the runner turns it
/// into an error, and the executor settles the task on it. It is deliberately
/// not a crashing agent, which would prove the same product transition through
/// less of the product.
///
/// Retry is `update_task_status` to `queue`, which is exactly what the board
/// sends when a card is dragged out of the Error column back into Queue, and
/// the queue promotes it from there on its own. Nothing in the executor is
/// called directly at any point.
///
/// The journey also holds the product to preserving the work it is retrying:
/// the failed attempt's worktree and branch have to survive the failure, the
/// reset, and the second run — which reattaches to them rather than starting
/// from an empty directory. Re-queuing a task asks it to continue; discarding
/// what it produced is a different operation and no part of this path.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_agent_run_is_recorded_and_retrying_the_task_reaches_human_review() {
    let context = TestContext::new("queue_agent_retry").expect("harness setup");
    let outcome = retry_journey(&context).await;
    context.finish(outcome);
}

async fn retry_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();

    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    // The first agent run fails and every one after it succeeds. The fixture
    // is told how many runs to fail and nothing else: it never learns that a
    // retry is what produced the second run, or that there is a task at all.
    context.set_child_env(fake_agent::FAILING_RUNS_VAR, "1");

    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("retry").await?;
    let outcome = fail_then_retry(session.driver(), &agent, &repository).await;
    context.close_session(session, "retry", &outcome).await?;
    let recovered = outcome?;

    assert_persisted(&root, &recovered, "human_review")?;

    // The directory the task points at is the one the retried agent ran in,
    // and it is still there once everything has settled. A task reporting a
    // worktree that no longer exists would be coherent in memory and wrong on
    // disk — which is precisely what re-queuing used to produce.
    if !recovered.worktree_path.is_dir() {
        bail!(
            "after the retry the task reports worktree {} but nothing is there",
            recovered.worktree_path.display()
        );
    }

    let runs = agent_runs(&agent)?;
    if runs.len() != 2 {
        bail!(
            "the agent ran {} times over the whole journey, expected exactly two: one failure \
             and one retry",
            runs.len()
        );
    }

    Ok(())
}

/// Prove what stopping a task does while its agent is still running.
///
/// Four claims, and the journey is only worth anything if it crosses the whole
/// distance between them. The agent the user was looking at has to be gone —
/// the actual process, named before the stop and looked for afterwards.
/// Nothing may start the task again on its own, because a stop that quietly
/// becomes a restart is not a stop. The task has to settle somewhere a person
/// has to act on before it runs again, which is `backlog`, and it has to
/// settle there on disk as well as in the board, or the next launch will
/// disagree with what the user was just told. And nothing may be destroyed:
/// the worktree, the branch and the work already in them all survive, because
/// stopping is not discarding and the product has no operation that discards.
///
/// A stoppable run needs an agent that is still there to stop, which is what
/// the fixture's blocking mode is for: it announces the process it is and then
/// waits. Nothing else about the journey is arranged — the queue notices the
/// task on its own tick, and the stop goes through the same command the
/// frontend would invoke.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_running_task_ends_its_agent_and_does_not_restart_it() {
    let context = TestContext::new("queue_task_cancellation").expect("harness setup");
    let outcome = cancellation_journey(&context).await;
    context.finish(outcome);
}

async fn cancellation_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();

    let agent = FakeAgent::install(&root)?;
    // Agent runs stay open until something ends them, so there is a real
    // process to stop when the journey asks the product to stop one.
    let release = agent.block_agent_runs()?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_agent::BLOCK_DIR_VAR, &release);

    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("cancellation").await?;
    let outcome = stop_a_running_task(session.driver(), &agent, &repository, &root).await;

    // A run the product ended is already gone, so there is normally nothing
    // here to release. Anything still alive is a failure, and it is left
    // exactly as it is: the session's own shutdown ends it along with the
    // application, since it owns the process group and returns only once that
    // group is gone. Releasing the runs here instead would let them finish and
    // carry the task onward, overwriting the on-disk state a preserved failure
    // exists to show.
    context
        .close_session(session, "cancellation", &outcome)
        .await?;
    outcome
}

/// What the journey saw in the interval after the stop was requested.
struct AfterStop {
    /// Whether the process the product was asked to stop is still running.
    agent_alive: bool,
    /// Agent runs that started after the stop.
    runs: usize,
    /// The processes those runs announced.
    pids: Vec<u32>,
}

/// Start one real execution, stop it through the product's own command, and
/// report every way the result departs from what the product says it does.
///
/// The violations are collected rather than raised one at a time. Stopping
/// touches the process, the queue, the task and the disk at once, and a report
/// that stopped at whichever of those broke first would describe a fraction of
/// what happened and hide the rest until the next run.
async fn stop_a_running_task(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
    root: &Path,
) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;

    let Prerequisites {
        project_id,
        task_id,
        title,
    } = create_prerequisites(
        driver,
        repository,
        "Cancellation journey",
        "Exercises stopping a task while its agent is still running.",
    )
    .await?;

    let already_ran = agent_runs(agent)?.len();
    if already_ran != 0 {
        bail!("the agent ran {already_ran} times before the task was ever started");
    }

    // --- A real execution, still running -----------------------------------
    ui::invoke(
        driver,
        "update_task_status",
        json!({ "taskId": task_id, "status": "in_progress" }),
    )
    .await?;

    let agent_pid = await_one_blocked_agent(agent).await?;

    // The process that announced itself belongs to the task run, not to the
    // product asking what version is installed.
    let runs = agent_runs(agent)?;
    if runs.len() != 1 {
        bail!(
            "{} agent runs started before the stop, expected exactly one",
            runs.len()
        );
    }
    let invocation = &runs[0];
    if invocation.flag("--output-format") != Some(OsStr::new("stream-json")) {
        bail!(
            "the agent was not invoked with the streaming protocol the runner parses: {:?}",
            invocation.args
        );
    }
    let prompt = invocation
        .prompt
        .as_deref()
        .context("the agent was invoked without reading a prompt from stdin")?;
    if prompt.is_empty() {
        bail!("the agent was invoked with an empty prompt");
    }
    // The prompt travels on stdin only, where other local processes cannot
    // read it and no argument size limit applies.
    if let Some(leaked) = invocation.args.iter().find(|arg| arg.to_string_lossy().contains(prompt)) {
        bail!("the prompt was also passed as an argument: {leaked:?}");
    }

    // And it ran where the product's own executions run.
    let worktree_path = invocation.working_dir.clone();
    let resolved_worktree = resolve(&worktree_path);
    if !resolved_worktree.starts_with(resolve(repository.state_root())) {
        bail!(
            "the agent ran in {}, which is outside this run's state root {} — isolation is \
             leaking",
            worktree_path.display(),
            repository.state_root().display()
        );
    }
    if resolved_worktree == resolve(&repository.path_buf()) {
        bail!("the agent ran in the repository itself instead of a task worktree");
    }

    if !agent.is_running(agent_pid) {
        bail!(
            "the agent process {agent_pid} was already gone before the stop was requested, so \
             this journey would prove nothing about stopping it"
        );
    }

    // --- The one action under test -----------------------------------------
    //
    // The command the frontend invokes, not the executor method behind it.
    ui::invoke(driver, "stop_task_execution", json!({ "taskId": task_id })).await?;

    // The phase the moment the stop returned, read through the product's own
    // command. Reported rather than asserted: it is what tells a reader whether
    // a later phase was left by the stop or written by something that ran after
    // it, and the queue is free to have acted before this line.
    let phase_at_stop =
        ui::invoke(driver, "get_execution_status", json!({ "taskId": task_id })).await?;

    let after = observe_after_stop(agent, agent_pid).await?;

    // --- What the product now says about the task --------------------------
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    let task = find_task(&listed, &task_id)
        .context("the product no longer lists the task it was asked to stop")?;
    let status = status_of(&task).unwrap_or("unreadable").to_string();
    let phase = task
        .get("phase")
        .and_then(Value::as_str)
        .unwrap_or("unreadable")
        .to_string();
    let progress = task.get("phase_progress").and_then(Value::as_i64);
    let branch = task
        .get("branch_name")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut broken: Vec<String> = Vec::new();

    if after.agent_alive {
        broken.push(format!(
            "the agent process {agent_pid} is still running {}s after the product reported the \
             stop as done — the user was told the run was over while it carried on working in \
             the task's worktree",
            RESTART_WINDOW.as_secs()
        ));
    }
    if after.runs > 0 {
        broken.push(format!(
            "{} further agent run(s) started within {}s of the stop with no user action in \
             between, as process(es) {:?} — nothing asked for this work again",
            after.runs,
            RESTART_WINDOW.as_secs(),
            after.pids
        ));
    }
    if status != "backlog" {
        broken.push(format!(
            "the task settled at status {status:?}; a stopped task belongs in `backlog`, the \
             one place the queue will not start it from without someone asking"
        ));
    }
    if phase != "idle" {
        broken.push(format!(
            "the task settled at phase {phase:?}; the run it names is not happening any more"
        ));
    }
    if progress != Some(0) {
        broken.push(format!(
            "the task reports {progress:?} phase progress for a run that was stopped"
        ));
    }

    // Nothing may be destroyed. Stopping is not discarding, and the product has
    // no operation that discards.
    if !worktree_path.is_dir() {
        broken.push(format!(
            "the worktree {} the agent was working in no longer exists",
            worktree_path.display()
        ));
    }
    match &branch {
        Some(name) if !repository.has_branch(name)? => broken.push(format!(
            "the task still records branch {name} but the branch no longer exists"
        )),
        Some(_) => {}
        None => broken.push(
            "the task records no branch, so the work the stopped run had produced can no longer \
             be found"
                .to_string(),
        ),
    }

    // The state the product reports has to be the state on disk, proven the
    // same structural way every other journey proves it.
    let executed = ExecutedTask {
        id: task_id.clone(),
        project_id: project_id.to_string(),
        title: title.clone(),
        worktree_path: worktree_path.clone(),
    };
    if let Err(error) = assert_persisted(root, &executed, &status) {
        broken.push(format!(
            "the state the product reports is not the state on disk: {error:#}"
        ));
    }

    // And a user has to be able to see where the task ended up.
    if let Err(error) = show_on_board(driver, &project_id, BACKLOG_COLUMN, &title).await {
        broken.push(format!(
            "the board does not show the stopped task: {error:#}"
        ));
    }

    if !broken.is_empty() {
        bail!(
            "stopping a running task did not do what the product says it does:\n  - {}\n\nthe \
             product reported phase {phase_at_stop} the moment the stop returned, and the agent \
             it was asked to stop was process {agent_pid}",
            broken.join("\n  - ")
        );
    }

    Ok(())
}

/// Wait until exactly one agent run has started and announced its process.
async fn await_one_blocked_agent(agent: &FakeAgent) -> Result<u32> {
    let started = Instant::now();
    loop {
        let pids = agent.blocked_pids()?;
        match pids.as_slice() {
            [] => {}
            [only] => return Ok(*only),
            several => bail!(
                "{} agent runs started at once, expected one: {several:?}",
                several.len()
            ),
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!(
                "no agent run announced itself within {}s of the task moving to in_progress; the \
                 fixture records in {}",
                EXECUTION_DEADLINE.as_secs(),
                agent.marker_dir().display()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Watch for one bounded interval after the stop.
///
/// The interval is spent in full, because half of what it establishes is a
/// negative: nothing may start the task again. Its length comes from the
/// executor's own cadence rather than from a guess — `polling_loop` sleeps
/// three seconds between passes, so four ticks is three complete opportunities
/// to act on the task plus the one the stop itself may have landed inside.
///
/// A process cannot come back once it is gone, so one look at the end is the
/// whole question about the stopped agent; the interval is what makes the
/// answer about the queue mean anything.
async fn observe_after_stop(agent: &FakeAgent, stopped: u32) -> Result<AfterStop> {
    tokio::time::sleep(RESTART_WINDOW).await;

    let pids: Vec<u32> = agent
        .blocked_pids()?
        .into_iter()
        .filter(|pid| *pid != stopped)
        .collect();

    Ok(AfterStop {
        agent_alive: agent.is_running(stopped),
        // Counted from the invocation records rather than from the announced
        // processes: a run that started and has not reached its announcement
        // yet is still a run that started.
        runs: agent_runs(agent)?.len().saturating_sub(1),
        pids,
    })
}

// --- The task drawer ------------------------------------------------------
//
// The drawer is where a person sees and controls a task, so these journeys
// drive it the way a person does: click the card, read what the drawer shows,
// press its buttons. Everything the drawer shows is checked against the
// product's own answer for the same moment, and every control it offers is
// checked for its effect on the real agent process and the persisted task.

const TASK_DRAWER: &str = "[data-testid=\"task-drawer\"]";
const DRAWER_STATUS: &str = "[data-testid=\"task-drawer-status\"]";
const DRAWER_ACTIVITY: &str = "[data-testid=\"task-drawer-activity\"]";
const DRAWER_OUTPUT_LINE: &str = "[data-testid=\"task-drawer-output-line\"]";
const DRAWER_STOP: &str = "[data-testid=\"task-drawer-stop\"]";
const DRAWER_RETRY: &str = "[data-testid=\"task-drawer-retry\"]";
const DRAWER_EDIT: &str = "[data-testid=\"task-drawer-edit\"]";
const DRAWER_ERROR: &str = "[data-testid=\"task-drawer-error\"]";
const DRAWER_ELAPSED: &str = "[data-testid=\"task-drawer-elapsed\"]";
const DRAWER_CHANGES_TAB: &str = "[data-testid=\"task-drawer-tab-changes\"]";
const DRAWER_CHANGES: &str = "[data-testid=\"task-drawer-changes\"]";
const DRAWER_TIMELINE_ROW: &str = "[data-testid=\"task-drawer-timeline-row\"]";
/// How many times the drawer is opened and closed before its listener
/// hygiene is judged.
const DRAWER_CYCLES: usize = 5;

/// Prove the drawer shows a running task's live activity and output as the
/// agent produces them, that its Stop really ends the agent through the
/// backend, and that it then shows the state the task really settled in.
///
/// The same journey holds the drawer's live subscription to its contract:
/// opening and closing it repeatedly leaves no listener behind, and a drawer
/// open on one task never shows another task's activity or output.
#[tokio::test(flavor = "multi_thread")]
async fn the_task_drawer_shows_a_running_task_live_and_stops_it() {
    let context = TestContext::new("task_drawer_stop").expect("harness setup");
    let outcome = drawer_stop_journey(&context).await;
    context.finish(outcome);
}

async fn drawer_stop_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();

    let agent = FakeAgent::install(&root)?;
    let release = agent.block_agent_runs()?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_agent::BLOCK_DIR_VAR, &release);
    // The run narrates its progress in the stream the product parses, so what
    // the drawer shows can only have come from real backend execution events.
    context.set_child_env(fake_agent::PROGRESS_VAR, "1");

    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("drawer-stop").await?;
    let outcome = observe_and_stop_in_the_drawer(session.driver(), &agent, &repository).await;
    // Anything still running is a failure and is left for the session's own
    // shutdown to end, exactly as the cancellation journey does.
    context.close_session(session, "drawer-stop", &outcome).await?;
    outcome
}

async fn observe_and_stop_in_the_drawer(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;

    let Prerequisites {
        project_id,
        task_id,
        title,
    } = create_prerequisites(
        driver,
        repository,
        "Drawer stop journey",
        "Exercises watching and stopping a running task from its drawer.",
    )
    .await?;
    // A second task in the same project that never runs: the drawer open on
    // it is where another task's events must not show up.
    let (bystander_id, bystander_title) = create_task_in(driver, &project_id, "Drawer bystander").await?;

    open_board(driver, &project_id).await?;

    // --- Listener hygiene --------------------------------------------------
    //
    // The board holds one `agent-event` listener for as long as it is mounted
    // and each open drawer holds one more. Opening and closing must return to
    // exactly that, however often it happens.
    await_listener_count(driver, 1).await.context("the board's own listener")?;
    for cycle in 1..=DRAWER_CYCLES {
        open_drawer(driver, &bystander_id, &bystander_title).await?;
        await_listener_count(driver, 2)
            .await
            .with_context(|| format!("with the drawer open, cycle {cycle}"))?;
        close_drawer(driver).await?;
        await_listener_count(driver, 1)
            .await
            .with_context(|| format!("after closing the drawer, cycle {cycle}"))?;
    }

    // A task with nothing running offers neither Stop nor Retry.
    open_drawer(driver, &bystander_id, &bystander_title).await?;
    await_drawer_status(driver, "backlog").await?;
    assert_not_offered(driver, DRAWER_STOP, "Stop, for a task that never ran").await?;
    assert_not_offered(driver, DRAWER_RETRY, "Retry, for a task that never failed").await?;

    // --- Task A runs while task B's drawer is open ---------------------------
    ui::invoke(
        driver,
        "update_task_status",
        json!({ "taskId": task_id, "status": "in_progress" }),
    )
    .await?;
    let agent_pid = await_one_blocked_agent(agent).await?;

    // The board received the running task's events: its card shows what the
    // agent is doing. This is what makes the next check about isolation rather
    // than about events never arriving at all.
    let first_activity = format!("Using {}", fake_agent::PROGRESS_FIRST_TOOL);
    await_card_activity(driver, &title, &first_activity).await?;

    let leaked = drawer_text(driver).await?;
    if leaked.contains(fake_agent::PROGRESS_FIRST_TEXT) || leaked.contains(&first_activity) {
        bail!(
            "the drawer open on {bystander_title:?} shows another task's live output: {leaked:?}"
        );
    }
    if count(driver, DRAWER_OUTPUT_LINE).await? != 0 {
        bail!("the drawer open on a task that never ran shows output lines");
    }
    if count(driver, DRAWER_ACTIVITY).await? != 0 {
        bail!("the drawer open on a task that is not running shows live activity");
    }
    await_drawer_status(driver, "backlog").await?;
    close_drawer(driver).await?;

    // --- Task A's drawer: live activity and output ---------------------------
    open_drawer(driver, &task_id, &title).await?;
    await_listener_count(driver, 2).await.context("with the running task's drawer open")?;
    await_drawer_status(driver, "inprogress").await?;
    await_text(driver, DRAWER_ACTIVITY, |text| text == first_activity, "the first activity").await?;
    await_output_containing(driver, fake_agent::PROGRESS_FIRST_TEXT).await?;
    ui::visible(driver, DRAWER_ELAPSED)
        .await
        .context("a running task's drawer shows how long it has been running")?;
    ui::visible(driver, DRAWER_STOP).await.context("a running task offers Stop")?;
    assert_not_offered(driver, DRAWER_RETRY, "Retry, for a running task").await?;
    assert_not_offered(driver, DRAWER_EDIT, "Edit, for a running task").await?;

    // The agent takes its next step; the open drawer follows it without being
    // reopened.
    if agent.advance_blocked_runs()? != 1 {
        bail!("the running agent could not be advanced to its next step");
    }
    let second_activity = format!("Using {}", fake_agent::PROGRESS_SECOND_TOOL);
    await_text(driver, DRAWER_ACTIVITY, |text| text == second_activity, "the activity after the agent's next step").await?;
    await_output_containing(driver, fake_agent::PROGRESS_SECOND_TEXT).await?;

    // Each thing the agent said is shown once: output is read from the
    // backend's record of the run, not accumulated per delivered event.
    let lines = output_lines(driver).await?;
    for said in [fake_agent::PROGRESS_FIRST_TEXT, fake_agent::PROGRESS_SECOND_TEXT] {
        let shown = lines.iter().filter(|line| line.as_str() == said).count();
        if shown != 1 {
            bail!("the drawer shows {said:?} {shown} times, expected once: {lines:?}");
        }
    }

    if !agent.is_running(agent_pid) {
        bail!("the agent {agent_pid} was gone before Stop was pressed; the journey would prove nothing");
    }

    // --- The action under test: Stop, pressed in the drawer ------------------
    ui::visible(driver, DRAWER_STOP)
        .await?
        .click()
        .await
        .context("could not press Stop in the drawer")?;

    // The backend really ended the process.
    let started = Instant::now();
    while agent.is_running(agent_pid) {
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("the agent {agent_pid} is still running {}s after Stop was pressed", EXECUTION_DEADLINE.as_secs());
        }
        tokio::time::sleep(POLL).await;
    }

    // The drawer settles on the state the backend settled the task in, and
    // stops offering Stop once nothing is live.
    let settled = await_status(driver, &project_id, &task_id, &["backlog"]).await?;
    await_drawer_status(driver, "backlog").await?;
    await_absent_from_drawer(driver, DRAWER_STOP, "Stop, once the agent is gone").await?;
    await_output_containing(driver, "Stopped").await?;
    if settled.get("phase").and_then(Value::as_str) != Some("idle") {
        bail!("the stopped task reports phase {:?}, expected idle", settled.get("phase"));
    }

    // Nothing starts it again on its own.
    let after = observe_after_stop(agent, agent_pid).await?;
    if after.runs > 0 {
        bail!("{} agent run(s) started after the stop with no user action: {:?}", after.runs, after.pids);
    }
    await_drawer_status(driver, "backlog").await?;

    close_drawer(driver).await?;
    await_listener_count(driver, 1).await.context("after closing the running task's drawer")?;
    Ok(())
}

/// Prove the drawer shows a failed task's whole failure and its output, and
/// that its Retry re-queues the task through the product's existing
/// lifecycle, all the way to Human Review.
#[tokio::test(flavor = "multi_thread")]
async fn the_task_drawer_shows_a_failure_in_full_and_retries_it() {
    let context = TestContext::new("task_drawer_retry").expect("harness setup");
    let outcome = drawer_retry_journey(&context).await;
    context.finish(outcome);
}

async fn drawer_retry_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();

    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_agent::FAILING_RUNS_VAR, "1");
    context.set_child_env(fake_agent::PROGRESS_VAR, "1");

    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("drawer-retry").await?;
    let outcome = fail_and_retry_in_the_drawer(session.driver(), &agent, &repository).await;
    context.close_session(session, "drawer-retry", &outcome).await?;
    let recovered = outcome?;

    assert_persisted(&root, &recovered, "human_review")?;
    Ok(())
}

async fn fail_and_retry_in_the_drawer(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
) -> Result<ExecutedTask> {
    ui::assert_frontend_is_real(driver).await?;

    let Prerequisites {
        project_id,
        task_id,
        title,
    } = create_prerequisites(
        driver,
        repository,
        "Drawer retry journey",
        "Fails with a detailed reason, then succeeds when retried from the drawer.",
    )
    .await?;

    open_board(driver, &project_id).await?;
    ui::invoke(
        driver,
        "update_task_status",
        json!({ "taskId": task_id, "status": "in_progress" }),
    )
    .await?;

    let first_run = await_agent_runs(agent, 1).await?;
    let failed_session = first_run[0]
        .flag("--session-id")
        .filter(|session| !session.is_empty())
        .context("the failed run carried no usable --session-id")?
        .to_os_string();
    let failed_worktree = resolve(&first_run[0].working_dir);

    let failed = await_status(driver, &project_id, &task_id, &["error", "human_review"]).await?;
    let recorded = failed
        .get("error_message")
        .and_then(Value::as_str)
        .context("the failed task records no error message")?
        .to_string();
    if recorded != fake_agent::recorded_detailed_failure() {
        bail!(
            "the task records {recorded:?}, not the failure the agent reported ({:?})",
            fake_agent::recorded_detailed_failure()
        );
    }
    let failed_branch = failed
        .get("branch_name")
        .and_then(Value::as_str)
        .context("a task that ran should record its branch")?
        .to_string();

    // --- The failure, in full --------------------------------------------------
    assert_card_in_column(driver, ERROR_COLUMN, &title).await?;
    open_drawer(driver, &task_id, &title).await?;
    await_drawer_status(driver, "error").await?;
    let shown = await_text(driver, DRAWER_ERROR, |text| !text.is_empty(), "the failure").await?;
    if shown.trim() != recorded {
        bail!("the drawer shows the failure as {shown:?}, but the task records {recorded:?}");
    }
    for reason in fake_agent::DETAILED_FAILURE {
        if !shown.contains(reason) {
            bail!("the drawer's failure omits {reason:?}: {shown:?}");
        }
    }

    // And the output the failed attempt produced, ending in its failure.
    await_output_containing(driver, fake_agent::PROGRESS_FIRST_TEXT).await?;
    await_output_containing(driver, fake_agent::DETAILED_FAILURE[2]).await?;

    ui::visible(driver, DRAWER_RETRY).await.context("a failed task offers Retry")?;
    assert_not_offered(driver, DRAWER_STOP, "Stop, for a failed task with nothing running").await?;

    // --- The action under test: Retry, pressed in the drawer -----------------
    ui::visible(driver, DRAWER_RETRY)
        .await?
        .click()
        .await
        .context("could not press Retry in the drawer")?;

    // Re-queued through the existing lifecycle: out of Error, the failure
    // cleared by the product's own reset, and the attempt's work kept for the
    // next run to continue from.
    let requeued = await_status(
        driver,
        &project_id,
        &task_id,
        &["queue", "in_progress", "ai_review", "human_review", "error"],
    )
    .await?;
    if status_of(&requeued) == Some("error") {
        bail!("pressing Retry left the task in error");
    }
    await_absent_from_drawer(driver, DRAWER_RETRY, "Retry, once the task is queued again").await?;
    let kept_branch = requeued.get("branch_name").and_then(Value::as_str);
    if kept_branch != Some(failed_branch.as_str()) {
        bail!("retrying dropped the task's branch: {kept_branch:?} instead of {failed_branch:?}");
    }

    // The queue starts it again on its own, in the same worktree.
    let runs = await_agent_runs(agent, 2).await?;
    let second_run = retry_among(&runs, &failed_session)?;
    let retried_worktree = resolve(&second_run.working_dir);
    if retried_worktree != failed_worktree {
        bail!(
            "the retry ran in {} instead of continuing in {}",
            retried_worktree.display(),
            failed_worktree.display()
        );
    }

    // --- Through to Human Review, still in the open drawer -------------------
    let settled = await_status(driver, &project_id, &task_id, &["human_review", "error"]).await?;
    if status_of(&settled) != Some("human_review") {
        bail!(
            "the retried run did not reach human review: {:?}",
            settled.get("error_message")
        );
    }
    await_drawer_status(driver, "humanreview").await?;
    // The output now belongs to the retry, not the attempt it replaced.
    await_output_containing(driver, "Agent completed").await?;
    let lines = output_lines(driver).await?;
    if lines.iter().any(|line| line.contains(fake_agent::DETAILED_FAILURE[0])) {
        bail!("after the retry succeeded the drawer still shows the failed attempt's output: {lines:?}");
    }
    if count(driver, DRAWER_ERROR).await? != 0 {
        bail!("a task in human review still shows a failure");
    }

    // The failure is gone from the current state, but not from the history:
    // Activity tells how the task got here, the failed run and the retry
    // included, from what the task file records.
    let timeline = await_timeline_reaching(driver, "ready_for_review").await?;
    assert_timeline_story(&timeline)?;

    // Human Review is read-only here: its changes are reachable, and nothing
    // else is offered.
    for (control, what) in [
        (DRAWER_STOP, "Stop, in human review"),
        (DRAWER_RETRY, "Retry, in human review"),
        (DRAWER_EDIT, "Edit, in human review"),
    ] {
        assert_not_offered(driver, control, what).await?;
    }
    ui::visible(driver, DRAWER_CHANGES_TAB).await?.click().await.context("could not open the Changes tab")?;
    let changes = await_text(driver, DRAWER_CHANGES, |text| !text.contains("Loading"), "the changes").await?;
    if changes.contains("Could not load the changes") {
        bail!("the drawer could not load the task's changes: {changes:?}");
    }

    close_drawer(driver).await?;
    assert_card_in_column(driver, HUMAN_REVIEW_COLUMN, &title).await?;

    Ok(ExecutedTask {
        id: task_id,
        project_id,
        title,
        worktree_path: retried_worktree,
    })
}

const DRAWER_START: &str = "[data-testid=\"task-drawer-start\"]";
const DRAWER_START_ERROR: &str = "[data-testid=\"task-drawer-start-error\"]";
/// How long a queued task is watched to prove it waits for capacity: three
/// passes of the executor's three-second tick, each a chance to start it.
const QUEUED_WINDOW: Duration = Duration::from_secs(10);

/// Prove the drawer's Start puts a Backlog task in the queue, and that the
/// queue, not the drawer, decides when it runs.
///
/// One journey covers the whole contract, because each part needs the state
/// the previous one left:
///
/// - A Start that fails keeps the task in Backlog, says why in the drawer,
///   and can be pressed again. The failure is real: the task store's
///   directory is made read-only, so the backend's own durable write refuses.
/// - Pressed twice in the same instant, Start sends exactly one enqueue. Every
///   `reorder_task` the window sends is counted at the IPC boundary.
/// - With the only execution slot taken, the started task stays visibly
///   Queued and no agent runs for it.
/// - Once the slot frees, the scheduler starts it on its own: the fixture
///   records a real agent process running in the task's own worktree, and
///   the open drawer follows the task to Running and on to Human Review.
/// - A Start sent from a view that still shows Backlog after that changes
///   nothing: the agent keeps running and is not started again.
#[tokio::test(flavor = "multi_thread")]
async fn the_task_drawer_starts_a_backlog_task_through_the_queue() {
    let context = TestContext::new("task_drawer_start").expect("harness setup");
    let outcome = drawer_start_journey(&context).await;
    context.finish(outcome);
}

async fn drawer_start_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();

    let agent = FakeAgent::install(&root)?;
    // Runs stay open until released, so the task holding the slot is
    // really running while the started task waits.
    let release = agent.block_agent_runs()?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_agent::BLOCK_DIR_VAR, &release);

    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("drawer-start").await?;
    let outcome = start_from_the_drawer(session.driver(), &agent, &repository, &root).await;
    if outcome.is_err() {
        // A journey that stopped early can leave a run blocked, and a blocked
        // run outlives the application. Let it end, so the failure reported is
        // the journey's own rather than a leaked process.
        let _ = agent.release_blocked_runs();
    }
    context
        .close_session(session, "drawer-start", &outcome)
        .await?;
    let started = outcome?;

    assert_persisted(&root, &started, "human_review")?;
    // The occupier's one run, and the started task's one run: no duplicate
    // enqueue or promotion ever launched a second agent for it.
    assert_agent_roles(&agent, "drawer_start_journey", 2, 0, 0)?;
    Ok(())
}

async fn start_from_the_drawer(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
    root: &Path,
) -> Result<ExecutedTask> {
    ui::assert_frontend_is_real(driver).await?;

    let Prerequisites {
        project_id,
        task_id,
        title,
    } = create_prerequisites(
        driver,
        repository,
        "Drawer start journey",
        "Started from its drawer while another task holds the only slot.",
    )
    .await?;
    let occupier = created_id(
        ui::invoke(
            driver,
            "create_task",
            json!({
                "params": {
                    "projectId": project_id,
                    "title": format!("Slot holder for {title}"),
                    "description": "Holds the only execution slot until it is stopped.",
                    "model": "default",
                    "planningMode": false,
                    "dependencies": [],
                }
            }),
        )
        .await?,
        "create_task",
    )?;

    // One slot, so a second task has to wait for it.
    ui::invoke(
        driver,
        "update_queue_config",
        json!({ "parallelTaskLimit": 1 }),
    )
    .await?;

    open_board(driver, &project_id).await?;
    count_enqueues(driver).await?;
    open_drawer(driver, &task_id, &title).await?;
    await_drawer_status(driver, "backlog").await?;
    ui::visible(driver, DRAWER_START)
        .await
        .context("a Backlog task offers Start")?;
    assert_not_offered(driver, DRAWER_STOP, "Stop, for a task that never ran").await?;

    // --- A Start the backend refuses -----------------------------------------
    let store = task_store_dir(root, &task_id)?;
    let read_only = ReadOnlyDir::hold(&store)?;
    ui::visible(driver, DRAWER_START)
        .await?
        .click()
        .await
        .context("could not press Start in the drawer")?;
    let reason = await_text(
        driver,
        DRAWER_START_ERROR,
        |text| !text.is_empty(),
        "why Start failed",
    )
    .await?;
    read_only.release()?;
    if !reason.starts_with("Could not start the task:") {
        bail!("the drawer explains the failed Start as {reason:?}");
    }
    await_drawer_status(driver, "backlog").await?;
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    let status = find_task(&listed, &task_id)
        .as_ref()
        .and_then(|t| status_of(t).map(str::to_string));
    if status.as_deref() != Some("backlog") {
        bail!("a refused Start left the task at {status:?} instead of backlog");
    }
    let retry = await_text(
        driver,
        DRAWER_START,
        |text| text == "Start",
        "Start, ready again",
    )
    .await?;
    if disabled(driver, DRAWER_START).await? {
        bail!("after a failed Start the drawer shows {retry:?} but will not let it be pressed");
    }
    if enqueues_for(driver, &task_id).await? != 1 {
        bail!(
            "one press of Start sent {} enqueues",
            enqueues_for(driver, &task_id).await?
        );
    }

    // --- Take the only slot ---------------------------------------------------
    ui::invoke(
        driver,
        "update_task_status",
        json!({ "taskId": occupier, "status": "in_progress" }),
    )
    .await?;
    let occupier_pid = await_one_blocked_agent(agent).await?;

    // --- The action under test: Start, pressed repeatedly at once ------------
    //
    // The first press disables the button before anything else can run. The
    // presses after it are forced through that anyway, as a keyboard repeat
    // or an activation racing the render could be, so what is proven is that
    // the drawer itself sends one enqueue however many presses reach it.
    let disabled_at_once = page(
        driver,
        r#"
        const b = document.querySelector(arguments[0]);
        b.click();
        const disabled = b.disabled;
        b.disabled = false;
        b.click();
        b.click();
        return disabled;
        "#,
        vec![Value::String(DRAWER_START.to_string())],
    )
    .await?;
    if disabled_at_once != Value::Bool(true) {
        bail!("Start was still enabled right after it was pressed: {disabled_at_once}");
    }
    await_drawer_status(driver, "queue").await?;
    await_absent_from_drawer(driver, DRAWER_START, "Start, once the task is queued").await?;
    if count(driver, DRAWER_START_ERROR).await? != 0 {
        bail!("a Start that succeeded still shows the earlier failure");
    }
    let sent = enqueues_for(driver, &task_id).await?;
    if sent != 2 {
        bail!(
            "pressing Start three times at once sent {} enqueues, expected exactly one after the \
             one refused earlier",
            sent.saturating_sub(1)
        );
    }

    // --- Queued while the slot is taken --------------------------------------
    let waited = Instant::now();
    while waited.elapsed() < QUEUED_WINDOW {
        let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
        let status = find_task(&listed, &task_id)
            .as_ref()
            .and_then(|t| status_of(t).map(str::to_string));
        if status.as_deref() != Some("queue") {
            bail!(
                "with the only slot taken, the started task went to {status:?} instead of waiting"
            );
        }
        await_drawer_status(driver, "queue").await?;
        tokio::time::sleep(POLL).await;
    }
    let runs = agent_runs(agent)?.len();
    if runs != 1 {
        bail!(
            "{runs} agent runs started while the only slot was taken, expected just the occupier's"
        );
    }

    // --- The slot frees, and the queue starts the task on its own -------------
    ui::invoke(driver, "stop_task_execution", json!({ "taskId": occupier })).await?;
    let occupier_worktree = resolve(&agent_runs(agent)?[0].working_dir);

    let runs = await_agent_runs(agent, 2).await?;
    let started_run = &runs[1];
    let worktree = resolve(&started_run.working_dir);
    if worktree == occupier_worktree {
        bail!(
            "the started task's agent ran in the occupier's worktree {}",
            worktree.display()
        );
    }
    if !worktree.starts_with(resolve(repository.state_root())) {
        bail!(
            "the started task's agent ran outside this run's state root: {}",
            worktree.display()
        );
    }
    let pid = await_blocked_agent_other_than(agent, occupier_pid).await?;
    await_drawer_status(driver, "inprogress").await?;
    ui::visible(driver, DRAWER_STOP)
        .await
        .context("a running task offers Stop")?;
    let running = await_status(driver, &project_id, &task_id, &["in_progress"]).await?;
    let recorded = running
        .get("worktree_path")
        .and_then(Value::as_str)
        .map(|p| resolve(Path::new(p)));
    if recorded.as_deref() != Some(worktree.as_path()) {
        bail!(
            "the agent ran in {}, but the task records its worktree as {recorded:?}",
            worktree.display()
        );
    }
    if !agent.is_running(pid) {
        bail!("the started task's agent {pid} is not running while the task shows Running");
    }

    // --- A Start from a view that is behind ------------------------------------
    //
    // Exactly what a drawer still showing Backlog sends. The task has left
    // Backlog, so the backend must change nothing: the agent keeps running,
    // the task stays in progress, and nothing starts it a second time.
    let answered = ui::invoke(
        driver,
        "reorder_task",
        json!({
            "taskId": task_id,
            "newStatus": "queue",
            "newPosition": 0,
            "expectedStatus": "backlog",
        }),
    )
    .await?;
    if status_of(&answered) != Some("in_progress") {
        bail!("a stale Start was answered with {answered} instead of the running task as it is");
    }
    let watched = Instant::now();
    while watched.elapsed() < QUEUED_WINDOW {
        let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
        let status = find_task(&listed, &task_id)
            .as_ref()
            .and_then(|t| status_of(t).map(str::to_string));
        if status.as_deref() != Some("in_progress") {
            bail!("after a stale Start the running task went to {status:?}");
        }
        if !agent.is_running(pid) {
            bail!("a stale Start ended the running agent {pid}");
        }
        tokio::time::sleep(POLL).await;
    }
    let runs = agent_runs(agent)?.len();
    if runs != 2 {
        bail!("{runs} agent runs after a stale Start, expected the same 2 as before it");
    }
    await_drawer_status(driver, "inprogress").await?;

    // --- And the run carries on as any other ---------------------------------
    let settled = Instant::now();
    loop {
        agent.release_blocked_runs()?;
        let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
        let status = find_task(&listed, &task_id)
            .as_ref()
            .and_then(|t| status_of(t).map(str::to_string));
        match status.as_deref() {
            Some("human_review") => break,
            Some("error") => bail!("the started task failed: {listed}"),
            _ => {}
        }
        if settled.elapsed() > EXECUTION_DEADLINE {
            bail!("the started task never reached human review; it is at {status:?}");
        }
        tokio::time::sleep(POLL).await;
    }
    await_drawer_status(driver, "humanreview").await?;
    close_drawer(driver).await?;
    assert_card_in_column(driver, HUMAN_REVIEW_COLUMN, &title).await?;

    // The occupier stopped where a stop leaves a task, and nothing started it
    // again.
    let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    let occupier_status = find_task(&listed, &occupier)
        .as_ref()
        .and_then(|t| status_of(t).map(str::to_string));
    if occupier_status.as_deref() != Some("backlog") {
        bail!("the stopped occupier is at {occupier_status:?} instead of backlog");
    }

    Ok(ExecutedTask {
        id: task_id,
        project_id,
        title,
        worktree_path: worktree,
    })
}

/// Count every `reorder_task` this window sends from now on, by task.
///
/// Counted on the wire, below Tauri's own entry points (which it defines
/// read-only): every command leaves the window as a `fetch` to the IPC
/// protocol, named in its URL and carrying its arguments as the body, or as
/// a `window.ipc.postMessage` when that protocol is unavailable. Both are
/// watched, so this sees exactly what crosses the boundary whichever part of
/// the frontend sent it. Starting a task is `reorder_task` to the queue. The
/// counts are read before the journey sends a `reorder_task` of its own (the
/// stale Start), so every one they hold came from the drawer.
async fn count_enqueues(driver: &WebDriver) -> Result<()> {
    let installed = page(
        driver,
        r#"
        const counts = (window.__slashitEnqueues = {});
        const note = (command, args) => {
            if (command !== "reorder_task" || !args) return;
            let taskId;
            try { taskId = (typeof args === "string" ? JSON.parse(args) : args).taskId; } catch (_) { return; }
            if (taskId) counts[taskId] = (counts[taskId] || 0) + 1;
        };
        let watched = 0;
        const fetch = window.fetch;
        window.fetch = function (resource, init) {
            const url = String(resource && resource.url ? resource.url : resource);
            if (/^(ipc:|https?:\/\/ipc\.localhost)/.test(url)) {
                note(decodeURIComponent(url.split("?")[0].split("/").pop()), init && init.body);
            }
            return fetch.apply(this, arguments);
        };
        if (window.fetch !== fetch) watched += 1;
        if (window.ipc && typeof window.ipc.postMessage === "function") {
            const post = window.ipc.postMessage;
            try {
                window.ipc.postMessage = function (data) {
                    try {
                        const message = JSON.parse(data);
                        note(message.cmd, message.payload);
                    } catch (_) {}
                    return post.apply(this, arguments);
                };
                if (window.ipc.postMessage !== post) watched += 1;
            } catch (_) {}
        }
        return watched;
        "#,
        Vec::new(),
    )
    .await?;
    if installed.as_u64().unwrap_or_default() == 0 {
        bail!("could not count the commands the window sends: neither IPC transport is writable");
    }
    Ok(())
}

async fn enqueues_for(driver: &WebDriver, task_id: &str) -> Result<u64> {
    let sent = page(
        driver,
        "return (window.__slashitEnqueues || {})[arguments[0]] || 0;",
        vec![Value::String(task_id.to_string())],
    )
    .await?;
    Ok(sent.as_u64().unwrap_or_default())
}

async fn disabled(driver: &WebDriver, selector: &str) -> Result<bool> {
    let found = page(
        driver,
        "const e = document.querySelector(arguments[0]); return e ? e.disabled : null;",
        vec![Value::String(selector.to_string())],
    )
    .await?;
    found
        .as_bool()
        .with_context(|| format!("{selector} is not on the page"))
}

/// Wait for a blocked agent run other than `other`, and return its process.
async fn await_blocked_agent_other_than(agent: &FakeAgent, other: u32) -> Result<u32> {
    let started = Instant::now();
    loop {
        if let Some(pid) = agent.blocked_pids()?.into_iter().find(|pid| *pid != other) {
            return Ok(pid);
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!(
                "no second agent run announced itself within {}s",
                EXECUTION_DEADLINE.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The directory holding the task store that records `task_id`.
fn task_store_dir(root: &Path, task_id: &str) -> Result<PathBuf> {
    for file in toml_files(root)? {
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(document) = contents.parse::<toml::Value>() else {
            continue;
        };
        if task_record(&document, task_id).is_some() {
            return file
                .parent()
                .map(Path::to_path_buf)
                .context("a task store with no parent directory");
        }
    }
    bail!(
        "no task store under {} records task {task_id}",
        root.display()
    )
}

/// A directory the application may read but not write, until released or
/// dropped.
struct ReadOnlyDir {
    path: PathBuf,
    restore: u32,
}

impl ReadOnlyDir {
    fn hold(path: &Path) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let restore = std::fs::metadata(path)
            .with_context(|| format!("could not read the mode of {}", path.display()))?
            .permissions()
            .mode();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o500))
            .with_context(|| format!("could not make {} read-only", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            restore,
        })
    }

    fn release(self) -> Result<()> {
        let path = self.path.clone();
        let restore = self.restore;
        std::mem::forget(self);
        Self::restore_mode(&path, restore)
    }

    fn restore_mode(path: &Path, mode: u32) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("could not make {} writable again", path.display()))
    }
}

impl Drop for ReadOnlyDir {
    fn drop(&mut self) {
        let _ = Self::restore_mode(&self.path, self.restore);
    }
}

/// Create one more task in an existing project, through the product's own
/// command. Returns its id and title.
async fn create_task_in(driver: &WebDriver, project_id: &str, title_prefix: &str) -> Result<(String, String)> {
    let title = format!(
        "{title_prefix} {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let task = ui::invoke(
        driver,
        "create_task",
        json!({
            "params": {
                "projectId": project_id,
                "title": title,
                "description": "Never runs.",
                "model": "default",
                "planningMode": false,
                "dependencies": [],
                "category": Value::Null,
                "priority": Value::Null,
                "complexity": Value::Null,
                "impact": Value::Null,
                "securitySeverity": Value::Null,
            }
        }),
    )
    .await?;
    Ok((created_id(task, "create_task")?, title))
}

/// Click the card with this title and wait for its drawer.
///
/// Searching again until the deadline covers only finding a card to click: the
/// board rebuilds its cards when the task list changes, so a located element can
/// be gone by the time it is used, and `click()` then reports an error. A click
/// that is reported as delivered is not repeated. If its drawer does not appear,
/// the failure carries a record of what the page did with the click
/// ([`CLICK_PROBE`], [`click_evidence`]), so the next occurrence names its cause.
async fn open_drawer(driver: &WebDriver, task_id: &str, title: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let mut last_problem = String::new();
        for card in driver.find_all(By::Css("[data-testid=\"task-card\"]")).await? {
            let Ok(shown) = card.find(By::Css(TASK_TITLE)).await else {
                continue;
            };
            if shown.prop("textContent").await.ok().flatten().as_deref() != Some(title) {
                continue;
            }
            // A probe that cannot be installed must not change the outcome.
            let probed = match card.to_json() {
                Ok(element) => page(driver, CLICK_PROBE, vec![element]).await.is_ok(),
                Err(_) => false,
            };
            match card.click().await {
                Ok(()) => {
                    let selector = format!("{TASK_DRAWER}[data-task-id=\"{task_id}\"]");
                    let shown = ui::visible(driver, &selector).await;
                    // The probe watches one click; it must not outlive it.
                    let evidence = match (&shown, probed) {
                        (Err(_), true) => Some(click_evidence(driver).await),
                        _ => None,
                    };
                    if probed {
                        stop_click_probe(driver).await;
                    }
                    if let Err(error) = shown {
                        let evidence = evidence
                            .unwrap_or_else(|| "the click probe could not be installed".to_string());
                        return Err(error.context(format!(
                            "clicking the card for {title:?} did not open its drawer\n\
                             page record of the click: {evidence}"
                        )));
                    }
                    return Ok(());
                }
                Err(error) => {
                    last_problem = error.to_string();
                    if probed {
                        stop_click_probe(driver).await;
                    }
                }
            }
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("could not open the drawer for {title:?}: no clickable card ({last_problem})");
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Installed in the page just before a card is clicked. It records, in
/// `window.__clickProbe`, every pointer, mouse and drag event that reaches the
/// document (and every scroll), every removal of the card being clicked or of the element the
/// button went down on, every insertion or removal of a drawer, and any page
/// error. Capture phase and passive: it does not change how the page handles
/// the events.
const CLICK_PROBE: &str = r#"
const card = arguments[0];
if (window.__clickProbe) { window.__clickProbe.stop(); }
const t0 = performance.now();
// The board labels each card's wrapper with the task it shows (`data-card-task-id`).
const taskOf = (node) => { const w = node.closest && node.closest('[data-card-task-id]'); return w ? w.getAttribute('data-card-task-id') : null; };
const probe = { events: [], removed: [], drawer: [], errors: [], card, taskAtInstall: taskOf(card), stop: null };
const stamp = () => Math.round(performance.now() - t0);
const name = (node) => (node.getAttribute && node.getAttribute('data-testid')) || (node.tagName || String(node)).toLowerCase();
const describe = (node) => {
  const own = node.closest && node.closest('[data-testid="task-card"]');
  return name(node) + (own ? (own === card ? ' (in the clicked card)' : ' (in another card)') : '');
};
const types = ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click', 'dragstart', 'dragend', 'pointercancel', 'contextmenu'];
// WebKit sends no click when the element the button went down on has left the
// page by the time it comes up, so the press target is tracked on its own.
let pressed = null;
const listener = (e) => {
  if (e.type === 'mousedown' || e.type === 'pointerdown') { pressed = e.target; }
  probe.events.push({
    t: stamp(), type: e.type, target: describe(e.target),
    pressTargetConnected: pressed ? pressed.isConnected : null, cardConnected: card.isConnected,
    cardShowsTask: taskOf(card), targetShowsTask: taskOf(e.target),
    cardTop: Math.round(card.getBoundingClientRect().top),
    x: Math.round(e.clientX), y: Math.round(e.clientY),
  });
};
for (const type of types) { window.addEventListener(type, listener, { capture: true, passive: true }); }
// Scrolling moves the card under a pointer that has not moved, so it is recorded
// with the card's position. Bounded: a long animation must not fill the record.
const scrolled = (e) => {
  if (probe.events.length >= 200) { return; }
  const target = e.target;
  probe.events.push({
    t: stamp(), type: 'scroll', target: target === document ? 'document' : name(target),
    scrollLeft: target.scrollLeft === undefined ? null : Math.round(target.scrollLeft),
    cardTop: Math.round(card.getBoundingClientRect().top), cardLeft: Math.round(card.getBoundingClientRect().left),
  });
};
window.addEventListener('scroll', scrolled, { capture: true, passive: true });
const isDrawer = (node) => node.nodeType === 1
  && (node.matches('[data-testid="task-drawer"]') || node.querySelector('[data-testid="task-drawer"]'));
const observer = new MutationObserver((records) => {
  for (const record of records) {
    for (const node of record.addedNodes) {
      if (isDrawer(node)) { probe.drawer.push({ t: stamp(), what: 'drawer added' }); }
    }
    for (const node of record.removedNodes) {
      if (isDrawer(node)) { probe.drawer.push({ t: stamp(), what: 'drawer removed' }); }
      if (node === card || (node.contains && node.contains(card))) {
        probe.removed.push({ t: stamp(), what: name(node) });
      }
      if (pressed && (node === pressed || (node.contains && node.contains(pressed)))) {
        probe.removed.push({ t: stamp(), what: 'press target ' + name(pressed) + ' removed with ' + name(node) });
      }
    }
  }
});
observer.observe(document.body, { childList: true, subtree: true });
// A panic or an uncaught error in the page: what the log would show if it were collected.
const failed = (e) => probe.errors.push({ t: stamp(), what: String(e.message || e.reason || e.type).slice(0, 300) });
window.addEventListener('error', failed);
window.addEventListener('unhandledrejection', failed);
const consoleError = console.error;
console.error = (...args) => {
  probe.errors.push({ t: stamp(), what: args.map(String).join(' ').slice(0, 300) });
  return consoleError.apply(console, args);
};
probe.stop = () => {
  window.removeEventListener('scroll', scrolled, { capture: true });
  window.removeEventListener('error', failed);
  window.removeEventListener('unhandledrejection', failed);
  console.error = consoleError;
  for (const type of types) { window.removeEventListener(type, listener, { capture: true }); }
  observer.disconnect();
};
window.__clickProbe = probe;
return true;
"#;

/// Remove the probe's listeners, observer and console wrapper. Best effort: a
/// page that is gone has nothing left to stop.
async fn stop_click_probe(driver: &WebDriver) {
    let _ = page(
        driver,
        "if (window.__clickProbe) { window.__clickProbe.stop(); window.__clickProbe = null; } return true;",
        vec![],
    )
    .await;
}

/// What the page recorded about a click that opened no drawer, as one line of
/// JSON: the events it saw, whether the clicked card was removed or replaced,
/// where the board's columns were scrolled, where the card sits, and which
/// element is at the card's centre.
async fn click_evidence(driver: &WebDriver) -> String {
    let script = r#"
      const probe = window.__clickProbe;
      if (!probe) { return { probe: 'missing' }; }
      const card = probe.card;
      const columns = document.querySelector('.snap-x.snap-mandatory');
      const rect = card.getBoundingClientRect();
      const at = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
      const taskOf = (node) => { const w = node.closest && node.closest('[data-card-task-id]'); return w ? w.getAttribute('data-card-task-id') : null; };
      const titleOf = (c) => (c.querySelector('[data-testid="task-title"]') || {}).textContent;
      const title = titleOf(card);
      const sameTitle = [...document.querySelectorAll('[data-testid="task-card"]')]
        .filter((c) => titleOf(c) === title);
      return {
        events: probe.events,
        cardRemovedFromBoard: probe.removed,
        drawerInsertedOrRemoved: probe.drawer,
        pageErrors: probe.errors,
        clickedCardStillConnected: card.isConnected,
        cardsWithThatTitleNow: sameTitle.length,
        clickedCardIsTheCurrentOne: sameTitle.includes(card),
        columnsScrollLeft: columns ? Math.round(columns.scrollLeft) : null,
        columnsScrollWidth: columns ? columns.scrollWidth : null,
        columnsClientWidth: columns ? columns.clientWidth : null,
        cardRect: [rect.left, rect.top, rect.width, rect.height].map(Math.round),
        viewport: [window.innerWidth, window.innerHeight],
        elementAtCardCentre: at ? (at.getAttribute('data-testid') || at.tagName.toLowerCase())
          + (card.contains(at) ? ' (inside the clicked card)' : ' (outside the clicked card)') : null,
        taskShownByTheCardWhenClicked: probe.taskAtInstall,
        taskShownByTheCardNow: taskOf(card),
        drawersNow: [...document.querySelectorAll('[data-testid="task-drawer"]')].map((d) => d.getAttribute('data-task-id')),
      };
    "#;
    match page(driver, script, vec![]).await {
        Ok(value) => value.to_string(),
        Err(error) => format!("the record could not be read: {error:#}"),
    }
}

async fn close_drawer(driver: &WebDriver) -> Result<()> {
    ui::visible(driver, "[data-testid=\"task-drawer-close\"]")
        .await?
        .click()
        .await
        .context("could not close the drawer")?;
    let started = Instant::now();
    while count(driver, TASK_DRAWER).await? != 0 {
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the drawer did not close");
        }
        tokio::time::sleep(POLL).await;
    }
    Ok(())
}

/// Run a small script against the page and hand back its JSON answer.
async fn page(driver: &WebDriver, script: &str, args: Vec<Value>) -> Result<Value> {
    Ok(driver
        .execute(script, args)
        .await
        .context("could not run a script in the window")?
        .json()
        .clone())
}

async fn count(driver: &WebDriver, selector: &str) -> Result<usize> {
    let found = page(
        driver,
        "return document.querySelectorAll(arguments[0]).length;",
        vec![Value::String(selector.to_string())],
    )
    .await?;
    Ok(found.as_u64().unwrap_or_default() as usize)
}

/// The `textContent` of the first match, which WebDriver's rendered text
/// would omit for a truncated or clamped element.
async fn text_of(driver: &WebDriver, selector: &str) -> Result<Option<String>> {
    let found = page(
        driver,
        "const e = document.querySelector(arguments[0]); return e ? e.textContent : null;",
        vec![Value::String(selector.to_string())],
    )
    .await?;
    Ok(found.as_str().map(str::to_string))
}

/// Everything the open drawer holds, as text.
async fn drawer_text(driver: &WebDriver) -> Result<String> {
    Ok(text_of(driver, TASK_DRAWER).await?.unwrap_or_default())
}

async fn output_lines(driver: &WebDriver) -> Result<Vec<String>> {
    let found = page(
        driver,
        "return Array.from(document.querySelectorAll(arguments[0])).map(e => e.textContent);",
        vec![Value::String(DRAWER_OUTPUT_LINE.to_string())],
    )
    .await?;
    Ok(found
        .as_array()
        .map(|lines| lines.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default())
}

/// The drawer's Activity rows, oldest first, as `(kind, title, detail)`.
async fn timeline_rows(driver: &WebDriver) -> Result<Vec<(String, String, String)>> {
    let found = page(
        driver,
        "return Array.from(document.querySelectorAll(arguments[0])).map(row => [\
           row.dataset.kind || '', \
           (row.querySelector('[data-testid=\"task-drawer-timeline-title\"]') || {}).textContent || '', \
           (row.querySelector('[data-testid=\"task-drawer-timeline-detail\"]') || {}).textContent || '']);",
        vec![Value::String(DRAWER_TIMELINE_ROW.to_string())],
    )
    .await?;
    Ok(found
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_array)
                .map(|cells| {
                    let cell = |i: usize| cells.get(i).and_then(Value::as_str).unwrap_or_default().trim().to_string();
                    (cell(0), cell(1), cell(2))
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Wait until the drawer's Activity shows a row of `kind`, and return all
/// its rows.
async fn await_timeline_reaching(driver: &WebDriver, kind: &str) -> Result<Vec<(String, String, String)>> {
    let started = Instant::now();
    loop {
        let rows = timeline_rows(driver).await?;
        if rows.iter().any(|(k, _, _)| k == kind) {
            return Ok(rows);
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the drawer's Activity never showed a {kind} row; it showed {rows:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

/// A failed run, a retry and a successful run, in that order and each once,
/// with the failure's reason and without the agent's own words.
fn assert_timeline_story(rows: &[(String, String, String)]) -> Result<()> {
    let kinds: Vec<&str> = rows.iter().map(|(k, _, _)| k.as_str()).collect();
    let story = [
        "created",
        "run_started",
        "run_failed",
        "moved",
        "run_started",
        "run_completed",
        // The stand-in agent changes nothing, so there is nothing to review.
        "ai_review_skipped",
        "ready_for_review",
    ];
    let milestones: Vec<&str> = kinds.iter().copied().filter(|k| story.contains(k)).collect();
    // A move into In Progress before the first run is how this journey
    // starts the task; everything after it is the story under test.
    let from_first_run = milestones.iter().position(|k| *k == "run_started").unwrap_or(0);
    let told: Vec<&str> = std::iter::once("created").chain(milestones[from_first_run..].iter().copied()).collect();
    if told != story {
        bail!("the drawer's Activity tells {told:?}, not {story:?}; all rows: {rows:?}");
    }
    let failure = rows.iter().find(|(k, _, _)| k == "run_failed").expect("checked above");
    if failure.1 != "Coding failed" || !failure.2.contains(fake_agent::DETAILED_FAILURE[0]) {
        bail!("the failed run reads {failure:?}");
    }
    if !rows.iter().any(|(k, title, _)| k == "moved" && title == "Retried") {
        bail!("the retry is not shown as one: {rows:?}");
    }
    if !rows.iter().any(|(k, title, _)| k == "tool_used" && *title == format!("Used {}", fake_agent::PROGRESS_FIRST_TOOL)) {
        bail!("the agent's tool call is not on the timeline: {rows:?}");
    }
    for (_, title, detail) in rows {
        if title.contains(fake_agent::PROGRESS_FIRST_TEXT) || detail.contains(fake_agent::PROGRESS_FIRST_TEXT) {
            bail!("the agent's own output became a timeline row: {rows:?}");
        }
    }
    Ok(())
}

/// Wait until the element's text satisfies `accept`, and return it.
async fn await_text(
    driver: &WebDriver,
    selector: &str,
    accept: impl Fn(&str) -> bool,
    what: &str,
) -> Result<String> {
    let started = Instant::now();
    let mut last = None;
    loop {
        // Judged only on what is shown now; `last` is kept for the report.
        let current = text_of(driver, selector).await?.map(|t| t.trim().to_string());
        if let Some(text) = current.as_deref().filter(|text| accept(text)) {
            return Ok(text.to_string());
        }
        last = current.or(last);
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the drawer never showed {what} in {selector}; it last showed {last:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn await_drawer_status(driver: &WebDriver, status: &str) -> Result<()> {
    let selector = format!("{DRAWER_STATUS}[data-status=\"{status}\"]");
    let started = Instant::now();
    while count(driver, &selector).await? == 0 {
        if started.elapsed() > RENDER_DEADLINE {
            let shown = page(
                driver,
                "const e = document.querySelector(arguments[0]); return e ? e.dataset.status : null;",
                vec![Value::String(DRAWER_STATUS.to_string())],
            )
            .await?;
            bail!("the drawer never showed status {status:?}; it shows {shown}");
        }
        tokio::time::sleep(POLL).await;
    }
    Ok(())
}

async fn await_output_containing(driver: &WebDriver, expected: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let lines = output_lines(driver).await?;
        if lines.iter().any(|line| line.contains(expected)) {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the drawer's output never contained {expected:?}; it holds {lines:?}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn await_card_activity(driver: &WebDriver, title: &str, expected: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let found = page(
            driver,
            r#"
            const [title] = arguments;
            for (const card of document.querySelectorAll('[data-testid="task-card"]')) {
                const t = card.querySelector('[data-testid="task-title"]');
                if (t && t.textContent === title) {
                    const a = card.querySelector('[data-testid="task-activity"]');
                    return a ? a.textContent : null;
                }
            }
            return null;
            "#,
            vec![Value::String(title.to_string())],
        )
        .await?;
        if found.as_str() == Some(expected) {
            return Ok(());
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!("the card for {title:?} never showed activity {expected:?}; it shows {found}");
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The number of `agent-event` listeners the frontend holds registered with
/// Tauri, which it publishes on the document element.
async fn listener_count(driver: &WebDriver) -> Result<Option<i64>> {
    let found = page(
        driver,
        "return document.documentElement.dataset.agentEventListeners ?? null;",
        Vec::new(),
    )
    .await?;
    Ok(found.as_str().and_then(|n| n.parse().ok()))
}

async fn await_listener_count(driver: &WebDriver, expected: i64) -> Result<()> {
    let started = Instant::now();
    loop {
        let found = listener_count(driver).await?;
        if found == Some(expected) {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the frontend holds {found:?} agent-event listeners, expected {expected}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn assert_not_offered(driver: &WebDriver, control: &str, what: &str) -> Result<()> {
    if count(driver, control).await? != 0 {
        bail!("the drawer offers {what}");
    }
    Ok(())
}

async fn await_absent_from_drawer(driver: &WebDriver, control: &str, what: &str) -> Result<()> {
    let started = Instant::now();
    while count(driver, control).await? != 0 {
        if started.elapsed() > RENDER_DEADLINE {
            bail!("the drawer still offers {what}");
        }
        tokio::time::sleep(POLL).await;
    }
    Ok(())
}

/// Open the board and confirm a card with this title is in this column.
async fn show_on_board(
    driver: &WebDriver,
    project_id: &str,
    column: &str,
    title: &str,
) -> Result<()> {
    open_board(driver, project_id).await?;
    assert_card_in_column(driver, column, title).await
}

async fn fail_then_retry(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
) -> Result<ExecutedTask> {
    ui::assert_frontend_is_real(driver).await?;

    let Prerequisites {
        project_id,
        task_id,
        title,
    } = create_prerequisites(
        driver,
        repository,
        "Retry journey",
        "Fails once, then succeeds when the task is retried.",
    )
    .await?;

    let already_ran = agent_runs(agent)?.len();
    if already_ran != 0 {
        bail!("the agent ran {already_ran} times before the task was ever started");
    }

    // --- Stage A: the first execution really fails -------------------------
    ui::invoke(
        driver,
        "update_task_status",
        json!({ "taskId": task_id, "status": "in_progress" }),
    )
    .await?;

    let first_run = await_agent_runs(agent, 1).await?;
    // The executor gives every execution a fresh session id, so this is what
    // tells the two runs apart later. Records come back in start order, but
    // identity is what the selection below relies on, not position.
    // An empty value is as unusable here as a missing one: it identifies no
    // execution, and taking it would make every later comparison against it
    // meaningless while still looking like a session.
    let failed_session = first_run[0]
        .flag("--session-id")
        .filter(|session| !session.is_empty())
        .context("the failed run carried no usable --session-id to tell the retry apart from")?
        .to_os_string();
    let failed_worktree = resolve(&first_run[0].working_dir);
    if !failed_worktree.starts_with(resolve(repository.state_root())) {
        bail!(
            "the first agent run happened in {}, which is outside this run's state root {}",
            failed_worktree.display(),
            repository.state_root().display()
        );
    }

    let failed = await_status(driver, &project_id, &task_id, &["error", "human_review"]).await?;
    if status_of(&failed) != Some("error") {
        bail!(
            "the agent reported a failure but the task settled at {:?} instead of error",
            status_of(&failed)
        );
    }

    // The failure the product recorded is the one this executable reported.
    // Anything else would mean the task failed for a reason the journey did
    // not arrange, and the retry below would be recovering from the wrong
    // thing.
    let reported = failed.get("error_message").and_then(Value::as_str);
    let expected = fake_agent::recorded_failure();
    if reported != Some(expected.as_str()) {
        bail!(
            "the task carries error message {reported:?}, but the agent's failure should be \
             recorded as {expected:?} — the failure under test is not the one that happened"
        );
    }
    let phase = failed.get("phase").and_then(Value::as_str);
    if phase != Some("failed") {
        bail!("a failed execution should leave the task in the failed phase, found {phase:?}");
    }

    // The failure reached the disk before anything retried it, so a user who
    // closed the application here would find it as they left it.
    let failure_on_disk = ExecutedTask {
        id: task_id.clone(),
        project_id: project_id.clone(),
        title: title.clone(),
        worktree_path: failed_worktree.clone(),
    };
    assert_persisted(repository.state_root(), &failure_on_disk, "error")?;

    // And a user would see it, in the column the board keeps for failures.
    open_board(driver, &project_id).await?;
    assert_card_in_column(driver, ERROR_COLUMN, &title).await?;

    // The work the failed attempt produced is still there, and the task still
    // knows where it is. This is what the retry below has to continue from.
    let failed_branch = failed
        .get("branch_name")
        .and_then(Value::as_str)
        .context("a task that ran should record the branch its execution used")?
        .to_string();
    let recorded_after_failure = failed
        .get("worktree_path")
        .and_then(Value::as_str)
        .map(|path| resolve(Path::new(path)));
    if recorded_after_failure.as_ref() != Some(&failed_worktree) {
        bail!(
            "after the failure the task records worktree {recorded_after_failure:?}, but the \
             agent ran in {}",
            failed_worktree.display()
        );
    }
    if !failed_worktree.is_dir() {
        bail!(
            "the failed attempt's worktree {} is gone; a failure must not discard the work",
            failed_worktree.display()
        );
    }
    if !repository.has_branch(&failed_branch)? {
        bail!("the failed attempt's branch {failed_branch} is gone after the failure");
    }

    // --- Stage B: retry is a real product operation ------------------------
    //
    // The same command the board sends for any other status change, moving the
    // card from Error back to Queue. The reset that makes a retry possible is
    // the product's, in `classify_status_transition`: leaving Error clears the
    // phase, the progress and the error message, and deliberately keeps the
    // worktree and branch, so the queue's promotion finds a task ready to run
    // again where it left off.
    let retried = ui::invoke(
        driver,
        "update_task_status",
        json!({ "taskId": task_id, "status": "queue" }),
    )
    .await?;

    let queued_status = status_of(&retried);
    if queued_status != Some("queue") {
        bail!("retrying should put the task in the queue, found {queued_status:?}");
    }

    // Asserted on what the command returned rather than by polling: this reset
    // is synchronous inside the command, so reading it back later could not
    // tell the reset apart from the second execution having already started.
    let cleared = retried.get("error_message").cloned();
    if !matches!(cleared, None | Some(Value::Null)) {
        bail!("retrying the task left the previous failure's message behind: {cleared:?}");
    }
    let reset_phase = retried.get("phase").and_then(Value::as_str);
    if reset_phase != Some("idle") {
        bail!(
            "retrying the task should return it to the idle phase the poller looks for, found \
             {reset_phase:?}"
        );
    }
    let reset_progress = retried.get("overall_progress").and_then(Value::as_u64);
    if reset_progress != Some(0) {
        bail!("retrying the task should reset its progress, found {reset_progress:?}");
    }

    // Resetting execution state is not the same operation as throwing the work
    // away. The command returns the task still pointing at the worktree and
    // branch of the attempt that failed, because that is what the next run
    // continues from.
    let kept_worktree = retried
        .get("worktree_path")
        .and_then(Value::as_str)
        .map(|path| resolve(Path::new(path)));
    if kept_worktree.as_ref() != Some(&failed_worktree) {
        bail!(
            "retrying dropped the task's worktree: it now records {kept_worktree:?} instead of {}",
            failed_worktree.display()
        );
    }
    let kept_branch = retried.get("branch_name").and_then(Value::as_str);
    if kept_branch != Some(failed_branch.as_str()) {
        bail!("retrying dropped the task's branch: {kept_branch:?} instead of {failed_branch:?}");
    }

    // --- Stage C: the second attempt succeeds ------------------------------
    //
    // Nothing moves the task on from Queue here. The queue's own promotion
    // does, on its next pass, which is the rest of the decided retry path.
    let runs = await_agent_runs(agent, 2).await?;
    if runs.len() != 2 {
        bail!(
            "the retry started {} agent runs, expected exactly one more",
            runs.len() - 1
        );
    }
    let second_run = retry_among(&runs, &failed_session)?;
    if second_run.flag("--output-format") != Some(OsStr::new("stream-json")) {
        bail!(
            "the retried run did not use the streaming protocol the runner parses: {:?}",
            second_run.args
        );
    }
    let retried_worktree = resolve(&second_run.working_dir);
    if !retried_worktree.starts_with(resolve(repository.state_root())) {
        bail!(
            "the retried agent run happened in {}, which is outside this run's state root {}",
            retried_worktree.display(),
            repository.state_root().display()
        );
    }
    if retried_worktree == resolve(&repository.path_buf()) {
        bail!("the retried agent ran in the repository itself instead of a task worktree");
    }
    // The retry continued the failed attempt's work rather than starting over
    // somewhere else: same worktree, reattached by the executor.
    if retried_worktree != failed_worktree {
        bail!(
            "the retried agent ran in {} but the failed attempt's worktree is {} — the retry did \
             not continue the existing work",
            retried_worktree.display(),
            failed_worktree.display()
        );
    }

    let settled = await_status(driver, &project_id, &task_id, &["human_review", "error"]).await?;
    if status_of(&settled) == Some("error") {
        bail!(
            "the retried run failed instead of completing: {}",
            settled
                .get("error_message")
                .and_then(Value::as_str)
                .unwrap_or("no error message recorded")
        );
    }

    // The same settled state the success journey derives, reached the second
    // time round.
    let reported_model = settled.get("model").and_then(Value::as_str);
    if reported_model != Some(REPORTED_MODEL) {
        bail!(
            "the task does not carry the model the agent reported: expected {REPORTED_MODEL:?}, \
             found {reported_model:?} — the runner did not parse the retried run's output"
        );
    }
    let overall = settled.get("overall_progress").and_then(Value::as_u64);
    if overall != Some(90) {
        bail!("a settled task should report 90 percent overall, found {overall:?}");
    }
    let settled_phase = settled.get("phase").and_then(Value::as_str);
    if settled_phase != Some("complete") {
        bail!("a settled task should be in the complete phase, found {settled_phase:?}");
    }
    let cleared = settled.get("error_message").cloned();
    if !matches!(cleared, None | Some(Value::Null)) {
        bail!("a task that recovered still reports the failure it recovered from: {cleared:?}");
    }

    // The product records the worktree the retried agent actually ran in.
    let recorded_worktree = settled.get("worktree_path").and_then(Value::as_str);
    if recorded_worktree.map(|path| resolve(Path::new(path))) != Some(retried_worktree.clone()) {
        bail!(
            "the task records worktree {recorded_worktree:?} but the retried agent ran in {} — \
             the execution the journey observed is not the one the task remembers",
            retried_worktree.display()
        );
    }

    let settled_branch = settled.get("branch_name").and_then(Value::as_str);
    if settled_branch != Some(failed_branch.as_str()) {
        bail!("the recovered task records branch {settled_branch:?}, expected {failed_branch:?}");
    }
    if !repository.has_branch(&failed_branch)? {
        bail!(
            "the task still records branch {failed_branch} but the branch no longer exists — the \
             retry destroyed the work it produced"
        );
    }

    // And the board moved it out of Error into the reviewable column.
    open_board(driver, &project_id).await?;
    assert_card_in_column(driver, HUMAN_REVIEW_COLUMN, &title).await?;

    Ok(ExecutedTask {
        id: task_id,
        project_id,
        title,
        worktree_path: retried_worktree,
    })
}

/// What the journey learned about the task it drove.
struct ExecutedTask {
    id: String,
    /// The project the task belongs to, which is what `list_tasks` is keyed
    /// on: a journey that carries the task further has to be able to read it
    /// back through the product's own API.
    project_id: String,
    title: String,
    worktree_path: PathBuf,
}

async fn execute_one_task(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
) -> Result<ExecutedTask> {
    ui::assert_frontend_is_real(driver).await?;

    let Prerequisites {
        project_id,
        task_id,
        title,
    } = create_prerequisites(
        driver,
        repository,
        "Queue journey",
        "Exercises the queue, the runner and the agent boundary.",
    )
    .await?;

    // The "before" half of the proof: nothing has run yet, so anything the
    // journey observes later was caused by what it does next.
    let already_ran = agent_runs(agent)?.len();
    if already_ran != 0 {
        bail!("the agent ran {already_ran} times before the task was ever started");
    }

    // --- The one action under test -----------------------------------------
    //
    // Exactly what the board sends when a card is dropped into In Progress.
    // Nothing else is touched: the poller has to notice this on its own.
    ui::invoke(
        driver,
        "update_task_status",
        json!({ "taskId": task_id, "status": "in_progress" }),
    )
    .await?;

    // --- Proof A: the agent really ran -------------------------------------
    let invocation = {
        let started = Instant::now();
        loop {
            let recorded = agent_runs(agent)?;
            if let Some(first) = recorded.into_iter().next() {
                break first;
            }
            if started.elapsed() > EXECUTION_DEADLINE {
                bail!(
                    "the agent was never started: no invocation was recorded in {} within {}s \
                     after the task moved to in_progress",
                    agent.marker_dir().display(),
                    EXECUTION_DEADLINE.as_secs()
                );
            }
            tokio::time::sleep(POLL).await;
        }
    };

    // It was started as the product's agent, not as some other process that
    // happened to touch the marker directory.
    if invocation.flag("--output-format") != Some(OsStr::new("stream-json")) {
        bail!(
            "the agent was not invoked with the streaming protocol the runner parses: {:?}",
            invocation.args
        );
    }
    let prompt = invocation
        .prompt
        .as_deref()
        .context("the agent was invoked without reading a prompt from stdin")?;
    if prompt.is_empty() {
        bail!("the agent was invoked with an empty prompt");
    }
    // The prompt travels on stdin only, where other local processes cannot
    // read it and no argument size limit applies.
    if let Some(leaked) = invocation.args.iter().find(|arg| arg.to_string_lossy().contains(prompt)) {
        bail!("the prompt was also passed as an argument: {leaked:?}");
    }

    // And it ran inside a worktree this run owns, not in the repository and
    // not anywhere outside the isolated state root.
    //
    // Compared in resolved form throughout: the shell reports the directory it
    // is actually in, while the product builds its paths by joining strings,
    // and a symlinked temporary directory would otherwise make two names for
    // one place look like a mismatch.
    let worktree_path = invocation.working_dir.clone();
    let resolved_worktree = resolve(&worktree_path);
    if !resolved_worktree.starts_with(resolve(repository.state_root())) {
        bail!(
            "the agent ran in {}, which is outside this run's state root {} — isolation is \
             leaking",
            worktree_path.display(),
            repository.state_root().display()
        );
    }
    if resolved_worktree == resolve(&repository.path_buf()) {
        bail!("the agent ran in the repository itself instead of a task worktree");
    }

    // --- Proof B: the product carried the task to its reviewable state -----
    //
    // `AiReview` is what the executor sets the moment the agent succeeds.
    // What happens next depends on whether the fixture wrote anything: with
    // no `WRITE_FILE_VAR`, the scheduled review finds an empty diff and skips
    // straight to `HumanReview`; with it set, the diff is real and a review
    // agent actually runs (see `assert_agent_roles`) before the same
    // `HumanReview` settling. Either way the terminal state is read back
    // through the frontend's own `list_tasks`.
    let final_task =
        await_status(driver, &project_id, &task_id, &["human_review", "error"]).await?;
    if status_of(&final_task) == Some("error") {
        bail!(
            "the task failed instead of completing: {}",
            final_task
                .get("error_message")
                .and_then(Value::as_str)
                .unwrap_or("no error message recorded")
        );
    }

    // The runner parsed the fixture's own stdout: the model on the task is the
    // one only that executable reports. A task forced into this state by any
    // other means could not carry it.
    let reported_model = final_task.get("model").and_then(Value::as_str);
    if reported_model != Some(REPORTED_MODEL) {
        bail!(
            "the task does not carry the model the agent reported: expected {REPORTED_MODEL:?}, \
             found {reported_model:?} — the runner did not parse this agent's output"
        );
    }

    // The product recorded the same worktree the agent ran in.
    let recorded_worktree = final_task.get("worktree_path").and_then(Value::as_str);
    if recorded_worktree.map(|path| resolve(Path::new(path))) != Some(resolved_worktree.clone()) {
        bail!(
            "the task records worktree {recorded_worktree:?} but the agent ran in {} — the \
             execution the journey observed is not the one the task remembers",
            worktree_path.display()
        );
    }

    // --- Proof C: a user would see it --------------------------------------
    open_board(driver, &project_id).await?;
    assert_card_in_column(driver, HUMAN_REVIEW_COLUMN, &title).await?;

    Ok(ExecutedTask {
        id: task_id,
        project_id,
        title,
        worktree_path,
    })
}

/// Confirm the final state reached the disk, not only the application's memory.
///
/// The layout of the task store is the product's business and has changed
/// before, so this searches the run's own state root rather than hard-coding a
/// path that would quietly stop proving anything the day it moved.
fn assert_persisted(root: &Path, task: &ExecutedTask, status: &str) -> Result<()> {
    let mut examined = 0usize;
    let mut found_as: Vec<String> = Vec::new();
    for file in toml_files(root)? {
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        examined += 1;
        // A state root holds more than the task store, and not every file in it
        // has to parse for this assertion to mean something.
        let Ok(document) = contents.parse::<toml::Value>() else {
            continue;
        };
        let Some(record) = task_record(&document, &task.id) else {
            continue;
        };
        let title = record.get("title").and_then(toml::Value::as_str);
        let recorded = record.get("status").and_then(toml::Value::as_str);
        // One record carrying the id, the title and the settled status is the
        // whole claim. Others may exist — the store has had more than one
        // layout — and an older one lagging behind is not evidence that this
        // state was never written.
        if title == Some(task.title.as_str()) && recorded == Some(status) {
            return Ok(());
        }
        found_as.push(format!(
            "{} records it as {title:?} with status {recorded:?}",
            file.display()
        ));
    }

    if found_as.is_empty() {
        bail!(
            "no file under {} holds a task record with id {} — nothing was persisted ({examined} \
             TOML files examined)",
            root.display(),
            task.id
        );
    }

    bail!(
        "task {} is persisted, but not as the state the application reported — expected the title \
         {:?} with status {status:?}, and {}",
        task.id,
        task.title,
        found_as.join("; ")
    )
}

/// The stored record for `id` in one parsed task store, if it holds one.
///
/// `Storage::save_project_tasks` writes a `version` and an array of tasks, so
/// one file routinely holds several. That is why this reads the array and
/// matches on the record's own `id` rather than looking for the values anywhere
/// in the document: a file can perfectly well contain the wanted id, the wanted
/// title and the wanted status while no single task has all three, and a proof
/// that cannot tell those apart is not proving the task was persisted.
fn task_record<'a>(document: &'a toml::Value, id: &str) -> Option<&'a toml::value::Table> {
    document
        .get("tasks")?
        .as_array()?
        .iter()
        .filter_map(toml::Value::as_table)
        .find(|record| record.get("id").and_then(toml::Value::as_str) == Some(id))
}

/// Every `.toml` under `root`, recursively.
fn toml_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A directory that vanished mid-walk is not this assertion's
            // problem; the state root has a live application's runtime in it.
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "toml") {
                found.push(path);
            }
        }
    }
    Ok(found)
}

/// A path with its symlinks resolved, or the path itself when it cannot be
/// resolved — a missing directory is a failure for the assertion that asked,
/// not for this helper.
fn resolve(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The isolated project and task a journey drives.
struct Prerequisites {
    project_id: String,
    task_id: String,
    title: String,
}

/// Create one repository, one project and one task, through the frontend's own
/// command surface.
///
/// Created by invoking rather than by clicking: these are three wizard dialogs
/// whose interaction the harness journeys already cover, and driving them here
/// would add failure modes that say nothing about the queue. The commands and
/// their argument shapes are copied from `src/services/`, so this is the same
/// boundary the UI crosses.
async fn create_prerequisites(
    driver: &WebDriver,
    repository: &GitFixture,
    title_prefix: &str,
    description: &str,
) -> Result<Prerequisites> {
    let repository_id = created_id(
        ui::invoke(
            driver,
            "create_repository",
            // `GitFixture::create` already ran `git init` and committed, so
            // this is registering an existing repository root, not asking
            // the product to create one.
            json!({
                "localPath": repository.path(),
                "remoteUrl": Value::Null,
                "initialize": Value::Null,
            }),
        )
        .await?,
        "create_repository",
    )?;

    let project_id = created_id(
        ui::invoke(
            driver,
            "create_project",
            json!({
                "name": "Acceptance Product Journey",
                "repositoryId": repository_id,
                "agentType": "claude_code",
            }),
        )
        .await?,
        "create_project",
    )?;

    // Unique per run, so no leftover state could satisfy the board assertion.
    let title = format!(
        "{title_prefix} {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );

    let task = ui::invoke(
        driver,
        "create_task",
        json!({
            "params": {
                "projectId": project_id,
                "title": title,
                "description": description,
                "model": "default",
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
            }
        }),
    )
    .await?;
    let task_id = created_id(task.clone(), "create_task")?;

    if status_of(&task) != Some("backlog") {
        bail!("a new task should start in the backlog, got {task}");
    }

    Ok(Prerequisites {
        project_id,
        task_id,
        title,
    })
}

/// Reload the window and open the board for a project.
///
/// Reloaded, and this is not a workaround for a slow render. The prerequisites
/// are created by invoking the backend directly, which is the same boundary the
/// UI crosses but not the same path: the real wizards update the frontend's own
/// signals with what the command returned, and nothing pushes a newly created
/// project to a frontend that did not ask for it. This window listed its
/// projects when it mounted, before any of them existed, so without a reload
/// the rail is legitimately empty and the assertion would be measuring the
/// test's shortcut rather than the product.
///
/// After the reload the frontend loads from disk, which makes every proof that
/// follows strictly stronger: what a user sees is rendered from the persisted
/// state, not from anything this process held in memory.
async fn open_board(driver: &WebDriver, project_id: &str) -> Result<()> {
    driver
        .refresh()
        .await
        .context("could not reload the application window")?;
    ui::assert_frontend_is_real(driver).await?;

    ui::visible(
        driver,
        &format!("[data-testid=\"rail-project-{project_id}\"]"),
    )
    .await?
    .click()
    .await
    .context("could not select the project in the rail")?;
    ui::visible(driver, KANBAN_BOARD)
        .await
        .context("selecting the project did not open the board")?;
    Ok(())
}

/// Wait until a board column holds a card with this title.
///
/// Read from the card titles rather than from the column's rendered text. A
/// card title is `line-clamp-2`, which is `display: -webkit-box` with
/// `overflow: hidden`, and WebDriver's rendered-text extraction omits it — the
/// column reports its heading, the badges and the progress, but not the one
/// string these proofs are about. `textContent` is what the element actually
/// holds, and holding it is the claim: the board received this task and put it
/// in this column.
async fn assert_card_in_column(driver: &WebDriver, column: &str, title: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let element = ui::visible(driver, column).await?;
        let titles = card_titles(&element).await?;
        if titles.iter().any(|shown| shown == title) {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!(
                "the board never showed {title:?} in {column} within {}s; the column holds \
                 {titles:?}",
                RENDER_DEADLINE.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Wait until the product reports this task with one of the settled statuses,
/// reading it back through the frontend's own `list_tasks`.
///
/// `settled` is every status the journey is prepared to see next, so a task
/// that lands in the wrong one is reported as the wrong destination rather
/// than as a timeout that says nothing about what happened.
async fn await_status(
    driver: &WebDriver,
    project_id: &str,
    task_id: &str,
    settled: &[&str],
) -> Result<Value> {
    let started = Instant::now();
    let mut last = String::from("nothing yet");
    loop {
        let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
        if let Some(found) = find_task(&listed, task_id) {
            last = status_of(&found).unwrap_or("unreadable").to_string();
            if settled.contains(&last.as_str()) {
                return Ok(found);
            }
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!(
                "the task never reached any of {settled:?} within {}s; last observed status was \
                 {last}",
                EXECUTION_DEADLINE.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Wait until the fixture has recorded `count` agent runs, and return them.
async fn await_agent_runs(agent: &FakeAgent, count: usize) -> Result<Vec<fake_agent::Invocation>> {
    let started = Instant::now();
    loop {
        let recorded = agent_runs(agent)?;
        if recorded.len() >= count {
            return Ok(recorded);
        }
        if started.elapsed() > EXECUTION_DEADLINE {
            bail!(
                "the agent ran {} times within {}s, expected {count}: no further invocation was \
                 recorded in {}",
                recorded.len(),
                EXECUTION_DEADLINE.as_secs(),
                agent.marker_dir().display()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The titles of the task cards in one board column.
async fn card_titles(column: &WebElement) -> Result<Vec<String>> {
    let elements = column
        .find_all(By::Css(TASK_TITLE))
        .await
        .context("could not look for task cards in the column")?;

    let mut titles = Vec::with_capacity(elements.len());
    for element in elements {
        let text = element
            .prop("textContent")
            .await
            .context("could not read a task card's title")?
            .unwrap_or_default();
        titles.push(text.trim().to_string());
    }
    Ok(titles)
}

/// The recorded invocations that are actual agent runs.
///
/// Not every execution of `claude` is an agent doing work. The product also
/// asks the executable to identify itself — `check_claude_cli` runs
/// `claude --version` to report whether an agent is installed, and the
/// frontend calls that whenever it wants to show the answer. That probe is
/// correct behaviour and says nothing about whether a task ran, but it lands
/// in the same record directory, so counting records would make "the agent has
/// not run yet" mean "the frontend has not asked what version is installed
/// yet" — two facts with no relationship, one of which the journey does not
/// control.
///
/// A run is identified by the flag the runner always passes and the probe
/// never does: `-p`, which makes the CLI read its prompt from stdin.
fn agent_runs(agent: &FakeAgent) -> Result<Vec<fake_agent::Invocation>> {
    Ok(agent
        .invocations()?
        .into_iter()
        .filter(|invocation| invocation.has_flag("-p"))
        .collect())
}

/// Which of the executor's three agent roles produced a run, told apart by
/// the exact `--allowedTools` set the executor gives each one
/// (`queue::executor`): the coding agent is the only one with `Bash`; the fix
/// agent is the only other one with `Edit`; the review agent has neither.
/// There is no role marker to read directly -- this is the one durable,
/// external difference between them a fixture that only echoes a fixed
/// string back can still be told apart by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentRole {
    Execution,
    Review,
    Fix,
}

fn agent_role(invocation: &fake_agent::Invocation) -> Result<AgentRole> {
    let tools = invocation
        .flag("--allowedTools")
        .context("an agent run had no --allowedTools flag")?
        .to_string_lossy();
    Ok(if tools.contains("Bash") {
        AgentRole::Execution
    } else if tools.contains("Edit") {
        AgentRole::Fix
    } else {
        AgentRole::Review
    })
}

/// Assert the whole journey ran exactly `execution` coding runs, `review`
/// review runs and `fix` fix runs, in that role order by when each run started
/// (the fixture's own start sequence, see `FakeAgent::invocations`) -- not merely a total
/// count, which would pass just as well if a review silently never ran at
/// all. Fixed instead of `>= 1` checks on purpose: this is exactly the
/// assertion strength that let the diff-boundary bug (AI review never firing
/// for a real git-backed change) hide behind a passing "ran once" check.
fn assert_agent_roles(
    agent: &FakeAgent,
    journey: &str,
    execution: usize,
    review: usize,
    fix: usize,
) -> Result<()> {
    let runs = agent_runs(agent)?;
    let roles = runs.iter().map(agent_role).collect::<Result<Vec<_>>>()?;

    let expected: Vec<AgentRole> = std::iter::repeat_n(AgentRole::Execution, execution)
        .chain(std::iter::repeat_n(AgentRole::Review, review))
        .chain(std::iter::repeat_n(AgentRole::Fix, fix))
        .collect();

    if roles != expected {
        bail!(
            "{journey}: expected agent roles {expected:?} ({execution} execution, {review} \
             review, {fix} fix), got {roles:?} ({} total runs)",
            roles.len()
        );
    }
    Ok(())
}

/// The run that is not the one `failed_session` identifies.
///
/// Records come back in the order the runs started, but this selects by
/// identity rather than by position: both attempts of a retried task share a
/// worktree and a set of flags, which is what makes picking the wrong one
/// dangerous rather than merely wrong: every assertion the journey makes
/// about the retry would still pass while describing the attempt it retried.
///
/// The executor gives each execution a fresh session id, so that is the one
/// value the two runs cannot share, and identity is what this selects on. A
/// candidate has to carry an identity of its own to be one: the runner omits
/// the flag entirely for a `None` session and passes it empty for an empty one,
/// so both are shapes the product can really produce, and neither is a
/// different identity. Accepting either would let the journey keep passing on
/// the day the executor stopped giving a retry a session of its own — the one
/// thing this selection exists to notice.
fn retry_among<'a>(
    runs: &'a [fake_agent::Invocation],
    failed_session: &OsStr,
) -> Result<&'a fake_agent::Invocation> {
    let retries: Vec<&fake_agent::Invocation> = runs
        .iter()
        .filter(|run| {
            matches!(
                run.flag("--session-id"),
                Some(session) if !session.is_empty() && session != failed_session
            )
        })
        .collect();

    match retries.as_slice() {
        [only] => Ok(only),
        [] => bail!(
            "no agent run carries a session of its own other than {failed_session:?}, so the \
             retry cannot be told apart from the attempt it was retrying"
        ),
        many => bail!(
            "{} agent runs carry a session other than the failed attempt's, expected exactly one \
             retry",
            many.len()
        ),
    }
}

fn created_id(value: Value, command: &str) -> Result<String> {
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("{command} returned no id: {value}"))
}

fn status_of(task: &Value) -> Option<&str> {
    task.get("status").and_then(Value::as_str)
}

fn find_task(listed: &Value, id: &str) -> Option<Value> {
    listed
        .as_array()?
        .iter()
        .find(|task| task.get("id").and_then(Value::as_str) == Some(id))
        .cloned()
}

/// Start the product on a configuration that says `placement = "auto"`.
///
/// That is the spelling every configuration written before SlashIt stopped
/// delegating to Worktrunk (`wt`) carries, and under it a machine with `wt`
/// installed used to get worktrees wherever `wt` put them, outside the
/// directory the harness owns and cleans. It now means the same as
/// `"managed"`, so every journey that checks its worktrees stay inside the
/// state root also checks that the old spelling still parses and no longer
/// hands placement to anything, `wt` on `PATH` or not.
///
/// Written as the product's own configuration file, before the first launch,
/// because placement is read once at startup and no command exposes it.
fn start_on_the_legacy_auto_placement(config_file: &Path) -> Result<()> {
    let parent = config_file
        .parent()
        .context("the config file has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;
    std::fs::write(config_file, "[worktree]\nplacement = \"auto\"\n")
        .with_context(|| format!("could not write {}", config_file.display()))
}

/// The smallest real git repository the product will accept as a project.
///
/// A real one, not a mock: the executor runs `git worktree add`, `git add` and
/// `git commit` against it, and the point of a product acceptance test is that
/// those are the actual commands that run.
struct GitFixture {
    path: PathBuf,
    state_root: PathBuf,
    /// This fixture's own bare `origin`, if it has one.
    origin: Option<PathBuf>,
}

impl GitFixture {
    fn create(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)
            .with_context(|| format!("could not create {}", path.display()))?;

        git(path, &["init", "--quiet", "--initial-branch=main"])?;
        // Repository-local, so the run depends on nothing about the machine's
        // global git configuration — and so the commit the executor makes
        // inside the worktree has an identity, since worktrees share this
        // configuration with the repository.
        git(path, &["config", "user.name", "SlashIt Acceptance"])?;
        git(path, &["config", "user.email", "acceptance@localhost"])?;
        // A developer with commit signing enabled globally would otherwise
        // have the product's `git commit` block on a passphrase prompt, and a
        // blocked commit inside the application is very hard to read from out
        // here.
        git(path, &["config", "commit.gpgsign", "false"])?;

        std::fs::write(path.join("README.md"), "Acceptance fixture repository.\n")
            .context("could not write the fixture's README")?;
        git(path, &["add", "README.md"])?;
        git(path, &["commit", "--quiet", "-m", "initial commit"])?;

        let state_root = path
            .parent()
            .context("the fixture has no parent directory")?
            .to_path_buf();

        // A bare `origin` beside it, inside the state root, with its `HEAD`
        // recorded locally the way `git clone` leaves it. The product starts
        // every ordinary task branch at `refs/remotes/origin/<default>` and
        // refuses a repository that has none, without ever fetching.
        //
        // Named after this fixture, because a journey can make several in one
        // state root, and taken with `create_dir`, which fails rather than
        // hand over a directory somebody already has. A shared origin would
        // receive every fixture's unrelated `main`, and all but the first
        // push would be rejected unless the commits happened to be identical.
        let name = path
            .file_name()
            .context("the fixture has no directory name")?
            .to_str()
            .context("the fixture's name is not UTF-8")?;
        if name.ends_with(".origin.git") {
            bail!("{name} is where another fixture would keep its origin");
        }
        let origin = state_root.join(format!("{name}.origin.git"));
        std::fs::create_dir(&origin)
            .with_context(|| format!("could not create {}", origin.display()))?;
        let url = origin.to_str().context("the origin path is not UTF-8")?;
        git(path, &["init", "--quiet", "--bare", url])?;
        git(path, &["remote", "add", "origin", url])?;
        git(path, &["push", "--quiet", "origin", "main"])?;
        git(path, &["remote", "set-head", "origin", "main"])?;

        Ok(Self {
            path: path.to_path_buf(),
            state_root,
            origin: Some(origin),
        })
    }

    fn path(&self) -> String {
        self.path.to_string_lossy().to_string()
    }

    fn path_buf(&self) -> PathBuf {
        self.path.clone()
    }

    /// The run's state root, which everything this journey creates — including
    /// the worktrees the product makes — has to stay inside.
    fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Whether the repository still has `branch`.
    ///
    /// A task's branch is where the work of its executions lives, so a journey
    /// asking this is asking whether that work survived. `rev-parse --verify`
    /// answers with its exit status alone, which is why this does not go
    /// through [`git`], whose job is to fail the journey when a command fails.
    fn has_branch(&self, branch: &str) -> Result<bool> {
        let status = std::process::Command::new("git")
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .with_context(|| format!("could not look up branch {branch}"))?;
        Ok(status.status.success())
    }

    /// Whether git still has a worktree registration for `path`.
    ///
    /// The directory being gone and git having forgotten it are two different
    /// facts, and a destructive journey needs both: `git worktree remove`
    /// deletes a directory *and* a record under `.git/worktrees`, and a
    /// cleanup that left the record behind would leave the repository unable
    /// to give this branch a worktree again.
    fn registers_worktree(&self, path: &Path) -> Result<bool> {
        let output = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .context("could not list the fixture's worktrees")?;
        if !output.status.success() {
            bail!(
                "git worktree list failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let wanted = resolve(path);
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .any(|listed| resolve(Path::new(listed)) == wanted))
    }

    /// The commit `branch` points at, or `None` if there is no such branch.
    fn branch_tip(&self, branch: &str) -> Result<Option<String>> {
        let output = std::process::Command::new("git")
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .with_context(|| format!("could not resolve branch {branch}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    }

    /// The contents of `file` as `revision` has it, or `None` if either the
    /// revision or the path is not there.
    ///
    /// This is what separates work that is merely on disk from work that is
    /// committed: a file only in the worktree dies with the directory, while
    /// one in the commit survives for as long as something still points at
    /// that commit.
    fn file_at(&self, revision: &str, file: &str) -> Result<Option<String>> {
        let output = std::process::Command::new("git")
            .args(["show", &format!("{revision}:{file}")])
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .with_context(|| format!("could not read {file} at {revision}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&output.stdout).to_string()))
    }

    /// Every ref in the repository from which `commit` can still be reached.
    ///
    /// Empty means the commit is unreachable: still in the object database
    /// until git collects it, but with nothing naming it, and nothing in the
    /// product able to find it again. That is the question a destructive
    /// journey is really asking about the work an agent produced.
    fn refs_reaching(&self, commit: &str) -> Result<Vec<String>> {
        let output = std::process::Command::new("git")
            .args(["for-each-ref", "--contains", commit, "--format=%(refname)"])
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .with_context(|| format!("could not look for refs reaching {commit}"))?;
        if !output.status.success() {
            bail!(
                "git for-each-ref --contains failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect())
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        // The fixture must come out the same on a developer's machine and on a
        // hosted runner, so its own commands read no global or system
        // configuration and never stop to ask a question. The product's git
        // calls are its own business; what they need is set as repository
        // configuration below, which they do read.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("could not run git {args:?} in {}", dir.display()))?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The file the destructive journeys have the agent leave behind.
///
/// A destructive test is only worth running against work that exists: the
/// question these journeys ask is what the product does to an agent's output,
/// and an empty worktree cannot answer it.
const WORK_FILE: &str = "agent-work.txt";

/// How long the product gets to finish a cleanup it started.
///
/// Both destructive paths spawn the removal and return before it runs, so the
/// journey has to wait for something rather than read it straight away. Thirty
/// seconds is the same headroom the board assertions get; the removal itself
/// is a handful of `git` calls against a repository with two commits in it.
const CLEANUP_DEADLINE: Duration = Duration::from_secs(30);

/// The Kanban column a finished task lands in.
const DONE_COLUMN: &str = "[data-testid=\"column-done\"]";

/// What the journey proved about a task's work before anything destroyed it.
struct EstablishedWork {
    /// The branch the executor created for the task and committed to.
    branch: String,
    /// The commit that branch pointed at, holding the agent's file.
    commit: String,
}

/// Prove that a real agent run left real, committed work on the task's own
/// branch, and return the identity of that work.
///
/// Everything here is read from git rather than from the product, and each
/// fact is separate from the others on purpose. That the directory is there
/// says nothing about whether git knows about it; that a file is in the
/// directory says nothing about whether it was committed; and it is only the
/// commit that makes the later question — whether destroying the worktree
/// destroys the work — mean anything at all.
async fn establish_work(
    driver: &WebDriver,
    repository: &GitFixture,
    executed: &ExecutedTask,
) -> Result<EstablishedWork> {
    let task = ui::invoke(
        driver,
        "list_tasks",
        json!({ "projectId": executed.project_id }),
    )
    .await?;
    let task = find_task(&task, &executed.id)
        .with_context(|| format!("the product no longer lists task {}", executed.id))?;

    let branch = task
        .get("branch_name")
        .and_then(Value::as_str)
        .context("the executed task records no branch, so it produced nothing to destroy")?
        .to_string();

    if !executed.worktree_path.is_dir() {
        bail!(
            "the product records worktree {} but nothing is there",
            executed.worktree_path.display()
        );
    }
    if !repository.registers_worktree(&executed.worktree_path)? {
        bail!(
            "git has no worktree registration for {} — the directory was not created as a git \
             worktree of the fixture",
            executed.worktree_path.display()
        );
    }

    let on_disk = std::fs::read_to_string(executed.worktree_path.join(WORK_FILE))
        .with_context(|| format!("the agent left no {WORK_FILE} in its worktree"))?;
    if on_disk != fake_agent::WORK_CONTENT {
        bail!("{WORK_FILE} holds {on_disk:?}, which is not what the agent writes");
    }

    let committed = repository
        .file_at(&branch, WORK_FILE)?
        .with_context(|| format!("branch {branch} does not carry {WORK_FILE}"))?;
    if committed != fake_agent::WORK_CONTENT {
        bail!("branch {branch} carries {committed:?} as {WORK_FILE}, not what the agent wrote");
    }

    let commit = repository
        .branch_tip(&branch)?
        .with_context(|| format!("branch {branch} does not exist"))?;

    // The work is on this branch and on nothing else. Without this the journey
    // could not tell losing the branch from losing the work, because a commit
    // some other ref also reaches survives the branch going away.
    let reaching = repository.refs_reaching(&commit)?;
    if reaching != vec![format!("refs/heads/{branch}")] {
        bail!(
            "the work commit {commit} is reachable from {reaching:?}, so branch {branch} is not \
             the only thing holding it and this journey would prove nothing"
        );
    }

    Ok(EstablishedWork { branch, commit })
}

/// Wait for the worktree the product said it would remove to actually go, and
/// report what git makes of it afterwards.
///
/// A directory that is gone while git still lists a registration for it is not
/// a finished cleanup: `git worktree add` refuses a branch a registration
/// still claims, so the task would be left unable to have a worktree at all.
async fn await_worktree_removal(repository: &GitFixture, worktree: &Path) -> Result<()> {
    let started = Instant::now();
    loop {
        let on_disk = worktree.exists();
        let registered = repository.registers_worktree(worktree)?;
        if !on_disk && !registered {
            return Ok(());
        }
        if started.elapsed() > CLEANUP_DEADLINE {
            bail!(
                "{} was not cleaned up within {}s: directory present = {on_disk}, git \
                 registration present = {registered}",
                worktree.display(),
                CLEANUP_DEADLINE.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Hold the product to not having destroyed the only copy of the work.
///
/// Removing the worktree directory is what `Done` and delete are for. Removing
/// the last reference to the commits made inside it is a different act, and
/// this is the assertion that keeps the two apart: after the dust settles,
/// something in the repository still has to be able to name the work.
fn assert_work_survives(
    repository: &GitFixture,
    work: &EstablishedWork,
    what_happened: &str,
) -> Result<()> {
    let reaching = repository.refs_reaching(&work.commit)?;
    if reaching.is_empty() {
        bail!(
            "{what_happened} left commit {} — the only copy of the work the agent produced — \
             reachable from no ref at all. Branch {} is gone, the worktree is gone, and nothing \
             in the repository can name that commit again; it survives only as a dangling \
             object until git collects it.",
            work.commit,
            work.branch
        );
    }
    Ok(())
}

/// Prove what moving a finished task to `Done` does to the work it produced.
///
/// `Done` is meant to be the end of a task's need for a worktree, so the
/// worktree going away is the behaviour under test rather than a defect. What
/// the journey will not accept is the same action taking the work with it: the
/// task ran, the executor committed what the agent wrote to the task's branch,
/// and no part of moving a card into the last column says that those commits
/// should stop existing.
///
/// Driven through `reorder_task`, which is exactly what the board sends when a
/// card is dragged into a column — no cleanup helper is called, and the test
/// deletes nothing itself.
#[tokio::test(flavor = "multi_thread")]
async fn finishing_a_task_removes_its_worktree_without_destroying_its_work() {
    let context = TestContext::new("queue_task_done").expect("harness setup");
    let outcome = done_journey(&context).await;
    context.finish(outcome);
}

async fn done_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();

    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    // The run has to produce something, or destroying its worktree would
    // destroy nothing and the journey would pass for the wrong reason.
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);

    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("done").await?;
    let outcome = finish_a_task(session.driver(), &agent, &repository).await;
    context.close_session(session, "done", &outcome).await?;
    let (executed, work) = outcome?;

    // The settled state is on disk and not only in the application's memory.
    assert_persisted(&root, &executed, "done")?;

    // Read once more after the application is gone, so nothing about a running
    // process can be holding the answer up.
    if executed.worktree_path.exists() {
        bail!(
            "the worktree {} outlived the application that was told to remove it",
            executed.worktree_path.display()
        );
    }
    assert_work_survives(&repository, &work, "moving the task to Done")?;

    // The fixture writes a real file, so the task's canonical diff is real
    // and non-empty by the time it reaches `AiReview` -- a review agent must
    // actually run, not just the one coding run. The fixture's fixed
    // response never contains `CHANGES_REQUESTED`, so no fix run follows.
    // (It carries no `VERDICT: APPROVED` either, so the review is recorded
    // as failed rather than approved; this journey does not read it.)
    assert_agent_roles(&agent, "done_journey", 1, 1, 0)?;

    Ok(())
}

async fn finish_a_task(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
) -> Result<(ExecutedTask, EstablishedWork)> {
    let executed = execute_one_task(driver, agent, repository).await?;
    let work = establish_work(driver, repository, &executed).await?;

    // --- The one action under test -----------------------------------------
    //
    // What the board sends when a card is dropped into the Done column and
    // the person confirms closing it without merging.
    ui::invoke(
        driver,
        "reorder_task",
        json!({
            "taskId": executed.id,
            "newStatus": "done",
            "closeWithoutMerge": true,
            "newPosition": 0,
        }),
    )
    .await?;

    // --- What the product says happened ------------------------------------
    let settled = await_status(driver, &executed.project_id, &executed.id, &["done"]).await?;

    await_worktree_removal(repository, &executed.worktree_path).await?;

    // The reference is cleared only once the removal succeeded, so a cleared
    // one is the product's own statement that the directory really went.
    let cleared = await_worktree_path_cleared(driver, &executed).await?;
    if let Some(still) = cleared {
        bail!(
            "the task still records worktree {still} after its removal, so the board and the \
             disk disagree about a directory that is gone"
        );
    }

    // Execution state is not reset on the way to Done: the task is finished,
    // not sent back into the workflow.
    let status = status_of(&settled);
    if status != Some("done") {
        bail!("the task reports {status:?} instead of done");
    }

    // --- What it did to the work -------------------------------------------
    assert_work_survives(repository, &work, "moving the task to Done")?;

    // The product keeps `branch_name` past Done on purpose — its own comment
    // says it is kept for PR creation — so the name it keeps has to still
    // name something.
    let recorded_branch = settled.get("branch_name").and_then(Value::as_str);
    if recorded_branch != Some(work.branch.as_str()) {
        bail!(
            "the finished task records branch {recorded_branch:?}, expected {:?}",
            work.branch
        );
    }
    if !repository.has_branch(&work.branch)? {
        bail!(
            "the finished task still records branch {} for creating a pull request from, but \
             the branch itself was deleted",
            work.branch
        );
    }

    // --- Proof a user would see it -----------------------------------------
    open_board(driver, &executed.project_id).await?;
    assert_card_in_column(driver, DONE_COLUMN, &executed.title).await?;

    Ok((executed, work))
}

/// Wait until the product stops recording a worktree for the task, and return
/// whatever it still records when the wait runs out.
async fn await_worktree_path_cleared(
    driver: &WebDriver,
    executed: &ExecutedTask,
) -> Result<Option<String>> {
    let started = Instant::now();
    loop {
        let listed = ui::invoke(
            driver,
            "list_tasks",
            json!({ "projectId": executed.project_id }),
        )
        .await?;
        let recorded = find_task(&listed, &executed.id)
            .and_then(|task| task.get("worktree_path").cloned())
            .filter(|value| !value.is_null())
            .and_then(|value| value.as_str().map(str::to_string));
        if recorded.is_none() {
            return Ok(None);
        }
        if started.elapsed() > CLEANUP_DEADLINE {
            return Ok(recorded);
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Prove what deleting a task does to the work it produced.
///
/// Deleting is the other destructive path, and it is not the same contract as
/// `Done`: the task record itself disappears, so the retry pass that rescues a
/// failed `Done` cleanup has nothing left to select. What the journey holds
/// the product to is the part that is the same either way — the user asked to
/// be rid of a task, not to have the commits it produced become unreachable.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_task_removes_its_worktree_without_destroying_its_work() {
    let context = TestContext::new("queue_task_delete").expect("harness setup");
    let outcome = delete_journey(&context).await;
    context.finish(outcome);
}

async fn delete_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();

    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);

    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("delete").await?;
    let outcome = delete_a_task(session.driver(), &agent, &repository).await;
    context.close_session(session, "delete", &outcome).await?;
    let (executed, work) = outcome?;

    // Gone from disk as well as from the running application.
    if find_persisted(&root, &executed.id)? {
        bail!(
            "task {} was deleted but a persisted record of it is still in the state root",
            executed.id
        );
    }
    if executed.worktree_path.exists() {
        bail!(
            "the worktree {} outlived the application that was told to remove it",
            executed.worktree_path.display()
        );
    }
    assert_work_survives(&repository, &work, "deleting the task")?;

    Ok(())
}

async fn delete_a_task(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
) -> Result<(ExecutedTask, EstablishedWork)> {
    let executed = execute_one_task(driver, agent, repository).await?;
    let work = establish_work(driver, repository, &executed).await?;

    // --- The one action under test -----------------------------------------
    let deleted = ui::invoke(driver, "delete_task", json!({ "taskId": executed.id })).await?;
    if deleted != Value::Bool(true) {
        bail!("delete_task answered {deleted} rather than reporting the task deleted");
    }

    // --- What the product says happened ------------------------------------
    let listed = ui::invoke(
        driver,
        "list_tasks",
        json!({ "projectId": executed.project_id }),
    )
    .await?;
    if find_task(&listed, &executed.id).is_some() {
        bail!(
            "the product still lists task {} after deleting it",
            executed.id
        );
    }

    await_worktree_removal(repository, &executed.worktree_path).await?;

    // --- What it did to the work -------------------------------------------
    assert_work_survives(repository, &work, "deleting the task")?;

    // --- Proof a user would see it -----------------------------------------
    open_board(driver, &executed.project_id).await?;
    assert_card_absent_from_board(driver, &executed.title).await?;

    // Same reasoning as `done_journey`: the fixture wrote real work before
    // the delete, so the task's real diff must have gone through an actual
    // review agent, not just the coding run, before it ever reached the
    // point where this journey deletes it.
    assert_agent_roles(agent, "delete_journey", 1, 1, 0)?;

    Ok((executed, work))
}

/// Whether any persisted record in the state root still carries this task id.
fn find_persisted(root: &Path, task_id: &str) -> Result<bool> {
    for file in toml_files(root)? {
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(document) = contents.parse::<toml::Value>() else {
            continue;
        };
        if task_record(&document, task_id).is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Assert `title` is on no column of the board.
///
/// Checked across the whole board rather than one column, because a delete
/// that only moved the card would otherwise pass by leaving it somewhere else.
async fn assert_card_absent_from_board(driver: &WebDriver, title: &str) -> Result<()> {
    let board = ui::visible(driver, KANBAN_BOARD).await?;
    let shown = card_titles(&board).await?;
    if shown.iter().any(|found| found == title) {
        bail!("the board still shows a card titled {title:?} after the task was deleted");
    }
    Ok(())
}

/// How long a refused cleanup is watched for a destructive attempt nobody
/// asked for.
///
/// The product used to carry a pass that re-attempted failed cleanups on a
/// timer, running once every ten ticks of the executor's three-second loop —
/// a full cycle of thirty seconds. That pass is gone, and its absence is part
/// of the contract now, so this window has to be long enough that the pass
/// would certainly have acted inside it: one whole cycle plus the tick a
/// refusal would have landed in. Anything shorter could not tell "nothing
/// retried" apart from "the retry has not come round yet".
const NO_RETRY_WINDOW: Duration = Duration::from_secs(35);

/// The status the product reports for a task right now, read back through the
/// frontend's own `list_tasks`.
///
/// A refused cleanup has to leave the status it found rather than the status a
/// journey assumed it would find, so the journeys read it before they ask for
/// anything instead of naming it themselves.
async fn current_status(driver: &WebDriver, executed: &ExecutedTask) -> Result<String> {
    let listed = ui::invoke(
        driver,
        "list_tasks",
        json!({ "projectId": executed.project_id }),
    )
    .await?;
    let task = find_task(&listed, &executed.id)
        .with_context(|| format!("the product no longer lists task {}", executed.id))?;
    let status = status_of(&task)
        .with_context(|| format!("the product lists task {} with no status", executed.id))?;
    Ok(status.to_string())
}

/// Spend the window the deleted retry pass ran on, twice, and prove nothing in
/// the product moved during either half.
///
/// Both journeys that end in a kept worktree hold the product to the same
/// contract: a refused cleanup stays refused until a person asks again, and the
/// obstacle going away by itself is not a person asking. Spelling that out
/// twice meant two copies of the same pair of multi-line diagnostics, where an
/// edit to one silently weakened the other journey's.
///
/// `release_obstacle` is the only thing that genuinely differs -- each journey
/// blocks the removal its own way -- so it is the only thing passed in. The
/// window itself is unchanged: still `NO_RETRY_WINDOW` before the release and
/// `NO_RETRY_WINDOW` after it, with a full `assert_worktree_kept` at each end
/// rather than a cheaper check.
async fn assert_nothing_retried_the_removal(
    driver: &WebDriver,
    repository: &GitFixture,
    executed: &ExecutedTask,
    before: &str,
    work: &EstablishedWork,
    release_obstacle: impl FnOnce() -> Result<()>,
) -> Result<()> {
    // The window the deleted retry pass ran on, spent doing nothing at all.
    // Everything the caller asserted has to still hold afterwards, because a
    // refused cleanup stays refused until a person asks again.
    tokio::time::sleep(NO_RETRY_WINDOW).await;

    assert_worktree_kept(
        driver,
        repository,
        executed,
        before,
        work,
        &format!(
            "{}s later, with nobody having asked for anything in between",
            NO_RETRY_WINDOW.as_secs()
        ),
    )
    .await
    .context(
        "something inside the product re-attempted a destructive cleanup that nobody asked \
         for. No user action happened between the refusal and this read, so whatever changed \
         was a worktree removal running on a timer — the one thing this contract says must \
         not exist.",
    )?;

    // Releasing the obstacle is not an event the product may act on. The
    // removal would succeed now, and that is precisely why this is worth
    // waiting out: nothing in SlashIt watches a kept worktree for its obstacle
    // going away, so the only thing that may change here is nothing.
    release_obstacle()?;
    tokio::time::sleep(NO_RETRY_WINDOW).await;

    assert_worktree_kept(
        driver,
        repository,
        executed,
        before,
        work,
        "after the obstacle was released and still nobody had asked for anything",
    )
    .await
    .context(
        "the worktree went away by itself once it became removable, so some pass is watching \
         refused cleanups and finishing them unprompted. A removal the product was told to \
         keep may only happen when a person asks for it again.",
    )?;

    Ok(())
}

/// Assert that a task whose cleanup the product refused still holds everything
/// the refusal promised to keep, and hand back the worktree path it records.
///
/// `when` names the moment being read, so a failure distinguishes a product
/// that gave the worktree away in the refusal itself from one that gave it
/// away later, with nobody having asked for anything in between.
async fn assert_worktree_kept(
    driver: &WebDriver,
    repository: &GitFixture,
    executed: &ExecutedTask,
    expected_status: &str,
    work: &EstablishedWork,
    when: &str,
) -> Result<String> {
    let listed = ui::invoke(
        driver,
        "list_tasks",
        json!({ "projectId": executed.project_id }),
    )
    .await?;
    let task = find_task(&listed, &executed.id)
        .with_context(|| format!("the task is no longer listed {when}"))?;

    let status = status_of(&task);
    if status != Some(expected_status) {
        bail!(
            "the task reads {status:?} {when}, and a cleanup the product refused has to leave \
             the status it found, which was {expected_status:?}. A task that reached its \
             terminal status anyway tells the user the checkout is gone while it is still on \
             disk."
        );
    }

    let Some(recorded) = task.get("worktree_path").and_then(Value::as_str) else {
        bail!(
            "the task records no worktree at all {when}, although the product was not able to \
             remove the one at {}. That field is the only persisted record of the directory, \
             so clearing it leaves nothing in the product able to find it again.",
            executed.worktree_path.display()
        );
    };
    if resolve(Path::new(recorded)) != resolve(&executed.worktree_path) {
        bail!(
            "the task records the worktree {recorded:?} {when}, which is not the {} whose \
             removal was refused — the record now points somewhere else entirely",
            executed.worktree_path.display()
        );
    }

    match task.get("error_message").and_then(Value::as_str) {
        None => bail!(
            "the task carries no error_message {when}, so the product kept the worktree at \
             {recorded} without recording anywhere a user can read why it is still there"
        ),
        Some(reason) if !reason.contains(recorded) => bail!(
            "the task explains itself with {reason:?} {when}, and that never names {recorded} \
             — the user is told the task could not be finished without being told which \
             directory is holding it back"
        ),
        Some(_) => {}
    }

    if !executed.worktree_path.exists() {
        bail!(
            "{} is gone {when}, so a removal the product declined to make happened anyway",
            executed.worktree_path.display()
        );
    }
    if !repository.has_branch(&work.branch)? {
        bail!(
            "branch {} no longer exists {when}, although the cleanup that owns its worktree \
             never ran",
            work.branch
        );
    }
    assert_work_survives(
        repository,
        work,
        &format!("the cleanup the product refused, read {when},"),
    )?;

    Ok(recorded.to_string())
}

/// Assert the task really is finished and its worktree really is gone, which
/// is the only state an explicit retry is allowed to leave behind.
async fn assert_cleanly_finished(
    driver: &WebDriver,
    repository: &GitFixture,
    executed: &ExecutedTask,
    work: &EstablishedWork,
) -> Result<()> {
    let listed = ui::invoke(
        driver,
        "list_tasks",
        json!({ "projectId": executed.project_id }),
    )
    .await?;
    let task = find_task(&listed, &executed.id)
        .context("the task is no longer listed after the retry it was explicitly asked for")?;

    let status = status_of(&task);
    if status != Some("done") {
        bail!(
            "the retry removed the worktree but left the task reading {status:?}. Removing the \
             checkout and recording the task as finished are one operation, so a card still in \
             its old column with its worktree gone is a state nobody asked for and nothing \
             will correct."
        );
    }
    if let Some(still) = task.get("worktree_path").and_then(Value::as_str) {
        bail!(
            "the task still records worktree {still:?} although the retry removed it, so the \
             board names a checkout that no longer exists"
        );
    }
    if let Some(stale) = task.get("error_message").and_then(Value::as_str) {
        bail!(
            "the finished task still explains itself with {stale:?}, so it goes on telling the \
             user its worktree was kept after the worktree was removed"
        );
    }
    if executed.worktree_path.exists() {
        bail!(
            "the product reported the task finished, but {} is still on disk — the terminal \
             status is only allowed to be committed once the removal has actually happened",
            executed.worktree_path.display()
        );
    }
    if repository.registers_worktree(&executed.worktree_path)? {
        bail!(
            "git still registers a worktree at {} after the retry reported it removed. The \
             directory is gone and the record is not, so this branch can never be given a \
             worktree again.",
            executed.worktree_path.display()
        );
    }
    if !repository.has_branch(&work.branch)? {
        bail!(
            "branch {} was deleted along with the worktree it belonged to, and it is the only \
             thing naming the work the agent committed",
            work.branch
        );
    }
    Ok(())
}

/// Makes a directory unreachable for as long as it is held, and puts it back
/// afterwards however the journey ends.
///
/// Restoring on drop is not tidiness: the harness has to be able to delete the
/// run's state root, and a directory left at mode zero would survive the test
/// and the cleanup after it.
struct Unreachable {
    path: PathBuf,
    restore: u32,
}

impl Unreachable {
    /// Take away every permission on `path`.
    ///
    /// This is the smallest obstacle that makes removal fail without altering
    /// anything git owns: `git worktree remove` cannot read `.git` inside a
    /// directory it may not enter, so it refuses before deleting a single
    /// file, `--force` refuses the same way, and `prune` leaves a record whose
    /// directory is plainly still there. The worktree is exactly as it was, so
    /// the attempt a user makes later has something to succeed at — which is
    /// what a refusal is supposed to leave behind.
    fn hold(path: &Path) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let restore = std::fs::metadata(path)
            .with_context(|| format!("could not read the mode of {}", path.display()))?
            .permissions()
            .mode();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000))
            .with_context(|| format!("could not close off {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            restore,
        })
    }

    fn release(self) -> Result<()> {
        // Dropping does the same thing; this exists so a journey can say when
        // it wants the obstacle gone and fail if it could not be removed.
        let path = self.path.clone();
        let restore = self.restore;
        std::mem::forget(self);
        Self::restore_mode(&path, restore)
    }

    fn restore_mode(path: &Path, mode: u32) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("could not reopen {}", path.display()))
    }
}

impl Drop for Unreachable {
    fn drop(&mut self) {
        let _ = Self::restore_mode(&self.path, self.restore);
    }
}

/// Prove that a `Done` whose worktree cleanup fails is not a `Done` at all,
/// and that nothing but a person asking again ever finishes it.
///
/// Moving a task to `Done` removes its worktree first and commits the terminal
/// status only if that removal succeeded, so a removal git will not make has
/// to leave the task exactly as it was found: the status it had, the worktree
/// it had, the branch it had and every byte in the checkout. The one thing it
/// gains is a reason a user can read.
///
/// The second half is what used to be a retry pass. Nothing in the product
/// re-attempts a destructive cleanup on a timer any more, so this journey
/// spends the window that pass ran on and requires that nothing moved, then
/// releases the obstacle and requires that nothing moved then either — because
/// no part of SlashIt is watching for the obstacle to go away. Only when the
/// journey asks a second time, with the same command the board sends when a
/// card is dragged onto Done, is the worktree allowed to go.
///
/// The obstacle is a permission the journey takes away and gives back. It is
/// not a simulated failure: the product runs its real `git worktree remove`
/// against a directory it genuinely cannot remove, and later against the same
/// directory once it can.
#[tokio::test(flavor = "multi_thread")]
async fn a_done_cleanup_that_fails_keeps_everything_and_only_an_explicit_retry_finishes_it() {
    if unsafe { libc::geteuid() } == 0 {
        // Root ignores the permission bits the obstacle is made of, so the
        // first cleanup would succeed and the journey would prove nothing.
        eprintln!("skipped: running as root, where the obstacle has no effect");
        return;
    }
    let context = TestContext::new("queue_task_done_refused").expect("harness setup");
    let outcome = done_refusal_journey(&context).await;
    context.finish(outcome);
}

async fn done_refusal_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();

    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);

    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("done-refused").await?;
    let outcome = finish_a_refused_cleanup_by_asking_again(
        session.driver(),
        &agent,
        &repository,
    )
    .await;
    context
        .close_session(session, "done-refused", &outcome)
        .await?;
    outcome
}

async fn finish_a_refused_cleanup_by_asking_again(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
) -> Result<()> {
    let executed = execute_one_task(driver, agent, repository).await?;
    let work = establish_work(driver, repository, &executed).await?;
    // Read rather than assumed, so the assertion below is that the refusal
    // left the status the product itself was in.
    let before = current_status(driver, &executed).await?;

    // The obstacle goes up before the task is finished, so the product's very
    // first removal attempt is the one that fails.
    let obstacle = Unreachable::hold(&executed.worktree_path)?;

    // --- The move onto Done has to be refused, not reported ----------------
    //
    // Exactly what the board sends when a card is dragged into the last
    // column and the close without merging is confirmed. Answering success here would tell the user their checkout had
    // been dealt with while it is still sitting on disk.
    let refusal = ui::invoke_expecting_refusal(
        driver,
        "reorder_task",
        json!({
            "taskId": executed.id,
            "newStatus": "done",
            "closeWithoutMerge": true,
            "newPosition": 0,
        }),
    )
    .await?;

    let recorded = assert_worktree_kept(
        driver,
        repository,
        &executed,
        &before,
        &work,
        "as soon as the refusal came back",
    )
    .await?;

    if !refusal.contains(&recorded) {
        bail!(
            "the product refused with {refusal:?}, which never names the worktree at {recorded} \
             it kept. The user is told the card cannot be moved without being told which \
             directory to deal with, and dealing with it is the only way forward."
        );
    }

    assert_nothing_retried_the_removal(driver, repository, &executed, &before, &work, || {
        obstacle.release()
    })
    .await?;

    // The checkout is readable again, and what the agent left in it is still
    // exactly what it left: a refusal keeps the directory, not merely a record
    // of where it used to be.
    let surviving = std::fs::read_to_string(executed.worktree_path.join(WORK_FILE))
        .with_context(|| {
            format!(
                "{WORK_FILE} can no longer be read in the worktree the product kept at {}, so \
                 the refused cleanup reached into a checkout it was supposed to leave whole",
                executed.worktree_path.display()
            )
        })?;
    if surviving != fake_agent::WORK_CONTENT {
        bail!(
            "{WORK_FILE} in the kept worktree holds {surviving:?}, which is not what the agent \
             wrote — a cleanup that was refused still changed the contents of the checkout"
        );
    }

    // --- Only an explicit second attempt is allowed to finish it -----------
    //
    // The same command again, which is the product's stated way out of a kept
    // worktree: the user deals with whatever git objected to and drags the
    // card onto Done once more.
    ui::invoke(
        driver,
        "reorder_task",
        json!({
            "taskId": executed.id,
            "newStatus": "done",
            "closeWithoutMerge": true,
            "newPosition": 0,
        }),
    )
    .await
    .context(
        "asking a second time, with the obstacle gone, was refused as well. Asking again is \
         the only route out of a kept worktree, so a task that cannot take it is stranded with \
         a checkout nothing will ever remove.",
    )?;

    assert_cleanly_finished(driver, repository, &executed, &work).await
}

/// An obstacle git walks into rather than one it refuses at.
///
/// [`Unreachable`] closes off the worktree directory itself, so git cannot
/// read `<worktree>/.git`, declines the removal before touching anything, and
/// leaves the checkout exactly as it was. This one is a level further in: the
/// worktree is perfectly readable, git validates it, accepts the removal and
/// starts deleting, and only then reaches a directory whose entries it is not
/// allowed to unlink. What git does at that point is what these journeys are
/// about, and it is nothing like refusing.
///
/// Mode `0o500` rather than `0o000` on purpose. Git has to be able to see that
/// there is something inside to delete, so that it fails on the unlink and not
/// on the listing; a directory it may not even open is the *other* obstacle,
/// and it produces the other outcome.
///
/// The planted content is committed rather than left untracked, and that is
/// load-bearing. `git worktree remove` checks the checkout is clean before it
/// deletes anything and refuses outright when it is not, so an untracked file
/// would make git decline at validation — the *other* obstacle's outcome
/// again, reached by a different route. Committing it is what leaves the
/// permission as the only thing git can trip over.
struct Undeletable {
    directory: PathBuf,
    restore: u32,
}

impl Undeletable {
    /// Plant the obstacle inside `worktree` and hold it.
    fn plant(worktree: &Path) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let directory = worktree.join("blocked");
        std::fs::create_dir(&directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        std::fs::write(directory.join("held.txt"), b"held by the obstacle\n")
            .context("could not put anything inside the obstacle")?;
        // Committed before the permission goes on, so the checkout git is
        // asked to remove is clean and the only thing standing in its way is
        // the directory it may not write to. See the note on the type.
        git(worktree, &["add", "blocked"])?;
        git(
            worktree,
            &["commit", "--quiet", "-m", "content the obstacle holds"],
        )?;
        let restore = std::fs::metadata(&directory)
            .with_context(|| format!("could not read the mode of {}", directory.display()))?
            .permissions()
            .mode();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500))
            .with_context(|| format!("could not close off {}", directory.display()))?;
        Ok(Self { directory, restore })
    }

    fn release(self) -> Result<()> {
        let directory = self.directory.clone();
        let restore = self.restore;
        std::mem::forget(self);
        Self::restore_mode(&directory, restore)
    }

    fn restore_mode(directory: &Path, mode: u32) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        if !directory.exists() {
            // The removal this obstacle was blocking finally happened, so
            // there is nothing left to reopen.
            return Ok(());
        }
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("could not reopen {}", directory.display()))
    }
}

impl Drop for Undeletable {
    fn drop(&mut self) {
        let _ = Self::restore_mode(&self.directory, self.restore);
    }
}

/// Prove that a cleanup git gives up on halfway through keeps everything too,
/// and that it is still a cleanup a user can finish by asking again.
///
/// The contract
/// [`a_done_cleanup_that_fails_keeps_everything_and_only_an_explicit_retry_finishes_it`]
/// establishes is only worth having if it holds for the removals that actually
/// fail. That journey uses an obstacle git refuses at, which leaves the
/// checkout whole and the record intact, so a later attempt has something to
/// succeed at. This one uses an obstacle git discovers only after it has
/// committed to the removal — and a removal git abandons partway is a
/// different situation entirely, because git tears down the worktree's
/// registration on its way out whether or not the checkout it was deleting is
/// still there.
///
/// That registration is what the journey is really about. A path git no longer
/// registers is one no `git worktree` command will act on again, so a product
/// that let the record go would leave the task holding a directory that can
/// never be removed by asking, however thoroughly the user deals with what git
/// objected to. The product saves the record before it starts and puts it back
/// when the removal did not finish, which is what makes the later attempt an
/// ordinary removal rather than an impossible one — and the later attempt
/// succeeding is how this journey proves it.
///
/// What changed is only who triggers that later attempt. Nothing retries on a
/// timer any more, so the journey waits out the window in which the deleted
/// pass would have acted, requires that nothing moved, and then asks itself.
#[tokio::test(flavor = "multi_thread")]
async fn a_cleanup_git_abandons_partway_keeps_everything_and_only_an_explicit_retry_finishes_it() {
    if unsafe { libc::geteuid() } == 0 {
        // Root ignores the permission bits the obstacle is made of, so the
        // first cleanup would succeed and the journey would prove nothing.
        eprintln!("skipped: running as root, where the obstacle has no effect");
        return;
    }
    let context = TestContext::new("queue_task_partial_cleanup").expect("harness setup");
    let outcome = partial_cleanup_journey(&context).await;
    context.finish(outcome);
}

async fn partial_cleanup_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();

    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);

    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("partial-cleanup").await?;
    let outcome =
        finish_a_cleanup_git_abandoned_by_asking_again(session.driver(), &agent, &repository).await;
    context
        .close_session(session, "partial-cleanup", &outcome)
        .await?;
    outcome
}

async fn finish_a_cleanup_git_abandoned_by_asking_again(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
) -> Result<()> {
    let executed = execute_one_task(driver, agent, repository).await?;
    let work = establish_work(driver, repository, &executed).await?;
    let before = current_status(driver, &executed).await?;

    // Planted before the task is finished, so the product's very first removal
    // is the one git abandons.
    let obstacle = Undeletable::plant(&executed.worktree_path)?;

    // --- The move onto Done has to be refused, not reported ----------------
    let refusal = ui::invoke_expecting_refusal(
        driver,
        "reorder_task",
        json!({
            "taskId": executed.id,
            "newStatus": "done",
            "closeWithoutMerge": true,
            "newPosition": 0,
        }),
    )
    .await?;

    let recorded = assert_worktree_kept(
        driver,
        repository,
        &executed,
        &before,
        &work,
        "as soon as the refusal came back",
    )
    .await?;

    if !refusal.contains(&recorded) {
        bail!(
            "the product refused with {refusal:?}, which never names the worktree at {recorded} \
             it kept. The user is told the card cannot be moved without being told which \
             directory to deal with, and dealing with it is the only way forward."
        );
    }

    // The record git tore down on its way out of the removal has to be back.
    // Without it every later attempt answers "is not a working tree", so the
    // task would hold a checkout that no amount of asking could ever remove.
    if !repository.registers_worktree(&executed.worktree_path)? {
        bail!(
            "git no longer registers a worktree at {} after abandoning its removal, and the \
             product kept the task pointing at it anyway. Nothing can remove a path git does \
             not register, so this task is holding a directory that will never come out.",
            executed.worktree_path.display()
        );
    }

    assert_nothing_retried_the_removal(driver, repository, &executed, &before, &work, || {
        obstacle.release()
    })
    .await?;

    // What a user has to put right before asking again. Git abandons the
    // removal partway through the checkout, so some of the files it had
    // already deleted are tracked ones, and a checkout missing them is dirty —
    // which the safe removal refuses, exactly as it refuses any other dirt.
    // Restoring them is the user's own `git checkout`, run here through the
    // fixture's git rather than through the product.
    git(&executed.worktree_path, &["checkout", "--", "."]).context(
        "the worktree the product kept could not be put back in order with git, which means \
         the checkout it left behind is not one a user could deal with either",
    )?;

    // --- Only an explicit second attempt is allowed to finish it -----------
    ui::invoke(
        driver,
        "reorder_task",
        json!({
            "taskId": executed.id,
            "newStatus": "done",
            "closeWithoutMerge": true,
            "newPosition": 0,
        }),
    )
    .await
    .context(
        "asking a second time, with the obstacle gone and the checkout back in order, was \
         refused as well. This is the attempt the restored registration exists to make \
         possible, so a task that cannot take it is stranded with a checkout nothing will \
         ever remove.",
    )?;

    assert_cleanly_finished(driver, repository, &executed, &work).await?;
    assert_work_survives(
        repository,
        &work,
        "a cleanup git abandoned partway and an explicit retry then finished",
    )?;

    Ok(())
}

/// A second worktree in the same repository, belonging to nobody in this
/// journey.
///
/// It exists to be left alone. Cleaning up one task is an operation on that
/// task's worktree, and a journey can only show that by having something else
/// registered at the same time that must come out the other side untouched.
struct Bystander {
    path: PathBuf,
    branch: String,
    file: PathBuf,
}

impl Bystander {
    /// Register a worktree of `repository` that this journey never cleans up.
    fn register(repository: &GitFixture) -> Result<Self> {
        let path = repository.state_root().join("bystander-worktree");
        let branch = "bystander-work".to_string();
        git(
            &repository.path_buf(),
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                &path.to_string_lossy(),
            ],
        )?;
        let file = path.join("bystander.txt");
        std::fs::write(&file, b"work that belongs to nobody in this journey\n")
            .with_context(|| format!("could not write {}", file.display()))?;
        git(&path, &["add", "bystander.txt"])?;
        git(&path, &["commit", "--quiet", "-m", "bystander work"])?;
        Ok(Self { path, branch, file })
    }
}

/// Prove that cleaning up one task's worktree does not reach into another's.
///
/// Cleaning up a task is an operation on that task's worktree. It used to end
/// in a repository-wide `git worktree prune`, which takes no path and decides
/// what is stale by whether it can read each registered worktree's `.git`. A
/// worktree it may not look inside reads exactly like one that is gone, so a
/// second worktree that is entirely present — merely unreadable at the moment,
/// as one on an unmounted drive or behind a permission would be — lost the
/// registration it needs while its files sat untouched on disk and its branch
/// stayed exactly where it was. Nothing about that worktree took part in the
/// cleanup that destroyed it.
///
/// The state that reached that prune is the one this journey builds: git
/// abandons a removal, as in
/// [`a_cleanup_git_abandons_partway_keeps_everything_and_only_an_explicit_retry_finishes_it`],
/// the leftover directory is then cleared by hand, and the task is finished
/// afterwards with nothing of its own left to remove. What the product does at
/// that point must still be about this task alone.
///
/// The assertions are about the bystander keeping its registration, its files
/// and its branch, not about which step might take them, so they go on holding
/// whatever the cleanup is made of.
#[tokio::test(flavor = "multi_thread")]
async fn cleaning_up_one_task_leaves_another_worktree_registered() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: running as root, where the obstacle has no effect");
        return;
    }
    let context = TestContext::new("queue_task_bystander").expect("harness setup");
    let outcome = bystander_journey(&context).await;
    context.finish(outcome);
}

async fn bystander_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();

    let agent = FakeAgent::install(&root)?;
    context.set_child_env("PATH", agent.path_value());
    context.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    context.set_child_env(fake_agent::WRITE_FILE_VAR, WORK_FILE);

    start_on_the_legacy_auto_placement(&context.state().config_file())?;
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("bystander").await?;
    let outcome = clean_up_beside_a_bystander(session.driver(), &agent, &repository).await;
    context.close_session(session, "bystander", &outcome).await?;
    outcome
}

async fn clean_up_beside_a_bystander(
    driver: &WebDriver,
    agent: &FakeAgent,
    repository: &GitFixture,
) -> Result<()> {
    let executed = execute_one_task(driver, agent, repository).await?;
    let work = establish_work(driver, repository, &executed).await?;
    let before = current_status(driver, &executed).await?;

    let bystander = Bystander::register(repository)?;
    // Unreadable, not absent. Everything it holds is still on disk, and the
    // only thing that changes is that git can no longer look inside it — which
    // is indistinguishable, to a repository-wide prune, from having been
    // deleted.
    let unreadable = Unreachable::hold(&bystander.path)?;

    let obstacle = Undeletable::plant(&executed.worktree_path)?;

    // The first move onto Done is refused: git abandons the removal when it
    // reaches the obstacle, and the product keeps the task, its status and its
    // worktree rather than reporting a cleanup that did not happen.
    ui::invoke_expecting_refusal(
        driver,
        "reorder_task",
        json!({
            "taskId": executed.id,
            "newStatus": "done",
            "closeWithoutMerge": true,
            "newPosition": 0,
        }),
    )
    .await?;
    assert_worktree_kept(
        driver,
        repository,
        &executed,
        &before,
        &work,
        "as soon as the refusal came back",
    )
    .await?;

    // The leftover directory is cleared the way a user clearing it would: the
    // obstacle goes, then the directory goes. What the product does next is
    // entirely about a task whose worktree is no longer there.
    obstacle.release()?;
    if executed.worktree_path.exists() {
        std::fs::remove_dir_all(&executed.worktree_path).with_context(|| {
            format!(
                "could not clear the leftover at {}",
                executed.worktree_path.display()
            )
        })?;
    }

    // And the task is finished by asking again, which is the only thing that
    // finishes a refused cleanup now. Nothing has run on its own in between.
    ui::invoke(
        driver,
        "reorder_task",
        json!({
            "taskId": executed.id,
            "newStatus": "done",
            "closeWithoutMerge": true,
            "newPosition": 0,
        }),
    )
    .await
    .context(
        "moving the card onto Done again, with the task's worktree already cleared away, was \
         refused — a task with nothing left to remove has to be able to finish",
    )?;

    let listed = ui::invoke(
        driver,
        "list_tasks",
        json!({ "projectId": executed.project_id }),
    )
    .await?;
    let task = find_task(&listed, &executed.id)
        .context("the task is no longer listed after the cleanup it was asked for a second time")?;
    if let Some(still) = task.get("worktree_path").and_then(Value::as_str) {
        bail!(
            "the task still records worktree {still:?} after its directory was cleared away and \
             it was explicitly finished, so the board goes on naming a checkout that is gone"
        );
    }
    let status = status_of(&task);
    if status != Some("done") {
        bail!(
            "the task reads {status:?} after a cleanup that had nothing left to remove, so \
             finishing it succeeded without the task ever becoming finished"
        );
    }

    // --- What the bystander must still have --------------------------------
    if !repository.registers_worktree(&bystander.path)? {
        bail!(
            "cleaning up {} destroyed the registration of {}, which took no part in it — the \
             directory is still there, so nothing can remove it and nothing can check its \
             branch out again",
            executed.worktree_path.display(),
            bystander.path.display()
        );
    }
    unreadable.release()?;
    if !bystander.file.is_file() {
        bail!(
            "{} lost the work it was holding while another task was cleaned up",
            bystander.path.display()
        );
    }
    if !repository.has_branch(&bystander.branch)? {
        bail!(
            "branch {} was deleted by a cleanup that had nothing to do with it",
            bystander.branch
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Workspace membership: attaching a Project to a Workspace and detaching it
// again, driven through the Workspaces page.
// ---------------------------------------------------------------------------

/// How long the Workspaces page gets to reflect a membership change or show
/// its toast. Both are one IPC round trip and one re-render away.
const MEMBERSHIP_DEADLINE: Duration = Duration::from_secs(20);
const NAV_WORKSPACES: &str = "[data-testid=\"nav-workspaces\"]";

/// Prove that a Project can be attached to a Workspace from the Workspaces
/// page, that the membership survives a full application restart, and that
/// detaching returns the Project to the set that can be attached again.
///
/// The prerequisites -- one Workspace and two standalone Projects -- are
/// created through the IPC bridge, as in the queue journeys; everything the
/// journey claims about membership happens through the rendered page: the
/// eligible-project dropdown, the Attach and Detach buttons, the member list
/// and the toasts. Two Projects rather than one on purpose: with a single
/// Project the eligible set flips between empty and non-empty on every
/// change, which re-renders the whole attach control and would hide a
/// dropdown that never refreshes its options.
///
/// The failure toast is exercised through the one refusal a user can really
/// reach: the page still offers a Project that another client attached in
/// the meantime. The backend's reason has to reach the user, and the page has
/// to catch up with the membership it was refused over.
#[tokio::test(flavor = "multi_thread")]
async fn a_project_attached_to_a_workspace_stays_a_member_across_restart_until_detached() {
    let context = TestContext::new("workspace_membership").expect("harness setup");
    let outcome = membership_journey(&context).await;
    context.finish(outcome);
}

struct MembershipFixture {
    workspace_id: String,
    alpha: (String, String),
    beta: (String, String),
}

async fn membership_journey(context: &TestContext) -> Result<()> {
    // Process A: attach through the page.
    let session = context.start_session("attach").await?;
    let attached = attach_through_the_page(session.driver(), context.state().path()).await;
    context.close_session(session, "attach", &attached).await?;
    let fixture = attached?;

    // The membership has to be what the application wrote, not something the
    // first process only held in memory.
    assert_persisted_scope(
        &context.state().config_file(),
        &fixture.alpha.0,
        Some(&fixture.workspace_id),
    )?;
    assert_persisted_scope(&context.state().config_file(), &fixture.beta.0, None)?;

    // Process B: a brand new application process against the same state.
    let session = context.start_session("detach").await?;
    let detached = detach_after_restart(session.driver(), &fixture).await;
    context.close_session(session, "detach", &detached).await?;
    detached?;

    // Detach is persisted; Beta was attached by the "other client" and a
    // refused UI attempt must not have changed that.
    assert_persisted_scope(&context.state().config_file(), &fixture.alpha.0, None)?;
    assert_persisted_scope(
        &context.state().config_file(),
        &fixture.beta.0,
        Some(&fixture.workspace_id),
    )?;
    Ok(())
}

async fn attach_through_the_page(driver: &WebDriver, root: &Path) -> Result<MembershipFixture> {
    ui::assert_frontend_is_real(driver).await?;

    let workspace_root = root.join("workspace-root");
    std::fs::create_dir_all(&workspace_root)
        .with_context(|| format!("could not create {}", workspace_root.display()))?;
    let workspace_id = created_id(
        ui::invoke(
            driver,
            "create_workspace",
            json!({ "name": "Acceptance Workspace", "rootPath": workspace_root }),
        )
        .await?,
        "create_workspace",
    )?;

    let mut projects = Vec::new();
    for name in ["Acceptance Alpha", "Acceptance Beta"] {
        let id = created_id(
            ui::invoke(
                driver,
                "create_project",
                json!({ "name": name, "repositoryId": Value::Null, "agentType": "claude_code" }),
            )
            .await?,
            "create_project",
        )?;
        projects.push((id, name.to_string()));
    }
    let beta = projects.pop().context("two projects were created")?;
    let alpha = projects.pop().context("two projects were created")?;

    // Reloaded for the same reason `open_board` reloads: the prerequisites
    // were created behind the frontend's back.
    driver
        .refresh()
        .await
        .context("could not reload the application window")?;
    let item = open_workspace(driver, &workspace_id).await?;

    // Before: nothing is a member, and both projects are eligible.
    assert_members(&item, &[]).await?;
    assert_eligible(&item, &[&alpha.1, &beta.1]).await?;

    // Attach Alpha.
    choose_and_attach(&item, &alpha.1).await?;
    await_toast(
        driver,
        "success",
        &format!("Attached {} to this workspace", alpha.1),
    )
    .await?;
    await_members(&item, &[&alpha.0]).await?;
    assert_eligible(&item, &[&beta.1]).await?;

    // The backend agrees with what the page shows.
    assert_backend_scope(driver, &alpha.0, Some(&workspace_id)).await?;
    assert_backend_scope(driver, &beta.0, None).await?;

    Ok(MembershipFixture {
        workspace_id,
        alpha,
        beta,
    })
}

async fn detach_after_restart(driver: &WebDriver, fixture: &MembershipFixture) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let (alpha_id, alpha_name) = (&fixture.alpha.0, &fixture.alpha.1);
    let (beta_id, beta_name) = (&fixture.beta.0, &fixture.beta.1);

    // After the restart: a fresh frontend, fed by a fresh backend that read
    // the membership off disk.
    let item = open_workspace(driver, &fixture.workspace_id).await?;
    await_members(&item, &[alpha_id]).await?;
    assert_eligible(&item, &[beta_name]).await?;

    // Detach Alpha: it leaves the member list and becomes eligible again.
    item.find(By::Css(format!(
        "[data-testid=\"workspace-member-{alpha_id}\"] [data-testid=\"workspace-detach\"]"
    )))
    .await
    .context("the member row has no Detach button")?
    .click()
    .await
    .context("could not click Detach")?;
    await_toast(
        driver,
        "success",
        &format!("Detached {alpha_name} from this workspace"),
    )
    .await?;
    await_members(&item, &[]).await?;
    assert_eligible(&item, &[alpha_name, beta_name]).await?;
    assert_backend_scope(driver, alpha_id, None).await?;

    // Another client attaches Beta while this page still offers it. The
    // attempt from the page must be refused, and the refusal must reach the
    // user in the backend's own words.
    ui::invoke(
        driver,
        "attach_project_to_workspace",
        json!({ "projectId": beta_id, "workspaceId": fixture.workspace_id }),
    )
    .await?;
    choose_and_attach(&item, beta_name).await?;
    await_toast(
        driver,
        "error",
        "Failed to attach project: Project already belongs to a workspace",
    )
    .await?;
    assert_backend_scope(driver, beta_id, Some(&fixture.workspace_id)).await?;

    // And the page stops contradicting the backend: it resyncs, showing Beta
    // as the member it now is and no longer offering it.
    await_members(&item, &[beta_id]).await?;
    assert_eligible(&item, &[alpha_name]).await?;

    Ok(())
}

/// Navigate to the Workspaces page and expand one workspace.
async fn open_workspace(driver: &WebDriver, workspace_id: &str) -> Result<WebElement> {
    ui::assert_frontend_is_real(driver).await?;
    ui::visible(driver, NAV_WORKSPACES)
        .await?
        .click()
        .await
        .context("could not open the Workspaces page")?;
    let item_selector = format!("[data-testid=\"workspace-{workspace_id}\"]");
    let item = ui::visible(driver, &item_selector)
        .await
        .context("the Workspaces page does not list the workspace")?;
    item.find(By::Css("[data-testid=\"workspace-toggle\"]"))
        .await
        .context("the workspace row has no toggle")?
        .click()
        .await
        .context("could not expand the workspace")?;
    ui::visible(
        driver,
        &format!("{item_selector} [data-testid=\"workspace-members\"]"),
    )
    .await
    .context("expanding the workspace did not show its member list")?;
    Ok(item)
}

/// The project ids the workspace currently renders as members.
async fn member_ids(item: &WebElement) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for row in item
        .find_all(By::Css("[data-testid^=\"workspace-member-\"]"))
        .await
        .context("could not read the member list")?
    {
        if let Some(testid) = row.attr("data-testid").await? {
            if let Some(id) = testid.strip_prefix("workspace-member-") {
                ids.push(id.to_string());
            }
        }
    }
    ids.sort();
    Ok(ids)
}

async fn assert_members(item: &WebElement, expected: &[&str]) -> Result<()> {
    let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    expected.sort();
    let shown = member_ids(item).await?;
    if shown != expected {
        bail!("the workspace shows members {shown:?}, expected {expected:?}");
    }
    Ok(())
}

async fn await_members(item: &WebElement, expected: &[&str]) -> Result<()> {
    let started = Instant::now();
    loop {
        match assert_members(item, expected).await {
            Ok(()) => return Ok(()),
            Err(error) if started.elapsed() > MEMBERSHIP_DEADLINE => {
                return Err(error.context(format!(
                    "the member list did not settle within {}s",
                    MEMBERSHIP_DEADLINE.as_secs()
                )))
            }
            Err(_) => tokio::time::sleep(POLL).await,
        }
    }
}

/// The trigger of the eligible-project dropdown.
async fn attach_trigger(item: &WebElement) -> Result<WebElement> {
    item.find(By::Css(
        "[data-testid=\"workspace-attach\"] button[aria-haspopup=\"listbox\"]",
    ))
    .await
    .context("the workspace offers no project dropdown to attach from")
}

/// Open the dropdown and read the project names it offers.
///
/// The list fades in, and until it has, WebDriver reports its options with no
/// rendered text and refuses to click them. So this waits for every option to
/// be readable -- the moment a user could see and pick it -- rather than
/// reading the list the instant it is inserted.
async fn open_eligible(item: &WebElement) -> Result<Vec<(String, WebElement)>> {
    let trigger = attach_trigger(item).await?;
    if trigger.attr("aria-expanded").await?.as_deref() != Some("true") {
        trigger
            .click()
            .await
            .context("could not open the project dropdown")?;
    }
    let started = Instant::now();
    loop {
        let mut offered = Vec::new();
        for option in item
            .find_all(By::Css(
                "[data-testid=\"workspace-attach\"] [role=\"option\"]",
            ))
            .await
            .context("could not read the dropdown's options")?
        {
            offered.push((option.text().await?.trim().to_string(), option));
        }
        if !offered.is_empty() && offered.iter().all(|(name, _)| !name.is_empty()) {
            return Ok(offered);
        }
        if started.elapsed() > MEMBERSHIP_DEADLINE {
            bail!(
                "the project dropdown opened but never showed readable options within {}s \
                 ({} option elements)",
                MEMBERSHIP_DEADLINE.as_secs(),
                offered.len()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Assert exactly these project names are offered for attachment.
///
/// Read from the rendered dropdown, never from the backend: the claim is what
/// the user is offered. The dropdown is closed again afterwards.
async fn assert_eligible(item: &WebElement, expected: &[&str]) -> Result<()> {
    let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    expected.sort();
    let started = Instant::now();
    loop {
        let mut offered: Vec<String> = open_eligible(item)
            .await?
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        offered.sort();
        attach_trigger(item)
            .await?
            .click()
            .await
            .context("could not close the project dropdown")?;
        if offered == expected {
            return Ok(());
        }
        if started.elapsed() > MEMBERSHIP_DEADLINE {
            bail!(
                "the attach dropdown offers {offered:?}, expected {expected:?} (still after {}s)",
                MEMBERSHIP_DEADLINE.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn choose_and_attach(item: &WebElement, project_name: &str) -> Result<()> {
    let offered = open_eligible(item).await?;
    let names: Vec<&str> = offered.iter().map(|(name, _)| name.as_str()).collect();
    let (_, option) = offered
        .iter()
        .find(|(name, _)| name == project_name)
        .with_context(|| {
            format!("the dropdown does not offer {project_name:?}; it offers {names:?}")
        })?;
    option
        .click()
        .await
        .context("could not choose the project")?;
    item.find(By::Css("[data-testid=\"workspace-attach-submit\"]"))
        .await
        .context("the workspace has no Attach button")?
        .click()
        .await
        .context("could not click Attach")?;
    Ok(())
}

/// Wait for a toast of this variant whose message contains `expected`, then
/// clear every toast off the screen.
///
/// Clearing is part of the wait, not tidiness: toasts are fixed to the bottom
/// right, up to 28rem wide, and a success toast lingers for four seconds. In a
/// narrow window that stack covers the attach controls, and the journey's
/// next click would land on a toast instead of the button under it.
async fn await_toast(driver: &WebDriver, variant: &str, expected: &str) -> Result<()> {
    let selector = format!("[data-testid=\"toast\"][data-variant=\"{variant}\"]");
    let started = Instant::now();
    loop {
        let mut shown = Vec::new();
        for toast in driver.find_all(By::Css(&selector)).await? {
            shown.push(toast.text().await.unwrap_or_default());
        }
        if shown.iter().any(|text| text.contains(expected)) {
            return dismiss_toasts(driver).await;
        }
        if started.elapsed() > MEMBERSHIP_DEADLINE {
            let all: Vec<String> = {
                let mut all = Vec::new();
                for toast in driver.find_all(By::Css("[data-testid=\"toast\"]")).await? {
                    all.push(format!(
                        "{}: {}",
                        toast.attr("data-variant").await?.unwrap_or_default(),
                        toast.text().await.unwrap_or_default()
                    ));
                }
                all
            };
            bail!(
                "no {variant} toast saying {expected:?} within {}s; toasts on screen: {all:?}",
                MEMBERSHIP_DEADLINE.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Dismiss every toast through its own close button and wait until none is
/// left in the document.
async fn dismiss_toasts(driver: &WebDriver) -> Result<()> {
    let started = Instant::now();
    loop {
        let toasts = driver.find_all(By::Css("[data-testid=\"toast\"]")).await?;
        if toasts.is_empty() {
            return Ok(());
        }
        for toast in toasts {
            // A toast can expire on its own between the query and the click;
            // that is the outcome this loop is waiting for, not a failure.
            if let Ok(close) = toast.find(By::Css("[data-testid=\"toast-dismiss\"]")).await {
                let _ = close.click().await;
            }
        }
        if started.elapsed() > MEMBERSHIP_DEADLINE {
            bail!(
                "toasts were still on screen after {}s of dismissing them",
                MEMBERSHIP_DEADLINE.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Prove that a Project whose Workspace the registry lost stays visible and
/// recoverable from the Workspaces page, across restarts.
///
/// The loss is made the way it happens in the field: the application is
/// stopped and `workspaces.toml` no longer holds the Workspace -- here the
/// entry is removed by hand, as a quarantined corrupt registry would lose it.
/// The next application process must neither drop the Project from view nor
/// quietly repair its membership: the page lists it as unresolved with the
/// missing Workspace's id, and only the user's Detach makes it standalone.
/// A third process shows the detachment held and the Project attachable to a
/// Workspace that still exists.
#[tokio::test(flavor = "multi_thread")]
async fn a_project_whose_workspace_is_lost_is_shown_unresolved_and_can_be_detached() {
    let context = TestContext::new("unresolved_membership").expect("harness setup");
    let outcome = unresolved_membership_journey(&context).await;
    context.finish(outcome);
}

struct UnresolvedFixture {
    lost_workspace: String,
    kept_workspace: String,
    alpha: (String, String),
    beta: (String, String),
}

async fn unresolved_membership_journey(context: &TestContext) -> Result<()> {
    // Process A: two Workspaces, Alpha a member of the one about to be lost.
    let session = context.start_session("orphan").await?;
    let prepared = prepare_lost_membership(session.driver(), context.state().path()).await;
    context.close_session(session, "orphan", &prepared).await?;
    let fixture = prepared?;

    // While the application is down, the registry loses the Workspace.
    let registry = context.state().workspaces_file();
    remove_registered_workspace(&registry, &fixture.lost_workspace)?;
    assert_persisted_scope(
        &context.state().config_file(),
        &fixture.alpha.0,
        Some(&fixture.lost_workspace),
    )?;

    // Process B: the dangling membership is shown and detached by the user.
    let session = context.start_session("recover").await?;
    let recovered = detach_unresolved(session.driver(), &context.state().config_file(), &fixture).await;
    context.close_session(session, "recover", &recovered).await?;
    recovered?;

    assert_persisted_scope(&context.state().config_file(), &fixture.alpha.0, None)?;
    if registry_holds(&registry, &fixture.lost_workspace)? {
        bail!("detaching recreated the lost workspace {}", fixture.lost_workspace);
    }

    // Process C: Alpha is standalone and can join the Workspace that exists.
    let session = context.start_session("reattach").await?;
    let reattached = reattach_after_recovery(session.driver(), &fixture).await;
    context.close_session(session, "reattach", &reattached).await?;
    reattached?;

    assert_persisted_scope(
        &context.state().config_file(),
        &fixture.alpha.0,
        Some(&fixture.kept_workspace),
    )?;
    Ok(())
}

async fn prepare_lost_membership(driver: &WebDriver, root: &Path) -> Result<UnresolvedFixture> {
    ui::assert_frontend_is_real(driver).await?;

    let mut workspaces = Vec::new();
    for (name, dir) in [("Lost Workspace", "lost-root"), ("Kept Workspace", "kept-root")] {
        let workspace_root = root.join(dir);
        std::fs::create_dir_all(&workspace_root)
            .with_context(|| format!("could not create {}", workspace_root.display()))?;
        workspaces.push(created_id(
            ui::invoke(
                driver,
                "create_workspace",
                json!({ "name": name, "rootPath": workspace_root }),
            )
            .await?,
            "create_workspace",
        )?);
    }
    let kept_workspace = workspaces.pop().context("two workspaces were created")?;
    let lost_workspace = workspaces.pop().context("two workspaces were created")?;

    let mut projects = Vec::new();
    for name in ["Acceptance Alpha", "Acceptance Beta"] {
        let id = created_id(
            ui::invoke(
                driver,
                "create_project",
                json!({ "name": name, "repositoryId": Value::Null, "agentType": "claude_code" }),
            )
            .await?,
            "create_project",
        )?;
        projects.push((id, name.to_string()));
    }
    let beta = projects.pop().context("two projects were created")?;
    let alpha = projects.pop().context("two projects were created")?;
    ui::invoke(
        driver,
        "attach_project_to_workspace",
        json!({ "projectId": alpha.0, "workspaceId": lost_workspace }),
    )
    .await?;

    // Before the loss the membership resolves, so nothing is unresolved.
    driver
        .refresh()
        .await
        .context("could not reload the application window")?;
    let item = open_workspace(driver, &lost_workspace).await?;
    await_members(&item, &[&alpha.0]).await?;
    ui::assert_absent(driver, UNRESOLVED_SECTION).await?;

    Ok(UnresolvedFixture {
        lost_workspace,
        kept_workspace,
        alpha,
        beta,
    })
}

async fn detach_unresolved(
    driver: &WebDriver,
    config_file: &Path,
    fixture: &UnresolvedFixture,
) -> Result<()> {
    let (alpha_id, alpha_name) = (&fixture.alpha.0, &fixture.alpha.1);
    let beta_name = &fixture.beta.1;

    // The surviving Workspace neither lists Alpha nor offers it.
    let kept = open_workspace(driver, &fixture.kept_workspace).await?;
    await_members(&kept, &[]).await?;
    assert_eligible(&kept, &[beta_name]).await?;
    ui::assert_absent(
        driver,
        &format!("[data-testid=\"workspace-{}\"]", fixture.lost_workspace),
    )
    .await
    .context("the lost workspace is still listed as if it existed")?;

    // Alpha is listed as unresolved, naming the Workspace it still records.
    let row = ui::visible(
        driver,
        &format!("{UNRESOLVED_SECTION} [data-testid=\"unresolved-member-{alpha_id}\"]"),
    )
    .await
    .context("the project whose workspace was lost is not shown")?;
    let shown_name = row
        .find(By::Css("[data-testid=\"unresolved-project-name\"]"))
        .await
        .context("the unresolved row does not name the project")?
        .text()
        .await?;
    if shown_name.trim() != alpha_name {
        bail!("the unresolved row names project {shown_name:?}, expected {alpha_name:?}");
    }
    let missing = row
        .find(By::Css("[data-testid=\"unresolved-workspace-id\"]"))
        .await
        .context("the unresolved row does not name the missing workspace")?
        .text()
        .await?;
    if missing.trim() != fixture.lost_workspace {
        bail!(
            "the unresolved row names workspace {missing:?}, expected {}",
            fixture.lost_workspace
        );
    }

    // Showing it changed nothing: startup did not rewrite the membership.
    assert_backend_scope(driver, alpha_id, Some(&fixture.lost_workspace)).await?;
    assert_persisted_scope(config_file, alpha_id, Some(&fixture.lost_workspace))?;

    row.find(By::Css("[data-testid=\"unresolved-detach\"]"))
        .await
        .context("the unresolved row has no Detach button")?
        .click()
        .await
        .context("could not click Detach")?;
    await_toast(
        driver,
        "success",
        &format!("Detached {alpha_name}; it is now standalone"),
    )
    .await?;
    await_absent(driver, UNRESOLVED_SECTION).await?;
    assert_backend_scope(driver, alpha_id, None).await?;
    assert_eligible(&kept, &[alpha_name, beta_name]).await?;
    Ok(())
}

async fn reattach_after_recovery(driver: &WebDriver, fixture: &UnresolvedFixture) -> Result<()> {
    let (alpha_id, alpha_name) = (&fixture.alpha.0, &fixture.alpha.1);
    let kept = open_workspace(driver, &fixture.kept_workspace).await?;
    await_members(&kept, &[]).await?;
    assert_eligible(&kept, &[alpha_name, &fixture.beta.1]).await?;
    ui::assert_absent(driver, UNRESOLVED_SECTION).await?;

    choose_and_attach(&kept, alpha_name).await?;
    await_toast(
        driver,
        "success",
        &format!("Attached {alpha_name} to this workspace"),
    )
    .await?;
    await_members(&kept, &[alpha_id]).await?;
    assert_backend_scope(driver, alpha_id, Some(&fixture.kept_workspace)).await?;
    Ok(())
}

const UNRESOLVED_SECTION: &str = "[data-testid=\"unresolved-memberships\"]";

/// Drop one Workspace from the registry file, keeping every other entry.
fn remove_registered_workspace(registry: &Path, workspace_id: &str) -> Result<()> {
    let raw = std::fs::read_to_string(registry)
        .with_context(|| format!("could not read {}", registry.display()))?;
    let mut document: toml::Value =
        toml::from_str(&raw).with_context(|| format!("{} is not TOML", registry.display()))?;
    let entries = document
        .get_mut("workspaces")
        .and_then(toml::Value::as_array_mut)
        .with_context(|| format!("{} holds no workspaces array", registry.display()))?;
    let before = entries.len();
    entries.retain(|w| w.get("id").and_then(toml::Value::as_str) != Some(workspace_id));
    if entries.len() + 1 != before {
        bail!(
            "{} did not hold workspace {workspace_id} exactly once",
            registry.display()
        );
    }
    std::fs::write(registry, toml::to_string_pretty(&document)?)
        .with_context(|| format!("could not rewrite {}", registry.display()))
}

fn registry_holds(registry: &Path, workspace_id: &str) -> Result<bool> {
    let raw = std::fs::read_to_string(registry)
        .with_context(|| format!("could not read {}", registry.display()))?;
    let document: toml::Value =
        toml::from_str(&raw).with_context(|| format!("{} is not TOML", registry.display()))?;
    Ok(find_table_with_id(&document, workspace_id).is_some())
}

/// Wait until nothing matches `selector`.
async fn await_absent(driver: &WebDriver, selector: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        if driver.find_all(By::Css(selector)).await?.is_empty() {
            return Ok(());
        }
        if started.elapsed() > MEMBERSHIP_DEADLINE {
            bail!(
                "{selector} was still shown after {}s",
                MEMBERSHIP_DEADLINE.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The workspace a project belongs to, as `list_projects` reports it.
async fn assert_backend_scope(
    driver: &WebDriver,
    project_id: &str,
    workspace_id: Option<&str>,
) -> Result<()> {
    let listed = ui::invoke(driver, "list_projects", json!({})).await?;
    let project = listed
        .as_array()
        .and_then(|all| {
            all.iter()
                .find(|p| p.get("id").and_then(Value::as_str) == Some(project_id))
        })
        .with_context(|| format!("list_projects does not return {project_id}: {listed}"))?;
    let scope = &project["scope"];
    let actual = match scope.get("kind").and_then(Value::as_str) {
        Some("standalone") => None,
        Some("in_workspace") => scope.get("workspace_id").and_then(Value::as_str),
        _ => bail!("project {project_id} has an unrecognised scope: {scope}"),
    };
    if actual != workspace_id {
        bail!("project {project_id} is in workspace {actual:?}, expected {workspace_id:?}");
    }
    Ok(())
}

/// The workspace a project belongs to, as the application wrote it to disk.
fn assert_persisted_scope(
    config_file: &Path,
    project_id: &str,
    workspace_id: Option<&str>,
) -> Result<()> {
    let raw = std::fs::read_to_string(config_file)
        .with_context(|| format!("could not read {}", config_file.display()))?;
    let document: toml::Value =
        toml::from_str(&raw).with_context(|| format!("{} is not TOML", config_file.display()))?;
    let project = find_table_with_id(&document, project_id).with_context(|| {
        format!(
            "{} holds no record for project {project_id}",
            config_file.display()
        )
    })?;
    let actual = match project.get("scope") {
        None => None,
        Some(scope) => match scope.get("kind").and_then(toml::Value::as_str) {
            Some("standalone") => None,
            Some("in_workspace") => scope.get("workspace_id").and_then(toml::Value::as_str),
            _ => bail!("project {project_id} was persisted with an unrecognised scope: {scope}"),
        },
    };
    if actual != workspace_id {
        bail!(
            "{} records project {project_id} in workspace {actual:?}, expected {workspace_id:?}",
            config_file.display()
        );
    }
    Ok(())
}

/// The table anywhere in the document whose `id` is `id`.
fn find_table_with_id<'a>(value: &'a toml::Value, id: &str) -> Option<&'a toml::value::Table> {
    match value {
        toml::Value::Table(table) => {
            if table.get("id").and_then(toml::Value::as_str) == Some(id) {
                return Some(table);
            }
            table.values().find_map(|v| find_table_with_id(v, id))
        }
        toml::Value::Array(items) => items.iter().find_map(|v| find_table_with_id(v, id)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn run(session: &str, working_dir: &str) -> fake_agent::Invocation {
        fake_agent::Invocation {
            working_dir: PathBuf::from(working_dir),
            args: ["-p", "--session-id", session]
                .iter()
                .map(OsString::from)
                .collect(),
            prompt: Some("do the work".to_string()),
        }
    }

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "slashit-persistence-{}-{}-{label}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    /// A store written the way `Storage::save_project_tasks` writes one.
    fn store(root: &Path, records: &[(&str, &str, &str)]) -> PathBuf {
        let mut document = String::from("version = 1\n");
        for (id, title, status) in records {
            document.push_str(&format!(
                "\n[[tasks]]\nid = \"{id}\"\ntitle = \"{title}\"\nstatus = \"{status}\"\n"
            ));
        }
        let file = root.join("tasks.toml");
        std::fs::write(&file, document).expect("write the task store");
        file
    }

    fn executed(id: &str, title: &str) -> ExecutedTask {
        ExecutedTask {
            id: id.to_string(),
            project_id: "project-under-test".to_string(),
            title: title.to_string(),
            worktree_path: PathBuf::from("/tmp/wt/task-abcd1234"),
        }
    }

    /// The state the journeys actually assert: one record holding all three.
    #[test]
    fn a_record_carrying_the_id_title_and_status_together_is_the_proof() {
        let root = scratch("settled");
        store(
            &root,
            &[
                ("11111111-aaaa", "some other task", "backlog"),
                ("22222222-bbbb", "the task under test", "human_review"),
            ],
        );

        assert!(assert_persisted(
            &root,
            &executed("22222222-bbbb", "the task under test"),
            "human_review"
        )
        .is_ok());
    }

    /// The proof this helper exists to make impossible. Neither record is the
    /// task the journey settled: one has its id under a different title and
    /// status, the other has the title and status under a different id. Read as
    /// loose strings the file contains every value being looked for, which is
    /// exactly why looking for loose strings proved nothing.
    #[test]
    fn values_split_across_two_records_are_not_a_persisted_task() {
        let root = scratch("split");
        store(
            &root,
            &[
                ("22222222-bbbb", "some other task", "backlog"),
                ("11111111-aaaa", "the task under test", "human_review"),
            ],
        );

        assert!(
            assert_persisted(
                &root,
                &executed("22222222-bbbb", "the task under test"),
                "human_review"
            )
            .is_err(),
            "a task's id, title and status were read from different records"
        );
    }

    /// Rejections that would otherwise look like the settled state.
    #[test]
    fn a_record_that_does_not_match_is_not_the_proof() {
        let root = scratch("mismatched");
        store(
            &root,
            &[("22222222-bbbb", "the task under test", "in_progress")],
        );

        for (case, id, status) in [
            (
                "the task was never written at all",
                "33333333-cccc",
                "human_review",
            ),
            (
                "the record settled at another status",
                "22222222-bbbb",
                "human_review",
            ),
        ] {
            assert!(
                assert_persisted(&root, &executed(id, "the task under test"), status).is_err(),
                "{case} was accepted as proof of the settled state"
            );
        }
    }

    /// What the runner produces for a `None` session: the flag is not passed at
    /// all, rather than passed with nothing after it.
    fn run_without_a_session(working_dir: &str) -> fake_agent::Invocation {
        fake_agent::Invocation {
            working_dir: PathBuf::from(working_dir),
            args: ["-p"].iter().map(OsString::from).collect(),
            prompt: Some("do the work".to_string()),
        }
    }

    /// The selection has to survive the records arriving in an order that says
    /// nothing about when they ran, because that is the only order the fixture
    /// guarantees. Reversed here on purpose: positional selection would return
    /// the failed attempt and every assertion downstream would still pass.
    #[test]
    fn the_retry_is_found_by_session_whatever_order_the_records_arrive_in() {
        let failed = OsString::from("session-of-the-failure");
        let shared_worktree = "/tmp/wt/task-abcd1234";

        for runs in [
            vec![
                run("session-of-the-failure", shared_worktree),
                run("session-of-the-retry", shared_worktree),
            ],
            vec![
                run("session-of-the-retry", shared_worktree),
                run("session-of-the-failure", shared_worktree),
            ],
        ] {
            let retry = retry_among(&runs, &failed).expect("one run is not the failed attempt");
            assert_eq!(
                retry.flag("--session-id"),
                Some(OsStr::new("session-of-the-retry")),
                "the retry was selected by position rather than by session"
            );
        }
    }

    /// Everything the retry is not, read together, because the ways this can go
    /// wrong are variations on one idea: a record the journey cannot show to be
    /// a second execution with an identity of its own. Each row would otherwise
    /// be selected and then satisfy every remaining assertion in the stage,
    /// since both attempts of a retried task share a worktree and a set of
    /// flags — which is what makes these quiet rather than loud.
    #[test]
    fn no_run_without_an_identity_of_its_own_is_taken_as_the_retry() {
        let failed = OsString::from("session-of-the-failure");
        let shared_worktree = "/tmp/wt/task-abcd1234";

        for (case, runs) in [
            (
                "a candidate that never reported a session",
                vec![
                    run("session-of-the-failure", shared_worktree),
                    run_without_a_session(shared_worktree),
                ],
            ),
            (
                "a candidate whose session is empty",
                vec![
                    run("session-of-the-failure", shared_worktree),
                    run("", shared_worktree),
                ],
            ),
            (
                "two runs reporting the failed attempt's own session",
                vec![
                    run("session-of-the-failure", shared_worktree),
                    run("session-of-the-failure", shared_worktree),
                ],
            ),
            (
                "more separately identified runs than the journey arranged",
                vec![
                    run("session-of-the-failure", shared_worktree),
                    run("session-of-the-retry", shared_worktree),
                    run("session-of-something-else", shared_worktree),
                ],
            ),
        ] {
            assert!(
                retry_among(&runs, &failed).is_err(),
                "{case} was accepted as the retry"
            );
        }
    }
}
