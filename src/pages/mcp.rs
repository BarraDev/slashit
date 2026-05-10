use leptos::prelude::*;

#[component]
pub fn McpOverview() -> impl IntoView {
    view! {
        <div class="space-y-6">
            <div class="flex items-center justify-between">
                <div>
                    <h1 class="text-2xl font-bold text-white/90">"MCP Server Overview"</h1>
                    <p class="text-sm text-white/40 mt-1">"Manage MCP servers and agent configurations"</p>
                </div>
            </div>

            <div class="border border-white/10 rounded-xl bg-white/[0.02] p-12">
                <div class="flex flex-col items-center justify-center text-center">
                    <svg class="w-16 h-16 mx-auto mb-4 text-white/20" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M5 12h14M5 12a2 2 0 01-2-2V6a2 2 0 012-2h14a2 2 0 012 2v4a2 2 0 01-2 2M5 12a2 2 0 00-2 2v4a2 2 0 002 2h14a2 2 0 002-2v-4a2 2 0 00-2-2m-2-4h.01M17 16h.01" />
                    </svg>
                    <h3 class="text-lg font-semibold text-white/70 mb-2">"Coming Soon"</h3>
                    <p class="text-white/40 max-w-md">"MCP server management and agent configuration will be available in a future release."</p>
                </div>
            </div>
        </div>
    }
}
