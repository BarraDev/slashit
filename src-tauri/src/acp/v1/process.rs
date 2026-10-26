// SPDX-License-Identifier: MPL-2.0
//! Owned child process: spawn, supervise, write, read, and reap.
//!
//! Ownership is split so that no lock is held while a task waits for I/O.
//!
//! - The supervisor task owns the `Child`. It is the only code that waits on
//!   or kills it. Other tasks reach it through a control channel.
//! - The writer task owns stdin. It drains a bounded queue.
//! - The reader task (in `connection`) owns stdout.
//! - The stderr task owns stderr and keeps a bounded, redacted tail.
//!
//! A blocked read therefore cannot stop a write, a kill, or a shutdown.

use super::connection::AcpError;
use super::frame::excerpt;
use crate::agents::provider_lease::ProviderLease;
use std::collections::VecDeque;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};

/// How many stderr lines are kept for error reports.
const STDERR_TAIL_LINES: usize = 20;

/// What the supervisor reports once the child has been reaped.
pub(super) type ExitReport = Option<Result<Exit, String>>;

/// A reaped child and what became of its process group.
#[derive(Debug, Clone)]
pub(super) struct Exit {
    pub status: ExitStatus,
    pub group: GroupEnd,
}

/// Whether the child's process group is known to be ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GroupEnd {
    /// SIGKILL was delivered to the group, or the group was already empty.
    /// On a platform without process groups, the leader alone is ended.
    Ended,
    /// The group could not be signalled, or its ownership could not be
    /// verified, so processes it left behind may still be running.
    Unconfirmed(String),
}

/// What one leader wait observed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum WaitOutcome {
    /// The leader has exited and is still unreaped.
    Exited,
    /// The wait was interrupted by a signal. Nothing is known, so it is retried.
    Interrupted,
    /// The leader's state cannot be observed. Its group must not be signalled.
    Failed(String),
}

/// The kernel calls behind supervision. Production uses `SystemKernel`. Tests
/// inject faults through this seam, so the error paths run without signalling
/// any real process group.
pub(super) trait Kernel: Send + Sync + 'static {
    /// One `waitid(P_PID, pid, WEXITED | WNOWAIT)`. Leaves the leader unreaped.
    fn wait_leader(&self, pid: u32) -> WaitOutcome;
    /// `killpg(pid, SIGKILL)`. A group that is already gone counts as ended.
    fn kill_group(&self, pid: u32) -> Result<(), String>;
}

/// The operating system's process calls.
pub(super) struct SystemKernel;

#[cfg(unix)]
impl Kernel for SystemKernel {
    fn wait_leader(&self, pid: u32) -> WaitOutcome {
        // Safety: `siginfo_t` is plain data and `waitid` fills it in.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let status = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if status == 0 {
            return WaitOutcome::Exited;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            WaitOutcome::Interrupted
        } else {
            WaitOutcome::Failed(format!("waitid: {error}"))
        }
    }

    fn kill_group(&self, pid: u32) -> Result<(), String> {
        // Safety: callers signal only a group whose leader is still unreaped
        // (see `Group`), so the group id still belongs to this process.
        if unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(format!("killpg: {error}"))
        }
    }
}

#[cfg(not(unix))]
impl Kernel for SystemKernel {
    fn wait_leader(&self, _pid: u32) -> WaitOutcome {
        WaitOutcome::Exited
    }

    fn kill_group(&self, _pid: u32) -> Result<(), String> {
        Ok(())
    }
}

/// Requests the supervisor understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Control {
    Kill,
}

/// How to start the agent. The working directory must be absolute.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: std::path::PathBuf,
    /// Added to the environment. With `clear_env` it is the whole environment.
    pub env: Vec<(String, String)>,
    /// Start from an empty environment, so only `env` reaches the agent. A
    /// provider run sets this, so a credential in SlashIt's own environment
    /// does not reach the child unless it is listed.
    pub clear_env: bool,
    /// Upper bound for one request, such as `initialize` or `session/new`.
    pub request_timeout: Duration,
    /// How long a permission request may wait for an owner decision before
    /// it is answered `cancelled`.
    pub permission_timeout: Duration,
}

