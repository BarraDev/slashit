//! Durable, Project-owned conversation state. Provider processes and their
//! output streams are deliberately not part of this model.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Task, TaskCategory, TaskPriority, TaskStatus};

pub const MESSAGE_LIMIT: usize = 16_000;
pub const TASK_TITLE_LIMIT: usize = 200;
pub const TASK_DESCRIPTION_LIMIT: usize = 8_000;
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

/// Lifecycle of a [`ProjectAction`].
///
/// `Approved` is durable intent, not proof of effect: it means the Human said
/// yes and SlashIt has not yet recorded the outcome. Only `Applied`, `Rejected`
/// and `Refused` are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectActionStatus {
    Proposed,
    Approved,
    Applied,
    Rejected,
    Refused,
}

/// One field the Coordinator proposes to change on an existing Task, with the
/// value SlashIt observed when the proposal was made.
///
/// `from` is filled in by SlashIt, never by the provider. Approval only applies
/// the change while the Task still holds `from` (compare-and-set), so an edit
/// cannot silently overwrite state that changed after the Human read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "field", rename_all = "snake_case")]
pub enum FieldChange {
    Title { from: String, to: String },
    Description { from: Option<String>, to: String },
    Priority { from: TaskPriority, to: TaskPriority },
    Category { from: TaskCategory, to: TaskCategory },
}

impl FieldChange {
    pub fn is_observed_in(&self, task: &Task) -> bool {
        match self {
            Self::Title { from, .. } => task.title == *from,
            Self::Description { from, .. } => task.description == *from,
            Self::Priority { from, .. } => task.priority == *from,
            Self::Category { from, .. } => task.category == *from,
        }
    }

    pub fn is_applied_to(&self, task: &Task) -> bool {
        match self {
            Self::Title { to, .. } => task.title == *to,
            Self::Description { to, .. } => task.description.as_deref() == Some(to.as_str()),
            Self::Priority { to, .. } => task.priority == *to,
            Self::Category { to, .. } => task.category == *to,
        }
    }

    pub fn apply(&self, task: &mut Task) {
        match self {
            Self::Title { to, .. } => task.title = to.clone(),
            Self::Description { to, .. } => task.description = Some(to.clone()),
            Self::Priority { to, .. } => task.priority = to.clone(),
            Self::Category { to, .. } => task.category = to.clone(),
        }
    }

    fn summary(&self) -> String {
        let quote = |text: &str| text.chars().take(120).collect::<String>();
        match self {
            Self::Title { from, to } => format!("title {:?} -> {:?}", quote(from), quote(to)),
            Self::Description { from, to } => format!(
                "description {:?} -> {:?}",
                quote(from.as_deref().unwrap_or("")),
                quote(to)
            ),
            Self::Priority { from, to } => format!("priority {from:?} -> {to:?}"),
            Self::Category { from, to } => format!("category {from:?} -> {to:?}"),
        }
    }
}

/// A SlashIt mutation the Coordinator proposes. Creating or editing a Task
/// never queues, starts or checks out anything; execution stays Task lifecycle
/// scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskMutation {
    /// `task_id` is allocated when the proposal is made and persisted with it,
    /// so an approved creation names the Task it will produce before that Task
    /// exists. That is what makes a retried approval idempotent.
    CreateTask {
        task_id: Uuid,
        title: String,
        description: Option<String>,
        priority: TaskPriority,
        category: TaskCategory,
    },
    EditTask {
        target_task_id: Uuid,
        target_task_title: String,
        changes: Vec<FieldChange>,
    },
    /// Return a Task to Backlog. `from` is the status SlashIt observed when the
    /// proposal was made; approval only moves a Task that still holds it.
    MoveTask {
        target_task_id: Uuid,
        target_task_title: String,
        from: TaskStatus,
        to: TaskStatus,
    },
}

impl TaskMutation {
    pub fn create(
        title: &str,
        description: Option<&str>,
        priority: Option<TaskPriority>,
        category: Option<TaskCategory>,
    ) -> Result<Self, String> {
        Ok(Self::CreateTask {
            task_id: Uuid::new_v4(),
            title: validate_title(title)?,
            description: description.map(validate_description).transpose()?,
            priority: priority.unwrap_or_default(),
            category: category.unwrap_or_default(),
        })
    }

