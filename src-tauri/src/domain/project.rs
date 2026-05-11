use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Whether a project is standalone or attached to a meta-workspace.
///
/// Stored projects from before the workspace concept existed deserialize as
/// `Standalone` via the `Default` impl + `#[serde(default)]` on the field.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub repository_id: Option<Uuid>,
    #[serde(default)]
    pub scope: ProjectScope,
    pub agent_type: AgentType,
    pub agent_config: AgentConfig,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub agent_type: AgentType,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
}
