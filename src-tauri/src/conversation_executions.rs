//! Which Task run each Project Conversation action started, and what that run
//! has become. This is the read-only projection behind the execution cards.
//!
//! The projection is pure. It reads the Conversation's ProjectActions, the
//! Tasks they name and an executor liveness snapshot, and returns one view per
//! recent action that can start work. It never reads Git, launches a provider,
//! changes a Task or persists anything, so a Conversation snapshot can carry it
//! without a second record of the same facts.
//!
//! Correlation. A ProjectAction's id is the approval identity. A run belongs to
//! an approval only through evidence that names that id:
//!
//! - before the run starts, `Task::approved_execution` (the binding that the
//!   executor's start write consumes);
//! - after it starts, `Task::executed_approvals`, which outlives the binding.
//!
//! Timestamps, the Task's status and its latest run never establish ownership.
//! An `AlreadyQueued` outcome is not evidence either: it says only that the Task
//! was already in Queue, so an unrelated queue operation is never shown as this
//! approval's run.
//!
//! Outcomes come from the Task's Activity milestones for that run's number. A
//! missing milestone is not a failure. A run with no terminal milestone and no
//! live owner is `Unconfirmed`, because reconciliation may still settle it.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use uuid::Uuid;

use crate::domain::conversation::{ProjectAction, ProjectActionStatus, TaskMutation};
use crate::domain::task::{
    ActivityColumn, ActivityKind, AttentionReason, ExecutedApproval, Task, TaskStatus,
};

/// The most recent execution-capable actions one Conversation shows.
pub const MAX_EXECUTIONS: usize = 10;

/// The most characters of a run's result shown. The full length is kept as
/// metadata.
pub const MAX_RESULT_CHARS: usize = 1_000;

/// The most characters of an action's explanation shown.
pub const MAX_EXPLANATION_CHARS: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    /// Approved, but no record shows the approval taking effect yet.
    PendingApproval,
    /// This approval queued the Task, and the scheduler has not started its run.
    Queued,
    /// The executor owns this approval's run right now.
    Running,
    /// The run started, but nothing settles it: no terminal milestone and no
    /// live owner. It is not reported as completed or failed.
    Unconfirmed,
    /// The coding run completed. The Task may still be in review or delivery.
    Completed,
    /// The run recorded its own failure.
    Failed,
    /// The run's outcome could not be attributed safely, and a person must
    /// inspect it before anything else runs.
    RecoveryRequired,
    /// The records disagree about which run this approval started, so nothing
    /// is linked.
    Ambiguous,
    /// The approval was applied, but no record links it to a run. This includes
    /// an unrelated `AlreadyQueued` outcome and older history.
    NoExecutionEvidence,
    /// The Task is missing or belongs to another Project. Both look the same.
    TaskUnavailable,
}

impl ExecutionState {
    /// Whether the state can still change without a Human decision, so the
    /// Conversation should keep refreshing it.
    pub fn is_transitional(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Unconfirmed)
    }
}

/// Text from a run or a proposal. It is untrusted content and is rendered as
/// plain text only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoundedText {
    pub text: String,
    /// Whether `text` is shorter than the full text.
    pub truncated: bool,
    /// The full length in characters, when known.
    pub original_chars: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ResultView {
    Available {
        text: BoundedText,
    },
    /// The provider's answer was already cut when it was recorded.
    Truncated {
        text: BoundedText,
    },
    /// The provider gave no answer, for the reason it recorded. This is not a
    /// failure of the run's work.
    Unavailable {
        reason: BoundedText,
    },
    /// A run that ended has no recorded result. Older runs and truncated
    /// history look like this, and it is not an execution failure.
    NotRecorded,
    /// No run has ended, so there is no result to show.
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionView {
    pub action_id: Uuid,
    pub task_id: Uuid,
    pub task_title: String,
    pub state: ExecutionState,
    /// Whether the state can still change, so refreshing should continue.
    pub transitional: bool,
    /// The run number, once a run under this approval has started.
    pub run: Option<u32>,
    pub result: ResultView,
    /// Whether a person must act: the Task's board attention for its latest
    /// run, or a run that needs recovery.
    pub needs_human: bool,
    /// The board's reason. Shown only for the Task's latest run, because the
    /// Task's current state says nothing about an older run.
    pub attention: Option<AttentionReason>,
    pub explanation: BoundedText,
}

/// What the projection reads. The caller takes these from the stores and
/// releases its locks before calling [`project`]; the projection awaits nothing.
pub struct Inputs<'a> {
    pub project_id: Uuid,
    pub actions: &'a [ProjectAction],
    pub tasks: &'a HashMap<Uuid, Task>,
    /// The Tasks whose run the executor owns right now. `None` when the
    /// executor cannot be asked, so no run can be confirmed live.
    pub live: Option<&'a HashSet<Uuid>>,
    /// Whether a pull request is being opened for a Task, as the board asks.
    pub delivery_in_flight: &'a dyn Fn(Uuid) -> bool,
}

