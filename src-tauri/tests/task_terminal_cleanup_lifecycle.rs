//! What a task's *lifecycle* is allowed to say while its terminal cleanup is
//! still able to destroy the checkout that lifecycle points at.
//!
//! The manager-level tests next door already prove that `WorktreeManager`
//! removes the right directory and reports the right answer. They cannot prove
//! anything about the board, because they never touch it. This file is the
//! other half: it drives the *real* `#[tauri::command]` bodies the UI invokes
//! -- `update_task_status`, not a copy of its logic -- against a real
//! `AppState` built by `app_core::build_state_with_paths`, over a real git
//! repository, and parks the real `git worktree remove` mid-flight so the
//! window that is normally microseconds wide can be inspected.
//!
//! # Why an integration test and not a unit test
//!
//! `AppState` cannot be built from outside the crate (`worktree` is a private
//! module, so `AppState::worktree_manager`'s type is unnameable), and
//! `tauri::State` is a newtype over a private reference with no constructor.
//! `build_state_with_paths` solves the first -- it hands back a whole
//! `AppState` without the caller naming any of its field types -- and
//! `tauri::test::mock_app` solves the second: a managed app is the only public
//! way to obtain a `State<'_, AppState>`. Together they let a test outside the
//! crate call the exact function the IPC layer calls.
//!
//! It has to be an integration test rather than an in-crate `#[cfg(test)]`
//! module for a second reason: the barrier below replaces `git` on `PATH`, and
//! `PATH` is process-global. An integration target is its own process, and
//! this file owns every test in it, so the mutation cannot race a test that
//! knows nothing about it. The library's unit-test binary runs several hundred
//! tests, dozens of which shell out to git, in parallel -- exactly the
//! situation `std::env::set_var` is unsound in.
//!
//! # The barrier
//!
//! A `git` shim goes first on `PATH`. Every invocation it does not recognise
//! is `exec`'d straight through to the real binary, so exit status, stdout,
//! stderr and stdin are the real ones and nothing else in the process is
//! affected. It intercepts exactly one shape -- `git worktree remove <target>`
//! and `git worktree remove --force <target>`, with no other arguments -- and
//! only when a FIFO directory for *that exact target string* exists, which
//! only the test that armed it created. An unrelated `git worktree remove`
//! anywhere in the process runs untouched.
//!
//! On a match the shim announces its arrival, blocks until released, then --
//! depending on how the test armed that boundary -- either runs the real git
//! or refuses without running anything, and reports the resulting exit status
//! back either way. Every pipe is opened by the
//! test in read-write mode, so the shim can never block on *opening* one and
//! can never block on *writing* to one: its only blocking point in the whole
//! script is the release read, and `Barrier`'s `Drop` satisfies that on every
//! exit path, panics included. A shim process therefore cannot outlive the
//! test that armed it.
//!
//! Unix-only: FIFOs, `PATH` and `/bin/sh` are all load-bearing here.
#![cfg(unix)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tauri::Manager;
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::unix::pipe;

use slashit_ui_lib::config::paths::{AppPaths, StateLocation, WorktreePlacement};
use slashit_ui_lib::config::storage::{AppConfig, WorktreeConfig};
use slashit_ui_lib::domain::{
    AgentConfig, AgentType, Project, ProjectScope, Repository, Task, TaskPhase, TaskStatus,
};
use slashit_ui_lib::test_helpers::create_test_task_full;
use slashit_ui_lib::AppState;

/// How long a call is given to come back before the test concludes it has
/// chosen to *wait* rather than answer.
///
/// This is not a synchronisation device, and the failure it guards against
/// never reaches it: a defective implementation publishes its successor state
/// synchronously, in microseconds, so a bound of any size catches it. What the
/// bound is for is the implemented design, which holds a competing transition
/// behind the task's lifecycle lease until the parked cleanup settles -- an
/// outcome this file explicitly permits and scores as "did not publish a
/// successor state".
///
/// Short rather than generous, because both are equally sound here and the
/// difference is entirely how long a *passing* run takes: nothing that is going
/// to answer without the barrier being released needs longer than this, and
/// anything that does answer is answering from the lease being free.
const DESIGN_MAY_WAIT: Duration = Duration::from_secs(5);

/// A bound on the one call this file requires to come back *unblocked*.
///
/// It is not synchronisation either: it turns the failure it guards against --
/// a shim that parks a removal it was never armed for -- into a named
/// assertion instead of a hung test binary.
const UNRELATED_REMOVAL_MUST_NOT_BLOCK: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Process-wide test environment
// ---------------------------------------------------------------------------

