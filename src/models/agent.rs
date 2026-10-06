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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentLogEntry {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// One `agent-event` from the backend's task executor.
///
/// Mirrors `AgentEvent` in `src-tauri/src/queue/executor.rs`. Every variant
/// names the task it is about, which is what lets a listener ignore events
/// for any other task.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A line SlashIt writes about the run (not the agent's own words).
    Log { task_id: String, level: LogLevel, message: String },
    /// Text the agent itself wrote.
    Output { task_id: String, text: String },
    PhaseChange { task_id: String, phase: crate::models::TaskPhase, progress: u8 },
    ToolUse { task_id: String, tool: String },
    Completed { task_id: String, success: bool, message: Option<String> },
    Error { task_id: String, message: String },
    /// A run SlashIt owns changed state. `starting`, `running` and `stopping`
    /// mean it owns the run now; `stopped` and `failed` mean the run is gone.
    RunState { task_id: String, status: AgentStatus },
}

impl AgentEvent {
    pub fn task_id(&self) -> &str {
        match self {
            Self::Log { task_id, .. }
            | Self::Output { task_id, .. }
            | Self::PhaseChange { task_id, .. }
            | Self::ToolUse { task_id, .. }
            | Self::Completed { task_id, .. }
            | Self::Error { task_id, .. }
            | Self::RunState { task_id, .. } => task_id,
        }
    }
}

/// What `get_task_run` reports: whether an agent is working on the task now,
/// and the task's latest execution in this session.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct TaskRunSnapshot {
    /// Whether the backend holds a live agent for the task, which is exactly
    /// when stopping it has something to end.
    pub live: bool,
    /// The run's state while it is live; how the latest execution ended
    /// otherwise; `None` when nothing has run since the app started.
    #[serde(default)]
    pub status: Option<AgentStatus>,
    pub last_execution: Option<ExecutionSnapshot>,
}

/// One run the backend owns right now, as `get_live_runs` reports it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LiveRun {
    pub task_id: Uuid,
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ExecutionSnapshot {
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub stopped_at: Option<chrono::DateTime<chrono::Utc>>,
    pub output: Vec<AgentLogEntry>,
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
    fn agent_events_decode_the_backend_wire_shape() {
        let output: AgentEvent = serde_json::from_value(serde_json::json!({
            "type": "output", "task_id": "a", "text": "hello"
        }))
        .expect("output");
        assert_eq!(output.task_id(), "a");

        let phase: AgentEvent = serde_json::from_value(serde_json::json!({
            "type": "phase_change", "task_id": "b", "phase": "qa_review", "progress": 80
        }))
        .expect("phase change");
        assert_eq!(
            phase,
            AgentEvent::PhaseChange {
                task_id: "b".to_string(),
                phase: crate::models::TaskPhase::QaReview,
                progress: 80
            }
        );

        let log: AgentEvent = serde_json::from_value(serde_json::json!({
            "type": "log", "task_id": "c", "level": "warn", "message": "m"
        }))
        .expect("log");
        assert_eq!(log.task_id(), "c");
    }

    #[test]
    fn a_task_run_snapshot_decodes_with_and_without_an_execution() {
        let none: TaskRunSnapshot =
            serde_json::from_value(serde_json::json!({"live": false, "last_execution": null}))
                .expect("no execution");
        assert_eq!(none, TaskRunSnapshot::default());

        let some: TaskRunSnapshot = serde_json::from_value(serde_json::json!({
            "live": true,
            "last_execution": {
                "started_at": "2026-09-27T10:00:00Z",
                "stopped_at": null,
                "output": [{"timestamp": "2026-09-27T10:00:01Z", "level": "error", "message": "boom"}]
            }
        }))
        .expect("an execution");
        let execution = some.last_execution.expect("present");
        assert!(some.live);
        assert_eq!(execution.output[0].level, LogLevel::Error);
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

    #[test]
    fn run_state_events_and_live_runs_decode_the_backend_wire_shape() {
        let running: AgentEvent = serde_json::from_value(serde_json::json!({
            "type": "run_state", "task_id": "a", "status": "running"
        }))
        .expect("running");
        assert_eq!(running, AgentEvent::RunState { task_id: "a".into(), status: AgentStatus::Running });
        assert_eq!(running.task_id(), "a");

        let failed: AgentEvent = serde_json::from_value(serde_json::json!({
            "type": "run_state", "task_id": "a", "status": {"failed": "boom"}
        }))
        .expect("failed");
        assert_eq!(failed, AgentEvent::RunState { task_id: "a".into(), status: AgentStatus::Failed("boom".into()) });

        let runs: Vec<LiveRun> = serde_json::from_value(serde_json::json!([
            {"task_id": "00000000-0000-0000-0000-000000000001", "status": "stopping"}
        ]))
        .expect("live runs");
        assert_eq!(runs[0].status, AgentStatus::Stopping);

        let snapshot: TaskRunSnapshot = serde_json::from_value(serde_json::json!({
            "live": false, "status": {"failed": "x"}, "last_execution": null
        }))
        .expect("snapshot with a status");
        assert_eq!(snapshot.status, Some(AgentStatus::Failed("x".into())));
    }

}
