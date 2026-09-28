//! The Human Review decision in the task drawer: the AI review summary, the
//! review history, and the actions a person takes on the changes (approve,
//! with or without a pull request, or request changes).
//!
//! What is shown is derived from the task record and the backend's answer
//! about pull request availability; nothing here decides on its own that
//! something succeeded. A command's own answer is applied to the board as
//! the new record, and its failure is shown in place, not only as a toast.

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::models::{review_findings, HumanReviewDecision, QaSignoff, QaStatus, Task, TaskStatus};
use crate::services::human_review_service::{
    approve_task, create_approved_task_pr, get_pr_availability, request_task_changes,
    PrAvailability, PrDelivery,
};

/// The most findings listed before the rest are summarised as a count.
const VISIBLE_FINDINGS: usize = 8;

/// Where the pull request for approved changes stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Not known yet: availability is still being asked.
    Checking,
    /// A pull request is recorded on the task.
    Created { url: Option<String> },
    /// The last attempt failed, and no pull request is recorded.
    Failed { reason: String },
    /// SlashIt cannot open one for this project.
    Unavailable { reason: String },
    /// It can, and has not tried yet.
    NotAttempted,
}

impl Delivery {
    pub fn for_task(task: &Task, availability: Option<&PrAvailability>) -> Self {
        let recorded_pr = task
            .pr_url
            .clone()
            .or_else(|| task.external_refs.iter().find(|r| r.is_pr()).and_then(|r| r.url().map(str::to_string)));
        if recorded_pr.is_some() || task.status == TaskStatus::PrCreated {
            return Self::Created { url: recorded_pr };
        }
        if let Some(reason) = task.human_review.pr_error.clone() {
            return Self::Failed { reason };
        }
        match availability {
            None => Self::Checking,
            Some(PrAvailability::Unavailable { reason }) => Self::Unavailable { reason: reason.clone() },
            Some(PrAvailability::Available) => Self::NotAttempted,
        }
    }
}

impl Delivery {
    /// This state, unless the last delivery answer says more: a pull request
    /// the record shows is final, and otherwise the answer is what just
    /// happened.
    pub fn or_answer(self, answer: Option<&PrDelivery>) -> Self {
        match (self, answer) {
            (created @ Self::Created { .. }, _) => created,
            (_, Some(PrDelivery::Failed { reason })) => Self::Failed { reason: reason.clone() },
            (_, Some(PrDelivery::Unavailable { reason })) => Self::Unavailable { reason: reason.clone() },
            (_, Some(PrDelivery::Created { url })) => Self::Created { url: Some(url.clone()) },
            (state, None) => state,
        }
    }
}

/// How the AI review's aggregate outcome reads, without claiming anything
/// about a single finding that the aggregate does not prove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiReviewSummary {
    pub verdict: &'static str,
    /// The heading over the findings, if there are any.
    pub findings_heading: Option<&'static str>,
    pub findings: Vec<String>,
    /// Whether the findings still need someone's attention.
    pub outstanding: bool,
}

impl AiReviewSummary {
    pub fn of(signoff: &QaSignoff) -> Self {
        let findings = review_findings(&signoff.issues_found);
        let (verdict, heading, outstanding) = match signoff.status {
            QaStatus::Approved => ("AI review approved the changes.", None, false),
            QaStatus::FixesApplied => (
                "AI review found issues and a fix run changed the code to address them. Check \
                 the Changes tab to confirm each fix.",
                Some("Fixed during AI review"),
                false,
            ),
            QaStatus::Rejected => (
                "AI review did not approve the changes.",
                Some("Needs your attention"),
                true,
            ),
        };
        Self {
            verdict,
            findings_heading: heading.filter(|_| !findings.is_empty()),
            findings,
            outstanding,
        }
    }
}

/// A short label for the card.
pub fn ai_review_label(status: &QaStatus) -> &'static str {
    match status {
        QaStatus::Approved => "AI approved",
        QaStatus::FixesApplied => "AI fixes applied",
        QaStatus::Rejected => "AI review: attention",
    }
}

