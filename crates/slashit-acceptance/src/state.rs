//! Temporary state roots, so a run can never see or damage real SlashIt data.
//!
//! `AppPaths` resolves every location through `directories::ProjectDirs`,
//! which on Linux honours the XDG environment variables. Pointing all four at
//! a temporary tree is therefore complete isolation with no product change:
//! the application reads and writes only inside the directory this module
//! owns.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The identifier `ProjectDirs::from("com", "barradev", "slashit-app")`
/// produces on Linux, and therefore the directory the application creates
/// under the isolated `XDG_CONFIG_HOME`.
const APP_DIR: &str = "slashit-app";

/// A private XDG tree for one acceptance run.
///
/// Removed on drop unless [`StateRoot::keep`] was called, which is how a
/// failing run leaves its evidence behind for inspection.
pub struct StateRoot {
    path: PathBuf,
    keep: bool,
    /// Set once the tree is known to be gone, so the `Drop` that follows an
    /// explicit [`StateRoot::cleanup`] neither repeats the work nor reports a
    /// missing directory as a failure.
    removed: bool,
}

/// How many names to try before giving up. A collision needs two candidates
/// to agree on the process id, the counter and the nanosecond clock, so more
/// than a couple of attempts means something is wrong with the parent
/// directory rather than unlucky.
const CLAIM_ATTEMPTS: usize = 32;

/// Distinguishes roots created inside one process, where the clock can repeat
/// but this cannot.
static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// Take exclusive ownership of `path`, or report that somebody else has it.
///
/// `create_dir` is the whole point: it either creates the directory or fails
/// with `AlreadyExists`, atomically, so two racing callers cannot both believe
/// they own the same root. `create_dir_all` succeeds against an existing
/// directory, which would have let two runs share one XDG tree and let either
/// one's `Drop` delete the other's live state.
fn claim(path: &Path) -> Result<bool> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("could not create {}", path.display())),
    }
}

/// Create a directory under `parent` that nobody else can be holding.
///
/// The name carries the process id, an in-process counter and the nanosecond
/// clock, and the directory itself is taken with [`claim`], so uniqueness is
/// established by the filesystem rather than assumed from the name. Callers
/// that derive a directory from a human-chosen label need exactly this: two
/// runs picking the same label, in the same second, in different processes,
/// must still end up with one directory each.
pub(crate) fn claim_unique(parent: &Path, label: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;

    for _ in 0..CLAIM_ATTEMPTS {
        let candidate = parent.join(format!(
            "{label}-{}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        if claim(&candidate)? {
            return Ok(candidate);
        }
    }

    bail!(
        "could not claim an unused directory for {label} under {} after {CLAIM_ATTEMPTS} attempts",
        parent.display()
    )
}

impl StateRoot {
    /// Create a fresh root under `parent`, owning it exclusively.
    pub fn create(parent: &Path, label: &str) -> Result<Self> {
        let path = claim_unique(parent, label)?;

        // Only now that the root is ours: anything below it is private by
        // construction, so plain `create_dir_all` is correct here.
        let root = Self {
            path,
            keep: false,
            removed: false,
        };

        for dir in [
            root.config_home(),
            root.data_home(),
            root.cache_home(),
            root.runtime_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }

        // XDG requires the runtime directory to be private, and GLib warns
        // loudly when it is not.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                root.runtime_dir(),
                std::fs::Permissions::from_mode(0o700),
            )
            .context("could not restrict the runtime directory")?;
        }

        Ok(root)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn config_home(&self) -> PathBuf {
        self.path.join("config")
    }

    pub fn data_home(&self) -> PathBuf {
        self.path.join("data")
    }

    pub fn cache_home(&self) -> PathBuf {
        self.path.join("cache")
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.path.join("runtime")
    }

    /// The `config.toml` the application persists projects and settings into.
    ///
    /// Reading it lets a persistence assertion corroborate what the restarted
    /// application reports with what actually reached the disk.
    pub fn config_file(&self) -> PathBuf {
        self.config_home().join(APP_DIR).join("config.toml")
    }

    /// Point a child process — and therefore everything it spawns — at this
    /// root.
    pub fn apply_to(&self, command: &mut Command) {
        command
            .env("XDG_CONFIG_HOME", self.config_home())
            .env("XDG_DATA_HOME", self.data_home())
            .env("XDG_CACHE_HOME", self.cache_home())
            .env("XDG_RUNTIME_DIR", self.runtime_dir())
            // Required on non-GNOME desktops: without it WebKitGTK segfaults
            // in the AT-SPI accessibility bridge.
            .env("NO_AT_BRIDGE", "1");
    }

    /// Survive teardown, because a failing run's state is evidence.
    pub fn keep(&mut self) {
        self.keep = true;
    }

    /// Remove this root now, and say so if it cannot be removed.
    ///
    /// `Drop` runs on the panic path and cannot return anything, so it can
    /// only ever be best effort. A run that finished normally and then failed
    /// to delete its own state is a different matter: reporting "clean" there
    /// would mean nothing more than that nobody looked. Calling this on a root
    /// that is kept, or already gone, is a no-op rather than an error.
    pub fn cleanup(&mut self) -> Result<()> {
        if self.keep || self.removed {
            return Ok(());
        }
        std::fs::remove_dir_all(&self.path)
            .with_context(|| format!("could not remove the state root {}", self.path.display()))?;
        self.removed = true;
        Ok(())
    }
}