    /// Diff the proposed values against `target`. Values the Task already
    /// holds are dropped; a proposal that changes nothing is refused.
    pub fn edit(
        target: &Task,
        title: Option<&str>,
        description: Option<&str>,
        priority: Option<TaskPriority>,
        category: Option<TaskCategory>,
    ) -> Result<Self, String> {
        let mut changes = Vec::new();
        if let Some(title) = title {
            let to = validate_title(title)?;
            if to != target.title {
                changes.push(FieldChange::Title { from: target.title.clone(), to });
            }
        }
        if let Some(description) = description {
            let to = validate_description(description)?;
            if target.description.as_deref() != Some(to.as_str()) {
                changes.push(FieldChange::Description { from: target.description.clone(), to });
            }
        }
        if let Some(to) = priority {
            if to != target.priority {
                changes.push(FieldChange::Priority { from: target.priority.clone(), to });
            }
        }
        if let Some(to) = category {
            if to != target.category {
                changes.push(FieldChange::Category { from: target.category.clone(), to });
            }
        }
        if changes.is_empty() {
            return Err("Coordinator proposed an edit that changes nothing".into());
        }
        Ok(Self::EditTask {
            target_task_id: target.id,
            target_task_title: target.title.chars().take(TASK_TITLE_LIMIT).collect(),
            changes,
        })
    }

    /// Propose moving `target` to `to`, if the Coordinator may do so.
    ///
    /// [`coordinator_may_move`] is deliberately narrower than the board: a
    /// move into any column that starts, resumes or finishes work belongs to
    /// the Task lifecycle, not to a conversational proposal.
    pub fn move_task(target: &Task, to: TaskStatus) -> Result<Self, String> {
        if target.status == to {
            return Err("Coordinator proposed a move that changes nothing".into());
        }
        if !coordinator_may_move(&target.status, &to) {
            return Err(format!(
                "The Coordinator cannot move a Task from {:?} to {to:?}",
                target.status
            ));
        }
        Ok(Self::MoveTask {
            target_task_id: target.id,
            target_task_title: target.title.chars().take(TASK_TITLE_LIMIT).collect(),
            from: target.status.clone(),
            to,
        })
    }

    pub fn summary(&self) -> String {
        let text = match self {
            Self::CreateTask { title, description, priority, category, .. } => format!(
                "Create Task {:?} (priority {priority:?}, category {category:?}){}",
                title,
                description.as_ref().map_or(String::new(), |d| format!(": {}", d.chars().take(600).collect::<String>()))
            ),
            Self::EditTask { target_task_id, target_task_title, changes } => format!(
                "Edit Task {target_task_id} ({target_task_title:?}): {}",
                changes.iter().map(FieldChange::summary).collect::<Vec<_>>().join("; ")
            ),
            Self::MoveTask { target_task_id, target_task_title, from, to } => format!(
                "Move Task {target_task_id} ({target_task_title:?}) from {from:?} to {to:?}; \
                 no work is started"
            ),
        };
        text.chars().take(1500).collect()
    }
}

