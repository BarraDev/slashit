use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::callback::Callback;
use crate::models::Project;
use crate::services::list_projects;
use crate::components::CreateProjectModal;

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

                        view! {
                            <button
                                data-testid=format!("rail-project-{}", pid)
                                on:click=move |_| set_selected_project.set(pid_click.clone())
                                class="rail-item"
                                class:active=is_active
                                title=name.clone()
                            >
                                {is_active.then(|| view! {
                                    <div class="rail-active-indicator"></div>
                                })}
                                <div class="rail-badge" class:active=is_active>
                                    <span>{initial}</span>
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
