// SPDX-License-Identifier: MPL-2.0
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

/// A named group of Projects.
///
/// Membership is not tracked here; a Project names the Workspace it belongs
/// to via `ProjectScope`.
///
/// `root_path` is an advanced, optional Shared Agent Context Directory. When
/// it is set, its `CLAUDE.md` and `AGENTS.md` are copied as text into the
/// instructions of member Projects' coding Workers. A Worker still starts in
/// its Task Checkout, and the folder is never granted to it as a directory.
/// When it is absent, the Workspace is grouping only. Records written before
/// the root became optional still carry it and keep their meaning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_path: Option<WorkspaceRoot>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Workspace {
    pub fn new(name: String, root_path: Option<WorkspaceRoot>) -> Self {
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
