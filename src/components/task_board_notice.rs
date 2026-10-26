// SPDX-License-Identifier: MPL-2.0
//! The warning for a Task board that could not be loaded.
//!
//! A board that cannot be read is never shown as an empty Project. The notice
//! names the file, says the stored file was not overwritten and that Task
//! changes wait for a repair, and puts the technical reason in a disclosure.
//!
//! The report is read once per mount: by the Dashboard for a Project, or by the
//! application-level notice on its own. The backend answers from
//! the startup load, so a file repaired while the app runs does not clear the
//! warning; the next start does.

use crate::models::task_boards::{for_project, unassociated, BoardStatus, TaskBoardReport};
use crate::services::task_board_service::read_board_status;
use leptos::prelude::*;
use leptos::task::spawn_local;

/// The unreadable boards of one Project (`Some`), or the boards that no loaded
/// Project can show (`None`), which appear at application level.
///
/// A Project's notice takes the status its Dashboard read, so the warning and
/// the board's inert state come from one answer. The application-level notice
/// reads on its own.
#[component]
pub fn TaskBoardNotice(
    #[prop(optional)] project_id: Option<String>,
    #[prop(optional)] status: Option<RwSignal<BoardStatus>>,
) -> impl IntoView {
    let status = status.unwrap_or_else(|| {
        let own = RwSignal::new(BoardStatus::Pending);
        spawn_local(async move {
            // The notice may have unmounted before the read answers.
            own.try_set(read_board_status().await);
        });
        own
    });

    let application_level = project_id.is_none();
    move || {
        let mut shown: Vec<TaskBoardReport> = Vec::new();
        let mut failure = None::<String>;
        match status.get() {
            BoardStatus::Pending => {}
            BoardStatus::Read(found) => {
                shown = match &project_id {
                    Some(id) => for_project(&found, id).into_iter().cloned().collect(),
                    None => unassociated(&found).into_iter().cloned().collect(),
                };
            }
            BoardStatus::Failed(error) => failure = Some(error),
        }
        // A failed read cannot say which boards exist. At application level it
        // is reported as such; a Project says only that its status is unknown.
        let (app_failure, unverified) = match (failure, application_level) {
            (Some(error), true) => (Some(error), None),
            (Some(error), false) => (None, Some(error)),
            (None, _) => (None, None),
        };
        if shown.is_empty() && app_failure.is_none() && unverified.is_none() {
            return ().into_any();
        }
        view! {
            <div class="flex flex-col gap-2 px-4 pt-3">
                {app_failure.map(|error| view! {
                    <p role="alert" class="text-sm text-amber-200">
                        "Could not check whether Task boards loaded: " {error}
                    </p>
                })}
                {unverified.map(unverified_notice)}
                {shown
                    .into_iter()
                    .map(|report| board_notice(&report, !application_level))
                    .collect_view()}
            </div>
        }
        .into_any()
    }
}

/// A Project whose board status could not be read. Its board stays inert, and
/// the notice does not claim the board is healthy or unreadable.
fn unverified_notice(error: String) -> impl IntoView {
    view! {
        <section role="alert" data-testid="task-board-status-unverified" class="rounded-lg border border-amber-500/40 bg-amber-500/10 px-4 py-3 text-sm text-amber-100">
            <h3 class="font-semibold text-amber-200">"Task board status could not be verified"</h3>
            <p class="mt-1 text-white/70">
                "SlashIt could not confirm whether this Project's Task board can be read, so its Task controls are unavailable. Restart SlashIt to check again."
            </p>
            <details class="mt-2 text-xs text-white/50">
                <summary class="cursor-pointer">"Technical reason"</summary>
                <p class="mt-1 break-words font-mono">{error}</p>
            </details>
        </section>
    }
}

fn board_notice(report: &TaskBoardReport, project_scoped: bool) -> impl IntoView {
    let heading = if project_scoped {
        "Tasks could not be loaded for this Project"
    } else {
        "A Task board could not be loaded"
    };
    let path = report.path.clone();
    let reason = report.reason.clone();
    // A Project's board is inert, so its own controls cannot be used. Elsewhere
    // a Task change is only refused by the backend, and the notice says so.
    let guidance = if project_scoped {
        "The Task controls on this board are unavailable until the file is repaired. Task changes attempted from other screens are refused."
    } else {
        "Task changes for this board are refused until the file is repaired."
    };
    view! {
        <section role="alert" data-testid="task-board-notice" class="rounded-lg border border-amber-500/40 bg-amber-500/10 px-4 py-3 text-sm text-amber-100">
            <h3 class="font-semibold text-amber-200">{heading}</h3>
            <p class="mt-1 text-white/70">
                "The stored file has not been overwritten, so the original data is still there. "
                {guidance}
                " Restart SlashIt after correcting the file."
            </p>
            <p class="mt-2 text-white/60">
                "File: " <code class="break-all text-white/80">{path}</code>
            </p>
            <details class="mt-2 text-xs text-white/50">
                <summary class="cursor-pointer">"Technical reason"</summary>
                <p class="mt-1 break-words font-mono">{reason}</p>
            </details>
        </section>
    }
}