/// The one `PATH`/XDG mutation this binary performs, and the directories it
/// points at.
///
/// XDG is redirected because `build_state_with_paths` builds a `PtyState`,
/// whose `SessionStore::new()` resolves the *real* application directories and
/// rewrites the developer's live `terminal_sessions.toml` as a side effect. It
/// takes no `AppPaths`, so redirecting the environment is the only way to keep
/// a test off the developer's own terminal session file.
struct TestEnvironment {
    /// Held for the process lifetime. A `static` is never dropped, so this
    /// directory outlives the run; it holds one shell script and a handful of
    /// FIFO directories, all under the system temp root.
    _root: TempDir,
    barrier_root: PathBuf,
}

static ENVIRONMENT: LazyLock<TestEnvironment> = LazyLock::new(install_test_environment);

/// Resolve `git` against the *current* `PATH`, before it is shimmed.
///
/// The absolute path is baked into the shim, which is what makes recursion
/// impossible: the shim never looks a binary up by name.
fn locate_real_git() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH must be set");
    std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("a real git binary must be on PATH")
}

fn install_test_environment() -> TestEnvironment {
    let root = TempDir::new().expect("tempdir for the shared test environment");
    let real_git = locate_real_git();
    // Named, not just `bin`: a parked shim is a `/bin/sh <this path> worktree
    // remove ...` process, so the directory name is what makes `pgrep -f
    // slashit-lifecycle-shim` a complete census of this file's children.
    let shim_dir = root.path().join("slashit-lifecycle-shim");
    let barrier_root = root.path().join("barriers");
    fs::create_dir_all(&shim_dir).expect("shim dir");
    fs::create_dir_all(&barrier_root).expect("barrier root");

    let shim = shim_dir.join("git");
    fs::write(
        &shim,
        format!(
            r#"#!/bin/sh
# TEST SHIM. Installed by tests/task_terminal_cleanup_lifecycle.rs.
#
# Delegates everything to the real git. The single exception is the exact
# destructive boundary `git worktree remove [--force] <target>` for a target
# one of this binary's tests has armed a FIFO directory for; that invocation
# announces itself, waits to be released, and only then does what it was asked.
#
# The `--force` arm is kept although production no longer has a forced
# removal, and that is what makes it useful: it announces itself as 'force',
# so a forced removal reintroduced anywhere under the armed path shows up as
# an unexpected crossing in the assertions below rather than passing unseen.
REAL_GIT='{real_git}'
BARRIER_ROOT='{barrier_root}'

target=''
mode=''
if [ "$1" = 'worktree' ] && [ "$2" = 'remove' ]; then
    if [ "$#" -eq 3 ]; then
        mode='plain'
        target="$3"
    elif [ "$#" -eq 4 ] && [ "$3" = '--force' ]; then
        mode='force'
        target="$4"
    fi
fi

if [ -n "$target" ]; then
    dir="$BARRIER_ROOT/$(printf '%s' "$target" | tr '/' '_')"
    if [ -d "$dir" ]; then
        # Every descriptor is taken before anything is announced. The test
        # holds all three pipes read-write, so none of these opens can block
        # and neither can the writes below; the release read is the only place
        # this script is allowed to stop. Holding them across the removal also
        # means the test may unlink the directory at any time without turning
        # a finished removal into a broken pipe.
        exec 8>"$dir/arrived"
        exec 9<"$dir/release"
        exec 7>"$dir/finished"

        printf '%s\n' "$mode" >&8
        read -r _release <&9 || true

        if [ -f "$dir/refuse" ]; then
            # The removal does not happen: no delegation, a distinctive
            # non-zero status, and the checkout left exactly as it was. That
            # is the only input the lifecycle under test consumes -- what
            # would have made a real git refuse is not part of the property.
            status=111
        else
            "$REAL_GIT" "$@"
            status=$?
        fi
        printf '%s\n' "$status" >&7
        exit $status
    fi
fi

exec "$REAL_GIT" "$@"
"#,
            real_git = real_git.display(),
            barrier_root = barrier_root.display(),
        ),
    )
    .expect("write git shim");
    make_executable(&shim);

    // One `set_var` burst, performed inside the `LazyLock` initialiser, so it
    // happens exactly once and every other test thread is parked inside the
    // lock while it runs.
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("fake home");
    let shimmed_path = {
        let existing = std::env::var_os("PATH").expect("PATH must be set");
        let mut dirs = vec![shim_dir.clone()];
        dirs.extend(std::env::split_paths(&existing));
        std::env::join_paths(dirs).expect("join PATH")
    };
    // SAFETY (soundness, not memory): this integration target owns every test
    // in it, all of them go through this `LazyLock`, and no thread has looked
    // at the environment before it returns.
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", root.path().join("xdg-config"));
    std::env::set_var("XDG_DATA_HOME", root.path().join("xdg-data"));
    std::env::set_var("XDG_CACHE_HOME", root.path().join("xdg-cache"));
    std::env::set_var("XDG_RUNTIME_DIR", root.path().join("xdg-runtime"));
    std::env::set_var("PATH", shimmed_path);

    TestEnvironment {
        _root: root,
        barrier_root,
    }
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).expect("stat shim").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod shim");
}

