//! A stand-in for the `claude` executable, so a journey can exercise the real
//! agent lifecycle without a model, a network or a credential.
//!
//! The product starts its agent with
//! `Command::new("claude")`, which resolves through `PATH`. That is the entire
//! seam this module uses: it writes an executable named `claude` into a
//! directory the test owns and puts that directory first on the `PATH` the
//! application process tree inherits. Nothing inside SlashIt is replaced,
//! subclassed or configured differently — the only faked thing is the external
//! program on the other side of a process boundary.
//!
//! The fixture records every invocation, which is what lets a journey prove
//! that *this* executable ran rather than inferring it from a task reaching a
//! successful state. A task can be forced into any state; a marker file can
//! only be written by the process that was actually started.

use anyhow::{bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The name the product looks up on `PATH`.
const EXECUTABLE: &str = "claude";

/// The variable the script writes its invocation records into. Set on the
/// application process tree alongside `PATH`, and inherited by the agent
/// because the runner does not clear the environment it passes on.
pub const MARKER_DIR_VAR: &str = "SLASHIT_FAKE_AGENT_MARKERS";

/// The model the fixture reports in its `system` line.
///
/// The runner copies this onto the task, so a journey can read it back through
/// the product's own API as second-hand evidence that the real stream parser
/// consumed this executable's real output.
pub const REPORTED_MODEL: &str = "slashit-fake-agent";

/// The text the fixture returns as its result.
pub const REPORTED_RESULT: &str = "fake agent completed without changing the repository";

/// The variable that scripts how many of the leading agent runs fail.
///
/// Unset, or zero, means every run succeeds, which is what every journey that
/// does not mention this variable depends on. Set to `1`, the first agent run
/// reports failure and every run after it succeeds — enough to drive a
/// failure, a retry and a recovery without the fixture ever being told which
/// of those is happening.
///
/// Only *agent runs* are counted. `claude --version` is the product asking
/// what is installed, a real CLI would answer it the same way whatever state
/// it is in, and a probe that consumed the scripted failure would make the
/// journey depend on whether the frontend happened to ask first.
pub const FAILING_RUNS_VAR: &str = "SLASHIT_FAKE_AGENT_FAILING_RUNS";

/// The text the fixture returns for a run scripted to fail.
///
/// The runner copies the result text of a failed run onto the task as its
/// error message, so a journey can read this back through the product's own
/// API and know the failure it sees is the one this executable reported.
pub const REPORTED_FAILURE: &str = "fake agent was scripted to fail this run";

/// The variable that makes an agent run leave a file behind in the working
/// directory it was started in.
///
/// Its value is the file name; the contents are always [`WORK_CONTENT`], so a
/// journey can recognise the work by reading it rather than by trusting that
/// something was written. Unset -- which is every journey that does not ask
/// for it -- the fixture changes nothing about the repository, exactly as it
/// did before this existed.
///
/// Only *agent runs* write. `claude --version` is the product asking what is
/// installed, and a probe that dropped a file into whatever directory it was
/// called from would put work in places no task owns.
///
/// A destructive journey needs this: proving that removing a worktree loses
/// the work in it is only worth anything if there was work in it. The
/// executor commits whatever the run produced, so a file written here becomes
/// a real commit on the task's real branch.
pub const WRITE_FILE_VAR: &str = "SLASHIT_FAKE_AGENT_WRITE_FILE";

/// What a run writes when [`WRITE_FILE_VAR`] asks it to.
///
/// Fixed rather than generated: a journey that finds this text has found the
/// output of this executable and not of anything else in the run.
pub const WORK_CONTENT: &str = "work produced by the fake agent\n";

/// The variable that holds an agent run open instead of letting it finish.
///
/// Its value is the directory [`FakeAgent::block_agent_runs`] created. Unset —
/// which is every journey that does not ask for it — the fixture behaves
/// exactly as it did before this existed, and neither that directory nor the
/// bookkeeping one below is created.
///
/// A run that blocks creates a release pipe of its own in that directory,
/// opens it, and only then announces its process id; it removes both once its
/// release arrives. Waiting on a pipe rather than sleeping is what makes a
/// cancellation journey deterministic: the run ends because the product
/// terminated it or because the test released it, never because an interval
/// happened to elapse. Opening before announcing is what makes a release
/// deliverable: an announced run is already readable, so a release aimed at it
/// cannot be refused for want of a reader.
///
/// Only *agent runs* block. `claude --version` is the product asking what is
/// installed, and a probe that hung would stall the frontend rather than the
/// execution the journey is about.
pub const BLOCK_DIR_VAR: &str = "SLASHIT_FAKE_AGENT_BLOCK_DIR";

/// The fixture script: a file that already exists, rather than one written
/// here and then run.
///
/// `execve` refuses a file that any process has open for writing, and a
/// process that has forked but has not yet reached its own `exec` still holds
/// a copy of every descriptor its parent had open at the moment of the fork.
/// A test that writes its own executable can therefore have that write carried
/// past the point where it runs the file, by an entirely unrelated concurrent
/// spawn, and get `ETXTBSY`. A file nobody ever opens for writing cannot be
/// caught that way, so the script is checked in and only ever linked to.
///
/// It is `/bin/sh` because the harness may not assume any language runtime
/// beyond a POSIX shell, and because the whole point is that this is an
/// ordinary external program. The values it reports are the constants above,
/// written out, and a test in this module asserts that they still are.
fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(EXECUTABLE)
}