pub(super) struct Owned {
    pub stdin: ChildStdin,
    pub stdout: BufReader<ChildStdout>,
    pub stderr: ChildStderr,
    pub control: mpsc::UnboundedSender<Control>,
    pub exit: watch::Receiver<ExitReport>,
    /// Leader waits this child has started. Test evidence of the waiter bound.
    #[cfg(test)]
    pub leader_waits: Arc<AtomicUsize>,
}

/// Starts the child and hands its pipes to separate owners.
///
/// With a `lease`, the provider's lease is marked before the spawn, inherited
/// by the child, and its process group recorded once the child exists. A
/// provider that cannot be recorded is ended before anything can use it. The
/// child leads its own process group, so ending the run ends the tools it
/// started too. See `agents::provider_lease`.
///
/// `held` is dropped only after the child has been reaped, on every path that
/// starts one. A caller puts there what the child reads, such as its
/// configuration directory.
pub(super) fn spawn(
    spec: &SpawnSpec,
    lease: Option<ProviderLease>,
    held: Box<dyn Send>,
) -> Result<Owned, AcpError> {
    spawn_with(spec, lease, held, Arc::new(SystemKernel))
}

/// `spawn`, with the kernel calls supplied by the caller.
pub(super) fn spawn_with(
    spec: &SpawnSpec,
    lease: Option<ProviderLease>,
    held: Box<dyn Send>,
    kernel: Arc<dyn Kernel>,
) -> Result<Owned, AcpError> {
    if !spec.cwd.is_absolute() {
        return Err(AcpError::InvalidWorkingDirectory(
            "the agent working directory must be absolute".to_string(),
        ));
    }
    let mut command = Command::new(&spec.program);
    command.args(&spec.args).current_dir(&spec.cwd);
    if spec.clear_env {
        command.env_clear();
    }
    command
        .envs(spec.env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If every owner is dropped without a clean shutdown, the child dies
        // with its handle instead of outliving SlashIt.
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    if let Some(lease) = &lease {
        lease.inherit_into(&mut command);
        // Before the spawn: a crash between the spawn and the group record
        // must leave evidence that a provider may be running.
        lease
            .mark_spawning()
            .map_err(|error| AcpError::Spawn(excerpt(&format!("lease mark: {error}"))))?;
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            // No child exists, so the mark no longer describes a provider.
            if let Some(lease) = &lease {
                let _ = lease.clear_spawn_mark();
            }
            return Err(AcpError::Spawn(excerpt(&error.to_string())));
        }
    };
    let pid = child.id();
    if let Some(lease) = &lease {
        let recorded = match pid {
            Some(pid) => lease.record_group(pid).map_err(|error| error.to_string()),
            None => Err("the provider exited before its process could be recorded".into()),
        };
        if let Err(message) = recorded {
            // The group is ended here, while the lock is still held, so no
            // provider runs that a later attempt could not account for.
            end_unsupervised(child, pid, held);
            return Err(AcpError::Spawn(excerpt(&format!(
                "the provider's lease could not be recorded, so it was ended: {message}"
            ))));
        }
    }

    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        end_unsupervised(child, pid, held);
        return Err(AcpError::Spawn("the provider's pipes were not captured".into()));
    };

    let (control, control_rx) = mpsc::unbounded_channel();
    let (exit_tx, exit_rx) = watch::channel(None);
    let leader_waits = Arc::new(AtomicUsize::new(0));
    tokio::spawn(supervise(
        child,
        pid,
        control_rx,
        exit_tx,
        leader_waits.clone(),
        kernel,
        held,
    ));

    Ok(Owned {
        stdin,
        stdout: BufReader::new(stdout),
        stderr,
        control,
        exit: exit_rx,
        #[cfg(test)]
        leader_waits,
    })
}

/// Ends a child that no supervisor owns, and drops `held` only once the child
/// has been reaped. Used when a start fails after the spawn.
fn end_unsupervised(mut child: Child, pid: Option<u32>, held: Box<dyn Send>) {
    // The child is still unreaped here, so its group id is ours to signal.
    if let Some(pid) = pid {
        let _ = SystemKernel.kill_group(pid);
    }
    let _ = child.start_kill();
    tokio::spawn(async move {
        let _ = child.wait().await;
        drop(held);
    });
}

/// The supervisor's view of the child's process group. A group is signalled
/// only while its id is known to belong to this process: before the leader
/// has been reaped, and only if no earlier wait failed to observe the leader.
struct Group {
    kernel: Arc<dyn Kernel>,
    pid: Option<u32>,
    trusted: bool,
    end: GroupEnd,
}

