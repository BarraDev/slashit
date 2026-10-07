//! Durable, Project-owned conversation state. Provider processes and their
//! output streams are deliberately not part of this model.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MESSAGE_LIMIT: usize = 16_000;
pub const PROJECTION_MESSAGE_COUNT: usize = 16;
pub const TASK_INDEX_LIMIT: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Human,
    Coordinator,
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Proposed,
    Approved,
    Running,
    Returned,
    Rejected,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAction {
    pub id: Uuid,
    pub target_task_id: Uuid,
    pub target_task_title: String,
    pub request: String,
    pub explanation: String,
    pub approved_request: Option<String>,
    pub status: ActionStatus,
    pub worker_result: Option<String>,
    #[serde(default)]
    pub coordinator_replied: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind {
    HumanMessage {
        text: String,
    },
    CoordinatorReply {
        text: String,
    },
    ActionProposed {
        action_id: Uuid,
    },
    ActionDecision {
        action_id: Uuid,
        approved: bool,
        request: Option<String>,
    },
    WorkerStarted {
        action_id: Uuid,
    },
    WorkerResult {
        action_id: Uuid,
        result: String,
    },
    RunFailed {
        role: Role,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: Uuid,
    pub role: Role,
    pub kind: EntryKind,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    pub project_id: Uuid,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub entries: Vec<Entry>,
    pub actions: Vec<TaskAction>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoordinatorOutput {
    Reply {
        text: String,
    },
    DelegateToTask {
        text: String,
        target_task_id: Uuid,
        request: String,
    },
}

impl Conversation {
    pub fn new(project_id: Uuid) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            project_id,
            revision: 0,
            created_at: now,
            updated_at: now,
            entries: Vec::new(),
            actions: Vec::new(),
        }
    }

    pub fn push(&mut self, role: Role, kind: EntryKind) {
        let now = Utc::now();
        self.entries.push(Entry {
            id: Uuid::new_v4(),
            role,
            kind,
            created_at: now,
        });
        self.revision = self.revision.saturating_add(1);
        self.updated_at = now;
    }

    pub fn has_unmediated_worker_result(&self) -> bool {
        self.actions
            .iter()
            .any(|action| action.status == ActionStatus::Returned && !action.coordinator_replied)
    }

    pub fn validate_text(value: &str) -> Result<(), String> {
        if value.trim().is_empty() || value.len() > MESSAGE_LIMIT {
            return Err(format!("Text must contain 1–{MESSAGE_LIMIT} bytes"));
        }
        Ok(())
    }

    pub fn parse_output(raw: &str) -> Result<CoordinatorOutput, String> {
        if raw.len() > MESSAGE_LIMIT * 2 + 512 {
            return Err("Coordinator response is too large".into());
        }
        let output: CoordinatorOutput = serde_json::from_str(raw)
            .map_err(|error| format!("Invalid Coordinator JSON: {error}"))?;
        match &output {
            CoordinatorOutput::Reply { text } => Self::validate_text(text)?,
            CoordinatorOutput::DelegateToTask { text, request, .. } => {
                Self::validate_text(text)?;
                Self::validate_text(request)?;
            }
        }
        Ok(output)
    }

    pub fn coordinator_projection(
        &self,
        current_message: &str,
        project_name: &str,
        project_root: Option<&str>,
        tasks: &[serde_json::Value],
    ) -> serde_json::Value {
        self.coordinator_projection_before(current_message, project_name, project_root, tasks, None)
    }

    pub fn coordinator_projection_before(
        &self,
        current_message: &str,
        project_name: &str,
        project_root: Option<&str>,
        tasks: &[serde_json::Value],
        exclude_entry: Option<Uuid>,
    ) -> serde_json::Value {
        let recent = self.entries.iter().filter(|entry| Some(entry.id) != exclude_entry).rev()
            .filter_map(|entry| match &entry.kind {
                EntryKind::HumanMessage { text } | EntryKind::CoordinatorReply { text } =>
                    Some(serde_json::json!({"kind":"message", "role": entry.role, "text": text.chars().take(4000).collect::<String>()})),
                EntryKind::ActionProposed { action_id } => self.actions.iter().find(|action| action.id == *action_id).map(|action|
                    serde_json::json!({"kind":"action_proposed", "target_task_id":action.target_task_id,"target_task_title":action.target_task_title.chars().take(500).collect::<String>(),"request":action.request.chars().take(3000).collect::<String>(),"explanation":action.explanation.chars().take(2000).collect::<String>()})),
                EntryKind::ActionDecision { action_id, approved, request } => self.actions.iter().find(|action| action.id == *action_id).map(|action|
                    serde_json::json!({"kind":"action_decision","approved":approved,"target_task_id":action.target_task_id,"target_task_title":action.target_task_title.chars().take(500).collect::<String>(),"request":request.as_deref().unwrap_or(&action.request).chars().take(3000).collect::<String>()})),
                EntryKind::WorkerStarted { action_id } => Some(serde_json::json!({"kind":"worker_started","action_id":action_id})),
                EntryKind::WorkerResult { action_id, result } => Some(serde_json::json!({"kind":"worker_result","action_id":action_id,"result":result.chars().take(3000).collect::<String>()})),
                EntryKind::RunFailed { role, message } => Some(serde_json::json!({"kind":"run_failed","role":role,"message":message.chars().take(1000).collect::<String>()})),
            }).take(PROJECTION_MESSAGE_COUNT).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>();
        serde_json::json!({
            "project": {"name": project_name, "root": project_root},
            "current_message": current_message,
            "recent_history": recent,
            "tasks": tasks.iter().take(TASK_INDEX_LIMIT).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_a_strict_discriminated_contract() {
        assert!(matches!(
            Conversation::parse_output(r#"{"type":"reply","text":"hello"}"#),
            Ok(CoordinatorOutput::Reply { .. })
        ));
        assert!(Conversation::parse_output(r#"{"type":"execute","text":"run it"}"#).is_err());
        assert!(
            Conversation::parse_output(r#"{"type":"reply","text":"hi","task_id":"x"}"#).is_err()
        );
    }

    #[test]
    fn projection_includes_bounded_semantic_action_history_and_excludes_current_turn() {
        let mut conversation = Conversation::new(Uuid::new_v4());
        conversation.push(Role::Human, EntryKind::HumanMessage { text: "one".into() });
        conversation.push(
            Role::Coordinator,
            EntryKind::CoordinatorReply { text: "two".into() },
        );
        let task_id = Uuid::new_v4();
        let action_id = Uuid::new_v4();
        conversation.actions.push(TaskAction {
            id: action_id,
            target_task_id: task_id,
            target_task_title: "Task A".into(),
            request: "change it".into(),
            explanation: "best next step".into(),
            approved_request: None,
            status: ActionStatus::Rejected,
            worker_result: None,
            coordinator_replied: false,
            created_at: Utc::now(),
        });
        conversation.push(Role::Coordinator, EntryKind::ActionProposed { action_id });
        conversation.push(
            Role::Human,
            EntryKind::ActionDecision {
                action_id,
                approved: false,
                request: None,
            },
        );
        let prior_history = conversation.clone();
        conversation.push(
            Role::Human,
            EntryKind::HumanMessage {
                text: "why?".into(),
            },
        );
        let current_id = conversation.entries.last().unwrap().id;
        let projection = prior_history.coordinator_projection_before(
            "why?",
            "Project",
            None,
            &[],
            Some(current_id),
        );
        assert_eq!(projection["recent_history"].as_array().unwrap().len(), 4);
        assert!(projection.to_string().contains("action_proposed"));
        assert!(projection.to_string().contains("action_decision"));
        assert!(projection.to_string().contains("Task A"));
        assert!(!projection["recent_history"].to_string().contains("why?"));
    }

    #[test]
    fn returned_worker_result_blocks_new_turn_until_coordinator_mediates_it() {
        let mut conversation = Conversation::new(Uuid::new_v4());
        let now = Utc::now();
        conversation.actions.push(TaskAction {
            id: Uuid::new_v4(),
            target_task_id: Uuid::new_v4(),
            target_task_title: "Task".into(),
            request: "request".into(),
            explanation: "why".into(),
            approved_request: Some("request".into()),
            status: ActionStatus::Returned,
            worker_result: Some("result".into()),
            coordinator_replied: false,
            created_at: now,
        });
        assert!(conversation.has_unmediated_worker_result());
        conversation.actions[0].coordinator_replied = true;
        assert!(!conversation.has_unmediated_worker_result());
    }
}
