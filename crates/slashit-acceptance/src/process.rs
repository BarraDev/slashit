//! Process ownership: everything the harness starts must die with it.
//!
//! `tauri-driver` spawns `WebKitWebDriver`, which in turn spawns the
//! application. Neither is reaped by its parent, so signalling only the
//! process the harness holds a handle to leaves two survivors reparented to
//! init — one of them still listening on a port, both still holding this
//! run's environment. Putting the provider in a brand new process group and
//! signalling the *group* is what makes teardown cover the whole tree.
//!
//! Sending a signal is not cleanup. Every function here confirms the outcome.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// How long a process group gets to honour SIGTERM before SIGKILL.
const GRACE: Duration = Duration::from_secs(3);
/// How long the group gets to disappear after SIGKILL.
const KILL_TIMEOUT: Duration = Duration::from_secs(3);

/// A child process that leads its own process group, together with every
/// descendant that group ever gains.
pub struct OwnedProcess {
    label: String,
    child: Child,
    pgid: i32,
    reaped: bool,
    /// Set only once the group has been *observed* to be empty, never merely
    /// because termination was attempted. A failed attempt has to stay
    /// retryable: `terminate` returns the error, and the `Drop` that follows
    /// would otherwise see a "done" flag and quietly do nothing.
    confirmed_gone: bool,
}

