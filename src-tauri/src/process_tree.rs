// SPDX-License-Identifier: MPL-2.0
//! Termination of a child process together with the descendants it started.
//!
//! A child spawned with `process_group(0)` leads its own process group, and
//! every descendant that does not leave that group shares its id. On Unix the
//! group is signalled. [`TreeOwner`] wraps that for a spawned child.
//!
//! Windows has no process group in this sense, and `taskkill /T` finds
//! descendants through their parent pid, so it cannot reach a descendant once
//! the leader has exited. [`TreeOwner`] therefore puts the child in a Job
//! Object: membership outlives the parent, terminating the job ends every
//! member, and closing its last handle ends them too.
//!
//! The leader's pid doubles as the group id only while the leader is unreaped.
//! Once it is reaped the number can be reissued, so callers signal a group only
//! before the reap, or after an `waitid(WNOWAIT)` that keeps the zombie in place
//! (see [`wait_for_exit_without_reaping`]).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::process::{Child, Command};

/// The process tree of one spawned child: the child and whatever it starts.
///
/// Dropping it ends the tree on any path the owner does not finish normally,
/// including a dropped future. Declare it after the [`Child`] it owns, so it
/// drops first, while the leader is still unreaped.
pub(crate) struct TreeOwner {
    #[cfg(unix)]
    leader: Option<u32>,
    #[cfg(windows)]
    job: windows::Job,
    /// The leader's process handle as an integer, so the owner stays `Send`.
    /// Valid while the child is unreaped, which is all this is used for.
    #[cfg(windows)]
    leader_handle: Option<usize>,
    unreaped: AtomicBool,
}

impl TreeOwner {
    /// Spawns `command` as the leader of a tree this owner ends.
    ///
    /// On Windows the child starts suspended and is put in its job before it
    /// runs, so it cannot start a process outside the job. A failure to create
    /// the job or to take ownership returns an error and leaves nothing running,
    /// so the caller never runs a process it cannot end.
    pub(crate) fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
            command.creation_flags(CREATE_SUSPENDED | windows::CREATE_NO_WINDOW);
        }
        #[cfg(windows)]
        let job = windows::Job::new()?;

        let child = command.spawn()?;
        let owner = Self {
            #[cfg(unix)]
            leader: child.id(),
            #[cfg(windows)]
            job,
            #[cfg(windows)]
            leader_handle: child.raw_handle().map(|handle| handle as usize),
            unreaped: AtomicBool::new(true),
        };
        #[cfg(windows)]
        // The child is still suspended if this fails. Ending the job ends it,
        // and so does closing the job if the owner is dropped instead.
        #[cfg(windows)]
        if let Err(e) = owner.take_ownership(&child) {
            owner.signal();
            return Err(e);
        }
        Ok((child, owner))
    }

    /// Puts the suspended leader in the job, then resumes it.
    #[cfg(windows)]
    fn take_ownership(&self, child: &Child) -> io::Result<()> {
        let handle = self
            .leader_handle
            .ok_or_else(|| io::Error::other("the child exited before it was owned"))?;
        self.job
            .assign(handle as windows_sys::Win32::Foundation::HANDLE)?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("the child exited before it was owned"))?;
        windows::resume_threads(pid)
    }

    /// Ends every process in the tree. A no-op once the leader is reaped.
    pub(crate) fn signal(&self) {
        if !self.unreaped.load(Ordering::SeqCst) {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self.leader {
            kill_process_group(pid);
        }
        #[cfg(windows)]
        self.job.terminate();
    }

    /// Ends the tree, then kills and reaps the leader.
    pub(crate) async fn stop(&self, child: &mut Child) {
        self.signal();
        let _ = child.kill().await; // kills and reaps
        self.reaped();
    }

    /// Waits for the leader to exit without reaping it, so the tree can still be
    /// signalled afterwards. Returns at once if the leader was never adopted.
    pub(crate) async fn wait_for_leader_exit(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.leader {
            wait_for_exit_without_reaping(pid).await;
        }
        #[cfg(windows)]
        if let Some(handle) = self.leader_handle {
            windows::wait_for_exit(handle).await;
        }
    }

    /// Recorded right after the leader's reap, after which its pid may be reissued.
    pub(crate) fn reaped(&self) {
        self.unreaped.store(false, Ordering::SeqCst);
    }
}

impl Drop for TreeOwner {
    fn drop(&mut self) {
        // On Windows, dropping the job closes its last handle, which ends any
        // member still running.
        self.signal();
    }
}

