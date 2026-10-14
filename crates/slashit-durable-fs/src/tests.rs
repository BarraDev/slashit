use super::*;
use crate::fault::{fail_at, trace};
use tempfile::TempDir;

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap()
}

fn leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect()
}

#[test]
fn creates_and_replaces_a_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("board.toml");
    write_atomic(&path, b"one", Permissions::Preserve).unwrap();
    assert_eq!(read(&path), b"one");
    let committed = write_atomic(&path, b"two", Permissions::Preserve).unwrap();
    assert_eq!(read(&path), b"two");
    assert!(committed.is_fully_durable(), "{committed:?}");
    assert!(leftovers(dir.path()).is_empty());
}

#[test]
fn creates_missing_parent_directories() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a/b/c/board.toml");
    write_atomic(&path, b"x", Permissions::OwnerOnly).unwrap();
    assert_eq!(read(&path), b"x");
}

#[test]
fn syncs_the_file_before_replacing_and_the_directory_after() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("board.toml");
    // The first write into a directory also syncs that directory's own entry.
    write_atomic(&path, b"first", Permissions::Preserve).unwrap();
    let t = trace();
    write_atomic(&path, b"x", Permissions::Preserve).unwrap();
    assert_eq!(
        t.stages(),
        [
            Stage::CreateTemp,
            Stage::WriteTemp,
            Stage::SetPermissions,
            Stage::SyncTemp,
            Stage::Replace,
            Stage::SyncDirectory,
        ]
    );
}

#[test]
fn syncs_every_directory_it_created() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a/b/board.toml");
    let t = trace();
    write_atomic(&path, b"x", Permissions::Preserve).unwrap();
    let stages = t.stages();
    let created = stages.iter().filter(|s| **s == Stage::SyncCreatedDirectory).count();
    assert_eq!(created, 2, "{stages:?}");
    assert_eq!(stages[0], Stage::CreateDirectory);
}

#[test]
fn a_failure_before_the_replacement_leaves_the_destination_untouched() {
    for stage in [
        Stage::CreateTemp,
        Stage::WriteTemp,
        Stage::SetPermissions,
        Stage::SyncTemp,
        Stage::Replace,
    ] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("board.toml");
        write_atomic(&path, b"old", Permissions::Preserve).unwrap();

        let _fault = fail_at(stage, io::ErrorKind::Other);
        let error = write_atomic(&path, b"new", Permissions::Preserve).unwrap_err();

        assert!(!error.is_published(), "{stage:?}: {error}");
        assert_eq!(error.stage(), stage);
        assert_eq!(read(&path), b"old", "{stage:?} must not change the destination");
        assert!(leftovers(dir.path()).is_empty(), "{stage:?} left a temporary file");
    }
}

#[test]
fn a_failed_first_write_leaves_no_destination() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("board.toml");
    let _fault = fail_at(Stage::SyncTemp, io::ErrorKind::Other);
    assert!(write_atomic(&path, b"new", Permissions::Preserve).is_err());
    assert!(!path.exists());
    assert!(leftovers(dir.path()).is_empty());
}

#[test]
fn a_directory_sync_failure_is_reported_as_published_but_not_durable() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("board.toml");
    write_atomic(&path, b"old", Permissions::Preserve).unwrap();

    let _fault = fail_at(Stage::SyncDirectory, io::ErrorKind::Other);
    let error = write_atomic(&path, b"new", Permissions::Preserve).unwrap_err();

    assert!(error.is_published());
    assert_eq!(error.stage(), Stage::SyncDirectory);
    // Readers already see the new content: this is not a failed write.
    assert_eq!(read(&path), b"new");
    assert!(error.to_string().contains("not confirmed durable"), "{error}");
    assert!(leftovers(dir.path()).is_empty());
}

#[test]
fn a_failure_creating_a_parent_directory_is_not_published() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a/board.toml");
    let _fault = fail_at(Stage::CreateDirectory, io::ErrorKind::PermissionDenied);
    let error = write_atomic(&path, b"x", Permissions::Preserve).unwrap_err();
    assert!(!error.is_published());
    assert!(!dir.path().join("a").exists());

    let _fault = fail_at(Stage::SyncCreatedDirectory, io::ErrorKind::Other);
    let error = write_atomic(&path, b"x", Permissions::Preserve).unwrap_err();
    assert!(!error.is_published());
    assert!(!path.exists());
}

