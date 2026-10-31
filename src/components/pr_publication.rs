// SPDX-License-Identifier: MPL-2.0
//! Publish to PR: the explicit action that pushes the task branch's current
//! commit to the task's existing pull request.
//!
//! It is a decision of its own. It does not approve the task, does not change
//! the Human Review record, and covers only the commit shown in its
//! confirmation. The backend revalidates everything and records the outcome;
//! this component only asks, confirms, and shows what the record says.

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::models::{PrPublication, PublicationState, Task, TaskStatus};
use crate::services::human_review_service::get_approval_status;
use crate::services::pr_service::publish_task_to_pr;

/// The first twelve characters of a commit, as the drawer names commits.
fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// What the approval says about `commit`, in words a person can act on.
fn approval_note(task: &Task, commit: &str) -> &'static str {
    if !task.human_review.is_approved() {
        "The task is not approved. Publishing this commit does not approve it."
    } else if task.human_review.approved_tip() == Some(commit) {
        "The approval covers this commit. Publishing does not change the approval."
    } else {
        "The approval covers an earlier commit, so it does not cover this one. Publishing does not \
         approve it."
    }
}

/// One line describing a recorded publication.
fn status_line(record: PrPublication) -> AnyView {
    let commit = short(&record.commit).to_string();
    match record.state {
        PublicationState::Pending => view! {
            <p data-testid="publish-to-pr-pending" class="text-xs text-amber-200">
                {format!("Publishing {commit} is being recorded. Nothing else can be published until its outcome is confirmed.")}
            </p>
        }
        .into_any(),
        PublicationState::Published => view! {
            <p data-testid="publish-to-pr-published" class="text-xs font-medium text-emerald-300">
                {format!("Published {commit} to the pull request. The approval was not changed.")}
            </p>
        }
        .into_any(),
        PublicationState::Refused { reason } => view! {
            <p data-testid="publish-to-pr-refused" class="text-xs text-amber-200">
                {format!("Not published: {reason}")}
            </p>
        }
        .into_any(),
        PublicationState::Unknown { reason } => view! {
            <p data-testid="publish-to-pr-unknown" class="text-xs text-amber-200">
                {format!(
                    "Publishing {commit} has an unknown outcome: {reason}. Publish again to confirm it against GitHub."
                )}
            </p>
        }
        .into_any(),
    }
}

/// The Publish to PR control and its confirmation, for a task with a branch
/// and a pull request.
#[component]
pub fn PublishToPrPanel(
    task_id: Uuid,
    task: Memo<Option<Task>>,
    /// Reload the board after a publication was recorded.
    refresh_tasks: Callback<()>,
) -> impl IntoView {
    // The commit a publication would push: the branch tip as the backend reads it now.
    let tip = RwSignal::new(None::<String>);
    let confirming = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // The last answer this panel received. The task record takes precedence
    // once a refresh has brought it.
    let answered = RwSignal::new(None::<PrPublication>);

    let load_tip = move || {
        spawn_local(async move {
            let answer = get_approval_status(task_id.to_string()).await;
            let _ = tip.try_set(answer.ok().and_then(|s| s.branch_tip));
        });
    };
    Effect::new(move |_| {
        // Re-read the tip whenever the task record changes.
        task.with(|_| ());
        load_tip();
    });

    let publish = move |commit: String, pr_url: String| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let answer = publish_task_to_pr(task_id.to_string(), commit, pr_url).await;
            let _ = busy.try_set(false);
            match answer {
                Ok(record) => {
                    let _ = answered.try_set(Some(record));
                    let _ = confirming.try_set(false);
                    refresh_tasks.run(());
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
        });
    };

    let shown = move || {
        task.with(|t| t.as_ref().and_then(|t| t.pr_publication.clone()))
            .or_else(|| answered.get())
    };

    view! {
        <div data-testid="publish-to-pr-panel" class="mt-3 space-y-2 border-t border-white/10 pt-3">
            <p class="text-xs font-semibold text-white/80">"Publish to pull request"</p>
            <p class="text-xs text-white/50">
                "Pushes one commit to this task's existing pull request. It is separate from approval: the task stays in Human Review."
            </p>

            {move || shown().map(status_line)}

            <Show when=move || tip.get().is_some() && !confirming.get() && task.with(|t| t.as_ref().is_some_and(|t| t.pr_url.is_some() && t.branch_name.is_some() && !matches!(t.status, TaskStatus::Queue | TaskStatus::InProgress | TaskStatus::AiReview)))>
                <button
                    type="button"
                    data-testid="publish-to-pr"
                    class="rounded-md border border-white/20 px-3 py-1.5 text-sm text-white/90 hover:bg-white/5"
                    on:click=move |_| confirming.set(true)
                >
                    "Publish to PR..."
                </button>
            </Show>

            <Show when=move || confirming.get()>
                <div data-testid="publish-to-pr-confirm" class="space-y-1 rounded-md border border-white/15 p-3 text-xs text-white/80">
                    <p>
                        {move || task.with(|t| t.as_ref().and_then(|t| t.branch_name.clone()).unwrap_or_default())}
                        " will be pushed to the pull request "
                        {move || task.with(|t| t.as_ref().and_then(|t| t.pr_url.clone()).unwrap_or_default())}
                        "."
                    </p>
                    <p data-testid="publish-to-pr-commit">
                        "Commit: "
                        <code>{move || tip.get().unwrap_or_default()}</code>
                    </p>
                    <p>
                        {move || {
                            match (task.with(|t| t.clone()), tip.get()) {
                                (Some(t), Some(c)) => approval_note(&t, &c).to_string(),
                                _ => String::new(),
                            }
                        }}
                    </p>
                    <p class="text-white/50">
                        "Only this commit is published. Review its changes in the task before you confirm. A later commit needs a new publication."
                    </p>
                    <div class="flex gap-2 pt-1">
                        <button
                            type="button"
                            data-testid="publish-to-pr-confirm-button"
                            class="rounded-md bg-white/10 px-3 py-1.5 text-sm text-white hover:bg-white/15 disabled:opacity-50"
                            disabled=move || busy.get()
                            on:click=move |_| {
                                let commit = tip.get_untracked();
                                let pr_url = task.with_untracked(|t| t.as_ref().and_then(|t| t.pr_url.clone()));
                                if let (Some(commit), Some(pr_url)) = (commit, pr_url) {
                                    publish(commit, pr_url);
                                }
                            }
                        >
                            "Publish this commit"
                        </button>
                        <button
                            type="button"
                            class="rounded-md px-3 py-1.5 text-sm text-white/60 hover:text-white"
                            disabled=move || busy.get()
                            on:click=move |_| confirming.set(false)
                        >
                            "Cancel"
                        </button>
                    </div>
                </div>
            </Show>

            <Show when=move || error.get().is_some()>
                <p data-testid="publish-to-pr-error" class="text-xs text-red-300">
                    {move || error.get().unwrap_or_default()}
                </p>
            </Show>
        </div>
    }
}
