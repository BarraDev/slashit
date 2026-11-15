// SPDX-License-Identifier: MPL-2.0
//! The Human Review decision in the task drawer: the AI review summary, the
//! review history, and the actions a person takes on the changes (approve,
//! with or without a pull request, complete them locally where no pull
//! request can be opened, or request changes).
//!
//! What is shown is derived from the task record and the backend's answer
//! about pull request availability; nothing here decides on its own that
//! something succeeded. A command's own answer is applied to the board as
//! the new record, and its failure is shown in place, not only as a toast.

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;

use crate::components::attention::Deliveries;
use crate::components::close_without_merge_dialog::close_consequences;
use crate::components::local_integration::{is_integrated, LocalIntegrationPanel};
use crate::models::{review_findings, HumanReviewDecision, QaSignoff, QaStatus, Task, TaskStatus};
use crate::services::human_review_service::{
    approve_task, approve_task_again, complete_task_locally, create_approved_task_pr,
    get_approval_status, get_pr_availability, request_task_changes, ApprovalCoverage,
    ApprovalStatus, PrAvailability, PrDelivery,
};

/// The most findings listed before the rest are summarised as a count.
const VISIBLE_FINDINGS: usize = 8;

/// How often the panel asks again whether the approval still covers the
/// branch. A branch that moves outside SlashIt changes no task record, so
/// without this poll a stale approval would keep looking current until the
/// person tried something.
const COVERAGE_POLL_MS: i32 = 5_000;

/// How long one coverage read may take before the panel stops trusting the
/// last answer. The backend reads refs locally, and the storage probes bound
/// their git questions at ten seconds too. The backend's own subprocess calls
/// have no timeout, so a read that outlasts this bound may still be running.
const COVERAGE_READ_MS: u32 = 10_000;

/// Shown in place of the last answer once a read outlasts its bound.
const COVERAGE_TIMED_OUT: &str =
    "The branch's current commit did not answer in time, so it cannot be approved yet.";

/// The window property the acceptance harness sets to hold this cadence for
/// one step of its journey. It proves that approving a commit the branch has
/// left is refused, which needs the branch to move after the panel read it
/// and before the person clicks; without a hold, a poll landing between the
/// move and the click would learn the move first and approve it, so whether
/// the refusal was proven would be a race. While it is set the panel asks
/// nothing on its own and publishes no answer, so what a person is shown is
/// frozen. Production never sets it.
///
/// It lives on the page, not in localStorage: a value written to storage would
/// survive a restart and freeze the approval display in a profile that is no
/// longer under test. A page load clears it.
///
/// Only a build made with `SLASHIT_ACCEPTANCE_BUILD` set reads it. That build
/// is `scripts/build-acceptance-app.sh`. Any other build reads the flag as a
/// compile-time `None`, so the seam is inert there and no script in the page
/// can hold the cadence.
const COVERAGE_POLL_HOLD: &str = "__slashitCoveragePollHold";

/// Whether the coverage cadence is being held by the acceptance harness. Always
/// false outside an acceptance build, because the flag is decided at compile time.
fn coverage_poll_held() -> bool {
    if option_env!("SLASHIT_ACCEPTANCE_BUILD").is_none() {
        return false;
    }
    web_sys::window()
        .and_then(|w| js_sys::Reflect::get(w.as_ref(), &COVERAGE_POLL_HOLD.into()).ok())
        .is_some_and(|held| held.is_truthy())
}

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
    /// A person finished the task without one. The work is on its branch,
    /// which was not merged.
    CompletedLocally { branch: Option<String> },
    /// It can, and has not tried yet.
    NotAttempted,
}