impl Group {
    fn new(kernel: Arc<dyn Kernel>, pid: Option<u32>) -> Self {
        let mut group = Self {
            kernel,
            pid,
            trusted: true,
            end: GroupEnd::Ended,
        };
        if pid.is_none() {
            group.distrust("the child's process id is unknown".into());
        }
        group
    }

    /// Ends the group's remaining processes, if its ownership still holds.
    fn signal(&mut self) {
        let Some(pid) = self.pid else { return };
        if !self.trusted {
            return;
        }
        if let Err(reason) = self.kernel.kill_group(pid) {
            self.note_unconfirmed(reason);
        }
    }

    /// Records that the group's ownership can no longer be verified, so it
    /// is never signalled again.
    fn distrust(&mut self, reason: String) {
        self.trusted = false;
        self.note_unconfirmed(reason);
    }

    /// Keeps the first reason the group could not be confirmed ended.
    fn note_unconfirmed(&mut self, reason: String) {
        if self.end == GroupEnd::Ended {
            self.end = GroupEnd::Unconfirmed(reason);
        }
    }
}

/// Owns the child. Ends its process group on request or when every handle is
/// gone, and reports the status once it has been reaped. `_held` is dropped
/// after that report, so what it holds outlives the process.
async fn supervise(
    mut child: Child,
    pid: Option<u32>,
    mut control: mpsc::UnboundedReceiver<Control>,
    exit: watch::Sender<ExitReport>,
    leader_waits: Arc<AtomicUsize>,
    kernel: Arc<dyn Kernel>,
    _held: Box<dyn Send>,
) {
    // One leader wait for the child's whole life. Each request below only
    // loops back to the select; it never starts another wait, so a burst of
    // requests cannot pile up blocked threads. The reap is gated on the
    // leader, so the group is ended before the reap lets its id go.
    let mut group = Group::new(kernel.clone(), pid);
    let mut leader = Box::pin(leader_exited(kernel, pid, leader_waits));
    let mut leader_done = false;
    let mut control_open = true;
    loop {
        tokio::select! {
            outcome = &mut leader, if !leader_done => {
                leader_done = true;
                match outcome {
                    Ok(()) => group.signal(),
                    Err(reason) => group.distrust(reason),
                }
            }
            status = child.wait(), if leader_done => {
                let report = match status {
                    Ok(status) => Ok(Exit { status, group: group.end.clone() }),
                    Err(error) => Err(excerpt(&error.to_string())),
                };
                let _ = exit.send(Some(report));
                return;
            }
            command = control.recv(), if control_open => match command {
                Some(Control::Kill) => {
                    group.signal();
                    let _ = child.start_kill();
                }
                None => {
                    control_open = false;
                    group.signal();
                    let _ = child.start_kill();
                }
            },
        }
    }
}

/// Completes once the leader has exited, without reaping it. The caller then
/// ends the leader's process group before the reap lets the id go.
///
/// On Unix the leader is waited for without reaping, so its process group id
/// still belongs to this process. The blocking wait is started here, once per
/// child, and is not cancelled by the caller. Interrupted waits are retried
/// inside that one blocking task.
///
/// Resolves to `Err` when the leader's state cannot be observed. The group
/// must then not be signalled, because its id may no longer belong to this
/// process.
async fn leader_exited(
    kernel: Arc<dyn Kernel>,
    pid: Option<u32>,
    leader_waits: Arc<AtomicUsize>,
) -> Result<(), String> {
    let Some(pid) = pid else {
        return Err("the child's process id is unknown".into());
    };
    leader_waits.fetch_add(1, Ordering::SeqCst);
    let waited = tokio::task::spawn_blocking(move || loop {
        match kernel.wait_leader(pid) {
            WaitOutcome::Exited => return Ok(()),
            WaitOutcome::Interrupted => continue,
            WaitOutcome::Failed(reason) => return Err(reason),
        }
    })
    .await;
    match waited {
        Ok(result) => result,
        Err(error) => Err(format!("the leader wait did not finish: {error}")),
    }
}

