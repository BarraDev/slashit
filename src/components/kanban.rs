use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::callback::Callback;
use wasm_bindgen::JsCast;
use crate::models::{Task, TaskStatus};
use crate::components::{TaskCard, TaskEditModal, TaskEditMode, toast, TaskContextMenu, DiffModal};
use crate::services::{reorder_task, queue_service, get_task_diff, get_task_diff_stat};
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

const COLUMNS: [ColumnData; 7] = [
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
        status: TaskStatus::Done,
        title: "Done",
        description: "Completed",
        color_class: "border-emerald-500",
        bg_class: "bg-emerald-500/5",
        count_bg_class: "bg-emerald-500/20 text-emerald-300",
        empty_icon: "",
        empty_message: "No completed tasks",
    },
];

#[component]
pub fn Kanban(
    tasks: Vec<Task>,
    #[prop(default = String::new())] project_id: String,
    #[prop(into)] selected_tasks: Signal<Vec<Uuid>>,
    set_selected_tasks: WriteSignal<Vec<Uuid>>,
) -> impl IntoView {
    // Use the provided selection state
    let selected_tasks_signal = selected_tasks;
    let set_selected_tasks_writer = set_selected_tasks;
    let (tasks_signal, set_tasks_signal) = signal(tasks);
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

    // Calculate stats
    let task_stats = move || {
        let tasks = tasks_signal.get();
        let total = tasks.len();
        let in_progress = tasks.iter().filter(|t| t.status == TaskStatus::InProgress).count();
        let done = tasks.iter().filter(|t| t.status == TaskStatus::Done || t.status == TaskStatus::PrCreated).count();
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
                    .filter(|t| {
                        let matches_status = if new_status == TaskStatus::Done {
                            t.status == TaskStatus::Done || t.status == TaskStatus::PrCreated
                        } else {
                            t.status == new_status
                        };
                        matches_status && t.id.to_string() != task_id
                    })
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
                    .filter(|t| {
                        let matches_status = if new_status == TaskStatus::Done {
                            t.status == TaskStatus::Done || t.status == TaskStatus::PrCreated
                        } else {
                            t.status == new_status
                        };
                        matches_status && t.id.to_string() != task_id
                    })
                    .count();
                column_count as i32
            };
            
            // Determine if this is a same-column reorder or cross-column move
            let is_same_column = current_status == new_status || 
                (new_status == TaskStatus::Done && current_status == TaskStatus::PrCreated) ||
                (new_status == TaskStatus::PrCreated && current_status == TaskStatus::Done);
                
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
            if status_tasks == TaskStatus::Done {
                tasks_list = tasks_list.iter()
                    .filter(|t| t.status == TaskStatus::Done || t.status == TaskStatus::PrCreated)
                    .cloned()
                    .collect();
            } else {
                tasks_list = tasks_list.iter()
                    .filter(|t| t.status == status_tasks.clone())
                    .cloned()
                    .collect();
            }
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
        if status_count == TaskStatus::Done {
            tasks.iter().filter(|t| t.status == TaskStatus::Done || t.status == TaskStatus::PrCreated).count()
        } else {
            tasks.iter().filter(|t| t.status == status_count.clone()).count()
        }
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
) -> impl IntoView {
    let task_for_click = task.clone();
    let task_for_context = task.clone();
    let task_for_menu_btn = task.clone();
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
                        TaskStatus::AiReview | TaskStatus::HumanReview | TaskStatus::Done
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
