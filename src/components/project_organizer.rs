// SPDX-License-Identifier: MPL-2.0
//! Organize Projects: rename a Project's displayed name, remove it from
//! SlashIt, and manage Workspaces (create them, attach or detach Projects).
//! Each action is saved on its own and reports its own result. Renaming and
//! removing change nothing on disk; a Workspace's root is created only when
//! the Workspace is created.

use leptos::callback::Callback;
use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::components::toast;
use crate::components::workspace_management::WorkspaceManagement;
use crate::models::Project;
use crate::services::{list_projects, remove_project, rename_project};

#[component]
pub fn ProjectOrganizer(
    #[prop(into)] show: Signal<bool>,
    set_show: WriteSignal<bool>,
    selected_project: Signal<String>,
    set_selected_project: WriteSignal<String>,
    selected_workspace: Signal<String>,
    on_changed: Callback<()>,
) -> impl IntoView {
    let projects = RwSignal::new(Vec::<Project>::new());
    let load_error = RwSignal::new(None::<String>);
    let action_error = RwSignal::new(None::<String>);

    // Overlapping reloads (on open, after a removal) apply only the newest reply,
    // and a reply that arrives after the panel closed is dropped.
    let reload_seq = StoredValue::new(0u64);
    let reload = move || {
        reload_seq.update_value(|n| *n += 1);
        let seq = reload_seq.get_value();
        spawn_local(async move {
            let result = list_projects().await;
            if reload_seq.try_get_value() != Some(seq) {
                return;
            }
            match result {
                Ok(list) => {
                    let _ = projects.try_set(list);
                    let _ = load_error.try_set(None);
                }
                Err(e) => {
                    let _ = load_error.try_set(Some(format!("Could not read the Projects: {e}")));
                }
            }
        });
    };

    // Read the list each time the panel opens, so it never shows a stale name.
    Effect::new(move |_| {
        if show.get() {
            action_error.set(None);
            reload();
        }
    });

    let close = move || set_show.set(false);

    view! {
        <Show when=move || show.get()>
            <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
                <div
                    class="absolute inset-0 bg-black/60 backdrop-blur-sm"
                    on:click=move |_| close()
                ></div>

                <div
                    data-testid="project-organizer"
                    role="dialog"
                    aria-modal="true"
                    aria-labelledby="project-organizer-title"
                    class="relative w-full max-w-xl max-h-[90vh] flex flex-col bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl"
                    on:click=move |e| e.stop_propagation()
                    on:keydown=move |e| {
                        if e.key() == "Escape" {
                            close();
                        }
                    }
                >
                    <div class="flex items-center justify-between p-6 border-b border-white/5">
                        <div>
                            <h2 id="project-organizer-title" class="text-lg font-semibold text-white/90">"Organize Projects"</h2>
                            <p class="text-xs text-white/40 mt-1">
                                "Renaming changes only the name shown here. Removing a Project keeps its folder, Tasks and history. Workspaces group Projects."
                            </p>
                        </div>
                        <button
                            type="button"
                            aria-label="Close"
                            on:click=move |_| close()
                            class="p-2 rounded-lg hover:bg-white/5 text-white/40 hover:text-white/60 transition-colors"
                        >
                            <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                            </svg>
                        </button>
                    </div>

                    // The dialog is bounded by the window; this body scrolls. Menus
                    // open in the flow (see CustomSelect), so nothing here clips them.
                    <div class="p-6 space-y-3 flex-1 min-h-0 overflow-y-auto">
                        {move || action_error.get().map(|message| view! {
                            <div role="alert" data-testid="project-organizer-error" class="p-3 rounded-lg bg-red-500/10 border border-red-500/30 text-red-400 text-sm">
                                {message}
                            </div>
                        })}
                        {move || load_error.get().map(|message| view! {
                            <div role="alert" class="p-3 rounded-lg bg-red-500/10 border border-red-500/30 text-red-400 text-sm">
                                {message}
                            </div>
                        })}

                        <Show
                            when=move || !projects.with(Vec::is_empty)
                            fallback=move || view! {
                                <p data-testid="project-organizer-empty" class="text-sm text-white/50">
                                    "No Projects are open. Open a folder to add one."
                                </p>
                            }
                        >
                            <ul class="space-y-3 max-h-[40vh] overflow-y-auto" data-testid="project-organizer-list">
                                // Keyed by id, and each row reads its Project from `projects`:
                                // a reload that renames a Project reaches the same row
                                // without remounting it.
                                <For
                                    each=move || projects.with(|ps| ps.iter().map(|p| p.id).collect::<Vec<Uuid>>())
                                    key=|id| *id
                                    children=move |id| view! {
                                        <ProjectRow
                                            id=id
                                            projects=projects
                                            action_error=action_error
                                            selected_project=selected_project
                                            set_selected_project=set_selected_project
                                            selected_workspace=selected_workspace
                                            on_changed=on_changed
                                            reload=Callback::new(move |()| reload())
                                        />
                                    }
                                />
                            </ul>
                        </Show>

                        <WorkspaceManagement on_changed=on_changed />
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// The Project selection to set after `removed` was removed, or None when the
/// selection moved on. A replacement is applied only while no Workspace is
/// selected: setting a Project leaves Workspace context, so a late replacement
/// would overwrite a Workspace the user just chose. In Workspace context the
/// stale id is cleared to empty, which keeps the Workspace selected.
fn selection_after_removal(
    selected_project: &str,
    selected_workspace: &str,
    removed: &str,
    replacement: Option<String>,
) -> Option<String> {
    if selected_project != removed {
        return None;
    }
    if selected_workspace.is_empty() {
        Some(replacement.unwrap_or_default())
    } else {
        Some(String::new())
    }
}

/// What a removal attempt left behind, from its reply and a fresh Project list.
#[derive(Debug, PartialEq, Eq)]
enum RemovalOutcome {
    Removed,
    /// Removed, although the reply reported an error (the save may be unconfirmed).
    RemovedWithError,
    /// Still listed: the removal did not happen.
    NotRemoved,
    /// The reply failed and the list could not be read, so the removal may have happened.
    Unknown,
}

/// `still_listed` is probed only after an error reply; a successful reply is final.
fn removal_outcome(reply: &Result<(), String>, still_listed: Option<bool>) -> RemovalOutcome {
    match (reply, still_listed) {
        (Ok(()), _) => RemovalOutcome::Removed,
        (Err(_), Some(false)) => RemovalOutcome::RemovedWithError,
        (Err(_), Some(true)) => RemovalOutcome::NotRemoved,
        (Err(_), None) => RemovalOutcome::Unknown,
    }
}

/// One Project's row. Its rename and remove actions are independent: a failed
/// rename leaves the name as it was, and a failed remove leaves the Project in
/// the rail.
#[component]
fn ProjectRow(
    id: Uuid,
    projects: RwSignal<Vec<Project>>,
    action_error: RwSignal<Option<String>>,
    selected_project: Signal<String>,
    set_selected_project: WriteSignal<String>,
    selected_workspace: Signal<String>,
    on_changed: Callback<()>,
    reload: Callback<()>,
) -> impl IntoView {
    // The name the list reports for this Project now.
    let listed = Memo::new(move |_| {
        projects.with(|ps| {
            ps.iter()
                .find(|p| p.id == id)
                .map(|p| p.name.clone())
                .unwrap_or_default()
        })
    });
    // The name as the backend last confirmed it, and what the user is typing.
    let initial = listed.get_untracked();
    let saved = RwSignal::new(initial.clone());
    let draft = RwSignal::new(initial);
    // A reload can bring a new name for this Project. The row adopts it, and
    // replaces the typed text only when the user has not started editing.
    Effect::new(move |prev: Option<String>| {
        let next = listed.get();
        if prev.as_ref().is_some_and(|p| *p != next) && !next.is_empty() {
            if draft.get_untracked() == saved.get_untracked() {
                draft.set(next.clone());
            }
            saved.set(next.clone());
        }
        next
    });
    let saving = RwSignal::new(false);
    let confirming = RwSignal::new(false);
    let removing = RwSignal::new(false);

    let can_save = move || {
        let typed = draft.get();
        let typed = typed.trim();
        !saving.get() && !typed.is_empty() && typed != saved.get()
    };

    let on_rename = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let name = draft.get_untracked().trim().to_string();
        if name.is_empty() {
            action_error.set(Some("A Project needs a name.".to_string()));
            return;
        }
        saving.set(true);
        spawn_local(async move {
            match rename_project(id.to_string(), name.clone()).await {
                Ok(()) => {
                    // The row can be unmounted while the request is in flight
                    // (the panel closed), so every write from here uses try_set.
                    let _ = saved.try_set(name.clone());
                    let _ = draft.try_set(name.clone());
                    action_error.set(None);
                    toast::success(format!("Renamed to '{name}'. The folder was not changed."));
                    on_changed.run(());
                }
                Err(e) => {
                    action_error.set(Some(format!("Could not rename the Project: {e}")));
                }
            }
            let _ = saving.try_set(false);
        });
    };

    let on_remove = move |_| {
        removing.set(true);
        // Read before the await: the row's signals are disposed if the panel closes mid-request.
        let name = saved.get_untracked();
        let id_text = id.to_string();
        spawn_local(async move {
            let reply = remove_project(id_text.clone()).await;
            // The backend can publish a removal and still report an error, so after an
            // error the Project list decides whether the Project is gone.
            let still_listed = match &reply {
                Ok(()) => None,
                Err(_) => list_projects()
                    .await
                    .ok()
                    .map(|ps| ps.iter().any(|p| p.id.to_string() == id_text)),
            };
            let outcome = removal_outcome(&reply, still_listed);
            if matches!(
                outcome,
                RemovalOutcome::Removed | RemovalOutcome::RemovedWithError
            ) {
                // In Workspace context the Project is only a stale id, so the selection
                // is cleared without switching context. In Project context the next
                // active Project is picked.
                let next = if selected_workspace
                    .try_get_untracked()
                    .is_some_and(|w| w.is_empty())
                {
                    list_projects()
                        .await
                        .unwrap_or_default()
                        .first()
                        .map(|p| p.id.to_string())
                } else {
                    None
                };
                // Read again after the await: a Workspace chosen meanwhile is never replaced.
                let project_now = selected_project.try_get_untracked().unwrap_or_default();
                let workspace_now = selected_workspace.try_get_untracked().unwrap_or_default();
                if let Some(selection) =
                    selection_after_removal(&project_now, &workspace_now, &id_text, next)
                {
                    let _ = set_selected_project.try_set(selection);
                }
            }
            match outcome {
                RemovalOutcome::Removed => {
                    let _ = action_error.try_set(None);
                    toast::success(format!(
                        "Removed '{name}' from SlashIt. Open its folder to use it again."
                    ));
                }
                RemovalOutcome::RemovedWithError => {
                    let e = reply.err().unwrap_or_default();
                    let _ = action_error.try_set(Some(format!(
                        "'{name}' was removed, but SlashIt could not confirm the save. {e}"
                    )));
                }
                RemovalOutcome::NotRemoved => {
                    let e = reply.err().unwrap_or_default();
                    let _ = action_error.try_set(Some(format!("'{name}' was not removed. {e}")));
                }
                RemovalOutcome::Unknown => {
                    let e = reply.err().unwrap_or_default();
                    let _ = action_error.try_set(Some(format!(
                        "'{name}' may not have been removed, and the Project list could not be read. {e}"
                    )));
                }
            }
            let _ = confirming.try_set(false);
            // Both lists are reloaded whatever the outcome: they show what the backend holds now.
            // try_run: the panel may have closed while the request was in flight.
            let _ = reload.try_run(());
            let _ = on_changed.try_run(());
            let _ = removing.try_set(false);
        });
    };

    view! {
        <li data-testid="project-organizer-row" class="rounded-lg border border-white/10 bg-white/[0.02] p-3">
            <form class="flex items-center gap-2" on:submit=on_rename>
                <label class="sr-only" for=format!("rename-{id}")>"Project name"</label>
                <input
                    id=format!("rename-{id}")
                    data-testid="project-organizer-name"
                    type="text"
                    maxlength="120"
                    prop:value=move || draft.get()
                    on:input=move |e| draft.set(event_target_value(&e))
                    disabled=move || saving.get() || removing.get()
                    class="flex-1 min-w-0 px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-sm text-white/90 focus:outline-none focus:border-blue-400"
                />
                <button
                    type="submit"
                    data-testid="project-organizer-save"
                    disabled=move || !can_save()
                    class="px-3 py-2 rounded-lg text-sm bg-blue-500 hover:bg-blue-600 text-white disabled:bg-white/5 disabled:text-white/30 transition-colors"
                >
                    {move || if saving.get() { "Saving..." } else { "Save name" }}
                </button>
                <button
                    type="button"
                    data-testid="project-organizer-remove"
                    disabled=move || saving.get() || removing.get() || confirming.get()
                    on:click=move |_| confirming.set(true)
                    class="px-3 py-2 rounded-lg text-sm text-red-300 hover:bg-red-500/10 disabled:text-white/30 transition-colors"
                >
                    "Remove"
                </button>
            </form>

            {move || confirming.get().then(|| view! {
                <div data-testid="project-organizer-confirm" class="mt-3 flex flex-wrap items-center justify-between gap-2 text-sm text-white/70">
                    <span>
                        {move || format!("Remove '{}' from SlashIt? Its folder, Tasks and history stay as they are.", saved.get())}
                    </span>
                    <span class="flex gap-2">
                        <button
                            type="button"
                            disabled=move || removing.get()
                            on:click=move |_| confirming.set(false)
                            class="px-3 py-1.5 rounded-lg text-white/70 hover:bg-white/5 transition-colors"
                        >
                            "Cancel"
                        </button>
                        <button
                            type="button"
                            data-testid="project-organizer-confirm-remove"
                            disabled=move || removing.get()
                            on:click=on_remove
                            class="px-3 py-1.5 rounded-lg bg-red-500/80 hover:bg-red-500 text-white disabled:bg-white/5 transition-colors"
                        >
                            {move || if removing.get() { "Removing..." } else { "Remove from SlashIt" }}
                        </button>
                    </span>
                </div>
            })}
        </li>
    }
}

#[cfg(test)]
mod tests {
    use super::{removal_outcome, selection_after_removal, RemovalOutcome};

    #[test]
    fn the_removed_selection_is_replaced_when_no_workspace_was_chosen() {
        assert_eq!(
            selection_after_removal("removed", "", "removed", Some("next".into())),
            Some("next".to_string())
        );
    }

    #[test]
    fn a_removed_selection_is_cleared_in_workspace_context_without_switching_it() {
        // Clearing only the stale id: no replacement Project is set, so the Workspace stays chosen.
        assert_eq!(
            selection_after_removal("removed", "ws-1", "removed", None),
            Some(String::new())
        );
        assert_eq!(
            selection_after_removal("removed", "ws-1", "removed", Some("next".into())),
            Some(String::new())
        );
    }

    #[test]
    fn a_selection_that_moved_on_is_left_alone() {
        assert_eq!(
            selection_after_removal("other", "", "removed", Some("next".into())),
            None
        );
        assert_eq!(selection_after_removal("", "", "removed", None), None);
    }

    #[test]
    fn a_removed_selection_with_no_project_left_becomes_empty() {
        assert_eq!(
            selection_after_removal("removed", "", "removed", None),
            Some(String::new())
        );
    }

    #[test]
    fn a_successful_removal_is_final_whatever_the_list_says() {
        assert_eq!(removal_outcome(&Ok(()), None), RemovalOutcome::Removed);
    }

    #[test]
    fn an_error_reply_is_not_reported_as_not_removed_when_the_list_shows_it_gone() {
        let reply = Err("could not confirm the save".to_string());
        assert_eq!(
            removal_outcome(&reply, Some(false)),
            RemovalOutcome::RemovedWithError
        );
    }

    #[test]
    fn an_error_reply_is_not_removed_only_when_the_list_still_shows_the_project() {
        let reply = Err("refused".to_string());
        assert_eq!(
            removal_outcome(&reply, Some(true)),
            RemovalOutcome::NotRemoved
        );
    }

    #[test]
    fn an_error_reply_with_an_unreadable_list_is_unknown_not_not_removed() {
        let reply = Err("could not confirm the save".to_string());
        assert_eq!(removal_outcome(&reply, None), RemovalOutcome::Unknown);
    }
}
