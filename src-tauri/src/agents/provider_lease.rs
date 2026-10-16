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
//! lock is free and that group is empty.
//!
//! Windows: no equivalent is implemented. [`acquire`] returns no lease and
//! [`prove_absent`] refuses, so a new attempt there is refused, and the first
//! attempt of a Task still runs as before.
//!
//! Limits, stated rather than hidden: a descendant that closes the descriptor
//! and then leaves its process group (`setsid`) is not seen. Lease files written
//! before this module existed have no lease, so their runs are never proven
//! absent and need a manual check.

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
    use std::io::{self, Seek, SeekFrom, Write};
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

        /// Records the provider's process group, which is its pid, then releases
        /// SlashIt's own hold. The provider keeps the lock.
        pub fn record_group(self, pid: u32) -> io::Result<()> {
            let mut file = &self.file;
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(format!("{pid}\n").as_bytes())?;
            file.sync_all()
        }
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
        let recorded = std::fs::read_to_string(path).unwrap_or_default();
        let Some(pid) = recorded.trim().parse::<i32>().ok() else {
            // The lock is free and no group was recorded: no provider was spawned.
            return Ok(());
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

        pub fn record_group(self, _pid: u32) -> io::Result<()> {
            Ok(())
        }
    }

    /// Absence cannot be shown on this platform, so a new attempt is refused.
    pub fn prove_absent(_path: &Path) -> Result<(), String> {
        Err(
            "This platform cannot show that no earlier coding provider is still running, so a new attempt is not available here."
                .to_string(),
        )
    }
}

pub use imp::{acquire, prove_absent};

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
        let result = prove_absent(&path);
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
        assert_eq!(prove_absent(&path), Ok(()));
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
        assert!(acquire(&path).unwrap().is_some());
    }
}