// ---------------------------------------------------------------------------
// The barrier
// ---------------------------------------------------------------------------

/// A hold on one exact `git worktree remove <target>`.
///
/// The directory name is derived from the target path, so two tests running at
/// once cannot arm each other's boundary and an unrelated removal cannot land
/// in one at all.
struct Barrier {
    dir: PathBuf,
    arrived: pipe::Receiver,
    release: pipe::Sender,
    finished: pipe::Receiver,
}

impl Barrier {
    /// Park every crossing of `target`'s destructive boundary, then let the
    /// real git do exactly what it was asked.
    fn arm(target: &str) -> io::Result<Self> {
        Self::install(target, false)
    }

    /// Park every crossing and then refuse it: git is never run, so the
    /// checkout and everything in it survive untouched and
    /// `WorktreeManager::remove` reaches its own "the directory is still
    /// there" conclusion through the ordinary production path.
    ///
    /// A shim refusal rather than, say, an unwritable directory because the
    /// outcome has to be the same for every user. A read-only checkout was
    /// verified by hand to produce the identical production-observable state
    /// -- non-zero git exit, checkout and work intact -- but it produces it
    /// only for a user the file mode applies to, and root is not one.
    fn arm_refusing(target: &str) -> io::Result<Self> {
        Self::install(target, true)
    }

    fn install(target: &str, refuse: bool) -> io::Result<Self> {
        let dir = ENVIRONMENT.barrier_root.join(target.replace('/', "_"));
        fs::create_dir_all(&dir)?;
        for name in ["arrived", "release", "finished"] {
            let fifo = dir.join(name);
            let status = Command::new("mkfifo").arg(&fifo).status()?;
            assert!(status.success(), "mkfifo {} failed", fifo.display());
        }
        if refuse {
            fs::write(dir.join("refuse"), b"")?;
        }

        // Read-write on every pipe. A receiver opened this way never reports
        // end-of-file when the shim exits, and -- the part that matters -- the
        // shim's own opens and writes can never block for want of a peer.
        let opts = || {
            let mut o = pipe::OpenOptions::new();
            o.read_write(true);
            o
        };
        Ok(Self {
            arrived: opts().open_receiver(dir.join("arrived"))?,
            release: opts().open_sender(dir.join("release"))?,
            finished: opts().open_receiver(dir.join("finished"))?,
            dir,
        })
    }
}