/// Writes queued frames to stdin. `None` closes stdin, which is the spec's
/// signal to end the session, and the agent sees end of input.
pub(super) fn spawn_writer(mut stdin: ChildStdin, mut queue: mpsc::Receiver<Option<Vec<u8>>>) {
    tokio::spawn(async move {
        while let Some(Some(frame)) = queue.recv().await {
            if stdin.write_all(&frame).await.is_err() || stdin.flush().await.is_err() {
                break;
            }
        }
        // Dropping stdin closes the pipe.
    });
}

pub(super) type StderrTail = Arc<StdMutex<VecDeque<String>>>;

/// Keeps the last few stderr lines, redacted and bounded.
pub(super) fn spawn_stderr_capture(stderr: ChildStderr) -> StderrTail {
    let tail: StderrTail = Arc::new(StdMutex::new(VecDeque::new()));
    let sink = tail.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        loop {
            line.clear();
            match read_line_bounded(&mut reader, &mut line, 1024).await {
                Ok(LineRead::Line { .. }) => {
                    let text = excerpt(&String::from_utf8_lossy(&line));
                    let mut lines = lock(&sink);
                    if lines.len() == STDERR_TAIL_LINES {
                        lines.pop_front();
                    }
                    lines.push_back(text);
                }
                Ok(LineRead::Eof) | Err(_) => return,
            }
        }
    });
    tail
}

/// The most recent stderr lines, joined for an error message.
pub(super) fn stderr_excerpt(tail: &StderrTail) -> String {
    lock(tail).iter().cloned().collect::<Vec<_>>().join(" | ")
}

pub(super) enum LineRead {
    Eof,
    /// The line without its newline. `truncated` means it was longer than
    /// the limit; the rest was discarded up to the newline.
    Line {
        truncated: bool,
    },
}

/// Reads one newline-terminated line into `line`, keeping at most `limit`
/// bytes. Memory use is bounded no matter what the peer sends.
pub(super) async fn read_line_bounded<R>(
    reader: &mut R,
    line: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<LineRead>
where
    R: AsyncBufRead + Unpin,
{
    let mut truncated = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if line.is_empty() && !truncated {
                return Ok(LineRead::Eof);
            }
            return Ok(LineRead::Line { truncated });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let content_len = newline.unwrap_or(available.len());
        let room = limit.saturating_sub(line.len());
        if content_len > room {
            truncated = true;
        }
        line.extend_from_slice(&available[..content_len.min(room)]);
        let consumed = newline.map_or(available.len(), |index| index + 1);
        reader.consume(consumed);
        if newline.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(LineRead::Line { truncated });
        }
    }
}

