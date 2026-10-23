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
pub(super) type ExitReport = Option<Result<ExitStatus, String>>;

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
    kill_group(pid);
    let _ = child.start_kill();
    tokio::spawn(async move {
        let _ = child.wait().await;
        drop(held);
    });
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
    _held: Box<dyn Send>,
) {
    // One leader wait for the child's whole life. Each request below only
    // loops back to the select; it never starts another wait, so a burst of
    // requests cannot pile up blocked threads. The reap is gated on the
    // leader, so the group is ended before the reap lets its id go.
    let mut leader = Box::pin(leader_exited(pid, leader_waits));
    let mut leader_done = false;
    let mut control_open = true;
    loop {
        tokio::select! {
            () = &mut leader, if !leader_done => leader_done = true,
            status = child.wait(), if leader_done => {
                let report = status.map_err(|error| excerpt(&error.to_string()));
                let _ = exit.send(Some(report));
                return;
            }
            command = control.recv(), if control_open => match command {
                Some(Control::Kill) => {
                    kill_group(pid);
                    let _ = child.start_kill();
                }
                None => {
                    control_open = false;
                    kill_group(pid);
                    let _ = child.start_kill();
                }
            },
        }
    }
}

/// Completes once the leader has exited, after its process group has been
/// ended.
///
/// On Unix the leader is waited for without reaping, so its process group id
/// still belongs to this process. Whatever the leader left running in that
/// group is ended before the reap lets the id go. The blocking wait is
/// started here, once per child, and is not cancelled by the caller.
#[cfg(unix)]
async fn leader_exited(pid: Option<u32>, leader_waits: Arc<AtomicUsize>) {
    let Some(pid) = pid else { return };
    leader_waits.fetch_add(1, Ordering::SeqCst);
    let _ = tokio::task::spawn_blocking(move || {
        // Safety: `siginfo_t` is plain data and `waitid` fills it in.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            );
        }
    })
    .await;
    kill_group(Some(pid));
}

#[cfg(not(unix))]
async fn leader_exited(_pid: Option<u32>, _leader_waits: Arc<AtomicUsize>) {}

/// Ends the child's process group. Best-effort: a group that has already gone
/// is the outcome being asked for.
#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // Safety: `pid` is the leader of a group this process created with
        // `process_group(0)`, so the group id equals the pid. Callers ensure
        // the leader has not been reaped, so the id still belongs to us.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

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
