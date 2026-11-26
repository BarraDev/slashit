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
use crate::services::{list_projects, remove_project, rename_project, set_project_pr_review_defaults};

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
    // Whether a read has succeeded, so an empty list is shown only once it is known.
    let loaded = RwSignal::new(false);

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
                    let _ = loaded.try_set(true);
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
    // A read still pending when the panel closes must not update the rows that stay
    // cached for the next opening, so closing invalidates it.
    Effect::new(move |_| {
        if !show.get() {
            reload_seq.update_value(|n| *n += 1);
        }
    });

    let close = move || set_show.set(false);

    // The element that had focus when the panel opened, which gets focus back on close.
    // Declared before the focus effect below, so it reads focus before the dialog takes it.
    let opener = StoredValue::new(None::<web_sys::HtmlElement>);
    Effect::new(move |_| {
        if show.get() {
            let active = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.active_element());
            opener.set_value(active.and_then(|e| {
                use wasm_bindgen::JsCast;
                e.dyn_into::<web_sys::HtmlElement>().ok()
            }));
        }
    });

    // Focus moves into the dialog when it opens, so its Escape handler receives keys
    // from the keyboard.
    let dialog_ref = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        if show.get() {
            if let Some(dialog) = dialog_ref.get() {
                let _ = dialog.focus();
            }
        }
    });

    // Closing returns focus to the element that opened the panel, if it is still there.
    Effect::new(move |_| {
        if !show.get() {
            if let Some(element) = opener.get_value() {
                let _ = element.focus();
            }
        }
    });

    view! {
        <Show when=move || show.get()>
            <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
                <div
                    class="absolute inset-0 bg-black/60 backdrop-blur-sm"
                    on:click=move |_| close()
                ></div>

                <div
                    node_ref=dialog_ref
                    tabindex="-1"
                    data-testid="project-organizer"
                    role="dialog"
                    aria-modal="true"
                    aria-labelledby="project-organizer-title"
                    class="relative w-full max-w-xl max-h-[90vh] flex flex-col bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl"
                    on:click=move |e| e.stop_propagation()
                    on:keydown=move |e| {
                        if e.key() == "Escape" {
                            close();
                        } else if e.key() == "Tab" {
                            // aria-modal keeps Tab inside the dialog: wrap at its edges.
                            if let Some(dialog) = dialog_ref.get() {
                                if let Some(target) = tab_wrap_target(&dialog, e.shift_key()) {
                                    e.prevent_default();
                                    let _ = target.focus();
                                }
                            }
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
                            fallback=move || if loaded.get() {
                                view! {
                                    <p data-testid="project-organizer-empty" class="text-sm text-white/50">
                                        "No Projects are open. Open a folder to add one."
                                    </p>
                                }.into_any()
                            } else if load_error.with(Option::is_some) {
                                // The read failed: the error above says so, and no request is pending.
                                ().into_any()
                            } else {
                                view! {
                                    <p class="text-sm text-white/50">"Loading Projects…"</p>
                                }.into_any()
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

/// Moves focus to the Organizer dialog, for when the control that had it is removed.
fn focus_organizer_dialog() {
    use wasm_bindgen::JsCast;
    let dialog = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.query_selector("[data-testid=\"project-organizer\"]").ok().flatten())
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok());
    if let Some(dialog) = dialog {
        let _ = dialog.focus();
    }
}

/// The controls Tab moves between inside the Organizer, in tab order.
const FOCUSABLE: &str = "button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex]:not([tabindex=\"-1\"])";

/// The index of the control Tab should wrap to, or None to let the browser move focus.
/// `current` is the focused control's index; None means focus is on the dialog itself or outside it.
fn tab_wrap_index(current: Option<usize>, count: usize, shift: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    if shift {
        (current.is_none() || current == Some(0)).then_some(count - 1)
    } else {
        (current.is_none() || current == Some(count - 1)).then_some(0)
    }
}

/// The control to focus instead of the browser's next Tab stop, when that stop would leave the dialog.
fn tab_wrap_target(dialog: &web_sys::HtmlElement, shift: bool) -> Option<web_sys::HtmlElement> {
    use wasm_bindgen::JsCast;
    let document = web_sys::window()?.document()?;
    let active = document.active_element()?;
    let active: &web_sys::Node = active.as_ref();
    let items = dialog.query_selector_all(FOCUSABLE).ok()?;
    let nodes: Vec<web_sys::Node> = (0..items.length()).filter_map(|i| items.item(i)).collect();
    let current = nodes.iter().position(|node| node.is_same_node(Some(active)));
    let index = tab_wrap_index(current, nodes.len(), shift)?;
    nodes.get(index)?.clone().dyn_into::<web_sys::HtmlElement>().ok()
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
    // The PR review defaults as the list reports them now; the toggle saves at once.
    let defaults = Memo::new(move |_| {
        projects.with(|ps| {
            ps.iter()
                .find(|p| p.id == id)
                .map(|p| p.pr_review_defaults)
                .unwrap_or_default()
        })
    });
    let saving_defaults = RwSignal::new(false);
    let save_defaults = move |next: crate::models::PrReviewDefaults| {
        if saving_defaults.get_untracked() {
            return;
        }
        saving_defaults.set(true);
        spawn_local(async move {
            match set_project_pr_review_defaults(id.to_string(), next).await {
                Ok(updated) => {
                    let _ = projects.try_update(|list| {
                        for project in list.iter_mut().filter(|p| p.id == id) {
                            project.pr_review_defaults = updated.pr_review_defaults;
                        }
                    });
                    let _ = action_error.try_set(None);
                }
                Err(e) => {
                    let _ = action_error.try_set(Some(format!(
                        "Could not save the PR review defaults: {e}"
                    )));
                }
            }
            let _ = saving_defaults.try_set(false);
        });
    };
    let saving = RwSignal::new(false);
    let confirming = RwSignal::new(false);
    let removing = RwSignal::new(false);

    // A rename and a removal of one row are not allowed at the same time.
    let can_save = move || {
        let typed = draft.get();
        let typed = typed.trim();
        !saving.get() && !removing.get() && !typed.is_empty() && typed != saved.get()
    };

    let on_rename = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        if saving.get_untracked() || removing.get_untracked() {
            return;
        }
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
                    // The shared list shows the new name, even if this row is reopened
                    // before the next reload finishes.
                    let _ = projects.try_update(|list| {
                        for project in list.iter_mut().filter(|p| p.id == id) {
                            project.name = name.clone();
                        }
                    });
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
                RemovalOutcome::Removed | RemovalOutcome::RemovedWithError | RemovalOutcome::Unknown
            ) {
                // In Workspace context the Project is only a stale id, so the selection
                // is cleared without switching context. In Project context the next
                // active Project is picked. An Unknown outcome may have removed the
                // Project, but the list is unknown too, so the selection is cleared
                // without a replacement rather than moved to an unverified Project.
                let next = if matches!(outcome, RemovalOutcome::Unknown) {
                    None
                } else if selected_workspace
                    .try_get_untracked()
                    .is_some_and(|w| w.is_empty())
                {
                    // A failed read is not an empty list: the next Project is unknown, so
                    // the selection is cleared and the person is told why.
                    match list_projects().await {
                        Ok(list) => list.first().map(|p| p.id.to_string()),
                        Err(e) => {
                            toast::error(format!(
                                "Could not confirm the next open Project ({e}). Choose one from the list."
                            ));
                            None
                        }
                    }
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
            // A removed row stays disabled until the reload drops it; only a refused
            // or unknown removal makes it actionable again.
            let row_gone = matches!(outcome, RemovalOutcome::Removed | RemovalOutcome::RemovedWithError);
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
            if row_gone {
                // The removed row's buttons are about to go; keep keyboard focus in the dialog.
                focus_organizer_dialog();
            }
            // Both lists are reloaded whatever the outcome: they show what the backend holds now.
            // try_run: the panel may have closed while the request was in flight.
            let _ = reload.try_run(());
            let _ = on_changed.try_run(());
            if !row_gone {
                let _ = removing.try_set(false);
            }
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

            <div class="mt-3 flex flex-wrap items-center gap-4 text-xs text-white/70">
                <span class="text-white/50">"PR review default (off unless set):"</span>
                <label class="flex items-center gap-1.5 cursor-pointer">
                    <input
                        type="checkbox"
                        data-testid="project-organizer-auto-push"
                        class="accent-yellow-500"
                        prop:checked=move || defaults.get().auto_push
                        disabled=move || saving_defaults.get() || removing.get()
                        on:change=move |ev| {
                            save_defaults(crate::models::PrReviewDefaults {
                                auto_push: event_target_checked(&ev),
                            });
                        }
                    />
                    "Auto-push branch"
                </label>
            </div>

            {move || confirming.get().then(|| view! {
                <div data-testid="project-organizer-confirm" class="mt-3 flex flex-wrap items-center justify-between gap-2 text-sm text-white/70">
                    <span>
                        {move || format!("Remove '{}' from SlashIt? Its folder, Tasks and history stay as they are.", saved.get())}
                    </span>
                    <span class="flex gap-2">
                        <button
                            type="button"
                            disabled=move || removing.get()
                            on:click=move |_| {
                                // The confirmation's buttons disappear here, which would drop focus out of the dialog.
                                confirming.set(false);
                                focus_organizer_dialog();
                            }
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
    use super::{removal_outcome, selection_after_removal, tab_wrap_index, RemovalOutcome};

    #[test]
    fn tab_leaves_the_organizer_only_by_wrapping_to_its_first_or_last_control() {
        // Forward from the last control, or from outside, wraps to the first.
        assert_eq!(tab_wrap_index(Some(2), 3, false), Some(0));
        assert_eq!(tab_wrap_index(None, 3, false), Some(0));
        // Forward within the dialog is left to the browser.
        assert_eq!(tab_wrap_index(Some(0), 3, false), None);
        assert_eq!(tab_wrap_index(Some(1), 3, false), None);
        // Backward from the first control, from the dialog itself, or from outside wraps to the last.
        assert_eq!(tab_wrap_index(Some(0), 3, true), Some(2));
        assert_eq!(tab_wrap_index(None, 3, true), Some(2));
        assert_eq!(tab_wrap_index(Some(1), 3, true), None);
        // A dialog with no controls has nothing to move to.
        assert_eq!(tab_wrap_index(None, 0, false), None);
        assert_eq!(tab_wrap_index(None, 0, true), None);
    }

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
