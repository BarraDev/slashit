use crate::models::github::GithubIssue;
use crate::services::github_service;
use leptos::prelude::*;
use leptos::task::spawn_local;

pub fn format_relative_time(dt: &chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(*dt);

    if duration.num_minutes() < 1 {
        "just now".to_string()
    } else if duration.num_minutes() < 60 {
        format!("{}m ago", duration.num_minutes())
    } else if duration.num_hours() < 24 {
        format!("{}h ago", duration.num_hours())
    } else if duration.num_days() < 30 {
        format!("{}d ago", duration.num_days())
    } else {
        format!("{}mo ago", duration.num_days() / 30)
    }
}

fn get_persisted_repo() -> String {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            if let Ok(Some(repo)) = storage.get_item("slashit_github_repo") {
                return repo;
            }
        }
    }
    String::new()
}

fn persist_repo(repo: &str) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item("slashit_github_repo", repo);
        }
    }
}

#[component]
pub fn GithubIssues(project_id: String) -> impl IntoView {
    let (repo, set_repo) = signal(get_persisted_repo());
    let (issues, set_issues) = signal(Vec::<GithubIssue>::new());
    let (selected_issue, set_selected_issue) = signal(Option::<GithubIssue>::None);
    let (loading, set_loading) = signal(false);
    let (error, set_error) = signal(Option::<String>::None);
    let (importing_number, set_importing_number) = signal(Option::<i32>::None);
    let (import_success, set_import_success) = signal(Option::<i32>::None);

    let project_id = project_id.clone();

    // Persist repo whenever it changes
    Effect::new(move |_| {
        let r = repo.get();
        if !r.is_empty() {
            persist_repo(&r);
        }
    });

    let load_issues = move |_| {
        let repo_val = repo.get();
        if repo_val.is_empty() {
            set_error.set(Some("Enter a repository (e.g. owner/repo)".to_string()));
            return;
        }
        set_loading.set(true);
        set_error.set(None);
        set_issues.set(Vec::new());
        set_selected_issue.set(None);

        spawn_local(async move {
            match github_service::get_issues(repo_val).await {
                Ok(result) => {
                    set_issues.set(result);
                    set_loading.set(false);
                }
                Err(e) => {
                    set_error.set(Some(e));
                    set_loading.set(false);
                }
            }
        });
    };

    let project_id_for_import = project_id.clone();
    let import_issue = move |issue_number: i32| {
        let repo_val = repo.get();
        let pid = project_id_for_import.clone();
        if pid.is_empty() {
            set_error.set(Some("No project selected. Select a project first.".to_string()));
            return;
        }
        set_importing_number.set(Some(issue_number));
        set_import_success.set(None);

        spawn_local(async move {
            match github_service::create_task_from_issue(repo_val, issue_number, pid).await {
                Ok(_task) => {
                    set_importing_number.set(None);
                    set_import_success.set(Some(issue_number));
                }
                Err(e) => {
                    set_importing_number.set(None);
                    set_error.set(Some(format!("Import failed: {}", e)));
                }
            }
        });
    };

    let state_badge_class = |state: &str| -> &'static str {
        match state.to_uppercase().as_str() {
            "OPEN" => "bg-green-500/20 text-green-300",
            "CLOSED" => "bg-purple-500/20 text-purple-300",
            _ => "bg-white/10 text-white/60",
        }
    };

    view! {
        <div class="space-y-6">
            <div class="flex items-center justify-between">
                <div>
                    <h1 class="text-2xl font-bold text-white/90">"GitHub Issues"</h1>
                    <p class="text-sm text-white/40 mt-1">"Fetch and manage issues from any GitHub repository"</p>
                </div>
            </div>

            // Repo input bar
            <div class="flex items-center gap-3">
                <input
                    type="text"
                    placeholder="owner/repo (e.g. leptos-rs/leptos)"
                    prop:value=move || repo.get()
                    on:input=move |ev| set_repo.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" {
                            let repo_val = repo.get();
                            if !repo_val.is_empty() {
                                set_loading.set(true);
                                set_error.set(None);
                                set_issues.set(Vec::new());
                                set_selected_issue.set(None);
                                spawn_local(async move {
                                    match github_service::get_issues(repo_val).await {
                                        Ok(result) => {
                                            set_issues.set(result);
                                            set_loading.set(false);
                                        }
                                        Err(e) => {
                                            set_error.set(Some(e));
                                            set_loading.set(false);
                                        }
                                    }
                                });
                            }
                        }
                    }
                    class="flex-1 px-4 py-2.5 rounded-lg bg-white/5 border border-white/10 text-white/90 placeholder-white/30 focus:outline-none focus:ring-2 focus:ring-yellow-500/50 text-sm"
                />
                <button
                    on:click=load_issues
                    disabled=move || loading.get()
                    class="px-5 py-2.5 rounded-lg bg-yellow-500 hover:bg-yellow-600 disabled:opacity-50 disabled:cursor-not-allowed text-black font-medium transition-colors text-sm"
                >
                    {move || if loading.get() { "Loading..." } else { "Load Issues" }}
                </button>
            </div>

            // Error message
            {move || error.get().map(|err| view! {
                <div class="p-3 rounded-lg bg-red-500/10 border border-red-500/30 text-red-300 text-sm">
                    {err}
                </div>
            })}

            // Loading spinner
            {move || loading.get().then(|| view! {
                <div class="flex items-center justify-center py-12">
                    <div class="flex items-center gap-3 text-white/50">
                        <svg class="w-5 h-5 animate-spin" fill="none" viewBox="0 0 24 24">
                            <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
                            <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
                        </svg>
                        <span>"Fetching issues..."</span>
                    </div>
                </div>
            })}

            // Issues list + detail
            {move || {
                let current_issues = issues.get();
                if current_issues.is_empty() && !loading.get() && error.get().is_none() && !repo.get().is_empty() {
                    Some(view! {
                        <div class="text-center py-12 text-white/40">
                            <p>"No issues found for this repository."</p>
                        </div>
                    }.into_any())
                } else {
                    None
                }
            }}

            <div class="grid grid-cols-3 gap-6 h-[calc(100vh-16rem)]"
                 style:display=move || if issues.get().is_empty() { "none" } else { "grid" }
            >
                // Issues list panel
                <div class="col-span-1 border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden flex flex-col">
                    <div class="p-3 border-b border-white/10 flex items-center justify-between">
                        <span class="text-sm text-white/50">
                            {move || format!("{} issues", issues.get().len())}
                        </span>
                    </div>
                    <div class="flex-1 overflow-y-auto p-2 space-y-2">
                        {move || {
                            issues.get().into_iter().map(|issue| {
                                let issue_for_click = issue.clone();
                                let is_selected = selected_issue.get().as_ref().map(|i| i.number == issue.number).unwrap_or(false);
                                let state_class = state_badge_class(&issue.state);
                                let time_str = format_relative_time(&issue.created_at);
                                view! {
                                    <button
                                        on:click=move |_| set_selected_issue.set(Some(issue_for_click.clone()))
                                        class=format!(
                                            "w-full p-3 rounded-lg text-left transition-all {}",
                                            if is_selected {
                                                "bg-yellow-500/20 border border-yellow-500"
                                            } else {
                                                "bg-white/5 border border-white/10 hover:border-white/20"
                                            }
                                        )
                                    >
                                        <div class="flex items-start gap-2">
                                            <span class=format!("px-2 py-0.5 text-xs rounded {}", state_class)>
                                                {issue.state.clone()}
                                            </span>
                                            <span class="text-xs text-white/50">"#" {issue.number}</span>
                                        </div>
                                        <p class="text-sm font-medium text-white/90 mt-2 line-clamp-2">{issue.title.clone()}</p>
                                        <div class="flex items-center gap-3 mt-2 text-xs text-white/40">
                                            <span>{time_str}</span>
                                            <span>{issue.comments} " comments"</span>
                                            {(!issue.assignees.is_empty()).then(|| {
                                                let assignee_names: Vec<String> = issue.assignees.iter().map(|a| a.login.clone()).collect();
                                                view! {
                                                    <span class="truncate">{assignee_names.join(", ")}</span>
                                                }
                                            })}
                                        </div>
                                        <div class="flex flex-wrap gap-1 mt-2">
                                            {issue.labels.iter().map(|label| {
                                                view! {
                                                    <span class="px-1.5 py-0.5 text-[10px] rounded bg-white/10 text-white/60">
                                                        {label.name.clone()}
                                                    </span>
                                                }
                                            }).collect::<Vec<_>>()}
                                        </div>
                                    </button>
                                }
                            }).collect::<Vec<_>>()
                        }}
                    </div>
                </div>

                // Detail panel
                <div class="col-span-2 border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden">
                    {move || {
                        let import_issue = import_issue.clone();
                        match selected_issue.get() {
                        Some(issue) => {
                            let state_class = state_badge_class(&issue.state);
                            let issue_number = issue.number;
                            let repo_for_link = repo.get();
                            let is_importing = importing_number.get() == Some(issue_number);
                            let was_imported = import_success.get() == Some(issue_number);
                            view! {
                                <div class="h-full overflow-y-auto">
                                    <div class="p-6 border-b border-white/10">
                                        <div class="flex items-center gap-3 mb-3">
                                            <span class=format!("px-3 py-1 text-sm rounded-full {}", state_class)>
                                                {issue.state.clone()}
                                            </span>
                                            <span class="text-lg font-bold text-white/90">"#" {issue.number}</span>
                                        </div>
                                        <h2 class="text-xl font-bold text-white/90">{issue.title.clone()}</h2>
                                        <div class="flex items-center gap-4 mt-2 text-sm text-white/40">
                                            <span>{format_relative_time(&issue.created_at)}</span>
                                            <span>{format!("Updated {}", format_relative_time(&issue.updated_at))}</span>
                                            <span>{format!("{} comments", issue.comments)}</span>
                                        </div>
                                    </div>

                                    <div class="p-6">
                                        // Labels
                                        {(!issue.labels.is_empty()).then(|| {
                                            view! {
                                                <div class="flex flex-wrap gap-2 mb-6">
                                                    {issue.labels.iter().map(|label| {
                                                        let label_class = {
                                                            let name = label.name.to_lowercase();
                                                            if name.contains("enhancement") || name.contains("feature") {
                                                                "bg-teal-500/20 text-teal-300"
                                                            } else if name.contains("bug") {
                                                                "bg-red-500/20 text-red-300"
                                                            } else if name.contains("priority") || name.contains("urgent") {
                                                                "bg-orange-500/20 text-orange-300"
                                                            } else if name.contains("doc") {
                                                                "bg-blue-500/20 text-blue-300"
                                                            } else {
                                                                "bg-white/10 text-white/60"
                                                            }
                                                        };
                                                        view! {
                                                            <span class=format!("px-2 py-1 text-xs rounded {}", label_class)>
                                                                {label.name.clone()}
                                                            </span>
                                                        }
                                                    }).collect::<Vec<_>>()}
                                                </div>
                                            }
                                        })}

                                        // Assignees
                                        {(!issue.assignees.is_empty()).then(|| {
                                            view! {
                                                <div class="mb-6">
                                                    <h3 class="text-xs font-medium text-white/40 uppercase tracking-wider mb-2">"Assignees"</h3>
                                                    <div class="flex items-center gap-2">
                                                        {issue.assignees.iter().map(|assignee| {
                                                            view! {
                                                                <span class="px-2 py-1 text-xs rounded bg-white/10 text-white/70">
                                                                    {assignee.login.clone()}
                                                                </span>
                                                            }
                                                        }).collect::<Vec<_>>()}
                                                    </div>
                                                </div>
                                            }
                                        })}

                                        // Body
                                        <div class="prose prose-invert max-w-none">
                                            <pre class="whitespace-pre-wrap text-sm text-white/70 bg-white/[0.02] p-4 rounded-lg border border-white/5 max-h-[40vh] overflow-y-auto">
                                                {if issue.body.is_empty() { "No description provided.".to_string() } else { issue.body.clone() }}
                                            </pre>
                                        </div>

                                        // Actions
                                        <div class="mt-6 pt-6 border-t border-white/10 flex gap-3">
                                            <button
                                                on:click=move |_| import_issue(issue_number)
                                                disabled=move || is_importing || was_imported
                                                class=move || format!(
                                                    "px-4 py-2 rounded-lg font-medium transition-colors text-sm {}",
                                                    if was_imported {
                                                        "bg-green-500/20 text-green-300 cursor-default"
                                                    } else if is_importing {
                                                        "bg-yellow-500/50 text-black/50 cursor-wait"
                                                    } else {
                                                        "bg-yellow-500 hover:bg-yellow-600 text-black"
                                                    }
                                                )
                                            >
                                                {move || {
                                                    if was_imported {
                                                        "Imported to Kanban"
                                                    } else if is_importing {
                                                        "Importing..."
                                                    } else {
                                                        "Import to Kanban"
                                                    }
                                                }}
                                            </button>
                                            <a
                                                href=format!("https://github.com/{}/issues/{}", repo_for_link, issue.number)
                                                target="_blank"
                                                class="px-4 py-2 rounded-lg border border-white/10 text-white/70 hover:text-white/90 hover:bg-white/5 transition-colors text-sm"
                                            >
                                                "View on GitHub"
                                            </a>
                                        </div>
                                    </div>
                                </div>
                            }.into_any()
                        }
                        None => view! {
                            <div class="h-full flex items-center justify-center">
                                <div class="text-center">
                                    <svg class="w-12 h-12 mx-auto mb-4 text-white/20" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
                                    </svg>
                                    <p class="text-white/40">"Select an issue to view details"</p>
                                </div>
                            </div>
                        }.into_any(),
                    }}}
                </div>
            </div>
        </div>
    }
}
