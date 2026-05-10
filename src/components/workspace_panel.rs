use leptos::prelude::*;
use crate::models::Workspace;

#[component]
pub fn WorkspacePanel(workspaces: Vec<Workspace>) -> impl IntoView {
    view! {
        <div class="border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden">
            <div class="px-4 py-3 border-b border-white/5">
                <div class="flex items-center gap-2">
                    <svg class="w-5 h-5 text-white/40" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 11H5m14 0a2 2 0 012 2v6a2 2 0 01-2 2H5a2 2 0 01-2-2v-6a2 2 0 012-2m14 0V9a2 2 0 00-2-2M5 11V9a2 2 0 012-2m0 0V5a2 2 0 012-2h6a2 2 0 012 2v2M7 7h10" />
                    </svg>
                    <h2 class="font-semibold text-white/90">"Workspaces"</h2>
                    <span class="px-2 py-0.5 rounded text-xs font-medium bg-white/5 text-white/40">
                        {workspaces.len()}
                    </span>
                </div>
            </div>

            <div class="divide-y divide-white/5">
                {workspaces.iter().map(|workspace| {
                    view! {
                        <WorkspaceItem workspace=workspace.clone() />
                    }
                }).collect::<Vec<_>>()}
            </div>

            {if workspaces.is_empty() {
                view! {
                    <div class="p-8 text-center">
                        <svg class="w-12 h-12 mx-auto mb-3 text-white/20" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 11H5m14 0a2 2 0 012 2v6a2 2 0 01-2 2H5a2 2 0 01-2-2v-6a2 2 0 012-2m14 0V9a2 2 0 00-2-2M5 11V9a2 2 0 012-2m0 0V5a2 2 0 012-2h6a2 2 0 012 2v2M7 7h10" />
                        </svg>
                        <p class="text-white/40 text-sm">"No workspaces configured"</p>
                    </div>
                }.into_any()
            } else {
                ().into_any()
            }}
        </div>
    }
}

#[component]
fn WorkspaceItem(workspace: Workspace) -> impl IntoView {
    let (expanded, set_expanded) = signal(false);

    let ws_id = workspace.id.to_string();
    let ws_path = workspace.path.clone();
    let ws_change_id = workspace.current_change_id.clone();

    view! {
        <div class="group">
            <button
                on:click=move |_| set_expanded.update(|e| *e = !*e)
                class="w-full px-4 py-3 flex items-center justify-between hover:bg-white/5 transition-colors"
            >
                <div class="flex items-center gap-3 flex-1 min-w-0">
                    <svg class=format!(
                        "w-4 h-4 text-white/30 transition-transform {}",
                        if expanded.get() { "rotate-90" } else { "" }
                    ) fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7" />
                    </svg>
                    <div class="flex-1 min-w-0 text-left">
                        <h3 class="font-medium text-white/90 truncate">{workspace.name}</h3>
                        <p class="text-xs text-white/40 truncate font-mono mt-0.5">
                            {ws_path.clone()}
                        </p>
                    </div>
                </div>
                <div class="flex items-center gap-2">
                    {if let Some(change_id) = &workspace.current_change_id {
                        view! {
                            <span class="px-2 py-1 rounded text-xs font-medium bg-blue-500/20 text-blue-300">
                                {format!("Change: {}", change_id)}
                            </span>
                        }.into_any()
                    } else {
                        ().into_any()
                    }}
                </div>
            </button>

            <Show when=move || expanded.get()>
                <div class="px-4 pb-4 pl-11">
                    <div class="bg-black/40 rounded-lg p-3 space-y-2">
                        <div class="flex items-center justify-between text-sm">
                            <span class="text-white/40">"ID:"</span>
                            <span class="text-white/70 font-mono text-xs">{ws_id.clone()}</span>
                        </div>
                        <div class="flex items-center justify-between text-sm">
                            <span class="text-white/40">"Path:"</span>
                            <span class="text-white/70 font-mono text-xs truncate ml-4">{ws_path.clone()}</span>
                        </div>
                        {if let Some(change_id) = &ws_change_id {
                            view! {
                                <div class="flex items-center justify-between text-sm">
                                    <span class="text-white/40">"Current Change:"</span>
                                    <span class="text-blue-300 font-mono text-xs">{change_id.to_string()}</span>
                                </div>
                            }.into_any()
                        } else {
                            ().into_any()
                        }}
                    </div>
                </div>
            </Show>
        </div>
    }
}
