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
/// How much of a Task's description an approval card carries.
pub const INPUT_PREVIEW_LIMIT: usize = 2_000;
/// The most an approved additional coding request may carry, in bytes.
pub const ADDITIONAL_REQUEST_LIMIT: usize = 4_000;

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
    /// A Human withdrew the approval of a retired direct delegation before it
    /// started. Terminal: the historical approval event is kept, and the Task
    /// is never touched.
    Withdrawn,
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
    /// Put a Backlog Task in the queue. `from` is the status SlashIt observed
    /// when the proposal was made (only `Backlog` is accepted); approval only
    /// queues a Task that still holds it. It never starts anything: the
    /// scheduler admits queued Tasks, and the executor creates or reattaches
    /// the Task Checkout.
    ///
    /// `input` names the exact coding input the Human approved. Approval only
    /// queues a Task whose input still matches it, so a Task edited after the
    /// proposal is refused rather than queued as something nobody approved.
    /// Approval also records the input on the Task, and the executor checks it
    /// again before the run starts (see `Task::approved_execution`).
    /// Proposals saved before this field existed carry none; they stay readable
    /// but cannot queue a Task, because the Human never saw its input.
    EnqueueTask {
        target_task_id: Uuid,
        target_task_title: String,
        from: TaskStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<ExecutionInput>,
    },
    /// Queue a Backlog Task that has no Task Checkout, for one run with one
    /// additional coding request. The Task description is not changed. `input`
    /// is required: its digest covers the Task input, the effective model and
    /// `request` (`queue::prompt::approved_input_digest`), so approval names
    /// exactly what the provider will receive. Like `EnqueueTask`, approval only
    /// queues the Task; admission, checkout and the launch stay with the executor.
    ExecuteRequest {
        target_task_id: Uuid,
        target_task_title: String,
        from: TaskStatus,
        request: String,
        input: ExecutionInput,
        /// The Task Checkout this request reuses, as inspected when it was
        /// proposed. Present only for a Task that already holds a checkout. A
        /// proposal saved without it, including every one written before this
        /// field existed, never authorizes reuse of a checkout.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkout: Option<Box<ExistingCheckoutEvidence>>,
    },
}

/// The facts of an existing Task Checkout that a Human approved an additional
/// request to run in, read from Git when the request was proposed.
///
/// Approval re-reads the checkout and refuses any difference, so the approved
/// run starts only in the checkout the person was shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExistingCheckoutEvidence {
    pub project_id: Uuid,
    pub task_id: Uuid,
    /// The repository path the checkout is registered in, as SlashIt resolved it.
    pub repository_path: String,
    pub checkout_path: String,
    /// The `.git` entry identity of the checkout (device and inode on Unix).
    /// `None` when it could not be read; a difference refuses, and absence never
    /// matches a present identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_entry_identity: Option<String>,
    /// The Task's branch, by its short name.
    pub branch: String,
    /// The full reference of the branch, `refs/heads/<branch>`.
    pub branch_ref: String,
    /// The full commit the branch and HEAD named when the request was proposed.
    pub head: String,
    /// The Task's own `base_commit`, which the Task diff is taken from. Reuse
    /// keeps this baseline; it is never reset to `head`.
    pub base_commit: String,
    /// The run number the approval would start, from `Task::next_run`.
    pub run: u32,
}

/// What a queued Task would be told to do, as the Human saw it when approving.
///
/// `digest` is the binding (see `queue::prompt::task_input_digest`);
/// `description` is only what the approval card shows, bounded for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionInput {
    pub digest: String,
    pub description: Option<String>,
    pub subtask_count: usize,
    /// The Task's model configuration as the Human saw it: a configured
    /// identifier, or an empty or `"default"` value for provider default. `None`
    /// for a proposal recorded before the model was shown.
    #[serde(default)]
    pub model: Option<String>,
}

