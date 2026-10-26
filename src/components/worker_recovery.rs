// SPDX-License-Identifier: MPL-2.0
//! Recovering an interrupted coding Worker from the task drawer: what is
//! confirmed, what is uncertain, and one explicit decision that accepts the
//! exact commit the person inspected.
//!
//! The snapshot shown here is informational. The decision is sent with every
//! fact it was shown as a compare-and-set expectation, and the backend
//! re-verifies all of it; nothing here decides on its own that anything
//! succeeded. No control here starts, retries or resumes a Worker.

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::models::{Task, WorkerHandoffStage};
use crate::services::worker_recovery_service::{
    accept_worker_result, get_worker_recovery_evidence, AcceptWorkerResultOutcome,
    AcceptWorkerResultRequest, WorkerRecoveryEvidence,
};

/// What a decision's answer means for the drawer.
#[derive(Debug, Clone)]
pub enum DecisionView {
    /// The record changed: adopt it.
    Applied { task: Task, message: &'static str },
    /// The record changed, but its durability is unconfirmed: adopt it, say so
    /// and do not offer to repeat the decision.
    Held { task: Task, detail: String },
    /// Nothing was recorded.
    Refused(String),
}

pub fn classify(outcome: AcceptWorkerResultOutcome) -> DecisionView {
    match outcome {
        AcceptWorkerResultOutcome::Accepted { task } => DecisionView::Applied {
            task,
            message: "The commit was accepted. The Task continues into AI Review.",
        },
        AcceptWorkerResultOutcome::AlreadyApplied { task } => DecisionView::Applied {
            task,
            message: "This commit was already accepted.",
        },
        AcceptWorkerResultOutcome::DurabilityUnconfirmed { task, detail } => {
            DecisionView::Held { task, detail }
        }
        AcceptWorkerResultOutcome::Refused { reason } => DecisionView::Refused(reason),
    }
}

/// Facts SlashIt verified, as lines for the "Confirmed" list.
pub fn confirmed_lines(evidence: &WorkerRecoveryEvidence) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(run) = evidence.run {
        lines.push(format!("Interrupted run: {run}"));
    }
    if let Some(branch) = &evidence.branch {
        lines.push(format!("Branch: {branch}"));
    }
    if let Some(start) = &evidence.start_head {
        lines.push(format!("Branch HEAD before the run: {}", short(start)));
    }
    if let Some(tip) = &evidence.branch_tip {
        lines.push(format!("Branch tip now: {tip}"));
    }
    if let Some(checkout) = &evidence.checkout {
        lines.push(format!("Task Checkout: {}", checkout.path));
        match checkout.clean {
            Some(true) => lines.push("The checkout has no uncommitted changes.".to_string()),
            Some(false) => lines.push(format!(
                "The checkout has uncommitted changes: {}",
                checkout.dirty_paths.join(", ")
            )),
            None => {}
        }
    }
    for commit in &evidence.commits {
        lines.push(format!("Commit {}: {}", short(&commit.sha), commit.subject));
    }
    if evidence.commits_truncated {
        lines.push("More commits follow; only the newest are listed.".to_string());
    }
    lines
}

/// What SlashIt cannot prove, as lines for the "Uncertain" list. Always
/// includes the provider disclosure: acceptance does not stop a process.
pub fn uncertain_lines(evidence: &WorkerRecoveryEvidence) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(reason) = &evidence.reason {
        lines.push(reason.clone());
    }
    match &evidence.observed_commit {
        Some(observed) => lines.push(format!(
            "SlashIt noticed commit {} during the interruption but could not attribute it.",
            short(observed)
        )),
        None => {
            lines.push("SlashIt did not record which commit, if any, the run produced.".to_string())
        }
    }
    lines.push(
        "The interrupted provider process may have survived the application crash and may still \
         be changing files or have external effects. Accepting a commit does not prove it has \
         stopped."
            .to_string(),
    );
    lines
}

