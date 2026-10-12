//! Which tasks cannot make progress without the user right now.
//!
//! Attention is derived, never stored: it follows from a task's status, its
//! Human Review record and its local integration record, which are already
//! persisted, plus one fact that only the running process knows -- whether a
//! pull request is being opened for the task at this moment. Nothing here is written to a task file, so
//! the answer after a restart is whatever the persisted record says it is.
//!
//! This crate is the rule itself, and how a task's record is read for it.
//! The backend and the WASM frontend (and any later surface, such as the CLI
//! or a tray) each copy the plain facts of a task into [`Facts`]; none of
//! them decides on its own which combinations need the user.

use serde::{Deserialize, Serialize};

/// Why a task needs the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReason {
    /// The last run failed. SlashIt does not continue a failed task on its
    /// own; someone has to repair or retry it.
    Failed,
    /// The task is in Human Review and nobody has decided on the changes it
    /// arrived with.
    Review,
    /// The changes are approved, but opening their pull request failed and
    /// nothing will try again until someone does.
    PrNotCreated,
    /// A local integration of the approved changes was started and not
    /// finished: its plan is on disk and only a person can resume or discard
    /// it. Nothing continues it after a restart.
    IntegrationUnfinished,
}

impl AttentionReason {
    /// What a person reads, after "Needs you".
    pub fn label(self) -> &'static str {
        match self {
            Self::Failed => "Failed",
            Self::Review => "Review",
            Self::PrNotCreated => "PR not created",
            Self::IntegrationUnfinished => "Integration unfinished",
        }
    }

    /// The serialized name, for places that need it as a plain string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Review => "review",
            Self::PrNotCreated => "pr_not_created",
            Self::IntegrationUnfinished => "integration_unfinished",
        }
    }
}

/// A task's status, as far as attention tells statuses apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Error,
    HumanReview,
    Done,
    /// Backlog, Queue, In Progress, AI Review or PR Created.
    Other,
}

/// The Human Review decision made in the task's current arrival.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approved,
    ChangesRequested,
}

/// What the rule reads about one task, copied from its record as is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub status: Status,
    /// The decision in the current Human Review arrival, if any.
    pub decision: Option<Decision>,
    /// The record keeps why the last pull request attempt failed.
    pub pr_error_recorded: bool,
    /// A pull request is linked to the task: its `pr_url`, or a pull request
    /// among its external references.
    pub pr_linked: bool,
    /// Its pull request is being opened right now.
    pub delivery_in_flight: bool,
    /// The record holds a local integration that is still `Pending`.
    pub integration_pending: bool,
}

impl Facts {
    /// Whether the task needs the user now, and why.
    pub fn needs_you(self) -> Option<AttentionReason> {
        let stage = match self.status {
            Status::Error => Stage::Failed,
            Status::HumanReview => Stage::HumanReview(match self.decision {
                None => Review::Undecided,
                Some(Decision::ChangesRequested) => Review::ChangesRequested,
                Some(Decision::Approved) => Review::Approved {
                    pr_failed: self.pr_error_recorded,
                    pr_linked: self.pr_linked,
                },
            }),
            Status::Done => Stage::Done,
            Status::Other => Stage::Elsewhere,
        };
        needs_you(stage, self.delivery_in_flight, self.integration_pending)
    }
}

/// Where a task stands, as far as attention is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The task is in Error.
    Failed,
    /// The task is in Human Review.
    HumanReview(Review),
    /// The task is Done. Nothing is asked of the user unless a local
    /// integration of work completed earlier was left unfinished.
    Done,
    /// Any other status: waiting, queued, running, under AI review or with an
    /// open pull request. Either nothing is asked of the user or SlashIt is
    /// still working.
    Elsewhere,
}

/// What has been decided about the changes a task is in Human Review with,
/// in its current arrival there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Review {
    /// No decision yet.
    Undecided,
    /// Changes were requested. Requesting them also queues the task, so this
    /// is only seen on a task that was moved back into Human Review by hand
    /// before it ran again.
    ChangesRequested,
    /// The changes are approved.
    Approved {
        /// The last attempt to open their pull request failed.
        pr_failed: bool,
        /// A pull request is linked to the task, however it got there.
        pr_linked: bool,
    },
}