/// The AI review of the changes under review, if one ran.
#[component]
pub fn AiReviewSection(signoff: QaSignoff) -> impl IntoView {
    let summary = AiReviewSummary::of(&signoff);
    let hidden = summary.findings.len().saturating_sub(VISIBLE_FINDINGS);
    let tone = if summary.outstanding { "text-amber-200/90" } else { "text-white/60" };
    view! {
        <div data-testid="task-drawer-review" class="space-y-1.5">
            <p data-testid="task-drawer-review-verdict" class=format!("text-sm {tone}")>{summary.verdict}</p>
            {summary.findings_heading.map(|heading| view! {
                <div class="space-y-1">
                    <h4
                        data-testid="task-drawer-review-heading"
                        class=if summary.outstanding {
                            "text-[11px] font-semibold uppercase tracking-wide text-amber-300"
                        } else {
                            "text-[11px] font-semibold uppercase tracking-wide text-emerald-300/80"
                        }
                    >
                        {heading}
                    </h4>
                    <ul class="list-disc pl-5 text-xs text-white/55 space-y-0.5">
                        {summary.findings.iter().take(VISIBLE_FINDINGS).map(|finding| view! {
                            <li data-testid="task-drawer-review-finding" class="break-words">{finding.clone()}</li>
                        }).collect_view()}
                    </ul>
                    {(hidden > 0).then(|| view! {
                        <p class="text-xs text-white/40">{format!("and {hidden} more")}</p>
                    })}
                </div>
            })}
        </div>
    }
}

