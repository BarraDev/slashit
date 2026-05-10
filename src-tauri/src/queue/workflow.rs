use crate::agents::roles::AgentSlot;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Extended queue configuration supporting multi-agent workflows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowConfig {
    /// Single agent or team workflow.
    pub mode: WorkflowMode,
    /// Maximum concurrent workflows.
    pub parallel_limit: u32,
    /// Whether review step is required before committing.
    pub review_required: bool,
    /// Whether to auto-commit via JJ after review passes.
    pub auto_commit: bool,
    /// Seconds to wait before retrying after rate limit.
    pub rate_limit_retry_delay_secs: u64,
    /// Maximum retry attempts per workflow.
    pub max_retries: u32,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            mode: WorkflowMode::Single,
            parallel_limit: 3,
            review_required: false,
            auto_commit: false,
            rate_limit_retry_delay_secs: 60,
            max_retries: 3,
        }
    }
}

/// State of a workflow execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowState {
    /// Lead agent analyzes and decomposes the task.
    Planning,
    /// Developer agents implement the task.
    Developing,
    /// Reviewer agent checks the work.
    Reviewing,
    /// JJ operations to commit changes.
    Committing,
    /// Workflow completed successfully.
    Complete,
    /// Workflow failed.
    Failed,
}

/// Mode of workflow execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum WorkflowMode {
    /// Single agent handles everything (Phase 2 behavior).
    #[default]
    Single,
    /// Multi-agent team: Lead → Developer → Reviewer pipeline.
    Team,
}


