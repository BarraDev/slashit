// SPDX-License-Identifier: MPL-2.0
//! The "Depends on" choice in the Create and Edit forms, and the dependency list
//! in the task drawer.
//!
//! A form names at most one Task of its Project. The backend decides every
//! change (`domain/task_dependencies.rs`); this module decides what the form
//! offers, and says why a choice is unavailable before a request is sent.
//!
//! A dependency sets where the Task's branch starts. It does not make the Task
//! wait for the dependency to finish, and the copy here must not say it does.

use leptos::prelude::*;
use uuid::Uuid;

use crate::models::{Task, TaskStatus};

/// The name a person sees for a column.
pub fn status_label(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Backlog => "Backlog",
        TaskStatus::Queue => "Queue",
        TaskStatus::InProgress => "In Progress",
        TaskStatus::AiReview => "AI Review",
        TaskStatus::HumanReview => "Human Review",
        TaskStatus::Done => "Done",
        TaskStatus::PrCreated => "PR Created",
        TaskStatus::Error => "Error",
    }
}

/// The first eight characters of a Task identifier, enough to tell apart two
/// Tasks with the same title.
pub fn short_id(id: Uuid) -> String {
    id.to_string()[..8].to_string()
}

/// The Tasks a form may name: the others in the same Project, by title.
pub fn candidates(all: &[Task], project_id: Uuid, subject: Option<Uuid>) -> Vec<Task> {
    let mut found: Vec<Task> = all
        .iter()
        .filter(|t| t.project_id == project_id && Some(t.id) != subject)
        .cloned()
        .collect();
    found.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    found
}

/// What a person sees for one choice: title, column and a short identifier.
pub fn option_label(task: &Task) -> String {
    format!("{} · {} · {}", task.title, status_label(&task.status), short_id(task.id))
}

/// Why `task`'s dependency cannot be changed from the form, or `None`.
///
/// This mirrors `Task::dependency_change_refusal` on the backend, which stays
/// the authority: a Task that passes here can still be refused for an approval
/// or unfinished work the form does not show.
pub fn change_blocked_reason(task: &Task) -> Option<&'static str> {
    if !matches!(task.status, TaskStatus::Backlog | TaskStatus::Queue) {
        return Some("A dependency can be changed only while the Task is in Backlog or Queue.");
    }
    if task.branch_name.is_some() || task.worktree_path.is_some() {
        return Some("This Task already has a checkout, so its dependency is fixed.");
    }
    None
}

