// SPDX-License-Identifier: MPL-2.0
//! Whether a project can run tasks, and whether it can deliver them to a
//! remote, as two separate answers.
//!
//! Local execution needs a supported repository with a base (see
//! [`super::default_base`]). Remote delivery additionally needs `origin`,
//! and is reported beside it, never as a condition of it. Everything here
//! only reads.

use super::default_base::{
    local_branch_commit, remote_base_awaiting_fetch, resolve_base_as_fetched, resolve_remote_base, ResolvedBase,
};
use super::restack::exact_ref;
use super::project_base::{self, Proposal};
use super::remote_head::{remote_state, RemoteState};
use super::vcs::{self, Head, Vcs};
use crate::domain::{BaseSync, ProjectBase};
use serde::Serialize;
use std::path::Path;

/// Where the project's local base branch stands against origin's, as the
/// repository last saw origin. Nothing is fetched to learn it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BaseComparison {
    /// Both name the same commit.
    Same,
    /// The local branch has commits origin's lacks.
    LocalAhead { commits: u32 },
    /// Origin's has commits the local branch lacks.
    RemoteAhead { commits: u32 },
    /// Each has commits the other lacks.
    Diverged { local_only: u32, remote_only: u32 },
}

/// The project's local base branch against the remote-tracking branch it
/// would otherwise be replaced by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BaseStanding {
    /// The local base branch.
    pub local_branch: String,
    /// The remote-tracking branch compared: origin's default branch, or
    /// `origin/<local branch>` when origin names no default.
    pub remote_branch: String,
    pub comparison: BaseComparison,
}

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
    /// Whether a person chose that local branch over origin's default
    /// branch. When false, origin's default branch is used wherever the
    /// repository names one, as it always was.
    pub prefers_local: bool,
    /// Whether new Task Checkouts start from origin's default branch fetched
    /// just before (`FetchRemote`), or from the refs as they are (`Local`).
    pub base_sync: BaseSync,
    /// The local base branch against origin's, when both exist.
    pub standing: Option<BaseStanding>,
    /// Where a task started now would start. `None` exactly when
    /// [`Self::blocked`] says why not, or [`Self::pending_fetch`] names the
    /// branch the start will fetch.
    pub base: Option<ResolvedBase>,
    pub blocked: Option<String>,
    /// Under the latest remote base: origin's default branch, named but not
    /// fetched yet. A task can still start (its start fetches it), so this
    /// is pending synchronization, not [`Self::blocked`]; [`Self::base`] is
    /// `None` until it has been fetched.
    pub pending_fetch: Option<String>,
    /// For a project with no recorded base: the branch that can be told
    /// (or only suggested, see [`Self::proposal_reason`]), offered for an
    /// explicit confirmation.
    pub proposal: Option<String>,
    /// Why nothing could be proposed, or why the proposal is only a
    /// suggestion.
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
    base_sync: BaseSync,
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
    // Nothing is fetched to report this: under `FetchRemote` it is origin's
    // default branch as last fetched.
    let (base, blocked, pending_fetch) = match resolve_base_as_fetched(repo_path, project_base, base_sync).await {
        Ok(base) => (Some(base), None, None),
        Err(why) => match base_sync {
            BaseSync::FetchRemote => match remote_base_awaiting_fetch(repo_path).await {
                Ok(Some(branch)) => (None, None, Some(branch)),
                // Pending-ness could not be read (or the branch is there
                // and something else refused it): the refusal stands as
                // reported, never hidden behind a pending answer.
                Ok(None) | Err(_) => (None, Some(why), None),
            },
            BaseSync::Local => (None, Some(why), None),
        },
    };
    let (proposal, proposal_reason) = match (project_base, supported) {
        (None, true) => match project_base::propose(repo_path).await? {
            Proposal::Branch(branch) => (Some(branch), None),
            Proposal::Suggested { branch, why } => (Some(branch), Some(why)),
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
        prefers_local: project_base.is_some_and(ProjectBase::prefers_local),
        base_sync,
        standing: if supported { standing(repo, repo_path, project_base).await } else { None },
        base,
        blocked,
        pending_fetch,
        proposal,
        proposal_reason,
        local_branches,
        jj_available: vcs::jj_available().await,
    })
}

/// How the project's local base branch compares with origin's. `None` when
/// there is no local base, no remote-tracking branch to compare, or Git
/// cannot tell.
async fn standing(repo: &Path, repo_path: &str, project_base: Option<&ProjectBase>) -> Option<BaseStanding> {
    let local_branch = project_base?.branch().to_string();
    let local = local_branch_commit(repo, &local_branch).await.ok().flatten()?;
    let (remote_branch, remote) = match resolve_remote_base(repo, repo_path).await {
        Ok(Some(base)) => (base.branch, base.commit),
        _ => {
            let tip = exact_ref(repo, &format!("refs/remotes/origin/{local_branch}")).await.ok().flatten()?;
            (local_branch.clone(), tip)
        }
    };
    let comparison = if local == remote {
        BaseComparison::Same
    } else {
        let local_only = count(repo, &remote, &local).await?;
        let remote_only = count(repo, &local, &remote).await?;
        match (local_only, remote_only) {
            (0, 0) => BaseComparison::Same,
            (commits, 0) => BaseComparison::LocalAhead { commits },
            (0, commits) => BaseComparison::RemoteAhead { commits },
            (local_only, remote_only) => BaseComparison::Diverged { local_only, remote_only },
        }
    };
    Some(BaseStanding { local_branch, remote_branch: format!("origin/{remote_branch}"), comparison })
}

/// How many commits `to` has that `from` does not.
async fn count(repo: &Path, from: &str, to: &str) -> Option<u32> {
    super::integration::count_between(repo, from, to).await.ok()
}
