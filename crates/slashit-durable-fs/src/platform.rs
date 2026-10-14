//! The few operations whose implementation differs by platform.

use crate::{DirectorySync, Permissions};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

#[cfg(unix)]
pub(crate) fn creation_options(options: &mut OpenOptions, permissions: Permissions) {
    use std::os::unix::fs::OpenOptionsExt;
    if permissions == Permissions::OwnerOnly {
        // Set at creation, so the file is never readable by anyone else, not
        // even for the instant before a later chmod would run.
        options.mode(0o600);
    }
}

#[cfg(not(unix))]
pub(crate) fn creation_options(_options: &mut OpenOptions, _permissions: Permissions) {}

/// Carry the destination's permission bits over, for [`Permissions::Preserve`].
#[cfg(unix)]
pub(crate) fn finish_permissions(file: &File, dest: &Path, permissions: Permissions) -> io::Result<()> {
    if permissions != Permissions::Preserve {
        return Ok(());
    }
    match std::fs::symlink_metadata(dest) {
        Ok(existing) if existing.file_type().is_file() => file.set_permissions(existing.permissions()),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
pub(crate) fn finish_permissions(_file: &File, _dest: &Path, _permissions: Permissions) -> io::Result<()> {
    Ok(())
}

/// Atomically replace `to` with `from`. `std::fs::rename` is `rename(2)` on
/// Unix and `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` (or its POSIX-semantics
/// equivalent) on Windows, with the standard library's own long-path handling.
pub(crate) fn replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// Synchronize one regular file's contents to stable storage. On Windows this
/// needs write access, so a read-only file fails closed instead of being
/// reported as synchronized.
pub(crate) fn sync_file(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    let file = OpenOptions::new().write(true).open(path)?;
    #[cfg(not(windows))]
    let file = File::open(path)?;
    file.sync_all()
}

/// Synchronize a directory so the entries created or replaced in it survive a
/// power loss.
#[cfg(unix)]
pub(crate) fn sync_directory(dir: &Path) -> io::Result<DirectorySync> {
    File::open(dir).and_then(|d| d.sync_all()).map(|()| DirectorySync::Synced)
}

/// Windows has no `fsync` for a directory. `FlushFileBuffers` on a handle
/// opened with `FILE_FLAG_BACKUP_SEMANTICS` and write access is expected to
/// flush it on NTFS (not documented by Microsoft). A refusal to open the
/// directory for writing is reported as unsupported rather than as a failure of
/// every write; the caller turns it into `DirectorySync::Unsupported`.
#[cfg(windows)]
pub(crate) fn sync_directory(dir: &Path) -> io::Result<DirectorySync> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                io::Error::new(io::ErrorKind::Unsupported, e)
            } else {
                e
            }
        })
        .and_then(|d| d.sync_all())
        .map(|()| DirectorySync::Synced)
}

/// Whether `error` says the filesystem cannot synchronize a directory at all,
/// as opposed to failing to do so.
pub(crate) fn is_unsupported(error: &io::Error) -> bool {
    if matches!(error.kind(), io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput) {
        return true;
    }
    // ERROR_INVALID_FUNCTION (1) and ERROR_NOT_SUPPORTED (50): FlushFileBuffers
    // on a directory of a filesystem that has no such operation.
    #[cfg(windows)]
    if matches!(error.raw_os_error(), Some(1) | Some(50)) {
        return true;
    }
    false
}
