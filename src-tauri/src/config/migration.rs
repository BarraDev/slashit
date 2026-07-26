//! Moving a project's shareable state between external and in-project storage.
//!
//! Changing [`StateLocation`] must move data, not just repoint a setting —
//! otherwise a user who flips the toggle silently loses their board. The
//! ordering below is chosen so that an interruption at *any* point leaves the
//! source intact:
//!
//! 1. plan and detect conflicts (read-only);
//! 2. take an exclusive lock so nothing else writes during the move;
//! 3. copy into a staging directory beside the destination;
//! 4. verify the copy against the source, file by file;
//! 5. atomically rename staging into place;
//! 6. only then delete the source and update the registry.
//!
//! Steps 3–4 are disposable: staging directories are always safe to delete,
//! because the source is untouched until step 5 has already succeeded. That is
//! what makes the operation recoverable after a crash — [`StateMigrator::sweep_staging`]
//! removes leftovers on startup with no risk of losing data.
//!
//! The copy-then-rename shape also gives cross-filesystem support for free.
//! `fs::rename` cannot move between mounts (`EXDEV`), and external state
//! (`~/.local/share`) very often lives on a different filesystem from a
//! project. Since we copy anyway, the only rename is staging → destination
//! *within one parent directory*, which is always the same filesystem.

use super::paths::{is_valid_in_project_dir, STATE_MARKER};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// What to do when source and destination both hold data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Refuse and report the conflicting paths. The default: guessing which
    /// copy of a user's kanban board is the good one is not a decision this
    /// code is entitled to make.
    #[default]
    Abort,
    /// Source wins; destination is archived beside itself, never deleted.
    PreferSource,
    /// Destination wins; the migration becomes a no-op move and the source is
    /// archived rather than removed.
    PreferDestination,
}

/// A read-only description of a proposed migration, safe to show in the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub from: PathBuf,
    pub to: PathBuf,
    pub source_exists: bool,
    pub source_file_count: usize,
    pub source_bytes: u64,
    pub destination_exists: bool,
    pub destination_file_count: usize,
    /// Relative paths that exist in both locations.
    pub conflicts: Vec<String>,
    /// True when the plan cannot proceed under [`ConflictPolicy::Abort`].
    pub requires_resolution: bool,
}

impl MigrationPlan {
    pub fn is_noop(&self) -> bool {
        self.from == self.to || (!self.source_exists && !self.destination_exists)
    }
}

/// What actually happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MigrationOutcome {
    /// Source and destination are the same place, or there was no state at all.
    NothingToDo,
    /// State was copied, verified and the source removed.
    Migrated { files: usize, bytes: u64 },
    /// Destination already held the state and the policy kept it.
    KeptDestination { archived_source: Option<PathBuf> },
}

/// Result plus any non-fatal notes worth surfacing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationReport {
    pub outcome: MigrationOutcome,
    pub from: PathBuf,
    pub to: PathBuf,
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("another migration is already running for this project (lock held at {0})")]
    Locked(PathBuf),
    #[error(
        "both locations contain state and no resolution was chosen. \
         Conflicting entries: {0}. Choose which copy to keep, or move one aside manually."
    )]
    Conflict(String),
    #[error("verification failed after copying: {0}")]
    VerificationFailed(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl MigrationError {
    fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

type Result<T> = std::result::Result<T, MigrationError>;

/// Guard that holds the migration lock and releases it on drop, including on
/// panic or early return.
struct MigrationLock {
    path: PathBuf,
}

impl MigrationLock {
    /// Acquire by exclusive creation. `create_new` is atomic on every supported
    /// platform, so two processes cannot both believe they hold it.
    fn acquire(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| MigrationError::io(format!("creating {}", parent.display()), e))?;
        }

        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = writeln!(f, "pid = {}", std::process::id());
                Ok(Self { path })
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // A lock left behind by a crashed process would block the user
                // forever. Treat one older than the staleness window as dead —
                // safe because the protected operation never deletes the source
                // before its final rename has already succeeded.
                const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(300);
                let stale = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().unwrap_or_default() > STALE_AFTER)
                    .unwrap_or(false);
                if stale {
                    let _ = fs::remove_file(&path);
                    return Self::acquire(path);
                }
                Err(MigrationError::Locked(path))
            }
            Err(e) => Err(MigrationError::io(format!("locking {}", path.display()), e)),
        }
    }
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Suffix used for staging directories. Recognised by [`StateMigrator::sweep_staging`].
const STAGING_PREFIX: &str = ".slashit-migrating-";
const LOCK_SUFFIX: &str = ".slashit-migrate.lock";