pub(super) fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::Ordering::SeqCst;

    const LONG: Duration = Duration::from_secs(30);

    /// One scripted leader wait.
    enum Step {
        Exited,
        Interrupted,
        Failed(&'static str),
        Panic,
    }

    /// Replays a script of leader waits and counts the group signals. It never
    /// sends a real signal, so no process group is touched by these tests.
    struct Scripted {
        waits: StdMutex<VecDeque<Step>>,
        kill_error: Option<&'static str>,
        wait_calls: AtomicUsize,
        kill_calls: AtomicUsize,
    }

    impl Scripted {
        fn new(steps: Vec<Step>, kill_error: Option<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                waits: StdMutex::new(steps.into()),
                kill_error,
                wait_calls: AtomicUsize::new(0),
                kill_calls: AtomicUsize::new(0),
            })
        }
    }

    impl Kernel for Scripted {
        fn wait_leader(&self, _pid: u32) -> WaitOutcome {
            self.wait_calls.fetch_add(1, SeqCst);
            let step = lock(&self.waits)
                .pop_front()
                .unwrap_or(Step::Failed("the script ran out"));
            match step {
                Step::Exited => WaitOutcome::Exited,
                Step::Interrupted => WaitOutcome::Interrupted,
                Step::Failed(reason) => WaitOutcome::Failed(reason.to_string()),
                Step::Panic => panic!("injected waitid fault"),
            }
        }

        fn kill_group(&self, _pid: u32) -> Result<(), String> {
            self.kill_calls.fetch_add(1, SeqCst);
            self.kill_error.map_or(Ok(()), |reason| Err(reason.to_string()))
        }
    }

    fn spec(program: &str, args: &[&str]) -> SpawnSpec {
        SpawnSpec {
            program: program.to_string(),
            args: args.iter().map(ToString::to_string).collect(),
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            clear_env: false,
            request_timeout: LONG,
            permission_timeout: LONG,
        }
    }

    /// Spawns a child under `kernel` and returns its reaped report.
    async fn reaped(kernel: Arc<Scripted>, spec: &SpawnSpec) -> Exit {
        let owned = spawn_with(spec, None, Box::new(()), kernel).expect("spawn");
        let mut exit = owned.exit.clone();
        let report = tokio::time::timeout(LONG, exit.wait_for(Option::is_some))
            .await
            .expect("the supervisor reports in time")
            .expect("the supervisor is running")
            .clone();
        report.expect("a report").expect("the child was reaped")
    }

    #[tokio::test]
    async fn an_interrupted_wait_is_retried_before_the_group_is_ended() {
        let kernel = Scripted::new(vec![Step::Interrupted, Step::Interrupted, Step::Exited], None);
        let exit = reaped(kernel.clone(), &spec("sh", &["-c", "exit 3"])).await;
        assert_eq!(kernel.wait_calls.load(SeqCst), 3);
        assert_eq!(kernel.kill_calls.load(SeqCst), 1);
        assert_eq!(exit.group, GroupEnd::Ended);
        assert_eq!(exit.status.code(), Some(3));
    }

    #[tokio::test]
    async fn a_failed_leader_wait_never_signals_the_group() {
        for failure in ["waitid: No child processes (ECHILD)", "waitid: Invalid argument (EINVAL)"] {
            let kernel = Scripted::new(vec![Step::Failed(failure)], None);
            let exit = reaped(kernel.clone(), &spec("sh", &["-c", "exit 0"])).await;
            assert_eq!(kernel.kill_calls.load(SeqCst), 0, "{failure}");
            assert_eq!(exit.group, GroupEnd::Unconfirmed(failure.to_string()));
            assert!(exit.status.success(), "the leader is still reaped: {failure}");
        }
    }

    #[tokio::test]
    async fn a_panicking_leader_wait_never_signals_the_group() {
        let kernel = Scripted::new(vec![Step::Panic], None);
        let exit = reaped(kernel.clone(), &spec("sh", &["-c", "exit 0"])).await;
        assert_eq!(kernel.kill_calls.load(SeqCst), 0);
        assert!(
            matches!(&exit.group, GroupEnd::Unconfirmed(reason) if reason.contains("did not finish")),
            "{:?}",
            exit.group
        );
        assert!(exit.status.success());
    }

    #[tokio::test]
    async fn a_failed_group_signal_is_reported_as_unconfirmed() {
        let kernel = Scripted::new(vec![Step::Exited], Some("killpg: Operation not permitted (EPERM)"));
        let exit = reaped(kernel.clone(), &spec("sh", &["-c", "exit 0"])).await;
        assert_eq!(kernel.kill_calls.load(SeqCst), 1);
        assert_eq!(
            exit.group,
            GroupEnd::Unconfirmed("killpg: Operation not permitted (EPERM)".to_string())
        );
    }

    #[test]
    fn a_distrusted_group_is_never_signalled_again() {
        let kernel = Scripted::new(Vec::new(), None);
        let mut group = Group::new(kernel.clone(), Some(4242));
        // Before the leader is known to have failed, the group is ours.
        group.signal();
        group.distrust("waitid: No child processes (ECHILD)".to_string());
        group.signal();
        group.signal();
        assert_eq!(kernel.kill_calls.load(SeqCst), 1);
        assert_eq!(
            group.end,
            GroupEnd::Unconfirmed("waitid: No child processes (ECHILD)".to_string())
        );
    }

    #[test]
    fn a_group_without_a_known_leader_is_never_signalled() {
        let kernel = Scripted::new(Vec::new(), None);
        let mut group = Group::new(kernel.clone(), None);
        group.signal();
        assert_eq!(kernel.kill_calls.load(SeqCst), 0);
        assert!(matches!(group.end, GroupEnd::Unconfirmed(_)));
    }

    #[test]
    fn the_first_unconfirmed_reason_is_kept() {
        let kernel = Scripted::new(Vec::new(), Some("killpg: EPERM"));
        let mut group = Group::new(kernel, Some(4242));
        group.signal();
        group.distrust("waitid: EINVAL".to_string());
        assert_eq!(group.end, GroupEnd::Unconfirmed("killpg: EPERM".to_string()));
    }
}
