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

/// The variable that holds an agent run open instead of letting it finish.
///
/// Its value is the path of a FIFO [`FakeAgent::block_agent_runs`] created.
/// Unset — which is every journey that does not ask for it — the fixture
/// behaves exactly as it did before this existed, and neither the pipe nor the
/// bookkeeping directory below is created.
///
/// A run that blocks first announces its own process id, then waits on the
/// pipe. Waiting on a pipe rather than sleeping is what makes a cancellation
/// journey deterministic: the run ends because the product terminated it or
/// because the test released it, never because an interval happened to elapse.
///
/// Only *agent runs* block. `claude --version` is the product asking what is
/// installed, and a probe that hung would stall the frontend rather than the
/// execution the journey is about.
pub const BLOCK_FIFO_VAR: &str = "SLASHIT_FAKE_AGENT_BLOCK_FIFO";

/// The smallest exchange `ClaudeRunner` accepts as a successful run: a
/// `system` line that names the session and model, a `result` line with
/// `is_error: false`, and exit status zero.
///
/// Written as `/bin/sh` because the harness may not assume any language
/// runtime beyond a POSIX shell, and because the whole point is that this is
/// an ordinary external program.
///
/// `printf '%s\0'` rather than one line per argument: the prompt the product
/// sends contains newlines, so a line-oriented record could not be parsed back
/// unambiguously. NUL is the one byte an argument cannot contain.
const SCRIPT: &str = r#"#!/bin/sh
# A stand-in for the Claude Code CLI, used by the SlashIt acceptance journeys.
# It never contacts a model. It records how it was called and emits the
# smallest stream-json exchange the real runner treats as a success.
set -e

if [ -z "$SLASHIT_FAKE_AGENT_MARKERS" ]; then
    echo "fake claude: SLASHIT_FAKE_AGENT_MARKERS is not set" >&2
    exit 97
fi

# Written in a staging directory and moved into place, because the test polls
# this directory while the agent is running: `mktemp` creates the file before
# anything is in it, so a reader could otherwise see a record that exists but
# is still empty. A rename within one filesystem is atomic, so only finished
# records are ever visible.
staging="$SLASHIT_FAKE_AGENT_MARKERS/.staging"
mkdir -p "$staging"
record=$(mktemp "$staging/XXXXXX")
printf '%s\0' "$PWD" "$@" > "$record"
mv "$record" "$SLASHIT_FAKE_AGENT_MARKERS/invocation-${record##*/}"

# Echo the session id back, so the caller can tie this run to the one it asked
# for rather than to any run at all.
# The prompt flag is noted in the same pass: it is what separates an agent run
# from `--version`, which a real CLI also answers without doing any work.
session=""
take_next=""
is_run=""
for arg in "$@"; do
    if [ -n "$take_next" ]; then
        session="$arg"
        take_next=""
        continue
    fi
    case "$arg" in
        --session-id) take_next="yes" ;;
        -p) is_run="yes" ;;
    esac
done

printf '{"type":"system","subtype":"init","session_id":"%s","model":"__MODEL__"}\n' "$session"

# Blocking mode, when a journey asked for it. Announce this process before
# waiting on the pipe, so the test can name the exact process it is about to
# ask the product to stop instead of inferring one. Staged and renamed for the
# same reason the invocation record is: a reader must never see a half-written
# pid. `$$` is this process, because the product exec'd this script directly.
if [ -n "$SLASHIT_FAKE_AGENT_BLOCK_FIFO" ] && [ -n "$is_run" ]; then
    processes="$SLASHIT_FAKE_AGENT_MARKERS/.processes"
    mkdir -p "$processes/.staging"
    pidfile=$(mktemp "$processes/.staging/XXXXXX")
    printf '%s\n' "$$" > "$pidfile"
    mv "$pidfile" "$processes/${pidfile##*/}"

    # Blocks in the kernel until a writer opens the pipe. A failed read is a
    # released or closed pipe, not a reason to abandon the run.
    read -r _release < "$SLASHIT_FAKE_AGENT_BLOCK_FIFO" || true
fi

# Absent means nothing is scripted to fail, so a journey that does not set this
# sees exactly the behaviour it saw before this existed -- not even the
# bookkeeping directory below is created.
failing=${SLASHIT_FAKE_AGENT_FAILING_RUNS:-0}

