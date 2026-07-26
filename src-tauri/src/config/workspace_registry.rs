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

impl WorkspaceRegistry {
    pub fn load() -> io::Result<Self> {
        let config_path = Self::config_path()?;
        let workspaces = if config_path.exists() {
            let content = fs::read_to_string(&config_path)?;
            match toml::from_str::<RegistryFile>(&content) {
                Ok(parsed) => parsed.workspaces.into_iter().map(|w| (w.id, w)).collect(),
                Err(e) => {
                    // A bad file should not block startup. Quarantine and start empty
                    // so the user can recreate workspaces and SlashIt remains usable.
                    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                    let quarantine = config_path.with_extension(format!("toml.corrupt-{ts}"));
                    let _ = fs::rename(&config_path, &quarantine);
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
        // partial write cannot leave the registry truncated.
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, toml_text)?;
        fs::rename(&tmp, path)
    }

}
