use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::callback::Callback;
use crate::models::{Repository, AgentType, Project};
use crate::models::repository_setup::{initialization_for, InitPreview, Vcs, VcsInitKind};
use crate::services::{create_project, create_repository, list_repositories, pick_folder, check_is_git_repo};
use crate::services::repository_setup_service::{get_project_readiness, preview_vcs_initialization};
use crate::components::toast;

/// Tell the person when a project was created that cannot run tasks yet,
/// and where to set it up. A project is never initialized behind their back.
async fn announce_setup_needed(project: &Project) {
    if let Ok(readiness) = get_project_readiness(project.id.to_string()).await {
        if let Some(blocked) = readiness.blocked {
            toast::info(format!(
                "'{}' needs repository setup before tasks can run: {blocked}",
                project.name
            ));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WizardStep {
    Choice,      // Step 1: Choose between Open Folder or Create Project
    CreateForm,  // Step 2: Create project form
}

#[component]
pub fn CreateProjectModal(
    #[prop(into)] show: Signal<bool>,
    set_show: WriteSignal<bool>,
    #[prop(optional)] on_project_created: Option<Callback<Project>>,
) -> impl IntoView {
    let (wizard_step, set_wizard_step) = signal(WizardStep::Choice);
    let (name, set_name) = signal(String::new());
    let (folder_path, set_folder_path) = signal(String::new());
    let (is_git_repo, set_is_git_repo) = signal(false);
    let (ancestor_repo_root, set_ancestor_repo_root) = signal(Option::<String>::None);
    // Initializing version control is an explicit choice, shown with what it
    // will do (see `InitPreview`).
    let (init_vcs, set_init_vcs) = signal(false);
    let init_kind = RwSignal::new(VcsInitKind::Git);
    let init_preview = RwSignal::new(None::<InitPreview>);
    let (selected_repository_id, set_selected_repository_id) = signal(Option::<String>::None);
    let (repositories, set_repositories) = signal(Vec::<Repository>::new());
    let (loading, set_loading) = signal(false);
    let (submitting, set_submitting) = signal(false);
    let (error_msg, set_error_msg) = signal(Option::<String>::None);

    // Reset form when modal opens
    Effect::new(move |_| {
        if show.get() {
            let set_repositories = set_repositories;
            let set_loading = set_loading;
            spawn_local(async move {
                set_loading.set(true);
                match list_repositories().await {
                    Ok(repos) => set_repositories.set(repos),
                    Err(e) => leptos::logging::warn!("Failed to load repositories: {}", e),
                }
                set_loading.set(false);
            });
            // Reset to initial state
            set_wizard_step.set(WizardStep::Choice);
            set_name.set(String::new());
            set_folder_path.set(String::new());
            set_is_git_repo.set(false);
            set_ancestor_repo_root.set(None);
            set_init_vcs.set(false);
            init_kind.set(VcsInitKind::Git);
            init_preview.set(None);
            set_selected_repository_id.set(None);
            set_error_msg.set(None);
        }
    });

    // Handle "Open Folder" - directly open folder picker and create project
    let handle_open_folder = {
        move |_| {
            let on_created = on_project_created;
            let set_show_clone = set_show;
            spawn_local(async move {
                match pick_folder().await {
                    Ok(Some(path)) => {
                        // Auto-get project name from folder
                        let project_name = path
                            .split(['/', '\\'])
                            .next_back()
                            .unwrap_or("New Project")
                            .to_string();

                        // Create repository from folder. "Open Folder" has no
                        // init-git affordance — it always opens the folder as-is.
                        match create_repository(path, None, None).await {
                            Ok(repo) => {
                                // Create project with the new repository
                                match create_project(project_name.clone(), Some(repo.id.to_string()), AgentType::ClaudeCode).await {
                                    Ok(project) => {
                                        toast::success(format!("Project '{}' created successfully!", project_name));
                                        announce_setup_needed(&project).await;
                                        set_show_clone.set(false);
                                        if let Some(callback) = on_created {
                                            callback.run(project);
                                        }
                                    }
                                    Err(e) => {
                                        toast::error(format!("Failed to create project: {}", e));
                                        leptos::logging::error!("Failed to create project: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                toast::error(format!("Failed to create repository: {}", e));
                                leptos::logging::error!("Failed to create repository: {}", e);
                            }
                        }
                    }
                    Ok(None) => {
                        // User cancelled - no toast needed
                    }
                    Err(e) => {
                        toast::error(format!("Failed to open folder picker: {}", e));
                        leptos::logging::error!("Failed to open folder picker: {}", e);
                    }
                }
            });
        }
    };

    // What the picked (or typed) folder is, and what initializing version
    // control there would do. UX only; `create_repository` re-derives it.
    let inspect_folder = move |path: String| {
        spawn_local(async move {
            // Check if it's a git repo (or lives inside one) — this is
            // UX only; `create_repository` re-derives the same
            // classification authoritatively at submit time, since
            // this result can be stale by then.
            match check_is_git_repo(path.clone()).await {
                Ok(detection) => {
                    set_is_git_repo.set(detection.is_git_repo);
                    set_ancestor_repo_root.set(detection.ancestor_root);
                }
                Err(_) => {
                    set_is_git_repo.set(false);
                    set_ancestor_repo_root.set(None);
                }
            }
            // What initializing would do here. An empty folder (a
            // brand new project) starts with it chosen; a folder
            // that already holds files is only snapshotted when the
            // person ticks the box.
            match preview_vcs_initialization(path).await {
                Ok(preview) => {
                    let empty_new_folder = preview.vcs == Vcs::None && preview.files == 0;
                    set_init_vcs.set(
                        empty_new_folder && preview.refusal(VcsInitKind::Git).is_none(),
                    );
                    init_kind.set(VcsInitKind::Git);
                    init_preview.set(Some(preview));
                }
                Err(e) => {
                    set_init_vcs.set(false);
                    init_preview.set(None);
                    leptos::logging::warn!("Could not preview version control setup: {}", e);
                }
            }
        });
    };

    // Handle folder picker in create form
    let handle_folder_pick = move |_| {
        spawn_local(async move {
            match pick_folder().await {
                Ok(Some(path)) => {
                    set_folder_path.set(path.clone());
                    // Auto-populate name from folder
                    if name.get().is_empty() {
                        if let Some(folder_name) = path.split(['/', '\\']).next_back() {
                            set_name.set(folder_name.to_string());
                        }
                    }
                    inspect_folder(path);
                }
                Ok(None) => {} // User cancelled
                Err(e) => {
                    set_error_msg.set(Some(format!("Failed to open folder picker: {}", e)));
                }
            }
        });
    };

    // Submit create project form
    let submit = {
        move |_| {
            let name_val = name.get();
            if name_val.trim().is_empty() {
                set_error_msg.set(Some("Project name is required".to_string()));
                return;
            }

            let folder_path_val = folder_path.get();
            if folder_path_val.trim().is_empty() {
                set_error_msg.set(Some("Please select a folder location".to_string()));
                return;
            }

            set_submitting.set(true);
            set_error_msg.set(None);

            let on_created = on_project_created;
            let set_show_clone = set_show;
            let initialize = match initialization_for(
                init_preview.get().as_ref(),
                &folder_path_val,
                init_vcs.get(),
                init_kind.get(),
            ) {
                Ok(initialize) => initialize,
                Err(e) => {
                    set_error_msg.set(Some(e));
                    set_submitting.set(false);
                    return;
                }
            };

            spawn_local(async move {
                // Create repository from folder path
                let repository_id = match create_repository(folder_path_val, None, initialize).await {
                    Ok(repo) => Some(repo.id.to_string()),
                    Err(e) => {
                        set_error_msg.set(Some(format!("Failed to create repository: {}", e)));
                        set_submitting.set(false);
                        return;
                    }
                };

                // Create the project
                match create_project(name_val.clone(), repository_id, AgentType::ClaudeCode).await {
                    Ok(project) => {
                        toast::success(format!("Project '{}' created successfully!", name_val));
                        announce_setup_needed(&project).await;
                        set_submitting.set(false);
                        set_show_clone.set(false);
                        if let Some(callback) = on_created {
                            callback.run(project);
                        }
                    }
                    Err(e) => {
                        toast::error(format!("Failed to create project: {}", e));
                        set_error_msg.set(Some(format!("Failed to create project: {}", e)));
                        set_submitting.set(false);
                    }
                }
            });
        }
    };

    view! {
        <Show when=move || show.get()>
            // "The wizard is open" has no other stable selector: the dialog is
            // rendered only while `show` is true, so acceptance tests need a
            // handle on its root to tell an opened wizard from an ignored click.
            <div data-testid="create-project-modal" class="fixed inset-0 z-50 flex items-center justify-center p-4">
                <div
                    class="absolute inset-0 bg-black/70 backdrop-blur-sm"
                    on:click=move |_| set_show.set(false)
                    aria-hidden="true"
                ></div>

                <div class="relative w-full max-w-xl bg-[#0B0B0F] border border-white/10 rounded-2xl shadow-2xl overflow-hidden">
                    // Step 1: Choice Screen
                    <Show when=move || wizard_step.get() == WizardStep::Choice>
                        <div class="p-8">
                            // Header
                            <div class="text-center mb-8">
                                <div class="w-16 h-16 mx-auto mb-4 rounded-2xl bg-gradient-to-br from-blue-500 to-purple-600 flex items-center justify-center shadow-lg shadow-blue-500/25">
                                    <svg class="w-8 h-8 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 11H5m14 0a2 2 0 012 2v6a2 2 0 01-2 2H5a2 2 0 01-2-2v-6a2 2 0 012-2m14 0V9a2 2 0 00-2-2M5 11V9a2 2 0 012-2m0 0V5a2 2 0 012-2h6a2 2 0 012 2v2M7 7h10" />
                                    </svg>
                                </div>
                                <h2 class="text-2xl font-bold text-white mb-2">"Get Started"</h2>
                                <p class="text-white/50 text-sm">"Open an existing folder or create a new project"</p>
                            </div>

                            // Choice Cards
                            <div class="grid grid-cols-2 gap-4 mb-6">
                                // Open Folder Card
                                <button
                                    type="button"
                                    on:click=handle_open_folder
                                    class="group p-6 rounded-xl bg-white/5 border border-white/10 hover:border-blue-500/50 hover:bg-blue-500/10 transition-all duration-300 text-left"
                                >
                                    <div class="w-12 h-12 rounded-xl bg-blue-500/20 group-hover:bg-blue-500/30 flex items-center justify-center mb-4 transition-colors">
                                        <svg class="w-6 h-6 text-blue-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                                        </svg>
                                    </div>
                                    <h3 class="text-lg font-semibold text-white mb-1 group-hover:text-blue-300 transition-colors">"Open Folder"</h3>
                                    <p class="text-sm text-white/40 group-hover:text-white/60 transition-colors">"Select an existing project folder to open"</p>
                                </button>

                                // Create Project Card
                                <button
                                    type="button"
                                    data-testid="wizard-create-project"
                                    on:click=move |_| set_wizard_step.set(WizardStep::CreateForm)
                                    class="group p-6 rounded-xl bg-white/5 border border-white/10 hover:border-purple-500/50 hover:bg-purple-500/10 transition-all duration-300 text-left"
                                >
                                    <div class="w-12 h-12 rounded-xl bg-purple-500/20 group-hover:bg-purple-500/30 flex items-center justify-center mb-4 transition-colors">
                                        <svg class="w-6 h-6 text-purple-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6m0 0v6m0-6h6m-6 0H6" />
                                        </svg>
                                    </div>
                                    <h3 class="text-lg font-semibold text-white mb-1 group-hover:text-purple-300 transition-colors">"Create Project"</h3>
                                    <p class="text-sm text-white/40 group-hover:text-white/60 transition-colors">"Set up a new project with custom settings"</p>
                                </button>
                            </div>

                            // Recent Projects Section (optional)
                            {move || if !repositories.get().is_empty() {
                                view! {
                                    <div class="border-t border-white/5 pt-6">
                                        <h4 class="text-xs font-medium text-white/40 uppercase tracking-wider mb-3">"Recent Repositories"</h4>
                                        <div class="space-y-2 max-h-32 overflow-y-auto">
                                            {repositories.get().into_iter().take(3).map(|repo| {
                                                let repo_path = repo.local_path.clone();
                                                let display_name = repo.local_path
                                                    .split(['/', '\\'])
                                                    .next_back()
                                                    .unwrap_or(&repo.local_path)
                                                    .to_string();
                                                let on_created = on_project_created;
                                                let set_show_clone = set_show;
                                                let repo_id = repo.id.to_string();
                                                view! {
                                                    <button
                                                        type="button"
                                                        on:click=move |_| {
                                                            let repo_id_clone = repo_id.clone();
                                                            let display_name_clone = display_name.clone();
                                                            let on_created = on_created;
                                                            let set_show_clone = set_show_clone;
                                                            spawn_local(async move {
                                                                let name_for_toast = display_name_clone.clone();
                                                                match create_project(display_name_clone, Some(repo_id_clone), AgentType::ClaudeCode).await {
                                                                    Ok(project) => {
                                                                        toast::success(format!("Project '{}' opened!", name_for_toast));
                                                                        set_show_clone.set(false);
                                                                        if let Some(callback) = on_created {
                                                                            callback.run(project);
                                                                        }
                                                                    }
                                                                    Err(e) => {
                                                                        toast::error(format!("Failed to create project: {}", e));
                                                                        leptos::logging::error!("Failed to create project: {}", e);
                                                                    }
                                                                }
                                                            });
                                                        }
                                                        class="w-full flex items-center gap-3 p-3 rounded-lg bg-white/5 hover:bg-white/10 border border-transparent hover:border-white/10 transition-all text-left group"
                                                    >
                                                        <div class="w-8 h-8 rounded-lg bg-white/5 flex items-center justify-center">
                                                            <svg class="w-4 h-4 text-white/40" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                                                            </svg>
                                                        </div>
                                                        <div class="flex-1 min-w-0">
                                                            <div class="text-sm font-medium text-white/80 truncate">{display_name.clone()}</div>
                                                            <div class="text-xs text-white/30 truncate">{repo_path}</div>
                                                        </div>
                                                        <svg class="w-4 h-4 text-white/20 group-hover:text-white/40 transition-colors" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5l7 7-7 7" />
                                                        </svg>
                                                    </button>
                                                }
                                            }).collect::<Vec<_>>()}
                                        </div>
                                    </div>
                                }.into_any()
                            } else {
                                view! { <div></div> }.into_any()
                            }}

                            // Close button
                            <button
                                type="button"
                                on:click=move |_| set_show.set(false)
                                class="absolute top-4 right-4 p-2 rounded-lg hover:bg-white/5 text-white/40 hover:text-white/60 transition-colors"
                                aria-label="Close modal"
                            >
                                <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                                </svg>
                            </button>
                        </div>
                    </Show>

                    // Step 2: Create Project Form
                    <Show when=move || wizard_step.get() == WizardStep::CreateForm>
                        // Header with back button
                        <div class="flex items-center justify-between p-6 border-b border-white/5">
                            <div class="flex items-center gap-3">
                                <button
                                    type="button"
                                    on:click=move |_| set_wizard_step.set(WizardStep::Choice)
                                    class="p-2 rounded-lg hover:bg-white/5 text-white/40 hover:text-white/60 transition-colors"
                                    aria-label="Go back"
                                >
                                    <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 19l-7-7 7-7" />
                                    </svg>
                                </button>
                                <div class="w-10 h-10 rounded-lg bg-gradient-to-br from-purple-500 to-pink-500 flex items-center justify-center">
                                    <svg class="w-5 h-5 text-white" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6m0 0v6m0-6h6m-6 0H6" />
                                    </svg>
                                </div>
                                <div>
                                    <h2 class="text-lg font-semibold text-white/90">"Create New Project"</h2>
                                    <p class="text-xs text-white/40">"Configure your project settings"</p>
                                </div>
                            </div>
                            <button
                                on:click=move |_| set_show.set(false)
                                class="p-2 rounded-lg hover:bg-white/5 text-white/40 hover:text-white/60 transition-colors"
                                type="button"
                                aria-label="Close modal"
                            >
                                <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                                </svg>
                            </button>
                        </div>

                        <div class="p-6 space-y-5">
                            {move || error_msg.get().map(|err| view! {
                                <div class="p-3 rounded-lg bg-red-500/10 border border-red-500/30 text-red-400 text-sm flex items-center gap-2">
                                    <svg class="w-4 h-4 flex-shrink-0" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
                                    </svg>
                                    {err}
                                </div>
                            })}

                            // Project Name
                            <div>
                                <label class="block text-sm font-medium text-white/70 mb-1.5">"Project Name"</label>
                                <input
                                    type="text"
                                    prop:value=move || name.get()
                                    on:input=move |ev| {
                                        set_name.set(event_target_value(&ev));
                                        set_error_msg.set(None);
                                    }
                                    class="w-full px-4 py-3 rounded-xl bg-white/5 border border-white/10 text-white placeholder-white/30 focus:outline-none focus:ring-2 focus:ring-purple-500/50 focus:border-purple-500/50 transition-all"
                                    placeholder="My Awesome Project"
                                    data-testid="create-project-name"
                                    disabled=move || submitting.get()
                                />
                            </div>

                            // Location (Folder Picker)
                            <div>
                                <label class="block text-sm font-medium text-white/70 mb-1.5">"Location"</label>
                                <div class="flex gap-2">
                                    <input
                                        type="text"
                                        prop:value=move || folder_path.get()
                                        data-testid="create-project-location"
                                        on:input=move |ev| {
                                            // What was shown was for another folder.
                                            set_folder_path.set(event_target_value(&ev));
                                            init_preview.set(None);
                                            set_init_vcs.set(false);
                                        }
                                        on:change=move |ev| inspect_folder(event_target_value(&ev))
                                        class="flex-1 px-4 py-3 rounded-xl bg-white/5 border border-white/10 text-white placeholder-white/30 focus:outline-none focus:ring-2 focus:ring-purple-500/50 transition-all"
                                        placeholder="Select a folder..."
                                        disabled=move || submitting.get()
                                    />
                                    <button
                                        type="button"
                                        on:click=handle_folder_pick
                                        class="px-4 py-3 rounded-xl bg-white/5 border border-white/10 text-white/70 hover:bg-white/10 hover:text-white transition-all flex items-center gap-2"
                                        disabled=move || submitting.get()
                                    >
                                        <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                                        </svg>
                                        "Browse"
                                    </button>
                                </div>
                                {move || if let Some(root) = ancestor_repo_root.get() {
                                    view! {
                                        <div class="mt-2 flex items-center gap-2 text-xs text-amber-400">
                                            <svg class="w-3.5 h-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 9v2m0 4h.01M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z" />
                                            </svg>
                                            {format!("This folder is inside an existing repository at '{}'. Select that folder instead.", root)}
                                        </div>
                                    }.into_any()
                                } else if is_git_repo.get() {
                                    view! {
                                        <div class="mt-2 flex items-center gap-2 text-xs text-green-400">
                                            <svg class="w-3.5 h-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                            </svg>
                                            "Git repository detected"
                                        </div>
                                    }.into_any()
                                } else {
                                    ().into_any()
                                }}
                            </div>

                            // Version control setup: what it would do, and an
                            // explicit choice to do it.
                            {move || init_preview.get().filter(|p| {
                                // Offered, or refused for a reason worth
                                // saying: not for a repository that is fine
                                // as it is, nor one inside another (said above).
                                p.action.is_some()
                                    || !matches!(p.vcs, Vcs::Git | Vcs::JjColocated | Vcs::InsideRepository { .. })
                            }).map(|preview| {
                                let action = preview.action.clone().unwrap_or_default();
                                let sample = preview.sample.clone();
                                let more = preview.files.saturating_sub(sample.len());
                                let offers_jj = preview.vcs == Vcs::None && preview.jj_available;
                                let refusal_preview = preview.clone();
                                let refusal = Signal::derive(move || refusal_preview.refusal(init_kind.get()));
                                view! {
                                    <div class="p-4 rounded-xl bg-white/5 border border-white/10 space-y-3" data-testid="vcs-setup">
                                        <label class="flex items-start gap-3 cursor-pointer">
                                            <input
                                                type="checkbox"
                                                data-testid="vcs-init-toggle"
                                                prop:checked=move || init_vcs.get() && refusal.get().is_none()
                                                on:change=move |ev| set_init_vcs.set(event_target_checked(&ev))
                                                class="mt-0.5 w-5 h-5 rounded-md bg-white/10 border-white/20 text-purple-500 focus:ring-purple-500/50 focus:ring-offset-0"
                                                disabled=move || submitting.get() || refusal.get().is_some()
                                            />
                                            <div>
                                                <div class="text-sm font-medium text-white/80">"Initialize version control"</div>
                                                <div class="text-xs text-white/50" data-testid="vcs-init-action">{action}</div>
                                            </div>
                                        </label>
                                        {(!sample.is_empty()).then(|| view! {
                                            <details class="text-xs text-white/40">
                                                <summary class="cursor-pointer">"Files the first commit would hold"</summary>
                                                <ul class="mt-1 ml-4 list-disc font-mono">
                                                    {sample.into_iter().map(|f| view! { <li>{f}</li> }).collect::<Vec<_>>()}
                                                </ul>
                                                {(more > 0).then(|| view! { <p class="mt-1">{format!("and {more} more")}</p> })}
                                            </details>
                                        })}
                                        {offers_jj.then(|| view! {
                                            <div class="flex gap-4 text-sm text-white/70">
                                                <label class="flex items-center gap-2 cursor-pointer">
                                                    <input type="radio" name="vcs-kind" data-testid="vcs-kind-git"
                                                        prop:checked=move || init_kind.get() == VcsInitKind::Git
                                                        on:change=move |_| init_kind.set(VcsInitKind::Git) />
                                                    "Git"
                                                </label>
                                                <label class="flex items-center gap-2 cursor-pointer">
                                                    <input type="radio" name="vcs-kind" data-testid="vcs-kind-jujutsu"
                                                        prop:checked=move || init_kind.get() == VcsInitKind::Jujutsu
                                                        on:change=move |_| init_kind.set(VcsInitKind::Jujutsu) />
                                                    "Jujutsu (colocated with Git)"
                                                </label>
                                            </div>
                                        })}
                                        {move || refusal.get().map(|why| view! {
                                            <p class="text-xs text-amber-400" data-testid="vcs-init-blocked">{why}</p>
                                        })}
                                        <Show when=move || !init_vcs.get() || refusal.get().is_some()>
                                            <p class="text-xs text-white/40" data-testid="vcs-init-skipped">
                                                "Without it the project is created in a setup-required state: tasks cannot run until version control is initialized in Settings > Repository. A remote is never required."
                                            </p>
                                        </Show>
                                    </div>
                                }
                            })}
                            // AI Agent (hardcoded to Claude Code)
                            <div class="flex items-center gap-3 p-4 rounded-xl bg-blue-500/10 border border-blue-500/30">
                                <svg class="w-5 h-5 text-blue-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9.75 17L9 20l-1 1h8l-1-1-.75-3M3 13h18M5 17h14a2 2 0 002-2V5a2 2 0 00-2-2H5a2 2 0 00-2 2v10a2 2 0 002 2z" />
                                </svg>
                                <div>
                                    <span class="text-sm font-medium text-blue-300">"Agent: Claude Code"</span>
                                </div>
                            </div>
                        </div>

                        // Footer with actions
                        <div class="flex items-center justify-end gap-3 p-6 border-t border-white/5">
                            <button
                                on:click=move |_| set_wizard_step.set(WizardStep::Choice)
                                class="px-5 py-2.5 rounded-xl text-white/70 hover:text-white/90 hover:bg-white/5 transition-all"
                                disabled=move || submitting.get()
                                type="button"
                            >
                                "Back"
                            </button>
                            <button
                                data-testid="create-project-submit"
                                on:click=submit
                                class="flex items-center gap-2 px-6 py-2.5 rounded-xl bg-gradient-to-r from-purple-500 to-pink-500 hover:from-purple-600 hover:to-pink-600 disabled:from-white/5 disabled:to-white/5 disabled:text-white/30 text-white font-medium transition-all shadow-lg shadow-purple-500/20"
                                disabled=move || submitting.get()
                                type="button"
                            >
                                {move || if submitting.get() {
                                    view! {
                                        <svg class="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
                                        </svg>
                                        <span>"Creating..."</span>
                                    }.into_any()
                                } else {
                                    view! {
                                        <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 6v6m0 0v6m0-6h6m-6 0H6" />
                                        </svg>
                                        <span>"Create Project"</span>
                                    }.into_any()
                                }}
                            </button>
                        </div>
                    </Show>
                </div>
            </div>
        </Show>
    }
}