impl ExecutionInput {
    pub fn observe(task: &Task, digest: String) -> Self {
        Self {
            digest,
            description: task
                .description
                .as_deref()
                .filter(|text| !text.is_empty())
                .map(|text| text.chars().take(INPUT_PREVIEW_LIMIT).collect()),
            subtask_count: task.subtasks.len(),
            model: Some(task.model.clone()),
        }
    }
}

/// How a Task's model configuration is described to the Human. An empty or
/// `"default"` value selects the provider default, which the provider resolves
/// at execution time; any other value is the configured identifier. The launch
/// applies the same rule (`queue::executor`), so the wording matches what runs.
pub fn model_policy_phrase(model: Option<&str>) -> String {
    match model {
        None => "model not recorded for this proposal".into(),
        Some("" | "default") => "provider default, selected at execution time".into(),
        Some(model) => format!("model {model}"),
    }
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

    /// Propose queueing `target`, if it is a Backlog Task that can run.
    pub fn enqueue_task(target: &Task, input_digest: String) -> Result<Self, String> {
        if target.status != TaskStatus::Backlog {
            return Err(format!(
                "The Coordinator can only queue a Backlog Task, and this one is in {:?}",
                target.status
            ));
        }
        if target.cleanup_in_flight || target.republish_pending_refusal().is_some() {
            return Err("This Task cannot run until an unfinished checkout operation is resolved".into());
        }
        Ok(Self::EnqueueTask {
            target_task_id: target.id,
            target_task_title: target.title.chars().take(TASK_TITLE_LIMIT).collect(),
            from: target.status.clone(),
            input: Some(ExecutionInput::observe(target, input_digest)),
        })
    }

    /// Propose one additional coding request for a Backlog Task.
    ///
    /// A Task with no Task Checkout is proposed without `checkout`, as before.
    /// A Task that already holds one qualifies only with `checkout`, the
    /// evidence the caller inspected for this exact Task, and only when the
    /// Task's own refusals allow reuse ([`Task::existing_checkout_refusal`]).
    /// Refused here for any other state, and while an earlier checkout
    /// operation is unfinished. Approval re-checks all of it.
    pub fn execute_request(
        target: &Task,
        request: &str,
        checkout: Option<ExistingCheckoutEvidence>,
    ) -> Result<Self, String> {
        let request = validate_request(request)?;
        if target.status != TaskStatus::Backlog {
            return Err(format!(
                "An additional request can only be run for a Backlog Task, and this one is in {:?}. \
                 Propose a new Task for distinct work.",
                target.status
            ));
        }
        let holds_checkout =
            target.worktree_path.is_some() || target.branch_name.is_some() || target.worker_handoff.is_some();
        match (holds_checkout, &checkout) {
            (false, Some(_)) => {
                return Err("This Task has no Task Checkout to reuse, so the request cannot name one".into());
            }
            (true, None) => {
                return Err(
                    "This Task already has a Task Checkout, and its checkout was not inspected for this request. \
                     Propose the request again."
                        .into(),
                );
            }
            (true, Some(evidence)) => {
                if evidence.task_id != target.id || evidence.project_id != target.project_id {
                    return Err("The inspected checkout belongs to another Task or Project".into());
                }
                if let Some(reason) = target.existing_checkout_refusal() {
                    return Err(reason);
                }
            }
            (false, None) => {}
        }
        if target.cleanup_in_flight || target.republish_pending_refusal().is_some() {
            return Err("This Task cannot run until an unfinished checkout operation is resolved".into());
        }
        let digest = crate::queue::prompt::approved_input_digest(target, Some(&request));
        Ok(Self::ExecuteRequest {
            target_task_id: target.id,
            target_task_title: target.title.chars().take(TASK_TITLE_LIMIT).collect(),
            from: target.status.clone(),
            request,
            input: ExecutionInput::observe(target, digest),
            checkout: checkout.map(Box::new),
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
            Self::EnqueueTask { target_task_id, target_task_title, from, input } => format!(
                "Queue Task {target_task_id} ({target_task_title:?}) from {from:?}{}; SlashIt's \
                 scheduler decides when it starts and the executor prepares its checkout",
                input.as_ref().map_or(String::new(), |input| format!(
                    ", to run exactly its current definition ({}){}",
                    input.digest.chars().take(12).collect::<String>(),
                    input.description.as_ref().map_or(String::new(), |d| format!(
                        ": {}", d.chars().take(600).collect::<String>()
                    ))
                ))
            ),
            Self::ExecuteRequest { target_task_id, target_task_title, from, request, input, checkout } => format!(
                "Queue Task {target_task_id} ({target_task_title:?}) from {from:?} for one run with an \
                 additional request, on its current definition ({}; {}){}: {}; the Task description is not \
                 changed, and SlashIt's scheduler decides when it starts",
                input.digest.chars().take(12).collect::<String>(),
                model_policy_phrase(input.model.as_deref()),
                checkout.as_ref().map_or(String::new(), |c| format!(
                    ", reusing its Task Checkout on branch {} at {} (base {}), preserving its existing commits",
                    c.branch,
                    c.head.chars().take(12).collect::<String>(),
                    c.base_commit.chars().take(12).collect::<String>(),
                )),
                request.chars().take(600).collect::<String>()
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
    /// Approval found the Task already in Queue. The record cannot say which
    /// front door queued it, so this does not claim it was this approval.
    AlreadyQueued { task_id: Uuid, title: String },
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

fn validate_request(value: &str) -> Result<String, String> {
    let request = value.trim();
    if request.is_empty() || request.len() > ADDITIONAL_REQUEST_LIMIT {
        return Err(format!("Additional request must contain 1–{ADDITIONAL_REQUEST_LIMIT} bytes"));
    }
    Ok(request.to_owned())
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
        /// True when the failure left the Human message it answers unanswered,
        /// so an explicit Retry may run the Coordinator for that message again.
        /// Absent from Conversations saved before this field existed, which
        /// keeps those failures visible without making them retryable.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        retryable: bool,
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
    /// The Human withdrew an approval of a retired direct delegation. `revision`
    /// is the Conversation revision the Human acted on.
    ActionWithdrawn {
        action_id: Uuid,
        revision: u64,
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
    /// Proposal to put a Backlog Task in the queue. SlashIt's scheduler, not the
    /// Coordinator, decides when it starts.
    EnqueueTask {
        text: String,
        target_task_id: Uuid,
    },
    /// Proposal to run a Backlog Task once more with one additional coding
    /// request. A Backlog Task qualifies with no Task Checkout, or with its own
    /// existing Task Checkout that SlashIt has inspected and found eligible.
    /// Distinct new work is a new Task. The description is not changed.
    ExecuteRequest {
        text: String,
        target_task_id: Uuid,
        request: String,
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

fn is_retryable_failure(kind: &EntryKind) -> bool {
    matches!(kind, EntryKind::RunFailed { role: Role::Coordinator, retryable: true, .. })
}

/// Entries that record a Human decision or its application to an earlier
/// proposal. They never answer a Human message.
fn is_proposal_decision(kind: &EntryKind) -> bool {
    matches!(
        kind,
        EntryKind::ActionDecision { .. }
            | EntryKind::ProjectActionDecision { .. }
            | EntryKind::ProjectActionApplied { .. }
            | EntryKind::ProjectActionRefused { .. }
            | EntryKind::ActionWithdrawn { .. }
    )
}

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

    /// The Human message that the Coordinator has not answered, when every entry
    /// after it is a retryable Coordinator failure. Retry re-runs the
    /// Coordinator for this message without saving it again. Returns the
    /// message's index and text.
    pub fn retryable_human_turn(&self) -> Option<(usize, &str)> {
        let index = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry.kind, EntryKind::HumanMessage { .. }))?;
        let EntryKind::HumanMessage { text } = &self.entries[index].kind else {
            return None;
        };
        // Decisions and applications refer to earlier proposals, so they do not
        // answer this message. Any other entry after it does.
        let mut failed = false;
        for entry in &self.entries[index + 1..] {
            if is_retryable_failure(&entry.kind) {
                failed = true;
            } else if !is_proposal_decision(&entry.kind) {
                return None;
            }
        }
        failed.then_some((index, text.as_str()))
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
            CoordinatorOutput::MoveTask { text, .. } | CoordinatorOutput::EnqueueTask { text, .. } => Self::validate_text(text)?,
            CoordinatorOutput::ExecuteRequest { text, request, .. } => {
                Self::validate_text(text)?;
                validate_request(request)?;
            }
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
                EntryKind::RunFailed { role, message, .. } => Some(serde_json::json!({"kind":"run_failed","role":role,"message":message.chars().take(1000).collect::<String>()})),
                EntryKind::ProjectActionProposed { action_id } => self.project_action(*action_id).map(|action| serde_json::json!({"kind":"project_action_proposed","action_id":action_id,"summary":action.mutation.summary()})),
                EntryKind::ProjectActionDecision { action_id, approved } => self.project_action(*action_id).map(|action| serde_json::json!({"kind":"project_action_decision","action_id":action_id,"approved":approved,"decision":decision_label(*approved),"scope":DECISION_SCOPE,"summary":action.mutation.summary()})),
                EntryKind::ProjectActionApplied { action_id, task_id } => Some(match self.project_action(*action_id).and_then(|action| action.outcome.as_ref()) {
                    Some(ProjectActionOutcome::AlreadyQueued { .. }) => serde_json::json!({"kind":"project_action_applied","action_id":action_id,"task_id":task_id,"outcome":"found_already_queued"}),
                    _ => serde_json::json!({"kind":"project_action_applied","action_id":action_id,"task_id":task_id}),
                }),
                EntryKind::ProjectActionRefused { action_id, reason } => Some(serde_json::json!({"kind":"project_action_refused","action_id":action_id,"reason":reason.chars().take(1000).collect::<String>()})),
                EntryKind::ActionWithdrawn { action_id, .. } => Some(serde_json::json!({"kind":"action_withdrawn","action_id":action_id,"scope":DECISION_SCOPE,"note":"The Human withdrew an earlier approval of a retired direct delegation. Nothing was started."})),
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

    /// The same evidence is stored on a Task, whose board refuses keys it does
    /// not declare, so a Conversation that holds it refuses them too.
    #[test]
    fn checkout_evidence_refuses_a_field_it_does_not_declare() {
        let evidence = ExistingCheckoutEvidence {
            project_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
            repository_path: "/repo".into(),
            checkout_path: "/repo/.checkouts/t".into(),
            git_entry_identity: None,
            branch: "task/t".into(),
            branch_ref: "refs/heads/task/t".into(),
            head: "a".repeat(40),
            base_commit: "b".repeat(40),
            run: 1,
        };
        let mut json = serde_json::to_value(&evidence).unwrap();
        assert_eq!(
            serde_json::from_value::<ExistingCheckoutEvidence>(json.clone()).unwrap(),
            evidence
        );
        json["added_by_a_newer_build"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ExistingCheckoutEvidence>(json).is_err());
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

    /// A Conversation written before direct delegation was retired still loads
    /// with every legacy status and entry, keeps their identities, and projects
    /// its history to the Coordinator.
    #[test]
    fn historical_legacy_conversation_json_loads_keeps_identities_and_projects() {
        let timestamp = "2026-01-01T00:00:00Z";
        let task = Uuid::new_v4();
        let ids: Vec<Uuid> = (0..7).map(|_| Uuid::new_v4()).collect();
        let statuses = ["proposed", "approved", "running", "returned", "rejected", "interrupted", "failed"];
        let actions: Vec<serde_json::Value> = statuses
            .iter()
            .zip(&ids)
            .map(|(status, id)| {
                serde_json::json!({
                    "id": id,
                    "target_task_id": task,
                    "target_task_title": "Task",
                    "request": "change a.txt",
                    "explanation": "why",
                    "approved_request": if *status == "proposed" { serde_json::Value::Null } else { serde_json::json!("change a.txt") },
                    "status": status,
                    "worker_result": if *status == "returned" { serde_json::json!("result") } else { serde_json::Value::Null },
                    "coordinator_replied": false,
                    "created_at": timestamp,
                })
            })
            .collect();
        let entry = |kind: serde_json::Value| serde_json::json!({"id": Uuid::new_v4(), "role": "human", "kind": kind, "created_at": timestamp});
        let raw = serde_json::json!({
            "id": Uuid::new_v4(),
            "project_id": Uuid::new_v4(),
            "revision": 12,
            "created_at": timestamp,
            "updated_at": timestamp,
            "entries": [
                entry(serde_json::json!({"kind": "action_proposed", "action_id": ids[0]})),
                entry(serde_json::json!({"kind": "action_decision", "action_id": ids[1], "approved": true, "request": "change a.txt"})),
                entry(serde_json::json!({"kind": "worker_started", "action_id": ids[2]})),
                entry(serde_json::json!({"kind": "worker_result", "action_id": ids[3], "result": "result"})),
                entry(serde_json::json!({"kind": "run_failed", "role": "worker", "message": "stopped"})),
            ],
            "actions": actions,
        });

        let conversation: Conversation = serde_json::from_value(raw).expect("historical JSON loads");
        assert_eq!(conversation.actions.len(), 7);
        assert!(conversation.project_actions.is_empty(), "absent project_actions default to empty");
        assert_eq!(conversation.actions[1].status, ActionStatus::Approved);
        assert_eq!(conversation.actions[1].id, ids[1]);
        assert!(conversation.has_unmediated_worker_result(), "a saved Returned result still awaits mediation");

        let stored = serde_json::to_value(&conversation).unwrap();
        assert_eq!(stored["actions"][1]["id"], serde_json::json!(ids[1]));
        assert_eq!(stored["actions"][1]["status"], "approved");
        assert_eq!(stored["actions"][3]["status"], "returned");
        assert_eq!(stored["actions"][6]["status"], "failed");
        assert_eq!(stored["revision"], 12);

        let projection = conversation.coordinator_projection("next", "Project", None, &[]).to_string();
        for kind in ["action_proposed", "action_decision", "worker_started", "worker_result", "run_failed"] {
            assert!(projection.contains(kind), "{kind} is still projected");
        }
    }

    #[test]
    fn a_withdrawn_legacy_approval_round_trips_and_projects_without_starting_work() {
        let action_id = Uuid::new_v4();
        let mut conversation = Conversation::new(Uuid::new_v4());
        conversation.actions.push(TaskAction {
            id: action_id,
            target_task_id: Uuid::new_v4(),
            target_task_title: "Task".into(),
            request: "change a.txt".into(),
            explanation: "why".into(),
            approved_request: Some("change a.txt".into()),
            status: ActionStatus::Withdrawn,
            worker_result: None,
            coordinator_replied: false,
            created_at: Utc::now(),
        });
        conversation.push(Role::Human, EntryKind::ActionWithdrawn { action_id, revision: 3 });

        let stored = serde_json::to_string(&conversation).unwrap();
        let reloaded: Conversation = serde_json::from_str(&stored).unwrap();
        assert_eq!(reloaded.actions[0].status, ActionStatus::Withdrawn);
        assert!(!reloaded.has_unmediated_worker_result());

        let projection = reloaded.coordinator_projection("why", "Project", None, &[]);
        let withdrawal = projection["recent_history"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["kind"] == "action_withdrawn")
            .expect("the withdrawal is projected");
        assert_eq!(withdrawal["scope"], "this_proposal_only");
        assert!(withdrawal["note"].as_str().unwrap().contains("Nothing was started"));
    }

    fn entry(kind: EntryKind) -> Entry {
        Entry { id: Uuid::new_v4(), role: Role::Human, kind, created_at: Utc::now() }
    }

    fn human(text: &str) -> EntryKind {
        EntryKind::HumanMessage { text: text.into() }
    }

    fn retryable_failure() -> EntryKind {
        EntryKind::RunFailed { role: Role::Coordinator, message: "failed".into(), retryable: true }
    }

    fn conversation_with(kinds: Vec<EntryKind>) -> Conversation {
        let mut conversation = Conversation::new(Uuid::new_v4());
        conversation.entries = kinds.into_iter().map(entry).collect();
        conversation
    }

    #[test]
    fn a_retryable_failure_after_an_unanswered_message_is_retryable() {
        let conversation = conversation_with(vec![human("first"), retryable_failure()]);
        assert_eq!(conversation.retryable_human_turn(), Some((0, "first")));
    }

    #[test]
    fn repeated_retryable_failures_keep_the_same_message_retryable() {
        let conversation = conversation_with(vec![human("first"), retryable_failure(), retryable_failure()]);
        assert_eq!(conversation.retryable_human_turn(), Some((0, "first")));
    }

    #[test]
    fn an_answered_message_or_a_refusal_is_not_retryable() {
        let answered = conversation_with(vec![human("first"), EntryKind::CoordinatorReply { text: "ok".into() }]);
        assert_eq!(answered.retryable_human_turn(), None);

        let refused = conversation_with(vec![
            human("first"),
            EntryKind::RunFailed { role: Role::Coordinator, message: "retired".into(), retryable: false },
        ]);
        assert_eq!(refused.retryable_human_turn(), None);
    }

    #[test]
    fn a_newer_message_after_a_failure_makes_only_the_newer_one_retryable() {
        let conversation = conversation_with(vec![human("first"), retryable_failure(), human("second")]);
        assert_eq!(conversation.retryable_human_turn(), None, "the newest message has no failure yet");

        let failed_again = conversation_with(vec![human("first"), retryable_failure(), human("second"), retryable_failure()]);
        assert_eq!(failed_again.retryable_human_turn(), Some((2, "second")));
    }

    #[test]
    fn a_saved_failure_without_the_flag_decodes_as_not_retryable_and_is_not_written_back() {
        let legacy: EntryKind = serde_json::from_value(serde_json::json!({
            "kind": "run_failed", "role": "coordinator", "message": "older"
        }))
        .expect("a pre-flag failure should decode");
        assert!(matches!(legacy, EntryKind::RunFailed { retryable: false, .. }));

        let refusal = EntryKind::RunFailed { role: Role::Coordinator, message: "x".into(), retryable: false };
        let saved = serde_json::to_value(&refusal).unwrap();
        assert!(saved.get("retryable").is_none(), "non-retryable entries keep the old shape");
    }

    #[test]
    fn a_decision_on_an_earlier_proposal_does_not_stop_a_failed_message_from_being_retried() {
        let decision = EntryKind::ProjectActionDecision { action_id: Uuid::new_v4(), approved: true };
        let applied = EntryKind::ProjectActionApplied { action_id: Uuid::new_v4(), task_id: Uuid::new_v4() };
        let conversation = conversation_with(vec![
            human("first"),
            retryable_failure(),
            decision,
            applied,
        ]);
        assert_eq!(conversation.retryable_human_turn(), Some((0, "first")));
    }

    #[test]
    fn a_refused_approval_of_an_earlier_proposal_does_not_stop_a_failed_message_from_being_retried() {
        let decision = EntryKind::ProjectActionDecision { action_id: Uuid::new_v4(), approved: true };
        let refused = EntryKind::ProjectActionRefused { action_id: Uuid::new_v4(), reason: "The Task no longer exists".into() };
        let conversation = conversation_with(vec![
            human("first"),
            retryable_failure(),
            decision,
            refused,
        ]);
        assert_eq!(conversation.retryable_human_turn(), Some((0, "first")));
    }

    #[test]
    fn a_reply_after_a_failure_answers_the_message() {
        let conversation = conversation_with(vec![
            human("first"),
            retryable_failure(),
            EntryKind::CoordinatorReply { text: "done".into() },
        ]);
        assert_eq!(conversation.retryable_human_turn(), None);
    }
}
