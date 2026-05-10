use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::callback::Callback;
use wasm_bindgen::JsCast;
use crate::models::{Task, TaskStatus};
use crate::models::task::{
    PrCommentKind, PrReviewApplyResult, PrReviewComment, PrReviewDecisionKind, PrReviewItem,
    PrReviewPlan,
};
use crate::components::{TaskCard, TaskEditModal, TaskEditMode, toast, TaskContextMenu, DiffModal};
use crate::services::{reorder_task, queue_service, get_task_diff, get_task_diff_stat, analyze_pr_comments, address_pr_review, discuss_pr_review_questions, find_pr_candidates, link_existing_pr, get_pr_push_recovery, recover_private_email_and_create_pr, refresh_task_pr_state, AddressPrReviewOptions, PrCandidate, PrPushRecoveryPlan};
use uuid::Uuid;
use std::collections::HashSet;

#[derive(Clone, PartialEq, Eq)]
struct ColumnData {
    status: TaskStatus,
    title: &'static str,
    description: &'static str,
    color_class: &'static str,
    bg_class: &'static str,
    count_bg_class: &'static str,
    empty_icon: &'static str,
    empty_message: &'static str,
}

const COLUMNS: [ColumnData; 8] = [
    ColumnData {
        status: TaskStatus::Backlog,
        title: "Backlog",
        description: "Tasks waiting to be queued",
        color_class: "border-slate-500",
        bg_class: "bg-slate-500/5",
        count_bg_class: "bg-slate-500/20 text-slate-300",
        empty_icon: "",
        empty_message: "No tasks in backlog",
    },
    ColumnData {
        status: TaskStatus::Error,
        title: "Error",
        description: "Failed tasks",
        color_class: "border-red-500",
        bg_class: "bg-red-500/5",
        count_bg_class: "bg-red-500/20 text-red-300",
        empty_icon: "",
        empty_message: "No errors",
    },
    ColumnData {
        status: TaskStatus::Queue,
        title: "Queue",
        description: "Ready to start",
        color_class: "border-cyan-500",
        bg_class: "bg-cyan-500/5",
        count_bg_class: "bg-cyan-500/20 text-cyan-300",
        empty_icon: "",
        empty_message: "Queue is empty",
    },
    ColumnData {
        status: TaskStatus::InProgress,
        title: "In Progress",
        description: "Currently working",
        color_class: "border-blue-500",
        bg_class: "bg-blue-500/5",
        count_bg_class: "bg-blue-500/20 text-blue-300",
        empty_icon: "",
        empty_message: "Nothing running",
    },
    ColumnData {
        status: TaskStatus::AiReview,
        title: "AI Review",
        description: "Awaiting AI check",
        color_class: "border-amber-500",
        bg_class: "bg-amber-500/5",
        count_bg_class: "bg-amber-500/20 text-amber-300",
        empty_icon: "",
        empty_message: "No AI reviews pending",
    },
    ColumnData {
        status: TaskStatus::HumanReview,
        title: "Human Review",
        description: "Ready for approval",
        color_class: "border-purple-500",
        bg_class: "bg-purple-500/5",
        count_bg_class: "bg-purple-500/20 text-purple-300",
        empty_icon: "",
        empty_message: "Awaiting your review",
    },
    ColumnData {
        status: TaskStatus::PrCreated,
        title: "PR Open",
        description: "Awaiting merge",
        color_class: "border-cyan-400",
        bg_class: "bg-cyan-400/5",
        count_bg_class: "bg-cyan-400/20 text-cyan-200",
        empty_icon: "",
        empty_message: "No open PRs",
    },
    ColumnData {
        status: TaskStatus::Done,
        title: "Done",
        description: "Merged / completed",
        color_class: "border-emerald-500",
        bg_class: "bg-emerald-500/5",
        count_bg_class: "bg-emerald-500/20 text-emerald-300",
        empty_icon: "",
        empty_message: "No completed tasks",
    },
];

