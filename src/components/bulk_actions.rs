use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;
use leptos::callback::Callback;

#[component]
pub fn BulkActions(
    selected_tasks: Signal<Vec<Uuid>>,
    set_selected_tasks: WriteSignal<Vec<Uuid>>,
    project_id: String,
    on_bulk_complete: Callback<()>,
) -> impl IntoView {
    let (processing, set_processing) = signal(false);
    let (error_msg, set_error_msg) = signal(None::<String>);
    let (confirm_delete, set_confirm_delete) = signal(false);

    let count = Signal::derive(move || selected_tasks.get().len());
    let has_selection = Signal::derive(move || count.get() > 0);

    let on_add_to_queue = {
        move |_| {
            let selected = selected_tasks.get();
            if selected.is_empty() {
                return;
            }

            set_processing.set(true);
            set_error_msg.set(None);

            spawn_local(async move {
                let task_ids: Vec<String> = selected.iter().map(|id| id.to_string()).collect();
                match crate::services::bulk_add_to_queue(task_ids).await {
                    Ok(_) => {
                        on_bulk_complete.run(());
                    }
                    Err(e) => {
                        set_error_msg.set(Some(e));
                    }
                }
                set_processing.set(false);
            });
        }
    };

    let on_bulk_pr = {
        move |_| {
            let selected = selected_tasks.get();
            if selected.is_empty() {
                return;
            }

            set_processing.set(true);
            set_error_msg.set(None);

            spawn_local(async move {
                let task_ids: Vec<String> = selected.iter().map(|id| id.to_string()).collect();
                match crate::services::bulk_create_prs(task_ids).await {
                    Ok(_) => {
                        on_bulk_complete.run(());
                    }
                    Err(e) => {
                        set_error_msg.set(Some(e));
                    }
                }
                set_processing.set(false);
            });
        }
    };

    let on_bulk_archive = {
        move |_| {
            let selected = selected_tasks.get();
            if selected.is_empty() {
                return;
            }

            set_processing.set(true);
            set_error_msg.set(None);

            spawn_local(async move {
                for task_id in &selected {
                    if let Err(e) = crate::services::update_task_status(
                        task_id.to_string(),
                        crate::models::TaskStatus::Backlog,
                    ).await {
                        set_error_msg.set(Some(format!("Failed to archive task: {}", e)));
                        set_processing.set(false);
                        return;
                    }
                }
                on_bulk_complete.run(());
                set_processing.set(false);
            });
        }
    };

    let on_bulk_delete_click = {
        move |_: web_sys::MouseEvent| {
            set_confirm_delete.set(true);
        }
    };

    let on_bulk_delete_confirm = {
        move |_: web_sys::MouseEvent| {
            let selected = selected_tasks.get();
            if selected.is_empty() {
                return;
            }

            set_processing.set(true);
            set_error_msg.set(None);
            set_confirm_delete.set(false);

            spawn_local(async move {
                let mut failed = 0;
                for task_id in &selected {
                    if crate::services::delete_task(task_id.to_string()).await.is_err() {
                        failed += 1;
                    }
                }
                if failed > 0 {
                    set_error_msg.set(Some(format!("Failed to delete {} task(s)", failed)));
                }
                on_bulk_complete.run(());
                set_processing.set(false);
            });
        }
    };

    let on_bulk_delete_cancel = {
        move |_: web_sys::MouseEvent| {
            set_confirm_delete.set(false);
        }
    };

    let on_clear_selection = {
        move |_: web_sys::MouseEvent| {
            set_selected_tasks.set(Vec::new());
        }
    };

    view! {
        <Show when=move || has_selection.get()>
            <div class="fixed bottom-4 left-1/2 -translate-x-1/2 z-50 bg-gray-900/95 backdrop-blur-sm border border-white/10 rounded-xl px-4 py-3 flex items-center gap-3 shadow-xl animate-modal-in">
                <span class="text-sm font-medium text-white">{move || format!("{} selected", count.get())}</span>

                <div class="flex items-center gap-2">
                    <button
                        data-testid="bulk-add-queue"
                        class="px-3 py-1.5 rounded-lg bg-blue-500/20 hover:bg-blue-500/30 text-blue-300 text-sm font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
                        on:click=on_add_to_queue
                        disabled=move || processing.get()
                        title="Add all selected tasks to queue"
                    >
                        "Add to Queue"
                    </button>
                    <button
                        data-testid="bulk-create-prs"
                        class="px-3 py-1.5 rounded-lg bg-purple-500/20 hover:bg-purple-500/30 text-purple-300 text-sm font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
                        on:click=on_bulk_pr
                        disabled=move || processing.get()
                        title="Create PRs for all selected tasks"
                    >
                        "Create PRs"
                    </button>
                    <button
                        data-testid="bulk-archive"
                        class="px-3 py-1.5 rounded-lg bg-gray-500/20 hover:bg-gray-500/30 text-gray-300 text-sm font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
                        on:click=on_bulk_archive
                        disabled=move || processing.get()
                        title="Move all selected tasks to backlog"
                    >
                        "Archive"
                    </button>

                    // Delete with confirmation
                    <Show
                        when=move || confirm_delete.get()
                        fallback=move || view! {
                            <button
                                data-testid="bulk-delete"
                                class="px-3 py-1.5 rounded-lg bg-red-500/20 hover:bg-red-500/30 text-red-300 text-sm font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
                                on:click=on_bulk_delete_click
                                disabled=move || processing.get()
                                title="Delete all selected tasks"
                            >
                                "Delete"
                            </button>
                        }
                    >
                        <div class="flex items-center gap-1 px-2 py-1 rounded-lg bg-red-500/20 border border-red-500/30">
                            <span class="text-xs text-red-300">"Confirm?"</span>
                            <button
                                class="px-2 py-0.5 rounded bg-red-500 hover:bg-red-600 text-white text-xs font-medium transition-colors"
                                on:click=on_bulk_delete_confirm
                            >
                                "Yes"
                            </button>
                            <button
                                class="px-2 py-0.5 rounded bg-white/10 hover:bg-white/20 text-white/70 text-xs transition-colors"
                                on:click=on_bulk_delete_cancel
                            >
                                "No"
                            </button>
                        </div>
                    </Show>
                </div>

                <button
                    class="p-1.5 rounded-lg hover:bg-white/10 text-white/50 hover:text-white transition-colors"
                    on:click=on_clear_selection
                    title="Clear selection"
                    aria-label="Clear selection"
                >
                    <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                    </svg>
                </button>

                <Show when=move || processing.get()>
                    <span class="text-sm text-white/50">"Processing..."</span>
                </Show>

                <Show when=move || error_msg.get().is_some()>
                    <span class="text-sm text-red-300">{move || error_msg.get().unwrap_or_default()}</span>
                </Show>
            </div>
        </Show>
    }
}

#[component]
pub fn TaskCheckbox(
    task_id: Uuid,
    selected_tasks: Signal<Vec<Uuid>>,
    set_selected_tasks: WriteSignal<Vec<Uuid>>,
    children: Children,
) -> impl IntoView {
    let is_selected = Signal::derive(move || {
        selected_tasks.get().contains(&task_id)
    });

    let on_toggle = move |_| {
        let mut tasks = selected_tasks.get();
        if tasks.contains(&task_id) {
            tasks.retain(|id| id != &task_id);
        } else {
            tasks.push(task_id);
        }
        set_selected_tasks.set(tasks);
    };

    view! {
        <label class="group relative flex items-start gap-3 cursor-pointer">
            <div class="flex items-center">
                <input
                    type="checkbox"
                    class=format!(
                        "w-4 h-4 rounded border-2 transition-all cursor-pointer {}",
                        if is_selected.get() {
                            "border-yellow-500 bg-yellow-500 text-black"
                        } else {
                            "border-white/20 bg-transparent hover:border-yellow-500/50"
                        }
                    )
                    checked=is_selected
                    on:change=on_toggle
                />
            </div>
            <div class="flex-1 min-w-0">
                {children()}
            </div>
        </label>
    }
}
