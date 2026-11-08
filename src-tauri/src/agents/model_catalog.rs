// SPDX-License-Identifier: MPL-2.0
//! The models a coding runtime can be asked to use, for the model picker.
//!
//! An entry is a catalog or alias entry, never a proof of access. `Catalog`
//! means the runtime's own model list names the model; authentication and
//! provider access are checked only when a run starts. `Unverified` means
//! SlashIt knows the name but has no runtime catalog that lists it, which is
//! the case for Claude Code's aliases and fixed model IDs.
//!
//! A failed refresh never discards the last good list: the caller gets that
//! list with the error, so a Task's saved model stays selectable.

use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

use crate::domain::task::CodingProvider;

/// Bounds one `opencode models` execution.
const OPENCODE_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);

/// Where an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySource {
    /// Claude Code's documented aliases and fixed model IDs.
    Alias,
    /// `opencode models`, which the runtime prints from its configuration.
    OpencodeModels,
}

/// How much SlashIt can say about an entry's availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// The runtime's own catalog lists this model. Access is not verified.
    Catalog,
    /// No runtime catalog lists this model, so nothing is claimed about it.
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeModel {
    pub id: String,
    pub name: String,
    pub source: DiscoverySource,
    pub availability: Availability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelCatalog {
    pub provider: CodingProvider,
    pub models: Vec<RuntimeModel>,
    /// Why the list is not fresh: a refresh failed, or the runtime is missing.
    /// The models shown are the last good list, when one exists.
    pub error: Option<String>,
}

/// The last successful OpenCode discovery, kept for the process lifetime.
static OPENCODE_CACHE: Mutex<Option<Vec<RuntimeModel>>> = Mutex::new(None);

/// Claude Code's aliases and fixed IDs. Always listed, whether or not the CLI
/// is installed, and always `Unverified`: no reliable non-interactive account
/// listing exists.
pub fn claude_models() -> Vec<RuntimeModel> {
    let entry = |id: &str, name: &str| RuntimeModel {
        id: id.to_string(),
        name: name.to_string(),
        source: DiscoverySource::Alias,
        availability: Availability::Unverified,
    };
    vec![
        entry("default", "Default (auto)"),
        entry("sonnet", "Claude Sonnet (latest alias)"),
        entry("opus", "Claude Opus (latest alias)"),
        entry("haiku", "Claude Haiku (latest alias)"),
        entry("claude-sonnet-4-6", "Claude Sonnet 4.6"),
        entry("claude-opus-4-6", "Claude Opus 4.6"),
        entry("claude-haiku-4-5-20251001", "Claude Haiku 4.5"),
    ]
}

/// Parses `opencode models` output: one `provider/model` per line. Blank lines
/// and anything that is not a provider/model id are skipped, and a repeated id
/// is kept once, in its first position.
pub fn parse_opencode_models(stdout: &str) -> Vec<RuntimeModel> {
    let mut seen = std::collections::HashSet::new();
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| is_provider_model(line))
        .filter(|line| seen.insert(line.to_string()))
        .map(|id| RuntimeModel {
            id: id.to_string(),
            name: id.to_string(),
            source: DiscoverySource::OpencodeModels,
            availability: Availability::Catalog,
        })
        .collect()
}

fn is_provider_model(line: &str) -> bool {
    match line.split_once('/') {
        Some((provider, name)) => {
            !provider.is_empty()
                && !name.is_empty()
                && line.len() <= 200
                && !line.chars().any(|c| c.is_whitespace() || c.is_control())
        }
        None => false,
    }
}

