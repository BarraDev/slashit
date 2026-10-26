// SPDX-License-Identifier: MPL-2.0
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role { Human, Coordinator, Worker }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus { Proposed, Approved, Running, Returned, Rejected, Interrupted, Failed, Withdrawn }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAction { pub id: Uuid, pub target_task_id: Uuid, pub target_task_title: String, pub request: String, pub explanation: String, pub approved_request: Option<String>, pub status: ActionStatus, pub worker_result: Option<String>, pub coordinator_replied: bool, pub created_at: String }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind { HumanMessage { text: String }, CoordinatorReply { text: String }, ActionProposed { action_id: Uuid }, ActionDecision { action_id: Uuid, approved: bool, request: Option<String> }, WorkerStarted { action_id: Uuid }, WorkerResult { action_id: Uuid, result: String }, RunFailed { role: Role, message: String, #[serde(default)] retryable: bool }, ProjectActionProposed { action_id: Uuid }, ProjectActionDecision { action_id: Uuid, approved: bool }, ProjectActionApplied { action_id: Uuid, task_id: Uuid }, ProjectActionRefused { action_id: Uuid, reason: String }, ActionWithdrawn { action_id: Uuid, revision: u64 } }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry { pub id: Uuid, pub role: Role, pub kind: EntryKind, pub created_at: String }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation { pub id: Uuid, pub project_id: Uuid, pub revision: u64, pub created_at: String, pub updated_at: String, pub entries: Vec<Entry>, pub actions: Vec<TaskAction>, #[serde(default)] pub project_actions: Vec<ProjectAction> }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectActionStatus { Proposed, Approved, Applied, Rejected, Refused }
/// Priority and category stay as their snake_case wire strings: the card only displays them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "field", rename_all = "snake_case")]
pub enum FieldChange { Title { from: String, to: String }, Description { from: Option<String>, to: String }, Priority { from: String, to: String }, Category { from: String, to: String } }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskMutation { CreateTask { task_id: Uuid, title: String, description: Option<String>, priority: String, category: String }, EditTask { target_task_id: Uuid, target_task_title: String, changes: Vec<FieldChange> }, MoveTask { target_task_id: Uuid, target_task_title: String, from: String, to: String }, EnqueueTask { target_task_id: Uuid, target_task_title: String, from: String, #[serde(default)] input: Option<ExecutionInput> }, ExecuteRequest { target_task_id: Uuid, target_task_title: String, from: String, request: String, input: ExecutionInput, #[serde(default)] checkout: Option<ExistingCheckout> } }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionInput { pub digest: String, pub description: Option<String>, pub subtask_count: usize, #[serde(default)] pub model: Option<String> }
/// The Task Checkout an additional request reuses, as the Coordinator proposal inspected it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExistingCheckout { pub checkout_path: String, pub branch: String, pub head: String, pub base_commit: String }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ProjectActionOutcome { Applied { task_id: Uuid, title: String }, AlreadyQueued { task_id: Uuid, title: String }, Refused { reason: String } }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectAction { pub id: Uuid, pub explanation: String, pub mutation: TaskMutation, pub status: ProjectActionStatus, #[serde(default)] pub outcome: Option<ProjectActionOutcome>, pub created_at: String }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRunStatus { Running }
/// What a Task run started by an approved Project action has become. Mirrors the backend projection (`conversation_executions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationExecutionState { PendingApproval, Queued, Running, Unconfirmed, Completed, Failed, RecoveryRequired, Ambiguous, NoExecutionEvidence, TaskUnavailable }
/// Text from a run or a proposal. Untrusted: it is shown as plain text only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationBoundedText { pub text: String, pub truncated: bool, pub original_chars: u32 }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ConversationExecutionResult { Available { text: ConversationBoundedText }, Truncated { text: ConversationBoundedText }, Unavailable { reason: ConversationBoundedText }, NotRecorded, NotApplicable }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationExecution { pub action_id: Uuid, pub task_id: Uuid, pub task_title: String, pub state: ConversationExecutionState, pub transitional: bool, pub run: Option<u32>, pub result: ConversationExecutionResult, pub needs_human: bool, pub attention: Option<crate::models::AttentionReason>, pub explanation: ConversationBoundedText }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub conversation: Conversation,
    pub coordinator_live: bool,
    pub worker_live: bool,
    pub run_status: Option<ConversationRunStatus>,
    #[serde(default)]
    pub continuation_error: Option<String>,
    /// True when the backend would re-run the Coordinator for the unanswered message.
    #[serde(default)]
    pub turn_retry_available: bool,
    /// Absent in snapshots written before execution visibility existed.
    #[serde(default)]
    pub executions: Vec<ConversationExecution>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HumanAction { Approve { request: Option<String> }, Reject, Withdraw }

#[cfg(test)]
mod tests {
    use super::Snapshot;
    use serde_json::json;
    use uuid::Uuid;

    fn snapshot_json() -> serde_json::Value {
        json!({
            "conversation": {
                "id": Uuid::new_v4(), "project_id": Uuid::new_v4(), "revision": 0,
                "created_at": "", "updated_at": "", "entries": [], "actions": []
            },
            "coordinator_live": false, "worker_live": false, "run_status": null
        })
    }

    #[test]
    fn continuation_error_is_typed_and_optional_for_existing_snapshots() {
        let legacy: Snapshot = serde_json::from_value(snapshot_json()).expect("legacy snapshot should decode");
        assert!(legacy.continuation_error.is_none());
        assert!(legacy.executions.is_empty(), "a snapshot from before execution visibility has none");

        let mut failed = snapshot_json();
        failed["continuation_error"] = json!("Coordinator unavailable; retry from saved result");
        let failed: Snapshot = serde_json::from_value(failed).expect("continuation failure should decode");
        assert_eq!(failed.continuation_error.as_deref(), Some("Coordinator unavailable; retry from saved result"));
    }

    #[test]
    fn run_failure_retryable_flag_defaults_to_false_for_saved_entries() {
        use super::EntryKind;
        let legacy: EntryKind = serde_json::from_value(json!({"kind": "run_failed", "role": "coordinator", "message": "older failure"}))
            .expect("a failure saved before the flag existed should decode");
        assert!(matches!(legacy, EntryKind::RunFailed { retryable: false, .. }));

        let retryable: EntryKind = serde_json::from_value(json!({"kind": "run_failed", "role": "coordinator", "message": "retry me", "retryable": true}))
            .expect("a retryable failure should decode");
        assert!(matches!(retryable, EntryKind::RunFailed { retryable: true, .. }));
    }
}
