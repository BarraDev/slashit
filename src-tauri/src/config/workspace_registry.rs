//! Workspace registry — OS-level index of all SlashIt meta-workspaces.
//!
//! Stored as TOML at the path [`AppPaths::workspaces_file`] resolves to. Each
//! entry holds the workspace id, display name, and root path on disk.
//!
//! Registering a workspace deliberately creates *nothing* inside the user's
//! folder. An earlier version created an empty `.slashit/` directory in every
//! registered root and never wrote to it; where a workspace's state lives is
//! now the user's decision, expressed as `StateLocation` and acted on by
//! [`crate::config::migration::StateMigrator`].

use super::migration::StateMigrator;
use super::paths::{AppPaths, IN_PROJECT_DIR};
use crate::domain::Workspace;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    workspaces: Vec<Workspace>,
}

pub struct WorkspaceRegistry {
    workspaces: HashMap<Uuid, Workspace>,
    config_path: PathBuf,
}

/// A `toml.corrupt-<timestamp>` path guaranteed not to already exist.
///
/// The timestamp has one-second resolution, so a second corruption handled
/// within the same second — plausible in a crash-loop, and easy to hit in
/// tests — would otherwise make `fs::rename` silently replace the first
/// backup instead of preserving both.
fn unique_quarantine_path(config_path: &Path, ts: &str) -> PathBuf {
    let base = config_path.with_extension(format!("toml.corrupt-{ts}"));
    if !base.exists() {
        return base;
    }
    let mut n: u32 = 1;
    loop {
        let candidate = config_path.with_extension(format!("toml.corrupt-{ts}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

impl WorkspaceRegistry {
    pub fn load() -> io::Result<Self> {
        Self::load_from(Self::config_path()?)
    }

    /// Load from an explicit path, split out from `load()` so tests can point
    /// it at a tempdir instead of the real OS config directory.
    fn load_from(config_path: PathBuf) -> io::Result<Self> {
        let workspaces = if config_path.exists() {
            let content = fs::read_to_string(&config_path)?;
            match toml::from_str::<RegistryFile>(&content) {
                Ok(parsed) => parsed.workspaces.into_iter().map(|w| (w.id, w)).collect(),
                Err(e) => {
                    // A bad file should not block startup. Quarantine and start empty
                    // so the user can recreate workspaces and SlashIt remains usable —
                    // but only once the corrupt file is actually safely backed up. If
                    // the rename itself fails (permissions, cross-device, ...), the
                    // corrupt file would be left in place, unbacked-up, while an empty
                    // *writable* registry proceeded — the next save would then
                    // silently overwrite that unbacked-up file with a near-empty one.
                    // Fail closed instead, the same as the unreadable-file case above.
                    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                    let quarantine = unique_quarantine_path(&config_path, &ts.to_string());
                    if let Err(rename_err) = fs::rename(&config_path, &quarantine) {
                        eprintln!(
                            "[workspace-registry] failed to parse {}: {e}. Quarantine to {} also failed: {rename_err}. Leaving the corrupt file in place.",
                            config_path.display(),
                            quarantine.display()
                        );
                        return Err(rename_err);
                    }
                    eprintln!(
                        "[workspace-registry] failed to parse {}: {e}. Quarantined to {}.",
                        config_path.display(),
                        quarantine.display()
                    );
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };
        Ok(Self { workspaces, config_path })
    }

    pub fn upsert(&mut self, workspace: Workspace) -> io::Result<()> {
        // Stage the change in a clone, persist, and only commit to self on success.
        // This keeps the in-memory map consistent with disk even if the write fails.
        let mut next = self.workspaces.clone();
        next.insert(workspace.id, workspace);
        Self::persist_map(&self.config_path, &next)?;
        self.workspaces = next;
        Ok(())
    }

    pub fn remove(&mut self, id: &Uuid) -> io::Result<bool> {
        if !self.workspaces.contains_key(id) {
            return Ok(false);
        }
        let mut next = self.workspaces.clone();
        next.remove(id);
        Self::persist_map(&self.config_path, &next)?;
        self.workspaces = next;
        Ok(true)
    }

    pub fn get(&self, id: &Uuid) -> Option<&Workspace> {
        self.workspaces.get(id)
    }

    pub fn all(&self) -> impl Iterator<Item = &Workspace> {
        self.workspaces.values()
    }

    pub fn contains_root(&self, root: &Path) -> bool {
        self.workspaces
            .values()
            .any(|w| w.root_path.as_path() == root)
    }

    fn config_path() -> io::Result<PathBuf> {
        Ok(AppPaths::new()?.workspaces_file())
    }

    /// Delete the empty `.slashit/` directories left behind by earlier versions.
    ///
    /// Only provably-empty directories are removed, so a workspace that has
    /// genuinely opted into in-project state is never disturbed. Returns how
    /// many were cleaned up.
    pub fn clean_legacy_state_dirs(&self) -> usize {
        self.workspaces
            .values()
            .filter(|w| {
                StateMigrator::remove_empty_legacy_dir(
                    &w.root_path.as_path().join(IN_PROJECT_DIR),
                )
            })
            .count()
    }

    fn persist_map(path: &Path, map: &HashMap<Uuid, Workspace>) -> io::Result<()> {
        let file = RegistryFile {
            workspaces: map.values().cloned().collect(),
        };
        let toml_text = toml::to_string_pretty(&file)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        // Write to a sibling temp file then atomically rename so a crash or
        // partial write cannot leave the registry truncated. The temp name is
        // unique per call (shared with `storage::write_atomic`) so concurrent
        // writers never race on the same temp path.
        let tmp = crate::config::storage::unique_temp_path(path);
        fs::write(&tmp, toml_text)?;
        fs::rename(&tmp, path)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_registry_file_loads_empty() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("workspaces.toml");

        let registry = WorkspaceRegistry::load_from(path).expect("should load a default registry");

        assert!(registry.all().next().is_none());
    }

    #[test]
    fn corrupt_file_is_quarantined_and_registry_starts_empty() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("workspaces.toml");
        fs::write(&path, "this is not valid toml {{{").expect("write corrupt file");

        let registry =
            WorkspaceRegistry::load_from(path.clone()).expect("a quarantinable file should still load");

        assert!(registry.all().next().is_none());
        assert!(!path.exists(), "corrupt file should have been moved aside");

        let quarantined: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("corrupt-"))
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine backup should exist");
    }

    #[test]
    fn a_second_corruption_in_the_same_second_does_not_replace_the_first_backup() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("workspaces.toml");
        let ts = "20260101-000000";

        // Simulate a quarantine backup already sitting at the exact path a
        // naive `toml.corrupt-<timestamp>` scheme would compute for a second
        // corruption handled within the same second.
        let first_backup = path.with_extension(format!("toml.corrupt-{ts}"));
        fs::write(&first_backup, "first crash's corrupt content\n").unwrap();

        let second = unique_quarantine_path(&path, ts);

        assert_ne!(second, first_backup, "a colliding timestamp must not reuse the existing backup's path");
        fs::write(&second, "second crash's corrupt content\n").unwrap();
        assert_eq!(
            fs::read_to_string(&first_backup).unwrap(),
            "first crash's corrupt content\n",
            "the first backup must survive a second corruption untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn quarantine_rename_failure_returns_err_instead_of_empty_registry() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("workspaces.toml");
        fs::write(&path, "this is not valid toml {{{").expect("write corrupt file");

        // Remove write permission on the parent directory so the quarantine
        // rename cannot happen: renaming requires write access to the
        // directory holding both the source and destination names.
        let original_perms = fs::metadata(temp.path()).unwrap().permissions();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o555)).unwrap();

        let result = WorkspaceRegistry::load_from(path.clone());

        // Restore so the tempdir can be cleaned up regardless of the outcome.
        fs::set_permissions(temp.path(), original_perms).unwrap();

        assert!(
            result.is_err(),
            "a failed quarantine rename must fail load() rather than silently starting empty"
        );
        assert!(
            path.exists(),
            "the corrupt file must be left in place when it could not be quarantined"
        );
    }
}
