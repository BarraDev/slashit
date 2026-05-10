use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::callback::Callback;
use crate::services::{list_projects, get_project, get_project_path, check_claude_cli};
use crate::components::toast;

#[component]
pub fn Sidebar(
    #[prop(into)] current_page: Signal<String>,
    on_navigate: Callback<String>,
    #[prop(into)] selected_project: Signal<String>,
    set_selected_project: WriteSignal<String>,
) -> impl IntoView {
    let (collapsed, set_collapsed) = signal(false);

    // Active project info
    let (project_name, set_project_name) = signal(String::new());
    let (project_path, set_project_path) = signal(String::new());

    // Claude CLI status
    let (claude_installed, set_claude_installed) = signal(false);
    let (claude_version, set_claude_version) = signal::<Option<String>>(None);

    // Check Claude CLI on mount
    Effect::new(move |prev: Option<bool>| {
        if prev.is_some() {
            return true;
        }
        spawn_local(async move {
            match check_claude_cli().await {
                Ok(status) => {
                    set_claude_installed.set(status.installed);
                    set_claude_version.set(status.version);
                }
                Err(_) => {
                    set_claude_installed.set(false);
                    set_claude_version.set(None);
                }
            }
        });
        true
    });

    // Load projects on mount and auto-select first project if none selected
    Effect::new(move |prev: Option<bool>| {
        if prev.is_some() {
            return true;
        }
        spawn_local(async move {
            match list_projects().await {
                Ok(ps) => {
                    // Auto-select first project if none selected
                    if selected_project.get().is_empty() && !ps.is_empty() {
                        set_selected_project.set(ps[0].id.to_string());
                    }
                }
                Err(e) => {
                    leptos::logging::warn!("Failed to load projects: {}", e);
                    toast::error(format!("Failed to load projects: {}", e));
                }
            }
        });
        true
    });

    // Fetch project info when selected project changes
    Effect::new(move |_| {
        let project_id = selected_project.get();
        if project_id.is_empty() {
            set_project_name.set(String::new());
            set_project_path.set(String::new());
            return;
        }
        let pid = project_id.clone();
        spawn_local(async move {
            if let Ok(Some(p)) = get_project(pid.clone()).await {
                set_project_name.set(p.name);
            }
            if let Ok(Some(path)) = get_project_path(pid).await {
                set_project_path.set(path);
            }
        });
    });



    view! {
        <aside
            data-testid="sidebar"
            class=move || format!(
                "flex flex-col border-r border-white/5 transition-all duration-300 bg-gradient-to-b from-white/[0.02] to-transparent {}",
                if collapsed.get() { "w-16" } else { "w-64" }
            )
        >
            // Header
            <div class=move || format!(
                "flex items-center border-b border-white/5 {}",
                if collapsed.get() { "flex-col gap-2 p-2" } else { "justify-between p-4" }
            )>
                {move || if collapsed.get() {
                    view! {
                        <div data-testid="sidebar-logo" class="w-8 h-8 rounded-lg bg-gradient-to-br from-blue-500 to-purple-500 flex items-center justify-center">
                            <span class="text-white font-bold text-sm">"S"</span>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div data-testid="sidebar-logo" class="flex items-center gap-3">
                            <div class="w-8 h-8 rounded-lg bg-gradient-to-br from-blue-500 to-purple-500 flex items-center justify-center">
                                <span class="text-white font-bold text-sm">"S"</span>
                            </div>
                            <div>
                                <h1 class="font-semibold text-white/90">"SlashIt"</h1>
                                <p class="text-xs text-white/30">"Workspace"</p>
                            </div>
                        </div>
                    }.into_any()
                }}
                <button
                    data-testid="sidebar-collapse"
                    aria-label=move || if collapsed.get() { "Expand sidebar" } else { "Collapse sidebar" }
                    aria-expanded=move || if collapsed.get() { "false" } else { "true" }
                    on:click=move |_| set_collapsed.update(|c| *c = !*c)
                    class="p-1.5 rounded-lg hover:bg-white/5 text-white/40 hover:text-white/60 transition-colors"
                >
                    {move || if collapsed.get() {
                        view! {
                            <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 5l7 7-7 7M5 5l7 7-7 7" />
                            </svg>
                        }.into_any()
                    } else {
                        view! {
                            <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M11 19l-7-7 7-7m8 14l-7-7 7-7" />
                            </svg>
                        }.into_any()
                    }}
                </button>
            </div>

            // Active project indicator
            {move || {
                let name = project_name.get();
                let path = project_path.get();
                if name.is_empty() {
                    view! { <div></div> }.into_any()
                } else if collapsed.get() {
                    // Collapsed: show colored dot with tooltip
                    view! {
                        <div class="px-4 py-2 border-b border-white/5" title=name.clone()>
                            <div class="w-2 h-2 rounded-full bg-yellow-400 mx-auto"></div>
                        </div>
                    }.into_any()
                } else {
                    // Expanded: show project name and path
                    let display_path = if path.len() > 30 {
                        format!("...{}", &path[path.len()-27..])
                    } else {
                        path.clone()
                    };
                    view! {
                        <div class="px-4 py-2 border-b border-white/5">
                            <div class="flex items-center gap-2">
                                <div class="w-2 h-2 rounded-full bg-yellow-400 shrink-0"></div>
                                <span class="text-sm font-medium text-yellow-400 truncate">{name}</span>
                            </div>
                            {if !path.is_empty() {
                                view! { <p class="text-xs text-white/30 truncate mt-0.5 pl-4">{display_path}</p> }.into_any()
                            } else {
                                view! { <span></span> }.into_any()
                            }}
                        </div>
                    }.into_any()
                }
            }}

            // Navigation
            <nav class=move || format!(
                "flex-1 space-y-1 overflow-y-auto {}",
                if collapsed.get() { "p-1.5" } else { "p-3" }
            )>
                <SidebarNavItem
                    page="dashboard".to_string()
                    current_page=current_page
                    on_navigate=on_navigate
                    label="Kanban".to_string()
                    shortcut="K".to_string()
                    collapsed=collapsed
                >
                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 17V7m0 10a2 2 0 01-2 2H5a2 2 0 01-2-2V7a2 2 0 012-2h2a2 2 0 012 2m0 10a2 2 0 002 2h2a2 2 0 002-2M9 7a2 2 0 012-2h2a2 2 0 012 2m0 10V7m0 10a2 2 0 002 2h2a2 2 0 002-2V7a2 2 0 00-2-2h-2a2 2 0 00-2 2" />
                    </svg>
                </SidebarNavItem>
                <SidebarNavItem
                    page="agent".to_string()
                    current_page=current_page
                    on_navigate=on_navigate
                    label="Agent Terminals".to_string()
                    shortcut="A".to_string()
                    collapsed=collapsed
                >
                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 9l3 3-3 3m5 0h3M5 20h14a2 2 0 002-2V6a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z" />
                    </svg>
                </SidebarNavItem>
                <SidebarNavItem
                    page="insights".to_string()
                    current_page=current_page
                    on_navigate=on_navigate
                    label="Insights".to_string()
                    shortcut="N".to_string()
                    collapsed=collapsed
                >
                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 19v-6a2 2 0 00-2-2H5a2 2 0 00-2 2v6a2 2 0 002 2h2a2 2 0 002-2zm0 0V9a2 2 0 012-2h2a2 2 0 012 2v10m-6 0a2 2 0 002 2h2a2 2 0 002-2m0 0V5a2 2 0 012-2h2a2 2 0 012 2v14a2 2 0 01-2 2h-2a2 2 0 01-2-2z" />
                    </svg>
                </SidebarNavItem>
                <SidebarNavItem
                    page="roadmap".to_string()
                    current_page=current_page
                    on_navigate=on_navigate
                    label="Roadmap".to_string()
                    shortcut="D".to_string()
                    collapsed=collapsed
                >
                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 20l-5.447-2.724A1 1 0 013 16.382V5.618a1 1 0 011.447-.894L9 7m0 13l6-3m-6 3V7m6 10l4.553 2.276A1 1 0 0021 18.382V7.618a1 1 0 00-.553-.894L15 4m0 13V4m0 0L9 7" />
                    </svg>
                </SidebarNavItem>
                <SidebarNavItem
                    page="context".to_string()
                    current_page=current_page
                    on_navigate=on_navigate
                    label="Context".to_string()
                    shortcut="C".to_string()
                    collapsed=collapsed
                >
                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 7v10c0 2.21 3.582 4 8 4s8-1.79 8-4V7M4 7c0 2.21 3.582 4 8 4s8-1.79 8-4M4 7c0-2.21 3.582-4 8-4s8 1.79 8 4" />
                    </svg>
                </SidebarNavItem>
                <SidebarNavItem
                    page="worktrees".to_string()
                    current_page=current_page
                    on_navigate=on_navigate
                    label="Worktrees".to_string()
                    shortcut="W".to_string()
                    collapsed=collapsed
                >
                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 11H5m14 0a2 2 0 012 2v6a2 2 0 01-2 2H5a2 2 0 01-2-2v-6a2 2 0 012-2m14 0V9a2 2 0 00-2-2M5 11V9a2 2 0 012-2m0 0V5a2 2 0 012-2h6a2 2 0 012 2v2M7 7h10" />
                    </svg>
                </SidebarNavItem>
                <SidebarNavItem
                    page="github_issues".to_string()
                    current_page=current_page
                    on_navigate=on_navigate
                    label="GitHub Issues".to_string()
                    shortcut="G".to_string()
                    collapsed=collapsed
                >
                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
                    </svg>
                </SidebarNavItem>
                <SidebarNavItem
                    page="github_prs".to_string()
                    current_page=current_page
                    on_navigate=on_navigate
                    label="GitHub PRs".to_string()
                    shortcut="P".to_string()
                    collapsed=collapsed
                >
                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 7h12m0 0l-4-4m4 4l-4 4m0 6H4m0 0l4 4m-4-4l4-4" />
                    </svg>
                </SidebarNavItem>
                <SidebarNavItem
                    page="settings".to_string()
                    current_page=current_page
                    on_navigate=on_navigate
                    label="Settings".to_string()
                    shortcut="⌘,".to_string()
                    collapsed=collapsed
                >
                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
                    </svg>
                </SidebarNavItem>

                // Labs section separator
                {move || if !collapsed.get() {
                    view! {
                        <div class="mt-4 mb-2 px-3">
                            <div class="flex items-center gap-2">
                                <div class="flex-1 h-px bg-white/5"></div>
                                <span class="text-[10px] uppercase tracking-wider text-white/20 font-medium">"Labs"</span>
                                <div class="flex-1 h-px bg-white/5"></div>
                            </div>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="my-2 mx-2 h-px bg-white/5"></div>
                    }.into_any()
                }}
                <SidebarNavItem
                    page="ideation".to_string()
                    current_page=current_page
                    on_navigate=on_navigate
                    label="Ideation".to_string()
                    shortcut="".to_string()
                    collapsed=collapsed
                    stub=true
                >
                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9.663 17h4.673M12 3v1m6.364 1.636l-.707.707M21 12h-1M4 12H3m3.343-5.657l-.707-.707m2.828 9.9a5 5 0 117.072 0l-.548.547A3.374 3.374 0 0014 18.469V19a2 2 0 11-4 0v-.531c0-.895-.356-1.754-.988-2.386l-.548-.547z" />
                    </svg>
                </SidebarNavItem>
                <SidebarNavItem
                    page="changelog".to_string()
                    current_page=current_page
                    on_navigate=on_navigate
                    label="Changelog".to_string()
                    shortcut="".to_string()
                    collapsed=collapsed
                    stub=true
                >
                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
                    </svg>
                </SidebarNavItem>
                <SidebarNavItem
                    page="mcp".to_string()
                    current_page=current_page
                    on_navigate=on_navigate
                    label="MCP Overview".to_string()
                    shortcut="".to_string()
                    collapsed=collapsed
                    stub=true
                >
                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 12h14M5 12a2 2 0 01-2-2V6a2 2 0 012-2h14a2 2 0 012 2v4a2 2 0 01-2 2M5 12a2 2 0 00-2 2v4a2 2 0 002 2h14a2 2 0 002-2v-4a2 2 0 00-2-2m-2-4h.01M17 16h.01" />
                    </svg>
                </SidebarNavItem>
            </nav>

            // Claude CLI status
            <div class="px-4 py-2 border-t border-white/5">
                {move || if collapsed.get() {
                    view! {
                        <div
                            class="flex justify-center"
                            title=move || if claude_installed.get() {
                                claude_version.get().unwrap_or_default()
                            } else {
                                "Claude CLI not found".to_string()
                            }
                        >
                            <div class=move || format!(
                                "w-2 h-2 rounded-full {}",
                                if claude_installed.get() { "bg-emerald-400" } else { "bg-red-400" }
                            )></div>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="flex items-center gap-2">
                            <div class=move || format!(
                                "w-2 h-2 rounded-full shrink-0 {}",
                                if claude_installed.get() { "bg-emerald-400" } else { "bg-red-400" }
                            )></div>
                            <span class="text-xs text-white/40 truncate">
                                {move || if claude_installed.get() {
                                    claude_version.get().map(|v| {
                                        // Extract just version number if it contains extra text
                                        if v.starts_with("claude") || v.starts_with("Claude") {
                                            v
                                        } else {
                                            format!("Claude {}", v)
                                        }
                                    }).unwrap_or_else(|| "Claude CLI".to_string())
                                } else {
                                    "Claude CLI not found".to_string()
                                }}
                            </span>
                        </div>
                    }.into_any()
                }}
            </div>

        </aside>
    }
}

