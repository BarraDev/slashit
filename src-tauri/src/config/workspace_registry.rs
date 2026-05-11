//! Workspace registry — OS-level index of all SlashIt meta-workspaces.
//!
//! Stored as TOML at `<config_dir>/slashit/workspaces.toml`. Each entry holds
//! the workspace id, display name, and root path on disk. Per-workspace state
//! (open worktrees, last agent, UI state) lives in `<root_path>/.slashit/`.

use crate::domain::Workspace;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const WORKSPACES_FILE: &str = "workspaces.toml";
const PER_WORKSPACE_DIR: &str = ".slashit";

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
        Self::ensure_per_workspace_dir(workspace.root_path.as_path())?;
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
        let proj_dirs = directories::ProjectDirs::from("com", "barradev", "slashit-app")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no project dirs"))?;
        let dir = proj_dirs.config_dir().to_path_buf();
        fs::create_dir_all(&dir)?;
        Ok(dir.join(WORKSPACES_FILE))
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

    fn ensure_per_workspace_dir(root: &Path) -> io::Result<()> {
        let dir = root.join(PER_WORKSPACE_DIR);
        fs::create_dir_all(dir)
    }
}