pub struct StateMigrator;

impl StateMigrator {
    /// Inspect both locations without touching anything.
    pub fn plan(from: &Path, to: &Path) -> Result<MigrationPlan> {
        let source = scan(from)?;
        let dest = scan(to)?;

        let conflicts: Vec<String> = source
            .files
            .iter()
            .filter(|(rel, _)| dest.files.iter().any(|(d, _)| d == rel))
            .map(|(rel, _)| rel.clone())
            .take(50)
            .collect();

        let requires_resolution = !conflicts.is_empty();

        Ok(MigrationPlan {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            source_exists: source.exists,
            source_file_count: source.files.len(),
            source_bytes: source.bytes,
            destination_exists: dest.exists,
            destination_file_count: dest.files.len(),
            conflicts,
            requires_resolution,
        })
    }

    /// Execute a migration. Idempotent: running it again after success is a
    /// no-op because the source no longer exists.
    pub fn migrate(from: &Path, to: &Path, policy: ConflictPolicy) -> Result<MigrationReport> {
        let mut warnings = Vec::new();
        let plan = Self::plan(from, to)?;

        if plan.from == plan.to {
            return Ok(MigrationReport {
                outcome: MigrationOutcome::NothingToDo,
                from: from.into(),
                to: to.into(),
                warnings,
            });
        }

        // Nothing at the source: either already migrated, or never existed.
        if !plan.source_exists || plan.source_file_count == 0 {
            if plan.source_exists {
                // An empty source directory is disposable; a non-empty one is not.
                remove_dir_if_empty(from, &mut warnings);
            }
            ensure_state_dir(to)?;
            return Ok(MigrationReport {
                outcome: MigrationOutcome::NothingToDo,
                from: from.into(),
                to: to.into(),
                warnings,
            });
        }

        // Lock is placed beside the destination so two processes migrating the
        // same project in opposite directions still contend on one file.
        let lock_path = lock_path_for(to);
        let _lock = MigrationLock::acquire(lock_path)?;

        if plan.requires_resolution {
            match policy {
                ConflictPolicy::Abort => {
                    return Err(MigrationError::Conflict(plan.conflicts.join(", ")));
                }
                ConflictPolicy::PreferDestination => {
                    let archived = archive(from, &mut warnings)?;
                    return Ok(MigrationReport {
                        outcome: MigrationOutcome::KeptDestination {
                            archived_source: archived,
                        },
                        from: from.into(),
                        to: to.into(),
                        warnings,
                    });
                }
                ConflictPolicy::PreferSource => {
                    if let Some(p) = archive(to, &mut warnings)? {
                        warnings.push(format!(
                            "previous destination state archived to {}",
                            p.display()
                        ));
                    }
                }
            }
        }

        let parent = to.parent().ok_or_else(|| {
            MigrationError::io(
                "destination has no parent directory",
                io::Error::from(io::ErrorKind::InvalidInput),
            )
        })?;
        fs::create_dir_all(parent)
            .map_err(|e| MigrationError::io(format!("creating {}", parent.display()), e))?;

        let staging = parent.join(format!("{STAGING_PREFIX}{}", std::process::id()));
        // A staging dir from a previous crashed run is always disposable.
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }

        // Copy, verify, then swap. Any failure removes staging and returns with
        // the source untouched.
        let result = (|| -> Result<(usize, u64)> {
            copy_tree(from, &staging)?;
            verify(from, &staging)?;
            Ok((plan.source_file_count, plan.source_bytes))
        })();

