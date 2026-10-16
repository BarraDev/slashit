//! Reading a recovered Task Checkout for a new coding attempt.
//!
//! Authorizing a new attempt and launching it both read the same facts here,
//! and [`confirm`] compares them with the decision the person made. Nothing in
//! this module writes to the checkout or to the repository. What it proves is
//! true at the moment it reads. A Git or file writer outside SlashIt is not
//! stopped by the lifecycle lease. The launch repeats the read after the
//! checkout is attached and before the run is recorded, but the uncommitted work
//! can still change between that read and the provider's first action. The
//! contract records this as a point-in-time limitation.

use std::fs;
use std::path::Path;

use crate::domain::task::NewAttemptDecision;
use crate::queue::executor::{CheckoutRefusal, TaskExecutor, WorkerCommitProof};

use super::checkout_fingerprint;

/// The facts of a Task Checkout read for a new attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckoutInspection {
    /// The commit HEAD names, which is also the tip of the branch.
    pub head: String,
    /// The content-aware fingerprint of the uncommitted work.
    pub dirty_fingerprint: String,
    /// The uncommitted paths, in the order the fingerprint uses.
    pub dirty_paths: Vec<String>,
}

/// Reads `checkout` as the registered checkout of `branch` in `repo`.
///
/// Refuses unless the checkout is registered to `branch`, sits on that branch,
/// and its HEAD is the branch tip. A dirty checkout is accepted, because
/// preserving uncommitted work is the point of a new attempt; the fingerprint
/// records it.
pub(crate) async fn inspect(
    repo: &str,
    checkout: &str,
    branch: &str,
) -> Result<CheckoutInspection, String> {
    let fingerprint = checkout_fingerprint::fingerprint(Path::new(checkout))
        .await
        .map_err(|e| e.to_string())?;
    let expected_ref = format!("refs/heads/{branch}");
    if fingerprint.branch_ref.as_deref() != Some(expected_ref.as_str()) {
        return Err(format!(
            "The Task Checkout is not on branch {branch}, so it cannot be built on."
        ));
    }
    match TaskExecutor::verify_worker_checkout(
        repo,
        checkout,
        branch,
        &fingerprint.head,
        WorkerCommitProof::ExactTip,
    )
    .await
    {
        Ok(()) => {}
        // Registered and on the tip, but with uncommitted work: the case this
        // module exists for.
        Err(CheckoutRefusal::Dirty) => {}
        Err(refusal) => return Err(refusal.to_string()),
    }
    Ok(CheckoutInspection {
        head: fingerprint.head,
        dirty_fingerprint: fingerprint.digest,
        dirty_paths: fingerprint.dirty_paths,
    })
}

/// The identity of the checkout's own `.git` entry, which a replaced or
/// re-created checkout cannot keep. `None` when it cannot be read.
pub(crate) fn git_entry_identity(checkout: &str) -> Option<String> {
    let meta = fs::metadata(Path::new(checkout).join(".git")).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("{}:{}", meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        meta.created().ok().map(|created| format!("{created:?}"))
    }
}

/// Why a new attempt is refused when the board's directory cannot be synchronized.
pub(crate) const BOARD_NOT_SYNCABLE: &str =
    "This filesystem cannot synchronize the directory that holds the Task board, so SlashIt \
     cannot confirm that a new attempt survives a power loss. Start new attempts on a \
     filesystem that supports directory synchronization.";

/// Fails closed when the board for `project_id` cannot be confirmed durable.
/// A new attempt is recorded and launched only when this holds: otherwise a
/// crash could lose the record of the launch while its provider still runs.
pub(crate) fn board_can_confirm(
    storage: &crate::config::Storage,
    project_id: uuid::Uuid,
) -> Result<(), String> {
    match storage.board_directory_sync(project_id) {
        Ok(slashit_durable_fs::DirectorySync::Synced) => Ok(()),
        Ok(slashit_durable_fs::DirectorySync::Unsupported) => Err(BOARD_NOT_SYNCABLE.into()),
        Err(error) => Err(format!("{BOARD_NOT_SYNCABLE} ({error:#})")),
    }
}

/// Whether the checkout still matches what the person was shown for `decision`.
pub(crate) fn confirm(
    decision: &NewAttemptDecision,
    inspection: &CheckoutInspection,
) -> Result<(), String> {
    if inspection.head != decision.expected_head {
        return Err(
            "The branch moved after you inspected it. Inspect the Task again and decide again."
                .into(),
        );
    }
    if inspection.dirty_fingerprint != decision.dirty_fingerprint {
        return Err(
            "The uncommitted work changed after you inspected it. Inspect the Task again and \
             decide again."
                .into(),
        );
    }
    Ok(())
}
