//! Integrating a task's approved work into a local branch, from the task
//! drawer: after the task was completed locally ("Integrate locally"), or
//! from Human Review as one action that also completes it ("Complete and
//! Integrate").
//!
//! The panel asks the backend what integrating would do (nothing is changed
//! by asking), shows the destination, the exact commits, the strategy and
//! the checkout it would affect, and integrates only after a deliberate
//! confirmation of exactly what was shown. That confirmation is a snapshot of
//! the plan taken when it was opened: it is what the box shows and what a
//! click sends, and it is closed, never silently replaced, when a later look
//! finds a different plan or a command refuses it. Nothing here pushes, and
//! nothing decides on its own that something succeeded: a command's answer
//! replaces the task, and its refusal stays in place until it is dismissed.

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
    // Why the last action was refused. Only a person dismisses it, or starts
    // the next action: looking again must not make it disappear.
    let error = RwSignal::new(None::<String>);
    let read_error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    // The plan the person is confirming, as it was shown when they opened the
    // confirmation. Present exactly while the confirmation is open.
    let confirming = RwSignal::new(None::<IntegrationPreview>);
    // How the plan changed under an open confirmation, until acknowledged.
    let plan_changed = RwSignal::new(None::<Vec<String>>);
    // Only the newest look may answer: an older one can arrive after it.
    let looks = StoredValue::new(0u64);

    // Asked again whenever the task's integration changes, and on demand.
    let refresh = move || {
        // The panel can be gone when an action resolves and asks again.
        let Some(previous) = looks.try_get_value() else {
            return;
        };
        let mine = previous + 1;
        looks.try_set_value(mine);
        spawn_local(async move {
            let answer = get_integration_preview(task_id.to_string()).await;
            if looks.try_get_value() != Some(mine) {
                return;
            }
            match answer {
                Ok(answer) => {
                    read_error.try_set(None);
                    // While an action runs its confirmation is already spent.
                    let shown = if busy.try_get_untracked().unwrap_or(true) {
                        None
                    } else {
                        confirming.try_get_untracked().flatten()
                    };
                    if let Some(shown) = shown {
                        let changes = shown.plan_changes(&answer);
                        if !changes.is_empty() {
                            confirming.try_set(None);
                            plan_changed.try_set(Some(changes));
                        }
                    }
                    preview.try_set(Some(answer));
                }
                Err(e) => {
                    read_error.try_set(Some(format!("Could not read the integration: {e}")));
                }
            }
        });
    };
    Effect::new(move |_| {
        // Tracked: a record written elsewhere, or a restart's reload.
        let _ = task.with(|t| t.as_ref().map(|t| (t.local_integration.clone(), t.updated_at)));
        refresh();
    });

    // One action at a time, each applying the task it returns. A completing
    // action (`not_completed` is the start of its message) can answer with a
    // task that is not completed: that is not an error, the task is applied
    // and says why it is still in review.
    let run = move |action: std::pin::Pin<Box<dyn std::future::Future<Output = Result<Task, String>>>>,
                    done: &'static str,
                    not_completed: Option<&'static str>| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        plan_changed.set(None);
        spawn_local(async move {
            match action.await {
                Ok(updated) => {
                    let failure = not_completed.and_then(|prefix| not_completed_message(prefix, &updated));
                    confirming.try_set(None);
                    apply_task.try_run(updated);
                    match failure {
                        None => crate::components::toast::success(done.to_string()),
                        Some(message) => {
                            error.try_set(Some(message));
                        }
                    }
                }
                Err(e) => {
                    // A refused confirmation is spent: the plan it named may
                    // no longer be the one on offer, so a new one is opened.
                    confirming.try_set(None);
                    error.try_set(Some(e));
                }
            }
            busy.try_set(false);
            refresh();
        });
    };

    let integrate = move |_| {
        let Some(confirmation) = confirming.with_untracked(|p| p.as_ref().and_then(IntegrationPreview::confirmation)) else {
            return;
        };
        if completing {
            run(
                Box::pin(complete_and_integrate_task(task_id.to_string(), confirmation)),
                "Integrated and completed. Nothing was pushed.",
                Some("The work is integrated, but the task was not completed"),
            );
            return;
        }
        run(Box::pin(integrate_task_locally(task_id.to_string(), confirmation)), "Integrated locally. Nothing was pushed.", None);
    };
    // The integration is recorded and only the completion is owed: finishing
    // it is the ordinary completion, which repeats no integration.
    let finish_completing = move |_| {
        let action = async move {
            complete_task_locally(task_id.to_string(), true)
                .await
                .map_err(|e| format!("The task was not completed: {e}"))
        };
        run(Box::pin(action), "Task completed.", Some("The task was not completed"));
    };
    let resume = move |_| {
        // A pending record that carries the intent to complete is resumed to
        // completion, so a task that is still in review is not a success.
        let completes = preview.with_untracked(|p| p.as_ref().is_some_and(IntegrationPreview::completes_task));
        let (done, not_completed) = if completes {
            ("Integrated and completed. Nothing was pushed.", Some("The work is integrated, but the task was not completed"))
        } else {
            ("Integration finished. Nothing was pushed.", None)
        };
        run(Box::pin(resume_local_integration(task_id.to_string())), done, not_completed);
    };
    let discard = move |_| {
        run(Box::pin(discard_local_integration(task_id.to_string())), "Integration record discarded. Discard touches no file.", None);
    };
    let reconfirm = move |tip: String| {
        run(Box::pin(reconfirm_approved_tip(task_id.to_string(), tip)), "Commit confirmed.", None);
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
                        let completes = p.completes_task();
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
                        let button_label = confirm_label(completing, state);
                        let completion_lines = completing.then(|| {
                            task.with_untracked(|t| {
                                IntegrationPreview::completion_lines(
                                    t.as_ref().and_then(|t| t.worktree_path.as_deref()),
                                    t.as_ref().and_then(|t| t.branch_name.as_deref()),
                                )
                            })
                        });
                        let in_checkout = state == PreviewState::ReadyInCheckout;
                        let snapshot = p.clone();
                        let target_tip = p.target_tip.clone().unwrap_or_default();
                        view! {
                            <div data-testid="local-integration-ready" data-state=if in_checkout { "ready_in_checkout" } else { "ready" } data-target-tip=target_tip class="space-y-2 text-xs text-white/60">
                                <p>{if state == PreviewState::AlreadyContained { "The destination branch already contains the approved commit." } else { "The approved work is on its Task branch and has not reached any other branch." }}</p>
                                {completing.then(|| view! {
                                    <p data-testid="local-integration-explainer">"One action: integrate the approved commit into the destination, then complete the task. Pushing and pull requests are not part of it."</p>
                                })}
                                <button data-testid="local-integration-start"
                                    class="px-3 py-1.5 rounded-lg text-sm bg-emerald-500/15 text-emerald-200 hover:bg-emerald-500/25 disabled:opacity-50"
                                    disabled=move || busy.get() || confirming.with(Option::is_some)
                                    on:click=move |_| {
                                        error.set(None);
                                        plan_changed.set(None);
                                        confirming.set(Some(snapshot.clone()));
                                    }>{button_label}</button>
                                {move || confirming.get().map(|shown| {
                                    let facts = shown.facts();
                                    let strategy_effect = shown.strategy.map(|s| s.effect());
                                    let commits = shown.commits.clone();
                                    let notes: Vec<String> = [shown.checkout_note(), shown.unapproved_note()].into_iter().flatten().collect();
                                    let remote = shown.remote_note();
                                    let button_label = confirm_label(completing, shown.state);
                                    view! {
                                    <div data-testid="local-integration-confirm" class="rounded-md border border-white/10 bg-black/20 p-2 space-y-2">
                                        <p class="text-xs font-semibold text-white/70">{format!("{button_label}?")}</p>
                                        <ul data-testid="local-integration-facts" class="space-y-0.5">
                                            {facts.into_iter().map(|(label, value)| view! {
                                                <li class="break-words"><span class="text-white/40">{format!("{label}: ")}</span><span class="font-mono text-white/75">{value}</span></li>
                                            }).collect_view()}
                                        </ul>
                                        {strategy_effect.map(|e| view! { <p data-testid="local-integration-strategy">{e}</p> })}
                                        {(!commits.is_empty()).then(|| view! {
                                            <div>
                                                <p class="text-white/40">"Commits the branch gains"</p>
                                                <ul data-testid="local-integration-commits" class="list-disc pl-5 font-mono">
                                                    {commits.into_iter().map(|c| view! { <li class="break-words">{c}</li> }).collect_view()}
                                                </ul>
                                            </div>
                                        })}
                                        {notes.into_iter().map(|note| view! { <p data-testid="local-integration-note" class="text-amber-200/80">{note}</p> }).collect_view()}
                                        {completion_lines.clone().map(|lines| view! {
                                            <ul data-testid="local-integration-completion" class="list-disc pl-5 space-y-0.5">
                                                {lines.into_iter().map(|line| view! { <li class="break-words">{line}</li> }).collect_view()}
                                            </ul>
                                        })}
                                        <p data-testid="local-integration-remote">{remote}</p>
                                        <div class="flex gap-2">
                                            <button data-testid="local-integration-confirm-button"
                                                class="px-3 py-1.5 rounded-lg text-sm font-medium bg-emerald-500/25 text-emerald-100 hover:bg-emerald-500/35 disabled:opacity-50"
                                                disabled=move || busy.get()
                                                on:click=integrate>{move || if busy.get() { "Integrating…" } else { button_label }}</button>
                                            <button data-testid="local-integration-cancel"
                                                class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/60 hover:bg-white/10"
                                                disabled=move || busy.get()
                                                on:click=move |_| confirming.set(None)>"Cancel"</button>
                                        </div>
                                    </div>
                                    }
                                })}
                            </div>
                        }.into_any()
                    }
                }
            })}

            {move || preview.with(|p| p.is_none()).then(|| view! {
                <p class="text-xs text-white/40">"Checking what integrating would do…"</p>
            })}
            {move || read_error.get().map(|e| view! {
                <p data-testid="local-integration-read-error" class="text-xs text-red-300 whitespace-pre-wrap break-words">{e}</p>
            })}
            {move || plan_changed.get().map(|changes| view! {
                <div data-testid="local-integration-plan-changed" role="alert" class="space-y-1 rounded-md border border-amber-500/30 bg-amber-500/[0.08] p-2 text-xs">
                    <p class="font-semibold text-amber-200">"The plan changed while you were confirming. Nothing was changed."</p>
                    <ul class="list-disc pl-5 text-white/70">
                        {changes.into_iter().map(|line| view! { <li class="break-words">{line}</li> }).collect_view()}
                    </ul>
                    <p class="text-white/60">"Review the new plan, then confirm again."</p>
                    <button data-testid="local-integration-plan-changed-dismiss"
                        class="px-2 py-1 rounded-md text-xs bg-white/5 text-white/60 hover:bg-white/10"
                        on:click=move |_| plan_changed.set(None)>"Dismiss"</button>
                </div>
            })}
            {move || error.get().map(|e| view! {
                <div data-testid="local-integration-error" role="alert" class="space-y-1 text-xs">
                    <p class="text-red-300 whitespace-pre-wrap break-words">{e}</p>
                    <button data-testid="local-integration-error-dismiss"
                        class="px-2 py-1 rounded-md text-xs bg-white/5 text-white/60 hover:bg-white/10"
                        on:click=move |_| error.set(None)>"Dismiss"</button>
                </div>
            })}
        </div>
    }
}

