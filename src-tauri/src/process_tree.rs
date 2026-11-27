// SPDX-License-Identifier: MPL-2.0
//! Termination of a child process together with the descendants it started.
//!
//! A child spawned with `process_group(0)` leads its own process group, and
//! every descendant that does not leave that group shares its id. On Unix the
//! group is signalled. Windows has no process group in this sense, so
//! `taskkill /T` walks the tree instead.
//!
//! The leader's pid doubles as the group id only while the leader is unreaped.
//! Once it is reaped the number can be reissued, so callers signal a group only
//! before the reap, or after an `waitid(WNOWAIT)` that keeps the zombie in place
//! (see [`wait_for_exit_without_reaping`]).

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

/// Kill the process `pid` and every descendant it started. `taskkill /T`
/// walks the tree, which Windows does not do for a plain kill. Best-effort,
/// like [`kill_process_group`]: a tree that has already gone is the outcome
/// asked for. The call blocks for the duration of `taskkill`.
#[cfg(windows)]
pub(crate) fn kill_process_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
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