#[component]
fn SidebarNavItem(
    page: String,
    current_page: Signal<String>,
    on_navigate: Callback<String>,
    label: String,
    shortcut: String,
    collapsed: ReadSignal<bool>,
    #[prop(default = false)] stub: bool,
    children: Children,
) -> impl IntoView {
    let page_clone = page.clone();
    let page_for_click = page.clone();
    let page_for_testid = page.clone();
    let label_clone = label.clone();
    let label_for_title = label.clone();
    let shortcut_clone = shortcut.clone();

    let is_active = Memo::new(move |_| current_page.get() == page_clone);

    view! {
        <button
            data-testid=format!("nav-{}", page_for_testid)
            on:click=move |_| on_navigate.run(page_for_click.clone())
            class=move || format!(
                "flex items-center rounded-lg transition-all duration-200 group relative w-full {} {} {}",
                if collapsed.get() { "justify-center py-2.5 px-0" } else { "gap-3 px-3 py-2.5 text-left" },
                if is_active.get() {
                    if stub { "bg-white/5 text-white/60 border-l-2 border-white/30" } else { "bg-yellow-500/10 text-white border-l-2 border-yellow-500" }
                } else if stub {
                    "text-white/30 hover:text-white/50 hover:bg-white/[0.02]"
                } else {
                    "text-white/50 hover:text-white/70 hover:bg-white/[0.02]"
                },
                if stub { "text-xs" } else { "" }
            )
            title=move || if collapsed.get() { label_for_title.clone() } else { String::new() }
        >
            {children()}

            {move || if !collapsed.get() {
                let show_shortcut = !stub && !shortcut_clone.is_empty();
                view! {
                    <div class="flex-1">
                        <span class=if stub { "text-xs font-medium" } else { "text-sm font-medium" }>{label_clone.clone()}</span>
                    </div>
                    {show_shortcut.then(|| view! {
                        <span class="text-xs text-white/30 opacity-0 group-hover:opacity-100 transition-opacity">{shortcut_clone.clone()}</span>
                    })}
                }.into_any()
            } else {
                ().into_any()
            }}
        </button>
    }
}
