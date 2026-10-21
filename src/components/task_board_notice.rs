//! The warning for a Task board that could not be loaded.
//!
//! A board that cannot be read is never shown as an empty Project. The notice
//! names the file, says the stored file was not overwritten and that Task
//! changes wait for a repair, and puts the technical reason in a disclosure.
//!
//! The report is read once, when the notice mounts. The backend answers from
//! the startup load, so a file repaired while the app runs does not clear the
//! warning; the next start does.

use crate::models::task_boards::{for_project, unassociated, TaskBoardReport};
use crate::services::task_board_service::list_task_board_diagnostics;
use leptos::prelude::*;
use leptos::task::spawn_local;

/// The unreadable boards of one Project (`Some`), or the boards that no loaded
/// Project can show (`None`), which appear at application level.
#[component]
pub fn TaskBoardNotice(#[prop(optional)] project_id: Option<String>) -> impl IntoView {
    let (reports, set_reports) = signal(Vec::<TaskBoardReport>::new());
    let (read_error, set_read_error) = signal(None::<String>);

    spawn_local(async move {
        match list_task_board_diagnostics().await {
            Ok(found) => set_reports.set(found),
            Err(error) => set_read_error.set(Some(error)),
        }
    });

    let application_level = project_id.is_none();
    move || {
        let found = reports.get();
        let shown: Vec<TaskBoardReport> = match &project_id {
            Some(id) => for_project(&found, id).into_iter().cloned().collect(),
            None => unassociated(&found).into_iter().cloned().collect(),
        };
        // A failed read cannot say which boards exist, so it is reported at
        // application level only, rather than as an empty Project.
        let failure = if application_level {
            read_error.get()
        } else {
            None
        };
        if shown.is_empty() && failure.is_none() {
            return ().into_any();
        }
        view! {
            <div class="flex flex-col gap-2 px-4 pt-3">
                {failure.map(|error| view! {
                    <p role="alert" class="text-sm text-amber-200">
                        "Could not check whether Task boards loaded: " {error}
                    </p>
                })}
                {shown
                    .into_iter()
                    .map(|report| board_notice(&report, !application_level))
                    .collect_view()}
            </div>
        }
        .into_any()
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
    view! {
        <section role="alert" class="rounded-lg border border-amber-500/40 bg-amber-500/10 px-4 py-3 text-sm text-amber-100">
            <h3 class="font-semibold text-amber-200">{heading}</h3>
            <p class="mt-1 text-white/70">
                "The stored file has not been overwritten, so the original data is still there. "
                "Task changes are unavailable until the file is repaired. "
                "Restart SlashIt after correcting the file."
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