/// Signal the whole group `pid` leads.
///
/// Best-effort by design: the only failure is a group that has already gone,
/// which is the outcome being asked for.
#[cfg(unix)]
pub(crate) fn kill_process_group(pid: u32) {
    // Safety: the caller passes the pid of a child it spawned as the leader of
    // its own group (`process_group(0)`), while that child is still unreaped, so
    // the group id equals the pid and still belongs to this process.
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
}

/// Kill the process `pid` and every descendant it started, by walking the tree
/// from `pid`. This only reaches descendants while `pid` still exists, so a
/// [`TreeOwner`] is the way to own a tree whose leader may exit first. Used for
/// processes that are not spawned through an owner. Best-effort, like
/// [`kill_process_group`]: a tree that has already gone is the outcome asked
/// for. The call blocks for the duration of `taskkill`.
#[cfg(windows)]
pub(crate) fn kill_process_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(windows::CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Block until the leader has exited, without reaping it.
///
/// An ordinary wait reaps as part of reporting the exit, and once that
/// happens the pid -- which doubles as the process group id -- can be reissued
/// to an unrelated process. `WNOWAIT` leaves the zombie in place instead, so
/// whatever runs immediately after this call still owns that number. The real
/// reap still has to happen; this only buys the caller a safe window to act
/// before it does.
///
/// A blocking syscall, so this runs on a blocking-pool thread rather than tying
/// up the async runtime for however long the child runs. Best-effort: the only
/// failure this can see is the child having already gone (`ECHILD`), and the
/// reap that follows reports that outcome on its own.
#[cfg(unix)]
pub(crate) async fn wait_for_exit_without_reaping(pid: u32) {
    let _ = tokio::task::spawn_blocking(move || {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            // Safety: `info` is a correctly sized out-parameter the kernel only
            // ever writes into.
            let ret =
                unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
            if ret == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
        }
    })
    .await;
}

