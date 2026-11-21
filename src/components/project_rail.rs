// SPDX-License-Identifier: MPL-2.0
use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::callback::Callback;
use uuid::Uuid;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use crate::models::{Project, Workspace};
use crate::services::{list_projects, workspace_service::list_workspaces};
use crate::services::human_review_service::get_attention_summary;
use crate::components::attention::BoardAttention;
use crate::components::task_live::ResponseOrder;
use crate::components::project_organizer::ProjectOrganizer;
use crate::components::{CreateProjectModal, ExportPhase, ExportProjectModal, ImportProjectModal, start_export};

/// How often the rail reads "Needs you" for the projects not on screen.
const ATTENTION_POLL_MS: i32 = 5_000;

/// Where a Project sits in the rail, given the Workspaces the registry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Standalone,
    Member(Uuid),
    /// Names a Workspace the registry does not hold. It stays visible until
    /// the membership is repaired, rather than vanishing from the rail.
    Unresolved(Uuid),
}

fn placement(project: &Project, known: &HashSet<Uuid>) -> Placement {
    match project.scope.workspace_id() {
        None => Placement::Standalone,
        Some(w) if known.contains(&w) => Placement::Member(w),
        Some(w) => Placement::Unresolved(w),
    }
}

/// Vertical project switcher rail — sits on the far left edge of the window.
/// Shows Workspace groups and standalone Projects in a narrow strip (~48px),
/// expanding on hover to reveal names.
///
/// A Workspace row has two separate controls: the chevron shows or hides its
/// member Projects, and the name enters Workspace context. Selecting a Project
/// leaves Workspace context.
#[component]
pub fn ProjectRail(
    #[prop(into)] selected_project: Signal<String>,
    set_selected_project: WriteSignal<String>,
    #[prop(into)] selected_workspace: Signal<String>,
    set_selected_workspace: WriteSignal<String>,
) -> impl IntoView {
    let projects = RwSignal::new(Vec::<Project>::new());
    // None until the registry has been read; Some(Err) when it could not be
    // read. Projects still show in that case, flat, with a notice.
    let workspaces = RwSignal::new(None::<Result<Vec<Workspace>, String>>);
    let collapsed = RwSignal::new(HashSet::<Uuid>::new());
    let (hovered, set_hovered) = signal(false);
    let (show_create_modal, set_show_create_modal) = signal(false);
    let (refresh_trigger, set_refresh_trigger) = signal(0u32);
    let (show_import_modal, set_show_import_modal) = signal(false);
    let (show_organizer, set_show_organizer) = signal(false);
    let export_phase = RwSignal::new(None::<ExportPhase>);

    // Reads overlap (one per refresh). Only the newest read is applied.
    let reads = StoredValue::new(0u64);
    let reload = move || {
        reads.update_value(|n| *n += 1);
        let read = reads.get_value();
        spawn_local(async move {
            // The registry is read first and the Projects after it, never both
            // at once: a newer Project list could name a Workspace that the
            // older registry does not have yet.
            let ws = list_workspaces().await;
            let ps = list_projects().await;
            if reads.get_value() != read {
                return;
            }
            workspaces.set(Some(ws));
            if let Ok(ps) = ps {
                projects.set(ps);
            }
        });
    };

    // Load on mount and whenever a mutation asks for a refresh.
    Effect::new(move |prev: Option<u32>| {
        let current = refresh_trigger.get();
        if let Some(prev) = prev {
            if prev == current { return current; }
        }
        reload();
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
        set_selected_workspace.set(String::new());
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
            <div class="flex-1 flex flex-col gap-1 py-2 overflow-y-auto overflow-x-hidden">
                {move || {
                    let active_project = selected_project.get();
                    let active_workspace = selected_workspace.get();
                    let is_expanded = hovered.get();
                    let all = projects.get();

                    // One Project row. `indented` marks a member of a Workspace.
                    let project_row = |project: &Project, indented: bool| -> AnyView {
                        let pid = project.id.to_string();
                        let pid_click = pid.clone();
                        let is_active = active_workspace.is_empty() && pid == active_project;
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
                                on:click=move |_| {
                                    set_selected_workspace.set(String::new());
                                    set_selected_project.set(pid_click.clone());
                                }
                                class="rail-item"
                                class:rail-member=indented
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
                        }.into_any()
                    };

                    let mut rows: Vec<AnyView> = Vec::new();
                    match workspaces.get() {
                        // The registry has not been read yet: show Projects flat.
                        None => rows.extend(all.iter().map(|p| project_row(p, false))),
                        Some(Err(message)) => {
                            rows.push(view! {
                                <div
                                    role="status"
                                    data-testid="rail-workspaces-unavailable"
                                    title=format!("Workspace groups could not be read: {message}. Your Projects are listed without groups.")
                                    class="rail-notice"
                                >
                                    <div class="rail-badge warn"><span>"!"</span></div>
                                    {is_expanded.then(|| view! {
                                        <span class="rail-label">{format!("Groups unavailable: {message}")}</span>
                                    })}
                                </div>
                            }.into_any());
                            rows.extend(all.iter().map(|p| project_row(p, false)));
                        }
                        Some(Ok(list)) => {
                            let known: HashSet<Uuid> = list.iter().map(|w| w.id).collect();
                            for ws in &list {
                                let wid = ws.id;
                                let wid_s = wid.to_string();
                                let open = collapsed.with(|c| !c.contains(&wid));
                                let is_active_ws = active_workspace == wid_s;
                                let wid_click = wid_s.clone();
                                let ws_name = ws.name.clone();
                                let initial = ws_name.chars().next().unwrap_or('W').to_uppercase().to_string();
                                let members: Vec<AnyView> = all
                                    .iter()
                                    .filter(|p| placement(p, &known) == Placement::Member(wid))
                                    .map(|p| project_row(p, true))
                                    .collect();
                                let toggle_label = format!(
                                    "{} {}",
                                    if open { "Collapse" } else { "Expand" },
                                    ws_name
                                );
                                let select_label = format!("Workspace {ws_name}");
                                rows.push(view! {
                                    <div class="rail-group" data-testid=format!("rail-workspace-{wid_s}")>
                                        <div class="rail-workspace-row">
                                            <button
                                                type="button"
                                                data-testid=format!("rail-workspace-toggle-{wid_s}")
                                                aria-expanded=open.to_string()
                                                aria-label=toggle_label.clone()
                                                title=toggle_label
                                                class="rail-toggle"
                                                on:click=move |_| collapsed.update(|c| {
                                                    if !c.remove(&wid) {
                                                        c.insert(wid);
                                                    }
                                                })
                                            >
                                                <svg class=if open { "w-3 h-3 transition-transform rotate-90" } else { "w-3 h-3 transition-transform" } fill="none" viewBox="0 0 24 24" stroke="currentColor" aria-hidden="true">
                                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7" />
                                                </svg>
                                            </button>
                                            <button
                                                type="button"
                                                data-testid=format!("rail-workspace-select-{wid_s}")
                                                aria-label=select_label.clone()
                                                aria-current=is_active_ws.then_some("true")
                                                title=select_label
                                                class="rail-item rail-workspace"
                                                class:active=is_active_ws
                                                on:click=move |_| set_selected_workspace.set(wid_click.clone())
                                            >
                                                <div class="rail-badge workspace" class:active=is_active_ws>
                                                    <span>{initial}</span>
                                                </div>
                                                {is_expanded.then(|| view! {
                                                    <span class="rail-label">{ws_name.clone()}</span>
                                                })}
                                            </button>
                                        </div>
                                        {open.then(|| members.into_iter().collect_view())}
                                    </div>
                                }.into_any());
                            }

                            // A Project whose Workspace is missing from the registry
                            // stays visible here until the user repairs it.
                            let unresolved: Vec<AnyView> = all
                                .iter()
                                .filter(|p| matches!(placement(p, &known), Placement::Unresolved(_)))
                                .map(|p| project_row(p, false))
                                .collect();
                            if !unresolved.is_empty() {
                                rows.push(view! {
                                    <div data-testid="rail-unresolved" class="rail-notice" role="status">
                                        <div class="rail-badge warn"><span>"!"</span></div>
                                        {is_expanded.then(|| view! {
                                            <span class="rail-label">"Unresolved membership: open Organize Projects"</span>
                                        })}
                                    </div>
                                }.into_any());
                                rows.extend(unresolved);
                            }

                            let standalone: Vec<AnyView> = all
                                .iter()
                                .filter(|p| placement(p, &known) == Placement::Standalone)
                                .map(|p| project_row(p, false))
                                .collect();
                            if !list.is_empty() && !standalone.is_empty() {
                                rows.push(view! { <div class="rail-separator" aria-hidden="true"></div> }.into_any());
                            }
                            rows.extend(standalone);
                        }
                    }
                    rows
                }}
            </div>

            // Add project button
            <div class="py-2 border-t border-white/5">
                <button
                    data-testid="rail-import-project"
                    aria-label="Import project"
                    on:click=move |_| set_show_import_modal.set(true)
                    class="rail-item"
                    title="Import project from a file"
                >
                    <div class="rail-badge add">
                        <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 16v2a2 2 0 002 2h12a2 2 0 002-2v-2M7 10l5 5m0 0l5-5m-5 5V3" />
                        </svg>
                    </div>
                    {move || hovered.get().then(|| view! {
                        <span class="rail-label">"Import project"</span>
                    })}
                </button>
                <button
                    data-testid="rail-export-project"
                    aria-label="Export selected project"
                    disabled=move || selected_project.get().is_empty() || !selected_workspace.get().is_empty()
                    on:click=move |_| {
                        let id = selected_project.get_untracked();
                        if let Some(project) = projects.get_untracked().into_iter().find(|p| p.id.to_string() == id) {
                            start_export(id, project.name, export_phase);
                        }
                    }
                    class="rail-item disabled:opacity-30"
                    title="Export the selected project to a file"
                >
                    <div class="rail-badge add">
                        <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 16v2a2 2 0 002 2h12a2 2 0 002-2v-2M16 8l-4-4m0 0L8 8m4-4v12" />
                        </svg>
                    </div>
                    {move || hovered.get().then(|| view! {
                        <span class="rail-label">"Export project"</span>
                    })}
                </button>
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
                <button
                    data-testid="rail-organize-projects"
                    aria-label="Organize projects"
                    on:click=move |_| set_show_organizer.set(true)
                    class="rail-item"
                    title="Organize projects and workspaces"
                >
                    <div class="rail-badge add">
                        <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 6h10M18 6h2M4 12h4M12 12h8M4 18h12M20 18h0M14 4v4M8 10v4M16 16v4" />
                        </svg>
                    </div>
                    {move || hovered.get().then(|| view! {
                        <span class="rail-label">"Organize projects"</span>
                    })}
                </button>
            </div>

            <ImportProjectModal
                show=show_import_modal
                set_show=set_show_import_modal
                on_project_created=on_project_created
                on_list_changed=Callback::new(move |_| set_refresh_trigger.update(|t| *t += 1))
            />
            <ExportProjectModal phase=export_phase />

            <CreateProjectModal
                show=show_create_modal
                set_show=set_show_create_modal
                on_project_created=on_project_created
            />

            <ProjectOrganizer
                show=show_organizer
                set_show=set_show_organizer
                selected_project=selected_project
                set_selected_project=set_selected_project
                selected_workspace=selected_workspace
                on_changed=Callback::new(move |_| set_refresh_trigger.update(|t| *t += 1))
            />
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ProjectScope;

    fn project(scope: ProjectScope) -> Project {
        let now = chrono::Utc::now();
        Project {
            id: Uuid::new_v4(),
            name: "p".to_string(),
            repository_id: None,
            scope,
            agent_type: crate::models::AgentType::ClaudeCode,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn a_member_of_a_known_workspace_is_placed_in_that_group() {
        let ws = Uuid::new_v4();
        let known = HashSet::from([ws]);
        let p = project(ProjectScope::InWorkspace { workspace_id: ws });
        assert_eq!(placement(&p, &known), Placement::Member(ws));
    }

    #[test]
    fn a_project_without_a_workspace_is_standalone() {
        let p = project(ProjectScope::Standalone);
        assert_eq!(placement(&p, &HashSet::new()), Placement::Standalone);
    }

    #[test]
    fn a_member_whose_workspace_is_missing_stays_visible_as_unresolved() {
        let missing = Uuid::new_v4();
        let p = project(ProjectScope::InWorkspace { workspace_id: missing });
        assert_eq!(
            placement(&p, &HashSet::new()),
            Placement::Unresolved(missing),
            "a Project must not disappear because its Workspace is not in the registry"
        );
    }
}
