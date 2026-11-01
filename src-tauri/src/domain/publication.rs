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
pub fn start_refusal(commit: &str, branch: &str, observed: &StartObservation) -> Option<String> {
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
        Some(_) if !observed.remote_head_is_ancestor => Some(
            "The remote branch has commits this branch does not contain, so publishing would not \
             be a fast-forward. Nothing was pushed, and nothing was forced. Integrate the remote \
             changes locally, then review and publish again."
                .to_string(),
        ),
        Some(_) => None,
    }
}

/// How a request that started pushing ended. `remote_after` is what the
/// remote branch was read back as afterwards: `Err` when it could not be read.
pub fn classify_outcome(
    commit: &str,
    remote_before: Option<&str>,
    remote_after: &Result<Option<String>, String>,
    push: &Result<(), String>,
) -> PublicationState {
    match remote_after {
        Ok(Some(head)) if head == commit => PublicationState::Published {},
        Err(error) => PublicationState::Unknown {
            reason: format!(
                "the remote branch could not be read after the push, so whether it changed is \
                 unknown: {error}"
            ),
        },
        Ok(after) => match push {
            Err(error) if after.as_deref() == remote_before => PublicationState::Refused {
                reason: format!("the remote refused the push, and its branch did not change: {error}"),
            },
            Err(error) => PublicationState::Unknown {
                reason: format!(
                    "the push failed and the remote branch is not at the published commit: {error}"
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
    /// The commit is already published to this pull request. Nothing else
    /// is pushed, and the existing record answers the request.
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
        assert_eq!(start_refusal(NEW, BRANCH, &ready()), None);
    }

    #[test]
    fn a_closed_or_merged_pull_request_is_refused() {
        let mut observed = ready();
        observed.pull_request.open = false;
        assert!(start_refusal(NEW, BRANCH, &observed).unwrap().contains("closed or merged"));
    }

    #[test]
    fn a_pull_request_for_another_head_or_not_this_branch_is_refused() {
        let mut other_head = ready();
        other_head.pull_request.head_branch = "someone-else".to_string();
        assert!(start_refusal(NEW, BRANCH, &other_head).is_some());

        let mut not_own = ready();
        not_own.pull_request.is_branch_pull_request = false;
        assert!(start_refusal(NEW, BRANCH, &not_own).is_some());
    }

    #[test]
    fn a_branch_that_moved_since_the_person_chose_the_commit_is_refused() {
        let mut moved = ready();
        moved.local_tip = Some(OLD.to_string());
        assert!(start_refusal(NEW, BRANCH, &moved).unwrap().contains("no longer at the commit"));

        let mut missing = ready();
        missing.local_tip = None;
        assert!(start_refusal(NEW, BRANCH, &missing).is_some());
    }

    #[test]
    fn a_missing_remote_branch_is_refused() {
        let mut gone = ready();
        gone.remote_head = None;
        assert!(start_refusal(NEW, BRANCH, &gone).unwrap().contains("missing on the remote"));
    }

    #[test]
    fn a_non_fast_forward_is_refused_before_any_push() {
        let mut diverged = ready();
        diverged.remote_head_is_ancestor = false;
        let message = start_refusal(NEW, BRANCH, &diverged).unwrap();
        assert!(message.contains("not be a fast-forward"));
        assert!(message.contains("nothing was forced"));
    }

    #[test]
    fn a_landed_push_is_published_even_when_the_push_reported_an_error() {
        let after = Ok(Some(NEW.to_string()));
        let push = Err("connection reset".to_string());
        assert_eq!(classify_outcome(NEW, Some(OLD), &after, &push), PublicationState::Published {});
    }

    #[test]
    fn a_push_the_remote_refused_with_no_change_is_refused() {
        let after = Ok(Some(OLD.to_string()));
        let push = Err("rejected (non-fast-forward)".to_string());
        assert!(matches!(
            classify_outcome(NEW, Some(OLD), &after, &push),
            PublicationState::Refused { .. }
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
            classify_outcome(NEW, Some(OLD), &after, &Err("timeout".to_string())),
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
}
