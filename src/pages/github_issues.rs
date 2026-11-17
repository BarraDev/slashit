// SPDX-License-Identifier: MPL-2.0
//! Issues from GitHub: browse a repository's open Issues, read one, and either
//! link it to an existing Task of a Project or create a new Task from it.
//!
//! Browsing is cached for the session (see `services::github_issue_cache`), so
//! leaving the page and coming back shows the last listing at once. Linking
//! names the Task and the Project explicitly, and never starts work.

use crate::components::markdown::render_safe_markdown;
use crate::components::task_dependency::status_label;
use crate::components::OpenTaskRequest;
use crate::components::task_issue_links::tasks_linking_issue;
use crate::models::github::GithubIssue;
use crate::models::{Project, Task};
use crate::services::github_issue_cache::{self, CacheKey, CachedIssues, IssueFilter};
use crate::services::{github_service, link_github_issue, list_tasks, project_service};
use chrono::{DateTime, Utc};
use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

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

/// `owner/name`: two non-empty parts and no whitespace.
fn is_repo_address(repo: &str) -> bool {
    match repo.split_once('/') {
        Some((owner, name)) => {
            !owner.is_empty()
                && !name.is_empty()
                && !name.contains('/')
                && !repo.chars().any(char::is_whitespace)
        }
        None => false,
    }
}

fn issue_url(repo: &str, number: i32) -> String {
    format!("https://github.com/{repo}/issues/{number}")
}

fn absolute_time(at: &DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M UTC").to_string()
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

/// How a selected Issue is brought into a Project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkMode {
    Existing,
    New,
}

/// The choice the page offers. Once a Task of the Project links the Issue,
/// creating another is not offered, whatever was chosen before.
fn shown_mode(chosen: LinkMode, already_linked: bool) -> LinkMode {
    if already_linked {
        LinkMode::Existing
    } else {
        chosen
    }
}

/// The Issue numbered `number` in the listing the page shows, as it is now.
fn issue_record(list: &[GithubIssue], number: i32) -> Option<&GithubIssue> {
    list.iter().find(|i| i.number == number)
}

