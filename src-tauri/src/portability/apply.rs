// SPDX-License-Identifier: MPL-2.0
//! Apply a validated export (format v1): create one inert Project and its Tasks.
//!
//! Created: the Project under its exported id, and each Task under its exported
//! id with its historical record. Not created: a Repository (the bundle carries
//! no local path, so the Project is created unlinked), any runtime authority
//! (the bundle has none), and Git content. Current approvals are withdrawn.
//!
//! Commit order. Projects and Repositories live in `config.toml`; each
//! Project's Tasks live in their own board file. One atomic write cannot cover
//! both, so the order is:
//!
//! 1. stage the board beside its destination (not visible to anything);
//! 2. commit the Project to `config.toml` (the commit point);
//! 3. publish the board, refusing to replace one;
//! 4. insert the Tasks into memory.
//!
//! If step 3 fails before the board is visible, step 2 is reverted. If that
//! revert also fails, the Project stays recorded without its Tasks, and the
//! error says so. That is the one state this version cannot undo; the bundle
//! file is the record needed to re-import it after the Project is deleted.

use std::collections::HashMap;

use serde::Serialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::commands::project::{get_default_agent_config, try_persist_projects};
use crate::config::paths::StateLocation;
use crate::config::{adopt_published, Storage};
use crate::domain::{
    AgentConfig, HumanReviewRecord, NewTask, Project, ProjectScope, Repository, Task, TaskStatus,
};
use crate::lifecycle::ProjectLifecycleGate;

use super::bundle::{self, BundleError, Payload, PortableTask};
use super::preview::{self, Conflict, Normalization, Target};

/// The live state an apply writes into, borrowed from the running instance.
pub struct Live<'a> {
    pub gate: &'a ProjectLifecycleGate,
    pub tasks: &'a RwLock<HashMap<Uuid, Task>>,
    pub projects: &'a RwLock<HashMap<Uuid, Project>>,
    pub repositories: &'a RwLock<HashMap<Uuid, Repository>>,
    pub storage: &'a Storage,
}

#[derive(Debug, Serialize)]
pub struct ApplyReport {
    pub project_id: Uuid,
    pub project_name: String,
    pub tasks: u32,
    /// What was withdrawn or changed so that nothing imported can act.
    pub normalization: Vec<Normalization>,
    /// The Repository the bundle named. It was not recreated: link a local
    /// checkout to the Project before it can run work.
    pub repository_remote: Option<String>,
    /// False when the commit or the board may not survive a power loss.
    pub durable: bool,
}

#[derive(Debug)]
pub enum ApplyError {
    /// Not a readable, valid export.
    Bundle(BundleError),
    /// The file's payload is not the one the caller confirmed. Preview it again.
    ChecksumMismatch { found: String },
    /// Identity collisions in the target. Nothing was written.
    Refused(Vec<Conflict>),
    /// Failed before anything became visible, or reverted. Nothing changed.
    Failed(String),
    /// The Project is recorded but its Tasks are not, and the revert failed.
    Incomplete(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bundle(e) => write!(f, "{e}"),
            Self::ChecksumMismatch { found } => write!(
                f,
                "the export's checksum is {found}, which is not the checksum that was confirmed; preview the export again"
            ),
            Self::Refused(conflicts) => {
                let kinds: Vec<&str> = conflicts.iter().map(|c| c.kind).collect();
                write!(f, "the import is refused, nothing was changed: {}", kinds.join(", "))
            }
            Self::Failed(message) | Self::Incomplete(message) => f.write_str(message),
        }
    }
}

/// Validate `bytes` again and create the Project it describes, if the target
/// still has no identity of it.
///
/// `confirmed_checksum` is the payload checksum the caller read from a Preview
/// of this file. The import proceeds only if the file still carries it.
pub async fn apply(
    live: Live<'_>,
    bytes: &[u8],
    confirmed_checksum: &str,
) -> Result<ApplyReport, ApplyError> {
    let decoded = bundle::decode(bytes).map_err(ApplyError::Bundle)?;
    if decoded.checksum != confirmed_checksum {
        return Err(ApplyError::ChecksumMismatch {
            found: decoded.checksum,
        });
    }
    let project_id = decoded.payload.project.id;

    // Lock order: gate, Tasks, Projects, Repositories. Project creation takes
    // the same order, so an import cannot deadlock with it.
    let _gate = live.gate.write(project_id).await;
    let mut tasks_w = live.tasks.write().await;
    let mut projects_w = live.projects.write().await;
    let repositories_r = live.repositories.read().await;

    // The identity rules are the Preview's, applied to the locked state. An
    // earlier Preview is not trusted for identities that may have changed.
    let report = preview::preview(
        bytes,
        &Target {
            projects: &projects_w,
            repositories: &repositories_r,
            tasks: &tasks_w,
            unreadable_boards: live.storage.task_board_diagnostics().len(),
        },
    )
    .map_err(ApplyError::Bundle)?;
    if !report.importable {
        return Err(ApplyError::Refused(report.conflicts));
    }
    drop(repositories_r);

    let (project, tasks, normalization, repository_remote) =
        materialize(decoded.payload).map_err(ApplyError::Bundle)?;
    let project_name = project.name.clone();
    let task_count = tasks.len() as u32;

    // Step 1: invisible until step 3.
    let staged_board = live
        .storage
        .stage_new_project_board(project_id, &tasks)
        .map_err(|e| ApplyError::Failed(format!("{e:#}")))?;

    // Step 2: the commit point. The Project's previous set is kept for a revert.
    let original = projects_w.clone();
    let mut proposed = projects_w.clone();
    proposed.insert(project.id, project);
    let outcome = try_persist_projects(live.storage, &proposed);
    let mut durable = true;
    match adopt_published(&mut *projects_w, proposed, outcome) {
        Ok(()) => {}
        Err(e) if e.is_published() => durable = false,
        Err(e) => {
            return Err(ApplyError::Failed(
                e.describe("record the imported Project"),
            ))
        }
    }
    // A save can succeed while the directory cannot be synchronized; only the
    // storage flag says so.
    durable &= !live.storage.config_is_unconfirmed();

    // Step 3: the board becomes visible, or the Project is reverted.
    match staged_board.publish_new() {
        Ok(committed) => durable &= committed.is_fully_durable(),
        Err(e) if e.is_published() => durable = false,
        Err(e) => {
            let board_error = format!("the Task board could not be published ({e})");
            return match try_persist_projects(live.storage, &original) {
                Ok(()) => {
                    *projects_w = original;
                    Err(ApplyError::Failed(format!("{board_error}, so the import was reverted")))
                }
                Err(revert) if revert.is_published() => {
                    *projects_w = original;
                    Err(ApplyError::Failed(format!(
                        "{board_error}; the revert is in place but not confirmed durable: {}",
                        revert.describe("revert the imported Project")
                    )))
                }
                Err(revert) => Err(ApplyError::Incomplete(format!(
                    "{board_error}, and the revert also failed, so Project {project_id} is recorded without its Tasks. Delete that Project before importing again: {}",
                    revert.describe("revert the imported Project")
                ))),
            };
        }
    }

    // Step 4: the Tasks are visible only now, after the board is in place.
    for task in tasks {
        tasks_w.insert(task.id, task);
    }
    Ok(ApplyReport {
        project_id,
        project_name,
        tasks: task_count,
        normalization,
        repository_remote,
        durable,
    })
}

