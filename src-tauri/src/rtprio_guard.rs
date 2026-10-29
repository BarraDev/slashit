// SPDX-License-Identifier: MPL-2.0
//! Startup guard for a Linux crash in WebKitGTK releases before 2.54.1.
//!
//! WebKitGTK 2.50.4, the version bundled in the official AppImage, moves the
//! UI process's IPC thread to `SCHED_RR` when the window starts. If the kernel
//! grants that, WebKit's `RLIMIT_RTTIME` fallback arms a 200 ms CPU budget for
//! the thread, and the kernel SIGKILLs the process when the budget runs out
//! during a startup burst. Lowering the soft `RLIMIT_RTPRIO` to zero makes the
//! kernel refuse the `SCHED_RR` request, so the thread stays `SCHED_OTHER` and
//! the budget is never enforced on it. A probe on the reference host confirmed
//! that an unprivileged process with a zero soft limit gets `EPERM` for
//! `SCHED_RR`, and that a child inherits the lowered limit. Launch measurements
//! showed no startup kill with the limit lowered. The source does not show
//! whether WebKit then falls back to RealtimeKit, which is a separate path that
//! was not ruled out.
//!
//! The guard is conditional. The loaded WebKitGTK is asked for its version with
//! `webkit_get_major_version` and the matching getters, which only return
//! constants. Among the releases checked, 2.54.1 is the earliest whose library
//! exports `WTF::HighPriorityThreads` instead of `WTF::RealTimeThreads`, so the
//! guard is skipped from that version on. Releases before it keep the guard,
//! and so does a version that cannot be read. No 2.53.x or 2.54.0 build was
//! available to check, so they are treated as affected. The version is the
//! library's own, not the distribution
//! package's: a distribution that patched an older release is still treated as
//! affected, which is the safe direction.
//!
//! Only the soft limit is lowered. The inherited hard limit is kept, so a
//! process can raise its soft limit back up to that value without privilege.
//! That keeps the change narrow: the terminals and agent processes that the
//! GUI starts inherit the lowered limit, and a child that needs realtime
//! scheduling can still ask for it. The headless `slashitd` never calls this
//! guard, so the agents it runs keep the inherited limit unchanged. The guard
//! only protects this process, and it does not change the kernel, RealtimeKit,
//! or WebKit. A process with `CAP_SYS_NICE` ignores the soft limit,
//! so the guard does not protect a privileged launch. SlashIt runs unprivileged.
//! Other systems are unaffected because the module is compiled only for Linux,
//! and only the GUI entry point calls it.
//!
//! This is a temporary compatibility workaround. It is meant to be removed once
//! the official Linux release is built with a WebKitGTK that does not need it,
//! and a CI-built release artifact passes the startup matrix with the guard
//! removed.
//!
//! The guard must run before the Tauri builder creates the first window,
//! because WebKit makes its scheduling request while the window is created.

use std::ffi::c_uint;
use std::io;

/// The `rlimit` pair as the kernel reports it.
type Limit = libc::rlimit;

/// The first WebKitGTK version treated as not needing the guard. The boundary
/// follows the exported symbols of the installed 2.50.4, 2.52.6, 2.54.1, and
/// 2.54.2 libraries. Short launches on 2.54.1 showed no startup kill, but no
/// startup matrix was run on it, so the boundary is not a measured minimum.
const FIRST_CORRECTED_VERSION: (u32, u32, u32) = (2, 54, 1);

extern "C" {
    fn webkit_get_major_version() -> c_uint;
    fn webkit_get_minor_version() -> c_uint;
    fn webkit_get_micro_version() -> c_uint;
}

/// Applies the guard when the loaded WebKitGTK needs it, and leaves the limit
/// untouched otherwise. Returns an error only when the guard was needed and
/// could not be applied.
pub fn apply_startup_guard() -> io::Result<()> {
    if guard_needed(runtime_webkit_version()) {
        lower_soft_rtprio_limit()
    } else {
        Ok(())
    }
}

