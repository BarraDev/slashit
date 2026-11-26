// SPDX-License-Identifier: MPL-2.0
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A project's relationship to workspaces. Mirrors
/// `src-tauri/src/domain/project.rs`'s `ProjectScope`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectScope {
    #[default]
    Standalone,
    InWorkspace { workspace_id: Uuid },
}

impl ProjectScope {
    pub fn workspace_id(&self) -> Option<Uuid> {
        match self {
            Self::Standalone => None,
            Self::InWorkspace { workspace_id } => Some(*workspace_id),
        }
    }
}

/// A Project's defaults for the PR Comment Review toggles. Mirrors
/// `src-tauri/src/domain/project.rs`'s `PrReviewDefaults`; both are off unless set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrReviewDefaults {
    #[serde(default)]
    pub auto_push: bool,
    #[serde(default)]
    pub auto_reply: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub repository_id: Option<Uuid>,
    #[serde(default)]
    pub scope: ProjectScope,
    pub agent_type: AgentType,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub pr_review_defaults: PrReviewDefaults,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    ClaudeCode,
    Cursor,
    Cody,
    Continue,
    Other(String),
}

