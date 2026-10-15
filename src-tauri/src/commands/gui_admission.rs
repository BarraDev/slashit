//! The shutdown boundary for desktop commands.
//!
//! Closing the process-wide admission gate (see `TaskExecutor::close_admission`)
//! is what lets the instance prove it is inert before it gives up its local
//! ownership. Worker starts, reviews, Project Runs and PR helpers already pass
//! through that gate. A desktop command is a different entry: it can save a
//! Task, move a Git reference, publish a PR or spawn a provider without ever
//! going through the executor, and Tauri runs it on its own task, so nothing
//! else would wait for it.
//!
//! A command that changes anything takes a [`GuiMutation`] argument. Tauri
//! builds the arguments inside the command's own future, when it first runs:
//! if the gate is closed the invocation is refused before the command body
//! runs, and otherwise the lease lives exactly as long as that future, because
//! it is one of the function's own parameters. A command that was spawned
//! before the gate closed but first polled after it is refused, which is safe. The drain therefore waits for
//! every admitted command, and no command can start changing state after the
//! gate closed. The frontend does not send the argument; it is not part of any
//! `invoke()` payload.
//!
//! `every_desktop_command_is_classified` keeps the boundary complete: adding a
//! command without either the argument or an entry in [`READ_ONLY`] /
//! [`LIFECYCLE`] fails that test.

use tauri::ipc::{CommandArg, CommandItem, InvokeError};
use tauri::Runtime;

use crate::queue::executor::AdmissionLease;

/// Proof that the process still accepted work when this command was
/// dispatched, held until the command finishes.
pub struct GuiMutation {
    _lease: Option<AdmissionLease>,
}

impl GuiMutation {
    /// For callers that invoke a command function directly, outside Tauri's
    /// dispatch and so outside any drain: tests, and a command that delegates
    /// to another one. It holds no lease and admits nothing.
    pub fn untracked() -> Self {
        Self { _lease: None }
    }
}

impl<'de, R: Runtime> CommandArg<'de, R> for GuiMutation {
    fn from_command(command: CommandItem<'de, R>) -> Result<Self, InvokeError> {
        let executor = command
            .message
            .state_ref()
            .try_get::<crate::AppState>()
            .and_then(|state| state.executor.get().cloned());
        // Commands are only dispatched after setup has installed the executor.
        // Before that there is no drain to race.
        let lease = match executor {
            Some(executor) => Some(
                executor
                    .admit_external_start()
                    .map_err(|reason| InvokeError::from_anyhow(anyhow::anyhow!(reason)))?,
            ),
            None => None,
        };
        Ok(Self { _lease: lease })
    }
}

/// Commands that only read. They are allowed to run, and to finish, while the
/// instance drains, and nothing waits for them.
#[cfg(test)]
const READ_ONLY: &[&str] = &[
    "greet",
    "get_theme",
    "list_themes",
    "get_appearance_mode",
    "get_use_project_rail",
    "get_execution_status",
    "get_storage_usage",
    "get_new_work_pause",
    "get_project_readiness",
    "check_worktree_exists",
    "get_task_run",
    "get_task_output",
    "get_live_runs",
    "get_feature_flags",
    "describe_feature_flags",
    "list_files",
    "read_file",
    "search_files",
    "pick_folder",
    "check_is_git_repo",
    "get_file_info",
    "get_issues",
    "get_issue",
    "get_prs",
    "get_pr",
    "check_acli_available",
    "list_jira_projects",
    "get_agent_status",
    "get_agent_logs",
    "check_claude_cli",
    "list_available_models",
    "list_mcp_servers",
    "get_mcp_agents",
    "search_memories",
    "get_graph_status",
    "list_projects",
    "get_project",
    "get_project_path",
    "get_qa_history",
    "check_recurring_issues",
    "get_queue_config",
    "get_queue_position",
    "get_queue_capacity",
    "get_in_progress_count",
    "list_repositories",
    "get_repository",
    "list_roadmap_features",
    "get_roadmap_feature",
    "get_session_history",
    "list_sessions",
    "get_state_location",
    "list_tasks",
    "updater_status",
    "get_workflow_config",
    "list_workflows",
    "get_workflow",
    "get_workflow_logs",
    "list_workspaces",
    "get_workspace",
    "list_pty_sessions",
    "get_pty_scrollback",
    "get_active_process_count",
];

/// The commands that start or perform the shutdown itself. Refusing them
/// would make the drain impossible to request.
#[cfg(test)]
const LIFECYCLE: &[&str] = &["force_quit", "updater_restart"];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read source directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// `(name, parameter list)` of every `#[tauri::command]` function.
    fn commands() -> Vec<(String, String)> {
        let mut files = Vec::new();
        rust_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let mut found = Vec::new();
        for file in files {
            let source = std::fs::read_to_string(&file).expect("read source file");
            // Attribute lines only: the marker also appears in comments and
            // in text a test feeds to a parser.
            let mut rest = source.as_str();
            while let Some(at) = rest.find("#[tauri::command]") {
                let line_start = rest[..at].rfind('\n').map_or(0, |i| i + 1);
                let is_attribute = rest[line_start..at].trim().is_empty()
                    && rest[at + "#[tauri::command]".len()..].trim_start_matches([' ', '\r']).starts_with('\n');
                rest = &rest[at + "#[tauri::command]".len()..];
                if !is_attribute {
                    continue;
                }
                let fn_at = rest.find("fn ").expect("a command is a function");
                let after = &rest[fn_at + 3..];
                let name: String = after
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                let open = after.find('(').expect("parameter list");
                let mut depth = 0usize;
                let mut close = open;
                for (offset, c) in after[open..].char_indices() {
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                close = open + offset;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                found.push((name, after[open..=close].to_string()));
            }
        }
        found
    }

    #[test]
    fn every_desktop_command_is_classified() {
        let commands = commands();
        assert!(commands.len() > 150, "the scan must find the command surface");

        let unclassified: Vec<_> = commands
            .iter()
            .filter(|(name, params)| {
                !params.contains("_gui: GuiMutation")
                    && !READ_ONLY.contains(&name.as_str())
                    && !LIFECYCLE.contains(&name.as_str())
            })
            .map(|(name, _)| name.as_str())
            .collect();
        assert!(
            unclassified.is_empty(),
            "these commands neither take a GuiMutation nor are declared read-only or \
             lifecycle: {unclassified:?}"
        );

        let names: BTreeSet<_> = commands.iter().map(|(name, _)| name.as_str()).collect();

        // Whatever the scan missed, the registered handler list is the truth
        // about what the webview can call.
        let lib = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"))
            .expect("read lib.rs");
        let registered = lib
            .split("generate_handler![")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .expect("the handler list");
        let unscanned: Vec<_> = registered
            .split(',')
            .map(|name| name.trim().rsplit("::").next().unwrap_or_default())
            .filter(|name| !name.is_empty() && !names.contains(name))
            .collect();
        assert!(unscanned.is_empty(), "registered but not found by the scan: {unscanned:?}");
        for listed in READ_ONLY.iter().chain(LIFECYCLE) {
            assert!(names.contains(listed), "{listed} is classified but is not a command");
        }
        for (name, params) in &commands {
            if READ_ONLY.contains(&name.as_str()) || LIFECYCLE.contains(&name.as_str()) {
                assert!(
                    !params.contains("GuiMutation"),
                    "{name} is classified as not mutating but takes a GuiMutation"
                );
            }
        }
    }
}
