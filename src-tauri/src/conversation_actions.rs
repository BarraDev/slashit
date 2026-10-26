// SPDX-License-Identifier: MPL-2.0
//! Human-gated Task operations proposed from a Project Conversation.
//!
//! The Coordinator never mutates SlashIt from its own output. A provider
//! response becomes a [`ProjectAction`] in `Proposed` state; only a Human
//! decision moves it. Approval executes through the same stores the Task
//! board uses ([`crate::lifecycle::create`] and
//! [`crate::lifecycle::record_if_changed`]), so a Task created, edited or
//! moved here is indistinguishable from one changed anywhere else. Nothing in
//! this module starts, checks out or publishes a Task; `EnqueueTask` only puts
//! a Backlog Task in Queue and leaves admission to the scheduler.
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
//! - `MoveTask` is compare-and-set on the status the Human saw. A Task already
//!   at the approved status records success; any other status is refused.
//! - `EnqueueTask` is compare-and-set on `Backlog` and on the coding input the
//!   Human saw (`queue::prompt::task_input_digest`, recorded with the
//!   proposal): a Task edited since is refused, not queued, and a queued Task
//!   carries that input to the executor, which checks it again before the run
//!   starts. A proposal saved before the input was recorded never showed the
//!   Human what would run, so approving it is refused and must be proposed
//!   again. A Task already in Queue is
//!   recorded as found queued, not as queued by this approval: the record
//!   cannot tell a retry that follows a crash from an unrelated enqueue. A
//!   Task that has moved on to any other column is refused.
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
    Conversation, CoordinatorOutput, EntryKind, ExistingCheckoutEvidence, FieldChange, ProjectAction,
    ProjectActionOutcome, ProjectActionStatus, Role, TaskMutation, coordinator_may_move,
};
use crate::domain::{NewTask, Project, Task, TaskStatus};
use crate::lifecycle::{
    self, EnqueueVerdict, ProjectLifecycleGate, StatusTransitionEffect, TaskLifecycleLocks, Tasks,
};
use crate::worktree::WorktreeManager;

/// Everything an approved operation touches. `project_id` is the scope the
/// Conversation belongs to; every Task target is validated against it here
/// rather than trusted from the proposal.
pub struct Scope<'a> {
    pub project_id: Uuid,
    pub tasks: &'a Tasks,
    pub storage: &'a Storage,
    pub projects: &'a RwLock<HashMap<Uuid, Project>>,
    /// The Task lifecycle's own serialization, taken by every status move.
    pub locks: &'a TaskLifecycleLocks,
    /// The Project lifecycle gate; creation takes it shared so a Project
    /// deletion cannot be deciding mid-birth.
    pub gate: &'a ProjectLifecycleGate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Reject,
}

/// Reads the Task Checkout of `task` in the repository at `repo_path`, for an
/// additional request that would reuse it.
///
/// Only the Task's own checkout qualifies: its recorded path, its own branch,
/// registered to that branch, on a commit that descends from the Task's
/// `base_commit`, with no uncommitted work. Each refusal says why. The read is
/// point-in-time; approval and the run start read it again.
pub async fn inspect_existing_checkout(repo_path: &str, task: &Task) -> Result<ExistingCheckoutEvidence, String> {
    let (Some(checkout), Some(branch)) = (task.worktree_path.as_deref(), task.branch_name.as_deref()) else {
        return Err("This Task has no complete Task Checkout to reuse".into());
    };
    let Some(base) = task.base_commit.clone() else {
        return Err(
            "This Task's checkout has no recorded base commit, so its Task diff cannot be established \
             and it cannot be reused. Propose a new Task for distinct work."
                .into(),
        );
    };
    if branch != WorktreeManager::branch_for_task(task.id) {
        return Err("The recorded checkout is not on this Task's own branch, so it cannot be reused".into());
    }
    let repository_path = std::fs::canonicalize(repo_path)
        .map_err(|e| format!("The repository could not be resolved: {e}"))?
        .to_string_lossy()
        .into_owned();
    // The user's own checkout is never a Task Checkout, even when the Task's
    // record names it, its branch is checked out there, or the path is a
    // symlink to it. `reattach` refuses the same classification.
    WorktreeManager::refuse_primary_checkout(&repository_path, checkout).await?;
    let inspection = crate::worktree::new_attempt::inspect(&repository_path, checkout, branch).await?;
    if !inspection.dirty_paths.is_empty() {
        return Err(
            "This Task's checkout has uncommitted changes. Commit or discard them before an additional \
             request can reuse it."
                .into(),
        );
    }
    let descends = crate::worktree::restack::is_ancestor(
        std::path::Path::new(&repository_path),
        &base,
        &inspection.head,
    )
    .await?;
    if !descends {
        return Err("The checkout's commit does not descend from this Task's recorded base commit".into());
    }
    Ok(ExistingCheckoutEvidence {
        project_id: task.project_id,
        task_id: task.id,
        repository_path,
        checkout_path: checkout.to_string(),
        git_entry_identity: crate::worktree::new_attempt::git_entry_identity(checkout),
        branch: branch.to_string(),
        branch_ref: format!("refs/heads/{branch}"),
        head: inspection.head,
        base_commit: base,
        run: task.next_run(),
    })
}

/// Re-reads the checkout a proposal named and refuses any difference from what
/// the Human was shown. Called under the Task's lifecycle lease before an
/// approval queues anything.
pub async fn recheck_existing_checkout(task: &Task, shown: &ExistingCheckoutEvidence) -> Result<(), String> {
    if let Some(reason) = task.existing_checkout_refusal() {
        return Err(reason);
    }
    verify_checkout_unchanged(task, shown).await
}

/// Re-reads the checkout that approved evidence names and refuses any
/// difference from it. Approval reaches it through [`recheck_existing_checkout`]
/// and the executor calls it again after the Task is Queued, before the run is
/// recorded. The Task is no longer Backlog by then, so only its settled state
/// is checked here.
///
/// The read is point-in-time. Holding the Task's lifecycle lease stops SlashIt
/// from changing the Task meanwhile; it cannot stop a Git or file writer
/// outside SlashIt.
pub async fn verify_checkout_unchanged(task: &Task, shown: &ExistingCheckoutEvidence) -> Result<(), String> {
    if let Some(reason) = task.settled_checkout_refusal() {
        return Err(reason);
    }
    let now = inspect_existing_checkout(&shown.repository_path, task).await?;
    let same_identity = match (&shown.git_entry_identity, &now.git_entry_identity) {
        (Some(before), Some(after)) => before == after,
        _ => false,
    };
    if now.project_id != shown.project_id
        || now.task_id != shown.task_id
        || now.repository_path != shown.repository_path
        || now.checkout_path != shown.checkout_path
        || !same_identity
        || now.branch != shown.branch
        || now.branch_ref != shown.branch_ref
        || now.head != shown.head
        || now.base_commit != shown.base_commit
        || now.run != shown.run
    {
        return Err(
            "The Task's checkout or branch changed after this request was proposed, so nothing was started. \
             Propose the request again to review the current checkout."
                .into(),
        );
    }
    Ok(())
}

