//! Keeps review tools a developer installed out of the application's reach.
//!
//! The product decides whether CodeRabbit is available the only way it can:
//! `which coderabbit`, then `coderabbit`, both resolved through the `PATH` it
//! was started with. Hosted CI has no such CLI, so there every AI Review takes
//! the "CLI not found" branch. A developer machine often has one, and without
//! this module every local AI Review would call the real service -- over the
//! network, with the developer's own credentials, and a few minutes slower
//! across the suite. Whether a journey talks to an external service must not
//! depend on what happens to be installed where it runs.
//!
//! The boundary is the `PATH` the application is launched with, and nothing
//! inside SlashIt knows about it. Every entry that holds one of [`ISOLATED`]
//! is replaced by a directory the harness owns, holding a link to every other
//! entry of the original and nothing else. Entries that hold none of them are
//! passed through unchanged and in order, so `git`, `jj`, `gh`, the fake agent
//! and every other executable resolve exactly as they did.
//!
//! Empty and relative entries are dropped. They are resolved against the
//! working directory of whichever process looks a name up -- for CodeRabbit,
//! a task's checkout -- so no check made here could say what they hold.
//!
//! This is not a hermetic `PATH`. Only the names in [`ISOLATED`] are removed,
//! and only by name: an alias such as `cr` pointing at the same binary stays
//! reachable, because nothing in the product runs it. A tool that shares a
//! directory with one of them runs through a link in the mirror, so one that
//! finds its own files relative to `$0` rather than its real location may
//! not find them.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::fake_agent::resolve_on_path;

/// The executables the application must never find, whatever the machine has
/// installed. Each one is something the product looks up on `PATH` and that
/// hosted CI does not have.
pub const ISOLATED: &[&str] = &["coderabbit"];

/// The `PATH` a journey asked for, or the one the harness inherited if it
/// asked for none.
pub fn requested_path(child_env: &[(OsString, OsString)], inherited: Option<OsString>) -> OsString {
    child_env
        .iter()
        .rev()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.clone())
        .or(inherited)
        .unwrap_or_default()
}

/// `path` with every entry that holds an [`ISOLATED`] name replaced by a
/// mirror without it, built in a fresh directory under `parent`.
///
/// Fails rather than returning a `PATH` that still resolves one of them: a
/// journey that cannot be isolated must not run with the real tool instead.
pub fn isolate(path: &OsStr, parent: &Path) -> Result<OsString> {
    let mut scratch: Option<PathBuf> = None;
    // `PATH` commonly names one directory several times; one mirror each.
    let mut mirrors: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut entries = Vec::new();
    for dir in std::env::split_paths(path) {
        if !dir.is_absolute() {
            continue;
        }
        if !holds_isolated(&dir) {
            entries.push(dir);
            continue;
        }
        let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if let Some(mirror) = mirrors.get(&key) {
            entries.push(mirror.clone());
            continue;
        }
        let root = match &scratch {
            Some(root) => root.clone(),
            None => {
                let root = claim_fresh(parent)?;
                scratch = Some(root.clone());
                root
            }
        };
        let mirror = mirror_without_isolated(&dir, &root.join(mirrors.len().to_string()))?;
        mirrors.insert(key, mirror.clone());
        entries.push(mirror);
    }

    let isolated = std::env::join_paths(&entries).context("could not rebuild PATH")?;
    for name in ISOLATED {
        if let Some(found) = resolve_on_path(name, &isolated) {
            bail!(
                "the application's PATH still resolves {name} to {} -- refusing to launch it with \
                 a developer-installed tool it must not reach",
                found.display()
            );
        }
    }
    Ok(isolated)
}

/// Whether `dir` has an entry under any isolated name, executable or not.
/// Mirroring a directory that did not need it costs nothing; missing one
/// that did would be the whole failure.
fn holds_isolated(dir: &Path) -> bool {
    ISOLATED
        .iter()
        .any(|name| std::fs::symlink_metadata(dir.join(name)).is_ok())
}

/// A new, empty directory under `parent`. Sessions of one test share a state
/// root, and a session must never build into a mirror another one uses.
fn claim_fresh(parent: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;
    for index in 0u32.. {
        let candidate = parent.join(index.to_string());
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("could not create {}", candidate.display()))
            }
        }
    }
    unreachable!("u32 exhausted claiming a PATH mirror")
}