#[component]
pub fn Kanban(
    tasks: ReadSignal<Vec<Task>>,
    set_tasks: WriteSignal<Vec<Task>>,
    #[prop(default = String::new())] project_id: String,
    #[prop(into)] selected_tasks: Signal<Vec<Uuid>>,
    set_selected_tasks: WriteSignal<Vec<Uuid>>,
) -> impl IntoView {
    // Use the provided selection state
    let selected_tasks_signal = selected_tasks;
    let set_selected_tasks_writer = set_selected_tasks;
    // Use parent-owned task signals so this component is not recreated when the
    // parent's task list updates (which would destroy modal/menu state).
    let tasks_signal = tasks;
    let set_tasks_signal = set_tasks;
    let (dragged_task, set_dragged_task) = signal(None::<(String, TaskStatus)>);
    let (drag_over_column, set_drag_over_column) = signal(None::<TaskStatus>);
    // Track which position within a column we're hovering over (task_id we're above, or None for end)
    let (drag_over_position, set_drag_over_position) = signal(None::<(TaskStatus, Option<String>)>);
    // Modal state
    let (show_modal, set_show_modal) = signal(false);
    let (modal_mode, set_modal_mode) = signal(TaskEditMode::Create);
    
    // Context menu state (shared across all task cards)
    let (show_context_menu, set_show_context_menu) = signal(false);
    let (context_menu_pos, set_context_menu_pos) = signal((0i32, 0i32));
    let (context_menu_task, set_context_menu_task) = signal(None::<Task>);

    // Diff modal state
    let show_diff_modal = RwSignal::new(false);
    let diff_content = RwSignal::new(String::new());
    let diff_stat_content = RwSignal::new(String::new());
    let diff_title = RwSignal::new(String::new());
    let show_pr_review_modal = RwSignal::new(false);
    let pr_review_task = RwSignal::new(None::<Task>);
    let pr_review_plan = RwSignal::new(None::<PrReviewPlan>);
    let pr_review_loading = RwSignal::new(false);
    let pr_review_applying = RwSignal::new(false);
    let pr_review_error = RwSignal::new(None::<String>);
    let pr_review_auto_push = RwSignal::new(true);
    let pr_review_auto_reply = RwSignal::new(true);
    let pr_review_dry_run = RwSignal::new(false);
    let pr_review_last_apply = RwSignal::new(None::<PrReviewApplyResult>);
    let show_pr_candidates_modal = RwSignal::new(false);
    let pr_candidate_task = RwSignal::new(None::<Task>);
    let pr_candidates = RwSignal::new(Vec::<PrCandidate>::new());
    let show_pr_recovery_modal = RwSignal::new(false);
    let pr_recovery_task = RwSignal::new(None::<Task>);
    let pr_recovery_plan = RwSignal::new(None::<PrPushRecoveryPlan>);
    let pr_recovery_email = RwSignal::new(String::new());
    let pr_recovery_loading = RwSignal::new(false);

    let project_id_modal = project_id.clone();
    
    let on_create_click = move |_| {
        set_modal_mode.set(TaskEditMode::Create);
        set_show_modal.set(true);
    };

    let on_task_save = Callback::new({
        move |task: Task| {
            set_tasks_signal.update(|tasks| {
                if let Some(existing) = tasks.iter_mut().find(|t| t.id == task.id) {
                    *existing = task;
                } else {
                    tasks.push(task);
                }
            });
        }
    });

    let on_task_delete = Callback::new({
        move |task_id: Uuid| {
            set_tasks_signal.update(|tasks| {
                tasks.retain(|t| t.id != task_id);
            });
        }
    });

    let on_task_click = Callback::new({
        move |task: Task| {
            // In review/done phases, show diff instead of edit
            if matches!(task.status, TaskStatus::AiReview | TaskStatus::HumanReview | TaskStatus::Done | TaskStatus::PrCreated) {
                let task_id = task.id.to_string();
                let task_title = task.title.clone();
                spawn_local(async move {
                    diff_title.set(format!("Diff: {}", task_title));
                    match get_task_diff(task_id.clone()).await {
                        Ok(diff) => diff_content.set(diff),
                        Err(e) => {
                            toast::error(format!("Failed to load diff: {}", e));
                            return;
                        }
                    }
                    match get_task_diff_stat(task_id).await {
                        Ok(stat) => diff_stat_content.set(stat),
                        Err(_) => diff_stat_content.set(String::new()),
                    }
                    show_diff_modal.set(true);
                });
            } else if matches!(task.status, TaskStatus::InProgress) {
                // InProgress — show info toast, task is running
                toast::info(format!("'{}' is currently running", task.title));
            } else {
                // Backlog, Queue, Error — open edit modal
                set_modal_mode.set(TaskEditMode::Edit(Box::new(task)));
                set_show_modal.set(true);
            }
        }
    });

    // Context menu edit handler
    let on_context_edit = Callback::new({
        move |task: Task| {
            set_modal_mode.set(TaskEditMode::Edit(Box::new(task)));
            set_show_modal.set(true);
        }
    });

    // Context menu move handler - update local state
    let on_context_move = Callback::new({
        move |(task, new_status): (Task, TaskStatus)| {
            set_tasks_signal.update(|tasks| {
                if let Some(t) = tasks.iter_mut().find(|t| t.id == task.id) {
                    t.status = new_status;
                }
            });
        }
    });

    let on_pr_created = Callback::new({
        move |updated_task: Task| {
            set_tasks_signal.update(|tasks| {
                if let Some(task) = tasks.iter_mut().find(|t| t.id == updated_task.id) {
                    task.pr_url = updated_task.pr_url.clone();
                    task.status = updated_task.status.clone();
                    task.updated_at = updated_task.updated_at;
                }
            });
        }
    });

    // Refresh PR state for any task with a linked PR on mount, so PrCreated
    // tasks whose PR was merged externally get moved to Done.
    {
        let initial_pr_task_ids: Vec<String> = tasks_signal
            .get_untracked()
            .iter()
            .filter(|t| t.pr_url.is_some() || t.external_refs.iter().any(|r| r.is_pr()))
            .map(|t| t.id.to_string())
            .collect();
        for tid in initial_pr_task_ids {
            spawn_local(async move {
                if let Ok(Some(updated)) = refresh_task_pr_state(tid).await {
                    // Only propagate if status or refs actually changed — a no-op
                    // update would still trigger reactive re-renders and could
                    // tear down open modals/menus.
                    let snapshot = tasks_signal.get_untracked();
                    let needs_update = snapshot.iter().any(|t| {
                        t.id == updated.id
                            && (t.status != updated.status
                                || t.external_refs != updated.external_refs)
                    });
                    if needs_update {
                        set_tasks_signal.update(|tasks| {
                            if let Some(t) = tasks.iter_mut().find(|t| t.id == updated.id) {
                                t.status = updated.status.clone();
                                t.external_refs = updated.external_refs.clone();
                                t.updated_at = updated.updated_at;
                            }
                        });
                    }
                }
            });
        }
    }

    let on_analyze_pr_comments = Callback::new({
        move |task: Task| {
            pr_review_task.set(Some(task.clone()));
            pr_review_error.set(None);
            pr_review_last_apply.set(task.pr_review_plan.as_ref().and_then(|p| p.last_apply.clone()));
            // Reuse cached plan only when it actually has content. An empty
            // cache (from a prior empty-fetch or older app version) would block
            // the backend call and the user could never refresh by reopening.
            let has_useful_cache = task.pr_review_plan.as_ref()
                .map(|p| !p.comments.is_empty() || !p.items.is_empty())
                .unwrap_or(false);
            if has_useful_cache {
                pr_review_plan.set(task.pr_review_plan.clone());
                pr_review_loading.set(false);
                show_pr_review_modal.set(true);
                return;
            }
            pr_review_plan.set(None);
            pr_review_loading.set(true);
            show_pr_review_modal.set(true);
            spawn_local(async move {
                match analyze_pr_comments(task.id.to_string()).await {
                    Ok(plan) => {
                        pr_review_plan.set(Some(plan));
                        pr_review_error.set(None);
                    }
                    Err(e) => {
                        pr_review_plan.set(None);
                        pr_review_error.set(Some(format!("Failed to analyze PR comments: {}", e)));
                    }
                }
                pr_review_loading.set(false);
            });
        }
    });

    let on_private_email_pr_error = Callback::new({
        move |task: Task| {
            request_pr_push_recovery(
                task,
                show_pr_recovery_modal,
                pr_recovery_task,
                pr_recovery_plan,
                pr_recovery_email,
                pr_recovery_loading,
            );
        }
    });

    // Calculate stats
    let task_stats = move || {
        let tasks = tasks_signal.get();
        let total = tasks.len();
        let in_progress = tasks.iter().filter(|t| t.status == TaskStatus::InProgress).count();
        let done = tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
        (total, in_progress, done)
    };

    view! {
        <div data-testid="kanban-board" class="flex flex-col h-full">
            // Header
            <div class="flex items-center justify-between mb-6 flex-shrink-0">
                <div class="flex items-center gap-4">
                    <div>
                        <h1 class="text-2xl font-bold text-white/90">"Kanban Board"</h1>
                        <p class="text-sm text-white/40 mt-0.5">"Drag and drop tasks to change status"</p>
                    </div>
                    <div class="flex items-center gap-3 ml-4">
                        {move || {
                            let (total, in_progress, done) = task_stats();
                            view! {
                                <div class="flex items-center gap-2 px-3 py-1.5 rounded-lg bg-white/5">
                                    <span class="text-xs text-white/40">"Total:"</span>
                                    <span class="text-sm font-medium text-white/70">{total}</span>
                                </div>
                                <div class="flex items-center gap-2 px-3 py-1.5 rounded-lg bg-blue-500/10">
                                    <span class="text-xs text-blue-400">"Running:"</span>
                                    <span class="text-sm font-medium text-blue-300">{in_progress}</span>
                                </div>
                                <div class="flex items-center gap-2 px-3 py-1.5 rounded-lg bg-emerald-500/10">
                                    <span class="text-xs text-emerald-400">"Done:"</span>
                                    <span class="text-sm font-medium text-emerald-300">{done}</span>
                                </div>
                            }
                        }}
                    </div>
                </div>
                <button
                    data-testid="new-task-button"
                    on:click=on_create_click
                    class="flex items-center gap-2 px-5 py-2.5 rounded-lg bg-gradient-to-r from-blue-500 to-purple-500 hover:from-blue-600 hover:to-purple-600 text-white font-medium transition-all shadow-lg shadow-blue-500/20 hover:shadow-blue-500/30 hover:scale-[1.02] active:scale-[0.98]"
                >
                    <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 4v16m8-8H4" />
                    </svg>
                    "New Task"
                </button>
            </div>

            // Kanban columns
            <div class="flex gap-4 overflow-x-auto pb-4 snap-x snap-mandatory">
                {COLUMNS.iter().map(|column| {
                    let status = column.status.clone();
                    let tasks = tasks_signal;
                    let drag_over = drag_over_column;

                    view! {
                        <KanbanColumn
                            status=status
                            column=column.clone()
                            tasks=tasks.into()
                            dragged_task=dragged_task.into()
                            set_dragged_task=set_dragged_task
                            drag_over_column=drag_over.into()
                            set_drag_over_column=set_drag_over_column
                            drag_over_position=drag_over_position.into()
                            set_drag_over_position=set_drag_over_position
                            set_tasks_signal=set_tasks_signal
                            on_task_click=on_task_click
                            show_context_menu=show_context_menu.into()
                            set_show_context_menu=set_show_context_menu
                            set_context_menu_pos=set_context_menu_pos
                            set_context_menu_task=set_context_menu_task
                            selected_tasks=selected_tasks_signal
                            set_selected_tasks=set_selected_tasks_writer
                            show_diff_modal=show_diff_modal
                            diff_content=diff_content
                            diff_stat_content=diff_stat_content
                            diff_title=diff_title
                            on_pr_created=on_pr_created
                            on_analyze_pr_comments=on_analyze_pr_comments
                            show_pr_candidates_modal=show_pr_candidates_modal
                            pr_candidate_task=pr_candidate_task
                            pr_candidates=pr_candidates
                        />
                    }
                }).collect::<Vec<_>>()}
            </div>

            // Task edit modal
            <TaskEditModal
                show=Signal::from(show_modal)
                set_show=set_show_modal
                mode=Signal::from(modal_mode)
                project_id=project_id_modal
                on_save=on_task_save
                on_delete=on_task_delete
            />
            
            // Context menu (rendered at Kanban level, shared by all cards)
            <TaskContextMenu
                show=Signal::from(show_context_menu)
                set_show=set_show_context_menu
                position=Signal::from(context_menu_pos)
                task=Signal::derive(move || context_menu_task.get())
                on_edit=on_context_edit
                on_delete=on_task_delete
                on_move=on_context_move
                on_pr_created=on_pr_created
                on_analyze_pr_comments=on_analyze_pr_comments
                on_private_email_pr_error=on_private_email_pr_error
            />

            // Diff modal (shared across all task cards)
            {move || {
                let title = diff_title.get();
                view! {
                    <DiffModal
                        show=show_diff_modal
                        diff=diff_content.into()
                        stat=diff_stat_content.into()
                        title=title
                    />
                }
            }}

            <PrReviewModal
                show=show_pr_review_modal
                task=pr_review_task
                plan=pr_review_plan
                loading=pr_review_loading
                applying=pr_review_applying
                error=pr_review_error
                auto_push=pr_review_auto_push
                auto_reply=pr_review_auto_reply
                dry_run=pr_review_dry_run
                last_apply=pr_review_last_apply
                set_tasks=set_tasks_signal
            />

            <PrCandidatesModal
                show=show_pr_candidates_modal
                task=pr_candidate_task
                candidates=pr_candidates
                on_pr_created=on_pr_created
            />

            <PrPushRecoveryModal
                show=show_pr_recovery_modal
                task=pr_recovery_task
                plan=pr_recovery_plan
                email=pr_recovery_email
                loading=pr_recovery_loading
                on_pr_created=on_pr_created
            />
        </div>
    }
}

fn request_pr_push_recovery(
    task_value: Task,
    show: RwSignal<bool>,
    task: RwSignal<Option<Task>>,
    plan: RwSignal<Option<PrPushRecoveryPlan>>,
    email: RwSignal<String>,
    loading: RwSignal<bool>,
) {
    task.set(Some(task_value.clone()));
    plan.set(None);
    email.set(String::new());
    loading.set(true);
    show.set(true);

    spawn_local(async move {
        match get_pr_push_recovery(task_value.id.to_string()).await {
            Ok(recovery_plan) => {
                email.set(recovery_plan.suggested_email.clone().unwrap_or_default());
                plan.set(Some(recovery_plan));
            }
            Err(e) => {
                show.set(false);
                toast::error(format!("Could not prepare PR recovery: {}", e));
            }
        }
        loading.set(false);
    });
}

