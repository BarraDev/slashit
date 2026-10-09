use leptos::prelude::*;
use crate::components::project_conversation::ProjectConversation;

/// The Project's Coordinator conversation, on its own full-height page.
#[component]
pub fn Conversations(project_id: String) -> impl IntoView {
    if project_id.is_empty() {
        return view! {
            <div data-testid="conversations-empty" class="flex h-full items-center justify-center text-sm text-white/50">
                "Select a project from the sidebar to talk with its Coordinator."
            </div>
        }.into_any();
    }

    view! {
        <div data-testid="conversations-page" class="h-full min-h-0 p-4">
            <ProjectConversation project_id=project_id />
        </div>
    }.into_any()
}
