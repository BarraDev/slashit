// SPDX-License-Identifier: MPL-2.0
//! Publishing a reviewed commit to an existing pull request.
//!
//! Publication is its own decision. A person chooses Publish to PR for one
//! exact commit, one branch and one pull request, and that choice authorizes
//! the push of that commit and nothing else. It does not approve the Task,
//! does not change its Human Review record, and does not cover any commit
//! made after it: a later commit needs a new publication request, and the
//! Task's approval, if any, still binds the commit it recorded.
//!
//! This module holds the durable record of such a request and the rules that
//! decide it. Every rule is a pure function of what was observed, so the
//! refusals can be tested without GitHub or a remote. The command in
//! `commands::pr::publication` observes the repository and the pull request,
//! asks these rules, and writes the outcome.
//!
//! Limitation: the record shows which SlashIt operation asked for a push. It
//! does not prove which human asked. A process running as the same user can
//! reach the same interface, so the gate is the GUI admission check, not an
//! identity check.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::task::{Task, TaskStatus};

/// The publication most recently requested for a Task. One record is kept:
/// a new request replaces it only once the previous one is settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrPublication {
    pub request_id: Uuid,
    /// The full object ID the person was shown and confirmed.
    pub commit: String,
    /// The Task branch the commit was published from.
    pub branch: String,
    /// The Task's pull request, which the commit updates.
    pub pr_url: String,
    /// The remote branch's head observed before the push, when there was
    /// one. A refused push leaves the remote at this commit, so reconciling
    /// a `Pending` or `Unknown` record compares against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_before: Option<String>,
    pub state: PublicationState,
    pub requested_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Where a publication stands. Only `Published` says the remote holds the
/// commit, and only because SlashIt read the remote branch back afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PublicationState {
    /// Empty braces rather than a unit variant: serde ignores extra keys on a unit
    /// variant of an internally tagged enum even under `deny_unknown_fields`.
    /// Persisted before the push started. The remote may or may not have
    /// changed, so this is settled by reading the remote, not by assumption.
    Pending {},
    /// The remote branch was read back at `commit`.
    Published {},
    /// Nothing reached the remote: refused before the push, or the remote
    /// refused it. A new request may run.
    Refused { reason: String },
    /// The outcome could not be determined. A new request waits until the
    /// remote has been read again and the record is settled.
    Unknown { reason: String },
}

impl PrPublication {
    /// A request that has not yet touched the remote.
    pub fn pending(
        request_id: Uuid,
        commit: String,
        branch: String,
        pr_url: String,
        remote_before: Option<String>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            request_id,
            commit,
            branch,
            pr_url,
            remote_before,
            state: PublicationState::Pending {},
            requested_at: now,
            updated_at: now,
        }
    }

    /// Whether this record still needs the remote to be read before anything
    /// else may publish.
    pub fn needs_reconciliation(&self) -> bool {
        matches!(self.state, PublicationState::Pending {} | PublicationState::Unknown { .. })
    }
}

/// What SlashIt observed about the pull request before a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestObservation {
    pub open: bool,
    /// The pull request's head branch, as GitHub reports it.
    pub head_branch: String,
    /// Whether GitHub's own pull request for the branch is the recorded one.
    pub is_branch_pull_request: bool,
}

/// What SlashIt observed about the repository before a push. Nothing here is
/// written by these checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartObservation {
    /// The local branch tip, as a push would read it.
    pub local_tip: Option<String>,
    /// The remote branch head, when the remote could be read and has one.
    pub remote_head: Option<String>,
    /// Whether the remote head is an ancestor of the commit (a fast-forward).
    /// Only meaningful when `remote_head` is `Some`.
    pub remote_head_is_ancestor: bool,
    pub pull_request: PullRequestObservation,
}