#[test]
fn a_filesystem_without_directory_sync_is_reported_not_failed() {
    let committed = tolerate_unsupported(io::Error::from(io::ErrorKind::Unsupported)).unwrap();
    assert_eq!(committed, DirectorySync::Unsupported);
    assert!(!Committed { directory: committed }.is_fully_durable());
    assert!(tolerate_unsupported(io::Error::from(io::ErrorKind::InvalidInput)).is_ok());
    // An I/O error is a real failure, never "unsupported".
    assert!(tolerate_unsupported(io::Error::other("EIO")).is_err());
    assert!(tolerate_unsupported(io::Error::from(io::ErrorKind::PermissionDenied)).is_err());
}

#[test]
fn a_staged_file_that_is_dropped_changes_nothing() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("board.toml");
    write_atomic(&path, b"old", Permissions::Preserve).unwrap();
    let staged = stage(&path, b"new", Permissions::Preserve).unwrap();
    assert_eq!(leftovers(dir.path()).len(), 1);
    drop(staged);
    assert_eq!(read(&path), b"old");
    assert!(leftovers(dir.path()).is_empty());
}

#[test]
fn a_staged_file_publishes_what_it_staged() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("board.toml");
    let staged = stage(&path, b"new", Permissions::Preserve).unwrap();
    assert!(!path.exists(), "staging must not publish");
    staged.publish().unwrap();
    assert_eq!(read(&path), b"new");
    assert!(leftovers(dir.path()).is_empty());
}

#[test]
fn a_colliding_temporary_name_is_skipped_not_overwritten() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("board.toml");
    // A crashed process with the same id and counter left these behind.
    for n in 0..3 {
        fs::write(temp_path(&path, n), b"leftover").unwrap();
    }
    let mut counters = 0..;
    let (file, tmp) = create_temp_with(&path, Permissions::Preserve, || counters.next().unwrap()).unwrap();
    drop(file);
    assert_eq!(tmp, temp_path(&path, 3));
    for n in 0..3 {
        assert_eq!(read(&temp_path(&path, n)), b"leftover", "a leftover must never be overwritten");
    }
}

#[test]
fn running_out_of_temporary_names_is_a_clean_failure() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("board.toml");
    fs::write(temp_path(&path, 7), b"leftover").unwrap();
    let error = create_temp_with(&path, Permissions::Preserve, || 7).unwrap_err();
    assert!(!error.is_published());
    assert_eq!(error.stage(), Stage::CreateTemp);
    assert_eq!(read(&temp_path(&path, 7)), b"leftover");
}

#[test]
fn the_first_write_into_a_directory_syncs_that_directorys_own_entry_once() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("board.toml");
    let t = trace();
    write_atomic(&path, b"1", Permissions::Preserve).unwrap();
    write_atomic(&path, b"2", Permissions::Preserve).unwrap();
    let synced = t.stages().iter().filter(|s| **s == Stage::SyncCreatedDirectory).count();
    assert_eq!(synced, 1, "{:?}", t.stages());
}

#[test]
fn a_directory_whose_entry_sync_failed_is_not_reported_durable_by_the_retry() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a/b/board.toml");
    {
        let _fault = fail_at(Stage::SyncCreatedDirectory, io::ErrorKind::Other);
        let error = write_atomic(&path, b"x", Permissions::Preserve).unwrap_err();
        assert!(!error.is_published());
    }
    // The directories now exist, so a naive retry would skip their syncs.
    assert!(path.parent().unwrap().is_dir());
    let t = trace();
    write_atomic(&path, b"x", Permissions::Preserve).unwrap();
    let resynced = t.stages().iter().filter(|s| **s == Stage::SyncCreatedDirectory).count();
    assert_eq!(resynced, 2, "both created entries are synced by the retry: {:?}", t.stages());
}

