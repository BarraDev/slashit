use leptos::prelude::*;
use leptos::task::spawn_local;
use crate::models::Subtask;
use crate::services::toggle_subtask;

#[component]
pub fn SubtaskList(subtasks: Vec<Subtask>, task_id: String) -> impl IntoView {
    if subtasks.is_empty() {
        return {
            ().into_any()
        };
    }

    let completed_count = subtasks.iter().filter(|s| s.completed).count();
    let total_count = subtasks.len();

    view! {
        <div class="subtask-list">
            <div class="subtask-summary">
                <small>{format!("Subtasks: {}/{}", completed_count, total_count)}</small>
            </div>
            <For
                each=move || subtasks.clone()
                key=|s| s.id.to_string()
                children=move |subtask| {
                    let task_id = task_id.clone();
                    let subtask_id = subtask.id.to_string();
                    let completed = subtask.completed;

                    view! {
                        <div class="subtask" class:completed=completed>
                            <input
                                type="checkbox"
                                checked=completed
                                on:change=move |_| {
                                    let task_id = task_id.clone();
                                    let subtask_id = subtask_id.clone();
                                    spawn_local(async move {
                                        if let Err(e) = toggle_subtask(task_id, subtask_id).await {
                                            eprintln!("Failed to toggle subtask: {}", e);
                                        }
                                    });
                                }
                            />
                            <span>{subtask.title}</span>
                        </div>
                    }
                }
            />
        </div>
    }.into_any()
}
