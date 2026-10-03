use crate::components::custom_select::{CustomSelect, SelectOption};
use crate::components::toast;
use crate::models::{Project, ProjectScope, Workspace};
use crate::services::{attach_project_to_workspace, detach_project_from_workspace};
use leptos::callback::Callback;
use leptos::prelude::*;
use leptos::task::spawn_local;

/// What a Workspace does and does not do for a member Project's Tasks, shown
/// with its member list. The agent may read Workspace context, but only the
/// Task Checkout is reviewed, committed and sent to a pull request.
const TASK_BOUNDARY_NOTE: &str = "Tasks in these projects start from the workspace folder, when it exists, and read its \
shared instructions, but each Task edits only its own checkout. Review, commit and pull request \
cover that checkout only; changes made elsewhere in the workspace are not included.";

/// Projects whose membership names a Workspace missing from `workspaces`,
/// sorted by name.
///
/// This happens when the registry lost a Workspace -- for example a corrupt
/// `workspaces.toml` was quarantined -- while the Project still records its
/// membership. Such a Project is listed under no Workspace and is not offered
/// for attaching, so without this it would vanish from the page.
///
/// Returns nothing unless the registry has actually been loaded: an empty
/// list that only stands in for "not loaded yet" or "failed to load" would
/// report every member as unresolved and offer to detach it.
fn unresolved_members(
    projects: &[Project],
    workspaces: &[Workspace],
    workspaces_loaded: bool,
) -> Vec<Project> {
    if !workspaces_loaded {
        return Vec::new();
    }
    let mut unresolved: Vec<Project> = projects
        .iter()
        .filter(|p| {
            p.scope
                .workspace_id()
                .is_some_and(|id| !workspaces.iter().any(|w| w.id == id))
        })
        .cloned()
        .collect();
    unresolved.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    unresolved
}

#[component]
pub fn WorkspacePanel(
    workspaces: ReadSignal<Vec<Workspace>>,
    /// Whether `workspaces` holds the registry as loaded, not a placeholder.
    workspaces_loaded: ReadSignal<bool>,
    projects: ReadSignal<Vec<Project>>,
    /// Fired after every attach or detach attempt, successful or refused, so
    /// the Workspaces page can reload the project list it owns and passes in
    /// as `projects`.
    on_membership_change: Callback<()>,
) -> impl IntoView {
    // No `overflow-hidden` here: the attach dropdown is absolutely positioned
    // inside the panel, and clipping would hide its options below the last
    // workspace row.
    view! {
        <div class="border border-white/10 rounded-xl bg-white/[0.02]">
            <div class="px-4 py-3 border-b border-white/5">
                <div class="flex items-center gap-2">
                    <svg class="w-5 h-5 text-white/40" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M3 7h18M3 12h18M3 17h18" />
                    </svg>
                    <h2 class="font-semibold text-white/90">"Workspaces"</h2>
                    <span class="px-2 py-0.5 rounded text-xs font-medium bg-white/5 text-white/40">
                        {move || workspaces.with(Vec::len)}
                    </span>
                </div>
            </div>

            <div class="divide-y divide-white/5">
                <For
                    each=move || workspaces.get()
                    key=|w| w.id
                    children=move |w| view! {
                        <WorkspaceItem workspace=w projects=projects on_membership_change=on_membership_change />
                    }
                />
            </div>

            <Show when=move || workspaces.with(Vec::is_empty)>
                <div class="p-8 text-center">
                    <p class="text-white/40 text-sm">"No workspaces configured"</p>
                </div>
            </Show>

            <UnresolvedMemberships
                workspaces=workspaces
                workspaces_loaded=workspaces_loaded
                projects=projects
                on_membership_change=on_membership_change
            />
        </div>
    }
}