/// Whether `stage` needs the user now, and why.
///
/// `delivery_in_flight` is whether a pull request is being opened for the
/// task right now. A person started that, so while it runs the task is
/// waiting on SlashIt and not on them, whatever the record still says about
/// an earlier attempt.
///
/// `integration_pending` is whether the task's local integration record is
/// still `Pending`. Resume and Discard are offered with an approved Human
/// Review and with a Done task that was completed locally, so only those ask
/// for it. A failed task is repaired first. In a new, undecided arrival in
/// Human Review the review is what is asked; in any other status the record
/// waits for the task to come back. The unfinished
/// integration outranks every Human Review reason: the target branch may
/// already have moved. It is not hidden by `delivery_in_flight`, which
/// describes a pull request, never an integration.
pub fn needs_you(stage: Stage, delivery_in_flight: bool, integration_pending: bool) -> Option<AttentionReason> {
    match stage {
        Stage::Failed => Some(AttentionReason::Failed),
        Stage::HumanReview(Review::Approved { .. }) | Stage::Done if integration_pending => {
            Some(AttentionReason::IntegrationUnfinished)
        }
        Stage::HumanReview(_) if delivery_in_flight => None,
        Stage::HumanReview(Review::Undecided) => Some(AttentionReason::Review),
        Stage::HumanReview(Review::Approved { pr_failed: true, pr_linked: false }) => {
            Some(AttentionReason::PrNotCreated)
        }
        Stage::HumanReview(Review::Approved { .. } | Review::ChangesRequested) => None,
        Stage::Done | Stage::Elsewhere => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPROVED: Review = Review::Approved { pr_failed: false, pr_linked: false };
    const PR_FAILED: Review = Review::Approved { pr_failed: true, pr_linked: false };

    #[test]
    fn a_failed_task_needs_the_user() {
        assert_eq!(needs_you(Stage::Failed, false, false), Some(AttentionReason::Failed));
    }

    #[test]
    fn human_review_alone_is_not_attention_only_an_undecided_one_is() {
        assert_eq!(
            needs_you(Stage::HumanReview(Review::Undecided), false, false),
            Some(AttentionReason::Review)
        );
        assert_eq!(needs_you(Stage::HumanReview(APPROVED), false, false), None);
        assert_eq!(needs_you(Stage::HumanReview(Review::ChangesRequested), false, false), None);
    }

    #[test]
    fn an_approval_whose_pull_request_failed_needs_the_user() {
        assert_eq!(needs_you(Stage::HumanReview(PR_FAILED), false, false), Some(AttentionReason::PrNotCreated));
    }

    /// A failure recorded before a pull request was linked by other means
    /// (Sync PR) describes an attempt that no longer matters.
    #[test]
    fn a_linked_pull_request_outranks_an_earlier_failure() {
        let linked = Review::Approved { pr_failed: true, pr_linked: true };
        assert_eq!(needs_you(Stage::HumanReview(linked), false, false), None);
    }

    #[test]
    fn nothing_in_human_review_needs_the_user_while_its_pull_request_is_being_opened() {
        for review in [Review::Undecided, APPROVED, PR_FAILED] {
            assert_eq!(needs_you(Stage::HumanReview(review), true, false), None, "{review:?}");
        }
    }

    #[test]
    fn a_failure_is_not_hidden_by_a_delivery_flag() {
        assert_eq!(needs_you(Stage::Failed, true, false), Some(AttentionReason::Failed));
    }

    #[test]
    fn every_other_status_is_left_alone() {
        assert_eq!(needs_you(Stage::Elsewhere, false, false), None);
        assert_eq!(needs_you(Stage::Elsewhere, true, false), None);
    }

    #[test]
    fn facts_are_read_the_way_the_stages_say() {
        let facts = |status, decision, pr_error_recorded, pr_linked| Facts {
            status,
            decision,
            pr_error_recorded,
            pr_linked,
            delivery_in_flight: false,
            integration_pending: false,
        };
        let approved = Some(Decision::Approved);
        assert_eq!(facts(Status::Error, None, false, false).needs_you(), Some(AttentionReason::Failed));
        assert_eq!(facts(Status::HumanReview, None, false, false).needs_you(), Some(AttentionReason::Review));
        assert_eq!(facts(Status::HumanReview, Some(Decision::ChangesRequested), false, false).needs_you(), None);
        assert_eq!(facts(Status::HumanReview, approved, false, false).needs_you(), None);
        assert_eq!(facts(Status::HumanReview, approved, true, false).needs_you(), Some(AttentionReason::PrNotCreated));
        assert_eq!(facts(Status::HumanReview, approved, true, true).needs_you(), None);
        assert_eq!(facts(Status::Other, None, true, false).needs_you(), None);
        let in_flight = Facts { delivery_in_flight: true, ..facts(Status::HumanReview, approved, true, false) };
        assert_eq!(in_flight.needs_you(), None);
    }

    #[test]
    fn reasons_read_as_words_and_serialize_as_their_names() {
        for (reason, label, name) in [
            (AttentionReason::Failed, "Failed", "failed"),
            (AttentionReason::Review, "Review", "review"),
            (AttentionReason::PrNotCreated, "PR not created", "pr_not_created"),
            (AttentionReason::IntegrationUnfinished, "Integration unfinished", "integration_unfinished"),
        ] {
            assert_eq!(reason.label(), label);
            assert_eq!(reason.as_str(), name);
            assert_eq!(serde_json::to_string(&reason).unwrap(), format!("\"{name}\""));
        }
    }

    #[test]
    fn an_unfinished_integration_needs_the_user_in_human_review_and_when_done() {
        for stage in [Stage::HumanReview(APPROVED), Stage::Done] {
            assert_eq!(needs_you(stage, false, true), Some(AttentionReason::IntegrationUnfinished), "{stage:?}");
            assert_eq!(needs_you(stage, false, false), None, "{stage:?}");
        }
    }

    /// The drawer offers Resume and Discard next to an approval, so a new
    /// arrival that is not decided yet asks for its review, not for them.
    #[test]
    fn an_undecided_arrival_asks_for_its_review_even_with_an_old_pending_record() {
        assert_eq!(needs_you(Stage::HumanReview(Review::Undecided), false, true), Some(AttentionReason::Review));
        assert_eq!(needs_you(Stage::HumanReview(Review::ChangesRequested), false, true), None);
    }

    #[test]
    fn an_unfinished_integration_outranks_the_other_review_reasons_but_not_a_failure() {
        assert_eq!(needs_you(Stage::HumanReview(PR_FAILED), false, true), Some(AttentionReason::IntegrationUnfinished));
        assert_eq!(needs_you(Stage::Failed, false, true), Some(AttentionReason::Failed));
    }

    #[test]
    fn an_opening_pull_request_does_not_hide_an_unfinished_integration() {
        assert_eq!(
            needs_you(Stage::HumanReview(APPROVED), true, true),
            Some(AttentionReason::IntegrationUnfinished)
        );
    }

    #[test]
    fn an_integration_record_waits_silently_in_statuses_that_offer_no_way_to_finish_it() {
        assert_eq!(needs_you(Stage::Elsewhere, false, true), None);
    }

    #[test]
    fn facts_carry_the_pending_integration_through_the_status_they_read() {
        let facts = |status| Facts {
            status,
            decision: Some(Decision::Approved),
            pr_error_recorded: false,
            pr_linked: false,
            delivery_in_flight: false,
            integration_pending: true,
        };
        assert_eq!(facts(Status::HumanReview).needs_you(), Some(AttentionReason::IntegrationUnfinished));
        assert_eq!(facts(Status::Done).needs_you(), Some(AttentionReason::IntegrationUnfinished));
        assert_eq!(facts(Status::Error).needs_you(), Some(AttentionReason::Failed));
        assert_eq!(facts(Status::Other).needs_you(), None);
        let integrated = Facts { integration_pending: false, ..facts(Status::Done) };
        assert_eq!(integrated.needs_you(), None, "an Integrated or absent record asks nothing");
    }
}
