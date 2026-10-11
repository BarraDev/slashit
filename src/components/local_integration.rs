//! Integrating a task's approved work into a local branch, from the task
//! drawer: after the task was completed locally ("Integrate locally"), or
//! from Human Review as one action that also completes it ("Complete and
//! Integrate").
//!
//! The panel asks the backend what integrating would do (nothing is changed
//! by asking), shows the destination, the exact commits, the strategy and
//! the checkout it would affect, and integrates only after a deliberate
//! confirmation of exactly what was shown. Nothing here pushes, and nothing
//! decides on its own that something succeeded: a command's answer replaces
//! the task, and its refusal is shown in place.

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::models::integration::{short, IntegrationPreview, IntegrationState, PreviewState};
use crate::models::Task;
use crate::services::integration_service::{
    complete_and_integrate_task, discard_local_integration, get_integration_preview, integrate_task_locally,
    reconfirm_approved_tip, resume_local_integration,
};
use crate::services::human_review_service::complete_task_locally;

#[component]
pub fn LocalIntegrationPanel(
    task_id: Uuid,
    #[prop(into)] task: Signal<Option<Task>>,
    /// Apply a task record a command returned.
    apply_task: Callback<Task>,
    /// The task is in Human Review and the action completes it too.
    #[prop(optional)]
    completing: bool,
) -> impl IntoView {
    let preview = RwSignal::new(None::<IntegrationPreview>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let confirming = RwSignal::new(false);

    // Asked again whenever the task's integration changes, and on demand.
    let refresh = move || {
        spawn_local(async move {
            match get_integration_preview(task_id.to_string()).await {
                Ok(answer) => {
                    error.try_set(None);
                    preview.try_set(Some(answer));
                }
                Err(e) => {
                    error.try_set(Some(format!("Could not read the integration: {e}")));
                }
            }
        });
    };
    Effect::new(move |_| {
        // Tracked: a record written elsewhere, or a restart's reload.
        let _ = task.with(|t| t.as_ref().map(|t| (t.local_integration.clone(), t.updated_at)));
        refresh();
    });

    // One action at a time, each applying the task it returns.
    let run = move |action: std::pin::Pin<Box<dyn std::future::Future<Output = Result<Task, String>>>>, done: &'static str| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match action.await {
                Ok(updated) => {
                    confirming.try_set(false);
                    apply_task.try_run(updated);
                    crate::components::toast::success(done.to_string());
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            busy.try_set(false);
            refresh();
        });
    };

    let integrate = move |_| {
        let Some(confirmation) = preview.with_untracked(|p| p.as_ref().and_then(IntegrationPreview::confirmation)) else {
            return;
        };
        if completing {
            // An answer that is not completed is not an error: the work is
            // integrated and the task says why it is still in review.
            if busy.get_untracked() {
                return;
            }
            busy.set(true);
            error.set(None);
            spawn_local(async move {
                match complete_and_integrate_task(task_id.to_string(), confirmation).await {
                    Ok(updated) if updated.completed_locally() => {
                        confirming.try_set(false);
                        apply_task.try_run(updated);
                        crate::components::toast::success(
                            "Integrated and completed. Nothing was pushed.".to_string(),
                        );
                    }
                    Ok(updated) => {
                        let reason = updated
                            .error_message
                            .clone()
                            .unwrap_or_else(|| "SlashIt recorded no reason.".to_string());
                        confirming.try_set(false);
                        apply_task.try_run(updated);
                        error.try_set(Some(format!(
                            "The work is integrated, but the task was not completed: {reason}"
                        )));
                    }
                    Err(e) => {
                        error.try_set(Some(e));
                    }
                }
                busy.try_set(false);
                refresh();
            });
            return;
        }
        run(Box::pin(integrate_task_locally(task_id.to_string(), confirmation)), "Integrated locally. Nothing was pushed.");
    };
    // The integration is recorded and only the completion is owed: finishing
    // it is the ordinary completion, which repeats no integration.
    let finish_completing = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match complete_task_locally(task_id.to_string(), true).await {
                Ok(updated) if updated.completed_locally() => {
                    apply_task.try_run(updated);
                    crate::components::toast::success("Task completed.".to_string());
                }
                Ok(updated) => {
                    let reason = updated
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "SlashIt recorded no reason.".to_string());
                    apply_task.try_run(updated);
                    error.try_set(Some(format!("The task was not completed: {reason}")));
                }
                Err(e) => {
                    error.try_set(Some(format!("The task was not completed: {e}")));
                }
            }
            busy.try_set(false);
            refresh();
        });
    };
    let resume = move |_| {
        run(Box::pin(resume_local_integration(task_id.to_string())), "Integration finished. Nothing was pushed.");
    };
    let discard = move |_| {
        run(Box::pin(discard_local_integration(task_id.to_string())), "Integration record discarded. Discard touches no file.");
    };
    let reconfirm = move |tip: String| {
        run(Box::pin(reconfirm_approved_tip(task_id.to_string(), tip)), "Commit confirmed.");
    };

    view! {
        <div data-testid="local-integration" class="space-y-2 border-t border-white/10 pt-2">
            <p class="text-[11px] font-semibold uppercase tracking-wide text-white/50">
                {if completing { "Complete and integrate locally" } else { "Integrate locally" }}
            </p>

            {move || preview.get().map(|p| {
                let state = p.state;
                match state {
                    PreviewState::Integrated => {
                        let line = p.integration.as_ref().map(|r| r.result_line());
                        view! {
                            <div data-testid="local-integration-result" data-state="integrated" class="space-y-1 text-xs text-white/60">
                                <p class="font-medium text-emerald-200/90">{line.unwrap_or_else(|| "Integrated.".to_string())}</p>
                                <p>"SlashIt did not push it. Publish it later yourself, when you choose to."</p>
                            </div>
                        }.into_any()
                    }
                    PreviewState::CompletionPending => {
                        let note = p.completion_pending_note();
                        let line = p.integration.as_ref().map(|r| r.result_line());
                        view! {
                            <div data-testid="local-integration-completion-pending" data-state="completion_pending" class="space-y-2 rounded-md border border-amber-500/20 bg-amber-500/[0.06] p-2 text-xs">
                                <p class="font-semibold text-amber-200">"Integrated, not completed yet"</p>
                                {line.map(|l| view! { <p data-testid="local-integration-result" class="text-emerald-200/90">{l}</p> })}
                                <p data-testid="local-integration-completion-note" class="text-white/70 whitespace-pre-wrap break-words">{note}</p>
                                <p class="text-white/50">"SlashIt did not push it, and no pull request exists."</p>
                                <button data-testid="local-integration-finish"
                                    class="px-3 py-1.5 rounded-lg text-sm bg-emerald-500/20 text-emerald-100 hover:bg-emerald-500/30 disabled:opacity-50"
                                    disabled=move || busy.get()
                                    on:click=finish_completing>{move || if busy.get() { "Completing…" } else { "Finish completing" }}</button>
                            </div>
                        }.into_any()
                    }
                    PreviewState::Pending => {
                        let note = p.pending_note();
                        let completes = p.integration.as_ref().is_some_and(|r| r.completes_task);
                        let record = p.integration.clone();
                        let can_discard = p.can_discard();
                        let problems = p.checkout_problems.clone();
                        let checkout = p.checked_out_at.clone();
                        view! {
                            <div data-testid="local-integration-pending" data-state="pending" data-observed=p.observed.clone().unwrap_or_default() class="space-y-2 rounded-md border border-amber-500/20 bg-amber-500/[0.06] p-2 text-xs">
                                <p class="font-semibold text-amber-200">"An integration was started and not finished"</p>
                                {note.map(|n| view! { <p data-testid="local-integration-pending-note" class="text-white/70">{n}</p> })}
                                {record.map(|r| view! {
                                    <ul class="space-y-0.5 text-white/55">
                                        <li>{format!("Destination branch: {}", r.target_branch)}</li>
                                        <li>{format!("Approved commit: {}", short(&r.approved_tip))}</li>
                                        <li>{format!("Planned result: {}", short(&r.result_oid))}</li>
                                        <li>{format!("Strategy: {}", r.strategy.label())}</li>
                                        {r.checkout_path.map(|path| view! { <li>{format!("Confirmed checkout: {path}")}</li> })}
                                    </ul>
                                })}
                                {checkout.map(|path| view! { <p class="text-white/55">{format!("The branch is checked out in {path}.")}</p> })}
                                {(!problems.is_empty()).then(|| view! {
                                    <ul data-testid="local-integration-checkout-problems" class="list-disc pl-5 text-amber-200/90">
                                        {problems.into_iter().map(|line| view! { <li class="break-words">{line}</li> }).collect_view()}
                                    </ul>
                                })}
                                <div class="flex gap-2">
                                    <button data-testid="local-integration-resume"
                                        class="px-3 py-1.5 rounded-lg text-sm bg-emerald-500/20 text-emerald-100 hover:bg-emerald-500/30 disabled:opacity-50"
                                        disabled=move || busy.get()
                                        on:click=resume>{if completes { "Resume and complete" } else { "Resume" }}</button>
                                    {can_discard.then(|| view! {
                                        <button data-testid="local-integration-discard"
                                            class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/70 hover:bg-white/10 disabled:opacity-50"
                                            disabled=move || busy.get()
                                            on:click=discard>"Discard"</button>
                                    })}
                                </div>
                            </div>
                        }.into_any()
                    }
                    PreviewState::NeedsReconfirmation => {
                        let reconfirm_info = p.reconfirm.clone();
                        let reason = p.reason.clone();
                        view! {
                            <div data-testid="local-integration-reconfirm" data-state="needs_reconfirmation" class="space-y-2 rounded-md border border-amber-500/20 bg-amber-500/[0.06] p-2 text-xs">
                                <p class="font-semibold text-amber-200">"Confirm the commit to integrate"</p>
                                {reason.map(|r| view! { <p class="text-white/70">{r}</p> })}
                                {reconfirm_info.map(|info| {
                                    let tip = info.branch_tip.clone();
                                    view! {
                                        <p data-testid="local-integration-reconfirm-tip" class="text-white/60">{format!("The Task branch is at {}. Confirming approves exactly this commit.", short(&info.branch_tip))}</p>
                                        <ul data-testid="local-integration-reconfirm-commits" class="list-disc pl-5 text-white/60">
                                            {info.commits.into_iter().map(|line| view! { <li class="break-words font-mono">{line}</li> }).collect_view()}
                                        </ul>
                                        {(!info.changed_files.is_empty()).then(|| view! {
                                            <div>
                                                <p class="text-white/40">"What these commits change"</p>
                                                <ul data-testid="local-integration-reconfirm-files" class="font-mono text-white/60">
                                                    {info.changed_files.into_iter().map(|line| view! { <li class="break-words">{line}</li> }).collect_view()}
                                                </ul>
                                            </div>
                                        })}
                                        <button data-testid="local-integration-reconfirm-button"
                                            class="px-3 py-1.5 rounded-lg text-sm bg-amber-500/20 text-amber-100 hover:bg-amber-500/30 disabled:opacity-50"
                                            disabled=move || busy.get()
                                            on:click=move |_| reconfirm(tip.clone())>"Confirm this commit"</button>
                                    }
                                })}
                            </div>
                        }.into_any()
                    }
                    PreviewState::Refused => {
                        let reason = p.reason.clone();
                        let conflicts = p.conflicts.clone();
                        let problems = p.checkout_problems.clone();
                        view! {
                            <div data-testid="local-integration-refused" data-state="refused" class="space-y-1 text-xs text-white/60">
                                <p class="text-white/70">"This cannot be integrated right now."</p>
                                {reason.map(|r| view! { <p data-testid="local-integration-reason" class="whitespace-pre-wrap break-words text-amber-200/90">{r}</p> })}
                                {(!conflicts.is_empty()).then(|| view! {
                                    <ul data-testid="local-integration-conflicts" class="list-disc pl-5">
                                        {conflicts.into_iter().map(|path| view! { <li class="break-words font-mono">{path}</li> }).collect_view()}
                                    </ul>
                                })}
                                {(!problems.is_empty()).then(|| view! {
                                    <p class="text-white/45">"Nothing was changed. Resolve the above, then look again."</p>
                                })}
                                <button data-testid="local-integration-refresh"
                                    class="px-2 py-1 rounded-md text-xs bg-white/5 text-white/60 hover:bg-white/10"
                                    on:click=move |_| refresh()>"Check again"</button>
                            </div>
                        }.into_any()
                    }
                    PreviewState::Ready | PreviewState::ReadyInCheckout | PreviewState::AlreadyContained => {
                        let facts = p.facts();
                        let strategy_effect = p.strategy.map(|s| s.effect());
                        let commits = p.commits.clone();
                        let notes: Vec<String> = [p.checkout_note(), p.unapproved_note()].into_iter().flatten().collect();
                        let remote = p.remote_note();
                        let button_label = match (completing, state == PreviewState::AlreadyContained) {
                            (true, _) => "Complete and Integrate",
                            (false, true) => "Record as integrated",
                            (false, false) => "Integrate locally",
                        };
                        let completion_lines = completing.then(|| {
                            task.with_untracked(|t| {
                                IntegrationPreview::completion_lines(
                                    t.as_ref().and_then(|t| t.worktree_path.as_deref()),
                                    t.as_ref().and_then(|t| t.branch_name.as_deref()),
                                )
                            })
                        });
                        let in_checkout = state == PreviewState::ReadyInCheckout;
                        view! {
                            <div data-testid="local-integration-ready" data-state=if in_checkout { "ready_in_checkout" } else { "ready" } class="space-y-2 text-xs text-white/60">
                                <p>{if state == PreviewState::AlreadyContained { "The destination branch already contains the approved commit." } else { "The approved work is on its Task branch and has not reached any other branch." }}</p>
                                {completing.then(|| view! {
                                    <p data-testid="local-integration-explainer">"One action: integrate the approved commit into the destination, then complete the task. Pushing and pull requests are not part of it."</p>
                                })}
                                <button data-testid="local-integration-start"
                                    class="px-3 py-1.5 rounded-lg text-sm bg-emerald-500/15 text-emerald-200 hover:bg-emerald-500/25 disabled:opacity-50"
                                    disabled=move || busy.get() || confirming.get()
                                    on:click=move |_| { error.set(None); confirming.set(true); }>{button_label}</button>
                                <Show when=move || confirming.get()>
                                    <div data-testid="local-integration-confirm" class="rounded-md border border-white/10 bg-black/20 p-2 space-y-2">
                                        <p class="text-xs font-semibold text-white/70">{format!("{button_label}?")}</p>
                                        <ul data-testid="local-integration-facts" class="space-y-0.5">
                                            {facts.clone().into_iter().map(|(label, value)| view! {
                                                <li class="break-words"><span class="text-white/40">{format!("{label}: ")}</span><span class="font-mono text-white/75">{value}</span></li>
                                            }).collect_view()}
                                        </ul>
                                        {strategy_effect.map(|e| view! { <p data-testid="local-integration-strategy">{e}</p> })}
                                        {(!commits.is_empty()).then(|| view! {
                                            <div>
                                                <p class="text-white/40">"Commits the branch gains"</p>
                                                <ul data-testid="local-integration-commits" class="list-disc pl-5 font-mono">
                                                    {commits.clone().into_iter().map(|c| view! { <li class="break-words">{c}</li> }).collect_view()}
                                                </ul>
                                            </div>
                                        })}
                                        {notes.clone().into_iter().map(|note| view! { <p data-testid="local-integration-note" class="text-amber-200/80">{note}</p> }).collect_view()}
                                        {completion_lines.clone().map(|lines| view! {
                                            <ul data-testid="local-integration-completion" class="list-disc pl-5 space-y-0.5">
                                                {lines.into_iter().map(|line| view! { <li class="break-words">{line}</li> }).collect_view()}
                                            </ul>
                                        })}
                                        <p data-testid="local-integration-remote">{remote.clone()}</p>
                                        <div class="flex gap-2">
                                            <button data-testid="local-integration-confirm-button"
                                                class="px-3 py-1.5 rounded-lg text-sm font-medium bg-emerald-500/25 text-emerald-100 hover:bg-emerald-500/35 disabled:opacity-50"
                                                disabled=move || busy.get()
                                                on:click=integrate>{move || if busy.get() { "Integrating…" } else { button_label }}</button>
                                            <button data-testid="local-integration-cancel"
                                                class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/60 hover:bg-white/10"
                                                disabled=move || busy.get()
                                                on:click=move |_| confirming.set(false)>"Cancel"</button>
                                        </div>
                                    </div>
                                </Show>
                            </div>
                        }.into_any()
                    }
                }
            })}

            {move || preview.with(|p| p.is_none()).then(|| view! {
                <p class="text-xs text-white/40">"Checking what integrating would do…"</p>
            })}
            {move || error.get().map(|e| view! {
                <p data-testid="local-integration-error" class="text-xs text-red-300 whitespace-pre-wrap break-words">{e}</p>
            })}
        </div>
    }
}

/// Whether a task has an integration record that says it finished.
pub fn is_integrated(task: &Task) -> bool {
    task.local_integration.as_ref().is_some_and(|r| r.state == IntegrationState::Integrated)
}
