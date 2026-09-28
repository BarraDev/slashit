use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::callback::Callback;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use crate::components::{Kanban, bulk_actions::BulkActions, toast};
use crate::components::task_live::ResponseOrder;
use crate::services::list_tasks;
use crate::models::Task;
use uuid::Uuid;

#[component]
pub fn Dashboard(project_id: String) -> impl IntoView {
    let (tasks, set_tasks) = signal(Vec::<Task>::new());
    let (selected_tasks, set_selected_tasks) = signal(Vec::<Uuid>::new());
    let (loading, set_loading) = signal(true);
    let (_error, set_error) = signal(None::<String>);
    let (has_project, set_has_project) = signal(!project_id.is_empty());

    let project_id_clone = project_id.clone();
    let project_id_for_effect = project_id.clone();

    // Load tasks when project_id changes
    Effect::new(move |prev_project: Option<String>| {
        let project_id = project_id_for_effect.clone();
        let is_new_project = prev_project.as_ref() != Some(&project_id);
        
        set_has_project.set(!project_id.is_empty());
        
        if project_id.is_empty() {
            set_loading.set(false);
            set_tasks.set(Vec::new());
            return project_id.clone();
        }
        
        // Only reload if project changed
        if is_new_project {
            set_loading.set(true);
            set_error.set(None);
            
            let pid = project_id.clone();
            spawn_local(async move {
                match list_tasks(pid).await {
                    Ok(t) => {
                        let count = t.len();
                        set_tasks.set(t);
                        set_error.set(None);
                        let _ = count; // tasks loaded silently
                    }
                    Err(e) => {
                        leptos::logging::warn!("Failed to load tasks: {}", e);
                        set_tasks.set(Vec::new());
                        set_error.set(Some(format!("Failed to load tasks: {}", e)));
                        toast::error(format!("Failed to load tasks: {}", e));
                    }
                }
                set_loading.set(false);
            });
        }
        
        project_id.clone()
    });

    // Every read of the task list after the first goes through here: the
    // poll, a live event saying a task changed, and a Stop. They overlap, so
    // each read takes a ticket and a late answer never overwrites a newer one.
    let order = StoredValue::new(ResponseOrder::default());
    let project_id_for_refresh = project_id.clone();
    let refresh_tasks = Callback::new(move |()| {
        let pid = project_id_for_refresh.clone();
        if pid.is_empty() {
            return;
        }
        let Some(ticket) = order.try_update_value(|o| o.issue()) else {
            return;
        };
        spawn_local(async move {
            if let Ok(t) = list_tasks(pid).await {
                let current = order.try_update_value(|o| o.accept(ticket)).unwrap_or(false);
                // Only update if tasks actually changed to avoid re-rendering
                // (which would destroy open modals/menus)
                if current && t != tasks.get_untracked() {
                    set_tasks.set(t);
                }
            }
        });
    });

    // A command's own answer is newer than any read issued before it was
    // applied, so those are dropped. One of them may have been issued after
    // the command ran and be newer still (the queue can start a re-queued
    // task at once), so a fresh read follows and restores anything dropped.
    let apply_task = Callback::new(move |task: Task| {
        order.update_value(|o| o.supersede_pending());
        set_tasks.update(|tasks| {
            if let Some(existing) = tasks.iter_mut().find(|t| t.id == task.id) {
                *existing = task;
            }
        });
        refresh_tasks.run(());
    });

    // A pull request attempt this board may not have started (one begun
    // before a project switch, say) has been answered: read the record again
    // now rather than show the one from before the attempt until the poll.
    if let Some(deliveries) = crate::components::attention::Deliveries::get() {
        Effect::new(move |seen: Option<u64>| {
            let settled = deliveries.settled();
            if seen.is_some_and(|seen| seen != settled) {
                refresh_tasks.run(());
            }
            settled
        });
    }

    // Poll tasks every 5s to reflect backend status changes (auto-promotion, review transitions)
    let project_id_for_poll = project_id.clone();
    Effect::new(move |_| {
        if project_id_for_poll.is_empty() {
            return;
        }

        let cb = Closure::wrap(Box::new(move || refresh_tasks.run(())) as Box<dyn Fn()>);

        let window = web_sys::window().unwrap();
        let interval_id = window.set_interval_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(), 5_000
        ).unwrap();
        cb.forget();

        on_cleanup(move || {
            if let Some(w) = web_sys::window() {
                w.clear_interval_with_handle(interval_id);
            }
        });
    });

    let on_bulk_complete = Callback::new(move |()| {
        set_selected_tasks.set(Vec::new());
        toast::success("Bulk action completed".to_string());
    });

    let project_id_for_bulk = project_id.clone();
    let project_id_for_kanban = project_id.clone();

    view! {
        <div class="h-full flex flex-col">
            {move || {
                let project_id_kanban = project_id_for_kanban.clone();
                let project_id_bulk = project_id_for_bulk.clone();
                
                if !has_project.get() {
                    // No project selected - show welcome state
                    view! {
                        <div class="flex-1 flex items-center justify-center">
                            <div class="text-center max-w-md">
                                <div class="w-20 h-20 mx-auto mb-6 rounded-2xl bg-gradient-to-br from-blue-500/20 to-purple-500/20 flex items-center justify-center">
                                    <svg class="w-10 h-10 text-blue-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M9 17V7m0 10a2 2 0 01-2 2H5a2 2 0 01-2-2V7a2 2 0 012-2h2a2 2 0 012 2m0 10a2 2 0 002 2h2a2 2 0 002-2M9 7a2 2 0 012-2h2a2 2 0 012 2m0 10V7m0 10a2 2 0 002 2h2a2 2 0 002-2V7a2 2 0 00-2-2h-2a2 2 0 00-2 2" />
                                    </svg>
                                </div>
                                <h2 class="text-2xl font-bold text-white/90 mb-3">"Welcome to SlashIt"</h2>
                                <p class="text-white/50 mb-6">"Select a project from the sidebar or create a new one to get started with your Kanban board."</p>
                                <div class="flex items-center justify-center gap-2 text-sm text-white/30">
                                    <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 7l5 5m0 0l-5 5m5-5H6" />
                                    </svg>
                                    "Click a project in the sidebar"
                                </div>
                            </div>
                        </div>
                    }.into_any()
                } else if loading.get() {
                    // Loading state
                    view! {
                        <div class="flex-1 flex items-center justify-center">
                            <div class="flex flex-col items-center gap-4">
                                <svg class="w-10 h-10 animate-spin text-yellow-500" fill="none" viewBox="0 0 24 24">
                                    <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
                                    <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
                                </svg>
                                <span class="text-white/50">"Loading tasks..."</span>
                            </div>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <>
                            <div class="flex-1 min-h-0">
                            <Kanban
                                tasks=tasks
                                set_tasks=set_tasks
                                project_id=project_id_kanban
                                selected_tasks=Signal::from(selected_tasks)
                                set_selected_tasks=set_selected_tasks
                                refresh_tasks=refresh_tasks
                                apply_task=apply_task
                            />
                            </div>

                            <BulkActions
                                selected_tasks=selected_tasks.into()
                                set_selected_tasks=set_selected_tasks
                                project_id=project_id_bulk
                                on_bulk_complete=on_bulk_complete
                            />
                        </>
                    }.into_any()
                }
            }}
        </div>
    }
}
