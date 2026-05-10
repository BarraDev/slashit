use crate::components::{AppLayout, QuitDialog};
use crate::pages::*;
use crate::services::get_project;
use leptos::prelude::*;
use leptos::callback::Callback;
use leptos::task::spawn_local;
/// Get the last selected page from localStorage, or default to "dashboard"
fn get_persisted_page() -> String {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            if let Ok(Some(page)) = storage.get_item("slashit_current_page") {
                return page;
            }
        }
    }
    "dashboard".to_string()
}

/// Persist the current page to localStorage
fn persist_page(page: &str) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item("slashit_current_page", page);
        }
    }
}

/// Get the last selected project from localStorage
fn get_persisted_selected_project() -> String {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            if let Ok(Some(project)) = storage.get_item("slashit_selected_project") {
                return project;
            }
        }
    }
    String::new()
}

/// Persist the selected project to localStorage
fn persist_selected_project(project_id: &str) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item("slashit_selected_project", project_id);
        }
    }
}

/// Clear the persisted selected project from localStorage
fn clear_persisted_selected_project() {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.remove_item("slashit_selected_project");
        }
    }
}

#[component]
pub fn App() -> impl IntoView {
    // Restore last selected page from localStorage
    let (current_page, set_current_page) = signal(get_persisted_page());
    // Restore last selected project from localStorage
    let (selected_project, set_selected_project) = signal(get_persisted_selected_project());

    // Provide current page as global context so components can react to navigation
    // even if they are kept alive/cached by Leptos.
    provide_context(current_page);
    
    // Provide selected project as context for child components
    provide_context(selected_project);
    
    // Validate persisted project exists on startup
    Effect::new(move |prev: Option<bool>| {
        // Only run once on mount
        if prev.is_some() {
            return true;
        }
        
        let project_id = selected_project.get_untracked();
        if !project_id.is_empty() {
            spawn_local(async move {
                match get_project(project_id.clone()).await {
                    Ok(Some(_)) => {
                        // Project exists, keep it selected
                    }
                    Ok(None) => {
                        // Project doesn't exist, clear selection
                        leptos::logging::log!("[App] Persisted project {} not found, clearing", project_id);
                        set_selected_project.set(String::new());
                        clear_persisted_selected_project();
                    }
                    Err(e) => {
                        // Error checking, log but don't clear (might be temporary)
                        leptos::logging::warn!("[App] Error validating project {}: {}", project_id, e);
                    }
                }
            });
        }
        
        true
    });
    
    // Persist selected project whenever it changes
    Effect::new(move |_| {
        let project = selected_project.get();
        persist_selected_project(&project);
    });

    let on_navigate = Callback::new({
        move |page: String| {
            persist_page(&page);
            set_current_page.set(page);
        }
    });

    view! {
        <QuitDialog />
        <AppLayout
            current_page=current_page
            on_navigate=on_navigate
            selected_project=selected_project
            set_selected_project=set_selected_project
        >
            <div class="content">
                {
                    let current_page = current_page;
                    let selected_project = selected_project;
                    move || match current_page.get().as_str() {
                        "dashboard" => view! { <Dashboard project_id=selected_project.get() /> }.into_any(),
                        "agent" => view! { <Agent project_id=selected_project.get() /> }.into_any(),
                        "roadmap" => view! { <Roadmap project_id=selected_project.get() /> }.into_any(),
                        "ideation" => view! { <Ideation project_id=selected_project.get() /> }.into_any(),
                        "context" => view! { <Context project_id=selected_project.get() /> }.into_any(),
                        "spec" => view! { <Spec project_id=selected_project.get() /> }.into_any(),
                        "settings" => view! { <Settings /> }.into_any(),
                        "insights" => view! { <Insights project_id=selected_project.get() /> }.into_any(),
                        "changelog" => view! { <Changelog project_id=selected_project.get() /> }.into_any(),
                        "mcp" => view! { <McpOverview /> }.into_any(),
                        "worktrees" => view! { <Worktrees project_id=selected_project.get() on_navigate=on_navigate /> }.into_any(),
                        "github_issues" => view! { <GithubIssues project_id=selected_project.get() /> }.into_any(),
                        "github_prs" => view! { <GithubPrs project_id=selected_project.get() /> }.into_any(),
                        _ => view! { <Dashboard project_id=selected_project.get() /> }.into_any(),
                    }
                }
            </div>
        </AppLayout>
    }
}
