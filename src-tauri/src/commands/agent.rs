use crate::domain::{AgentExecution, AgentStatus, AgentLogEntry, LogLevel};
use crate::acp::AcpClient;
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use std::io::{BufRead, BufReader};

type AgentExecutions = Arc<RwLock<HashMap<Uuid, AgentExecution>>>;
type RunningClients = Arc<RwLock<HashMap<Uuid, Arc<AcpClient>>>>;
type AgentLogs = Arc<RwLock<HashMap<Uuid, Vec<AgentLogEntry>>>>;

#[derive(Clone)]
pub struct AgentState {
    pub executions: AgentExecutions,
    pub running_clients: RunningClients,
    pub logs: AgentLogs,
}

impl AgentState {
    pub fn new() -> Self {
        Self {
            executions: Arc::new(RwLock::new(HashMap::new())),
            running_clients: Arc::new(RwLock::new(HashMap::new())),
            logs: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new()
    }
}

#[tauri::command]
pub async fn start_agent(
    state: tauri::State<'_, crate::AppState>,
    workspace_id: String,
    task_id: Option<String>,
) -> Result<AgentExecution, String> {
    let workspace_id = Uuid::parse_str(&workspace_id).map_err(|e| e.to_string())?;
    let task_id = task_id.and_then(|t| Uuid::parse_str(&t).ok());
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();

    let execution = AgentExecution {
        id,
        workspace_id,
        task_id,
        agent_type: "claude-code".to_string(),
        status: AgentStatus::Starting,
        started_at: now,
        stopped_at: None,
    };

    state.agent.executions.write().await.insert(id, execution.clone());
    state.agent.logs.write().await.insert(id, Vec::new());

    let client = AcpClient::start("claude", &["--stdio"], &[])
        .map_err(|e| format!("Failed to start agent: {}", e))?;

    let client = Arc::new(client);

    client.initialize("SlashIt".to_string(), "0.1.0".to_string())
        .await
        .map_err(|e| format!("Failed to initialize agent: {}", e))?;

    start_log_collection(client.clone(), id, state.agent.logs.clone());

    state.agent.running_clients.write().await.insert(id, client.clone());
    state.agent.executions.write().await.insert(id, AgentExecution {
        status: AgentStatus::Running,
        ..execution.clone()
    });

    Ok(execution)
}

fn start_log_collection(client: Arc<AcpClient>, execution_id: Uuid, logs: AgentLogs) {
    tokio::spawn(async move {
        let child = client.child.clone();
        let mut child_guard = child.lock().await;
        if let Some(stderr) = child_guard.stderr.as_mut() {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                let log_entry = AgentLogEntry {
                    timestamp: chrono::Utc::now(),
                    level: LogLevel::Info,
                    message: line.clone(),
                };
                logs.write().await.entry(execution_id).or_default().push(log_entry);
            }
        }
    });
}

#[tauri::command]
pub async fn stop_agent(
    state: tauri::State<'_, crate::AppState>,
    execution_id: String,
) -> Result<bool, String> {
    let execution_id = Uuid::parse_str(&execution_id).map_err(|e| e.to_string())?;

    if let Some(client) = state.agent.running_clients.write().await.remove(&execution_id) {
        client.kill().await.map_err(|e| e.to_string())?;

        let mut executions = state.agent.executions.write().await;
        if let Some(execution) = executions.get_mut(&execution_id) {
            execution.status = AgentStatus::Stopped;
            execution.stopped_at = Some(chrono::Utc::now());
        }

        Ok(true)
    } else {
        Ok(false)
    }
}

#[tauri::command]
pub async fn get_agent_status(
    state: tauri::State<'_, crate::AppState>,
    execution_id: String,
) -> Result<Option<AgentExecution>, String> {
    let execution_id = Uuid::parse_str(&execution_id).map_err(|e| e.to_string())?;
    let executions = state.agent.executions.read().await;
    Ok(executions.get(&execution_id).cloned())
}

#[tauri::command]
pub async fn get_agent_logs(
    state: tauri::State<'_, crate::AppState>,
    execution_id: String,
) -> Result<Vec<AgentLogEntry>, String> {
    let execution_id = Uuid::parse_str(&execution_id).map_err(|e| e.to_string())?;
    let logs = state.agent.logs.read().await;
    Ok(logs.get(&execution_id).cloned().unwrap_or_default())
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClaudeCliStatus {
    pub installed: bool,
    pub version: Option<String>,
    pub path: Option<String>,
}

#[tauri::command]
pub async fn check_claude_cli() -> Result<ClaudeCliStatus, String> {
    // Check if claude binary is found
    let which_output = tokio::process::Command::new("which")
        .arg("claude")
        .output()
        .await;

    let path = match which_output {
        Ok(o) if o.status.success() => {
            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
        }
        _ => None,
    };

    if path.is_none() {
        return Ok(ClaudeCliStatus {
            installed: false,
            version: None,
            path: None,
        });
    }

    // Get version
    let version_output = tokio::process::Command::new("claude")
        .arg("--version")
        .output()
        .await;

    let version = match version_output {
        Ok(o) if o.status.success() => {
            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
        }
        _ => None,
    };

    Ok(ClaudeCliStatus {
        installed: true,
        version,
        path,
    })
}

/// List available Claude models by checking the CLI.
#[tauri::command]
pub async fn list_available_models() -> Result<Vec<ModelInfo>, String> {
    let mut models = vec![
        ModelInfo { id: "default".to_string(), name: "Default (auto)".to_string(), alias: None },
    ];

    // Check if Claude CLI is available (fast — no API call)
    let cli_available = tokio::process::Command::new("which")
        .arg("claude")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    if cli_available {
        // Aliases (latest in each family)
        models.push(ModelInfo { id: "sonnet".to_string(), name: "Claude Sonnet (Latest)".to_string(), alias: Some("sonnet".to_string()) });
        models.push(ModelInfo { id: "opus".to_string(), name: "Claude Opus (Latest)".to_string(), alias: Some("opus".to_string()) });
        models.push(ModelInfo { id: "haiku".to_string(), name: "Claude Haiku (Latest)".to_string(), alias: Some("haiku".to_string()) });
        // Specific model IDs
        models.push(ModelInfo { id: "claude-sonnet-4-6".to_string(), name: "Claude Sonnet 4.6".to_string(), alias: None });
        models.push(ModelInfo { id: "claude-opus-4-6".to_string(), name: "Claude Opus 4.6".to_string(), alias: None });
        models.push(ModelInfo { id: "claude-haiku-4-5-20251001".to_string(), name: "Claude Haiku 4.5".to_string(), alias: None });
    }

    Ok(models)
}