/// Record a provider's CreateTask/EditTask output as a pending proposal.
/// Anything else is a programming error: Reply and DelegateToTask are not
/// Project actions.
pub fn record_proposal(
    conversation: &mut Conversation,
    project_id: Uuid,
    output: CoordinatorOutput,
    tasks: &HashMap<Uuid, Task>,
    checkout: Option<ExistingCheckoutEvidence>,
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
        CoordinatorOutput::MoveTask { text, target_task_id, to } => {
            let target = tasks
                .get(&target_task_id)
                .ok_or("Coordinator proposed an unknown Task")?;
            if target.project_id != project_id {
                return Err("Coordinator proposed a Task from another Project".into());
            }
            (text, TaskMutation::move_task(target, to)?)
        }
        CoordinatorOutput::EnqueueTask { text, target_task_id } => {
            let target = tasks
                .get(&target_task_id)
                .ok_or("Coordinator proposed an unknown Task")?;
            if target.project_id != project_id {
                return Err("Coordinator proposed a Task from another Project".into());
            }
            (
                text,
                TaskMutation::enqueue_task(target, crate::queue::prompt::task_input_digest(target))?,
            )
        }
        CoordinatorOutput::ExecuteRequest { text, target_task_id, request } => {
            let target = tasks
                .get(&target_task_id)
                .ok_or("Coordinator proposed an unknown Task")?;
            if target.project_id != project_id {
                return Err("Coordinator proposed a Task from another Project".into());
            }
            (text, TaskMutation::execute_request(target, &request, checkout)?)
        }
        CoordinatorOutput::Reply { .. }
        | CoordinatorOutput::DelegateToTask { .. }
        | CoordinatorOutput::InspectTask { .. }
        | CoordinatorOutput::ListTasks { .. }
        | CoordinatorOutput::InspectTaskActivity { .. }
        | CoordinatorOutput::InspectTaskPullRequest { .. }
        | CoordinatorOutput::InspectProject {} => {
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
    match execute(scope, action_id, &mutation).await? {
        Execution::Applied { task_id, title } => {
            set_status(
                &mut candidate,
                action_id,
                ProjectActionStatus::Applied,
                Some(ProjectActionOutcome::Applied { task_id, title }),
            );
            candidate.push(Role::Coordinator, EntryKind::ProjectActionApplied { action_id, task_id });
        }
        Execution::AlreadyQueued { task_id, title } => {
            set_status(
                &mut candidate,
                action_id,
                ProjectActionStatus::Applied,
                Some(ProjectActionOutcome::AlreadyQueued { task_id, title }),
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
    AlreadyQueued { task_id: Uuid, title: String },
    Refused(String),
}

/// Step 2 of the protocol. `Err` is transient (the Task store could not
/// persist); `Refused` is a definitive answer for the current state.
async fn execute(scope: &Scope<'_>, action_id: Uuid, mutation: &TaskMutation) -> Result<Execution, String> {
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
            let task = lifecycle::create(
                scope.gate,
                scope.tasks,
                scope.projects,
                scope.storage,
                scope.project_id,
                move |existing| {
                Task::new_backlog(existing, new)
            })
            .await?;
            Ok(Execution::Applied { task_id: task.id, title: task.title })
        }
        TaskMutation::EditTask { target_task_id, changes, .. } => {
            edit_task(scope, *target_task_id, changes).await
        }
        TaskMutation::MoveTask { target_task_id, from, to, .. } => {
            move_task(scope, *target_task_id, from, to).await
        }
        TaskMutation::EnqueueTask { target_task_id, from, input, .. } => {
            // A proposal saved before the input was recorded never showed the Human
            // what would run, so approving it cannot authorize queueing: the empty
            // digest matches no Task, and the write refuses it atomically.
            enqueue_task(scope, *target_task_id, from, lifecycle::InputApproval {
                id: action_id,
                digest: input.as_ref().map_or("", |i| i.digest.as_str()),
                request: None,
                checkout: None,
            })
            .await
        }
        // The request and the digest the Human saw travel together. The Task
        // is compared with `approved_input_digest(task, Some(request))`, so an
        // edit to the Task or a change to the request fails the comparison.
        TaskMutation::ExecuteRequest { target_task_id, from, request, input, checkout, .. } => {
            // A request that reuses a checkout is rechecked against Git under
            // the Task's lifecycle lease inside `enqueue_task`, never before
            // it. A Task no longer in Backlog is left to the queue there, which
            // answers a retry of this same approval.
            match enqueue_task(scope, *target_task_id, from, lifecycle::InputApproval {
                id: action_id,
                digest: input.digest.as_str(),
                request: Some(request.as_str()),
                checkout: checkout.as_deref(),
            })
            .await?
            {
                // A retry finds the Task queued by this approval itself, bound
                // to it or already run under it: that is Applied. Anything else
                // queued the Task without this request, so reporting it as
                // applied would claim a run that will not carry the text.
                Execution::AlreadyQueued { task_id, title } => {
                    let carries_this_request = scope.tasks.read().await.get(&task_id).is_some_and(|task| {
                        task.approved_execution.as_ref().is_some_and(|b| b.approval_id == action_id)
                            || task.executed_approvals.iter().any(|e| e.approval_id == action_id)
                    });
                    Ok(if carries_this_request {
                        Execution::Applied { task_id, title }
                    } else {
                        Execution::Refused(
                            "The Task is already queued, so the additional request was not attached".into(),
                        )
                    })
                }
                other => Ok(other),
            }
        }
    }
}

enum MoveCheck {
    Missing,
    OtherProject,
    NotAllowed,
    AlreadyApplied(String),
    Stale,
    Applied(String),
}

/// Move a Task through the same transition the board uses: the one classifier,
/// the same activity record, the same reset of execution state. The target is
/// only ever `Backlog` (see [`coordinator_may_move`]), so nothing here queues,
/// starts or checks out anything.
///
/// Unlike the board, this does not call `end_active_ownership`. That ends a
/// live run or reviewer before the write, which is an external side effect
/// ahead of a fallible store write and ahead of the compare-and-set below. It
/// is only needed for a Task whose status disagrees with its owner: a
/// promotion to In Progress is atomic with the start of a run, so a Task still
/// in Queue has none, and a Task in Error has already ended its run. Should
/// either status ever coexist with a live owner, the move would leave that
/// owner running against a Backlog Task; the Coordinator path holds no
/// executor handle to close that, and the board remains the way to stop one.
///
/// The lifecycle lease is held so a move cannot interleave with another
/// transition of the same Task, and the status compare-and-set runs inside the
/// Task store's write so the Task cannot move between the check and the change.
async fn move_task(
    scope: &Scope<'_>,
    task_id: Uuid,
    from: &TaskStatus,
    to: &TaskStatus,
) -> Result<Execution, String> {
    // Re-validated here rather than trusted from the persisted proposal.
    if !coordinator_may_move(from, to) {
        return Ok(Execution::Refused("That move is not available to the Coordinator".into()));
    }
    let _lease = scope.locks.acquire(task_id).await?;
    let check = Mutex::new(MoveCheck::Missing);
    let revise = |staged: &mut HashMap<Uuid, Task>| -> bool {
        let verdict;
        let mut changed = false;
        match staged.get_mut(&task_id) {
            None => verdict = MoveCheck::Missing,
            Some(task) if task.project_id != scope.project_id => verdict = MoveCheck::OtherProject,
            Some(task) if task.status == *to => verdict = MoveCheck::AlreadyApplied(task.title.clone()),
            Some(task) if task.status != *from => verdict = MoveCheck::Stale,
            Some(task) if !coordinator_may_move(&task.status, to) => verdict = MoveCheck::NotAllowed,
            Some(_) => {
                // The end of the destination column, taken before the Task
                // leaves its own status so it can never count itself. Status
                // and position land in the same staged write.
                let position = (*to == TaskStatus::Backlog)
                    .then(|| Task::next_backlog_position(staged, scope.project_id));
                let task = staged.get_mut(&task_id).expect("checked present above");
                let previous = task.status.clone();
                task.status = to.clone();
                if let Some(position) = position {
                    task.position = position;
                }
                task.record_move(&previous);
                if let StatusTransitionEffect::ResetExecutionState =
                    lifecycle::classify_status_transition(&previous, to)
                {
                    task.reset_execution_state();
                }
                verdict = MoveCheck::Applied(task.title.clone());
                changed = true;
            }
        }
        *check.lock().unwrap() = verdict;
        changed
    };
    let recorded = lifecycle::record_if_changed(scope.tasks, scope.storage, task_id, &revise).await;
    let verdict = check.into_inner().unwrap();
    if let Err(error) = recorded {
        return if scope.tasks.read().await.contains_key(&task_id) {
            Err(error)
        } else {
            Ok(Execution::Refused("The Task no longer exists".into()))
        };
    }
    Ok(match verdict {
        MoveCheck::Applied(title) | MoveCheck::AlreadyApplied(title) => {
            Execution::Applied { task_id, title }
        }
        MoveCheck::Missing => Execution::Refused("The Task no longer exists".into()),
        MoveCheck::OtherProject => Execution::Refused("The Task belongs to another Project".into()),
        MoveCheck::NotAllowed => Execution::Refused("That move is not available to the Coordinator".into()),
        MoveCheck::Stale => Execution::Refused(
            "The Task moved after this change was proposed; nothing was changed".into(),
        ),
    })
}

/// Put a Backlog Task in the queue through the Task lifecycle's own enqueue,
/// the one the board's Start and "Add to Queue" reach, so the Task is
/// indistinguishable from one queued there.
///
/// This only queues. It takes no admission permit, creates no checkout or
/// branch and starts no agent: the executor's scheduler promotes a queued Task
/// when it chooses to, and the executor prepares its checkout then. A Task
/// already in Queue records [`Execution::AlreadyQueued`], not success: the
/// record cannot say whether this approval or another front door queued it, so
/// a retry after a crash and an unrelated enqueue look the same and neither is
/// claimed.
///
/// The lease is held so the enqueue cannot interleave with another transition
/// of the same Task.
async fn enqueue_task(
    scope: &Scope<'_>,
    task_id: Uuid,
    from: &TaskStatus,
    approval: lifecycle::InputApproval<'_>,
) -> Result<Execution, String> {
    // Re-validated here rather than trusted from the persisted proposal.
    if *from != TaskStatus::Backlog {
        return Ok(Execution::Refused("The Coordinator can only queue a Backlog Task".into()));
    }
    let _lease = scope.locks.acquire(task_id).await?;
    // Git is read only once the lease is held, so the observation is made for
    // the write that follows it. A Task no longer in Backlog is left to the
    // queue below, which answers a retry of this same approval.
    if let Some(shown) = approval.checkout {
        let task = scope.tasks.read().await.get(&task_id).cloned();
        match task {
            None => return Ok(Execution::Refused("The Task no longer exists".into())),
            Some(task) if task.status == TaskStatus::Backlog => {
                if let Err(reason) = recheck_existing_checkout(&task, shown).await {
                    return Ok(Execution::Refused(reason));
                }
            }
            Some(_) => {}
        }
    }
    Ok(
        match lifecycle::enqueue_leased(
            scope.tasks, scope.storage, scope.project_id, task_id, from, Some(approval),
        )
        .await?
        {
            EnqueueVerdict::Queued(title) => Execution::Applied { task_id, title },
            EnqueueVerdict::AlreadyQueued(title) => Execution::AlreadyQueued { task_id, title },
            EnqueueVerdict::Moved(status) => Execution::Refused(format!(
                "The Task is now in {status:?} instead of Backlog; nothing was changed. \
                 Check it on the board."
            )),
            EnqueueVerdict::InputChanged => Execution::Refused(
                "This request does not name the exact Task input you approved, or the Task was edited after \
                 it was proposed, so it could run something other than what you approved; nothing was \
                 changed. Ask the Coordinator to propose queueing it again."
                    .into(),
            ),
            EnqueueVerdict::Missing => Execution::Refused("The Task no longer exists".into()),
            EnqueueVerdict::OtherProject => Execution::Refused("The Task belongs to another Project".into()),
            EnqueueVerdict::Blocked(reason) => Execution::Refused(format!("{reason}; nothing was changed")),
        },
    )
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

/// Additional requests that reuse a Task's existing checkout, proved against
/// disposable Git repositories with real `git worktree` checkouts.
#[cfg(test)]
mod existing_checkout_tests {
    use super::*;
    use crate::domain::conversation::{Conversation, CoordinatorOutput};
    use crate::domain::task::{WorkerHandoff, WorkerHandoffStage};
    use crate::test_helpers::create_test_task_with_status;
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit_file(dir: &Path, name: &str, body: &str, message: &str) -> String {
        std::fs::write(dir.join(name), body).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", message]);
        git(dir, &["rev-parse", "HEAD"])
    }

    pub(super) struct Fixture {
        pub(super) dir: tempfile::TempDir,
        pub(super) repo: String,
        pub(super) checkout: std::path::PathBuf,
        pub(super) task: Task,
        pub(super) base: String,
    }

    /// A repository with `main`, and a Backlog Task whose own branch has one
    /// prior run's commit in its checkout. Its `base_commit` is `main`.
    pub(super) fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        let base = commit_file(&repo, "base.txt", "base\n", "base");
        let mut task = create_test_task_with_status("Existing checkout", TaskStatus::Backlog);
        task.project_id = Uuid::new_v4();
        let branch = WorktreeManager::branch_for_task(task.id);
        let checkout = dir.path().join("checkout");
        git(&repo, &["worktree", "add", "-q", "-b", &branch, checkout.to_str().unwrap(), "main"]);
        commit_file(&checkout, "prior.txt", "prior run\n", "prior run");
        task.worktree_path = Some(checkout.to_string_lossy().into_owned());
        task.branch_name = Some(branch);
        task.base_commit = Some(base.clone());
        Fixture { repo: repo.to_string_lossy().into_owned(), checkout, task, base, dir }
    }

    fn handoff(stage: WorkerHandoffStage) -> WorkerHandoff {
        WorkerHandoff {
            review_mode: Default::default(),
            task_id: Uuid::nil(),
            project_id: Uuid::nil(),
            repository_id: None,
            run: Some(1),
            branch: None,
            start_head: None,
            stage,
            acceptance: None,
            review_mode_unproven: false,
        }
    }

    #[tokio::test]
    async fn clean_checkout_is_bound_to_its_head_and_keeps_the_task_base() {
        let fx = fixture();
        let head = git(&fx.checkout, &["rev-parse", "HEAD"]);
        let evidence = inspect_existing_checkout(&fx.repo, &fx.task).await.unwrap();
        assert_eq!(evidence.head, head, "the prior run's commit is the checkout's HEAD");
        assert_eq!(evidence.base_commit, fx.base, "the Task's own base is kept, not reset to HEAD");
        assert_eq!(evidence.branch_ref, format!("refs/heads/{}", WorktreeManager::branch_for_task(fx.task.id)));
        assert_eq!(evidence.run, fx.task.next_run());
        assert_eq!(evidence.task_id, fx.task.id);
        assert_eq!(evidence.project_id, fx.task.project_id);
        assert!(fx.dir.path().join("checkout/prior.txt").exists(), "earlier commits stay in the checkout");
    }

    #[tokio::test]
    async fn uncommitted_work_of_every_kind_is_refused_and_left_in_place() {
        let dirtiers: [fn(&Path); 3] = [
            |c| std::fs::write(c.join("base.txt"), "changed\n").unwrap(),
            |c| {
                std::fs::write(c.join("staged.txt"), "staged\n").unwrap();
                git(c, &["add", "staged.txt"]);
            },
            |c| std::fs::write(c.join("untracked.txt"), "untracked\n").unwrap(),
        ];
        for dirty in dirtiers {
            let fx = fixture();
            dirty(&fx.checkout);
            let err = inspect_existing_checkout(&fx.repo, &fx.task).await.unwrap_err();
            assert!(err.contains("uncommitted changes"), "{err}");
            assert!(fx.checkout.join("prior.txt").exists(), "the checkout was not touched");
        }
    }

    #[tokio::test]
    async fn detached_missing_and_foreign_checkouts_are_refused() {
        let detached = fixture();
        git(&detached.checkout, &["checkout", "-q", "--detach"]);
        assert!(inspect_existing_checkout(&detached.repo, &detached.task).await.is_err());

        let missing = fixture();
        std::fs::remove_dir_all(&missing.checkout).unwrap();
        assert!(inspect_existing_checkout(&missing.repo, &missing.task).await.is_err());

        let foreign = fixture();
        let mut task = foreign.task.clone();
        task.branch_name = Some("main".into());
        let err = inspect_existing_checkout(&foreign.repo, &task).await.unwrap_err();
        assert!(err.contains("own branch"), "{err}");
    }

    /// The user's own checkout is never a Task Checkout, whatever the Task's
    /// record says: here its branch is checked out in the primary checkout, and
    /// the record names that checkout, directly or through a symlink.
    #[tokio::test]
    async fn the_primary_checkout_and_its_symlink_alias_are_refused_and_left_untouched() {
        let fx = fixture();
        let repo = Path::new(&fx.repo);
        let branch = fx.task.branch_name.clone().unwrap();
        git(repo, &["worktree", "remove", fx.checkout.to_str().unwrap()]);
        git(repo, &["checkout", "-q", &branch]);
        std::fs::write(repo.join("wip.txt"), "uncommitted\n").unwrap();
        let head = git(repo, &["rev-parse", "HEAD"]);

        for path in [repo.to_path_buf(), repo.join("..").join("repo")] {
            let mut task = fx.task.clone();
            task.worktree_path = Some(path.to_string_lossy().into_owned());
            let err = inspect_existing_checkout(&fx.repo, &task).await.unwrap_err();
            assert!(err.contains("repository's own checkout"), "{path:?}: {err}");
        }
        #[cfg(unix)]
        {
            let alias = fx.dir.path().join("alias");
            std::os::unix::fs::symlink(repo, &alias).unwrap();
            let mut task = fx.task.clone();
            task.worktree_path = Some(alias.to_string_lossy().into_owned());
            let err = inspect_existing_checkout(&fx.repo, &task).await.unwrap_err();
            assert!(err.contains("repository's own checkout"), "{alias:?}: {err}");
            // The same refusal when the Project itself is reached through the alias.
            let err = inspect_existing_checkout(alias.to_str().unwrap(), &task).await.unwrap_err();
            assert!(err.contains("repository's own checkout"), "{alias:?} via alias: {err}");
        }
        assert_eq!(git(repo, &["rev-parse", "HEAD"]), head, "the branch did not move");
        assert_eq!(git(repo, &["symbolic-ref", "--short", "HEAD"]), branch);
        assert_eq!(std::fs::read_to_string(repo.join("wip.txt")).unwrap(), "uncommitted\n");
    }

    /// A Project whose repository path is itself a linked worktree is
    /// supported: the Task Checkout is a different linked worktree, and the
    /// main checkout it was linked from is still not one.
    #[tokio::test]
    async fn a_linked_worktree_project_reuses_a_separate_task_checkout_only() {
        let fx = fixture();
        let main_repo = Path::new(&fx.repo);
        let project_checkout = fx.dir.path().join("project-worktree");
        git(main_repo, &["worktree", "add", "-q", "-b", "project-branch", project_checkout.to_str().unwrap(), "main"]);
        let project = project_checkout.to_string_lossy().into_owned();

        let evidence = inspect_existing_checkout(&project, &fx.task).await.unwrap();
        assert_eq!(evidence.checkout_path, fx.task.worktree_path.clone().unwrap());

        let mut as_project = fx.task.clone();
        as_project.worktree_path = Some(project.clone());
        assert!(inspect_existing_checkout(&project, &as_project).await.is_err(), "the Project's own path");
        let mut as_main = fx.task.clone();
        git(main_repo, &["worktree", "remove", fx.checkout.to_str().unwrap()]);
        git(main_repo, &["checkout", "-q", as_main.branch_name.as_deref().unwrap()]);
        as_main.worktree_path = Some(fx.repo.clone());
        let err = inspect_existing_checkout(&project, &as_main).await.unwrap_err();
        assert!(err.contains("repository's own checkout"), "{err}");
    }

    /// The Project's own linked worktree is never a Task Checkout, however the
    /// Project path reaches it: its root, a subdirectory of the root, or an
    /// alias of either. Git lists the main checkout first, so the Project's
    /// own root is found only by asking the Project for its top level.
    #[tokio::test]
    async fn the_project_linked_worktree_root_is_refused_from_every_project_path() {
        let fx = fixture();
        let main_repo = Path::new(&fx.repo);
        let branch = fx.task.branch_name.clone().unwrap();
        let project_root = fx.dir.path().join("project-worktree");
        git(main_repo, &["worktree", "add", "-q", "-b", "project-branch", project_root.to_str().unwrap(), "main"]);
        let subdir = project_root.join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        // The Task's own branch moves into the Project's linked worktree.
        git(main_repo, &["worktree", "remove", fx.checkout.to_str().unwrap()]);
        git(&project_root, &["checkout", "-q", &branch]);

        let mut task = fx.task.clone();
        task.worktree_path = Some(project_root.to_string_lossy().into_owned());
        let projects = [project_root.clone(), subdir.clone(), subdir.join("..")];
        let subdir_alias = fx.dir.path().join("subdir-alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&subdir, &subdir_alias).unwrap();
        for project in projects.iter().chain(cfg!(unix).then_some(&subdir_alias)) {
            let err = inspect_existing_checkout(project.to_str().unwrap(), &task).await.unwrap_err();
            assert!(err.contains("repository's own checkout"), "{}: {err}", project.display());
        }

        // Refusal leaves staged and untracked work, and the branch, exactly as they were.
        std::fs::write(project_root.join("staged.txt"), "staged\n").unwrap();
        git(&project_root, &["add", "staged.txt"]);
        std::fs::write(project_root.join("untracked.txt"), "untracked\n").unwrap();
        let head = git(&project_root, &["rev-parse", "HEAD"]);
        for project in projects.iter().chain(cfg!(unix).then_some(&subdir_alias)) {
            let err = inspect_existing_checkout(project.to_str().unwrap(), &task).await.unwrap_err();
            assert!(err.contains("repository's own checkout"), "{}: {err}", project.display());
        }

        assert_eq!(git(&project_root, &["rev-parse", "HEAD"]), head, "HEAD did not move");
        assert_eq!(git(&project_root, &["symbolic-ref", "--short", "HEAD"]), branch);
        assert_eq!(std::fs::read_to_string(project_root.join("staged.txt")).unwrap(), "staged\n");
        assert_eq!(std::fs::read_to_string(project_root.join("untracked.txt")).unwrap(), "untracked\n");
    }

    #[tokio::test]
    async fn a_missing_or_unrelated_base_commit_is_refused_and_never_inferred() {
        let fx = fixture();
        let mut no_base = fx.task.clone();
        no_base.base_commit = None;
        let err = inspect_existing_checkout(&fx.repo, &no_base).await.unwrap_err();
        assert!(err.contains("base commit"), "{err}");

        let orphan = {
            let root = Path::new(&fx.repo);
            git(root, &["checkout", "-q", "--orphan", "unrelated"]);
            commit_file(root, "other.txt", "other\n", "unrelated root")
        };
        let mut unrelated = fx.task.clone();
        unrelated.base_commit = Some(orphan);
        let err = inspect_existing_checkout(&fx.repo, &unrelated).await.unwrap_err();
        assert!(err.contains("does not descend"), "{err}");
    }

    #[test]
    fn a_started_or_unsettled_handoff_blocks_reuse_and_none_does_not() {
        let mut task = create_test_task_with_status("handoff", TaskStatus::Backlog);
        task.worktree_path = Some("/x".into());
        task.branch_name = Some("b".into());
        for stage in [
            WorkerHandoffStage::Started {},
            WorkerHandoffStage::CommittedAwaitingReview { commit: "c".into() },
            WorkerHandoffStage::RecoveryRequired { reason: "r".into(), observed_commit: None },
            WorkerHandoffStage::ResolvedToAiReview { commit: "c".into() },
        ] {
            task.worker_handoff = Some(handoff(stage.clone()));
            assert!(task.existing_checkout_refusal().is_some(), "{stage:?} without supersession");
        }
        task.worker_handoff = None;
        assert_eq!(task.existing_checkout_refusal(), None, "no handoff is pending");
    }

    #[tokio::test]
    async fn only_a_task_with_a_checkout_needs_inspected_evidence() {
        let fx = fixture();
        let plain = create_test_task_with_status("plain", TaskStatus::Backlog);
        let evidence = inspect_existing_checkout(&fx.repo, &fx.task).await.unwrap();
        assert!(TaskMutation::execute_request(&fx.task, "extra", None).is_err(), "a checkout must be inspected");
        assert!(TaskMutation::execute_request(&plain, "extra", Some(evidence.clone())).is_err());
        assert!(TaskMutation::execute_request(&plain, "extra", None).is_ok(), "the no-checkout path is unchanged");
        let accepted = TaskMutation::execute_request(&fx.task, "extra", Some(evidence.clone())).unwrap();
        let TaskMutation::ExecuteRequest { checkout, .. } = &accepted else { unreachable!() };
        assert_eq!(checkout.as_deref(), Some(&evidence));
        assert!(accepted.summary().contains("reusing its Task Checkout on branch"));
    }

    #[test]
    fn a_binding_saved_before_checkout_evidence_existed_reuses_nothing() {
        let json = serde_json::json!({
            "approval_id": Uuid::new_v4(),
            "digest": "d",
            "for_run": 1,
            "request": "extra"
        });
        let old: crate::domain::task::ApprovedExecution = serde_json::from_value(json).unwrap();
        assert_eq!(old.checkout, None, "absent evidence never authorizes reuse");
    }

    #[test]
    fn a_proposal_saved_without_checkout_evidence_authorizes_nothing() {
        let json = serde_json::json!({
            "kind": "execute_request",
            "target_task_id": Uuid::new_v4(),
            "target_task_title": "old",
            "from": "backlog",
            "request": "extra",
            "input": { "digest": "d", "description": null, "subtask_count": 0 }
        });
        let old: TaskMutation = serde_json::from_value(json).unwrap();
        let TaskMutation::ExecuteRequest { checkout, .. } = old else { unreachable!() };
        assert_eq!(checkout, None, "absent evidence never authorizes reuse");
    }

    #[tokio::test]
    async fn the_recheck_refuses_a_moved_head_and_accepts_the_unchanged_checkout() {
        let fx = fixture();
        let shown = inspect_existing_checkout(&fx.repo, &fx.task).await.unwrap();
        recheck_existing_checkout(&fx.task, &shown).await.expect("an unchanged checkout passes");
        commit_file(&fx.checkout, "later.txt", "later\n", "moved after proposal");
        let err = recheck_existing_checkout(&fx.task, &shown).await.unwrap_err();
        assert!(err.contains("changed after this request was proposed"), "{err}");
    }

    #[tokio::test]
    async fn a_proposal_records_its_evidence_and_a_checkout_task_without_it_is_refused() {
        let fx = fixture();
        let project_id = fx.task.project_id;
        let tasks = HashMap::from([(fx.task.id, fx.task.clone())]);
        let output = || CoordinatorOutput::ExecuteRequest {
            text: "explain".into(),
            target_task_id: fx.task.id,
            request: "add a test".into(),
        };
        let mut conversation = Conversation::new(project_id);
        assert!(record_proposal(&mut conversation, project_id, output(), &tasks, None).is_err());
        assert!(conversation.project_actions.is_empty());
        let evidence = inspect_existing_checkout(&fx.repo, &fx.task).await.unwrap();
        record_proposal(&mut conversation, project_id, output(), &tasks, Some(evidence.clone())).unwrap();
        let recorded = conversation.project_actions.last().unwrap();
        let TaskMutation::ExecuteRequest { checkout, .. } = &recorded.mutation else { unreachable!() };
        assert_eq!(checkout.as_deref(), Some(&evidence));
    }
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
        locks: TaskLifecycleLocks,
        gate: crate::lifecycle::ProjectLifecycleGate,
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
                locks: TaskLifecycleLocks::new(),
                gate: crate::lifecycle::ProjectLifecycleGate::new(),
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
                locks: &self.locks,
                gate: &self.gate,
            }
        }

        fn conversation(&self) -> Conversation {
            self.storage.create_primary_conversation(self.project_id).unwrap()
        }

        async fn seed_task(&self, project_id: Uuid, title: &str) -> Task {
            // Some tests seed a Task belonging to another Project on
            // purpose. A Task exists only for a Project that exists, so give
            // any unregistered id the Project it needs; the tests still
            // answer for the project's own Scope.
            if !self.projects.read().await.contains_key(&project_id) {
                self.projects.write().await.insert(
                    project_id,
                    Project {
                        id: project_id,
                        name: "foreign".into(),
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
                    },
                );
            }
            lifecycle::create(&self.gate, &self.tasks, &self.projects, &self.storage, project_id, |existing| {
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
            record_proposal(conversation, self.project_id, output, &tasks, None)?;
            Ok(conversation.project_actions.last().unwrap().id)
        }

        /// Record a move action directly, as a stored Conversation could carry one.
        fn propose_unchecked(&self, conversation: &mut Conversation, task: &Task, to: TaskStatus) -> Uuid {
            let action = ProjectAction::proposed(
                "Tidy".into(),
                TaskMutation::MoveTask {
                    target_task_id: task.id,
                    target_task_title: task.title.clone(),
                    from: TaskStatus::Queue,
                    to,
                },
            );
            let id = action.id;
            conversation.project_actions.push(action);
            id
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
    async fn an_approved_request_queues_in_the_exact_checkout_the_human_was_shown() {
        let fx = super::existing_checkout_tests::fixture();
        let world = World::new();
        let mut task = fx.task.clone();
        task.project_id = world.project_id;
        world.tasks.write().await.insert(task.id, task.clone());
        let board: Vec<Task> = world.tasks.read().await.values().cloned().collect();
        world.storage.save_project_tasks(world.project_id, &board).unwrap();

        let evidence = super::inspect_existing_checkout(&fx.repo, &task).await.unwrap();
        let output = CoordinatorOutput::ExecuteRequest {
            text: "Run it again".into(),
            target_task_id: task.id,
            request: "add a test".into(),
        };
        let mut conversation = world.conversation();
        {
            let tasks = world.tasks.read().await;
            record_proposal(&mut conversation, world.project_id, output, &tasks, Some(evidence.clone())).unwrap();
        }
        let id = conversation.project_actions.last().unwrap().id;
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let queued = world.tasks.read().await[&task.id].clone();
        assert_eq!(queued.status, TaskStatus::Queue);
        assert_eq!(queued.worktree_path.as_deref(), Some(evidence.checkout_path.as_str()));
        assert_eq!(queued.base_commit.as_deref(), Some(fx.base.as_str()), "the Task base is not reset to HEAD");
        assert_eq!(queued.approved_execution.and_then(|b| b.checkout), Some(evidence));
        assert!(fx.checkout.join("prior.txt").exists(), "earlier commits stay in the checkout");
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
    async fn a_repeated_request_after_a_rejection_is_a_new_proposal_needing_its_own_approval() {
        let world = World::new();
        let mut conversation = world.conversation();
        let first = world.propose(&mut conversation, CREATE).await.unwrap();
        decide(&world.scope(), &mut conversation, first, Decision::Reject).await.unwrap();

        // The Human repeats the request and the Coordinator proposes it again.
        conversation.push(Role::Human, EntryKind::HumanMessage { text: "Add the Task again".into() });
        let second = world.propose(&mut conversation, CREATE).await.unwrap();

        assert_ne!(first, second, "the repeat is a new proposal, not the old one revived");
        assert_eq!(conversation.project_action(first).unwrap().status, ProjectActionStatus::Rejected);
        assert_eq!(conversation.project_action(second).unwrap().status, ProjectActionStatus::Proposed);
        assert!(world.tasks.read().await.is_empty(), "repeating a request must not create anything");

        // The rejection stays in the durable history and in what the model sees.
        let rejections = conversation.entries.iter().filter(|e| matches!(
            e.kind, EntryKind::ProjectActionDecision { action_id, approved: false } if action_id == first
        )).count();
        assert_eq!(rejections, 1);
        let projection = conversation.coordinator_projection("Add the Task again", "Project", None, &[]);
        let history = projection["recent_history"].as_array().unwrap();
        let decision = history.iter().find(|e| e["kind"] == "project_action_decision").unwrap();
        assert_eq!(decision["decision"], "rejected");
        assert_eq!(decision["scope"], "this_proposal_only");

        // Only a fresh approval of the new proposal applies a mutation.
        assert!(decide(&world.scope(), &mut conversation, first, Decision::Approve).await.is_err());
        assert!(world.tasks.read().await.is_empty());
        decide(&world.scope(), &mut conversation, second, Decision::Approve).await.unwrap();
        assert_eq!(world.tasks.read().await.len(), 1);
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
        execute(&world.scope(), conversation.project_actions[0].id, &conversation.project_actions[0].mutation.clone()).await.unwrap();
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
        execute(&world.scope(), conversation.project_actions[0].id, &conversation.project_actions[0].mutation.clone()).await.unwrap();
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

    fn move_json(task_id: Uuid, to: &str) -> String {
        format!(r#"{{"type":"move_task","text":"Tidy the board","target_task_id":"{task_id}","to":"{to}"}}"#)
    }

    /// Put a seeded Task in `status` through the Task store, as the board would.
    async fn place(world: &World, task_id: Uuid, status: TaskStatus) {
        lifecycle::commit_task(&world.tasks, &world.storage, task_id, &move |staged: &mut HashMap<Uuid, Task>| {
            staged.get_mut(&task_id).unwrap().status = status.clone();
        })
        .await
        .unwrap();
    }

    fn moves(task: &Task) -> usize {
        task.activity
            .iter()
            .filter(|entry| matches!(entry.kind, crate::domain::ActivityKind::Moved { .. }))
            .count()
    }

    #[test]
    fn the_coordinator_may_only_return_queued_or_failed_tasks_to_backlog() {
        let all = [
            TaskStatus::Backlog, TaskStatus::Queue, TaskStatus::InProgress, TaskStatus::AiReview,
            TaskStatus::HumanReview, TaskStatus::Done, TaskStatus::PrCreated, TaskStatus::Error,
        ];
        let allowed: Vec<_> = all
            .iter()
            .flat_map(|from| all.iter().map(move |to| (from, to)))
            .filter(|(from, to)| coordinator_may_move(from, to))
            .collect();
        assert_eq!(
            allowed,
            vec![
                (&TaskStatus::Queue, &TaskStatus::Backlog),
                (&TaskStatus::Error, &TaskStatus::Backlog)
            ]
        );
    }

    #[test]
    fn move_proposals_are_strict() {
        let id = Uuid::new_v4();
        assert!(matches!(
            Conversation::parse_output(&move_json(id, "backlog")),
            Ok(CoordinatorOutput::MoveTask { to: TaskStatus::Backlog, .. })
        ));
        for bad in [
            format!(r#"{{"type":"move_task","text":"x","target_task_id":"{id}"}}"#),
            format!(r#"{{"type":"move_task","text":"x","target_task_id":"{id}","to":"later"}}"#),
            format!(r#"{{"type":"move_task","text":"x","target_task_id":"{id}","to":"backlog","position":0}}"#),
            format!(r#"{{"type":"move_task","text":" ","target_task_id":"{id}","to":"backlog"}}"#),
        ] {
            assert!(Conversation::parse_output(&bad).is_err(), "accepted: {bad}");
        }
    }

    #[tokio::test]
    async fn approving_a_move_returns_a_queued_task_to_backlog_through_the_board_transition() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Queued work").await;
        place(&world, task.id, TaskStatus::Queue).await;
        let before = world.tasks.read().await[&task.id].clone();
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &move_json(task.id, "backlog")).await.unwrap();
        assert_eq!(world.tasks.read().await[&task.id].status, TaskStatus::Queue, "a proposal changes nothing");
        assert!(conversation.project_action(id).unwrap().mutation.summary().contains("no work is started"));

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let after = world.tasks.read().await[&task.id].clone();
        assert_eq!(after.status, TaskStatus::Backlog);
        assert_eq!(moves(&after), moves(&before) + 1, "the move is recorded like any board move");
        assert!(after.worktree_path.is_none() && after.branch_name.is_none() && after.worktree_id.is_none());
        assert_eq!(after.phase, crate::domain::TaskPhase::Idle);
        assert_eq!(world.tasks.read().await.len(), 1, "no Task is created");
        let action = conversation.project_action(id).unwrap();
        assert_eq!(action.status, ProjectActionStatus::Applied);
        assert_eq!(
            action.outcome,
            Some(ProjectActionOutcome::Applied { task_id: task.id, title: after.title.clone() })
        );
        let persisted = world.storage.load_project_tasks(world.project_id).unwrap();
        assert_eq!(persisted.iter().find(|t| t.id == task.id).unwrap().status, TaskStatus::Backlog);
        let durable = world.storage.load_primary_conversation(world.project_id).unwrap().unwrap();
        assert!(durable.entries.iter().any(|e| matches!(
            e.kind,
            EntryKind::ProjectActionApplied { action_id, task_id } if action_id == id && task_id == task.id
        )));
    }

    #[tokio::test]
    async fn moving_a_failed_task_clears_the_failure_but_keeps_its_checkout() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Failed work").await;
        lifecycle::commit_task(&world.tasks, &world.storage, task.id, &|staged: &mut HashMap<Uuid, Task>| {
            let task = staged.values_mut().next().unwrap();
            task.status = TaskStatus::Error;
            task.phase = crate::domain::TaskPhase::Failed;
            task.error_message = Some("agent crashed".into());
            task.worktree_path = Some("/tmp/checkout".into());
            task.branch_name = Some("slashit/failed".into());
        })
        .await
        .unwrap();
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &move_json(task.id, "backlog")).await.unwrap();

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let after = world.tasks.read().await[&task.id].clone();
        assert_eq!(after.status, TaskStatus::Backlog);
        assert_eq!((after.phase, after.error_message.clone()), (crate::domain::TaskPhase::Idle, None));
        assert_eq!(after.worktree_path.as_deref(), Some("/tmp/checkout"), "moving never discards a checkout");
        assert_eq!(after.branch_name.as_deref(), Some("slashit/failed"));
        assert_eq!((after.phase_progress, after.overall_progress), (0, 0));
    }

    #[tokio::test]
    async fn a_moved_task_takes_the_end_of_the_backlog_without_colliding() {
        let world = World::new();
        let moving = world.seed_task(world.project_id, "Moving").await;
        place(&world, moving.id, TaskStatus::Queue).await;
        let first = world.seed_task(world.project_id, "First").await;
        let second = world.seed_task(world.project_id, "Second").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &move_json(moving.id, "backlog")).await.unwrap();

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let tasks = world.tasks.read().await;
        let mut positions: Vec<i32> = [moving.id, first.id, second.id].iter().map(|id| tasks[id].position).collect();
        assert_eq!(tasks[&moving.id].position, tasks[&second.id].position + 1, "lands after the last card");
        positions.sort_unstable();
        positions.dedup();
        assert_eq!(positions.len(), 3, "no two cards share a position");
    }

    #[tokio::test]
    async fn rejecting_a_move_changes_nothing_and_cannot_be_approved_later() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Queued work").await;
        place(&world, task.id, TaskStatus::Queue).await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &move_json(task.id, "backlog")).await.unwrap();

        decide(&world.scope(), &mut conversation, id, Decision::Reject).await.unwrap();
        assert_eq!(world.tasks.read().await[&task.id].status, TaskStatus::Queue);
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Rejected);
        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        assert_eq!(world.tasks.read().await[&task.id].status, TaskStatus::Queue);
    }

    #[tokio::test]
    async fn moves_that_start_or_finish_work_are_never_proposed() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Work").await;
        let foreign = world.seed_task(Uuid::new_v4(), "Elsewhere").await;
        place(&world, foreign.id, TaskStatus::Queue).await;
        let mut conversation = world.conversation();

        // Backlog cannot be moved forward: queueing is an execution decision.
        for to in ["queue", "in_progress", "ai_review", "human_review", "done", "pr_created", "error", "backlog"] {
            let result = world.propose(&mut conversation, &move_json(task.id, to)).await;
            assert!(result.is_err(), "a Backlog Task must not be proposed for {to}");
        }
        // Work that is running or awaiting review is not the Coordinator's to pull back.
        for from in [TaskStatus::InProgress, TaskStatus::AiReview, TaskStatus::HumanReview, TaskStatus::Done, TaskStatus::PrCreated] {
            place(&world, task.id, from.clone()).await;
            let result = world.propose(&mut conversation, &move_json(task.id, "backlog")).await;
            assert!(result.is_err(), "a {from:?} Task must not be proposed for Backlog");
        }
        assert!(world.propose(&mut conversation, &move_json(Uuid::new_v4(), "backlog")).await.is_err());
        assert!(world.propose(&mut conversation, &move_json(foreign.id, "backlog")).await.is_err());
        assert!(conversation.project_actions.is_empty(), "refused proposals leave no action behind");
        assert_eq!(world.tasks.read().await[&foreign.id].status, TaskStatus::Queue);
    }

    #[tokio::test]
    async fn a_move_approved_after_the_task_changed_is_refused_and_changes_nothing() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Queued work").await;
        place(&world, task.id, TaskStatus::Queue).await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &move_json(task.id, "backlog")).await.unwrap();
        // The executor claims the Task between the proposal and the approval.
        place(&world, task.id, TaskStatus::InProgress).await;

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let action = conversation.project_action(id).unwrap();
        assert_eq!(action.status, ProjectActionStatus::Refused);
        assert!(matches!(action.outcome, Some(ProjectActionOutcome::Refused { .. })));
        let after = world.tasks.read().await[&task.id].clone();
        assert_eq!(after.status, TaskStatus::InProgress);
        assert_eq!(moves(&after), 0);
        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        assert_eq!(world.tasks.read().await[&task.id].status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn a_move_for_a_deleted_or_foreign_task_is_refused() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Queued work").await;
        place(&world, task.id, TaskStatus::Queue).await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &move_json(task.id, "backlog")).await.unwrap();
        world.tasks.write().await.remove(&task.id);
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Refused);

        // The same Task id now belongs to another Project: approval must not touch it.
        let mut hijacked = task.clone();
        hijacked.project_id = Uuid::new_v4();
        hijacked.status = TaskStatus::Queue;
        world.tasks.write().await.insert(task.id, hijacked);
        let id = world.propose_unchecked(&mut conversation, &task, TaskStatus::Backlog);
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Refused);
        assert_eq!(world.tasks.read().await[&task.id].status, TaskStatus::Queue);
    }

    #[tokio::test]
    async fn a_tampered_stored_move_is_refused_at_approval() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Queued work").await;
        place(&world, task.id, TaskStatus::Queue).await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &move_json(task.id, "backlog")).await.unwrap();
        // A hand-edited Conversation file asks for a move proposals can never express.
        conversation.project_actions[0].mutation = TaskMutation::MoveTask {
            target_task_id: task.id,
            target_task_title: "Queued work".into(),
            from: TaskStatus::Queue,
            to: TaskStatus::InProgress,
        };

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Refused);
        assert_eq!(world.tasks.read().await[&task.id].status, TaskStatus::Queue);
    }

    #[tokio::test]
    async fn repeated_move_approval_is_idempotent_and_recovers_after_a_lost_outcome() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Queued work").await;
        place(&world, task.id, TaskStatus::Queue).await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &move_json(task.id, "backlog")).await.unwrap();
        // Steps 1 and 2 by hand, then a crash before the outcome is recorded.
        conversation.project_actions[0].status = ProjectActionStatus::Approved;
        conversation.push(Role::Human, EntryKind::ProjectActionDecision { action_id: id, approved: true });
        world.storage.save_conversation(&conversation).unwrap();
        execute(&world.scope(), conversation.project_actions[0].id, &conversation.project_actions[0].mutation.clone()).await.unwrap();
        let moved = moves(&world.tasks.read().await[&task.id]);

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Applied);
        let after = world.tasks.read().await[&task.id].clone();
        assert_eq!(after.status, TaskStatus::Backlog);
        assert_eq!(moves(&after), moved, "a retry observes the move, it does not repeat it");
    }

    #[tokio::test]
    async fn a_failed_task_write_leaves_a_move_retryable_and_the_task_unchanged() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Queued work").await;
        place(&world, task.id, TaskStatus::Queue).await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &move_json(task.id, "backlog")).await.unwrap();
        world.block_task_persistence();

        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        assert_eq!(world.tasks.read().await[&task.id].status, TaskStatus::Queue, "a failed save must not show the move");
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Approved);

        world.unblock_task_persistence();
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert_eq!(world.tasks.read().await[&task.id].status, TaskStatus::Backlog);
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Applied);
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
        assert!(record_proposal(&mut conversation, Uuid::new_v4(), reply, &HashMap::new(), None).is_err());
        assert!(conversation.project_actions.is_empty());
    }
    fn enqueue_json(task_id: Uuid) -> String {
        format!(r#"{{"type":"enqueue_task","text":"Ready to build","target_task_id":"{task_id}"}}"#)
    }

    async fn snapshot_of(world: &World, task_id: Uuid) -> Task {
        world.tasks.read().await[&task_id].clone()
    }

    #[test]
    fn enqueue_proposals_are_strict() {
        let id = Uuid::new_v4();
        assert!(matches!(
            Conversation::parse_output(&enqueue_json(id)),
            Ok(CoordinatorOutput::EnqueueTask { target_task_id, .. }) if target_task_id == id
        ));
        for bad in [
            r#"{"type":"enqueue_task","text":"x"}"#.to_string(),
            format!(r#"{{"type":"enqueue_task","text":"x","target_task_id":"{id}","status":"in_progress"}}"#),
            format!(r#"{{"type":"enqueue_task","text":"x","target_task_id":"{id}","position":0}}"#),
            format!(r#"{{"type":"enqueue_task","text":"  ","target_task_id":"{id}"}}"#),
        ] {
            assert!(Conversation::parse_output(&bad).is_err(), "accepted: {bad}");
        }
    }

    #[tokio::test]
    async fn only_a_runnable_backlog_task_of_this_project_can_be_proposed_for_the_queue() {
        let world = World::new();
        let mut conversation = world.conversation();
        let foreign = world.seed_task(Uuid::new_v4(), "Elsewhere").await;
        assert!(world.propose(&mut conversation, &enqueue_json(foreign.id)).await.is_err());
        assert!(world.propose(&mut conversation, &enqueue_json(Uuid::new_v4())).await.is_err());
        for status in [
            TaskStatus::Queue, TaskStatus::InProgress, TaskStatus::AiReview, TaskStatus::HumanReview,
            TaskStatus::Done, TaskStatus::PrCreated, TaskStatus::Error,
        ] {
            let task = world.seed_task(world.project_id, "Not in backlog").await;
            place(&world, task.id, status.clone()).await;
            assert!(
                world.propose(&mut conversation, &enqueue_json(task.id)).await.is_err(),
                "a Task in {status:?} was proposed for the queue"
            );
        }
        let stuck = world.seed_task(world.project_id, "Cleanup unfinished").await;
        lifecycle::commit_task(&world.tasks, &world.storage, stuck.id, &move |staged: &mut HashMap<Uuid, Task>| {
            staged.get_mut(&stuck.id).unwrap().cleanup_in_flight = true;
        })
        .await
        .unwrap();
        assert!(world.propose(&mut conversation, &enqueue_json(stuck.id)).await.is_err());
        assert!(conversation.project_actions.is_empty(), "a refused proposal must record nothing");
    }

    #[tokio::test]
    async fn a_proposal_queues_nothing_and_a_rejection_can_never_queue_later() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Build it").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
        assert_eq!(snapshot_of(&world, task.id).await.status, TaskStatus::Backlog, "a proposal is not a decision");

        decide(&world.scope(), &mut conversation, id, Decision::Reject).await.unwrap();
        assert_eq!(snapshot_of(&world, task.id).await.status, TaskStatus::Backlog);
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Rejected);
        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        assert_eq!(snapshot_of(&world, task.id).await.status, TaskStatus::Backlog);
    }

    #[tokio::test]
    async fn approving_queues_the_task_at_the_top_and_starts_nothing() {
        let world = World::new();
        let waiting = world.seed_task(world.project_id, "Already waiting").await;
        place(&world, waiting.id, TaskStatus::Queue).await;
        let task = world.seed_task(world.project_id, "Build it").await;
        let before = snapshot_of(&world, task.id).await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let after = snapshot_of(&world, task.id).await;
        assert_eq!(after.status, TaskStatus::Queue);
        assert_eq!(after.position, 0, "the board's enqueue puts the Task at the top of Queue");
        assert_eq!(snapshot_of(&world, waiting.id).await.position, 1);
        assert_eq!(moves(&after), moves(&before) + 1);
        assert!(after.worktree_path.is_none() && after.branch_name.is_none() && after.worktree_id.is_none());
        assert_eq!(after.phase, crate::domain::TaskPhase::Idle);
        assert!(!after.is_ready_to_execute(), "queued is not started: only the scheduler promotes");
        let action = conversation.project_action(id).unwrap();
        assert_eq!(action.status, ProjectActionStatus::Applied);
        assert_eq!(
            action.outcome,
            Some(ProjectActionOutcome::Applied { task_id: task.id, title: "Build it".into() })
        );
        let persisted = world.storage.load_project_tasks(world.project_id).unwrap();
        assert_eq!(persisted.iter().find(|t| t.id == task.id).unwrap().status, TaskStatus::Queue);
    }

    #[tokio::test]
    async fn a_task_moved_after_the_proposal_is_refused_and_changed_by_nothing() {
        let world = World::new();
        for status in [TaskStatus::InProgress, TaskStatus::Done, TaskStatus::Error, TaskStatus::HumanReview] {
            let task = world.seed_task(world.project_id, "Moved on").await;
            let mut conversation = world.conversation();
            let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
            place(&world, task.id, status.clone()).await;
            let before = snapshot_of(&world, task.id).await;

            decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

            let after = snapshot_of(&world, task.id).await;
            assert_eq!((after.status.clone(), after.updated_at), (status, before.updated_at));
            assert!(matches!(
                conversation.project_action(id).unwrap().outcome,
                Some(ProjectActionOutcome::Refused { .. })
            ));
        }
    }

    #[tokio::test]
    async fn a_deleted_foreign_or_blocked_task_is_refused_at_approval() {
        let world = World::new();
        let mut conversation = world.conversation();

        let gone = world.seed_task(world.project_id, "Gone").await;
        let gone_id = world.propose(&mut conversation, &enqueue_json(gone.id)).await.unwrap();
        world.tasks.write().await.remove(&gone.id);
        decide(&world.scope(), &mut conversation, gone_id, Decision::Approve).await.unwrap();
        assert!(matches!(
            conversation.project_action(gone_id).unwrap().outcome,
            Some(ProjectActionOutcome::Refused { .. })
        ));

        let moved = world.seed_task(world.project_id, "Reassigned").await;
        let moved_id = world.propose(&mut conversation, &enqueue_json(moved.id)).await.unwrap();
        let other_project = Uuid::new_v4();
        lifecycle::commit_task(&world.tasks, &world.storage, moved.id, &move |staged: &mut HashMap<Uuid, Task>| {
            staged.get_mut(&moved.id).unwrap().project_id = other_project;
        })
        .await
        .unwrap();
        decide(&world.scope(), &mut conversation, moved_id, Decision::Approve).await.unwrap();
        assert_eq!(snapshot_of(&world, moved.id).await.status, TaskStatus::Backlog);
        assert!(matches!(
            conversation.project_action(moved_id).unwrap().outcome,
            Some(ProjectActionOutcome::Refused { .. })
        ));

        let blocked = world.seed_task(world.project_id, "Restack pending").await;
        let blocked_id = world.propose(&mut conversation, &enqueue_json(blocked.id)).await.unwrap();
        lifecycle::commit_task(&world.tasks, &world.storage, blocked.id, &move |staged: &mut HashMap<Uuid, Task>| {
            staged.get_mut(&blocked.id).unwrap().cleanup_in_flight = true;
        })
        .await
        .unwrap();
        decide(&world.scope(), &mut conversation, blocked_id, Decision::Approve).await.unwrap();
        assert_eq!(snapshot_of(&world, blocked.id).await.status, TaskStatus::Backlog);
        assert!(matches!(
            conversation.project_action(blocked_id).unwrap().outcome,
            Some(ProjectActionOutcome::Refused { .. })
        ));
    }

    #[tokio::test]
    async fn a_tampered_stored_enqueue_from_another_column_is_refused() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Tampered").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
        conversation.project_actions[0].mutation = TaskMutation::EnqueueTask {
            target_task_id: task.id,
            target_task_title: task.title.clone(),
            from: TaskStatus::Error,
            input: None,
        };
        place(&world, task.id, TaskStatus::Error).await;
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert_eq!(snapshot_of(&world, task.id).await.status, TaskStatus::Error);
        assert!(matches!(
            conversation.project_action(id).unwrap().outcome,
            Some(ProjectActionOutcome::Refused { .. })
        ));
    }

    #[tokio::test]
    async fn repeated_approval_never_queues_twice() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Once").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        let queued = snapshot_of(&world, task.id).await;

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let after = snapshot_of(&world, task.id).await;
        assert_eq!(moves(&after), moves(&queued));
        assert_eq!(after.updated_at, queued.updated_at, "a repeated approval must not rewrite the Task");
    }

    /// The crash this cannot see through: the Task was queued, the outcome was
    /// lost. The record cannot say a Task in Queue was queued by this approval,
    /// so the outcome says only what was found, and nothing is rewritten.
    #[tokio::test]
    async fn a_task_found_already_queued_is_recorded_as_found_not_as_queued_by_this_approval() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Queued elsewhere").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
        // Another front door queues it between the proposal and the approval.
        place(&world, task.id, TaskStatus::Queue).await;
        let before = snapshot_of(&world, task.id).await;

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let after = snapshot_of(&world, task.id).await;
        assert_eq!((after.status.clone(), after.updated_at, after.position), (TaskStatus::Queue, before.updated_at, before.position));
        assert_eq!(moves(&after), moves(&before), "nothing is written for a Task already in Queue");
        let action = conversation.project_action(id).unwrap();
        assert_eq!(action.status, ProjectActionStatus::Applied, "the Human's intent holds: the Task is queued");
        assert_eq!(
            action.outcome,
            Some(ProjectActionOutcome::AlreadyQueued { task_id: task.id, title: "Queued elsewhere".into() })
        );
        // The Coordinator is told what was found, not that this approval did it.
        let projection = conversation.coordinator_projection("next", "p", None, &[]);
        assert!(projection.to_string().contains("found_already_queued"));
    }

    #[tokio::test]
    async fn a_lost_outcome_after_queueing_recovers_by_repeating_the_approval() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Crash window").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        let queued = snapshot_of(&world, task.id).await;
        // Replay the first two steps by hand: durable Approved, Task already queued.
        conversation.project_actions[0].status = ProjectActionStatus::Approved;
        conversation.project_actions[0].outcome = None;

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let action = conversation.project_action(id).unwrap();
        assert_eq!(action.status, ProjectActionStatus::Applied);
        assert!(matches!(action.outcome, Some(ProjectActionOutcome::AlreadyQueued { .. })));
        assert_eq!(snapshot_of(&world, task.id).await.updated_at, queued.updated_at);
    }

    #[tokio::test]
    async fn a_failed_task_write_leaves_an_enqueue_retryable_and_the_task_unchanged() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Retry me").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
        world.block_task_persistence();

        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        assert_eq!(snapshot_of(&world, task.id).await.status, TaskStatus::Backlog, "a failed save must not show the move");
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Approved);

        world.unblock_task_persistence();
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert_eq!(snapshot_of(&world, task.id).await.status, TaskStatus::Queue);
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Applied);
        assert!(matches!(
            conversation.project_action(id).unwrap().outcome,
            Some(ProjectActionOutcome::Applied { .. })
        ));
    }

    #[tokio::test]
    async fn concurrent_approvals_queue_a_task_once() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Racing").await;
        let first = world.scope();
        let second = world.scope();
        let mut a = world.conversation();
        let id = world.propose(&mut a, &enqueue_json(task.id)).await.unwrap();
        let mut b = a.clone();
        let (ra, rb) = tokio::join!(
            decide(&first, &mut a, id, Decision::Approve),
            decide(&second, &mut b, id, Decision::Approve)
        );
        assert!(ra.is_ok() && rb.is_ok());
        let after = snapshot_of(&world, task.id).await;
        assert_eq!(after.status, TaskStatus::Queue);
        assert_eq!(
            after.activity.iter().filter(|e| matches!(e.kind, crate::domain::ActivityKind::Moved { .. })).count(),
            1,
            "two racing approvals must record one move"
        );
    }

    async fn edit_in_store(world: &World, task_id: Uuid, edit: impl Fn(&mut Task) + Send + Sync + 'static) {
        lifecycle::commit_task(&world.tasks, &world.storage, task_id, &move |staged: &mut HashMap<Uuid, Task>| {
            edit(staged.get_mut(&task_id).unwrap());
        })
        .await
        .unwrap();
    }

    fn recorded_input(conversation: &Conversation, id: Uuid) -> Option<crate::domain::conversation::ExecutionInput> {
        match &conversation.project_action(id).unwrap().mutation {
            TaskMutation::EnqueueTask { input, .. } => input.clone(),
            other => panic!("not an enqueue: {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_enqueue_proposal_records_the_exact_input_and_an_unchanged_task_is_queued() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Bind me").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();

        let input = recorded_input(&conversation, id).expect("a new proposal always records its input");
        assert_eq!(input.digest, crate::queue::prompt::task_input_digest(&task));
        assert_eq!(input.description.as_deref(), Some("original"));
        assert!(conversation.project_action(id).unwrap().mutation.summary().contains("original"));

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert_eq!(snapshot_of(&world, task.id).await.status, TaskStatus::Queue);
        assert!(matches!(
            conversation.project_action(id).unwrap().outcome,
            Some(ProjectActionOutcome::Applied { .. })
        ));
    }

    #[tokio::test]
    async fn a_task_edited_after_the_proposal_is_refused_and_nothing_is_queued() {
        type Edit = (&'static str, fn(&mut Task));
        let edits: [Edit; 4] = [
            ("description", |t| t.description = Some("something else entirely".into())),
            ("title", |t| t.title = "Renamed".into()),
            ("subtasks", |t| t.subtasks.push(crate::domain::task::Subtask { id: Uuid::new_v4(), title: "also delete the tests".into(), completed: false })),
            ("description removed", |t| t.description = None),
        ];
        for (what, edit) in edits {
            let world = World::new();
            let task = world.seed_task(world.project_id, "Edited").await;
            let mut conversation = world.conversation();
            let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
            edit_in_store(&world, task.id, edit).await;

            decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

            let after = snapshot_of(&world, task.id).await;
            assert_eq!(after.status, TaskStatus::Backlog, "{what}: an edited Task must not be queued");
            assert_eq!(moves(&after), 0, "{what}: no move may be recorded");
            assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Refused, "{what}");
            let Some(ProjectActionOutcome::Refused { reason }) = &conversation.project_action(id).unwrap().outcome else {
                panic!("{what}: expected a refusal");
            };
            assert!(reason.contains("edited after it was proposed"), "{what}: {reason}");
        }
    }

    #[tokio::test]
    async fn an_approval_retried_after_an_edit_is_refused_rather_than_run_as_edited() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Retry then edit").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
        world.block_task_persistence();
        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Approved);
        world.unblock_task_persistence();

        edit_in_store(&world, task.id, |t| t.description = Some("a different job".into())).await;
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        assert_eq!(snapshot_of(&world, task.id).await.status, TaskStatus::Backlog);
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Refused);
    }

    #[tokio::test]
    async fn an_edit_of_something_the_prompt_ignores_does_not_block_a_queue() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Position only").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
        edit_in_store(&world, task.id, |t| t.position += 7).await;

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert_eq!(snapshot_of(&world, task.id).await.status, TaskStatus::Queue);
    }

    #[tokio::test]
    async fn a_proposal_saved_before_the_input_was_recorded_still_reads_but_cannot_queue_a_task() {
        let id = Uuid::new_v4();
        let legacy: TaskMutation = serde_json::from_value(serde_json::json!({
            "kind": "enqueue_task", "target_task_id": id, "target_task_title": "Old", "from": "backlog"
        }))
        .expect("an enqueue saved before the input field existed must stay readable");
        assert!(matches!(&legacy, TaskMutation::EnqueueTask { input: None, .. }));
        let rewritten = serde_json::to_value(&legacy).unwrap();
        assert!(rewritten.get("input").is_none(), "an absent input is not written back as null");

        let world = World::new();
        let task = world.seed_task(world.project_id, "Legacy").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
        conversation.project_actions[0].mutation = TaskMutation::EnqueueTask {
            target_task_id: task.id,
            target_task_title: task.title.clone(),
            from: TaskStatus::Backlog,
            input: None,
        };
        edit_in_store(&world, task.id, |t| t.description = Some("edited".into())).await;
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        let after = snapshot_of(&world, task.id).await;
        assert_eq!(after.status, TaskStatus::Backlog, "an approval that never named the input cannot queue it");
        assert_eq!(after.approved_execution, None);
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Refused);
    }

    #[tokio::test]
    async fn an_approved_queue_binds_the_task_to_its_approval_and_next_run() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Bound").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        let queued = snapshot_of(&world, task.id).await;
        let binding = queued.approved_execution.clone().expect("bound");
        assert_eq!(binding.approval_id, id);
        assert_eq!(binding.digest, crate::queue::prompt::task_input_digest(&queued));
        assert_eq!(binding.for_run, queued.next_run());
    }

    #[tokio::test]
    async fn a_task_found_queued_or_moved_is_not_bound_and_a_move_ends_a_binding() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Found").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
        place(&world, task.id, TaskStatus::Queue).await;
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert_eq!(snapshot_of(&world, task.id).await.approved_execution, None);

        let mut t = snapshot_of(&world, task.id).await;
        t.approved_execution = Some(crate::domain::task::ApprovedExecution { approval_id: id, digest: "d".into(), for_run: 1, request: None, checkout: None });
        let from = t.status.clone();
        t.status = TaskStatus::Backlog;
        t.record_move(&from);
        assert_eq!(t.approved_execution, None, "retry, drag or manual enqueue supersede the binding");
    }

    #[test]
    fn a_task_saved_before_the_binding_existed_still_reads_and_is_not_rewritten_with_one() {
        let task = crate::test_helpers::create_test_task("Legacy");
        let mut value = serde_json::to_value(&task).unwrap();
        assert!(value.get("approved_execution").is_none());
        value.as_object_mut().unwrap().remove("approved_execution");
        let back: Task = serde_json::from_value(value).unwrap();
        assert_eq!(back.approved_execution, None);
    }

    #[tokio::test]
    async fn a_task_already_queued_is_still_reported_as_found_not_as_queued_by_this_approval() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Queued elsewhere").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &enqueue_json(task.id)).await.unwrap();
        place(&world, task.id, TaskStatus::Queue).await;
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        assert!(matches!(
            conversation.project_action(id).unwrap().outcome,
            Some(ProjectActionOutcome::AlreadyQueued { .. })
        ));
    }

    fn execute_request_json(task_id: Uuid, request: &str) -> String {
        serde_json::json!({
            "type": "execute_request",
            "text": "Ready for one more pass",
            "target_task_id": task_id,
            "request": request,
        })
        .to_string()
    }

    /// The binding a queued request carries, read from the stored Task.
    fn binding_of(task: &Task) -> Option<crate::domain::task::ApprovedExecution> {
        task.approved_execution.clone()
    }

    #[tokio::test]
    async fn an_approved_request_queues_the_task_once_with_the_request_bound_and_its_description_unchanged() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Extend").await;
        let request = "Also add a changelog entry";
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &execute_request_json(task.id, request)).await.unwrap();

        let ProjectAction { mutation: TaskMutation::ExecuteRequest { request: proposed, input, .. }, .. } =
            conversation.project_action(id).unwrap().clone()
        else {
            panic!("expected an ExecuteRequest proposal");
        };
        assert_eq!(proposed, request);
        assert_eq!(input.digest, crate::queue::prompt::approved_input_digest(&task, Some(request)));

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let queued = snapshot_of(&world, task.id).await;
        assert_eq!(queued.status, TaskStatus::Queue);
        assert_eq!(queued.description.as_deref(), Some("original"), "the description is never overwritten");
        let binding = binding_of(&queued).expect("bound");
        assert_eq!(binding.approval_id, id);
        assert_eq!(binding.request.as_deref(), Some(request));
        assert_eq!(binding.digest, input.digest);
        assert_eq!(moves(&queued), 1, "a repeated approval queues nothing a second time");
        assert!(matches!(
            conversation.project_action(id).unwrap().outcome,
            Some(ProjectActionOutcome::Applied { .. })
        ));
    }

    #[tokio::test]
    async fn an_additional_request_is_refused_for_any_state_but_backlog_and_for_a_task_that_holds_a_checkout() {
        let world = World::new();
        let mut conversation = world.conversation();
        for status in [TaskStatus::Queue, TaskStatus::Error, TaskStatus::HumanReview, TaskStatus::Done] {
            let task = world.seed_task(world.project_id, "Not backlog").await;
            place(&world, task.id, status.clone()).await;
            let err = world.propose(&mut conversation, &execute_request_json(task.id, "more")).await.unwrap_err();
            assert!(err.contains("Backlog"), "{status:?}: {err}");
        }

        let task = world.seed_task(world.project_id, "Has a checkout").await;
        edit_in_store(&world, task.id, |t| t.worktree_path = Some("/checkouts/has-one".into())).await;
        let err = world.propose(&mut conversation, &execute_request_json(task.id, "more")).await.unwrap_err();
        assert!(err.contains("Task Checkout"), "{err}");
        assert!(conversation.project_actions.is_empty(), "no proposal is recorded for a refused request");
    }

    #[tokio::test]
    async fn an_edited_task_or_a_request_altered_after_proposal_is_refused_and_nothing_is_queued() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Edited after").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &execute_request_json(task.id, "more")).await.unwrap();
        edit_in_store(&world, task.id, |t| t.description = Some("something else".into())).await;

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let after = snapshot_of(&world, task.id).await;
        assert_eq!(after.status, TaskStatus::Backlog);
        assert_eq!(after.approved_execution, None);
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Refused);
    }

    #[tokio::test]
    async fn a_retry_after_the_run_consumed_the_approval_does_not_queue_the_request_again() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Run once").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &execute_request_json(task.id, "more")).await.unwrap();
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        // A run started under this approval, then the Task was moved back to
        // Backlog. The approval record was persisted as still Approved.
        edit_in_store(&world, task.id, move |t| {
            t.status = TaskStatus::Backlog;
            t.approved_execution = None;
            t.executed_approvals.push(crate::domain::task::ExecutedApproval {
                approval_id: id,
                run: 1,
                digest: crate::queue::prompt::approved_input_digest(t, Some("more")),
                checkout: None,
            });
        })
        .await;
        conversation.project_actions[0].status = ProjectActionStatus::Approved;
        conversation.project_actions[0].outcome = None;

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let after = snapshot_of(&world, task.id).await;
        assert_eq!(after.status, TaskStatus::Backlog, "a consumed approval never queues again");
        assert_eq!(after.approved_execution, None);
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Refused);
    }

    #[tokio::test]
    async fn a_failed_task_write_leaves_an_execute_request_retryable_and_the_task_unbound() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Retry the request").await;
        let mut conversation = world.conversation();
        let request = "Also add a changelog entry";
        let id = world.propose(&mut conversation, &execute_request_json(task.id, request)).await.unwrap();
        world.block_task_persistence();

        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        let failed = snapshot_of(&world, task.id).await;
        assert_eq!(failed.status, TaskStatus::Backlog, "a failed save must not show the queue move");
        assert_eq!(failed.approved_execution, None, "a failed save must not show the binding");
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Approved);

        world.unblock_task_persistence();
        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let queued = snapshot_of(&world, task.id).await;
        assert_eq!(queued.status, TaskStatus::Queue);
        assert_eq!(binding_of(&queued).map(|b| (b.approval_id, b.request)), Some((id, Some(request.into()))));
        assert_eq!(moves(&queued), 1);
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Applied);
    }

    /// The Task write reached the file but its directory sync failed. The
    /// approval was already durable, so the retry must find the queued Task
    /// and its binding, and must not queue or bind it a second time.
    #[tokio::test]
    async fn a_published_but_not_durable_execute_request_is_not_queued_again_by_a_retry() {
        use slashit_durable_fs::{fault, Stage};
        let world = World::new();
        let task = world.seed_task(world.project_id, "Durability unknown").await;
        let mut conversation = world.conversation();
        let request = "Also add a changelog entry";
        let id = world.propose(&mut conversation, &execute_request_json(task.id, request)).await.unwrap();
        // Step 1 of the protocol, as a crash after it would leave it.
        conversation.project_actions[0].status = ProjectActionStatus::Approved;

        let fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
        assert!(decide(&world.scope(), &mut conversation, id, Decision::Approve).await.is_err());
        drop(fault);

        let published = snapshot_of(&world, task.id).await;
        assert_eq!(published.status, TaskStatus::Queue, "the write is in place");
        assert_eq!(binding_of(&published).map(|b| b.approval_id), Some(id));
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Approved);

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let after = snapshot_of(&world, task.id).await;
        assert_eq!(moves(&after), moves(&published), "the retry must not move the Task again");
        assert_eq!(after.updated_at, published.updated_at, "the retry must not rewrite the Task");
        assert_eq!(binding_of(&after).map(|b| (b.approval_id, b.request)), Some((id, Some(request.into()))));
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Applied);
    }

    /// Another front door queues the Task between the proposal and the
    /// approval. The request cannot be attached to that run, so the approval
    /// must be refused, not recorded as applied.
    #[tokio::test]
    async fn a_task_queued_elsewhere_refuses_the_request_instead_of_applying_it() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Queued first").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &execute_request_json(task.id, "more")).await.unwrap();
        place(&world, task.id, TaskStatus::Queue).await;
        let before = snapshot_of(&world, task.id).await;

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let after = snapshot_of(&world, task.id).await;
        assert_eq!(after.approved_execution, None, "no request is attached to the queued Task");
        assert_eq!(moves(&after), moves(&before));
        let action = conversation.project_action(id).unwrap();
        assert_eq!(action.status, ProjectActionStatus::Refused);
        assert!(matches!(action.outcome, Some(ProjectActionOutcome::Refused { .. })));
    }

    #[tokio::test]
    async fn a_request_altered_after_proposal_is_refused_and_nothing_is_queued() {
        let world = World::new();
        let task = world.seed_task(world.project_id, "Request swapped").await;
        let mut conversation = world.conversation();
        let id = world.propose(&mut conversation, &execute_request_json(task.id, "the approved text")).await.unwrap();
        let TaskMutation::ExecuteRequest { request, .. } = &mut conversation.project_actions[0].mutation else {
            panic!("expected an ExecuteRequest proposal");
        };
        *request = "a different text".into();

        decide(&world.scope(), &mut conversation, id, Decision::Approve).await.unwrap();

        let after = snapshot_of(&world, task.id).await;
        assert_eq!(after.status, TaskStatus::Backlog);
        assert_eq!(after.approved_execution, None);
        assert_eq!(conversation.project_action(id).unwrap().status, ProjectActionStatus::Refused);
    }

    #[test]
    fn an_additional_request_is_validated_and_its_digest_follows_the_request_and_the_model() {
        let task = crate::test_helpers::create_test_task("Digest");
        let id = task.id;
        assert!(Conversation::parse_output(&execute_request_json(id, "   ")).is_err());
        assert!(Conversation::parse_output(&execute_request_json(id, &"x".repeat(4_001))).is_err());
        assert!(Conversation::parse_output(&execute_request_json(id, "one more pass")).is_ok());

        let base = crate::queue::prompt::approved_input_digest(&task, Some("a"));
        assert_ne!(base, crate::queue::prompt::approved_input_digest(&task, Some("b")));
        assert_ne!(base, crate::queue::prompt::approved_input_digest(&task, None));
        let mut other_model = task.clone();
        other_model.model = "opus".into();
        assert_ne!(base, crate::queue::prompt::approved_input_digest(&other_model, Some("a")));
        assert_eq!(
            crate::queue::prompt::approved_input_digest(&task, None),
            crate::queue::prompt::task_input_digest(&task),
            "an ordinary approval keeps its existing digest"
        );
    }

    #[test]
    fn the_approval_records_the_model_policy_and_a_change_of_policy_fails_closed() {
        use crate::domain::task::ApprovedExecution;
        use crate::domain::conversation::model_policy_phrase;

        let mut explicit = crate::test_helpers::create_test_task("Model policy");
        explicit.model = "claude-opus-4-0-20250514".into();
        let mutation = TaskMutation::execute_request(&explicit, "extra", None).unwrap();
        let TaskMutation::ExecuteRequest { input, .. } = &mutation else {
            panic!("expected an ExecuteRequest proposal");
        };
        assert_eq!(input.model.as_deref(), Some("claude-opus-4-0-20250514"));
        assert_eq!(model_policy_phrase(input.model.as_deref()), "model claude-opus-4-0-20250514");

        let mut provider_default = explicit.clone();
        provider_default.model = "default".into();
        let TaskMutation::ExecuteRequest { input, .. } = TaskMutation::execute_request(&provider_default, "extra", None).unwrap() else {
            panic!("expected an ExecuteRequest proposal");
        };
        assert_eq!(model_policy_phrase(input.model.as_deref()), "provider default, selected at execution time");
        assert_eq!(model_policy_phrase(None), "model not recorded for this proposal");

        // The Human approved the explicit model. Switching the Task to provider
        // default after approval must not start the run with the approved input.
        let mut approved = explicit.clone();
        approved.approved_execution = Some(ApprovedExecution {
            approval_id: uuid::Uuid::new_v4(),
            digest: crate::queue::prompt::approved_input_digest(&explicit, Some("extra")),
            for_run: explicit.next_run(),
            request: Some("extra".into()),
            checkout: None,
        });
        assert_eq!(crate::queue::prompt::check_approved_input(&approved), crate::queue::prompt::ApprovedInputCheck::Matches);
        let mut switched = approved.clone();
        switched.model = "default".into();
        assert_eq!(crate::queue::prompt::check_approved_input(&switched), crate::queue::prompt::ApprovedInputCheck::Differs);

        // The reverse direction: approved as provider default, then pinned.
        let mut approved_default = provider_default.clone();
        approved_default.approved_execution = Some(ApprovedExecution {
            approval_id: uuid::Uuid::new_v4(),
            digest: crate::queue::prompt::approved_input_digest(&provider_default, Some("extra")),
            for_run: provider_default.next_run(),
            request: Some("extra".into()),
            checkout: None,
        });
        let mut pinned = approved_default.clone();
        pinned.model = "claude-opus-4-0-20250514".into();
        assert_eq!(crate::queue::prompt::check_approved_input(&pinned), crate::queue::prompt::ApprovedInputCheck::Differs);

        // Empty and "default" are the same policy, but the raw-model digest
        // treats them as different. That refuses a start and never authorizes one.
        let mut empty = provider_default.clone();
        empty.model = String::new();
        assert_ne!(
            crate::queue::prompt::approved_input_digest(&provider_default, Some("extra")),
            crate::queue::prompt::approved_input_digest(&empty, Some("extra")),
        );
    }
}

