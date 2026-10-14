//! Durable, atomic replacement of one file.
//!
//! [`write_atomic`] is the single protocol SlashIt uses to persist a Task
//! board, the configuration and a Project Conversation:
//!
//! 1. create any missing parent directories and synchronize the entries that
//!    made them reachable;
//! 2. create a uniquely named temporary file next to the destination, write
//!    the bytes and apply the permissions;
//! 3. synchronize the temporary file's contents to stable storage;
//! 4. atomically replace the destination with it;
//! 5. synchronize the containing directory, so the replacement itself
//!    survives a power loss.
//!
//! # What this guarantees, and what it does not
//!
//! The guarantee is **per file**. After `Ok`, a crash or power loss leaves the
//! destination holding either its previous complete content or this complete
//! content, never a mixture. When [`Committed::is_fully_durable`] it also
//! leaves it holding this content, as far as the platform's flush operations
//! can say; they cannot say what a drive's own cache does after it reports
//! done. No test can prove behavior across a power loss; the tests prove the
//! order of the operations and the failure semantics.
//!
//! The directory entries that make the file reachable are covered like this:
//! the file's own entry (synced after the replacement), the entry of every
//! directory this process created through [`write_atomic`] (synced as it is
//! created, and re-synced on a later write if that sync had failed), and the
//! entry of the destination directory itself in its parent, once per process.
//! Older ancestors are assumed to have been durable since an earlier run.
//!
//! It is not a transaction. Two files written by two calls, a file and a Git
//! ref, or a file and a running process can each be at a different point after
//! a crash. Callers that need an order between such effects must establish it
//! themselves, by writing in a recoverable order and reconciling on startup.
//!
//! # The two kinds of failure
//!
//! A failure before the replacement leaves the destination exactly as it was
//! ([`WriteError::NotPublished`]). A failure *after* the replacement leaves the
//! new content visible to every reader but not confirmed durable
//! ([`WriteError::PublishedNotDurable`]); a caller must not treat that as
//! "nothing happened", and must not claim the write was committed either.
//!
//! # Platforms
//!
//! * Linux and other Unix: `fsync` on the file and on the directory.
//! * macOS: `File::sync_all` issues `F_FULLFSYNC`, which asks the drive to
//!   flush its own cache; plain `fsync` does not. The directory is synchronized
//!   the same way when the filesystem supports it. A filesystem that rejects
//!   `F_FULLFSYNC` for files fails the write at [`Stage::SyncTemp`] rather than
//!   silently skipping the flush.
//! * Windows: `FlushFileBuffers` on the file, the standard library's replace
//!   (`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`, with long-path handling;
//!   Microsoft does not document it as atomic, and a reader that holds the
//!   destination open can make it fail cleanly), then `FlushFileBuffers` on a
//!   handle to the directory opened with `FILE_FLAG_BACKUP_SEMANTICS`. That
//!   directory flush is expected to work on NTFS but is not documented by
//!   Microsoft; where the open or the flush is refused the result is reported
//!   as [`DirectorySync::Unsupported`], and the hosted-runner test logs what
//!   was observed. `MOVEFILE_WRITE_THROUGH` is not used: Microsoft documents
//!   its flush only for a move performed as copy and delete.
//!
//! Where a filesystem refuses to synchronize a directory (some network and
//! FUSE filesystems), the write still succeeds, but [`Committed::directory`]
//! says [`DirectorySync::Unsupported`], [`Committed::is_fully_durable`] is
//! false, and the entry's durability is whatever that filesystem provides.
//!
//! Replacing a file gives the destination a new inode. Its owner becomes the
//! user running SlashIt, and a symbolic link at the destination is replaced by
//! a regular file rather than followed.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(any(test, feature = "fault-injection"))]
pub mod fault;

mod platform;

/// The step a write was at. Reported in errors, and the unit that fault
/// injection and tracing address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Creating a missing parent directory.
    CreateDirectory,
    /// Synchronizing the directory entry of a directory this call created.
    SyncCreatedDirectory,
    /// Creating the temporary file.
    CreateTemp,
    /// Writing the bytes into the temporary file.
    WriteTemp,
    /// Applying permissions to the temporary file.
    SetPermissions,
    /// Synchronizing the temporary file's contents to stable storage.
    SyncTemp,
    /// The atomic replacement of the destination.
    Replace,
    /// Synchronizing the containing directory after the replacement.
    SyncDirectory,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Stage::CreateDirectory => "creating the parent directory",
            Stage::SyncCreatedDirectory => "synchronizing a created parent directory",
            Stage::CreateTemp => "creating the temporary file",
            Stage::WriteTemp => "writing the temporary file",
            Stage::SetPermissions => "setting permissions on the temporary file",
            Stage::SyncTemp => "synchronizing the temporary file",
            Stage::Replace => "replacing the destination",
            Stage::SyncDirectory => "synchronizing the containing directory",
        })
    }
}