/// Projects whose Workspace is missing from the registry, each with the id it
/// still records and a Detach that returns it to standalone. Nothing here
/// recreates the missing Workspace or changes membership on its own.
#[component]
fn UnresolvedMemberships(
    workspaces: ReadSignal<Vec<Workspace>>,
    workspaces_loaded: ReadSignal<bool>,
    projects: ReadSignal<Vec<Project>>,
    on_membership_change: Callback<()>,
) -> impl IntoView {
    let (busy, set_busy) = signal(false);
    let unresolved = move || {
        projects.with(|ps| {
            workspaces.with(|ws| unresolved_members(ps, ws, workspaces_loaded.get()))
        })
    };

    let on_detach = move |project_id: String, project_name: String| {
        set_busy.set(true);
        spawn_local(async move {
            match detach_project_from_workspace(project_id).await {
                Ok(_) => {
                    toast::success(format!("Detached {}; it is now standalone", project_name));
                }
                Err(e) => {
                    toast::error(format!("Failed to detach project: {}", e));
                }
            }
            on_membership_change.run(());
            set_busy.set(false);
        });
    };

    view! {
        <Show when=move || !unresolved().is_empty()>
            <div class="border-t border-white/10 px-4 py-3 space-y-2" data-testid="unresolved-memberships">
                <h3 class="text-xs font-medium text-amber-300/80 uppercase tracking-wide">"Unresolved membership"</h3>
                <p class="text-xs text-white/40">
                    "These projects belong to a workspace that is no longer registered. "
                    "Detach one to make it standalone; the missing workspace is not recreated."
                </p>
                <div class="space-y-1">
                    <For
                        each=unresolved
                        key=|p| p.id
                        children=move |p| {
                            let name = p.name.clone();
                            let name_for_detach = name.clone();
                            let pid = p.id.to_string();
                            let row_testid = format!("unresolved-member-{}", pid);
                            let missing = p.scope.workspace_id().map(|id| id.to_string()).unwrap_or_default();
                            view! {
                                <div
                                    data-testid=row_testid
                                    class="flex items-center justify-between gap-3 px-3 py-2 rounded-lg bg-amber-500/[0.06]"
                                >
                                    <div class="min-w-0">
                                        // Both lines wrap rather than truncate: the full name and
                                        // id are what identify the project and the missing
                                        // workspace.
                                        <p class="text-sm text-white/80 break-words" data-testid="unresolved-project-name">{name}</p>
                                        <p class="text-xs text-white/40 font-mono break-all">
                                            "Missing workspace: "
                                            <span data-testid="unresolved-workspace-id">{missing}</span>
                                        </p>
                                    </div>
                                    <button
                                        data-testid="unresolved-detach"
                                        disabled=move || busy.get()
                                        on:click=move |_| on_detach(pid.clone(), name_for_detach.clone())
                                        class="text-xs px-2 py-1 rounded bg-white/5 hover:bg-red-500/20 hover:text-red-300 text-white/50 transition-colors disabled:opacity-50"
                                    >
                                        "Detach"
                                    </button>
                                </div>
                            }
                        }
                    />
                </div>
            </div>
        </Show>
    }
}

