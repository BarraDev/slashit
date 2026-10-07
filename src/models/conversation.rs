use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role { Human, Coordinator, Worker }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus { Proposed, Approved, Running, Returned, Rejected, Interrupted, Failed }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAction { pub id: Uuid, pub target_task_id: Uuid, pub target_task_title: String, pub request: String, pub explanation: String, pub approved_request: Option<String>, pub status: ActionStatus, pub worker_result: Option<String>, pub coordinator_replied: bool, pub created_at: String }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind { HumanMessage { text: String }, CoordinatorReply { text: String }, ActionProposed { action_id: Uuid }, ActionDecision { action_id: Uuid, approved: bool, request: Option<String> }, WorkerStarted { action_id: Uuid }, WorkerResult { action_id: Uuid, result: String }, RunFailed { role: Role, message: String } }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry { pub id: Uuid, pub role: Role, pub kind: EntryKind, pub created_at: String }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation { pub id: Uuid, pub project_id: Uuid, pub revision: u64, pub created_at: String, pub updated_at: String, pub entries: Vec<Entry>, pub actions: Vec<TaskAction> }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRunStatus { Running }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot { pub conversation: Conversation, pub coordinator_live: bool, pub worker_live: bool, pub run_status: Option<ConversationRunStatus>, #[serde(default)] pub continuation_error: Option<String> }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HumanAction { Approve { request: Option<String> }, Reject }

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

        let mut failed = snapshot_json();
        failed["continuation_error"] = json!("Coordinator unavailable; retry from saved result");
        let failed: Snapshot = serde_json::from_value(failed).expect("continuation failure should decode");
        assert_eq!(failed.continuation_error.as_deref(), Some("Coordinator unavailable; retry from saved result"));
    }
}
