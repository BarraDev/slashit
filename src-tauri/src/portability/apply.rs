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
//! 1. write the import record (`<id>.import-pending`, beside `config.toml`);
//! 2. stage the board beside its destination (not visible to anything);
//! 3. commit the Project to `config.toml` (the commit point);
//! 4. publish the board, refusing to replace one;
//! 5. remove the import record and insert the Tasks into memory.
//!
//! The record is what separates an interrupted import from a Project that is
//! legitimately empty. If step 4 fails before the board is visible, step 3 is
//! reverted and the record removed. If the revert fails, or the process ends
//! first, the record stays. The next start settles it with
//! [`recover_interrupted_imports`]: a Project with no board is removed, so the
//! import can be retried, and nothing else is touched.

use std::collections::HashMap;

use serde::Serialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::commands::project::{get_default_agent_config, try_persist_projects};
use crate::config::paths::StateLocation;
use crate::config::{adopt_published, ImportRecord, Storage};
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

    // A Project held back by an earlier import, or a cleanup still owed for its id,
    // keeps its record, board, configuration entry or cleanup record. The preview
    // sees only memory, so the disk is checked
    // here: a new import must not replace, clear, or adopt any of them.
    match live.storage.project_import_residue(project_id) {
        Ok(false) => {}
        Ok(true) => {
            return Err(ApplyError::Failed(format!(
                "Project {project_id} still has state from an earlier import or a pending cleanup on this instance: an import record, Task board, configuration entry, or cleanup record remains. The import record clears once a start can confirm the configuration durable, and a cleanup clears when it completes; import again after both."
            )))
        }
        Err(e) => {
            return Err(ApplyError::Failed(format!(
                "the state of Project {project_id} could not be read, so nothing was imported: {e:#}"
            )))
        }
    }

    // Step 1: the record that makes an interrupted import recognizable. Nothing
    // is committed yet, so a refusal here leaves nothing to undo. Written in
    // place but not confirmed durable, the import proceeds and reports it.
    // Whether the Project's configuration entry is confirmed durable. Only that and
    // the board decide when the record may go; the record's own write does not.
    let mut config_durable = true;
    let mut durable = match live.storage.mark_project_import_pending(project_id) {
        Ok(committed) => committed.is_fully_durable(),
        Err(e) if e.is_published() => false,
        Err(e) => {
            return Err(ApplyError::Failed(format!(
                "the import record could not be written, so nothing was imported: {e}"
            )))
        }
    };

    // Step 2: invisible until step 4.
    let staged_board = match live.storage.stage_new_project_board(project_id, &tasks) {
        Ok(staged) => staged,
        Err(e) => {
            clear_import_record(live.storage, project_id);
            return Err(ApplyError::Failed(format!("{e:#}")));
        }
    };

    // Step 3: the commit point. The Project's previous set is kept for a revert.
    let original = projects_w.clone();
    let mut proposed = projects_w.clone();
    proposed.insert(project.id, project);
    let outcome = try_persist_projects(live.storage, &proposed);
    match adopt_published(&mut *projects_w, proposed, outcome) {
        Ok(()) => {}
        Err(e) if e.is_published() => {
            durable = false;
            config_durable = false;
        }
        Err(e) => {
            clear_import_record(live.storage, project_id);
            return Err(ApplyError::Failed(
                e.describe("record the imported Project"),
            ));
        }
    }
    // A save can succeed while the directory cannot be synchronized; only the
    // storage flag says so.
    config_durable &= !live.storage.config_is_unconfirmed();
    durable &= config_durable;

    // Step 4: the board becomes visible, or the Project is reverted.
    let board_durable = match staged_board.publish_new() {
        Ok(committed) => committed.is_fully_durable(),
        Err(e) if e.is_published() => false,
        Err(e) => {
            let board_error = format!("the Task board could not be published ({e})");
            return match try_persist_projects(live.storage, &original) {
                // A revert the filesystem cannot synchronize is in place but may not
                // survive a power loss, which could bring back the Project entry from
                // Step 3. Keep the record, as for an unconfirmed revert below.
                Ok(()) if live.storage.config_is_unconfirmed() => {
                    live.storage.hold_back_project(project_id);
                    *projects_w = original;
                    Err(ApplyError::Incomplete(format!(
                        "{board_error}; the revert is in place but not confirmed durable on this filesystem, so the import record is kept until a start confirms the configuration. Restart SlashIt to settle the import."
                    )))
                }
                Ok(()) => {
                    *projects_w = original;
                    clear_import_record(live.storage, project_id);
                    Err(ApplyError::Failed(format!("{board_error}, so the import was reverted")))
                }
                // The revert is in place. The record stays until a start confirms
                // the Project is gone.
                Err(revert) if revert.is_published() => {
                    // The Project is out of memory, but its entry may still be on
                    // disk after a power loss: later saves must keep it.
                    live.storage.hold_back_project(project_id);
                    *projects_w = original;
                    Err(ApplyError::Failed(format!(
                        "{board_error}; the revert is in place but not confirmed durable: {}",
                        revert.describe("revert the imported Project")
                    )))
                }
                // The Project leaves memory, so nothing in this process can use it.
                // The record stays, and the next start settles it.
                Err(revert) => {
                    // The revert did not publish, so the Project's entry is still
                    // on disk while memory has no Project. Later saves keep it.
                    live.storage.hold_back_project(project_id);
                    *projects_w = original;
                    Err(ApplyError::Incomplete(format!(
                        "{board_error}, and the revert also failed, so Project {project_id} is recorded without its Tasks. Restart SlashIt: the interrupted import is then undone, and the bundle can be imported again. {}",
                        revert.describe("revert the imported Project")
                    )))
                }
            };
        }
    };

    // Step 5: the board is in place, so the import is settled. The Tasks are
    // visible only now. The record goes only when the configuration and the
    // board are both confirmed durable. Otherwise a power loss could leave the
    // Project's entry or its board without the other, and the record is what
    // lets the next start hold that state back.
    if board_durable && config_durable {
        clear_import_record(live.storage, project_id);
    }
    durable &= board_durable;
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