/// The execution views for the Conversation's most recent execution-capable
/// actions, oldest first. Infallible: a missing or foreign Task becomes
/// `TaskUnavailable`, and nothing else is inferred from absent evidence.
pub fn project(inputs: &Inputs<'_>) -> Vec<ExecutionView> {
    let relevant: Vec<(&ProjectAction, Uuid, &str)> = inputs
        .actions
        .iter()
        .filter_map(|action| {
            execution_target(action).map(|(task_id, title)| (action, task_id, title))
        })
        .collect();
    let start = relevant.len().saturating_sub(MAX_EXECUTIONS);
    relevant[start..]
        .iter()
        .map(|&(action, task_id, title)| view(inputs, action, task_id, title))
        .collect()
}

/// The Task an action would start work on, and its title as proposed. Only
/// queueing and additional requests can start a run, and only once applied or
/// approved.
fn execution_target(action: &ProjectAction) -> Option<(Uuid, &str)> {
    if !matches!(
        action.status,
        ProjectActionStatus::Approved | ProjectActionStatus::Applied
    ) {
        return None;
    }
    match &action.mutation {
        TaskMutation::EnqueueTask {
            target_task_id,
            target_task_title,
            ..
        }
        | TaskMutation::ExecuteRequest {
            target_task_id,
            target_task_title,
            ..
        } => Some((*target_task_id, target_task_title.as_str())),
        TaskMutation::CreateTask { .. }
        | TaskMutation::EditTask { .. }
        | TaskMutation::MoveTask { .. } => None,
    }
}

fn view(
    inputs: &Inputs<'_>,
    action: &ProjectAction,
    task_id: Uuid,
    proposed_title: &str,
) -> ExecutionView {
    let explanation = bounded(&action.explanation, MAX_EXPLANATION_CHARS, None);
    let task = inputs
        .tasks
        .get(&task_id)
        .filter(|task| task.project_id == inputs.project_id);
    let Some(task) = task else {
        return ExecutionView {
            action_id: action.id,
            task_id,
            task_title: proposed_title.to_owned(),
            state: ExecutionState::TaskUnavailable,
            transitional: false,
            run: None,
            result: ResultView::NotApplicable,
            needs_human: false,
            attention: None,
            explanation,
        };
    };

    let latest = latest_run(task);
    let executed: Vec<&ExecutedApproval> = task
        .executed_approvals
        .iter()
        .filter(|record| record.approval_id == action.id)
        .collect();
    let bound = task
        .approved_execution
        .as_ref()
        .is_some_and(|binding| binding.approval_id == action.id);
    let (state, run) = match executed.as_slice() {
        [] if bound && task.status == TaskStatus::Queue => (ExecutionState::Queued, None),
        // A start consumes the binding in the same write that records the run.
        // A binding that survives into Error means the queued start failed before
        // any run began, so it is a failure with no run number.
        [] if bound && task.status == TaskStatus::Error => (ExecutionState::Failed, None),
        [] if bound => (ExecutionState::Ambiguous, None),
        [] if action.status == ProjectActionStatus::Approved => {
            (ExecutionState::PendingApproval, None)
        }
        [] => (ExecutionState::NoExecutionEvidence, None),
        [_] if bound => (ExecutionState::Ambiguous, None),
        [record] => {
            let live = inputs.live.is_some_and(|live| live.contains(&task.id));
            (
                run_state(task, record.run, live && record.run == latest),
                Some(record.run),
            )
        }
        _ => (ExecutionState::Ambiguous, None),
    };

    let terminal = matches!(
        state,
        ExecutionState::Completed | ExecutionState::Failed | ExecutionState::RecoveryRequired
    );
    let result = match run {
        Some(run) if terminal => result_for(task, run).unwrap_or(ResultView::NotRecorded),
        _ => ResultView::NotApplicable,
    };
    let delivering = (inputs.delivery_in_flight)(task.id);
    let attention = match (state, run) {
        // A binding left by a failed start: the Task's Error is this approval's.
        (ExecutionState::Failed, None) => task.needs_you(delivering),
        (_, Some(run)) if run == latest && attention_belongs_to(task, run) => {
            task.needs_you(delivering)
        }
        _ => None,
    };
    ExecutionView {
        action_id: action.id,
        task_id,
        task_title: task.title.clone(),
        state,
        transitional: state.is_transitional(),
        run,
        result,
        needs_human: attention.is_some() || state == ExecutionState::RecoveryRequired,
        attention,
        explanation,
    }
}

