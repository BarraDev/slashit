// SPDX-License-Identifier: MPL-2.0
//! Organize Projects: rename a Project's displayed name, or remove it from
//! SlashIt. Each action is saved on its own and reports its own result.
//! Nothing on disk is changed by either action.

use leptos::callback::Callback;
use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::components::toast;
use crate::models::Project;
use crate::services::{list_projects, remove_project, rename_project};

#[component]
pub fn ProjectOrganizer(
    #[prop(into)] show: Signal<bool>,
    set_show: WriteSignal<bool>,
    selected_project: Signal<String>,
    set_selected_project: WriteSignal<String>,
    on_changed: Callback<()>,
) -> impl IntoView {
    let projects = RwSignal::new(Vec::<Project>::new());
    let load_error = RwSignal::new(None::<String>);
    let action_error = RwSignal::new(None::<String>);

    let reload = move || {
        spawn_local(async move {
            match list_projects().await {
                Ok(list) => {
                    projects.set(list);
                    load_error.set(None);
                }
                Err(e) => load_error.set(Some(format!("Could not read the Projects: {e}"))),
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
                    class="relative w-full max-w-xl bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl"
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
                                "Renaming changes only the name shown here. Removing a Project keeps its folder, Tasks and history."
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

                    <div class="p-6 space-y-3 max-h-[60vh] overflow-y-auto">
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
                            <ul class="space-y-3" data-testid="project-organizer-list">
                                <For
                                    each=move || projects.get()
                                    key=|project| project.id
                                    let:project
                                >
                                    <ProjectRow
                                        project=project
                                        action_error=action_error
                                        selected_project=selected_project
                                        set_selected_project=set_selected_project
                                        on_changed=on_changed
                                        reload=Callback::new(move |()| reload())
                                    />
                                </For>
                            </ul>
                        </Show>
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// One Project's row. Its rename and remove actions are independent: a failed
/// rename leaves the name as it was, and a failed remove leaves the Project in
/// the rail.
#[component]
fn ProjectRow(
    project: Project,
    action_error: RwSignal<Option<String>>,
    selected_project: Signal<String>,
    set_selected_project: WriteSignal<String>,
    on_changed: Callback<()>,
    reload: Callback<()>,
) -> impl IntoView {
    let id: Uuid = project.id;
    // The name as the backend last confirmed it, and what the user is typing.
    let saved = RwSignal::new(project.name.clone());
    let draft = RwSignal::new(project.name.clone());
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
        spawn_local(async move {
            match remove_project(id.to_string()).await {
                Ok(()) => {
                    let name = saved.get_untracked();
                    action_error.set(None);
                    if selected_project.get_untracked() == id.to_string() {
                        // The selected Project left; select the first one still active, if any.
                        let remaining = list_projects().await.unwrap_or_default();
                        set_selected_project
                            .set(remaining.first().map(|p| p.id.to_string()).unwrap_or_default());
                    }
                    toast::success(format!(
                        "Removed '{name}' from SlashIt. Open its folder to use it again."
                    ));
                    reload.run(());
                    on_changed.run(());
                    // Stop before the row is gone; try_set above covers the rest.
                }
                Err(e) => {
                    let _ = confirming.try_set(false);
                    action_error.set(Some(format!(
                        "'{}' was not removed. {e}",
                        saved.get_untracked()
                    )));
                }
            }
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