/// Which permissions the new file gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permissions {
    /// The platform default for a new file. When the destination already
    /// exists as a regular file, its permission bits are carried over, so an
    /// administrator's chmod survives a rewrite (Unix; Windows has no mode).
    Preserve,
    /// Owner-only (`0600`) from the moment the file exists, for files that may
    /// hold secrets. Windows inherits the parent directory's ACL, which is
    /// user-scoped under the application data directory.
    OwnerOnly,
}

/// How the containing directory's entry was made durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectorySync {
    /// The directory was synchronized after the replacement.
    Synced,
    /// The filesystem does not support synchronizing a directory. The new
    /// content is in place and its bytes are synchronized, but this call
    /// cannot say the replacement itself survives a power loss.
    Unsupported,
}

/// A write that reached its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    pub directory: DirectorySync,
}

impl Committed {
    /// True when the file's bytes and the replacement itself were both
    /// confirmed synchronized. False only for [`DirectorySync::Unsupported`],
    /// where the filesystem could not be asked about the directory.
    pub fn is_fully_durable(&self) -> bool {
        matches!(self.directory, DirectorySync::Synced)
    }
}

/// A write that did not complete.
#[derive(Debug)]
pub enum WriteError {
    /// Nothing was published. The destination holds exactly what it held
    /// before the call (or does not exist, if it did not). The temporary file
    /// was removed on a best-effort basis.
    NotPublished { stage: Stage, source: io::Error },
    /// The destination was replaced and readers already see the new content,
    /// but the final synchronization failed, so it may not survive a power
    /// loss. Do not report this as a failed write that changed nothing, and do
    /// not report it as committed.
    PublishedNotDurable { stage: Stage, source: io::Error },
}

impl WriteError {
    /// True when the destination already holds the new content.
    pub fn is_published(&self) -> bool {
        matches!(self, WriteError::PublishedNotDurable { .. })
    }

    pub fn stage(&self) -> Stage {
        match self {
            WriteError::NotPublished { stage, .. } | WriteError::PublishedNotDurable { stage, .. } => *stage,
        }
    }

    pub fn io_error(&self) -> &io::Error {
        match self {
            WriteError::NotPublished { source, .. } | WriteError::PublishedNotDurable { source, .. } => source,
        }
    }
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::NotPublished { stage, source } => {
                write!(f, "nothing was written: failed {stage}: {source}")
            }
            WriteError::PublishedNotDurable { stage, source } => write!(
                f,
                "the new content is in place but is not confirmed durable: failed {stage}: {source}"
            ),
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.io_error())
    }
}

/// Per-process counter that makes temporary names unique between concurrent
/// writers inside this process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How many times a colliding temporary name is retried. A collision needs a
/// leftover from a crashed process that had the same id and counter.
const TEMP_ATTEMPTS: u32 = 16;

fn temp_path(path: &Path, counter: u64) -> PathBuf {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    path.with_file_name(format!("{file_name}.{}-{counter}.tmp", std::process::id()))
}

fn not_published(stage: Stage) -> impl FnOnce(io::Error) -> WriteError {
    move |source| WriteError::NotPublished { stage, source }
}

/// Run `op` unless fault injection has this stage armed.
///
/// `target` is the file or directory the step is about; fault injection can
/// be limited to one file name so a test fails exactly the write it means to.
fn step<T>(stage: Stage, target: &Path, op: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    #[cfg(any(test, feature = "fault-injection"))]
    fault::before(stage, target)?;
    #[cfg(not(any(test, feature = "fault-injection")))]
    let _ = (stage, target);
    op()
}

/// A filesystem that cannot synchronize a directory (EINVAL or ENOTSUP) is not
/// a failure of the write; it is a durability the filesystem does not offer,
/// reported as [`DirectorySync::Unsupported`]. Anything else, EIO above all, is
/// a real failure.
fn tolerate_unsupported(error: io::Error) -> io::Result<DirectorySync> {
    if platform::is_unsupported(&error) {
        Ok(DirectorySync::Unsupported)
    } else {
        Err(error)
    }
}

/// A fully written and synchronized temporary file that has not replaced its
/// destination yet.
///
/// Splitting [`write_atomic`] in two lets a caller re-check its precondition
/// at the last moment before publishing, which is how the Task store keeps its
/// "never replace a board that cannot be read" guard close to the rename.
/// Dropping a `StagedFile` removes the temporary file and changes nothing else.
#[must_use = "a staged file does nothing until it is published"]
pub struct StagedFile {
    tmp: PathBuf,
    dest: PathBuf,
    published: bool,
}

/// Stage `bytes` for `path`: everything in [`write_atomic`] up to, but not
/// including, the replacement.
pub fn stage(path: &Path, bytes: &[u8], permissions: Permissions) -> Result<StagedFile, WriteError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_dir_all_durable(parent)?;
    }
    let (mut file, tmp) = create_temp(path, permissions)?;
    // From here the temporary file exists; any early return removes it.
    let staged = StagedFile { tmp, dest: path.to_path_buf(), published: false };
    step(Stage::WriteTemp, path, || file.write_all(bytes)).map_err(not_published(Stage::WriteTemp))?;
    step(Stage::SetPermissions, path, || platform::finish_permissions(&file, path, permissions))
        .map_err(not_published(Stage::SetPermissions))?;
    step(Stage::SyncTemp, path, || file.sync_all()).map_err(not_published(Stage::SyncTemp))?;
    drop(file);
    Ok(staged)
}