/// Remove the import record once the state it guards is settled. A failed
/// removal is harmless: the next start finds the record and settles it again.
fn clear_import_record(storage: &Storage, project_id: Uuid) {
    if let Err(error) = storage.clear_project_import_pending(project_id) {
        eprintln!("Warning: {error:#}; the next start removes it");
    }
}

/// What startup did about the import records it found.
#[derive(Debug, Default)]
pub struct ImportRecovery {
    /// One message per record that still needs attention. Such a record stays
    /// on disk, so the next start checks it again.
    pub failures: Vec<String>,
    /// Projects kept out of this process, and so their Tasks as well. Nothing
    /// is deleted for them: the Project and its record stay for repair.
    pub quarantined: Vec<Uuid>,
}

/// Settle the import records a previous process left behind. Runs at startup,
/// before any Project is used.
///
/// - A record whose content does not confirm its file name, or a record that
///   cannot be listed: its Project is quarantined. Nothing is inferred from the
///   record, and nothing on disk is changed.
/// - Without an authoritative configuration (`authoritative` is false, which
///   means the configuration was read with salvage), every record's Project is
///   quarantined. Settling needs proof that a Project is absent. The salvage
///   reader recovers no Projects today, so what this holds back is the Task
///   board, which would otherwise load with no Project to own it.
/// - No Project under the record: the import never committed. The record goes.
/// - A Project with a board: the import completed. The Project is loaded. Its
///   record goes once the configuration is `confirmed` durable and the board's
///   directory entry is synchronized at this start; until then the record stays,
///   because it is what holds back a Project whose entry or board a power loss
///   could drop.
/// - A Project without a board: the import was interrupted. The Project is
///   removed from the configuration, which is the only change made. Its
///   record goes once that removal is confirmed.
///
/// `confirmed` is false on a filesystem that cannot synchronize a directory: a
/// write there never confirms, so the record cannot go, but the configuration
/// that was read in full still settles the import.
pub fn recover_interrupted_imports(
    storage: &Storage,
    projects: &mut HashMap<Uuid, Project>,
    authoritative: bool,
    confirmed: bool,
) -> ImportRecovery {
    let mut recovery = ImportRecovery::default();
    let records = match storage.import_records() {
        Ok(records) => records,
        Err(error) => {
            // Without the records, nothing shows that an import is settled. Every
            // Project is held back, so no later configuration save drops one.
            recovery.quarantined = projects.keys().copied().collect();
            for id in &recovery.quarantined {
                storage.hold_back_project(*id);
            }
            projects.clear();
            recovery.failures.push(format!(
                "the import records could not be read, so no Project is loaded: {error:#}"
            ));
            return recovery;
        }
    };
    for ImportRecord {
        project_id,
        path,
        problem,
    } in records
    {
        let Some(project_id) = project_id else {
            recovery.failures.push(format!(
                "{}: {}; nothing was changed",
                path.display(),
                problem.unwrap_or_default()
            ));
            continue;
        };
        if let Some(problem) = problem {
            quarantine(storage, projects, project_id, &mut recovery);
            recovery.failures.push(format!(
                "{project_id}: the import record {} cannot be trusted ({problem}); the Project is held back and the record is kept",
                path.display()
            ));
            continue;
        }
        if !authoritative {
            quarantine(storage, projects, project_id, &mut recovery);
            recovery.failures.push(format!(
                "{project_id}: held back until the configuration loads completely; its import record is kept"
            ));
            continue;
        }
        settle_record(storage, projects, project_id, confirmed, &mut recovery);
    }
    recovery
}

