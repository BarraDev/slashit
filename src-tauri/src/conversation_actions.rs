//! Human-gated Task operations proposed from a Project Conversation.
//!
//! The Coordinator never mutates SlashIt from its own output. A provider
//! response becomes a [`ProjectAction`] in `Proposed` state; only a Human
//! decision moves it. Approval executes through the same stores the Task
//! board uses ([`crate::lifecycle::create`] and
//! [`crate::lifecycle::record_if_changed`]), so a Task created or edited here
//! is indistinguishable from one created or edited anywhere else. Nothing in
//! this module queues, starts, checks out or publishes a Task.
//!
//! # Persistence protocol
//!
//! The Conversation file and the Task file cannot be written atomically
//! together. The protocol instead orders the writes so that every crash point
//! is recoverable by repeating the same approval:
//!
//! 1. Persist `Approved` (the Human's decision) in the Conversation.
//! 2. Apply the mutation to the Task store.
//! 3. Persist `Applied` plus the Task reference in the Conversation.
//!
//! A crash after (1) or (2) leaves the action `Approved`; re-approving it is a
//! retry that must observe, not repeat, what step (2) did:
//!
//! - `CreateTask` names its Task id in the proposal. Step (2) is skipped when
//!   a Task with that id already exists, so a retry can never create a second
//!   Task. No title matching is involved.
//! - `EditTask` is compare-and-set against the values the Human saw. A retry
//!   that finds every field already at its approved value records success; a
//!   Task that holds neither the observed nor the approved value is refused.
//!
//! One residual case is accepted: a Task created in step (2), then deleted
//! before the retry, is created again, because "never created" and "created
//! then deleted" are indistinguishable without a tombstone. The Human approved
//! that Task, and the window requires a crash plus a deletion.
//!
//! Callers hold the Project's Conversation lock around every call.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::RwLock;
use uuid::Uuid;

use crate::config::Storage;
use crate::domain::conversation::{
    Conversation, CoordinatorOutput, EntryKind, FieldChange, ProjectAction, ProjectActionOutcome,
    ProjectActionStatus, Role, TaskMutation,
};
use crate::domain::{NewTask, Project, Task};
use crate::lifecycle::{self, Tasks};

/// Everything an approved operation touches. `project_id` is the scope the
/// Conversation belongs to; every Task target is validated against it here
/// rather than trusted from the proposal.
pub struct Scope<'a> {
    pub project_id: Uuid,
    pub tasks: &'a Tasks,
    pub storage: &'a Storage,
    pub projects: &'a RwLock<HashMap<Uuid, Project>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Reject,
}

/// Record a provider's CreateTask/EditTask output as a pending proposal.
/// Anything else is a programming error: Reply and DelegateToTask are not
/// Project actions.
pub fn record_proposal(
    conversation: &mut Conversation,
    project_id: Uuid,
    output: CoordinatorOutput,
    tasks: &HashMap<Uuid, Task>,
) -> Result<(), String> {
    let (explanation, mutation) = match output {
        CoordinatorOutput::CreateTask { text, title, description, priority, category } => (
            text,
            TaskMutation::create(&title, description.as_deref(), priority, category)?,
        ),
        CoordinatorOutput::EditTask {
            text, target_task_id, title, description, priority, category,
        } => {
            let target = tasks
                .get(&target_task_id)
                .ok_or("Coordinator proposed an unknown Task")?;
            if target.project_id != project_id {
                return Err("Coordinator proposed a Task from another Project".into());
            }
            (
                text,
                TaskMutation::edit(
                    target,
                    title.as_deref(),
                    description.as_deref(),
                    priority,
                    category,
                )?,
            )
        }
        CoordinatorOutput::Reply { .. } | CoordinatorOutput::DelegateToTask { .. } => {
            return Err("Output is not a Project action".into());
        }
    };
    let action = ProjectAction::proposed(explanation, mutation);
    let action_id = action.id;
    conversation.project_actions.push(action);
    conversation.push(Role::Coordinator, EntryKind::ProjectActionProposed { action_id });
    Ok(())
}

