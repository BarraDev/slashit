use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// A canonical, validated path to a workspace root directory.
///
/// `try_new` canonicalizes and verifies the path is a directory at construction
/// time, so callers downstream can treat the path as a real, existing folder.
/// On registry load from disk we trust the stored value without re-validating
/// (the folder may have been removed externally; UI is responsible for
/// surfacing that).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    pub fn try_new(path: impl Into<PathBuf>) -> Result<Self, String> {
        let raw = path.into();
        let canonical = raw
            .canonicalize()
            .map_err(|e| format!("Invalid workspace path '{}': {e}", raw.display()))?;
        if !canonical.is_dir() {
            return Err(format!(
                "Workspace path must be a directory: {}",
                canonical.display()
            ));
        }
        Ok(Self(canonical))
    }

    /// Construct from a path that has already been validated elsewhere
    /// (e.g. when loading from on-disk storage).
    pub fn from_trusted(path: PathBuf) -> Self {
        Self(path)
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for WorkspaceRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// A meta-workspace folder that coordinates one or more projects.
///
/// The workspace folder is what agents are launched from (cwd). It holds
/// shared instruction files (`AGENTS.md`, `CLAUDE.md`, `.agents/`, etc.) and a
/// `projects.toml` registry listing the projects belonging to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    pub root_path: WorkspaceRoot,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Workspace {
    pub fn new(name: String, root_path: WorkspaceRoot) -> Self {
        let now = chrono::Utc::now();
        Self {
            id: Uuid::new_v4(),
            name,
            root_path,
            created_at: now,
            updated_at: now,
        }
    }
}
