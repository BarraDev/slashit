use leptos::prelude::*;

#[component]
pub fn Changelog(
    #[prop(default = String::new())] project_id: String,
) -> impl IntoView {
    let _project_id = project_id;
    view! {
        <div class="space-y-6">
            <div class="flex items-center justify-between">
                <div>
                    <h1 class="text-2xl font-bold text-white/90">"Changelog Generator"</h1>
                    <p class="text-sm text-white/40 mt-1">"Generate changelogs from completed work"</p>
                </div>
            </div>

            <div class="border border-white/10 rounded-xl bg-white/[0.02] p-12">
                <div class="flex flex-col items-center justify-center text-center">
                    <svg class="w-16 h-16 mx-auto mb-4 text-white/20" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
                    </svg>
                    <h3 class="text-lg font-semibold text-white/70 mb-2">"Coming Soon"</h3>
                    <p class="text-white/40 max-w-md">"Automatic changelog generation from tasks and git history will be available in a future release."</p>
                </div>
            </div>
        </div>
    }
}
