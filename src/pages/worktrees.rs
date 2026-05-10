use leptos::prelude::*;
use leptos::callback::Callback;

#[component]
pub fn Worktrees(
    #[prop(default = String::new())] project_id: String,
    #[prop(optional)] on_navigate: Option<Callback<String>>,
) -> impl IntoView {
    let _project_id = project_id;
    view! {
        <div class="space-y-6">
            <div class="flex items-center justify-between">
                <div>
                    <h1 class="text-2xl font-bold text-white/90">"Worktrees"</h1>
                    <p class="text-sm text-white/40 mt-1">"Manage Jujutsu worktrees"</p>
                </div>
            </div>

            <div class="border border-white/10 rounded-xl bg-white/[0.02] p-12">
                <div class="flex flex-col items-center justify-center text-center">
                    <svg class="w-16 h-16 mx-auto mb-4 text-white/20" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M19 11H5m14 0a2 2 0 012 2v6a2 2 0 01-2 2H5a2 2 0 01-2-2v-6a2 2 0 012-2m14 0V9a2 2 0 00-2-2M5 11V9a2 2 0 012-2m0 0V5a2 2 0 012-2h6a2 2 0 012 2v2M7 7h10" />
                    </svg>
                    <h3 class="text-lg font-semibold text-white/70 mb-2">"No Worktrees"</h3>
                    <p class="text-white/40 max-w-md mb-6">"Worktrees are created automatically when SlashIt builds features"</p>
                    <button
                        on:click=move |_| {
                            if let Some(nav) = on_navigate {
                                nav.run("agent".to_string());
                            }
                        }
                        class="inline-flex items-center gap-2 px-6 py-3 rounded-xl bg-yellow-500 hover:bg-yellow-600 text-black font-medium transition-colors"
                    >
                        <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 9l3 3-3 3m5 0h3M5 20h14a2 2 0 002-2V6a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z" />
                        </svg>
                        <span>"Go to Agent Terminals"</span>
                    </button>
                    <p class="text-xs text-white/30 mt-4">"to create worktrees manually"</p>
                </div>
            </div>
        </div>
    }
}
