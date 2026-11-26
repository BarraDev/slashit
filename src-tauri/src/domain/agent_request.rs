// SPDX-License-Identifier: MPL-2.0
//! A request an external agent makes for one operation on one Task, and what a
//! person decided about it. See
//! `docs/architecture/agent-review-delivery.md` for the threat model.
//!
//! The record is the authorization. Nothing secret is handed to the agent: a
//! request id names a record, and the record names the only Task, commit,
//! operation and Human Review arrival it can ever be used for. It is stored on
//! the Task, so it is written with the same durable board write as the rest of
//! the Task and survives a restart.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::task::{Task, TaskStatus};

/// How long a request waits for a person before it expires.
pub const REQUEST_LIFETIME_MINUTES: i64 = 30;
/// How long a person's authorization of a request can be used.
pub const AUTHORIZATION_LIFETIME_MINUTES: i64 = 10;
/// The most requests kept on one Task. Only requests that never authorized
/// anything are dropped to make room.
pub const MAX_REQUESTS_PER_TASK: usize = 50;
/// The longest name an agent may give itself.
pub const MAX_ACTOR_CHARS: usize = 64;

/// What an agent asks for. Deliberately a closed list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestedOperation {
    /// A person approves the changes. The person's decision in the desktop app
    /// records the approval; the agent executes nothing.
    Approve,
    /// A person authorizes the agent to open the pull request, once.
    OpenPr,
}

impl RequestedOperation {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "approve" => Some(Self::Approve),
            "open_pr" | "open-pr" => Some(Self::OpenPr),
            _ => None,
        }
    }
}

/// What a person decided in the desktop app. No other path writes one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanDecision {
    pub authorized: bool,
    pub decided_at: DateTime<Utc>,
    /// Until when an authorized operation may be started. Absent for a refusal
    /// and for an approval, which the decision itself carries out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// How the one use of an authorization ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UseOutcome {
    PrOpened { url: String },
    Failed { reason: String },
}

/// The one use of an authorization. Written before the operation has any
/// effect, so a crash leaves a used authorization whose outcome is unknown,
/// never one that can be used again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestUse {
    /// The name the executing agent gave itself. Not authenticated.
    pub executed_by: String,
    pub started_at: DateTime<Utc>,
    /// Absent while the operation runs, and for good if the process ended
    /// before it could record the outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<UseOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRequest {
    pub id: Uuid,
    pub operation: RequestedOperation,
    /// The full object ID of the only commit this request is about.
    pub commit: String,
    /// The Human Review arrival it was made in. A Task that went back to work
    /// leaves that arrival, and the request with it.
    pub arrival: u32,
    /// The name the requesting agent gave itself. Not authenticated.
    pub requested_by: String,
    pub requested_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// Set when the branch was seen to have left `commit`. It is never cleared,
    /// so a branch that later returns to the commit does not revive it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalidated_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<HumanDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<RequestUse>,
}

/// Where a request stands. Derived, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    /// Waiting for a person.
    Pending,
    /// A person approved the changes. Nothing is left to execute.
    Approved,
    /// A person authorized the operation and it has not been started.
    Authorized,
    Refused,
    Expired,
    /// The Task or its branch is no longer what the request names.
    Invalidated,
    /// The authorization was consumed. See [`AgentRequest::used`].
    Used,
}

impl RequestState {
    /// Whether the request can still lead to something.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::Authorized)
    }
}

impl AgentRequest {
    pub fn new(
        operation: RequestedOperation,
        commit: String,
        arrival: u32,
        requested_by: String,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            operation,
            commit,
            arrival,
            requested_by,
            requested_at: now,
            expires_at: now + Duration::minutes(REQUEST_LIFETIME_MINUTES),
            invalidated_at: None,
            decision: None,
            used: None,
        }
    }

    /// Why the Task, as recorded, is not what this request names. The branch
    /// itself is checked by the caller, which can read Git.
    pub fn binding_refusal(&self, task: &Task) -> Option<&'static str> {
        if task.status != TaskStatus::HumanReview {
            return Some("The task is not in Human Review.");
        }
        if task.human_review.arrivals != self.arrival {
            return Some("The task went back to work since this request was made.");
        }
        match self.operation {
            RequestedOperation::Approve => task
                .human_review
                .current_decision()
                .is_some()
                .then_some("A decision about these changes is already recorded."),
            RequestedOperation::OpenPr => {
                if task.human_review.approved_tip() != Some(self.commit.as_str()) {
                    Some("A person has not approved exactly this commit.")
                } else if task.authoritative_pr_url().is_some() {
                    Some("The task already has a pull request.")
                } else {
                    None
                }
            }
        }
    }

    /// Where the request stands at `now`. `branch_tip` is the branch commit
    /// when the caller read it.
    pub fn state(&self, task: &Task, branch_tip: Option<&str>, now: DateTime<Utc>) -> RequestState {
        if self.used.is_some() {
            return RequestState::Used;
        }
        if self.invalidated_at.is_some() {
            return RequestState::Invalidated;
        }
        let deadline = match &self.decision {
            Some(d) if !d.authorized => return RequestState::Refused,
            Some(_) if self.operation == RequestedOperation::Approve => {
                return RequestState::Approved
            }
            // An authorization without a deadline is not one.
            Some(d) => match d.expires_at {
                Some(at) => at,
                None => return RequestState::Invalidated,
            },
            None => self.expires_at,
        };
        if now >= deadline {
            return RequestState::Expired;
        }
        if self.binding_refusal(task).is_some() || branch_tip.is_some_and(|tip| tip != self.commit)
        {
            return RequestState::Invalidated;
        }
        if self.decision.is_some() {
            RequestState::Authorized
        } else {
            RequestState::Pending
        }
    }
}

