use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::callback::Callback;
use uuid::Uuid;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use crate::models::Project;
use crate::services::list_projects;
use crate::services::human_review_service::get_attention_summary;
use crate::components::attention::BoardAttention;
use crate::components::task_live::ResponseOrder;
use crate::components::CreateProjectModal;

/// How often the rail reads "Needs you" for the projects not on screen.
const ATTENTION_POLL_MS: i32 = 5_000;

/// Vertical project switcher rail — sits on the far left edge of the window.
/// Shows project initials in a narrow strip (~48px), expands on hover to reveal names.
#[component]
pub fn ProjectRail(
    #[prop(into)] selected_project: Signal<String>,
    set_selected_project: WriteSignal<String>,
) -> impl IntoView {
    let (projects, set_projects) = signal(Vec::<Project>::new());
    let (hovered, set_hovered) = signal(false);
    let (show_create_modal, set_show_create_modal) = signal(false);
    let (refresh_trigger, set_refresh_trigger) = signal(0u32);

    // Load projects on mount and when refresh_trigger changes
    Effect::new(move |prev: Option<u32>| {
        let current = refresh_trigger.get();
        if let Some(prev) = prev {
            if prev == current { return current; }
        }
        spawn_local(async move {
            if let Ok(ps) = list_projects().await {
                set_projects.set(ps);
            }
        });
        current
    });

    // How many tasks need the user in each project. The open board's own
    // count wins for its project, so the rail never disagrees with the
    // header; the backend's summary covers the rest. Reads overlap (the
    // poll, a project switch), so a late answer never replaces a newer one.
    let summary = RwSignal::new(HashMap::<Uuid, usize>::new());
    let order = StoredValue::new(ResponseOrder::default());
    let read_attention = move || {
        let Some(ticket) = order.try_update_value(|o| o.issue()) else {
            return;
        };
        spawn_local(async move {
            let Ok(projects) = get_attention_summary().await else {
                return;
            };
            if order.try_update_value(|o| o.accept(ticket)) == Some(true) {
                summary.try_set(projects.into_iter().map(|p| (p.project_id, p.tasks.len())).collect());
            }
        });
    };
    let board = BoardAttention::get();
    let deliveries = crate::components::attention::Deliveries::get();
    Effect::new(move |_| {
        // Read again whenever the selection changes, whenever the open
        // board's count changes or goes away (leaving the board hands its
        // project back to the summary, which must not be the one from before
        // the user acted there), and whenever a pull request attempt is
        // answered.
        selected_project.track();
        if let Some(board) = board {
            board.0.track();
        }
        if let Some(deliveries) = deliveries {
            deliveries.settled();
        }
        read_attention();
    });
    {
        let cb = Closure::wrap(Box::new(read_attention) as Box<dyn Fn()>);
        let interval = web_sys::window().and_then(|w| {
            w.set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ATTENTION_POLL_MS)
                .ok()
        });
        cb.forget();
        on_cleanup(move || {
            if let (Some(w), Some(id)) = (web_sys::window(), interval) {
                w.clear_interval_with_handle(id);
            }
        });
    }
    let needs_you = move |project: Uuid| -> usize {
        match board.and_then(|b| b.0.get()) {
            Some((open, count)) if open == project => count,
            _ => summary.with(|s| s.get(&project).copied().unwrap_or(0)),
        }
    };

    let on_project_created = Callback::new(move |project: Project| {
        let pid = project.id.to_string();
        set_selected_project.set(pid);
        set_refresh_trigger.update(|t| *t += 1);
    });

    view! {
        <div
            data-testid="project-rail"
            class="project-rail"
            class:expanded=move || hovered.get()
            on:mouseenter=move |_| set_hovered.set(true)
            on:mouseleave=move |_| set_hovered.set(false)
        >
            // Project items
            <div class="flex-1 flex flex-col gap-1 py-2 overflow-y-auto overflow-x-hidden">
                {move || {
                    let active = selected_project.get();
                    let is_expanded = hovered.get();
                    projects.get().into_iter().map(|project| {
                        let pid = project.id.to_string();
                        let pid_click = pid.clone();
                        let is_active = pid == active;
                        let name = project.name.clone();
                        let initial = name.chars().next().unwrap_or('?').to_uppercase().to_string();
                        let project_uuid = project.id;
                        let testid = format!("rail-attention-{pid}");
                        let name_for_label = name.clone();
                        let label = move || match needs_you(project_uuid) {
                            0 => name_for_label.clone(),
                            n => format!("{name_for_label}, needs you: {n}"),
                        };

                        view! {
                            <button
                                data-testid=format!("rail-project-{}", pid)
                                on:click=move |_| set_selected_project.set(pid_click.clone())
                                class="rail-item"
                                class:active=is_active
                                title=label.clone()
                                aria-label=label
                            >
                                {is_active.then(|| view! {
                                    <div class="rail-active-indicator"></div>
                                })}
                                <div class="rail-badge" class:active=is_active>
                                    <span>{initial}</span>
                                    {move || {
                                        let count = needs_you(project_uuid);
                                        (count > 0).then(|| view! {
                                            <span
                                                data-testid=testid.clone()
                                                data-count=count.to_string()
                                                class="rail-attention"
                                                aria-hidden="true"
                                            >
                                                {count}
                                            </span>
                                        })
                                    }}
                                </div>
                                {is_expanded.then(|| view! {
                                    <span class="rail-label">{name.clone()}</span>
                                })}
                            </button>
                        }
                    }).collect::<Vec<_>>()
                }}
            </div>

            // Add project button
            <div class="py-2 border-t border-white/5">
                <button
                    data-testid="rail-add-project"
                    aria-label="Add project"
                    on:click=move |_| set_show_create_modal.set(true)
                    class="rail-item"
                    title="New project"
                >
                    <div class="rail-badge add">
                        <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 4v16m8-8H4" />
                        </svg>
                    </div>
                    {move || hovered.get().then(|| view! {
                        <span class="rail-label">"New project"</span>
                    })}
                </button>
            </div>

            <CreateProjectModal
                show=show_create_modal
                set_show=set_show_create_modal
                on_project_created=on_project_created
            />
        </div>
    }
}
