// SPDX-License-Identifier: MPL-2.0
//! Starting a new coding attempt after an interrupted Worker, from the task
//! drawer. The new attempt builds on the Task Checkout as it is now. It is
//! separate from accepting the Worker's commit, which the recovery panel above
//! offers.
//!
//! The panel shows what the attempt will do with the checkout, then takes two
//! explicit steps: an authorization that binds every fact shown, and a start.
//! Neither step is applied on screen before the backend answers. A refreshed
//! snapshot that differs in any bound fact closes the confirmation.

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::components::worker_recovery::Latest;
use crate::models::{Task, WorkerHandoffStage};
use crate::services::new_attempt_service::{
    authorize_new_attempt, get_new_attempt_evidence, start_new_attempt,
    AuthorizeNewAttemptOutcome, AuthorizeNewAttemptRequest, NewAttemptEvidence,
    StartNewAttemptRequest,
};

/// The request as it would be sent for `shown`, without the id that makes it
/// one decision. Two requests with the same basis describe the same recovery.
fn basis(request: &AuthorizeNewAttemptRequest) -> AuthorizeNewAttemptRequest {
    AuthorizeNewAttemptRequest {
        decision_id: Uuid::nil(),
        ..request.clone()
    }
}

/// Facts about the attempt, as lines for the "What happens" list.
pub fn effect_lines(shown: &NewAttemptEvidence) -> Vec<String> {
    let mut lines = vec![
        "A new coding Worker starts on this Task Checkout, on this branch, as it stands now."
            .to_string(),
        "Nothing is reset, cleaned, stashed or discarded. Uncommitted changes stay in the checkout."
            .to_string(),
        "The new attempt's commit will include every uncommitted change listed below (git add -A)."
            .to_string(),
        "The coding provider can modify any file and run shell commands in this checkout. It can also repeat work the interrupted run already did."
            .to_string(),
    ];
    if shown.dirty_paths.is_empty() {
        lines.push("The checkout has no uncommitted changes.".to_string());
    }
    lines
}

/// What the interrupted run is, as lines for the "Evidence" list.
pub fn evidence_lines(shown: &NewAttemptEvidence) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(run) = shown.run {
        lines.push(format!("Interrupted run: {run}"));
    }
    if let Some(reason) = &shown.recovery_reason {
        lines.push(format!("Recorded reason: {reason}"));
    }
    if let Some(branch) = &shown.branch {
        lines.push(format!("Branch: {branch}"));
    }
    if let Some(checkout) = &shown.checkout {
        lines.push(format!("Task Checkout: {checkout}"));
    }
    if let Some(head) = &shown.expected_head {
        lines.push(format!("Branch HEAD now: {head}"));
    }
    lines
}