impl Drop for StateRoot {
    fn drop(&mut self) {
        if self.keep || self.removed {
            return;
        }
        // The fallback for panics and abnormal exits, so it must not add a
        // second panic to the first. It can still name what it failed to
        // remove, which is the difference between a leak somebody can clean
        // up and one nobody ever hears about.
        match std::fs::remove_dir_all(&self.path) {
            Ok(()) => self.removed = true,
            // Somebody else got there first, which is the outcome we wanted.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "[acceptance] could not remove the state root {}: {error}",
                self.path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_creates_all_four_xdg_directories_and_removes_itself() {
        let parent = std::env::temp_dir();
        let path;
        {
            let root = StateRoot::create(&parent, "unit").expect("create a root");
            path = root.path().to_path_buf();
            for dir in [
                root.config_home(),
                root.data_home(),
                root.cache_home(),
                root.runtime_dir(),
            ] {
                assert!(dir.is_dir(), "{} should exist", dir.display());
            }
            assert!(root.config_file().starts_with(root.config_home()));
        }
        assert!(!path.exists(), "a dropped root should leave nothing behind");
    }

    /// `Drop` cannot report, so a run that finishes normally has to be able to
    /// ask for removal and be told whether it happened.
    #[test]
    fn explicit_cleanup_removes_the_root_and_can_be_repeated() {
        let parent = std::env::temp_dir();
        let mut root = StateRoot::create(&parent, "unit-cleanup").expect("create a root");
        let path = root.path().to_path_buf();

        root.cleanup()
            .expect("an ordinary root has to remove itself");
        assert!(!path.exists(), "cleanup reported success but left {path:?}");
        // The `Drop` that follows every explicit cleanup must not treat work
        // already done as a failure.
        root.cleanup().expect("cleanup is idempotent");
    }

    /// Evidence outranks tidiness: the failure path marks the root kept, and
    /// nothing after that may delete it.
    #[test]
    fn explicit_cleanup_leaves_a_kept_root_alone() {
        let parent = std::env::temp_dir();
        let path;
        {
            let mut root = StateRoot::create(&parent, "unit-kept-cleanup").expect("create a root");
            path = root.path().to_path_buf();
            root.keep();
            root.cleanup().expect("cleanup of a kept root is a no-op");
            assert!(
                path.exists(),
                "cleanup deleted evidence it was told to keep"
            );
        }
        assert!(path.exists(), "a kept root must survive the drop as well");
        std::fs::remove_dir_all(&path).expect("clean up the deliberately kept root");
    }

    /// Artifact directories are claimed with this same primitive, which is why
    /// it is shared. A name like `{test}-{seconds}` created with
    /// `create_dir_all` hands two contexts started in the same second one
    /// directory, and whichever finishes first deletes the other's evidence.
    #[test]
    fn two_claims_on_one_label_never_share_a_directory() {
        let parent =
            std::env::temp_dir().join(format!("slashit-claim-unique-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);

        let first = claim_unique(&parent, "same-name").expect("first claim");
        let second = claim_unique(&parent, "same-name").expect("second claim");

        assert_ne!(
            first, second,
            "two claims on one label were handed the same directory"
        );
        assert!(first.is_dir() && second.is_dir(), "both must exist");
        assert_eq!(
            std::fs::read_dir(&parent).expect("read parent").count(),
            2,
            "each claim owns a directory of its own"
        );

        std::fs::remove_dir_all(&parent).expect("clean up");
    }

    #[test]
    fn a_kept_root_survives_drop() {
        let parent = std::env::temp_dir();
        let path;
        {
            let mut root = StateRoot::create(&parent, "unit-kept").expect("create a root");
            path = root.path().to_path_buf();
            root.keep();
        }
        assert!(path.exists(), "a kept root should survive for inspection");
        std::fs::remove_dir_all(&path).expect("clean up the deliberately kept root");
    }

    #[test]
    fn two_roots_never_collide() {
        let parent = std::env::temp_dir();
        let first = StateRoot::create(&parent, "unit-unique").expect("first");
        let second = StateRoot::create(&parent, "unit-unique").expect("second");
        assert_ne!(first.path(), second.path());
    }

    /// The primitive the whole guarantee rests on. `create_dir_all` would
    /// return `Ok` for the second caller here, which is exactly how two runs
    /// could end up sharing one XDG tree.
    #[test]
    fn claiming_a_directory_twice_fails_the_second_time() {
        let parent = std::env::temp_dir().join(format!("slashit-claim-{}", std::process::id()));
        std::fs::create_dir_all(&parent).expect("create parent");
        let path = parent.join("root");

        assert!(claim(&path).expect("first claim"), "the first caller owns it");
        assert!(
            !claim(&path).expect("second claim"),
            "a second caller must be told the name is taken, not handed the same directory"
        );

        std::fs::remove_dir_all(&parent).expect("clean up");
    }

    /// Names are not trusted to be unique because they are hard to guess.
    /// Many threads racing on one label must still end up with one directory
    /// each, and each root must still own the tree it later deletes.
    #[test]
    fn concurrent_creation_hands_out_disjoint_roots() {
        const THREADS: usize = 16;
        let parent =
            std::env::temp_dir().join(format!("slashit-state-race-{}", std::process::id()));
        std::fs::create_dir_all(&parent).expect("create parent");

        let paths: Vec<PathBuf> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    scope.spawn(|| {
                        let mut root = StateRoot::create(&parent, "race").expect("create a root");
                        // Kept so every directory is still on disk when the
                        // assertions run; removed by hand below.
                        root.keep();
                        root.path().to_path_buf()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread"))
                .collect()
        });

        let unique: std::collections::BTreeSet<&PathBuf> = paths.iter().collect();
        assert_eq!(
            unique.len(),
            THREADS,
            "two threads were handed the same state root: {paths:?}"
        );
        let on_disk = std::fs::read_dir(&parent).expect("read parent").count();
        assert_eq!(on_disk, THREADS, "every root must be its own directory");

        std::fs::remove_dir_all(&parent).expect("clean up");
    }
}