impl StagedFile {
    /// Atomically replace the destination, then synchronize its directory.
    pub fn publish(mut self) -> Result<Committed, WriteError> {
        step(Stage::Replace, &self.dest, || platform::replace(&self.tmp, &self.dest)).map_err(not_published(Stage::Replace))?;
        self.published = true;
        let parent = self.dest.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        match step(Stage::SyncDirectory, &self.dest, || platform::sync_directory(parent)).or_else(tolerate_unsupported) {
            Ok(directory) => Ok(Committed { directory }),
            Err(source) => Err(WriteError::PublishedNotDurable { stage: Stage::SyncDirectory, source }),
        }
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

/// Durably and atomically replace `path` with `bytes`. See the crate
/// documentation for the protocol, the guarantee and the failure model.
pub fn write_atomic(path: &Path, bytes: &[u8], permissions: Permissions) -> Result<Committed, WriteError> {
    stage(path, bytes, permissions)?.publish()
}

fn create_temp(path: &Path, permissions: Permissions) -> Result<(File, PathBuf), WriteError> {
    create_temp_with(path, permissions, || TEMP_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn create_temp_with(
    path: &Path,
    permissions: Permissions,
    mut next_counter: impl FnMut() -> u64,
) -> Result<(File, PathBuf), WriteError> {
    let mut last = None;
    for _ in 0..TEMP_ATTEMPTS {
        let tmp = temp_path(path, next_counter());
        let opened = step(Stage::CreateTemp, path, || {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            platform::creation_options(&mut options, permissions);
            options.open(&tmp)
        });
        match opened {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(WriteError::NotPublished { stage: Stage::CreateTemp, source: e }),
        }
    }
    Err(WriteError::NotPublished {
        stage: Stage::CreateTemp,
        source: last.unwrap_or_else(|| io::Error::other("no unused temporary name")),
    })
}

/// Directory entries whose durability is still owed, and directories whose own
/// entry was already synced by this process. Held across the creation and the
/// syncs, so a concurrent writer that finds a directory another thread just
/// created waits for that thread's syncs, and learns if they failed.
#[derive(Default)]
struct DirectoryState {
    owed: Vec<PathBuf>,
    entry_synced: std::collections::HashSet<PathBuf>,
}

static DIRECTORY_STATE: std::sync::Mutex<Option<DirectoryState>> = std::sync::Mutex::new(None);

fn parent_of(path: &Path) -> &Path {
    path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."))
}

/// `create_dir_all`, then synchronize the entry of every directory this
/// process created and has not yet confirmed, plus (once) the destination
/// directory's own entry. See the crate documentation for what that covers.
fn create_dir_all_durable(dir: &Path) -> Result<(), WriteError> {
    let mut guard = DIRECTORY_STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let state = guard.get_or_insert_with(DirectoryState::default);

    let mut missing = Vec::new();
    let mut cursor = Some(dir);
    while let Some(candidate) = cursor.filter(|p| !p.as_os_str().is_empty()) {
        if candidate.exists() {
            break;
        }
        missing.push(candidate.to_path_buf());
        cursor = candidate.parent();
    }
    if !missing.is_empty() {
        // Owed before they exist: `create_dir_all` can create some of them and
        // then fail, and a later call that finds those directories in place
        // must still sync their entries. An owed entry that does not exist is
        // dropped when the syncs run.
        state.owed.extend(missing);
        step(Stage::CreateDirectory, dir, || fs::create_dir_all(dir)).map_err(not_published(Stage::CreateDirectory))?;
    }
    // The destination directory's own entry in its parent, once per process:
    // a directory created by someone else (the application's start-up) has no
    // reason to have been synced.
    if !state.entry_synced.contains(dir) && !state.owed.iter().any(|d| d == dir) {
        state.owed.push(dir.to_path_buf());
    }

    // Deepest owed entry that is this directory or one of its ancestors, in
    // creation order (shallowest first), so each parent is synced after the
    // entry above it exists durably.
    let mut relevant: Vec<PathBuf> = state.owed.iter().filter(|d| dir.starts_with(d)).cloned().collect();
    relevant.sort_by_key(|d| d.components().count());
    for entry in relevant {
        if !entry.exists() {
            state.owed.retain(|d| d != &entry);
            continue;
        }
        step(Stage::SyncCreatedDirectory, &entry, || platform::sync_directory(parent_of(&entry)))
            .or_else(tolerate_unsupported)
            .map_err(not_published(Stage::SyncCreatedDirectory))?;
        state.owed.retain(|d| d != &entry);
        state.entry_synced.insert(entry);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
