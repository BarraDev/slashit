// SPDX-License-Identifier: MPL-2.0
//! Publishing the fixture executables a journey's child processes run.
//!
//! Journeys stand in for the external programs SlashIt reaches (`claude`,
//! `gh`, `xdg-open`) and for shells the product is told to start, by writing
//! a small shell script and having the product resolve it through `PATH` or
//! the environment. A harness descriptor held open for writing is copied
//! into every child the harness forks while the write is still in progress,
//! and `execve` of the file keeps failing with `ETXTBSY` until every such
//! child execs. Journeys fork constantly -- each step starts another driver,
//! provider or application process -- so an in-process writer could make the
//! product's next launch of a fixture fail with "Text file busy".
//!
//! [`write_executable`] removes the writer from the equation: its descriptor
//! lives only in `/bin/cp`, which has exited by the time the call returns.
//!
//! This helper is a deliberate near-copy of the backend's
//! `slashit_ui_lib::test_helpers::write_executable`. The harness must not
//! depend on that crate: `slashit-ui` is the application binary, so a
//! dependency from `slashit-acceptance` would compile and link the whole
//! backend -- Tauri, the agents, the IPC server -- just to reach one
//! `pub fn`, and would break the harness's standalone
//! `cargo test -p slashit-acceptance` builds, which never want any of it. One
//! small function is cheaper and clearer than that dependency.

