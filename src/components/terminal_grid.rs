use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::callback::Callback;
use super::terminal::RealTerminal;
use crate::services::{pty_service, get_project_path};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// Helper to get value from input event
fn event_target_value(ev: &web_sys::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|i| i.value())
        .unwrap_or_default()
}

#[derive(Clone, Debug)]
pub struct TerminalSession {
    pub id: String,
    pub name: String,
    pub is_new: bool,
    pub working_directory: Option<String>,
    pub project_id: Option<String>,
}

#[component]
pub fn TerminalGrid(
    #[prop(default = String::new())] project_id: String,
) -> impl IntoView {
    // React to navigation (even if Leptos keeps this component alive between page switches)
    let current_page = use_context::<ReadSignal<String>>();
    // Get the selected project from context (fallback to prop)
    let selected_project = use_context::<ReadSignal<String>>();
    let (current_project_id, set_current_project_id) = signal(project_id);

    // All terminals (across all projects)
    let (all_terminals, set_all_terminals) = signal(Vec::<TerminalSession>::new());
    // Filtered terminals for current project
    let terminals = Signal::derive(move || {
        let pid = current_project_id.get();
        if pid.is_empty() {
            // No project selected — show all
            all_terminals.get()
        } else {
            // Show only terminals that belong to this project
            all_terminals.get().into_iter()
                .filter(|t| t.project_id.as_deref() == Some(pid.as_str()))
                .collect()
        }
    });
    let set_terminals = set_all_terminals;
    let (show_files, set_show_files) = signal(false);
    let (loading, set_loading) = signal(true);
    let (error, set_error) = signal(None::<String>);
    let (next_terminal_num, set_next_terminal_num) = signal(1u32);
    let (project_path, set_project_path) = signal(None::<String>);
    
    // Invoke All dropdown state
    let (show_invoke_dropdown, set_show_invoke_dropdown) = signal(false);
    let (custom_command, set_custom_command) = signal(String::new());
    
    // Click-outside handler to close dropdown - add listener ONCE on mount
    Effect::new(move |prev: Option<bool>| {
        if prev.is_some() {
            return true; // Only run on first mount
        }
        
        // Add click listener to document once
        let closure = Closure::<dyn Fn(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            // Only process if dropdown is open
            if !show_invoke_dropdown.get() {
                return;
            }
            
            if let Some(target) = ev.target() {
                if let Some(element) = target.dyn_ref::<web_sys::Element>() {
                    // Check if click is outside the dropdown container
                    let is_inside = element.closest(".invoke-dropdown-container").ok().flatten().is_some();
                    if !is_inside {
                        set_show_invoke_dropdown.set(false);
                    }
                }
            }
        });
        
        if let Some(window) = web_sys::window() {
            if let Some(document) = window.document() {
                let _ = document.add_event_listener_with_callback(
                    "click",
                    closure.as_ref().unchecked_ref(),
                );
                // Keep the closure alive for the lifetime of the page
                closure.forget();
            }
        }
        
        true
    });

    // Shared loader so we can refresh the terminal list when navigating back to this panel.
    // Guard: track whether we've already started loading to prevent duplicate spawns
    let (has_loaded, set_has_loaded) = signal(false);

    // Load sessions once on mount
    {
        Effect::new(move |prev: Option<bool>| {
            if prev.is_some() || has_loaded.get_untracked() {
                return true;
            }
            set_has_loaded.set(true);
            leptos::logging::log!("[TerminalGrid] Loading existing PTY sessions...");

            spawn_local(async move {
                match pty_service::list_pty_sessions().await {
                    Ok(sessions) => {
                        leptos::logging::log!("[TerminalGrid] Found {} existing sessions", sessions.len());

                        let mut max_num = 0u32;
                        let term_sessions: Vec<TerminalSession> = sessions
                            .into_iter()
                            .map(|info| {
                                if let Some(num_str) = info.name.strip_prefix("Terminal ") {
                                    if let Ok(num) = num_str.parse::<u32>() {
                                        max_num = max_num.max(num);
                                    }
                                }
                                TerminalSession {
                                    id: info.id,
                                    name: info.name,
                                    is_new: false,
                                    working_directory: None,
                                    project_id: info.project_id,
                                }
                            })
                            .collect();

                        set_next_terminal_num.set(max_num + 1);
                        set_terminals.set(term_sessions);
                        set_loading.set(false);
                    }
                    Err(e) => {
                        leptos::logging::warn!("[TerminalGrid] Failed to load sessions: {}", e);
                        set_error.set(Some(e));
                        set_loading.set(false);
                    }
                }
            });
            true
        });
    }
    
    // Load the project path when selected project changes + update filter
    Effect::new(move |prev_project: Option<String>| {
        let current_project = selected_project.map(|s| s.get()).unwrap_or_default();

        // Update the filter signal
        set_current_project_id.set(current_project.clone());

        // Only reload path if the project actually changed
        if prev_project.as_ref() != Some(&current_project) && !current_project.is_empty() {
            let project_id = current_project.clone();
            spawn_local(async move {
                match get_project_path(project_id).await {
                    Ok(path) => {
                        set_project_path.set(path);
                    }
                    Err(_e) => {
                        set_project_path.set(None);
                    }
                }
            });
        } else if current_project.is_empty() {
            set_project_path.set(None);
        }
        
        current_project
    });

    // Calculate grid layout based on terminal count
    let grid_class = move || {
        let count = terminals.get().len();
        match count {
            0 | 1 => "grid-cols-1 grid-rows-1",
            2 => "grid-cols-1 md:grid-cols-2 grid-rows-1",
            3 => "grid-cols-1 md:grid-cols-2 lg:grid-cols-3 grid-rows-1",
            4 => "grid-cols-2 grid-rows-2",
            5 | 6 => "grid-cols-2 lg:grid-cols-3 grid-rows-2",
            _ => "grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 grid-rows-2",
        }
    };

    // Calculate terminal height based on count - single terminal fills all space
    let terminal_height_class = move || {
        let count = terminals.get().len();
        match count {
            1 => "h-full min-h-0", // Single terminal fills everything
            2 | 3 => "h-full min-h-[200px]",
            _ => "h-full min-h-[200px]",
        }
    };

    // Create new terminal handler - fetches project path if needed to avoid race condition
    let create_terminal = move |_| {
        let current_count = terminals.get().len();
        if current_count >= 12 {
            return;
        }
        
        let name_id = next_terminal_num.get();
        set_next_terminal_num.update(|n| *n += 1);
        
        // Get the current project path, or fetch it if we have a project selected but no path yet
        let cached_path = project_path.get();
        let current_project_id = selected_project.map(|s| s.get()).unwrap_or_default();
        
        if cached_path.is_some() || current_project_id.is_empty() {
            // We have the path cached or no project selected - create terminal immediately
            let pid = current_project_id.clone();
            set_terminals.update(|terms| {
                terms.push(TerminalSession {
                    id: String::new(),
                    name: format!("Terminal {}", name_id),
                    is_new: true,
                    working_directory: cached_path,
                    project_id: if pid.is_empty() { None } else { Some(pid) },
                });
            });
        } else {
            // Project selected but path not cached - fetch it first, then create terminal
            let project_id = current_project_id.clone();
            let pid_for_session = if project_id.is_empty() { None } else { Some(project_id.clone()) };
            spawn_local(async move {
                let wd = match get_project_path(project_id).await {
                    Ok(path) => {
                        set_project_path.set(path.clone());
                        path
                    }
                    Err(_) => None,
                };

                set_terminals.update(|terms| {
                    terms.push(TerminalSession {
                        id: String::new(),
                        name: format!("Terminal {}", name_id),
                        is_new: true,
                        working_directory: wd,
                        project_id: pid_for_session,
                    });
                });
            });
        }
    };

    view! {
        <div data-testid="terminal-grid" class="flex flex-col h-full gap-4">
            <div class="flex items-center justify-between flex-shrink-0">
                <div>
                    <h2 class="text-lg font-semibold text-white/90">"Agent Terminals"</h2>
                    <p class="text-sm text-white/40">"AI agent execution environments"</p>
                </div>

                <div class="flex items-center gap-3">
                    <span class="text-sm text-white/50">
                        {move || format!("{} / 12", terminals.get().len())}
                    </span>

                    <div class="relative invoke-dropdown-container">
                        <button
                            data-testid="invoke-all-button"
                            aria-expanded=move || if show_invoke_dropdown.get() { "true" } else { "false" }
                            aria-haspopup="true"
                            on:click=move |ev: web_sys::MouseEvent| {
                                ev.stop_propagation(); // Prevent document click handler from closing immediately
                                set_show_invoke_dropdown.update(|v| *v = !*v);
                            }
                            class="px-4 py-2 rounded-lg bg-yellow-500 hover:bg-yellow-600 text-black font-medium text-sm transition-colors flex items-center gap-2"
                        >
                            "Invoke All"
                            <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 9l-7 7-7-7" />
                            </svg>
                        </button>
                        
                        <Show when=move || show_invoke_dropdown.get()>
                            <div class="absolute right-0 mt-2 w-64 bg-[#1a1a1a] border border-white/10 rounded-lg shadow-xl z-50">
                                <div class="p-2">
                                    <button
                                        on:click=move |_| {
                                            set_show_invoke_dropdown.set(false);
                                            spawn_local(async move {
                                                match pty_service::invoke_all_terminals("codebuff").await {
                                                    Ok(count) => leptos::logging::log!("[TerminalGrid] Invoked codebuff on {} terminals", count),
                                                    Err(e) => leptos::logging::error!("[TerminalGrid] Failed to invoke codebuff: {}", e),
                                                }
                                            });
                                        }
                                        class="w-full text-left px-3 py-2 rounded hover:bg-white/10 text-white/90 text-sm flex items-center gap-2"
                                    >
                                        <span class="w-2 h-2 rounded-full bg-green-500"></span>
                                        "codebuff"
                                    </button>
                                    <button
                                        on:click=move |_| {
                                            set_show_invoke_dropdown.set(false);
                                            spawn_local(async move {
                                                match pty_service::invoke_all_terminals("claude").await {
                                                    Ok(count) => leptos::logging::log!("[TerminalGrid] Invoked claude on {} terminals", count),
                                                    Err(e) => leptos::logging::error!("[TerminalGrid] Failed to invoke claude: {}", e),
                                                }
                                            });
                                        }
                                        class="w-full text-left px-3 py-2 rounded hover:bg-white/10 text-white/90 text-sm flex items-center gap-2"
                                    >
                                        <span class="w-2 h-2 rounded-full bg-purple-500"></span>
                                        "claude"
                                    </button>
                                    <div class="border-t border-white/10 my-2"></div>
                                    <div class="px-3 py-2">
                                        <label class="text-xs text-white/50 mb-1 block">"Custom command"</label>
                                        <div class="flex gap-2">
                                            <input
                                                type="text"
                                                placeholder="Enter command..."
                                                prop:value=move || custom_command.get()
                                                on:input=move |ev: web_sys::Event| {
                                                    let value = event_target_value(&ev);
                                                    set_custom_command.set(value);
                                                }
                                                on:keydown=move |ev: web_sys::KeyboardEvent| {
                                                    if ev.key() == "Enter" {
                                                        let cmd = custom_command.get();
                                                        if !cmd.is_empty() {
                                                            set_show_invoke_dropdown.set(false);
                                                            spawn_local(async move {
                                                                match pty_service::invoke_all_terminals(&cmd).await {
                                                                    Ok(count) => leptos::logging::log!("[TerminalGrid] Invoked '{}' on {} terminals", cmd, count),
                                                                    Err(e) => leptos::logging::error!("[TerminalGrid] Failed to invoke '{}': {}", cmd, e),
                                                                }
                                                            });
                                                            set_custom_command.set(String::new());
                                                        }
                                                    }
                                                }
                                                class="flex-1 px-2 py-1 bg-black/50 border border-white/10 rounded text-sm text-white/90 placeholder-white/30 focus:outline-none focus:border-yellow-500/50"
                                            />
                                            <button
                                                on:click=move |_| {
                                                    let cmd = custom_command.get();
                                                    if !cmd.is_empty() {
                                                        set_show_invoke_dropdown.set(false);
                                                        spawn_local(async move {
                                                            match pty_service::invoke_all_terminals(&cmd).await {
                                                                Ok(count) => leptos::logging::log!("[TerminalGrid] Invoked '{}' on {} terminals", cmd, count),
                                                                Err(e) => leptos::logging::error!("[TerminalGrid] Failed to invoke '{}': {}", cmd, e),
                                                            }
                                                        });
                                                        set_custom_command.set(String::new());
                                                    }
                                                }
                                                class="px-2 py-1 bg-yellow-500 hover:bg-yellow-600 text-black rounded text-sm font-medium"
                                            >
                                                "Run"
                                            </button>
                                        </div>
                                    </div>
                                </div>
                            </div>
                        </Show>
                    </div>

                    <button
                        data-testid="new-terminal-button"
                        aria-label="New terminal"
                        on:click=create_terminal
                        class="px-4 py-2 rounded-lg bg-white/10 hover:bg-white/20 text-white/80 text-sm transition-colors flex items-center gap-2"
                    >
                        <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 4v16m8-8H4" />
                        </svg>
                        "New Terminal"
                    </button>

                    <button
                        data-testid="files-toggle-button"
                        aria-label=move || if show_files.get() { "Hide file browser" } else { "Show file browser" }
                        aria-pressed=move || if show_files.get() { "true" } else { "false" }
                        on:click=move |_| set_show_files.update(|v| *v = !*v)
                        class=move || {
                            if show_files.get() {
                                "px-4 py-2 rounded-lg bg-yellow-500/20 text-yellow-400 border border-yellow-500 text-sm flex items-center gap-2 transition-colors"
                            } else {
                                "px-4 py-2 rounded-lg bg-white/5 text-white/70 hover:bg-white/10 border border-white/10 text-sm flex items-center gap-2 transition-colors"
                            }
                        }
                    >
                        <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                        </svg>
                        "Files"
                    </button>
                </div>
            </div>

            <div class="flex gap-4 flex-1 min-h-0 overflow-hidden">
                <div class="flex-1 min-h-0 overflow-hidden">
                    {move || {
                        if loading.get() {
                            view! {
                                <div class="flex items-center justify-center h-full">
                                    <div class="flex flex-col items-center gap-3">
                                        <svg class="w-8 h-8 animate-spin text-yellow-500" fill="none" viewBox="0 0 24 24">
                                            <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
                                            <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
                                        </svg>
                                        <span class="text-white/50 text-sm">"Loading terminal sessions..."</span>
                                    </div>
                                </div>
                            }.into_any()
                        } else if let Some(err) = error.get() {
                            view! {
                                <div class="flex items-center justify-center h-full">
                                    <div class="text-center">
                                        <div class="text-red-400 mb-2">"Error loading sessions"</div>
                                        <div class="text-white/40 text-sm mb-4">{err}</div>
                                        <button
                                            on:click=create_terminal
                                            class="px-6 py-3 rounded-lg bg-yellow-500 hover:bg-yellow-600 text-black font-medium transition-colors"
                                        >
                                            "Create New Terminal"
                                        </button>
                                    </div>
                                </div>
                            }.into_any()
                        } else {
                            let terms = terminals.get();
                            if terms.is_empty() {
                                view! {
                                    <div class="flex items-center justify-center h-full">
                                        <div class="text-center">
                                            <div class="text-white/50 mb-4">
                                                <svg class="w-16 h-16 mx-auto mb-3 opacity-50" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M8 9l3 3-3 3m5 0h3M5 20h14a2 2 0 002-2V6a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z" />
                                                </svg>
                                                <div class="text-lg mb-1">"No terminals open"</div>
                                                <div class="text-sm text-white/30">"Click the button below to create a terminal session"</div>
                                            </div>
                                            <button
                                                on:click=create_terminal
                                                class="px-6 py-3 rounded-lg bg-yellow-500 hover:bg-yellow-600 text-black font-medium transition-colors flex items-center gap-2 mx-auto"
                                            >
                                                <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 4v16m8-8H4" />
                                                </svg>
                                                "Create Terminal"
                                            </button>
                                        </div>
                                    </div>
                                }.into_any()
                            } else {
                                let height_class = terminal_height_class();
                                let g_class = grid_class();
                                view! {
                                    <div class=format!("grid gap-4 h-full {}", g_class)>
                                        {terms.into_iter().map(|terminal| {
                                            let term_id = terminal.id.clone();
                                            let term_name = terminal.name.clone();
                                            let term_name_for_close = term_name.clone();
                                            let term_name_for_remove = term_name.clone();
                                            let term_name_for_view = term_name.clone();
                                            let term_name_for_testid = term_name.clone();
                                            let height = height_class.to_string();
                                            let session_id = if terminal.id.is_empty() { None } else { Some(terminal.id.clone()) };
                                            let term_working_dir = terminal.working_directory.clone();
                                            
                                            let on_session_created = {
                                                let term_id_for_close = term_id.clone();
                                                Callback::new(move |new_session_id: Option<String>| {
                                                    // If we got a new session ID, update the terminal entry
                                                    if let Some(new_id) = new_session_id.clone() {
                                                        let name_match = term_name_for_close.clone();
                                                        set_terminals.update(|terms| {
                                                            if let Some(term) = terms.iter_mut().find(|t| t.id == term_id_for_close || (t.id.is_empty() && t.name == name_match)) {
                                                                term.id = new_id;
                                                                term.is_new = false;
                                                            }
                                                        });
                                                    }
                                                })
                                            };
                                            
                                            let on_close = {
                                                let term_id_for_remove = term_id.clone();
                                                Callback::new(move |()| {
                                                    let name_match = term_name_for_remove.clone();
                                                    set_terminals.update(|terms| {
                                                        terms.retain(|t| !(t.id == term_id_for_remove || (t.id.is_empty() && t.name == name_match)));
                                                    });
                                                })
                                            };
                                            
                                            view! {
                                                <div data-testid=format!("terminal-{}", term_name_for_testid) class=format!("overflow-hidden {}", height)>
                                                    <RealTerminal 
                                                        name=term_name_for_view
                                                        session_id=session_id
                                                        working_directory=term_working_dir
                                                        on_session_created=on_session_created
                                                        on_close=on_close
                                                    />
                                                </div>
                                            }
                                        }).collect::<Vec<_>>()}
                                    </div>
                                }.into_any()
                            }
                        }
                    }}
                </div>

                <Show when=move || show_files.get()>
                    <div class="w-72 border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden flex-shrink-0">
                        <div class="p-4 border-b border-white/10">
                            <h3 class="font-medium text-white/90">"Project Files"</h3>
                            <p class="text-xs text-white/50 mt-1">"Drag files into a terminal"</p>
                        </div>
                        <FileExplorer />
                    </div>
                </Show>
            </div>
        </div>
    }
}