impl Delivery {
    pub fn for_task(task: &Task, availability: Option<&PrAvailability>) -> Self {
        if task.completed_locally() {
            return Self::CompletedLocally { branch: task.branch_name.clone() };
        }
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
            (done @ Self::CompletedLocally { .. }, _) => done,
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

/// What the record says about a Human Review that has no AI verdict, in one
/// sentence. `None` when there is a verdict, or the record names no skip or
/// failure. The reason is shown as the record kept it.
pub fn no_verdict_summary(
    signoff: Option<&QaSignoff>,
    activity: &[slashit_activity::Entry],
) -> Option<String> {
    if signoff.is_some() {
        return None;
    }
    match slashit_activity::no_verdict(activity)? {
        slashit_activity::NoVerdict::Skipped(reason) => {
            Some(format!("AI review was skipped: {}", as_sentence(reason)))
        }
        slashit_activity::NoVerdict::Failed(reason) => {
            Some(format!("AI review failed: {}", as_sentence(reason)))
        }
    }
}

/// A short card label for a Human Review with no AI verdict.
pub fn no_verdict_label(activity: &[slashit_activity::Entry]) -> Option<&'static str> {
    match slashit_activity::no_verdict(activity)? {
        slashit_activity::NoVerdict::Skipped(_) => Some("AI review skipped"),
        slashit_activity::NoVerdict::Failed(_) => Some("AI review failed"),
    }
}

/// The reason with a closing full stop, so it reads as the end of a sentence
/// whichever way the record phrased it.
fn as_sentence(text: &str) -> String {
    let text = text.trim();
    if text.ends_with(['.', '!', '?']) {
        text.to_string()
    } else {
        format!("{text}.")
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

/// Why an approval that no longer covers the branch cannot be renewed from
/// here, from what the task record shows; `None` when it may be offered.
///
/// This only hides a button that would be refused. The backend decides, and
/// also refuses while a delivery is in flight, which the record does not show.
fn renewal_blocker(task: &Task) -> Option<&'static str> {
    if task.status != TaskStatus::HumanReview {
        return Some("This task is no longer in review, so its approval cannot be renewed here.");
    }
    if task.pr_url.is_some() || task.external_refs.iter().any(|r| r.is_pr()) {
        return Some(
            "This task has a pull request, so its approval is not renewed. The pull request holds the branch that was delivered.",
        );
    }
    if task.integrating_current_approval() {
        return Some(
            "A local integration of this task was started with the approved commit. Resume or discard it first.",
        );
    }
    None
}

/// Whether a delivery answer means a pull request was actually tried, so the
/// drawer numbers it as an attempt.
fn tried_a_pull_request(answer: Option<&PrDelivery>) -> bool {
    matches!(answer, Some(PrDelivery::Created { .. } | PrDelivery::Failed { .. }))
}

/// The commit on screen that an approval would record. Only an answer that
/// says the approval is not recorded yet, and names a readable commit, counts.
/// Anything else (not loaded, a moved or missing approval, no readable commit)
/// shows nothing to approve.
fn displayed_tip(status: Option<&ApprovalStatus>) -> Option<String> {
    status
        .filter(|s| matches!(s.coverage, ApprovalCoverage::NotApplicable))
        .and_then(|s| s.branch_tip.clone())
}

/// The coverage reads the panel has started, and whether the latest one is still
/// running. Only the latest read may publish, because an older answer describes
/// a branch state the panel has since asked about again. A cadence tick starts
/// nothing while the latest read runs: otherwise a read that outlasts the tick
/// would be superseded by every tick and never shown.
#[derive(Debug, Default)]
struct CoverageReads {
    started: u32,
    latest_running: bool,
}

impl CoverageReads {
    /// Starts a read and returns its number, or `None` when a cadence tick finds
    /// the latest read still running.
    fn start(&mut self, cadence: bool) -> Option<u32> {
        if cadence && self.latest_running {
            return None;
        }
        self.started += 1;
        self.latest_running = true;
        Some(self.started)
    }

    /// Whether the answer to `read` may be published. Only the latest read
    /// finishes the running one, so a superseded answer cannot release a read
    /// that is still running.
    fn finish(&mut self, read: u32) -> bool {
        if read != self.started {
            return false;
        }
        self.latest_running = false;
        true
    }

    /// Gives up on `read` because it outlasted its bound. Returns whether it was
    /// still the running latest read. Its late answer then publishes nothing,
    /// and the next cadence tick asks again.
    fn abandon(&mut self, read: u32) -> bool {
        if read != self.started || !self.latest_running {
            return false;
        }
        self.started += 1;
        self.latest_running = false;
        true
    }
}

/// What the panel may say about an approval, read from the latest coverage
/// answer. Only a read that names the approval current says current. A missing
/// read is unknown, never current, and a stale answer from before the approval
/// is unknown too. A branch at the approved commit still is not current while
/// the published pull request shows another head, or none that was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverageView {
    Current,
    Stale,
    Unknown,
    /// The branch is at the approved commit, but the published pull request is at another one.
    PublishedMoved,
    /// The branch is at the approved commit, but the published pull request's head is not known.
    PublishedUnverified,
    /// The Task has no branch, so the approval names no commit, and a pull request is linked
    /// to it. The approval cannot be compared with that pull request's head.
    NoCommit,
}

fn coverage_view(status: Option<&ApprovalStatus>, has_branch: bool, approved_now: Option<&str>) -> CoverageView {
    // An answer names the approval it was read for. After a renewal, the answer
    // for the earlier approval says nothing about the new one.
    if status.is_some_and(|s| s.approved_tip.as_deref() != approved_now) {
        return CoverageView::Unknown;
    }
    match status.map(|s| s.coverage) {
        Some(ApprovalCoverage::Current) => status.map_or(CoverageView::Current, |s| published_view(s, approved_now)),
        // Only a task without a branch has nothing to bind the approval to. Its
        // approval covers no pull request, so a linked one is never current for it.
        Some(ApprovalCoverage::NotApplicable) if !has_branch => match status.and_then(|s| s.published.as_ref()) {
            Some(_) => CoverageView::NoCommit,
            None => CoverageView::Current,
        },
        Some(
            ApprovalCoverage::Moved | ApprovalCoverage::Unrecorded | ApprovalCoverage::BranchMissing,
        ) => CoverageView::Stale,
        Some(ApprovalCoverage::NotApplicable) | None => CoverageView::Unknown,
    }
}

/// What the published pull request allows the panel to say about an approval
/// the branch covers. The approval covers the pull request only when its head
/// is the approved commit. A head that was not read is unverified, never
/// current.
fn published_view(status: &ApprovalStatus, approved: Option<&str>) -> CoverageView {
    let Some(published) = status.published.as_ref() else {
        return CoverageView::Current;
    };
    match (published.head.as_deref(), approved) {
        (Some(head), Some(approved)) if head == approved => CoverageView::Current,
        (Some(_), Some(_)) => CoverageView::PublishedMoved,
        _ => CoverageView::PublishedUnverified,
    }
}

/// The heading when the published pull request is at another commit than the
/// approved one. It names both, and says the approval does not cover the head.
fn published_moved_heading(approved: Option<&str>, head: Option<&str>) -> String {
    match (approved, head) {
        (Some(approved), Some(head)) => format!(
            "Approved for {}; the published pull request is at {}, which this approval does not cover",
            short(approved),
            short(head)
        ),
        _ => "The published pull request is at another commit than the one you approved".to_string(),
    }
}

/// The heading for an approval that names no commit while a pull request is
/// linked. The approval is real, but it cannot show what the pull request holds.
fn no_commit_heading(approved: Option<&str>) -> String {
    match approved {
        Some(approved) => format!(
            "Approved for {}, but the task has no branch, so the published pull request is not covered",
            short(approved)
        ),
        None => "Approved, but no commit was recorded, so the published pull request is not covered".to_string(),
    }
}

/// The heading when the published pull request's head is not known.
fn published_unverified_heading(approved: Option<&str>) -> String {
    match approved {
        Some(approved) => format!("Approved for {}; the published pull request is not verified", short(approved)),
        None => "Approved; the published pull request is not verified".to_string(),
    }
}

/// The line under the heading. A head that was read says what it is and when
/// GitHub reported it, and that changes after the approved commit need review.
/// A head that was not read says why.
fn published_detail(status: Option<&ApprovalStatus>, approved: Option<&str>) -> Option<String> {
    let published = status?.published.as_ref()?;
    match published.head.as_deref() {
        Some(head) => {
            let at = published.observed_at.as_deref().and_then(read_time).unwrap_or_else(|| "an unknown time".to_string());
            Some(match approved {
                Some(approved) => format!(
                    "Changes after {} require review in the pull request. GitHub reported its head as {} at {at}.",
                    short(approved),
                    short(head)
                ),
                // Without a commit there is nothing for the head to be compared with.
                None => format!(
                    "GitHub reported its head as {} at {at}. This approval names no commit to compare it with.",
                    short(head)
                ),
            })
        }
        None => published.unverified.clone(),
    }
}

/// The UTC clock time of an RFC 3339 timestamp, as `HH:MM UTC`.
fn read_time(observed_at: &str) -> Option<String> {
    Some(format!("{} UTC", observed_at.get(11..16)?))
}

/// Whether the branch has no commit because the repository does not have it.
/// A read error is not this: it is shown as an error, with a retry.
fn branch_not_found(status: Option<&ApprovalStatus>) -> bool {
    status.is_some_and(|s| matches!(s.coverage, ApprovalCoverage::NotApplicable) && s.branch_tip.is_none())
}

/// Whether the commit the Changes tab shows is the one an approval would cover.
/// Both must be present and equal. A tab that is not loaded, whose load failed,
/// or that offers no commit to approve never matches, so absent review content
/// cannot satisfy the approval.
fn viewed_matches(viewed: Option<&str>, shown: Option<&str>) -> bool {
    matches!((viewed, shown), (Some(v), Some(s)) if v == s)
}

/// Whether the approve controls may be used. Approval needs the pull request
/// answer. A task that is not loaded cannot be approved. A task with a branch
/// also needs the commit on screen, because the backend records only the commit
/// the person was shown. A task without a branch has no commit to show.
fn approval_ready(availability_known: bool, task_has_branch: Option<bool>, tip_shown: bool) -> bool {
    availability_known
        && match task_has_branch {
            None => false,
            Some(false) => true,
            Some(true) => tip_shown,
        }
}

/// The first twelve characters of a commit, as the drawer names commits.
fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// What the approved line says: the exact commit the approval covers, so an
/// approval is never shown without it. When the record carries no commit (an
/// approval made before this was recorded, or a task without a branch) it
/// says only that it was approved.
fn approved_label(approved_tip: Option<&str>) -> String {
    match approved_tip {
        Some(tip) => format!("Approved for {}", short(tip)),
        None => "Approved".to_string(),
    }
}

/// The stale heading: the approval covered one commit and the branch is now
/// at another, so the approval is stale — never left looking current. When
/// either commit is not known, it says only that the branch changed.
fn stale_heading(approved: Option<&str>, current: Option<&str>) -> String {
    match (approved, current) {
        (Some(approved), Some(current)) => {
            format!("Approved for {}; branch now at {}; stale", short(approved), short(current))
        }
        _ => "The branch is not what you approved".to_string(),
    }
}

/// The heading when the branch could not be checked against the approval. It
/// names the approved commit but never says the approval is current.
fn unverified_heading(approved: Option<&str>) -> String {
    match approved {
        Some(approved) => format!("Approved for {}; not verified against the branch", short(approved)),
        None => "Approved; not verified against the branch".to_string(),
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
    /// The commit the Changes tab is showing, when it shows one.
    viewed_tip: RwSignal<Option<String>>,
    /// Asked when the Changes tab shows a commit other than the one to approve.
    on_stale: Callback<()>,
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
    let confirming_local = RwSignal::new(false);
    // Whether the approval still covers the branch as it is. Asked again
    // whenever the task record changes, so a branch that moved is not shown
    // as approved.
    let coverage = RwSignal::new(None::<ApprovalStatus>);
    // Kept apart from `error`, which an action clears: a failed read clears
    // itself when a later read succeeds.
    let coverage_error = RwSignal::new(None::<String>);
    let reads = StoredValue::new(CoverageReads::default());
    // Asks what the branch is now. Only the latest question's answer is kept.
    // `cadence` is the poll's tick: it starts nothing while a read runs.
    let ask_coverage_read = move |cadence: bool| {
        // A held cadence asks nothing: the panel keeps what it showed when
        // the hold began, so an acceptance step can move the branch and click
        // without the panel having read the move.
        if coverage_poll_held() {
            return;
        }
        let Some(read) = reads.try_update_value(|r| r.start(cadence)).flatten() else {
            return;
        };
        spawn_local(async move {
            // A read that outlasts its bound stops being trusted: the last
            // answer may describe a branch that has since moved, and nothing
            // says the next one is coming. The timer is owned here, so it is
            // cancelled when the read finishes and never fires for a read
            // that completed.
            let _bound = gloo_timers::callback::Timeout::new(COVERAGE_READ_MS, move || {
                if reads.try_update_value(|r| r.abandon(read)).unwrap_or(false) {
                    coverage.try_set(None);
                    coverage_error.try_set(Some(COVERAGE_TIMED_OUT.to_string()));
                }
            });
            let answer = get_approval_status(task_id.to_string()).await;
            // Clears the running read only when it is the latest, so an
            // answer that is superseded still publishes nothing.
            if !reads.try_update_value(|r| r.finish(read)).unwrap_or(false) {
                return;
            }
            // A read that was in flight when the hold began may have read the
            // branch after the hold began, so it must not publish either.
            if coverage_poll_held() {
                return;
            }
            match answer {
                Ok(status) => {
                    // Set only what changed: the poll asks on a cadence, and
                    // an unchanged answer must not rebuild the panel (its
                    // controls with it) under the person's hand.
                    if coverage.get_untracked().as_ref() != Some(&status) {
                        coverage.try_set(Some(status));
                        coverage_error.try_set(None);
                    }
                }
                // Without a current answer there is no commit to approve, so the
                // earlier one must not keep the controls enabled.
                Err(e) => {
                    let message = format!(
                        "The branch's current commit could not be read, so it cannot be approved yet: {e}"
                    );
                    let unchanged = coverage.get_untracked().is_none()
                        && coverage_error.get_untracked().as_deref() == Some(message.as_str());
                    if !unchanged {
                        coverage.try_set(None);
                        coverage_error.try_set(Some(message));
                    }
                }
            }
        });
    };
    let ask_coverage = move || ask_coverage_read(false);
    Effect::new(move |_| {
        task.track();
        ask_coverage();
    });
    // The branch can move outside SlashIt while the panel is up, and that
    // changes no task record, so the answer above would go stale: an older
    // approval would keep looking current until the person tried something.
    // Asked on a cadence while the panel is shown, a moved branch is flagged
    // on its own, and a decided panel learns the branch's commit again. The
    // answer is read-only (it never exports the bookmark), so the cadence
    // changes nothing but this panel's picture.
    {
        let ask = Closure::wrap(Box::new(move || ask_coverage_read(true)) as Box<dyn Fn()>);
        let interval = web_sys::window().and_then(|w| {
            w.set_interval_with_callback_and_timeout_and_arguments_0(
                ask.as_ref().unchecked_ref(),
                COVERAGE_POLL_MS,
            )
            .ok()
        });
        let ask = StoredValue::new_local(Some(ask));
        on_cleanup(move || {
            if let (Some(w), Some(id)) = (web_sys::window(), interval) {
                w.clear_interval_with_handle(id);
            }
            // The interval is cleared before the closure is freed, so no
            // callback can fire on a freed closure: the closure is disposed
            // here instead of `forget()`-ed for the interval's sake, which
            // leaked one every time the panel mounted.
            ask.dispose();
        });
    }
    // The branch moved while the Changes tab shows another commit: ask the tab to
    // follow it. Keyed on the branch's commit alone, so a failed reload does not
    // trigger another one.
    Effect::new(move |_| {
        if let Some(tip) = coverage.with(|c| displayed_tip(c.as_ref())) {
            if !viewed_tip.with_untracked(|v| viewed_matches(v.as_deref(), Some(&tip))) {
                on_stale.run(());
            }
        }
    });
    // A decided task whose approval no longer covers the branch offers
    // approving the branch as it is now, and the person can only be shown
    // that commit the same way: the tab follows it, keyed on the branch's
    // commit alone, and read untracked, so a failed reload does not trigger
    // another one.
    Effect::new(move |_| {
        if coverage
            .with(|c| {
                c.as_ref()
                    .filter(|c| matches!(c.coverage, ApprovalCoverage::Moved | ApprovalCoverage::Unrecorded))
                    .and_then(|c| c.branch_tip.clone())
            })
            .is_some_and(|tip| {
                !viewed_tip.with_untracked(|v| viewed_matches(v.as_deref(), Some(tip.as_str())))
            })
        {
            on_stale.run(());
        }
    });
    // Read at each use, so the controls follow both the availability answer and
    // the commit answer. Enabling them on availability alone was not enough.
    let approval_ready_now = move || {
        let shown = coverage.with(|c| displayed_tip(c.as_ref()));
        approval_ready(
            availability.with(Option::is_some),
            task.with(|t| t.as_ref().map(|t| t.branch_name.is_some())),
            shown.as_deref().is_some_and(|s| viewed_tip.with(|v| viewed_matches(v.as_deref(), Some(s)))),
        )
    };
    // The approval covers `shown`. If the Changes tab was read at another commit, what
    // the person looked at is not what they would approve: refuse, and refresh it.
    let diverged = move |shown: Option<&str>| {
        let viewed = viewed_tip.get_untracked();
        // Only a task with a branch has a commit to compare. The backend approves a
        // task without one as it is, so the comparison does not apply to it.
        let branch = task.with_untracked(|t| t.as_ref().map(|t| t.branch_name.is_some()));
        let diverged = match branch {
            Some(false) => false,
            _ => !viewed_matches(viewed.as_deref(), shown),
        };
        if diverged {
            error.set(Some(
                "The Changes tab does not show the commit this would approve, because it is not loaded or shows another commit. It is being reloaded; look at it again before approving."
                    .to_string(),
            ));
            ask_coverage();
            on_stale.run(());
        }
        diverged
    };
    let approve_current_branch = move |tip: String| {
        if busy.get_untracked() || diverged(Some(&tip)) {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match approve_task_again(task_id.to_string(), tip).await {
                Ok(updated) => {
                    apply_task.try_run(updated);
                    crate::components::toast::success("The branch is approved as it is now.".to_string());
                }
                Err(e) => {
                    error.try_set(Some(format!("The branch was not approved: {e}")));
                }
            }
            busy.try_set(false);
        });
    };

    // Attempts to open the pull request are registered app-wide, so the
    // card and the header know the task is waiting on SlashIt while one runs,
    // even after this drawer is closed.
    let deliveries = Deliveries::get();
    let in_flight = move || deliveries.is_some_and(|d| d.in_flight(task_id));
    let attempt = move || deliveries.and_then(|d| d.record(task_id));

    let apply_outcome = move |task: Task, pr: Option<PrDelivery>| {
        apply_task.try_run(task);
        if let Some(PrDelivery::Created { url }) = &pr {
            crate::components::toast::success(format!("Pull request created: {url}"));
        }
        if let Some(PrDelivery::Failed { .. }) = &pr {
            crate::components::toast::error("The pull request was not created.".to_string());
        }
        if let Some(PrDelivery::Unavailable { reason }) = &pr {
            availability.try_set(Some(PrAvailability::Unavailable { reason: reason.clone() }));
        }
        last_delivery.try_set(pr);
    };

    let approve = move |create_pr: bool| {
        // The commit on screen. The approval covers that commit and no other.
        let shown = displayed_tip(coverage.get_untracked().as_ref());
        if busy.get_untracked() || diverged(shown.as_deref()) {
            return;
        }
        busy.set(true);
        error.set(None);
        if create_pr {
            if let Some(d) = deliveries {
                d.begin(task_id);
            }
        }
        spawn_local(async move {
            let attempted = match approve_task(task_id.to_string(), create_pr, shown).await {
                Ok(outcome) => {
                    let attempted = tried_a_pull_request(outcome.pr.as_ref());
                    apply_outcome(outcome.task, outcome.pr);
                    attempted
                }
                Err(e) => {
                    error.try_set(Some(e));
                    // A refusal usually means the branch moved: show what it is now.
                    ask_coverage();
                    false
                }
            };
            if create_pr {
                if let Some(d) = deliveries {
                    d.finish(task_id, attempted);
                }
            }
            busy.try_set(false);
        });
    };

    let retry_pr = move |_| {
        if busy.get_untracked() || deliveries.is_some_and(|d| d.in_flight_untracked(task_id)) {
            return;
        }
        busy.set(true);
        error.set(None);
        if let Some(d) = deliveries {
            d.begin(task_id);
        }
        spawn_local(async move {
            let attempted = match create_approved_task_pr(task_id.to_string()).await {
                Ok(outcome) => {
                    let attempted = tried_a_pull_request(outcome.pr.as_ref());
                    apply_outcome(outcome.task, outcome.pr);
                    attempted
                }
                Err(e) => {
                    error.try_set(Some(e));
                    false
                }
            };
            if let Some(d) = deliveries {
                d.finish(task_id, attempted);
            }
            busy.try_set(false);
        });
    };

    // `without_pull_request` is the person's choice to keep the work local in
    // a project that could open a pull request; it is never implied.
    let complete_locally = move |without_pull_request: bool| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match complete_task_locally(task_id.to_string(), without_pull_request).await {
                // The task as it now is: Done, or still in Human Review with
                // the reason Git kept its checkout.
                Ok(updated) if updated.completed_locally() => {
                    confirming_local.try_set(false);
                    let title = updated.title.clone();
                    apply_task.try_run(updated);
                    crate::components::toast::success(format!(
                        "'{title}' completed locally. Its branch was not merged."
                    ));
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
    // What the approved status may claim, from the latest answer. Only a read
    // that names the approval current shows it as current; a stale answer shows
    // it as stale; a missing or failed read shows it as unverified. The
    // delivery checks the branch again before it publishes anything.
    let approval_view = Memo::new(move |_| {
        let (has_branch, approved) = task.with(|t| {
            t.as_ref().map_or((false, None), |t| {
                (t.branch_name.is_some(), t.human_review.approved_tip().map(str::to_string))
            })
        });
        coverage.with(|c| coverage_view(c.as_ref(), has_branch, approved.as_deref()))
    });
    let in_review = Memo::new(move |_| task.with(|t| t.as_ref().is_some_and(|t| t.status == TaskStatus::HumanReview)));
    // The approved work is being integrated into a local branch, or already
    // is, and the task is still in review: that is the one delivery, and no
    // second one (a pull request, a plain completion) is offered next to it.
    let integrating = Memo::new(move |_| {
        task.with(|t| {
            t.as_ref().is_some_and(|t| t.status == TaskStatus::HumanReview && t.integrating_current_approval())
        })
    });
    // The controls that finish approved changes locally: a button, then the
    // consequences, then a deliberate confirmation.
    let local_completion = move |without_pull_request: bool| {
        view! {
            <Show when=move || in_review.get()>
                <button
                    data-testid="human-review-complete-locally"
                    class="px-3 py-1.5 rounded-lg text-sm bg-emerald-500/15 text-emerald-200 hover:bg-emerald-500/25 disabled:opacity-50"
                    disabled=move || busy.get() || confirming_local.get()
                    on:click=move |_| {
                        error.set(None);
                        confirming_local.set(true);
                    }
                >
                    "Complete locally"
                </button>
                <Show when=move || confirming_local.get()>
                    <div data-testid="human-review-complete-locally-confirm" class="rounded-md border border-white/10 bg-black/20 p-2 space-y-2">
                        <p class="text-xs font-semibold text-white/70">"Complete locally?"</p>
                        <ul data-testid="human-review-complete-locally-consequences" class="list-disc pl-5 space-y-0.5 text-xs text-white/60">
                            {move || task.with(|t| t.as_ref().map(close_consequences).unwrap_or_default())
                                .into_iter()
                                .map(|line| view! { <li class="break-words">{line}</li> })
                                .collect_view()}
                        </ul>
                        <div class="flex gap-2">
                            <button
                                data-testid="human-review-complete-locally-confirm-button"
                                class="px-3 py-1.5 rounded-lg text-sm font-medium bg-emerald-500/25 text-emerald-100 hover:bg-emerald-500/35 disabled:opacity-50"
                                disabled=move || busy.get()
                                on:click=move |_| complete_locally(without_pull_request)
                            >
                                {move || if busy.get() { "Completing…" } else { "Complete locally" }}
                            </button>
                            <button
                                data-testid="human-review-complete-locally-cancel"
                                class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/60 hover:bg-white/10"
                                disabled=move || busy.get()
                                on:click=move |_| confirming_local.set(false)
                            >
                                "Cancel"
                            </button>
                        </div>
                    </div>
                </Show>
            </Show>
        }
    };

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

            // A pull request is being opened. Said here and on the card, so
            // the 10 seconds or so it can take never look like nothing.
            <Show when=in_flight>
                <p data-testid="human-review-delivering" role="status" class="flex items-center gap-2 text-xs text-sky-200">
                    <svg class="w-3.5 h-3.5 animate-spin" aria-hidden="true" fill="none" viewBox="0 0 24 24">
                        <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
                        <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"></path>
                    </svg>
                    "Creating PR\u{2026}"
                </p>
            </Show>

            // Undecided: the decision itself.
            <Show when=move || in_review.get() && decision.get().is_none()>
                <p class="text-xs text-white/50">"Decide on these changes. The Changes tab shows the commit you would approve; edits not yet committed are listed apart there and are not approved."</p>
                // An approval covers the branch commit that is current when the
                // person approves. Naming the branch shows what that will be.
                {move || task.with(|t| t.as_ref().and_then(|t| t.branch_name.clone())).map(|branch| view! {
                    <p data-testid="human-review-approval-branch" class="text-xs text-white/50 break-all">
                        "Approval covers the current commit of branch "
                        <code class="font-mono text-white/70">{branch}</code>
                        ". If the branch moves after you approve, the approval no longer applies and you approve again."
                    </p>
                })}
                // The exact commit, read from Git now, when it can be read.
                {move || coverage.get()
                    .filter(|c| matches!(c.coverage, ApprovalCoverage::NotApplicable))
                    .and_then(|c| c.branch_tip)
                    .map(|tip| view! {
                        <p data-testid="human-review-approval-commit" class="text-xs text-white/50 break-all">
                            "Current commit: "
                            <code class="font-mono text-white/70">{tip}</code>
                        </p>
                    })}
                // A branch the repository does not have offers no commit to approve.
                {move || {
                    let branch = task.with(|t| t.as_ref().and_then(|t| t.branch_name.clone()))?;
                    coverage
                        .with(|c| branch_not_found(c.as_ref()))
                        .then(|| view! {
                            <p data-testid="human-review-branch-missing" class="text-xs text-amber-200/80 break-all">
                                "Branch "
                                <code class="font-mono text-white/70">{branch}</code>
                                " was not found in the repository, so there is no commit to approve."
                            </p>
                        })
                }}
                // Why there is no AI verdict, when the record says why.
                {move || task.with(|t| t.as_ref().and_then(|t| no_verdict_summary(t.qa_signoff.as_ref(), &t.activity))).map(|summary| view! {
                    <p data-testid="human-review-ai-status" class="text-xs text-amber-200/80">{summary}</p>
                })}
                // A Manual run has no AI review on record, so only a recorded
                // review is named as a check.
                <p data-testid="human-review-test-evidence" class="text-xs text-white/50">
                    {move || {
                        let ai_review = task.with(|t| t.as_ref().is_some_and(|t| t.qa_signoff.is_some()));
                        if ai_review {
                            "No test run is recorded for this Task. The AI review is the only recorded check."
                        } else {
                            "No test run and no AI review is recorded for this Task. You are the only check on these changes."
                        }
                    }}
                </p>
                <div class="flex flex-wrap gap-2">
                    {move || {
                        let available = availability.get();
                        let (label, create_pr) = match available {
                            None => ("Checking…", false),
                            Some(PrAvailability::Available) => ("Approve & Create PR", true),
                            Some(PrAvailability::Unavailable { .. }) => ("Approve", false),
                        };
                        view! {
                            <button
                                data-testid="human-review-approve"
                                data-creates-pr=create_pr.to_string()
                                class="px-3 py-1.5 rounded-lg text-sm font-medium bg-emerald-500/20 text-emerald-200 hover:bg-emerald-500/30 disabled:opacity-50"
                                disabled=move || busy.get() || !approval_ready_now()
                                on:click=move |_| approve(create_pr)
                            >
                                {move || if busy.get() && !composing.get() { "Working…" } else { label }}
                            </button>
                        }
                    }}
                    // A project that can open a pull request keeps that as its
                    // default; approving for local delivery is a separate,
                    // named choice that opens nothing.
                    <Show when=move || matches!(availability.get(), Some(PrAvailability::Available))>
                        <button
                            data-testid="human-review-approve-local"
                            class="px-3 py-1.5 rounded-lg text-sm font-medium bg-white/5 text-emerald-200/90 hover:bg-white/10 disabled:opacity-50"
                            disabled=move || busy.get() || !approval_ready_now()
                            on:click=move |_| approve(false)
                        >
                            "Approve for local delivery"
                        </button>
                    </Show>
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

            // Approved: the decision, then delivery on its own line. The
            // commit the approval covers is named, so the decision always
            // shows what it was about. While the approval covers the branch
            // the status reads as current; once it does not, the same line
            // names both commits and says stale — a stale approval never
            // keeps a green check as if the branch were approved. One
            // persistent live region says both, so the change is announced.
            <Show when=move || decision.get() == Some(HumanReviewDecision::Approved)>
                <p
                    data-testid="human-review-approved"
                    role="status"
                    data-approval=move || match approval_view.get() {
                        CoverageView::Current => "current",
                        CoverageView::Stale => "stale",
                        CoverageView::Unknown => "unknown",
                        CoverageView::PublishedMoved => "published-moved",
                        CoverageView::PublishedUnverified => "published-unverified",
                        CoverageView::NoCommit => "no-commit",
                    }
                    class=move || if approval_view.get() == CoverageView::Current {
                        "flex items-center gap-2 text-sm font-medium text-emerald-300"
                    } else {
                        "flex items-center gap-2 text-sm font-medium text-amber-200"
                    }
                >
                    <Show when=move || approval_view.get() == CoverageView::Current>
                        <svg class="w-4 h-4 shrink-0" fill="none" viewBox="0 0 24 24" stroke="currentColor" aria-hidden="true">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                        </svg>
                    </Show>
                    <Show when=move || approval_view.get() != CoverageView::Current>
                        <svg class="w-4 h-4 shrink-0" fill="none" viewBox="0 0 24 24" stroke="currentColor" aria-hidden="true">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 9v4m0 4h.01M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z" />
                        </svg>
                    </Show>
                    {move || {
                        let tip = task.with(|t| {
                            t.as_ref().and_then(|t| t.human_review.approved_tip().map(str::to_string))
                        });
                        match approval_view.get() {
                            CoverageView::Stale => coverage.with(|c| {
                                let (approved, current) = c
                                    .as_ref()
                                    .map_or((None, None), |c| (c.approved_tip.as_deref(), c.branch_tip.as_deref()));
                                stale_heading(approved, current)
                            }),
                            CoverageView::Unknown => unverified_heading(tip.as_deref()),
                            CoverageView::PublishedMoved => coverage.with(|c| {
                                let head = c.as_ref().and_then(|c| c.published.as_ref()).and_then(|p| p.head.as_deref());
                                published_moved_heading(tip.as_deref(), head)
                            }),
                            CoverageView::PublishedUnverified => published_unverified_heading(tip.as_deref()),
                            CoverageView::NoCommit => no_commit_heading(tip.as_deref()),
                            CoverageView::Current => approved_label(tip.as_deref()),
                        }
                    }}
                </p>
                // The published pull request, when the branch alone would not show it.
                {move || matches!(approval_view.get(), CoverageView::PublishedMoved | CoverageView::PublishedUnverified | CoverageView::NoCommit)
                    .then(|| {
                        let approved = task.with(|t| t.as_ref().and_then(|t| t.human_review.approved_tip().map(str::to_string)));
                        coverage.with(|c| published_detail(c.as_ref(), approved.as_deref()))
                    })
                    .flatten()
                    .map(|detail| view! {
                        <p data-testid="human-review-published" class="text-xs text-white/70 break-words">{detail}</p>
                    })
                }
                // Gated on the same view as the status line: an answer for an
                // earlier approval must not put its reason or its renewal
                // control beside a newer approval.
                {move || (approval_view.get() == CoverageView::Stale).then(|| coverage.get()).flatten().map(|c| {
                    let blocker = task.with(|t| t.as_ref().and_then(renewal_blocker));
                    let tip = c.branch_tip.clone()
                        .filter(|_| matches!(c.coverage, ApprovalCoverage::Moved | ApprovalCoverage::Unrecorded))
                        .filter(|_| blocker.is_none());
                    let reason = c.reason.clone().unwrap_or_default();
                    view! {
                        // The status line above already names both commits and
                        // says stale; this box holds what to do about it.
                        <div data-testid="human-review-approval-stale" data-coverage=format!("{:?}", c.coverage) class="rounded-md border border-amber-500/20 bg-amber-500/[0.06] p-2 space-y-2">
                            <p class="text-xs text-white/70 whitespace-pre-wrap break-words">{reason}</p>
                            {blocker.map(|why| view! {
                                <p data-testid="human-review-approve-again-unavailable" class="text-xs text-white/50">{why}</p>
                            })}
                            {tip.map(|tip| {
                                let shown = tip.clone();
                                view! {
                                <button
                                    data-testid="human-review-approve-again"
                                    class="px-3 py-1.5 rounded-lg text-sm bg-emerald-500/15 text-emerald-200 hover:bg-emerald-500/25 disabled:opacity-50"
                                    disabled=move || busy.get() || !viewed_tip.with(|v| viewed_matches(v.as_deref(), Some(shown.as_str())))
                                    on:click=move |_| approve_current_branch(tip.clone())
                                >
                                    "Approve the branch as it is now"
                                </button>
                                }
                            })}
                        </div>
                    }
                })}
                {move || delivery.get().map(|d| if integrating.get() {
                    view! {
                        <div data-testid="human-review-delivery" data-delivery="integrating_locally" class="text-xs text-white/60 space-y-2">
                            <p>"This task's approved work is being delivered to a local branch. No pull request is created."</p>
                            <LocalIntegrationPanel task_id=task_id task=task apply_task=apply_task completing=true />
                        </div>
                    }.into_any()
                } else { match d {
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
                            {move || attempt().filter(|a| a.finished > 0).map(|a| {
                                let when = a.last_finished_at.unwrap_or_default();
                                view! {
                                    <p data-testid="human-review-attempt" data-attempt=a.finished.to_string() class="text-xs text-white/55">
                                        {format!("Attempt {} failed at {when}.", a.finished)}
                                    </p>
                                }
                            })}
                            <p class="text-xs text-white/45">"Your approval is kept. Nothing was merged."</p>
                            {move || task.with(|t| t.as_ref().is_some_and(|t| t.pr_create_unconfirmed.is_some())).then(|| view! {
                                <p data-testid="human-review-unconfirmed-hint" class="text-xs text-amber-200">
                                    "SlashIt asked GitHub to open this pull request and never learned the outcome. It will not open another until you settle that in Pull request recovery below."
                                </p>
                            })}
                            <button
                                data-testid="human-review-retry-pr"
                                class="px-3 py-1.5 rounded-lg text-sm bg-blue-500/15 text-blue-200 hover:bg-blue-500/25 disabled:opacity-50"
                                disabled=move || busy.get() || in_flight()
                                on:click=retry_pr
                            >
                                {move || if busy.get() || in_flight() { "Creating…" } else { "Retry Create PR" }}
                            </button>
                        </div>
                    }.into_any(),
                    Delivery::Unavailable { reason } => view! {
                        <div data-testid="human-review-delivery" data-delivery="unavailable" class="text-xs text-white/55 space-y-2">
                            <p>"SlashIt cannot create a GitHub pull request for this project."</p>
                            <p data-testid="human-review-pr-unavailable" class="text-white/45">{reason}</p>
                            <p class="text-white/45">"Nothing was merged. The task stays in Human Review with its work on its branch until you complete it."</p>
                            {local_completion(false)}
                            <LocalIntegrationPanel task_id=task_id task=task apply_task=apply_task completing=true />
                        </div>
                    }.into_any(),
                    Delivery::CompletedLocally { branch } => view! {
                        <div data-testid="human-review-delivery" data-delivery="completed_locally" class="text-xs text-white/60 space-y-1">
                            <p data-testid="human-review-completed-locally" class="font-medium text-emerald-200/90">"Completed locally."</p>
                            <p>
                                {move || if task.with(|t| t.as_ref().is_some_and(is_integrated)) {
                                    "Nothing was pushed, and no pull request exists."
                                } else {
                                    "Nothing was merged or pushed, and no pull request exists."
                                }}
                            </p>
                            <p data-testid="human-review-completed-branch">
                                {match branch {
                                    Some(branch) => format!("The work is on the branch {branch} in your local repository. Its Task Checkout was removed."),
                                    None => "This task has no branch recorded. Its Task Checkout was removed.".to_string(),
                                }}
                            </p>
                            <LocalIntegrationPanel task_id=task_id task=task apply_task=apply_task />
                        </div>
                    }.into_any(),
                    Delivery::NotAttempted => view! {
                        <div data-testid="human-review-delivery" data-delivery="not_attempted" class="text-xs text-white/55 space-y-2">
                            <p>"No pull request has been created yet."</p>
                            <button
                                data-testid="human-review-retry-pr"
                                class="px-3 py-1.5 rounded-lg text-sm bg-blue-500/15 text-blue-200 hover:bg-blue-500/25 disabled:opacity-50"
                                disabled=move || busy.get() || in_flight()
                                on:click=retry_pr
                            >
                                {move || if busy.get() || in_flight() { "Creating…" } else { "Create PR" }}
                            </button>
                            <p data-testid="human-review-local-choice" class="text-white/45">
                                "Or keep this work local: finish it without a pull request. Nothing is pushed, and you can publish later."
                            </p>
                            {local_completion(true)}
                            <LocalIntegrationPanel task_id=task_id task=task apply_task=apply_task completing=true />
                        </div>
                    }.into_any(),
                } })}
            </Show>

            {move || coverage_error.get().map(|e| view! {
                <div class="flex flex-wrap items-center gap-2">
                    <p data-testid="human-review-commit-error" class="text-xs text-red-300 whitespace-pre-wrap break-words">{e}</p>
                    // Asks again through the same latest-answer guard; approval stays
                    // disabled until a read succeeds.
                    <button
                        data-testid="human-review-commit-retry"
                        class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/80 hover:bg-white/10"
                        on:click=move |_| ask_coverage()
                    >
                        "Try reading the commit again"
                    </button>
                </div>
            })}
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
    fn a_task_completed_locally_reads_as_such_and_not_as_unavailable() {
        let mut t = task(TaskStatus::Done);
        t.branch_name = Some("task-1".into());
        let unavailable = PrAvailability::Unavailable { reason: "no origin".into() };
        // Done alone proves nothing about the branch.
        assert_eq!(
            Delivery::for_task(&t, Some(&unavailable)),
            Delivery::Unavailable { reason: "no origin".into() }
        );

        let mut entries = Vec::new();
        slashit_activity::record(&mut entries, chrono::Utc::now(), slashit_activity::Kind::CompletedLocally {});
        t.activity = entries;
        assert!(t.completed_locally());
        let done = Delivery::CompletedLocally { branch: Some("task-1".into()) };
        assert_eq!(Delivery::for_task(&t, Some(&unavailable)), done);
        assert_eq!(Delivery::for_task(&t, None), done, "no availability answer is needed");
        assert_eq!(done.clone().or_answer(Some(&PrDelivery::Unavailable { reason: "x".into() })), done);

        // The entry without the status is not a completion either.
        t.status = TaskStatus::HumanReview;
        assert!(!t.completed_locally());
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
    fn a_skipped_or_failed_review_is_named_as_such_and_never_as_a_rejection() {
        let mut entries = Vec::new();
        slashit_activity::record(
            &mut entries,
            chrono::Utc::now(),
            slashit_activity::Kind::RunStarted {
                run: 1,
                addressing_feedback: false,
            },
        );
        slashit_activity::record(
            &mut entries,
            chrono::Utc::now(),
            slashit_activity::Kind::AiReviewSkipped {
                review: 1,
                reason: "no reviewer model was configured".into(),
            },
        );
        assert_eq!(
            no_verdict_summary(None, &entries).as_deref(),
            Some("AI review was skipped: no reviewer model was configured.")
        );
        assert_eq!(no_verdict_label(&entries), Some("AI review skipped"));
        assert_eq!(
            no_verdict_summary(Some(&signoff(QaStatus::Rejected, &[])), &entries),
            None,
            "a recorded verdict is never replaced by the skip text"
        );

        let mut failed = Vec::new();
        slashit_activity::record(
            &mut failed,
            chrono::Utc::now(),
            slashit_activity::Kind::AiReviewFailed {
                review: 1,
                reason: "the task's changes could not be read: git exited 128.".into(),
            },
        );
        assert_eq!(
            no_verdict_summary(None, &failed).as_deref(),
            Some("AI review failed: the task's changes could not be read: git exited 128.")
        );
        assert_eq!(no_verdict_label(&failed), Some("AI review failed"));
        assert_eq!(no_verdict_summary(None, &[]), None, "no record, no claim");
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
    fn only_a_tried_pull_request_counts_as_an_attempt() {
        assert!(tried_a_pull_request(Some(&PrDelivery::Failed { reason: "x".into() })));
        assert!(tried_a_pull_request(Some(&PrDelivery::Created { url: "u".into() })));
        assert!(!tried_a_pull_request(Some(&PrDelivery::Unavailable { reason: "x".into() })));
        assert!(!tried_a_pull_request(None));
    }

    #[test]
    fn work_being_integrated_locally_is_the_one_delivery_of_the_current_approval() {
        use crate::models::integration::{IntegrationState, IntegrationStrategy, LocalIntegration};
        let record = |state, tip: &str| LocalIntegration {
            state,
            target_branch: "main".into(),
            target_old_oid: "a".repeat(40),
            approved_tip: tip.into(),
            result_oid: "c".repeat(40),
            strategy: IntegrationStrategy::FastForward,
            checkout_path: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            completes_task: true,
        };
        let tip = "b".repeat(40);
        let mut t = task(TaskStatus::HumanReview);
        assert!(!t.integrating_current_approval(), "nothing started");

        t.local_integration = Some(record(IntegrationState::Pending, &"9".repeat(40)));
        assert!(t.integrating_current_approval(), "a pending integration always counts");

        t.human_review.entries.push(crate::models::HumanReviewEntry {
            sequence: 1,
            arrival: t.human_review.arrivals,
            decision: HumanReviewDecision::Approved,
            feedback: None,
            approved_tip: Some(tip.clone()),
            decided_at: chrono::Utc::now(),
        });
        t.local_integration = Some(record(IntegrationState::Integrated, &tip));
        assert!(t.integrating_current_approval(), "the approved commit is the integrated one");

        t.local_integration = Some(record(IntegrationState::Integrated, &"9".repeat(40)));
        assert!(!t.integrating_current_approval(), "an earlier approval's integration is not this delivery");
    }

    #[test]
    fn an_approval_is_not_offered_for_renewal_where_the_backend_would_refuse_it() {
        use crate::models::integration::{IntegrationState, IntegrationStrategy, LocalIntegration};
        let mut t = task(TaskStatus::HumanReview);
        assert_eq!(renewal_blocker(&t), None, "an ordinary reviewed task may renew");

        t.pr_url = Some("https://github.com/o/r/pull/1".into());
        assert!(renewal_blocker(&t).is_some_and(|w| w.contains("pull request")));
        t.pr_url = None;

        t.local_integration = Some(LocalIntegration {
            state: IntegrationState::Pending,
            target_branch: "main".into(),
            target_old_oid: "a".repeat(40),
            approved_tip: "b".repeat(40),
            result_oid: "c".repeat(40),
            strategy: IntegrationStrategy::FastForward,
            checkout_path: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            completes_task: true,
        });
        assert!(renewal_blocker(&t).is_some_and(|w| w.contains("Resume or discard")));
        t.local_integration = None;

        assert!(renewal_blocker(&task(TaskStatus::Done)).is_some_and(|w| w.contains("no longer in review")));
    }

    #[test]
    fn card_labels_are_words_not_enum_names() {
        for status in [QaStatus::Approved, QaStatus::FixesApplied, QaStatus::Rejected] {
            let label = ai_review_label(&status);
            assert!(!label.contains("FixesApplied") && !label.contains("Rejected"), "{label}");
        }
    }

    fn answer(coverage: ApprovalCoverage, branch_tip: Option<&str>) -> ApprovalStatus {
        ApprovalStatus {
            coverage,
            approved_tip: None,
            branch_tip: branch_tip.map(str::to_string),
            rewritten: None,
            reason: None,
            published: None,
        }
    }

    /// An answer for a branch at the approved commit, with the published pull
    /// request as the poll last read it: either its head, or why it is unknown.
    fn published_answer(approved: &str, head: Option<&str>, unverified: Option<&str>) -> ApprovalStatus {
        let published = crate::services::human_review_service::PublishedHead {
            head: head.map(str::to_string),
            observed_at: head.map(|_| "2026-11-15T11:22:57.123Z".to_string()),
            unverified: unverified.map(str::to_string),
        };
        ApprovalStatus {
            approved_tip: Some(approved.to_string()),
            published: Some(published),
            ..answer(ApprovalCoverage::Current, Some(approved))
        }
    }

    /// Missing evidence: nothing is on screen until the answer names a commit.
    #[test]
    fn no_commit_is_shown_before_the_answer_arrives_or_when_none_is_readable() {
        assert_eq!(displayed_tip(None), None);
        assert_eq!(displayed_tip(Some(&answer(ApprovalCoverage::NotApplicable, None))), None);
    }

    /// Current tip: an undecided branch shows the commit an approval would record.
    #[test]
    fn the_commit_read_for_an_undecided_branch_is_the_one_shown() {
        let tip = "a".repeat(40);
        assert_eq!(displayed_tip(Some(&answer(ApprovalCoverage::NotApplicable, Some(&tip)))), Some(tip));
    }

    #[test]
    fn a_missing_branch_is_named_apart_from_a_failed_read() {
        let tip = "a".repeat(40);
        assert!(branch_not_found(Some(&answer(ApprovalCoverage::NotApplicable, None))));
        assert!(!branch_not_found(Some(&answer(ApprovalCoverage::NotApplicable, Some(&tip)))));
        assert!(!branch_not_found(Some(&answer(ApprovalCoverage::Current, None))));
        assert!(!branch_not_found(None), "no answer yet, and no failed read either");
    }

    /// Stale tip: an approval that no longer covers the branch shows no commit
    /// to approve, so a stale answer cannot authorize a different state.
    #[test]
    fn a_moved_or_missing_approval_shows_no_commit_to_approve() {
        let tip = "b".repeat(40);
        for coverage in [ApprovalCoverage::Current, ApprovalCoverage::Moved, ApprovalCoverage::Unrecorded, ApprovalCoverage::BranchMissing] {
            assert_eq!(displayed_tip(Some(&answer(coverage, Some(&tip)))), None, "{coverage:?}");
        }
    }

    /// The controls need the availability answer, the task, and, for a branch,
    /// the commit on screen. Availability alone is not evidence.
    #[test]
    fn approval_waits_for_the_commit_on_a_branch_and_for_availability() {
        // Missing evidence: availability known, branch present, commit not loaded.
        assert!(!approval_ready(true, Some(true), false));
        // Availability still being asked.
        assert!(!approval_ready(false, Some(true), true));
        assert!(!approval_ready(false, Some(false), false));
        // The task is not loaded.
        assert!(!approval_ready(true, None, true));
        // Current tip shown.
        assert!(approval_ready(true, Some(true), true));
        // A task without a branch has no commit to show.
        assert!(approval_ready(true, Some(false), false));
    }

    /// An approval is never shown without the exact commit it covers.
    #[test]
    fn the_approved_line_names_the_commit_the_approval_covers() {
        let tip = "a".repeat(40);
        assert_eq!(approved_label(Some(&tip)), format!("Approved for {}", &tip[..12]));
        // An approval recorded before commits were kept, or one made without a
        // branch, has no commit to name and still reads as an approval.
        assert_eq!(approved_label(None), "Approved");
    }

    /// A stale approval is never displayed as current: both commits are named
    /// and it is said to be stale.
    #[test]
    fn a_stale_approval_names_both_commits_and_says_it_is_stale() {
        let (approved, current) = ("a".repeat(40), "b".repeat(40));
        let heading = stale_heading(Some(&approved), Some(&current));
        assert!(heading.contains("stale"), "{heading}");
        assert!(heading.contains(&approved[..12]), "{heading}");
        assert!(heading.contains(&current[..12]), "{heading}");
        // Without either commit the heading says only that the branch changed.
        assert_eq!(stale_heading(Some(&approved), None), "The branch is not what you approved");
        assert_eq!(stale_heading(None, Some(&current)), "The branch is not what you approved");
        assert_eq!(stale_heading(None, None), "The branch is not what you approved");
    }

    #[test]
    fn the_decision_refuses_a_commit_other_than_the_one_the_changes_tab_shows() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        // A tab that shows no commit does not match a commit to approve, and
        // neither side absent is evidence that anything was presented.
        assert!(!viewed_matches(None, Some(&a)));
        assert!(!viewed_matches(None, None));
        assert!(viewed_matches(Some(&a), Some(&a)));
        assert!(!viewed_matches(Some(&a), Some(&b)));
        // The tab shows a commit but none is offered to approve.
        assert!(!viewed_matches(Some(&a), None));
    }

    /// CR3: a read longer than the cadence is still published. Every tick
    /// during it starts nothing, so the answer is not superseded.
    #[test]
    fn a_read_longer_than_the_cadence_is_published() {
        let mut reads = CoverageReads::default();
        let read = reads.start(false).expect("the read starts");
        for _ in 0..5 {
            assert_eq!(reads.start(true), None, "a tick must not start over a running read");
        }
        assert!(reads.finish(read), "the only read's answer is published");
        assert!(reads.start(true).is_some(), "the cadence resumes once the read finished");
    }

    /// CR3: a task change during a read supersedes it, and only the newer answer
    /// is published, whichever completes first.
    #[test]
    fn a_newer_read_supersedes_an_older_one_in_either_completion_order() {
        let mut reads = CoverageReads::default();
        let older = reads.start(true).unwrap();
        let newer = reads.start(false).unwrap();
        assert!(reads.finish(newer), "the latest answer publishes first");
        assert!(!reads.finish(older), "the superseded answer arrives late and publishes nothing");

        let mut reads = CoverageReads::default();
        let older = reads.start(true).unwrap();
        let newer = reads.start(false).unwrap();
        assert!(!reads.finish(older), "the superseded answer arrives first and publishes nothing");
        assert!(reads.finish(newer));
    }

    /// CR3: a superseded read that finishes must not release the cadence while
    /// the latest read still runs.
    #[test]
    fn a_superseded_read_does_not_release_a_running_latest_read() {
        let mut reads = CoverageReads::default();
        let older = reads.start(true).unwrap();
        let newer = reads.start(false).unwrap();
        assert!(!reads.finish(older));
        assert_eq!(reads.start(true), None, "the latest read is still running");
        assert!(reads.finish(newer));
        assert!(reads.start(true).is_some());
    }

    /// CR3: a failed read is the latest answer too, so it clears the running
    /// read like any other, and the next tick asks again.
    #[test]
    fn a_failed_read_clears_the_running_read() {
        let mut reads = CoverageReads::default();
        let read = reads.start(true).unwrap();
        assert!(reads.finish(read));
        assert!(reads.start(true).is_some());
    }

    /// A read that outlasts its bound is abandoned, so a hung request cannot
    /// keep the cadence waiting. Its late answer publishes nothing, and the
    /// next tick asks again.
    #[test]
    fn an_abandoned_read_publishes_nothing_and_the_cadence_retries() {
        let mut reads = CoverageReads::default();
        let hung = reads.start(true).unwrap();
        assert_eq!(reads.start(true), None, "the hung read holds the cadence until its bound");
        assert!(reads.abandon(hung), "the latest running read is abandoned at its bound");
        // Before any retry: the window between the bound and the next tick is
        // where a late answer could otherwise still be published.
        assert!(!reads.finish(hung), "the hung read's answer, if it arrives late, publishes nothing");
        assert!(reads.start(true).is_some(), "the next tick asks again");
    }

    /// The bound of a read that already finished, or was superseded, abandons
    /// nothing: a stale timer must not discard a newer answer.
    #[test]
    fn a_bound_for_a_finished_or_superseded_read_abandons_nothing() {
        let mut reads = CoverageReads::default();
        let older = reads.start(true).unwrap();
        let newer = reads.start(false).unwrap();
        assert!(!reads.abandon(older), "a superseded read is not abandoned");
        assert!(reads.abandon(newer), "the latest running read is abandoned");

        let mut reads = CoverageReads::default();
        let read = reads.start(true).unwrap();
        assert!(reads.finish(read));
        assert!(!reads.abandon(read), "a finished read has nothing to abandon");
        assert!(reads.start(true).is_some(), "a finished read leaves the cadence free");
    }

    /// The view for an answer read for the current approval.
    fn view(coverage: Option<ApprovalCoverage>, has_branch: bool) -> CoverageView {
        let approved = "a".repeat(40);
        let status = coverage.map(|c| ApprovalStatus { approved_tip: Some(approved.clone()), ..answer(c, None) });
        coverage_view(status.as_ref(), has_branch, Some(&approved))
    }

    /// An answer read for an earlier approval says nothing about a renewed one,
    /// even when it reads as current or stale.
    #[test]
    fn an_answer_for_an_earlier_approval_is_unknown_for_the_renewed_one() {
        let (earlier, renewed) = ("a".repeat(40), "b".repeat(40));
        for coverage in [ApprovalCoverage::Current, ApprovalCoverage::Moved] {
            let status = ApprovalStatus { approved_tip: Some(earlier.clone()), ..answer(coverage, None) };
            assert_eq!(coverage_view(Some(&status), true, Some(&renewed)), CoverageView::Unknown, "{coverage:?}");
            assert_ne!(coverage_view(Some(&status), true, Some(&earlier)), CoverageView::Unknown, "{coverage:?}");
        }
    }

    /// CR4: an unknown or failed read is never the current approval. The sequence
    /// the panel goes through: unknown, current, moved, failed after moved,
    /// failed after current, recovered.
    #[test]
    fn coverage_states_are_current_stale_or_unknown_and_never_current_by_default() {
        assert_eq!(view(None, true), CoverageView::Unknown, "before any read");
        assert_eq!(view(Some(ApprovalCoverage::Current), true), CoverageView::Current);
        assert_eq!(view(Some(ApprovalCoverage::Moved), true), CoverageView::Stale);
        // A failed read clears the answer, so the view is unknown, not current.
        assert_eq!(view(None, true), CoverageView::Unknown, "a failed read after a moved branch");
        assert_eq!(view(None, true), CoverageView::Unknown, "a failed read after a current branch");
        assert_eq!(view(Some(ApprovalCoverage::Current), true), CoverageView::Current, "recovered");
    }

    #[test]
    fn stale_answers_are_stale_and_an_unrecorded_approval_is_too() {
        for coverage in [ApprovalCoverage::Moved, ApprovalCoverage::Unrecorded, ApprovalCoverage::BranchMissing] {
            assert_eq!(view(Some(coverage), true), CoverageView::Stale, "{coverage:?}");
        }
    }

    /// A pre-approval answer for a branch names no approval, so it does not show
    /// the approval as current. Only a task without a branch has nothing to bind.
    #[test]
    fn an_undecided_answer_does_not_verify_a_new_approval() {
        assert_eq!(view(Some(ApprovalCoverage::NotApplicable), true), CoverageView::Unknown);
        assert_eq!(view(Some(ApprovalCoverage::NotApplicable), false), CoverageView::Current);
    }

    /// A branch at the approved commit verifies the approval for the published
    /// pull request only when the pull request's head is that same commit. A
    /// head that differs, or was not read, is never current.
    #[test]
    fn a_published_head_must_be_the_approved_commit_to_read_as_current() {
        let (approved, moved) = ("a".repeat(40), "b".repeat(40));
        let view_of = |status: &ApprovalStatus| coverage_view(Some(status), true, Some(&approved));
        assert_eq!(view_of(&published_answer(&approved, Some(&approved), None)), CoverageView::Current);
        assert_eq!(view_of(&published_answer(&approved, Some(&moved), None)), CoverageView::PublishedMoved);
        assert_eq!(
            view_of(&published_answer(&approved, None, Some("GitHub could not be asked."))),
            CoverageView::PublishedUnverified,
        );
        // The renewed approval's view must not borrow the earlier answer's head.
        let renewed = "c".repeat(40);
        assert_eq!(
            coverage_view(Some(&published_answer(&approved, Some(&moved), None)), true, Some(&renewed)),
            CoverageView::Unknown,
        );
        // No pull request means the branch alone decides, as before.
        let without_pr = ApprovalStatus { approved_tip: Some(approved.clone()), ..answer(ApprovalCoverage::Current, Some(&approved)) };
        assert_eq!(view_of(&without_pr), CoverageView::Current);
    }

    /// A Task without a branch has no commit for its approval to name. A linked
    /// pull request cannot be compared with it, whether its head is known or not,
    /// so the approval is never current for it. With no pull request, the approval
    /// is the whole answer, as before.
    #[test]
    fn a_branchless_approval_never_claims_a_linked_pull_request_is_covered() {
        use crate::services::human_review_service::PublishedHead;
        let known = PublishedHead {
            head: Some("b".repeat(40)),
            observed_at: Some("2026-11-15T11:22:57.123Z".to_string()),
            unverified: None,
        };
        let unknown = PublishedHead { head: None, observed_at: None, unverified: Some("GitHub could not be asked.".into()) };
        let branchless = |published: Option<PublishedHead>| ApprovalStatus {
            published,
            ..answer(ApprovalCoverage::NotApplicable, None)
        };
        assert_eq!(coverage_view(Some(&branchless(None)), false, None), CoverageView::Current, "no pull request");
        assert_eq!(coverage_view(Some(&branchless(Some(known))), false, None), CoverageView::NoCommit);
        assert_eq!(coverage_view(Some(&branchless(Some(unknown))), false, None), CoverageView::NoCommit);
    }

    /// The copy for an approval with no commit beside a linked pull request says
    /// what is missing, and never that the approval is current. The head is named
    /// as read, without comparing it to a commit the approval never recorded.
    #[test]
    fn a_branchless_approval_wording_names_what_is_missing() {
        let heading = no_commit_heading(None);
        assert!(heading.contains("not covered") && !heading.contains("current"), "{heading}");
        let approved = "a".repeat(40);
        let named = no_commit_heading(Some(&approved));
        assert!(named.contains(&approved[..12]) && named.contains("no branch"), "{named}");

        let head = "b".repeat(40);
        let read = ApprovalStatus {
            published: Some(crate::services::human_review_service::PublishedHead {
                head: Some(head.clone()),
                observed_at: Some("2026-11-15T11:22:57.123Z".to_string()),
                unverified: None,
            }),
            ..answer(ApprovalCoverage::NotApplicable, None)
        };
        let detail = published_detail(Some(&read), None).expect("a read head is described");
        assert!(detail.contains(&head[..12]) && detail.contains("11:22 UTC"), "{detail}");
        assert!(detail.contains("names no commit"), "{detail}");
        assert!(!detail.contains("Changes after"), "{detail}");
    }

    #[test]
    fn published_wording_names_both_commits_and_never_says_current() {
        let (approved, head) = ("a".repeat(40), "b".repeat(40));
        let moved = published_moved_heading(Some(&approved), Some(&head));
        assert!(moved.contains(&approved[..12]) && moved.contains(&head[..12]), "{moved}");
        assert!(moved.contains("does not cover"), "{moved}");
        assert!(!moved.contains("current"), "{moved}");

        let read = published_answer(&approved, Some(&head), None);
        let detail = published_detail(Some(&read), Some(&approved)).expect("a read head is described");
        assert!(detail.contains(&format!("Changes after {} require review", &approved[..12])), "{detail}");
        assert!(detail.contains(&head[..12]) && detail.contains("11:22 UTC"), "{detail}");

        let unread = published_answer(&approved, None, Some("GitHub could not be asked."));
        assert_eq!(published_detail(Some(&unread), Some(&approved)).as_deref(), Some("GitHub could not be asked."));
        assert!(published_unverified_heading(Some(&approved)).contains("not verified"));
        assert!(!published_unverified_heading(Some(&approved)).contains("current"));
        assert_eq!(published_detail(Some(&answer(ApprovalCoverage::Current, None)), Some(&approved)), None);
    }

    #[test]
    fn an_unverified_heading_names_the_commit_but_never_says_current() {
        let approved = "a".repeat(40);
        let heading = unverified_heading(Some(&approved));
        assert!(heading.contains(&approved[..12]), "{heading}");
        assert!(heading.contains("not verified"), "{heading}");
        assert!(!heading.contains("current"), "{heading}");
        assert!(unverified_heading(None).contains("not verified"));
    }
}
