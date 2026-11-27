// SPDX-License-Identifier: MPL-2.0
use crate::components::{provide_updater_context, setup_keyboard_shortcuts, AppLayout, OpenTaskRequest, QuitDialog, UpdateBanner};
use crate::components::workspace_home::WorkspaceHome;
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
                // A page that was removed, or that the navigation no longer
                // offers, would open with no sidebar entry to return by, so
                // fall back to the default.
                if page_restorable(&page) {
                    return page;
                }
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
const SELECTED_WORKSPACE_KEY: &str = "slashit_selected_workspace";

fn get_persisted_selected_workspace() -> String {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(SELECTED_WORKSPACE_KEY).ok().flatten())
        .unwrap_or_default()
}

fn persist_selected_workspace(workspace_id: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        if workspace_id.is_empty() {
            let _ = storage.remove_item(SELECTED_WORKSPACE_KEY);
        } else {
            let _ = storage.set_item(SELECTED_WORKSPACE_KEY, workspace_id);
        }
    }
}

/// The error `get_workspace` returns when the registry has no Workspace with
/// that id. It is the backend command's message, matched exactly.
const WORKSPACE_NOT_FOUND: &str = "Workspace not found";

/// The Workspace selection after checking the persisted one, `checked`, on
/// startup. The check's `result` clears the selection only when it reports
/// that the Workspace does not exist; any other error may be temporary and
/// keeps it. A selection the user changed while the check was pending is
/// left as they set it.
fn workspace_selection_after_check(
    current: String,
    checked: &str,
    result: Result<(), &str>,
) -> String {
    match result {
        Err(WORKSPACE_NOT_FOUND) if current == checked => String::new(),
        _ => current,
    }
}

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
    // Workspace context is separate from the selected Project: entering a
    // Workspace leaves the Project selection as it was.
    let (selected_workspace, set_selected_workspace) = signal(get_persisted_selected_workspace());
    // Bumped by the rail after a Project or Workspace change, so an open Workspace
    // page re-reads its record and members. Shared by context, because the rail
    // and the page are siblings in the layout.
    let (refresh_trigger, set_refresh_trigger) = signal(0u32);
    provide_context(refresh_trigger);
    provide_context(set_refresh_trigger);

    // Provide current page as global context so components can react to navigation
    // even if they are kept alive/cached by Leptos.
    provide_context(current_page);
    
    // Provide selected project as context for child components
    provide_context(selected_project);

    // Updater state is provided once, here, so the notice and the Settings
    // Updates tab read the same signals instead of running competing checks
    // and disagreeing about the result.
    let _ = provide_updater_context();

    // What the board and the project rail both read about "Needs you".
    crate::components::attention::Deliveries::provide();
    crate::components::attention::BoardAttention::provide();

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

    // Validate the persisted Workspace on startup, once. Without this a
    // Workspace removed from the registry reopens as "Workspace unavailable"
    // on every launch.
    Effect::new(move |prev: Option<()>| {
        if prev.is_some() {
            return;
        }
        let workspace_id = selected_workspace.get_untracked();
        if workspace_id.is_empty() {
            return;
        }
        spawn_local(async move {
            let result = crate::services::workspace_service::get_workspace(&workspace_id)
                .await
                .map(|_| ());
            if let Err(e) = &result {
                leptos::logging::warn!("[App] Error validating workspace {}: {}", workspace_id, e);
            }
            let Some(current) = selected_workspace.try_get_untracked() else {
                return;
            };
            let next = workspace_selection_after_check(
                current.clone(),
                &workspace_id,
                result.as_ref().copied().map_err(String::as_str),
            );
            if next != current {
                set_selected_workspace.set(next);
            }
        });
    });

    // Persist selected project whenever it changes
    Effect::new(move |_| {
        let project = selected_project.get();
        persist_selected_project(&project);
    });

    // Choosing a Project leaves Workspace context. Clearing the Project (for
    // example after removing it) does not, and the first run only records the
    // value, so a restored Workspace selection survives startup.
    Effect::new(move |prev: Option<String>| {
        let project = selected_project.get();
        if let Some(prev) = prev {
            if prev != project && !project.is_empty() {
                set_selected_workspace.set(String::new());
            }
        }
        project
    });
    Effect::new(move |_| persist_selected_workspace(&selected_workspace.get()));

    let on_navigate = Callback::new({
        move |page: String| {
            persist_page(&page);
            set_current_page.set(page);
        }
    });

    // A Task the Issues page asks to open: its Project becomes the selected one,
    // the board is shown, and the board opens the Task's drawer.
    let open_task = RwSignal::new(None::<(String, uuid::Uuid)>);
    provide_context(OpenTaskRequest(open_task));
    Effect::new(move |_| {
        if let Some((project, _)) = open_task.get() {
            if selected_project.get_untracked() != project {
                set_selected_project.set(project);
            }
            on_navigate.run("dashboard".to_string());
        }
    });

    // The Ctrl/Cmd page shortcuts shown in the sidebar. The listener lives for
    // the whole session, so it is installed once, with this component.
    setup_keyboard_shortcuts(on_navigate);

    view! {
        <QuitDialog />
        // Floats over the layout rather than sitting above it: AppLayout's root
        // is `h-screen`, so a preceding sibling would push the app off-screen.
        <UpdateBanner />
        <AppLayout
            current_page=current_page
            on_navigate=on_navigate
            selected_project=selected_project
            set_selected_project=set_selected_project
            selected_workspace=selected_workspace
            set_selected_workspace=set_selected_workspace
        >
            <div class=move || if current_page.get() == "conversations" && selected_workspace.get().is_empty() { "content h-full min-h-0" } else { "content" }>
                {
                    let current_page = current_page;
                    let selected_project = selected_project;
                    move || {
                        // Workspace context shows the Workspace's own page. Project
                        // pages are not rendered here, because they belong to one Project.
                        // Global pages stay reachable, and get no Project id, so a
                        // stale selection is never shown as the current Project.
                        let page = current_page.get();
                        let in_workspace = !selected_workspace.get().is_empty();
                        if in_workspace && page != "settings" && page != "mcp" {
                            return view! {
                                <WorkspaceHome
                                    workspace_id=selected_workspace
                                    set_selected_project=set_selected_project
                                    set_selected_workspace=set_selected_workspace
                                />
                            }.into_any();
                        }
                        match page.as_str() {
                        "conversations" => view! { <Conversations project_id=selected_project.get() /> }.into_any(),
                        "dashboard" => view! { <Dashboard project_id=selected_project.get() /> }.into_any(),
                        "agent" => view! { <Agent project_id=selected_project.get() /> }.into_any(),
                        "roadmap" => view! { <Roadmap project_id=selected_project.get() /> }.into_any(),
                        "ideation" => view! { <Ideation project_id=selected_project.get() /> }.into_any(),
                        "context" => view! { <Context project_id=selected_project.get() /> }.into_any(),
                        // Read inside the Settings page, not here: this closure would
                        // otherwise rebuild Settings, and its active tab, on every Project switch.
                        "settings" => view! {
                            <Settings project_id=Signal::derive(move || {
                                if selected_workspace.get().is_empty() { selected_project.get() } else { String::new() }
                            }) />
                        }.into_any(),
                        "insights" => view! { <Insights project_id=selected_project.get() /> }.into_any(),
                        "changelog" => view! { <Changelog project_id=selected_project.get() /> }.into_any(),
                        "mcp" => view! { <McpOverview /> }.into_any(),
                        "worktrees" => view! { <Worktrees project_id=selected_project.get() on_navigate=on_navigate /> }.into_any(),
                        "github_issues" => view! { <GithubIssues project_id=selected_project.get() /> }.into_any(),
                        "github_prs" => view! { <GithubPrs project_id=selected_project.get() /> }.into_any(),
                        _ => view! { <Dashboard project_id=selected_project.get() /> }.into_any(),
                        }
                    }
                }
            </div>
        </AppLayout>
    }
}