fn short(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

/// Which of several asynchronous reads is the newest. Only the newest answer
/// may replace what is shown.
#[derive(Debug, Clone, Copy, Default)]
pub struct Latest(u32);

impl Latest {
    pub fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_add(1);
        self.0
    }

    pub fn is_current(&self, ticket: u32) -> bool {
        self.0 == ticket
    }
}

/// The identity of the person's decision across confirmations, refreshes and
/// unknown outcomes.
///
/// A decision is bound to every fact in [`AcceptWorkerResultRequest`] (task,
/// project, repository, run, stage, branch, checkout, observed commit and the
/// exact tip). Evidence that differs in any of them is a different decision:
/// the confirmation closes and a new id is made when it is confirmed again.
/// Evidence that is identical keeps both. A decision whose outcome is unknown
/// is never replaced: it is sent again exactly as first sent, with its id,
/// until SlashIt gives a definite answer. New evidence is not proof that it
/// failed.
#[derive(Debug, Clone, Default)]
pub struct DecisionState {
    /// What the current confirmation was started for, without its id.
    basis: Option<AcceptWorkerResultRequest>,
    id: Option<Uuid>,
    /// A request sent but never answered.
    unknown: Option<AcceptWorkerResultRequest>,
}

impl DecisionState {
    fn basis_of(evidence: &WorkerRecoveryEvidence) -> Option<AcceptWorkerResultRequest> {
        AcceptWorkerResultRequest::for_evidence(evidence, Uuid::nil())
    }

    /// Newly read evidence. Answers whether an open confirmation must close
    /// because it no longer describes what is shown.
    pub fn evidence_changed(&mut self, shown: &WorkerRecoveryEvidence) -> bool {
        let Some(basis) = self.basis.as_ref() else {
            return false;
        };
        if Self::basis_of(shown).as_ref() == Some(basis) {
            return false;
        }
        self.basis = None;
        if self.unknown.is_none() {
            self.id = None;
        }
        true
    }

    /// The person opened the confirmation for `shown`: from now on, evidence
    /// that differs in any bound fact closes it, whether or not a request has
    /// been sent.
    pub fn opened(&mut self, shown: &WorkerRecoveryEvidence) {
        self.basis = Self::basis_of(shown);
    }

    /// The request to send for `shown`: the unanswered one if there is one,
    /// otherwise this confirmation's, with an id made on first use.
    pub fn request(
        &mut self,
        shown: &WorkerRecoveryEvidence,
        fresh_id: impl FnOnce() -> Uuid,
    ) -> Option<AcceptWorkerResultRequest> {
        if let Some(unanswered) = &self.unknown {
            return Some(unanswered.clone());
        }
        let basis = Self::basis_of(shown)?;
        let id = *self.id.get_or_insert_with(fresh_id);
        self.basis = Some(basis.clone());
        Some(AcceptWorkerResultRequest {
            decision_id: id,
            ..basis
        })
    }

    /// The request was sent and no answer came back.
    pub fn unanswered(&mut self, sent: AcceptWorkerResultRequest) {
        self.unknown = Some(sent);
    }

    /// SlashIt answered definitely (applied, held or refused).
    pub fn answered(&mut self) {
        *self = Self::default();
    }

    /// The person closed the confirmation. An unanswered request survives.
    pub fn cancelled(&mut self) {
        if self.unknown.is_none() {
            self.basis = None;
            self.id = None;
        }
    }

    pub fn unanswered_tip(&self) -> Option<&str> {
        self.unknown.as_ref().map(|r| r.tip.as_str())
    }
}