/// The state of run `run` from its own milestones. Precedence: recovery, then
/// failure, then completion. Without a terminal milestone, the run is
/// `Running` only while the executor owns it as the Task's latest run;
/// otherwise it is `Unconfirmed`.
fn run_state(task: &Task, run: u32, live_latest: bool) -> ExecutionState {
    let milestone = |matches: fn(&ActivityKind, u32) -> bool| {
        task.activity.iter().any(|entry| matches(&entry.kind, run))
    };
    if milestone(
        |kind, run| matches!(kind, ActivityKind::RecoveryRequired { run: Some(r), .. } if *r == run),
    ) {
        ExecutionState::RecoveryRequired
    } else if milestone(
        |kind, run| matches!(kind, ActivityKind::RunFailed { run: Some(r), .. } if *r == run),
    ) {
        ExecutionState::Failed
    } else if milestone(
        |kind, run| matches!(kind, ActivityKind::RunCompleted { run: r } if *r == run),
    ) {
        ExecutionState::Completed
    } else if live_latest {
        ExecutionState::Running
    } else {
        ExecutionState::Unconfirmed
    }
}

/// The result recorded for exactly run `run`. Another run's result never
/// applies.
fn result_for(task: &Task, run: u32) -> Option<ResultView> {
    task.activity.iter().find_map(|entry| match &entry.kind {
        ActivityKind::RunResult {
            run: recorded,
            result,
        } if *recorded == run => Some(match result {
            slashit_activity::RunResult::Available { text } => ResultView::Available {
                text: bounded(text, MAX_RESULT_CHARS, None),
            },
            slashit_activity::RunResult::Truncated {
                text,
                original_chars,
            } => ResultView::Truncated {
                text: bounded(text, MAX_RESULT_CHARS, Some(*original_chars)),
            },
            slashit_activity::RunResult::Unavailable { reason } => ResultView::Unavailable {
                reason: bounded(reason, MAX_RESULT_CHARS, None),
            },
        }),
        _ => None,
    })
}

/// Whether the Task's current attention belongs to run `run`. Attention describes
/// the Task as it is now, so it belongs to the run only while nothing after the
/// run's last own milestone took the Task back to Backlog or Queue, failed it
/// before a run began, stopped it or interrupted it. Those later records mean
/// the current attention is about a different approval, or about none.
fn attention_belongs_to(task: &Task, run: u32) -> bool {
    let own = |kind: &ActivityKind| {
        matches!(
            kind,
            ActivityKind::RunStarted { run: r, .. }
                | ActivityKind::RunCompleted { run: r }
                | ActivityKind::RunResult { run: r, .. }
                | ActivityKind::RunFailed { run: Some(r), .. }
                | ActivityKind::RecoveryRequired { run: Some(r), .. }
                if *r == run
        )
    };
    let Some(last_own) = task.activity.iter().rposition(|entry| own(&entry.kind)) else {
        return false;
    };
    task.activity[last_own..].iter().all(|entry| {
        !matches!(
            entry.kind,
            ActivityKind::Moved {
                to: ActivityColumn::Backlog | ActivityColumn::Queue,
                ..
            } | ActivityKind::RunFailed { run: None, .. }
                | ActivityKind::RecoveryRequired { run: None, .. }
                | ActivityKind::Stopped { .. }
                | ActivityKind::Interrupted { .. }
        )
    })
}

/// The newest run number the Task's records mention, from its milestones and
/// its approvals. Zero when there is none.
fn latest_run(task: &Task) -> u32 {
    let started = task.activity.iter().filter_map(|entry| match entry.kind {
        ActivityKind::RunStarted { run, .. } => Some(run),
        _ => None,
    });
    let executed = task.executed_approvals.iter().map(|record| record.run);
    started.chain(executed).max().unwrap_or(0)
}