/// The version of the WebKitGTK library loaded into this process, or `None`
/// when it reports major version 0, which is not a release.
fn runtime_webkit_version() -> Option<(u32, u32, u32)> {
    // SAFETY: the three getters take no arguments and return constants of the
    // loaded library. They do not touch GTK or WebKit state.
    let (major, minor, micro) = unsafe {
        (
            webkit_get_major_version(),
            webkit_get_minor_version(),
            webkit_get_micro_version(),
        )
    };
    (major != 0).then_some((major, minor, micro))
}

/// Whether the guard is needed for `version`. An unknown version keeps the
/// guard, because removing it could bring back the crash.
fn guard_needed(version: Option<(u32, u32, u32)>) -> bool {
    version.is_none_or(|version| version < FIRST_CORRECTED_VERSION)
}

/// Lowers the soft `RLIMIT_RTPRIO` of the calling process to zero.
///
/// The hard limit is left unchanged. Returns an error when the limit cannot be
/// read or written, so the caller can report that the protection is not in
/// place. A limit that is already zero is left alone.
pub fn lower_soft_rtprio_limit() -> io::Result<()> {
    lower_soft_limit_with(read_rtprio_limit, write_rtprio_limit)
}

/// Runs the read-then-write sequence with the system calls injected, so the
/// error paths can be tested without changing the test process's limits.
fn lower_soft_limit_with(
    read: impl FnOnce() -> io::Result<Limit>,
    write: impl FnOnce(Limit) -> io::Result<()>,
) -> io::Result<()> {
    let current = read()?;
    match lowered(current) {
        Some(next) => write(next),
        None => Ok(()),
    }
}

/// The limit to write, or `None` when the soft limit is already zero.
fn lowered(current: Limit) -> Option<Limit> {
    (current.rlim_cur != 0).then_some(Limit {
        rlim_cur: 0,
        rlim_max: current.rlim_max,
    })
}

