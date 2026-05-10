use leptos::prelude::*;
use crate::models::{Task, TaskStatus, TaskCategory, TaskPriority, TaskPhase, QaStatus, ExternalRef};

/// Compact task card designed to fit well within Kanban columns
/// Inspired by Auto Claude's clean, modern design
#[component]
pub fn TaskCard(task: Task) -> impl IntoView {
    let is_stuck = task.stuck_since.is_some();
    let is_running = task.status == TaskStatus::InProgress;
    let show_review = matches!(task.status, TaskStatus::AiReview | TaskStatus::HumanReview);

    let card_class = move || {
        let mut classes = vec![
            "relative rounded-xl p-3.5 transition-all duration-200 cursor-pointer".to_string(),
            "bg-[#12121a] border border-white/[0.06]".to_string(),
        ];

        if is_running {
            classes.push("border-blue-500/30 shadow-[0_0_15px_rgba(59,130,246,0.1)]".to_string());
        } else if is_stuck {
            classes.push("border-amber-500/30 shadow-[0_0_15px_rgba(245,158,11,0.1)]".to_string());
        }

        classes.join(" ")
    };

    view! {
        <div data-testid="task-card" class=card_class()>
            // Running indicator (subtle glow bar at top)
            {is_running.then(|| view! {
                <div class="absolute top-0 left-2 right-2 h-0.5 bg-gradient-to-r from-blue-500 via-blue-400 to-blue-500 rounded-full animate-pulse"></div>
            })}
            
            // Stuck indicator
            {is_stuck.then(|| view! {
                <div class="absolute top-0 left-2 right-2 h-0.5 bg-gradient-to-r from-amber-500 via-amber-400 to-amber-500 rounded-full animate-pulse"></div>
            })}

            // Title - clean and prominent
            <h4 data-testid="task-title" class="font-medium text-white/90 text-[13px] leading-snug mb-2.5 pr-6 line-clamp-2">
                {task.title.clone()}
            </h4>

            // Badges row: Category + Priority (smaller, more subtle)
            <div class="flex items-center gap-1.5 mb-2.5">
                <CategoryBadge category=task.category.clone() />
                <PriorityBadge priority=task.priority.clone() />
                {is_running.then(|| view! {
                    <span class="px-1.5 py-0.5 text-[10px] font-medium rounded-md bg-blue-500/20 text-blue-300 animate-pulse">
                        "Running"
                    </span>
                })}
            </div>

            // Progress bar - clean design
            <div data-testid="task-progress">
                <CompactProgressBar progress=task.overall_progress />
            </div>

            // Phase + subtasks inline
            <div class="flex items-center justify-between mt-2.5">
                <CompactPhaseIndicator phase=task.phase.clone() />
                <div class="flex items-center gap-2 text-[10px]">
                    {(!task.subtasks.is_empty()).then(|| {
                        let completed = task.subtasks.iter().filter(|s| s.completed).count();
                        let total = task.subtasks.len();
                        view! {
                            <span class="flex items-center gap-1 text-white/40">
                                <svg class="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2m-6 9l2 2 4-4" />
                                </svg>
                                {format!("{}/{}", completed, total)}
                            </span>
                        }
                    })}
                    {(!task.dependencies.is_empty()).then(|| view! {
                        <span class="flex items-center gap-1 text-white/40">
                            <svg class="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13.828 10.172a4 4 0 00-5.656 0l-4 4a4 4 0 105.656 5.656l1.102-1.101m-.758-4.899a4 4 0 005.656 0l4-4a4 4 0 00-5.656-5.656l-1.1 1.1" />
                            </svg>
                            {task.dependencies.len()}
                        </span>
                    })}
                </div>
            </div>

            // External references (issues, PRs, tickets)
            {(!task.external_refs.is_empty()).then(|| {
                let refs = task.external_refs.clone();
                view! {
                    <div class="flex items-center gap-1.5 mt-2.5 pt-2.5 border-t border-white/[0.04] flex-wrap">
                        {refs.into_iter().map(|ext_ref| {
                            view! { <ExternalRefBadge ext_ref=ext_ref /> }
                        }).collect_view()}
                    </div>
                }
            })}

            // Fallback for old tasks without external_refs migrated
            {(task.external_refs.is_empty() && (task.github_issue_url.is_some() || task.pr_url.is_some())).then(|| {
                let issue_url = task.github_issue_url.clone();
                let pr_url = task.pr_url.clone();
                view! {
                    <div class="flex items-center gap-1.5 mt-2.5 pt-2.5 border-t border-white/[0.04]">
                        {issue_url.map(|url| view! {
                            <a
                                href=url
                                target="_blank"
                                class="flex items-center gap-1 px-2 py-1 text-[10px] rounded-md bg-white/[0.04] text-white/50 hover:bg-white/10 hover:text-white/80 transition-colors"
                                on:click=move |e: web_sys::MouseEvent| e.stop_propagation()
                            >
                                <svg class="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
                                </svg>
                                "Issue"
                            </a>
                        })}
                        {pr_url.map(|url| view! {
                            <a
                                href=url
                                target="_blank"
                                class="flex items-center gap-1 px-2 py-1 text-[10px] rounded-md bg-green-500/10 text-green-400 hover:bg-green-500/20 transition-colors"
                                on:click=move |e: web_sys::MouseEvent| e.stop_propagation()
                            >
                                <svg class="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 9l3 3-3 3m5 0h3M5 20h14a2 2 0 002-2V6a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z" />
                                </svg>
                                "PR"
                            </a>
                        })}
                    </div>
                }
            })}

            // Review status - compact and clean
            {show_review.then(|| view! {
                <div class="mt-2.5 pt-2.5 border-t border-white/[0.04] space-y-1.5">
                    {task.qa_signoff.as_ref().map(|qa| view! {
                        <div class=format!(
                            "flex items-center gap-1.5 text-[10px] px-2 py-1 rounded-md {}",
                            if matches!(qa.status, QaStatus::Approved) {
                                "bg-green-500/10 text-green-400"
                            } else {
                                "bg-yellow-500/10 text-yellow-400"
                            }
                        )>
                            <svg class="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2m-6 9l2 2 4-4" />
                            </svg>
                            <span>{format!("{:?}", qa.status)}</span>
                        </div>
                    })}
                    {task.human_review.as_ref().map(|hr| view! {
                        <div class=format!(
                            "flex items-center gap-1.5 text-[10px] px-2 py-1 rounded-md {}",
                            if hr.approved {
                                "bg-green-500/10 text-green-400"
                            } else {
                                "bg-purple-500/10 text-purple-400"
                            }
                        )>
                            <svg class="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M16 7a4 4 0 11-8 0 4 4 0 018 0zM12 14a7 7 0 00-7 7h14a7 7 0 00-7-7z" />
                            </svg>
                            <span>{if hr.approved { "Approved" } else { "Pending Review" }}</span>
                        </div>
                    })}
                </div>
            })}

            // Error message - show when task has error
            {task.error_message.as_ref().map(|msg| {
                let msg = msg.clone();
                view! {
                    <div class="mt-2.5 pt-2.5 border-t border-white/[0.04]">
                        <div class="flex items-start gap-1.5 px-2 py-1.5 rounded-md bg-red-500/10 text-[10px] text-red-400">
                            <svg class="w-3 h-3 mt-0.5 flex-shrink-0" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-2.5L13.732 4c-.77-.833-1.964-.833-2.732 0L4.082 16.5c-.77.833.192 2.5 1.732 2.5z" />
                            </svg>
                            <span class="line-clamp-3">{msg}</span>
                        </div>
                    </div>
                }
            })}

            // Model tag - cleaner bottom section
            <div class="flex items-center gap-1.5 mt-2.5 pt-2.5 border-t border-white/[0.04]">
                <span class="px-2 py-0.5 rounded-md bg-white/[0.04] text-[10px] text-white/40 font-mono truncate max-w-[90px]">
                    {if task.model == "default" || task.model.is_empty() { "auto".to_string() } else { task.model.clone() }}
                </span>
                {task.planning_mode.then(|| view! {
                    <span class="px-2 py-0.5 rounded-md bg-purple-500/10 text-[10px] text-purple-400">"Plan"</span>
                })}
                {task.branch_name.as_ref().map(|branch| {
                    view! {
                        <span class="px-2 py-0.5 rounded-md bg-cyan-500/10 text-[10px] text-cyan-400 font-mono truncate max-w-[100px]" title=branch.clone()>
                            {branch.clone()}
                        </span>
                    }
                })}
                {task.worktree_path.as_ref().map(|_| {
                    view! {
                        <span class="px-1.5 py-0.5 rounded-md bg-green-500/10 text-[10px] text-green-400">"WT"</span>
                    }
                })}
            </div>
        </div>
    }
}