/// The decision section for a task in Human Review, and what became of an
/// approval once the task has moved on to its pull request.
#[component]
pub fn HumanReviewPanel(
    task_id: Uuid,
    #[prop(into)] task: Signal<Option<Task>>,
    /// Apply a task record a command returned.
    apply_task: Callback<Task>,
) -> impl IntoView {
    let availability = RwSignal::new(None::<PrAvailability>);
    spawn_local(async move {
        let answer = get_pr_availability(task_id.to_string())
            .await
            .unwrap_or_else(|e| PrAvailability::Unavailable {
                reason: format!("SlashIt could not tell whether this project can take a pull request: {e}"),
            });
        availability.try_set(Some(answer));
    });

    // What the last delivery attempt in this drawer answered. Preferred over
    // what the record says until the record agrees, so an answer is never
    // lost to a write that did not land or to an availability read that is
    // out of date.
    let last_delivery = RwSignal::new(None::<PrDelivery>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let composing = RwSignal::new(false);
    let feedback = RwSignal::new(String::new());

    let apply_outcome = move |task: Task, pr: Option<PrDelivery>| {
        apply_task.try_run(task);
        if let Some(PrDelivery::Created { url }) = &pr {
            crate::components::toast::success(format!("Pull request created: {url}"));
        }
        if let Some(PrDelivery::Unavailable { reason }) = &pr {
            availability.try_set(Some(PrAvailability::Unavailable { reason: reason.clone() }));
        }
        last_delivery.try_set(pr);
    };

    let approve = move |create_pr: bool| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match approve_task(task_id.to_string(), create_pr).await {
                Ok(outcome) => apply_outcome(outcome.task, outcome.pr),
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            busy.try_set(false);
        });
    };

    let retry_pr = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match create_approved_task_pr(task_id.to_string()).await {
                Ok(outcome) => apply_outcome(outcome.task, outcome.pr),
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            busy.try_set(false);
        });
    };

    let send_back = move |_| {
        let text = feedback.get_untracked();
        if busy.get_untracked() || text.trim().is_empty() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match request_task_changes(task_id.to_string(), text).await {
                Ok(updated) => {
                    feedback.try_set(String::new());
                    composing.try_set(false);
                    apply_task.try_run(updated);
                    crate::components::toast::success(
                        "Feedback saved. The task is queued to run again.".to_string(),
                    );
                }
                // The feedback stays in the box, so nothing typed is lost.
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            busy.try_set(false);
        });
    };

    let decision = Memo::new(move |_| {
        task.with(|t| t.as_ref().and_then(|t| t.human_review.current_decision().map(|e| e.decision)))
    });
    let in_review = Memo::new(move |_| task.with(|t| t.as_ref().is_some_and(|t| t.status == TaskStatus::HumanReview)));
    let delivery = Memo::new(move |_| {
        task.with(|t| {
            t.as_ref().map(|t| {
                let recorded = availability.with(|a| Delivery::for_task(t, a.as_ref()));
                last_delivery.with(|last| recorded.or_answer(last.as_ref()))
            })
        })
    });

    view! {
        <section data-testid="human-review" class="rounded-lg border border-purple-500/20 bg-purple-500/[0.05] p-3 space-y-3">
            {move || task.with(|t| t.as_ref().map(|t| {
                let earlier: Vec<(u32, String)> = t
                    .human_review
                    .earlier_requests()
                    .into_iter()
                    .map(|e| (e.sequence, e.feedback.clone().unwrap_or_default()))
                    .collect();
                let iteration = earlier.len() + 1;
                view! {
                    <div class="space-y-2">
                        <div class="flex items-center gap-2">
                            <h3 class="text-xs font-semibold uppercase tracking-wide text-purple-300">"Human Review"</h3>
                            {(iteration > 1).then(|| view! {
                                <span data-testid="human-review-iteration" data-iteration=iteration.to_string() class="text-[11px] px-1.5 py-0.5 rounded bg-white/10 text-white/60">
                                    {format!("Review {iteration}, after requested changes")}
                                </span>
                            })}
                        </div>
                        {(!earlier.is_empty()).then(|| view! {
                            <div data-testid="human-review-history" class="space-y-1">
                                <p class="text-[11px] text-white/40">"Earlier feedback"</p>
                                <ol class="space-y-1">
                                    {earlier.into_iter().map(|(sequence, text)| {
                                        let title = text.clone();
                                        view! {
                                            <li data-testid="human-review-history-entry" class="text-xs text-white/60 border-l-2 border-white/10 pl-2 whitespace-pre-wrap break-words line-clamp-3" title=title>
                                                <span class="text-white/35">{format!("#{sequence} ")}</span>{text}
                                            </li>
                                        }
                                    }).collect_view()}
                                </ol>
                            </div>
                        })}
                    </div>
                }
            }))}

            // Undecided: the decision itself.
            <Show when=move || in_review.get() && decision.get().is_none()>
                <p class="text-xs text-white/50">"Decide on these changes. The diff is in the Changes tab."</p>
                <div class="flex flex-wrap gap-2">
                    {move || {
                        let available = availability.get();
                        let (label, create_pr) = match available {
                            None => ("Checking…", false),
                            Some(PrAvailability::Available) => ("Approve & Create PR", true),
                            Some(PrAvailability::Unavailable { .. }) => ("Approve", false),
                        };
                        let ready = available.is_some();
                        view! {
                            <button
                                data-testid="human-review-approve"
                                data-creates-pr=create_pr.to_string()
                                class="px-3 py-1.5 rounded-lg text-sm font-medium bg-emerald-500/20 text-emerald-200 hover:bg-emerald-500/30 disabled:opacity-50"
                                disabled=move || busy.get() || !ready
                                on:click=move |_| approve(create_pr)
                            >
                                {move || if busy.get() && !composing.get() { "Working…" } else { label }}
                            </button>
                        }
                    }}
                    <button
                        data-testid="human-review-request-changes"
                        class="px-3 py-1.5 rounded-lg text-sm font-medium bg-amber-500/15 text-amber-200 hover:bg-amber-500/25 disabled:opacity-50"
                        disabled=move || busy.get()
                        on:click=move |_| {
                            error.set(None);
                            composing.update(|c| *c = !*c);
                        }
                    >
                        "Request Changes"
                    </button>
                </div>
                <Show when=move || composing.get()>
                    <div data-testid="human-review-feedback-form" class="space-y-2">
                        <label class="block text-xs text-white/50" for="human-review-feedback">
                            "What should change? The next run gets this with the task, which stays as written."
                        </label>
                        <textarea
                            id="human-review-feedback"
                            data-testid="human-review-feedback"
                            rows="5"
                            class="w-full rounded-lg bg-black/40 border border-white/10 p-2 text-sm text-white/85 focus:outline-none focus:border-amber-400/50"
                            prop:value=move || feedback.get()
                            on:input=move |ev| feedback.set(event_target_value(&ev))
                        ></textarea>
                        <div class="flex gap-2">
                            <button
                                data-testid="human-review-send-feedback"
                                class="px-3 py-1.5 rounded-lg text-sm font-medium bg-amber-500/25 text-amber-100 hover:bg-amber-500/35 disabled:opacity-50"
                                disabled=move || busy.get() || feedback.with(|f| f.trim().is_empty())
                                on:click=send_back
                            >
                                {move || if busy.get() { "Sending…" } else { "Send back to the agent" }}
                            </button>
                            <button
                                data-testid="human-review-cancel-feedback"
                                class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/60 hover:bg-white/10"
                                disabled=move || busy.get()
                                on:click=move |_| composing.set(false)
                            >
                                "Cancel"
                            </button>
                        </div>
                    </div>
                </Show>
            </Show>

            // Changes were requested and the task has not run again yet.
            <Show when=move || in_review.get() && decision.get() == Some(HumanReviewDecision::ChangesRequested)>
                <p data-testid="human-review-changes-requested" class="text-sm text-amber-200/90">
                    "You requested changes, and the task has not run again yet. Move it to Queue to run it with your feedback."
                </p>
            </Show>

            // Approved: the decision, then delivery on its own line.
            <Show when=move || decision.get() == Some(HumanReviewDecision::Approved)>
                <p data-testid="human-review-approved" class="flex items-center gap-2 text-sm font-medium text-emerald-300">
                    <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                    </svg>
                    "Approved"
                </p>
                {move || delivery.get().map(|d| match d {
                    Delivery::Checking => view! {
                        <p data-testid="human-review-delivery" data-delivery="checking" class="text-xs text-white/40">"Checking whether a pull request can be created…"</p>
                    }.into_any(),
                    Delivery::Created { url } => view! {
                        <div data-testid="human-review-delivery" data-delivery="created" class="text-xs text-white/60 space-y-1">
                            <p>"Pull request created. Nothing is merged until it is merged on GitHub."</p>
                            {url.map(|url| {
                                let open = url.clone();
                                view! {
                                    <button
                                        data-testid="human-review-pr-link"
                                        class="text-cyan-300 hover:underline break-all text-left"
                                        on:click=move |_| {
                                            let open = open.clone();
                                            spawn_local(async move {
                                                if let Err(e) = crate::services::open_url_external(open).await {
                                                    crate::components::toast::error(format!("Could not open the pull request: {e}"));
                                                }
                                            });
                                        }
                                    >
                                        {url}
                                    </button>
                                }
                            })}
                        </div>
                    }.into_any(),
                    Delivery::Failed { reason } => view! {
                        <div data-testid="human-review-delivery" data-delivery="failed" class="rounded-md border border-red-500/20 bg-red-500/[0.06] p-2 space-y-2">
                            <p class="text-xs font-semibold text-red-300">"The pull request was not created"</p>
                            <p data-testid="human-review-pr-error" class="text-xs text-red-200/90 whitespace-pre-wrap break-words">{reason}</p>
                            <p class="text-xs text-white/45">"Your approval is kept. Nothing was merged."</p>
                            <button
                                data-testid="human-review-retry-pr"
                                class="px-3 py-1.5 rounded-lg text-sm bg-blue-500/15 text-blue-200 hover:bg-blue-500/25 disabled:opacity-50"
                                disabled=move || busy.get()
                                on:click=retry_pr
                            >
                                {move || if busy.get() { "Creating…" } else { "Retry Create PR" }}
                            </button>
                        </div>
                    }.into_any(),
                    Delivery::Unavailable { reason } => view! {
                        <div data-testid="human-review-delivery" data-delivery="unavailable" class="text-xs text-white/55 space-y-1">
                            <p>"SlashIt cannot create a GitHub pull request for this project."</p>
                            <p data-testid="human-review-pr-unavailable" class="text-white/45">{reason}</p>
                            <p class="text-white/45">"Nothing was merged. The task stays in Human Review with its work on its branch."</p>
                        </div>
                    }.into_any(),
                    Delivery::NotAttempted => view! {
                        <div data-testid="human-review-delivery" data-delivery="not_attempted" class="text-xs text-white/55 space-y-2">
                            <p>"No pull request has been created yet."</p>
                            <button
                                data-testid="human-review-retry-pr"
                                class="px-3 py-1.5 rounded-lg text-sm bg-blue-500/15 text-blue-200 hover:bg-blue-500/25 disabled:opacity-50"
                                disabled=move || busy.get()
                                on:click=retry_pr
                            >
                                {move || if busy.get() { "Creating…" } else { "Create PR" }}
                            </button>
                        </div>
                    }.into_any(),
                })}
            </Show>

            {move || error.get().map(|e| view! {
                <p data-testid="human-review-error" class="text-xs text-red-300 whitespace-pre-wrap break-words">{e}</p>
            })}
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::HumanReviewRecord;

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

    fn signoff(status: QaStatus, issues: &[&str]) -> QaSignoff {
        QaSignoff {
            status,
            issues_found: issues.iter().map(|s| s.to_string()).collect(),
            timestamp: chrono::Utc::now(),
            session_id: Uuid::new_v4(),
        }
    }

    #[test]
    fn a_task_from_before_review_records_existed_still_loads() {
        let t = task(TaskStatus::HumanReview);
        assert_eq!(t.human_review, HumanReviewRecord::default());
    }

    #[test]
    fn delivery_prefers_what_is_recorded_over_what_is_possible() {
        let available = PrAvailability::Available;
        let unavailable = PrAvailability::Unavailable { reason: "no origin".into() };

        let mut t = task(TaskStatus::HumanReview);
        assert_eq!(Delivery::for_task(&t, None), Delivery::Checking);
        assert_eq!(Delivery::for_task(&t, Some(&available)), Delivery::NotAttempted);
        assert_eq!(
            Delivery::for_task(&t, Some(&unavailable)),
            Delivery::Unavailable { reason: "no origin".into() }
        );

        t.human_review.pr_error = Some("gh: not logged in".into());
        assert_eq!(
            Delivery::for_task(&t, Some(&available)),
            Delivery::Failed { reason: "gh: not logged in".into() }
        );

        t.pr_url = Some("https://github.com/o/r/pull/1".into());
        t.status = TaskStatus::PrCreated;
        assert_eq!(
            Delivery::for_task(&t, None),
            Delivery::Created { url: Some("https://github.com/o/r/pull/1".into()) }
        );
    }

    #[test]
    fn the_last_answer_is_shown_until_the_record_catches_up() {
        let failed = PrDelivery::Failed { reason: "push rejected".into() };
        assert_eq!(
            Delivery::NotAttempted.or_answer(Some(&failed)),
            Delivery::Failed { reason: "push rejected".into() },
            "a failure whose write did not land is still shown"
        );
        let unavailable = PrDelivery::Unavailable { reason: "origin moved".into() };
        assert_eq!(
            Delivery::Failed { reason: "old".into() }.or_answer(Some(&unavailable)),
            Delivery::Unavailable { reason: "origin moved".into() }
        );
        let created = Delivery::Created { url: Some("u".into()) };
        assert_eq!(created.clone().or_answer(Some(&failed)), created, "a recorded PR wins");
        assert_eq!(Delivery::Checking.or_answer(None), Delivery::Checking);
    }

    #[test]
    fn the_ai_summary_claims_only_what_its_status_proves() {
        let fixed = AiReviewSummary::of(&signoff(
            QaStatus::FixesApplied,
            &["- ISSUE: [high] a.rs:1 - bug", "ISSUE: [high] a.rs:1 - bug"],
        ));
        assert_eq!(fixed.findings_heading, Some("Fixed during AI review"));
        assert_eq!(fixed.findings, vec!["[high] a.rs:1 - bug"]);
        assert!(!fixed.outstanding);
        assert!(!fixed.verdict.contains("FixesApplied"));

        let rejected = AiReviewSummary::of(&signoff(QaStatus::Rejected, &["AI review failed: timeout"]));
        assert!(rejected.outstanding);
        assert_eq!(rejected.findings_heading, Some("Needs your attention"));

        let approved = AiReviewSummary::of(&signoff(QaStatus::Approved, &[]));
        assert_eq!(approved.verdict, "AI review approved the changes.");
        assert_eq!(approved.findings_heading, None);
    }

    #[test]
    fn card_labels_are_words_not_enum_names() {
        for status in [QaStatus::Approved, QaStatus::FixesApplied, QaStatus::Rejected] {
            let label = ai_review_label(&status);
            assert!(!label.contains("FixesApplied") && !label.contains("Rejected"), "{label}");
        }
    }
}
