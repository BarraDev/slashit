use leptos::prelude::*;

#[component]
pub fn Ideation(project_id: String) -> impl IntoView {
    let _project_id = project_id;
    view! {
        <div class="space-y-6">
            <div class="flex items-center justify-between">
                <div>
                    <h1 class="text-2xl font-bold text-white/90">"Ideation"</h1>
                    <p class="text-sm text-white/40 mt-1">"AI-powered improvement discovery"</p>
                </div>
            </div>

            <div class="border border-white/10 rounded-xl bg-white/[0.02] p-12">
                <div class="flex flex-col items-center justify-center text-center">
                    <svg class="w-16 h-16 mx-auto mb-4 text-white/20" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M9.663 17h4.673M12 3v1m6.364 1.636l-.707.707M21 12h-1M4 12H3m3.343-5.657l-.707-.707m2.828 9.9a5 5 0 117.072 0l-.548.547A3.374 3.374 0 0014 18.469V19a2 2 0 11-4 0v-.531c0-.895-.356-1.754-.988-2.386l-.548-.547z" />
                    </svg>
                    <h3 class="text-lg font-semibold text-white/70 mb-2">"Coming Soon"</h3>
                    <p class="text-white/40 max-w-md">"AI-powered project analysis and improvement suggestions will be available in a future release."</p>
                </div>
            </div>
        </div>
    }
}