#[component]
fn ExternalRefBadge(ext_ref: ExternalRef) -> impl IntoView {
    match ext_ref {
        ExternalRef::GithubIssue { url, number, state, .. } => {
            let state_color = match state.as_deref() {
                Some("open") => "text-green-400",
                Some("closed") => "text-purple-400",
                _ => "text-white/50",
            };
            view! {
                <a
                    href=url
                    target="_blank"
                    class="flex items-center gap-1 px-2 py-1 text-[10px] rounded-md bg-white/[0.04] text-white/50 hover:bg-white/10 hover:text-white/80 transition-colors"
                    on:click=move |e: web_sys::MouseEvent| e.stop_propagation()
                >
                    <svg class="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
                    </svg>
                    {format!("#{}", number)}
                    <span class=state_color>{"\u{00B7}"}</span>
                </a>
            }.into_any()
        }
        ExternalRef::GithubPr { url, number, state, .. } => {
            let (icon_color, status_char) = match state.as_deref() {
                Some("MERGED") => ("text-purple-400", "M"),
                Some("CLOSED") => ("text-red-400", "X"),
                _ => ("text-green-400", "O"),
            };
            view! {
                <a
                    href=url
                    target="_blank"
                    class=format!("flex items-center gap-1 px-2 py-1 text-[10px] rounded-md bg-green-500/10 {} hover:bg-green-500/20 transition-colors", icon_color)
                    on:click=move |e: web_sys::MouseEvent| e.stop_propagation()
                >
                    <svg class="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 9l3 3-3 3m5 0h3M5 20h14a2 2 0 002-2V6a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z" />
                    </svg>
                    {format!("PR #{}", number)}
                    <span class="ml-0.5 text-[9px]">{status_char}</span>
                </a>
            }.into_any()
        }
        ExternalRef::JiraTicket { key, .. } => {
            view! {
                <span class="flex items-center gap-1 px-2 py-1 text-[10px] rounded-md bg-blue-500/10 text-blue-400">
                    <svg class="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2" />
                    </svg>
                    {key}
                </span>
            }.into_any()
        }
        ExternalRef::LinearTicket { id } => {
            view! {
                <span class="flex items-center gap-1 px-2 py-1 text-[10px] rounded-md bg-violet-500/10 text-violet-400">
                    {id}
                </span>
            }.into_any()
        }
        ExternalRef::GitlabIssue { url } => {
            let number = url.rsplit('/').next().unwrap_or("?").to_string();
            view! {
                <a
                    href=url
                    target="_blank"
                    class="flex items-center gap-1 px-2 py-1 text-[10px] rounded-md bg-orange-500/10 text-orange-400 hover:bg-orange-500/20 transition-colors"
                    on:click=move |e: web_sys::MouseEvent| e.stop_propagation()
                >
                    {format!("GL #{}", number)}
                </a>
            }.into_any()
        }
    }
}

