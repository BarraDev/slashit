// SPDX-License-Identifier: MPL-2.0
//! The three portability operations over the running instance's state.
//!
//! The IPC handlers and the desktop commands both call these methods, so the
//! lock order, the path rules and the file reads are written once. Callers
//! supply the state and nothing else; they add only the transport and its
//! presentation.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::config::Storage;
use crate::domain::{Project, Repository, Task};
use crate::lifecycle::{ProjectLifecycleGate, TaskLifecycleLocks, Tasks};

use super::apply::{self, ApplyError, ApplyReport};
use super::bundle;
use super::export;
use super::preview::{self, Report};

/// The instance state the three operations read or write.
pub struct Live<'a> {
    pub gate: &'a ProjectLifecycleGate,
    pub leases: &'a TaskLifecycleLocks,
    pub tasks: &'a Tasks,
    pub projects: &'a RwLock<HashMap<Uuid, Project>>,
    pub repositories: &'a RwLock<HashMap<Uuid, Repository>>,
    pub storage: &'a Storage,
}

/// What an export wrote. Serialized field names are part of the IPC reply.
#[derive(Debug, Serialize)]
pub struct ExportOutcome {
    pub path: String,
    pub project_id: String,
    pub tasks: usize,
    pub bytes: usize,
    /// False when the new file may not survive a power loss. It is in place.
    pub durable: bool,
}

/// The path must name a file, and it must be absolute: the instance resolves
/// it from its own working directory otherwise.
fn absolute(path: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("the export path must be absolute".to_string())
    }
}

impl Live<'_> {
    /// Write one Project's portable export to a new file.
    ///
    /// The Project's lifecycle gate and every Task lease are held only while the
    /// records are read, so no Task transition runs mid-snapshot. They are
    /// released before the file is written.
    ///
    /// `project_id` is echoed as the caller wrote it, so the reply and the
    /// not-found message name what the caller asked for.
    pub async fn export(&self, project_id: &str, destination: &str) -> Result<ExportOutcome, String> {
        let id = Uuid::parse_str(project_id).map_err(|_| format!("Invalid project_id: {project_id}"))?;
        let target = absolute(destination)?;
        let gate = self.gate.read(id).await;
        let leases =
            crate::lifecycle::hold_project_task_leases(self.leases, self.tasks, id).await?;
        // Lock order: Tasks, Projects, Repositories (see `portability::apply`).
        let built = {
            let tasks = self.tasks.read().await;
            let projects = self.projects.read().await;
            let repositories = self.repositories.read().await;
            match projects.get(&id) {
                None => Err(format!("Project {project_id} not found")),
                Some(project) => {
                    let repository = project.repository_id.and_then(|id| repositories.get(&id));
                    let mine: Vec<&Task> = tasks
                        .values()
                        .filter(|t| t.project_id == id)
                        .collect();
                    export::build_payload(project, repository, &mine)
                }
            }
        };
        drop(leases);
        drop(gate);
        let payload = built?;
        let bytes = bundle::encode(&payload, chrono::Utc::now(), env!("CARGO_PKG_VERSION"))?;
        let durable = export::write_bundle(&target, &bytes)?;
        Ok(ExportOutcome {
            path: destination.to_string(),
            project_id: project_id.to_string(),
            tasks: payload.tasks.len(),
            bytes: bytes.len(),
            durable,
        })
    }

    /// Validate an export file and describe what importing it would do. Reads
    /// the file and the instance's memory; nothing is written.
    pub async fn preview(&self, path: &str) -> Result<Report, String> {
        let source = absolute(path)?;
        let bytes = bundle::read_bundle_file(&source)?;
        // Lock order: Tasks, Projects, Repositories, as in Apply and Project creation.
        let tasks = self.tasks.read().await;
        let projects = self.projects.read().await;
        let repositories = self.repositories.read().await;
        let target = preview::Target {
            projects: &projects,
            repositories: &repositories,
            tasks: &tasks,
            unreadable_boards: self.storage.task_board_diagnostics().len(),
        };
        preview::preview(&bytes, &target).map_err(|e| e.to_string())
    }

    /// Create the Project of an export file, when the confirmed checksum matches
    /// its payload. The apply validates again under the locks it commits with.
    pub async fn apply(&self, path: &str, checksum: &str) -> Result<ApplyReport, ApplyError> {
        let source = absolute(path).map_err(ApplyError::File)?;
        let bytes = bundle::read_bundle_file(&source).map_err(ApplyError::File)?;
        let live = apply::Live {
            gate: self.gate,
            tasks: self.tasks,
            projects: self.projects,
            repositories: self.repositories,
            storage: self.storage,
        };
        apply::apply(live, &bytes, checksum).await
    }
}
