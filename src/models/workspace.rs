use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A product-level container over several projects.
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
