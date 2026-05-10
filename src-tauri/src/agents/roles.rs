use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Defines the role an agent plays within a workflow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Plans tasks, decomposes work, coordinates other agents.
    Lead,
    /// Implements code changes.
    Developer,
    /// Reviews code, runs QA checks.
    Reviewer,
    /// Custom user-defined role with a name.
    Custom(String),
}

/// A running agent slot within a workflow execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSlot {
    pub role: AgentRole,
    pub execution_id: Option<Uuid>,
    pub task_id: Option<Uuid>,
    pub status: AgentSlotStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentSlotStatus {
    Idle,
    Working,
    WaitingForInput,
    Completed,
    Failed,
}