/// Apply the Human's decision on one Project action.
///
/// `Ok` means the Conversation on disk reflects the decision and, for an
/// approval, its recorded outcome. `Err` after an approval leaves the action
/// `Approved`, so the same call is the retry.
pub async fn decide(
    scope: &Scope<'_>,
    conversation: &mut Conversation,
    action_id: Uuid,
    decision: Decision,
) -> Result<(), String> {
    let status = conversation
        .project_action(action_id)
        .ok_or("Action not found")?
        .status;
    match (status, decision) {
        // A repeated approval of finished work is a no-op, never a second effect.
        (ProjectActionStatus::Applied, Decision::Approve) => return Ok(()),
        (ProjectActionStatus::Proposed, Decision::Reject) => {
            let mut candidate = conversation.clone();
            set_status(&mut candidate, action_id, ProjectActionStatus::Rejected, None);
            candidate.push(
                Role::Human,
                EntryKind::ProjectActionDecision { action_id, approved: false },
            );
            return persist_then_commit(scope, conversation, candidate);
        }
        (ProjectActionStatus::Proposed, Decision::Approve) => {
            let mut candidate = conversation.clone();
            set_status(&mut candidate, action_id, ProjectActionStatus::Approved, None);
            candidate.push(
                Role::Human,
                EntryKind::ProjectActionDecision { action_id, approved: true },
            );
            // Step 1 of the protocol: the decision is durable before any effect.
            persist_then_commit(scope, conversation, candidate)?;
        }
        (ProjectActionStatus::Approved, Decision::Approve) => {}
        (ProjectActionStatus::Approved, Decision::Reject) => {
            return Err("An approved action cannot be rejected".into());
        }
        _ => return Err("Action is no longer awaiting a decision".into()),
    }

    let mutation = conversation
        .project_action(action_id)
        .ok_or("Action not found")?
        .mutation
        .clone();
    let mut candidate = conversation.clone();
    match execute(scope, &mutation).await? {
        Execution::Applied { task_id, title } => {
            set_status(
                &mut candidate,
                action_id,
                ProjectActionStatus::Applied,
                Some(ProjectActionOutcome::Applied { task_id, title }),
            );
            candidate.push(Role::Coordinator, EntryKind::ProjectActionApplied { action_id, task_id });
        }
        Execution::Refused(reason) => {
            set_status(
                &mut candidate,
                action_id,
                ProjectActionStatus::Refused,
                Some(ProjectActionOutcome::Refused { reason: reason.clone() }),
            );
            candidate.push(Role::Coordinator, EntryKind::ProjectActionRefused { action_id, reason });
        }
    }
    // Step 3 of the protocol.
    persist_then_commit(scope, conversation, candidate)
}

/// The caller's Conversation never advances beyond what was persisted: the
/// candidate replaces it only after a successful save.
fn persist_then_commit(
    scope: &Scope<'_>,
    conversation: &mut Conversation,
    candidate: Conversation,
) -> Result<(), String> {
    save(scope, &candidate)?;
    *conversation = candidate;
    Ok(())
}

fn set_status(
    conversation: &mut Conversation,
    action_id: Uuid,
    status: ProjectActionStatus,
    outcome: Option<ProjectActionOutcome>,
) {
    if let Some(action) = conversation.project_actions.iter_mut().find(|a| a.id == action_id) {
        action.status = status;
        if outcome.is_some() {
            action.outcome = outcome;
        }
    }
}

fn save(scope: &Scope<'_>, conversation: &Conversation) -> Result<(), String> {
    scope.storage.save_conversation(conversation).map_err(|error| error.to_string())
}

enum Execution {
    Applied { task_id: Uuid, title: String },
    Refused(String),
}