/// Why a publication was refused before any push. Each message names the
/// condition a person can act on.
pub fn start_refusal(
    commit: &str,
    branch: &str,
    inspected_remote_head: &str,
    observed: &StartObservation,
) -> Option<String> {
    let pr = &observed.pull_request;
    if !pr.open {
        return Some(
            "The pull request is closed or merged, so the commit cannot be published to it. \
             Nothing was pushed."
                .to_string(),
        );
    }
    if pr.head_branch != branch || !pr.is_branch_pull_request {
        return Some(
            "The recorded pull request is not the open pull request for this task's branch, so \
             nothing was pushed."
                .to_string(),
        );
    }
    if observed.local_tip.as_deref() != Some(commit) {
        return Some(
            "The task's branch is no longer at the commit you chose to publish, so nothing was \
             pushed. Review the branch as it is and choose Publish to PR again."
                .to_string(),
        );
    }
    match observed.remote_head.as_deref() {
        None => Some(
            "The pull request's branch is missing on the remote, so nothing was pushed. Restore \
             the branch on GitHub first."
                .to_string(),
        ),
        Some(head) if head != inspected_remote_head => Some(
            "The pull request's branch on the remote moved after you inspected it, so nothing was \
             pushed. Choose Publish to PR again to compare the commit with the new remote head."
                .to_string(),
        ),
        Some(_) if !observed.remote_head_is_ancestor => Some(
            "The remote branch has commits this branch does not contain, so publishing would not \
             be a fast-forward. Nothing was pushed, and nothing was forced. Integrate the remote \
             changes locally, then review and publish again."
                .to_string(),
        ),
        Some(_) => None,
    }
}

/// Why a push did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushFailure {
    /// The push was stopped before `git push` was started.
    NotSent(String),
    /// Git reported the branch update as rejected, by its own lease check or by
    /// the remote, so this push changed nothing.
    Rejected(String),
    /// The push failed without saying whether the branch was updated.
    Unconfirmed(String),
}

impl PushFailure {
    pub fn into_message(self) -> String {
        match self {
            PushFailure::NotSent(m) | PushFailure::Rejected(m) | PushFailure::Unconfirmed(m) => m,
        }
    }
}

/// How a request that started pushing ended. `remote_before` is the reviewed
/// remote head the push required. `remote_after` is what the remote branch was
/// read back as afterwards: `Err` when it could not be read.
pub fn classify_outcome(
    commit: &str,
    remote_before: Option<&str>,
    remote_after: &Result<Option<String>, String>,
    push: &Result<(), PushFailure>,
) -> PublicationState {
    if matches!(remote_after, Ok(Some(head)) if head == commit) {
        return PublicationState::Published {};
    }
    // These two prove this push left the branch alone, whatever it holds now.
    match push {
        Err(PushFailure::NotSent(error)) => {
            return PublicationState::Refused { reason: format!("nothing was pushed: {error}") };
        }
        Err(PushFailure::Rejected(error)) => {
            let moved = matches!(remote_after, Ok(after) if after.as_deref() != remote_before);
            let reason = if moved {
                format!(
                    "The pull request's branch on the remote is no longer at the revision you \
                     reviewed ({}), so the push was refused and nothing was pushed. Choose Publish \
                     to PR again to compare the commit with the new remote head. {error}",
                    remote_before.unwrap_or("none")
                )
            } else {
                format!("the remote refused the push, and nothing was pushed: {error}")
            };
            return PublicationState::Refused { reason };
        }
        Ok(()) | Err(PushFailure::Unconfirmed(_)) => {}
    }
    match remote_after {
        Err(error) => PublicationState::Unknown {
            reason: format!(
                "the remote branch could not be read after the push, so whether it changed is \
                 unknown: {error}"
            ),
        },
        Ok(after) => match push {
            Err(failure) if after.as_deref() == remote_before => PublicationState::Refused {
                reason: format!(
                    "the push failed, and the remote branch did not change: {}",
                    failure.clone().into_message()
                ),
            },
            Err(failure) => PublicationState::Unknown {
                reason: format!(
                    "the push failed and the remote branch is not at the published commit: {}",
                    failure.clone().into_message()
                ),
            },
            Ok(()) => PublicationState::Unknown {
                reason: "the push reported success, but the remote branch is not at the published \
                         commit"
                    .to_string(),
            },
        },
    }
}

