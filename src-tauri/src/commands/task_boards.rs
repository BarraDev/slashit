//! Task boards this run could not load: a read-only report for the warning
//! the frontend shows. Changes nothing, and never reads a board again; see
//! `Storage::task_board_diagnostics`.

use crate::config::storage::TaskBoardDiagnostic;
use serde::Serialize;
use uuid::Uuid;

/// One unreadable board, and whether its Project is loaded in this run.
///
/// A board whose Project is not loaded cannot be shown on that Project's board,
/// so the frontend reports it at application level instead. The frontend reads
/// this shape from `src/models/task_boards_payload.json`; keep them in step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskBoardReport {
    #[serde(flatten)]
    pub diagnostic: TaskBoardDiagnostic,
    pub project_loaded: bool,
}

fn reports(
    diagnostics: Vec<TaskBoardDiagnostic>,
    is_loaded: impl Fn(Uuid) -> bool,
) -> Vec<TaskBoardReport> {
    diagnostics
        .into_iter()
        .map(|diagnostic| {
            let project_loaded = diagnostic.project_id.is_some_and(&is_loaded);
            TaskBoardReport {
                diagnostic,
                project_loaded,
            }
        })
        .collect()
}

#[tauri::command]
pub async fn list_task_board_diagnostics(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<TaskBoardReport>, String> {
    let projects = state.project.projects.read().await;
    Ok(reports(state.storage.task_board_diagnostics(), |id| {
        projects.contains_key(&id)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload the frontend deserializes, shared by both sides.
    const FRONTEND_PAYLOAD: &str = include_str!("../../../src/models/task_boards_payload.json");

    #[test]
    fn the_payload_matches_what_the_frontend_reads() {
        let loaded: Uuid = "3f2b8c1e-6d4a-4b7e-9a1c-2d5e8f0a1b3c".parse().unwrap();
        let diagnostics = vec![
            TaskBoardDiagnostic {
                project_id: Some(loaded),
                path: "/data/slashit/projects/3f2b8c1e-6d4a-4b7e-9a1c-2d5e8f0a1b3c/tasks.toml"
                    .to_string(),
                reason: "parse failed at line 5, column 22 (key `status`): invalid type"
                    .to_string(),
            },
            TaskBoardDiagnostic {
                project_id: None,
                path: "/data/slashit/tasks/hand-made.toml".to_string(),
                reason: "layout version 2 is newer than this SlashIt understands (1)".to_string(),
            },
        ];

        let produced = serde_json::to_value(reports(diagnostics, |id| id == loaded)).unwrap();
        let expected: serde_json::Value = serde_json::from_str(FRONTEND_PAYLOAD).unwrap();
        assert_eq!(produced, expected);
    }

    #[test]
    fn a_board_whose_project_is_not_loaded_is_not_reported_as_loaded() {
        let stranger = Uuid::new_v4();
        let out = reports(
            vec![TaskBoardDiagnostic {
                project_id: Some(stranger),
                path: "/data/tasks.toml".to_string(),
                reason: "read failed: denied".to_string(),
            }],
            |_| false,
        );
        assert!(!out[0].project_loaded);
        assert_eq!(out[0].diagnostic.project_id, Some(stranger));
    }
}