/// What the dependency field offers for a form.
#[derive(Clone, Debug, PartialEq)]
pub enum DependencyChoice {
    /// A new Task, or an edit that can change its single dependency.
    Open,
    /// The dependency cannot change in the Task's current state.
    Fixed(&'static str),
    /// The Task holds several dependencies, which this form cannot change.
    Several(Vec<Uuid>),
}

/// The choice a form offers for `task`, or for a new Task when there is none.
pub fn choice_for(task: Option<&Task>) -> DependencyChoice {
    match task {
        None => DependencyChoice::Open,
        Some(task) if task.dependencies.len() > 1 => DependencyChoice::Several(task.dependencies.clone()),
        Some(task) => match change_blocked_reason(task) {
            Some(reason) => DependencyChoice::Fixed(reason),
            None => DependencyChoice::Open,
        },
    }
}

/// The dependency a form's selection names, as the request sends it.
pub fn requested(selected: Option<Uuid>) -> Vec<String> {
    selected.map(|id| vec![id.to_string()]).unwrap_or_default()
}

/// The "Depends on" field of the Create and Edit forms.
#[component]
pub fn DependencyField(
    #[prop(into)] candidates: Signal<Vec<Task>>,
    selected: RwSignal<Option<Uuid>>,
    #[prop(into)] choice: Signal<DependencyChoice>,
    #[prop(into)] disabled: Signal<bool>,
) -> impl IntoView {
    view! {
        <div data-testid="task-dependency-field">
            <label class="block text-sm font-medium text-white/70 mb-1.5" for="task-dependency-select">
                "Depends on"
            </label>
            {move || match choice.get() {
                DependencyChoice::Several(recorded) => {
                    let names = candidates.with(|all| {
                        recorded
                            .iter()
                            .map(|id| match all.iter().find(|t| t.id == *id) {
                                Some(task) => option_label(task),
                                None => format!("Task {} (not on this board)", short_id(*id)),
                            })
                            .collect::<Vec<_>>()
                    });
                    view! {
                        <div data-testid="task-dependency-several" class="space-y-1.5">
                            <ul class="space-y-1 text-sm text-white/70">
                                {names.into_iter().map(|name| view! { <li>{name}</li> }).collect::<Vec<_>>()}
                            </ul>
                            <p class="text-xs text-white/50">
                                "This Task depends on several Tasks. Changing several dependencies is not supported yet, so they stay as they are."
                            </p>
                        </div>
                    }
                    .into_any()
                }
                other => {
                    let fixed = match other {
                        DependencyChoice::Fixed(reason) => Some(reason),
                        _ => None,
                    };
                    view! {
                        <select
                            id="task-dependency-select"
                            data-testid="task-dependency-select"
                            class="w-full px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white focus:outline-none focus:ring-2 focus:ring-blue-500/50"
                            disabled=move || disabled.get() || fixed.is_some()
                            on:change=move |ev| {
                                let value = event_target_value(&ev);
                                selected.set(Uuid::parse_str(&value).ok());
                            }
                        >
                            <option value="" selected=move || selected.get().is_none()>"No dependency"</option>
                            {move || {
                                let current = selected.get();
                                let offered = candidates.get();
                                let mut rendered: Vec<AnyView> = Vec::new();
                                // A recorded dependency that is not offered (for example, one that is not on this
                                // board) is still the recorded choice, so it is shown rather than hidden behind
                                // "No dependency".
                                if let Some(id) = current.filter(|id| !offered.iter().any(|t| t.id == *id)) {
                                    let label = format!("Task {} (not on this board)", short_id(id));
                                    rendered.push(view! { <option value=id.to_string() selected=true>{label}</option> }.into_any());
                                }
                                for task in offered {
                                    let id = task.id.to_string();
                                    let is_selected = current == Some(task.id);
                                    let label = option_label(&task);
                                    rendered.push(view! { <option value=id selected=is_selected>{label}</option> }.into_any());
                                }
                                rendered
                            }}
                        </select>
                        {fixed.map(|reason| view! {
                            <p data-testid="task-dependency-fixed" class="mt-1.5 text-xs text-white/50">{reason}</p>
                        })}
                        <p class="mt-1.5 text-xs text-white/40">
                            "The dependency sets where this Task's branch starts. It does not make the Task wait for the dependency to finish."
                        </p>
                    }
                    .into_any()
                }
            }}
        </div>
    }
}

/// The dependencies of a Task in the drawer, each a control that opens it.
#[component]
pub fn DependencyList(
    task: Task,
    tasks: ReadSignal<Vec<Task>>,
    on_open: Callback<Uuid>,
) -> impl IntoView {
    let dependencies = task.dependencies;
    view! {
        <section data-testid="task-drawer-dependencies" class="space-y-2">
            <h3 class="text-xs font-semibold uppercase tracking-wide text-white/40">"Depends on"</h3>
            <ul class="space-y-1.5">
                {dependencies
                    .into_iter()
                    .map(|id| {
                        let found = tasks.with(|all| all.iter().find(|t| t.id == id).cloned());
                        match found {
                            Some(parent) => {
                                let label = option_label(&parent);
                                view! {
                                    <li>
                                        <button
                                            type="button"
                                            data-testid="task-drawer-dependency"
                                            class="text-left text-sm text-blue-300 hover:text-blue-200 underline underline-offset-2"
                                            on:click=move |_| on_open.run(id)
                                        >
                                            {label}
                                        </button>
                                    </li>
                                }
                                .into_any()
                            }
                            None => view! {
                                <li data-testid="task-drawer-dependency-missing" class="text-xs text-white/40">
                                    {format!("Task {} is not on this board", short_id(id))}
                                </li>
                            }
                            .into_any(),
                        }
                    })
                    .collect::<Vec<_>>()}
            </ul>
            <p class="text-xs text-white/40">
                "The dependency sets where this Task's branch starts. It does not make the Task wait for the dependency to finish."
            </p>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(project_id: Uuid, title: &str, status: &str) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "project_id": project_id,
            "title": title,
            "description": null,
            "status": status,
            "model": "default",
            "planning_mode": false,
            "dependencies": [],
            "workspace_id": null,
            "jj_change_id": null,
            "category": "feature",
            "priority": "medium",
            "complexity": "moderate",
            "impact": "medium",
            "security_severity": "none",
            "phase": "idle",
            "phase_progress": 0,
            "overall_progress": 0,
            "subtasks": [],
            "sequence_number": 0,
            "github_issue_url": null,
            "gitlab_issue_url": null,
            "linear_ticket_id": null,
            "pr_url": null,
            "qa_signoff": null,
            "stuck_since": null,
            "created_at": chrono::Utc::now(),
            "updated_at": chrono::Utc::now(),
        }))
        .expect("a test task")
    }

    #[test]
    fn candidates_are_the_other_tasks_of_the_project_by_title() {
        let project = Uuid::new_v4();
        let subject = task(project, "Subject", "backlog");
        let beta = task(project, "beta", "done");
        let alpha = task(project, "Alpha", "queue");
        let foreign = task(Uuid::new_v4(), "Foreign", "backlog");
        let all = vec![subject.clone(), beta, alpha, foreign];

        let found = candidates(&all, project, Some(subject.id));

        let titles: Vec<&str> = found.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["Alpha", "beta"]);
    }

    #[test]
    fn an_option_names_the_title_column_and_a_short_identifier() {
        let parent = task(Uuid::new_v4(), "Write the parser", "human_review");

        let label = option_label(&parent);

        assert!(label.starts_with("Write the parser · Human Review · "), "{label}");
        assert!(label.ends_with(&short_id(parent.id)), "{label}");
        assert_eq!(short_id(parent.id).len(), 8);
    }

    #[test]
    fn a_task_in_backlog_or_queue_without_a_checkout_may_change_its_dependency() {
        let project = Uuid::new_v4();
        assert_eq!(change_blocked_reason(&task(project, "a", "backlog")), None);
        assert_eq!(change_blocked_reason(&task(project, "a", "queue")), None);
    }

    #[test]
    fn a_task_past_backlog_or_queue_cannot_change_its_dependency() {
        let project = Uuid::new_v4();
        for status in ["in_progress", "human_review", "done", "error"] {
            assert!(change_blocked_reason(&task(project, "a", status)).is_some(), "{status}");
        }
    }

    #[test]
    fn a_task_with_a_checkout_cannot_change_its_dependency() {
        let mut checked_out = task(Uuid::new_v4(), "a", "backlog");
        checked_out.branch_name = Some("slashit/a".to_string());
        assert!(change_blocked_reason(&checked_out).is_some());
    }

    #[test]
    fn a_new_form_is_open_and_a_task_with_several_dependencies_is_read_only() {
        let mut legacy = task(Uuid::new_v4(), "legacy", "backlog");
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        legacy.dependencies = vec![first, second];

        assert_eq!(choice_for(None), DependencyChoice::Open);
        assert_eq!(choice_for(Some(&legacy)), DependencyChoice::Several(vec![first, second]));
    }

    #[test]
    fn a_task_past_its_checkout_offers_a_fixed_choice_with_the_reason() {
        let running = task(Uuid::new_v4(), "running", "in_progress");
        assert!(matches!(choice_for(Some(&running)), DependencyChoice::Fixed(_)));
    }

    #[test]
    fn a_selection_is_sent_as_one_identifier_or_none() {
        let id = Uuid::new_v4();
        assert_eq!(requested(Some(id)), vec![id.to_string()]);
        assert!(requested(None).is_empty());
    }
}
