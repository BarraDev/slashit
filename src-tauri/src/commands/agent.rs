// SPDX-License-Identifier: MPL-2.0
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

/// The models a runtime offers the picker, each labelled with where it came
/// from and how much is known about it. `refresh` runs discovery again.
#[tauri::command]
pub async fn list_runtime_models(
    provider: crate::domain::task::CodingProvider,
    refresh: bool,
) -> Result<crate::agents::model_catalog::ModelCatalog, String> {
    Ok(crate::agents::model_catalog::catalog(provider, refresh).await)
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

/// Why `model` may not be saved as a Task's Worker model, or `None` when it may.
///
/// The rule is the one the desktop's own Task form applies to the field, so the
/// control channel adds none of its own: the identifier is read through the
/// same list the model picker is built from, and for an OpenCode Worker it
/// must be a `provider/model` id, the only form the run's launch accepts —
/// so a Task can never be configured to a model it would be refused a run on.
///
/// The requested identifier is what callers save, whatever this returns:
/// choices are kept exactly as made, because the provider resolves them and
/// SlashIt never substitutes another model.
pub(crate) async fn worker_model_refusal(
    provider: crate::domain::task::CodingProvider,
    model: &str,
) -> Option<String> {
    match provider {
        // An OpenCode Task is never run on an implicit or Claude-shaped model
        // either; the same check refuses the launch.
        crate::domain::task::CodingProvider::OpenCode => {
            if crate::queue::executor::is_opencode_model_id(model) {
                None
            } else {
                Some(format!(
                    "OpenCode takes a model as provider/model, for example \
                     ollama/llama3.2:3b, and {model} is not one, so it was not saved. Choose an \
                     OpenCode model for this Task, or run it with Claude."
                ))
            }
        }
        crate::domain::task::CodingProvider::Claude => {
            let offered = match list_available_models().await {
                Ok(models) => models,
                Err(e) => return Some(format!(
                    "the list of models the Claude Worker can take could not be read, so \
                     \"{model}\" was not saved: {e}"
                )),
            };
            if offered.iter().any(|m| m.id == model) {
                None
            } else {
                let ids = offered
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(format!(
                    "\"{model}\" is not a model a Claude Worker can take, so it was not saved. \
                     The model picker offers: {ids}. A provider/model identifier belongs to an \
                     OpenCode Task."
                ))
            }
        }
    }
}

#[cfg(test)]
mod worker_model_refusal_tests {
    use super::*;

    /// Whatever the environment offers, every offered identifier is written
    /// and an identifier outside it is refused. The offered list is read the
    /// same way the write reads it, so the test tracks the model picker.
    #[tokio::test]
    async fn a_claude_worker_takes_exactly_what_the_model_picker_offers() {
        let offered = list_available_models().await.expect("the offered models");
        for id in &offered {
            assert!(
                worker_model_refusal(crate::domain::task::CodingProvider::Claude, &id.id)
                    .await
                    .is_none(),
                "{:?} is offered, so a Task may be configured to it",
                id.id
            );
        }
        let refusal = worker_model_refusal(
            crate::domain::task::CodingProvider::Claude,
            "not-a-model-the-picker-offers",
        )
        .await
        .expect("refused");
        assert!(refusal.contains("not-a-model-the-picker-offers"), "{refusal}");
        assert!(refusal.contains("was not saved"), "{refusal}");
    }

    /// A `provider/model` id is never a Claude model, and an OpenCode Worker
    /// takes no other form — the launch would refuse it later, so the write
    /// refuses it now.
    #[tokio::test]
    async fn an_opencode_worker_takes_only_a_provider_model_id() {
        use crate::domain::task::CodingProvider;

        assert!(
            worker_model_refusal(CodingProvider::OpenCode, "ollama/llama3.2:3b")
                .await
                .is_none()
        );
        for refused in ["default", "sonnet", "ollama/", "/llama3.2:3b", "ollama/llama 3"] {
            let refusal = worker_model_refusal(CodingProvider::OpenCode, refused)
                .await
                .unwrap_or_else(|| panic!("{refused} must be refused"));
            assert!(refusal.contains("provider/model"), "{refusal}");
            assert!(refusal.contains("was not saved"), "{refusal}");
        }
    }
}
