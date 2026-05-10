use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::callback::Callback;
use crate::models::Project;
use crate::services::list_projects;
use crate::components::CreateProjectModal;

/// A tab representing an open project in the top bar
#[derive(Clone, PartialEq)]
struct ProjectTab {
    id: String,
    name: String,
    path: Option<String>,
}

/// Persist open tabs to localStorage
fn get_persisted_tabs() -> Vec<String> {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            if let Ok(Some(tabs)) = storage.get_item("slashit_open_tabs") {
                return serde_json::from_str(&tabs).unwrap_or_default();
            }
        }
    }
    Vec::new()
}

fn persist_tabs(tabs: &[String]) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            if let Ok(json) = serde_json::to_string(tabs) {
                let _ = storage.set_item("slashit_open_tabs", &json);
            }
        }
    }
}

/// Project tabs component for viewing and switching between open projects.
/// This is separate from the sidebar - the sidebar lists ALL projects,
/// while tabs show only currently OPEN projects that the user is working on.
#[component]
pub fn ProjectTabs(
    #[prop(into)] selected_project: Signal<String>,
    set_selected_project: WriteSignal<String>,
) -> impl IntoView {
    let (tabs, set_tabs) = signal(Vec::<ProjectTab>::new());
    let (all_projects, set_all_projects) = signal(Vec::<Project>::new());
    let (loading, set_loading) = signal(true);
    let (show_create_modal, set_show_create_modal) = signal(false);
    let (refresh_trigger, set_refresh_trigger) = signal(0u32);

    // Load projects and restore tabs on mount (also reload when refresh_trigger changes)
    Effect::new(move |prev_trigger: Option<u32>| {
        let current_trigger = refresh_trigger.get();
        // Skip if this is not the first run AND the trigger hasn't changed
        if let Some(prev) = prev_trigger {
            if prev == current_trigger {
                return current_trigger;
            }
        }
        spawn_local(async move {
            match list_projects().await {
                Ok(projects) => {
                    set_all_projects.set(projects.clone());
                    set_loading.set(false);
                    
                    // Restore persisted tabs
                    let persisted = get_persisted_tabs();
                    let restored_tabs: Vec<ProjectTab> = projects.iter()
                        .filter(|p| persisted.contains(&p.id.to_string()))
                        .map(|p| ProjectTab {
                            id: p.id.to_string(),
                            name: p.name.clone(),
                            path: p.repository_id.map(|r| r.to_string()),
                        })
                        .collect();
                    
                    if !restored_tabs.is_empty() {
                        set_tabs.set(restored_tabs);
                    }
                }
                Err(e) => {
                    leptos::logging::warn!("Failed to load projects: {}", e);
                    set_loading.set(false);
                }
            }
        });
        current_trigger
    });

    // Sync tabs with selected project - add to tabs if not already present
    Effect::new(move |_| {
        let selected = selected_project.get();
        if !selected.is_empty() {
            // Add to tabs if not already present
            let current_tabs = tabs.get();
            if !current_tabs.iter().any(|t| t.id == selected) {
                // Find project info from all projects
                let projects = all_projects.get();
                if let Some(project) = projects.iter().find(|p| p.id.to_string() == selected) {
                    let new_tab = ProjectTab {
                        id: project.id.to_string(),
                        name: project.name.clone(),
                        path: project.repository_id.map(|r| r.to_string()),
                    };
                    set_tabs.update(|t| {
                        t.push(new_tab);
                    });
                    // Persist updated tabs
                    let tab_ids: Vec<String> = tabs.get().iter().map(|t| t.id.clone()).collect();
                    persist_tabs(&tab_ids);
                }
            }
        }
    });

    // Also update all_projects when a new project is created (listen for changes)
    Effect::new(move |prev_len: Option<usize>| {
        let selected = selected_project.get();
        let projects = all_projects.get();
        let current_len = projects.len();
        
        // If a new project was added and it's selected but we don't have it in tabs
        if let Some(prev) = prev_len {
            if current_len > prev && !selected.is_empty() {
                // Reload projects to get the new one
                spawn_local(async move {
                    if let Ok(new_projects) = list_projects().await {
                        set_all_projects.set(new_projects);
                    }
                });
            }
        }
        
        current_len
    });

    // Close a tab - removes from view but doesn't delete the project
    let close_tab = move |tab_id: String| {
        let current_selected = selected_project.get();
        let current_tabs = tabs.get();
        
        // If we're closing the active tab, switch to another tab
        if tab_id == current_selected {
            // Find another tab to switch to
            if let Some(other_tab) = current_tabs.iter().find(|t| t.id != tab_id) {
                set_selected_project.set(other_tab.id.clone());
            } else {
                // No other tabs, clear selection
                set_selected_project.set(String::new());
            }
        }
        
        set_tabs.update(|t| t.retain(|tab| tab.id != tab_id));
        let tab_ids: Vec<String> = tabs.get().iter().map(|t| t.id.clone()).collect();
        persist_tabs(&tab_ids);
    };

    // Callback when a new project is created
    let on_project_created = Callback::new(move |project: Project| {
        let project_id = project.id.to_string();
        let project_name = project.name.clone();
        
        // Add to tabs
        let new_tab = ProjectTab {
            id: project_id.clone(),
            name: project_name,
            path: project.repository_id.map(|r| r.to_string()),
        };
        set_tabs.update(|t| t.push(new_tab));
        
        // Select the new project
        set_selected_project.set(project_id.clone());
        
        // Persist tabs
        let tab_ids: Vec<String> = tabs.get().iter().map(|t| t.id.clone()).collect();
        persist_tabs(&tab_ids);
        
        // Refresh project list
        set_refresh_trigger.update(|t| *t += 1);
    });

    view! {
        <div data-testid="project-tabs" class="flex items-center gap-1 px-4 py-2 border-b border-white/5 bg-white/[0.01]">
            // Tabs
            <div class="flex items-center gap-1 overflow-x-auto flex-1">
                {move || {
                    if loading.get() {
                        return view! {
                            <div class="text-sm text-white/30 px-4 flex items-center gap-2">
                                <svg class="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24">
                                    <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
                                    <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
                                </svg>
                                "Loading..."
                            </div>
                        }.into_any();
                    }
                    
                    let current_active = selected_project.get();
                    let tab_views = tabs.get().into_iter().map(|tab| {
                        let tab_id = tab.id.clone();
                        let tab_id_for_click = tab.id.clone();
                        let tab_id_for_close = tab.id.clone();
                        let tab_id_for_testid = tab.id.clone();
                        let tab_name = tab.name.clone();
                        let tab_path = tab.path.clone();
                        let is_active = tab_id == current_active;
                        
                        view! {
                            <div
                                data-testid=format!("project-tab-{}", tab_id_for_testid)
                                class=move || format!(
                                    "group relative flex items-center gap-2 px-4 py-2 rounded-lg transition-all cursor-pointer {}",
                                    if is_active {
                                        "bg-white/10 text-white border border-yellow-500/50"
                                    } else {
                                        "text-white/50 hover:text-white/70 hover:bg-white/[0.02]"
                                    }
                                )
                                on:click=move |_| set_selected_project.set(tab_id_for_click.clone())
                                title=move || tab_path.clone().unwrap_or_default()
                            >
                                {is_active.then(|| view! {
                                    <div class="absolute bottom-0 left-0 right-0 h-0.5 bg-yellow-500 rounded-full"></div>
                                })}

                                <svg class="w-4 h-4 text-yellow-500/70" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                                </svg>

                                <span class="text-sm font-medium truncate max-w-[120px]">{tab_name}</span>
                                
                                {is_active.then(|| view! {
                                    <span class="w-2 h-2 rounded-full bg-yellow-500 animate-pulse"></span>
                                })}

                                <button
                                    data-testid="close-tab-button"
                                    aria-label="Close tab"
                                    on:click=move |ev: web_sys::MouseEvent| {
                                        ev.stop_propagation();
                                        close_tab(tab_id_for_close.clone());
                                    }
                                    class="opacity-0 group-hover:opacity-100 p-0.5 rounded hover:bg-white/10 transition-all ml-1"
                                >
                                    <svg class="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                                    </svg>
                                </button>
                            </div>
                        }
                    }).collect::<Vec<_>>();
                    
                    view! { <>{tab_views}</> }.into_any()
                }}

                // Empty state
                {move || {
                    if tabs.get().is_empty() && !loading.get() {
                        view! {
                            <div class="text-sm text-white/30 px-4 flex items-center gap-2">
                                <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 7l5 5m0 0l-5 5m5-5H6" />
                                </svg>
                                "Select a project to get started"
                            </div>
                        }.into_any()
                    } else {
                        ().into_any()
                    }
                }}
            </div>

            // Right side - add project and refresh buttons
            <div class="flex items-center gap-1 ml-auto">
                // Add project button
                <button
                    data-testid="add-project-button"
                    aria-label="Create new project"
                    on:click=move |_| set_show_create_modal.set(true)
                    class="p-2 rounded-lg text-white/40 hover:text-white/60 hover:bg-white/5 transition-colors"
                    title="Create new project"
                >
                    <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 4v16m8-8H4" />
                    </svg>
                </button>
                
                // Refresh button
                <button
                    data-testid="refresh-projects-button"
                    aria-label="Refresh projects"
                    on:click=move |_| {
                        set_refresh_trigger.update(|t| *t += 1);
                    }
                    class="p-2 rounded-lg text-white/40 hover:text-white/60 hover:bg-white/5 transition-colors"
                    title="Refresh projects"
                >
                    <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
                    </svg>
                </button>
            </div>
            
            // Create project modal
            <CreateProjectModal
                show=show_create_modal
                set_show=set_show_create_modal
                on_project_created=on_project_created
            />
        </div>
    }
}
