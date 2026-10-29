// SPDX-License-Identifier: MPL-2.0
//! Git processes that SlashIt starts from its own environment.
//!
//! The official Linux AppImage puts its bundled library directories first in
//! `LD_LIBRARY_PATH` for every process it starts, the application included.
//! `git` and the helpers it starts (`git-remote-https`, `ssh`) then load a
//! bundled library before the host's copy. The bundled copy can be older: the
//! AppImage ships an older libnghttp2 than the host's libcurl needs, so an
//! HTTPS fetch or push fails with a missing-symbol error. The commands here
//! remove only the entries that point into the AppImage, from the environment
//! of the child. The parent process keeps its environment, so components that
//! need the bundled libraries are unaffected, and outside an AppImage (where
//! `APPDIR` is unset) the environment is passed through unchanged.

use std::ffi::{OsStr, OsString};
use std::path::Path;

/// A `tokio` command for `program` whose environment has no AppImage library
/// entries. See the module documentation.
pub(crate) fn tokio_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    remove_appimage_library_path(command.as_std_mut());
    command
}

/// A standard-library command for `program`, scrubbed like [`tokio_command`].
pub(crate) fn std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    remove_appimage_library_path(&mut command);
    command
}

fn remove_appimage_library_path(command: &mut std::process::Command) {
    scrub(
        command,
        std::env::var_os("APPDIR").as_deref(),
        std::env::var_os("LD_LIBRARY_PATH").as_deref(),
    );
}

/// Applies the scrub to `command` for an explicit AppImage directory and
/// library path, so the behavior can be tested without changing this
/// process's environment.
fn scrub(
    command: &mut std::process::Command,
    appdir: Option<&OsStr>,
    library_path: Option<&OsStr>,
) {
    let (Some(appdir), Some(library_path)) = (appdir, library_path) else {
        return;
    };
    if appdir.is_empty() {
        return;
    }
    match host_library_path(library_path, Path::new(appdir)) {
        LibraryPath::Unchanged => {}
        LibraryPath::Set(value) => {
            command.env("LD_LIBRARY_PATH", value);
        }
        LibraryPath::Removed => {
            command.env_remove("LD_LIBRARY_PATH");
        }
    }
}

/// What to do with `LD_LIBRARY_PATH` for a child process.
#[derive(Debug, PartialEq, Eq)]
enum LibraryPath {
    /// No entry points into the AppImage; the child inherits the value as is.
    Unchanged,
    /// The host entries that remain, in their original order.
    Set(OsString),
    /// Every entry pointed into the AppImage; the variable is removed.
    Removed,
}

/// The `library_path` entries that are neither inside `appdir` nor empty.
/// An entry is inside when its resolved location is inside the resolved
/// `appdir`, so a symlink into the AppImage counts and `..` is resolved. An
/// entry that cannot be resolved (missing, or a broken link) falls back to the
/// component comparison. The entry itself is kept as written, so `/tmp/.mount_X2/lib`
/// is not inside `/tmp/.mount_X`. An empty entry is dropped too: the loader
/// reads it as the current directory, and the AppImage launcher leaves one at
/// the end of the path it sets.
fn host_library_path(library_path: &OsStr, appdir: &Path) -> LibraryPath {
    let appdir_resolved = std::fs::canonicalize(appdir).ok();
    let entries: Vec<_> = std::env::split_paths(library_path).collect();
    let kept: Vec<_> = entries
        .iter()
        .filter(|entry| {
            !entry.as_os_str().is_empty() && !is_inside(entry, appdir, appdir_resolved.as_deref())
        })
        .cloned()
        .collect();
    if kept.len() == entries.len() {
        LibraryPath::Unchanged
    } else if kept.is_empty() {
        LibraryPath::Removed
    } else {
        // `join_paths` fails only for an entry that contains the separator,
        // and every entry came from `split_paths`, so it cannot fail here.
        std::env::join_paths(kept).map_or(LibraryPath::Removed, LibraryPath::Set)
    }
}

