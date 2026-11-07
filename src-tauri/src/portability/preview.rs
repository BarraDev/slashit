// SPDX-License-Identifier: MPL-2.0
//! Read-only import preview: validate a bundle and compare it with the target.
//!
//! The target is the running instance's in-memory state, passed in by the
//! caller. Preview reads nothing from disk and writes nothing anywhere, so it
//! cannot run the config salvage path (`Storage::load_config` can move a
//! damaged config file aside) or any board load with side effects. It creates
//! no Project, Repository or Task, and it never makes an approval current.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::domain::{Project, Repository, Task, TaskStatus};

use super::bundle::{self, BundleError, OMITTED};
use super::export;

/// The target store as the running instance holds it.
pub struct Target<'a> {
    pub projects: &'a HashMap<Uuid, Project>,
    pub repositories: &'a HashMap<Uuid, Repository>,
    pub tasks: &'a HashMap<Uuid, Task>,
    /// Boards the instance could not read when it started. A Task in one of
    /// them could share an identity with the bundle, so the preview reports
    /// that it cannot rule a conflict out, rather than reporting none.
    pub unreadable_boards: usize,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub format_version: u32,
    pub build: String,
    pub created_at: DateTime<Utc>,
    pub project: ProjectSummary,
    pub repository: Option<RepositorySummary>,
    pub counts: Counts,
    /// The payload checksum the apply must name back to import this exact file.
    pub checksum: String,
    /// Identity collisions. Any entry here means the import would be refused.
    pub conflicts: Vec<Conflict>,
    /// Historical state the import must neutralize before anything can act on it.
    pub normalization: Vec<Normalization>,
    /// Git object identifiers carried as text. They name objects in the
    /// exporting repository and are not recreated or checked here.
    pub git_references: Vec<GitReference>,
    /// What the bundle deliberately leaves out.
    pub excluded: Vec<&'static str>,
    /// True when the bundle is valid and no identity collides.
    pub importable: bool,
}

#[derive(Debug, Serialize)]
pub struct ProjectSummary {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct RepositorySummary {
    pub id: Uuid,
    pub remote_url: Option<String>,
    /// Always true for v1: the exporter never writes a local path, so the
    /// importing user must choose the local checkout.
    pub path_remap_required: bool,
}

#[derive(Debug, Serialize)]
pub struct Counts {
    pub tasks: u32,
    pub dependencies: u32,
    pub activity_entries: u32,
    pub review_entries: u32,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Conflict {
    pub kind: &'static str,
    pub id: String,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct Normalization {
    pub task_id: Uuid,
    pub reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct GitReference {
    pub task_id: Uuid,
    pub field: &'static str,
    pub commit: String,
}

/// Validate `bytes` and describe what importing it would do.
///
/// `Err` means the bundle is not one this build can read at all. `Ok` with
/// `importable: false` means it is valid but collides with the target.
pub fn preview(bytes: &[u8], target: &Target<'_>) -> Result<Report, BundleError> {
    let decoded = bundle::decode(bytes)?;
    let payload = &decoded.payload;
    let mut conflicts = Vec::new();

    if target.projects.contains_key(&payload.project.id) {
        conflicts.push(Conflict {
            kind: "project_id",
            id: payload.project.id.to_string(),
            detail: "a Project with this id already exists in the target".into(),
        });
    }
    if let Some(repository) = &payload.repository {
        if target.repositories.contains_key(&repository.id) {
            conflicts.push(Conflict {
                kind: "repository_id",
                id: repository.id.to_string(),
                detail: "a Repository with this id already exists in the target".into(),
            });
        }
        if let Some(url) = &repository.remote_url {
            let clash = target.repositories.values().any(|r| {
                r.remote_url.as_deref().map(export::portable_url).as_deref() == Some(url.as_str())
            });
            if clash {
                conflicts.push(Conflict {
                    kind: "repository_remote",
                    id: url.clone(),
                    detail: "a Repository with this remote is already registered in the target"
                        .into(),
                });
            }
        }
    }
    for task in &payload.tasks {
        if target.tasks.contains_key(&task.id) {
            conflicts.push(Conflict {
                kind: "task_id",
                id: task.id.to_string(),
                detail: "a Task with this id already exists in the target".into(),
            });
        }
    }
    if target.unreadable_boards > 0 {
        conflicts.push(Conflict {
            kind: "unreadable_target_board",
            id: target.unreadable_boards.to_string(),
            detail: "some target Task boards could not be read, so conflicts cannot be ruled out"
                .into(),
        });
    }

    let mut normalization = Vec::new();
    let mut git_references = Vec::new();
    for task in &payload.tasks {
        if matches!(task.status, TaskStatus::HumanReview) {
            normalization.push(Normalization {
                task_id: task.id,
                reason: "human_review_without_checkout",
            });
        }
        if task.human_review.has_current_approval() {
            normalization.push(Normalization {
                task_id: task.id,
                reason: "approval_current_in_bundle",
            });
        }
        if let Some(commit) = &task.base_commit {
            git_references.push(GitReference {
                task_id: task.id,
                field: "base_commit",
                commit: commit.clone(),
            });
        }
        for entry in &task.human_review.entries {
            if let Some(tip) = &entry.approved_tip {
                git_references.push(GitReference {
                    task_id: task.id,
                    field: "approved_tip",
                    commit: tip.clone(),
                });
            }
        }
    }

    let manifest = payload.manifest();
    let importable = conflicts.is_empty();
    Ok(Report {
        format_version: bundle::FORMAT_VERSION,
        build: decoded.build,
        created_at: decoded.created_at,
        checksum: decoded.checksum,
        project: ProjectSummary {
            id: payload.project.id,
            name: payload.project.name.clone(),
        },
        repository: payload.repository.as_ref().map(|r| RepositorySummary {
            id: r.id,
            remote_url: r.remote_url.clone(),
            path_remap_required: true,
        }),
        counts: Counts {
            tasks: manifest.tasks,
            dependencies: manifest.dependencies,
            activity_entries: manifest.activity_entries,
            review_entries: manifest.review_entries,
        },
        conflicts,
        normalization,
        git_references,
        excluded: OMITTED.to_vec(),
        importable,
    })
}