#[component]
pub fn GithubIssues(project_id: String) -> impl IntoView {
    let initial_repo = get_persisted_repo();
    let initial = (!initial_repo.is_empty())
        .then(|| github_issue_cache::cached(&CacheKey::github(&initial_repo, IssueFilter::Open)))
        .flatten();

    // Browsing. `active` is the listing the page shows or is loading; a response
    // applies only while it is still the newest request for that listing.
    let repo = RwSignal::new(initial_repo.clone());
    let shown_repo = RwSignal::new(initial_repo.clone());
    let filter = RwSignal::new(IssueFilter::Open);
    let active = RwSignal::new((!initial_repo.is_empty()).then(|| CacheKey::github(&initial_repo, IssueFilter::Open)));
    let issues = RwSignal::new(initial.as_ref().map(|c| c.issues.clone()).unwrap_or_default());
    let refreshed_at = RwSignal::new(initial.as_ref().map(|c| c.refreshed_at));
    let request_seq = RwSignal::new(0_u64);
    let loading = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let selected = RwSignal::new(None::<i32>);

    // Linking. The destination Project is chosen; its Tasks are read from the
    // backend and reloaded after every change.
    let projects = RwSignal::new(Vec::<Project>::new());
    let destination = RwSignal::new(project_id.clone());
    let tasks = RwSignal::new(Vec::<Task>::new());
    let tasks_seq = RwSignal::new(0_u64);
    let mode = RwSignal::new(LinkMode::Existing);
    let chosen_task = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let notice = RwSignal::new(None::<String>);
    let link_error = RwSignal::new(None::<String>);
    let open_request = expect_context::<OpenTaskRequest>();

    let selected_issue = Memo::new(move |_| {
        let number = selected.get()?;
        issues.with(|list| list.iter().find(|i| i.number == number).cloned())
    });

    // Persist the repository whenever it changes.
    Effect::new(move |_| {
        let r = repo.get();
        if !r.is_empty() {
            persist_repo(&r);
        }
    });

    let reload_tasks = move |project: String| {
        if project.is_empty() {
            return;
        }
        let seq = tasks_seq.get_untracked() + 1;
        tasks_seq.set(seq);
        spawn_local(async move {
            let result = list_tasks(project.clone()).await;
            // A reply for a Project the person has since left, or one a later reload overtook, is not shown.
            if destination.try_get_untracked().as_deref() != Some(project.as_str())
                || tasks_seq.try_get_untracked() != Some(seq)
            {
                return;
            }
            match result {
                Ok(list) => {
                    tasks.try_set(list);
                }
                // Keep the list already shown; say that it could not be refreshed.
                Err(e) => {
                    link_error.try_set(Some(format!("Could not load the Tasks of this Project: {e}")));
                }
            }
        });
    };

    // Load a listing for the selected filter. A cached listing is shown without
    // a network read unless `force` asks for one. A refresh keeps what is shown;
    // a different repository or filter starts empty, so an unqueried listing is
    // never shown as an empty result. Returns whether the listing was applied.
    let load = move |force: bool| -> bool {
        let repo_val = repo.get_untracked().trim().to_string();
        if !is_repo_address(&repo_val) {
            error.set(Some("Enter a repository as owner/repo, for example leptos-rs/leptos.".to_string()));
            return false;
        }
        let state_filter = filter.get_untracked();
        let key = CacheKey::github(&repo_val, state_filter);
        let same_listing = active.get_untracked().as_ref() == Some(&key);
        if !force {
            if let Some(entry) = github_issue_cache::cached(&key) {
                request_seq.update(|s| *s += 1);
                if !same_listing {
                    selected.set(None);
                }
                active.set(Some(key));
                shown_repo.set(repo_val);
                issues.set(entry.issues);
                refreshed_at.set(Some(entry.refreshed_at));
                error.set(None);
                loading.set(false);
                return true;
            }
        }
        if !same_listing {
            issues.set(Vec::new());
            refreshed_at.set(None);
            selected.set(None);
        }
        let seq = request_seq.get_untracked() + 1;
        request_seq.set(seq);
        active.set(Some(key.clone()));
        shown_repo.set(repo_val.clone());
        loading.set(true);
        error.set(None);
        let state = state_filter.gh_state();

        spawn_local(async move {
            let result = github_service::get_issues(repo_val, state).await;
            let latest = request_seq.try_get_untracked();
            let current = active.try_get_untracked().flatten();
            let may_apply = latest.is_some_and(|latest| {
                github_issue_cache::may_apply(latest, seq, current.as_ref(), &key)
            });
            match result {
                Ok(list) => {
                    if may_apply {
                        let at = Utc::now();
                        github_issue_cache::remember(
                            key,
                            CachedIssues {
                                issues: list.clone(),
                                refreshed_at: at,
                            },
                        );
                        issues.try_set(list);
                        refreshed_at.try_set(Some(at));
                        error.try_set(None);
                        loading.try_set(false);
                    }
                }
                Err(e) => {
                    if may_apply {
                        // The listing already shown stays; the error says it is older.
                        error.try_set(Some(e));
                        loading.try_set(false);
                    }
                }
            }
        });
        true
    };

    // Select a filter. It takes effect only if its listing is applied, so the
    // buttons never name a state the list does not show.
    let choose_filter = move |next: IssueFilter| {
        let previous = filter.get_untracked();
        if next == previous {
            return;
        }
        filter.set(next);
        if active.get_untracked().is_some() && !load(false) {
            filter.set(previous);
        }
    };

    // Projects and the Tasks of the starting Project. A Project that is not
    // selected leaves the linking area empty until one is chosen.
    spawn_local(async move {
        match project_service::list_projects().await {
            Ok(list) => {
                projects.try_set(list);
            }
            Err(e) => {
                link_error.try_set(Some(format!("Could not load the Projects: {e}")));
            }
        }
    });
    reload_tasks(project_id.clone());
    // A remembered repository without a cached listing is read now. Otherwise
    // the page would show its empty state as if GitHub had answered.
    if initial.is_none() && !initial_repo.is_empty() {
        load(false);
    }

    let choose_project = move |project: String| {
        destination.set(project.clone());
        tasks.set(Vec::new());
        chosen_task.set(String::new());
        notice.set(None);
        link_error.set(None);
        reload_tasks(project);
    };

    // The Issue's Tasks in the destination Project, and the ones it could still
    // be linked to.
    let linked_here = move |number: i32| {
        tasks.with(|list| {
            tasks_linking_issue(list, &shown_repo.get(), number)
                .into_iter()
                .cloned()
                .collect::<Vec<Task>>()
        })
    };
    let candidates = move |number: i32| {
        tasks.with(|list| {
            let linked = tasks_linking_issue(list, &shown_repo.get(), number);
            list.iter()
                .filter(|t| !linked.iter().any(|l| l.id == t.id))
                .cloned()
                .collect::<Vec<Task>>()
        })
    };

    // The board of the destination Project opens the Task's drawer.
    let open_task = move |task_id: Uuid| {
        open_request.0.set(Some((destination.get_untracked(), task_id)));
    };

    let link_existing = move |number: i32| {
        if busy.get_untracked() {
            return;
        }
        let task_id = chosen_task.get_untracked();
        let project = destination.get_untracked();
        if task_id.is_empty() {
            link_error.set(Some("Choose a Task to link.".to_string()));
            return;
        }
        let url = issue_url(&shown_repo.get_untracked(), number);
        busy.set(true);
        notice.set(None);
        link_error.set(None);
        spawn_local(async move {
            let result = link_github_issue(task_id, url).await;
            busy.try_set(false);
            match result {
                Ok(Some(task)) => {
                    notice.try_set(Some(format!(
                        "Linked issue #{number} to \"{}\". Nothing was started.",
                        task.title
                    )));
                    chosen_task.try_set(String::new());
                }
                Ok(None) => {
                    link_error.try_set(Some(
                        "That Task no longer exists. The list has been reloaded.".to_string(),
                    ));
                }
                Err(e) => {
                    link_error.try_set(Some(e));
                }
            }
            reload_tasks(project);
        });
    };

    let create_new = move |number: i32| {
        if busy.get_untracked() {
            return;
        }
        let project = destination.get_untracked();
        let repo_val = shown_repo.get_untracked();
        busy.set(true);
        notice.set(None);
        link_error.set(None);
        spawn_local(async move {
            let result =
                github_service::create_task_from_issue(repo_val, number, project.clone()).await;
            busy.try_set(false);
            match result {
                Ok(task) => {
                    notice.try_set(Some(format!(
                        "Created Task \"{}\" in Backlog. Nothing was started.",
                        task.title
                    )));
                }
                Err(e) => {
                    link_error.try_set(Some(e));
                }
            }
            reload_tasks(project);
        });
    };

    view! {
        <div class="space-y-6">
            <div>
                <h1 class="text-2xl font-bold text-white/90">"Issues"</h1>
                <p class="text-sm text-white/40 mt-1">"Open Issues from GitHub. Link them to a Task or create a Task from them."</p>
            </div>

            <form
                class="flex items-center gap-3"
                on:submit=move |ev| {
                    ev.prevent_default();
                    load(false);
                }
            >
                <label class="sr-only" for="issues-repo">"GitHub repository"</label>
                <input
                    id="issues-repo"
                    type="text"
                    placeholder="owner/repo (e.g. leptos-rs/leptos)"
                    prop:value=move || repo.get()
                    on:input=move |ev| repo.set(event_target_value(&ev))
                    class="flex-1 px-4 py-2.5 rounded-lg bg-white/5 border border-white/10 text-white/90 placeholder-white/30 focus:outline-none focus:ring-2 focus:ring-yellow-500/50 text-sm"
                />
                <button
                    type="submit"
                    disabled=move || loading.get()
                    class="px-5 py-2.5 rounded-lg bg-yellow-500 hover:bg-yellow-600 disabled:opacity-50 disabled:cursor-not-allowed text-black font-medium transition-colors text-sm"
                >
                    {move || if loading.get() { "Loading..." } else { "Load Issues" }}
                </button>
                {move || active.with(|a| a.is_some()).then(|| view! {
                    <button
                        type="button"
                        data-testid="issues-refresh"
                        on:click=move |_| { load(true); }
                        disabled=move || loading.get()
                        class="px-4 py-2.5 rounded-lg border border-white/10 text-white/70 hover:text-white/90 hover:bg-white/5 disabled:opacity-50 transition-colors text-sm"
                    >
                        "Refresh"
                    </button>
                })}
            </form>

            <div role="group" aria-label="Issue state" class="flex items-center gap-2">
                {IssueFilter::ALL
                    .into_iter()
                    .map(|f| view! {
                        <button
                            type="button"
                            data-testid=format!("issues-filter-{}", f.gh_state())
                            aria-pressed=move || (filter.get() == f).to_string()
                            on:click=move |_| choose_filter(f)
                            class=move || if filter.get() == f {
                                "px-3 py-1.5 rounded-lg bg-white/10 text-white/90 text-sm"
                            } else {
                                "px-3 py-1.5 rounded-lg text-white/50 hover:text-white/90 hover:bg-white/5 text-sm"
                            }
                        >
                            {f.label()}
                        </button>
                    })
                    .collect_view()}
            </div>

            {move || refreshed_at.get().map(|at| view! {
                <p data-testid="issues-refreshed" class="text-xs text-white/40">
                    {format!("Last refreshed {} ({})", format_relative_time(&at), absolute_time(&at))}
                </p>
            })}

            // Error. Earlier results stay on screen beneath it.
            {move || error.get().map(|err| view! {
                <div role="alert" class="p-3 rounded-lg bg-red-500/10 border border-red-500/30 text-red-300 text-sm whitespace-pre-wrap break-words">
                    {err}
                </div>
            })}

            // Loading, before any results are on screen.
            {move || (loading.get() && issues.with(|l| l.is_empty())).then(|| view! {
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

            {move || {
                let empty = issues.with(|l| l.is_empty());
                let listed = active.with(|a| a.is_some());
                (empty && listed && !loading.get() && error.with(|e| e.is_none())).then(|| view! {
                    <div class="text-center py-12 text-white/40">
                        <p>{move || empty_message(filter.get(), &shown_repo.get())}</p>
                    </div>
                })
            }}
            {move || (active.with(|a| a.is_none())).then(|| view! {
                <div class="text-center py-12 text-white/40">
                    <p>"Enter a repository to browse its Issues."</p>
                </div>
            })}

            {move || (!issues.with(|l| l.is_empty())).then(|| view! {
                <div class="grid grid-cols-3 gap-6 h-[calc(100vh-18rem)]">
                    // Issues list panel
                    <div class="col-span-1 border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden flex flex-col">
                        <div class="p-3 border-b border-white/10 flex items-center justify-between">
                            <span data-testid="issues-count" class="text-sm text-white/50">
                                {move || format!("{} {}", issues.with(|l| l.len()), listing_name(filter.get()))}
                            </span>
                        </div>
                        // A full page may hide more Issues; the count is not the repository's total.
                        {move || github_issue_cache::is_incomplete(issues.with(|l| l.len())).then(|| view! {
                            <p data-testid="issues-incomplete" role="status" class="px-3 py-2 border-b border-white/10 text-xs text-yellow-300/80">
                                {format!("GitHub returned the first {} Issues for this listing. More may exist, so this list may be incomplete.", github_issue_cache::LISTING_LIMIT)}
                            </p>
                        })}
                        <div class="flex-1 overflow-y-auto p-2 space-y-2">
                            <For
                                each=move || issues.get()
                                key=|issue| issue.number
                                children=move |issue: GithubIssue| {
                                    let number = issue.number;
                                    // The row reads its record from the list, so a refreshed Issue updates in place.
                                    let record = move || issues.with(|l| issue_record(l, number).cloned());
                                    view! {
                                        <button
                                            data-testid="issue-row"
                                            on:click=move |_| selected.set(Some(number))
                                            class=move || format!(
                                                "w-full p-3 rounded-lg text-left transition-all {}",
                                                if selected.get() == Some(number) {
                                                    "bg-yellow-500/20 border border-yellow-500"
                                                } else {
                                                    "bg-white/5 border border-white/10 hover:border-white/20"
                                                }
                                            )
                                        >
                                            {move || record().map(|issue| {
                                                let state_class = state_badge_class(&issue.state);
                                                let time_str = format_relative_time(&issue.created_at);
                                                let assignees = issue.assignees.iter().map(|a| a.login.clone()).collect::<Vec<_>>().join(", ");
                                                view! {
                                            <div class="flex items-start gap-2">
                                                <span class=format!("px-2 py-0.5 text-xs rounded {}", state_class)>
                                                    {issue.state.clone()}
                                                </span>
                                                <span class="text-xs text-white/50">"#" {number}</span>
                                            </div>
                                            <p class="text-sm font-medium text-white/90 mt-2 line-clamp-2">{issue.title.clone()}</p>
                                            <div class="flex items-center gap-3 mt-2 text-xs text-white/40">
                                                <span>{time_str}</span>
                                                <span>{issue.comments} " comments"</span>
                                                {(!assignees.is_empty()).then(|| view! {
                                                    <span class="truncate">{assignees}</span>
                                                })}
                                            </div>
                                            <div class="flex flex-wrap gap-1 mt-2">
                                                {issue.labels.iter().map(|label| view! {
                                                    <span class="px-1.5 py-0.5 text-[10px] rounded bg-white/10 text-white/60">
                                                        {label.name.clone()}
                                                    </span>
                                                }).collect::<Vec<_>>()}
                                            </div>
                                                }
                                            })}
                                        </button>
                                    }
                                }
                            />
                        </div>
                    </div>

                    // Detail panel
                    <div class="col-span-2 border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden flex flex-col min-h-0">
                        {move || match selected_issue.get() {
                            Some(issue) => {
                                let number = issue.number;
                                let state_class = state_badge_class(&issue.state);
                                let body = if issue.body.trim().is_empty() {
                                    "<p>No description provided.</p>".to_string()
                                } else {
                                    render_safe_markdown(&issue.body)
                                };
                                let linked_list = move || linked_here(number);
                                let linked_ids = move || linked_here(number).iter().map(|t| t.id).collect::<Vec<Uuid>>();
                                let candidate_list = move || candidates(number);
                                let url = issue_url(&shown_repo.get(), number);
                                view! {
                                    // The header stays above the scrolling body, so no scroll position can hide the title.
                                    <div class="flex flex-col h-full min-h-0" data-testid="issue-detail">
                                        <div class="p-6 border-b border-white/10 shrink-0">
                                            <div class="flex items-center gap-3 mb-3">
                                                <span class=format!("px-3 py-1 text-sm rounded-full {}", state_class)>
                                                    {issue.state.clone()}
                                                </span>
                                                <span class="text-lg font-bold text-white/90">"#" {number}</span>
                                                <span class="text-sm text-white/40">{shown_repo.get()}</span>
                                            </div>
                                            <h2 class="text-xl font-bold text-white/90 break-words">{issue.title.clone()}</h2>
                                            <dl class="flex flex-wrap gap-x-6 gap-y-1 mt-3 text-sm text-white/40">
                                                <div>
                                                    <dt class="inline">"Opened "</dt>
                                                    <dd class="inline" title=absolute_time(&issue.created_at)>{format_relative_time(&issue.created_at)}</dd>
                                                </div>
                                                <div>
                                                    <dt class="inline">"Updated "</dt>
                                                    <dd class="inline" title=absolute_time(&issue.updated_at)>{format_relative_time(&issue.updated_at)}</dd>
                                                </div>
                                                <div>
                                                    <dd class="inline">{format!("{} comments", issue.comments)}</dd>
                                                </div>
                                            </dl>
                                        </div>

                                        <div class="flex-1 min-h-0 overflow-y-auto p-6 space-y-6" data-testid="issue-detail-body">
                                            {(!issue.labels.is_empty()).then(|| view! {
                                                <div class="flex flex-wrap gap-2">
                                                    {issue.labels.iter().map(|label| view! {
                                                        <span class="px-2 py-1 text-xs rounded bg-white/10 text-white/60">{label.name.clone()}</span>
                                                    }).collect::<Vec<_>>()}
                                                </div>
                                            })}

                                            {(!issue.assignees.is_empty()).then(|| view! {
                                                <div>
                                                    <h3 class="text-xs font-medium text-white/40 uppercase tracking-wider mb-2">"Assignees"</h3>
                                                    <div class="flex items-center gap-2">
                                                        {issue.assignees.iter().map(|assignee| view! {
                                                            <span class="px-2 py-1 text-xs rounded bg-white/10 text-white/70">{assignee.login.clone()}</span>
                                                        }).collect::<Vec<_>>()}
                                                    </div>
                                                </div>
                                            })}

                                            <div class="pt-6 border-t border-white/10 space-y-4">
                                                <div class="flex items-center justify-between gap-3">
                                                    <h3 class="text-xs font-medium text-white/40 uppercase tracking-wider">"Tasks"</h3>
                                                    <a
                                                        href=url
                                                        target="_blank"
                                                        class="text-sm text-white/70 hover:text-white/90 underline underline-offset-2"
                                                    >
                                                        "View on GitHub"
                                                    </a>
                                                </div>

                                                <div data-testid="issue-linked-tasks">
                                                    {move || linked_list().is_empty().then(|| view! {
                                                        <p class="text-sm text-white/40">"No Task in this Project links this Issue."</p>
                                                    })}
                                                    {move || (!linked_list().is_empty()).then(|| view! {
                                                        <p class="text-sm text-white/50 mb-2">"Already linked in this Project:"</p>
                                                    })}
                                                    // The list itself is never rebuilt; each row reads its Task from the list.
                                                    <ul class="space-y-1">
                                                        <For
                                                            each=linked_ids
                                                            key=|id| *id
                                                            children=move |task_id: Uuid| {
                                                                let record = move || tasks.with(|l| l.iter().find(|t| t.id == task_id).cloned());
                                                                view! {
                                                                    <li class="flex flex-wrap items-center gap-2 text-sm text-white/80">
                                                                        {move || record().map(|task| view! {
                                                                            <span>{task.title.clone()}</span>
                                                                            <span class="text-xs text-white/40">{status_label(&task.status)}</span>
                                                                        })}
                                                                        <button
                                                                            type="button"
                                                                            data-testid="issue-open-task"
                                                                            on:click=move |_| open_task(task_id)
                                                                            class="px-2 py-1 rounded-lg text-xs bg-white/5 text-white/80 hover:bg-white/10"
                                                                        >
                                                                            "Open Task"
                                                                        </button>
                                                                    </li>
                                                                }
                                                            }
                                                        />
                                                    </ul>
                                                </div>

                                                <fieldset class="space-y-3" disabled=move || busy.get()>
                                                    <legend class="sr-only">"What to do with this Issue"</legend>
                                                    <div class="flex flex-wrap gap-4 text-sm text-white/80">
                                                        <label class="flex items-center gap-2 cursor-pointer">
                                                            <input
                                                                type="radio"
                                                                name="issue-task-mode"
                                                                data-testid="issue-mode-existing"
                                                                prop:checked=move || !linked_list().is_empty() || mode.get() == LinkMode::Existing
                                                                on:change=move |_| mode.set(LinkMode::Existing)
                                                            />
                                                            {move || if linked_list().is_empty() { "Link to an existing Task" } else { "Link another existing Task" }}
                                                        </label>
                                                        // Offered only while no Task of this Project links the Issue.
                                                        {move || linked_list().is_empty().then(|| view! {
                                                            <label class="flex items-center gap-2 cursor-pointer">
                                                                <input
                                                                    type="radio"
                                                                    name="issue-task-mode"
                                                                    data-testid="issue-mode-new"
                                                                    prop:checked=move || mode.get() == LinkMode::New
                                                                    on:change=move |_| mode.set(LinkMode::New)
                                                                />
                                                                "Create a new Task"
                                                            </label>
                                                        })}
                                                    </div>

                                                    {move || match shown_mode(mode.get(), !linked_list().is_empty()) {
                                                        LinkMode::Existing => {
                                                            let options = candidate_list();
                                                            let none_left = options.is_empty();
                                                            view! {
                                                                <div class="flex flex-wrap items-center gap-2">
                                                                    <label class="sr-only" for="issue-task-select">"Task to link"</label>
                                                                    <select
                                                                        id="issue-task-select"
                                                                        data-testid="issue-task-select"
                                                                        class="flex-1 min-w-0 px-2 py-1.5 rounded-lg text-sm bg-white/5 border border-white/10 text-white/80"
                                                                        prop:value=move || chosen_task.get()
                                                                        on:change=move |ev| chosen_task.set(event_target_value(&ev))
                                                                    >
                                                                        <option value="">{if none_left { "No other Task in this Project" } else { "Choose a Task" }}</option>
                                                                        {options.iter().map(|task| view! {
                                                                            <option value=task.id.to_string()>{format!("{} ({})", task.title, status_label(&task.status))}</option>
                                                                        }).collect::<Vec<_>>()}
                                                                    </select>
                                                                    <button
                                                                        type="button"
                                                                        data-testid="issue-link-existing"
                                                                        on:click=move |_| link_existing(number)
                                                                        disabled=move || busy.get() || chosen_task.with(|c| c.is_empty()) || destination.with(|d| d.is_empty())
                                                                        class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/80 hover:bg-white/10 disabled:opacity-50"
                                                                    >
                                                                        "Link to Task"
                                                                    </button>
                                                                </div>
                                                            }.into_any()
                                                        }
                                                        LinkMode::New => {
                                                            view! {
                                                                <div class="space-y-2">
                                                                    <p class="text-sm text-white/50">
                                                                        "Creates one Task in Backlog from this Issue. It does not start an agent."
                                                                    </p>
                                                                    <button
                                                                        type="button"
                                                                        data-testid="issue-create-task"
                                                                        on:click=move |_| create_new(number)
                                                                        disabled=move || busy.get() || destination.with(|d| d.is_empty())
                                                                        class="px-4 py-2 rounded-lg font-medium text-sm bg-yellow-500 hover:bg-yellow-600 text-black disabled:opacity-50 disabled:cursor-not-allowed"
                                                                    >
                                                                        {move || if busy.get() { "Working..." } else { "Create Task" }}
                                                                    </button>
                                                                </div>
                                                            }.into_any()
                                                        }
                                                    }}
                                                </fieldset>

                                                <div class="flex flex-wrap items-center gap-3">
                                                    <label class="text-sm text-white/50" for="issue-project-select">"Project"</label>
                                                    <select
                                                        id="issue-project-select"
                                                        data-testid="issue-project-select"
                                                        class="px-2 py-1.5 rounded-lg text-sm bg-white/5 border border-white/10 text-white/80"
                                                        prop:value=move || destination.get()
                                                        on:change=move |ev| choose_project(event_target_value(&ev))
                                                    >
                                                        <option value="" disabled=true>"Choose a Project"</option>
                                                        <For
                                                            each=move || projects.get()
                                                            key=|p| p.id.to_string()
                                                            children=move |p: Project| {
                                                                let id = p.id.to_string();
                                                                view! { <option value=id>{p.name.clone()}</option> }
                                                            }
                                                        />
                                                    </select>
                                                </div>

                                                {move || notice.get().map(|n| view! {
                                                    <p data-testid="issue-notice" role="status" class="text-sm text-green-300">{n}</p>
                                                })}
                                                {move || link_error.get().map(|e| view! {
                                                    <p data-testid="issue-link-error" role="alert" class="text-sm text-red-300 whitespace-pre-wrap break-words">{e}</p>
                                                })}
                                            </div>

                                            // Body: the safe Markdown renderer keeps no raw HTML, fetches no
                                            // images and keeps only safe links. Long bodies and code blocks
                                            // scroll inside this box.
                                            <div>
                                                <h3 class="text-xs font-medium text-white/40 uppercase tracking-wider mb-2">"Description"</h3>
                                                <div
                                                    data-testid="issue-body"
                                                    class="prose prose-invert max-w-none text-sm text-white/70 bg-white/[0.02] p-4 rounded-lg border border-white/5 max-h-[40vh] overflow-auto break-words"
                                                    inner_html=body
                                                ></div>
                                            </div>
                                        </div>
                                    </div>
                                }.into_any()
                            }
                            None => view! {
                                <div class="h-full flex items-center justify-center">
                                    <p class="text-white/40">"Select an Issue to read it"</p>
                                </div>
                            }.into_any(),
                        }}
                    </div>
                </div>
            })}
        </div>
    }
}

/// The words that name a listing in the list header: "12 open Issues".
fn listing_name(filter: IssueFilter) -> &'static str {
    match filter {
        IssueFilter::Open => "open Issues",
        IssueFilter::Closed => "closed Issues",
        IssueFilter::All => "Issues",
    }
}

fn empty_message(filter: IssueFilter, repo: &str) -> String {
    match filter {
        IssueFilter::Open => format!("No open Issues in {repo}."),
        IssueFilter::Closed => format!("No closed Issues in {repo}."),
        IssueFilter::All => format!("No Issues in {repo}."),
    }
}

fn state_badge_class(state: &str) -> &'static str {
    match state.to_uppercase().as_str() {
        "OPEN" => "bg-green-500/20 text-green-300",
        "CLOSED" => "bg-purple-500/20 text-purple-300",
        _ => "bg-white/10 text-white/60",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_is_owner_slash_name_without_spaces() {
        assert!(is_repo_address("leptos-rs/leptos"));
        assert!(!is_repo_address(""));
        assert!(!is_repo_address("leptos-rs"));
        assert!(!is_repo_address("/leptos"));
        assert!(!is_repo_address("leptos-rs/"));
        assert!(!is_repo_address("a/b/c"));
        assert!(!is_repo_address("a /b"));
    }

    #[test]
    fn creating_a_task_is_not_offered_once_the_issue_is_linked() {
        assert_eq!(shown_mode(LinkMode::New, true), LinkMode::Existing);
        assert_eq!(shown_mode(LinkMode::New, false), LinkMode::New);
        assert_eq!(shown_mode(LinkMode::Existing, false), LinkMode::Existing);
    }

    #[test]
    fn an_issue_address_is_built_from_the_repository_and_number() {
        assert_eq!(issue_url("o/r", 7), "https://github.com/o/r/issues/7");
    }

    #[test]
    fn a_hostile_issue_body_renders_without_active_content() {
        // The body is untrusted GitHub text: no script, no event handler, no
        // image fetch, no javascript: destination.
        let html = render_safe_markdown(
            "<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>\n\n[click](javascript:alert(1))",
        );
        assert!(!html.contains("<script"));
        assert!(!html.contains("<img"));
        assert!(!html.contains("onerror"));
        assert!(!html.contains("javascript:"));
    }

    #[test]
    fn a_row_reads_the_record_a_refresh_last_stored() {
        let issue = |title: &str, state: &str| GithubIssue {
            number: 7,
            title: title.into(),
            body: String::new(),
            state: state.into(),
            labels: Vec::new(),
            assignees: Vec::new(),
            comments: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let before = vec![issue("Old title", "OPEN")];
        assert_eq!(issue_record(&before, 7).map(|i| i.title.as_str()), Some("Old title"));
        let after = vec![issue("New title", "CLOSED")];
        let record = issue_record(&after, 7).expect("the Issue is still listed");
        assert_eq!((record.title.as_str(), record.state.as_str()), ("New title", "CLOSED"));
        assert!(issue_record(&after, 8).is_none());
    }
}