use anyhow::{anyhow, Context, Result};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Write `body` to `path` as an executable script another process can run.
///
/// The bytes are copied in by a short-lived `/bin/cp` child, not written by
/// the harness process: no descriptor of this process is open for writing
/// when the child that runs the script execs it, so no concurrently forked
/// child can have inherited one and made that exec fail with `ETXTBSY`. The
/// source file is never executed, so writing it in-process is safe.
///
/// An existing `path` is replaced, because the copy opens its destination
/// rather than recreating it. `cp` also writes through what it finds there:
/// it follows a symbolic link at the destination, and it opens and truncates
/// a file even when that file has other hard links. Either would rewrite a
/// file a publication must never touch -- the checked-in fake agent script a
/// generated entry used to point at, or whatever else shares its entry's
/// inode -- so an entry that would be written through is removed first and
/// the copy creates a fresh file in its place. A lone regular file is written
/// in place, which is also what keeps a failed copy from having destroyed
/// the previous script.
///
/// The copy itself is Linux-only, matching the crate: `/bin/cp` is named by
/// an absolute path, both because the Linux desktop runtime is the only
/// target and because journeys shadow `PATH`, which must not decide which
/// `cp` runs. A caller whose publication fails gets the copy's own error, and
/// finds no temporary `.source` left behind for the next journey step.
pub fn write_executable(path: &Path, body: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut source = path.as_os_str().to_owned();
    source.push(".source");
    let source = PathBuf::from(source);
    std::fs::write(&source, body)
        .with_context(|| format!("could not write {}", source.display()))?;
    // `cp` writes through an entry that shares its inode with another file:
    // it follows a symlink at the destination, and it opens and truncates a
    // file even when that file has other hard links. Either would rewrite a
    // file this publication must never touch, so such an entry is removed
    // first and the copy creates a fresh file in its place. A lone regular
    // file is written in place, which keeps a failed copy from having
    // destroyed the previous script.
    let writes_through = std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.file_type().is_symlink() || meta.nlink() > 1);
    if writes_through {
        std::fs::remove_file(path)
            .with_context(|| format!("could not replace the entry at {}", path.display()))?;
    }
    // A failed publication must not leave the temporary source behind for the
    // next journey step to trip over, and the caller learns of the copy, not
    // of the cleanup. The source write itself is reported above, and a
    // failure after the copy has published leaves the script exactly as a
    // successful publication would, minus its mode bits, which the caller
    // learns of directly.
    let copied = std::process::Command::new("/bin/cp")
        .arg(&source)
        .arg(path)
        .status();
    let copied = match copied {
        Ok(status) if status.success() => Ok(status),
        Ok(status) => Err(anyhow!("could not create {}: {status}", path.display())),
        Err(error) => {
            Err(anyhow::Error::from(error).context(format!("could not create {}", path.display())))
        }
    };
    if copied.is_err() {
        let _ = std::fs::remove_file(&source);
    }
    copied?;
    std::fs::remove_file(&source)
        .with_context(|| format!("could not remove {}", source.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("could not make {} executable", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write_executable;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// A scratch directory for one test, named for the test that made it and
    /// removed when the returned guard drops -- including during a panic's
    /// unwind, so a failing test leaves no litter behind in `TMPDIR`. The
    /// guard's name is unique, so concurrent test binaries do not collide.
    fn scratch(label: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("slashit-fixture-{label}-"))
            .tempdir()
            .expect("create the scratch directory")
    }

    /// A published script runs, and nothing of the publication is left
    /// behind for a later journey step to trip over.
    #[test]
    fn a_published_script_runs_and_leaves_no_source() {
        let dir = scratch("single");
        let path = dir.path().join("fixture.sh");
        write_executable(&path, "#!/bin/sh\necho published\n").expect("publish");

        let run = std::process::Command::new(&path)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(run.status.success(), "{run:?}");
        assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "published");
        assert!(
            !dir.path().join("fixture.sh.source").exists(),
            "the source copy is removed"
        );
        assert!(
            std::fs::metadata(&path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0),
            "the script is executable"
        );
    }

    /// A publication that cannot be copied reports the copy's own failure,
    /// and leaves no temporary source behind, so the next journey step never
    /// finds a stale `.source` at the same name.
    #[test]
    fn a_copy_failure_is_reported_and_leaves_no_source() {
        // Root ignores the write bit this test relies on, so it has no
        // reproduction for an unwritable destination.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = scratch("copy-failure");
        let path = dir.path().join("read-only.sh");
        std::fs::write(&path, "stale\n").expect("write the stale script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444))
            .expect("make the destination unwritable");

        let error = write_executable(&path, "#!/bin/sh\nexit 0\n")
            .expect_err("an unwritable destination defeats the copy");
        assert!(
            error.to_string().contains("could not create"),
            "the copy's own error is reported, not the cleanup: {error:#}"
        );
        assert!(
            !dir.path().join("read-only.sh.source").exists(),
            "the failed publication leaves no temporary source"
        );
        assert!(
            std::fs::read_to_string(&path).is_ok_and(|text| text == "stale\n"),
            "the publication never rewrote what stands there"
        );

        // Restore before the scratch directory is removed, so cleanup cannot
        // be blamed for needing its own privileges.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("restore the destination");
    }

    /// A published script replaces a symlink at its destination rather than
    /// rewriting what that symlink points at.
    #[test]
    fn a_published_entry_replaces_a_link_instead_of_following_it() {
        let dir = scratch("publish-over-link");
        let target = dir.path().join("checked-in.sh");
        write_executable(&target, "#!/bin/sh\necho original\n").expect("publish the target");
        let entry = dir.path().join("entry.sh");
        std::os::unix::fs::symlink(&target, &entry).expect("link to the target");

        write_executable(&entry, "#!/bin/sh\necho replaced\n").expect("publish over the link");

        // The entry is now its own file, and the checked-in script it used to
        // link to still says what it always said.
        assert!(std::fs::symlink_metadata(&entry).is_ok_and(|meta| !meta.file_type().is_symlink()));
        let published = std::fs::read_to_string(&entry).expect("read the published entry");
        assert_eq!(published, "#!/bin/sh\necho replaced\n");
        let target_text = std::fs::read_to_string(&target).expect("read the target");
        assert_eq!(target_text, "#!/bin/sh\necho original\n");
    }

    /// A published script replaces a hard link at its destination rather than
    /// writing through it: `cp` opens and truncates the shared inode, so
    /// without the entry's removal a publication would rewrite the other
    /// link's file too.
    #[test]
    fn a_published_entry_replaces_a_hard_link_instead_of_writing_through_it() {
        let dir = scratch("publish-over-hard-link");
        let sibling = dir.path().join("sibling.sh");
        write_executable(&sibling, "#!/bin/sh\necho original\n").expect("publish the sibling");
        let entry = dir.path().join("entry.sh");
        std::fs::hard_link(&sibling, &entry).expect("hard-link the entry to the sibling");

        write_executable(&entry, "#!/bin/sh\necho replaced\n").expect("publish over the hard link");

        // The entry is a file of its own now, and the file it used to share
        // an inode with still says what it always said.
        assert_eq!(
            std::fs::metadata(&entry).expect("read the published entry").nlink(),
            1,
            "the published entry shares its inode with nothing"
        );
        let published = std::fs::read_to_string(&entry).expect("read the published entry");
        assert_eq!(published, "#!/bin/sh\necho replaced\n");
        let sibling_text = std::fs::read_to_string(&sibling).expect("read the sibling");
        assert_eq!(sibling_text, "#!/bin/sh\necho original\n");
    }

    /// Eight threads that run `/bin/true` until dropped, each joined: the
    /// spawner pressure a published fixture must survive, stopped
    /// deterministically even when the test body panics (the guard's `Drop`
    /// runs during unwinding).
    struct SpawnerThreads {
        stop: Arc<AtomicBool>,
        handles: Vec<std::thread::JoinHandle<()>>,
    }

    impl SpawnerThreads {
        fn start(count: usize) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let handles: Vec<_> = (0..count)
                .map(|_| {
                    let stop = Arc::clone(&stop);
                    std::thread::spawn(move || {
                        while !stop.load(Ordering::Relaxed) {
                            let _ = Command::new("/bin/true")
                                .stdin(Stdio::null())
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .status();
                        }
                    })
                })
                .collect();
            Self { stop, handles }
        }
    }

    impl Drop for SpawnerThreads {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            for handle in self.handles.drain(..) {
                let _ = handle.join();
            }
        }
    }

    /// Publication survives the concurrency it exists for: scripts published
    /// by [`write_executable`] are executable while other threads fork
    /// constantly, which is exactly what a journey does between steps.
    ///
    /// The in-process writer this helper replaces fails erratically in this
    /// setting (`a script written in process is busy while a fork holds it
    /// open`, in the backend's `test_helpers`): the failure is a genuine
    /// write-then-exec race, so the regression must write and exec against
    /// real spawner pressure, not merely call the helper. Fifty scripts keeps
    /// the run to a second or two with the spawners stopped deterministically;
    /// a longer run only repeats the same fork pressure.
    #[test]
    fn every_published_script_executes_while_other_threads_spawn() {
        let dir = scratch("stress");
        let spawners = SpawnerThreads::start(8);
        for i in 0..50 {
            let path = dir.path().join(format!("stress-{i}.sh"));
            write_executable(&path, "#!/bin/sh\nexit 0\n")
                .unwrap_or_else(|error| panic!("publishing {} failed: {error:#}", path.display()));
            let status = Command::new(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap_or_else(|error| panic!("{} did not execute: {error}", path.display()));
            assert!(status.success(), "{} exited with {status}", path.display());
        }
        drop(spawners);
    }
}