#[component]
fn PrCandidatesModal(
    show: RwSignal<bool>,
    task: RwSignal<Option<Task>>,
    candidates: RwSignal<Vec<PrCandidate>>,
    on_pr_created: Callback<Task>,
) -> impl IntoView {
    view! {
        <Show when=move || show.get()>
            <div
                class="fixed inset-0 z-[70] bg-black/60 flex items-center justify-center p-4"
                on:click=move |_| show.set(false)
            >
                <div
                    class="w-full max-w-2xl bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl overflow-hidden"
                    on:click=move |e| e.stop_propagation()
                >
                    <div class="flex items-center justify-between px-5 py-4 border-b border-white/10">
                        <div>
                            <h2 class="text-lg font-semibold text-white/90">"Link Existing PR"</h2>
                            <p class="text-xs text-white/40">"Choose the PR that belongs to this task."</p>
                        </div>
                        <button
                            class="p-2 rounded-md hover:bg-white/10 text-white/60 transition-colors"
                            on:click=move |_| show.set(false)
                            aria-label="Close"
                        >
                            <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                            </svg>
                        </button>
                    </div>

                    <div class="p-4 space-y-2 max-h-[60vh] overflow-y-auto">
                        {move || candidates.get().into_iter().map(|candidate| {
                            let candidate_for_click = candidate.clone();
                            view! {
                                <button
                                    class="w-full text-left p-3 rounded-lg bg-white/[0.04] border border-white/10 hover:border-yellow-500/50 hover:bg-yellow-500/10 transition-colors"
                                    on:click=move |_| {
                                        let Some(task_value) = task.get() else {
                                            return;
                                        };
                                        let pr_url = candidate_for_click.url.clone();
                                        show.set(false);
                                        spawn_local(async move {
                                            match link_existing_pr(task_value.id.to_string(), pr_url.clone()).await {
                                                Ok(Some(updated)) => {
                                                    on_pr_created.run(updated);
                                                    toast::success(format!("Linked PR: {}", pr_url));
                                                }
                                                Ok(None) => toast::error("Task not found while linking PR".to_string()),
                                                Err(e) => toast::error(format!("Failed to link PR: {}", e)),
                                            }
                                        });
                                    }
                                >
                                    <div class="flex items-center justify-between gap-3">
                                        <div class="min-w-0">
                                            <div class="text-sm font-medium text-white/90 truncate">
                                                {format!("#{} {}", candidate.number, candidate.title)}
                                            </div>
                                            <div class="text-xs text-white/45 truncate">
                                                {format!("{} - {} - {}", candidate.state, candidate.head_ref_name, candidate.reason)}
                                            </div>
                                        </div>
                                        <span class="text-xs text-yellow-300 flex-shrink-0">"Link"</span>
                                    </div>
                                </button>
                            }
                        }).collect_view()}
                    </div>
                </div>
            </div>
        </Show>
    }
}