/// One recorded run of the fixture.
pub struct Invocation {
    /// The directory the product started the agent in.
    pub working_dir: PathBuf,
    /// Everything after the program name, in order.
    pub args: Vec<OsString>,
}

impl Invocation {
    /// The value the product passed for `flag`, if it passed one.
    pub fn flag(&self, flag: &str) -> Option<&OsStr> {
        let position = self.args.iter().position(|arg| arg == flag)?;
        self.args.get(position + 1).map(OsString::as_os_str)
    }

    /// Whether `flag` appears at all.
    pub fn has_flag(&self, flag: &str) -> bool {
        self.args.iter().any(|arg| arg == flag)
    }
}

/// An installed fake agent: the directory holding it, and where it records.
pub struct FakeAgent {
    executable: PathBuf,
    marker_dir: PathBuf,
    path_value: OsString,
}

impl FakeAgent {
    /// Install the fixture under `root`, and compose the `PATH` the
    /// application should run with.
    ///
    /// `root` is expected to be inside the test's own state root, so removing
    /// that root removes this too.
    pub fn install(root: &Path) -> Result<Self> {
        let bin_dir = root.join("fake-bin");
        let marker_dir = root.join("agent-invocations");
        for dir in [&bin_dir, &marker_dir] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }

        use std::os::unix::fs::PermissionsExt;

