use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::callback::Callback;
use crate::components::{Sidebar, ProjectTabs, ProjectRail, ToastContainer};
use crate::services::{get_project, get_project_path, appearance_service};

#[component]
pub fn AppLayout(
    #[prop(into)] current_page: Signal<String>,
    on_navigate: Callback<String>,
    #[prop(into)] selected_project: Signal<String>,
    set_selected_project: WriteSignal<String>,
    children: Children,
) -> impl IntoView {
    let (ctx_name, set_ctx_name) = signal(String::new());
    let (ctx_path, set_ctx_path) = signal(String::new());
    let (use_rail, set_use_rail) = signal(true); // default: show rail

    // Load rail preference on mount
    Effect::new(move |prev: Option<bool>| {
        if prev.is_some() { return true; }
        spawn_local(async move {
            if let Ok(val) = appearance_service::get_use_project_rail().await {
                set_use_rail.set(val);
            }
        });
        true
    });

    // Update context bar when selected project changes
    Effect::new(move |_| {
        let project_id = selected_project.get();
        if project_id.is_empty() {
            set_ctx_name.set(String::new());
            set_ctx_path.set(String::new());
            return;
        }
        let pid = project_id.clone();
        spawn_local(async move {
            if let Ok(Some(p)) = get_project(pid.clone()).await {
                set_ctx_name.set(p.name);
            }
            if let Ok(Some(path)) = get_project_path(pid).await {
                set_ctx_path.set(path);
            }
        });
    });

    view! {
        <div class="flex h-screen bg-[#08080C] text-white">
            // Show ProjectRail when use_rail is true
            <Show when=move || use_rail.get()>
                <ProjectRail
                    selected_project=selected_project
                    set_selected_project=set_selected_project
                />
            </Show>
            <Sidebar
                current_page=current_page
                on_navigate=on_navigate
                selected_project=selected_project
                set_selected_project=set_selected_project
            />
            <div class="flex-1 flex flex-col overflow-hidden bg-gradient-to-br from-transparent via-blue-500/[0.02] to-purple-500/[0.02]">
                // Show ProjectTabs when use_rail is false
                <Show when=move || !use_rail.get()>
                    <ProjectTabs
                        selected_project=selected_project
                        set_selected_project=set_selected_project
                    />
                </Show>
                // Project context bar — hidden when Rail sidebar is active (redundant)
                <Show when=move || !use_rail.get()>
                    {move || {
                        let name = ctx_name.get();
                        if name.is_empty() {
                            view! { <div></div> }.into_any()
                        } else {
                            let path = ctx_path.get();
                            view! {
                                <div class="flex items-center gap-3 px-4 py-1.5 bg-zinc-800/50 border-b border-white/5 text-sm">
                                    <div class="w-1.5 h-1.5 rounded-full bg-yellow-400"></div>
                                    <span class="text-white/70 font-medium">{name}</span>
                                    {if !path.is_empty() {
                                        view! {
                                            <span class="text-white/30">{path}</span>
                                        }.into_any()
                                    } else {
                                        view! { <span></span> }.into_any()
                                    }}
                                </div>
                            }.into_any()
                        }
                    }}
                </Show>
                <main class="flex-1 min-h-0 overflow-auto">
                    <div class="h-full max-w-[1800px] mx-auto p-6">
                        {children()}
                    </div>
                </main>
            </div>
        </div>
        <ToastContainer />
    }
}
