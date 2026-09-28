//! Which tasks need the user, across every project.
//!
//! A read, and only of attention: the board of the project being looked at
//! already has its tasks, so this exists for everything else that wants to
//! say "this project needs you" -- the project rail, today -- without
//! loading whole boards to find out.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use uuid::Uuid;

use crate::domain::task::AttentionReason;
use crate::domain::Task;

/// The tasks in one project that need the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectAttention {
    pub project_id: Uuid,
    /// Never empty: a project nothing is needed from is left out.
    pub tasks: Vec<TaskAttention>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskAttention {
    pub task_id: Uuid,
    pub reason: AttentionReason,
}

/// Every project that has a task needing the user, in a stable order:
/// projects by id, and their tasks by column position, then id.
pub fn summarize(
    tasks: &HashMap<Uuid, Task>,
    delivery_in_flight: impl Fn(Uuid) -> bool,
) -> Vec<ProjectAttention> {
    let mut by_project: BTreeMap<Uuid, Vec<(i32, Uuid, AttentionReason)>> = BTreeMap::new();
    for task in tasks.values() {
        if let Some(reason) = task.needs_you(delivery_in_flight(task.id)) {
            by_project.entry(task.project_id).or_default().push((task.position, task.id, reason));
        }
    }
    by_project
        .into_iter()
        .map(|(project_id, mut found)| {
            found.sort_by_key(|(position, id, _)| (*position, *id));
            ProjectAttention {
                project_id,
                tasks: found
                    .into_iter()
                    .map(|(_, task_id, reason)| TaskAttention { task_id, reason })
                    .collect(),
            }
        })
        .collect()
}

#[tauri::command]
pub async fn get_attention_summary(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<ProjectAttention>, String> {
    let tasks = state.task.tasks.read().await;
    let deliveries = &state.task.deliveries;
    Ok(summarize(&tasks, |id| deliveries.contains(id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task::HumanReviewDecision;
    use crate::domain::TaskStatus;
    use crate::test_helpers::create_test_task_full;

    fn task(project_id: Uuid, status: TaskStatus, position: i32) -> Task {
        let mut task = create_test_task_full("t", project_id, status, position);
        task.human_review.record_arrival();
        task
    }

    #[test]
    fn only_projects_and_tasks_that_need_the_user_are_listed() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let quiet = Uuid::from_u128(3);

        let failed = task(a, TaskStatus::Error, 1);
        let review = task(a, TaskStatus::HumanReview, 0);
        let mut approved = task(b, TaskStatus::HumanReview, 0);
        approved.human_review.push(HumanReviewDecision::Approved, None, chrono::Utc::now());
        let mut pr_failed = task(b, TaskStatus::HumanReview, 1);
        pr_failed.human_review.push(HumanReviewDecision::Approved, None, chrono::Utc::now());
        pr_failed.human_review.pr_error = Some("gh failed".into());
        let running = task(quiet, TaskStatus::InProgress, 0);

        let all: HashMap<Uuid, Task> = [&failed, &review, &approved, &pr_failed, &running]
            .into_iter()
            .map(|t| (t.id, t.clone()))
            .collect();

        assert_eq!(
            summarize(&all, |_| false),
            vec![
                ProjectAttention {
                    project_id: a,
                    tasks: vec![
                        TaskAttention { task_id: review.id, reason: AttentionReason::Review },
                        TaskAttention { task_id: failed.id, reason: AttentionReason::Failed },
                    ],
                },
                ProjectAttention {
                    project_id: b,
                    tasks: vec![TaskAttention { task_id: pr_failed.id, reason: AttentionReason::PrNotCreated }],
                },
            ]
        );

        // While its pull request is being opened again, the task in `b`
        // waits on SlashIt, and `b` drops out.
        let summary = summarize(&all, |id| id == pr_failed.id);
        assert_eq!(summary.iter().map(|p| p.project_id).collect::<Vec<_>>(), vec![a]);
    }
}