#[component]
fn FileExplorer() -> impl IntoView {
    let (expanded, set_expanded) = signal(vec!["src".to_string()]);

    let files = vec![
        ("src/main.rs", false),
        ("src/app.rs", false),
        ("src/components", true),
        ("src/pages", true),
        ("src-tauri", true),
        ("Cargo.toml", false),
    ];

    view! {
        <div class="p-2 space-y-0.5 max-h-[400px] overflow-auto">
            {files.into_iter().map(|(path, is_dir)| {
                let path_str = path.to_string();
                let path_for_click = path_str.clone();
                let path_for_check = path_str.clone();
                let name = path.split('/').next_back().unwrap_or(path).to_string();
                let is_expanded = move || expanded.get().contains(&path_for_check);

                view! {
                    <div
                        class=move || {
                            if is_dir {
                                "flex items-center gap-2 px-2 py-1.5 rounded cursor-pointer hover:bg-white/5 text-white/70"
                            } else {
                                "flex items-center gap-2 px-2 py-1.5 rounded cursor-pointer hover:bg-white/5 text-white/50"
                            }
                        }
                        on:click=move |_| {
                            if is_dir {
                                let path = path_for_click.clone();
                                set_expanded.update(|e| {
                                    if e.contains(&path) {
                                        e.retain(|p| p != &path);
                                    } else {
                                        e.push(path);
                                    }
                                });
                            }
                        }
                    >
                        {if is_dir {
                            view! {
                                <svg 
                                    class=move || format!("w-3 h-3 transition-transform {}", if is_expanded() { "rotate-90" } else { "" })
                                    fill="none" 
                                    viewBox="0 0 24 24" 
                                    stroke="currentColor"
                                >
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7" />
                                </svg>
                            }.into_any()
                        } else {
                            view! { <div class="w-3"></div> }.into_any()
                        }}

                        <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            {if is_dir {
                                view! {
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                                }.into_any()
                            } else {
                                view! {
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
                                }.into_any()
                            }}
                        </svg>

                        <span class="text-sm truncate">{name}</span>
                    </div>
                }
            }).collect::<Vec<_>>()}
        </div>
    }
}