/// The only Task moves a Project Conversation may propose: returning a Task
/// that is waiting (`Queue`) or has failed (`Error`) to `Backlog`.
///
/// Every other destination is an execution decision. `Queue` and `InProgress`
/// make the Task eligible to run, `AiReview` and `HumanReview` and `PrCreated`
/// are outcomes of work, and `Done` removes the Task Checkout. Those stay with
/// the normal Task lifecycle (queueing is its own capability).
pub fn coordinator_may_move(from: &TaskStatus, to: &TaskStatus) -> bool {
    matches!(
        (from, to),
        (TaskStatus::Queue | TaskStatus::Error, TaskStatus::Backlog)
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ProjectActionOutcome {
    Applied { task_id: Uuid, title: String },
    Refused { reason: String },
}

/// A Human-gated SlashIt operation proposed from the Project Conversation.
///
/// Distinct from [`TaskAction`], which models delegation to a Task Worker. The
/// Conversation records the proposal, the decision and the outcome reference;
/// the Task itself stays owned by the normal Task store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectAction {
    pub id: Uuid,
    pub explanation: String,
    pub mutation: TaskMutation,
    pub status: ProjectActionStatus,
    #[serde(default)]
    pub outcome: Option<ProjectActionOutcome>,
    pub created_at: DateTime<Utc>,
}

impl ProjectAction {
    pub fn proposed(explanation: String, mutation: TaskMutation) -> Self {
        Self {
            id: Uuid::new_v4(),
            explanation,
            mutation,
            status: ProjectActionStatus::Proposed,
            outcome: None,
            created_at: Utc::now(),
        }
    }
}

fn validate_title(value: &str) -> Result<String, String> {
    let title = value.trim();
    if title.is_empty() || title.chars().count() > TASK_TITLE_LIMIT {
        return Err(format!("Task title must contain 1–{TASK_TITLE_LIMIT} characters"));
    }
    Ok(title.to_owned())
}

fn validate_description(value: &str) -> Result<String, String> {
    let description = value.trim();
    if description.is_empty() || description.len() > TASK_DESCRIPTION_LIMIT {
        return Err(format!("Task description must contain 1–{TASK_DESCRIPTION_LIMIT} bytes"));
    }
    Ok(description.to_owned())
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
    ProjectActionProposed {
        action_id: Uuid,
    },
    ProjectActionDecision {
        action_id: Uuid,
        approved: bool,
    },
    ProjectActionApplied {
        action_id: Uuid,
        task_id: Uuid,
    },
    ProjectActionRefused {
        action_id: Uuid,
        reason: String,
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
    /// Absent in Conversations saved before Coordinator Task operations.
    #[serde(default)]
    pub project_actions: Vec<ProjectAction>,
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
    CreateTask {
        text: String,
        title: String,
        description: Option<String>,
        priority: Option<TaskPriority>,
        category: Option<TaskCategory>,
    },
    EditTask {
        text: String,
        target_task_id: Uuid,
        title: Option<String>,
        description: Option<String>,
        priority: Option<TaskPriority>,
        category: Option<TaskCategory>,
    },
    /// Proposal to return a Queued or failed Task to Backlog. Never starts work.
    MoveTask {
        text: String,
        target_task_id: Uuid,
        to: TaskStatus,
    },
    /// Read-only: answered by SlashIt from current state, never a proposal.
    InspectTask {
        target_task_id: Uuid,
    },
    /// Read-only: a bounded, filtered page of this Project's Tasks.
    ListTasks {
        status: Option<TaskStatus>,
        limit: Option<u8>,
    },
    /// Read-only: the most recent milestones of one Task's activity.
    InspectTaskActivity {
        target_task_id: Uuid,
        limit: Option<u8>,
    },
    /// Read-only: the last known pull request and CI state of one Task.
    InspectTaskPullRequest {
        target_task_id: Uuid,
    },
    /// Read-only: this Project's name, Workspace, base branch and Task counts.
    InspectProject {},
}

/// Scope attached to every projected Human decision on a proposal.
pub const DECISION_SCOPE: &str = "this_proposal_only";

/// How the Coordinator must read decisions in `recent_history`.
pub const HISTORY_SEMANTICS: &str = "A decision entry records the Human's answer to one specific earlier proposal and applies to that proposal only. It is not a standing instruction. A rejected proposal never forbids a later proposal. When the current message explicitly asks for the same or an equivalent change again, treat it as a new request and propose it again; SlashIt will ask for a new approval.";

fn decision_label(approved: bool) -> &'static str {
    if approved { "approved" } else { "rejected" }
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
            project_actions: Vec::new(),
        }
    }

    pub fn project_action(&self, id: Uuid) -> Option<&ProjectAction> {
        self.project_actions.iter().find(|action| action.id == id)
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
            CoordinatorOutput::CreateTask { text, title, description, .. } => {
                Self::validate_text(text)?;
                validate_title(title)?;
                description.as_deref().map(validate_description).transpose()?;
            }
            CoordinatorOutput::EditTask { text, title, description, priority, category, .. } => {
                Self::validate_text(text)?;
                if title.is_none() && description.is_none() && priority.is_none() && category.is_none() {
                    return Err("EditTask must propose at least one field".into());
                }
                title.as_deref().map(validate_title).transpose()?;
                description.as_deref().map(validate_description).transpose()?;
            }
            CoordinatorOutput::MoveTask { text, .. } => Self::validate_text(text)?,
            CoordinatorOutput::InspectTask { .. }
            | CoordinatorOutput::InspectTaskPullRequest { .. }
            | CoordinatorOutput::InspectProject {} => {}
            CoordinatorOutput::ListTasks { limit, .. } | CoordinatorOutput::InspectTaskActivity { limit, .. } => {
                if *limit == Some(0) {
                    return Err("A read limit must be at least 1".into());
                }
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
                    serde_json::json!({"kind":"action_decision","approved":approved,"decision":decision_label(*approved),"scope":DECISION_SCOPE,"target_task_id":action.target_task_id,"target_task_title":action.target_task_title.chars().take(500).collect::<String>(),"request":request.as_deref().unwrap_or(&action.request).chars().take(3000).collect::<String>()})),
                EntryKind::WorkerStarted { action_id } => Some(serde_json::json!({"kind":"worker_started","action_id":action_id})),
                EntryKind::WorkerResult { action_id, result } => Some(serde_json::json!({"kind":"worker_result","action_id":action_id,"result":result.chars().take(3000).collect::<String>()})),
                EntryKind::RunFailed { role, message } => Some(serde_json::json!({"kind":"run_failed","role":role,"message":message.chars().take(1000).collect::<String>()})),
                EntryKind::ProjectActionProposed { action_id } => self.project_action(*action_id).map(|action| serde_json::json!({"kind":"project_action_proposed","action_id":action_id,"summary":action.mutation.summary()})),
                EntryKind::ProjectActionDecision { action_id, approved } => self.project_action(*action_id).map(|action| serde_json::json!({"kind":"project_action_decision","action_id":action_id,"approved":approved,"decision":decision_label(*approved),"scope":DECISION_SCOPE,"summary":action.mutation.summary()})),
                EntryKind::ProjectActionApplied { action_id, task_id } => Some(serde_json::json!({"kind":"project_action_applied","action_id":action_id,"task_id":task_id})),
                EntryKind::ProjectActionRefused { action_id, reason } => Some(serde_json::json!({"kind":"project_action_refused","action_id":action_id,"reason":reason.chars().take(1000).collect::<String>()})),
            }).take(PROJECTION_MESSAGE_COUNT).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>();
        serde_json::json!({
            "project": {"name": project_name, "root": project_root},
            "current_message": current_message,
            "history_semantics": HISTORY_SEMANTICS,
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
    fn projected_decisions_state_their_scope_and_stored_conversations_stay_unchanged() {
        let mut conversation = Conversation::new(Uuid::new_v4());
        let action = ProjectAction::proposed(
            "Add Task".into(),
            TaskMutation::MoveTask {
                target_task_id: Uuid::new_v4(),
                target_task_title: "T".into(),
                from: TaskStatus::Queue,
                to: TaskStatus::Backlog,
            },
        );
        let action_id = action.id;
        conversation.project_actions.push(action);
        conversation.push(Role::Coordinator, EntryKind::ProjectActionProposed { action_id });
        conversation.push(Role::Human, EntryKind::ProjectActionDecision { action_id, approved: false });

        // Nothing about the projection is persisted, so older documents load as before.
        let stored = serde_json::to_string(&conversation).unwrap();
        assert!(!stored.contains("this_proposal_only") && !stored.contains("history_semantics"));
        let reloaded: Conversation = serde_json::from_str(&stored).unwrap();

        let projection = reloaded.coordinator_projection("Add it again", "Project", None, &[]);
        assert!(projection["history_semantics"].as_str().unwrap().contains("that proposal only"));
        let decision = &projection["recent_history"][1];
        assert_eq!(decision["kind"], "project_action_decision");
        assert_eq!(decision["approved"], false);
        assert_eq!(decision["decision"], "rejected");
        assert_eq!(decision["scope"], "this_proposal_only");
        assert_eq!(projection["current_message"], "Add it again");
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
