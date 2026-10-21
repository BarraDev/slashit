//! Mirror of the backend's unreadable Task board report
//! (`commands::task_boards`). The rules that decide where a report is shown
//! live here, so a Project board and the application-level notice cannot both
//! claim, or both miss, the same board.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TaskBoardReport {
    /// The Project the board belongs to, when the file names one.
    pub project_id: Option<String>,
    pub path: String,
    /// The technical reason. Contains no Task content.
    pub reason: String,
    pub project_loaded: bool,
}

/// What the diagnostic read established. Only a successful read can show that a
/// Project's board is readable; a failed read proves nothing either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardStatus {
    /// No answer yet.
    Pending,
    /// The read answered with the unreadable boards, which may be none.
    Read(Vec<TaskBoardReport>),
    /// The read could not be completed, with the reason it gave.
    Failed(String),
}

impl BoardStatus {
    /// Whether a Project's own Task controls may change its board. Only a
    /// successful read that lists no unreadable board for the Project allows it.
    pub fn project_board_writable(&self, project_id: &str) -> bool {
        matches!(self, Self::Read(reports) if for_project(reports, project_id).is_empty())
    }
}

/// The reports a Project's own board shows: its loaded Project's boards only.
pub fn for_project<'a>(
    reports: &'a [TaskBoardReport],
    project_id: &str,
) -> Vec<&'a TaskBoardReport> {
    reports
        .iter()
        .filter(|report| report.project_loaded && report.project_id.as_deref() == Some(project_id))
        .collect()
}

/// The reports no loaded Project can show, so they appear at application level.
pub fn unassociated(reports: &[TaskBoardReport]) -> Vec<&TaskBoardReport> {
    reports
        .iter()
        .filter(|report| !report.project_loaded)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same payload the backend test serializes its reports to.
    const PAYLOAD: &str = include_str!("task_boards_payload.json");
    const LOADED: &str = "3f2b8c1e-6d4a-4b7e-9a1c-2d5e8f0a1b3c";

    fn payload() -> Vec<TaskBoardReport> {
        serde_json::from_str(PAYLOAD).expect("payload matches the frontend model")
    }

    #[test]
    fn the_board_of_a_loaded_project_is_shown_on_that_project_only() {
        let reports = payload();
        let shown = for_project(&reports, LOADED);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].path, reports[0].path);
        assert!(for_project(&reports, "00000000-0000-0000-0000-000000000000").is_empty());
    }

    #[test]
    fn a_board_no_loaded_project_can_show_goes_to_the_application_notice() {
        let reports = payload();
        let app_level = unassociated(&reports);
        assert_eq!(app_level.len(), 1);
        assert_eq!(app_level[0].project_id, None);
        // The loaded project's board is never also shown at application level.
        assert!(app_level.iter().all(|report| !report.project_loaded));
    }

    #[test]
    fn a_project_with_no_unreadable_board_shows_nothing() {
        let healthy: Vec<TaskBoardReport> = Vec::new();
        assert!(for_project(&healthy, LOADED).is_empty());
        assert!(unassociated(&healthy).is_empty());
    }

    #[test]
    fn a_healthy_board_is_writable_after_a_successful_read() {
        let status = BoardStatus::Read(Vec::new());
        assert!(status.project_board_writable(LOADED));
    }

    #[test]
    fn an_unreadable_board_is_not_writable_after_a_successful_read() {
        let status = BoardStatus::Read(payload());
        assert!(!status.project_board_writable(LOADED));
    }

    #[test]
    fn a_healthy_project_stays_writable_beside_an_unreadable_one() {
        let status = BoardStatus::Read(payload());
        let other = "00000000-0000-0000-0000-000000000000";
        assert!(status.project_board_writable(other));
    }

    #[test]
    fn a_pending_board_is_not_writable() {
        assert!(!BoardStatus::Pending.project_board_writable(LOADED));
    }

    #[test]
    fn a_failed_read_is_not_taken_as_a_healthy_board() {
        let status = BoardStatus::Failed("command failed".into());
        assert!(!status.project_board_writable(LOADED));
        assert!(!status.project_board_writable("00000000-0000-0000-0000-000000000000"));
    }
}
