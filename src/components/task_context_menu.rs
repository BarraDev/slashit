use leptos::prelude::*;
use leptos::callback::Callback;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;
use crate::models::{Task, TaskStatus};
use crate::services::{delete_task, reorder_task, create_pr};
use crate::components::toast;
use uuid::Uuid;

/// Props for the context menu
#[component]
pub fn TaskContextMenu(
    #[prop(into)] show: Signal<bool>,
    set_show: WriteSignal<bool>,
    #[prop(into)] position: Signal<(i32, i32)>,
    #[prop(into)] task: Signal<Option<Task>>,
    on_edit: Callback<Task>,
    on_delete: Callback<Uuid>,
    on_move: Callback<(Task, TaskStatus)>,
    on_pr_created: Callback<Task>,
    on_analyze_pr_comments: Callback<Task>,
    on_private_email_pr_error: Callback<Task>,
) -> impl IntoView {
    let (show_move_submenu, set_show_move_submenu) = signal(false);
    
    // Close menu when clicking outside
    let menu_ref = NodeRef::<leptos::html::Div>::new();
    
    // Handle escape key to close
    Effect::new(move |_| {
        if show.get() {
            let handler = wasm_bindgen::closure::Closure::wrap(Box::new(move |e: web_sys::KeyboardEvent| {
                if e.key() == "Escape" {
                    set_show.set(false);
                }
            }) as Box<dyn Fn(_)>);
            
            if let Some(window) = web_sys::window() {
                let _ = window.add_event_listener_with_callback("keydown", handler.as_ref().unchecked_ref());
                handler.forget();
            }
        }
    });

    let on_edit_click = {
        move |_: web_sys::MouseEvent| {
            if let Some(t) = task.get() {
                on_edit.run(t);
                set_show.set(false);
            }
        }
    };

    let on_delete_click = {
        move |_: web_sys::MouseEvent| {
            if let Some(t) = task.get() {
                let task_id = t.id;
                let task_title = t.title.clone();

                spawn_local(async move {
                    match delete_task(task_id.to_string()).await {
                        Ok(true) => {
                            toast::success(format!("Task '{}' deleted", task_title));
                            on_delete.run(task_id);
                        }
                        Ok(false) => {
                            toast::error("Task not found".to_string());
                        }
                        Err(e) => {
                            toast::error(format!("Failed to delete: {}", e));
                        }
                    }
                    set_show.set(false);
                });
            }
        }
    };

    let on_add_to_queue_click = {
        move |_: web_sys::MouseEvent| {
            if let Some(t) = task.get() {
                let task_clone = t.clone();

                spawn_local(async move {
                    match reorder_task(task_clone.id.to_string(), Some(TaskStatus::Queue), 0).await {
                        Ok(Some(_)) => {
                            toast::success(format!("'{}' added to queue", task_clone.title));
                            on_move.run((task_clone, TaskStatus::Queue));
                        }
                        _ => {
                            toast::error("Failed to add to queue".to_string());
                        }
                    }
                    set_show.set(false);
                });
            }
        }
    };

    let create_move_handler = move |status: TaskStatus| {
        let set_show = set_show;
        let on_move = on_move;
        move |_: web_sys::MouseEvent| {
            if let Some(t) = task.get() {
                let task_clone = t.clone();
                let status = status.clone();
                let on_move = on_move;
                let set_show = set_show;
                
                spawn_local(async move {
                    match reorder_task(task_clone.id.to_string(), Some(status.clone()), 0).await {
                        Ok(Some(_)) => {
                            toast::success(format!("Moved to {:?}", status));
                            on_move.run((task_clone, status));
                        }
                        _ => {
                            toast::error("Failed to move task".to_string());
                        }
                    }
                    set_show.set(false);
                });
            }
        }
    };

    // Use static array instead of Vec to avoid move issues
    const STATUSES: [(TaskStatus, &str, &str); 6] = [
        (TaskStatus::Backlog, "Backlog", "📋"),
        (TaskStatus::Queue, "Queue", "⏳"),
        (TaskStatus::InProgress, "In Progress", "🔨"),
        (TaskStatus::AiReview, "AI Review", "🤖"),
        (TaskStatus::HumanReview, "Human Review", "👀"),
        (TaskStatus::Done, "Done", "✅"),
    ];

    view! {
        <Show when=move || show.get()>
            // Backdrop to close menu
            <div 
                class="fixed inset-0 z-40"
                on:click=move |_| set_show.set(false)
                on:contextmenu=move |e| {
                    e.prevent_default();
                    set_show.set(false);
                }
            ></div>
            
            // Menu
            <div
                node_ref=menu_ref
                class="fixed z-50 min-w-[180px] py-1.5 bg-[#1a1a24] border border-white/10 rounded-lg shadow-2xl animate-fade-in"
                style=move || {
                    let (x, y) = position.get();
                    // Clamp to viewport bounds
                    let max_x = web_sys::window()
                        .and_then(|w| w.inner_width().ok())
                        .and_then(|w| w.as_f64())
                        .unwrap_or(1920.0) as i32 - 200;
                    let max_y = web_sys::window()
                        .and_then(|w| w.inner_height().ok())
                        .and_then(|h| h.as_f64())
                        .unwrap_or(1080.0) as i32 - 300;
                    let clamped_x = x.min(max_x).max(0);
                    let clamped_y = y.min(max_y).max(0);
                    format!("left: {}px; top: {}px;", clamped_x, clamped_y)
                }
            >
                // Edit
                <button
                    class="w-full px-3 py-2 text-left text-sm text-white/80 hover:bg-white/10 flex items-center gap-2 transition-colors"
                    on:click=on_edit_click
                >
                    <svg class="w-4 h-4 text-white/50" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M11 5H6a2 2 0 00-2 2v11a2 2 0 002 2h11a2 2 0 002-2v-5m-1.414-9.414a2 2 0 112.828 2.828L11.828 15H9v-2.828l8.586-8.586z" />
                    </svg>
                    "Edit Task"
                </button>

                // Add to Queue
                <button
                    class="w-full px-3 py-2 text-left text-sm text-white/80 hover:bg-white/10 flex items-center gap-2 transition-colors"
                    on:click=on_add_to_queue_click
                >
                    <svg class="w-4 h-4 text-white/50" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6m0 0v6m0-6h6m-6 0H6" />
                    </svg>
                    "Add to Queue"
                </button>

                // Move to submenu
                <div 
                    class="relative"
                    on:mouseenter=move |_| set_show_move_submenu.set(true)
                    on:mouseleave=move |_| set_show_move_submenu.set(false)
                >
                    <button
                        class="w-full px-3 py-2 text-left text-sm text-white/80 hover:bg-white/10 flex items-center justify-between transition-colors"
                    >
                        <span class="flex items-center gap-2">
                            <svg class="w-4 h-4 text-white/50" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M17 8l4 4m0 0l-4 4m4-4H3" />
                            </svg>
                            "Move to"
                        </span>
                        <svg class="w-3 h-3 text-white/40" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7" />
                        </svg>
                    </button>
                    
                    // Submenu
                    <Show when=move || show_move_submenu.get()>
                        <div class="absolute left-full top-0 ml-1 min-w-[160px] py-1.5 bg-[#1a1a24] border border-white/10 rounded-lg shadow-2xl">
                            {STATUSES.iter().map(|(status, label, icon)| {
                                let status = status.clone();
                                let handler = create_move_handler(status.clone());
                                view! {
                                    <button
                                        class="w-full px-3 py-2 text-left text-sm text-white/80 hover:bg-white/10 flex items-center gap-2 transition-colors"
                                        on:click=handler
                                    >
                                        <span class="text-xs">{*icon}</span>
                                        {*label}
                                    </button>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    </Show>
                </div>

                // Create PR (only for HumanReview/Done tasks without a PR)
                {
                    let set_show = set_show;
                    let on_create_pr = move |_: leptos::ev::MouseEvent| {
                        if let Some(t) = task.get() {
                            let mut task_after_create = t.clone();
                            let task_id = t.id.to_string();
                            set_show.set(false);
                            spawn_local(async move {
                                match create_pr(task_id).await {
                                    Ok(url) => {
                                        task_after_create.pr_url = Some(url.clone());
                                        task_after_create.status = TaskStatus::PrCreated;
                                        on_pr_created.run(task_after_create);
                                        toast::success(format!("PR linked: {}", url));
                                    }
                                    Err(e) => {
                                        if e.contains("GH007") || e.contains("private email address") {
                                            on_private_email_pr_error.run(task_after_create);
                                        } else {
                                            toast::error(format!("Failed to find or create PR: {}", e));
                                        }
                                    }
                                }
                            });
                        }
                    };
                    move || {
                        let can_create_pr = task.get().map(|t| {
                            matches!(t.status, TaskStatus::HumanReview | TaskStatus::Done)
                                && t.pr_url.is_none()
                                && !t.external_refs.iter().any(|r| r.is_pr())
                        }).unwrap_or(false);

                        can_create_pr.then(|| view! {
                            <button
                                class="w-full px-3 py-2 text-left text-sm text-white/80 hover:bg-white/10 flex items-center gap-2 transition-colors"
                                on:click=on_create_pr
                            >
                                <svg class="w-4 h-4 text-white/50" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10 6H6a2 2 0 00-2 2v10a2 2 0 002 2h10a2 2 0 002-2v-4M14 4h6m0 0v6m0-6L10 14" />
                                </svg>
                                "Find/Create PR"
                            </button>
                        })
                    }
                }

                // Analyze PR comments before applying fixes
                {
                    let set_show = set_show;
                    let on_analyze = move |_: leptos::ev::MouseEvent| {
                        if let Some(t) = task.get() {
                            set_show.set(false);
                            on_analyze_pr_comments.run(t);
                        }
                    };
                    move || {
                        let has_pr = task.get().map(|t| {
                            t.pr_url.is_some() || t.external_refs.iter().any(|r| r.is_pr())
                        }).unwrap_or(false);

                        has_pr.then(|| view! {
                            <button
                                class="w-full px-3 py-2 text-left text-sm text-white/80 hover:bg-white/10 flex items-center gap-2 transition-colors"
                                on:click=on_analyze
                            >
                                <svg class="w-4 h-4 text-white/50" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5h6m-6 4h6m-6 4h4m5 8H6a2 2 0 01-2-2V5a2 2 0 012-2h7l5 5v11a2 2 0 01-2 2z" />
                                </svg>
                                "Review PR comments"
                            </button>
                        })
                    }
                }

                // Divider
                <div class="my-1.5 border-t border-white/5"></div>

                // Delete
                <button
                    class="w-full px-3 py-2 text-left text-sm text-red-400 hover:bg-red-500/10 flex items-center gap-2 transition-colors"
                    on:click=on_delete_click
                >
                    <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
                    </svg>
                    "Delete Task"
                </button>
            </div>
        </Show>
    }
}

/// 3-dot menu button icon
#[component]
pub fn MoreVerticalIcon(#[prop(into, optional)] class: String) -> impl IntoView {
    view! {
        <svg class=format!("w-4 h-4 {}", class) fill="none" viewBox="0 0 24 24" stroke="currentColor">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 5v.01M12 12v.01M12 19v.01M12 6a1 1 0 110-2 1 1 0 010 2zm0 7a1 1 0 110-2 1 1 0 010 2zm0 7a1 1 0 110-2 1 1 0 010 2z" />
        </svg>
    }
}
