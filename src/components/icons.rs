use leptos::prelude::*;

// Icon components below use #[prop(into)] class which Leptos macros consume in view!{}.
// Rust's dead code analysis cannot see into macro expansions, so these appear as
// "field is never read" warnings but are actually used.

#[component]
pub fn KanbanIcon(#[prop(into)] class: String) -> impl IntoView {
    view! {
        <svg class=format!("w-5 h-5 {}", class) xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="3" y="3" width="18" height="18" rx="2" ry="2"></rect>
            <line x1="9" y1="3" x2="9" y2="21"></line>
            <line x1="15" y1="3" x2="15" y2="21"></line>
        </svg>
    }
}

#[component]
pub fn TerminalIcon(#[prop(into)] class: String) -> impl IntoView {
    view! {
        <svg class=format!("w-5 h-5 {}", class) xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="4 17 10 11 4 5"></polyline>
            <line x1="12" y1="19" x2="20" y2="19"></line>
        </svg>
    }
}

#[component]
pub fn RoadmapIcon(#[prop(into)] class: String) -> impl IntoView {
    view! {
        <svg class=format!("w-5 h-5 {}", class) xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="1 4 1 10 7 10"></polyline>
            <polyline points="23 20 23 14 17 14"></polyline>
            <path d="M20.49 9A9 9 0 0 0 5.64 5.64L1 10m22 4l-4.64 4.36A9 9 0 0 1 3.51 15"></path>
        </svg>
    }
}

#[component]
pub fn SettingsIcon(#[prop(into)] class: String) -> impl IntoView {
    view! {
        <svg class=format!("w-5 h-5 {}", class) xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="3"></circle>
            <path d="M12 1v6m0 6v6M1 12h6m6 0h6m-9.9-7.1l4.24 4.24m0 4.24l-4.24 4.24M4.93 19.07l4.24-4.24m4.24 0l4.24-4.24"></path>
        </svg>
    }
}

#[component]
pub fn ContextIcon(#[prop(into)] class: String) -> impl IntoView {
    view! {
        <svg class=format!("w-5 h-5 {}", class) xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"></path>
            <polyline points="3.27 6.96 12 12.01 20.73 6.96"></polyline>
            <line x1="12" y1="22.08" x2="12" y2="12"></line>
        </svg>
    }
}

#[component]
pub fn LightbulbIcon(#[prop(into)] class: String) -> impl IntoView {
    view! {
        <svg class=format!("w-5 h-5 {}", class) xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <line x1="9" y1="18" x2="15" y2="18"></line>
            <line x1="10" y1="22" x2="14" y2="22"></line>
            <path d="M15.09 14c.18-.9.27-1.85.26-2.83a7.03 7.03 0 0 0-4-6.32"></path>
            <path d="M8.91 14c-.18-.9-.27-1.85-.26-2.83A7.03 7.03 0 0 1 12 4.85"></path>
        </svg>
    }
}

#[component]
pub fn PlayIcon(#[prop(into)] class: String) -> impl IntoView {
    view! {
        <svg class=format!("w-5 h-5 {}", class) xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polygon points="5 3 19 12 5 21 5 3"></polygon>
        </svg>
    }
}

#[component]
pub fn StopIcon(#[prop(into)] class: String) -> impl IntoView {
    view! {
        <svg class=format!("w-5 h-5 {}", class) xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="4" y="4" width="16" height="16"></rect>
        </svg>
    }
}

#[component]
pub fn TrashIcon(#[prop(into)] class: String) -> impl IntoView {
    view! {
        <svg class=format!("w-5 h-5 {}", class) xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="3 6 5 6 21 6"></polyline>
            <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"></path>
        </svg>
    }
}

#[component]
pub fn PlusIcon(#[prop(into)] class: String) -> impl IntoView {
    view! {
        <svg class=format!("w-5 h-5 {}", class) xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <line x1="12" y1="5" x2="12" y2="19"></line>
            <line x1="5" y1="12" x2="19" y2="12"></line>
        </svg>
    }
}

#[component]
pub fn XIcon(#[prop(into)] class: String) -> impl IntoView {
    view! {
        <svg class=format!("w-5 h-5 {}", class) xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <line x1="18" y1="6" x2="6" y2="18"></line>
            <line x1="6" y1="6" x2="18" y2="18"></line>
        </svg>
    }
}
