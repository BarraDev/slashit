//! Proof that no coding provider from an earlier run is still working in a
//! Task Checkout.
//!
//! A provider is ended on every path SlashIt controls, but a SlashIt that dies
//! hard leaves it running, and nothing on disk says so. A new attempt on that
//! checkout would then overlap a provider that still edits it. Process IDs and
//! working-directory scans cannot settle this (pids are reused, and a provider
//! can change directory), so the answer comes from a lock the provider holds.
//!
//! Unix: each Task has one lease file. A run takes an exclusive `flock` on it
//! and passes the descriptor to the provider, which keeps it for its whole
//! life. The kernel releases the lock only when every copy of the descriptor
//! is closed, so the lock stays held after SlashIt dies and is free only once
//! the provider has exited. The provider's process group is recorded beside it
//! as a second check, because a descendant that closed the inherited
//! descriptor would not hold the lock. A new attempt is admitted only when the
//! lock is free and that group is empty. A missing lease file is never read as
//! absence: see [`prove_absent_for_start`].
//!
//! The record has three states. Empty means no provider was spawned. A pid is
//! the provider's group. The spawn mark is written before every spawn and
//! replaced by the pid once the group is recorded. A spawn mark that survives
//! means a spawn may have happened without its group being recorded (SlashIt
//! died in between, or the record write failed), so it is never read as
//! absence: the Task needs a person to inspect its checkout.
//!
//! Windows: no equivalent is implemented, and nothing here claims that a
//! surviving provider can be ruled out. [`acquire`] returns no lease, so
//! ordinary first attempts and retries (Direct and Pending, with or without a
//! checkout) are admitted and promoted exactly as before; their other guards
//! (lifecycle, checkout, concurrency, recovery) apply unchanged. Only
//! [`prove_absent`] refuses there, so the explicit new attempt stays
//! unsupported.
//!
//! Limits, stated rather than hidden:
//!
//! - A descendant that closes the descriptor and then leaves its process group
//!   (`setsid`) is not seen.
//! - On Unix, a Task whose record shows a checkout or a run but that has no
//!   lease file is refused until a person has checked its Task Checkout. That
//!   includes a checkout attached by hand before any run, which carries no mark
//!   that tells it from a legacy run.
//! - Legacy Tasks are not covered by the lease guarantees. An older build could
//!   lose its checkout or run record after a failed save and still have
//!   launched a provider, and a missing historical record cannot show that no
//!   provider ever existed. Such a Task is not claimed crash-safe. Only runs
//!   made with a lease get the guarantees above.
//! - A queued Task whose lease record shows a live group or a spawn mark is
//!   left queued (see `queue::start_lease_admits`). That check reads the
//!   record without a lock, so a provider that holds the lock but has not yet
//!   recorded its group can still be promoted. Its start is then refused, so
//!   the guarantee holds, but it can briefly occupy a slot.

/// An exclusive lock on a Task's lease file, held by the caller until the
/// provider has been spawned.
#[derive(Debug)]
pub struct ProviderLease {
    #[cfg(unix)]
    file: std::fs::File,
}

