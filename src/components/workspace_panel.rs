use crate::models::Workspace;
use leptos::prelude::*;

#[component]
pub fn WorkspacePanel(workspaces: ReadSignal<Vec<Workspace>>) -> impl IntoView {
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
                    children=|w| view! { <WorkspaceItem workspace=w /> }
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
fn WorkspaceItem(workspace: Workspace) -> impl IntoView {
    let (expanded, set_expanded) = signal(false);
    let ws_id = workspace.id.to_string();
    let ws_path = workspace.root_path.clone();
    let ws_path_for_detail = ws_path.clone();
    let ws_path_for_header = ws_path.clone();
    let ws_name = workspace.name.clone();

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
                            <span class="text-white/70 font-mono text-xs truncate ml-4">{ws_path_for_detail.clone()}</span>
                        </div>
                    </div>
                </div>
            </Show>
        </div>
    }
}