impl OwnedProcess {
    /// Spawn `command` as the leader of a brand new process group.
    ///
    /// The caller must not set its own `process_group`; this function owns
    /// that decision, because the whole cleanup contract depends on it.
    pub fn spawn(label: impl Into<String>, command: &mut Command) -> Result<Self> {
        use std::os::unix::process::CommandExt;

        let label = label.into();
        command.process_group(0);
        let child = command
            .spawn()
            .with_context(|| format!("could not start {label}"))?;
        let pgid = child.id() as i32;
        Ok(Self {
            label,
            child,
            pgid,
            reaped: false,
            confirmed_gone: false,
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// True while any process remains in the owned group.
    ///
    /// `kill(-pgid, 0)` performs the permission and existence checks without
    /// delivering a signal, which is the cheapest honest way to ask.
    pub fn group_alive(&self) -> bool {
        // SAFETY: `kill` with signal 0 delivers nothing; it only reports
        // whether the group exists and is signallable by this process.
        unsafe { libc::kill(-self.pgid, 0) == 0 }
    }

    fn signal_group(&self, signal: libc::c_int) {
        // SAFETY: a negative pid addresses the process group. A failure here
        // means the group is already gone, which the callers re-check anyway.
        unsafe {
            libc::kill(-self.pgid, signal);
        }
    }

    /// Reap the direct child, blocking until it exits.
    fn reap(&mut self) {
        if !self.reaped {
            let _ = self.child.wait();
            self.reaped = true;
        }
    }

    /// Reap the direct child if it has already exited, without blocking.
    ///
    /// This is what lets a well-behaved shutdown finish quickly. An exited
    /// process that nobody has waited on stays a zombie, and a zombie group
    /// *leader* keeps `kill(-pgid, 0)` succeeding — so without this the grace
    /// loop below can never observe the group as empty, burns the whole
    /// `GRACE` interval, and escalates to SIGKILL on every normal teardown.
    fn try_reap(&mut self) {
        if !self.reaped && matches!(self.child.try_wait(), Ok(Some(_))) {
            self.reaped = true;
        }
    }

    /// One nonblocking reap followed by a group-existence check. `true` means
    /// the group has been *observed* empty, which is the only thing allowed to
    /// set `confirmed_gone`.
    fn observe_gone(&mut self) -> bool {
        self.try_reap();
        if self.group_alive() {
            return false;
        }
        self.reap();
        true
    }

    /// Poll until the group is observed empty, or until `deadline` passes.
    ///
    /// The deadline is tested *after* an observation, never before one. A
    /// `while` on the clock takes its last look before its last sleep, so a
    /// group that exits during that final interval goes unseen and is sent a
    /// signal it had already earned its way out of. Taking the deadline as an
    /// argument is also what makes that boundary testable without racing it.
    fn wait_until_gone(&mut self, deadline: Instant) -> bool {
        loop {
            if self.observe_gone() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Terminate the whole group and verify it is gone.
    ///
    /// Returns `Ok` only after the group has been observed empty. That
    /// observation is the thing recorded, so a second call after success is a
    /// no-op while a second call after failure genuinely tries again.
    pub fn terminate(&mut self) -> Result<()> {
        if self.confirmed_gone {
            return Ok(());
        }

        self.signal_group(libc::SIGTERM);
        if self.wait_until_gone(Instant::now() + GRACE) {
            self.confirmed_gone = true;
            return Ok(());
        }

        // No blocking reap here. `wait_until_gone` reaps without blocking on
        // every pass, and blocks only once the group has been observed empty —
        // by which point the child has already exited. Reaping eagerly instead
        // would wait on the child for as long as it takes, which is precisely
        // what `KILL_TIMEOUT` is here to refuse.
        self.signal_group(libc::SIGKILL);
        if self.wait_until_gone(Instant::now() + KILL_TIMEOUT) {
            self.confirmed_gone = true;
            return Ok(());
        }

        // Deliberately leaves `confirmed_gone` false: the caller is told, and
        // `Drop` gets another attempt.
        bail!(
            "process group {} ({}) still has live members after SIGKILL",
            self.pgid,
            self.label
        )
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // A test that panics mid-journey never reaches the explicit shutdown,
        // so this is the fallback for the unwind path and must not add a
        // second panic to the first. The verified path remains `terminate`,
        // which returns its error to a caller that can act on it. What must
        // not happen is the fallback failing in silence: a process group that
        // outlived its owner is the exact thing this module exists to
        // prevent, and whoever has to kill it by hand needs its number.
        if let Err(error) = self.terminate() {
            eprintln!(
                "[acceptance] teardown left process group {} ({}) behind: {error:#}",
                self.pgid, self.label
            );
        }
    }
}

/// Report any process still *visibly* pointed at `config_home`.
///
/// This is the second line of defence, not the first. The lifecycle guarantee
/// is process-group ownership: [`OwnedProcess::terminate`] owns the group it
/// created and returns success only once that group has been observed empty.
/// This scan adds what a group cannot cover: a process that carries this run's
/// environment without ever being in the group the signal went to — one that
/// called `setsid` or `setpgid` for itself, or one a session daemon started on
/// the harness's behalf. Reparenting is not such a case; an orphan adopted by
/// init keeps its group and `kill(-pgid, …)` still reaches it, which is the
/// whole point of owning the group. This looks at the environment rather than
/// at process
/// names, because names are a weak check: the application, the native driver
/// and the provider have three different ones, and a developer's real SlashIt
/// may be running at the same time and must not be mistaken for a leak. Only
/// processes this harness launched carry an `XDG_CONFIG_HOME` inside the run's
/// temporary root.
///
/// `Ok` means no survivor was *visible*, which is weaker than no survivor
/// existing: `/proc` entries this process cannot read are skipped, so a
/// process hidden from the reader is indistinguishable here from no process at
/// all. Hence the name, and hence the ordering — this confirms the group
/// guarantee, it does not replace it.
pub fn assert_no_visible_survivors(config_home: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    // Assembled from the path's own bytes rather than through `display()`,
    // which substitutes U+FFFD for every byte that is not valid UTF-8. The
    // kernel reports the original bytes, so a needle built that way could
    // never equal the entry it was looking for, and a live process still
    // holding this run's state would be reported as no process at all.
    let mut needle = b"XDG_CONFIG_HOME=".to_vec();
    needle.extend_from_slice(config_home.as_os_str().as_bytes());
    let mut survivors = Vec::new();

    let entries = std::fs::read_dir("/proc").context("could not read /proc")?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<i32>().ok()) else {
            continue;
        };
        // A process that exits between the listing and the read is not a
        // survivor, so an unreadable entry is simply skipped. An entry that
        // belongs to somebody else is skipped by the same branch, which is
        // exactly what makes the guarantee above a visible-survivor one.
        let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        // Full-entry equality, not a substring search: one run's config home
        // is a prefix of another's whenever a counter rolls over a digit.
        if environ
            .split(|byte| *byte == 0)
            .any(|var| var == needle.as_slice())
        {
            let cmdline = std::fs::read_to_string(entry.path().join("comm"))
                .unwrap_or_else(|_| "<unknown>".to_string());
            survivors.push(format!("{pid} ({})", cmdline.trim()));
        }
    }

    if !survivors.is_empty() {
        bail!(
            "processes are still running against the run's config home {}: {}",
            config_home.display(),
            survivors.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether one specific process still exists, as opposed to the group it
    /// belongs to. `terminate` returns only once the group is observed empty,
    /// and a zombie still answers here, so a `false` after a successful
    /// teardown means the process is genuinely gone rather than merely
    /// unreaped.
    fn pid_alive(pid: i32) -> bool {
        // SAFETY: signal 0 delivers nothing and only reports existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// The whole point of the process group: a grandchild the harness never
    /// saw must not outlive teardown.
    #[test]
    fn terminate_takes_down_a_grandchild_the_harness_never_held() {
        // The shell reports the background child's pid *after* forking it, so
        // that number appearing is proof the grandchild exists. Waiting on
        // `group_alive` instead proves nothing at all: the shell is itself a
        // member of the group from the moment it is spawned, so the check
        // succeeds on its first attempt and teardown can beat the fork — SIGTERM
        // then kills `sh` alone, no grandchild is ever created, and a test named
        // for group-wide teardown passes without having tested it.
        let reported = std::env::temp_dir().join(format!(
            "slashit-acceptance-grandchild-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&reported);

        let mut command = Command::new("sh");
        command
            .arg("-c")
            // `sleep` in the background is the grandchild; the shell waits so
            // the direct child stays alive too. The path travels as an
            // environment value rather than inside the script, because
            // interpolating it would mean `display()`, and a temporary
            // directory is no more required to be valid UTF-8 here than
            // anywhere else in this module.
            .arg("sleep 120 & echo $! > \"$SLASHIT_GRANDCHILD_MARKER\"; wait")
            .env("SLASHIT_GRANDCHILD_MARKER", reported.as_os_str())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let mut owned = OwnedProcess::spawn("grandchild probe", &mut command).expect("spawn");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut grandchild = None;
        while grandchild.is_none() {
            // An empty file is the moment between the redirect and the write,
            // so the gate is the pid parsing, not the file existing.
            grandchild = std::fs::read_to_string(&reported)
                .ok()
                .and_then(|reported| reported.trim().parse::<i32>().ok());
            if grandchild.is_some() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_file(&reported);

        let grandchild = grandchild.expect("the shell never reported a background child");
        assert_ne!(
            grandchild, owned.pgid,
            "the reported pid is the shell itself, so there is no grandchild to test"
        );
        assert!(
            pid_alive(grandchild),
            "the grandchild should still be running when teardown starts"
        );

        owned.terminate().expect("terminate must verify the group is gone");
        assert!(
            !owned.group_alive(),
            "no member of the group may survive terminate"
        );
        // The whole point, asserted against the specific process rather than
        // against the group it happened to be in.
        assert!(
            !pid_alive(grandchild),
            "the grandchild the harness never held must not survive teardown"
        );
    }

    /// The direct child is reaped as soon as it exits, so a group that
    /// honours SIGTERM is observed empty immediately instead of lingering as
    /// a zombie leader. Without that, `kill(-pgid, 0)` keeps succeeding, the
    /// loop runs to the deadline, and every ordinary teardown pays `GRACE`
    /// and then escalates to SIGKILL.
    ///
    /// `sleep` rather than `sh -c` on purpose: it makes the process that
    /// exits on SIGTERM the group leader, which is precisely the zombie that
    /// used to hide the group's death.
    #[test]
    fn a_process_that_honours_sigterm_finishes_well_inside_the_grace_period() {
        let mut command = Command::new("sleep");
        command
            .arg("120")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let mut owned = OwnedProcess::spawn("sigterm probe", &mut command).expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !owned.group_alive() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(owned.group_alive(), "the probe should be running");

        let started = Instant::now();
        owned.terminate().expect("terminate");
        let elapsed = started.elapsed();

        // A generous threshold rather than a millisecond budget: the correct
        // path takes tens of milliseconds, and the defect took the full
        // GRACE plus a SIGKILL round trip.
        assert!(
            elapsed < GRACE,
            "a cooperative process took {elapsed:?}, which means the grace loop ran to its \
             deadline instead of noticing the exit"
        );
        assert!(!owned.group_alive(), "nothing may survive");
    }

    /// The regression this guards: `terminate` used to record completion
    /// *before* signalling, so when verification failed it returned `Err`
    /// while leaving a flag that made `Drop`'s retry a no-op. "Cleaned up"
    /// has to mean "observed empty", and nothing may set it on a path that
    /// observed nothing.
    #[test]
    fn cleanup_is_recorded_only_after_the_group_is_observed_empty() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 120")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let mut owned = OwnedProcess::spawn("state probe", &mut command).expect("spawn");
        assert!(
            !owned.confirmed_gone,
            "a live process must not be recorded as cleaned up"
        );

        owned.terminate().expect("terminate");
        assert!(
            owned.confirmed_gone,
            "a verified teardown must be recorded, or every later call repeats the work"
        );
    }

    /// The boundary the grace loop used to fall straight through: the deadline
    /// has passed *and* the group is already gone. A clock-first `while` never
    /// looks again, so a process that exits during the final sleep is answered
    /// with SIGKILL and the caller pays the whole grace period for a shutdown
    /// that had already succeeded.
    ///
    /// An expired deadline reproduces exactly that state with no timing to get
    /// lucky with: the observation either happens before the loop gives up, or
    /// it does not. The old shape returns `false` here; the current one must
    /// return `true`.
    #[test]
    fn an_expired_deadline_still_takes_one_final_observation() {
        let mut command = Command::new("true");
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let mut owned = OwnedProcess::spawn("already gone probe", &mut command).expect("spawn");

        // Reach the "gone" state before asking. The nonblocking reap is part
        // of it: an unwaited-for leader stays a zombie, and a zombie keeps the
        // group looking alive.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            owned.try_reap();
            if !owned.group_alive() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !owned.group_alive(),
            "the probe should have exited on its own"
        );

        assert!(
            owned.wait_until_gone(Instant::now()),
            "an already-expired deadline must still observe the group once; giving up on the \
             clock alone is what escalated to SIGKILL against a group that had already exited"
        );

        // And the contract that observation feeds: success is recorded, so
        // `Drop` does not repeat the work.
        owned
            .terminate()
            .expect("a group that is already gone must verify as gone");
        assert!(
            owned.confirmed_gone,
            "a group observed empty must be recorded as cleaned up"
        );
    }

    /// `KILL_TIMEOUT` exists for the case where SIGKILL does not settle the
    /// group promptly, and a blocking `reap()` used to run before it: a direct
    /// child the signal never reached parks `Child::wait()`, and the timeout
    /// written for exactly that case is never reached.
    ///
    /// A process that survives SIGKILL is not something a test can conjure, so
    /// the signal is pointed elsewhere instead. `pgid` is redirected at a decoy
    /// group, which leaves the victim's own child running while termination
    /// proceeds as usual. The decoy's leader is deliberately left unreaped,
    /// because a zombie leader keeps `kill(-pgid, 0)` succeeding: the group
    /// never looks empty, and both loops run to their deadlines, which is the
    /// state the deadlines exist to bound.
    ///
    /// The old ordering blocks here for as long as the child lives. The current
    /// one has to give up after `GRACE` plus `KILL_TIMEOUT` and report failure.
    #[test]
    fn a_child_the_signal_never_reached_cannot_park_the_kill_timeout() {
        // The decoy supplies a group that stays alive for the whole test: its
        // leader dies on the first signal and is never reaped here, and a
        // zombie is still a group member as far as `kill` is concerned.
        let mut decoy_command = Command::new("sleep");
        decoy_command
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut decoy = OwnedProcess::spawn("decoy group", &mut decoy_command).expect("spawn");

        let mut victim_command = Command::new("sleep");
        victim_command
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut victim =
            OwnedProcess::spawn("unreachable child", &mut victim_command).expect("spawn");
        let real_pgid = victim.pgid;
        victim.pgid = decoy.pgid;

        // Generous, because the point is not how long the bounded path takes
        // but that it is bounded at all: the unbounded path waits out the
        // child's whole 30 second lifetime.
        let budget = GRACE + KILL_TIMEOUT + Duration::from_secs(5);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let failed = victim.terminate().is_err();
            let _ = sender.send((victim, failed));
        });

        let (mut victim, reported_failure) = receiver.recv_timeout(budget).expect(
            "terminate never returned: the post-SIGKILL path is parked on a blocking wait for a \
             child the signal did not reach, so KILL_TIMEOUT bounds nothing",
        );
        assert!(
            reported_failure,
            "a group that never emptied must be reported, not quietly accepted"
        );
        assert!(
            !victim.confirmed_gone,
            "nothing may be recorded as cleaned up on a path that observed nothing"
        );

        victim.pgid = real_pgid;
        victim.terminate().expect("the real group must terminate");
        decoy.terminate().expect("the decoy group must terminate");
    }

    #[test]
    fn terminate_is_idempotent() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 120")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let mut owned = OwnedProcess::spawn("idempotence probe", &mut command).expect("spawn");
        owned.terminate().expect("first terminate");
        owned
            .terminate()
            .expect("a second terminate must not fail, because Drop also calls it");
    }

    #[test]
    fn a_state_root_nothing_ever_used_reports_no_survivors() {
        let root = std::env::temp_dir().join(format!(
            "slashit-acceptance-unused-{}-{}",
            std::process::id(),
            line!()
        ));
        assert_no_visible_survivors(&root)
            .expect("a root no process ever saw must come back clean");
    }

    /// A path is a sequence of bytes, and so is an environment value. The
    /// needle used to be built through `Path::display()`, which replaces every
    /// byte that is not valid UTF-8 with U+FFFD, while `/proc/<pid>/environ`
    /// is read and compared as raw bytes. Against a config home holding such a
    /// byte the two could never be equal: the walk found nothing, and a live
    /// process still pointed at the run's state was reported as no survivor at
    /// all. A cleanup check that fails open is worse than no check, because
    /// the next run then starts against a root somebody else is still writing.
    ///
    /// Reachable rather than exotic: every state root is created under
    /// `std::env::temp_dir()`, which hands back `TMPDIR` byte for byte.
    ///
    /// The readiness gate below is not decoration. `spawn` returning is not
    /// the moment the child's environment becomes visible: `Command` takes
    /// glibc's `posix_spawn` path here, which clones with
    /// `CLONE_VM | CLONE_VFORK`, and the kernel releases the parent inside
    /// `exec_mmap` before the new `mm` is installed and its `env_start` and
    /// `env_end` are set. Read inside that window `/proc/<pid>/environ` serves
    /// either nothing at all or the *parent's* environment, neither of which
    /// carries the override — which is how this test once failed on a hosted
    /// runner for a child that was demonstrably alive while passing on every
    /// developer machine: a runner has fewer processes to walk and fewer cores
    /// to walk them with, so the scan reached the child's pid while the window
    /// was still open. Measured here, a read taken the instant `spawn` returns
    /// misses the entry in 295 of 300 attempts, 294 of them at exactly the
    /// parent's environ length, while the same probe forced onto `fork` and
    /// `exec` by a `pre_exec` closure misses 0 of 300. `group_alive` cannot
    /// close the window, being true from the clone onwards and saying nothing
    /// about the process image, so the test waits for the child's own marker
    /// instead: an environment entry that can only be observed after the exec
    /// it was passed to. `SLASHIT_ACCEPTANCE_RUN_ID` exists for that gate
    /// alone and stays inside this test; production identity remains the
    /// process group.
    #[test]
    fn a_survivor_is_found_even_when_the_config_home_is_not_valid_utf8() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        // 0xFF cannot appear anywhere in valid UTF-8, so this is a perfectly
        // ordinary filesystem path that has no lossless string form at all.
        let mut raw = std::env::temp_dir().into_os_string().into_vec();
        raw.extend_from_slice(
            format!("/slashit-acceptance-non-utf8-{}-", std::process::id()).as_bytes(),
        );
        raw.push(0xFF);
        let config_home = PathBuf::from(OsString::from_vec(raw));
        assert!(
            config_home.to_str().is_none(),
            "the path has to be genuinely non-UTF-8, or this test proves nothing"
        );
        std::fs::create_dir_all(&config_home).expect("create a non-UTF-8 config home");

        let run_id = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default()
        );

        // `sleep` is the process fixture here, not a delay: a long-lived child
        // this test owns, signals and reaps.
        let mut command = Command::new("sleep");
        command
            .arg("120")
            .env("XDG_CONFIG_HOME", config_home.as_os_str())
            .env("SLASHIT_ACCEPTANCE_RUN_ID", &run_id)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut owned = OwnedProcess::spawn("non-utf8 survivor", &mut command).expect("spawn");
        let pid = owned.pgid;

        // Readiness by condition, with a deadline: the loop ends the moment
        // the kernel reports this child's own marker. A fixed wait would prove
        // nothing about the state that follows it.
        let marker = format!("SLASHIT_ACCEPTANCE_RUN_ID={run_id}").into_bytes();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut ready = false;
        while !ready {
            ready = std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
                environ
                    .split(|byte| *byte == 0)
                    .any(|var| var == marker.as_slice())
            });
            if ready || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let found = assert_no_visible_survivors(&config_home);

        // Tear everything down before asserting, so a failure reports the
        // defect instead of also leaking a process and a directory into the
        // rest of the suite. `Drop` covers the panic path as well.
        let terminated = owned.terminate();
        let after_teardown = assert_no_visible_survivors(&config_home);
        let removed = std::fs::remove_dir_all(&config_home);

        assert!(
            ready,
            "/proc/{pid}/environ never carried this child's own run marker, so the scan \
             would have been reading the pre-exec window rather than testing the comparison"
        );
        let report = found
            .expect_err("a live child holding this config home has to be reported")
            .to_string();
        assert!(
            report.contains(&pid.to_string()),
            "the report has to name pid {pid}, or it cannot be acted on: {report}"
        );
        terminated.expect("terminate the probe");
        removed.expect("remove the temporary config home");
        after_teardown.expect("a terminated group must leave the config home unused");
    }
}
