// SPDX-License-Identifier: MPL-2.0
//! Export and import of a whole Project as a portable file.
//!
//! Export asks for a destination, then writes a new file. Import chooses a file,
//! previews it read-only, and applies it only after a separate confirmation.
//! Selecting a file never imports it.

use leptos::callback::Callback;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::models::Project;
use crate::services::portability_service::{
    apply_import, can_confirm, choose_export_destination, choose_import_file, export_project,
    import_detail, import_headline, preview_import, ExportOutcome, ImportOutcome, PreviewReport,
    Tone,
};
use crate::services::project_service::get_project;

/// The export flow. `None` means no export is in progress or on screen.
#[derive(Clone, PartialEq)]
pub enum ExportPhase {
    /// The save dialog is open. Nothing is on screen yet.
    Choosing,
    Writing,
    Done(ExportOutcome),
    Failed(String),
}

/// Ask for a destination, then write the export there. Cancelling the dialog
/// leaves nothing behind.
pub fn start_export(project_id: String, project_name: String, phase: RwSignal<Option<ExportPhase>>) {
    if matches!(phase.get_untracked(), Some(ExportPhase::Choosing | ExportPhase::Writing)) {
        return;
    }
    phase.set(Some(ExportPhase::Choosing));
    spawn_local(async move {
        match choose_export_destination(project_name).await {
            Ok(None) => {
                let _ = phase.try_set(None);
            }
            Ok(Some(path)) => {
                let _ = phase.try_set(Some(ExportPhase::Writing));
                let next = match export_project(project_id, path).await {
                    Ok(outcome) => ExportPhase::Done(outcome),
                    Err(message) => ExportPhase::Failed(message),
                };
                // Closing the dialog while the file is written does not stop the
                // write. It only keeps the report from reappearing unasked.
                if matches!(phase.try_get_untracked().flatten(), Some(ExportPhase::Writing)) {
                    let _ = phase.try_set(Some(next));
                }
            }
            Err(message) => {
                let _ = phase.try_set(Some(ExportPhase::Failed(message)));
            }
        }
    });
}