#[component]
pub fn NewAttemptPanel(
    task_id: Uuid,
    #[prop(into)] task: Signal<Option<Task>>,
    apply_task: Callback<Task>,
) -> impl IntoView {
    let evidence = RwSignal::new(None::<Result<NewAttemptEvidence, String>>);
    let confirming = RwSignal::new(false);
    let acknowledged = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let refusal = RwSignal::new(None::<String>);
    let held = RwSignal::new(None::<String>);
    // The decision for the confirmation in progress. Kept across retries so a
    // repeated request is the same decision, and dropped when it is answered
    // definitely or the evidence it was made on changes.
    let decision = StoredValue::new(None::<AuthorizeNewAttemptRequest>);
    // Set while the last request's answer is unknown: it is sent again, exactly.
    let unknown = RwSignal::new(false);
    let asked = StoredValue::new(Latest::default());

    let reload = move || {
        let ticket = asked.try_update_value(|latest| latest.next());
        spawn_local(async move {
            let answer = get_new_attempt_evidence(task_id.to_string()).await;
            let current = ticket
                .zip(asked.try_get_value())
                .is_some_and(|(ticket, latest)| latest.is_current(ticket));
            if !current {
                return;
            }
            if let Ok(shown) = &answer {
                // A confirmation closes when the recovery it was made on changes.
                let changed = decision
                    .try_with_value(|d| {
                        d.as_ref().is_some_and(|d| {
                            AuthorizeNewAttemptRequest::for_evidence(shown, d.decision_id)
                                .as_ref()
                                .map(basis)
                                != Some(basis(d))
                        })
                    })
                    .unwrap_or(false);
                if changed && !unknown.get_untracked() {
                    decision.try_update_value(|d| *d = None);
                    confirming.try_set(false);
                }
            }
            evidence.try_set(Some(answer));
        });
    };

    let recovering = Memo::new(move |_| {
        task.with(|t| {
            t.as_ref()
                .and_then(|t| t.worker_handoff.as_ref())
                .is_some_and(|h| matches!(&h.stage, WorkerHandoffStage::RecoveryRequired { .. }))
        })
    });
    Effect::new(move |_| {
        task.track();
        if recovering.get() {
            reload();
        }
    });

    let begin = move |_| {
        if busy.get_untracked() {
            return;
        }
        let Some(Ok(shown)) = evidence.get_untracked() else {
            return;
        };
        if decision.with_value(Option::is_none) {
            match AuthorizeNewAttemptRequest::for_evidence(&shown, Uuid::new_v4()) {
                Some(request) => decision.set_value(Some(request)),
                None => {
                    refusal.set(Some(
                        "The evidence is incomplete, so a new attempt cannot be decided.".into(),
                    ));
                    return;
                }
            }
        }
        refusal.set(None);
        acknowledged.set(false);
        confirming.set(true);
    };

    let confirm = move |_| {
        if busy.get_untracked() || !acknowledged.get_untracked() {
            return;
        }
        let Some(request) = decision.with_value(Clone::clone) else {
            return;
        };
        busy.set(true);
        refusal.set(None);
        held.set(None);
        spawn_local(async move {
            let decision_id = request.decision_id;
            match authorize_new_attempt(request.clone()).await {
                Ok(AuthorizeNewAttemptOutcome::Authorized { .. })
                | Ok(AuthorizeNewAttemptOutcome::AlreadyAuthorized { .. }) => {
                    unknown.try_set(false);
                    let started = start_new_attempt(StartNewAttemptRequest {
                        task_id,
                        decision_id,
                    })
                    .await;
                    match started {
                        Ok(task) => {
                            decision.try_set_value(None);
                            apply_task.try_run(task);
                            crate::components::toast::success(
                                "A new attempt started on this checkout.".to_string(),
                            );
                            confirming.try_set(false);
                        }
                        Err(reason) => {
                            // The authorization may stand. Starting again repeats
                            // only the start, under the same decision.
                            refusal.try_set(Some(reason));
                            reload();
                        }
                    }
                }
                Ok(AuthorizeNewAttemptOutcome::Refused { reason }) => {
                    unknown.try_set(false);
                    decision.try_set_value(None);
                    refusal.try_set(Some(reason));
                    confirming.try_set(false);
                    reload();
                }
                Ok(AuthorizeNewAttemptOutcome::DurabilityUnconfirmed { task, detail }) => {
                    unknown.try_set(false);
                    apply_task.try_run(task);
                    held.try_set(Some(detail));
                    confirming.try_set(false);
                }
                Err(e) => {
                    // The answer is unknown. This exact request is sent again, with
                    // the same id, until SlashIt gives a definite answer.
                    unknown.try_set(true);
                    refusal.try_set(Some(format!(
                        "The decision may or may not have been recorded ({e}). Confirm again to send the same decision; it cannot apply twice."
                    )));
                    reload();
                }
            }
            busy.try_set(false);
        });
    };

    view! {
        <Show when=move || recovering.get()>
            <section data-testid="new-attempt-panel" class="rounded-lg border border-white/10 bg-white/[0.03] p-3 space-y-3">
                <h3 class="text-xs font-semibold uppercase tracking-wide text-white/60">"New attempt"</h3>
                <p data-testid="new-attempt-explanation" class="text-xs text-white/50">
                    "You can start a new coding attempt on the preserved checkout. This is a decision to run the coding provider again, not an acceptance of the interrupted commit."
                </p>
                {move || held.get().map(|detail| view! {
                    <p data-testid="new-attempt-held" role="alert" class="text-xs text-amber-200 whitespace-pre-wrap break-words">
                        {format!("The decision is on the board, but SlashIt could not confirm it was saved to disk: {detail}. No attempt starts until the decision is confirmed; decide again to continue.")}
                    </p>
                })}
                {move || match evidence.get() {
                    None => view! { <p class="text-xs text-white/40">"Reading the checkout…"</p> }.into_any(),
                    Some(Err(e)) => view! {
                        <p data-testid="new-attempt-evidence-error" role="alert" class="text-xs text-red-300 break-words">
                            {format!("The checkout could not be read: {e}")}
                        </p>
                    }.into_any(),
                    Some(Ok(shown)) => {
                        let eligible = shown.eligible;
                        let dirty = shown.dirty_paths.clone();
                        let effects = effect_lines(&shown);
                        let facts = evidence_lines(&shown);
                        let refusals = shown.refusals.clone();
                        view! {
                            <div class="space-y-2">
                                <div>
                                    <p class="text-xs font-semibold text-white/70">"Evidence"</p>
                                    <ul data-testid="new-attempt-evidence" class="list-disc pl-5 space-y-0.5 text-xs text-white/60">
                                        {facts.into_iter().map(|l| view! { <li class="break-words">{l}</li> }).collect_view()}
                                    </ul>
                                </div>
                                <div>
                                    <p class="text-xs font-semibold text-white/70">"Uncommitted changes"</p>
                                    {if dirty.is_empty() {
                                        view! { <p data-testid="new-attempt-dirty" class="text-xs text-white/50">"None."</p> }.into_any()
                                    } else {
                                        view! {
                                            <ul data-testid="new-attempt-dirty" class="list-disc pl-5 space-y-0.5 text-xs font-mono text-white/60">
                                                {dirty.iter().map(|p| view! { <li class="break-all">{p.clone()}</li> }).collect_view()}
                                            </ul>
                                        }.into_any()
                                    }}
                                </div>
                                <div>
                                    <p class="text-xs font-semibold text-amber-300">"What starting does"</p>
                                    <ul data-testid="new-attempt-effects" class="list-disc pl-5 space-y-0.5 text-xs text-white/60">
                                        {effects.into_iter().map(|l| view! { <li class="break-words">{l}</li> }).collect_view()}
                                    </ul>
                                </div>
                                {(!refusals.is_empty()).then(|| view! {
                                    <div>
                                        <p class="text-xs font-semibold text-red-300">"Cannot start a new attempt yet"</p>
                                        <ul data-testid="new-attempt-refusals" class="list-disc pl-5 space-y-0.5 text-xs text-red-200">
                                            {refusals.iter().map(|l| view! { <li class="break-words">{l.clone()}</li> }).collect_view()}
                                        </ul>
                                    </div>
                                })}
                                {move || refusal.get().map(|reason| view! {
                                    <p data-testid="new-attempt-error" role="alert" class="text-xs text-red-300 whitespace-pre-wrap break-words">{reason}</p>
                                })}
                                <Show when=move || !confirming.get()>
                                    <button
                                        data-testid="new-attempt-review"
                                        class="px-3 py-1.5 rounded-lg text-sm bg-amber-500/15 text-amber-200 hover:bg-amber-500/25 disabled:opacity-50"
                                        disabled=!eligible
                                        on:click=begin
                                    >
                                        "Start a new attempt…"
                                    </button>
                                </Show>
                                <Show when=move || confirming.get()>
                                    <div data-testid="new-attempt-confirm" class="rounded-md border border-white/10 bg-black/20 p-2 space-y-2">
                                        <p class="text-xs font-semibold text-white/70">"Start a new coding attempt on this checkout?"</p>
                                        <label class="flex items-start gap-2 text-xs text-white/60">
                                            <input
                                                data-testid="new-attempt-acknowledge"
                                                type="checkbox"
                                                class="mt-0.5"
                                                prop:checked=move || acknowledged.get()
                                                on:change=move |ev| acknowledged.set(event_target_checked(&ev))
                                            />
                                            <span>"I understand that the uncommitted changes listed above will be included in the new attempt's commit, and that the coding provider can change any file in this checkout and run shell commands."</span>
                                        </label>
                                        <Show when=move || unknown.get()>
                                            <p data-testid="new-attempt-unknown" class="text-xs text-amber-200">"An earlier decision may or may not have been recorded. Confirming again sends the same decision."</p>
                                        </Show>
                                        <div class="flex gap-2">
                                            <button
                                                data-testid="new-attempt-start"
                                                class="px-3 py-1.5 rounded-lg text-sm font-medium bg-amber-500/25 text-amber-100 hover:bg-amber-500/35 disabled:opacity-50"
                                                disabled=move || busy.get() || !acknowledged.get()
                                                on:click=confirm
                                            >
                                                {move || if busy.get() { "Starting…" } else { "Start new attempt" }}
                                            </button>
                                            <button
                                                data-testid="new-attempt-cancel"
                                                class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/60 hover:bg-white/10"
                                                disabled=move || busy.get()
                                                on:click=move |_| {
                                                    if !unknown.get_untracked() {
                                                        decision.set_value(None);
                                                    }
                                                    confirming.set(false);
                                                }
                                            >
                                                "Cancel"
                                            </button>
                                        </div>
                                    </div>
                                </Show>
                            </div>
                        }.into_any()
                    }
                }}
            </section>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence() -> NewAttemptEvidence {
        NewAttemptEvidence {
            task_id: Uuid::from_u128(1),
            project_id: Uuid::from_u128(2),
            repository_id: Some(Uuid::from_u128(3)),
            run: Some(2),
            recovery_reason: Some("interrupted".into()),
            observed_commit: None,
            branch: Some("task/one".into()),
            checkout: Some("/work/one".into()),
            expected_head: Some("b".repeat(40)),
            dirty_fingerprint: Some("sha256:abc".into()),
            dirty_paths: vec!["notes.txt".into()],
            eligible: true,
            refusals: Vec::new(),
        }
    }

    #[test]
    fn the_effects_name_the_commit_of_uncommitted_work_and_the_provider_power() {
        let text = effect_lines(&evidence()).join("\n");
        assert!(text.contains("git add -A"));
        assert!(text.contains("shell commands"));
        assert!(text.contains("Nothing is reset, cleaned, stashed or discarded"));
        assert!(text.contains("repeat work"));
    }

    #[test]
    fn a_clean_checkout_says_so_and_a_dirty_one_does_not() {
        let mut clean = evidence();
        clean.dirty_paths.clear();
        assert!(effect_lines(&clean).join("\n").contains("no uncommitted changes"));
        assert!(!effect_lines(&evidence()).join("\n").contains("no uncommitted changes"));
    }

    #[test]
    fn the_evidence_lines_show_the_run_and_the_exact_head() {
        let text = evidence_lines(&evidence()).join("\n");
        assert!(text.contains("Interrupted run: 2"));
        assert!(text.contains(&"b".repeat(40)));
    }

    #[test]
    fn a_decision_ignores_its_own_id_when_comparing_recoveries() {
        let first = AuthorizeNewAttemptRequest::for_evidence(&evidence(), Uuid::from_u128(7)).unwrap();
        let second = AuthorizeNewAttemptRequest::for_evidence(&evidence(), Uuid::from_u128(8)).unwrap();
        assert_ne!(first, second);
        assert_eq!(basis(&first), basis(&second));
        let mut moved = evidence();
        moved.dirty_fingerprint = Some("sha256:def".into());
        let third = AuthorizeNewAttemptRequest::for_evidence(&moved, Uuid::from_u128(7)).unwrap();
        assert_ne!(basis(&first), basis(&third));
    }
}
