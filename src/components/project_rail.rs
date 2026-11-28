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

type Registry = Option<Result<Vec<Workspace>, String>>;

/// Whether selecting a Workspace must leave the current page. Workspace Home
/// is not shown on Settings or MCP, so a Workspace click from either would
/// otherwise leave the user on a page that ignores the selection.
fn opens_workspace_home_from(page: &str) -> bool {
    matches!(page, "settings" | "mcp")
}

/// One top-level entry of the rail. Its key identifies the entry itself (not
/// its position or its data), so the keyed list keeps the entry's DOM, and
/// with it keyboard focus, while the data it shows changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopRow {
    WorkspacesUnavailable,
    Workspace(Uuid),
    UnresolvedNotice,
    Project(Uuid),
    Separator,
}

impl TopRow {
    fn key(self) -> String {
        match self {
            TopRow::WorkspacesUnavailable => "notice:workspaces".to_string(),
            TopRow::Workspace(id) => format!("workspace:{id}"),
            TopRow::UnresolvedNotice => "notice:unresolved".to_string(),
            TopRow::Project(id) => format!("project:{id}"),
            TopRow::Separator => "separator".to_string(),
        }
    }
}

/// The top-level entries in display order. Membership decides whether a
/// Project is listed under its Workspace or at this level.
fn top_rows(registry: &Registry, projects: &[Project]) -> Vec<TopRow> {
    match registry {
        // The registry has not been read yet: show Projects flat.
        None => projects.iter().map(|p| TopRow::Project(p.id)).collect(),
        Some(Err(_)) => std::iter::once(TopRow::WorkspacesUnavailable)
            .chain(projects.iter().map(|p| TopRow::Project(p.id)))
            .collect(),
        Some(Ok(list)) => {
            let known: HashSet<Uuid> = list.iter().map(|w| w.id).collect();
            let mut rows: Vec<TopRow> = list.iter().map(|w| TopRow::Workspace(w.id)).collect();
            // A Project whose Workspace is missing from the registry stays
            // visible here until the user repairs it.
            let unresolved: Vec<TopRow> = projects
                .iter()
                .filter(|p| matches!(placement(p, &known), Placement::Unresolved(_)))
                .map(|p| TopRow::Project(p.id))
                .collect();
            if !unresolved.is_empty() {
                rows.push(TopRow::UnresolvedNotice);
                rows.extend(unresolved);
            }
            let standalone: Vec<TopRow> = projects
                .iter()
                .filter(|p| placement(p, &known) == Placement::Standalone)
                .map(|p| TopRow::Project(p.id))
                .collect();
            if !list.is_empty() && !standalone.is_empty() {
                rows.push(TopRow::Separator);
            }
            rows.extend(standalone);
            rows
        }
    }
}

/// The member Projects of one Workspace, in the order the list holds them.
fn member_ids(registry: &Registry, projects: &[Project], workspace: Uuid) -> Vec<Uuid> {
    let known: HashSet<Uuid> = match registry {
        Some(Ok(list)) => list.iter().map(|w| w.id).collect(),
        _ => HashSet::new(),
    };
    projects
        .iter()
        .filter(|p| placement(p, &known) == Placement::Member(workspace))
        .map(|p| p.id)
        .collect()
}

