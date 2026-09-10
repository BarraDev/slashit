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

    pin_worktree_placement(&context.state().config_file())?;
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

    pin_worktree_placement(&context.state().config_file())?;
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

    pin_worktree_placement(&context.state().config_file())?;
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
        .flag("-p")
        .context("the agent was invoked without a prompt")?;
    if prompt.is_empty() {
        bail!("the agent was invoked with an empty prompt");
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
    // tells the two runs apart later. Records are named by `mktemp` and read
    // back in name order, which is unique but not chronological, so position
    // in that list proves nothing about which run came first.
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
    if reported != Some(fake_agent::REPORTED_FAILURE) {
        bail!(
            "the task carries error message {reported:?}, but the agent reported {:?} — the \
             failure under test is not the one that happened",
            fake_agent::REPORTED_FAILURE
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
        .flag("-p")
        .context("the agent was invoked without a prompt")?;
    if prompt.is_empty() {
        bail!("the agent was invoked with an empty prompt");
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
    // `AiReview` is what the executor sets the moment the agent succeeds, and
    // the review it then schedules finds no diff to review (the fixture
    // changes nothing), so the settled state is `HumanReview`. Both are read
    // back through the frontend's own `list_tasks`.
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
            json!({ "localPath": repository.path(), "remoteUrl": Value::Null }),
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
/// never does: the prompt.
fn agent_runs(agent: &FakeAgent) -> Result<Vec<fake_agent::Invocation>> {
    Ok(agent
        .invocations()?
        .into_iter()
        .filter(|invocation| invocation.has_flag("-p"))
        .collect())
}

/// The run that is not the one `failed_session` identifies.
///
/// Records are read back in file-name order, and the fixture names them with
/// `mktemp`, which is unique but not monotonic — so the position of a record in
/// `runs` says nothing about when it ran. Both attempts of a retried task also
/// share a worktree and a set of flags, which is what makes picking the wrong
/// one dangerous rather than merely wrong: every assertion the journey makes
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

/// Force SlashIt to place worktrees under its own state root.
///
/// The default placement delegates to worktrunk (`wt`) when the developer has
/// it installed, and `wt` decides the location from the user's configuration —
/// which would put this run's worktrees outside the directory the harness
/// owns and cleans. Hosted CI has no `wt` and therefore already behaves this
/// way, so pinning it makes a local run match CI rather than diverging from
/// it.
///
/// Written as the product's own configuration file, before the first launch,
/// because placement is read once at startup and no command exposes it.
fn pin_worktree_placement(config_file: &Path) -> Result<()> {
    let parent = config_file
        .parent()
        .context("the config file has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;
    std::fs::write(config_file, "[worktree]\nplacement = \"managed\"\n")
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
}

impl GitFixture {
    fn create(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)
            .with_context(|| format!("could not create {}", path.display()))?;

        git(path, &["init", "--quiet"])?;
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

        Ok(Self {
            path: path.to_path_buf(),
            state_root,
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
/// a finished cleanup: `wt switch` refuses a worktree whose directory is
/// missing and refuses to create one whose branch already exists, so the task
/// would be left unable to have a worktree at all.
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

    pin_worktree_placement(&context.state().config_file())?;
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

    let invocations = agent_runs(&agent)?.len();
    if invocations != 1 {
        bail!("the agent ran {invocations} times over the whole journey, expected exactly once");
    }

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
    // What the board sends when a card is dropped into the Done column.
    ui::invoke(
        driver,
        "reorder_task",
        json!({
            "taskId": executed.id,
            "newStatus": "done",
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

    pin_worktree_placement(&context.state().config_file())?;
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

    let invocations = agent_runs(agent)?.len();
    if invocations != 1 {
        bail!("the agent ran {invocations} times over the whole journey, expected exactly once");
    }

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

    pin_worktree_placement(&context.state().config_file())?;
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
    // column. Answering success here would tell the user their checkout had
    // been dealt with while it is still sitting on disk.
    let refusal = ui::invoke_expecting_refusal(
        driver,
        "reorder_task",
        json!({
            "taskId": executed.id,
            "newStatus": "done",
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

    pin_worktree_placement(&context.state().config_file())?;
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

    pin_worktree_placement(&context.state().config_file())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn run(session: &str, working_dir: &str) -> fake_agent::Invocation {
        fake_agent::Invocation {
            working_dir: PathBuf::from(working_dir),
            args: ["-p", "do the work", "--session-id", session]
                .iter()
                .map(OsString::from)
                .collect(),
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
            args: ["-p", "do the work"].iter().map(OsString::from).collect(),
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