#[component]
fn PrPushRecoveryModal(
    show: RwSignal<bool>,
    task: RwSignal<Option<Task>>,
    plan: RwSignal<Option<PrPushRecoveryPlan>>,
    email: RwSignal<String>,
    loading: RwSignal<bool>,
    on_pr_created: Callback<Task>,
) -> impl IntoView {
    let on_confirm = move |_| {
        let Some(task_value) = task.get() else {
            return;
        };
        let author_email = email.get();
        if author_email.trim().is_empty() || !author_email.contains('@') {
            toast::error("Enter a valid GitHub noreply email".to_string());
            return;
        }

        loading.set(true);
        spawn_local(async move {
            match recover_private_email_and_create_pr(task_value.id.to_string(), author_email).await {
                Ok(url) => {
                    let mut updated = task_value;
                    updated.pr_url = Some(url.clone());
                    updated.status = TaskStatus::PrCreated;
                    on_pr_created.run(updated);
                    show.set(false);
                    toast::success(format!("PR linked: {}", url));
                }
                Err(e) => toast::error(format!("Failed to recover PR push: {}", e)),
            }
            loading.set(false);
        });
    };

    view! {
        <Show when=move || show.get()>
            <div
                class="fixed inset-0 z-[75] bg-black/60 flex items-center justify-center p-4"
                on:click=move |_| {
                    if !loading.get() {
                        show.set(false);
                    }
                }
            >
                <div
                    class="w-full max-w-xl bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl overflow-hidden"
                    on:click=move |e| e.stop_propagation()
                >
                    <div class="flex items-center justify-between px-5 py-4 border-b border-white/10">
                        <div class="min-w-0">
                            <h2 class="text-lg font-semibold text-white/90">"Fix PR Push"</h2>
                            <p class="text-xs text-white/40 truncate">
                                {move || task.get().map(|t| t.title).unwrap_or_else(|| "GitHub rejected the branch push".to_string())}
                            </p>
                        </div>
                        <button
                            class="p-2 rounded-md hover:bg-white/10 text-white/60 transition-colors"
                            on:click=move |_| show.set(false)
                            disabled=move || loading.get()
                            aria-label="Close"
                        >
                            <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                            </svg>
                        </button>
                    </div>

                    <div class="p-5 space-y-4">
                        <Show
                            when=move || !loading.get() || plan.get().is_some()
                            fallback=|| view! {
                                <div class="py-10 text-center text-sm text-white/50">"Inspecting task branch..."</div>
                            }
                        >
                            {move || plan.get().map(|p| view! {
                                <div class="space-y-3">
                                    <div class="rounded-lg border border-yellow-500/25 bg-yellow-500/10 p-3 text-sm text-yellow-100">
                                        "GitHub rejected this push because the task commit author email is private. SlashIt can set the repo-local author email, rewrite only the task branch tip author, then retry without creating a duplicate PR."
                                    </div>

                                    <div class="grid grid-cols-[110px_1fr] gap-x-3 gap-y-2 text-xs">
                                        <span class="text-white/40">"Branch"</span>
                                        <span class="text-white/80 truncate">{p.branch_name.clone()}</span>
                                        <span class="text-white/40">"Commit"</span>
                                        <span class="text-white/80 truncate">{format!("{} {}", p.commit_sha.chars().take(12).collect::<String>(), p.commit_subject)}</span>
                                        <span class="text-white/40">"Current author"</span>
                                        <span class="text-white/80 truncate">{format!("{} <{}>", p.author_name, p.author_email)}</span>
                                    </div>

                                    <label class="block space-y-1">
                                        <span class="text-xs font-medium text-white/60">"GitHub noreply email"</span>
                                        <input
                                            class="w-full px-3 py-2 bg-white/[0.04] border border-white/10 rounded-lg text-sm text-white/90 focus:outline-none focus:border-yellow-500/50"
                                            prop:value=move || email.get()
                                            on:input=move |ev| email.set(event_target_value(&ev))
                                            placeholder="12345+user@users.noreply.github.com"
                                        />
                                    </label>
                                </div>
                            })}
                        </Show>
                    </div>

                    <div class="flex justify-end gap-2 px-5 py-4 border-t border-white/10">
                        <button
                            class="px-3 py-2 text-sm rounded-lg text-white/70 hover:bg-white/10 transition-colors"
                            on:click=move |_| show.set(false)
                            disabled=move || loading.get()
                        >
                            "Cancel"
                        </button>
                        <button
                            class="px-3 py-2 text-sm rounded-lg bg-yellow-500 text-black font-medium hover:bg-yellow-400 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
                            on:click=on_confirm
                            disabled=move || loading.get() || plan.get().is_none()
                        >
                            {move || if loading.get() { "Fixing..." } else { "Fix and Create PR" }}
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}

#[component]
fn PrReviewModal(
    show: RwSignal<bool>,
    task: RwSignal<Option<Task>>,
    plan: RwSignal<Option<PrReviewPlan>>,
    loading: RwSignal<bool>,
    applying: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    auto_push: RwSignal<bool>,
    auto_reply: RwSignal<bool>,
    dry_run: RwSignal<bool>,
    last_apply: RwSignal<Option<PrReviewApplyResult>>,
    set_tasks: WriteSignal<Vec<Task>>,
) -> impl IntoView {
    let show_raw_comments = RwSignal::new(false);
    // Default true once an apply has happened — focus on what's new since.
    let show_only_new = RwSignal::new(false);
    let discussing = RwSignal::new(false);

    // Reactive count of items that are both Fix-decisioned and user-approved.
    let approved_fix_count = move || plan.get()
        .map(|p| p.items.iter()
            .filter(|i| i.approved && matches!(i.decision, PrReviewDecisionKind::Fix))
            .count())
        .unwrap_or(0);

    // Question items the user has annotated, ready to send back to the agent.
    let pending_discussion_count = move || plan.get()
        .map(|p| p.items.iter()
            .filter(|i| matches!(i.decision, PrReviewDecisionKind::Question)
                && !i.user_note.trim().is_empty())
            .count())
        .unwrap_or(0);

    // Filter: when "only new" is on and we have a previous apply, hide items
    // whose related comment hasn't been updated since the last apply.
    let item_visible = move |it: &PrReviewItem, p: &PrReviewPlan| -> bool {
        if !show_only_new.get() { return true; }
        let Some(la) = last_apply.get() else { return true; };
        let Some(cid) = it.comment_id else { return true; };
        let Some(comment) = p.comments.iter().find(|c| c.id == Some(cid)) else { return true; };
        let ts = comment.updated_at.or(comment.created_at);
        ts.map(|t| t > la.applied_at).unwrap_or(true)
    };

    let trigger_analyze = move |force: bool| {
        let Some(task_value) = task.get() else { return; };
        if !force {
            let cached = task_value.pr_review_plan.clone()
                .filter(|p| !p.comments.is_empty() || !p.items.is_empty());
            if let Some(cached) = cached {
                last_apply.set(cached.last_apply.clone());
                show_only_new.set(cached.last_apply.is_some());
                plan.set(Some(cached));
                error.set(None);
                return;
            }
        }
        plan.set(None);
        error.set(None);
        loading.set(true);
        spawn_local(async move {
            match analyze_pr_comments(task_value.id.to_string()).await {
                Ok(p) => {
                    show_only_new.set(p.last_apply.is_some());
                    plan.set(Some(p));
                    error.set(None);
                }
                Err(e) => {
                    plan.set(None);
                    error.set(Some(format!("Failed to analyze PR comments: {}", e)));
                }
            }
            loading.set(false);
        });
    };

    let on_apply = move |_| {
        let Some(task_value) = task.get() else { return; };
        let Some(current_plan) = plan.get() else {
            toast::error("No plan loaded".to_string());
            return;
        };
        if approved_fix_count() == 0 {
            toast::error("Approve at least one Fix item before applying".to_string());
            return;
        }

        let opts = AddressPrReviewOptions {
            auto_push: auto_push.get(),
            auto_reply: auto_reply.get(),
            dry_run: dry_run.get(),
        };
        applying.set(true);
        let task_id = task_value.id.to_string();
        spawn_local(async move {
            match address_pr_review(task_id.clone(), current_plan, opts).await {
                Ok(result) => {
                    let pushed = result.pushed;
                    let replies = result.replies_posted;
                    let errs = result.reply_errors.len();
                    last_apply.set(Some(result));
                    let mut msg = String::from("Applied. ");
                    if pushed { msg.push_str("Pushed. "); }
                    if replies > 0 { msg.push_str(&format!("Posted {} replies. ", replies)); }
                    if errs > 0 { msg.push_str(&format!("{} reply errors.", errs)); }
                    toast::success(msg);
                    // Refresh task PR state so the card moves if reviewer reacts.
                    let task_id_inner = task_id.clone();
                    spawn_local(async move {
                        if let Ok(Some(updated)) = refresh_task_pr_state(task_id_inner).await {
                            set_tasks.update(|tasks| {
                                if let Some(t) = tasks.iter_mut().find(|t| t.id == updated.id) {
                                    *t = updated;
                                }
                            });
                        }
                    });
                }
                Err(e) => toast::error(format!("Failed to apply PR fixes: {}", e)),
            }
            applying.set(false);
        });
    };

    let on_discuss = move |_| {
        let Some(task_value) = task.get() else { return; };
        let Some(current_plan) = plan.get() else { return; };
        if pending_discussion_count() == 0 {
            toast::error("Add a note to at least one Question item first".to_string());
            return;
        }
        discussing.set(true);
        let task_id = task_value.id.to_string();
        spawn_local(async move {
            match discuss_pr_review_questions(task_id, current_plan).await {
                Ok(updated) => {
                    plan.set(Some(updated));
                    error.set(None);
                    toast::success("Agent re-evaluated the questions".to_string());
                }
                Err(e) => toast::error(format!("Failed to discuss: {}", e)),
            }
            discussing.set(false);
        });
    };

    view! {
        <Show when=move || show.get()>
            <div
                class="fixed inset-0 z-[70] bg-black/60 flex items-center justify-center p-4"
                on:click=move |_| {
                    if !loading.get() && !applying.get() && !discussing.get() {
                        show.set(false);
                    }
                }
            >
                <div
                    class="w-full max-w-6xl max-h-[90vh] bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl overflow-hidden flex flex-col"
                    on:click=move |e| e.stop_propagation()
                >
                    <div class="flex items-start justify-between px-5 py-4 border-b border-white/10 gap-4">
                        <div class="min-w-0 flex-1">
                            <div class="flex items-center gap-2 flex-wrap">
                                <h2 class="text-lg font-semibold text-white/90">"PR Comment Review"</h2>
                                {move || plan.get().and_then(|p| p.review_decision).map(|d| {
                                    let (bg, fg, label) = match d.as_str() {
                                        "APPROVED" => ("bg-emerald-500/15", "text-emerald-300", "Approved"),
                                        "CHANGES_REQUESTED" => ("bg-amber-500/15", "text-amber-300", "Changes requested"),
                                        "REVIEW_REQUIRED" => ("bg-blue-500/15", "text-blue-300", "Review required"),
                                        _ => ("bg-white/10", "text-white/60", "Commented"),
                                    };
                                    view! {
                                        <span class=format!("text-[10px] px-2 py-0.5 rounded-full font-medium {} {}", bg, fg)>
                                            {label}
                                        </span>
                                    }
                                })}
                            </div>
                            <p class="text-xs text-white/40 truncate mt-0.5">
                                {move || task.get().map(|t| t.title).unwrap_or_else(|| "Review comments before applying fixes".to_string())}
                            </p>
                        </div>
                        <button
                            class="p-2 rounded-md hover:bg-white/10 text-white/60 transition-colors flex-shrink-0"
                            on:click=move |_| show.set(false)
                            disabled=move || applying.get()
                            aria-label="Close"
                        >
                            <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                            </svg>
                        </button>
                    </div>

                    <div class="px-5 py-3 overflow-y-auto flex-1 space-y-4">
                        <Show
                            when=move || !loading.get()
                            fallback=|| view! {
                                <div class="py-12 text-center text-sm text-white/50">
                                    <div class="inline-flex items-center gap-2">
                                        <svg class="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24">
                                            <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"/>
                                            <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"/>
                                        </svg>
                                        "Analyzing PR comments..."
                                    </div>
                                </div>
                            }
                        >
                            {move || error.get().map(|msg| view! {
                                <div class="px-3 py-2 rounded-md border border-red-500/30 bg-red-500/10 text-xs text-red-200">
                                    {msg}
                                </div>
                            })}

                            {move || last_apply.get().map(|res| {
                                let summary_class = if res.reply_errors.is_empty() {
                                    "border-emerald-500/30 bg-emerald-500/10 text-emerald-100"
                                } else {
                                    "border-amber-500/30 bg-amber-500/10 text-amber-100"
                                };
                                view! {
                                    <div class=format!("px-3 py-2 rounded-md border text-xs space-y-1 {}", summary_class)>
                                        <div class="font-medium">
                                            {format!(
                                                "Last apply at {}: fixed {}, pushed {}, replies {}{}",
                                                res.applied_at.format("%H:%M:%S"),
                                                res.fixed_ids.len(),
                                                if res.pushed { "yes" } else { "no" },
                                                res.replies_posted,
                                                if res.reply_errors.is_empty() { String::new() } else { format!(", {} errors", res.reply_errors.len()) },
                                            )}
                                        </div>
                                        {(!res.reply_errors.is_empty()).then(|| view! {
                                            <ul class="list-disc list-inside opacity-80">
                                                {res.reply_errors.iter().take(5).map(|e| view! { <li>{e.clone()}</li> }).collect_view()}
                                            </ul>
                                        })}
                                    </div>
                                }
                            })}

                            // Raw comments collapsible
                            {move || plan.get().map(|p| {
                                let count = p.comments.len();
                                view! {
                                    <div class="rounded-md border border-white/10">
                                        <button
                                            class="w-full text-left px-3 py-2 text-xs text-white/70 hover:bg-white/5 flex items-center justify-between"
                                            on:click=move |_| show_raw_comments.update(|v| *v = !*v)
                                        >
                                            <span>{format!("PR comments ({})", count)}</span>
                                            <span class="text-white/40">
                                                {move || if show_raw_comments.get() { "Hide" } else { "Show" }}
                                            </span>
                                        </button>
                                        <Show when=move || show_raw_comments.get()>
                                            <div class="border-t border-white/10 max-h-60 overflow-y-auto p-3 space-y-2 text-xs">
                                                {p.comments.iter().map(|c| {
                                                    let kind = match c.kind {
                                                        PrCommentKind::Inline => "inline",
                                                        PrCommentKind::Review => "review",
                                                        PrCommentKind::Conversation => "conversation",
                                                    };
                                                    let loc = match (&c.path, c.line) {
                                                        (Some(p), Some(l)) => format!("{}:{}", p, l),
                                                        (Some(p), None) => p.clone(),
                                                        _ => "PR-level".to_string(),
                                                    };
                                                    view! {
                                                        <div class="rounded bg-white/[0.03] p-2">
                                                            <div class="text-white/50 font-mono">
                                                                {format!("[{}] {} - {}", kind, c.author, loc)}
                                                            </div>
                                                            <div class="text-white/80 whitespace-pre-wrap mt-1">{c.body.clone()}</div>
                                                        </div>
                                                    }
                                                }).collect_view()}
                                            </div>
                                        </Show>
                                    </div>
                                }
                            })}

                            // Items list
                            {move || plan.get().map(|p| {
                                if p.comments.is_empty() {
                                    return view! {
                                        <div class="rounded-md border border-white/10 bg-white/[0.03] p-4 text-sm text-white/60">
                                            <div class="font-medium text-white/80 mb-1">"No reviewer comments yet"</div>
                                            <p>{format!(
                                                "{} did not receive any review comments, conversation comments, or inline comments. Click Re-analyze once a reviewer leaves feedback.",
                                                p.pr_url,
                                            )}</p>
                                        </div>
                                    }.into_any();
                                }
                                if p.items.is_empty() {
                                    return view! {
                                        <div class="rounded-md border border-amber-500/20 bg-amber-500/[0.04] p-4 text-sm text-white/70">
                                            <div class="font-medium text-amber-200 mb-1">"Triage returned no structured items"</div>
                                            <p class="text-white/60">"The agent could not produce a JSON plan. Raw output below — Re-analyze, or paste a manual decision into the items list above."</p>
                                            <pre class="mt-2 text-xs whitespace-pre-wrap font-mono text-white/50 max-h-60 overflow-y-auto">{p.raw_plan.clone()}</pre>
                                        </div>
                                    }.into_any();
                                }
                                let total = p.items.len();
                                let visible: Vec<(usize, PrReviewItem)> = p.items.iter().enumerate()
                                    .filter(|(_, it)| item_visible(it, &p))
                                    .map(|(idx, it)| (idx, it.clone()))
                                    .collect();
                                let visible_count = visible.len();
                                let hidden = total.saturating_sub(visible_count);
                                let items_view = visible.into_iter().map(|(idx, item)| {
                                    render_review_item(idx, item, plan)
                                }).collect_view();
                                view! {
                                    <div class="space-y-2">
                                        {(hidden > 0).then(|| view! {
                                            <div class="text-[11px] text-white/45 px-1">
                                                {format!("{} hidden (already addressed before last apply)", hidden)}
                                            </div>
                                        })}
                                        {items_view}
                                    </div>
                                }.into_any()
                            })}
                        </Show>
                    </div>

                    <div class="flex items-center justify-between gap-3 px-5 py-3 border-t border-white/10 bg-white/[0.02]">
                        <div class="flex items-center gap-4 text-xs text-white/70">
                            <label class="flex items-center gap-1.5 cursor-pointer">
                                <input
                                    type="checkbox"
                                    class="accent-yellow-500"
                                    prop:checked=move || auto_push.get()
                                    on:change=move |ev| auto_push.set(event_target_checked(&ev))
                                />
                                "Auto-push branch"
                            </label>
                            <label class="flex items-center gap-1.5 cursor-pointer">
                                <input
                                    type="checkbox"
                                    class="accent-yellow-500"
                                    prop:checked=move || auto_reply.get()
                                    on:change=move |ev| auto_reply.set(event_target_checked(&ev))
                                />
                                "Auto-reply on PR"
                            </label>
                            <label
                                class="flex items-center gap-1.5 cursor-pointer"
                                title="Hide items whose comment hasn't changed since the last apply"
                            >
                                <input
                                    type="checkbox"
                                    class="accent-yellow-500"
                                    prop:checked=move || show_only_new.get()
                                    on:change=move |ev| show_only_new.set(event_target_checked(&ev))
                                    disabled=move || last_apply.get().is_none()
                                />
                                "Only new since last apply"
                            </label>
                            <label
                                class="flex items-center gap-1.5 cursor-pointer"
                                title="Run the agent read-only: describe what it would change without editing, pushing, or replying"
                            >
                                <input
                                    type="checkbox"
                                    class="accent-yellow-500"
                                    prop:checked=move || dry_run.get()
                                    on:change=move |ev| dry_run.set(event_target_checked(&ev))
                                />
                                "Dry run (no edits)"
                            </label>
                        </div>
                        <div class="flex items-center gap-2">
                            <button
                                class="px-3 py-1.5 rounded-lg text-xs text-white/70 hover:bg-white/10 transition-colors disabled:opacity-40"
                                on:click=on_discuss
                                disabled=move || loading.get() || applying.get() || discussing.get() || pending_discussion_count() == 0
                                title="Send Question items with notes back to the agent"
                            >
                                {move || if discussing.get() {
                                    "Re-discussing...".to_string()
                                } else {
                                    format!("Re-discuss {} questions", pending_discussion_count())
                                }}
                            </button>
                            <button
                                class="px-3 py-1.5 rounded-lg text-xs text-white/70 hover:bg-white/10 transition-colors disabled:opacity-40"
                                on:click=move |_| trigger_analyze(true)
                                disabled=move || loading.get() || applying.get() || discussing.get()
                                title="Run the triage agent again, ignoring the cached plan"
                            >
                                "Re-analyze"
                            </button>
                            <button
                                class="px-3 py-1.5 rounded-lg text-xs text-white/60 hover:bg-white/10 transition-colors disabled:opacity-40"
                                on:click=move |_| show.set(false)
                                disabled=move || applying.get() || discussing.get()
                            >
                                "Close"
                            </button>
                            <button
                                class="px-4 py-1.5 rounded-lg text-sm bg-yellow-500 text-black font-medium hover:bg-yellow-400 transition-colors disabled:opacity-40 disabled:hover:bg-yellow-500"
                                on:click=on_apply
                                disabled=move || loading.get() || applying.get() || discussing.get() || approved_fix_count() == 0
                            >
                                {move || if applying.get() {
                                    if dry_run.get() { "Planning...".to_string() } else { "Applying...".to_string() }
                                } else if dry_run.get() {
                                    format!("Dry-run {} fixes", approved_fix_count())
                                } else {
                                    format!("Apply {} fixes", approved_fix_count())
                                }}
                            </button>
                        </div>
                    </div>
                </div>
            </div>
        </Show>
    }
}

fn render_review_item(
    idx: usize,
    item: PrReviewItem,
    plan: RwSignal<Option<PrReviewPlan>>,
) -> impl IntoView {
    let comment_id = item.comment_id;
    let related = plan.get_untracked()
        .and_then(|p| comment_id.and_then(|id| p.comments.into_iter().find(|c| c.id == Some(id))));

    // Apply a mutation to the item at `idx` inside the (possibly absent) plan.
    let update_item = move |mutate: &dyn Fn(&mut PrReviewItem)| {
        plan.update(|opt| {
            if let Some(it) = opt.as_mut().and_then(|p| p.items.get_mut(idx)) {
                mutate(it);
            }
        });
    };

    let toggle_approved = move |ev: leptos::ev::Event| {
        let v = event_target_checked(&ev);
        update_item(&|it| it.approved = v);
    };
    let on_decision_change = move |ev: leptos::ev::Event| {
        let decision = match event_target_value(&ev).as_str() {
            "fix" => PrReviewDecisionKind::Fix,
            "skip" => PrReviewDecisionKind::Skip,
            _ => PrReviewDecisionKind::Question,
        };
        let approved = matches!(decision, PrReviewDecisionKind::Fix);
        update_item(&|it| {
            it.decision = decision.clone();
            it.approved = approved;
        });
    };
    let on_reasoning_input = move |ev: leptos::ev::Event| {
        let v = event_target_value(&ev);
        update_item(&|it| it.reasoning = v.clone());
    };
    let on_change_input = move |ev: leptos::ev::Event| {
        let v = event_target_value(&ev);
        update_item(&|it| it.proposed_change = v.clone());
    };
    let on_user_note_input = move |ev: leptos::ev::Event| {
        let v = event_target_value(&ev);
        update_item(&|it| it.user_note = v.clone());
    };

    let location = related.as_ref().map(|c| match (&c.path, c.line) {
        (Some(p), Some(l)) => format!("{}:{}", p, l),
        (Some(p), None) => p.clone(),
        _ => "PR-level".to_string(),
    }).unwrap_or_else(|| "PR-level".to_string());
    let original_body = related.as_ref().map(|c| c.body.clone());
    let author = related.as_ref().map(|c| c.author.clone()).unwrap_or_default();

    let decision_class = match item.decision {
        PrReviewDecisionKind::Fix => "border-emerald-500/30 bg-emerald-500/[0.04]",
        PrReviewDecisionKind::Skip => "border-white/10 bg-white/[0.02]",
        PrReviewDecisionKind::Question => "border-amber-500/30 bg-amber-500/[0.04]",
    };

    let summary = item.summary.clone();
    let reasoning = item.reasoning.clone();
    let proposed = item.proposed_change.clone();
    let user_note = item.user_note.clone();
    let approved_init = item.approved;
    let is_question = matches!(item.decision, PrReviewDecisionKind::Question);
    let decision_value = match item.decision {
        PrReviewDecisionKind::Fix => "fix",
        PrReviewDecisionKind::Skip => "skip",
        PrReviewDecisionKind::Question => "question",
    };

    view! {
        <div class=format!("rounded-lg border {} p-3 space-y-2", decision_class)>
            <div class="flex items-start gap-3">
                <input
                    type="checkbox"
                    class="mt-1 accent-yellow-500"
                    prop:checked=approved_init
                    on:change=toggle_approved
                    title="Approve to include in apply"
                />
                <div class="flex-1 min-w-0 space-y-1">
                    <div class="flex items-center gap-2 flex-wrap text-xs text-white/45">
                        <span class="font-mono">{location}</span>
                        {(!author.is_empty()).then(|| view! { <span>"-"</span> <span>{author}</span> })}
                        {comment_id.map(|id| view! { <span class="opacity-60">{format!("(id {})", id)}</span> })}
                    </div>
                    <div class="text-sm text-white/90 font-medium">{summary}</div>
                </div>
                <select
                    class="text-xs bg-white/[0.05] border border-white/10 rounded px-2 py-1 text-white/80 flex-shrink-0"
                    on:change=on_decision_change
                >
                    <option value="fix" selected=decision_value == "fix">"Fix"</option>
                    <option value="skip" selected=decision_value == "skip">"Skip"</option>
                    <option value="question" selected=decision_value == "question">"Question"</option>
                </select>
            </div>

            {original_body.map(|body| view! {
                <details class="text-xs text-white/55">
                    <summary class="cursor-pointer hover:text-white/80">"Original comment"</summary>
                    <pre class="mt-1 whitespace-pre-wrap font-mono opacity-80">{body}</pre>
                </details>
            })}

            <div class="grid grid-cols-1 lg:grid-cols-2 gap-3">
                <label class="block">
                    <span class="text-[10px] uppercase tracking-wide text-white/40">"Reply / reasoning"</span>
                    <textarea
                        class="w-full mt-1 bg-white/[0.04] border border-white/10 rounded p-2 text-xs text-white/85 font-mono resize-y focus:outline-none focus:border-yellow-500/50"
                        rows="2"
                        on:input=on_reasoning_input
                        prop:value=reasoning
                        placeholder="What you would tell the reviewer. Posted as the PR reply when auto-reply is on."
                    ></textarea>
                </label>

                <label class="block">
                    <span class="text-[10px] uppercase tracking-wide text-white/40">"Proposed change"</span>
                    <textarea
                        class="w-full mt-1 bg-white/[0.04] border border-white/10 rounded p-2 text-xs text-white/85 font-mono resize-y focus:outline-none focus:border-yellow-500/50"
                        rows="2"
                        on:input=on_change_input
                        prop:value=proposed
                        placeholder="What the agent will change if approved."
                    ></textarea>
                </label>
            </div>

            {is_question.then(|| view! {
                <label class="block">
                    <span class="text-[10px] uppercase tracking-wide text-amber-300/80">
                        "Your note for the agent (re-discuss)"
                    </span>
                    <textarea
                        class="w-full mt-1 bg-amber-500/[0.04] border border-amber-500/30 rounded p-2 text-xs text-white/85 resize-y focus:outline-none focus:border-amber-400/60"
                        rows="2"
                        on:input=on_user_note_input
                        prop:value=user_note
                        placeholder="Tell the agent what to do (e.g. 'go ahead and fix it', or 'no, the issue is X — re-evaluate')."
                    ></textarea>
                </label>
            })}
        </div>
    }
}

#[component]
fn KanbanColumn(
    status: TaskStatus,
    column: ColumnData,
    tasks: Signal<Vec<Task>>,
    dragged_task: Signal<Option<(String, TaskStatus)>>,
    set_dragged_task: WriteSignal<Option<(String, TaskStatus)>>,
    drag_over_column: Signal<Option<TaskStatus>>,
    set_drag_over_column: WriteSignal<Option<TaskStatus>>,
    drag_over_position: Signal<Option<(TaskStatus, Option<String>)>>,
    set_drag_over_position: WriteSignal<Option<(TaskStatus, Option<String>)>>,
    set_tasks_signal: WriteSignal<Vec<Task>>,
    on_task_click: Callback<Task>,
    show_context_menu: Signal<bool>,
    set_show_context_menu: WriteSignal<bool>,
    set_context_menu_pos: WriteSignal<(i32, i32)>,
    set_context_menu_task: WriteSignal<Option<Task>>,
    selected_tasks: Signal<Vec<Uuid>>,
    set_selected_tasks: WriteSignal<Vec<Uuid>>,
    show_diff_modal: RwSignal<bool>,
    diff_content: RwSignal<String>,
    diff_stat_content: RwSignal<String>,
    diff_title: RwSignal<String>,
    on_pr_created: Callback<Task>,
    on_analyze_pr_comments: Callback<Task>,
    show_pr_candidates_modal: RwSignal<bool>,
    pr_candidate_task: RwSignal<Option<Task>>,
    pr_candidates: RwSignal<Vec<PrCandidate>>,
) -> impl IntoView {
    let status_drop = status.clone();
    let status_tasks = status.clone();
    let status_count = status.clone();
    let column_clone = column.clone();
    let tasks_signal = tasks;

    let on_drop = move |e: leptos::ev::DragEvent| {
        e.prevent_default();
        
        // Get the drop position before clearing state
        let drop_position = drag_over_position.get();
        set_drag_over_column.set(None);
        set_drag_over_position.set(None);

        if let Some((task_id, current_status)) = dragged_task.get() {
            let task_id_clone = task_id.clone();
            let new_status = status_drop.clone();
            let set_tasks_signal = set_tasks_signal;
            let tasks_snapshot = tasks.get();
            
            // Calculate the target position based on drop location
            let target_position = if let Some((_, before_task_id)) = drop_position {
                // Get tasks in the target column sorted by position
                let mut column_tasks: Vec<&Task> = tasks_snapshot
                    .iter()
                    .filter(|t| t.status == new_status && t.id.to_string() != task_id)
                    .collect();
                column_tasks.sort_by_key(|t| t.position);
                
                if let Some(before_id) = before_task_id {
                    // Find the position of the task we're dropping before
                    column_tasks
                        .iter()
                        .position(|t| t.id.to_string() == before_id)
                        .map(|idx| idx as i32)
                        .unwrap_or(column_tasks.len() as i32)
                } else {
                    // Dropping at the end
                    column_tasks.len() as i32
                }
            } else {
                // Default to end of column
                let column_count = tasks_snapshot
                    .iter()
                    .filter(|t| t.status == new_status && t.id.to_string() != task_id)
                    .count();
                column_count as i32
            };
            
            // Determine if this is a same-column reorder or cross-column move
            let is_same_column = current_status == new_status;
                
            spawn_local(async move {
                // Use reorder_task for both within-column and cross-column moves
                let new_status_param = if is_same_column { None } else { Some(new_status.clone()) };
                
                // For drops into InProgress, check capacity first
                if !is_same_column && new_status == TaskStatus::InProgress {
                    if let Ok(0) = queue_service::get_queue_capacity().await {
                        toast::error("No capacity — queue is full. Move to Queue instead.".to_string());
                        return;
                    }
                }

                match reorder_task(task_id_clone.clone(), new_status_param, target_position).await {
                    Ok(Some(updated_task)) => {
                        set_tasks_signal.update(|tasks| {
                            if let Ok(task_uuid) = Uuid::parse_str(&task_id_clone) {
                                if let Some(task) = tasks.iter_mut().find(|t| t.id == task_uuid) {
                                    let task_title = task.title.clone();
                                    task.status = updated_task.status.clone();
                                    task.position = updated_task.position;
                                    if !is_same_column {
                                        toast::success(format!("Moved '{}' to {:?}", task_title, new_status));
                                    }
                                }
                            }
                            tasks.sort_by_key(|t| (t.status.clone() as i32, t.position));
                        });
                    }
                    Ok(None) => {
                        toast::error("Task not found".to_string());
                    }
                    Err(e) => {
                        leptos::logging::warn!("Failed to reorder task: {}", e);
                        toast::error(format!("Failed to move task: {}", e));
                    }
                }
            });
        }
    };

    let on_drag_over = {
        let status = status.clone();
        move |e: leptos::ev::DragEvent| {
            e.prevent_default();
            // Set drop effect to fix "denied" cursor
            if let Some(dt) = e.data_transfer() {
                dt.set_drop_effect("move");
            }
            set_drag_over_column.set(Some(status.clone()));
            // When hovering on column (not on a task card), set position to end (None)
            set_drag_over_position.set(Some((status.clone(), None)));
        }
    };

    let on_drag_leave = move |e: leptos::ev::DragEvent| {
        e.prevent_default();
        set_drag_over_column.set(None);
        set_drag_over_position.set(None);
    };

    // Helper function to get column tasks - can be called multiple times
    let get_column_tasks = {
        let status_tasks = status_tasks.clone();
        move || {
            let mut tasks_list: Vec<Task> = tasks.get();
            tasks_list = tasks_list.iter()
                .filter(|t| t.status == status_tasks.clone())
                .cloned()
                .collect();
            // Sort by position
            tasks_list.sort_by_key(|t| t.position);
            tasks_list
        }
    };

    // Select all handler for this column
    let status_for_select_all = status.clone();
    let get_column_tasks_for_select = get_column_tasks.clone();
    let on_select_all = move |_| {
        let column_task_ids: Vec<Uuid> = get_column_tasks_for_select().iter().map(|t| t.id).collect();
        let current_selected: HashSet<Uuid> = selected_tasks.get().into_iter().collect();
        let column_ids_set: HashSet<Uuid> = column_task_ids.iter().cloned().collect();
        
        // Check if all column tasks are already selected
        let all_selected = column_task_ids.iter().all(|id| current_selected.contains(id));
        
        if all_selected {
            // Deselect all column tasks
            let new_selected: Vec<Uuid> = current_selected
                .into_iter()
                .filter(|id| !column_ids_set.contains(id))
                .collect();
            set_selected_tasks.set(new_selected);
        } else {
            // Select all column tasks
            let mut new_selected: Vec<Uuid> = current_selected.into_iter().collect();
            for id in column_task_ids {
                if !new_selected.contains(&id) {
                    new_selected.push(id);
                }
            }
            set_selected_tasks.set(new_selected);
        }
    };

    // Check if all tasks in column are selected - use Signal::derive for Copy semantics
    let get_column_tasks_for_all = get_column_tasks.clone();
    let all_column_selected = Signal::derive(move || {
        let column_task_ids: Vec<Uuid> = get_column_tasks_for_all().iter().map(|t| t.id).collect();
        if column_task_ids.is_empty() {
            return false;
        }
        let current_selected: HashSet<Uuid> = selected_tasks.get().into_iter().collect();
        column_task_ids.iter().all(|id| current_selected.contains(id))
    });

    let get_column_tasks_for_some = get_column_tasks.clone();
    let some_column_selected = Signal::derive(move || {
        let column_task_ids: Vec<Uuid> = get_column_tasks_for_some().iter().map(|t| t.id).collect();
        let current_selected: HashSet<Uuid> = selected_tasks.get().into_iter().collect();
        let selected_count = column_task_ids.iter().filter(|id| current_selected.contains(id)).count();
        selected_count > 0 && selected_count < column_task_ids.len()
    });

    let task_count = move || {
        let tasks = tasks.get();
        tasks.iter().filter(|t| t.status == status_count.clone()).count()
    };

    let status_for_drag = status.clone();
    let status_for_class = status.clone();
    let status_for_testid = format!("{:?}", status).to_lowercase();
    let column_bg = column_clone.bg_class;
    let empty_icon = column_clone.empty_icon;
    let empty_msg = column_clone.empty_message;

    view! {
        <div
            data-testid=format!("column-{}", status_for_testid)
            class=move || {
                let is_drag_over = drag_over_column.get() == Some(status_for_class.clone());
                format!(
                    "min-w-[240px] w-[240px] flex-shrink-0 snap-start rounded-xl border-2 transition-all duration-300 flex flex-col max-h-[calc(100vh-180px)] {} {}",
                    if is_drag_over {
                        format!("border-white/30 {} scale-[1.01] shadow-xl", column_bg)
                    } else {
                        "border-white/5 bg-white/[0.01] hover:border-white/10".to_string()
                    },
                    column_clone.color_class
                )
            }
            on:dragover=on_drag_over
            on:drop=on_drop
            on:dragleave=on_drag_leave
        >
            // Column header
            <div class="sticky top-0 z-10 rounded-t-xl">
                <div class=format!(
                    "px-4 py-4 border-b border-white/5 rounded-t-xl {}",
                    column_clone.bg_class
                )>
                    <div class="flex items-center justify-between">
                        <div class="flex items-center gap-3">
                            // Select all checkbox
                            <label 
                                class="flex items-center cursor-pointer group/selectall"
                                data-testid="select-all-column"
                            >
                                <input
                                    type="checkbox"
                                    class="sr-only peer"
                                    prop:checked=move || all_column_selected.get()
                                    prop:indeterminate=move || some_column_selected.get()
                                    on:change=on_select_all.clone()
                                />
                                <div class=move || format!(
                                    "w-4 h-4 rounded border-2 transition-all flex items-center justify-center {}",
                                    if all_column_selected.get() {
                                        "bg-yellow-500 border-yellow-500"
                                    } else if some_column_selected.get() {
                                        "bg-yellow-500/50 border-yellow-500"
                                    } else {
                                        "border-white/20 group-hover/selectall:border-white/40"
                                    }
                                )>
                                    {move || (all_column_selected.get() || some_column_selected.get()).then(|| view! {
                                        <svg class="w-3 h-3 text-black" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="3" d="M5 13l4 4L19 7" />
                                        </svg>
                                    })}
                                </div>
                            </label>
                            <h3 class="font-semibold text-white/90">{column_clone.title}</h3>
                            <span class=format!(
                                "px-2.5 py-1 rounded-full text-sm font-bold min-w-[28px] text-center {}",
                                column_clone.count_bg_class
                            )>
                                {task_count}
                            </span>
                        </div>
                    </div>
                    <p class="text-xs text-white/40 mt-1">{column_clone.description}</p>
                </div>
            </div>

            // Column content - compact spacing
            <div class="p-2 flex-1 overflow-y-auto">
                <div class="space-y-1">
                    {move || {
                        let tasks = get_column_tasks();
                        if tasks.is_empty() {
                            view! {
                                <div class="flex flex-col items-center justify-center h-32 rounded-xl border-2 border-dashed border-white/10 bg-white/[0.01]">
                                    <svg class="w-6 h-6 text-white/15 mb-2" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M12 6v6m0 0v6m0-6h6m-6 0H6" />
                                    </svg>
                                    <p class="text-xs text-white/25 text-center px-4">{empty_msg}</p>
                                </div>
                            }.into_any()
                        } else {
                            let set_drag = set_dragged_task;
                            let on_click = on_task_click;
                            let column_status = status.clone();
                            let column_status_end = status.clone();
                            let task_ids: Vec<String> = tasks.iter().map(|t| t.id.to_string()).collect();
                            let task_count = tasks.len();
                            view! {
                                <>
                                    {tasks.into_iter().enumerate().map(move |(idx, task)| {
                                        let on_click = on_click;
                                        let column_status = column_status.clone();
                                        let next_task_id = task_ids.get(idx + 1).cloned();
                                        view! {
                                            <KanbanTaskCard 
                                                task=task 
                                                column_status=column_status
                                                next_task_id=next_task_id
                                                dragged_task=set_drag
                                                drag_over_position=drag_over_position
                                                set_drag_over_position=set_drag_over_position
                                                on_click=on_click
                                                set_show_context_menu=set_show_context_menu
                                                set_context_menu_pos=set_context_menu_pos
                                                set_context_menu_task=set_context_menu_task
                                                selected_tasks=selected_tasks
                                                set_selected_tasks=set_selected_tasks
                                                show_diff_modal=show_diff_modal
                                                diff_content=diff_content
                                                diff_stat_content=diff_stat_content
                                                diff_title=diff_title
                                                on_pr_created=on_pr_created
                                                on_analyze_pr_comments=on_analyze_pr_comments
                                                show_pr_candidates_modal=show_pr_candidates_modal
                                                pr_candidate_task=pr_candidate_task
                                                pr_candidates=pr_candidates
                                            />
                                        }
                                    }).collect::<Vec<_>>()}
                                    // End-of-column drop indicator
                                    <div
                                        class=move || {
                                            let show_end_indicator = if let Some((status, target)) = drag_over_position.get() {
                                                status == column_status_end && target.is_none() && task_count > 0
                                            } else {
                                                false
                                            };
                                            format!(
                                                "h-1.5 rounded-full mt-2 transition-all duration-150 {}",
                                                if show_end_indicator {
                                                    "bg-gradient-to-r from-blue-500 via-purple-500 to-blue-500 opacity-100 scale-y-100 shadow-lg shadow-blue-500/50"
                                                } else {
                                                    "bg-transparent opacity-0 scale-y-0"
                                                }
                                            )
                                        }
                                    >
                                        <div class="w-full h-full rounded-full animate-pulse bg-white/30"></div>
                                    </div>
                                </>
                            }.into_any()
                        }
                    }}
                </div>
            </div>
        </div>
    }
}

#[component]
fn KanbanTaskCard(
    task: Task,
    #[prop(default = 0)] _task_index: usize,
    column_status: TaskStatus,
    next_task_id: Option<String>,
    dragged_task: WriteSignal<Option<(String, TaskStatus)>>,
    drag_over_position: Signal<Option<(TaskStatus, Option<String>)>>,
    set_drag_over_position: WriteSignal<Option<(TaskStatus, Option<String>)>>,
    on_click: Callback<Task>,
    set_show_context_menu: WriteSignal<bool>,
    set_context_menu_pos: WriteSignal<(i32, i32)>,
    set_context_menu_task: WriteSignal<Option<Task>>,
    selected_tasks: Signal<Vec<Uuid>>,
    set_selected_tasks: WriteSignal<Vec<Uuid>>,
    show_diff_modal: RwSignal<bool>,
    diff_content: RwSignal<String>,
    diff_stat_content: RwSignal<String>,
    diff_title: RwSignal<String>,
    on_pr_created: Callback<Task>,
    on_analyze_pr_comments: Callback<Task>,
    show_pr_candidates_modal: RwSignal<bool>,
    pr_candidate_task: RwSignal<Option<Task>>,
    pr_candidates: RwSignal<Vec<PrCandidate>>,
) -> impl IntoView {
    let task_for_click = task.clone();
    let task_for_context = task.clone();
    let task_for_menu_btn = task.clone();
    let task_for_pr_review = task.clone();
    let task_id = task.id.to_string();
    let task_id_for_indicator = task.id.to_string();
    let task_uuid = task.id;

    // Check if this task is selected
    let is_selected = move || selected_tasks.get().contains(&task_uuid);

    let task_status = task.status.clone();
    let is_in_progress = task_status == TaskStatus::InProgress;
    let (is_dragging, set_is_dragging) = signal(false);
    // Track if mouse is in lower half (for showing indicator below instead of above)
    let (is_lower_half, set_is_lower_half) = signal(false);

    // Right-click context menu handler
    let on_context_menu = {
        let task = task_for_context.clone();
        move |e: web_sys::MouseEvent| {
            e.prevent_default();
            e.stop_propagation();
            set_context_menu_pos.set((e.client_x(), e.client_y()));
            set_context_menu_task.set(Some(task.clone()));
            set_show_context_menu.set(true);
        }
    };

    // 3-dot menu button click handler
    let on_menu_button_click = {
        let task = task_for_menu_btn.clone();
        move |e: web_sys::MouseEvent| {
            e.prevent_default();
            e.stop_propagation();
            // Position menu near the button
            set_context_menu_pos.set((e.client_x(), e.client_y()));
            set_context_menu_task.set(Some(task.clone()));
            set_show_context_menu.set(true);
        }
    };

    let on_drag_start = {
        let task_id = task_id.clone();
        let task_status = task_status.clone();
        move |e: web_sys::DragEvent| {
            if let Some(dt) = e.data_transfer() {
                dt.set_effect_allowed("move");
                let _ = dt.set_data("text/plain", &task_id);
                // Let browser use the default drag ghost — it captures the draggable element naturally
            }
            dragged_task.set(Some((task_id.clone(), task_status.clone())));
            set_is_dragging.set(true);
        }
    };

    let on_drag_end = move |_e| {
        dragged_task.set(None);
        set_is_dragging.set(false);
        set_drag_over_position.set(None);
    };

    // Handle drag over this task card to determine drop position using getBoundingClientRect
    let on_task_drag_over = {
        let task_id = task_id.clone();
        let next_task_id = next_task_id.clone();
        let column_status = column_status.clone();
        move |e: web_sys::DragEvent| {
            e.prevent_default();
            e.stop_propagation();
            
            // Set drop effect to fix "denied" cursor
            if let Some(dt) = e.data_transfer() {
                dt.set_drop_effect("move");
            }
            
            // Get mouse Y position
            let mouse_y = e.client_y() as f64;

            // Get the target element and calculate position using getBoundingClientRect
            let target = e.current_target();
            if let Some(target) = target {
                if let Ok(element) = target.dyn_into::<web_sys::Element>() {
                    let rect = element.get_bounding_client_rect();
                    let element_top = rect.top();
                    let element_height = rect.height();

                    // Use a 30% dead zone around the midpoint to prevent flickering
                    let upper_threshold = element_top + (element_height * 0.35);
                    let lower_threshold = element_top + (element_height * 0.65);

                    // Only update if cursor is outside the dead zone
                    let in_lower_half = if mouse_y < upper_threshold {
                        false
                    } else if mouse_y > lower_threshold {
                        true
                    } else {
                        // Inside dead zone — keep current state
                        is_lower_half.get_untracked()
                    };
                    set_is_lower_half.set(in_lower_half);
                    
                    if in_lower_half {
                        // Drop after this task (before next task, or at end if no next task)
                        set_drag_over_position.set(Some((column_status.clone(), next_task_id.clone())));
                    } else {
                        // Drop before this task
                        set_drag_over_position.set(Some((column_status.clone(), Some(task_id.clone()))));
                    }
                } else {
                    // Fallback: drop before this task
                    set_drag_over_position.set(Some((column_status.clone(), Some(task_id.clone()))));
                }
            }
        }
    };
    
    let on_task_drag_leave = {
        move |_e: web_sys::DragEvent| {
            set_is_lower_half.set(false);
        }
    };

    let handle_click = {
        let task = task_for_click.clone();
        move |e: web_sys::MouseEvent| {
            if !is_dragging.get() {
                e.stop_propagation();
                on_click.run(task.clone());
            }
        }
    };

    // Store task_id and next_task_id for use in reactive closures
    let task_id_for_above = task_id_for_indicator.clone();
    let task_id_above_2 = task_id_for_indicator.clone();
    let column_status_above = column_status.clone();
    let column_status_above_2 = column_status.clone();
    let column_status_below = column_status.clone();
    let column_status_below_2 = column_status.clone();

    let next_task_id_below = next_task_id.clone();
    let next_task_id_below_2 = next_task_id.clone();

    // Helper function to check if indicator should show above
    let check_show_above = move || {
        if let Some((status, Some(before_id))) = drag_over_position.get() {
            status == column_status_above && before_id == task_id_for_above && !is_lower_half.get()
        } else {
            false
        }
    };
    
    // Helper function to check if indicator should show below  
    let check_show_below = move || {
        if let Some((status, target_id)) = drag_over_position.get() {
            if status != column_status_below {
                return false;
            }
            if is_lower_half.get() {
                target_id == next_task_id_below
            } else {
                false
            }
        } else {
            false
        }
    };

    view! {
        <div class="relative group/card">
            // Drop indicator above task (shows when hovering on upper half)
            <div
                class=move || {
                    let show = if let Some((status, Some(before_id))) = drag_over_position.get() {
                        status == column_status_above_2 && before_id == task_id_above_2 && !is_lower_half.get()
                    } else {
                        false
                    };
                    format!(
                        "h-1.5 rounded-full mb-1.5 transition-all duration-150 {}",
                        if show {
                            "bg-gradient-to-r from-blue-500 via-purple-500 to-blue-500 opacity-100 scale-y-100 shadow-lg shadow-blue-500/50"
                        } else {
                            "bg-transparent opacity-0 scale-y-0"
                        }
                    )
                }
            ></div>
            
            // Selection checkbox (appears on hover or when selected)
            <div 
                class=move || format!(
                    "absolute -left-1 top-3 z-20 cursor-pointer transition-all duration-200 {}",
                    if is_selected() {
                        "opacity-100"
                    } else {
                        "opacity-0 group-hover/card:opacity-100"
                    }
                )
                data-testid="task-checkbox"
                on:click=move |e: web_sys::MouseEvent| {
                    e.stop_propagation();
                    let mut current = selected_tasks.get();
                    if current.contains(&task_uuid) {
                        current.retain(|id| *id != task_uuid);
                    } else {
                        current.push(task_uuid);
                    }
                    set_selected_tasks.set(current);
                }
            >
                <div class=move || format!(
                    "w-5 h-5 rounded border-2 transition-all flex items-center justify-center shadow-lg {}",
                    if is_selected() {
                        "bg-yellow-500 border-yellow-500"
                    } else {
                        "bg-[#1a1a24] border-white/30 hover:border-yellow-500/50"
                    }
                )>
                    {move || is_selected().then(|| view! {
                        <svg class="w-3 h-3 text-black" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="3" d="M5 13l4 4L19 7" />
                        </svg>
                    })}
                </div>
            </div>
            
            <div
                class=move || {
                    let show_above = check_show_above();
                    let show_below = check_show_below();
                    let selected = is_selected();
                    format!(
                        "transition-all duration-150 {} relative {} {}",
                        if is_in_progress { "cursor-progress" } else { "cursor-grab active:cursor-grabbing" },
                        if is_dragging.get() {
                            "opacity-25 scale-[0.98] z-50".to_string()
                        } else if show_above || show_below {
                            "ring-2 ring-blue-500/50 rounded-lg".to_string()
                        } else {
                            "hover:translate-y-[-2px] hover:shadow-lg hover:shadow-black/30".to_string()
                        },
                        if selected {
                            "ring-2 ring-yellow-500/50 rounded-lg"
                        } else {
                            ""
                        }
                    )
                }
                draggable="true"
                on:dragstart=on_drag_start
                on:dragend=on_drag_end
                on:dragover=on_task_drag_over
                on:dragleave=on_task_drag_leave
                on:click=handle_click
                on:contextmenu=on_context_menu
            >
                // 3-dot menu button (appears on hover)
                <button
                    class="absolute top-2 right-2 p-1.5 rounded-md opacity-0 group-hover/card:opacity-100 hover:bg-white/10 z-10 transition-all"
                    on:click=on_menu_button_click
                    title="Task options"
                    aria-label="Task options"
                >
                    <svg class="w-4 h-4 text-white/60" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 5v.01M12 12v.01M12 19v.01M12 6a1 1 0 110-2 1 1 0 010 2zm0 7a1 1 0 110-2 1 1 0 010 2zm0 7a1 1 0 110-2 1 1 0 010 2z" />
                    </svg>
                </button>
                <TaskCard task=task.clone() />
                // "Diff" button for review/done statuses
                {
                    let show_diff = matches!(
                        column_status,
                        TaskStatus::AiReview | TaskStatus::HumanReview | TaskStatus::Done | TaskStatus::PrCreated
                    );
                    show_diff.then(|| {
                        let task_id = task.id.to_string();
                        let task_title = task.title.clone();
                        view! {
                            <div class="flex justify-end px-2 pb-1.5 -mt-1">
                                <button
                                    class="px-2 py-0.5 text-[10px] rounded bg-white/[0.06] text-white/50 hover:bg-white/10 hover:text-white/80 transition-colors"
                                    on:click=move |e: web_sys::MouseEvent| {
                                        e.stop_propagation();
                                        let task_id = task_id.clone();
                                        let task_title = task_title.clone();
                                        spawn_local(async move {
                                            diff_title.set(format!("Diff: {}", task_title));
                                            // Fetch diff and stat in parallel
                                            let diff_fut = get_task_diff(task_id.clone());
                                            let stat_fut = get_task_diff_stat(task_id);
                                            let (diff_result, stat_result) = futures::join!(diff_fut, stat_fut);
                                            match diff_result {
                                                Ok(diff) => diff_content.set(diff),
                                                Err(e) => {
                                                    toast::error(format!("Failed to load diff: {}", e));
                                                    return;
                                                }
                                            }
                                            match stat_result {
                                                Ok(stat) => diff_stat_content.set(stat),
                                                Err(_) => diff_stat_content.set(String::new()),
                                            }
                                            show_diff_modal.set(true);
                                        });
                                    }
                                    title="View diff"
                                >
                                    "Diff"
                                </button>
                            </div>
                        }
                    })
                }
                // Visible PR review action for tasks that already have a PR.
                {
                    let has_pr = task.pr_url.is_some() || task.external_refs.iter().any(|r| r.is_pr());
                    let can_sync_pr = matches!(task.status, TaskStatus::HumanReview | TaskStatus::Done)
                        && !has_pr;

                    (has_pr || can_sync_pr).then(|| {
                        let task_for_pr_review = task_for_pr_review.clone();
                        let task_for_sync_pr = task.clone();
                        view! {
                            <div class="flex justify-end gap-1.5 px-2 pb-1.5 -mt-1">
                                {can_sync_pr.then(|| {
                                    let task_for_sync_pr = task_for_sync_pr.clone();
                                    view! {
                                        <button
                                            class="px-2 py-0.5 text-[10px] rounded bg-green-500/10 text-green-300 hover:bg-green-500/20 transition-colors"
                                            on:click=move |e: web_sys::MouseEvent| {
                                                e.stop_propagation();
                                                let task_id = task_for_sync_pr.id.to_string();
                                                let branch_label = task_for_sync_pr.branch_name.clone()
                                                    .unwrap_or_else(|| "no branch".to_string());
                                                let issue_labels: Vec<String> = task_for_sync_pr.external_refs.iter()
                                                    .filter_map(|r| match r {
                                                        crate::models::ExternalRef::GithubIssue { number, .. } => Some(format!("#{}", number)),
                                                        _ => None,
                                                    })
                                                    .collect();
                                                let issue_label = if issue_labels.is_empty() {
                                                    "no linked issue".to_string()
                                                } else {
                                                    issue_labels.join(", ")
                                                };
                                                toast::info(format!("Checking PR for branch `{}` / issue {}", branch_label, issue_label));
                                                let task_for_modal = task_for_sync_pr.clone();
                                                spawn_local(async move {
                                                    match find_pr_candidates(task_id.clone()).await {
                                                        Ok(candidates) if candidates.is_empty() => {
                                                            toast::info(format!(
                                                                "No PR candidates found for branch `{}` / issue {}",
                                                                branch_label,
                                                                issue_label
                                                            ));
                                                        }
                                                        Ok(candidates) if candidates.len() == 1 => {
                                                            let candidate = candidates[0].clone();
                                                            match link_existing_pr(task_id, candidate.url.clone()).await {
                                                                Ok(Some(updated)) => {
                                                                    on_pr_created.run(updated);
                                                                    toast::success(format!("Linked PR: {}", candidate.url));
                                                                }
                                                                Ok(None) => toast::error("Task not found while linking PR".to_string()),
                                                                Err(e) => toast::error(format!("Failed to link PR: {}", e)),
                                                            }
                                                        }
                                                        Ok(candidates) => {
                                                            pr_candidate_task.set(Some(task_for_modal));
                                                            pr_candidates.set(candidates);
                                                            show_pr_candidates_modal.set(true);
                                                        }
                                                        Err(e) => toast::error(format!("Failed to find PR candidates: {}", e)),
                                                    }
                                                });
                                            }
                                            title="Find and link an existing pull request for this branch"
                                        >
                                            "Sync PR"
                                        </button>
                                    }
                                })}
                                {has_pr.then(|| {
                                    let task_for_refresh = task_for_pr_review.clone();
                                    view! {
                                        <button
                                            class="px-2 py-0.5 text-[10px] rounded bg-cyan-500/10 text-cyan-300 hover:bg-cyan-500/20 transition-colors"
                                            on:click=move |e: web_sys::MouseEvent| {
                                                e.stop_propagation();
                                                let task_id = task_for_refresh.id.to_string();
                                                spawn_local(async move {
                                                    match refresh_task_pr_state(task_id).await {
                                                        Ok(Some(updated)) => {
                                                            let new_status = updated.status.clone();
                                                            on_pr_created.run(updated);
                                                            if matches!(new_status, TaskStatus::Done) {
                                                                toast::success("PR merged — moved to Done".to_string());
                                                            } else {
                                                                toast::info("PR state refreshed".to_string());
                                                            }
                                                        }
                                                        Ok(None) => {}
                                                        Err(e) => toast::error(format!("Failed to refresh PR: {}", e)),
                                                    }
                                                });
                                            }
                                            title="Refresh PR state from GitHub"
                                        >
                                            "Refresh"
                                        </button>
                                        <button
                                            class="px-2 py-0.5 text-[10px] rounded bg-yellow-500/10 text-yellow-300 hover:bg-yellow-500/20 transition-colors"
                                            on:click=move |e: web_sys::MouseEvent| {
                                                e.stop_propagation();
                                                on_analyze_pr_comments.run(task_for_pr_review.clone());
                                            }
                                            title="Analyze PR comments before applying fixes"
                                        >
                                            "Review PR"
                                        </button>
                                    }
                                })}
                            </div>
                        }
                    })
                }
            </div>
            
            // Drop indicator below task (shows when hovering on lower half)
            <div
                class=move || {
                    let show = if let Some((status, target_id)) = drag_over_position.get() {
                        if status != column_status_below_2 {
                            false
                        } else if is_lower_half.get() {
                            target_id == next_task_id_below_2
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    format!(
                        "h-1.5 rounded-full mt-1.5 transition-all duration-150 {}",
                        if show {
                            "bg-gradient-to-r from-blue-500 via-purple-500 to-blue-500 opacity-100 scale-y-100 shadow-lg shadow-blue-500/50"
                        } else {
                            "bg-transparent opacity-0 scale-y-0"
                        }
                    )
                }
            ></div>
        </div>
    }
}