/// What every rail row reads. Rows read these signals when they render, so a
/// row stays mounted and shows fresh data instead of being rebuilt.
#[derive(Clone, Copy)]
struct RailCtx {
    projects: RwSignal<Vec<Project>>,
    workspaces: RwSignal<Registry>,
    selected_project: Signal<String>,
    set_selected_project: WriteSignal<String>,
    selected_workspace: Signal<String>,
    set_selected_workspace: WriteSignal<String>,
    current_page: Signal<String>,
    on_navigate: Callback<String>,
    hovered: ReadSignal<bool>,
    collapsed: RwSignal<HashSet<Uuid>>,
    needs_you: Callback<Uuid, usize>,
}

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
    #[prop(into)] current_page: Signal<String>,
    on_navigate: Callback<String>,
) -> impl IntoView {
    let projects = RwSignal::new(Vec::<Project>::new());
    // None until the registry has been read; Some(Err) when it could not be
    // read. Projects still show in that case, flat, with a notice.
    let workspaces = RwSignal::new(None::<Result<Vec<Workspace>, String>>);
    let collapsed = RwSignal::new(HashSet::<Uuid>::new());
    let (hovered, set_hovered) = signal(false);
    let (show_create_modal, set_show_create_modal) = signal(false);
    // Provided by App, which also gives it to the Workspace page.
    let refresh_trigger = expect_context::<ReadSignal<u32>>();
    let set_refresh_trigger = expect_context::<WriteSignal<u32>>();
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
            // try_get_value: the rail may have unmounted during the awaits. A
            // disposed counter then reads as None, which drops the reply.
            if reads.try_get_value() != Some(read) {
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

    // The rows of the list. Recomputed only when the registry or the Project
    // list changes; selection and hover do not rebuild it.
    let top = Memo::new(move |_| workspaces.with(|w| projects.with(|ps| top_rows(w, ps))));

    provide_context(RailCtx {
        projects,
        workspaces,
        selected_project,
        set_selected_project,
        selected_workspace,
        set_selected_workspace,
        current_page,
        on_navigate,
        hovered,
        collapsed,
        needs_you: Callback::new(needs_you),
    });

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
                <For
                    each=move || top.get()
                    key=|row| row.key()
                    children=move |row| match row {
                        TopRow::WorkspacesUnavailable => view! { <RailWorkspacesUnavailable /> }.into_any(),
                        TopRow::Workspace(id) => view! { <RailWorkspace id=id /> }.into_any(),
                        TopRow::UnresolvedNotice => view! { <RailUnresolvedNotice /> }.into_any(),
                        TopRow::Project(id) => view! { <RailProjectRow id=id indented=false /> }.into_any(),
                        TopRow::Separator => view! { <div class="rail-separator" aria-hidden="true"></div> }.into_any(),
                    }
                />
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

/// One Project row. Its label, attention count and active state are read from
/// the signals when they change; the row itself is not rebuilt. `indented`
/// marks a member of a Workspace.
#[component]
fn RailProjectRow(id: Uuid, indented: bool) -> impl IntoView {
    let ctx = expect_context::<RailCtx>();
    let name = move || {
        ctx.projects.with(|ps| {
            ps.iter()
                .find(|p| p.id == id)
                .map(|p| p.name.clone())
                .unwrap_or_default()
        })
    };
    let attention = move || ctx.needs_you.run(id);
    let is_active = move || {
        ctx.selected_workspace.get().is_empty() && ctx.selected_project.get() == id.to_string()
    };
    let label = move || match attention() {
        0 => name(),
        n => format!("{}, needs you: {n}", name()),
    };
    let initial = move || {
        name()
            .chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .to_string()
    };
    let attention_testid = format!("rail-attention-{id}");
    view! {
        <button
            data-testid=format!("rail-project-{id}")
            on:click=move |_| {
                ctx.set_selected_workspace.set(String::new());
                ctx.set_selected_project.set(id.to_string());
            }
            class="rail-item"
            class:rail-member=indented
            class:active=is_active
            title=label
            aria-label=label
        >
            {move || is_active().then(|| view! {
                <div class="rail-active-indicator"></div>
            })}
            <div class="rail-badge" class:active=is_active>
                <span>{initial}</span>
                {move || {
                    let count = attention();
                    (count > 0).then(|| view! {
                        <span
                            data-testid=attention_testid.clone()
                            data-count=count.to_string()
                            class="rail-attention"
                            aria-hidden="true"
                        >
                            {count}
                        </span>
                    })
                }}
            </div>
            {move || ctx.hovered.get().then(|| view! {
                <span class="rail-label">{name()}</span>
            })}
        </button>
    }
}

/// A Workspace's group: the chevron shows or hides its members, and the name
/// enters Workspace context. The two are separate controls, so the chevron
/// never changes the selection.
#[component]
fn RailWorkspace(id: Uuid) -> impl IntoView {
    let ctx = expect_context::<RailCtx>();
    let name = move || {
        ctx.workspaces.with(|w| match w {
            Some(Ok(list)) => list
                .iter()
                .find(|ws| ws.id == id)
                .map(|ws| ws.name.clone())
                .unwrap_or_default(),
            _ => String::new(),
        })
    };
    let open = move || ctx.collapsed.with(|c| !c.contains(&id));
    let is_active = move || ctx.selected_workspace.get() == id.to_string();
    let members = move || {
        ctx.workspaces
            .with(|w| ctx.projects.with(|ps| member_ids(w, ps, id)))
    };
    let toggle_label = move || format!("{} {}", if open() { "Collapse" } else { "Expand" }, name());
    let select_label = move || format!("Workspace {}", name());
    let initial = move || {
        name()
            .chars()
            .next()
            .unwrap_or('W')
            .to_uppercase()
            .to_string()
    };
    view! {
        <div class="rail-group" data-testid=format!("rail-workspace-{id}")>
            <div class="rail-workspace-row">
                <button
                    type="button"
                    data-testid=format!("rail-workspace-toggle-{id}")
                    aria-expanded=move || open().to_string()
                    aria-label=toggle_label
                    title=toggle_label
                    class="rail-toggle"
                    on:click=move |_| ctx.collapsed.update(|c| {
                        if !c.remove(&id) {
                            c.insert(id);
                        }
                    })
                >
                    <svg class=move || if open() { "w-3 h-3 transition-transform rotate-90" } else { "w-3 h-3 transition-transform" } fill="none" viewBox="0 0 24 24" stroke="currentColor" aria-hidden="true">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7" />
                    </svg>
                </button>
                <button
                    type="button"
                    data-testid=format!("rail-workspace-select-{id}")
                    aria-label=select_label
                    aria-current=move || is_active().then_some("true")
                    title=select_label
                    class="rail-item rail-workspace"
                    class:active=is_active
                    on:click=move |_| {
                        ctx.set_selected_workspace.set(id.to_string());
                        if opens_workspace_home_from(&ctx.current_page.get_untracked()) {
                            ctx.on_navigate.run("dashboard".to_string());
                        }
                    }
                >
                    <div class="rail-badge workspace" class:active=is_active>
                        <span>{initial}</span>
                    </div>
                    {move || ctx.hovered.get().then(|| view! {
                        <span class="rail-label">{name()}</span>
                    })}
                </button>
            </div>
            {move || open().then(|| view! {
                <For
                    each=members
                    key=|pid| *pid
                    children=move |pid| view! { <RailProjectRow id=pid indented=true /> }
                />
            })}
        </div>
    }
}

/// Shown when the registry could not be read. The Projects still list below it.
#[component]
fn RailWorkspacesUnavailable() -> impl IntoView {
    let ctx = expect_context::<RailCtx>();
    let message = move || {
        ctx.workspaces.with(|w| match w {
            Some(Err(message)) => message.clone(),
            _ => String::new(),
        })
    };
    view! {
        <div
            role="status"
            data-testid="rail-workspaces-unavailable"
            title=move || format!("Workspace groups could not be read: {}. Your Projects are listed without groups.", message())
            class="rail-notice"
        >
            <div class="rail-badge warn"><span>"!"</span></div>
            {move || ctx.hovered.get().then(|| view! {
                <span class="rail-label">{format!("Groups unavailable: {}", message())}</span>
            })}
        </div>
    }
}

/// Heads the Projects whose Workspace is missing from the registry.
#[component]
fn RailUnresolvedNotice() -> impl IntoView {
    let ctx = expect_context::<RailCtx>();
    view! {
        <div data-testid="rail-unresolved" class="rail-notice" role="status">
            <div class="rail-badge warn"><span>"!"</span></div>
            {move || ctx.hovered.get().then(|| view! {
                <span class="rail-label">"Unresolved membership: open Organize Projects"</span>
            })}
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
            pr_review_defaults: Default::default(),
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

    fn workspace(id: Uuid) -> Workspace {
        let now = chrono::Utc::now();
        Workspace {
            id,
            name: "ws".to_string(),
            root_path: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn a_row_keeps_its_key_when_its_project_is_renamed_or_regrouped() {
        // Keys identify the entry, so a rename (same id) keeps the DOM node
        // and its focus, and a regrouped Project keeps its own key.
        let p = project(ProjectScope::Standalone);
        let before = top_rows(&Some(Ok(vec![])), std::slice::from_ref(&p));
        let mut renamed = p.clone();
        renamed.name = "renamed".to_string();
        let after = top_rows(&Some(Ok(vec![])), std::slice::from_ref(&renamed));
        assert_eq!(
            before.iter().map(|r| r.key()).collect::<Vec<_>>(),
            after.iter().map(|r| r.key()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_list_places_members_under_their_workspace_and_standalone_below() {
        let ws = workspace(Uuid::new_v4());
        let member = project(ProjectScope::InWorkspace {
            workspace_id: ws.id,
        });
        let standalone = project(ProjectScope::Standalone);
        let rows = top_rows(&Some(Ok(vec![ws.clone()])), &[member.clone(), standalone.clone()]);
        assert_eq!(
            rows,
            vec![
                TopRow::Workspace(ws.id),
                TopRow::Separator,
                TopRow::Project(standalone.id),
            ]
        );
        assert_eq!(
            member_ids(
                &Some(Ok(vec![ws.clone()])),
                &[member.clone(), standalone],
                ws.id
            ),
            vec![member.id]
        );
    }

    #[test]
    fn a_workspace_click_from_settings_or_mcp_leaves_that_page() {
        assert!(opens_workspace_home_from("settings"));
        assert!(opens_workspace_home_from("mcp"));
        assert!(!opens_workspace_home_from("dashboard"));
        assert!(!opens_workspace_home_from("conversations"));
    }

    #[test]
    fn a_failed_registry_read_keeps_projects_visible_flat() {
        let p = project(ProjectScope::Standalone);
        let rows = top_rows(&Some(Err("corrupt".to_string())), std::slice::from_ref(&p));
        assert_eq!(
            rows,
            vec![TopRow::WorkspacesUnavailable, TopRow::Project(p.id)]
        );
    }
}