#[component]
pub fn WorkerRecoveryPanel(
    task_id: Uuid,
    #[prop(into)] task: Signal<Option<Task>>,
    apply_task: Callback<Task>,
) -> impl IntoView {
    let evidence = RwSignal::new(None::<Result<WorkerRecoveryEvidence, String>>);
    let confirming = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let refusal = RwSignal::new(None::<String>);
    let held = RwSignal::new(None::<String>);
    // One id per confirmation of the same evidence, kept across clicks and
    // unknown outcomes so a repeated request is the same decision.
    let decision = StoredValue::new(DecisionState::default());
    let unanswered_tip = RwSignal::new(None::<String>);
    let asked = StoredValue::new(Latest::default());

    let reload = move || {
        let ticket = asked.try_update_value(|latest| latest.next());
        spawn_local(async move {
            let answer = get_worker_recovery_evidence(task_id.to_string()).await;
            let current = ticket
                .zip(asked.try_get_value())
                .is_some_and(|(ticket, latest)| latest.is_current(ticket));
            if !current {
                return;
            }
            if let Ok(shown) = &answer {
                let close = decision
                    .try_update_value(|d| d.evidence_changed(shown))
                    .unwrap_or(false);
                if close {
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
    // Git is only asked about a Task that is waiting for this decision.
    Effect::new(move |_| {
        task.track();
        if recovering.get() {
            reload();
        }
    });

    let confirm = move |_| {
        if busy.get_untracked() {
            return;
        }
        let Some(Ok(shown)) = evidence.get_untracked() else {
            return;
        };
        let Some(request) = decision
            .try_update_value(|d| d.request(&shown, Uuid::new_v4))
            .flatten()
        else {
            refusal.set(Some(
                "The evidence is incomplete, so nothing can be accepted.".to_string(),
            ));
            return;
        };
        busy.set(true);
        refusal.set(None);
        let sent = request.clone();
        spawn_local(async move {
            match accept_worker_result(request).await {
                Ok(outcome) => match classify(outcome) {
                    DecisionView::Applied { task, message } => {
                        decision.try_update_value(|d| d.answered());
                        unanswered_tip.try_set(None);
                        apply_task.try_run(task);
                        crate::components::toast::success(message.to_string());
                        confirming.try_set(false);
                    }
                    DecisionView::Held { task, detail } => {
                        decision.try_update_value(|d| d.answered());
                        unanswered_tip.try_set(None);
                        apply_task.try_run(task);
                        held.try_set(Some(detail));
                        confirming.try_set(false);
                    }
                    DecisionView::Refused(reason) => {
                        decision.try_update_value(|d| d.answered());
                        unanswered_tip.try_set(None);
                        refusal.try_set(Some(reason));
                        // A definite refusal ends this confirmation; the next
                        // one is made against the evidence read below.
                        confirming.try_set(false);
                        reload();
                    }
                },
                Err(e) => {
                    // The answer is unknown: this exact request, with its id,
                    // is what any further attempt sends, even if the evidence
                    // shown changes meanwhile, so it cannot apply twice.
                    decision.try_update_value(|d| d.unanswered(sent));
                    unanswered_tip.try_set(
                        decision
                            .try_with_value(|d| d.unanswered_tip().map(str::to_string))
                            .flatten(),
                    );
                    refusal.try_set(Some(format!(
                        "The decision may or may not have been recorded ({e}). Check the Task, \
                         then confirm again if it is still waiting."
                    )));
                    reload();
                }
            }
            busy.try_set(false);
        });
    };

    view! {
        <Show when=move || recovering.get() || held.get().is_some()>
            <section data-testid="worker-recovery-panel" class="rounded-lg border border-white/10 bg-white/[0.03] p-3 space-y-3">
                <h3 class="text-xs font-semibold uppercase tracking-wide text-white/60">"Interrupted Worker"</h3>
                <p data-testid="worker-recovery-explanation" class="text-xs text-white/50">
                    "The coding Worker was interrupted, so Start and Retry are blocked: running it again could repeat work or its external effects. If the preserved branch already holds the result you want, you can accept that exact commit. It then continues into AI Review. No Worker is started."
                </p>
                {move || held.get().map(|detail| view! {
                    <p data-testid="worker-recovery-held" role="alert" class="text-xs text-amber-200 whitespace-pre-wrap break-words">
                        {format!("The decision is on the board, but SlashIt could not confirm it was saved to disk: {detail}. Do not repeat it. SlashIt re-checks the commit before AI Review begins.")}
                    </p>
                })}
                <Show when=move || recovering.get()>
                    {move || match evidence.get() {
                        None => view! { <p class="text-xs text-white/40">"Reading the preserved evidence…"</p> }.into_any(),
                        Some(Err(e)) => view! {
                            <p data-testid="worker-recovery-evidence-error" role="alert" class="text-xs text-red-300 break-words">
                                {format!("The evidence could not be read: {e}")}
                            </p>
                        }.into_any(),
                        Some(Ok(shown)) => {
                            let eligible = shown.eligible && shown.branch_tip.is_some();
                            let tip = shown.branch_tip.clone().unwrap_or_default();
                            let opened_for = StoredValue::new(shown.clone());
                            view! {
                                <div class="space-y-2">
                                    <div>
                                        <p class="text-xs font-semibold text-emerald-300">"Confirmed"</p>
                                        <ul data-testid="worker-recovery-confirmed" class="list-disc pl-5 space-y-0.5 text-xs text-white/60">
                                            {confirmed_lines(&shown).into_iter().map(|l| view! { <li class="break-words">{l}</li> }).collect_view()}
                                        </ul>
                                    </div>
                                    <div>
                                        <p class="text-xs font-semibold text-amber-300">"Uncertain"</p>
                                        <ul data-testid="worker-recovery-uncertain" class="list-disc pl-5 space-y-0.5 text-xs text-white/60">
                                            {uncertain_lines(&shown).into_iter().map(|l| view! { <li class="break-words">{l}</li> }).collect_view()}
                                        </ul>
                                    </div>
                                    {(!shown.refusals.is_empty()).then(|| view! {
                                        <div>
                                            <p class="text-xs font-semibold text-red-300">"Cannot be accepted yet"</p>
                                            <ul data-testid="worker-recovery-refusals" class="list-disc pl-5 space-y-0.5 text-xs text-red-200">
                                                {shown.refusals.iter().map(|l| view! { <li class="break-words">{l.clone()}</li> }).collect_view()}
                                            </ul>
                                        </div>
                                    })}
                                    {move || refusal.get().map(|reason| view! {
                                        <p data-testid="worker-recovery-error" role="alert" class="text-xs text-red-300 whitespace-pre-wrap break-words">{reason}</p>
                                    })}
                                    <Show when=move || !confirming.get()>
                                        <button
                                            data-testid="worker-recovery-review"
                                            class="px-3 py-1.5 rounded-lg text-sm bg-emerald-500/15 text-emerald-200 hover:bg-emerald-500/25 disabled:opacity-50"
                                            disabled=!eligible
                                            on:click=move |_| {
                                                refusal.set(None);
                                                decision.update_value(|d| d.opened(&opened_for.get_value()));
                                                confirming.set(true);
                                            }
                                        >
                                            "Accept this commit…"
                                        </button>
                                    </Show>
                                    <Show when=move || confirming.get()>
                                        <div data-testid="worker-recovery-confirm" class="rounded-md border border-white/10 bg-black/20 p-2 space-y-2">
                                            <p class="text-xs font-semibold text-white/70">"Accept this exact commit as the Worker's result?"</p>
                                            <p data-testid="worker-recovery-confirm-commit" class="text-xs font-mono break-all text-white/80">
                                                {let tip = tip.clone(); move || unanswered_tip.get().unwrap_or_else(|| tip.clone())}
                                            </p>
                                            <Show when=move || unanswered_tip.get().is_some()>
                                                <p data-testid="worker-recovery-unanswered" class="text-xs text-amber-200">"An earlier decision to accept this commit may or may not have been recorded. Sending it again is safe: it cannot apply twice."</p>
                                            </Show>
                                            <p class="text-xs text-white/50">"The Task moves to AI Review with this commit. Nothing is reset, cleaned or restarted. A provider process from the interrupted run may still be running."</p>
                                            <div class="flex gap-2">
                                                <button
                                                    data-testid="worker-recovery-accept"
                                                    class="px-3 py-1.5 rounded-lg text-sm font-medium bg-emerald-500/25 text-emerald-100 hover:bg-emerald-500/35 disabled:opacity-50"
                                                    disabled=move || busy.get()
                                                    on:click=confirm
                                                >
                                                    {move || if busy.get() { "Accepting…" } else { "Accept and continue to AI Review" }}
                                                </button>
                                                <button
                                                    data-testid="worker-recovery-cancel"
                                                    class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/60 hover:bg-white/10"
                                                    disabled=move || busy.get()
                                                    on:click=move |_| {
                                                        decision.update_value(|d| d.cancelled());
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
                </Show>
            </section>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::worker_recovery_service::{CheckoutEvidence, EvidenceCommit};

    fn evidence() -> WorkerRecoveryEvidence {
        WorkerRecoveryEvidence {
            task_id: Uuid::from_u128(1),
            project_id: Uuid::from_u128(2),
            repository_id: Some(Uuid::from_u128(3)),
            stage: "recovery_required".into(),
            reason: Some("The Worker start was recorded but its outcome was not.".into()),
            run: Some(2),
            branch: Some("task/one".into()),
            start_head: Some("a".repeat(40)),
            observed_commit: None,
            branch_tip: Some("b".repeat(40)),
            commits: vec![EvidenceCommit {
                sha: "b".repeat(40),
                subject: "Add the feature".into(),
            }],
            commits_truncated: false,
            checkout: Some(CheckoutEvidence {
                path: "/work/one".into(),
                exists: true,
                registered: true,
                head: Some("b".repeat(40)),
                clean: Some(true),
                dirty_paths: Vec::new(),
            }),
            active_execution: false,
            eligible: true,
            refusals: Vec::new(),
        }
    }

    #[test]
    fn confirmed_and_uncertain_facts_are_listed_separately() {
        let shown = evidence();
        let confirmed = confirmed_lines(&shown).join("\n");
        assert!(
            confirmed.contains(&"b".repeat(40)),
            "the exact tip is shown whole"
        );
        assert!(confirmed.contains("Add the feature"));
        assert!(confirmed.contains("no uncommitted changes"));
        let uncertain = uncertain_lines(&shown).join("\n");
        assert!(uncertain.contains("may have survived the application crash"));
        assert!(uncertain.contains("did not record which commit"));
        assert!(
            !confirmed.contains("survived"),
            "uncertainty never appears as confirmed"
        );
    }

    #[test]
    fn an_observed_commit_is_described_as_unattributed() {
        let mut shown = evidence();
        shown.observed_commit = Some("c".repeat(40));
        let uncertain = uncertain_lines(&shown).join("\n");
        assert!(uncertain.contains("could not attribute"), "{uncertain}");
    }

    #[test]
    fn a_dirty_checkout_names_its_paths() {
        let mut shown = evidence();
        let checkout = shown.checkout.as_mut().unwrap();
        checkout.clean = Some(false);
        checkout.dirty_paths = vec!["?? scratch.txt".into()];
        assert!(confirmed_lines(&shown)
            .join("\n")
            .contains("?? scratch.txt"));
    }

    fn task() -> Task {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "project_id": Uuid::new_v4(),
            "title": "t",
            "description": null,
            "status": "ai_review",
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
            "phase": "qa_review",
            "phase_progress": 80,
            "overall_progress": 80,
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
        .expect("a task decodes")
    }

    #[test]
    fn each_answer_maps_to_exactly_one_drawer_behavior() {
        assert!(matches!(
            classify(AcceptWorkerResultOutcome::Accepted { task: task() }),
            DecisionView::Applied { .. }
        ));
        assert!(matches!(
            classify(AcceptWorkerResultOutcome::AlreadyApplied { task: task() }),
            DecisionView::Applied { .. }
        ));
        assert!(matches!(
            classify(AcceptWorkerResultOutcome::DurabilityUnconfirmed {
                task: task(),
                detail: "x".into()
            }),
            DecisionView::Held { .. }
        ));
        assert!(matches!(
            classify(AcceptWorkerResultOutcome::Refused { reason: "stale".into() }),
            DecisionView::Refused(reason) if reason == "stale"
        ));
    }

    #[test]
    fn unchanged_evidence_and_repeated_clicks_keep_one_decision() {
        let mut state = DecisionState::default();
        let shown = evidence();
        let first = state.request(&shown, || Uuid::from_u128(7)).unwrap();
        let again = state.request(&shown, || Uuid::from_u128(8)).unwrap();
        assert_eq!(first, again);
        assert_eq!(first.decision_id, Uuid::from_u128(7));
        assert!(
            !state.evidence_changed(&evidence()),
            "a refresh of the same facts"
        );
    }

    #[test]
    fn evidence_that_differs_in_any_bound_fact_closes_the_confirmation_with_a_new_id() {
        type Change = fn(&mut WorkerRecoveryEvidence);
        let changes: [(&str, Change); 6] = [
            ("tip", |e| e.branch_tip = Some("c".repeat(40))),
            ("observed", |e| e.observed_commit = Some("d".repeat(40))),
            ("run", |e| e.run = Some(3)),
            ("branch", |e| e.branch = Some("task/two".into())),
            ("repository", |e| {
                e.repository_id = Some(Uuid::from_u128(99))
            }),
            ("checkout", |e| {
                e.checkout.as_mut().unwrap().path = "/work/two".into()
            }),
        ];
        for (field, change) in changes {
            let mut state = DecisionState::default();
            let first = state.request(&evidence(), || Uuid::from_u128(7)).unwrap();
            let mut moved = evidence();
            change(&mut moved);
            assert!(state.evidence_changed(&moved), "{field}");
            let second = state.request(&moved, || Uuid::from_u128(8)).unwrap();
            assert_ne!(first.decision_id, second.decision_id, "{field}");
        }
    }

    #[test]
    fn facts_that_are_not_part_of_the_decision_do_not_reset_it() {
        let mut state = DecisionState::default();
        state.request(&evidence(), Uuid::new_v4).unwrap();
        let mut shown = evidence();
        shown.commits_truncated = true;
        shown.refusals = vec!["unrelated wording".into()];
        assert!(!state.evidence_changed(&shown));
    }

    #[test]
    fn an_unanswered_decision_is_sent_again_exactly_even_after_the_evidence_moves() {
        let mut state = DecisionState::default();
        let sent = state.request(&evidence(), || Uuid::from_u128(7)).unwrap();
        state.unanswered(sent.clone());

        let mut moved = evidence();
        moved.branch_tip = Some("c".repeat(40));
        assert!(state.evidence_changed(&moved), "the confirmation closes");
        assert_eq!(state.unanswered_tip(), Some("b".repeat(40).as_str()));
        let retry = state.request(&moved, || Uuid::from_u128(8)).unwrap();
        assert_eq!(retry, sent, "same id and same facts, not the new evidence");

        state.answered();
        let fresh = state.request(&moved, || Uuid::from_u128(8)).unwrap();
        assert_eq!(fresh.decision_id, Uuid::from_u128(8));
        assert_eq!(fresh.tip, "c".repeat(40));
    }

    #[test]
    fn cancelling_forgets_a_decision_but_not_an_unanswered_one() {
        let mut state = DecisionState::default();
        let first = state.request(&evidence(), || Uuid::from_u128(7)).unwrap();
        state.cancelled();
        let second = state.request(&evidence(), || Uuid::from_u128(8)).unwrap();
        assert_ne!(first.decision_id, second.decision_id);

        state.unanswered(second.clone());
        state.cancelled();
        assert_eq!(state.request(&evidence(), Uuid::new_v4).unwrap(), second);
    }

    #[test]
    fn a_confirmation_opened_but_not_yet_sent_closes_when_the_evidence_moves() {
        let mut state = DecisionState::default();
        state.opened(&evidence());
        assert!(!state.evidence_changed(&evidence()));
        let mut moved = evidence();
        moved.branch_tip = Some("c".repeat(40));
        assert!(state.evidence_changed(&moved));
        assert!(
            !state.evidence_changed(&moved),
            "closed once, not repeatedly"
        );
    }

    #[test]
    fn only_the_newest_read_may_replace_the_evidence() {
        let mut latest = Latest::default();
        let older = latest.next();
        let newer = latest.next();
        assert!(!latest.is_current(older));
        assert!(latest.is_current(newer));
    }
}