/// The settled state a `Pending` or `Unknown` record reaches once the remote
/// has been read again.
///
/// `commit_on_remote` says whether the record's commit is the remote head or
/// one of its ancestors, and `None` when Git could not say. The remote head
/// may have moved on since (someone else pushed), so the head alone does not
/// settle the record: a commit reachable from the head reached the remote,
/// and a head that does not contain it means it did not. `None` (the
/// function) when the remote still cannot say.
pub fn reconcile(
    record: &PrPublication,
    remote_now: &Result<Option<String>, String>,
    commit_on_remote: Option<bool>,
) -> Option<PublicationState> {
    let head = remote_now.as_ref().ok()?;
    let refused = || PublicationState::Refused {
        reason: "the remote branch does not contain the commit, so an earlier publication did not reach \
                 it. Nothing was pushed by that record, and it is safe to publish again."
            .to_string(),
    };
    match commit_on_remote {
        Some(true) => Some(PublicationState::Published {}),
        Some(false) => Some(refused()),
        None if head.as_deref() == record.remote_before.as_deref() => Some(refused()),
        None => None,
    }
}

/// What a new request should do when a record already exists for the Task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replay {
    /// No usable record: the request may start.
    Start,
    /// The commit is already published from this branch to this pull request.
    /// Nothing else is pushed, and the existing record answers the request.
    AlreadyPublished,
    /// An earlier request must be reconciled first.
    MustReconcile,
}

/// Decide what an existing record means for a new request to publish
/// `commit` from `branch` to `pr_url`. A published record answers the request
/// only when it is for the same commit, branch and pull request.
pub fn replay(existing: Option<&PrPublication>, commit: &str, branch: &str, pr_url: &str) -> Replay {
    match existing {
        None => Replay::Start,
        Some(record) if record.needs_reconciliation() => Replay::MustReconcile,
        Some(record) if record.state == PublicationState::Published {}
            && record.commit == commit
            && record.branch == branch
            && record.pr_url == pr_url =>
        {
            Replay::AlreadyPublished
        }
        Some(_) => Replay::Start,
    }
}

/// What an observed open pull request says about a Task's current approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenDelivery {
    /// The open pull request holds the approved commit, and SlashIt published
    /// that commit to this pull request.
    Delivered,
    /// Some evidence is missing or disagrees. The reason names what, for the
    /// person reading the board and for the tests.
    NotYetObserved(String),
}

/// Whether an observed open pull request shows a published Task's current
/// approval as delivered (G2 of `docs/architecture/post-pr-corrections.md`).
///
/// Every piece is required, and each is read from the same observation:
/// - the current decision is an approval that recorded its commit B;
/// - the observation reads `OPEN` with a full head equal to B (an absent,
///   abbreviated or different head is not evidence);
/// - `pr_publication` is `Published` at B for this pull request. An absent,
///   other, pending, unknown or refused record is not evidence.
///
/// Pure: it decides nothing about the Task's status, owner or record.
pub fn open_delivery(
    task: &Task,
    pr_url: &str,
    observed_state: &str,
    observed_head: Option<&str>,
) -> OpenDelivery {
    use OpenDelivery::NotYetObserved;
    if !observed_state.eq_ignore_ascii_case("OPEN") {
        return NotYetObserved("the pull request is not open".to_string());
    }
    if !task.human_review.is_approved() {
        return NotYetObserved("the Task has no current approval".to_string());
    }
    let Some(approved) = task.human_review.approved_tip() else {
        return NotYetObserved("the current approval records no commit".to_string());
    };
    let Some(head) = observed_head else {
        return NotYetObserved(format!(
            "GitHub reported no full head for the pull request; the approved commit is {approved}"
        ));
    };
    if head != approved {
        return NotYetObserved(format!(
            "the pull request head is {head}; the approved commit is {approved}"
        ));
    }
    match task.pr_publication.as_ref() {
        None => NotYetObserved(format!("no publication record shows {approved} published")),
        Some(p) if p.pr_url != pr_url => {
            NotYetObserved("the publication record names another pull request".to_string())
        }
        Some(p) if p.commit != approved => NotYetObserved(format!(
            "the publication record names {}, not the approved commit {approved}",
            p.commit
        )),
        Some(p) if !matches!(p.state, PublicationState::Published {}) => {
            NotYetObserved("the publication is not settled as published".to_string())
        }
        Some(_) => OpenDelivery::Delivered,
    }
}

/// Whether a Task is in a post-PR correction for the pull request it is
/// linked to: linked before the observation, and out of PR Created in a
/// status a correction run or its review occupies.
pub fn is_post_pr_correction(task: &Task, linked_before: bool) -> bool {
    linked_before
        && matches!(
            task.status,
            TaskStatus::Queue | TaskStatus::InProgress | TaskStatus::AiReview | TaskStatus::HumanReview
        )
}

