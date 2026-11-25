// SPDX-License-Identifier: MPL-2.0
//! The durable record of integrating a Task's approved work into a local
//! branch.
//!
//! Integration is a delivery fact about a Task, not a lifecycle status: a
//! Task is Done when it is closed, and says nothing about whether its work
//! reached any branch. This record is written before a branch moves and
//! finished after, so that a crash at any point leaves enough on disk to
//! tell which of the two happened. See `commands::integration`.

use serde::{Deserialize, Serialize};

/// How the target branch was (or will be) advanced.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationStrategy {
    /// The target was an ancestor of the approved commit, so it moved
    /// straight to it. No commit was created.
    FastForward,
    /// The two lines had diverged; a merge commit with the target and the
    /// approved commit as its parents was created. Neither line was
    /// rewritten.
    MergeCommit,
    /// The target already contained the approved commit, so nothing moved.
    /// Recorded only when a person asked for the integration to be
    /// recognised.
    AlreadyContained,
}

impl IntegrationStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FastForward => "fast_forward",
            Self::MergeCommit => "merge_commit",
            Self::AlreadyContained => "already_contained",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationState {
    /// The plan is on disk and the target may or may not have moved. The
    /// target branch itself says which.
    Pending,
    /// The target held `result_oid` when this was recorded.
    Integrated,
}

/// One integration of a Task's approved commit into a local branch.
///
/// `approved_tip` names the exact commit that was integrated, never "the
/// branch": a later integration of newer approved work replaces this record
/// (the activity timeline keeps the earlier one), and whether a Task's
/// current approved work is integrated is always asked of Git against that
/// commit, never read from this record alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalIntegration {
    pub state: IntegrationState,
    /// The local branch that was advanced.
    pub target_branch: String,
    /// The commit `target_branch` named when the plan was made. The branch
    /// is only ever moved from this commit.
    pub target_old_oid: String,
    /// The approved commit being integrated.
    pub approved_tip: String,
    /// The commit `target_branch` is advanced to.
    pub result_oid: String,
    pub strategy: IntegrationStrategy,
    /// The checkout that had `target_branch` checked out when a person
    /// confirmed advancing it there, so that resuming moves the branch in
    /// that checkout and in no other. `None` when the branch was checked out
    /// nowhere, which is also what every earlier record loads as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_path: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The person asked to complete the Task with this integration (Complete
    /// and Integrate from Human Review), so finishing the integration also
    /// completes the Task. Every earlier record loads as `false`: an
    /// integration of a Task that was already completed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub completes_task: bool,
}

impl LocalIntegration {
    pub fn is_pending(&self) -> bool {
        self.state == IntegrationState::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(completes_task: bool) -> LocalIntegration {
        LocalIntegration {
            state: IntegrationState::Integrated,
            target_branch: "main".into(),
            target_old_oid: "a".repeat(40),
            approved_tip: "b".repeat(40),
            result_oid: "c".repeat(40),
            strategy: IntegrationStrategy::FastForward,
            checkout_path: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            completes_task,
        }
    }

    #[test]
    fn a_record_written_before_completion_was_part_of_it_loads_as_not_completing() {
        let mut value = serde_json::to_value(record(false)).unwrap();
        assert!(value.get("completes_task").is_none(), "an integration after completion writes no new key");
        value.as_object_mut().unwrap().remove("completes_task");
        let loaded: LocalIntegration = serde_json::from_value(value).unwrap();
        assert!(!loaded.completes_task);
    }

    #[test]
    fn the_intent_to_complete_is_written_and_read_back() {
        let value = serde_json::to_value(record(true)).unwrap();
        assert_eq!(value.get("completes_task"), Some(&serde_json::json!(true)));
        let loaded: LocalIntegration = serde_json::from_value(value).unwrap();
        assert!(loaded.completes_task);
    }
}