#[cfg(unix)]
mod imp {
    use super::ProviderLease;
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    /// Creates the lease file if needed and takes its lock without waiting.
    pub fn acquire(path: &Path) -> io::Result<Option<ProviderLease>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        flock(&file, libc::LOCK_EX | libc::LOCK_NB).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "a coding provider still holds this Task Checkout",
                )
            } else {
                error
            }
        })?;
        Ok(Some(ProviderLease { file }))
    }

    impl ProviderLease {
        /// Clears close-on-exec on the lease in the child, so the provider inherits
        /// it. Call before spawning; the descriptor stays close-on-exec in SlashIt.
        pub fn inherit_into(&self, command: &mut tokio::process::Command) {
            let fd = self.file.as_raw_fd();
            // Safety: between fork and exec this only calls `fcntl`, which is
            // async-signal-safe, on a descriptor this process owns.
            unsafe {
                command.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        /// A lease whose record writes fail, as on a full disk. Test-only.
        #[cfg(test)]
        pub fn read_only_for_test(path: &Path) -> io::Result<ProviderLease> {
            Ok(ProviderLease {
                file: std::fs::File::open(path)?,
            })
        }

        /// Records that a provider is about to be spawned. Called before the spawn,
        /// and an error here means nothing is spawned. A crash between the spawn
        /// and [`Self::record_group`] then leaves this mark, which is evidence that
        /// something may be running, instead of an empty record that reads as
        /// "no provider was spawned".
        pub fn mark_spawning(&self) -> io::Result<()> {
            write_record(&self.file, SPAWNING)
        }

        /// Judges the record while this lease holds the lock. Holding the lock
        /// means no provider that holds it can be running, so only the record can
        /// show a group that survived its leader. Used before a new lease is
        /// handed out, so that a mark or a live group is never overwritten.
        pub fn prove_record_absent(&self) -> Result<(), String> {
            let mut file = &self.file;
            let mut recorded = String::new();
            file.seek(SeekFrom::Start(0))
                .and_then(|_| file.read_to_string(&mut recorded))
                .map_err(|error| format!("The provider lease cannot be read: {error}"))?;
            judge_record(&recorded)
        }

        /// Clears the spawn mark after the spawn itself failed. No child exists in
        /// that case, so the record is empty again. If the clear cannot be written,
        /// the mark stays and blocks absence, which is the fail-closed outcome.
        pub fn clear_spawn_mark(&self) -> io::Result<()> {
            write_record(&self.file, "")
        }

        /// Records the provider's process group, which is its pid. The caller then
        /// drops the lease, which releases SlashIt's own hold; the provider keeps
        /// the lock.
        ///
        /// When the record cannot be written, the provider may already be running
        /// with its group unrecorded. The group is ended while this lease still
        /// holds the lock. The spawn mark, or a partly written record, is left in
        /// place, so the Task is never read as absent.
        pub fn record_group(&self, pid: u32) -> io::Result<()> {
            let written = write_record(&self.file, &format!("{pid}\n"));
            if written.is_err() {
                // Safety: the group is the provider's own, made by `process_group(0)`
                // at spawn, so its id is the pid. SIGKILL reaches every member.
                unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
            }
            written
        }
    }

    /// The record written before a spawn. It is not a pid, so no group can be
    /// checked against it, and [`prove_absent`] refuses it.
    const SPAWNING: &str = "spawning\n";

    /// Replaces the record in place and never truncates it first. A failed write
    /// can then leave the old mark or a partial pid, and neither reads as absence.
    fn write_record(file: &std::fs::File, contents: &str) -> io::Result<()> {
        let mut file = file;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(contents.as_bytes())?;
        file.set_len(contents.len() as u64)?;
        file.sync_all()
    }

    /// `Ok` when no provider of the Task can still be running. `Err` gives the
    /// reason, for the person, when absence cannot be shown.
    pub fn prove_absent(path: &Path) -> Result<(), String> {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(
                    "No provider lease exists for this Task, so it cannot be shown that no provider is still running. It was started before this build, or was never started."
                        .to_string(),
                );
            }
            Err(error) => return Err(format!("The provider lease cannot be read: {error}")),
        };
        match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(
                    "A coding provider from an earlier attempt still holds this Task Checkout. It may still be changing it."
                        .to_string(),
                );
            }
            Err(error) => return Err(format!("The provider lease cannot be checked: {error}")),
        }
        let recorded = std::fs::read_to_string(path)
            .map_err(|error| format!("The provider lease cannot be read: {error}"))?;
        judge_record(&recorded)
    }

    /// Whether the record already on disk forbids a start. Read-only and takes
    /// no lock, so it cannot make a real launch's `acquire` fail. It is for
    /// deciding whether to promote a queued Task, never for admitting a launch:
    /// the start itself still proves absence under the lock.
    pub fn record_forbids_start(path: &Path) -> bool {
        match std::fs::read_to_string(path) {
            Ok(recorded) => judge_record(&recorded).is_err(),
            Err(_) => true,
        }
    }

    /// Judges a record. Called only when the lock is known to be free, or held
    /// by the caller, which is the case for [`ProviderLease::prove_record_absent`].
    fn judge_record(recorded: &str) -> Result<(), String> {
        if recorded.trim().is_empty() {
            // Nothing was recorded: no provider was spawned.
            return Ok(());
        }
        let Some(pid) = recorded.trim().parse::<i32>().ok() else {
            // A spawn mark, or a record that a failed write left partly written. A
            // provider may have been started and its group never recorded, so this is
            // not evidence of absence.
            return Err(
                "A coding provider launch in this Task Checkout was not recorded, so it may have started. Inspect the Task Checkout before any new attempt."
                    .to_string(),
            );
        };
        // Safety: signal 0 only tests whether the group exists.
        if unsafe { libc::killpg(pid, 0) } == 0 {
            return Err(
                "Processes from an earlier provider's process group are still running in this Task Checkout."
                    .to_string(),
            );
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Ok(()),
            _ => Err("The earlier provider's process group cannot be checked.".to_string()),
        }
    }

    fn flock(file: &std::fs::File, operation: libc::c_int) -> io::Result<()> {
        loop {
            // Safety: the descriptor is valid for the lifetime of `file`.
            if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::ProviderLease;
    use std::io;
    use std::path::Path;

    /// No lease on this platform: the first attempt runs unchanged.
    pub fn acquire(_path: &Path) -> io::Result<Option<ProviderLease>> {
        Ok(None)
    }

    impl ProviderLease {
        pub fn inherit_into(&self, _command: &mut tokio::process::Command) {}

        pub fn record_group(&self, _pid: u32) -> io::Result<()> {
            Ok(())
        }

        pub fn mark_spawning(&self) -> io::Result<()> {
            Ok(())
        }

        pub fn clear_spawn_mark(&self) -> io::Result<()> {
            Ok(())
        }

        pub fn prove_record_absent(&self) -> Result<(), String> {
            Ok(())
        }
    }

    /// No lease exists here, so no record can forbid a start.
    pub fn record_forbids_start(_path: &Path) -> bool {
        false
    }

    /// Absence cannot be shown on this platform, so a new attempt is refused.
    pub fn prove_absent(_path: &Path) -> Result<(), String> {
        Err(
            "This platform cannot show that no earlier coding provider is still running, so a new attempt is not available here."
                .to_string(),
        )
    }
}

pub use imp::{acquire, prove_absent, record_forbids_start};

/// Whether this platform creates provider leases at all.
pub const PLATFORM_HAS_LEASES: bool = cfg!(unix);

/// Whether a Task with no lease file may start or be promoted. A Task that has
/// never checked out has no provider behind it. Where leases exist, any other
/// Task is refused, because a missing lease proves nothing. Where they do not
/// (Windows), no lease file can ever be missing for a reason, so the Task's
/// ordinary guards decide and this one does not: absence cannot be proven there,
/// and nothing is claimed.
pub const fn missing_lease_admits(never_checked_out: bool, platform_has_leases: bool) -> bool {
    never_checked_out || !platform_has_leases
}

/// Takes the lease for a launch that may write the checkout, and only when the
/// record shows no provider left running. The record is judged under the lock
/// just taken, so no mark or live group is overwritten by the new launch.
/// `Ok(None)` only on a platform without leases.
pub fn acquire_proven(path: &std::path::Path) -> Result<Option<ProviderLease>, String> {
    let lease = acquire(path).map_err(|error| error.to_string())?;
    if let Some(lease) = &lease {
        lease.prove_record_absent()?;
    }
    Ok(lease)
}

/// The refusal for a Task with a recorded checkout or run and no lease file.
/// Nothing shows whether a provider from that run is still active, and a
/// missing lease never shows it.
pub const UNLEASED_CHECKOUT_REFUSAL: &str =
    "This Task has a recorded checkout or run, but no provider lease exists for it, so SlashIt cannot show that no provider from an earlier run is still active.";

/// [`prove_absent`] for a start that is not a new attempt. Where leases exist, a
/// missing lease file is not evidence of absence. It is admitted only when
/// `never_checked_out` says the Task's own record shows no checkout and no run
/// (see [`crate::domain::Task::never_checked_out`]), so no provider can have been
/// launched for it. Any other Task without a lease is refused. A lease file that
/// exists must prove absence, including on a crash that left a spawn mark.
/// Without leases (Windows) the start is not gated here; see
/// [`missing_lease_admits`].
pub fn prove_absent_for_start(
    path: &std::path::Path,
    never_checked_out: bool,
) -> Result<(), String> {
    if !PLATFORM_HAS_LEASES {
        return Ok(());
    }
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if missing_lease_admits(never_checked_out, PLATFORM_HAS_LEASES) {
                Ok(())
            } else {
                Err(UNLEASED_CHECKOUT_REFUSAL.to_string())
            }
        }
        _ => prove_absent(path),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn lease_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("task.lock")
    }

    /// Waits until absence is proven. A killed descendant can hold the descriptor
    /// for a moment after its leader is reaped, and its group can still hold a
    /// zombie until init reaps it, so a single reading right after the kill is
    /// not a fair test of the rule.
    async fn wait_until_proven_absent(path: &std::path::Path) {
        for _ in 0..500 {
            if prove_absent(path).is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("absence was never proven: {:?}", prove_absent(path));
    }

    /// Waits until the lock is free, whatever the record says. A fork in another
    /// test thread can briefly hold a copy of the lease descriptor until its
    /// exec, so a single reading right after an exit is not a fair test of the
    /// rule about the record.
    async fn wait_until_lock_free(path: &std::path::Path) {
        for _ in 0..500 {
            match prove_absent(path) {
                Err(reason) if reason.contains("still holds") => {}
                _ => return,
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the lock was never released: {:?}", prove_absent(path));
    }

    /// [`acquire_proven`], retried past the transient "still holds" that a fork in
    /// another test thread can cause. Any other answer is the test's answer.
    async fn acquire_proven_when_lock_free(
        path: &std::path::Path,
    ) -> Result<Option<ProviderLease>, String> {
        for _ in 0..500 {
            match acquire_proven(path) {
                Err(reason) if reason.contains("still holds") => {}
                other => return other,
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the lock was never released: {:?}", acquire_proven(path));
    }

    /// Repeats a reading past the transient "still holds". A fork in another test
    /// thread can hold a copy of the lease descriptor, and so its lock, until its
    /// exec. That is a refusal, never an admission, so a test about the rule for
    /// a free lock settles the reading before judging it. Any other answer is
    /// returned as it is.
    async fn settled<T>(mut read: impl FnMut() -> Result<T, String>) -> Result<T, String> {
        for _ in 0..500 {
            match read() {
                Err(reason) if reason.contains("still holds") => {}
                other => return other,
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        read()
    }

    /// Spawns a shell that holds the lease, as a provider does.
    fn spawn_holder(lease: ProviderLease, script: &str) -> tokio::process::Child {
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        lease.inherit_into(&mut command);
        let child = command.spawn().expect("spawn");
        let pid = child.id().expect("pid");
        lease.record_group(pid).expect("record group");
        child
    }

    #[tokio::test]
    async fn a_missing_lease_file_is_never_proven_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(prove_absent(&lease_path(&dir)).is_err());
    }

    #[tokio::test]
    async fn a_lease_survives_its_creator_and_is_free_only_after_the_provider_exits() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire(&path).unwrap().expect("unix lease");
        let mut child = spawn_holder(lease, "sleep 30");

        // The provider's lock outlives SlashIt's own hold, which is released once
        // the lease is consumed by spawning.
        assert!(matches!(prove_absent(&path), Err(reason) if reason.contains("still holds")));

        let pid = child.id().unwrap() as i32;
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        let _ = child.wait().await;
        wait_until_proven_absent(&path).await;
    }

    /// Linux only: the script enumerates its descriptors through `/proc`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_descendant_that_closes_the_lease_is_still_seen_through_its_group() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire(&path).unwrap().expect("unix lease");
        // The leader closes every descriptor above stdio, then leaves a sleeping
        // descendant behind in its own group and exits. The lock is free by then.
        let script = "for f in /proc/$$/fd/*; do n=${f##*/}; if [ \"$n\" -gt 2 ]; then eval \"exec $n>&-\"; fi; done; sleep 30 & exit 0";
        let mut child = spawn_holder(lease, script);
        let pid = child.id().unwrap() as i32;
        let _ = child.wait().await;

        // Lock free, but the group still has a live member.
        let result = settled(|| prove_absent(&path)).await;
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        assert!(
            matches!(&result, Err(reason) if reason.contains("process group")),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_free_lock_with_an_empty_group_is_proven_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire(&path).unwrap().expect("unix lease");
        let mut child = spawn_holder(lease, "exit 0");
        let _ = child.wait().await;
        assert_eq!(settled(|| prove_absent(&path)).await, Ok(()));
    }

    #[tokio::test]
    async fn a_second_acquire_while_the_lease_is_held_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire(&path).unwrap().expect("unix lease");
        let mut child = spawn_holder(lease, "sleep 30");
        assert!(acquire(&path).is_err());
        let pid = child.id().unwrap() as i32;
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        let _ = child.wait().await;
        wait_until_proven_absent(&path).await;
        assert!(settled(|| acquire(&path).map_err(|error| error.to_string())).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_group_whose_record_cannot_be_written_is_ended() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire(&path).unwrap().expect("unix lease");
        lease.mark_spawning().unwrap();
        drop(lease);
        // A read-only descriptor makes the record write fail, as a full disk would.
        let read_only = ProviderLease {
            file: std::fs::File::open(&path).unwrap(),
        };
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().expect("spawn");
        let pid = child.id().expect("pid");

        assert!(read_only.record_group(pid).is_err());

        let _ = child.wait().await;
        // The group was ended by the failed record, so nothing is left to account for.
        assert_eq!(unsafe { libc::killpg(pid as i32, 0) }, -1);
        // The mark was not replaced, so the failed record does not read as absence.
        wait_until_lock_free(&path).await;
        assert!(matches!(settled(|| prove_absent(&path)).await, Err(reason) if reason.contains("may have started")));
    }

    #[tokio::test]
    async fn a_spawn_mark_without_a_recorded_group_is_never_proven_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire(&path).unwrap().expect("unix lease");
        // SlashIt died after the mark and before any spawn, so the lock is free
        // and no group exists. The mark still refuses absence.
        lease.mark_spawning().unwrap();
        drop(lease);
        wait_until_lock_free(&path).await;
        assert!(matches!(settled(|| prove_absent(&path)).await, Err(reason) if reason.contains("may have started")));
    }

    #[tokio::test]
    async fn a_spawn_mark_cleared_after_a_failed_spawn_is_proven_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire(&path).unwrap().expect("unix lease");
        lease.mark_spawning().unwrap();
        // The spawn itself failed, so no child exists and the record is empty again.
        lease.clear_spawn_mark().unwrap();
        drop(lease);
        wait_until_proven_absent(&path).await;
    }

    #[tokio::test]
    async fn a_partly_written_record_is_never_proven_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        std::fs::write(&path, b"12x\n").unwrap();
        assert!(matches!(settled(|| prove_absent(&path)).await, Err(reason) if reason.contains("may have started")));
    }

    /// Linux only: the script enumerates its descriptors through `/proc`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_crash_between_the_spawn_and_the_group_record_is_not_read_as_absence() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire(&path).unwrap().expect("unix lease");
        lease.mark_spawning().unwrap();
        // The provider is spawned with the lease, and SlashIt "dies" before the
        // group is recorded. The leader closes the lease, leaves a sleeping
        // descendant in its group, and exits, so the lock is free while that
        // descendant is still running.
        let script = "for f in /proc/$$/fd/*; do n=${f##*/}; if [ \"$n\" -gt 2 ]; then eval \"exec $n>&-\"; fi; done; sleep 30 & exit 0";
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        lease.inherit_into(&mut command);
        let mut child = command.spawn().expect("spawn");
        drop(lease);
        let pid = child.id().unwrap() as i32;
        let _ = child.wait().await;

        // The leader and the lease are gone; only the descendant is left.
        wait_until_lock_free(&path).await;
        let result = settled(|| prove_absent(&path)).await;
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        assert!(
            matches!(&result, Err(reason) if reason.contains("may have started")),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_launch_is_admitted_when_nothing_is_recorded_or_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        assert!(acquire_proven_when_lock_free(&path).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_launch_is_refused_while_a_spawn_mark_remains_and_keeps_the_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire(&path).unwrap().expect("unix lease");
        lease.mark_spawning().unwrap();
        drop(lease);
        wait_until_lock_free(&path).await;

        let refused = acquire_proven_when_lock_free(&path).await;
        assert!(matches!(&refused, Err(reason) if reason.contains("may have started")), "{refused:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "spawning\n");
    }

    /// Linux only: the script enumerates its descriptors through `/proc`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_launch_is_refused_when_its_record_shows_a_group_still_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire(&path).unwrap().expect("unix lease");
        // The leader closes its lease, leaves a sleeping descendant in its group,
        // and exits. The lock is free, and the group was recorded.
        let script = "for f in /proc/$$/fd/*; do n=${f##*/}; if [ \"$n\" -gt 2 ]; then eval \"exec $n>&-\"; fi; done; sleep 30 & exit 0";
        let mut child = spawn_holder(lease, script);
        let pid = child.id().unwrap() as i32;
        let _ = child.wait().await;
        wait_until_lock_free(&path).await;

        let refused = acquire_proven_when_lock_free(&path).await;
        let recorded = std::fs::read_to_string(&path).unwrap();
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        assert!(
            matches!(&refused, Err(reason) if reason.contains("process group")),
            "{refused:?}"
        );
        assert_eq!(recorded, format!("{pid}\n"), "the recorded group is not overwritten");
    }

    #[tokio::test]
    async fn a_completed_provider_with_a_marked_and_recorded_group_releases_eligibility() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        let lease = acquire(&path).unwrap().expect("unix lease");
        lease.mark_spawning().unwrap();
        let mut child = spawn_holder(lease, "exit 0");
        let _ = child.wait().await;
        wait_until_proven_absent(&path).await;
    }

    #[tokio::test]
    async fn a_missing_lease_is_admitted_only_for_a_task_that_never_checked_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        assert_eq!(prove_absent_for_start(&path, true), Ok(()));
        assert_eq!(
            prove_absent_for_start(&path, false),
            Err(UNLEASED_CHECKOUT_REFUSAL.to_string()),
            "a missing lease is not evidence that an earlier provider is absent"
        );

        let lease = acquire(&path).unwrap().expect("unix lease");
        lease.mark_spawning().unwrap();
        drop(lease);
        assert!(
            prove_absent_for_start(&path, true).is_err(),
            "a lease file that exists is judged by its record, whatever the Task says"
        );
    }

    #[tokio::test]
    async fn a_lease_with_an_empty_record_and_a_free_lock_proves_absence_for_a_checked_out_task() {
        let dir = tempfile::tempdir().unwrap();
        let path = lease_path(&dir);
        drop(acquire(&path).unwrap().expect("unix lease"));
        assert_eq!(settled(|| prove_absent_for_start(&path, false)).await, Ok(()));
    }
}