        let (files, bytes) = match result {
            Ok(v) => v,
            Err(e) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(e);
            }
        };

        // Merge case: destination exists but had no conflicting files, or the
        // policy said source wins. Move its surviving entries into staging so
        // the final rename is still a single atomic step.
        if to.exists() {
            merge_into(to, &staging, &mut warnings)?;
            let retired = parent.join(format!("{STAGING_PREFIX}old-{}", std::process::id()));
            let _ = fs::remove_dir_all(&retired);
            fs::rename(to, &retired)
                .map_err(|e| MigrationError::io(format!("retiring {}", to.display()), e))?;
            match fs::rename(&staging, to) {
                Ok(()) => {
                    let _ = fs::remove_dir_all(&retired);
                }
                Err(e) => {
                    // Put the original destination back before surfacing the error.
                    let _ = fs::rename(&retired, to);
                    let _ = fs::remove_dir_all(&staging);
                    return Err(MigrationError::io(
                        format!("installing migrated state at {}", to.display()),
                        e,
                    ));
                }
            }
        } else {
            fs::rename(&staging, to).map_err(|e| {
                let _ = fs::remove_dir_all(&staging);
                MigrationError::io(format!("installing state at {}", to.display()), e)
            })?;
        }

        write_marker(to, &mut warnings);

        // The destination is now complete and verified. Only now is the source
        // expendable.
        if let Err(e) = fs::remove_dir_all(from) {
            warnings.push(format!(
                "state was migrated successfully, but the old directory {} could not be removed: {e}",
                from.display()
            ));
        }

        Ok(MigrationReport {
            outcome: MigrationOutcome::Migrated { files, bytes },
            from: from.into(),
            to: to.into(),
            warnings,
        })
    }

    /// Remove staging leftovers under `parent`. Safe by construction: staging
    /// directories only ever exist while a source directory is still intact.
    pub fn sweep_staging(parent: &Path) -> usize {
        let Ok(entries) = fs::read_dir(parent) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(STAGING_PREFIX) {
                if fs::remove_dir_all(entry.path()).is_ok() {
                    removed += 1;
                }
            } else if name.ends_with(LOCK_SUFFIX) {
                // Locks are per-process; a leftover one means a crash.
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().unwrap_or_default().as_secs() > 300)
                    .unwrap_or(true);
                if stale && fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        removed
    }

    /// Remove a legacy `.slashit/` directory that is empty, leaving anything
    /// non-empty strictly alone.
    ///
    /// Earlier versions created this directory in every registered workspace
    /// root and never wrote to it. Deleting only provably-empty directories
    /// means user data can never be caught by this cleanup.
    pub fn remove_empty_legacy_dir(dir: &Path) -> bool {
        if !dir.is_dir() || is_valid_in_project_dir(dir) {
            return false;
        }
        fs::remove_dir(dir).is_ok()
    }
}

fn lock_path_for(to: &Path) -> PathBuf {
    let parent = to.parent().unwrap_or(Path::new("."));
    let name = to
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "state".into());
    parent.join(format!("{name}{LOCK_SUFFIX}"))
}

fn ensure_state_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| MigrationError::io(format!("creating {}", dir.display()), e))
}

fn write_marker(dir: &Path, warnings: &mut Vec<String>) {
    let marker = dir.join(STATE_MARKER);
    if marker.exists() {
        return;
    }
    let body = "# SlashIt project state. Safe to commit if you want to share it.\nversion = 1\n";
    if let Err(e) = fs::write(&marker, body) {
        warnings.push(format!("could not write {}: {e}", marker.display()));
    }
}

fn remove_dir_if_empty(dir: &Path, warnings: &mut Vec<String>) {
    if let Ok(mut it) = fs::read_dir(dir) {
        if it.next().is_none() && fs::remove_dir(dir).is_err() {
            warnings.push(format!("could not remove empty {}", dir.display()));
        }
    }
}

/// Move a directory aside instead of deleting it. Used whenever a conflict
/// policy discards one side — the discarded copy is always recoverable.
fn archive(dir: &Path, warnings: &mut Vec<String>) -> Result<Option<PathBuf>> {
    if !dir.exists() {
        return Ok(None);
    }
    let parent = dir.parent().unwrap_or(Path::new("."));
    let base = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "state".into());

    for n in 0..1000 {
        let candidate = if n == 0 {
            parent.join(format!("{base}.superseded"))
        } else {
            parent.join(format!("{base}.superseded.{n}"))
        };
        if candidate.exists() {
            continue;
        }
        return match fs::rename(dir, &candidate) {
            Ok(()) => Ok(Some(candidate)),
            Err(e) => {
                warnings.push(format!("could not archive {}: {e}", dir.display()));
                Err(MigrationError::io(
                    format!("archiving {}", dir.display()),
                    e,
                ))
            }
        };
    }
    Ok(None)
}