/// Step 2 of the protocol. `Err` is transient (the Task store could not
/// persist); `Refused` is a definitive answer for the current state.
async fn execute(scope: &Scope<'_>, mutation: &TaskMutation) -> Result<Execution, String> {
    match mutation {
        TaskMutation::CreateTask { task_id, title, description, priority, category } => {
            let project = scope
                .projects
                .read()
                .await
                .get(&scope.project_id)
                .cloned()
                .ok_or("Project not found")?;
            if let Some(existing) = scope.tasks.read().await.get(task_id) {
                return Ok(if existing.project_id == scope.project_id {
                    Execution::Applied { task_id: existing.id, title: existing.title.clone() }
                } else {
                    Execution::Refused("The reserved Task id belongs to another Project".into())
                });
            }
            let new = NewTask {
                id: *task_id,
                project_id: scope.project_id,
                title: title.clone(),
                description: description.clone(),
                model: project.agent_config.model.unwrap_or_else(|| "sonnet".into()),
                planning_mode: false,
                dependencies: Vec::new(),
                category: category.clone(),
                priority: priority.clone(),
                complexity: Default::default(),
                impact: Default::default(),
                security_severity: Default::default(),
                github_issue_url: None,
                gitlab_issue_url: None,
                linear_ticket_id: None,
            };
            let task = lifecycle::create(scope.tasks, scope.storage, scope.project_id, move |existing| {
                Task::new_backlog(existing, new)
            })
            .await?;
            Ok(Execution::Applied { task_id: task.id, title: task.title })
        }
        TaskMutation::EditTask { target_task_id, changes, .. } => {
            edit_task(scope, *target_task_id, changes).await
        }
    }
}

enum EditCheck {
    Missing,
    OtherProject,
    AlreadyApplied(String),
    Stale,
    Applied(String),
}

