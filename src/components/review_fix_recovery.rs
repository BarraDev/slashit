// SPDX-License-Identifier: MPL-2.0
//! The Human exit from an unresolved AI Review fix, in the task drawer. It says
//! what is blocked and why, where the preserved edits are, and what a person
//! can do. Resolve asks the backend, which checks the checkout itself and
//! returns any refusal. Nothing here commits, discards, approves or removes.

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::models::Task;
use crate::services::review_fix_service::resolve_review_fix;
use crate::services::task_service::list_tasks;

/// Replaces the shown Task with the board's own copy, the authoritative one.
async fn refresh_authoritative(task: Signal<Option<Task>>, apply_task: Callback<Task>, task_id: Uuid) {
    if let Some(project_id) = task.try_get_untracked().flatten().map(|t| t.project_id.to_string()) {
        if let Ok(tasks) = list_tasks(project_id).await {
            if let Some(fresh) = tasks.into_iter().find(|t| t.id == task_id) {
                apply_task.run(fresh);
            }
        }
    }
}

#[component]
pub fn ReviewFixRecoveryPanel(
    task_id: Uuid,
    #[prop(into)] task: Signal<Option<Task>>,
    apply_task: Callback<Task>,
) -> impl IntoView {
    let busy = RwSignal::new(false);
    let refusal = RwSignal::new(None::<String>);
    let record = move || task.get().and_then(|t| t.review_fix_residue);
    let reference_missing = move || task.get().is_some_and(|t| t.worktree_path.is_none());

    let resolve = move |_| {
        let Some(attempt_id) = record().map(|r| r.attempt_id) else {
            return;
        };
        busy.set(true);
        refusal.set(None);
        spawn_local(async move {
            let answer = resolve_review_fix(task_id, attempt_id).await;
            let _ = busy.try_set(false);
            match answer {
                Ok(()) => match task.try_get_untracked().flatten() {
                    // Clear only the record this Resolve answered for. A board refresh may
                    // have installed a newer attempt meanwhile, and that one is still open.
                    Some(mut updated) if updated.review_fix_residue.as_ref().is_some_and(|r| r.attempt_id == attempt_id) => {
                        updated.review_fix_residue = None;
                        apply_task.run(updated);
                    }
                    Some(_) => refresh_authoritative(task, apply_task, task_id).await,
                    None => {}
                },
                Err(reason) => {
                    let _ = refusal.try_set(Some(reason));
                    // A refusal can mean the record moved on; show what the board now holds.
                    refresh_authoritative(task, apply_task, task_id).await;
                }
            }
        });
    };

    view! {
        {move || record().map(|r| view! {
            <section data-testid="review-fix-recovery" class="rounded-lg border border-amber-500/30 bg-amber-500/[0.06] p-3 space-y-2">
                <h3 class="text-xs font-semibold uppercase tracking-wide text-amber-200">"Fix unresolved"</h3>
                <p class="text-sm text-amber-100/90">
                    "SlashIt is not committing, pushing or removing this Task Checkout. An AI Review or PR review fix may have left edits in it. They are kept exactly as they were found."
                </p>
                <dl class="text-xs text-white/60 space-y-1">
                    <div>
                        <dt class="inline text-white/40">"Branch: "</dt>
                        <dd class="inline" data-testid="review-fix-branch">{r.branch.clone()}</dd>
                    </div>
                    <div>
                        <dt class="inline text-white/40">"Checkout: "</dt>
                        <dd class="inline break-all" data-testid="review-fix-checkout">{r.checkout_path.clone()}</dd>
                    </div>
                    <div>
                        <dt class="inline text-white/40">"Why: "</dt>
                        <dd class="inline" data-testid="review-fix-reason">{r.reason.clone()}</dd>
                    </div>
                </dl>
                <ol class="text-xs text-white/50 list-decimal pl-4 space-y-1">
                    <li>"Use Changes to inspect the preserved edits."</li>
                    <li>"Remove the edits you do not want, outside SlashIt, or commit the rest yourself on the task branch in that checkout. A commit you make does not approve the Task; Human Review still applies."</li>
                    <li>"Press Resolve. It is refused while the checkout has uncommitted changes, is on another branch, or the Task is running."</li>
                </ol>
                {move || reference_missing().then(|| view! {
                    <p data-testid="review-fix-missing-reference" class="text-xs text-white/50">
                        "This Task no longer records a Task Checkout. If its folder is gone, Resolve clears this record. Nothing is inferred as reviewed or approved."
                    </p>
                })}
                {move || refusal.get().map(|reason| view! {
                    <p data-testid="review-fix-refusal" role="alert" class="text-sm text-red-300 whitespace-pre-wrap break-words">{reason}</p>
                })}
                <button
                    data-testid="review-fix-resolve"
                    class="px-3 py-1.5 rounded-lg text-sm bg-amber-500/15 text-amber-100 hover:bg-amber-500/25 disabled:opacity-50"
                    disabled=move || busy.get()
                    on:click=resolve
                >
                    {move || if busy.get() { "Resolving..." } else { "Resolve" }}
                </button>
            </section>
        })}
    }
}
