// SPDX-License-Identifier: MPL-2.0
//! The Organize Projects panel: reorder, rename, remove from the rail and
//! restore. Edits are a draft until Save; Cancel discards them.

use std::collections::HashMap;

use leptos::callback::Callback;
use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::models::{arrange, Project, ProjectRailConfig};
use crate::services::{rename_project, update_project_rail};

/// Move `index` one place up or down within `order`, if that stays in range.
fn step(order: &mut [Uuid], index: usize, down: bool) {
    let target = if down { index + 1 } else { index.wrapping_sub(1) };
    if index < order.len() && target < order.len() {
        order.swap(index, target);
    }
}

#[component]
pub fn ProjectOrganizer(
    #[prop(into)] show: Signal<bool>,
    set_show: WriteSignal<bool>,
    #[prop(into)] projects: Signal<Vec<Project>>,
    #[prop(into)] rail: Signal<ProjectRailConfig>,
    on_saved: Callback<()>,
) -> impl IntoView {
    let draft_order = RwSignal::new(Vec::<Uuid>::new());
    let draft_hidden = RwSignal::new(Vec::<Uuid>::new());
    let draft_names = RwSignal::new(HashMap::<Uuid, String>::new());
    let error = RwSignal::new(None::<String>);
    let saving = RwSignal::new(false);

    // Start each opening from the saved state, so an earlier cancel leaves nothing behind.
    Effect::new(move |prev: Option<bool>| {
        let open = show.get();
        if open && prev != Some(true) {
            let (visible, hidden) = arrange(&projects.get_untracked(), &rail.get_untracked());
            draft_order.set(visible.iter().map(|p| p.id).collect());
            draft_hidden.set(hidden.iter().map(|p| p.id).collect());
            draft_names.set(projects.get_untracked().into_iter().map(|p| (p.id, p.name)).collect());
            error.set(None);
        }
        open
    });

    let save = move |_| {
        if saving.get_untracked() {
            return;
        }
        let names = draft_names.get_untracked();
        let mut renames = Vec::new();
        for project in projects.get_untracked() {
            let wanted = names.get(&project.id).map(|n| n.trim().to_string()).unwrap_or_default();
            if wanted.is_empty() {
                error.set(Some(format!("“{}” needs a name.", project.name)));
                return;
            }
            if wanted != project.name {
                renames.push((project.id, wanted));
            }
        }
        let order = draft_order.get_untracked();
        let hidden = draft_hidden.get_untracked();
        saving.set(true);
        error.set(None);
        spawn_local(async move {
            // Renames first. A failed rename stops the save here, so no rail
            // change is applied on top of a half-applied draft.
            for (id, name) in renames {
                if let Err(e) = rename_project(id.to_string(), name).await {
                    error.set(Some(format!("The name could not be saved: {e}")));
                    on_saved.run(());
                    saving.set(false);
                    return;
                }
            }
            match update_project_rail(order, hidden).await {
                Ok(_) => {
                    set_show.set(false);
                    on_saved.run(());
                }
                Err(e) => error.set(Some(format!("The rail could not be saved. {e}"))),
            }
            saving.set(false);
        });
    };

    let name_of = move |id: Uuid| {
        draft_names.with(|names| names.get(&id).cloned().unwrap_or_default())
    };

    view! {
        <Show when=move || show.get()>
            <div
                class="fixed inset-0 z-50 flex items-center justify-center bg-black/60"
                data-testid="organize-projects-dialog"
            >
                <div
                    role="dialog"
                    aria-modal="true"
                    aria-labelledby="organize-projects-title"
                    class="w-[min(520px,92vw)] max-h-[85vh] overflow-y-auto rounded-xl bg-zinc-900 border border-white/10 p-5 text-white"
                >
                    <h2 id="organize-projects-title" class="text-lg font-semibold">"Organize Projects"</h2>
                    <p class="text-sm text-white/60 mt-1">
                        "Removing a Project from the rail keeps its repository, Tasks and history. Nothing is deleted."
                    </p>

                    <h3 class="text-xs uppercase tracking-wide text-white/40 mt-4 mb-2">"On the rail"</h3>
                    {move || {
                        let order = draft_order.get();
                        if order.is_empty() {
                            view! {
                                <p class="text-sm text-white/50" data-testid="organize-empty-rail">
                                    "No Projects are on the rail. Restore one below."
                                </p>
                            }.into_any()
                        } else {
                            let last = order.len() - 1;
                            view! {
                                <ul class="flex flex-col gap-2">
                                    {order.into_iter().enumerate().map(|(index, id)| {
                                        view! {
                                            <li class="flex items-center gap-2" data-testid="organize-row">
                                                <input
                                                    type="text"
                                                    aria-label=format!("Display name for position {}", index + 1)
                                                    class="flex-1 min-w-0 rounded bg-black/40 border border-white/10 px-2 py-1 text-sm"
                                                    prop:value=move || name_of(id)
                                                    on:input=move |ev| {
                                                        let value = event_target_value(&ev);
                                                        draft_names.update(|names| { names.insert(id, value); });
                                                    }
                                                />
                                                <button
                                                    type="button"
                                                    class="px-2 py-1 text-sm rounded bg-white/5 hover:bg-white/10 disabled:opacity-30"
                                                    aria-label="Move up"
                                                    disabled=index == 0
                                                    on:click=move |_| draft_order.update(|o| step(o, index, false))
                                                >"Up"</button>
                                                <button
                                                    type="button"
                                                    class="px-2 py-1 text-sm rounded bg-white/5 hover:bg-white/10 disabled:opacity-30"
                                                    aria-label="Move down"
                                                    disabled=index == last
                                                    on:click=move |_| draft_order.update(|o| step(o, index, true))
                                                >"Down"</button>
                                                <button
                                                    type="button"
                                                    class="px-2 py-1 text-sm rounded bg-white/5 hover:bg-white/10"
                                                    on:click=move |_| {
                                                        draft_order.update(|o| o.retain(|x| *x != id));
                                                        draft_hidden.update(|h| h.push(id));
                                                    }
                                                >"Remove from rail"</button>
                                            </li>
                                        }
                                    }).collect::<Vec<_>>()}
                                </ul>
                            }.into_any()
                        }
                    }}

                    <h3 class="text-xs uppercase tracking-wide text-white/40 mt-5 mb-2">"Removed from the rail"</h3>
                    {move || {
                        let hidden = draft_hidden.get();
                        if hidden.is_empty() {
                            view! { <p class="text-sm text-white/50">"No Projects are removed."</p> }.into_any()
                        } else {
                            view! {
                                <ul class="flex flex-col gap-2">
                                    {hidden.into_iter().map(|id| {
                                        view! {
                                            <li class="flex items-center gap-2" data-testid="organize-hidden-row">
                                                <span class="flex-1 min-w-0 truncate text-sm">{move || name_of(id)}</span>
                                                <button
                                                    type="button"
                                                    class="px-2 py-1 text-sm rounded bg-white/5 hover:bg-white/10"
                                                    on:click=move |_| {
                                                        draft_hidden.update(|h| h.retain(|x| *x != id));
                                                        draft_order.update(|o| o.push(id));
                                                    }
                                                >"Restore"</button>
                                            </li>
                                        }
                                    }).collect::<Vec<_>>()}
                                </ul>
                            }.into_any()
                        }
                    }}

                    {move || error.get().map(|message| view! {
                        <p role="alert" class="mt-4 text-sm text-red-300" data-testid="organize-error">{message}</p>
                    })}

                    <div class="flex justify-end gap-2 mt-5">
                        <button
                            type="button"
                            class="px-3 py-1.5 text-sm rounded bg-white/5 hover:bg-white/10"
                            on:click=move |_| set_show.set(false)
                        >"Cancel"</button>
                        <button
                            type="button"
                            data-testid="organize-save"
                            class="px-3 py-1.5 text-sm rounded bg-indigo-600 hover:bg-indigo-500 disabled:opacity-50"
                            disabled=move || saving.get()
                            on:click=save
                        >"Save"</button>
                    </div>
                </div>
            </div>
        </Show>
    }
}
