// SPDX-License-Identifier: MPL-2.0
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A product-level container over several projects.
///
/// Mirrors `src-tauri/src/domain/workspace.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    /// Advanced: the Shared Agent Context Directory. Absent for a Workspace
    /// that only groups Projects.
    #[serde(default)]
    pub root_path: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}
