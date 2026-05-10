use crate::models::github::PullRequest;
use crate::services::github_service;
use super::github_issues::format_relative_time;
use leptos::prelude::*;
use leptos::task::spawn_local;

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

fn get_persisted_pr_filter() -> String {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            if let Ok(Some(filter)) = storage.get_item("slashit_pr_filter") {
                return filter;
            }
        }
    }
    "all".to_string()
}

fn persist_pr_filter(filter: &str) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item("slashit_pr_filter", filter);
        }
    }
}

#[component]
pub fn GithubPrs(project_id: String) -> impl IntoView {
    let _project_id = project_id;
    let (repo, set_repo) = signal(get_persisted_repo());
    let (prs, set_prs) = signal(Vec::<PullRequest>::new());
    let (loading, set_loading) = signal(false);
    let (error, set_error) = signal(Option::<String>::None);
    let (filter, set_filter) = signal(get_persisted_pr_filter());

    // Persist repo whenever it changes
    Effect::new(move |_| {
        let r = repo.get();
        if !r.is_empty() {
            persist_repo(&r);
        }
    });

    // Persist filter whenever it changes
    Effect::new(move |_| {
        let f = filter.get();
        persist_pr_filter(&f);
    });

    let load_prs = move |_| {
        let repo_val = repo.get();
        if repo_val.is_empty() {
            set_error.set(Some("Enter a repository (e.g. owner/repo)".to_string()));
            return;
        }
        set_loading.set(true);
        set_error.set(None);
        set_prs.set(Vec::new());

        spawn_local(async move {
            match github_service::get_prs(repo_val).await {
                Ok(result) => {
                    set_prs.set(result);
                    set_loading.set(false);
                }
                Err(e) => {
                    set_error.set(Some(e));
                    set_loading.set(false);
                }
            }
        });
    };

    view! {
        <div class="space-y-6">
            <div class="flex items-center justify-between">
                <div>
                    <h1 class="text-2xl font-bold text-white/90">"GitHub Pull Requests"</h1>
                    <p class="text-sm text-white/40 mt-1">"Fetch and review pull requests from any GitHub repository"</p>
                </div>
                <div class="flex items-center gap-2">
                    {["all", "open", "merged", "closed"].into_iter().map(|f| {
                        let label = match f {
                            "all" => "All",
                            "open" => "Open",
                            "merged" => "Merged",
                            "closed" => "Closed",
                            _ => f,
                        };
                        let f_owned = f.to_string();
                        let f_for_click = f.to_string();
                        view! {
                            <button
                                class=move || format!(
                                    "px-3 py-1.5 rounded-lg text-sm transition-colors {}",
                                    if filter.get() == f_owned {
                                        "bg-yellow-500/20 text-yellow-400"
                                    } else {
                                        "text-white/50 hover:text-white/70 hover:bg-white/5"
                                    }
                                )
                                on:click=move |_| set_filter.set(f_for_click.clone())
                            >
                                {label}
                            </button>
                        }
                    }).collect::<Vec<_>>()}
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
                                set_prs.set(Vec::new());
                                spawn_local(async move {
                                    match github_service::get_prs(repo_val).await {
                                        Ok(result) => {
                                            set_prs.set(result);
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
                    on:click=load_prs
                    disabled=move || loading.get()
                    class="px-5 py-2.5 rounded-lg bg-yellow-500 hover:bg-yellow-600 disabled:opacity-50 disabled:cursor-not-allowed text-black font-medium transition-colors text-sm"
                >
                    {move || if loading.get() { "Loading..." } else { "Load PRs" }}
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
                        <span>"Fetching pull requests..."</span>
                    </div>
                </div>
            })}

            // PR list
            <div class="border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden"
                 style:display=move || if prs.get().is_empty() { "none" } else { "block" }
            >
                <div class="p-3 border-b border-white/10 flex items-center justify-between">
                    <span class="text-sm text-white/50">
                        {move || {
                            let all = prs.get();
                            let f = filter.get();
                            let filtered_count = if f == "all" {
                                all.len()
                            } else {
                                all.iter().filter(|pr| pr.state.to_uppercase() == f.to_uppercase()).count()
                            };
                            format!("{} pull requests", filtered_count)
                        }}
                    </span>
                </div>
                <div class="divide-y divide-white/10">
                    {move || {
                        let f = filter.get();
                        let repo_val = repo.get();
                        prs.get().into_iter().filter(|pr| {
                            if f == "all" { return true; }
                            pr.state.to_uppercase() == f.to_uppercase()
                        }).map(|pr| {
                            let repo_for_link = repo_val.clone();
                            view! { <PrItem pr=pr repo=repo_for_link /> }
                        }).collect::<Vec<_>>()
                    }}
                </div>
            </div>

            // Empty state after load
            {move || {
                let current_prs = prs.get();
                if current_prs.is_empty() && !loading.get() && error.get().is_none() && !repo.get().is_empty() {
                    Some(view! {
                        <div class="text-center py-12 text-white/40">
                            <p>"No pull requests found for this repository."</p>
                        </div>
                    })
                } else {
                    None
                }
            }}
        </div>
    }
}

#[component]
fn PrItem(pr: PullRequest, repo: String) -> impl IntoView {
    let state_class = match pr.state.to_uppercase().as_str() {
        "OPEN" => "bg-green-500/20 text-green-300",
        "MERGED" => "bg-purple-500/20 text-purple-300",
        "CLOSED" => "bg-red-500/20 text-red-300",
        _ => "bg-white/10 text-white/60",
    };

    let has_approval = pr.reviews.iter().any(|r| r.state == "APPROVED");
    let approvals = pr.reviews.iter().filter(|r| r.state == "APPROVED").count();
    let changes_requested = pr.reviews.iter().filter(|r| r.state == "CHANGES_REQUESTED").count();
    let review_count = pr.reviews.len();
    let time_str = format_relative_time(&pr.created_at);

    view! {
        <a
            href=format!("https://github.com/{}/pull/{}", repo, pr.number)
            target="_blank"
            class="block p-4 hover:bg-white/[0.02] transition-colors"
        >
            <div class="flex items-start gap-4">
                <div class="flex-1 min-w-0">
                    <div class="flex items-center gap-2 mb-1">
                        <span class=format!("px-2 py-0.5 text-xs rounded {}", state_class)>
                            {pr.state.clone()}
                        </span>
                        <span class="text-sm text-white/50">"#" {pr.number}</span>
                        {has_approval.then(|| {
                            view! {
                                <span class="px-2 py-0.5 text-xs rounded bg-green-500/20 text-green-300">"Approved"</span>
                            }
                        })}
                    </div>
                    <h3 class="text-base font-medium text-white/90">{pr.title.clone()}</h3>
                    <div class="flex items-center gap-4 mt-2 text-sm text-white/50">
                        <span>{format!("by {}", pr.author)}</span>
                        <span>{time_str}</span>
                        <span>{format!("{} reviews", review_count)}</span>
                    </div>
                </div>
                <div class="text-right flex-shrink-0">
                    <div class="flex items-center gap-2 text-sm">
                        <span class="text-green-400">{format!("+{}", pr.additions)}</span>
                        <span class="text-red-400">{format!("-{}", pr.deletions)}</span>
                    </div>
                    <div class="flex items-center gap-1 mt-1 justify-end">
                        {(approvals > 0).then(|| {
                            view! {
                                <span class="px-2 py-0.5 text-xs rounded bg-green-500/20 text-green-300">
                                    {format!("+{}", approvals)}
                                </span>
                            }
                        })}
                        {(changes_requested > 0).then(|| {
                            view! {
                                <span class="px-2 py-0.5 text-xs rounded bg-red-500/20 text-red-300">
                                    {format!("-{}", changes_requested)}
                                </span>
                            }
                        })}
                        {(approvals == 0 && changes_requested == 0 && review_count > 0).then(|| {
                            view! {
                                <span class="px-2 py-0.5 text-xs rounded bg-yellow-500/20 text-yellow-300">
                                    "Pending"
                                </span>
                            }
                        })}
                    </div>
                </div>
            </div>
        </a>
    }
}
