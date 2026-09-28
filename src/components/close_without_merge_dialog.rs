//! The confirmation every ordinary move from Human Review to Done goes
//! through: a drop on the Done column and the card menu's Move to, Done.
//!
//! Done from Human Review is not delivery. The backend refuses the move
//! unless it is confirmed as closing without merging (see
//! `commands::task::refuse_unconfirmed_close`), and this dialog says what the
//! move actually does, as `lifecycle::terminalize` does it:
//!
//! - no pull request is created, and nothing is merged anywhere;
//! - the task is marked Done;
//! - its Task Checkout is removed with `git worktree remove`, which refuses a
//!   checkout that still has uncommitted changes, and then the task stays in
//!   Human Review;
//! - its branch is never deleted or pushed by this, so its commits stay
//!   there. A run's work is committed onto that branch before Human Review.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::models::Task;
use crate::services::human_review_service::close_without_merging;

/// A move to Done waiting for the person to confirm it.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingClose {
    pub task: Task,
    /// Where in the Done column the card was dropped.
    pub position: i32,
}

/// What closing `task` without merging does, one statement per line.
pub fn close_consequences(task: &Task) -> Vec<String> {
    let mut lines = vec![
        "No pull request will be created.".to_string(),
        "Nothing will be merged into main or any other branch.".to_string(),
        "The task will be marked Done.".to_string(),
    ];
    if let Some(path) = &task.worktree_path {
        lines.push(format!(
            "Its Task Checkout at {path} will be removed. If it has uncommitted changes, Git \
             refuses and the task stays in Human Review."
        ));
    }
    match &task.branch_name {
        Some(branch) => lines.push(format!(
            "Its commits stay on the branch {branch} in your local repository. SlashIt does not \
             delete or push that branch."
        )),
        None => lines.push("This task has no branch recorded, so no commits are kept for it.".to_string()),
    }
    lines
}

#[component]
pub fn CloseWithoutMergeDialog(
    pending: RwSignal<Option<PendingClose>>,
    /// Apply a task record the command returned.
    apply_task: Callback<Task>,
    /// Read the task list again, for the neighbours the move renumbered.
    refresh_tasks: Callback<()>,
) -> impl IntoView {
    let closing = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    // A new request starts clean.
    Effect::new(move |_| {
        if pending.with(|p| p.is_some()) {
            error.set(None);
        }
    });

    let cancel = move |_| {
        if !closing.get_untracked() {
            pending.set(None);
        }
    };

    let confirm = move |_| {
        let Some(request) = pending.get_untracked() else { return };
        if closing.get_untracked() {
            return;
        }
        closing.set(true);
        error.set(None);
        spawn_local(async move {
            match close_without_merging(request.task.id.to_string(), request.position).await {
                Ok(Some(task)) => {
                    apply_task.try_run(task);
                    refresh_tasks.try_run(());
                    pending.try_set(None);
                    crate::components::toast::success(format!(
                        "'{}' closed without merging",
                        request.task.title
                    ));
                }
                Ok(None) => {
                    error.try_set(Some("This task no longer exists.".to_string()));
                }
                // The task is where it was; say why, here.
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            closing.try_set(false);
        });
    };

    view! {
        {move || pending.get().map(|request| {
            let lines = close_consequences(&request.task);
            view! {
                <div class="fixed inset-0 z-[60] flex items-center justify-center" data-testid="close-without-merge-overlay">
                    <div class="absolute inset-0 bg-black/60" on:click=cancel></div>
                    <div
                        data-testid="close-without-merge-dialog"
                        data-task-id=request.task.id.to_string()
                        role="alertdialog"
                        aria-modal="true"
                        aria-labelledby="close-without-merge-title"
                        class="relative w-full max-w-md rounded-xl border border-white/10 bg-[#14141c] p-5 shadow-2xl space-y-4"
                    >
                        <div class="space-y-1">
                            <h2 id="close-without-merge-title" class="text-base font-semibold text-white/90">"Close without merging?"</h2>
                            <p class="text-sm text-white/50 break-words">{request.task.title.clone()}</p>
                        </div>
                        <ul data-testid="close-without-merge-consequences" class="list-disc pl-5 space-y-1 text-sm text-white/70">
                            {lines.into_iter().map(|line| view! { <li class="break-words">{line}</li> }).collect_view()}
                        </ul>
                        <p class="text-xs text-white/45">"To deliver the work instead, cancel and approve it in the task drawer."</p>
                        {move || error.get().map(|e| view! {
                            <p data-testid="close-without-merge-error" class="text-sm text-red-300 whitespace-pre-wrap break-words">{e}</p>
                        })}
                        <div class="flex justify-end gap-2">
                            <button
                                data-testid="close-without-merge-cancel"
                                class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/70 hover:bg-white/10 disabled:opacity-50"
                                disabled=move || closing.get()
                                on:click=cancel
                            >
                                "Cancel"
                            </button>
                            <button
                                data-testid="close-without-merge-confirm"
                                class="px-3 py-1.5 rounded-lg text-sm font-medium bg-red-500/20 text-red-200 hover:bg-red-500/30 disabled:opacity-50"
                                disabled=move || closing.get()
                                on:click=confirm
                            >
                                {move || if closing.get() { "Closing…" } else { "Close without merging" }}
                            </button>
                        </div>
                    </div>
                </div>
            }
        })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(worktree: Option<&str>, branch: Option<&str>) -> Task {
        let mut task: Task = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4(),
            "project_id": uuid::Uuid::new_v4(),
            "title": "t",
            "description": null,
            "status": "human_review",
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
            "phase": "complete",
            "phase_progress": 0,
            "overall_progress": 90,
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
        .unwrap();
        task.worktree_path = worktree.map(str::to_string);
        task.branch_name = branch.map(str::to_string);
        task
    }

    #[test]
    fn the_consequences_name_the_checkout_and_the_branch_that_keeps_the_work() {
        let lines = close_consequences(&task(Some("/w/task-1"), Some("task-1")));
        let text = lines.join("\n");
        assert!(text.contains("No pull request will be created."));
        assert!(text.contains("Nothing will be merged"));
        assert!(text.contains("marked Done"));
        assert!(text.contains("/w/task-1 will be removed"));
        assert!(text.contains("uncommitted changes"));
        assert!(text.contains("branch task-1"));
        assert!(text.contains("does not delete or push"));
    }

    #[test]
    fn nothing_is_promised_about_a_checkout_or_branch_the_task_does_not_have() {
        let text = close_consequences(&task(None, None)).join("\n");
        assert!(!text.contains("will be removed"));
        assert!(text.contains("no branch recorded"));
    }
}
