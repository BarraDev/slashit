use leptos::prelude::*;
use crate::components::TerminalGrid;

#[component]
pub fn Agent(
    #[prop(default = String::new())] project_id: String,
) -> impl IntoView {
    view! {
        <div class="flex flex-col h-[calc(100vh-8rem)]">
            <TerminalGrid project_id=project_id />
        </div>
    }
}
