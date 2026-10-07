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
pub struct Snapshot { pub conversation: Conversation, pub coordinator_live: bool, pub worker_live: bool, pub run_status: Option<ConversationRunStatus> }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HumanAction { Approve { request: Option<String> }, Reject }