/// A workflow execution tracking a task through the agent pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: Uuid,
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub state: WorkflowState,
    pub mode: WorkflowMode,
    pub agents: Vec<AgentSlot>,
    pub transitions: Vec<WorkflowTransition>,
    pub retry_count: u32,
    pub max_retries: u32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Records a state transition in the workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowTransition {
    pub from: WorkflowState,
    pub to: WorkflowState,
    pub reason: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::roles::{AgentRole, AgentSlotStatus};

    impl Workflow {
        fn new_single(task_id: Uuid, project_id: Uuid) -> Self {
            let now = chrono::Utc::now();
            Self {
                id: Uuid::new_v4(),
                task_id,
                project_id,
                state: WorkflowState::Planning,
                mode: WorkflowMode::Single,
                agents: vec![AgentSlot {
                    role: AgentRole::Developer,
                    execution_id: None,
                    task_id: Some(task_id),
                    status: AgentSlotStatus::Idle,
                }],
                transitions: Vec::new(),
                retry_count: 0,
                max_retries: 3,
                created_at: now,
                updated_at: now,
            }
        }

        fn new_team(task_id: Uuid, project_id: Uuid) -> Self {
            let now = chrono::Utc::now();
            Self {
                id: Uuid::new_v4(),
                task_id,
                project_id,
                state: WorkflowState::Planning,
                mode: WorkflowMode::Team,
                agents: vec![
                    AgentSlot {
                        role: AgentRole::Lead,
                        execution_id: None,
                        task_id: Some(task_id),
                        status: AgentSlotStatus::Idle,
                    },
                    AgentSlot {
                        role: AgentRole::Developer,
                        execution_id: None,
                        task_id: Some(task_id),
                        status: AgentSlotStatus::Idle,
                    },
                    AgentSlot {
                        role: AgentRole::Reviewer,
                        execution_id: None,
                        task_id: Some(task_id),
                        status: AgentSlotStatus::Idle,
                    },
                ],
                transitions: Vec::new(),
                retry_count: 0,
                max_retries: 3,
                created_at: now,
                updated_at: now,
            }
        }

        fn transition(&mut self, to: WorkflowState, reason: String) -> Result<(), String> {
            if !self.can_transition(&to) {
                return Err(format!(
                    "Invalid transition from {:?} to {:?}",
                    self.state, to
                ));
            }

            let transition = WorkflowTransition {
                from: self.state.clone(),
                to: to.clone(),
                reason,
                timestamp: chrono::Utc::now(),
            };

            self.transitions.push(transition);
            self.state = to;
            self.updated_at = chrono::Utc::now();

            Ok(())
        }

        fn can_transition(&self, to: &WorkflowState) -> bool {
            match (&self.state, to) {
                (WorkflowState::Planning, WorkflowState::Developing) => true,
                (WorkflowState::Planning, WorkflowState::Failed) => true,
                (WorkflowState::Developing, WorkflowState::Reviewing) => true,
                (WorkflowState::Developing, WorkflowState::Committing) => true,
                (WorkflowState::Developing, WorkflowState::Failed) => true,
                (WorkflowState::Reviewing, WorkflowState::Developing) => true,
                (WorkflowState::Reviewing, WorkflowState::Committing) => true,
                (WorkflowState::Reviewing, WorkflowState::Failed) => true,
                (WorkflowState::Committing, WorkflowState::Complete) => true,
                (WorkflowState::Committing, WorkflowState::Failed) => true,
                (_, WorkflowState::Planning) if self.retry_count < self.max_retries => true,
                _ => false,
            }
        }

        fn is_terminal(&self) -> bool {
            matches!(self.state, WorkflowState::Complete | WorkflowState::Failed)
        }
    }

    #[test]
    fn test_single_workflow_creation() {
        let task_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let wf = Workflow::new_single(task_id, project_id);

        assert_eq!(wf.state, WorkflowState::Planning);
        assert_eq!(wf.mode, WorkflowMode::Single);
        assert_eq!(wf.agents.len(), 1);
        assert_eq!(wf.agents[0].role, AgentRole::Developer);
    }

    #[test]
    fn test_team_workflow_creation() {
        let task_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let wf = Workflow::new_team(task_id, project_id);

        assert_eq!(wf.state, WorkflowState::Planning);
        assert_eq!(wf.mode, WorkflowMode::Team);
        assert_eq!(wf.agents.len(), 3);
        assert_eq!(wf.agents[0].role, AgentRole::Lead);
        assert_eq!(wf.agents[1].role, AgentRole::Developer);
        assert_eq!(wf.agents[2].role, AgentRole::Reviewer);
    }

    #[test]
    fn test_valid_transitions() {
        let mut wf = Workflow::new_team(Uuid::new_v4(), Uuid::new_v4());

        assert!(wf.transition(WorkflowState::Developing, "Plan complete".into()).is_ok());
        assert!(wf.transition(WorkflowState::Reviewing, "Code done".into()).is_ok());
        assert!(wf.transition(WorkflowState::Developing, "Issues found".into()).is_ok());
        assert!(wf.transition(WorkflowState::Reviewing, "Fixed".into()).is_ok());
        assert!(wf.transition(WorkflowState::Committing, "Approved".into()).is_ok());
        assert!(wf.transition(WorkflowState::Complete, "Committed".into()).is_ok());

        assert_eq!(wf.transitions.len(), 6);
        assert!(wf.is_terminal());
    }

    #[test]
    fn test_invalid_transition() {
        let mut wf = Workflow::new_single(Uuid::new_v4(), Uuid::new_v4());

        // Can't go from Planning directly to Complete
        assert!(wf.transition(WorkflowState::Complete, "skip".into()).is_err());
    }

    #[test]
    fn test_retry_transition() {
        let mut wf = Workflow::new_single(Uuid::new_v4(), Uuid::new_v4());
        wf.transition(WorkflowState::Developing, "start".into()).unwrap();
        wf.transition(WorkflowState::Failed, "error".into()).unwrap();

        // Can retry back to Planning
        wf.retry_count = 0;
        assert!(wf.can_transition(&WorkflowState::Planning));

        // Can't retry if max retries exceeded
        wf.retry_count = 3;
        assert!(!wf.can_transition(&WorkflowState::Planning));
    }
}
