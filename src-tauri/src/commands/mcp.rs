use crate::domain::mcp::{McpServer, McpAgent, McpAgentConfig};

#[derive(Clone)]
pub struct McpState;

impl McpState {
    pub fn new() -> Self {
        Self
    }
}

impl Default for McpState {
    fn default() -> Self {
        Self::new()
    }
}

#[tauri::command]
pub async fn list_mcp_servers() -> Vec<McpServer> {
    vec![
        McpServer {
            id: "context7".to_string(),
            name: "Context7".to_string(),
            description: "Documentation lookup".to_string(),
            enabled: true,
            category: crate::domain::mcp::McpCategory::Documentation,
        },
        McpServer {
            id: "auto-claude-tools".to_string(),
            name: "Auto-Claude Tools".to_string(),
            description: "Build progress".to_string(),
            enabled: true,
            category: crate::domain::mcp::McpCategory::Tools,
        },
        McpServer {
            id: "graphiti-memory".to_string(),
            name: "Graphiti Memory".to_string(),
            description: "Persistent memory".to_string(),
            enabled: true,
            category: crate::domain::mcp::McpCategory::Memory,
        },
    ]
}

#[tauri::command]
pub async fn toggle_mcp_server(_server_id: String, _enabled: bool) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
pub async fn get_mcp_agents() -> Vec<McpAgent> {
    vec![
        McpAgent {
            id: "spec-writer".to_string(),
            name: "Spec Writer".to_string(),
            model: "Opus 4.5".to_string(),
            thinking_mode: crate::domain::mcp::ThinkingMode::UltraThink,
            description: "Writes comprehensive specifications".to_string(),
            mcp_count: 2,
            role: crate::domain::mcp::AgentRole::SpecCreation,
        },
    ]
}

#[tauri::command]
pub async fn configure_agent(_agent_id: String, _config: McpAgentConfig) -> Result<(), String> {
    Ok(())
}
