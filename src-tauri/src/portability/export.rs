// SPDX-License-Identifier: MPL-2.0
//! Turn one Project's in-memory record into a portable payload, and write it.
//!
//! The caller decides what is consistent: [`build_payload`] reads the records
//! it is given and nothing else. The IPC handler supplies them under the
//! Project's lifecycle gate and every Task lease, so no Task transition runs
//! while they are read.

use std::path::Path;

use slashit_activity::{Entry as ActivityEntry, Kind as ActivityKind};
use slashit_durable_fs::{Permissions, WriteError};

use crate::domain::publication::PublicationState;
use crate::domain::{ExternalRef, HumanReviewRecord, Project, Repository, Task, TaskStatus};

use super::bundle::{
    Payload, PortableHumanReview, PortableProject, PortableRepository, PortableTask,
};

/// Why a Task blocks an export, or `None` when it can be written.
///
/// Each check reuses the predicate the Task lifecycle itself uses, so an export
/// refuses exactly the states that would make an imported record ambiguous.
pub fn refusal(task: &Task) -> Option<String> {
    let state = match task.status {
        TaskStatus::Queue | TaskStatus::InProgress | TaskStatus::AiReview => Some(format!(
            "it is in {:?}, where a run or review may still be active",
            task.status
        )),
        _ => None,
    };
    state
        .or_else(|| {
            task.worker_recovery_refusal()
                .map(|_| "it has unresolved Worker recovery evidence".into())
        })
        .or_else(|| {
            task.republish_pending_refusal()
                .map(|_| "a restack of its branch is unfinished".into())
        })
        .or_else(|| {
            task.cleanup_in_flight
                .then(|| "a checkout cleanup was interrupted".into())
        })
        .or_else(|| {
            task.local_integration
                .as_ref()
                .filter(|r| r.is_pending())
                .map(|_| "an integration of its approved work is unfinished".into())
        })
        .or_else(|| {
            task.pr_create_unconfirmed
                .as_ref()
                .map(|_| "a pull request creation is unconfirmed".into())
        })
        .or_else(|| {
            task.pr_publication
                .as_ref()
                .filter(|p| {
                    matches!(
                        p.state,
                        PublicationState::Pending {} | PublicationState::Unknown { .. }
                    )
                })
                .map(|_| "a pull request publication is unsettled".into())
        })
        .or_else(|| {
            (task.approved_execution.is_some() || task.new_attempt.is_some())
                .then(|| "an approval to start a run is still pending".into())
        })
}

/// The payload for one Project, or the reason it cannot be exported.
///
/// `tasks` must be the Project's Tasks only. Refusals name every blocked Task,
/// so one retry shows the whole list rather than one Task at a time.
pub fn build_payload(
    project: &Project,
    repository: Option<&Repository>,
    tasks: &[&Task],
) -> Result<Payload, String> {
    let blocked: Vec<String> = {
        let mut lines: Vec<String> = tasks
            .iter()
            .filter_map(|t| refusal(t).map(|why| format!("task {:?} ({}): {why}", t.title, t.id)))
            .collect();
        lines.sort();
        lines
    };
    if !blocked.is_empty() {
        return Err(format!(
            "this Project cannot be exported yet. Let the blocked Tasks settle, then try again: {}",
            blocked.join("; ")
        ));
    }
    // A Project that names a repository the instance does not hold would
    // export as if it had none, silently losing its identity.
    if project.repository_id.is_some() && repository.is_none() {
        return Err(
            "this Project's repository is not registered, so its identity cannot be exported"
                .into(),
        );
    }

    let mut sorted: Vec<&Task> = tasks.to_vec();
    sorted.sort_by_key(|t| (t.sequence_number, t.id));
    Ok(Payload {
        project: PortableProject {
            id: project.id,
            name: project.name.clone(),
            agent_type: project.agent_type.clone(),
            model: project.agent_config.model.clone(),
            base: project.base.clone(),
            created_at: project.created_at,
            updated_at: project.updated_at,
        },
        repository: repository.map(|r| PortableRepository {
            id: r.id,
            remote_url: r.remote_url.as_deref().map(portable_url),
            remote_type: r.remote_type.clone(),
            created_at: r.created_at,
        }),
        tasks: sorted.into_iter().map(portable_task).collect(),
    })
}

fn portable_task(task: &Task) -> PortableTask {
    PortableTask {
        id: task.id,
        title: task.title.clone(),
        description: task.description.clone(),
        status: task.status.clone(),
        model: task.model.clone(),
        provider: task.provider,
        review_mode: task.review_mode,
        reviewer_model: task.reviewer_model.clone(),
        planning_mode: task.planning_mode,
        dependencies: task.dependencies.clone(),
        category: task.category.clone(),
        priority: task.priority.clone(),
        complexity: task.complexity.clone(),
        impact: task.impact.clone(),
        security_severity: task.security_severity.clone(),
        subtasks: task.subtasks.clone(),
        sequence_number: task.sequence_number,
        position: task.position,
        external_refs: task.external_refs.iter().map(portable_ref).collect(),
        github_issue_url: task.github_issue_url.as_deref().map(portable_url),
        gitlab_issue_url: task.gitlab_issue_url.as_deref().map(portable_url),
        linear_ticket_id: task.linear_ticket_id.clone(),
        jira_issue_key: task.jira_issue_key.clone(),
        pr_url: task.pr_url.as_deref().map(portable_url),
        qa_signoff: task.qa_signoff.clone().map(|mut qa| {
            // Agent-written text, like the timeline: scrubbed the same way.
            qa.issues_found = qa
                .issues_found
                .iter()
                .map(|issue| scrub_text(issue))
                .collect();
            qa
        }),
        human_review: portable_review(&task.human_review),
        base_commit: task.base_commit.clone(),
        activity: portable_activity(&task.activity),
        created_at: task.created_at,
        updated_at: task.updated_at,
    }
}

