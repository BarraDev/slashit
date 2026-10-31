// SPDX-License-Identifier: MPL-2.0
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::callback::Callback;
use crate::components::task_dependency::{self, choice_for, requested, DependencyChoice, DependencyField};
use crate::models::{Task, TaskCategory, TaskPriority};
use crate::models::task::{CodingProvider, ReviewMode};
use crate::services::{create_task, update_task, delete_task, set_task_dependencies, agent_service::list_available_models, agent_service::ModelInfo};
use uuid::Uuid;

#[derive(Clone, PartialEq)]
pub enum TaskEditMode {
    Create,
    Edit(Box<Task>),
}

/// Whether `model` is an OpenCode model id, `provider/model`: both parts are
/// present, and the id has no whitespace or control characters. The backend
/// applies the same rule before it starts a run.
fn is_opencode_model_id(model: &str) -> bool {
    let Some((provider, name)) = model.split_once('/') else {
        return false;
    };
    !provider.is_empty()
        && !name.is_empty()
        && model.len() <= 200
        && model.chars().all(|c| !c.is_whitespace() && !c.is_control())
}

#[component]
pub fn TaskEditModal(
    #[prop(into)] show: Signal<bool>,
    set_show: WriteSignal<bool>,
    #[prop(into)] mode: Signal<TaskEditMode>,
    /// Every Task on the board. The form offers the ones of its own Project.
    #[prop(into)] tasks: Signal<Vec<Task>>,
    project_id: String,
    #[prop(into)] on_save: Callback<Task>,
    #[prop(into)] on_delete: Callback<Uuid>,
) -> impl IntoView {
    let (title, set_title) = signal(String::new());
    let (description, set_description) = signal(String::new());
    let (category, set_category) = signal(TaskCategory::Feature);
    let (priority, set_priority) = signal(TaskPriority::Medium);
    let (model, set_model) = signal("default".to_string());
    let (provider, set_provider) = signal(CodingProvider::Claude);
    let (review_mode, set_review_mode) = signal(ReviewMode::Automatic);
    // The AI Reviewer model as a text value; empty means no reviewer is chosen.
    let (reviewer_model, set_reviewer_model) = signal(String::new());
    let (planning_mode, set_planning_mode) = signal(false);

    // The Depends on choice: at most one Task of this Project. The form keeps
    // the selection, and the save sends it only when it changed.
    let dependency = RwSignal::new(None::<Uuid>);
    let project_uuid = Uuid::parse_str(&project_id).ok();
    let dependency_options = Signal::derive(move || {
        let subject = match mode.get() {
            TaskEditMode::Create => None,
            TaskEditMode::Edit(task) => Some(task.id),
        };
        match project_uuid {
            Some(project) => task_dependency::candidates(&tasks.get(), project, subject),
            None => Vec::new(),
        }
    });
    let dependency_choice = Signal::derive(move || match mode.get() {
        TaskEditMode::Create => choice_for(None),
        TaskEditMode::Edit(task) => choice_for(Some(&*task)),
    });

    let (available_models, set_available_models) = signal(Vec::<ModelInfo>::new());

    // Load available models on mount
    Effect::new(move |prev: Option<bool>| {
        if prev.is_some() { return true; }
        spawn_local(async move {
            if let Ok(models) = list_available_models().await {
                set_available_models.set(models);
            }
        });
        true
    });

    let (submitting, set_submitting) = signal(false);
    let (deleting, set_deleting) = signal(false);
    let (error_msg, set_error_msg) = signal(None::<String>);
    let (confirm_delete, set_confirm_delete) = signal(false);
    
    // Validation state - track if fields have been touched
    let (title_touched, set_title_touched) = signal(false);
    
    // Validation helpers
    let title_error = move || {
        if title_touched.get() && title.get().trim().is_empty() {
            Some("Title is required".to_string())
        } else {
            None
        }
    };
    
    let is_form_valid = move || {
        !title.get().trim().is_empty()
    };

    // Reset form when modal opens or mode changes
    Effect::new(move |_| {
        if show.get() {
            match mode.get() {
                TaskEditMode::Create => {
                    set_title.set(String::new());
                    set_description.set(String::new());
                    set_category.set(TaskCategory::Feature);
                    set_priority.set(TaskPriority::Medium);
                    set_model.set("default".to_string());
                    set_provider.set(CodingProvider::Claude);
                    set_review_mode.set(ReviewMode::Automatic);
                    set_reviewer_model.set(String::new());
                    set_planning_mode.set(false);
                    set_title_touched.set(false);
                    dependency.set(None);
                }
                TaskEditMode::Edit(task) => {
                    set_title.set(task.title.clone());
                    set_description.set(task.description.clone().unwrap_or_default());
                    set_category.set(task.category.clone());
                    set_priority.set(task.priority.clone());
                    set_model.set(task.model.clone());
                    set_provider.set(task.provider);
                    set_review_mode.set(task.review_mode);
                    set_reviewer_model.set(task.reviewer_model.clone().unwrap_or_default());
                    set_planning_mode.set(task.planning_mode);
                    set_title_touched.set(true); // Already has content
                    dependency.set(task.dependencies.first().copied());
                }
            }
            set_error_msg.set(None);
            set_confirm_delete.set(false);
        }
    });

    let is_edit_mode = move || matches!(mode.get(), TaskEditMode::Edit(_));
    let get_task_id = move || {
        if let TaskEditMode::Edit(task) = mode.get() {
            Some(task.id)
        } else {
            None
        }
    };

    // Store project_id in a signal so it can be accessed multiple times in closures
    let (project_id_signal, _) = signal(project_id.clone());

    let on_delete_handler = on_delete;
    let handle_delete = move |_| {
        if !confirm_delete.get() {
            set_confirm_delete.set(true);
            return;
        }

        if let Some(task_id) = get_task_id() {
            set_deleting.set(true);
            let on_delete = on_delete_handler;
            let set_show = set_show;
            let set_deleting = set_deleting;
            let set_error_msg = set_error_msg;

            spawn_local(async move {
                match delete_task(task_id.to_string()).await {
                    Ok(_) => {
                        on_delete.run(task_id);
                        set_deleting.set(false);
                        set_show.set(false);
                    }
                    Err(e) => {
                        set_error_msg.set(Some(e));
                        set_deleting.set(false);
                    }
                }
            });
        }
    };

    view! {
        <Show when=move || show.get()>
            <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
                <div
                    class="absolute inset-0 bg-black/60 backdrop-blur-sm"
                    on:click=move |_| set_show.set(false)
                ></div>

                <div class="relative w-full max-w-2xl max-h-[90vh] overflow-y-auto bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl">
                    <div class="sticky top-0 z-10 flex items-center justify-between p-6 border-b border-white/5 bg-[#0B0B0F]">
                        <h2 class="text-lg font-semibold text-white/90">
                            {move || if is_edit_mode() { "Edit Task" } else { "Create New Task" }}
                        </h2>
                        <button
                            on:click=move |_| set_show.set(false)
                            class="p-2 rounded-lg hover:bg-white/5 text-white/40 hover:text-white/60 transition-colors"
                        >
                            <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                            </svg>
                        </button>
                    </div>

                    <div class="p-6 space-y-5">
                        {move || error_msg.get().map(|err| view! {
                            <div class="p-3 rounded-lg bg-red-500/10 border border-red-500/30 text-red-400 text-sm">
                                {err}
                            </div>
                        })}

                        // Title
                        <div>
                            <label class="block text-sm font-medium text-white/70 mb-1.5">
                                "Title"
                                <span class="text-red-400 ml-1">"*"</span>
                            </label>
                            <input
                                type="text"
                                prop:value=move || title.get()
                                on:input=move |ev| {
                                    set_title.set(event_target_value(&ev));
                                    set_title_touched.set(true);
                                }
                                on:blur=move |_| set_title_touched.set(true)
                                class=move || format!(
                                    "w-full px-3 py-2 rounded-lg bg-white/5 border text-white placeholder-white/30 focus:outline-none focus:ring-2 transition-colors {}",
                                    if title_error().is_some() {
                                        "border-red-500/50 focus:ring-red-500/50 focus:border-red-500/50"
                                    } else if title_touched.get() && !title.get().trim().is_empty() {
                                        "border-green-500/50 focus:ring-green-500/50 focus:border-green-500/50"
                                    } else {
                                        "border-white/10 focus:ring-blue-500/50"
                                    }
                                )
                                placeholder="Task title..."
                                disabled=move || submitting.get()
                            />
                            {move || title_error().map(|err| view! {
                                <p class="mt-1.5 text-sm text-red-400 flex items-center gap-1">
                                    <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
                                    </svg>
                                    {err}
                                </p>
                            })}
                        </div>

                        // Description
                        <div>
                            <label class="block text-sm font-medium text-white/70 mb-1.5">"Description"</label>
                            <textarea
                                prop:value=move || description.get()
                                on:input=move |ev| set_description.set(event_target_value(&ev))
                                class="w-full px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white placeholder-white/30 focus:outline-none focus:ring-2 focus:ring-blue-500/50 resize-none"
                                placeholder="Task description..."
                                rows="3"
                                disabled=move || submitting.get()
                            ></textarea>
                        </div>

                        // Category & Priority row
                        <div class="grid grid-cols-2 gap-4">
                            <div>
                                <label class="block text-sm font-medium text-white/70 mb-1.5">"Category"</label>
                                <select
                                    on:change=move |ev| {
                                        let value = event_target_value(&ev);
                                        set_category.set(match value.as_str() {
                                            "bug_fix" => TaskCategory::BugFix,
                                            "refactoring" => TaskCategory::Refactoring,
                                            "documentation" => TaskCategory::Documentation,
                                            "security" => TaskCategory::Security,
                                            "performance" => TaskCategory::Performance,
                                            "ui_ux" => TaskCategory::UiUx,
                                            "infrastructure" => TaskCategory::Infrastructure,
                                            "testing" => TaskCategory::Testing,
                                            _ => TaskCategory::Feature,
                                        });
                                    }
                                    class="w-full px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white focus:outline-none focus:ring-2 focus:ring-blue-500/50"
                                    disabled=move || submitting.get()
                                >
                                    <option value="feature" selected=move || matches!(category.get(), TaskCategory::Feature)>"Feature"</option>
                                    <option value="bug_fix" selected=move || matches!(category.get(), TaskCategory::BugFix)>"Bug Fix"</option>
                                    <option value="refactoring" selected=move || matches!(category.get(), TaskCategory::Refactoring)>"Refactoring"</option>
                                    <option value="documentation" selected=move || matches!(category.get(), TaskCategory::Documentation)>"Documentation"</option>
                                    <option value="security" selected=move || matches!(category.get(), TaskCategory::Security)>"Security"</option>
                                    <option value="performance" selected=move || matches!(category.get(), TaskCategory::Performance)>"Performance"</option>
                                    <option value="ui_ux" selected=move || matches!(category.get(), TaskCategory::UiUx)>"UI/UX"</option>
                                    <option value="infrastructure" selected=move || matches!(category.get(), TaskCategory::Infrastructure)>"Infrastructure"</option>
                                    <option value="testing" selected=move || matches!(category.get(), TaskCategory::Testing)>"Testing"</option>
                                </select>
                            </div>

                            <div>
                                <label class="block text-sm font-medium text-white/70 mb-1.5">"Priority"</label>
                                <select
                                    on:change=move |ev| {
                                        let value = event_target_value(&ev);
                                        set_priority.set(match value.as_str() {
                                            "urgent" => TaskPriority::Urgent,
                                            "high" => TaskPriority::High,
                                            "low" => TaskPriority::Low,
                                            _ => TaskPriority::Medium,
                                        });
                                    }
                                    class="w-full px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white focus:outline-none focus:ring-2 focus:ring-blue-500/50"
                                    disabled=move || submitting.get()
                                >
                                    <option value="urgent" selected=move || matches!(priority.get(), TaskPriority::Urgent)>"🔴 Urgent"</option>
                                    <option value="high" selected=move || matches!(priority.get(), TaskPriority::High)>"🟠 High"</option>
                                    <option value="medium" selected=move || matches!(priority.get(), TaskPriority::Medium)>"🟡 Medium"</option>
                                    <option value="low" selected=move || matches!(priority.get(), TaskPriority::Low)>"🟢 Low"</option>
                                </select>
                            </div>
                        </div>

                        // Coding Worker. Changed on an existing Task only: a new
                        // Task is created with Claude, as before.
                        {move || match mode.get() {
                            TaskEditMode::Edit(_) => view! {
                                <div>
                                    <label class="block text-sm font-medium text-white/70 mb-1.5">"Coding Worker"</label>
                                    <select
                                        on:change=move |ev| {
                                            let next = match event_target_value(&ev).as_str() {
                                                "opencode" => CodingProvider::OpenCode,
                                                _ => CodingProvider::Claude,
                                            };
                                            // The model is kept, so switching back to Claude restores it.
                                            // Saving with OpenCode stores the provider/model typed in the AI Model field.
                                            // A provider/model id is not a Claude model, so it is not kept for Claude.
                                            if next == CodingProvider::Claude && model.get().contains('/') {
                                                set_model.set("default".to_string());
                                            }
                                            set_provider.set(next);
                                        }
                                        class="w-full px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white focus:outline-none focus:ring-2 focus:ring-blue-500/50"
                                        disabled=move || submitting.get()
                                    >
                                        <option value="claude" selected=move || provider.get() == CodingProvider::Claude>"Claude Code"</option>
                                        <option value="opencode" selected=move || provider.get() == CodingProvider::OpenCode>"OpenCode"</option>
                                    </select>
                                </div>
                            }
                            .into_any(),
                            TaskEditMode::Create => ().into_any(),
                        }}

                        // Review mode. Changed on an existing Task only; a new Task is
                        // Automatic. A run keeps the mode it started with.
                        {move || match mode.get() {
                            TaskEditMode::Edit(_) => view! {
                                <div>
                                    <label class="block text-sm font-medium text-white/70 mb-1.5">"Review"</label>
                                    <select
                                        on:change=move |ev| {
                                            let next = match event_target_value(&ev).as_str() {
                                                "manual" => ReviewMode::Manual,
                                                _ => ReviewMode::Automatic,
                                            };
                                            set_review_mode.set(next);
                                        }
                                        class="w-full px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white focus:outline-none focus:ring-2 focus:ring-blue-500/50"
                                        disabled=move || submitting.get()
                                    >
                                        <option value="automatic" selected=move || review_mode.get() == ReviewMode::Automatic>"Automatic: AI Review runs after each coding run"</option>
                                        <option value="manual" selected=move || review_mode.get() == ReviewMode::Manual>"Manual: no AI Review; you inspect the changes"</option>
                                    </select>
                                    <p class="mt-1 text-xs text-white/50">
                                        {move || match review_mode.get() {
                                            ReviewMode::Automatic => "A finished coding run starts AI Review automatically.",
                                            ReviewMode::Manual => "A finished coding run goes straight to Human Review. AI Review does not run, so you must inspect the changes yourself. Nothing is approved or published automatically.",
                                        }}
                                    </p>
                                </div>
                            }
                            .into_any(),
                            TaskEditMode::Create => ().into_any(),
                        }}

                        // AI Model
                        <div>
                            <label class="block text-sm font-medium text-white/70 mb-1.5">"AI Model"</label>
                            // OpenCode names its model as provider/model. The run starts
                            // only on that exact id, and it is part of the approved input.
                            <input
                                type="text"
                                placeholder="provider/model, for example ollama/llama3.2:3b"
                                prop:value=move || model.get()
                                on:input=move |ev| set_model.set(event_target_value(&ev))
                                class="w-full px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white focus:outline-none focus:ring-2 focus:ring-blue-500/50"
                                hidden=move || provider.get() != CodingProvider::OpenCode
                                disabled=move || submitting.get()
                            />

                            <select
                                on:change=move |ev| set_model.set(event_target_value(&ev))
                                class="w-full px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white focus:outline-none focus:ring-2 focus:ring-blue-500/50"
                                disabled=move || submitting.get() || provider.get() == CodingProvider::OpenCode
                                hidden=move || provider.get() == CodingProvider::OpenCode
                            >
                                {move || {
                                    let models = available_models.get();
                                    let current = model.get();
                                    let items: Vec<ModelInfo> = if provider.get() == CodingProvider::OpenCode {
                                        vec![ModelInfo { id: "default".to_string(), name: "OpenCode default".to_string(), alias: None }]
                                    } else if models.is_empty() {
                                        vec![ModelInfo { id: "default".to_string(), name: "Default (auto)".to_string(), alias: None }]
                                    } else {
                                        models
                                    };
                                    items.iter().map(|m| {
                                        let id = m.id.clone();
                                        let name = m.name.clone();
                                        let selected = id == current;
                                        view! {
                                            <option value=id selected=selected>{name}</option>
                                        }
                                    }).collect::<Vec<_>>()
                                }}
                            </select>
                            {move || match mode.get() {
                                TaskEditMode::Edit(task) => task.last_observed_model.map(|observed| view! {
                                    <p class="mt-1.5 text-xs text-white/50">
                                        "Last reported by the provider: " {observed}
                                        ". This is evidence from the last run and does not change the model setting."
                                    </p>
                                }),
                                TaskEditMode::Create => None,
                            }}
                        </div>

                        // AI Reviewer: the Claude model an automatic AI Review runs on. None is
                        // chosen by default, so no reviewer starts until a person picks one.
                        <div>
                            <label class="block text-sm font-medium text-white/70 mb-1.5">"AI Reviewer"</label>
                            <select
                                on:change=move |ev| set_reviewer_model.set(event_target_value(&ev))
                                class="w-full px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white focus:outline-none focus:ring-2 focus:ring-blue-500/50"
                                disabled=move || submitting.get()
                            >
                                {move || {
                                    let current = reviewer_model.get();
                                    let mut items = vec![ModelInfo {
                                        id: String::new(),
                                        name: "Not chosen: AI Review will not run".to_string(),
                                        alias: None,
                                    }];
                                    // The provider default is not a reviewer choice.
                                    items.extend(available_models.get().into_iter().filter(|m| m.id != "default"));
                                    // A configured reviewer stays selectable when the list lacks it, so
                                    // saving this form cannot clear it by accident.
                                    if !current.is_empty() && !items.iter().any(|m| m.id == current) {
                                        items.push(ModelInfo {
                                            id: current.clone(),
                                            name: format!("{current} (configured)"),
                                            alias: None,
                                        });
                                    }
                                    items.iter().map(|m| {
                                        let id = m.id.clone();
                                        let name = m.name.clone();
                                        let selected = id == current;
                                        view! {
                                            <option value=id selected=selected>{name}</option>
                                        }
                                    }).collect::<Vec<_>>()
                                }}
                            </select>
                            <p class="mt-1 text-xs text-white/50">
                                {move || match review_mode.get() {
                                    ReviewMode::Automatic => "AI Review runs on this model after each coding run. Without one, a finished run waits in Human Review and no reviewer starts.",
                                    ReviewMode::Manual => "Manual review runs no AI Review, so this model is not used.",
                                }}
                            </p>
                        </div>

                        // Depends on
                        <DependencyField
                            candidates=dependency_options
                            selected=dependency
                            choice=dependency_choice
                            disabled=Signal::derive(move || submitting.get())
                        />

                        // Planning mode toggle
                        <div class="flex items-center gap-3">
                            <button
                                type="button"
                                on:click=move |_| set_planning_mode.set(!planning_mode.get())
                                class=move || format!(
                                    "relative w-11 h-6 rounded-full transition-colors {}",
                                    if planning_mode.get() { "bg-purple-500" } else { "bg-white/10" }
                                )
                                disabled=move || submitting.get()
                            >
                                <span class=move || format!(
                                    "absolute top-0.5 left-0.5 w-5 h-5 bg-white rounded-full shadow transition-transform {}",
                                    if planning_mode.get() { "translate-x-5" } else { "" }
                                )></span>
                            </button>
                            <span class="text-sm text-white/70">"Planning Mode"</span>
                            <span class="text-xs text-white/40">"(AI will plan before coding)"</span>
                        </div>
                    </div>

                    <div class="sticky bottom-0 flex items-center justify-between gap-3 p-6 border-t border-white/5 bg-[#0B0B0F]">
                        <div>
                            {move || is_edit_mode().then(|| {
                                view! {
                                    <button
                                        on:click=handle_delete
                                        class=move || format!(
                                            "px-4 py-2 rounded-lg transition-colors {}",
                                            if confirm_delete.get() {
                                                "bg-red-500 hover:bg-red-600 text-white"
                                            } else {
                                                "text-red-400 hover:text-red-300 hover:bg-red-500/10"
                                            }
                                        )
                                        disabled=move || deleting.get()
                                    >
                                        {move || if deleting.get() {
                                            "Deleting...".to_string()
                                        } else if confirm_delete.get() {
                                            "Confirm Delete".to_string()
                                        } else {
                                            "Delete Task".to_string()
                                        }}
                                    </button>
                                }
                            })}
                        </div>

                        <div class="flex items-center gap-3">
                            <button
                                on:click=move |_| set_show.set(false)
                                class="px-4 py-2 rounded-lg text-white/70 hover:text-white/90 hover:bg-white/5 transition-colors"
                                disabled=move || submitting.get()
                            >
                                "Cancel"
                            </button>
                            <button
                                on:click=move |_| {
                                    // Mark title as touched to show validation error if empty
                                    set_title_touched.set(true);
                                    if !is_form_valid() {
                                        return;
                                    }
                                    
                                    let title_val = title.get();
                                    if title_val.trim().is_empty() {
                                        set_error_msg.set(Some("Task title is required".to_string()));
                                        return;
                                    }
                                    // An OpenCode run needs its model as provider/model. The backend
                                    // refuses any other id when the run starts, so the form refuses it first.
                                    if provider.get() == CodingProvider::OpenCode && !is_opencode_model_id(model.get().trim()) {
                                        set_error_msg.set(Some(
                                            "OpenCode needs its model as provider/model, for example ollama/llama3.2:3b".to_string(),
                                        ));
                                        return;
                                    }

                                    set_submitting.set(true);
                                    set_error_msg.set(None);

                                    let project_id = project_id_signal.get();
                                    let on_save = on_save;
                                    
                                    let description_val = description.get();
                                    let category_val = category.get();
                                    let priority_val = priority.get();
                                    let provider_val = provider.get();
                                    let review_mode_val = review_mode.get();
                                    let reviewer_val = reviewer_model.get();
                                    // OpenCode saves the provider/model id typed in the form, so a
                                    // Claude model left in the form is never saved with it.
                                    let model_val = if provider_val == CodingProvider::OpenCode {
                                        model.get().trim().to_string()
                                    } else {
                                        model.get()
                                    };
                                    let planning_val = planning_mode.get();
                                    let mode_val = mode.get();
                                    let dependency_val = dependency.get();
                                    // Sent only when the form can change the dependency and the selection
                                    // differs from the recorded one; otherwise the record is left alone.
                                    let edit_dependency = match &mode_val {
                                        TaskEditMode::Edit(task)
                                            if choice_for(Some(&**task)) == DependencyChoice::Open
                                                && dependency_val != task.dependencies.first().copied() =>
                                        {
                                            Some(requested(dependency_val))
                                        }
                                        _ => None,
                                    };

                                    spawn_local(async move {
                                        let result = match mode_val {
                                            TaskEditMode::Create => {
                                                create_task(crate::services::task_service::CreateTaskParams {
                                                    project_id,
                                                    title: title_val,
                                                    description: if description_val.is_empty() { None } else { Some(description_val) },
                                                    model: model_val,
                                                    reviewer_model: Some(reviewer_val),
                                                    planning_mode: planning_val,
                                                    dependencies: requested(dependency_val),
                                                    category: Some(category_val),
                                                    priority: Some(priority_val),
                                                    complexity: None,
                                                    impact: None,
                                                    security_severity: None,
                                                }).await
                                            }
                                            TaskEditMode::Edit(task) => {
                                                // The dependency is refused or written before the other fields, so
                                                // a refusal leaves the Task as it was and the form as it is.
                                                if let Some(wanted) = edit_dependency {
                                                    if let Err(e) = set_task_dependencies(task.id.to_string(), wanted).await {
                                                        set_error_msg.set(Some(e));
                                                        set_submitting.set(false);
                                                        return;
                                                    }
                                                }
                                                update_task(crate::services::task_service::UpdateTaskParams {
                                                    task_id: task.id.to_string(),
                                                    title: Some(title_val),
                                                    description: Some(if description_val.is_empty() { None } else { Some(description_val) }),
                                                    category: Some(category_val),
                                                    priority: Some(priority_val),
                                                    complexity: None,
                                                    impact: None,
                                                    security_severity: None,
                                                    model: Some(model_val),
                                                    reviewer_model: Some(reviewer_val),
                                                    provider: Some(provider_val),
                                                    review_mode: Some(review_mode_val),
                                                    planning_mode: Some(planning_val),
                                                }).await.map(|opt| opt.unwrap_or(*task))
                                            }
                                        };

                                        match result {
                                            Ok(task) => {
                                                on_save.run(task);
                                                set_submitting.set(false);
                                                set_show.set(false);
                                            }
                                            Err(e) => {
                                                set_error_msg.set(Some(e));
                                                set_submitting.set(false);
                                            }
                                        }
                                    });
                                }
                                class=move || format!(
                                    "flex items-center gap-2 px-4 py-2 rounded-lg text-white transition-colors {}",
                                    if !is_form_valid() || submitting.get() {
                                        "bg-white/5 text-white/30 cursor-not-allowed"
                                    } else {
                                        "bg-blue-500 hover:bg-blue-600"
                                    }
                                )
                                disabled=move || submitting.get()
                            >
                                {move || if submitting.get() {
                                    view! {
                                        <svg class="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
                                        </svg>
                                        <span>{if is_edit_mode() { "Saving..." } else { "Creating..." }}</span>
                                    }.into_any()
                                } else {
                                    view! {
                                        <span>{if is_edit_mode() { "Save Changes" } else { "Create Task" }}</span>
                                    }.into_any()
                                }}
                            </button>
                        </div>
                    </div>
                </div>
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::is_opencode_model_id;

    #[test]
    fn an_opencode_model_is_provider_and_model_without_whitespace() {
        assert!(is_opencode_model_id("ollama/llama3.2:3b"));
        assert!(is_opencode_model_id("ollama/dcostenco/prism-coder:4b"));
        assert!(!is_opencode_model_id("default"));
        assert!(!is_opencode_model_id("sonnet"));
        assert!(!is_opencode_model_id(""));
        assert!(!is_opencode_model_id("ollama/"));
        assert!(!is_opencode_model_id("/llama3.2:3b"));
        assert!(!is_opencode_model_id("ollama/llama 3"));
        assert!(!is_opencode_model_id("ollama/llama\n3"));
    }
}