#[component]
fn WorkspaceItem(
    workspace: Workspace,
    projects: ReadSignal<Vec<Project>>,
    on_membership_change: Callback<()>,
) -> impl IntoView {
    let (expanded, set_expanded) = signal(false);
    let (busy, set_busy) = signal(false);
    let (selected_to_attach, set_selected_to_attach) = signal(String::new());
    let ws_id = workspace.id;
    let ws_path = workspace.root_path.clone();
    let ws_path_for_detail = ws_path.clone();
    let ws_path_for_header = ws_path.clone();
    let ws_name = workspace.name.clone();

    let members = move || {
        projects
            .get()
            .into_iter()
            .filter(|p| p.scope.workspace_id() == Some(ws_id))
            .collect::<Vec<_>>()
    };
    let eligible_options = move || {
        projects
            .get()
            .into_iter()
            .filter(|p| p.scope == ProjectScope::Standalone)
            .map(|p| SelectOption::new(p.id.to_string(), p.name))
            .collect::<Vec<_>>()
    };

    let on_attach = move |_| {
        let project_id = selected_to_attach.get();
        if project_id.is_empty() {
            toast::error("Pick a project to attach".to_string());
            return;
        }
        set_busy.set(true);
        spawn_local(async move {
            match attach_project_to_workspace(project_id, ws_id.to_string()).await {
                Ok(project) => {
                    toast::success(format!("Attached {} to this workspace", project.name));
                    set_selected_to_attach.set(String::new());
                    on_membership_change.run(());
                }
                Err(e) => {
                    toast::error(format!("Failed to attach project: {}", e));
                    // A refusal usually means this page is out of date (another
                    // client changed the membership), so resync rather than keep
                    // offering what the backend just declined. The selection is
                    // cleared too: after the resync the project may no longer be
                    // offered, and Attach must not resubmit a hidden choice.
                    set_selected_to_attach.set(String::new());
                    on_membership_change.run(());
                }
            }
            set_busy.set(false);
        });
    };

    let on_detach = move |project_id: String, project_name: String| {
        set_busy.set(true);
        spawn_local(async move {
            match detach_project_from_workspace(project_id).await {
                Ok(_) => {
                    toast::success(format!("Detached {} from this workspace", project_name));
                    on_membership_change.run(());
                }
                Err(e) => {
                    toast::error(format!("Failed to detach project: {}", e));
                    on_membership_change.run(());
                }
            }
            set_busy.set(false);
        });
    };

    view! {
        <div class="group" data-testid=format!("workspace-{}", ws_id)>
            <button
                data-testid="workspace-toggle"
                on:click=move |_| set_expanded.update(|e| *e = !*e)
                class="w-full px-4 py-3 flex items-center justify-between hover:bg-white/5 transition-colors"
            >
                <div class="flex items-center gap-3 flex-1 min-w-0">
                    <svg class=move || format!(
                        "w-4 h-4 text-white/30 transition-transform {}",
                        if expanded.get() { "rotate-90" } else { "" }
                    ) fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7" />
                    </svg>
                    <div class="flex-1 min-w-0 text-left">
                        <h3 class="font-medium text-white/90 truncate">{ws_name}</h3>
                        <p class="text-xs text-white/40 truncate font-mono mt-0.5">{ws_path_for_header}</p>
                    </div>
                </div>
                <span class="px-2 py-0.5 rounded text-xs font-medium bg-white/5 text-white/40">
                    {move || members().len()}
                </span>
            </button>

            <Show when=move || expanded.get()>
                <div class="px-4 pb-4 pl-11 space-y-3">
                    <div class="bg-black/40 rounded-lg p-3 space-y-2">
                        <div class="flex items-center justify-between text-sm">
                            <span class="text-white/40">"ID:"</span>
                            <span class="text-white/70 font-mono text-xs">{ws_id.to_string()}</span>
                        </div>
                        <div class="flex items-center justify-between text-sm">
                            <span class="text-white/40">"Path:"</span>
                            <span class="text-white/70 font-mono text-xs truncate ml-4">{ws_path_for_detail.clone()}</span>
                        </div>
                    </div>

                    <div class="space-y-2">
                        <h4 class="text-xs font-medium text-white/40 uppercase tracking-wide">"Projects in this workspace"</h4>
                        <p class="text-xs text-white/40" data-testid="workspace-task-boundary">{TASK_BOUNDARY_NOTE}</p>
                        <div class="space-y-1" data-testid="workspace-members">
                            <For
                                each=members
                                key=|p| p.id
                                children=move |p| {
                                    let name = p.name.clone();
                                    let name_for_detach = name.clone();
                                    let pid = p.id.to_string();
                                    let member_testid = format!("workspace-member-{}", pid);
                                    view! {
                                        <div
                                            data-testid=member_testid
                                            class="flex items-center justify-between px-3 py-2 rounded-lg bg-white/[0.03]"
                                        >
                                            <span class="text-sm text-white/80 truncate">{name}</span>
                                            <button
                                                data-testid="workspace-detach"
                                                disabled=move || busy.get()
                                                on:click=move |_| on_detach(pid.clone(), name_for_detach.clone())
                                                class="text-xs px-2 py-1 rounded bg-white/5 hover:bg-red-500/20 hover:text-red-300 text-white/50 transition-colors disabled:opacity-50"
                                            >
                                                "Detach"
                                            </button>
                                        </div>
                                    }
                                }
                            />
                            <Show when=move || members().is_empty()>
                                <p class="text-xs text-white/30 px-3 py-2">"No projects attached yet"</p>
                            </Show>
                        </div>
                    </div>

                    <div class="space-y-2">
                        <h4 class="text-xs font-medium text-white/40 uppercase tracking-wide">"Attach a project"</h4>
                        <Show
                            when=move || !eligible_options().is_empty()
                            fallback=|| view! {
                                <p class="text-xs text-white/30">"No standalone projects available to attach"</p>
                            }
                        >
                            <div class="flex gap-2" data-testid="workspace-attach">
                                <div class="flex-1">
                                    <CustomSelect
                                        options=eligible_options()
                                        selected=Signal::derive(move || selected_to_attach.get())
                                        on_change=Callback::new(move |v| set_selected_to_attach.set(v))
                                        placeholder="Select a project...".to_string()
                                        disabled=busy.get()
                                    />
                                </div>
                                <button
                                    data-testid="workspace-attach-submit"
                                    disabled=move || busy.get() || selected_to_attach.get().is_empty()
                                    on:click=on_attach
                                    class="px-3 py-2 rounded-lg bg-yellow-500 hover:bg-yellow-600 text-black text-sm font-medium disabled:opacity-50 transition-colors"
                                >
                                    "Attach"
                                </button>
                            </div>
                        </Show>
                    </div>
                </div>
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{unresolved_members, TASK_BOUNDARY_NOTE};
    use crate::models::{AgentType, Project, ProjectScope, Workspace};
    use uuid::Uuid;

    fn project(name: &str, scope: ProjectScope) -> Project {
        Project {
            id: Uuid::new_v4(),
            name: name.to_string(),
            repository_id: None,
            scope,
            agent_type: AgentType::ClaudeCode,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn workspace() -> Workspace {
        Workspace {
            id: Uuid::new_v4(),
            name: "ws".to_string(),
            root_path: "/tmp/ws".to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn names(projects: &[Project]) -> Vec<&str> {
        projects.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn the_boundary_note_limits_delivery_to_the_task_checkout() {
        assert!(TASK_BOUNDARY_NOTE.contains("edits only its own checkout"));
        assert!(TASK_BOUNDARY_NOTE.contains("cover that checkout only"));
        assert!(TASK_BOUNDARY_NOTE.contains("are not included"));
    }

    #[test]
    fn only_members_of_a_missing_workspace_are_unresolved() {
        let kept = workspace();
        let lost = Uuid::new_v4();
        let projects = vec![
            project("standalone", ProjectScope::Standalone),
            project("member", ProjectScope::InWorkspace { workspace_id: kept.id }),
            project("orphan-b", ProjectScope::InWorkspace { workspace_id: lost }),
            project("orphan-a", ProjectScope::InWorkspace { workspace_id: lost }),
        ];

        let unresolved = unresolved_members(&projects, &[kept], true);

        assert_eq!(names(&unresolved), ["orphan-a", "orphan-b"]);
        assert!(unresolved
            .iter()
            .all(|p| p.scope.workspace_id() == Some(lost)));
    }

    #[test]
    fn an_empty_registry_leaves_every_member_unresolved() {
        let lost = Uuid::new_v4();
        let projects = vec![project("orphan", ProjectScope::InWorkspace { workspace_id: lost })];

        assert_eq!(names(&unresolved_members(&projects, &[], true)), ["orphan"]);
    }

    #[test]
    fn nothing_is_unresolved_before_the_registry_has_loaded() {
        // The page starts with an empty list and falls back to one on a load
        // error; neither proves a workspace is missing.
        let projects = vec![project(
            "member",
            ProjectScope::InWorkspace { workspace_id: Uuid::new_v4() },
        )];

        assert!(unresolved_members(&projects, &[], false).is_empty());
    }
}