#[cfg(test)]
mod tests {
    use super::{workspace_selection_after_check, WORKSPACE_NOT_FOUND};

    #[test]
    fn stale_persisted_workspace_is_cleared() {
        let id = "8d3c0f4e-0000-4000-8000-000000000001";
        assert_eq!(
            workspace_selection_after_check(id.to_string(), id, Err(WORKSPACE_NOT_FOUND)),
            ""
        );
    }

    #[test]
    fn transient_error_keeps_persisted_workspace() {
        let id = "8d3c0f4e-0000-4000-8000-000000000001";
        for error in ["Failed to read workspaces.toml: Resource busy", "IPC timeout"] {
            assert_eq!(workspace_selection_after_check(id.to_string(), id, Err(error)), id);
        }
        assert_eq!(workspace_selection_after_check(id.to_string(), id, Ok(())), id);
    }

    #[test]
    fn selection_changed_during_check_is_kept() {
        let stale = "8d3c0f4e-0000-4000-8000-000000000001";
        let chosen = "8d3c0f4e-0000-4000-8000-000000000002";
        assert_eq!(
            workspace_selection_after_check(chosen.to_string(), stale, Err(WORKSPACE_NOT_FOUND)),
            chosen
        );
        assert_eq!(
            workspace_selection_after_check(String::new(), stale, Err(WORKSPACE_NOT_FOUND)),
            ""
        );
    }
}