/// Move entries from `src` into `dst`, skipping any that already exist there.
fn merge_into(src: &Path, dst: &Path, warnings: &mut Vec<String>) -> Result<()> {
    let entries = fs::read_dir(src)
        .map_err(|e| MigrationError::io(format!("reading {}", src.display()), e))?;
    for entry in entries.flatten() {
        let target = dst.join(entry.file_name());
        if target.exists() {
            continue;
        }
        let from = entry.path();
        if fs::rename(&from, &target).is_err() {
            // Cross-device or non-empty: fall back to a copy.
            let copied = if from.is_dir() {
                copy_tree(&from, &target).is_ok()
            } else {
                fs::copy(&from, &target).is_ok()
            };
            if !copied {
                warnings.push(format!("could not carry over {}", from.display()));
            }
        }
    }
    Ok(())
}

struct Scan {
    exists: bool,
    files: Vec<(String, u64)>,
    bytes: u64,
}

/// Recursively list regular files as (relative path, size).
///
/// Symlinks are recorded but not followed, so a symlink loop inside a project
/// cannot hang the scan and a symlink pointing outside cannot cause the copy to
/// pull in unrelated data.
fn scan(root: &Path) -> Result<Scan> {
    let mut files = Vec::new();
    let mut bytes = 0u64;
    if !root.exists() {
        return Ok(Scan {
            exists: false,
            files,
            bytes,
        });
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => return Err(MigrationError::io(format!("reading {}", dir.display()), e)),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() && !meta.is_symlink() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                bytes += meta.len();
                files.push((rel, meta.len()));
            }
        }
    }
    files.sort();
    Ok(Scan {
        exists: true,
        files,
        bytes,
    })
}

/// Recursive copy that preserves Unix permission bits.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .map_err(|e| MigrationError::io(format!("creating {}", dst.display()), e))?;

    let entries = fs::read_dir(src)
        .map_err(|e| MigrationError::io(format!("reading {}", src.display()), e))?;

    for entry in entries.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = entry
            .metadata()
            .map_err(|e| MigrationError::io(format!("stat {}", from.display()), e))?;

        if meta.is_dir() && !meta.is_symlink() {
            copy_tree(&from, &to)?;
        } else if meta.is_symlink() {
            #[cfg(unix)]
            {
                let target = fs::read_link(&from)
                    .map_err(|e| MigrationError::io(format!("readlink {}", from.display()), e))?;
                let _ = std::os::unix::fs::symlink(target, &to);
            }
            #[cfg(not(unix))]
            {
                let _ = fs::copy(&from, &to);
            }
        } else {
            fs::copy(&from, &to).map_err(|e| {
                MigrationError::io(format!("copying {} -> {}", from.display(), to.display()), e)
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode();
                let _ = fs::set_permissions(&to, fs::Permissions::from_mode(mode));
            }
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(src) {
            let _ = fs::set_permissions(dst, fs::Permissions::from_mode(meta.permissions().mode()));
        }
    }

    Ok(())
}

