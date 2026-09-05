use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecution {
    pub id: Uuid,
    pub worktree_id: Option<Uuid>,
    pub task_id: Option<Uuid>,
    pub agent_type: String,
    pub status: AgentStatus,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub stopped_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLogEntry {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json(worktree_id: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "worktree_id": worktree_id,
            "task_id": null,
            "agent_type": "claude",
            "status": "running",
            "started_at": "2026-09-05T00:00:00Z",
            "stopped_at": null,
        })
    }

    #[test]
    fn deserializes_worktree_id_when_present() {
        let json = sample_json(serde_json::json!("00000000-0000-0000-0000-000000000002"));
        let exec: AgentExecution = serde_json::from_value(json).expect("should deserialize");
        assert_eq!(
            exec.worktree_id,
            Some(Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap())
        );
    }

    #[test]
    fn deserializes_worktree_id_null_as_none() {
        let json = sample_json(serde_json::Value::Null);
        let exec: AgentExecution = serde_json::from_value(json).expect("should deserialize");
        assert_eq!(exec.worktree_id, None);
    }

    #[test]
    fn round_trips_worktree_id_through_serialize_and_deserialize() {
        let exec = AgentExecution {
            id: Uuid::nil(),
            worktree_id: Some(Uuid::nil()),
            task_id: None,
            agent_type: "claude".to_string(),
            status: AgentStatus::Running,
            started_at: chrono::Utc::now(),
            stopped_at: None,
        };

        let value = serde_json::to_value(&exec).expect("should serialize");
        // Confirm the wire field is named `worktree_id`, not `workspace_id`.
        assert!(value.get("worktree_id").is_some());
        assert!(value.get("workspace_id").is_none());

        let round_tripped: AgentExecution =
            serde_json::from_value(value).expect("should deserialize");
        assert_eq!(round_tripped.worktree_id, exec.worktree_id);
    }
}