#[component]
fn CategoryBadge(category: TaskCategory) -> impl IntoView {
    let (label, color_class) = match category {
        TaskCategory::Feature => ("Feature", "bg-blue-500/10 text-blue-400"),
        TaskCategory::BugFix => ("Bug", "bg-red-500/10 text-red-400"),
        TaskCategory::Refactoring => ("Refactor", "bg-orange-500/10 text-orange-400"),
        TaskCategory::Documentation => ("Docs", "bg-slate-500/10 text-slate-400"),
        TaskCategory::Security => ("Security", "bg-red-500/10 text-red-400"),
        TaskCategory::Performance => ("Perf", "bg-green-500/10 text-green-400"),
        TaskCategory::UiUx => ("UI/UX", "bg-pink-500/10 text-pink-400"),
        TaskCategory::Infrastructure => ("Infra", "bg-cyan-500/10 text-cyan-400"),
        TaskCategory::Testing => ("Test", "bg-yellow-500/10 text-yellow-400"),
    };

    view! {
        <span class=format!("px-1.5 py-0.5 text-[10px] font-medium rounded-md {}", color_class)>
            {label}
        </span>
    }
}

#[component]
fn PriorityBadge(priority: TaskPriority) -> impl IntoView {
    let (label, color_class, dot_color) = match priority {
        TaskPriority::Urgent => ("Urgent", "text-red-400", "bg-red-500"),
        TaskPriority::High => ("High", "text-orange-400", "bg-orange-500"),
        TaskPriority::Medium => ("Med", "text-yellow-400", "bg-yellow-500"),
        TaskPriority::Low => ("Low", "text-green-400", "bg-green-500"),
    };

    view! {
        <span class=format!("flex items-center gap-1 text-[10px] {}", color_class)>
            <span class=format!("w-1.5 h-1.5 rounded-full {}", dot_color)></span>
            <span>{label}</span>
        </span>
    }
}