#[test]
fn concurrent_writers_never_corrupt_the_destination() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("board.toml");
    let threads: Vec<_> = (0..8u8)
        .map(|i| {
            let path = path.clone();
            std::thread::spawn(move || {
                let body = vec![b'a' + i; 64 * 1024];
                for _ in 0..10 {
                    // Windows can refuse a replace while another writer or a
                    // scanner has the destination open; that must be a clean,
                    // unpublished failure, never a torn file.
                    if let Err(error) = write_atomic(&path, &body, Permissions::Preserve) {
                        assert!(!error.is_published(), "{error}");
                    }
                }
                body
            })
        })
        .collect();
    let bodies: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(bodies.contains(&read(&path)), "the file must be exactly one writer's content");
    assert!(leftovers(dir.path()).is_empty());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn owner_only_files_are_private_from_creation_and_after_replacement() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        write_atomic(&path, b"secret", Permissions::OwnerOnly).unwrap();
        assert_eq!(mode(&path), 0o600);
        // A previously wider file is narrowed, never widened.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&path, b"secret 2", Permissions::OwnerOnly).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_failed_owner_only_write_never_exposes_the_temporary_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let staged = stage(&path, b"secret", Permissions::OwnerOnly).unwrap();
        let tmp = fs::read_dir(dir.path()).unwrap().next().unwrap().unwrap().path();
        assert_eq!(mode(&tmp), 0o600);
        drop(staged);
    }

    #[test]
    fn preserve_keeps_an_existing_files_mode() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tasks.toml");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&path, b"new", Permissions::Preserve).unwrap();
        assert_eq!(mode(&path), 0o640);
    }

    #[test]
    fn preserve_gives_a_new_file_the_platform_default() {
        let dir = TempDir::new().unwrap();
        let control = dir.path().join("control");
        fs::write(&control, b"x").unwrap();
        let path = dir.path().join("tasks.toml");
        write_atomic(&path, b"new", Permissions::Preserve).unwrap();
        assert_eq!(mode(&path), mode(&control));
    }

    #[test]
    fn a_symlink_at_the_destination_is_replaced_not_followed() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"untouched").unwrap();
        let path = dir.path().join("board.toml");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        write_atomic(&path, b"new", Permissions::Preserve).unwrap();
        assert_eq!(read(&target), b"untouched");
        assert_eq!(read(&path), b"new");
        assert!(!fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
    }

    #[test]
    fn a_real_directory_sync_reports_synced_on_this_platform() {
        let dir = TempDir::new().unwrap();
        let committed = write_atomic(&dir.path().join("f"), b"x", Permissions::Preserve).unwrap();
        // Linux and macOS temp filesystems support directory sync; on Linux
        // anything else is a regression, on macOS the result is evidence.
        #[cfg(target_os = "linux")]
        assert_eq!(committed.directory, DirectorySync::Synced);
        #[cfg(not(target_os = "linux"))]
        assert!(matches!(committed.directory, DirectorySync::Synced | DirectorySync::Unsupported), "{committed:?}");
        eprintln!("directory sync on this platform: {:?}", committed.directory);
    }
}

#[cfg(windows)]
mod windows {
    use super::*;

    #[test]
    fn replacement_reports_what_the_directory_flush_did() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f");
        write_atomic(&path, b"one", Permissions::OwnerOnly).unwrap();
        let committed = write_atomic(&path, b"two", Permissions::OwnerOnly).unwrap();
        // NTFS on the hosted runner flushes the directory; a filesystem that
        // cannot says so. Either is a truthful answer, a failure is not.
        assert!(matches!(committed.directory, DirectorySync::Synced | DirectorySync::Unsupported), "{committed:?}");
        eprintln!("directory sync on this platform: {:?}", committed.directory);
        assert_eq!(read(&path), b"two");
    }

    #[test]
    fn a_long_path_is_written_like_any_other() {
        let dir = TempDir::new().unwrap();
        let mut deep = dir.path().to_path_buf();
        while deep.as_os_str().len() < 300 {
            deep.push("segment-of-a-long-directory-name");
        }
        let path = deep.join("board.toml");
        // std handles the verbatim prefix for its own calls; so must we.
        write_atomic(&path, b"one", Permissions::Preserve).unwrap();
        write_atomic(&path, b"two", Permissions::Preserve).unwrap();
        assert_eq!(read(&path), b"two");
    }

    #[test]
    fn replacing_a_file_that_is_open_for_reading_succeeds_or_fails_cleanly() {
        // A reader holding the destination open must leave either the old or
        // the new content, never a partial file.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f");
        write_atomic(&path, b"one", Permissions::Preserve).unwrap();
        let reader = File::open(&path).unwrap();
        let result = write_atomic(&path, b"two", Permissions::Preserve);
        drop(reader);
        let body = read(&path);
        match result {
            Ok(_) => assert_eq!(body, b"two"),
            Err(e) => {
                assert!(!e.is_published(), "{e}");
                assert_eq!(body, b"one");
            }
        }
        assert!(leftovers(dir.path()).is_empty());
    }
}
