use crate::domain::{AgentExecution, AgentStatus, AgentLogEntry};
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

type AgentExecutions = Arc<RwLock<HashMap<Uuid, AgentExecution>>>;
type AgentLogs = Arc<RwLock<HashMap<Uuid, Vec<AgentLogEntry>>>>;

/// The execution and log records the executor owns. The executor and the IPC
/// server share these maps; nothing else writes to them.
#[derive(Clone)]
pub struct AgentState {
    pub executions: AgentExecutions,
    pub logs: AgentLogs,
}

impl AgentState {
    pub fn new() -> Self {
        Self {
            executions: Arc::new(RwLock::new(HashMap::new())),
            logs: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new()
    }
}

/// Count provider processes SlashIt is running, without counting an executor
/// coding record twice.
///
/// The executor returns execution ids for the records it inserted itself and
/// separate task ids for provider flows that have no `AgentExecution` record
/// (AI review/fix, PR helpers, and a pre-record coding start). An active record
/// the executor does not own is still counted as-is: it is a provider process
/// nothing else would wait for at quit. A shared helper keeps tray, quit, and
/// IPC status on the same safety rule.
pub(crate) async fn active_agent_count(
    executions: &AgentExecutions,
    executor: Option<&crate::queue::TaskExecutor>,
) -> usize {
    let (executor_execution_ids, executor_task_ids) = match executor {
        Some(executor) => executor.active_agent_owners().await,
        None => (std::collections::HashSet::new(), std::collections::HashSet::new()),
    };
    let unowned_count = executions
        .read()
        .await
        .values()
        .filter(|execution| {
            matches!(
                execution.status,
                AgentStatus::Starting | AgentStatus::Running | AgentStatus::Stopping
            ) && !executor_execution_ids.contains(&execution.id)
        })
        .count();
    // Every executor-owned coding run has an execution id as soon as it is
    // registered, even during the short window before its record exists.
    // Those records are excluded above, so this counts each owned provider
    // exactly once while still covering that pre-record window.
    executor_execution_ids.len() + executor_task_ids.len() + unowned_count
        + executor.map_or(0, crate::queue::TaskExecutor::counted_project_run_count)
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