/// An agent's name for itself, as it is stored and shown to a person.
pub fn check_actor(actor: &str) -> Result<(), String> {
    let plain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.');
    if (1..=MAX_ACTOR_CHARS).contains(&actor.len()) && actor.bytes().all(plain) {
        Ok(())
    } else {
        Err(format!(
            "The actor name is 1 to {MAX_ACTOR_CHARS} letters, digits, hyphens, dots or underscores."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER: &str = "89abcdef0123456789abcdef0123456789abcdef";

    fn approved_task() -> Task {
        let mut task = crate::test_helpers::create_test_task_full(
            "requested",
            Uuid::new_v4(),
            TaskStatus::HumanReview,
            0,
        );
        task.human_review.record_arrival();
        task.human_review.approve(Some(COMMIT.to_string()), Utc::now());
        task
    }

    fn open_pr_request(task: &Task, now: DateTime<Utc>) -> AgentRequest {
        AgentRequest::new(
            RequestedOperation::OpenPr,
            COMMIT.to_string(),
            task.human_review.arrivals,
            "agent".to_string(),
            now,
        )
    }

    fn authorize(request: &mut AgentRequest, now: DateTime<Utc>) {
        request.decision = Some(HumanDecision {
            authorized: true,
            decided_at: now,
            expires_at: Some(now + Duration::minutes(AUTHORIZATION_LIFETIME_MINUTES)),
        });
    }

    #[test]
    fn a_request_and_an_authorization_each_expire() {
        let task = approved_task();
        let now = Utc::now();
        let mut request = open_pr_request(&task, now);
        let waited = now + Duration::minutes(REQUEST_LIFETIME_MINUTES);
        assert_eq!(request.state(&task, Some(COMMIT), now), RequestState::Pending);
        assert_eq!(request.state(&task, Some(COMMIT), waited), RequestState::Expired);

        authorize(&mut request, now);
        let used_by = now + Duration::minutes(AUTHORIZATION_LIFETIME_MINUTES);
        assert_eq!(
            request.state(&task, Some(COMMIT), used_by - Duration::seconds(1)),
            RequestState::Authorized
        );
        assert_eq!(request.state(&task, Some(COMMIT), used_by), RequestState::Expired);
    }

    #[test]
    fn a_moved_branch_a_new_arrival_or_another_approval_invalidates() {
        let task = approved_task();
        let now = Utc::now();
        let mut request = open_pr_request(&task, now);
        authorize(&mut request, now);
        assert_eq!(request.state(&task, Some(OTHER), now), RequestState::Invalidated);

        let mut returned = task.clone();
        returned.human_review.record_arrival();
        returned.human_review.approve(Some(COMMIT.to_string()), now);
        assert_eq!(request.state(&returned, Some(COMMIT), now), RequestState::Invalidated);

        let mut renewed = task.clone();
        renewed.human_review.approve_again(OTHER.to_string(), now);
        assert_eq!(request.state(&renewed, None, now), RequestState::Invalidated);

        // Seen moved once, it stays invalid even when the branch comes back.
        request.invalidated_at = Some(now);
        assert_eq!(request.state(&task, Some(COMMIT), now), RequestState::Invalidated);
    }

    #[test]
    fn actor_names_are_bounded_and_plain() {
        assert!(check_actor("claude-code.2_a").is_ok());
        for bad in ["", "a b", "a\nb", &"a".repeat(MAX_ACTOR_CHARS + 1)] {
            assert!(check_actor(bad).is_err(), "{bad:?}");
        }
    }
}
