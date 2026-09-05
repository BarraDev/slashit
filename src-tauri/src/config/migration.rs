//! Moving a project's shareable state between external and in-project storage.
//!
//! Changing [`StateLocation`] must move data, not just repoint a setting —
//! otherwise a user who flips the toggle silently loses their board. The
//! ordering below is chosen so that an interruption at *any* point leaves at
//! least one full, recoverable copy of both the source and the pre-migration
//! destination on disk:
//!
//! 1. take an exclusive lock so nothing else writes during the move;
//! 2. resolve any leftover state a previous, interrupted [`StateMigrator::migrate`]
//!    call left beside this exact destination (see [`StateMigrator::recover`]).
//!    This runs *before* planning, so retrying a migration after a crash can
//!    never silently start fresh and ignore data stranded in a retired copy;
//! 3. plan and detect conflicts *under the lock, after recovery* — this is the
//!    authoritative plan, not the read-only preview [`StateMigrator::plan`]
//!    returns to the UI before the lock is held. A conflict that appears
//!    between the preview and this point — including one recovery itself just
//!    surfaced by reinstating a destination the preview never saw — is caught
//!    here and handled by the same policy as any other conflict; it can never
//!    silently fall through as source-wins;
//! 4. copy the source into a staging directory beside the destination;
//! 5. verify the copy against the source, file by file. This check only
//!    covers the source's data; it says nothing yet about any destination-only
//!    data carried over in the next step;
//! 6. if the destination already holds non-conflicting data, *copy* (never
//!    move) it into staging too, so the destination itself is untouched by
//!    this step. A collision or copy failure here aborts the whole migration
//!    rather than downgrading to a warning — there is no separate
//!    post-copy verification of this step, so its safety comes from that
//!    fail-closed error handling, not from a checksum;
//! 7. retire the destination (rename aside) and atomically rename staging
//!    into its place;
//! 8. only after that swap has succeeded, delete the source and the retired
//!    destination.
//!
//! Steps 4–6 build a scratch copy in staging while leaving both the source
//! and the destination fully intact, so an interruption before step 7 leaves
//! nothing to recover: staging is pure scratch and can always be deleted.
//! An interruption *during* step 7 — after the destination has been retired
//! but before staging has replaced it — is the one window where the retired
//! copy is briefly the only surviving copy of pre-migration destination data.
//! [`StateMigrator::recover`] resolves that window deterministically by
//! pairing each staging directory with its retired counterpart instead of
//! treating every `.slashit-migrating-*` directory as equally disposable —
//! and step 2 above calls it automatically, so nothing else needs to invoke
//! it for this invariant to hold.
//!
//! The copy-then-rename shape also gives cross-filesystem support for free.
//! `fs::rename` cannot move between mounts (`EXDEV`), and external state
//! (`~/.local/share`) very often lives on a different filesystem from a
//! project. Since we copy anyway, the only rename is staging → destination
//! *within one parent directory*, which is always the same filesystem.

use super::paths::{is_valid_in_project_dir, sanitize_component, STATE_MARKER};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// State was copied (the source portion checked file-by-file against the
    /// source; any destination-only carry-over instead relies on the copy
    /// call itself failing closed) and the source removed.
    Migrated { files: usize, bytes: u64 },
    /// Destination already held the state and the policy kept it.
    KeptDestination { archived_source: Option<PathBuf> },
}

/// What [`StateMigrator::recover`] did about one leftover `.slashit-migrating-*`
/// transaction it found beside a destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// An incomplete scratch copy with no retired counterpart was deleted;
    /// the swap never started, so nothing but scratch work was touched.
    RemovedIncompleteScratch,
    /// The destination already reflects a completed swap; the stale retired
    /// copy was deleted.
    RemovedStaleRetired,
    /// A swap whose scratch copy had already finished copying (the source
    /// portion checked against the source; any destination-only carry-over
    /// present only if its copy succeeded, since a failure would have
    /// aborted before retiring `to`), but crashed before installing it, was
    /// completed here.
    CompletedSwap,
    /// The destination was missing entirely and no scratch copy remained to
    /// re-install; the retired copy was restored as the destination.
    RestoredFromRetired,
    /// Both a scratch and a retired copy exist for a transaction whose
    /// destination is also already present. Left untouched — completing or
    /// discarding either would be a guess, not a recovery.
    LeftForManualReview,
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