/// What an observation of an open or closed pull request does to its Task's
/// status and owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrObservationEffect {
    /// The Task moves to PR Created, and its live owner is ended first, as a
    /// link has always done.
    MovesToPrCreated,
    /// Only the pull request's state is recorded. The status, the run and the
    /// Task Checkout are kept, and no owner is ended.
    RecordsStateOnly,
}

/// The effect of one observed pull request state (and its full head) on a
/// Task. Outside a post-PR correction this is today's rule; inside one, only
/// an open pull request that [`open_delivery`] verifies moves the Task.
///
/// The caller passes the state and head of the same observation it writes,
/// and handles `MERGED` itself.
///
/// `latest` says the observation is still the newest reading of the pull
/// request (no newer reading with another state or head has been recorded).
/// A superseded observation is not evidence inside a correction, whatever
/// its head says: a newer reading decides, and the one judged here does not.
pub fn pr_observation_effect(
    task: &Task,
    pr_url: &str,
    linked_before: bool,
    observed_state: &str,
    observed_head: Option<&str>,
    latest: bool,
) -> PrObservationEffect {
    if !is_post_pr_correction(task, linked_before) {
        return PrObservationEffect::MovesToPrCreated;
    }
    // Only Human Review can be delivered by an observation. A queued, running
    // or AI-reviewed correction belongs to its run, which keeps its status.
    if task.status != TaskStatus::HumanReview || !latest {
        return PrObservationEffect::RecordsStateOnly;
    }
    match open_delivery(task, pr_url, observed_state, observed_head) {
        OpenDelivery::Delivered => PrObservationEffect::MovesToPrCreated,
        OpenDelivery::NotYetObserved(_) => PrObservationEffect::RecordsStateOnly,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";
    const PR: &str = "https://github.com/BarraDev/slashit/pull/9";
    const BRANCH: &str = "task-branch";

    fn ready() -> StartObservation {
        StartObservation {
            local_tip: Some(NEW.to_string()),
            remote_head: Some(OLD.to_string()),
            remote_head_is_ancestor: true,
            pull_request: PullRequestObservation {
                open: true,
                head_branch: BRANCH.to_string(),
                is_branch_pull_request: true,
            },
        }
    }

    fn record(state: PublicationState) -> PrPublication {
        let mut r = PrPublication::pending(
            Uuid::nil(),
            NEW.to_string(),
            BRANCH.to_string(),
            PR.to_string(),
            Some(OLD.to_string()),
            Utc::now(),
        );
        r.state = state;
        r
    }

    #[test]
    fn a_ready_fast_forward_on_an_open_pull_request_may_start() {
        assert_eq!(start_refusal(NEW, BRANCH, OLD, &ready()), None);
    }

    #[test]
    fn a_closed_or_merged_pull_request_is_refused() {
        let mut observed = ready();
        observed.pull_request.open = false;
        assert!(start_refusal(NEW, BRANCH, OLD, &observed).unwrap().contains("closed or merged"));
    }

    #[test]
    fn a_pull_request_for_another_head_or_not_this_branch_is_refused() {
        let mut other_head = ready();
        other_head.pull_request.head_branch = "someone-else".to_string();
        assert!(start_refusal(NEW, BRANCH, OLD, &other_head).is_some());

        let mut not_own = ready();
        not_own.pull_request.is_branch_pull_request = false;
        assert!(start_refusal(NEW, BRANCH, OLD, &not_own).is_some());
    }

    #[test]
    fn a_branch_that_moved_since_the_person_chose_the_commit_is_refused() {
        let mut moved = ready();
        moved.local_tip = Some(OLD.to_string());
        assert!(start_refusal(NEW, BRANCH, OLD, &moved).unwrap().contains("no longer at the commit"));

        let mut missing = ready();
        missing.local_tip = None;
        assert!(start_refusal(NEW, BRANCH, OLD, &missing).is_some());
    }

    #[test]
    fn a_missing_remote_branch_is_refused() {
        let mut gone = ready();
        gone.remote_head = None;
        assert!(start_refusal(NEW, BRANCH, OLD, &gone).unwrap().contains("missing on the remote"));
    }

    #[test]
    fn a_remote_head_other_than_the_inspected_one_is_refused_even_as_a_fast_forward() {
        let mut moved = ready();
        moved.remote_head = Some("3".repeat(40));
        let message = start_refusal(NEW, BRANCH, OLD, &moved).unwrap();
        assert!(message.contains("moved after you inspected it"));
    }

    #[test]
    fn a_non_fast_forward_is_refused_before_any_push() {
        let mut diverged = ready();
        diverged.remote_head_is_ancestor = false;
        let message = start_refusal(NEW, BRANCH, OLD, &diverged).unwrap();
        assert!(message.contains("not be a fast-forward"));
        assert!(message.contains("nothing was forced"));
    }

    #[test]
    fn a_landed_push_is_published_even_when_the_push_reported_an_error() {
        let after = Ok(Some(NEW.to_string()));
        let push = Err(PushFailure::Unconfirmed("connection reset".to_string()));
        assert_eq!(classify_outcome(NEW, Some(OLD), &after, &push), PublicationState::Published {});
    }

    #[test]
    fn a_push_the_remote_refused_with_no_change_is_refused() {
        let after = Ok(Some(OLD.to_string()));
        let push = Err(PushFailure::Rejected("rejected (non-fast-forward)".to_string()));
        assert!(matches!(
            classify_outcome(NEW, Some(OLD), &after, &push),
            PublicationState::Refused { .. }
        ));
    }

    #[test]
    fn a_rejected_push_to_a_moved_remote_is_refused_as_no_longer_reviewed() {
        let push = Err(PushFailure::Rejected("stale info".to_string()));
        for after in [Ok(Some("3".repeat(40))), Ok(None)] {
            let state = classify_outcome(NEW, Some(OLD), &after, &push);
            assert!(
                matches!(&state, PublicationState::Refused { reason }
                    if reason.contains("no longer at the revision you reviewed") && reason.contains(OLD)),
                "{state:?}"
            );
        }
    }

    #[test]
    fn an_unconfirmed_failure_with_a_moved_remote_stays_unknown() {
        let after = Ok(Some("3".repeat(40)));
        let push = Err(PushFailure::Unconfirmed("connection reset".to_string()));
        assert!(matches!(
            classify_outcome(NEW, Some(OLD), &after, &push),
            PublicationState::Unknown { .. }
        ));
    }

    #[test]
    fn a_success_the_remote_does_not_show_is_unknown() {
        let after = Ok(Some(OLD.to_string()));
        assert!(matches!(
            classify_outcome(NEW, Some(OLD), &after, &Ok(())),
            PublicationState::Unknown { .. }
        ));
    }

    #[test]
    fn an_unreadable_remote_after_the_push_is_unknown_never_refused() {
        let after = Err("network down".to_string());
        assert!(matches!(
            classify_outcome(NEW, Some(OLD), &after, &Err(PushFailure::Unconfirmed("timeout".to_string()))),
            PublicationState::Unknown { .. }
        ));
    }

    #[test]
    fn a_pending_record_is_settled_by_what_the_remote_holds_now() {
        let pending = record(PublicationState::Pending {});
        // The commit is the head, or reachable from it: it reached the remote.
        assert_eq!(
            reconcile(&pending, &Ok(Some(NEW.to_string())), Some(true)),
            Some(PublicationState::Published {})
        );
        // A later commit by someone else sits on top of it: still published.
        assert_eq!(
            reconcile(&pending, &Ok(Some("3".repeat(40))), Some(true)),
            Some(PublicationState::Published {})
        );
        // The head is unchanged, or a third commit is not built on this one: refused.
        assert!(matches!(
            reconcile(&pending, &Ok(Some(OLD.to_string())), Some(false)),
            Some(PublicationState::Refused { .. })
        ));
        assert!(matches!(
            reconcile(&pending, &Ok(Some("3".repeat(40))), Some(false)),
            Some(PublicationState::Refused { .. })
        ));
        // Ancestry unknown and the head unchanged: refused. Ancestry unknown and
        // the head moved: nothing is settled.
        assert!(matches!(
            reconcile(&pending, &Ok(Some(OLD.to_string())), None),
            Some(PublicationState::Refused { .. })
        ));
        assert_eq!(reconcile(&pending, &Ok(Some("3".repeat(40))), None), None);
        // An unreadable remote settles nothing, whatever the ancestry says.
        assert_eq!(reconcile(&pending, &Err("offline".to_string()), Some(true)), None);
    }

    #[test]
    fn a_pending_or_unknown_record_forces_reconciliation_before_a_new_request() {
        assert_eq!(replay(Some(&record(PublicationState::Pending {})), NEW, BRANCH, PR), Replay::MustReconcile);
        let unknown = record(PublicationState::Unknown { reason: "x".into() });
        assert_eq!(replay(Some(&unknown), NEW, BRANCH, PR), Replay::MustReconcile);
    }

    #[test]
    fn a_duplicate_request_for_a_published_commit_is_answered_without_a_push() {
        let published = record(PublicationState::Published {});
        assert_eq!(replay(Some(&published), NEW, BRANCH, PR), Replay::AlreadyPublished);
        // A different commit is a new request.
        assert_eq!(replay(Some(&published), OLD, BRANCH, PR), Replay::Start);
        // The same commit and pull request from another branch is a new request.
        assert_eq!(replay(Some(&published), NEW, "other-branch", PR), Replay::Start);
        assert_eq!(
            replay(Some(&published), NEW, BRANCH, "https://github.com/BarraDev/slashit/pull/10"),
            Replay::Start
        );
    }

    #[test]
    fn a_refused_record_lets_a_new_request_start() {
        let refused = record(PublicationState::Refused { reason: "x".into() });
        assert_eq!(replay(Some(&refused), NEW, BRANCH, PR), Replay::Start);
        assert_eq!(replay(None, NEW, BRANCH, PR), Replay::Start);
    }

    #[test]
    fn serialization_round_trips_every_state() {
        for state in [
            PublicationState::Pending {},
            PublicationState::Published {},
            PublicationState::Refused { reason: "r".into() },
            PublicationState::Unknown { reason: "u".into() },
        ] {
            let r = record(state);
            let text = toml::to_string(&r).expect("serialize");
            let back: PrPublication = toml::from_str(&text).expect("deserialize");
            assert_eq!(back, r);
        }
    }

    // ── G2: open delivery evidence (open_delivery) and the effect that
    // follows from it (pr_observation_effect) ──

    const PR_URL: &str = "https://github.com/testorg/testrepo/pull/9";

    /// A Task in Human Review, approved at `APPROVED`, linked to `PR`, with
    /// `publication` recorded for the approved commit (or none).
    fn approved_correction(status: TaskStatus, publication: Option<PublicationState>) -> Task {
        let mut task = crate::test_helpers::create_test_task_full(
            "correction",
            Uuid::new_v4(),
            status,
            0,
        );
        task.pr_url = Some(PR_URL.to_string());
        task.human_review.record_arrival();
        task.human_review.approve(Some(APPROVED.to_string()), Utc::now());
        task.pr_publication = publication.map(|state| {
            let mut p = PrPublication::pending(
                Uuid::new_v4(),
                APPROVED.to_string(),
                "task-branch".to_string(),
                PR_URL.to_string(),
                None,
                Utc::now(),
            );
            p.state = state;
            p
        });
        task
    }

    const APPROVED: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
    const OTHER: &str = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";

    fn published() -> Option<PublicationState> {
        Some(PublicationState::Published {})
    }

    #[test]
    fn an_open_pr_at_the_approved_head_with_a_published_record_is_delivered() {
        let task = approved_correction(TaskStatus::HumanReview, published());
        assert_eq!(
            open_delivery(&task, PR_URL, "OPEN", Some(APPROVED)),
            OpenDelivery::Delivered
        );
    }

    #[test]
    fn each_missing_or_disagreeing_piece_of_evidence_is_not_delivered() {
        let cases: Vec<(&str, Task, &str, Option<&str>)> = vec![
            ("an absent head", approved_correction(TaskStatus::HumanReview, published()), "OPEN", None),
            ("an abbreviated head", approved_correction(TaskStatus::HumanReview, published()), "OPEN", Some(&APPROVED[..7])),
            ("a different head", approved_correction(TaskStatus::HumanReview, published()), "OPEN", Some(OTHER)),
            ("no publication record", approved_correction(TaskStatus::HumanReview, None), "OPEN", Some(APPROVED)),
            ("a pending record", approved_correction(TaskStatus::HumanReview, Some(PublicationState::Pending {})), "OPEN", Some(APPROVED)),
            ("an unknown record", approved_correction(TaskStatus::HumanReview, Some(PublicationState::Unknown { reason: "x".into() })), "OPEN", Some(APPROVED)),
            ("a refused record", approved_correction(TaskStatus::HumanReview, Some(PublicationState::Refused { reason: "x".into() })), "OPEN", Some(APPROVED)),
            ("a closed pull request", approved_correction(TaskStatus::HumanReview, published()), "CLOSED", Some(APPROVED)),
            ("no current approval", {
                let mut t = approved_correction(TaskStatus::HumanReview, published());
                t.human_review.withdraw_approval();
                t
            }, "OPEN", Some(APPROVED)),
        ];
        for (why, task, state, head) in cases {
            assert!(
                matches!(open_delivery(&task, PR_URL, state, head), OpenDelivery::NotYetObserved(_)),
                "{why} must not be evidence"
            );
        }
    }

    #[test]
    fn a_record_for_another_commit_or_pull_request_is_not_evidence() {
        let mut other_commit = approved_correction(TaskStatus::HumanReview, published());
        other_commit.pr_publication.as_mut().unwrap().commit = OTHER.to_string();
        assert!(matches!(
            open_delivery(&other_commit, PR_URL, "OPEN", Some(APPROVED)),
            OpenDelivery::NotYetObserved(_)
        ));

        let mut other_pr = approved_correction(TaskStatus::HumanReview, published());
        other_pr.pr_publication.as_mut().unwrap().pr_url =
            "https://github.com/testorg/testrepo/pull/10".to_string();
        assert!(matches!(
            open_delivery(&other_pr, PR_URL, "OPEN", Some(APPROVED)),
            OpenDelivery::NotYetObserved(_)
        ));
    }

    #[test]
    fn a_correction_outside_pr_created_moves_only_on_open_delivery() {
        let unverified = approved_correction(TaskStatus::HumanReview, None);
        assert_eq!(
            pr_observation_effect(&unverified, PR_URL, true, "OPEN", Some(APPROVED), true),
            PrObservationEffect::RecordsStateOnly
        );
        let verified = approved_correction(TaskStatus::HumanReview, published());
        assert_eq!(
            pr_observation_effect(&verified, PR_URL, true, "OPEN", Some(APPROVED), true),
            PrObservationEffect::MovesToPrCreated
        );
        // A closure during a correction records the state only, even when the
        // record and head agree: G2 covers open pull requests only.
        assert_eq!(
            pr_observation_effect(&verified, PR_URL, true, "CLOSED", Some(APPROVED), true),
            PrObservationEffect::RecordsStateOnly
        );
    }

    #[test]
    fn a_running_correction_records_only_the_state_until_its_head_is_delivered() {
        for status in [TaskStatus::Queue, TaskStatus::InProgress, TaskStatus::AiReview] {
            let task = approved_correction(status.clone(), published());
            assert_eq!(
                pr_observation_effect(&task, PR_URL, true, "OPEN", Some(APPROVED), true),
                PrObservationEffect::RecordsStateOnly,
                "{status:?} is a correction whose run owns the status"
            );
        }
    }

    #[test]
    fn outside_a_correction_the_effect_is_todays_rule() {
        // A first link (not linked before) and a Task that is not a correction
        // keep the move they have always made, whatever the observation.
        let first_link = approved_correction(TaskStatus::HumanReview, None);
        assert_eq!(
            pr_observation_effect(&first_link, PR_URL, false, "OPEN", None, true),
            PrObservationEffect::MovesToPrCreated
        );
        let closed_in_pr_created = approved_correction(TaskStatus::PrCreated, None);
        assert_eq!(
            pr_observation_effect(&closed_in_pr_created, PR_URL, true, "CLOSED", None, true),
            PrObservationEffect::MovesToPrCreated
        );
    }

    #[test]
    fn a_superseded_open_observation_during_a_correction_records_only_the_state() {
        // The same OPEN answer at the approved head moves the correction only
        // while it is the newest reading. A newer reading that replaced it
        // (another head, or another state) decides instead, so this one holds.
        let verified = approved_correction(TaskStatus::HumanReview, published());
        assert_eq!(
            pr_observation_effect(&verified, PR_URL, true, "OPEN", Some(APPROVED), true),
            PrObservationEffect::MovesToPrCreated
        );
        assert_eq!(
            pr_observation_effect(&verified, PR_URL, true, "OPEN", Some(APPROVED), false),
            PrObservationEffect::RecordsStateOnly
        );
    }
}
