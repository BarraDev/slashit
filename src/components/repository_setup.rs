//! Repository settings for one project: whether it can run tasks, where
//! new tasks start, what is known about its remote, and the explicit
//! actions that fix each.
//!
//! Every action here is a button a person presses; nothing runs because
//! the panel was opened. Each action's own answer replaces what is shown.

use crate::components::toast;
use crate::models::repository_setup::{
    BaseSource, Head, InitPreview, Readiness, RemoteState, Vcs, VcsInitKind,
};
use crate::services::repository_setup_service::{
    detect_remote_default_branch, get_project_readiness, initialize_project_vcs,
    preview_vcs_initialization, set_project_base,
};
use leptos::prelude::*;
use leptos::task::spawn_local;

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

#[component]
pub fn RepositorySetup(project_id: String) -> impl IntoView {
    let readiness = RwSignal::new(None::<Readiness>);
    let preview = RwSignal::new(None::<InitPreview>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let chosen = RwSignal::new(String::new());
    let pid = StoredValue::new(project_id);

    // Show `answer` as the new state, or its failure in place.
    let apply = move |answer: Result<Readiness, String>| {
        match answer {
            Ok(report) => {
                error.set(None);
                let needs_init =
                    report.vcs == Vcs::None || (report.vcs == Vcs::Git && !report.has_commits);
                let path = report.path.clone();
                readiness.set(Some(report));
                preview.set(None);
                if needs_init {
                    spawn_local(async move {
                        match preview_vcs_initialization(path).await {
                            Ok(p) => preview.set(Some(p)),
                            Err(e) => error.set(Some(e)),
                        }
                    });
                }
            }
            Err(e) => error.set(Some(e)),
        }
        busy.set(false);
    };

    let refresh = move || {
        let id = pid.get_value();
        if id.is_empty() {
            return;
        }
        spawn_local(async move { apply(get_project_readiness(id).await) });
    };
    refresh();

    let choose_base = move |branch: String| {
        busy.set(true);
        let id = pid.get_value();
        spawn_local(async move {
            let answer = set_project_base(id, branch.clone()).await;
            if answer.is_ok() {
                toast::success(format!("New tasks will start from {branch} when origin does not name a default branch."));
            }
            apply(answer);
        });
    };
    let detect = move |_| {
        busy.set(true);
        let id = pid.get_value();
        spawn_local(async move { apply(detect_remote_default_branch(id).await) });
    };
    let initialize = move |kind: VcsInitKind| {
        // Only what the preview on screen showed is initialized; the backend
        // refuses when the folder no longer holds that many files.
        let Some(shown) = preview.get_untracked().map(|p| p.files) else {
            return;
        };
        busy.set(true);
        let id = pid.get_value();
        spawn_local(async move {
            let answer = initialize_project_vcs(id, kind, shown).await;
            if answer.is_ok() {
                toast::success("Version control initialized; tasks can start.".to_string());
            }
            apply(answer);
        });
    };

    view! {
        <div class="space-y-6" data-testid="repository-setup">
            <h2 class="text-lg font-semibold text-white/90">"Repository"</h2>
            {move || pid.get_value().is_empty().then(|| view! {
                <p class="text-sm text-white/50">"Select a project to see its repository setup."</p>
            })}
            {move || error.get().map(|e| view! {
                <div class="p-3 rounded-lg bg-red-500/10 border border-red-500/30 text-red-400 text-sm" data-testid="repository-setup-error">{e}</div>
            })}
            {move || readiness.get().map(|r| {
                let ready = r.base.clone();
                let blocked = r.blocked.clone();
                view! {
                    <div class="space-y-4">
                        <div class="p-4 rounded-xl bg-white/5 border border-white/10">
                            <p class="text-sm text-white/40">"Folder"</p>
                            <p class="text-sm text-white/80 font-mono break-all">{r.path.clone()}</p>
                            <p class="text-sm text-white/60 mt-2" data-testid="repository-vcs">{r.vcs.label()}</p>
                            {match &r.head {
                                Head::Detached => Some(view! { <p class="text-xs text-white/40 mt-1">"Git HEAD is detached (normal in a colocated Jujutsu repository)."</p> }.into_any()),
                                Head::Branch { branch } => Some(view! { <p class="text-xs text-white/40 mt-1">{format!("The primary checkout is on {branch}.")}</p> }.into_any()),
                                _ => None,
                            }}
                        </div>

                        // Local execution.
                        <div class="p-4 rounded-xl bg-white/5 border border-white/10 space-y-2" data-testid="repository-tasks">
                            <p class="font-medium text-white/90">"Running tasks"</p>
                            {match (ready, blocked) {
                                (Some(base), _) => view! {
                                    <p class="text-sm text-green-400" data-testid="repository-ready">
                                        {match base.source {
                                            BaseSource::Remote => format!("Ready. New tasks start from origin's default branch {} ({}).", base.branch, short(&base.commit)),
                                            BaseSource::Local => format!("Ready. New tasks start from the local branch {} ({}).", base.branch, short(&base.commit)),
                                        }}
                                    </p>
                                }.into_any(),
                                (None, Some(why)) => view! {
                                    <p class="text-sm text-amber-400" data-testid="repository-blocked">{why}</p>
                                }.into_any(),
                                (None, None) => ().into_any(),
                            }}
                        </div>

                        // The project's local base branch.
                        {(r.vcs == Vcs::Git || r.vcs == Vcs::JjColocated).then(|| {
                            let branches = r.local_branches.clone();
                            let current = r.project_base.clone();
                            let proposal = r.proposal.clone();
                            let reason = r.proposal_reason.clone();
                            view! {
                                <div class="p-4 rounded-xl bg-white/5 border border-white/10 space-y-3" data-testid="repository-base">
                                    <p class="font-medium text-white/90">"Base branch"</p>
                                    <p class="text-xs text-white/40">"Where new tasks start when origin does not name a default branch. Switching the branch you have checked out does not change it."</p>
                                    <p class="text-sm text-white/70" data-testid="repository-base-current">
                                        {current.clone().map(|b| format!("Base branch: {b}")).unwrap_or_else(|| "No base branch chosen.".to_string())}
                                    </p>
                                    {proposal.map(|branch| {
                                        let label = format!("Use {branch} as base branch");
                                        view! {
                                            <button type="button" data-testid="repository-base-use-proposal"
                                                class="px-3 py-1.5 rounded-lg text-sm bg-blue-500/20 text-blue-300 hover:bg-blue-500/30"
                                                disabled=move || busy.get()
                                                on:click=move |_| choose_base(branch.clone())>{label}</button>
                                        }
                                    })}
                                    {reason.map(|why| view! { <p class="text-xs text-white/50">{why}</p> })}
                                    {(!branches.is_empty()).then(|| view! {
                                        <div class="flex gap-2 items-center">
                                            <select data-testid="repository-base-select"
                                                class="px-3 py-1.5 rounded-lg bg-white/5 border border-white/10 text-sm text-white/80"
                                                on:change=move |ev| chosen.set(event_target_value(&ev))>
                                                <option value="">"Choose a branch"</option>
                                                {branches.into_iter().map(|b| view! { <option value=b.clone()>{b.clone()}</option> }).collect::<Vec<_>>()}
                                            </select>
                                            <button type="button" data-testid="repository-base-set"
                                                class="px-3 py-1.5 rounded-lg text-sm bg-white/10 text-white/80 hover:bg-white/20"
                                                disabled=move || busy.get() || chosen.get().is_empty()
                                                on:click=move |_| choose_base(chosen.get())>"Set as base branch"</button>
                                        </div>
                                    })}
                                </div>
                            }
                        })}

                        // Remote delivery: optional.
                        {(r.vcs == Vcs::Git || r.vcs == Vcs::JjColocated).then(|| view! {
                            <div class="p-4 rounded-xl bg-white/5 border border-white/10 space-y-2" data-testid="repository-remote">
                                <p class="font-medium text-white/90">"Remote"</p>
                                {match r.remote.clone() {
                                    RemoteState::NoOrigin => view! {
                                        <p class="text-sm text-white/60" data-testid="repository-remote-none">
                                            "No remote named origin. Tasks run, commit and are reviewed locally; pull requests become available once a GitHub repository is added as origin."
                                        </p>
                                    }.into_any(),
                                    RemoteState::DefaultBranch { branch } => view! {
                                        <p class="text-sm text-white/60">{format!("origin's default branch: {branch}")}</p>
                                    }.into_any(),
                                    RemoteState::DefaultBranchUnknown => view! {
                                        <p class="text-sm text-white/60">"origin's default branch is not recorded in this repository. Tasks can still start from the base branch."</p>
                                    }.into_any(),
                                    RemoteState::DefaultBranchUnusable { reason } => view! {
                                        <p class="text-sm text-amber-400">{reason}</p>
                                    }.into_any(),
                                }}
                                {(r.remote != RemoteState::NoOrigin).then(|| view! {
                                    <div>
                                        <button type="button" data-testid="repository-detect-default-branch"
                                            class="px-3 py-1.5 rounded-lg text-sm bg-white/10 text-white/80 hover:bg-white/20"
                                            disabled=move || busy.get()
                                            on:click=detect>"Detect default branch"</button>
                                        <p class="text-xs text-white/40 mt-1">"Asks origin which branch is its default and records it as origin/HEAD in this repository. Nothing is fetched or pushed, and origin is not changed."</p>
                                    </div>
                                })}
                            </div>
                        })}

                        // Initialization.
                        {move || preview.get().map(|p| {
                            let action = p.action.clone();
                            let git_refusal = p.refusal(VcsInitKind::Git);
                            let jj_refusal = p.refusal(VcsInitKind::Jujutsu);
                            let offers_jj = p.vcs == Vcs::None && p.jj_available;
                            let git_disabled = git_refusal.is_some();
                            // Said once when it is the same reason for both.
                            let jj_reason = jj_refusal.clone().filter(|why| offers_jj && Some(why) != git_refusal.as_ref());
                            view! {
                                <div class="p-4 rounded-xl bg-white/5 border border-white/10 space-y-3" data-testid="repository-initialize">
                                    <p class="font-medium text-white/90">"Initialize version control"</p>
                                    {action.map(|a| view! { <p class="text-sm text-white/60" data-testid="repository-initialize-action">{a}</p> })}
                                    <div class="flex gap-2">
                                        <button type="button" data-testid="repository-initialize-git"
                                            class="px-3 py-1.5 rounded-lg text-sm bg-purple-500/20 text-purple-300 hover:bg-purple-500/30"
                                            disabled=move || busy.get() || git_disabled
                                            on:click=move |_| initialize(VcsInitKind::Git)>"Initialize with Git"</button>
                                        {offers_jj.then(|| {
                                            let disabled = jj_refusal.is_some();
                                            view! {
                                                <button type="button" data-testid="repository-initialize-jujutsu"
                                                    class="px-3 py-1.5 rounded-lg text-sm bg-purple-500/20 text-purple-300 hover:bg-purple-500/30"
                                                    disabled=move || busy.get() || disabled
                                                    on:click=move |_| initialize(VcsInitKind::Jujutsu)>"Initialize with Jujutsu"</button>
                                            }
                                        })}
                                    </div>
                                    {git_refusal.map(|why| view! { <p class="text-xs text-amber-400" data-testid="repository-initialize-git-blocked">{why}</p> })}
                                    {jj_reason.map(|why| view! { <p class="text-xs text-amber-400" data-testid="repository-initialize-jujutsu-blocked">{format!("Jujutsu: {why}")}</p> })}
                                </div>
                            }
                        })}
                    </div>
                }
            })}
        </div>
    }
}