/// Suffix used for staging directories. Recognised by [`StateMigrator::recover`].
///
/// Followed by a destination tag and a transaction id (see
/// [`destination_tag`] and [`next_transaction_id`]): `{STAGING_PREFIX}{tag}-{txn}`
/// for a scratch copy, `{STAGING_PREFIX}old-{tag}-{txn}` for a retired one. The
/// tag scopes both forms to one exact destination, so two destinations that
/// happen to share a parent directory (siblings under the same project key,
/// for instance) can never collide, and recovering one destination can never
/// touch state left behind by a migration to a different one.
const STAGING_PREFIX: &str = ".slashit-migrating-";
const LOCK_SUFFIX: &str = ".slashit-migrate.lock";

pub struct StateMigrator;

impl StateMigrator {
    /// Inspect both locations without touching anything.
    pub fn plan(from: &Path, to: &Path) -> Result<MigrationPlan> {
        let source = scan(from)?;
        let dest = scan(to)?;

        // A conflict is any relative path that exists on both sides and
        // isn't a directory on both sides. Two directories at the same path
        // are not a conflict — they get merged by recursing, and any real
        // collision inside them shows up as its own entry. A directory
        // colliding with a file (in either direction) is a conflict even
        // though neither side's *file* list contains the directory's path
        // itself, which is exactly the case a flat file-list comparison
        // would silently miss.
        let conflicts: Vec<String> = source
            .entries
            .iter()
            .filter_map(|(rel, source_is_dir)| {
                dest.entries.iter().find(|(d, _)| d == rel).and_then(
                    |(_, dest_is_dir)| {
                        if *source_is_dir && *dest_is_dir {
                            None
                        } else {
                            Some(rel.clone())
                        }
                    },
                )
            })
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
    ///
    /// The lock is acquired *before* the authoritative plan is computed, so
    /// nothing below this point ever acts on a plan that could be stale by
    /// the time it's used. [`StateMigrator::plan`] remains available on its
    /// own as an unlocked, read-only preview for the UI.
    pub fn migrate(from: &Path, to: &Path, policy: ConflictPolicy) -> Result<MigrationReport> {
        let mut warnings = Vec::new();

        if from == to {
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

        // Resolve any leftover `.slashit-migrating-*` state for this exact
        // destination before planning anything. A previous `migrate` call
        // that crashed between retiring `to` and installing staging in its
        // place can leave the only safe copy of pre-crash destination data
        // sitting in a retired directory that nothing else will ever look
        // at; recovering it here — under the lock, before `plan` — means a
        // retry can never silently start a fresh migration that ignores it.
        for outcome in Self::recover(to) {
            warnings.push(format!("resolved an interrupted previous migration: {outcome:?}"));
        }

        // Authoritative: computed under the lock and after recovery, so a
        // conflict that only appeared after an earlier UI preview — or after
        // recovery just reinstated a destination the preview never saw — is
        // caught here and handled by `policy` like any other conflict, never
        // silently ignored.
        let plan = Self::plan(from, to)?;

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

        // Scoped to this exact destination and this exact call, so a second
        // migration running concurrently for a sibling destination under the
        // same parent — or a retry of this same destination after a crash —
        // can never collide on the same staging/retired names.
        let dest_tag = destination_tag(to);
        let txn = next_transaction_id();

        let staging = parent.join(format!("{STAGING_PREFIX}{dest_tag}-{txn}"));
        // A staging dir from a previous crashed run is always disposable —
        // recovery above already handled anything worth keeping for this
        // destination, before this fresh transaction id even existed.
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

        // Merge case: destination exists but had no conflicting entries under
        // the authoritative plan above. Copy (never move) its surviving
        // entries into staging, so `to` itself stays untouched — and fully
        // recoverable — until the swap below has succeeded. A failure here
        // aborts the migration with both `from` and `to` still intact.
        if to.exists() {
            if let Err(e) = merge_into(to, &staging) {
                let _ = fs::remove_dir_all(&staging);
                return Err(e);
            }
            let retired = parent.join(format!("{STAGING_PREFIX}old-{dest_tag}-{txn}"));
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

        // The destination is now complete: the source portion was checked
        // file-by-file above, and any destination-only carry-over is present
        // because `merge_into` returned `Ok` rather than a fail-closed error.
        // Only now is the source expendable.
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

    /// Resolve any leftover `.slashit-migrating-*` state for the destination
    /// `to`, left behind by a crash during a previous [`Self::migrate`] call.
    ///
    /// [`Self::migrate`] calls this itself, under its lock and before it
    /// plans, so a caller never needs to invoke it separately for the
    /// crash-recovery invariant to hold. It is also exposed here as a public
    /// method so it can be run proactively (for example, once at startup for
    /// every known destination) without waiting for a retry.
    ///
    /// Only leftovers tagged for this exact `to` are ever considered (see
    /// [`destination_tag`]) — a sibling destination sharing the same parent
    /// directory has its own tag, so its leftovers are never touched here.
    /// Within that scope, every leftover also carries a transaction id (see
    /// [`next_transaction_id`]): a scratch copy at
    /// `{STAGING_PREFIX}{tag}-{txn}`, and — only if the crash happened after
    /// the destination was retired but before staging replaced it — a retired
    /// copy of the pre-migration destination at
    /// `{STAGING_PREFIX}old-{tag}-{txn}`. Those two are paired up by
    /// transaction id rather than treated as interchangeable disposable junk,
    /// because the retired copy is the *only* surviving copy of
    /// destination-only data during that narrow window:
    ///
    /// - scratch only: the swap never started; the scratch copy is pure
    ///   working state and is deleted.
    /// - scratch and retired, destination missing: crashed mid-swap after
    ///   both copy steps had already succeeded. The scratch copy is exactly
    ///   what would have been installed, so the swap is completed here.
    /// - scratch and retired, destination present: another run must have
    ///   already finished (or something wrote a fresh destination since).
    ///   Ambiguous — left untouched rather than guessed at.
    /// - retired only, destination present: the swap already succeeded and
    ///   the final cleanup step just didn't run; the retired copy is stale
    ///   and is deleted.
    /// - retired only, destination missing: the swap never completed and
    ///   nothing else survived it. The retired copy is restored as the
    ///   destination rather than deleted.
    pub fn recover(to: &Path) -> Vec<RecoveryOutcome> {
        let Some(parent) = to.parent() else {
            return Vec::new();
        };
        let Ok(dir) = fs::read_dir(parent) else {
            return Vec::new();
        };

        // Scoped to this exact destination: a sibling destination sharing
        // `parent` has its own, different tag, so its leftovers never match
        // either prefix below and are left completely untouched — recovering
        // one destination can never act on state stranded by another.
        let tag = destination_tag(to);
        let old_prefix = format!("{STAGING_PREFIX}old-{tag}-");
        let scratch_prefix = format!("{STAGING_PREFIX}{tag}-");
        let mut scratch = std::collections::BTreeMap::new();
        let mut retired = std::collections::BTreeMap::new();
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(txn) = name.strip_prefix(&old_prefix) {
                retired.insert(txn.to_string(), entry.path());
            } else if let Some(txn) = name.strip_prefix(&scratch_prefix) {
                scratch.insert(txn.to_string(), entry.path());
            }
        }

        // A lock left behind by a crashed process is otherwise only cleaned
        // up lazily, the next time something tries to acquire it (see
        // `MigrationLock::acquire`). Clear a stale one eagerly too, so a
        // destination nobody is actively migrating doesn't sit locked.
        let lock = lock_path_for(to);
        let stale_lock = fs::metadata(&lock)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or_default().as_secs() > 300)
            .unwrap_or(false);
        if stale_lock {
            let _ = fs::remove_file(&lock);
        }

        let mut txns: Vec<String> = scratch.keys().chain(retired.keys()).cloned().collect();
        txns.sort();
        txns.dedup();

        txns.into_iter()
            .map(|txn| resolve_one(to, scratch.get(&txn), retired.get(&txn)))
            .collect()
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

/// Discriminator for `to`'s staging/retired directory names, scoping them to
/// this exact destination among any siblings sharing its parent.
///
/// `to.file_name()` is already unique among those siblings — no two distinct
/// paths in one parent can share a name — so using it directly would already
/// prevent cross-destination collisions. But sanitising it for filesystem
/// safety can only ever *introduce* a collision between two different names
/// that sanitise to the same string, never remove one, so — exactly like
/// [`super::paths::worktree_dir_name`] does for branch names — a short hash of
/// the original is appended whenever sanitising actually changed it.
fn destination_tag(to: &Path) -> String {
    let name = to
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "dest".to_string());
    let sanitized = sanitize_component(&name, "dest");
    if sanitized == name {
        return sanitized;
    }
    let digest = Sha256::digest(name.as_bytes());
    format!("{sanitized}-{}", hex::encode(&digest[..3]))
}

/// A discriminator for one [`StateMigrator::migrate`] call, unique enough that
/// two calls racing for the same destination's staging slot never collide.
///
/// Deliberately not just `std::process::id()`: two migrations to *different*
/// destinations can run concurrently in the same process (and therefore share
/// a pid), and a pid can in principle be reused across process restarts. The
/// nanosecond timestamp and an atomic counter are the actual uniqueness
/// guarantees; the pid is included only to make a leftover directory easier to
/// trace back to the process that created it.
fn next_transaction_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos:x}-{n}", std::process::id())
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

/// Decide and act on the fate of one transaction's leftovers, per
/// [`StateMigrator::recover`]'s documented cases.
fn resolve_one(to: &Path, scratch: Option<&PathBuf>, retired: Option<&PathBuf>) -> RecoveryOutcome {
    match (scratch, retired) {
        (Some(scratch), None) => {
            let _ = fs::remove_dir_all(scratch);
            RecoveryOutcome::RemovedIncompleteScratch
        }
        (None, Some(retired)) => {
            if to.exists() {
                let _ = fs::remove_dir_all(retired);
                RecoveryOutcome::RemovedStaleRetired
            } else if fs::rename(retired, to).is_ok() {
                RecoveryOutcome::RestoredFromRetired
            } else {
                RecoveryOutcome::LeftForManualReview
            }
        }
        (Some(scratch), Some(retired)) => {
            if to.exists() {
                RecoveryOutcome::LeftForManualReview
            } else if fs::rename(scratch, to).is_ok() {
                let _ = fs::remove_dir_all(retired);
                RecoveryOutcome::CompletedSwap
            } else {
                RecoveryOutcome::LeftForManualReview
            }
        }
        (None, None) => unreachable!("resolve_one called for a transaction with no leftovers"),
    }
}

/// Copy destination-only entries from `dst_original` into `staging`, leaving
/// `dst_original` completely untouched.
///
/// This must be a copy, never a move: `dst_original` (the pre-migration
/// destination) has to remain fully recoverable at its original path until
/// the caller has successfully swapped `staging` into place. A collision that
/// isn't a directory on both sides, or a copy that fails partway, aborts the
/// whole migration instead of silently dropping the destination's data —
/// `plan()` should already have surfaced any such collision as a conflict
/// before this runs, so reaching one here means the filesystem changed under
/// us and the safe response is to fail closed, not guess.
fn merge_into(dst_original: &Path, staging: &Path) -> Result<()> {
    let entries = fs::read_dir(dst_original)
        .map_err(|e| MigrationError::io(format!("reading {}", dst_original.display()), e))?;
    for entry in entries.flatten() {
        let from = entry.path();
        let target = staging.join(entry.file_name());
        let meta = entry
            .metadata()
            .map_err(|e| MigrationError::io(format!("stat {}", from.display()), e))?;

        match fs::symlink_metadata(&target) {
            Ok(target_meta) => {
                if meta.is_dir() && !meta.is_symlink() && target_meta.file_type().is_dir() {
                    merge_into(&from, &target)?;
                    continue;
                }
                return Err(MigrationError::Conflict(format!(
                    "{} collides with already-migrated source content",
                    from.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(MigrationError::io(format!("stat {}", target.display()), e)),
        }

        if meta.is_dir() && !meta.is_symlink() {
            copy_tree(&from, &target)?;
        } else {
            copy_entry(&from, &target, &meta)?;
        }
    }
    Ok(())
}

struct Scan {
    exists: bool,
    files: Vec<(String, u64)>,
    /// Every filesystem node under the root, both directories and leaves, as
    /// (relative path, is_dir). Used by [`StateMigrator::plan`] to detect
    /// type collisions that a leaf-only file list would miss.
    entries: Vec<(String, bool)>,
    bytes: u64,
}

/// Recursively list regular files as (relative path, size).
///
/// Symlinks are recorded but not followed, so a symlink loop inside a project
/// cannot hang the scan and a symlink pointing outside cannot cause the copy to
/// pull in unrelated data.
fn scan(root: &Path) -> Result<Scan> {
    let mut files = Vec::new();
    let mut entries = Vec::new();
    let mut bytes = 0u64;
    if !root.exists() {
        return Ok(Scan {
            exists: false,
            files,
            entries,
            bytes,
        });
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => return Err(MigrationError::io(format!("reading {}", dir.display()), e)),
        };
        for entry in read.flatten() {
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            if meta.is_dir() && !meta.is_symlink() {
                entries.push((rel, true));
                stack.push(path);
            } else {
                bytes += meta.len();
                entries.push((rel.clone(), false));
                files.push((rel, meta.len()));
            }
        }
    }
    files.sort();
    entries.sort();
    Ok(Scan {
        exists: true,
        files,
        entries,
        bytes,
    })
}

/// Copy one non-directory entry (regular file or symlink), preserving Unix
/// permission bits. Fails closed: a symlink that can't be recreated is an
/// error here, not a silently-dropped entry, so callers that need "never
/// destroy data" semantics (like [`merge_into`]) can rely on `?` alone.
fn copy_entry(from: &Path, to: &Path, meta: &fs::Metadata) -> Result<()> {
    if meta.is_symlink() {
        #[cfg(unix)]
        {
            let target = fs::read_link(from)
                .map_err(|e| MigrationError::io(format!("readlink {}", from.display()), e))?;
            std::os::unix::fs::symlink(target, to).map_err(|e| {
                MigrationError::io(format!("symlinking {}", to.display()), e)
            })?;
        }
        #[cfg(not(unix))]
        {
            fs::copy(from, to).map_err(|e| {
                MigrationError::io(format!("copying {} -> {}", from.display(), to.display()), e)
            })?;
        }
    } else {
        fs::copy(from, to).map_err(|e| {
            MigrationError::io(format!("copying {} -> {}", from.display(), to.display()), e)
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode();
            let _ = fs::set_permissions(to, fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
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
        } else {
            copy_entry(&from, &to, &meta)?;
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

    /// Build a scratch/retired directory name for `to`, the way
    /// [`StateMigrator::migrate`] and [`StateMigrator::recover`] do, so tests
    /// can plant leftovers under a fake, easy-to-read transaction id instead
    /// of duplicating the real (pid + nanosecond) generator.
    fn scratch_dir_for(to: &Path, txn: &str) -> PathBuf {
        to.parent()
            .unwrap()
            .join(format!("{STAGING_PREFIX}{}-{txn}", destination_tag(to)))
    }

    fn retired_dir_for(to: &Path, txn: &str) -> PathBuf {
        to.parent()
            .unwrap()
            .join(format!("{STAGING_PREFIX}old-{}-{txn}", destination_tag(to)))
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
    fn nested_non_conflicting_destination_entries_are_preserved() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        seed(&from);
        write(&to.join("tasks/keep-me.toml"), "keep me\n");

        let report = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap();
        assert!(matches!(report.outcome, MigrationOutcome::Migrated { .. }));
        assert_eq!(
            fs::read_to_string(to.join("tasks/keep-me.toml")).unwrap(),
            "keep me\n"
        );
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
    fn incomplete_scratch_with_no_retired_counterpart_is_removed() {
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path();
        let to = parent.join("state");
        let junk = scratch_dir_for(&to, "4242");
        fs::create_dir_all(junk.join("tasks")).unwrap();
        fs::write(junk.join("tasks/x.toml"), "x").unwrap();
        let keep = parent.join("real-state");
        fs::create_dir_all(&keep).unwrap();

        let outcomes = StateMigrator::recover(&to);
        assert_eq!(outcomes, vec![RecoveryOutcome::RemovedIncompleteScratch]);
        assert!(!junk.exists());
        assert!(keep.exists(), "recovery must not touch unrelated directories");
    }

    #[test]
    fn interrupted_migration_leaves_source_intact() {
        // Simulate a crash between copy and rename: staging exists, source is
        // still there, destination was never created.
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("dest/state");
        seed(&from);
        let staging = scratch_dir_for(&to, "999");
        copy_tree(&from, &staging).unwrap();

        assert!(from.join("tasks/a.toml").is_file());
        assert!(!to.exists());

        // Recovery removes the orphaned scratch copy, then the retry
        // succeeds cleanly.
        StateMigrator::recover(&to);
        assert!(!staging.exists());

        let r = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap();
        assert!(matches!(r.outcome, MigrationOutcome::Migrated { files: 3, .. }));
    }

    #[test]
    fn recovery_completes_a_swap_interrupted_between_retire_and_install() {
        // Both copy steps already succeeded and `to` was retired, but the
        // process died before staging could be renamed into place.
        let tmp = TempDir::new().unwrap();
        let to = tmp.path().join("dest/state");
        fs::create_dir_all(to.parent().unwrap()).unwrap();

        let scratch = scratch_dir_for(&to, "777");
        write(&scratch.join("roadmap.toml"), "new\n");
        let retired = retired_dir_for(&to, "777");
        write(&retired.join("roadmap.toml"), "old\n");

        let outcomes = StateMigrator::recover(&to);
        assert_eq!(outcomes, vec![RecoveryOutcome::CompletedSwap]);
        assert!(!scratch.exists());
        assert!(!retired.exists());
        assert_eq!(
            fs::read_to_string(to.join("roadmap.toml")).unwrap(),
            "new\n",
            "the already-fully-copied scratch content must be what gets installed"
        );
    }

    #[test]
    fn retrying_a_migration_recovers_a_stranded_swap_before_starting_a_new_one() {
        // Reproduces a full crash-and-retry cycle end to end through
        // `migrate` itself, not just through `recover`: a previous call to
        // `migrate(&from, &to, ..)` fully copied and merged its candidate
        // into staging and retired the original `to`, then the process died
        // before installing staging as the new `to`. `from` (the configured
        // source) is still there, `to` is still missing, and the caller
        // retries the exact same migration. The stranded swap must be
        // installed first, not silently ignored by a fresh migration that
        // only ever looks at `from` and an empty `to`.
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("dest/state");
        seed(&from);

        fs::create_dir_all(to.parent().unwrap()).unwrap();
        let scratch = scratch_dir_for(&to, "9001");
        write(&scratch.join("recovered.toml"), "from the interrupted swap\n");
        let retired = retired_dir_for(&to, "9001");
        fs::create_dir_all(&retired).unwrap();

        assert!(!to.exists(), "destination is missing, as after the crash");

        let report = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap();

        assert!(matches!(report.outcome, MigrationOutcome::Migrated { .. }));
        assert_eq!(
            fs::read_to_string(to.join("recovered.toml")).unwrap(),
            "from the interrupted swap\n",
            "a stranded swap must be installed before the retried migration runs"
        );
        assert!(
            to.join("tasks/a.toml").is_file(),
            "the retried migration's own data must still land on top of the recovered state"
        );
        assert!(!from.exists(), "source removed only after everything landed");
        assert!(!scratch.exists());
        assert!(!retired.exists());
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("resolved an interrupted previous migration")),
            "recovery must be visible in the report, not silent"
        );
    }

    #[test]
    fn two_destinations_sharing_a_parent_get_independent_staging_names() {
        // Two sibling projects under one project key, migrating at the same
        // time (same process, so the same pid) must never compute the same
        // staging directory name for each other.
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path().join("data/projects/repo-abc12345");
        let to_a = parent.join("11111111-1111-1111-1111-111111111111");
        let to_b = parent.join("22222222-2222-2222-2222-222222222222");
        let from_a = tmp.path().join("from-a");
        let from_b = tmp.path().join("from-b");
        seed(&from_a);
        seed(&from_b);

        let report_a = StateMigrator::migrate(&from_a, &to_a, ConflictPolicy::Abort).unwrap();
        let report_b = StateMigrator::migrate(&from_b, &to_b, ConflictPolicy::Abort).unwrap();

        assert!(matches!(report_a.outcome, MigrationOutcome::Migrated { .. }));
        assert!(matches!(report_b.outcome, MigrationOutcome::Migrated { .. }));
        assert!(to_a.join("tasks/a.toml").is_file());
        assert!(to_b.join("tasks/a.toml").is_file());
        assert_ne!(
            destination_tag(&to_a),
            destination_tag(&to_b),
            "distinct destinations must never share a tag"
        );

        // Nothing left behind for either destination once both succeed.
        let leftovers: Vec<_> = fs::read_dir(&parent)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "no scratch or retired dirs should survive success");
    }

    #[test]
    fn recovery_for_one_destination_ignores_a_stranded_swap_belonging_to_a_sibling() {
        // Destination B has a fully-stranded swap (scratch + retired, B
        // itself missing) sitting right beside destination A, which shares
        // the same parent directory. Recovering A must not touch, complete,
        // or otherwise notice B's leftovers.
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path().join("data/projects/repo-abc12345");
        fs::create_dir_all(&parent).unwrap();
        let to_a = parent.join("aaaaaaaa-0000-0000-0000-000000000000");
        let to_b = parent.join("bbbbbbbb-0000-0000-0000-000000000000");

        write(&to_a.join("roadmap.toml"), "a is already installed\n");

        let b_scratch = scratch_dir_for(&to_b, "42");
        write(&b_scratch.join("roadmap.toml"), "b candidate\n");
        let b_retired = retired_dir_for(&to_b, "42");
        fs::create_dir_all(&b_retired).unwrap();
        assert!(!to_b.exists(), "b's destination is missing, as after a crash");

        let outcomes = StateMigrator::recover(&to_a);

        assert!(
            outcomes.is_empty(),
            "recovering a's destination must find nothing of its own to resolve"
        );
        assert!(b_scratch.exists(), "b's stranded scratch must survive untouched");
        assert!(b_retired.exists(), "b's stranded retired copy must survive untouched");
        assert!(!to_b.exists(), "b's destination must not be silently installed by a's recovery");
        assert_eq!(
            fs::read_to_string(to_a.join("roadmap.toml")).unwrap(),
            "a is already installed\n",
            "a's own state must be untouched"
        );

        // Recovering B afterward still works normally — this destination's
        // own recovery path isn't broken by A's parent-sharing presence.
        let b_outcomes = StateMigrator::recover(&to_b);
        assert_eq!(b_outcomes, vec![RecoveryOutcome::CompletedSwap]);
        assert_eq!(
            fs::read_to_string(to_b.join("roadmap.toml")).unwrap(),
            "b candidate\n"
        );
    }

    #[test]
    fn recovery_removes_a_stale_retired_copy_once_the_swap_already_succeeded() {
        let tmp = TempDir::new().unwrap();
        let to = tmp.path().join("dest/state");
        write(&to.join("roadmap.toml"), "installed\n");

        let retired = retired_dir_for(&to, "555");
        write(&retired.join("roadmap.toml"), "stale\n");

        let outcomes = StateMigrator::recover(&to);
        assert_eq!(outcomes, vec![RecoveryOutcome::RemovedStaleRetired]);
        assert!(!retired.exists());
        assert_eq!(
            fs::read_to_string(to.join("roadmap.toml")).unwrap(),
            "installed\n"
        );
    }

    #[test]
    fn recovery_restores_the_destination_from_a_retired_copy_when_nothing_else_survived() {
        // `to` is missing entirely and no scratch copy remains: the retired
        // copy is the only safe data left, so it must be restored, not
        // deleted.
        let tmp = TempDir::new().unwrap();
        let to = tmp.path().join("dest/state");
        let retired = retired_dir_for(&to, "321");
        write(&retired.join("roadmap.toml"), "only copy left\n");

        let outcomes = StateMigrator::recover(&to);
        assert_eq!(outcomes, vec![RecoveryOutcome::RestoredFromRetired]);
        assert!(!retired.exists());
        assert_eq!(
            fs::read_to_string(to.join("roadmap.toml")).unwrap(),
            "only copy left\n"
        );
    }

    #[test]
    fn recovery_leaves_an_ambiguous_scratch_and_retired_pair_alone_when_destination_exists() {
        let tmp = TempDir::new().unwrap();
        let to = tmp.path().join("dest/state");
        write(&to.join("roadmap.toml"), "already here\n");

        let scratch = scratch_dir_for(&to, "111");
        write(&scratch.join("roadmap.toml"), "candidate\n");
        let retired = retired_dir_for(&to, "111");
        write(&retired.join("roadmap.toml"), "candidate-old\n");

        let outcomes = StateMigrator::recover(&to);
        assert_eq!(outcomes, vec![RecoveryOutcome::LeftForManualReview]);
        assert!(scratch.exists(), "nothing should be deleted when ambiguous");
        assert!(retired.exists(), "nothing should be deleted when ambiguous");
        assert_eq!(
            fs::read_to_string(to.join("roadmap.toml")).unwrap(),
            "already here\n"
        );
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

    #[test]
    fn type_mismatch_between_source_and_destination_is_a_conflict() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        // Source has a plain file named "notes"; destination has a directory
        // of the same name. A flat leaf-file comparison would never see
        // these collide, since the destination's only *file* entries are
        // nested inside "notes/", not "notes" itself.
        write(&from.join("notes"), "a note\n");
        write(&to.join("notes/inner.toml"), "nested\n");

        let plan = StateMigrator::plan(&from, &to).unwrap();
        assert!(plan.requires_resolution);
        assert_eq!(plan.conflicts, vec!["notes".to_string()]);

        let err = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap_err();
        assert!(matches!(err, MigrationError::Conflict(_)));
        assert_eq!(fs::read_to_string(from.join("notes")).unwrap(), "a note\n");
        assert!(to.join("notes/inner.toml").is_file());
    }

    #[test]
    fn conflict_appearing_after_preview_is_caught_by_revalidation() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        seed(&from);

        let preview = StateMigrator::plan(&from, &to).unwrap();
        assert!(
            !preview.requires_resolution,
            "no conflict exists yet at preview time"
        );

        // Simulate another writer landing on the destination in the window
        // between the UI showing this preview and the user confirming it.
        write(&to.join("tasks/a.toml"), "raced in\n");

        let err = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap_err();
        assert!(
            matches!(err, MigrationError::Conflict(_)),
            "a stale preview must never let a new conflict through as source-wins"
        );
        assert_eq!(fs::read_to_string(from.join("tasks/a.toml")).unwrap(), "id = 1\n");
        assert_eq!(fs::read_to_string(to.join("tasks/a.toml")).unwrap(), "raced in\n");
    }

    #[test]
    fn merge_into_copies_destination_only_entries_without_touching_the_original() {
        let tmp = TempDir::new().unwrap();
        let to = tmp.path().join("to");
        let staging = tmp.path().join("staging");
        write(&to.join("keep.toml"), "keep me\n");
        fs::create_dir_all(&staging).unwrap();

        merge_into(&to, &staging).unwrap();

        assert_eq!(
            fs::read_to_string(to.join("keep.toml")).unwrap(),
            "keep me\n",
            "the original destination must survive untouched"
        );
        assert_eq!(
            fs::read_to_string(staging.join("keep.toml")).unwrap(),
            "keep me\n"
        );
    }

    #[test]
    fn successful_merge_leaves_no_staging_or_retired_leftovers() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        seed(&from);
        write(&to.join("notes.md"), "keep me\n");

        StateMigrator::migrate(&from, &to, ConflictPolicy::Abort).unwrap();

        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "no scratch or retired dirs should survive success");
        assert_eq!(fs::read_to_string(to.join("notes.md")).unwrap(), "keep me\n");
        assert!(to.join("tasks/a.toml").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn destination_carry_over_copy_failure_aborts_the_migration() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc_geteuid() } == 0 {
            return; // root ignores permission bits
        }
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        seed(&from);
        let unreadable = to.join("secret.toml");
        write(&unreadable, "keep out\n");
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();

        let result = StateMigrator::migrate(&from, &to, ConflictPolicy::Abort);

        // Restore before asserting so the tempdir can always be cleaned up.
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(
            result.is_err(),
            "a failed destination carry-over must abort the migration, not warn and continue"
        );
        assert!(from.join("tasks/a.toml").is_file(), "source intact");
        assert!(
            to.join("secret.toml").is_file(),
            "destination-only file was never moved out of `to`"
        );
        assert_eq!(fs::read_to_string(&unreadable).unwrap(), "keep out\n");

        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "staging must be cleaned up after failure");
    }
}