#[component]
pub fn ExportProjectModal(phase: RwSignal<Option<ExportPhase>>) -> impl IntoView {
    let visible = move || {
        matches!(
            phase.get(),
            Some(ExportPhase::Writing | ExportPhase::Done(_) | ExportPhase::Failed(_))
        )
    };

    view! {
        <Show when=visible>
            <div data-testid="export-project-modal" class="fixed inset-0 z-50 flex items-center justify-center p-4">
                <div class="absolute inset-0 bg-black/70 backdrop-blur-sm" aria-hidden="true"></div>
                <div class="relative w-full max-w-md bg-[#0B0B0F] border border-white/10 rounded-2xl shadow-2xl p-6 text-white/80 text-sm space-y-3">
                    {move || match phase.get() {
                        Some(ExportPhase::Writing) => view! {
                            <p data-testid="export-status">"Writing the export..."</p>
                        }.into_any(),
                        Some(ExportPhase::Done(outcome)) => view! {
                            <div data-testid="export-status" class="space-y-2">
                                <p class={if outcome.durable { "text-base font-semibold text-emerald-300" } else { "text-base font-semibold text-amber-300" }}>
                                    {if outcome.durable { "Export written" } else { "Export written, but not confirmed" }}
                                </p>
                                <p>{format!("{} task(s) written to a new file:", outcome.tasks)}</p>
                                <p class="break-all font-mono text-xs text-white/60">{outcome.path.clone()}</p>
                                {(!outcome.durable).then(|| view! {
                                    <p data-testid="export-not-durable" class="text-amber-300">
                                        "This filesystem could not confirm the file. It may not survive a power loss. Keep another copy until you verify it."
                                    </p>
                                })}
                                <p class="text-white/50">
                                    "The export contains your Task text and history. Treat it as sensitive. It does not include credentials, repository paths or agent commands."
                                </p>
                            </div>
                        }.into_any(),
                        Some(ExportPhase::Failed(message)) => view! {
                            <div data-testid="export-status" class="space-y-2">
                                <p class="text-base font-semibold text-red-300">"Export not written"</p>
                                <p>{message}</p>
                            </div>
                        }.into_any(),
                        _ => ().into_any(),
                    }}
                    <div class="flex justify-end pt-2">
                        <button
                            data-testid="export-close"
                            type="button"
                            on:click=move |_| phase.set(None)
                            class="px-4 py-2 rounded-lg bg-white/10 hover:bg-white/15 text-white"
                        >
                            "Close"
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// The import flow's visible state.
#[derive(Clone, PartialEq)]
enum ImportPhase {
    /// No file has been chosen in this session of the dialog.
    Idle,
    Previewing,
    Previewed { path: String, report: PreviewReport },
    /// The file could not be previewed. `path` is absent when the dialog itself failed.
    PreviewFailed { path: Option<String>, message: String },
    Applying,
    Applied(ImportOutcome),
}

fn tone_class(tone: Tone) -> &'static str {
    match tone {
        Tone::Success => "text-emerald-300",
        Tone::Warning => "text-amber-300",
        Tone::Error => "text-red-300",
    }
}

#[component]
pub fn ImportProjectModal(
    #[prop(into)] show: Signal<bool>,
    set_show: WriteSignal<bool>,
    on_project_created: Callback<Project>,
) -> impl IntoView {
    let phase = RwSignal::new(ImportPhase::Idle);
    // Bumped whenever the dialog is closed or reset. A request that finishes
    // after that may still complete, but it must not put its result on screen.
    let session = StoredValue::new(0u64);
    // One file dialog at a time.
    let choosing = StoredValue::new(false);

    let close = move |_| {
        session.update_value(|s| *s += 1);
        phase.set(ImportPhase::Idle);
        set_show.set(false);
    };

    // Choosing a file only previews it. Nothing is imported here.
    let choose = move |_| {
        if choosing.get_value() {
            return;
        }
        choosing.set_value(true);
        let mine = session.get_value();
        spawn_local(async move {
            let chosen = choose_import_file().await;
            choosing.set_value(false);
            if session.get_value() != mine {
                return;
            }
            match chosen {
                Ok(None) => {}
                Ok(Some(path)) => {
                    let _ = phase.try_set(ImportPhase::Previewing);
                    let next = match preview_import(path.clone()).await {
                        Ok(report) => ImportPhase::Previewed { path: path.clone(), report },
                        Err(message) => ImportPhase::PreviewFailed { path: Some(path), message },
                    };
                    if session.get_value() == mine {
                        let _ = phase.try_set(next);
                    }
                }
                Err(message) => {
                    let _ = phase.try_set(ImportPhase::PreviewFailed { path: None, message });
                }
            }
        });
    };

    // Apply runs only from this explicit confirmation, with the checksum that the
    // preview reported for this file.
    let confirm = move |_| {
        let ImportPhase::Previewed { path, report } = phase.get_untracked() else {
            return;
        };
        if !can_confirm(&report) {
            return;
        }
        phase.set(ImportPhase::Applying);
        let mine = session.get_value();
        spawn_local(async move {
            let outcome = match apply_import(path, report.checksum.clone()).await {
                Ok(outcome) => outcome,
                Err(message) => ImportOutcome::Failed { message },
            };
            // The import may have happened even if the dialog was closed, so the
            // list is refreshed in every case: the caller adds a tab, selects the
            // Project and reloads the list.
            if let ImportOutcome::Applied { project_id, .. } = &outcome {
                if let Ok(Some(project)) = get_project(project_id.clone()).await {
                    on_project_created.run(project);
                }
            }
            if session.get_value() == mine {
                let _ = phase.try_set(ImportPhase::Applied(outcome));
            }
        });
    };

    let reset_choice = move |_| {
        session.update_value(|s| *s += 1);
        phase.set(ImportPhase::Idle);
    };

    view! {
        <Show when=move || show.get()>
            <div data-testid="import-project-modal" class="fixed inset-0 z-50 flex items-center justify-center p-4">
                <div class="absolute inset-0 bg-black/70 backdrop-blur-sm" aria-hidden="true"></div>
                <div class="relative w-full max-w-lg max-h-[85vh] overflow-y-auto bg-[#0B0B0F] border border-white/10 rounded-2xl shadow-2xl p-6 text-white/80 text-sm space-y-4">
                    <div>
                        <h2 class="text-lg font-semibold text-white">"Import Project"</h2>
                        <p class="text-white/50">
                            "Creates a new Project from an export file. It is not linked to a local repository, so its Tasks cannot run until you link one."
                        </p>
                    </div>

                    {move || match phase.get() {
                        ImportPhase::Idle => view! {
                            <div class="flex justify-start">
                                <button
                                    data-testid="import-choose-file"
                                    type="button"
                                    on:click=choose
                                    class="px-4 py-2 rounded-lg bg-white/10 hover:bg-white/15 text-white"
                                >
                                    "Choose file"
                                </button>
                            </div>
                        }.into_any(),
                        ImportPhase::Previewing => view! {
                            <p data-testid="import-status">"Reading the file..."</p>
                        }.into_any(),
                        ImportPhase::PreviewFailed { path, message } => view! {
                            <div data-testid="import-status" class="space-y-3">
                                <p class="text-red-300 font-semibold">"This file cannot be imported"</p>
                                {path.map(|p| view! {
                                    <p class="break-all font-mono text-xs text-white/50">{p}</p>
                                })}
                                <p>{message}</p>
                                <p class="text-white/50">"Nothing was changed."</p>
                                <button type="button" on:click=choose class="px-4 py-2 rounded-lg bg-white/10 hover:bg-white/15 text-white">
                                    "Choose another file"
                                </button>
                            </div>
                        }.into_any(),
                        ImportPhase::Previewed { path, report } => {
                            let confirmable = can_confirm(&report);
                            let conflicts = report.conflicts.clone();
                            let excluded = report.excluded.clone();
                            let normalization_count = report.normalization.len();
                            let repo_text = report
                                .repository
                                .as_ref()
                                .and_then(|r| r.remote_url.clone())
                                .map(|url| format!("Repository remote recorded in the file: {url}. It is not restored."))
                                .unwrap_or_else(|| "The file records no repository.".to_string());
                            view! {
                                <div data-testid="import-preview" class="space-y-3">
                                    <p class="break-all font-mono text-xs text-white/50">{path}</p>
                                    <dl class="grid grid-cols-2 gap-x-4 gap-y-1">
                                        <dt class="text-white/50">"Project"</dt>
                                        <dd data-testid="import-project-name" class="text-white">{report.project.name.clone()}</dd>
                                        <dt class="text-white/50">"Tasks"</dt>
                                        <dd data-testid="import-task-count" class="text-white">{report.counts.tasks}</dd>
                                        <dt class="text-white/50">"Dependencies"</dt>
                                        <dd class="text-white">{report.counts.dependencies}</dd>
                                        <dt class="text-white/50">"Preserved history"</dt>
                                        <dd class="text-white">
                                            {format!("{} activity entries, {} review entries", report.counts.activity_entries, report.counts.review_entries)}
                                        </dd>
                                    </dl>

                                    {(!excluded.is_empty()).then(|| view! {
                                        <div data-testid="import-omissions">
                                            <p class="text-white/50">"Left out of the file:"</p>
                                            <p class="text-white/70">{excluded.join(", ")}</p>
                                        </div>
                                    })}

                                    {(normalization_count > 0).then(|| view! {
                                        <p data-testid="import-normalization" class="text-amber-300">
                                            {format!("{normalization_count} Task(s) will be changed so nothing imported can act: approvals are withdrawn, and Tasks waiting for review return to Backlog.")}
                                        </p>
                                    })}

                                    <div class="rounded-lg border border-white/10 p-3 space-y-1 text-white/60">
                                        <p>"Not restored: repository links, local checkouts, credentials, agent commands, execution approvals, branches and pull requests."</p>
                                        <p>{repo_text}</p>
                                    </div>

                                    {(!conflicts.is_empty()).then(|| view! {
                                        <div data-testid="import-conflicts" class="space-y-1">
                                            <p class="text-red-300 font-semibold">"This file cannot be imported here, because these identities already exist:"</p>
                                            {conflicts.into_iter().map(|c| view! {
                                                <p class="font-mono text-xs text-white/70">{format!("{}: {} ({})", c.kind, c.id, c.detail)}</p>
                                            }).collect::<Vec<_>>()}
                                        </div>
                                    })}

                                    <div class="flex justify-end gap-2 pt-2">
                                        <button type="button" on:click=reset_choice class="px-4 py-2 rounded-lg bg-white/10 hover:bg-white/15 text-white">
                                            "Choose another file"
                                        </button>
                                        {confirmable.then(|| view! {
                                            <button
                                                data-testid="import-confirm"
                                                type="button"
                                                on:click=confirm
                                                class="px-4 py-2 rounded-lg bg-blue-600 hover:bg-blue-500 text-white font-semibold"
                                            >
                                                "Import Project"
                                            </button>
                                        })}
                                    </div>
                                </div>
                            }.into_any()
                        }
                        ImportPhase::Applying => view! {
                            <p data-testid="import-status">"Importing..."</p>
                        }.into_any(),
                        ImportPhase::Applied(outcome) => {
                            let (tone, headline) = import_headline(&outcome);
                            let detail = import_detail(&outcome);
                            let linked_hint = matches!(outcome, ImportOutcome::Applied { .. });
                            view! {
                                <div data-testid="import-result" class="space-y-3">
                                    <p data-testid="import-headline" class=format!("font-semibold {}", tone_class(tone))>{headline}</p>
                                    {detail.map(|d| view! { <p class="text-white/60 break-words">{d}</p> })}
                                    {linked_hint.then(|| view! {
                                        <p data-testid="import-link-hint" class="text-white/60">
                                            "Link a local repository to this Project before its Tasks can run. The import did not create one."
                                        </p>
                                    })}
                                </div>
                            }.into_any()
                        }
                    }}

                    <div class="flex justify-end pt-2">
                        <button data-testid="import-close" type="button" on:click=close class="px-4 py-2 rounded-lg bg-white/10 hover:bg-white/15 text-white">
                            "Close"
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}
