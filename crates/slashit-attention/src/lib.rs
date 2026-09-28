//! Which tasks cannot make progress without the user right now.
//!
//! Attention is derived, never stored: it follows from a task's status and
//! its Human Review record, which are already persisted, plus one fact that
//! only the running process knows -- whether a pull request is being opened
//! for the task at this moment. Nothing here is written to a task file, so
//! the answer after a restart is whatever the persisted record says it is.
//!
//! This crate is the rule itself. The backend, the WASM frontend and the CLI
//! each describe a task to [`needs_you`] as a [`Stage`]; none of them decides
//! on its own which combinations need the user.

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
}

impl AttentionReason {
    /// What a person reads, after "Needs you".
    pub fn label(self) -> &'static str {
        match self {
            Self::Failed => "Failed",
            Self::Review => "Review",
            Self::PrNotCreated => "PR not created",
        }
    }

    /// The serialized name, for places that need it as a plain string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Review => "review",
            Self::PrNotCreated => "pr_not_created",
        }
    }
}

/// Where a task stands, as far as attention is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The task is in Error.
    Failed,
    /// The task is in Human Review.
    HumanReview(Review),
    /// Any other status: waiting, queued, running, under AI review, with an
    /// open pull request, or done. Either nothing is asked of the user or
    /// SlashIt is still working.
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
pub fn needs_you(stage: Stage, delivery_in_flight: bool) -> Option<AttentionReason> {
    match stage {
        Stage::Failed => Some(AttentionReason::Failed),
        Stage::HumanReview(_) if delivery_in_flight => None,
        Stage::HumanReview(Review::Undecided) => Some(AttentionReason::Review),
        Stage::HumanReview(Review::Approved { pr_failed: true, pr_linked: false }) => {
            Some(AttentionReason::PrNotCreated)
        }
        Stage::HumanReview(Review::Approved { .. } | Review::ChangesRequested) => None,
        Stage::Elsewhere => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPROVED: Review = Review::Approved { pr_failed: false, pr_linked: false };
    const PR_FAILED: Review = Review::Approved { pr_failed: true, pr_linked: false };

    #[test]
    fn a_failed_task_needs_the_user() {
        assert_eq!(needs_you(Stage::Failed, false), Some(AttentionReason::Failed));
    }

    #[test]
    fn human_review_alone_is_not_attention_only_an_undecided_one_is() {
        assert_eq!(
            needs_you(Stage::HumanReview(Review::Undecided), false),
            Some(AttentionReason::Review)
        );
        assert_eq!(needs_you(Stage::HumanReview(APPROVED), false), None);
        assert_eq!(needs_you(Stage::HumanReview(Review::ChangesRequested), false), None);
    }

    #[test]
    fn an_approval_whose_pull_request_failed_needs_the_user() {
        assert_eq!(needs_you(Stage::HumanReview(PR_FAILED), false), Some(AttentionReason::PrNotCreated));
    }

    /// A failure recorded before a pull request was linked by other means
    /// (Sync PR) describes an attempt that no longer matters.
    #[test]
    fn a_linked_pull_request_outranks_an_earlier_failure() {
        let linked = Review::Approved { pr_failed: true, pr_linked: true };
        assert_eq!(needs_you(Stage::HumanReview(linked), false), None);
    }

    #[test]
    fn nothing_in_human_review_needs_the_user_while_its_pull_request_is_being_opened() {
        for review in [Review::Undecided, APPROVED, PR_FAILED] {
            assert_eq!(needs_you(Stage::HumanReview(review), true), None, "{review:?}");
        }
    }

    #[test]
    fn a_failure_is_not_hidden_by_a_delivery_flag() {
        assert_eq!(needs_you(Stage::Failed, true), Some(AttentionReason::Failed));
    }

    #[test]
    fn every_other_status_is_left_alone() {
        assert_eq!(needs_you(Stage::Elsewhere, false), None);
        assert_eq!(needs_you(Stage::Elsewhere, true), None);
    }

    #[test]
    fn reasons_read_as_words_and_serialize_as_their_names() {
        for (reason, label, name) in [
            (AttentionReason::Failed, "Failed", "failed"),
            (AttentionReason::Review, "Review", "review"),
            (AttentionReason::PrNotCreated, "PR not created", "pr_not_created"),
        ] {
            assert_eq!(reason.label(), label);
            assert_eq!(reason.as_str(), name);
            assert_eq!(serde_json::to_string(&reason).unwrap(), format!("\"{name}\""));
        }
    }
}