/// Keep a Project out of this process. Its Tasks are filtered out too, and its
/// configuration entry is held in place by [`Storage::hold_back_project`].
fn quarantine(
    storage: &Storage,
    projects: &mut HashMap<Uuid, Project>,
    project_id: Uuid,
    recovery: &mut ImportRecovery,
) {
    storage.hold_back_project(project_id);
    projects.remove(&project_id);
    recovery.quarantined.push(project_id);
}

/// Settle one trusted record under an authoritative configuration.
fn settle_record(
    storage: &Storage,
    projects: &mut HashMap<Uuid, Project>,
    project_id: Uuid,
    confirmed: bool,
    recovery: &mut ImportRecovery,
) {
    // A board that cannot be stat'ed may still exist, so it counts as present.
    let board_present = storage
        .tasks_board_files(project_id)
        .iter()
        .any(|file| file.try_exists().unwrap_or(true));
    // Under an authoritative configuration, a Project absent from memory is absent
    // from the file. With its board still present, the import's completion cannot
    // be shown, so the board is held back rather than adopted or cleared.
    if !projects.contains_key(&project_id) && board_present {
        quarantine(storage, projects, project_id, recovery);
        recovery.failures.push(format!(
            "{project_id}: the import record remains, its Project is not in the configuration, and its Task board is present; the board is held back and nothing was changed"
        ));
        return;
    }
    if projects.contains_key(&project_id) && !board_present {
        let mut proposed = projects.clone();
        proposed.remove(&project_id);
        match try_persist_projects(storage, &proposed) {
            Ok(()) => *projects = proposed,
            Err(error) => {
                // The interrupted Project is not usable in this process, even
                // when its removal is not in place. The record stays, and the
                // entry is held so a later save does not drop it.
                quarantine(storage, projects, project_id, recovery);
                recovery.failures.push(format!(
                    "{project_id}: {}",
                    error.describe("remove the interrupted import")
                ));
                return;
            }
        }
    }
    // The record goes only once the configuration is confirmed durable. Until then
    // it stays whatever this start settled: a removal written here may return
    // after a power loss, and the record is what repeats it.
    if !confirmed {
        recovery.failures.push(format!(
            "{project_id}: settled from the configuration, and its import record is kept until the configuration can be confirmed durable on this filesystem"
        ));
        return;
    }
    // The configuration's confirmation covers its own directory only. The board's
    // directory entry is not covered by it, so it is synchronized here too: a
    // board that was never proven durable would otherwise lose its entry after
    // the record is gone.
    match storage.sync_board_directories(project_id) {
        Ok(slashit_durable_fs::DirectorySync::Synced) => {}
        Ok(slashit_durable_fs::DirectorySync::Unsupported) => {
            recovery.failures.push(format!(
                "{project_id}: the directory of its Task board cannot be synchronized on this filesystem, so the board is not confirmed durable; the import record is kept and the Project loads until a start can confirm it"
            ));
            return;
        }
        Err(error) => {
            recovery.failures.push(format!(
                "{project_id}: the directory of its Task board could not be synchronized ({error:#}); the import record is kept"
            ));
            return;
        }
    }
    if let Err(error) = storage.clear_project_import_pending(project_id) {
        recovery.failures.push(format!("{project_id}: {error:#}"));
    }
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

    // Store what the parser accepts, and report what changed (see
    // `preview::reviewer_model_for_import`). Storing the raw text would start a
    // reviewer run with `--model ""` or a padded identifier.
    let (reviewer_model, reviewer_note) =
        super::preview::reviewer_model_for_import(task_id, t.reviewer_model.as_deref())?;
    normalization.extend(reviewer_note);

    let mut task = Task::new_backlog(
        &HashMap::new(),
        NewTask {
            id: task_id,
            project_id,
            title: t.title,
            description: t.description,
            model: t.model,
            reviewer_model,
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
