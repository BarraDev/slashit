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
const POLL: Duration = Duration::from_millis(250);

/// The Kanban column for a status, as the board names it: the frontend builds
/// the identifier with `format!("{:?}", status).to_lowercase()`.
const HUMAN_REVIEW_COLUMN: &str = "[data-testid=\"column-humanreview\"]";
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
    let failed_session = first_run[0]
        .flag("--session-id")
        .context("the failed run carried no --session-id to tell the retry apart from")?
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
        title,
        worktree_path: retried_worktree,
    })
}

/// What the journey learned about the task it drove.
struct ExecutedTask {
    id: String,
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
    let mut holding: Vec<PathBuf> = Vec::new();
    for file in toml_files(root)? {
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        examined += 1;
        if !contents.contains(&task.id) {
            continue;
        }
        holding.push(file);
        // One file carrying the id, the title and the settled status is the
        // whole claim. Others may exist — the store has had more than one
        // layout — and an older one lagging behind is not evidence that this
        // state was never written.
        if contents.contains(&task.title) && contents.contains(status) {
            return Ok(());
        }
    }

    if holding.is_empty() {
        bail!(
            "no file under {} mentions task {} — nothing was persisted ({examined} TOML files \
             examined)",
            root.display(),
            task.id
        );
    }

    bail!(
        "task {} is recorded in {} but no file carries its title and the status {status:?}, so \
         the state the application reported never reached the disk",
        task.id,
        holding
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
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
/// value the two runs cannot share, and identity is what this selects on.
fn retry_among<'a>(
    runs: &'a [fake_agent::Invocation],
    failed_session: &OsStr,
) -> Result<&'a fake_agent::Invocation> {
    let retries: Vec<&fake_agent::Invocation> = runs
        .iter()
        .filter(|run| run.flag("--session-id") != Some(failed_session))
        .collect();

    match retries.as_slice() {
        [only] => Ok(only),
        [] => bail!(
            "every agent run reports session {failed_session:?}, so the retry cannot be told \
             apart from the attempt it was retrying"
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

    /// Two runs reporting one session would mean the journey cannot say which
    /// execution it is describing, so it must not quietly pick either.
    #[test]
    fn an_indistinguishable_pair_of_runs_is_refused() {
        let failed = OsString::from("session-of-the-failure");
        let runs = vec![
            run("session-of-the-failure", "/tmp/wt/task-abcd1234"),
            run("session-of-the-failure", "/tmp/wt/task-abcd1234"),
        ];

        assert!(retry_among(&runs, &failed).is_err());
    }

    /// More retries than the journey arranged means something ran that it did
    /// not account for, which is not something to select a "the" retry from.
    #[test]
    fn more_than_one_unexpected_run_is_refused() {
        let failed = OsString::from("session-of-the-failure");
        let runs = vec![
            run("session-of-the-failure", "/tmp/wt/task-abcd1234"),
            run("session-of-the-retry", "/tmp/wt/task-abcd1234"),
            run("session-of-something-else", "/tmp/wt/task-abcd1234"),
        ];

        assert!(retry_among(&runs, &failed).is_err());
    }
}