if [ -n "$is_run" ] && [ "$failing" -gt 0 ]; then
    # Claim the next run number. `mkdir` refuses an existing directory and
    # creates a missing one in a single indivisible step, so two agents
    # starting at the same moment can never be handed the same number -- and
    # unlike a counter file, there is no read-modify-write to lose.
    runs="$SLASHIT_FAKE_AGENT_MARKERS/.runs"
    mkdir -p "$runs"
    ordinal=1
    while ! mkdir "$runs/$ordinal" 2>/dev/null; do
        ordinal=$((ordinal + 1))
    done

    if [ "$ordinal" -le "$failing" ]; then
        # A well-formed result that reports failure, with a zero exit status:
        # the agent ran and said it could not do the work, which is a different
        # thing from the agent crashing.
        printf '{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"%s","result":"__FAILURE__"}\n' "$session"
        exit 0
    fi
fi

printf '{"type":"result","subtype":"success","is_error":false,"session_id":"%s","result":"__RESULT__"}\n' "$session"
exit 0
"#;

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

        let executable = bin_dir.join(EXECUTABLE);
        let script = SCRIPT
            .replace("__MODEL__", REPORTED_MODEL)
            .replace("__RESULT__", REPORTED_RESULT)
            .replace("__FAILURE__", REPORTED_FAILURE);
        std::fs::write(&executable, script)
            .with_context(|| format!("could not write {}", executable.display()))?;

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("could not make {} executable", executable.display()))?;

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

    /// Hold every agent run open: announce its process id, then wait.
    ///
    /// Returns the value for [`BLOCK_FIFO_VAR`], which the caller sets on the
    /// application's environment. The pipe is created here rather than by the
    /// fixture so that it exists before the application does — a run that had
    /// to create it could race a test already trying to release it.
    pub fn block_agent_runs(&self) -> Result<PathBuf> {
        use std::os::unix::ffi::OsStrExt;

        let pipe = self.release_pipe();
        let path = std::ffi::CString::new(pipe.as_os_str().as_bytes())
            .with_context(|| format!("{} is not a usable path", pipe.display()))?;

        // SAFETY: the pointer is to a NUL-terminated path this process keeps
        // alive across the call, which is all `mkfifo` asks of its caller.
        if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("could not create the release pipe {}", pipe.display()));
        }
        Ok(pipe)
    }

    /// The process ids of runs that announced themselves and then blocked.
    ///
    /// Empty unless a journey asked for [`BLOCK_FIFO_VAR`]. The names are
    /// `mktemp` names — unique but not monotonic — so a caller telling one run
    /// from another must compare sets, never positions.
    pub fn blocked_pids(&self) -> Result<Vec<u32>> {
        let dir = self.process_dir();
        if !dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut pids = Vec::new();
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
            pids.push(pid);
        }
        pids.sort_unstable();
        Ok(pids)
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

    /// Let every currently blocked run finish.
    ///
    /// The pipe is opened non-blocking on purpose. With no reader the open
    /// fails immediately with `ENXIO`, so releasing runs the product has
    /// already terminated is a no-op — rather than a test that hangs forever
    /// holding open a pipe nobody will ever read.
    pub fn release_blocked_runs(&self) -> Result<usize> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let pipe = self.release_pipe();
        let mut released = 0;
        // At most one release per run that ever blocked: a bound the fixture's
        // own records supply, rather than a guessed number of attempts.
        for _ in 0..self.blocked_pids()?.len() {
            let Ok(mut writer) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&pipe)
            else {
                break; // nothing is waiting on it
            };
            writer
                .write_all(b"release\n")
                .with_context(|| format!("could not release a run through {}", pipe.display()))?;
            released += 1;
        }
        Ok(released)
    }

    /// The pipe a blocked run waits on.
    ///
    /// Inside the marker directory, so the harness's existing cleanup removes
    /// it, and hidden from [`invocations`](Self::invocations) by the same
    /// `is_file` filter that hides the staging directory: a FIFO is not a
    /// regular file.
    fn release_pipe(&self) -> PathBuf {
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
        let pipe = agent.block_agent_runs().expect("create the release pipe");

        let mut child = std::process::Command::new(agent.executable())
            .args(["-p", "do the work", "--session-id", "blocked"])
            .env(MARKER_DIR_VAR, agent.marker_dir())
            .env(BLOCK_FIFO_VAR, &pipe)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("start the fixture");

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

    /// The product asks what is installed through the same executable. A probe
    /// that blocked would stall the frontend rather than the execution a
    /// cancellation journey is about, and would leave a pid record that the
    /// journey would then have to tell apart from a real run.
    #[test]
    fn a_version_probe_does_not_block_when_agent_runs_do() {
        let root = scratch("blocking-probe");
        let agent = FakeAgent::install(&root).expect("install");
        let pipe = agent.block_agent_runs().expect("create the release pipe");

        let probe = std::process::Command::new(agent.executable())
            .arg("--version")
            .env(MARKER_DIR_VAR, agent.marker_dir())
            .env(BLOCK_FIFO_VAR, &pipe)
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
