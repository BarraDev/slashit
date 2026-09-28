//! Mirrors of the backend's repository readiness types
//! (`worktree::readiness`, `worktree::vcs_init`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Vcs {
    Missing,
    None,
    InsideRepository { root: String },
    Git,
    JjColocated,
    JjNotColocated,
}

impl Vcs {
    pub fn label(&self) -> &'static str {
        match self {
            Vcs::Missing => "Folder not found",
            Vcs::None => "No version control",
            Vcs::InsideRepository { .. } => "Inside another repository",
            Vcs::Git => "Git",
            Vcs::JjColocated => "Jujutsu (colocated with Git)",
            Vcs::JjNotColocated => "Jujutsu (not colocated with Git)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Head {
    Branch { branch: String },
    Unborn { branch: String },
    Detached,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteState {
    NoOrigin,
    DefaultBranch { branch: String },
    DefaultBranchUnknown,
    DefaultBranchUnusable { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseSource {
    Remote,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ResolvedBase {
    pub branch: String,
    pub commit: String,
    pub source: BaseSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Readiness {
    pub path: String,
    pub vcs: Vcs,
    pub has_commits: bool,
    pub head: Head,
    pub remote: RemoteState,
    pub project_base: Option<String>,
    pub base: Option<ResolvedBase>,
    pub blocked: Option<String>,
    pub proposal: Option<String>,
    pub proposal_reason: Option<String>,
    pub local_branches: Vec<String>,
    pub jj_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VcsInitKind {
    Git,
    Jujutsu,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InitPreview {
    pub vcs: Vcs,
    pub action: Option<String>,
    pub blocked: Option<String>,
    pub files: usize,
    pub sample: Vec<String>,
    pub git_identity_missing: Option<String>,
    pub jj_available: bool,
    pub jj_identity_missing: Option<String>,
}

impl InitPreview {
    /// Why initializing with `kind` cannot work here, if it cannot.
    pub fn refusal(&self, kind: VcsInitKind) -> Option<String> {
        if let Some(blocked) = &self.blocked {
            return Some(blocked.clone());
        }
        match kind {
            VcsInitKind::Git => self.git_identity_missing.clone(),
            VcsInitKind::Jujutsu if !self.jj_available => {
                Some("Jujutsu (jj) is not installed.".to_string())
            }
            VcsInitKind::Jujutsu if self.vcs != Vcs::None => {
                Some("This folder is already a Git repository; initialize it with Git.".to_string())
            }
            VcsInitKind::Jujutsu => self.jj_identity_missing.clone(),
        }
    }
}
