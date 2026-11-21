// SPDX-License-Identifier: MPL-2.0
//! The page shown in Workspace context. It names the Workspace, its effective
//! root and its active member Projects. Project tools (Kanban, terminals,
//! Project settings) are not shown here, because they belong to one Project.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::models::workspace::{Workspace, WorkspaceRootInfo};
use crate::models::Project;
use crate::services::list_projects;
use crate::services::workspace_service::{get_workspace, get_workspace_root};

#[derive(Clone)]
struct Loaded {
    workspace: Workspace,
    root: Option<WorkspaceRootInfo>,
    members: Vec<Project>,
}

#[component]
pub fn WorkspaceHome(
    #[prop(into)] workspace_id: Signal<String>,
    set_selected_project: WriteSignal<String>,
    set_selected_workspace: WriteSignal<String>,
) -> impl IntoView {
    let loaded = RwSignal::new(None::<Result<Loaded, String>>);

    // Reads the Workspace, its root and its members for the id that was
    // selected when the read started. A reply for an id that is no longer
    // selected is dropped, so a quick switch never shows the wrong Workspace.
    Effect::new(move |_| {
        let id = workspace_id.get();
        if id.is_empty() {
            return;
        }
        loaded.set(None);
        spawn_local(async move {
            let result = load(&id).await;
            if workspace_id.get_untracked() == id {
                loaded.set(Some(result));
            }
        });
    });

    view! {
        <div class="space-y-6" data-testid="workspace-home">
            {move || match loaded.get() {
                None => view! {
                    <p class="text-sm text-white/40">"Loading Workspace..."</p>
                }.into_any(),
                Some(Err(message)) => view! {
                    <div role="alert" data-testid="workspace-home-unavailable" class="space-y-3">
                        <h1 class="text-2xl font-bold text-white/90">"Workspace unavailable"</h1>
                        <p class="text-sm text-red-300">{message}</p>
                        <button
                            type="button"
                            class="px-3 py-2 rounded-lg bg-white/5 hover:bg-white/10 text-white/70 text-sm transition-colors"
                            on:click=move |_| set_selected_workspace.set(String::new())
                        >
                            "Back to Projects"
                        </button>
                    </div>
                }.into_any(),
                Some(Ok(data)) => {
                    let name = data.workspace.name.clone();
                    let root_view = match data.root {
                        Some(root) => {
                            let kind = if root.custom {
                                "Shared Agent Context Directory (custom)"
                            } else {
                                "Workspace directory (managed by SlashIt)"
                            };
                            let missing = (!root.exists).then(|| view! {
                                <span class="text-red-300">" Missing: agents of member Projects cannot start here until it exists."</span>
                            });
                            let shown = short_path(&root.path);
                            view! {
                                <p class="text-sm text-white/50" data-testid="workspace-home-root">
                                    {kind}" · "
                                    <span class="font-mono text-white/60" title=root.path data-testid="workspace-home-root-path">{shown}</span>
                                    {missing}
                                </p>
                            }.into_any()
                        }
                        None => view! {
                            <p class="text-sm text-white/40" data-testid="workspace-home-root">"Root directory unavailable."</p>
                        }.into_any(),
                    };
                    let member_count = data.members.len();
                    view! {
                        <div>
                            <p class="text-xs uppercase tracking-wide text-white/40">"Workspace"</p>
                            <h1 class="text-2xl font-bold text-white/90" data-testid="workspace-home-name">{name}</h1>
                            {root_view}
                        </div>

                        <section class="space-y-3" data-testid="workspace-home-members">
                            <h2 class="font-semibold text-white/80">
                                {format!("Projects ({member_count})")}
                            </h2>
                            {if data.members.is_empty() {
                                view! {
                                    <p class="text-sm text-white/40" data-testid="workspace-home-empty">
                                        "No active Projects are in this Workspace. Use Organize Projects in the Project Rail to add one."
                                    </p>
                                }.into_any()
                            } else {
                                view! {
                                    <ul class="space-y-2">
                                        {data.members.into_iter().map(|project| {
                                            let pid = project.id.to_string();
                                            let pname = project.name.clone();
                                            view! {
                                                <li>
                                                    <button
                                                        type="button"
                                                        data-testid="workspace-home-member"
                                                        class="w-full text-left px-4 py-3 rounded-lg border border-white/10 bg-white/[0.02] hover:bg-white/[0.05] text-white/80 transition-colors"
                                                        on:click=move |_| {
                                                            set_selected_project.set(pid.clone());
                                                            set_selected_workspace.set(String::new());
                                                        }
                                                    >
                                                        {pname}
                                                    </button>
                                                </li>
                                            }
                                        }).collect_view()}
                                    </ul>
                                }.into_any()
                            }}
                        </section>

                        <p class="text-xs text-white/30" data-testid="workspace-home-conversation">
                            "A Workspace Conversation is not available yet. Open a Project to work on its Tasks and Conversations."
                        </p>
                    }.into_any()
                }
            }}
        </div>
    }
}

async fn load(id: &str) -> Result<Loaded, String> {
    let workspace = get_workspace(id).await?;
    // The root is advisory: its failure must not hide a Workspace that exists.
    let root = get_workspace_root(id).await.ok();
    let members = list_projects()
        .await?
        .into_iter()
        .filter(|p| p.scope.workspace_id().map(|w| w.to_string()).as_deref() == Some(id))
        .collect();
    Ok(Loaded { workspace, root, members })
}

/// The last two segments of a long path, so the Workspace title stays the main
/// content. The full path is still shown in the element's tooltip.
fn short_path(path: &str) -> String {
    let segments: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if path.len() <= 48 || segments.len() <= 2 {
        return path.to_string();
    }
    format!("…/{}", segments[segments.len() - 2..].join("/"))
}

#[cfg(test)]
mod tests {
    use super::short_path;

    #[test]
    fn a_short_path_is_shown_whole() {
        assert_eq!(short_path("/home/me/ws"), "/home/me/ws");
    }

    #[test]
    fn a_long_path_keeps_its_last_two_segments() {
        let long = "/home/ruiandrada/.local/share/slashit/workspaces/field-workspace-root";
        assert_eq!(short_path(long), "…/workspaces/field-workspace-root");
    }
}
