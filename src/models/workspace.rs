use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A meta-workspace folder that coordinates one or more projects.
///
/// Mirrors `src-tauri/src/domain/workspace.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    pub root_path: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}
