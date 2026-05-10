use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum McpCategory {
    Documentation,
    Tools,
    Memory,
    Integration,
    Automation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpServer {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub category: McpCategory,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ThinkingMode {
    UltraThink,
    High,
    Low,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentRole {
    SpecCreation,
    Build,
    QA,
    Utility,
    Ideation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpAgent {
    pub id: String,
    pub name: String,
    pub model: String,
    pub thinking_mode: ThinkingMode,
    pub description: String,
    pub mcp_count: usize,
    pub role: AgentRole,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpAgentConfig {
    pub model: Option<String>,
    pub thinking_mode: Option<String>,
}