/// The records an import creates, the normalizations applied, and the remote the bundle named.
type Materialized = (Project, Vec<Task>, Vec<Normalization>, Option<String>);

/// Turn a validated payload into the records an import creates. Pure: nothing
/// is read from or written to the instance.
fn materialize(payload: Payload) -> Result<Materialized, BundleError> {
    let Payload {
        project: p,
        repository,
        tasks,
    } = payload;
    let agent_config = AgentConfig {
        model: p.model,
        ..get_default_agent_config(&p.agent_type)
    };
    let project = Project {
        id: p.id,
        name: p.name,
        // The bundle never names a local path, so no Repository is recreated.
        repository_id: None,
        scope: ProjectScope::Standalone,
        // Matches a new Project: its board stays in SlashIt's own state directory.
        state_location: StateLocation::External,
        base: p.base,
        agent_type: p.agent_type,
        agent_config,
        created_at: p.created_at,
        updated_at: p.updated_at,
    };

    let mut normalization = Vec::new();
    let mut out = Vec::with_capacity(tasks.len());
    for task in tasks {
        out.push(materialize_task(p.id, task, &mut normalization)?);
    }
    let remote = repository.and_then(|r| r.remote_url);
    Ok((project, out, normalization, remote))
}

fn materialize_task(
    project_id: Uuid,
    t: PortableTask,
    normalization: &mut Vec<Normalization>,
) -> Result<Task, BundleError> {
    let task_id = t.id;
    let activity: Vec<slashit_activity::Entry> = t
        .activity
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .map_err(|e| BundleError::Malformed(format!("task {task_id} activity: {e}")))?;

    // An approval describes the arrival it was made in. Closing that arrival
    // is the existing withdrawal, so the approval stays as history only.
    let mut review = HumanReviewRecord {
        arrivals: t.human_review.arrivals,
        entries: t.human_review.entries,
        pr_error: None,
    };
    let arrivals_before = review.arrivals;
    review.withdraw_approval();
    if review.arrivals != arrivals_before {
        normalization.push(Normalization {
            task_id,
            reason: "approval_withdrawn",
        });
    }

    // A review wait needs its checkout, which the bundle does not carry. The
    // Task returns to the backlog and its review history stays.
    let status = match t.status {
        TaskStatus::HumanReview => {
            normalization.push(Normalization {
                task_id,
                reason: "human_review_without_checkout",
            });
            TaskStatus::Backlog
        }
        other => other,
    };

    let mut task = Task::new_backlog(
        &HashMap::new(),
        NewTask {
            id: task_id,
            project_id,
            title: t.title,
            description: t.description,
            model: t.model,
            reviewer_model: t.reviewer_model,
            planning_mode: t.planning_mode,
            dependencies: t.dependencies,
            category: t.category,
            priority: t.priority,
            complexity: t.complexity,
            impact: t.impact,
            security_severity: t.security_severity,
            github_issue_url: t.github_issue_url,
            gitlab_issue_url: t.gitlab_issue_url,
            linear_ticket_id: t.linear_ticket_id,
        },
    );
    task.status = status;
    task.review_mode = t.review_mode;
    task.provider = t.provider;
    task.subtasks = t.subtasks;
    task.sequence_number = t.sequence_number;
    task.position = t.position;
    task.external_refs = t.external_refs;
    task.jira_issue_key = t.jira_issue_key;
    task.pr_url = t.pr_url;
    task.qa_signoff = t.qa_signoff;
    task.human_review = review;
    task.base_commit = t.base_commit;
    task.activity = activity;
    task.created_at = t.created_at;
    task.updated_at = t.updated_at;
    Ok(task)
}