/// Link every entry of `source` except the isolated names into `mirror`.
///
/// Links point at the original path, not its resolved target, so an entry
/// that is itself a link behaves exactly as it did.
fn mirror_without_isolated(source: &Path, mirror: &Path) -> Result<PathBuf> {
    std::fs::create_dir(mirror).with_context(|| format!("could not create {}", mirror.display()))?;
    let entries = std::fs::read_dir(source).with_context(|| {
        format!(
            "{} holds an isolated tool but cannot be listed to mirror the rest of it",
            source.display()
        )
    })?;
    for entry in entries {
        let name = entry
            .with_context(|| format!("could not read an entry of {}", source.display()))?
            .file_name();
        if ISOLATED.iter().any(|isolated| name == OsStr::new(isolated)) {
            continue;
        }
        std::os::unix::fs::symlink(source.join(&name), mirror.join(&name)).with_context(|| {
            format!(
                "could not link {} into {}",
                source.join(&name).display(),
                mirror.display()
            )
        })?;
    }
    Ok(mirror.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "slashit-developer-tools-{}-{}-{label}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    fn executable(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("create the bin directory");
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write the executable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
        path
    }

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).expect("join the PATH")
    }

    /// The developer's directory keeps everything but the review tool, in
    /// its original position; the directories around it are untouched.
    #[test]
    fn only_the_isolated_name_disappears() {
        let root = scratch("only-name");
        let fake_bin = root.join("fake-bin");
        let developer = root.join("developer-bin");
        let system = root.join("system-bin");
        let claude = executable(&fake_bin, "claude");
        executable(&developer, "coderabbit");
        executable(&developer, "helper");
        executable(&developer, "git");
        let system_git = executable(&system, "git");
        executable(&system, "jj");

        let isolated = isolate(
            &path_of(&[&fake_bin, &developer, &system]),
            &root.join("mirrors"),
        )
        .expect("isolate");
        let entries: Vec<PathBuf> = std::env::split_paths(&isolated).collect();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], fake_bin);
        assert_ne!(entries[1], developer, "the developer directory must be mirrored");
        assert!(entries[1].starts_with(root.join("mirrors")));
        assert_eq!(entries[2], system);

        assert_eq!(resolve_on_path("coderabbit", &isolated), None);
        assert_eq!(resolve_on_path("claude", &isolated), Some(claude));
        // A sibling still resolves in the same position, to the same file.
        let helper = resolve_on_path("helper", &isolated).expect("the sibling resolves");
        assert_eq!(helper, entries[1].join("helper"));
        assert_eq!(
            helper.canonicalize().unwrap(),
            developer.join("helper").canonicalize().unwrap()
        );
        // Precedence is unchanged: the developer's git still shadows the
        // system one, exactly as it did before isolation.
        let git = resolve_on_path("git", &isolated).expect("git resolves");
        assert_eq!(git.canonicalize().unwrap(), developer.join("git").canonicalize().unwrap());
        assert_ne!(git, system_git);
        assert!(resolve_on_path("jj", &isolated).is_some());

        std::fs::remove_dir_all(root).ok();
    }

    /// Isolating the application's `PATH` must leave the developer's own
    /// machine exactly as it was: the file is still there, and a shell with
    /// the original `PATH` still finds it.
    #[test]
    fn the_developer_installation_is_untouched() {
        let root = scratch("untouched");
        let developer = root.join("developer-bin");
        let tool = executable(&developer, "coderabbit");
        let original = path_of(&[&developer]);
        let inherited_before = std::env::var_os("PATH");

        isolate(&original, &root.join("mirrors")).expect("isolate");

        assert!(tool.is_file());
        assert_eq!(resolve_on_path("coderabbit", &original), Some(tool));
        assert_eq!(std::env::var_os("PATH"), inherited_before);

        std::fs::remove_dir_all(root).ok();
    }

    /// A directory named several times gets one mirror, and nothing is built
    /// at all when no entry needs one.
    #[test]
    fn duplicates_share_a_mirror_and_clean_paths_build_nothing() {
        let root = scratch("duplicates");
        let developer = root.join("developer-bin");
        executable(&developer, "coderabbit");
        let mirrors = root.join("mirrors");

        let isolated = isolate(&path_of(&[&developer, &developer]), &mirrors).expect("isolate");
        let entries: Vec<PathBuf> = std::env::split_paths(&isolated).collect();
        assert_eq!(entries[0], entries[1]);

        let clean = root.join("clean-bin");
        executable(&clean, "git");
        let untouched = isolate(&path_of(&[&clean]), &root.join("unused")).expect("isolate");
        assert_eq!(untouched, path_of(&[&clean]));
        assert!(!root.join("unused").exists());

        std::fs::remove_dir_all(root).ok();
    }

    /// An empty or relative entry means "wherever the looking-up process
    /// is", which for CodeRabbit is a task checkout. It is not passed on.
    #[test]
    fn relative_and_empty_entries_are_dropped() {
        let root = scratch("relative");
        let system = root.join("system-bin");
        executable(&system, "git");
        let mut path = OsString::from(":.:relative-bin:");
        path.push(system.as_os_str());

        let isolated = isolate(&path, &root.join("mirrors")).expect("isolate");
        assert_eq!(isolated, path_of(&[&system]));

        std::fs::remove_dir_all(root).ok();
    }

    /// Two sessions of one test never share, or overwrite, a mirror.
    #[test]
    fn every_call_builds_into_a_fresh_directory() {
        let root = scratch("fresh");
        let developer = root.join("developer-bin");
        executable(&developer, "coderabbit");
        let mirrors = root.join("mirrors");
        let path = path_of(&[&developer]);

        let first = isolate(&path, &mirrors).expect("first");
        let second = isolate(&path, &mirrors).expect("second");
        assert_ne!(first, second);

        std::fs::remove_dir_all(root).ok();
    }

    /// A journey that set its own `PATH` gets that one; otherwise the one the
    /// harness inherited. Either way it then goes through [`isolate`].
    #[test]
    fn the_requested_path_prefers_the_journeys_own() {
        let inherited = Some(OsString::from("/inherited"));
        assert_eq!(requested_path(&[], inherited.clone()), "/inherited");
        let child_env = vec![
            (OsString::from("OTHER"), OsString::from("x")),
            (OsString::from("PATH"), OsString::from("/journey")),
        ];
        assert_eq!(requested_path(&child_env, inherited), "/journey");
        assert_eq!(requested_path(&[], None), "");
    }
}