        let script = script_path();
        let mode = std::fs::metadata(&script)
            .with_context(|| format!("could not read the fixture script {}", script.display()))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            bail!(
                "the fixture script {} is not executable — the checkout lost its mode bits",
                script.display()
            );
        }

        let executable = bin_dir.join(EXECUTABLE);
        std::os::unix::fs::symlink(&script, &executable).with_context(|| {
            format!(
                "could not point {} at {}",
                executable.display(),
                script.display()
            )
        })?;

        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let mut path_value = bin_dir.clone().into_os_string();
        if !inherited.is_empty() {
            path_value.push(":");
            path_value.push(&inherited);
        }

        let agent = Self {
            executable,
            marker_dir,
            path_value,
        };
        agent.assert_it_shadows_any_real_agent()?;
        Ok(agent)
    }

    /// The `PATH` to give the application: this fixture first, then whatever
    /// the test process inherited, because the product still needs `git`.
    pub fn path_value(&self) -> &OsStr {
        &self.path_value
    }

    pub fn marker_dir(&self) -> &Path {
        &self.marker_dir
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Resolve `claude` the way the product will, and refuse to continue
    /// unless the answer is this fixture.
    ///
    /// The check is cheap and the failure it prevents is not: a `PATH` built
    /// wrongly would silently hand the journey the developer's real Claude
    /// Code, with `--dangerously-skip-permissions`, pointed at a scratch
    /// repository. This runs before the application is ever started.
    fn assert_it_shadows_any_real_agent(&self) -> Result<()> {
        match resolve_on_path(EXECUTABLE, &self.path_value) {
            Some(found) if found == self.executable => Ok(()),
            Some(found) => bail!(
                "the composed PATH resolves {EXECUTABLE} to {} instead of the fixture at {} — \
                 refusing to run a journey that would start a real agent",
                found.display(),
                self.executable.display()
            ),
            None => bail!(
                "the composed PATH resolves no {EXECUTABLE} at all, so the fixture at {} would \
                 never be found",
                self.executable.display()
            ),
        }
    }

    /// Every completed invocation, ordered by file name.
    ///
    /// Only finished records are visible: the fixture stages each one in a
    /// subdirectory and renames it in, and the `is_file` filter below skips
    /// that subdirectory. Ordering is by name, which `mktemp` makes unique but
    /// not monotonic; no journey so far needs more than "how many, and what
    /// was in them".
    pub fn invocations(&self) -> Result<Vec<Invocation>> {
        let mut names: Vec<PathBuf> = std::fs::read_dir(&self.marker_dir)
            .with_context(|| format!("could not read {}", self.marker_dir.display()))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        names.sort();

        names.iter().map(|path| read_invocation(path)).collect()
    }

    /// How many times the fixture has run.
    pub fn invocation_count(&self) -> Result<usize> {
        Ok(self.invocations()?.len())
    }

    /// Hold every agent run open until something releases it.
    ///
    /// Returns the value for [`BLOCK_DIR_VAR`], which the caller sets on the
    /// application's environment. The directory is created here rather than by
    /// the fixture so that it exists before the application does; the pipes
    /// inside it belong to the runs, one each, and are created by the run that
    /// waits on one.
    pub fn block_agent_runs(&self) -> Result<PathBuf> {
        let dir = self.release_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
        Ok(dir)
    }

    /// The process ids of runs that announced themselves and then blocked.
    ///
    /// Empty unless a journey asked for [`BLOCK_DIR_VAR`]. A run withdraws its
    /// own announcement as its release arrives, so these are the runs blocked
    /// now rather than the runs that ever blocked — except for runs something
    /// else ended while they waited, which withdraw nothing and stay listed.
    pub fn blocked_pids(&self) -> Result<Vec<u32>> {
        let mut pids: Vec<u32> = self
            .blocked_runs()?
            .into_iter()
            .map(|(_, pid)| pid)
            .collect();
        pids.sort_unstable();
        Ok(pids)
    }

    /// Every announced run: the name it announced itself under, and its pid.
    ///
    /// The name is also the name of that run's own release pipe, which is what
    /// lets a release be addressed to one run rather than offered to all of
    /// them. They are `mktemp` names — unique but not monotonic — so a caller
    /// telling one run from another must compare sets, never positions.
    fn blocked_runs(&self) -> Result<Vec<(OsString, u32)>> {
        let dir = self.process_dir();
        if !dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut runs = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("could not read {}", dir.display()))?
            .flatten()
        {
            let path = entry.path();
            // The staging subdirectory a half-written record lives in.
            if !path.is_file() {
                continue;
            }
            let recorded = std::fs::read_to_string(&path)
                .with_context(|| format!("could not read the pid record {}", path.display()))?;
            let pid = recorded.trim().parse::<u32>().with_context(|| {
                format!(
                    "the pid record {} does not hold a process id: {recorded:?}",
                    path.display()
                )
            })?;
            runs.push((entry.file_name(), pid));
        }
        Ok(runs)
    }

    /// Whether `pid` is still a running process of this fixture.
    ///
    /// Two things a bare `kill(pid, 0)` gets wrong here, and both of them would
    /// report a terminated agent as alive: a process that has exited but has
    /// not been reaped is still a process, and a process id the system has
    /// since handed to something else is still a process. So the state and the
    /// command line are read from `/proc`, and only a live process whose
    /// command line names this fixture counts as this fixture running.
    pub fn is_running(&self, pid: u32) -> bool {
        use std::os::unix::ffi::OsStrExt;

        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false; // exited and reaped, or never existed
        };
        // The command field is parenthesised and may itself contain spaces and
        // parentheses, so the state is the first field after the last `)`.
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next());
        if state == Some("Z") {
            return false; // exited, still waiting to be reaped
        }

        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            return false;
        };
        cmdline
            .split(|byte| *byte == 0)
            .any(|argument| Path::new(OsStr::from_bytes(argument)) == self.executable)
    }

    /// Let every currently blocked run finish, and say how many took a release.
    ///
    /// One release per announced run, written to that run's own pipe. A
    /// release written to a pipe shared by every run would carry no address:
    /// it would be taken by whichever waiting run the kernel handed it to,
    /// which is not necessarily the one whose state justified sending it.
    ///
    /// The pipe is opened non-blocking on purpose, and the fixture earns that
    /// by opening its own pipe before it announces itself: an announced run is
    /// a reader by construction, so a release aimed at one cannot be refused
    /// for want of a reader. A refusal therefore means that run has stopped
    /// waiting in between — the product terminating it mid-release is the
    /// ordinary way that happens — and it is counted as what it is, a run that
    /// took no release. Any other failure is a broken fixture rather than a
    /// race to swallow.
    pub fn release_blocked_runs(&self) -> Result<usize> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut released = 0;
        for (name, _) in self.blocked_runs()? {
            let pipe = self.release_dir().join(&name);
            let delivered = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&pipe)
                .and_then(|mut writer| writer.write_all(b"release\n"));
            match delivered {
                Ok(()) => released += 1,
                // That run stopped waiting: `ENXIO` because nothing has the
                // pipe open to read from any more, `ENOENT` because the run
                // took the pipe away with it, `EPIPE` because it stopped
                // reading between the open and the write.
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::ENXIO | libc::ENOENT | libc::EPIPE)
                    ) => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("could not release a run through {}", pipe.display())
                    })
                }
            }
        }
        Ok(released)
    }

    /// Where a blocked run puts the pipe it waits on.
    ///
    /// Inside the marker directory, so the harness's existing cleanup removes
    /// it, and hidden from [`invocations`](Self::invocations) by the same
    /// `is_file` filter that hides the staging directory: a directory is not a
    /// regular file, and neither is a pipe.
    fn release_dir(&self) -> PathBuf {
        self.marker_dir.join(".release")
    }

    /// Where a blocking run announces its process id.
    fn process_dir(&self) -> PathBuf {
        self.marker_dir.join(".processes")
    }
}

