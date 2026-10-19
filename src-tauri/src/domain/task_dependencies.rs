//! The rules for the Task dependencies a person configures.
//!
//! A new configuration names at most one Task. The executor stacks a Task's
//! branch on a single dependency, and no rule yet says how several parents
//! combine into one stack, so this module refuses a request that would name
//! more. A Task that already holds several keeps them: nothing here removes or
//! replaces them, a change to such a set is refused, and the executor starts
//! such a Task only from a base that holds the work of every one of them.
//!
//! Every function here refuses rather than repairs. A malformed identifier is
//! an error, not something to skip, and a refused request changes nothing.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::domain::{Task, TaskStatus};

/// How many dependencies one Task may name in a new configuration.
pub const SUPPORTED_DEPENDENCY_COUNT: usize = 1;

/// Read the dependency identifiers a request names.
///
/// Refuses an identifier that is not a UUID, a repeated identifier, and more
/// than [`SUPPORTED_DEPENDENCY_COUNT`] identifiers. Nothing is dropped.
pub fn parse_requested(raw: &[String]) -> Result<Vec<Uuid>, String> {
    let mut seen = HashSet::with_capacity(raw.len());
    let mut ids = Vec::with_capacity(raw.len());
    for text in raw {
        let id = Uuid::parse_str(text)
            .map_err(|_| format!("\"{text}\" is not a valid Task identifier; nothing was changed"))?;
        if !seen.insert(id) {
            return Err(format!("Task {id} is listed as a dependency more than once; nothing was changed"));
        }
        ids.push(id);
    }
    if ids.len() > SUPPORTED_DEPENDENCY_COUNT {
        return Err(format!(
            "A Task can depend on one other Task, but {} were named; nothing was changed",
            ids.len()
        ));
    }
    Ok(ids)
}

/// Check the requested dependencies of `task_id` against the Tasks they name.
///
/// `graph` must hold every Task of `project_id` that the check can reach. A
/// dependency that is absent from it is refused as not found in this Project;
/// the caller that holds every Task can tell it apart from a Task of another
/// Project. A dependency cycle is refused when the requested Task can be reached
/// from a requested dependency through the Tasks already recorded. The Task's own
/// recorded dependencies are not followed, because the request replaces them.
pub fn check_targets(
    graph: &HashMap<Uuid, Task>,
    task_id: Uuid,
    project_id: Uuid,
    requested: &[Uuid],
) -> Result<(), String> {
    for dependency in requested {
        if *dependency == task_id {
            return Err("A Task cannot depend on itself; nothing was changed".to_string());
        }
        let Some(target) = graph.get(dependency) else {
            return Err(format!(
                "Task {dependency} was not found in this Project; nothing was changed"
            ));
        };
        if target.project_id != project_id {
            return Err(format!(
                "Task {dependency} belongs to another Project; nothing was changed"
            ));
        }
        if reaches(graph, *dependency, task_id) {
            return Err(format!(
                "Depending on \"{}\" would make this Task depend on itself through a chain of \
                 dependencies; nothing was changed",
                target.title
            ));
        }
    }
    Ok(())
}

/// Whether `to` can be reached from `from` by following recorded dependencies.
///
/// Tasks already on a cycle are visited once, so a damaged record cannot make
/// the walk loop. `to` is never expanded, so its own recorded dependencies, which
/// a request replaces, are not followed.
fn reaches(graph: &HashMap<Uuid, Task>, from: Uuid, to: Uuid) -> bool {
    let mut visited = HashSet::new();
    let mut pending = vec![from];
    while let Some(current) = pending.pop() {
        if current == to {
            return true;
        }
        if !visited.insert(current) {
            continue;
        }
        if let Some(task) = graph.get(&current) {
            pending.extend(task.dependencies.iter().copied());
        }
    }
    false
}

/// Why `task`'s dependencies may not become `requested`, or `None` when they may.
///
/// A request that matches the recorded dependencies changes nothing and is
/// never refused. A Task that already holds several dependencies cannot be
/// changed here. Otherwise the Task's own state decides, through
/// [`Task::dependency_change_refusal`].
pub fn change_refusal(task: &Task, requested: &[Uuid]) -> Option<String> {
    if task.dependencies == requested {
        return None;
    }
    if task.dependencies.len() > SUPPORTED_DEPENDENCY_COUNT {
        return Some(
            "This Task has more than one dependency, and changing them is not supported yet, so \
             they were kept; nothing was changed"
                .to_string(),
        );
    }
    task.dependency_change_refusal()
}