async fn edit_task(
    scope: &Scope<'_>,
    task_id: Uuid,
    changes: &[FieldChange],
) -> Result<Execution, String> {
    let check = Mutex::new(EditCheck::Missing);
    // The compare-and-set runs inside the Task store's write, so no edit can
    // land between the staleness check and the change.
    let revise = |staged: &mut HashMap<Uuid, Task>| -> bool {
        let verdict;
        let mut changed = false;
        match staged.get_mut(&task_id) {
            None => verdict = EditCheck::Missing,
            Some(task) if task.project_id != scope.project_id => verdict = EditCheck::OtherProject,
            Some(task) if changes.iter().all(|c| c.is_applied_to(task)) => {
                verdict = EditCheck::AlreadyApplied(task.title.clone());
            }
            Some(task) if !changes.iter().all(|c| c.is_observed_in(task)) => verdict = EditCheck::Stale,
            Some(task) => {
                changes.iter().for_each(|c| c.apply(task));
                verdict = EditCheck::Applied(task.title.clone());
                changed = true;
            }
        }
        *check.lock().unwrap() = verdict;
        changed
    };
    let recorded = lifecycle::record_if_changed(scope.tasks, scope.storage, task_id, &revise).await;
    let verdict = check.into_inner().unwrap();
    if let Err(error) = recorded {
        // The store reports an absent Task as an error; a real write failure
        // leaves the Task present and is retryable.
        return if scope.tasks.read().await.contains_key(&task_id) {
            Err(error)
        } else {
            Ok(Execution::Refused("The Task no longer exists".into()))
        };
    }
    Ok(match verdict {
        EditCheck::Applied(title) | EditCheck::AlreadyApplied(title) => {
            Execution::Applied { task_id, title }
        }
        EditCheck::Missing => Execution::Refused("The Task no longer exists".into()),
        EditCheck::OtherProject => Execution::Refused("The Task belongs to another Project".into()),
        EditCheck::Stale => Execution::Refused(
            "The Task changed after this edit was proposed; nothing was changed".into(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::paths::{AppPaths, StateLocation};
    use crate::domain::conversation::{Conversation, CoordinatorOutput};
    use crate::domain::{
        AgentConfig, AgentType, ProjectScope, TaskCategory, TaskPriority, TaskStatus,
    };
    use std::sync::Arc;

    struct World {
        _tmp: tempfile::TempDir,
        paths: AppPaths,
        tasks: Tasks,
        projects: RwLock<HashMap<Uuid, Project>>,
        storage: Storage,
        project_id: Uuid,
    }

    impl World {
        fn new() -> Self {
            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path();
            std::fs::create_dir_all(root.join("config")).unwrap();
            let paths = AppPaths::with_roots(
                root.join("config"),
                root.join("data"),
                root.join("cache"),
                root.join("runtime"),
            );
            let project_id = Uuid::new_v4();
            let project = Project {
                id: project_id,
                name: "p".into(),
                repository_id: None,
                scope: ProjectScope::Standalone,
                state_location: StateLocation::External,
                base: None,
                agent_type: AgentType::ClaudeCode,
                agent_config: AgentConfig {
                    agent_type: AgentType::ClaudeCode,
                    command: "claude".into(),
                    args: Vec::new(),
                    env: HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            Self {
                storage: Storage::with_paths(paths.clone()),
                paths,
                tasks: Arc::new(RwLock::new(HashMap::new())),
                projects: RwLock::new(HashMap::from([(project_id, project)])),
                project_id,
                _tmp: tmp,
            }
        }

        fn scope(&self) -> Scope<'_> {
            Scope {
                project_id: self.project_id,
                tasks: &self.tasks,
                storage: &self.storage,
                projects: &self.projects,
            }
        }

        fn conversation(&self) -> Conversation {
            self.storage.create_primary_conversation(self.project_id).unwrap()
        }

        async fn seed_task(&self, project_id: Uuid, title: &str) -> Task {
            lifecycle::create(&self.tasks, &self.storage, project_id, |existing| {
                Task::new_backlog(
                    existing,
                    NewTask {
                        id: Uuid::new_v4(),
                        project_id,
                        title: title.into(),
                        description: Some("original".into()),
                        model: "sonnet".into(),
                        planning_mode: false,
                        dependencies: vec![],
                        category: Default::default(),
                        priority: Default::default(),
                        complexity: Default::default(),
                        impact: Default::default(),
                        security_severity: Default::default(),
                        github_issue_url: None,
                        gitlab_issue_url: None,
                        linear_ticket_id: None,
                    },
                )
            })
            .await
            .unwrap()
        }

        async fn propose(&self, conversation: &mut Conversation, json: &str) -> Result<Uuid, String> {
            let output = Conversation::parse_output(json)?;
            let tasks = self.tasks.read().await;
            record_proposal(conversation, self.project_id, output, &tasks)?;
            Ok(conversation.project_actions.last().unwrap().id)
        }

        /// Block every Task write by placing a file where the tasks directory belongs.
        fn block_task_persistence(&self) {
            let blocker = self.paths.config_dir().join("tasks");
            let _ = std::fs::remove_dir_all(&blocker);
            std::fs::write(&blocker, b"not a directory").unwrap();
        }

        fn unblock_task_persistence(&self) {
            std::fs::remove_file(self.paths.config_dir().join("tasks")).unwrap();
        }
    }

    const CREATE: &str = r#"{"type":"create_task","text":"Worth tracking","title":"Add dark mode","description":"Support a dark theme","priority":"high"}"#;

    #[test]
    fn create_and_edit_proposals_are_strict_and_bounded() {
        let parsed = Conversation::parse_output(CREATE).unwrap();
        assert!(matches!(parsed, CoordinatorOutput::CreateTask { priority: Some(TaskPriority::High), .. }));
        for bad in [
            r#"{"type":"create_task","text":"x","title":"  "}"#.to_string(),
            r#"{"type":"create_task","text":"x"}"#.to_string(),
            r#"{"type":"create_task","text":"x","title":"t","status":"in_progress"}"#.to_string(),
            r#"{"type":"create_task","text":"x","title":"t","priority":"someday"}"#.to_string(),
            format!(r#"{{"type":"create_task","text":"x","title":"{}"}}"#, "t".repeat(201)),
            format!(r#"{{"type":"create_task","text":"x","title":"t","description":"{}"}}"#, "d".repeat(8_001)),
            format!(r#"{{"type":"edit_task","text":"x","target_task_id":"{}"}}"#, Uuid::new_v4()),
            format!(r#"{{"type":"edit_task","text":"x","target_task_id":"{}","status":"done"}}"#, Uuid::new_v4()),
        ] {
            assert!(Conversation::parse_output(&bad).is_err(), "accepted: {bad}");
        }
    }

    #[tokio::test]
    async fn approving_create_makes_exactly_one_backlog_task_without_execution_state() {
        let world = World::new();
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, CREATE).await.unwrap();
        assert!(world.tasks.read().await.is_empty(), "a proposal must not create anything");

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let tasks = world.tasks.read().await;
        assert_eq!(tasks.len(), 1);
        let task = tasks.values().next().unwrap();
        assert_eq!(task.project_id, world.project_id);
        assert_eq!(task.status, TaskStatus::Backlog);
        assert_eq!((task.title.as_str(), task.priority.clone()), ("Add dark mode", TaskPriority::High));
        assert_eq!(task.category, TaskCategory::Feature);
        assert!(task.worktree_path.is_none() && task.branch_name.is_none() && task.worktree_id.is_none());
        assert_eq!(task.phase, crate::domain::TaskPhase::Idle);
        let action = conversation.project_action(id).unwrap();
        assert_eq!(action.status, ProjectActionStatus::Applied);
        assert_eq!(
            action.outcome,
            Some(ProjectActionOutcome::Applied { task_id: task.id, title: task.title.clone() })
        );
        let persisted = world.storage.load_project_tasks(world.project_id).unwrap();
        assert_eq!(persisted.len(), 1, "the Task must be on disk, not only in memory");
    }

    #[tokio::test]
    async fn rejecting_create_changes_nothing_and_cannot_be_approved_later() {
        let world = World::new();
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, CREATE).await.unwrap();
        decide(&world.scope(), &mut conversation, id, Decision::Reject).await.unwrap();
        assert!(world.tasks.read().await.is_empty());
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Rejected);
        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        assert!(world.tasks.read().await.is_empty());
    }

    #[tokio::test]
    async fn repeated_and_restarted_approvals_never_create_a_second_task() {
        let world = World::new();
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, CREATE).await.unwrap();
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert_eq!(world.tasks.read().await.len(), 1);

        // A restart reloads both stores from disk and the old approval is replayed.
        let mut reloaded = world.storage.load_primary_conversation(world.project_id).unwrap().unwrap();
        decide(&world.scope(), &mut reloaded, id, Decision::Approve).await.unwrap();
        assert_eq!(world.tasks.read().await.len(), 1);
        assert_eq!(world.storage.load_project_tasks(world.project_id).unwrap().len(), 1);
        let applied = reloaded.entries.iter()
            .filter(|e| matches!(e.kind, EntryKind::ProjectActionApplied { .. })).count();
        assert_eq!(applied, 1);
    }

    #[tokio::test]
    async fn a_crash_between_task_creation_and_the_recorded_outcome_is_recovered_without_a_duplicate() {
        let world = World::new();
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, CREATE).await.unwrap();
        // Replay the first two protocol steps by hand: durable Approved, then the Task.
        conversation.project_actions[0].status = ProjectActionStatus::Approved;
        conversation.push(Role::Human, EntryKind::ProjectActionDecision { action_id: id, approved: true });
        world.storage.save_conversation(&conversation).unwrap();
        execute(&world.scope(), &conversation.project_actions[0].mutation.clone()).await.unwrap();
        assert_eq!(world.tasks.read().await.len(), 1);

        let mut restarted = world.storage.load_primary_conversation(world.project_id).unwrap().unwrap();
        assert_eq!(restarted.project_action(id).unwrap().status, ProjectActionStatus::Approved);
        decide(&world.scope(), &mut restarted, id, Decision::Approve).await.unwrap();

        assert_eq!(world.tasks.read().await.len(), 1);
        assert_eq!(restarted.project_action(id).unwrap().status, ProjectActionStatus::Applied);
        let decisions = restarted.entries.iter()
            .filter(|e| matches!(e.kind, EntryKind::ProjectActionDecision { .. })).count();
        assert_eq!(decisions, 1, "a retry must not record a second Human decision");
    }

    /// Make the next Conversation save fail by putting a directory where its file belongs.
    /// Returns the durable bytes so they can be restored afterwards.
    fn block_conversation_persistence(world: &World, conversation: &Conversation) -> Vec<u8> {
        let path = world.paths.conversation_file(conversation.id);
        let durable = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        durable
    }

    fn unblock_conversation_persistence(world: &World, conversation: &Conversation, durable: &[u8]) {
        let path = world.paths.conversation_file(conversation.id);
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, durable).unwrap();
    }

    #[tokio::test]
    async fn a_failed_outcome_save_keeps_the_caller_at_the_durable_approved_state_and_retries_cleanly() {
        let world = World::new();
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, CREATE).await.unwrap();
        // Steps 1 and 2 by hand: durable Approved, then the Task exists.
        conversation.project_actions[0].status = ProjectActionStatus::Approved;
        conversation.push(Role::Human, EntryKind::ProjectActionDecision { action_id: id, approved: true });
        world.storage.save_conversation(&conversation).unwrap();
        execute(&world.scope(), &conversation.project_actions[0].mutation.clone()).await.unwrap();
        // Step 3 fails: only the final Conversation save is blocked.
        let durable = block_conversation_persistence(&world, &conversation);

        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Approved);
        assert_eq!(world.tasks.read().await.len(), 1);

        unblock_conversation_persistence(&world, &conversation, &durable);
        let persisted = world.storage.load_primary_conversation(world.project_id).unwrap();
        assert_eq!(
            persisted.unwrap().project_action(id).unwrap().status,
            ProjectActionStatus::Approved,
            "disk must not claim more than was saved"
        );

        // Retry with the very same in-memory object.
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        let persisted = world.storage.load_primary_conversation(world.project_id).unwrap().unwrap();
        assert_eq!(persisted.project_action(id).unwrap().status, ProjectActionStatus::Applied);
        assert_eq!(world.tasks.read().await.len(), 1);
        let decisions = persisted.entries.iter()
            .filter(|e| matches!(e.kind, EntryKind::ProjectActionDecision { .. })).count();
        assert_eq!(decisions, 1);
    }

    #[tokio::test]
    async fn a_failed_approval_save_leaves_memory_and_disk_proposed_and_creates_no_task() {
        let world = World::new();
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, CREATE).await.unwrap();
        world.storage.save_conversation(&conversation).unwrap();
        let durable = block_conversation_persistence(&world, &conversation);
        // Renaming a file onto a directory fails regardless of privileges.
        assert!(world.paths.conversation_file(conversation.id).is_dir());

        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Proposed);
        assert!(conversation.entries.iter().all(|e| !matches!(e.kind, EntryKind::ProjectActionDecision { .. })));
        assert!(world.tasks.read().await.is_empty());

        assert!(decide(&world.scope(), &mut conversation, id, Decision::Reject).await.is_err());
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Proposed);

        // The durable Conversation is untouched: no Task, no Human decision, still Proposed.
        unblock_conversation_persistence(&world, &conversation, &durable);
        let persisted = world.storage.load_primary_conversation(world.project_id).unwrap().unwrap();
        assert_eq!(persisted.project_action(id).unwrap().status, ProjectActionStatus::Proposed);
        assert!(persisted.entries.iter().all(|e| !matches!(e.kind, EntryKind::ProjectActionDecision { .. })));
        assert!(world.storage.load_project_tasks(world.project_id).unwrap().is_empty());

        // Retrying with the same in-memory object now succeeds exactly once.
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert_eq!(world.tasks.read().await.len(), 1);
        let persisted = world.storage.load_primary_conversation(world.project_id).unwrap().unwrap();
        assert_eq!(persisted.project_action(id).unwrap().status, ProjectActionStatus::Applied);
        let decisions = persisted.entries.iter()
            .filter(|e| matches!(e.kind, EntryKind::ProjectActionDecision { .. })).count();
        assert_eq!(decisions, 1);
    }

    #[tokio::test]
    async fn a_failed_task_write_leaves_the_action_retryable_and_creates_nothing() {
        let world = World::new();
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, CREATE).await.unwrap();
        world.block_task_persistence();

        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        assert!(world.tasks.read().await.is_empty(), "a failed save must not show a Task");
        let persisted = world.storage.load_primary_conversation(world.project_id).unwrap().unwrap();
        assert_eq!(persisted.project_action(id).unwrap().status, ProjectActionStatus::Approved);

        world.unblock_task_persistence();
        let mut retry = persisted;
        decide(&world.scope(), &mut retry, id, Decision::Approve).await.unwrap();
        assert_eq!(world.tasks.read().await.len(), 1);
    }

    #[tokio::test]
    async fn approval_is_refused_when_the_project_is_gone() {
        let world = World::new();
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, CREATE).await.unwrap();
        world.projects.write().await.clear();
        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        assert!(world.tasks.read().await.is_empty());
    }

    fn edit_json(task_id: Uuid, fields: &str) -> String {
        format!(r#"{{"type":"edit_task","text":"Clarify","target_task_id":"{task_id}",{fields}}}"#)
    }

    #[tokio::test]
    async fn approving_edit_changes_only_the_approved_fields() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Old title").await;
        let mut conversation = world.conversation();
        let id = world
            .propose(&mut conversation, &edit_json(task.id, r#""title":"New title","priority":"urgent""#))
            .await
            .unwrap();
        assert_eq!(world.tasks.read().await[&task.id].title, "Old title");

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let after = world.tasks.read().await[&task.id].clone();
        assert_eq!((after.title.as_str(), after.priority.clone()), ("New title", TaskPriority::Urgent));
        assert_eq!(after.description, task.description);
        assert_eq!(after.category, task.category);
        assert_eq!(after.status, TaskStatus::Backlog);
        assert!(after.worktree_path.is_none() && after.branch_name.is_none());
        assert_eq!(world.storage.load_project_tasks(world.project_id).unwrap()
            .into_iter().find(|t| t.id == task.id).unwrap().title, "New title");
    }

    #[tokio::test]
    async fn rejecting_edit_changes_nothing() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Old title").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &edit_json(task.id, r#""title":"New""#)).await.unwrap();
        decide(&world.scope(), &mut conversation, id, Decision::Reject).await.unwrap();
        assert_eq!(world.tasks.read().await[&task.id].title, "Old title");
        assert_eq!(world.tasks.read().await[&task.id].updated_at, task.updated_at);
    }

    #[tokio::test]
    async fn an_edit_cannot_target_another_projects_or_an_unknown_task() {
        let world = World::new();
        let other = world.seed_task(Uuid::new_v4(), "Foreign").await;
        let mut conversation = world.conversation();
        let foreign = world.propose(&mut conversation, &edit_json(other.id, r#""title":"Hijack""#)).await;
        assert!(foreign.unwrap_err().contains("another Project"));
        let unknown = world.propose(&mut conversation, &edit_json(Uuid::new_v4(), r#""title":"x""#)).await;
        assert!(unknown.unwrap_err().contains("unknown Task"));
        assert!(conversation.project_actions.is_empty());
        assert_eq!(world.tasks.read().await[&other.id].title, "Foreign");
    }

    #[tokio::test]
    async fn approving_an_edit_for_a_deleted_task_is_refused() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Doomed").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &edit_json(task.id, r#""title":"x""#)).await.unwrap();
        world.tasks.write().await.remove(&task.id);

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let action = conversation.project_action(id).unwrap();
        assert_eq!(action.status, ProjectActionStatus::Refused);
        assert!(matches!(action.outcome, Some(ProjectActionOutcome::Refused { .. })));
        assert!(world.tasks.read().await.is_empty());
    }

    #[tokio::test]
    async fn a_stale_edit_is_refused_and_leaves_newer_state_untouched() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Old title").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &edit_json(task.id, r#""title":"Proposed""#)).await.unwrap();
        // A user edits the title after the proposal and before approval.
        lifecycle::commit_task(&world.tasks, &world.storage, task.id, &|staged: &mut HashMap<Uuid, Task>| {
            staged.get_mut(&task.id).unwrap().title = "Edited by the user".into();
        })
        .await
        .unwrap();

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Refused);
        assert_eq!(world.tasks.read().await[&task.id].title, "Edited by the user");
        // The refusal is terminal: approving again does not apply the stale edit.
        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        assert_eq!(world.tasks.read().await[&task.id].title, "Edited by the user");
    }

    #[tokio::test]
    async fn repeated_edit_approval_is_idempotent_and_recovers_after_a_lost_outcome() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Old title").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &edit_json(task.id, r#""title":"New""#)).await.unwrap();
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        let stamp = world.tasks.read().await[&task.id].updated_at;
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert_eq!(world.tasks.read().await[&task.id].updated_at, stamp, "a repeat must not rewrite the Task");

        // Crash after the Task write, before the Conversation recorded it.
        conversation.project_actions[0].status = ProjectActionStatus::Approved;
        conversation.project_actions[0].outcome = None;
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Applied);
        assert_eq!(world.tasks.read().await[&task.id].updated_at, stamp);
    }

    #[test]
    fn conversations_saved_before_project_actions_still_load_unchanged() {
        let legacy = serde_json::json!({
            "id": Uuid::new_v4(), "project_id": Uuid::new_v4(), "revision": 3,
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
            "entries": [{"id": Uuid::new_v4(), "role": "coordinator", "created_at": "2026-01-01T00:00:00Z",
                         "kind": {"kind": "action_proposed", "action_id": Uuid::new_v4()}}],
            "actions": [{"id": Uuid::new_v4(), "target_task_id": Uuid::new_v4(), "target_task_title": "T",
                         "request": "r", "explanation": "e", "approved_request": null, "status": "proposed",
                         "worker_result": null, "created_at": "2026-01-01T00:00:00Z"}]
        });
        let conversation: Conversation = serde_json::from_value(legacy).unwrap();
        assert!(conversation.project_actions.is_empty());
        assert_eq!(conversation.actions.len(), 1);
        assert_eq!(conversation.actions[0].status, crate::domain::conversation::ActionStatus::Proposed);
    }

    #[test]
    fn delegate_and_reply_output_still_parse_and_are_not_project_actions() {
        let target = Uuid::new_v4();
        assert!(matches!(
            Conversation::parse_output(&format!(
                r#"{{"type":"delegate_to_task","text":"t","target_task_id":"{target}","request":"r"}}"#
            )),
            Ok(CoordinatorOutput::DelegateToTask { .. })
        ));
        let mut conversation = Conversation::new(Uuid::new_v4());
        let reply = Conversation::parse_output(r#"{"type":"reply","text":"hi"}"#).unwrap();
        assert!(record_proposal(&mut conversation, Uuid::new_v4(), reply, &HashMap::new()).is_err());
        assert!(conversation.project_actions.is_empty());
    }
}