/// Parse one NUL-separated record: the working directory, then the arguments.
fn read_invocation(path: &Path) -> Result<Invocation> {
    use std::os::unix::ffi::OsStringExt;

    let raw = std::fs::read(path)
        .with_context(|| format!("could not read the invocation record {}", path.display()))?;

    // `printf '%s\0'` terminates every field, so the split leaves one trailing
    // empty piece that is not a field.
    let mut fields: Vec<OsString> = raw
        .split(|byte| *byte == 0)
        .map(|field| OsString::from_vec(field.to_vec()))
        .collect();
    if fields.last().is_some_and(|last| last.is_empty()) {
        fields.pop();
    }
    if fields.is_empty() {
        bail!(
            "the invocation record {} is empty — the fixture was interrupted before it wrote \
             anything",
            path.display()
        );
    }

    let working_dir = PathBuf::from(fields.remove(0));
    Ok(Invocation {
        working_dir,
        args: fields,
    })
}

/// The first executable named `name` in `path`, using the same left-to-right
/// rule the operating system applies.
fn resolve_on_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            candidate.is_file()
                && std::fs::metadata(candidate)
                    .is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "slashit-fake-agent-{}-{}-{label}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    /// The script is a file of its own now, so this is what keeps the values
    /// it reports and the values this module names from drifting apart.
    #[test]
    fn the_fixture_reports_what_this_module_says_it_reports() {
        let script = std::fs::read_to_string(script_path()).expect("read the fixture script");
        for expected in [
            REPORTED_MODEL,
            REPORTED_RESULT,
            REPORTED_FAILURE,
            WORK_CONTENT,
            MARKER_DIR_VAR,
            FAILING_RUNS_VAR,
            WRITE_FILE_VAR,
            BLOCK_DIR_VAR,
        ] {
            assert!(
                script.contains(expected),
                "the fixture script never mentions {expected:?}"
            );
        }
    }

    /// The fixture has to behave like the program it stands in for, so the
    /// test runs it the way the product does and reads what the product would
    /// read.
    #[test]
    fn the_fixture_records_its_arguments_and_emits_a_successful_result() {
        let root = scratch("runs");
        let agent = FakeAgent::install(&root).expect("install");

        let output = std::process::Command::new(agent.executable())
            .args([
                "-p",
                "a prompt\nwith a newline in it",
                "--verbose",
                "--output-format",
                "stream-json",
                "--session-id",
                "session-under-test",
            ])
            .current_dir(&root)
            .env(MARKER_DIR_VAR, agent.marker_dir())
            .output()
            .expect("run the fixture");

        assert!(
            output.status.success(),
            "the fixture must exit zero, got {:?} with stderr {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );

        // What the runner's stream parser looks for.
        let stdout = String::from_utf8(output.stdout).expect("the fixture emits UTF-8");
        let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            2,
            "expected a system and a result line: {stdout}"
        );
        for line in &lines {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("the fixture emitted invalid JSON ({e}): {line}"));
        }
        let result: serde_json::Value =
            serde_json::from_str(lines[1]).expect("the result line parses");
        assert_eq!(result["type"], "result");
        assert_eq!(result["is_error"], false);
        assert_eq!(result["session_id"], "session-under-test");

        // And the record it left behind.
        let invocations = agent.invocations().expect("read the records");
        assert_eq!(invocations.len(), 1, "one run should record once");
        let invocation = &invocations[0];
        // Canonicalised on both sides: the shell reports its own view of the
        // directory, and a symlinked temp directory would otherwise fail here
        // for a reason that has nothing to do with the fixture.
        let recorded = std::fs::canonicalize(&invocation.working_dir).expect("canonicalise");
        let expected = std::fs::canonicalize(&root).expect("canonicalise");
        assert_eq!(recorded, expected);
        assert_eq!(
            invocation.flag("--session-id"),
            Some(OsStr::new("session-under-test"))
        );
        assert!(invocation.has_flag("--verbose"));
        // The prompt survives the round trip intact, newline and all.
        assert_eq!(
            invocation.flag("-p"),
            Some(OsStr::new("a prompt\nwith a newline in it"))
        );

        std::fs::remove_dir_all(&root).expect("clean up");
    }

    /// Two runs must be two records, or "exactly one invocation" could never
    /// be asserted.
    #[test]
    fn each_run_records_separately() {
        let root = scratch("counts");
        let agent = FakeAgent::install(&root).expect("install");

        for _ in 0..3 {
            // `output`, not `status`: the fixture writes its protocol to
            // stdout, and inheriting the test harness's would scatter JSON
            // through the test report.
            let run = std::process::Command::new(agent.executable())
                .arg("--session-id")
                .arg("s")
                .env(MARKER_DIR_VAR, agent.marker_dir())
                .output()
                .expect("run the fixture");
            assert!(run.status.success());
        }

        assert_eq!(agent.invocation_count().expect("count"), 3);

        std::fs::remove_dir_all(&root).expect("clean up");
    }

    /// A scripted failure has to land on the run the journey means, and only
    /// on that one. The two ways it could go wrong are symmetrical: a version
    /// probe absorbing the failure would make the retry journey pass without a
    /// failure ever happening, and a failure that repeats would make the retry
    /// look broken when it is not.
    #[test]
    fn a_scripted_failure_lands_on_the_first_agent_run_only() {
        let root = scratch("scripted-failure");
        let agent = FakeAgent::install(&root).expect("install");

        let run = |args: &[&str]| {
            let output = std::process::Command::new(agent.executable())
                .args(args)
                .env(MARKER_DIR_VAR, agent.marker_dir())
                .env(FAILING_RUNS_VAR, "1")
                .output()
                .expect("run the fixture");
            assert!(
                output.status.success(),
                "a scripted failure is still a zero exit: {:?}",
                output.status.code()
            );
            let stdout = String::from_utf8(output.stdout).expect("the fixture emits UTF-8");
            let last = stdout
                .lines()
                .rfind(|l| !l.trim().is_empty())
                .expect("a result line")
                .to_string();
            serde_json::from_str::<serde_json::Value>(&last).expect("the result line parses")
        };

        // The product asking what is installed is not an agent run, so it must
        // not consume the scripted failure.
        let probe = run(&["--version"]);
        assert_eq!(probe["is_error"], false, "a version probe never fails");

        let first = run(&["-p", "do the work", "--session-id", "one"]);
        assert_eq!(
            first["is_error"], true,
            "the first agent run is scripted to fail"
        );
        assert_eq!(first["result"], REPORTED_FAILURE);

        let second = run(&["-p", "do the work", "--session-id", "two"]);
        assert_eq!(
            second["is_error"], false,
            "only the first run was scripted to fail"
        );
        assert_eq!(second["result"], REPORTED_RESULT);

        // The bookkeeping the fixture keeps for itself is not a record: it is a
        // directory, and the records are files.
        assert_eq!(agent.invocation_count().expect("count"), 3);

        std::fs::remove_dir_all(&root).expect("clean up");
    }

    /// A fixture process the test owns, ended whatever becomes of the test.
    ///
    /// A failed assertion unwinds straight past the `wait` that would have
    /// collected the run, and a dropped [`std::process::Child`] is neither
    /// killed nor reaped, so a panicking test would otherwise hand a still
    /// blocked run to whatever cleans up after the job. That is not a way of
    /// passing: every assertion below still fails exactly when it did.
    struct OwnedRun(std::process::Child);

    impl std::ops::Deref for OwnedRun {
        type Target = std::process::Child;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl std::ops::DerefMut for OwnedRun {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    impl Drop for OwnedRun {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }

    /// Start one agent run that will block until it is released.
    fn blocking_run(agent: &FakeAgent, releases: &Path, session: &str) -> OwnedRun {
        OwnedRun(
            std::process::Command::new(agent.executable())
                .args(["-p", "do the work", "--session-id", session])
                .env(MARKER_DIR_VAR, agent.marker_dir())
                .env(BLOCK_DIR_VAR, releases)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("start the fixture"),
        )
    }

    /// Wait for `condition`, or give up and say what was still true.
    fn until(condition: impl Fn() -> bool, complaint: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !condition() {
            assert!(std::time::Instant::now() < deadline, "{complaint}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Blocking mode is what a cancellation journey stands on, so it has to
    /// hold up on its own: the run must still be there to be stopped, it must
    /// have named the process that a test can then watch, and that process must
    /// be the one the caller started rather than some descendant of it.
    #[test]
    fn a_blocking_run_announces_its_own_process_and_waits_to_be_released() {
        let root = scratch("blocking");
        let agent = FakeAgent::install(&root).expect("install");
        let releases = agent
            .block_agent_runs()
            .expect("create the release directory");

        let mut child = blocking_run(&agent, &releases, "blocked");

        until(
            || agent.blocked_pids().expect("read the pid records").len() == 1,
            "the run never announced itself as blocked",
        );

        // The announced process is the one the caller started, not a child of
        // it. Everything a journey concludes about termination depends on this
        // being the process the product itself holds.
        let pids = agent.blocked_pids().expect("read the pid records");
        assert_eq!(pids, vec![child.id()]);
        let pid = pids[0];
        assert!(agent.is_running(pid), "the announced process must be alive");

        // Still there to be stopped: blocking is the whole point.
        assert!(
            child.try_wait().expect("poll the child").is_none(),
            "the run finished instead of blocking"
        );

        assert_eq!(
            agent.release_blocked_runs().expect("release"),
            1,
            "one blocked run should take one release"
        );
        let status = child.wait().expect("wait for the released run");
        assert!(status.success(), "a released run still exits zero");
        assert!(
            !agent.is_running(pid),
            "the process must be gone once it has exited and been reaped"
        );

        // And it recorded itself the way every other run does.
        assert_eq!(agent.invocation_count().expect("count"), 1);

        std::fs::remove_dir_all(&root).expect("clean up");
    }

    /// The contract the whole release protocol stands on: a run that has
    /// announced itself is *already* able to take a release.
    ///
    /// Announcing and becoming readable are two different events, and while
    /// they were ordered the other way round there was a window in which a
    /// test could see a blocked run and still be refused when it tried to
    /// release it — a non-blocking open of a pipe with no reader fails with
    /// `ENXIO` outright rather than waiting for one. So this races the fixture
    /// on purpose, with none of `until`'s polite pause between seeing the pid
    /// and acting on it. Under the previous ordering that window was not
    /// merely likely but reliable: reacting this promptly lost every attempt.
    ///
    /// The ordering is now a property of the fixture's program text rather
    /// than of scheduling, so one attempt already settles it; the repetition
    /// is what would catch a later change that made it depend on timing
    /// again.
    #[test]
    fn a_run_is_ready_to_be_released_by_the_time_it_announces_itself() {
        let root = scratch("blocking-readiness");
        let agent = FakeAgent::install(&root).expect("install");

        // Enough attempts that an ordering which only usually wins would be
        // found out, few enough that this stays a fraction of a second.
        for attempt in 1..=25 {
            // Each attempt is its own experiment. A record outlives the run
            // something else ended, so a leftover would have the next attempt
            // counting the previous one's process.
            std::fs::remove_dir_all(agent.process_dir()).ok();
            std::fs::remove_dir_all(agent.release_dir()).ok();
            let releases = agent
                .block_agent_runs()
                .expect("create the release directory");
            let mut run = blocking_run(&agent, &releases, "ready");

            let pid = loop {
                if let [pid] = agent.blocked_pids().expect("read the pid records")[..] {
                    break pid;
                }
            };

            assert_eq!(
                agent.release_blocked_runs().expect("release"),
                1,
                "attempt {attempt}: the announced run would not take its release"
            );
            assert!(
                run.wait().expect("wait for the released run").success(),
                "attempt {attempt}: a released run still exits zero"
            );
            assert!(
                !agent.is_running(pid),
                "attempt {attempt}: the released run must be gone"
            );
        }

        std::fs::remove_dir_all(&root).expect("clean up");
    }

    /// Several runs blocked at once is what a concurrency journey will ask
    /// for, so a release has to be per run rather than per pipe: three blocked
    /// runs take three releases, and all three of them end.
    #[test]
    fn every_blocked_run_takes_its_own_release() {
        let root = scratch("blocking-several");
        let agent = FakeAgent::install(&root).expect("install");
        let releases = agent
            .block_agent_runs()
            .expect("create the release directory");

        let mut runs: Vec<OwnedRun> = (1..=3)
            .map(|ordinal| blocking_run(&agent, &releases, &format!("blocked-{ordinal}")))
            .collect();
        let mut started: Vec<u32> = runs.iter().map(|run| run.id()).collect();
        started.sort_unstable();

        until(
            || agent.blocked_pids().expect("read the pid records").len() == 3,
            "three runs never announced themselves as blocked",
        );
        assert_eq!(
            agent.blocked_pids().expect("read the pid records"),
            started,
            "the announced processes are the three that were started"
        );

        assert_eq!(
            agent.release_blocked_runs().expect("release"),
            3,
            "three blocked runs should take three releases"
        );

        for run in &mut runs {
            assert!(
                run.wait().expect("wait for the released run").success(),
                "every released run still exits zero"
            );
        }
        for pid in started {
            assert!(
                !agent.is_running(pid),
                "no released run may outlive the test"
            );
        }

        // And releasing again hands out nothing. The three records are still
        // there, so a release counted per record rather than per waiting run
        // would claim three more and leave them in the pipe for whatever
        // blocked on it next.
        assert_eq!(
            agent.release_blocked_runs().expect("release"),
            0,
            "a run that has taken its release does not take another"
        );
        assert_eq!(agent.invocation_count().expect("count"), 3);

        std::fs::remove_dir_all(&root).expect("clean up");
    }

    /// A release belongs to a run that is still waiting for one. The records
    /// outlive the processes they name, so counting records rather than live
    /// runs would report a release that nobody was there to take — and would
    /// let a journey conclude it had freed a run the product had already
    /// killed.
    #[test]
    fn a_run_that_has_already_ended_does_not_take_a_release() {
        let root = scratch("blocking-ended");
        let agent = FakeAgent::install(&root).expect("install");
        let releases = agent
            .block_agent_runs()
            .expect("create the release directory");

        let mut ended = blocking_run(&agent, &releases, "ended");
        let mut waiting = blocking_run(&agent, &releases, "waiting");
        let ended_pid = ended.id();
        let waiting_pid = waiting.id();

        until(
            || agent.blocked_pids().expect("read the pid records").len() == 2,
            "both runs never announced themselves as blocked",
        );

        // Killed and collected, the way the product ends a run it is stopping:
        // the pid record it left behind stays exactly where it was.
        ended.kill().expect("end one of the blocked runs");
        ended.wait().expect("collect the ended run");
        assert!(!agent.is_running(ended_pid));
        assert_eq!(
            agent.blocked_pids().expect("read the pid records").len(),
            2,
            "a record outlives the run it names"
        );

        assert_eq!(
            agent.release_blocked_runs().expect("release"),
            1,
            "only the run still waiting should take a release"
        );
        assert!(
            waiting.wait().expect("wait for the released run").success(),
            "the run that was still waiting is the one that was released"
        );
        assert!(!agent.is_running(waiting_pid));

        std::fs::remove_dir_all(&root).expect("clean up");
    }

    /// The product asks what is installed through the same executable. A probe
    /// that blocked would stall the frontend rather than the execution a
    /// cancellation journey is about, and would leave a pid record that the
    /// journey would then have to tell apart from a real run.
    #[test]
    fn a_version_probe_does_not_block_when_agent_runs_do() {
        let root = scratch("blocking-probe");
        let agent = FakeAgent::install(&root).expect("install");
        let releases = agent
            .block_agent_runs()
            .expect("create the release directory");

        let probe = std::process::Command::new(agent.executable())
            .arg("--version")
            .env(MARKER_DIR_VAR, agent.marker_dir())
            .env(BLOCK_DIR_VAR, &releases)
            .output()
            .expect("run the probe");

        assert!(probe.status.success());
        assert!(
            agent
                .blocked_pids()
                .expect("read the pid records")
                .is_empty(),
            "a probe must not announce itself as a blocked run"
        );
        assert_eq!(
            agent.release_blocked_runs().expect("release"),
            0,
            "there was nothing to release"
        );

        std::fs::remove_dir_all(&root).expect("clean up");
    }

    /// Nothing blocks unless a journey asks for it, so the journeys that
    /// existed before this did are unaffected by it.
    #[test]
    fn a_run_without_the_pipe_finishes_as_it_always_did() {
        let root = scratch("blocking-absent");
        let agent = FakeAgent::install(&root).expect("install");

        let run = std::process::Command::new(agent.executable())
            .args(["-p", "do the work", "--session-id", "free"])
            .env(MARKER_DIR_VAR, agent.marker_dir())
            .output()
            .expect("run the fixture");

        assert!(run.status.success());
        assert!(
            agent
                .blocked_pids()
                .expect("read the pid records")
                .is_empty(),
            "no pid record should exist when nothing was asked to block"
        );
        let stdout = String::from_utf8(run.stdout).expect("the fixture emits UTF-8");
        assert!(
            stdout.contains(REPORTED_RESULT),
            "the run should have completed normally: {stdout}"
        );

        std::fs::remove_dir_all(&root).expect("clean up");
    }

    /// The guard that keeps a mis-built `PATH` from reaching a real agent.
    #[test]
    fn resolution_prefers_the_first_executable_entry() {
        let root = scratch("resolution");
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).expect("create");
        std::fs::create_dir_all(&second).expect("create");

        // Present in both, executable only in the second: a non-executable
        // file must not shadow the real one, exactly as the shell behaves.
        std::fs::write(first.join(EXECUTABLE), "not executable").expect("write");
        let real = second.join(EXECUTABLE);
        std::fs::write(&real, "#!/bin/sh\n").expect("write");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let path = OsString::from(format!("{}:{}", first.display(), second.display()));
        assert_eq!(resolve_on_path(EXECUTABLE, &path), Some(real));

        // And nothing at all is reported as nothing, not as a false match.
        let empty = OsString::from(first.display().to_string());
        assert_eq!(resolve_on_path(EXECUTABLE, &empty), None);

        std::fs::remove_dir_all(&root).expect("clean up");
    }
}