/// Cut `text` to `limit` characters. `stored_original` is the full length when
/// the storage already truncated the text before it was recorded.
fn bounded(text: &str, limit: usize, stored_original: Option<u32>) -> BoundedText {
    let full = text.chars().count();
    BoundedText {
        text: text.chars().take(limit).collect(),
        truncated: full > limit || stored_original.is_some(),
        original_chars: stored_original.unwrap_or(u32::try_from(full).unwrap_or(u32::MAX)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::conversation::ProjectActionOutcome;
    use crate::domain::task::ApprovedExecution;
    use crate::test_helpers::create_test_task_with_status;
    use slashit_activity::RunResult;

    fn project_id() -> Uuid {
        Uuid::from_u128(7)
    }

    /// A Backlog-or-Queue Task owned by `project`, stored by id.
    fn task_in(project: Uuid, title: &str, status: TaskStatus) -> Task {
        let mut task = create_test_task_with_status(title, status);
        task.project_id = project;
        task
    }

    fn store(tasks: Vec<Task>) -> HashMap<Uuid, Task> {
        tasks.into_iter().map(|task| (task.id, task)).collect()
    }

    fn enqueue(
        task: &Task,
        status: ProjectActionStatus,
        outcome: Option<ProjectActionOutcome>,
    ) -> ProjectAction {
        let mut action = ProjectAction::proposed(
            "Queue this Task".into(),
            TaskMutation::EnqueueTask {
                target_task_id: task.id,
                target_task_title: task.title.clone(),
                from: TaskStatus::Backlog,
                input: None,
            },
        );
        action.status = status;
        action.outcome = outcome;
        action
    }

    fn applied(task: &Task) -> Option<ProjectActionOutcome> {
        Some(ProjectActionOutcome::Applied {
            task_id: task.id,
            title: task.title.clone(),
        })
    }

    fn bind(task: &mut Task, approval: Uuid, for_run: u32) {
        task.status = TaskStatus::Queue;
        task.approved_execution = Some(ApprovedExecution {
            approval_id: approval,
            digest: "digest".into(),
            for_run,
            request: None,
            checkout: None,
        });
    }

    /// Start run `run` under `approval`, as the executor's start write does.
    fn start(task: &mut Task, approval: Uuid, run: u32) {
        task.approved_execution = None;
        task.status = TaskStatus::InProgress;
        task.executed_approvals.push(ExecutedApproval {
            approval_id: approval,
            run,
            digest: "digest".into(),
            checkout: None,
        });
        task.record_activity(ActivityKind::RunStarted {
            run,
            addressing_feedback: false,
        });
    }

    fn finish(task: &mut Task, run: u32) {
        task.record_activity(ActivityKind::RunCompleted { run });
    }

    fn answer(task: &mut Task, run: u32, text: &str) {
        task.record_activity(ActivityKind::RunResult {
            run,
            result: RunResult::from_provider(Ok(text.to_owned())),
        });
    }

    fn run_views(
        actions: &[ProjectAction],
        tasks: &HashMap<Uuid, Task>,
        live: Option<&HashSet<Uuid>>,
    ) -> Vec<ExecutionView> {
        let none = |_: Uuid| false;
        project(&Inputs {
            project_id: project_id(),
            actions,
            tasks,
            live,
            delivery_in_flight: &none,
        })
    }

    fn only(views: &[ExecutionView], action: &ProjectAction) -> ExecutionView {
        views
            .iter()
            .find(|view| view.action_id == action.id)
            .cloned()
            .expect("the action is projected")
    }

    #[test]
    fn approval_a_cannot_see_approval_bs_run() {
        let mut task = task_in(project_id(), "Shared", TaskStatus::Backlog);
        let a = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        let b = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        start(&mut task, b.id, 1);
        finish(&mut task, 1);
        let tasks = store(vec![task]);
        let views = run_views(&[a.clone(), b.clone()], &tasks, None);

        assert_eq!(only(&views, &b).state, ExecutionState::Completed);
        assert_eq!(only(&views, &b).run, Some(1));
        let other = only(&views, &a);
        assert_eq!(other.state, ExecutionState::NoExecutionEvidence);
        assert_eq!(
            other.run, None,
            "a run started by another approval is not this one's"
        );
    }

    #[test]
    fn queued_task_binds_to_the_approval_that_queued_it() {
        let mut task = task_in(project_id(), "Queued", TaskStatus::Backlog);
        let a = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        let b = enqueue(
            &task,
            ProjectActionStatus::Applied,
            Some(ProjectActionOutcome::AlreadyQueued {
                task_id: task.id,
                title: task.title.clone(),
            }),
        );
        bind(&mut task, a.id, 1);
        let tasks = store(vec![task]);
        let views = run_views(&[a.clone(), b.clone()], &tasks, None);

        let queued = only(&views, &a);
        assert_eq!(queued.state, ExecutionState::Queued);
        assert_eq!(
            queued.run, None,
            "the run is not established until it starts"
        );
        assert!(queued.transitional);
        assert_eq!(only(&views, &b).state, ExecutionState::NoExecutionEvidence);
    }

    #[test]
    fn retried_approval_keeps_the_original_execution() {
        let mut task = task_in(project_id(), "Retried", TaskStatus::Backlog);
        let a = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        start(&mut task, a.id, 1);
        finish(&mut task, 1);
        answer(&mut task, 1, "first answer");
        // The retry finds the Task already in Queue. It must not gain a second run.
        let retry = ProjectAction {
            status: ProjectActionStatus::Applied,
            outcome: Some(ProjectActionOutcome::AlreadyQueued {
                task_id: task.id,
                title: task.title.clone(),
            }),
            ..a.clone()
        };
        let tasks = store(vec![task]);
        let view = only(&run_views(&[retry], &tasks, None), &a);

        assert_eq!(view.state, ExecutionState::Completed);
        assert_eq!(view.run, Some(1));
        assert!(
            matches!(view.result, ResultView::Available { ref text } if text.text == "first answer")
        );
    }

    #[test]
    fn unrelated_already_queued_acquires_no_ownership() {
        // Someone else's binding queued the Task. This approval only found it queued.
        let mut task = task_in(project_id(), "Elsewhere", TaskStatus::Backlog);
        let other = Uuid::from_u128(99);
        bind(&mut task, other, 1);
        let found = enqueue(
            &task,
            ProjectActionStatus::Applied,
            Some(ProjectActionOutcome::AlreadyQueued {
                task_id: task.id,
                title: task.title.clone(),
            }),
        );
        let tasks = store(vec![task]);
        let view = only(
            &run_views(std::slice::from_ref(&found), &tasks, None),
            &found,
        );

        assert_eq!(view.state, ExecutionState::NoExecutionEvidence);
        assert_eq!(view.run, None);
        assert!(!view.needs_human);
    }

    #[test]
    fn run_n_never_receives_the_result_of_run_m() {
        let mut task = task_in(project_id(), "Two runs", TaskStatus::Backlog);
        let first = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        let second = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        start(&mut task, first.id, 1);
        finish(&mut task, 1);
        answer(&mut task, 1, "answer of run one");
        start(&mut task, second.id, 2);
        finish(&mut task, 2);
        let tasks = store(vec![task]);
        let views = run_views(&[first.clone(), second.clone()], &tasks, None);

        assert!(
            matches!(only(&views, &first).result, ResultView::Available { ref text } if text.text == "answer of run one")
        );
        assert_eq!(only(&views, &second).result, ResultView::NotRecorded);
    }

    #[test]
    fn completed_run_without_a_result_is_not_recorded_and_not_failed() {
        let mut task = task_in(project_id(), "No result", TaskStatus::HumanReview);
        let a = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        start(&mut task, a.id, 1);
        finish(&mut task, 1);
        let tasks = store(vec![task]);
        let view = only(&run_views(std::slice::from_ref(&a), &tasks, None), &a);

        assert_eq!(view.state, ExecutionState::Completed);
        assert_eq!(view.result, ResultView::NotRecorded);
        assert!(
            !view.needs_human,
            "a completed coding run is not a delivered Task"
        );
    }

    #[test]
    fn available_truncated_and_unavailable_results_keep_their_meaning() {
        let mut task = task_in(project_id(), "Results", TaskStatus::Backlog);
        let available = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        let truncated = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        let unavailable = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        start(&mut task, available.id, 1);
        finish(&mut task, 1);
        answer(&mut task, 1, "short");
        start(&mut task, truncated.id, 2);
        finish(&mut task, 2);
        task.record_activity(ActivityKind::RunResult {
            run: 2,
            result: RunResult::Truncated {
                text: "cut".into(),
                original_chars: 9_000,
            },
        });
        start(&mut task, unavailable.id, 3);
        finish(&mut task, 3);
        task.record_activity(ActivityKind::RunResult {
            run: 3,
            result: RunResult::from_provider(Err("the provider sent no final answer".into())),
        });
        let actions = [available.clone(), truncated.clone(), unavailable.clone()];
        let tasks = store(vec![task]);
        let views = run_views(&actions, &tasks, None);

        assert!(matches!(
            only(&views, &available).result,
            ResultView::Available { .. }
        ));
        match only(&views, &truncated).result {
            ResultView::Truncated { text } => {
                assert_eq!(text.original_chars, 9_000);
                assert!(text.truncated);
            }
            other => panic!("expected truncated, got {other:?}"),
        }
        match only(&views, &unavailable).result {
            ResultView::Unavailable { reason } => assert!(reason.text.contains("no final answer")),
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[test]
    fn failed_and_recovery_required_are_distinct() {
        let mut task = task_in(project_id(), "Failed", TaskStatus::Error);
        let failed = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        let recovered = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        start(&mut task, failed.id, 1);
        task.record_activity(ActivityKind::RunFailed {
            run: Some(1),
            reason: "provider exited".into(),
        });
        start(&mut task, recovered.id, 2);
        task.record_activity(ActivityKind::RecoveryRequired {
            run: Some(2),
            reason: "outcome unknown".into(),
        });
        let tasks = store(vec![task]);
        let views = run_views(&[failed.clone(), recovered.clone()], &tasks, None);

        let failed_view = only(&views, &failed);
        let recovered_view = only(&views, &recovered);
        assert_eq!(failed_view.state, ExecutionState::Failed);
        assert_eq!(recovered_view.state, ExecutionState::RecoveryRequired);
        assert!(recovered_view.needs_human);
        assert!(!failed_view.transitional && !recovered_view.transitional);
    }

    #[test]
    fn a_failed_latest_run_carries_the_board_attention_of_its_task() {
        let mut task = task_in(project_id(), "Attention", TaskStatus::Backlog);
        let a = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        start(&mut task, a.id, 1);
        // A failed run leaves its Task in Error, which is what the board reads.
        task.record_activity(ActivityKind::RunFailed {
            run: Some(1),
            reason: "boom".into(),
        });
        task.status = TaskStatus::Error;
        let tasks = store(vec![task]);
        let view = only(&run_views(std::slice::from_ref(&a), &tasks, None), &a);

        assert_eq!(view.attention, Some(AttentionReason::Failed));
        assert!(view.needs_human);
    }

    #[test]
    fn another_projects_task_looks_exactly_like_a_missing_one() {
        let foreign = task_in(Uuid::from_u128(8), "Foreign", TaskStatus::Backlog);
        let mut foreign_run = foreign.clone();
        start(&mut foreign_run, Uuid::from_u128(5), 1);
        finish(&mut foreign_run, 1);
        answer(&mut foreign_run, 1, "secret answer");
        let foreign_action = enqueue(&foreign, ProjectActionStatus::Applied, applied(&foreign));
        let missing_action = enqueue(&foreign, ProjectActionStatus::Applied, applied(&foreign));
        let mut tasks = store(vec![foreign_run]);
        let views_foreign = run_views(std::slice::from_ref(&foreign_action), &tasks, None);
        tasks.clear();
        let views_missing = run_views(std::slice::from_ref(&missing_action), &tasks, None);

        let foreign_view = only(&views_foreign, &foreign_action);
        let missing_view = only(&views_missing, &missing_action);
        assert_eq!(foreign_view.state, ExecutionState::TaskUnavailable);
        assert_eq!(missing_view.state, ExecutionState::TaskUnavailable);
        assert_eq!(foreign_view.result, ResultView::NotApplicable);
        assert_eq!(foreign_view.run, None);
        assert!(!format!("{foreign_view:?}").contains("secret answer"));
        assert_eq!(foreign_view.run, missing_view.run);
        assert_eq!(foreign_view.result, missing_view.result);
    }

    #[test]
    fn a_deleted_task_does_not_break_the_other_actions() {
        let kept = task_in(project_id(), "Kept", TaskStatus::Backlog);
        let mut kept_run = kept.clone();
        let kept_action = enqueue(&kept, ProjectActionStatus::Applied, applied(&kept));
        start(&mut kept_run, kept_action.id, 1);
        finish(&mut kept_run, 1);
        let deleted = task_in(project_id(), "Deleted", TaskStatus::Backlog);
        let deleted_action = enqueue(&deleted, ProjectActionStatus::Applied, applied(&deleted));
        let tasks = store(vec![kept_run]);
        let views = run_views(&[deleted_action.clone(), kept_action.clone()], &tasks, None);

        assert_eq!(
            only(&views, &deleted_action).state,
            ExecutionState::TaskUnavailable
        );
        assert_eq!(only(&views, &kept_action).state, ExecutionState::Completed);
    }

    #[test]
    fn reloaded_records_reconstruct_the_same_projection() {
        let mut task = task_in(project_id(), "Reloaded", TaskStatus::Backlog);
        let a = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        start(&mut task, a.id, 1);
        finish(&mut task, 1);
        answer(&mut task, 1, "stored answer");
        let before = run_views(std::slice::from_ref(&a), &store(vec![task.clone()]), None);

        let task_json = serde_json::to_string(&task).unwrap();
        let action_json = serde_json::to_string(&a).unwrap();
        let reloaded_task: Task = serde_json::from_str(&task_json).unwrap();
        let reloaded_action: ProjectAction = serde_json::from_str(&action_json).unwrap();
        let after = run_views(
            std::slice::from_ref(&reloaded_action),
            &store(vec![reloaded_task]),
            None,
        );

        assert_eq!(before, after);
    }

    #[test]
    fn historical_task_without_execution_fields_reads_and_has_no_evidence() {
        let task = task_in(project_id(), "Historical", TaskStatus::Backlog);
        let mut value = serde_json::to_value(&task).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("executed_approvals");
        object.remove("approved_execution");
        let historical: Task = serde_json::from_value(value)
            .expect("a Task saved before execution evidence still reads");
        let a = enqueue(
            &historical,
            ProjectActionStatus::Applied,
            applied(&historical),
        );
        let view = only(
            &run_views(std::slice::from_ref(&a), &store(vec![historical]), None),
            &a,
        );

        assert_eq!(view.state, ExecutionState::NoExecutionEvidence);
        assert_eq!(view.run, None);
    }

    #[test]
    fn a_run_without_a_terminal_milestone_is_unconfirmed_not_failed() {
        let mut task = task_in(project_id(), "Unconfirmed", TaskStatus::InProgress);
        let a = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        start(&mut task, a.id, 1);
        let tasks = store(vec![task]);

        // Nothing owns the run, and the executor's answer is unknown.
        let unknown = only(&run_views(std::slice::from_ref(&a), &tasks, None), &a);
        // The executor reports no live run for the Task either.
        let empty = HashSet::new();
        let idle = only(
            &run_views(std::slice::from_ref(&a), &tasks, Some(&empty)),
            &a,
        );

        for view in [unknown, idle] {
            assert_eq!(view.state, ExecutionState::Unconfirmed);
            assert!(view.transitional);
            assert_eq!(view.result, ResultView::NotApplicable);
        }
    }

    #[test]
    fn a_live_latest_run_is_running_and_an_older_one_is_not() {
        let mut task = task_in(project_id(), "Live", TaskStatus::InProgress);
        let first = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        let second = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        start(&mut task, first.id, 1);
        start(&mut task, second.id, 2);
        let id = task.id;
        let tasks = store(vec![task]);
        let live: HashSet<Uuid> = [id].into_iter().collect();
        let views = run_views(&[first.clone(), second.clone()], &tasks, Some(&live));

        assert_eq!(only(&views, &second).state, ExecutionState::Running);
        assert_eq!(
            only(&views, &first).state,
            ExecutionState::Unconfirmed,
            "an older run is not the live one"
        );
    }

    #[test]
    fn a_run_recorded_under_two_approvals_is_ambiguous_and_linked_to_neither() {
        let mut task = task_in(project_id(), "Ambiguous", TaskStatus::Backlog);
        let a = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        task.executed_approvals.push(ExecutedApproval {
            approval_id: a.id,
            run: 1,
            digest: "d".into(),
            checkout: None,
        });
        task.executed_approvals.push(ExecutedApproval {
            approval_id: a.id,
            run: 2,
            digest: "d".into(),
            checkout: None,
        });
        let tasks = store(vec![task]);
        let view = only(&run_views(std::slice::from_ref(&a), &tasks, None), &a);

        assert_eq!(view.state, ExecutionState::Ambiguous);
        assert_eq!(view.run, None);
    }

    #[test]
    fn projection_is_bounded_in_actions_result_and_explanation() {
        let mut task = task_in(project_id(), "Bounded", TaskStatus::Backlog);
        let actions: Vec<ProjectAction> = (0..MAX_EXECUTIONS as u32 + 3)
            .map(|run| {
                let action = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
                start(&mut task, action.id, run + 1);
                finish(&mut task, run + 1);
                action
            })
            .collect();
        let long = "x".repeat(MAX_RESULT_CHARS + 50);
        answer(&mut task, MAX_EXECUTIONS as u32 + 3, &long);
        let mut actions = actions;
        actions.last_mut().unwrap().explanation = "e".repeat(MAX_EXPLANATION_CHARS + 20);
        let tasks = store(vec![task]);
        let views = run_views(&actions, &tasks, None);

        assert_eq!(views.len(), MAX_EXECUTIONS);
        assert_eq!(
            views.first().unwrap().action_id,
            actions[3].id,
            "the oldest actions are dropped"
        );
        let newest = views.last().unwrap();
        match &newest.result {
            ResultView::Available { text } => {
                assert_eq!(text.text.chars().count(), MAX_RESULT_CHARS);
                assert!(text.truncated);
                assert_eq!(text.original_chars as usize, MAX_RESULT_CHARS + 50);
            }
            other => panic!("expected an available result, got {other:?}"),
        }
        assert_eq!(
            newest.explanation.text.chars().count(),
            MAX_EXPLANATION_CHARS
        );
        assert_eq!(
            newest.explanation.original_chars as usize,
            MAX_EXPLANATION_CHARS + 20
        );
    }

    #[test]
    fn a_start_that_failed_before_any_run_is_a_failure_with_no_run_number() {
        let mut task = task_in(project_id(), "Pre-start", TaskStatus::Backlog);
        let b = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        bind(&mut task, b.id, 1);
        // Promotion failed after the binding was written. The binding was not cleared.
        task.status = TaskStatus::Error;
        task.record_activity(ActivityKind::RunFailed {
            run: None,
            reason: "no worktree".into(),
        });
        let tasks = store(vec![task]);
        let view = only(&run_views(std::slice::from_ref(&b), &tasks, None), &b);

        assert_eq!(view.state, ExecutionState::Failed);
        assert_eq!(view.run, None);
        assert_eq!(view.attention, Some(AttentionReason::Failed));
        assert!(view.needs_human);
    }

    #[test]
    fn later_board_state_is_not_attributed_to_an_earlier_completed_run() {
        let mut task = task_in(project_id(), "Moved back", TaskStatus::Backlog);
        let a = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        start(&mut task, a.id, 1);
        finish(&mut task, 1);
        task.status = TaskStatus::HumanReview;
        // A person moved the Task back to Backlog, then a later approval's start was refused.
        task.record_activity(ActivityKind::Moved {
            from: ActivityColumn::HumanReview,
            to: ActivityColumn::Backlog,
        });
        task.status = TaskStatus::Error;
        task.record_activity(ActivityKind::RunFailed {
            run: None,
            reason: "stale".into(),
        });
        let tasks = store(vec![task]);
        let view = only(&run_views(std::slice::from_ref(&a), &tasks, None), &a);

        assert_eq!(view.state, ExecutionState::Completed);
        assert_eq!(view.attention, None);
        assert!(!view.needs_human);
    }

    #[test]
    fn a_completed_run_keeps_the_review_attention_it_produced() {
        let mut task = task_in(project_id(), "Reviewed", TaskStatus::HumanReview);
        let a = enqueue(&task, ProjectActionStatus::Applied, applied(&task));
        start(&mut task, a.id, 1);
        finish(&mut task, 1);
        // The run's changes carried the Task into Human Review.
        task.status = TaskStatus::HumanReview;
        task.record_activity(ActivityKind::ReadyForReview { arrival: 1 });
        let tasks = store(vec![task]);
        let view = only(&run_views(std::slice::from_ref(&a), &tasks, None), &a);

        assert_eq!(view.state, ExecutionState::Completed);
        assert_eq!(view.attention, Some(AttentionReason::Review));
        assert!(view.needs_human);
    }

    #[test]
    fn create_edit_and_move_proposals_never_appear() {
        let task = task_in(project_id(), "Plain", TaskStatus::Backlog);
        let mut action = ProjectAction::proposed(
            "Move it".into(),
            TaskMutation::MoveTask {
                target_task_id: task.id,
                target_task_title: task.title.clone(),
                from: TaskStatus::Queue,
                to: TaskStatus::Backlog,
            },
        );
        action.status = ProjectActionStatus::Applied;
        assert!(run_views(std::slice::from_ref(&action), &store(vec![task]), None).is_empty());
    }
}