/// Runs `opencode models` under the same isolated configuration a Worker gets,
/// so the picker lists only models a Worker can select. `--standalone` keeps the
/// listing off the shared background service, which does not read this
/// environment. The process group is killed when the deadline passes, so the
/// private server it starts does not outlive the listing.
async fn run_opencode_models() -> Result<Vec<RuntimeModel>, String> {
    let config_dir = tempfile::Builder::new()
        .prefix("slashit-opencode-discovery-")
        .tempdir()
        .map_err(|e| format!("The OpenCode discovery directory could not be created: {e}"))?;
    let config_path = config_dir
        .path()
        .to_str()
        .ok_or_else(|| "The OpenCode discovery directory path is not UTF-8".to_string())?
        .to_string();
    let mut command = tokio::process::Command::new(crate::agents::opencode_run::OPENCODE_PROGRAM);
    command
        .args(["models", "--standalone"])
        .env_clear()
        .envs(crate::agents::opencode_run::opencode_spawn_env(
            std::env::vars_os(),
            &config_path,
        ))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let child = command
        .spawn()
        .map_err(|e| format!("OpenCode could not be started: {e}"))?;
    #[cfg(unix)]
    let group = child.id();
    let output = match tokio::time::timeout(OPENCODE_DISCOVERY_TIMEOUT, child.wait_with_output()).await {
        Ok(output) => output.map_err(|e| format!("OpenCode could not be run: {e}"))?,
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = group {
                // Safety: `pid` is the leader of the group made by `process_group(0)`.
                unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
            }
            return Err("OpenCode did not list its models in time".to_string());
        }
    };
    if !output.status.success() {
        return Err(format!(
            "OpenCode exited with {} while listing its models",
            output.status
        ));
    }
    let models = parse_opencode_models(&String::from_utf8_lossy(&output.stdout));
    if models.is_empty() {
        return Err("OpenCode listed no models".to_string());
    }
    Ok(models)
}

/// The catalog for `provider`. `refresh` runs discovery again; otherwise a
/// cached OpenCode list is returned without running the CLI.
pub async fn catalog(provider: CodingProvider, refresh: bool) -> ModelCatalog {
    match provider {
        CodingProvider::Claude => ModelCatalog {
            provider,
            models: claude_models(),
            error: None,
        },
        CodingProvider::OpenCode => {
            if !refresh {
                if let Some(cached) = OPENCODE_CACHE.lock().ok().and_then(|c| c.clone()) {
                    return ModelCatalog {
                        provider,
                        models: cached,
                        error: None,
                    };
                }
            }
            match run_opencode_models().await {
                Ok(models) => {
                    if let Ok(mut cache) = OPENCODE_CACHE.lock() {
                        *cache = Some(models.clone());
                    }
                    ModelCatalog {
                        provider,
                        models,
                        error: None,
                    }
                }
                Err(error) => {
                    // Keep the last good list: a failed refresh must not empty
                    // the picker or drop a saved model.
                    let models = OPENCODE_CACHE
                        .lock()
                        .ok()
                        .and_then(|c| c.clone())
                        .unwrap_or_default();
                    ModelCatalog {
                        provider,
                        models,
                        error: Some(error),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_output_keeps_provider_model_lines_once_in_order() {
        let stdout = "ollama/llama3.2:3b\n\n  opencode-go/beam-free  \nnot a model\n/leading\ntrailing/\nollama/llama3.2:3b\nbad id/with space\n";
        let ids: Vec<String> = parse_opencode_models(stdout).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["ollama/llama3.2:3b", "opencode-go/beam-free"]);
    }

    #[test]
    fn every_opencode_entry_is_catalog_not_access() {
        let models = parse_opencode_models("ollama/llama3.2:3b\n");
        assert_eq!(models[0].availability, Availability::Catalog);
        assert_eq!(models[0].source, DiscoverySource::OpencodeModels);
    }

    #[test]
    fn claude_entries_are_aliases_and_never_claim_availability() {
        let models = claude_models();
        assert!(models.iter().all(|m| m.source == DiscoverySource::Alias));
        assert!(models.iter().all(|m| m.availability == Availability::Unverified));
        assert_eq!(models[0].id, "default");
    }
}