#[cfg(windows)]
mod windows {
    use std::io;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, WaitForSingleObject, INFINITE, THREAD_SUSPEND_RESUME,
    };

    /// `taskkill` and spawned children must not open a console window.
    pub(super) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// An unnamed Job Object. Its members are ended when it is terminated, and
    /// when its last handle closes: this value's `Drop`.
    pub(super) struct Job(HANDLE);

    // Safety: a job handle names a kernel object, and the calls made on it are
    // thread-safe.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// A job that ends its members when its last handle closes.
        pub(super) fn new() -> io::Result<Self> {
            // Safety: null security attributes and name create an unnamed job.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // Owned from here, so an error below closes it.
            let job = Job(handle);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // Safety: `limits` is the structure the information class names, and
            // its size is passed with it.
            let set = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if set == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        /// Puts `process` in the job. Its children are then in the job too,
        /// including after `process` itself has exited.
        pub(super) fn assign(&self, process: HANDLE) -> io::Result<()> {
            // Safety: both handles are valid for the duration of the call.
            if unsafe { AssignProcessToJobObject(self.0, process) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Ends every process in the job. Best-effort: an empty job is the
        /// outcome asked for, and closing the handle still ends members.
        pub(super) fn terminate(&self) {
            // Safety: `self.0` is a job handle this value owns.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // Safety: `self.0` is closed once, here. With KILL_ON_JOB_CLOSE this
            // ends any member still running.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Resumes every thread of the suspended process `pid`. A process created
    /// suspended has only its main thread, so this resumes that one thread.
    pub(super) fn resume_threads(pid: u32) -> io::Result<()> {
        // Safety: a snapshot of all threads; closed below on every path.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let result = resume_listed(snapshot, pid);
        // Safety: `snapshot` is a handle this function opened and closes once.
        unsafe {
            CloseHandle(snapshot);
        }
        result
    }

    fn resume_listed(snapshot: HANDLE, pid: u32) -> io::Result<()> {
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut resumed = 0usize;
        // Safety: `entry` is sized as the API requires, and `snapshot` is open.
        let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                resume_thread(entry.th32ThreadID)?;
                resumed += 1;
            }
            // Safety: as above.
            more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        if resumed == 0 {
            return Err(io::Error::other(
                "the suspended process has no thread to resume",
            ));
        }
        Ok(())
    }

    fn resume_thread(thread_id: u32) -> io::Result<()> {
        // Safety: opened with the single right needed; closed below.
        let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
        if thread.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Safety: `thread` is open and valid until it is closed below.
        let previous = unsafe { ResumeThread(thread) };
        unsafe {
            CloseHandle(thread);
        }
        if previous == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Block until the process behind `handle` has exited, without reaping it.
    /// A process handle stays valid until it is closed, so unlike a pid it
    /// cannot be reissued while the child that holds it is alive.
    pub(super) async fn wait_for_exit(handle: usize) {
        let _ = tokio::task::spawn_blocking(move || {
            // Safety: the handle belongs to the child the caller holds, which
            // outlives this call. The caller ends the process if this would
            // otherwise never return.
            unsafe {
                WaitForSingleObject(handle as HANDLE, INFINITE);
            }
        })
        .await;
    }
}

/// Runtime checks of [`TreeOwner`] on Windows. They run in the Windows test job
/// (`.github/workflows/ci.yml`, `updater-transport`), which builds this crate's
/// library test executable and runs the `process_tree` tests from it.
///
/// The fixture is that same executable, started with only `fixture_entry`
/// selected and a mode in the environment. No shell or `PATH` program is
/// involved, so every process tree here is deterministic.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::TreeOwner;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };

    /// Which behaviour the fixture process takes.
    const MODE: &str = "SLASHIT_TREE_FIXTURE_MODE";
    /// The directory the fixture records pids in.
    const RECORD: &str = "SLASHIT_TREE_FIXTURE_RECORD";
    const ENTRY: &str = "process_tree::windows_tests::fixture_entry";

    /// The fixture. Does nothing unless a test started this executable with
    /// [`MODE`] set, so the ordinary test run passes it at once.
    #[test]
    fn fixture_entry() {
        let Ok(mode) = std::env::var(MODE) else {
            return;
        };
        let record = PathBuf::from(std::env::var_os(RECORD).expect("a record directory"));
        match mode.as_str() {
            // Starts a descendant that inherits this process's stdout and
            // stderr, then leaves at once with the descendant still running.
            "exit_leaving_descendant" => {
                record_pid(&record, "leader", std::process::id());
                start_descendant(&record);
            }
            "sleep_with_descendant" => {
                record_pid(&record, "leader", std::process::id());
                start_descendant(&record);
                sleep_forever();
            }
            "sleep_forever" => sleep_forever(),
            other => panic!("unknown fixture mode {other}"),
        }
    }

    fn sleep_forever() -> ! {
        loop {
            std::thread::sleep(Duration::from_secs(600));
        }
    }

    /// Starts a descendant that inherits this process's handles, and records
    /// its pid. Written beside the target and renamed, so a reader never sees
    /// a partial number.
    ///
    /// The descendant is left running on purpose: whether the tree ends it is
    /// what the test checks. Its handle is forgotten, not waited on, because
    /// waiting would never return.
    fn start_descendant(record: &Path) {
        let child = fixture_command("sleep_forever", record)
            .spawn()
            .expect("start the descendant");
        record_pid(record, "descendant", child.id());
        std::mem::forget(child);
    }

    fn record_pid(record: &Path, name: &str, pid: u32) {
        let temp = record.join(format!("{name}.tmp"));
        std::fs::write(&temp, pid.to_string()).expect("write a pid");
        std::fs::rename(temp, record.join(name)).expect("publish a pid");
    }

    /// The fixture as a standard command. Its stdio is inherited, so whatever
    /// starts it decides where its output goes.
    fn fixture_command(mode: &str, record: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().expect("test exe"));
        command
            .args(["--exact", ENTRY, "--nocapture", "--test-threads=1"])
            .env(MODE, mode)
            .env(RECORD, record);
        command
    }

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                dir: tempfile::tempdir().expect("tempdir"),
            }
        }

        /// The fixture as an owned run's command: piped output, no stdin.
        fn command(&self, mode: &str) -> tokio::process::Command {
            let mut command = tokio::process::Command::from(fixture_command(mode, self.dir.path()));
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            command
        }

        /// The pid the fixture recorded as `name`, once it has started.
        async fn recorded(&self, name: &str) -> u32 {
            for _ in 0..400 {
                let text = std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default();
                if let Ok(pid) = text.trim().parse() {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("the fixture never recorded {name}");
        }
    }

    /// Runs the fixture the way `queue::coderabbit::run_review` does: its
    /// output is drained while the leader's exit is awaited, and the tree is
    /// ended once the leader has exited. `stop` ends the run early through
    /// the same path a timeout or cancellation takes.
    /// Whether the run completed before `stop` ended it.
    async fn run_until(
        mut command: tokio::process::Command,
        stop: impl std::future::Future<Output = ()>,
    ) -> bool {
        let (mut child, owner) = TreeOwner::spawn(&mut command).expect("spawn an owned tree");
        let mut out = child.stdout.take().expect("stdout");
        let mut err = child.stderr.take().expect("stderr");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let completed = {
            let drain =
                async { tokio::join!(out.read_to_end(&mut stdout), err.read_to_end(&mut stderr)) };
            let end_group = async {
                owner.wait_for_leader_exit().await;
                owner.signal();
            };
            tokio::select! {
                biased;
                _ = stop => false,
                _ = async { tokio::join!(drain, end_group) } => true,
            }
        };

        if !completed {
            owner.stop(&mut child).await;
            return false;
        }
        child.wait().await.expect("the leader's exit status");
        owner.reaped();
        true
    }

    /// Gone, or a process that has exited and not yet been reclaimed by its
    /// last handle.
    fn is_running(pid: u32) -> bool {
        // Safety: a handle this function opens and closes on every path. Its
        // only use is to wait on the process with a zero timeout.
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                return false;
            }
            let running = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
            CloseHandle(handle);
            running
        }
    }

    async fn wait_until_stopped(pids: &[u32]) -> bool {
        for _ in 0..400 {
            if pids.iter().all(|pid| !is_running(*pid)) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    /// A process the run never started. It must survive every cleanup below.
    struct Unrelated(std::process::Child);

    impl Unrelated {
        fn start(fixture: &Fixture) -> Self {
            let mut command = fixture_command("sleep_forever", fixture.dir.path());
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            Unrelated(command.spawn().expect("start an unrelated process"))
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Unrelated {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    async fn a_leader_that_exits_first_completes_and_ends_its_descendant() {
        let fixture = Fixture::new();
        let unrelated = Unrelated::start(&fixture);
        let completed = tokio::time::timeout(
            Duration::from_secs(60),
            run_until(
                fixture.command("exit_leaving_descendant"),
                std::future::pending(),
            ),
        )
        .await
        .expect("a descendant kept the inherited pipes open");

        assert!(completed);
        let (leader, descendant) = (
            fixture.recorded("leader").await,
            fixture.recorded("descendant").await,
        );
        assert!(wait_until_stopped(&[leader, descendant]).await);
        assert!(
            is_running(unrelated.pid()),
            "an unrelated process was ended"
        );
    }

    #[tokio::test]
    async fn a_timed_out_run_ends_the_whole_tree() {
        let fixture = Fixture::new();
        let unrelated = Unrelated::start(&fixture);
        let completed = run_until(
            fixture.command("sleep_with_descendant"),
            tokio::time::sleep(Duration::from_secs(2)),
        )
        .await;

        assert!(!completed);
        let (leader, descendant) = (
            fixture.recorded("leader").await,
            fixture.recorded("descendant").await,
        );
        assert!(wait_until_stopped(&[leader, descendant]).await);
        assert!(
            is_running(unrelated.pid()),
            "an unrelated process was ended"
        );
    }

    #[tokio::test]
    async fn a_cancelled_run_ends_the_whole_tree() {
        let fixture = Fixture::new();
        let unrelated = Unrelated::start(&fixture);
        let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
        let (completed, (leader, descendant)) = tokio::join!(
            run_until(fixture.command("sleep_with_descendant"), async {
                let _ = cancelled.await;
            }),
            async {
                let ids = (
                    fixture.recorded("leader").await,
                    fixture.recorded("descendant").await,
                );
                cancel.send(()).expect("the run is still waiting");
                ids
            }
        );

        assert!(!completed);
        assert!(wait_until_stopped(&[leader, descendant]).await);
        assert!(
            is_running(unrelated.pid()),
            "an unrelated process was ended"
        );
    }

    #[tokio::test]
    async fn dropping_the_owner_ends_the_whole_tree() {
        let fixture = Fixture::new();
        let unrelated = Unrelated::start(&fixture);
        let mut command = fixture.command("sleep_with_descendant");
        let (mut child, owner) = TreeOwner::spawn(&mut command).expect("spawn an owned tree");
        let (leader, descendant) = (
            fixture.recorded("leader").await,
            fixture.recorded("descendant").await,
        );

        drop(owner);
        assert!(wait_until_stopped(&[leader, descendant]).await);
        assert!(
            is_running(unrelated.pid()),
            "an unrelated process was ended"
        );
        // The leader was never reaped by the owner; reap it now.
        let _ = child.kill().await;
    }
}
