use crate::components::custom_select::{CustomSelect, SelectOption};
use crate::components::toast;
use crate::models::{Project, ProjectScope, Workspace};
use crate::services::{attach_project_to_workspace, detach_project_from_workspace};
use leptos::callback::Callback;
use leptos::prelude::*;
use leptos::task::spawn_local;

#[component]
pub fn WorkspacePanel(
    workspaces: ReadSignal<Vec<Workspace>>,
    projects: ReadSignal<Vec<Project>>,
    /// Fired after a successful attach or detach, so the parent can reload
    /// the project list that both this panel and the rest of the app share.
    on_membership_change: Callback<()>,
) -> impl IntoView {
    view! {
        <div class="border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden">
            <div class="px-4 py-3 border-b border-white/5">
                <div class="flex items-center gap-2">
                    <svg class="w-5 h-5 text-white/40" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M3 7h18M3 12h18M3 17h18" />
                    </svg>
                    <h2 class="font-semibold text-white/90">"Workspaces"</h2>
                    <span class="px-2 py-0.5 rounded text-xs font-medium bg-white/5 text-white/40">
                        {move || workspaces.with(Vec::len)}
                    </span>
                </div>
            </div>

            <div class="divide-y divide-white/5">
                <For
                    each=move || workspaces.get()
                    key=|w| w.id
                    children=move |w| view! {
                        <WorkspaceItem workspace=w projects=projects on_membership_change=on_membership_change />
                    }
                />
            </div>

            <Show when=move || workspaces.with(Vec::is_empty)>
                <div class="p-8 text-center">
                    <p class="text-white/40 text-sm">"No workspaces configured"</p>
                </div>
            </Show>
        </div>
    }
}

#[component]
fn WorkspaceItem(
    workspace: Workspace,
    projects: ReadSignal<Vec<Project>>,
    on_membership_change: Callback<()>,
) -> impl IntoView {
    let (expanded, set_expanded) = signal(false);
    let (busy, set_busy) = signal(false);
    let (selected_to_attach, set_selected_to_attach) = signal(String::new());
    let ws_id = workspace.id;
    let ws_path = workspace.root_path.clone();
    let ws_path_for_detail = ws_path.clone();
    let ws_path_for_header = ws_path.clone();
    let ws_name = workspace.name.clone();

    let members = move || {
        projects
            .get()
            .into_iter()
            .filter(|p| p.scope.workspace_id() == Some(ws_id))
            .collect::<Vec<_>>()
    };
    let eligible_options = move || {
        projects
            .get()
            .into_iter()
            .filter(|p| p.scope == ProjectScope::Standalone)
            .map(|p| SelectOption::new(p.id.to_string(), p.name))
            .collect::<Vec<_>>()
    };

    let on_attach = move |_| {
        let project_id = selected_to_attach.get();
        if project_id.is_empty() {
            toast::error("Pick a project to attach".to_string());
            return;
        }
        set_busy.set(true);
        spawn_local(async move {
            match attach_project_to_workspace(project_id, ws_id.to_string()).await {
                Ok(project) => {
                    toast::success(format!("Attached {} to this workspace", project.name));
                    set_selected_to_attach.set(String::new());
                    on_membership_change.run(());
                }
                Err(e) => toast::error(format!("Failed to attach project: {}", e)),
            }
            set_busy.set(false);
        });
    };

    let on_detach = move |project_id: String, project_name: String| {
        set_busy.set(true);
        spawn_local(async move {
            match detach_project_from_workspace(project_id).await {
                Ok(_) => {
                    toast::success(format!("Detached {} from this workspace", project_name));
                    on_membership_change.run(());
                }
                Err(e) => toast::error(format!("Failed to detach project: {}", e)),
            }
            set_busy.set(false);
        });
    };

    view! {
        <div class="group">
            <button
                on:click=move |_| set_expanded.update(|e| *e = !*e)
                class="w-full px-4 py-3 flex items-center justify-between hover:bg-white/5 transition-colors"
            >
                <div class="flex items-center gap-3 flex-1 min-w-0">
                    <svg class=move || format!(
                        "w-4 h-4 text-white/30 transition-transform {}",
                        if expanded.get() { "rotate-90" } else { "" }
                    ) fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7" />
                    </svg>
                    <div class="flex-1 min-w-0 text-left">
                        <h3 class="font-medium text-white/90 truncate">{ws_name}</h3>
                        <p class="text-xs text-white/40 truncate font-mono mt-0.5">{ws_path_for_header}</p>
                    </div>
                </div>
                <span class="px-2 py-0.5 rounded text-xs font-medium bg-white/5 text-white/40">
                    {move || members().len()}
                </span>
            </button>

            <Show when=move || expanded.get()>
                <div class="px-4 pb-4 pl-11 space-y-3">
                    <div class="bg-black/40 rounded-lg p-3 space-y-2">
                        <div class="flex items-center justify-between text-sm">
                            <span class="text-white/40">"ID:"</span>
                            <span class="text-white/70 font-mono text-xs">{ws_id.to_string()}</span>
                        </div>
                        <div class="flex items-center justify-between text-sm">
                            <span class="text-white/40">"Path:"</span>
                            <span class="text-white/70 font-mono text-xs truncate ml-4">{ws_path_for_detail.clone()}</span>
                        </div>
                    </div>

                    <div class="space-y-2">
                        <h4 class="text-xs font-medium text-white/40 uppercase tracking-wide">"Projects in this workspace"</h4>
                        <div class="space-y-1">
                            <For
                                each=members
                                key=|p| p.id
                                children=move |p| {
                                    let name = p.name.clone();
                                    let name_for_detach = name.clone();
                                    let pid = p.id.to_string();
                                    view! {
                                        <div class="flex items-center justify-between px-3 py-2 rounded-lg bg-white/[0.03]">
                                            <span class="text-sm text-white/80 truncate">{name}</span>
                                            <button
                                                disabled=move || busy.get()
                                                on:click=move |_| on_detach(pid.clone(), name_for_detach.clone())
                                                class="text-xs px-2 py-1 rounded bg-white/5 hover:bg-red-500/20 hover:text-red-300 text-white/50 transition-colors disabled:opacity-50"
                                            >
                                                "Detach"
                                            </button>
                                        </div>
                                    }
                                }
                            />
                            <Show when=move || members().is_empty()>
                                <p class="text-xs text-white/30 px-3 py-2">"No projects attached yet"</p>
                            </Show>
                        </div>
                    </div>

                    <div class="space-y-2">
                        <h4 class="text-xs font-medium text-white/40 uppercase tracking-wide">"Attach a project"</h4>
                        <Show
                            when=move || !eligible_options().is_empty()
                            fallback=|| view! {
                                <p class="text-xs text-white/30">"No standalone projects available to attach"</p>
                            }
                        >
                            <div class="flex gap-2">
                                <div class="flex-1">
                                    <CustomSelect
                                        options=eligible_options()
                                        selected=Signal::derive(move || selected_to_attach.get())
                                        on_change=Callback::new(move |v| set_selected_to_attach.set(v))
                                        placeholder="Select a project...".to_string()
                                        disabled=busy.get()
                                    />
                                </div>
                                <button
                                    disabled=move || busy.get() || selected_to_attach.get().is_empty()
                                    on:click=on_attach
                                    class="px-3 py-2 rounded-lg bg-yellow-500 hover:bg-yellow-600 text-black text-sm font-medium disabled:opacity-50 transition-colors"
                                >
                                    "Attach"
                                </button>
                            </div>
                        </Show>
                    </div>
                </div>
            </Show>
        </div>
    }
}