/// What the action reads in a preview in `state`.
fn confirm_label(completing: bool, state: PreviewState) -> &'static str {
    match (completing, state == PreviewState::AlreadyContained) {
        (true, _) => "Complete and Integrate",
        (false, true) => "Record as integrated",
        (false, false) => "Integrate locally",
    }
}

/// The explanation for a completing action whose answer is a task that did
/// not complete, or `None` when it did. `prefix` says what is already true.
fn not_completed_message(prefix: &str, task: &Task) -> Option<String> {
    (!task.completed_locally()).then(|| {
        let reason = task.error_message.as_deref().unwrap_or("SlashIt recorded no reason.");
        format!("{prefix}: {reason}")
    })
}

/// Whether the work approved now was integrated. A finished integration of
/// earlier approved work (the task went back to work since) does not say so.
pub fn is_integrated(task: &Task) -> bool {
    task.integrating_current_approval()
        && task.local_integration.as_ref().is_some_and(|r| r.state == IntegrationState::Integrated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TaskStatus;

    fn task(status: TaskStatus) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "project_id": Uuid::new_v4(),
            "title": "t",
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
        .expect("a task as the backend sends it")
    }

    const PREFIX: &str = "The work is integrated, but the task was not completed";

    /// A resumed integration whose checkout the completion could not remove:
    /// the backend answers `Ok` with the task still in Human Review and the
    /// reason recorded.
    #[test]
    fn a_resumed_integration_whose_checkout_cleanup_was_refused_is_not_reported_as_completed() {
        let mut kept = task(TaskStatus::HumanReview);
        kept.error_message = Some("The worktree at /w was kept: locked".into());
        assert_eq!(
            not_completed_message(PREFIX, &kept).as_deref(),
            Some("The work is integrated, but the task was not completed: The worktree at /w was kept: locked")
        );
        kept.error_message = None;
        assert_eq!(
            not_completed_message(PREFIX, &kept).as_deref(),
            Some("The work is integrated, but the task was not completed: SlashIt recorded no reason.")
        );
    }

    /// Approval A was integrated, the task went back to work, and approval B
    /// was completed locally without integrating: the drawer must not say
    /// that the work was integrated, and must say so again once B is.
    #[test]
    fn an_earlier_approvals_integration_is_not_the_current_approvals() {
        use crate::models::integration::{IntegrationStrategy, LocalIntegration};
        let record = |tip: &str| LocalIntegration {
            state: IntegrationState::Integrated,
            target_branch: "main".into(),
            target_old_oid: "a".repeat(40),
            approved_tip: tip.into(),
            result_oid: "c".repeat(40),
            strategy: IntegrationStrategy::FastForward,
            checkout_path: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            completes_task: false,
        };
        let approve = |t: &mut Task, sequence: u32, tip: &str| {
            t.human_review.entries.push(crate::models::HumanReviewEntry {
                sequence,
                arrival: t.human_review.arrivals,
                decision: crate::models::HumanReviewDecision::Approved,
                feedback: None,
                approved_tip: Some(tip.into()),
                decided_at: chrono::Utc::now(),
            });
        };
        let (a, b) = ("a1".repeat(20), "b2".repeat(20));
        let mut t = task(TaskStatus::Done);
        approve(&mut t, 1, &a);
        t.local_integration = Some(record(&a));
        assert!(is_integrated(&t), "approval A is the integrated one");

        approve(&mut t, 2, &b);
        assert!(!is_integrated(&t), "approval B was completed without integrating");

        t.local_integration = Some(record(&b));
        assert!(is_integrated(&t), "B integrated after completing locally");
    }

    #[test]
    fn a_task_that_completed_has_nothing_to_explain() {
        let mut done = task(TaskStatus::Done);
        let mut entries = Vec::new();
        slashit_activity::record(&mut entries, chrono::Utc::now(), slashit_activity::Kind::CompletedLocally {});
        done.activity = entries;
        assert_eq!(not_completed_message(PREFIX, &done), None);
    }
}
