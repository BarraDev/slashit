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
    /// The folder this preview describes.
    pub path: String,
    pub vcs: Vcs,
    pub action: Option<String>,
    pub blocked: Option<String>,
    pub files: usize,
    pub sample: Vec<String>,
    pub git_identity_missing: Option<String>,
    pub jj_available: bool,
    pub jj_blocked: Option<String>,
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
            VcsInitKind::Jujutsu => self.jj_blocked.clone(),
        }
    }
}

/// What Create Project asks the backend to initialize for the folder
/// `submitted`: `(kind, number of files shown)`, or nothing.
///
/// Initializing is only ever what the person was shown: a preview of a
/// different folder than the one submitted (the location changed after it
/// was taken, or before a new one came back) is not consent for this one,
/// and is refused rather than quietly dropped.
pub fn initialization_for(
    preview: Option<&InitPreview>,
    submitted: &str,
    chosen: bool,
    kind: VcsInitKind,
) -> Result<Option<(VcsInitKind, usize)>, String> {
    if !chosen {
        return Ok(None);
    }
    match preview {
        Some(p) if p.path == submitted && p.action.is_some() && p.refusal(kind).is_none() => {
            Ok(Some((kind, p.files)))
        }
        Some(p) if p.path == submitted => Ok(None),
        _ => Err(
            "The folder changed since SlashIt showed what initializing would do. Check the \
             folder again (press Browse, or leave the Location field) before creating the project."
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(path: &str) -> InitPreview {
        InitPreview {
            path: path.to_string(),
            vcs: Vcs::None,
            action: Some("Create a repository".to_string()),
            blocked: None,
            files: 3,
            sample: vec![],
            git_identity_missing: None,
            jj_available: true,
            jj_blocked: Some("big.bin is too large".to_string()),
        }
    }

    #[test]
    fn initialization_is_bound_to_the_previewed_folder() {
        let shown = preview("/work/a");
        assert_eq!(
            initialization_for(Some(&shown), "/work/a", true, VcsInitKind::Git),
            Ok(Some((VcsInitKind::Git, 3)))
        );
        // Another folder submitted, or no preview back yet: refused.
        assert!(initialization_for(Some(&shown), "/work/b", true, VcsInitKind::Git).is_err());
        assert!(initialization_for(None, "/work/a", true, VcsInitKind::Git).is_err());
        // Not chosen: nothing, whatever the preview.
        assert_eq!(
            initialization_for(None, "/work/b", false, VcsInitKind::Git),
            Ok(None)
        );
        // A kind the preview refuses is not initialized.
        assert_eq!(
            initialization_for(Some(&shown), "/work/a", true, VcsInitKind::Jujutsu),
            Ok(None)
        );
    }
}
