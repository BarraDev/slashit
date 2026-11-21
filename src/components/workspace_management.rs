// SPDX-License-Identifier: MPL-2.0
//! Workspace creation and membership, shown inside Organize Projects. This is
//! the one place that creates Workspaces and attaches or detaches Projects.

use crate::components::toast;
use crate::components::workspace_panel::{submitted_folder, WorkspacePanel};
use crate::models::{project::Project, workspace::Workspace};
use crate::services::{list_projects, pick_folder, workspace_service};
use leptos::callback::Callback;
use leptos::prelude::*;
use leptos::task::spawn_local;

#[component]
pub fn WorkspaceManagement(on_changed: Callback<()>) -> impl IntoView {
    let (workspaces, set_workspaces) = signal::<Vec<Workspace>>(Vec::new());
    // Whether `workspaces` is the registry as last read, rather than the
    // empty list the view starts with or falls back to on a load error. Only
    // a loaded list can prove a Project's Workspace is missing.
    let (workspaces_loaded, set_workspaces_loaded) = signal(false);
    let (projects, set_projects) = signal::<Vec<Project>>(Vec::new());
    let (new_name, set_new_name) = signal(String::new());
    let (new_path, set_new_path) = signal(String::new());
    let (creating, set_creating) = signal(false);
    // Counts membership reloads that were applied. A row that started a
    // membership change waits for this count to pass its own start.
    let membership_applied = RwSignal::new(0u64);
    provide_context(membership_applied);

    // Reloads overlap -- one per attach, detach and created workspace -- and
    // their replies can arrive out of order. Each read takes a number, and a
    // reply is applied only if no later read of the same list was started, so
    // an older registry can never replace a newer one.
    let workspaces_read = StoredValue::new(0u64);
    let projects_read = StoredValue::new(0u64);
    // Takes the next read number, or None once the organizer has closed: a
    // disposed StoredValue cannot be read, so no request is then started.
    let next = |counter: StoredValue<u64>| {
        counter.try_update_value(|n| {
            *n += 1;
            *n
        })
    };

    let load_workspaces = move |read: u64| async move {
        let result = workspace_service::list_workspaces().await;
        // try_get_value: the organizer may have closed during the await.
        if workspaces_read.try_get_value() != Some(read) {
            return;
        }
        match result {
            Ok(ws) => {
                set_workspaces.set(ws);
                set_workspaces_loaded.set(true);
            }
            Err(e) => {
                set_workspaces.set(Vec::new());
                set_workspaces_loaded.set(false);
                toast::error(format!("Could not load workspaces: {}", e));
            }
        }
    };

    // The registry is read first and the projects after it, never both at
    // once. A project list newer than the registry could name a Workspace
    // another client just created, and that Project would briefly show as
    // unresolved with a Detach button on it.
    //
    // The projects read is numbered before the task starts, so a reply that
    // is superseded by a later call never reports as applied. Pending rows
    // release their busy state on that count (see `PendingReload`).
    let reload_membership = move || {
        let Some(workspace_read) = next(workspaces_read) else {
            return;
        };
        let Some(read) = next(projects_read) else {
            return;
        };
        spawn_local(async move {
            load_workspaces(workspace_read).await;
            let result = list_projects().await;
            if projects_read.try_get_value() != Some(read) {
                return;
            }
            match result {
                Ok(ps) => set_projects.set(ps),
                Err(e) => {
                    set_projects.set(Vec::new());
                    toast::error(format!("Could not load projects: {}", e));
                }
            }
            let _ = membership_applied.try_update(|n| *n += 1);
        });
    };

    Effect::new(move |_| reload_membership());

    let on_membership_change = Callback::new(move |()| {
        reload_membership();
        on_changed.run(());
    });

    let on_pick_folder = move |_| {
        spawn_local(async move {
            match pick_folder().await {
                Ok(Some(p)) => set_new_path.set(p),
                Ok(None) => {}
                Err(e) => toast::error(format!("Folder picker failed: {}", e)),
            }
        });
    };

    let on_create = move |_| {
        let name = new_name.get();
        // The folder is submitted as typed; only a blank field means no folder.
        let path = new_path.get();
        if name.trim().is_empty() {
            toast::error("A workspace name is required".to_string());
            return;
        }
        set_creating.set(true);
        spawn_local(async move {
            let root = submitted_folder(&path);
            match workspace_service::create_workspace(&name, root).await {
                Ok(_) => {
                    let _ = set_new_name.try_set(String::new());
                    let _ = set_new_path.try_set(String::new());
                    toast::success("Workspace created".to_string());
                    reload_membership();
                    on_changed.run(());
                }
                Err(failure) => {
                    // The backend can publish a Workspace and still report an
                    // unconfirmed save. Reload so that it is shown, and clear the
                    // draft only in that case so a retry cannot duplicate it.
                    let (clear_draft, notice) =
                        workspace_service::creation_failure_notice(&failure);
                    if clear_draft {
                        let _ = set_new_name.try_set(String::new());
                        let _ = set_new_path.try_set(String::new());
                    }
                    reload_membership();
                    toast::error(notice);
                }
            }
            let _ = set_creating.try_set(false);
        });
    };

    view! {
        <div class="space-y-4" data-testid="workspace-management">
            <div class="border-t border-white/10 pt-4 space-y-3">
                <h3 class="font-semibold text-white/80">"Workspaces"</h3>
                <p class="text-xs text-white/40">
                    "A workspace groups projects; each project belongs to at most one workspace. A workspace needs no folder. Its optional Shared Agent Context Directory is advanced: when set, the CLAUDE.md and AGENTS.md it holds are copied as context into the agent runs of member projects."
                </p>
                <div class="flex flex-wrap gap-2">
                    <input
                        data-testid="workspace-name-input"
                        class="flex-1 min-w-[10rem] px-3 py-2 rounded-lg bg-black/40 border border-white/10 text-white/90 placeholder-white/30"
                        placeholder="Workspace name"
                        prop:value=move || new_name.get()
                        on:input=move |ev| set_new_name.set(event_target_value(&ev))
                    />
                    <button
                        data-testid="workspace-create"
                        on:click=on_create
                        disabled=move || creating.get()
                        class="px-4 py-2 rounded-lg bg-yellow-500 hover:bg-yellow-600 text-black font-medium disabled:opacity-50 transition-colors"
                    >
                        {move || if creating.get() { "Creating..." } else { "Create workspace" }}
                    </button>
                </div>
                <details class="text-sm text-white/50">
                    <summary class="cursor-pointer">"Advanced: Shared Agent Context Directory"</summary>
                    <div class="flex gap-2 mt-2">
                        <input
                            class="flex-1 px-3 py-2 rounded-lg bg-black/40 border border-white/10 text-white/90 placeholder-white/30 font-mono text-sm"
                            placeholder="Shared Agent Context Directory (optional)"
                            prop:value=move || new_path.get()
                            on:input=move |ev| set_new_path.set(event_target_value(&ev))
                        />
                        <button
                            on:click=on_pick_folder
                            class="px-3 py-2 rounded-lg bg-white/5 hover:bg-white/10 text-white/70 transition-colors"
                        >
                            "Browse..."
                        </button>
                    </div>
                </details>
            </div>

            <WorkspacePanel
                workspaces=workspaces
                workspaces_loaded=workspaces_loaded
                projects=projects
                on_membership_change=on_membership_change
            />
        </div>
    }
}