impl Drop for Barrier {
    /// Release anything still parked, then take the boundary away.
    ///
    /// This runs on the panicking path too, which is the whole point: the
    /// release read is the only place the shim can block, so satisfying it
    /// unconditionally here is what guarantees no shim process outlives the
    /// test. Several tokens rather than one, because a failed test may have
    /// left more than one invocation parked.
    fn drop(&mut self) {
        for _ in 0..4 {
            // The test holds a read end of this pipe open, so this cannot
            // block; a full buffer would be the only failure and 32 bytes
            // cannot fill one.
            let _ = self.release.try_write(b"release\n");
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Read one newline-terminated announcement.
///
/// Byte at a time because the messages are a word long and a partial read must
/// not be mistaken for a whole one.
async fn read_announcement(pipe: &mut pipe::Receiver) -> String {
    let mut line = Vec::new();
    loop {
        let byte = pipe
            .read_u8()
            .await
            .expect("the barrier pipe must stay readable");
        if byte == b'\n' {
            return String::from_utf8(line).expect("announcements are ASCII");
        }
        line.push(byte);
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A repository, a project, a task, and the task's real worktree with real
/// committed work in it.
struct Fixture {
    _tmp: TempDir,
    repo_path: String,
    worktree_path: String,
    branch: String,
    task_id: uuid::Uuid,
    work_file: PathBuf,
    /// A second checkout of the same repository that no test ever arms, kept
    /// so the shim's precision can be demonstrated rather than asserted.
    unrelated_worktree: String,
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git must be spawnable");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build the whole world this file's tests need, and hand back a live
/// `AppState` alongside it.
async fn build_fixture(name: &str) -> (Fixture, AppState) {
    let tmp = TempDir::new().expect("fixture tempdir");
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).expect("repo dir");

    git(&repo_dir, &["init", "-q", "-b", "main"]);
    git(&repo_dir, &["config", "user.email", "test@example.com"]);
    git(&repo_dir, &["config", "user.name", "Test"]);
    fs::write(repo_dir.join("README.md"), "base\n").expect("seed file");
    git(&repo_dir, &["add", "."]);
    git(&repo_dir, &["commit", "-q", "-m", "init"]);
    let repo_path = repo_dir.to_str().expect("utf-8 repo path").to_string();

    let paths = Arc::new(AppPaths::with_roots(
        tmp.path().join("config"),
        tmp.path().join("data"),
        tmp.path().join("cache"),
        tmp.path().join("runtime"),
    ));

    let repository_id = uuid::Uuid::new_v4();
    let project_id = uuid::Uuid::new_v4();
    let task_id = uuid::Uuid::new_v4();
    // Exactly what `WorktreeManager::branch_for_task` derives. The point of
    // spelling it out is that a task's branch -- and therefore the worktree
    // path built from it -- is a pure function of the task id, so a successor
    // execution of the *same* task lands on the *same* checkout. That is what
    // makes an overlapping cleanup a collision rather than a coincidence.
    let branch = format!("task-{}", &task_id.to_string()[..8]);

    // `Managed` rather than the default `Auto`: `Auto` delegates to worktrunk
    // when `wt` is installed, which it is on this machine, and the boundary
    // under test is git's.
    let mut config = AppConfig {
        worktree: WorktreeConfig {
            placement: WorktreePlacement::Managed,
        },
        ..Default::default()
    };
    config.repositories.insert(
        repository_id.to_string(),
        Repository {
            id: repository_id,
            local_path: repo_path.clone(),
            remote_url: None,
            remote_type: None,
            created_at: chrono::Utc::now(),
        },
    );
    config.projects.insert(
        project_id.to_string(),
        Project {
            id: project_id,
            name: format!("lifecycle-{name}"),
            repository_id: Some(repository_id),
            scope: ProjectScope::Standalone,
            state_location: StateLocation::External,
            agent_type: AgentType::ClaudeCode,
            agent_config: AgentConfig {
                agent_type: AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: Default::default(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
    );

    let storage = slashit_ui_lib::config::Storage::with_paths((*paths).clone());
    storage.save_config(&config).expect("seed config");

    let (state, _report) = slashit_ui_lib::app_core::build_state_with_paths(paths)
        .await
        .expect("state must build under a tempdir");

    // The product's own worktree creation, so the checkout under test sits
    // exactly where the product would put it.
    let info = state
        .worktree_manager
        .create(&repo_path, &branch)
        .await
        .expect("worktree creation");
    let worktree_path = info.path.clone();

    // Real work, committed, so the checkout is clean and an ordinary
    // `git worktree remove` succeeds on the first attempt -- one crossing of
    // the destructive boundary, which is what makes the interleaving below a
    // single unambiguous step.
    let work_file = PathBuf::from(&worktree_path).join("work.txt");
    fs::write(&work_file, "work produced by the task\n").expect("write work");
    let worktree_dir = PathBuf::from(&worktree_path);
    git(&worktree_dir, &["config", "user.email", "test@example.com"]);
    git(&worktree_dir, &["config", "user.name", "Test"]);
    git(&worktree_dir, &["add", "."]);
    git(&worktree_dir, &["commit", "-q", "-m", "task work"]);

    // A second worktree in the same repository, belonging to nothing under
    // test. Removing it exercises the identical `git worktree remove <path>`
    // shape the shim intercepts, with a different target.
    let unrelated = tmp.path().join("unrelated-checkout");
    let unrelated_worktree = unrelated
        .to_str()
        .expect("utf-8 unrelated path")
        .to_string();
    git(
        &repo_dir,
        &[
            "worktree",
            "add",
            "-q",
            &unrelated_worktree,
            "-b",
            "unrelated",
        ],
    );

    let mut task = create_test_task_full(name, project_id, TaskStatus::InProgress, 0);
    task.id = task_id;
    task.phase = TaskPhase::Coding;
    task.worktree_path = Some(worktree_path.clone());
    task.branch_name = Some(branch.clone());
    state.task.tasks.write().await.insert(task_id, task.clone());
    state
        .storage
        .save_project_tasks(project_id, &[task])
        .expect("seed the task board");

    (
        Fixture {
            _tmp: tmp,
            repo_path,
            worktree_path,
            branch,
            task_id,
            work_file,
            unrelated_worktree,
        },
        state,
    )
}

/// The queue executor's own admission rule, restated where the property that
/// depends on it is asserted.
///
/// `TaskExecutor::is_pending` is private, and duplicating it here rather than
/// reaching for it is deliberate: this is the definition of "the product will
/// start running this task on its next pass", and a change to it has to be a
/// visible change to this regression's meaning.
fn executor_would_start(task: &Task) -> bool {
    task.status == TaskStatus::InProgress && task.phase == TaskPhase::Idle
}

// ---------------------------------------------------------------------------
// The regressions
// ---------------------------------------------------------------------------

/// A task that is moved back out of `Done` must not become an executable task
/// again while the cleanup that `Done` started can still delete the checkout
/// that task would run in.
///
/// This deliberately does not say *how*. Refusing the move, holding it open
/// until cleanup settles, and retrying it afterwards are all designs that pass
/// this test; the only thing it forbids is a successor state that is both
/// authoritative and executable while the old cleanup is still mid-removal.
#[test]
fn a_reactivated_task_does_not_become_executable_while_its_cleanup_can_still_destroy_its_worktree()
{
    LazyLock::force(&ENVIRONMENT);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let (fixture, state) = rt.block_on(build_fixture("reactivation"));
    let app = tauri::test::mock_app();
    app.manage(state);

    rt.block_on(async {
        // `State` is what the command signature demands; `inner()` is the
        // same `AppState` the rest of the assertions read.
        let state: &AppState = app.state::<AppState>().inner();
        let mut barrier = Barrier::arm(&fixture.worktree_path).expect("arm the boundary");

        // (1) The task is non-terminal, with a real checkout holding real work.
        assert!(
            fixture.work_file.exists(),
            "the fixture must start with work on disk"
        );

        // (2) Ask for the terminal transition through the real command.
        //     Not awaited to exhaustion here: an implementation is free to hold
        //     this open until its cleanup has finished, and this test must not
        //     require it to answer early.
        let terminal = slashit_ui_lib::commands::task::update_task_status(
            app.state(),
            fixture.task_id.to_string(),
            TaskStatus::Done,
        );
        tokio::pin!(terminal);
        let mut terminal_outcome: Option<Result<Option<Task>, String>> = None;

        // (3) Run it until the cleanup it started is parked *inside* the
        //     destructive step. The announcement is the proof: the shim only
        //     writes it after `git worktree remove <this exact path>` has been
        //     invoked and before the real git has been allowed to run.
        let arrival_fut = read_announcement(&mut barrier.arrived);
        tokio::pin!(arrival_fut);
        let arrival = loop {
            tokio::select! {
                outcome = &mut terminal, if terminal_outcome.is_none() => {
                    terminal_outcome = Some(outcome);
                }
                announced = &mut arrival_fut => break announced,
            }
        };
        assert_eq!(
            arrival, "plain",
            "the boundary reached must be the ordinary removal of a clean checkout"
        );
        assert!(
            fixture.work_file.exists(),
            "nothing may be deleted before the barrier is released"
        );

        // The shim must be blind to every removal but the one it was armed
        // for. Demonstrated rather than claimed: an unrelated checkout in the
        // same repository is removed through the same shimmed `git`, with the
        // same argument shape, while the armed boundary is still parked -- and
        // it has to come back on its own.
        let unrelated = tokio::time::timeout(
            UNRELATED_REMOVAL_MUST_NOT_BLOCK,
            tokio::process::Command::new("git")
                .args(["worktree", "remove", &fixture.unrelated_worktree])
                .current_dir(&fixture.repo_path)
                .output(),
        )
        .await;
        match &unrelated {
            Ok(Ok(output)) => assert!(
                output.status.success(),
                "an unrelated `git worktree remove` must behave exactly as it would \
                 without the shim: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            other => panic!("the shim parked or broke a removal it was never armed for: {other:?}"),
        }
        assert!(
            !Path::new(&fixture.unrelated_worktree).exists(),
            "the unrelated checkout must actually be gone, which is what proves the \
             invocation reached the real git"
        );

        if terminal_outcome.is_none() {
            terminal_outcome = tokio::time::timeout(DESIGN_MAY_WAIT, &mut terminal)
                .await
                .ok();
        }

        // (4a) The terminal transition must not already be settled: at this
        //      instant not one byte has been removed, so nothing could have
        //      told it that it succeeded.
        let settled_terminal = matches!(
            &terminal_outcome,
            Some(Ok(Some(task))) if task.status == TaskStatus::Done
        );

        // (4b) Ask for the task to be reactivated, the same way a card dragged
        //      back out of Done does.
        let reactivation = slashit_ui_lib::commands::task::update_task_status(
            app.state(),
            fixture.task_id.to_string(),
            TaskStatus::InProgress,
        );
        let reactivation_outcome = tokio::time::timeout(DESIGN_MAY_WAIT, reactivation)
            .await
            .ok();

        // What the rest of the product would now read.
        let authoritative = state
            .task
            .tasks
            .read()
            .await
            .get(&fixture.task_id)
            .cloned()
            .expect("the task must still exist");

        // (4c) And what a successor execution would be given. This goes
        //      through `create_worktree`, the command every successor
        //      acquisition reaches -- not `WorktreeManager::reattach`
        //      underneath it. The distinction is the property: the manager is
        //      handed a branch and knows nothing about tasks, so it can only
        //      ever answer "git has this registered"; whether *this task* may
        //      be given that checkout right now is a lifecycle question, and
        //      asking it anywhere else would test a layer that cannot know the
        //      answer.
        let successor_checkout = tokio::time::timeout(
            DESIGN_MAY_WAIT,
            slashit_ui_lib::commands::worktree::create_worktree(
                app.state(),
                fixture.task_id.to_string(),
            ),
        )
        .await
        .unwrap_or_else(|_| Err("still waiting for the cleanup to settle".to_string()));

        // (5) Let the removal finish before anything is asserted, so a failing
        //     run cannot leave a git process parked while the harness unwinds.
        barrier
            .release
            .write_all(b"release\n")
            .await
            .expect("release the boundary");
        let removal_status = read_announcement(&mut barrier.finished).await;

        // (6) What the code actually does, recorded rather than assumed.
        let checkout_survived = Path::new(&fixture.worktree_path).exists();
        let work_survived = fixture.work_file.exists();

        // Every violation is collected before any of them is raised, so one
        // run shows the whole shape of the defect rather than only whichever
        // assertion happens to sit first.
        let mut violations: Vec<String> = Vec::new();

        if settled_terminal {
            violations.push(format!(
                "the terminal transition reported success while its own cleanup was still \
                 parked at `git worktree remove {path}` with nothing yet removed; it \
                 answered {outcome:?}",
                path = fixture.worktree_path,
                outcome = terminal_outcome
                    .as_ref()
                    .map(|r| r.as_ref().map(|t| t.as_ref().map(|t| t.status.clone()))),
            ));
        }

        if executor_would_start(&authoritative)
            && authoritative.worktree_path.as_deref() == Some(fixture.worktree_path.as_str())
        {
            violations.push(format!(
                "a successor execution state became authoritative while the terminal cleanup \
                 was parked inside `git worktree remove`: status={status:?} phase={phase:?} \
                 worktree_path={wt:?}, and the reactivation answered {reactivation:?}",
                status = authoritative.status,
                phase = authoritative.phase,
                wt = authoritative.worktree_path,
                reactivation = reactivation_outcome
                    .as_ref()
                    .map(|r| r.as_ref().map(|t| t.as_ref().map(|t| t.status.clone()))),
            ));
        }

        if successor_checkout.as_deref() == Ok(fixture.worktree_path.as_str()) {
            violations.push(format!(
                "a successor execution acquired the very checkout the parked cleanup was \
                 about to delete: the worktree command resolved to {successor:?}",
                successor = successor_checkout,
            ));
        }

        assert!(
            violations.is_empty(),
            "no active or executable successor state may be authoritative while a terminal \
             cleanup can still destructively act on the task's worktree.\n\
             Violations:\n - {joined}\n\
             After the boundary was released (git exited {removal_status}) the checkout at \
             {path} still exists: {checkout_survived}; the work committed inside it still \
             exists: {work_survived}.\n\
             This regression does not require a competing reactivation to be refused, to \
             wait, or to retry -- any of those satisfies it.",
            joined = violations.join("\n - "),
            path = fixture.worktree_path,
        );
    });
}

/// A terminal transition whose worktree cleanup did not happen must not be
/// reported, or recorded, as a task that is finished with its worktree.
///
/// This is not the manager-level "a dirty checkout survives cleanup" property
/// and does not restate it: nothing here is dirty, and nothing here asserts
/// what `WorktreeManager::remove` decides. It asserts what the *lifecycle*
/// does with the answer it gets. `remove` reports, through the ordinary
/// production path, that the checkout is still on disk -- and the board still
/// moves the card to `Done` and the command still answers success, so the
/// product now states that a task is finished with a worktree it demonstrably
/// failed to finish with. Manager-level safety cannot show that, because the
/// manager never sees the board.
#[test]
fn a_terminal_transition_whose_cleanup_did_not_happen_is_not_reported_as_done() {
    LazyLock::force(&ENVIRONMENT);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let (fixture, state) = rt.block_on(build_fixture("refusal"));
    let app = tauri::test::mock_app();
    app.manage(state);

    rt.block_on(async {
        let state: &AppState = app.state::<AppState>().inner();
        let mut barrier = Barrier::arm_refusing(&fixture.worktree_path).expect("arm the boundary");

        let before = state
            .task
            .tasks
            .read()
            .await
            .get(&fixture.task_id)
            .cloned()
            .expect("the task must exist before the transition");
        assert_eq!(
            before.status,
            TaskStatus::InProgress,
            "the fixture must start in a non-terminal state"
        );

        let terminal = slashit_ui_lib::commands::task::update_task_status(
            app.state(),
            fixture.task_id.to_string(),
            TaskStatus::Done,
        );
        tokio::pin!(terminal);
        let mut terminal_outcome: Option<Result<Option<Task>, String>> = None;

        // `remove_with_git` makes exactly one attempt: the ordinary removal,
        // never a forced one. Waiting for that single crossing is what makes
        // "the cleanup is over and it removed nothing" a fact rather than a
        // guess -- after it there is no git left to run. The count is itself an
        // assertion: the shim announces a forced removal as `force`, so a
        // destructive fallback reintroduced here would produce a second
        // crossing this loop never collects and the equality below never sees.
        let mut crossings: Vec<(String, String)> = Vec::new();
        for _ in 0..1 {
            let arrival_fut = read_announcement(&mut barrier.arrived);
            tokio::pin!(arrival_fut);
            let arrival = loop {
                tokio::select! {
                    outcome = &mut terminal, if terminal_outcome.is_none() => {
                        terminal_outcome = Some(outcome);
                    }
                    announced = &mut arrival_fut => break announced,
                }
            };
            barrier
                .release
                .write_all(b"release\n")
                .await
                .expect("release the boundary");
            let status = read_announcement(&mut barrier.finished).await;
            crossings.push((arrival, status));
        }
        assert_eq!(
            crossings,
            vec![("plain".to_string(), "111".to_string())],
            "the one removal attempt must have been refused for this test to mean anything"
        );

        if terminal_outcome.is_none() {
            terminal_outcome = tokio::time::timeout(DESIGN_MAY_WAIT, &mut terminal)
                .await
                .ok();
        }

        let authoritative = state
            .task
            .tasks
            .read()
            .await
            .get(&fixture.task_id)
            .cloned()
            .expect("the task must still exist");

        let mut violations: Vec<String> = Vec::new();

        if matches!(&terminal_outcome, Some(Ok(_))) {
            violations.push(format!(
                "the terminal transition answered success although its cleanup removed \
                 nothing: {outcome:?}",
                outcome = terminal_outcome
                    .as_ref()
                    .map(|r| r.as_ref().map(|t| t.as_ref().map(|t| t.status.clone()))),
            ));
        }
        if authoritative.status != before.status {
            violations.push(format!(
                "the board moved the task from {before:?} to {after:?} on a cleanup that \
                 did not happen",
                before = before.status,
                after = authoritative.status,
            ));
        }
        if authoritative.worktree_path.as_deref() != Some(fixture.worktree_path.as_str()) {
            violations.push(format!(
                "the only record of the surviving checkout was dropped: worktree_path is \
                 now {wt:?}",
                wt = authoritative.worktree_path,
            ));
        }
        if authoritative.branch_name.as_deref() != Some(fixture.branch.as_str()) {
            violations.push(format!(
                "the branch naming the task's commits was dropped: branch_name is now \
                 {branch:?}",
                branch = authoritative.branch_name,
            ));
        }
        if !fixture.work_file.exists() {
            violations.push(
                "the work inside the checkout is gone even though no removal ran".to_string(),
            );
        }

        assert!(
            violations.is_empty(),
            "a terminal transition whose cleanup did not happen must leave the task where it \
             was and must say so.\nViolations:\n - {joined}\nThe checkout at {path} is still \
             on disk, holding the work it held before the transition was requested.",
            joined = violations.join("\n - "),
            path = fixture.worktree_path,
        );
    });
}

/// A card dragged onto `Done` lands at the position it was dropped at, computed
/// against the board as it is when the move is committed rather than as it was
/// before the cleanup ran.
///
/// The two halves of a terminal drag are separated by a subprocess. Positions
/// worked out before it describe a column that may have changed by the time the
/// removal finishes -- another card dropped into `Done` meanwhile is the
/// ordinary way -- and committing them would publish the card at a position
/// that was right a moment ago and renumber its new neighbours from a stale
/// reading. So this asserts the whole `Done` column, not just the moved card:
/// a correct implementation leaves it gapless and in the order the user sees.
#[test]
fn a_terminal_drag_commits_column_positions_computed_after_its_cleanup() {
    LazyLock::force(&ENVIRONMENT);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let (fixture, state) = rt.block_on(build_fixture("reorder"));
    let app = tauri::test::mock_app();
    app.manage(state);

    rt.block_on(async {
        let state: &AppState = app.state::<AppState>().inner();
        let project_id = state
            .task
            .tasks
            .read()
            .await
            .get(&fixture.task_id)
            .expect("the fixture task")
            .project_id;

        // Two cards already finished, so the destination column is not empty
        // and an off-by-one is visible rather than absorbed.
        let mut settled = Vec::new();
        for (index, name) in ["already done A", "already done B"].iter().enumerate() {
            let mut task =
                create_test_task_full(name, project_id, TaskStatus::Done, index as i32);
            task.phase = TaskPhase::Complete;
            settled.push(task.id);
            state.task.tasks.write().await.insert(task.id, task);
        }

        // Dropped between them. The cleanup is real and succeeds -- the
        // fixture's checkout is clean -- so this is the ordinary terminal drag,
        // with a `git worktree remove` between the drop and the commit.
        let moved = slashit_ui_lib::commands::task::reorder_task(
            app.state(),
            fixture.task_id.to_string(),
            Some(TaskStatus::Done),
            1,
        )
        .await
        .expect("a clean checkout must be removable")
        .expect("the task must still exist");

        assert_eq!(moved.status, TaskStatus::Done);
        assert_eq!(moved.worktree_path, None, "the terminal move cleaned up first");
        assert_eq!(moved.position, 1, "the card lands where it was dropped");
        assert!(
            !Path::new(&fixture.worktree_path).exists(),
            "and the checkout it was finished with is gone"
        );

        let tasks = state.task.tasks.read().await;
        let mut column: Vec<(i32, String)> = tasks
            .values()
            .filter(|t| t.project_id == project_id && t.status == TaskStatus::Done)
            .map(|t| (t.position, t.title.clone()))
            .collect();
        column.sort();
        assert_eq!(
            column,
            vec![
                (0, "already done A".to_string()),
                (1, "reorder".to_string()),
                (2, "already done B".to_string()),
            ],
            "the whole column must be gapless and in the order the drop implies"
        );

        // The same column on disk, because that is the one a restart reads.
        let persisted = state
            .storage
            .load_project_tasks(project_id)
            .expect("the board must be readable");
        let mut persisted_column: Vec<(i32, String)> = persisted
            .iter()
            .filter(|t| t.status == TaskStatus::Done)
            .map(|t| (t.position, t.title.clone()))
            .collect();
        persisted_column.sort();
        assert_eq!(persisted_column, column, "memory and disk must agree");

        drop(tasks);
        assert_eq!(settled.len(), 2);
    });
}

/// A drag classified before the lease was held is a drag classified against the
/// past, and `Done` reached the board without any cleanup because of it.
///
/// The window is real and narrow. `reorder_task` used to read the task's status
/// first and decide from that reading whether the move was terminal, then wait
/// for the task's lifecycle lease. Whatever held that lease could finish and
/// leave the task in a different column, and the decision was never revisited:
/// a move that looked like an ordinary reposition within `Done` became a move
/// *into* `Done`, took the ordinary path, and assigned the status itself. The
/// worktree was never touched and the board said the task was finished with it.
///
/// This drives that exact interleaving through the real command. The lease is
/// held by this test, standing in for any lifecycle operation; the status
/// changes underneath while the drag waits for it. It does not assert which
/// answer the command gives -- refusing the move is as good as performing it --
/// only that a committed `Done` is a `Done` whose cleanup actually happened.
#[test]
fn a_drag_that_waited_for_the_lease_is_classified_against_the_task_it_finds() {
    LazyLock::force(&ENVIRONMENT);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let (fixture, state) = rt.block_on(build_fixture("late-classification"));
    let app = tauri::test::mock_app();
    app.manage(state);

    rt.block_on(async {
        let state: &AppState = app.state::<AppState>().inner();

        // The precondition the defect needs: a task already in `Done` that
        // still holds a checkout. Reachable in production from a record written
        // by an older build, and from attaching a worktree to a finished task.
        {
            let mut tasks = state.task.tasks.write().await;
            let task = tasks.get_mut(&fixture.task_id).expect("the fixture task");
            task.status = TaskStatus::Done;
        }

        let lease = state
            .task_lifecycle_locks
            .acquire(fixture.task_id)
            .await
            .expect("the lease must be free at the start");

        // Dropped onto `Done`, from `Done`. Whatever the command samples now is
        // what it must not still be acting on after it has waited.
        let drag = slashit_ui_lib::commands::task::reorder_task(
            app.state(),
            fixture.task_id.to_string(),
            Some(TaskStatus::Done),
            0,
        );
        tokio::pin!(drag);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut drag)
                .await
                .is_err(),
            "the drag must be waiting for the lease this test is holding"
        );

        // The task leaves `Done` while the drag waits, which is what makes the
        // drag's own reading stale.
        {
            let mut tasks = state.task.tasks.write().await;
            let task = tasks.get_mut(&fixture.task_id).expect("the fixture task");
            task.status = TaskStatus::InProgress;
            task.phase = TaskPhase::Coding;
        }
        drop(lease);

        let outcome = tokio::time::timeout(DESIGN_MAY_WAIT, &mut drag)
            .await
            .expect("the drag must finish once the lease is free");

        let authoritative = state
            .task
            .tasks
            .read()
            .await
            .get(&fixture.task_id)
            .cloned()
            .expect("the task must still exist");

        if authoritative.status == TaskStatus::Done {
            assert_eq!(
                authoritative.worktree_path, None,
                "the board says this task is finished with its worktree while still \
                 recording one; the move was classified before the lease was held, so it \
                 wrote `Done` itself instead of going through terminalization. It answered \
                 {outcome:?}"
            );
            assert!(
                !Path::new(&fixture.worktree_path).exists(),
                "`Done` was committed but the checkout at {} is still on disk, so no \
                 cleanup ran",
                fixture.worktree_path
            );
        } else {
            // Refusing or declining to move is a perfectly good answer; the
            // checkout must simply still be intact.
            assert!(
                Path::new(&fixture.worktree_path).exists(),
                "the move did not reach `Done`, so nothing may have removed the checkout"
            );
        }
    });
}
