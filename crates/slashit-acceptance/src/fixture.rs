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
use std::path::{Path, PathBuf};

/// A name no other publication in this process is using, in the
/// destination's own directory -- where the finished publication has to be
/// built, because `std::fs::rename` is atomic only within one filesystem.
/// The destination's own file name, distinguished by this process's id and a
/// per-process counter, also keeps two publications to the same directory
/// from colliding. A process that died before cleaning up leaves its
/// temporaries behind -- invisible clutter under names only publications
/// use -- and cannot be mistaken for another's.
fn scratch(path: &Path, tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "script".into());
    name.push(format!(
        ".{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    path.with_file_name(name)
}

/// A temporary file removed when this value drops, however the guarded
/// function ends -- error return or panic alike. The removal's own failure
/// is not reported: the caller learns of the step that failed, and a
/// leftover is at worst clutter under a name only publications use.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Write `body` to `path` as an executable script another process can run.
///
/// The bytes are copied in by a short-lived `/bin/cp` child, not written by
/// the harness process: no descriptor of this process is open for writing
/// when the child that runs the script execs it, so no concurrently forked
/// child can have inherited one and made that exec fail with `ETXTBSY`. The
/// source file is never executed, so writing it in-process is safe.
///
/// The publication itself is prepared out of the way and moved into place
/// atomically: the copy lands in a uniquely named temporary file beside the
/// destination, receives its final mode, and only the finished copy is
/// renamed onto `path`. Until that rename, whatever stands at `path` -- a
/// plain script, a symbolic link to a script elsewhere, or a hard link
/// sharing an entry's inode -- is untouched, so a failed copy, mode change
/// or rename cannot truncate, remove or corrupt the previous script, and the
/// temporaries of the failed attempt are removed. The rename replaces the
/// entry itself, so the publication follows no destination symlink and
/// rewrites no file another link shares: both keep saying what they said
/// before, and the published entry has an inode of its own.
///
/// The copy is Linux-only, matching the crate: `/bin/cp` is named by an
/// absolute path, both because the Linux desktop runtime is the only
/// target and because journeys shadow `PATH`, which must not decide which
/// `cp` runs. A caller whose publication fails gets the failing step's own
/// error; cleanup runs either way.
pub fn write_executable(path: &Path, body: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let source = RemoveOnDrop(scratch(path, "source"));
    let staged = RemoveOnDrop(scratch(path, "staged"));
    std::fs::write(&source.0, body)
        .with_context(|| format!("could not write {}", source.0.display()))?;
    let copied = std::process::Command::new("/bin/cp")
        .arg(&source.0)
        .arg(&staged.0)
        .status();
    match copied {
        Ok(status) if status.success() => {}
        Ok(status) => return Err(anyhow!("could not create {}: {status}", staged.0.display())),
        Err(error) => {
            return Err(
                anyhow::Error::from(error).context(format!("could not create {}", staged.0.display()))
            )
        }
    }
    std::fs::set_permissions(&staged.0, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("could not make {} executable", staged.0.display()))?;
    std::fs::rename(&staged.0, path)
        .with_context(|| format!("could not publish {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write_executable;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;
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

    /// What a directory holds, sorted, for assertions that are about the
    /// absence or presence of entries rather than their order.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("list the directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn runs_echoing(path: &Path, expected: &str) {
        let run = Command::new(path)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("{} did not execute: {error}", path.display()));
        assert!(run.status.success(), "{run:?}");
        assert_eq!(
            String::from_utf8_lossy(&run.stdout).trim(),
            expected,
            "{} printed the wrong thing",
            path.display()
        );
    }

    /// A published script runs with the complete content it was given, and
    /// nothing of the publication is left behind for a later journey step to
    /// trip over.
    #[test]
    fn a_published_script_runs_and_leaves_no_temporaries() {
        let dir = scratch("single");
        let path = dir.path().join("fixture.sh");
        write_executable(&path, "#!/bin/sh\necho published\n").expect("publish");

        runs_echoing(&path, "published");
        assert_eq!(
            entries(dir.path()),
            vec!["fixture.sh"],
            "no temporary survives the publication"
        );
        assert!(
            std::fs::metadata(&path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0),
            "the script is executable"
        );
    }

    /// A publication over an existing script replaces it with the complete
    /// new content: the rename moves a fully written file, so `path` never
    /// shows a partial publication, and the previous content is not
    /// truncated in place on the way there.
    #[test]
    fn a_replacement_publishes_the_complete_new_content() {
        let dir = scratch("replacement");
        let path = dir.path().join("fixture.sh");
        write_executable(&path, "#!/bin/sh\necho first\n").expect("publish the first");

        write_executable(&path, "#!/bin/sh\necho second\n").expect("publish the replacement");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read the published script"),
            "#!/bin/sh\necho second\n",
            "the replacement published all of its content"
        );
        runs_echoing(&path, "second");
    }

    /// A publication whose preparation fails leaves the previous script
    /// exactly as it stood: every step before the rename works on uniquely
    /// named temporaries, so a directory without write permission defeats
    /// the publication before anything at `path` is touched.
    #[test]
    fn a_failed_publication_leaves_the_previous_script_intact() {
        // Root ignores the write bit this test relies on, so it has no
        // reproduction for a publication defeated by permissions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = scratch("publication-failure");
        let path = dir.path().join("argued-away.sh");
        write_executable(&path, "#!/bin/sh\necho previous\n").expect("publish the previous script");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555))
            .expect("take the directory's write permission away");
        // Declared after `dir`, so it drops first, also while an assertion
        // unwinds: `TempDir` cannot remove a read-only directory's entries.
        let _restore = RestoreWriteAccess(dir.path().to_path_buf());

        let error = write_executable(&path, "#!/bin/sh\nexit 0\n")
            .expect_err("a directory without write permission defeats the publication");
        assert!(
            error.to_string().contains("could not write"),
            "the failing step's own error is reported, not the cleanup: {error:#}"
        );
        assert_eq!(
            entries(dir.path()),
            vec!["argued-away.sh"],
            "the failed publication leaves no temporary behind"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read what stands at the destination"),
            "#!/bin/sh\necho previous\n",
            "the failed publication never touched what stands there"
        );
        assert!(
            std::fs::metadata(&path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0),
            "what stands there is still executable"
        );
    }

    /// Restores a directory's write permission when dropped, so the scratch
    /// directory can be removed whether the test passes or unwinds.
    struct RestoreWriteAccess(std::path::PathBuf);

    impl Drop for RestoreWriteAccess {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    /// A publication can prepare for a destination it cannot replace -- here
    /// a directory -- and fails at the rename with the temporaries cleaned
    /// up and what stands at the destination untouched.
    #[test]
    fn a_publication_that_cannot_rename_leaves_no_temporaries_behind() {
        let dir = scratch("rename-failure");
        let occupied = dir.path().join("occupied");
        std::fs::create_dir(&occupied).expect("occupy the destination with a directory");

        let error = write_executable(&occupied, "#!/bin/sh\nexit 0\n")
            .expect_err("a directory cannot be replaced by the rename");
        assert!(
            error.to_string().contains("could not publish"),
            "the rename's own error is reported, not the cleanup: {error:#}"
        );
        assert_eq!(
            entries(dir.path()),
            vec!["occupied"],
            "no temporary survives the failed publication"
        );
        assert!(
            std::fs::metadata(&occupied).is_ok_and(|meta| meta.is_dir()),
            "what stands at the destination is untouched"
        );
    }

    /// A published script replaces a symlink at its destination rather than
    /// rewriting what that symlink points at: the rename moves the entry
    /// itself, so nothing follows the link.
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

    /// A published script replaces a hard link at its destination rather
    /// than writing through it: the rename gives the entry a fresh inode, so
    /// the file another link shares keeps what it said.
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