/// Confirm every source file exists at the destination with the same size.
///
/// Size equality is not a checksum, but it catches the failure that actually
/// happens in practice — a truncated copy from a full disk — at a fraction of
/// the cost of hashing a whole board.
fn verify(src: &Path, dst: &Path) -> Result<()> {
    let a = scan(src)?;
    let b = scan(dst)?;

    for (rel, size) in &a.files {
        match b.files.iter().find(|(r, _)| r == rel) {
            None => {
                return Err(MigrationError::VerificationFailed(format!(
                    "{rel} is missing from the copy"
                )))
            }
            Some((_, copied)) if copied != size => {
                return Err(MigrationError::VerificationFailed(format!(
                    "{rel} is {copied} bytes but should be {size}"
                )))
            }
            Some(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn seed(dir: &Path) {
        write(&dir.join("tasks/a.toml"), "id = 1\n");
        write(&dir.join("tasks/b.toml"), "id = 2\n");
        write(&dir.join("roadmap.toml"), "features = []\n");
    }

    #[test]
    fn migrates_and_removes_source() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("external/proj");
        let to = tmp.path().join("repo/.slashit");
        seed(&from);

        let report = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap();

        assert!(matches!(
            report.outcome,
            MigrationOutcome::Migrated { files: 3, .. }
        ));
        assert!(!from.exists(), "source must be gone after success");
        assert_eq!(fs::read_to_string(to.join("tasks/a.toml")).unwrap(), "id = 1\n");
        assert!(to.join(STATE_MARKER).is_file(), "marker identifies our dir");
    }

    #[test]
    fn migrates_back_the_other_direction() {
        let tmp = TempDir::new().unwrap();
        let inproj = tmp.path().join("repo/.slashit");
        let external = tmp.path().join("data/projects/repo-abc12345");
        seed(&inproj);

        StateMigrator::migrate(&inproj, &external, ConflictPolicy::Abort).unwrap();
        assert!(!inproj.exists());
        assert!(external.join("roadmap.toml").is_file());

        // And round-trips.
        StateMigrator::migrate(&external, &inproj, ConflictPolicy::Abort).unwrap();
        assert!(!external.exists());
        assert!(inproj.join("roadmap.toml").is_file());
    }

    #[test]
    fn empty_source_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        fs::create_dir_all(&from).unwrap();

        let report = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap();
        assert_eq!(report.outcome, MigrationOutcome::NothingToDo);
        assert!(to.is_dir(), "destination is prepared even when empty");
    }

    #[test]
    fn missing_source_is_a_noop_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("nope");
        let to = tmp.path().join("to");

        for _ in 0..3 {
            let r = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap();
            assert_eq!(r.outcome, MigrationOutcome::NothingToDo);
        }
    }

    #[test]
    fn conflicting_files_abort_without_touching_anything() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        seed(&from);
        write(&to.join("tasks/a.toml"), "id = 999\n");

        let err = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap_err();
        assert!(matches!(err, MigrationError::Conflict(_)));

        // Both sides survive, byte for byte.
        assert_eq!(fs::read_to_string(from.join("tasks/a.toml")).unwrap(), "id = 1\n");
        assert_eq!(fs::read_to_string(to.join("tasks/a.toml")).unwrap(), "id = 999\n");
    }

    #[test]
    fn prefer_destination_archives_the_source() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        seed(&from);
        write(&to.join("tasks/a.toml"), "id = 999\n");

        let report =
            StateMigrator::migrate(&from, &to, ConflictPolicy::PreferDestination).unwrap();
        match report.outcome {
            MigrationOutcome::KeptDestination { archived_source } => {
                let archived = archived_source.expect("source archived, never deleted");
                assert!(archived.join("tasks/a.toml").is_file());
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(fs::read_to_string(to.join("tasks/a.toml")).unwrap(), "id = 999\n");
        assert!(!from.exists(), "source was moved aside");
    }

    #[test]
    fn prefer_source_archives_the_destination() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        seed(&from);
        write(&to.join("tasks/a.toml"), "id = 999\n");

        StateMigrator::migrate(&from, &to, ConflictPolicy::PreferSource).unwrap();

        assert_eq!(fs::read_to_string(to.join("tasks/a.toml")).unwrap(), "id = 1\n");
        assert!(
            to.with_file_name("to.superseded").join("tasks/a.toml").is_file(),
            "old destination is recoverable"
        );
    }

    #[test]
    fn non_conflicting_destination_entries_are_preserved() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        seed(&from);
        write(&to.join("notes.md"), "keep me\n");

        let report = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap();
        assert!(matches!(report.outcome, MigrationOutcome::Migrated { .. }));
        assert_eq!(fs::read_to_string(to.join("notes.md")).unwrap(), "keep me\n");
        assert!(to.join("tasks/a.toml").is_file());
    }

    #[test]
    fn a_held_lock_blocks_a_second_migration() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        seed(&from);

        let _held = MigrationLock::acquire(lock_path_for(&to)).unwrap();
        let err = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap_err();
        assert!(matches!(err, MigrationError::Locked(_)));
        assert!(from.join("tasks/a.toml").is_file(), "source untouched");
    }

    #[test]
    fn lock_is_released_on_drop() {
        let tmp = TempDir::new().unwrap();
        let to = tmp.path().join("to");
        let p = lock_path_for(&to);
        {
            let _l = MigrationLock::acquire(p.clone()).unwrap();
            assert!(p.exists());
        }
        assert!(!p.exists(), "drop must release");
        MigrationLock::acquire(p).unwrap();
    }

    #[test]
    fn staging_leftovers_are_swept() {
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path();
        let junk = parent.join(format!("{STAGING_PREFIX}4242"));
        fs::create_dir_all(junk.join("tasks")).unwrap();
        fs::write(junk.join("tasks/x.toml"), "x").unwrap();
        let keep = parent.join("real-state");
        fs::create_dir_all(&keep).unwrap();

        assert_eq!(StateMigrator::sweep_staging(parent), 1);
        assert!(!junk.exists());
        assert!(keep.exists(), "sweep must not touch real directories");
    }

    #[test]
    fn interrupted_migration_leaves_source_intact() {
        // Simulate a crash between copy and rename: staging exists, source is
        // still there, destination was never created.
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("dest/state");
        seed(&from);
        let staging = to.parent().unwrap().join(format!("{STAGING_PREFIX}999"));
        copy_tree(&from, &staging).unwrap();

        assert!(from.join("tasks/a.toml").is_file());
        assert!(!to.exists());

        // Recovery sweeps staging, then the retry succeeds cleanly.
        StateMigrator::sweep_staging(to.parent().unwrap());
        assert!(!staging.exists());

        let r = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap();
        assert!(matches!(r.outcome, MigrationOutcome::Migrated { files: 3, .. }));
    }

    #[test]
    fn verification_catches_a_truncated_copy() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        seed(&src);
        copy_tree(&src, &dst).unwrap();
        verify(&src, &dst).unwrap();

        fs::write(dst.join("roadmap.toml"), "").unwrap();
        let err = verify(&src, &dst).unwrap_err();
        assert!(matches!(err, MigrationError::VerificationFailed(_)));
    }

    #[test]
    fn verification_catches_a_missing_file() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        seed(&src);
        copy_tree(&src, &dst).unwrap();
        fs::remove_file(dst.join("tasks/b.toml")).unwrap();

        assert!(matches!(
            verify(&src, &dst).unwrap_err(),
            MigrationError::VerificationFailed(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn permissions_are_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        seed(&from);
        let secretish = from.join("tasks/a.toml");
        fs::set_permissions(&secretish, fs::Permissions::from_mode(0o600)).unwrap();

        StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap();

        let mode = fs::metadata(to.join("tasks/a.toml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "restrictive modes must survive the move");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_source_fails_without_destroying_anything() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc_geteuid() } == 0 {
            return; // root ignores permission bits
        }
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        seed(&from);
        let locked = from.join("tasks");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let result = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort);

        // Restore before asserting so the tempdir can always be cleaned up.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err(), "must not report success it did not achieve");
        assert!(from.join("tasks/a.toml").is_file(), "source intact");
        assert!(!to.join("tasks/a.toml").exists(), "no partial state installed");
    }

    #[cfg(unix)]
    extern "C" {
        #[link_name = "geteuid"]
        fn libc_geteuid() -> u32;
    }

    #[test]
    fn empty_legacy_dir_is_removed_but_populated_one_is_not() {
        let tmp = TempDir::new().unwrap();

        let empty = tmp.path().join("a/.slashit");
        fs::create_dir_all(&empty).unwrap();
        assert!(StateMigrator::remove_empty_legacy_dir(&empty));
        assert!(!empty.exists());

        let populated = tmp.path().join("b/.slashit");
        fs::create_dir_all(&populated).unwrap();
        fs::write(populated.join("tasks.toml"), "x").unwrap();
        assert!(!StateMigrator::remove_empty_legacy_dir(&populated));
        assert!(populated.join("tasks.toml").is_file(), "user data survives");
    }

    #[test]
    fn plan_reports_both_sides_without_mutating() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        seed(&from);
        write(&to.join("tasks/a.toml"), "conflict\n");

        let plan = StateMigrator::plan(&from, &to).unwrap();
        assert!(plan.source_exists && plan.destination_exists);
        assert_eq!(plan.source_file_count, 3);
        assert!(plan.requires_resolution);
        assert_eq!(plan.conflicts, vec!["tasks/a.toml".to_string()]);
        assert!(plan.source_bytes > 0);
        // Nothing changed on disk.
        assert!(from.join("tasks/b.toml").is_file());
        assert_eq!(fs::read_to_string(to.join("tasks/a.toml")).unwrap(), "conflict\n");
    }

    #[test]
    fn nested_trees_and_symlinks_survive() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        write(&from.join("deep/a/b/c/leaf.toml"), "deep\n");
        #[cfg(unix)]
        std::os::unix::fs::symlink("leaf.toml", from.join("deep/a/b/c/link.toml")).unwrap();

        StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap();
        assert_eq!(
            fs::read_to_string(to.join("deep/a/b/c/leaf.toml")).unwrap(),
            "deep\n"
        );
        #[cfg(unix)]
        assert!(fs::symlink_metadata(to.join("deep/a/b/c/link.toml"))
            .unwrap()
            .is_symlink());
    }
}