/// Platform-neutral. The Windows branch is exercised through the pure rule,
/// because no lease file is ever created there.
#[cfg(test)]
mod admission_on_every_platform {
    use super::*;

    #[test]
    fn a_task_that_never_checked_out_starts_its_first_attempt_without_a_lease() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task.lock");
        assert_eq!(prove_absent_for_start(&path, true), Ok(()));
        assert!(missing_lease_admits(true, true));
        assert!(missing_lease_admits(true, false));
    }

    #[test]
    fn where_leases_exist_a_checked_out_task_without_a_lease_is_refused() {
        assert!(!missing_lease_admits(false, true));
    }

    #[test]
    fn where_no_lease_can_exist_a_checked_out_task_is_not_gated_by_the_missing_lease() {
        assert!(missing_lease_admits(false, false));
    }

    #[cfg(unix)]
    #[test]
    fn this_platform_refuses_a_checked_out_task_without_a_lease() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task.lock");
        assert_eq!(
            prove_absent_for_start(&path, false),
            Err(UNLEASED_CHECKOUT_REFUSAL.to_string())
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn this_platform_starts_a_checked_out_task_without_a_lease() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task.lock");
        assert_eq!(prove_absent_for_start(&path, false), Ok(()));
        assert!(!record_forbids_start(&path));
    }

    #[cfg(not(unix))]
    #[test]
    fn a_new_attempt_is_never_provable_on_this_platform() {
        let dir = tempfile::tempdir().unwrap();
        assert!(prove_absent(&dir.path().join("task.lock")).is_err());
    }
}