fn portable_review(record: &HumanReviewRecord) -> PortableHumanReview {
    PortableHumanReview {
        arrivals: record.arrivals,
        entries: record.entries.clone(),
    }
}

fn portable_ref(reference: &ExternalRef) -> ExternalRef {
    match reference.clone() {
        ExternalRef::GithubIssue {
            url,
            number,
            repo,
            state,
        } => ExternalRef::GithubIssue {
            url: portable_url(&url),
            number,
            repo,
            state,
        },
        ExternalRef::GithubPr {
            url,
            number,
            repo,
            state,
            merge_commit,
        } => ExternalRef::GithubPr {
            url: portable_url(&url),
            number,
            repo,
            state,
            merge_commit,
        },
        ExternalRef::GitlabIssue { url } => ExternalRef::GitlabIssue {
            url: portable_url(&url),
        },
        other => other,
    }
}

/// Drop what an export must not carry from a Task's timeline: raw provider
/// output and tool calls. Everything else stays, scrubbed of credentials and
/// machine paths.
fn portable_activity(entries: &[ActivityEntry]) -> Vec<serde_json::Value> {
    entries
        .iter()
        .filter(|e| {
            !matches!(
                e.kind,
                ActivityKind::RunResult { .. }
                    | ActivityKind::ToolUsed { .. }
                    | ActivityKind::ToolsOmitted { .. }
            )
        })
        .map(|e| {
            let mut value = serde_json::to_value(e).expect("an activity entry serializes");
            scrub_value(&mut value);
            value
        })
        .collect()
}

fn scrub_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = scrub_text(text),
        serde_json::Value::Array(items) => items.iter_mut().for_each(scrub_value),
        serde_json::Value::Object(fields) => fields.values_mut().for_each(scrub_value),
        _ => {}
    }
}

/// Free text with credentials masked (the shared activity redactor) and
/// absolute machine paths replaced by `<path>`.
///
/// Limit: a path that is not written as a single whitespace-delimited token
/// beginning with `/` (for example one inside a quoted, unspaced sentence
/// fragment) can survive. Free text is user-authored or provider-authored, so
/// this is a best-effort scrub, not a guarantee.
fn scrub_text(text: &str) -> String {
    slashit_activity::redact(text)
        .split(' ')
        .map(|word| {
            if is_machine_path(word) {
                "<path>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_machine_path(word: &str) -> bool {
    let token = word.trim_start_matches(['(', '[', '"', '\'', '`']);
    token.starts_with('/') && token[1..].contains('/') && !token.contains("://")
}

/// A URL without user information, query or fragment. Two spellings of one
/// repository compare equal after this, so the same function also gives the
/// identity used for conflict checks.
pub fn portable_url(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let Some((scheme, rest)) = without_query.split_once("://") else {
        return without_query.to_string();
    };
    let end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}{path}")
}

/// Write a bundle to a new file, owner-readable only.
///
/// Refuses an existing path of any kind and a missing parent directory. The
/// bytes are staged beside the destination and then linked into place, so
/// readers see either nothing or the complete file. The link refuses an
/// existing path atomically, so a file another process creates after staging
/// is never replaced. Nothing else in the store is touched.
pub fn write_bundle(path: &Path, bytes: &[u8]) -> Result<bool, String> {
    write_bundle_with(path, bytes, || {})
}

/// [`write_bundle`] with a hook that runs after the bytes are staged and right
/// before they are published: the window in which a competing file would be
/// replaced. Tests use the hook to create that file deterministically.
pub(super) fn write_bundle_with(
    path: &Path,
    bytes: &[u8],
    before_publish: impl FnOnce(),
) -> Result<bool, String> {
    if !path.is_absolute() {
        return Err("the export path must be absolute".into());
    }
    let parent = path.parent().ok_or("the export path has no directory")?;
    if !parent.is_dir() {
        return Err(format!("the directory {} does not exist", parent.display()));
    }
    if exists(path) {
        return Err(already_exists(path));
    }
    let staged =
        slashit_durable_fs::stage(path, bytes, Permissions::OwnerOnly).map_err(describe)?;
    before_publish();
    match staged.publish_new() {
        Ok(committed) => Ok(committed.is_fully_durable()),
        // The file is in place and readable; only its directory entry is
        // unconfirmed. Reporting that as a failure would invite a retry that
        // the existing file then refuses.
        Err(error) if error.is_published() => Ok(false),
        Err(error) if error.io_error().kind() == std::io::ErrorKind::AlreadyExists => {
            Err(already_exists(path))
        }
        Err(error) => Err(describe(error)),
    }
}

fn already_exists(path: &Path) -> String {
    format!("{} already exists; choose another path", path.display())
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn describe(error: WriteError) -> String {
    format!("could not write the export: {error}")
}