fn read_rtprio_limit() -> io::Result<Limit> {
    let mut limit = Limit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid, writable `rlimit` for the duration of the call.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &mut limit) };
    if rc == 0 {
        Ok(limit)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn write_rtprio_limit(limit: Limit) -> io::Result<()> {
    // SAFETY: `limit` is a valid `rlimit` for the duration of the call, and the
    // kernel validates the values itself.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_RTPRIO, &limit) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Selects the child mode in the process that a parent test spawns, so the
    /// real system calls run where nothing else depends on the limits.
    const CHILD_ENV: &str = "SLASHIT_RTPRIO_GUARD_CHILD";
    /// Printed by the child when every real-syscall check ran.
    const CHECKED: &str = "RTPRIO_GUARD_CHECKED";
    /// Printed by the child when the host's hard limit is zero, so the
    /// positive-limit check cannot run there. The parent reports this as a skip.
    const SKIPPED: &str = "RTPRIO_GUARD_SKIPPED";
    /// Printed by the child when it runs as root. Root bypasses the limit, so
    /// the `SCHED_RR` refusal is not evaluated. The parent reports this.
    const ROOT_SKIPPED: &str = "RTPRIO_GUARD_ROOT_SKIPPED";
    /// When set, a skipped positive-limit check fails the test instead. Use it
    /// on a host where the hard limit must be non-zero.
    const REQUIRE_POSITIVE: &str = "SLASHIT_RTPRIO_GUARD_REQUIRE_POSITIVE";

    fn limit(cur: libc::rlim_t, max: libc::rlim_t) -> Limit {
        Limit {
            rlim_cur: cur,
            rlim_max: max,
        }
    }

    #[test]
    fn lowering_keeps_the_hard_limit() {
        let next = lowered(limit(95, 95)).expect("a non-zero soft limit is lowered");
        assert_eq!(next.rlim_cur, 0);
        assert_eq!(next.rlim_max, 95);
    }

    #[test]
    fn lowering_keeps_an_unlimited_hard_limit() {
        let next = lowered(limit(95, libc::RLIM_INFINITY)).expect("lowered");
        assert_eq!(next.rlim_cur, 0);
        assert_eq!(next.rlim_max, libc::RLIM_INFINITY);
    }

    #[test]
    fn a_zero_soft_limit_needs_no_write() {
        assert_eq!(lowered(limit(0, 95)), None);
    }

    #[test]
    fn a_zero_hard_limit_needs_no_write() {
        // The kernel keeps soft at or below hard, so a zero hard limit forces a
        // zero soft limit. Nothing can be lowered, and nothing is written.
        assert_eq!(lowered(limit(0, 0)), None);
    }

    #[test]
    fn the_bundled_2_50_4_needs_the_guard() {
        assert!(guard_needed(Some((2, 50, 4))));
    }

    #[test]
    fn versions_2_52_and_2_54_0_need_the_guard() {
        assert!(guard_needed(Some((2, 52, 6))));
        assert!(guard_needed(Some((2, 53, 92))));
        assert!(guard_needed(Some((2, 54, 0))));
    }

    #[test]
    fn versions_2_54_1_and_later_skip_the_guard() {
        assert!(!guard_needed(Some((2, 54, 1))));
        assert!(!guard_needed(Some((2, 54, 2))));
        assert!(!guard_needed(Some((3, 0, 0))));
    }

    #[test]
    fn an_unknown_version_keeps_the_guard() {
        assert!(guard_needed(None));
    }

    #[test]
    fn the_loaded_library_reports_a_webkitgtk_4_1_version() {
        // WebKitGTK 4.1 is major version 2. The test binary links the same
        // library as the app, so this exercises the real getters.
        let (major, _, _) = runtime_webkit_version().expect("the library reports a version");
        assert_eq!(major, 2);
    }

    #[test]
    fn a_failed_read_is_reported_and_nothing_is_written() {
        let mut wrote = false;
        let result = lower_soft_limit_with(
            || Err(io::Error::from_raw_os_error(libc::EPERM)),
            |_| {
                wrote = true;
                Ok(())
            },
        );
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EPERM));
        assert!(!wrote);
    }

    #[test]
    fn a_failed_write_is_reported_not_swallowed() {
        let result = lower_soft_limit_with(
            || Ok(limit(95, 95)),
            |_| Err(io::Error::from_raw_os_error(libc::EINVAL)),
        );
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn the_write_receives_the_lowered_limit() {
        let mut seen = None;
        lower_soft_limit_with(
            || Ok(limit(95, 95)),
            |next| {
                seen = Some(next);
                Ok(())
            },
        )
        .expect("the injected calls succeed");
        let seen = seen.expect("a write happened");
        assert_eq!((seen.rlim_cur, seen.rlim_max), (0, 95));
    }

    /// Runs `test_name` in a child process in the given mode, then checks the
    /// child's exit status and markers. A child that ran no checks cannot pass.
    fn run_child(test_name: &str, mode: &str) -> String {
        let exe = std::env::current_exe().expect("test binary path");
        let output = std::process::Command::new(exe)
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, mode)
            .output()
            .expect("spawn the child test process");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "child process failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains(CHECKED) || stdout.contains(SKIPPED),
            "child process ran no checks:\n{stdout}"
        );
        if stdout.contains(ROOT_SKIPPED) {
            // Written to stderr directly, as the positive-limit skip below is,
            // so that libtest's capture does not hide it.
            use std::io::Write;
            let _ = writeln!(
                io::stderr(),
                "NOT EXERCISED: the SCHED_RR refusal was not checked because the test runs as root."
            );
        }
        stdout
    }

    /// The positive-limit path: the soft limit is lowered from a non-zero value
    /// with the inherited hard limit kept, and `SCHED_RR` is refused.
    #[test]
    fn the_real_limit_is_lowered_in_a_child_process() {
        match std::env::var(CHILD_ENV).as_deref() {
            Ok("positive") => return run_positive_child_checks(),
            Ok(other) => panic!("unexpected {CHILD_ENV} mode: {other}"),
            Err(_) => {}
        }

        let stdout = run_child(
            "rtprio_guard::tests::the_real_limit_is_lowered_in_a_child_process",
            "positive",
        );
        if stdout.contains(SKIPPED) {
            assert!(
                std::env::var_os(REQUIRE_POSITIVE).is_none(),
                "the positive-limit check was skipped because the hard RLIMIT_RTPRIO is 0, \
                 and {REQUIRE_POSITIVE} is set"
            );
            // Written to stderr directly: libtest captures `eprintln!` output of
            // passing tests, which would hide the skip from the test output.
            use std::io::Write;
            let _ = writeln!(
                io::stderr(),
                "NOT EXERCISED: the positive-limit check was skipped because the hard \
                 RLIMIT_RTPRIO is 0 on this host. Set {REQUIRE_POSITIVE} to make this a failure."
            );
        }
    }

    /// The zero-hard-limit path. The child lowers its own hard limit to zero,
    /// which needs no privilege, so this runs on every host. The guard must
    /// leave both limits at zero and must not fail.
    #[test]
    fn a_zero_hard_limit_is_left_alone_in_a_child_process() {
        match std::env::var(CHILD_ENV).as_deref() {
            Ok("zero_hard") => return run_zero_hard_child_checks(),
            Ok(other) => panic!("unexpected {CHILD_ENV} mode: {other}"),
            Err(_) => {}
        }

        run_child(
            "rtprio_guard::tests::a_zero_hard_limit_is_left_alone_in_a_child_process",
            "zero_hard",
        );
    }

    fn run_positive_child_checks() {
        let inherited = read_rtprio_limit().expect("read the inherited limit");
        if inherited.rlim_max == 0 {
            println!("{SKIPPED}");
            return;
        }

        // The soft limit must be non-zero before the guard runs, or the check
        // would pass without lowering anything. An unprivileged process may
        // raise its soft limit up to the hard limit.
        let raised = limit(inherited.rlim_max, inherited.rlim_max);
        write_rtprio_limit(raised).expect("raise the soft limit to the hard limit");
        assert_ne!(read_rtprio_limit().expect("read").rlim_cur, 0);

        lower_soft_rtprio_limit().expect("lower the soft limit");
        let after = read_rtprio_limit().expect("read the limit after lowering");
        assert_eq!(after.rlim_cur, 0, "soft limit must be zero");
        assert_eq!(
            after.rlim_max, inherited.rlim_max,
            "hard limit must be kept"
        );
        assert_sched_rr_refused();
        println!("{CHECKED}");
    }

    fn run_zero_hard_child_checks() {
        // Lowering the hard limit needs no privilege, and only this child is
        // affected.
        write_rtprio_limit(limit(0, 0)).expect("lower the hard limit to zero");

        lower_soft_rtprio_limit().expect("a zero hard limit is a valid no-op");
        let after = read_rtprio_limit().expect("read the limit after the guard");
        assert_eq!(
            (after.rlim_cur, after.rlim_max),
            (0, 0),
            "both limits stay zero"
        );
        assert_sched_rr_refused();
        println!("{CHECKED}");
    }

    /// Behavior, not only the stored value. Root bypasses the limit, so the
    /// refusal is checked only for an unprivileged process.
    fn assert_sched_rr_refused() {
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            println!("{ROOT_SKIPPED}");
            return;
        }
        let param = libc::sched_param { sched_priority: 1 };
        // SAFETY: `param` is a valid `sched_param` for the duration of the call.
        let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_RR, &param) };
        assert_eq!(rc, -1, "SCHED_RR must be refused with a zero soft limit");
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM),
            "SCHED_RR must fail with EPERM"
        );
    }
}