/// Whether `entry` points into the AppImage. A relative entry is resolved
/// against the child's working directory, not this process's, so it keeps the
/// component comparison.
fn is_inside(entry: &Path, appdir: &Path, appdir_resolved: Option<&Path>) -> bool {
    if !entry.is_absolute() {
        return entry.starts_with(appdir);
    }
    match (std::fs::canonicalize(entry), appdir_resolved) {
        (Ok(resolved), Some(root)) => resolved.starts_with(root),
        _ => entry.starts_with(appdir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    const MOUNT: &str = "/tmp/.mount_SlashItX";

    #[test]
    fn appimage_entries_are_removed_and_host_entries_kept_in_order() {
        let library_path = OsString::from(format!("{MOUNT}/usr/lib/:/opt/host/lib:{MOUNT}/lib/"));
        assert_eq!(
            host_library_path(&library_path, Path::new(MOUNT)),
            LibraryPath::Set(OsString::from("/opt/host/lib"))
        );
    }

    #[test]
    fn a_sibling_directory_with_the_same_text_prefix_is_a_host_entry() {
        let library_path = OsString::from(format!("{MOUNT}2/lib:{MOUNT}/lib"));
        assert_eq!(
            host_library_path(&library_path, Path::new(MOUNT)),
            LibraryPath::Set(OsString::from(format!("{MOUNT}2/lib")))
        );
    }

    #[test]
    fn a_path_without_appimage_entries_is_unchanged() {
        let library_path = OsString::from("/opt/a:/opt/b");
        assert_eq!(
            host_library_path(&library_path, Path::new(MOUNT)),
            LibraryPath::Unchanged
        );
    }

    #[test]
    fn an_empty_entry_left_by_the_launcher_is_removed() {
        let library_path = OsString::from(format!("{MOUNT}/usr/lib/:/opt/host/lib:"));
        assert_eq!(
            host_library_path(&library_path, Path::new(MOUNT)),
            LibraryPath::Set(OsString::from("/opt/host/lib"))
        );
    }

    #[test]
    fn a_path_of_only_appimage_entries_is_removed() {
        let library_path = OsString::from(format!("{MOUNT}/usr/lib/:{MOUNT}/lib64/"));
        assert_eq!(
            host_library_path(&library_path, Path::new(MOUNT)),
            LibraryPath::Removed
        );
    }

    #[test]
    fn no_appdir_or_no_library_path_leaves_the_command_alone() {
        let mut command = std::process::Command::new("git");
        scrub(&mut command, None, Some(OsStr::new("/opt/a")));
        scrub(&mut command, Some(OsStr::new(MOUNT)), None);
        scrub(&mut command, Some(OsStr::new("")), Some(OsStr::new(MOUNT)));
        assert_eq!(command.get_envs().count(), 0);
    }

    /// Fixture: an AppImage directory with a bundled library directory, and a
    /// host directory next to it. Returns the temporary root with the AppImage
    /// directory `appdir` and the host directory `host`.
    #[cfg(unix)]
    fn appimage_fixture() -> (TempDir, PathBuf, PathBuf) {
        let root = TempDir::new().expect("create temp dir");
        let appdir = root.path().join("appdir");
        let host = root.path().join("host/lib");
        std::fs::create_dir_all(appdir.join("usr/lib")).expect("create appdir");
        std::fs::create_dir_all(&host).expect("create host dir");
        (root, appdir, host)
    }

    /// An entry that reaches the AppImage through a symlink is removed. Kept,
    /// the loader would resolve it to the bundled library.
    #[cfg(unix)]
    #[test]
    fn a_symlink_into_the_appimage_is_removed() {
        let (root, appdir, _) = appimage_fixture();
        std::os::unix::fs::symlink(&appdir, root.path().join("alias")).expect("symlink");
        let library_path = OsString::from(root.path().join("alias/usr/lib"));
        assert_eq!(
            host_library_path(&library_path, &appdir),
            LibraryPath::Removed
        );
    }

    /// An entry written through `..` that resolves outside the AppImage is a
    /// host entry, and it is kept.
    #[cfg(unix)]
    #[test]
    fn a_parent_traversal_out_of_the_appimage_is_kept() {
        let (_root, appdir, host) = appimage_fixture();
        let library_path = OsString::from(format!("{}/../host/lib", appdir.display()));
        assert_eq!(
            host_library_path(&library_path, &appdir),
            LibraryPath::Unchanged
        );
        assert_eq!(
            std::fs::canonicalize(&library_path).expect("canonicalize"),
            std::fs::canonicalize(&host).expect("canonicalize host")
        );
    }

    /// An entry written through `..` that resolves into the AppImage is
    /// removed, even though its text does not start with the AppImage path.
    #[cfg(unix)]
    #[test]
    fn a_parent_traversal_into_the_appimage_is_removed() {
        let (root, appdir, _) = appimage_fixture();
        let library_path = OsString::from(format!(
            "{}/host/lib/../../appdir/usr/lib",
            root.path().display()
        ));
        assert_eq!(
            host_library_path(&library_path, &appdir),
            LibraryPath::Removed
        );
    }

    /// An AppImage directory that is itself reached through a symlink still
    /// matches the real spelling of its entries.
    #[cfg(unix)]
    #[test]
    fn an_appimage_spelled_through_a_symlink_matches_its_real_entries() {
        let (root, appdir, _) = appimage_fixture();
        let appdir_alias = root.path().join("alias");
        std::os::unix::fs::symlink(&appdir, &appdir_alias).expect("symlink");
        let library_path = OsString::from(appdir.join("usr/lib"));
        assert_eq!(
            host_library_path(&library_path, &appdir_alias),
            LibraryPath::Removed
        );
    }

    /// An entry that does not exist cannot be resolved, so the component
    /// comparison decides it: inside the AppImage it is removed, and a host
    /// entry is kept.
    #[cfg(unix)]
    #[test]
    fn a_missing_entry_keeps_the_component_comparison() {
        let (_root, appdir, _) = appimage_fixture();
        let library_path = OsString::from(format!(
            "{}:/opt/host/lib",
            appdir.join("usr/lib/missing").display()
        ));
        assert_eq!(
            host_library_path(&library_path, &appdir),
            LibraryPath::Set(OsString::from("/opt/host/lib"))
        );
    }

    /// A child started through `scrub` sees the host entries of the library
    /// path and none of the AppImage entries. The parent's environment is not
    /// read or changed.
    #[cfg(unix)]
    #[test]
    fn a_child_sees_only_the_host_library_entries() {
        let library_path = OsString::from(format!("{MOUNT}/usr/lib/:/opt/host/lib"));
        let mut command = std::process::Command::new("/usr/bin/env");
        scrub(&mut command, Some(OsStr::new(MOUNT)), Some(&library_path));
        let output = command.output().expect("run env");
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(
            printed
                .lines()
                .any(|line| line == "LD_LIBRARY_PATH=/opt/host/lib"),
            "{printed}"
        );
        assert!(!printed.contains(MOUNT), "{printed}");
    }

    /// When every entry was inside the AppImage, the child has no
    /// `LD_LIBRARY_PATH` at all, even if the parent process has one.
    #[cfg(unix)]
    #[test]
    fn a_child_has_no_library_path_when_only_appimage_entries_existed() {
        let library_path = OsString::from(format!("{MOUNT}/usr/lib/"));
        let mut command = std::process::Command::new("/usr/bin/env");
        scrub(&mut command, Some(OsStr::new(MOUNT)), Some(&library_path));
        let output = command.output().expect("run env");
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(
            !printed
                .lines()
                .any(|line| line.starts_with("LD_LIBRARY_PATH=")),
            "{printed}"
        );
    }

    /// Read-only HTTPS transport from `git`, started through [`std_command`]
    /// the way SlashIt starts it. Run it from an AppImage launch, with
    /// `APPDIR` and `LD_LIBRARY_PATH` set to the AppImage's library paths:
    /// it must succeed. `git_https_transport_fails_without_the_scrub` runs the
    /// same request without the scrub and fails there, which is the defect.
    /// Needs network access, so it is ignored by default.
    #[test]
    #[ignore = "needs network and an AppImage launch (APPDIR set)"]
    fn git_https_transport_survives_an_appimage_library_path() {
        if std::env::var_os("APPDIR").is_none() {
            eprintln!("APPDIR is unset; not an AppImage launch, nothing to reproduce");
            return;
        }
        let output = std_command("git")
            .args([
                "ls-remote",
                "--heads",
                "https://github.com/BarraDev/slashit.git",
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git ls-remote failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The control for the test above: the same request from an unscrubbed
    /// `git`, as SlashIt started it before this module. Under an AppImage
    /// launch it fails with the bundled libnghttp2 missing a host symbol.
    #[test]
    #[ignore = "control for the AppImage defect: needs network and an AppImage launch"]
    fn git_https_transport_fails_without_the_scrub() {
        if std::env::var_os("APPDIR").is_none() {
            eprintln!("APPDIR is unset; not an AppImage launch, nothing to reproduce");
            return;
        }
        let output = std::process::Command::new("git")
            .args([
                "ls-remote",
                "--heads",
                "https://github.com/BarraDev/slashit.git",
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git");
        assert!(
            !output.status.success(),
            "unscrubbed git unexpectedly succeeded"
        );
    }
}