/// Compact progress bar for task cards - cleaner design
#[component]
fn CompactProgressBar(progress: u8) -> impl IntoView {
    let percentage = progress as f32;
    let is_complete = progress >= 100;

    view! {
        <div class="flex items-center gap-2">
            <div class="flex-1 relative h-1.5 bg-white/[0.06] rounded-full overflow-hidden">
                <div
                    class=move || format!(
                        "absolute top-0 left-0 h-full rounded-full transition-all duration-500 {}",
                        if is_complete { 
                            "bg-gradient-to-r from-green-500 to-emerald-500" 
                        } else if progress > 50 {
                            "bg-gradient-to-r from-blue-500 to-cyan-500"
                        } else {
                            "bg-gradient-to-r from-blue-600 to-blue-500"
                        }
                    )
                    style=format!("width: {}%", percentage)
                >
                    // Shimmer effect for running tasks
                    <div class="absolute inset-0 bg-gradient-to-r from-transparent via-white/20 to-transparent -translate-x-full animate-shimmer"></div>
                </div>
            </div>
            <span class=move || format!(
                "text-[10px] w-7 text-right font-medium {}",
                if is_complete { "text-green-400" } else { "text-white/50" }
            )>
                {format!("{}%", progress)}
            </span>
        </div>
    }
}

/// Full progress bar (kept for compatibility)
#[component]
pub fn ProgressBar(progress: u8) -> impl IntoView {
    let percentage = progress as f32;
    let is_complete = progress >= 100;

    view! {
        <div class="relative h-1.5 bg-white/10 rounded-full overflow-hidden">
            <div
                class=format!(
                    "absolute top-0 left-0 h-full rounded-full transition-all duration-500 {}",
                    if is_complete {
                        "bg-green-500"
                    } else {
                        "bg-blue-500"
                    }
                )
                style=format!("width: {}%", percentage)
            >
                <div class="absolute inset-0 bg-gradient-to-r from-transparent via-white/20 to-transparent animate-shimmer"></div>
            </div>
        </div>
        <div class="flex justify-between mt-1">
            <span class="text-xs text-white/40">"Progress"</span>
            <span class="text-xs text-white/60">{format!("{}%", progress)}</span>
        </div>
    }
}

/// Compact phase indicator for task cards - cleaner design
#[component]
fn CompactPhaseIndicator(phase: TaskPhase) -> impl IntoView {
    let (label, color_class, bg_class) = match phase {
        TaskPhase::Idle => ("Idle", "text-white/40", "bg-white/[0.04]"),
        TaskPhase::Planning => ("Planning", "text-yellow-400", "bg-yellow-500/10"),
        TaskPhase::Coding => ("Coding", "text-blue-400", "bg-blue-500/10"),
        TaskPhase::QaReview => ("QA Review", "text-purple-400", "bg-purple-500/10"),
        TaskPhase::QaFixing => ("Fixing", "text-orange-400", "bg-orange-500/10"),
        TaskPhase::Complete => ("Complete", "text-green-400", "bg-green-500/10"),
        TaskPhase::Failed => ("Failed", "text-red-400", "bg-red-500/10"),
    };

    view! {
        <div class=format!("flex items-center gap-1.5 px-2 py-0.5 rounded-md {} {}", bg_class, color_class)>
            <span class="w-1.5 h-1.5 rounded-full bg-current animate-pulse"></span>
            <span class="text-[10px] font-medium">{label}</span>
        </div>
    }
}

