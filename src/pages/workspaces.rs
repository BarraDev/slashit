use crate::components::toast;
use crate::components::workspace_panel::WorkspacePanel;
use crate::models::workspace::Workspace;
use crate::services::{pick_folder, workspace_service};
use leptos::prelude::*;
use leptos::task::spawn_local;

#[component]
pub fn Workspaces() -> impl IntoView {
    let (workspaces, set_workspaces) = signal::<Vec<Workspace>>(Vec::new());
    let (new_name, set_new_name) = signal(String::new());
    let (new_path, set_new_path) = signal(String::new());
    let (creating, set_creating) = signal(false);

    let reload = move || {
        spawn_local(async move {
            match workspace_service::list_workspaces().await {
                Ok(ws) => set_workspaces.set(ws),
                Err(e) => {
                    set_workspaces.set(Vec::new());
                    toast::error(format!("Could not load workspaces: {}", e));
                }
            }
        });
    };

    Effect::new(move |_| {
        reload();
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
        let path = new_path.get();
        if name.trim().is_empty() || path.trim().is_empty() {
            toast::error("Name and root path are required".to_string());
            return;
        }
        set_creating.set(true);
        spawn_local(async move {
            match workspace_service::create_workspace(&name, &path).await {
                Ok(_) => {
                    set_new_name.set(String::new());
                    set_new_path.set(String::new());
                    toast::success("Workspace created".to_string());
                    reload();
                }
                Err(e) => toast::error(format!("Failed to create workspace: {}", e)),
            }
            set_creating.set(false);
        });
    };

    view! {
        <div class="space-y-6">
            <div>
                <h1 class="text-2xl font-bold text-white/90">"Workspaces"</h1>
                <p class="text-sm text-white/40 mt-1">
                    "A workspace is a meta folder that coordinates one or more projects. Agents launch from the workspace root and read its instruction files."
                </p>
            </div>

            <div class="border border-white/10 rounded-xl bg-white/[0.02] p-4 space-y-3">
                <h2 class="font-semibold text-white/80">"Add Workspace"</h2>
                <div class="flex gap-2">
                    <input
                        class="flex-1 px-3 py-2 rounded-lg bg-black/40 border border-white/10 text-white/90 placeholder-white/30"
                        placeholder="Workspace name"
                        prop:value=move || new_name.get()
                        on:input=move |ev| set_new_name.set(event_target_value(&ev))
                    />
                    <input
                        class="flex-1 px-3 py-2 rounded-lg bg-black/40 border border-white/10 text-white/90 placeholder-white/30 font-mono text-sm"
                        placeholder="Root path (e.g. ~/Repo/ps)"
                        prop:value=move || new_path.get()
                        on:input=move |ev| set_new_path.set(event_target_value(&ev))
                    />
                    <button
                        on:click=on_pick_folder
                        class="px-3 py-2 rounded-lg bg-white/5 hover:bg-white/10 text-white/70 transition-colors"
                    >
                        "Browse..."
                    </button>
                    <button
                        on:click=on_create
                        disabled=move || creating.get()
                        class="px-4 py-2 rounded-lg bg-yellow-500 hover:bg-yellow-600 text-black font-medium disabled:opacity-50 transition-colors"
                    >
                        {move || if creating.get() { "Creating..." } else { "Create" }}
                    </button>
                </div>
            </div>

            <WorkspacePanel workspaces=workspaces />
        </div>
    }
}
