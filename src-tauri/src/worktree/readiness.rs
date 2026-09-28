//! Whether a project can run tasks, and whether it can deliver them to a
//! remote, as two separate answers.
//!
//! Local execution needs a supported repository with a base (see
//! [`super::default_base`]). Remote delivery additionally needs `origin`,
//! and is reported beside it, never as a condition of it. Everything here
//! only reads.

use super::default_base::{resolve_default_base, ResolvedBase};
use super::project_base::{self, Proposal};
use super::remote_head::{remote_state, RemoteState};
use super::vcs::{self, Head, Vcs};
use crate::domain::ProjectBase;
use serde::Serialize;
use std::path::Path;

/// What a project's repository settings show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Readiness {
    pub path: String,
    pub vcs: Vcs,
    pub has_commits: bool,
    pub head: Head,
    /// Origin, as known locally. `NoOrigin` too for a folder without Git.
    pub remote: RemoteState,
    /// The project's recorded local base branch.
    pub project_base: Option<String>,
    /// Where a task started now would start. `None` exactly when
    /// [`Self::blocked`] says why not.
    pub base: Option<ResolvedBase>,
    pub blocked: Option<String>,
    /// For a project with no recorded base: the branch that could be
    /// captured unambiguously, offered for an explicit confirmation.
    pub proposal: Option<String>,
    /// Why nothing could be proposed, when nothing could.
    pub proposal_reason: Option<String>,
    /// Local branches that can be chosen as the base, task branches aside.
    pub local_branches: Vec<String>,
    pub jj_available: bool,
}

/// Report on the repository at `repo_path` for a project whose recorded
/// base is `project_base`.
pub async fn readiness(
    repo_path: &str,
    project_base: Option<&ProjectBase>,
) -> Result<Readiness, String> {
    let repo = Path::new(repo_path);
    let vcs = vcs::detect(repo).await?;
    let supported = vcs.supports_task_checkouts();
    let (has_commits, head, remote, local_branches) = if supported {
        (
            vcs::has_commits(repo).await,
            vcs::head(repo).await,
            remote_state(repo_path).await,
            vcs::local_branches(repo).await,
        )
    } else {
        (false, Head::Unknown, RemoteState::NoOrigin, Vec::new())
    };
    let (base, blocked) = match resolve_default_base(repo_path, project_base).await {
        Ok(base) => (Some(base), None),
        Err(why) => (None, Some(why)),
    };
    let (proposal, proposal_reason) = match (project_base, supported) {
        (None, true) => match project_base::propose(repo_path).await? {
            Proposal::Branch(branch) => (Some(branch), None),
            Proposal::Undecided(why) => (None, Some(why)),
        },
        _ => (None, None),
    };
    Ok(Readiness {
        path: repo_path.to_string(),
        vcs,
        has_commits,
        head,
        remote,
        project_base: project_base.map(|b| b.branch().to_string()),
        base,
        blocked,
        proposal,
        proposal_reason,
        local_branches,
        jj_available: vcs::jj_available().await,
    })
}
