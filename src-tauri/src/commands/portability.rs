// SPDX-License-Identifier: MPL-2.0
//! Desktop entry points for portable Project export and import.
//!
//! Each operation calls the same [`Live`] methods the IPC verbs use, so the
//! lock order, path rules and validation exist once. The dialog commands only
//! choose a path. Nothing is written or imported until the frontend calls the
//! export or apply command with a path the user confirmed.

use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

use crate::commands::gui_admission::GuiMutation;
use crate::portability::apply::{ApplyError, ApplyReport};
use crate::portability::live::{ExportOutcome, Live};
use crate::portability::preview::{Conflict, Normalization, Report};

const FILE_EXTENSION: &str = "json";

/// The result of Apply, one variant per outcome the interface must show
/// differently. Only `Applied` changes state the user can rely on.
#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ImportOutcome {
    /// The Project and its Tasks exist. `durable: false` means the change is in
    /// place but may not survive a power loss on this filesystem.
    Applied {
        project_id: String,
        project_name: String,
        tasks: u32,
        normalization: Vec<Normalization>,
        repository_remote: Option<String>,
        durable: bool,
    },
    /// Identity collisions with the target. Nothing changed.
    Refused { conflicts: Vec<Conflict> },
    /// The file changed after it was previewed. Nothing changed.
    ChecksumMismatch { message: String },
    /// The file is not a readable, valid export, or the path is unusable.
    /// Nothing changed.
    Invalid { message: String },
    /// Refused or failed before anything became visible, or reverted. Nothing
    /// changed.
    Failed { message: String },
    /// Partly applied. A start settles it, so the Project must not be treated as
    /// usable until then.
    Incomplete { message: String },
}

fn import_outcome(result: Result<ApplyReport, ApplyError>) -> ImportOutcome {
    match result {
        Ok(report) => ImportOutcome::Applied {
            project_id: report.project_id.to_string(),
            project_name: report.project_name,
            tasks: report.tasks,
            normalization: report.normalization,
            repository_remote: report.repository_remote,
            durable: report.durable,
        },
        Err(ApplyError::Refused(conflicts)) => ImportOutcome::Refused { conflicts },
        Err(error @ ApplyError::ChecksumMismatch { .. }) => ImportOutcome::ChecksumMismatch {
            message: error.to_string(),
        },
        Err(error @ (ApplyError::Bundle(_) | ApplyError::File(_))) => ImportOutcome::Invalid {
            message: error.to_string(),
        },
        Err(ApplyError::Failed(message)) => ImportOutcome::Failed { message },
        Err(ApplyError::Incomplete(message)) => ImportOutcome::Incomplete { message },
    }
}

fn live(state: &crate::AppState) -> Live<'_> {
    Live {
        gate: &state.task.project_gate,
        leases: &state.task_lifecycle_locks,
        tasks: &state.task.tasks,
        projects: &state.project.projects,
        repositories: &state.repository.repositories,
        storage: &state.storage,
    }
}

/// A file name for the export dialog, derived from the Project name.
fn suggested_file_name(project_name: &str) -> String {
    let stem: String = project_name
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
        .collect();
    let stem = stem.trim();
    let stem = if stem.is_empty() { "project" } else { stem };
    format!("{stem}.slashit-project.{FILE_EXTENSION}")
}

/// Ask where to save an export. `None` when the user cancels. Nothing is
/// written here.
#[tauri::command]
pub async fn choose_export_destination(
    app: AppHandle,
    project_name: String,
) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Export Project")
        .set_file_name(suggested_file_name(&project_name))
        .add_filter("SlashIt Project", &[FILE_EXTENSION])
        .save_file(move |path| {
            let _ = tx.send(path.map(|p| p.to_string()));
        });
    rx.await.map_err(|_| "The save dialog closed unexpectedly".to_string())
}

/// Write one Project's export to a new file. Refuses an existing path.
#[tauri::command]
pub async fn export_project(
    _gui: GuiMutation,
    state: State<'_, crate::AppState>,
    project_id: String,
    path: String,
) -> Result<ExportOutcome, String> {
    live(&state).export(&project_id, &path).await
}

/// Ask which export file to import. `None` when the user cancels. Nothing is
/// read or imported here.
#[tauri::command]
pub async fn choose_import_file(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Import Project")
        .add_filter("SlashIt Project", &[FILE_EXTENSION])
        .pick_file(move |path| {
            let _ = tx.send(path.map(|p| p.to_string()));
        });
    rx.await.map_err(|_| "The open dialog closed unexpectedly".to_string())
}

/// Describe what importing a file would do. Reads the file; writes nothing.
#[tauri::command]
pub async fn preview_import(
    state: State<'_, crate::AppState>,
    path: String,
) -> Result<Report, String> {
    live(&state).preview(&path).await
}

/// Import a previewed file. `checksum` must be the one the preview reported
/// for this file, so the user confirms exactly what was shown.
#[tauri::command]
pub async fn apply_import(
    _gui: GuiMutation,
    state: State<'_, crate::AppState>,
    path: String,
    checksum: String,
) -> Result<ImportOutcome, String> {
    Ok(import_outcome(live(&state).apply(&path, &checksum).await))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suggested_name_cannot_carry_a_path() {
        assert_eq!(suggested_file_name("../etc/x"), "etcx.slashit-project.json");
        assert_eq!(suggested_file_name("  "), "project.slashit-project.json");
        assert_eq!(suggested_file_name("Alpha 2"), "Alpha 2.slashit-project.json");
    }

    #[test]
    fn a_completed_apply_keeps_its_durability_and_its_fields() {
        let report = ApplyReport {
            project_id: uuid::Uuid::nil(),
            project_name: "Alpha".into(),
            tasks: 3,
            normalization: vec![],
            repository_remote: Some("https://example.invalid/r.git".into()),
            durable: false,
        };
        match import_outcome(Ok(report)) {
            ImportOutcome::Applied { project_name, tasks, repository_remote, durable, .. } => {
                assert_eq!(project_name, "Alpha");
                assert_eq!(tasks, 3);
                assert_eq!(repository_remote.as_deref(), Some("https://example.invalid/r.git"));
                assert!(!durable, "an unconfirmed apply must stay unconfirmed");
            }
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    #[test]
    fn only_a_successful_apply_reports_applied() {
        let refused = import_outcome(Err(ApplyError::Refused(vec![Conflict {
            kind: "project_id",
            id: "x".into(),
            detail: "exists".into(),
        }])));
        assert!(matches!(refused, ImportOutcome::Refused { .. }));

        let stale = import_outcome(Err(ApplyError::ChecksumMismatch { found: "abc".into() }));
        assert!(matches!(stale, ImportOutcome::ChecksumMismatch { .. }));

        let unreadable = import_outcome(Err(ApplyError::File("the export path must be absolute".into())));
        assert!(matches!(unreadable, ImportOutcome::Invalid { .. }));

        let failed = import_outcome(Err(ApplyError::Failed("reverted".into())));
        assert!(matches!(failed, ImportOutcome::Failed { .. }));

        let incomplete = import_outcome(Err(ApplyError::Incomplete("restart".into())));
        assert!(matches!(incomplete, ImportOutcome::Incomplete { .. }));
    }
}