/// The column name a person sees for `status`.
pub fn column_name(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Backlog => "Backlog",
        TaskStatus::Queue => "Queue",
        TaskStatus::InProgress => "In Progress",
        TaskStatus::AiReview => "AI Review",
        TaskStatus::HumanReview => "Human Review",
        TaskStatus::Done => "Done",
        TaskStatus::PrCreated => "PR Created",
        TaskStatus::Error => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{NewTask, TaskCategory, TaskComplexity, TaskImpact, TaskPriority, SecuritySeverity};

    fn task(project_id: Uuid, title: &str, dependencies: Vec<Uuid>) -> Task {
        let existing = HashMap::new();
        let mut task = Task::new_backlog(
            &existing,
            NewTask {
                id: Uuid::new_v4(),
                project_id,
                title: title.to_string(),
                description: None,
                model: "default".to_string(),
                planning_mode: false,
                dependencies,
                category: TaskCategory::default(),
                priority: TaskPriority::default(),
                complexity: TaskComplexity::default(),
                impact: TaskImpact::default(),
                security_severity: SecuritySeverity::default(),
                github_issue_url: None,
                gitlab_issue_url: None,
                linear_ticket_id: None,
            },
        );
        task.status = TaskStatus::Backlog;
        task
    }

    fn board(tasks: &[Task]) -> HashMap<Uuid, Task> {
        tasks.iter().map(|t| (t.id, t.clone())).collect()
    }

    #[test]
    fn a_valid_identifier_is_read_once() {
        let id = Uuid::new_v4();
        assert_eq!(parse_requested(&[id.to_string()]), Ok(vec![id]));
        assert_eq!(parse_requested(&[]), Ok(vec![]));
    }

    #[test]
    fn a_malformed_identifier_is_refused_not_dropped() {
        let error = parse_requested(&["not-a-task".to_string()]).expect_err("refused");
        assert!(error.contains("not-a-task"), "{error}");
        assert!(error.contains("nothing was changed"), "{error}");
    }

    #[test]
    fn a_repeated_identifier_is_refused() {
        let id = Uuid::new_v4().to_string();
        let error = parse_requested(&[id.clone(), id]).expect_err("refused");
        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn more_than_one_new_dependency_is_refused() {
        let ids = [Uuid::new_v4().to_string(), Uuid::new_v4().to_string()];
        let error = parse_requested(&ids).expect_err("refused");
        assert!(error.contains("2 were named"), "{error}");
    }

    #[test]
    fn a_task_cannot_depend_on_itself() {
        let project = Uuid::new_v4();
        let subject = task(project, "subject", vec![]);
        let graph = board(std::slice::from_ref(&subject));
        let error = check_targets(&graph, subject.id, project, &[subject.id]).expect_err("refused");
        assert!(error.contains("itself"), "{error}");
    }

    #[test]
    fn a_missing_task_is_refused() {
        let project = Uuid::new_v4();
        let subject = task(project, "subject", vec![]);
        let graph = board(std::slice::from_ref(&subject));
        let missing = Uuid::new_v4();
        let error = check_targets(&graph, subject.id, project, &[missing]).expect_err("refused");
        assert!(error.contains(&missing.to_string()), "{error}");
    }

    #[test]
    fn a_task_of_another_project_is_refused() {
        let project = Uuid::new_v4();
        let subject = task(project, "subject", vec![]);
        let foreign = task(Uuid::new_v4(), "foreign", vec![]);
        let graph = board(&[subject.clone(), foreign.clone()]);
        let error = check_targets(&graph, subject.id, project, &[foreign.id]).expect_err("refused");
        assert!(error.contains("another Project"), "{error}");
    }

    #[test]
    fn a_direct_cycle_is_refused() {
        let project = Uuid::new_v4();
        let a = task(project, "a", vec![]);
        let b = task(project, "b", vec![a.id]);
        let graph = board(&[a.clone(), b.clone()]);
        let error = check_targets(&graph, a.id, project, &[b.id]).expect_err("refused");
        assert!(error.contains("depend on itself"), "{error}");
    }

    #[test]
    fn an_indirect_cycle_is_refused() {
        let project = Uuid::new_v4();
        let a = task(project, "a", vec![]);
        let b = task(project, "b", vec![a.id]);
        let c = task(project, "c", vec![b.id]);
        let graph = board(&[a.clone(), b, c.clone()]);
        let error = check_targets(&graph, a.id, project, &[c.id]).expect_err("refused");
        assert!(error.contains("\"c\""), "{error}");
    }

    #[test]
    fn a_cycle_that_already_exists_elsewhere_does_not_hang_the_check() {
        let project = Uuid::new_v4();
        let mut x = task(project, "x", vec![]);
        let mut y = task(project, "y", vec![x.id]);
        x.dependencies = vec![y.id];
        y.dependencies = vec![x.id];
        let subject = task(project, "subject", vec![]);
        let graph = board(&[x.clone(), y, subject.clone()]);
        assert_eq!(check_targets(&graph, subject.id, project, &[x.id]), Ok(()));
    }

    #[test]
    fn a_valid_dependency_is_accepted() {
        let project = Uuid::new_v4();
        let parent = task(project, "parent", vec![]);
        let child = task(project, "child", vec![]);
        let graph = board(&[parent.clone(), child.clone()]);
        assert_eq!(check_targets(&graph, child.id, project, &[parent.id]), Ok(()));
    }

    #[test]
    fn an_unchanged_set_is_never_refused_even_when_several_are_recorded() {
        let project = Uuid::new_v4();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let legacy = task(project, "legacy", vec![a, b]);
        assert_eq!(change_refusal(&legacy, &[a, b]), None);
    }

    #[test]
    fn a_change_to_a_multi_dependency_record_is_refused() {
        let project = Uuid::new_v4();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let legacy = task(project, "legacy", vec![a, b]);
        assert!(change_refusal(&legacy, &[]).expect("refused").contains("more than one"));
        assert!(change_refusal(&legacy, &[a]).expect("refused").contains("more than one"));
    }

    #[test]
    fn a_change_is_allowed_for_a_backlog_task_without_a_checkout() {
        let project = Uuid::new_v4();
        let subject = task(project, "subject", vec![]);
        assert_eq!(change_refusal(&subject, &[Uuid::new_v4()]), None);
    }

    #[test]
    fn a_change_is_refused_once_the_task_has_a_checkout() {
        let project = Uuid::new_v4();
        let mut subject = task(project, "subject", vec![]);
        subject.status = TaskStatus::InProgress;
        subject.branch_name = Some("slashit/subject".to_string());
        assert!(change_refusal(&subject, &[Uuid::new_v4()]).is_some());
    }
}
