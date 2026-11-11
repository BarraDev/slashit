// SPDX-License-Identifier: MPL-2.0
//! Durable evidence that an AI Review fix run may have edited a Task Checkout.
//!
//! The record is written before the fix agent starts and removed only when the
//! checkout is known to match its baseline or the fix was committed. A record
//! that survives an interruption, a failure or a cancellation refuses every
//! later Worker commit until a Human decides (see
//! `docs/architecture/task-checkouts-and-stacks.md`, "Failed review-fix runs").
//!
//! It lives in the checkout's Git directory: Git never commits that directory,
//! older builds ignore the file, and the persisted Task gains no field.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const FILE_NAME: &str = "slashit-review-fix.json";
const VERSION: u32 = 1;

/// Why a Worker commit is refused while a record is unresolved.
pub const UNRESOLVED_REFUSAL: &str = "An AI Review fix run ended with its edits unresolved. \
    The Task Checkout may hold partial edits from that fix. They were preserved and not \
    committed; a Human decision is needed before the Task's work can be committed.";

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewFixRecord {
    pub version: u32,
    pub task_id: Uuid,
    /// The checkout's content before the fix agent ran; see `CheckoutSnapshot::content`.
    pub baseline_content: String,
    pub started_at: DateTime<Utc>,
}

async fn record_path(working_dir: &str) -> Result<PathBuf, String> {
    let output = crate::host_process::tokio_command("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(working_dir)
        .output()
        .await
        .map_err(|e| format!("could not locate the checkout's Git directory: {e}"))?;
    if !output.status.success() {
        return Err("could not locate the checkout's Git directory".to_string());
    }
    let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(git_dir).join(FILE_NAME))
}

/// Writes the record before a fix agent may edit. Refuses when a record already
/// exists: an earlier unresolved fix's baseline must never be overwritten.
pub async fn begin(working_dir: &str, task_id: Uuid, baseline_content: &str) -> Result<(), String> {
    let path = record_path(working_dir).await?;
    if path.exists() {
        return Err(
            "an earlier AI Review fix run is still unresolved in this checkout".to_string(),
        );
    }
    let record = ReviewFixRecord {
        version: VERSION,
        task_id,
        baseline_content: baseline_content.to_string(),
        started_at: Utc::now(),
    };
    let bytes = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
    slashit_durable_fs::write_atomic(&path, &bytes, slashit_durable_fs::Permissions::Preserve)
        .map_err(|e| format!("the record could not be made durable: {e}"))?;
    Ok(())
}

/// The unresolved record, if any. An unreadable record is an error, so commits
/// fail closed rather than proceed.
pub async fn pending(working_dir: &str) -> Result<Option<ReviewFixRecord>, String> {
    let path = record_path(working_dir).await?;
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("the review-fix record is unreadable: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("the review-fix record could not be read: {e}")),
    }
}

/// Removes the record. Only the run that wrote it may call this.
pub async fn clear(working_dir: &str) -> Result<(), String> {
    let path = record_path(working_dir).await?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("the review-fix record could not be removed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(status.success());
        dir
    }

    #[tokio::test]
    async fn a_record_round_trips_and_is_cleared() {
        let dir = repo();
        let path = dir.path().to_str().unwrap();
        let task = Uuid::new_v4();
        assert_eq!(pending(path).await.unwrap(), None);
        begin(path, task, "content-a").await.unwrap();
        let record = pending(path).await.unwrap().expect("record present");
        assert_eq!(record.task_id, task);
        assert_eq!(record.baseline_content, "content-a");
        clear(path).await.unwrap();
        assert_eq!(pending(path).await.unwrap(), None);
    }

    #[tokio::test]
    async fn begin_never_overwrites_an_unresolved_baseline() {
        let dir = repo();
        let path = dir.path().to_str().unwrap();
        begin(path, Uuid::new_v4(), "first").await.unwrap();
        assert!(begin(path, Uuid::new_v4(), "second").await.is_err());
        assert_eq!(
            pending(path).await.unwrap().unwrap().baseline_content,
            "first"
        );
    }

    #[tokio::test]
    async fn an_unreadable_record_fails_closed() {
        let dir = repo();
        let path = dir.path().to_str().unwrap();
        begin(path, Uuid::new_v4(), "x").await.unwrap();
        let file = record_path(path).await.unwrap();
        std::fs::write(&file, b"not json").unwrap();
        assert!(pending(path).await.is_err());
    }
}
