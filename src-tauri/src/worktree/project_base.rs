//! Establishing a project's local base branch (`domain::ProjectBase`).
//!
//! A project's base is captured once, when the project is registered or its
//! version control is initialized, and only when the repository says
//! unambiguously which branch it is:
//!
//! - Git: the primary checkout's `HEAD` is symbolically attached to a local
//!   branch that has a commit. When `refs/remotes/origin/HEAD` names a
//!   different branch of origin's, the two disagree and nothing is captured.
//! - Jujutsu colocated with Git, where Git's `HEAD` is normally detached: the
//!   branch JJ's `trunk()` alias names, when it is exactly `<D>` or
//!   `<D>@origin` and the local branch (bookmark) `D` exists: that is JJ's
//!   own configuration saying which line is the trunk. A repository whose
//!   Git `HEAD` is attached is read as Git is. (Initializing with Jujutsu
//!   records the bookmark SlashIt itself just created; see
//!   `commands::repository_setup`.)
//!
//! A colocated JJ repository with exactly one bookmark and no `trunk()`
//! naming it gets that bookmark as a *suggestion*: offered for the person to
//! confirm in the project's repository settings, never captured on its own,
//! since having one bookmark does not make it the trunk.
//!
//! Anything else -- a detached Git `HEAD`, several candidate bookmarks, no
//! commit at all -- is left undecided, with the reason, for the person to
//! choose in the project's repository settings. Nothing here is consulted
//! when a task starts: that is what keeps a switch of the primary checkout
//! from moving where new tasks start. Nothing here writes to the repository.

use super::default_base::{jj_trunk_alias, local_branch_commit, resolve_remote_base};
use super::vcs::{self, Head, Vcs};
use crate::domain::ProjectBase;
use std::path::Path;

/// What [`propose`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proposal {
    /// This local branch is, unambiguously, the project's base.
    Branch(String),
    /// This local branch is the likely base, but only a person can say so;
    /// why it is only suggested.
    Suggested { branch: String, why: String },
    /// No branch can be told; why, in words a person can act on.
    Undecided(String),
}

impl Proposal {
    pub fn into_base(self) -> Option<ProjectBase> {
        match self {
            Self::Branch(branch) => Some(ProjectBase::LocalBranch { branch }),
            Self::Suggested { .. } | Self::Undecided(_) => None,
        }
    }
}

/// Tell the project base of the repository at `repo_path`, if it can be
/// told unambiguously. See the module documentation.
pub async fn propose(repo_path: &str) -> Result<Proposal, String> {
    let repo = Path::new(repo_path);
    let vcs = vcs::detect(repo).await?;
    if let Some(refused) = vcs.refusal(repo_path) {
        return Ok(Proposal::Undecided(refused));
    }
    if !vcs::has_commits(repo).await {
        return Ok(Proposal::Undecided(format!(
            "The repository at {repo_path} has no commits yet, so there is no branch a task could \
             start from."
        )));
    }
    match vcs::head(repo).await {
        Head::Branch { branch } => attached(repo, repo_path, branch).await,
        _ if vcs == Vcs::JjColocated => jj_bookmark(repo).await,
        Head::Detached if vcs::local_branches(repo).await.is_empty() => Ok(Proposal::Undecided(
            "The primary checkout is on a detached HEAD and the repository has no local \
                 branch, so there is nothing a task could start from. Create a branch first (for \
                 example `git switch -c <name>`), then choose it as the project's base."
                .to_string(),
        )),
        Head::Detached => Ok(Proposal::Undecided(
            "The primary checkout is on a detached HEAD, so SlashIt cannot tell which branch is \
             the project's base. Choose it explicitly."
                .to_string(),
        )),
        Head::Unborn { branch } => Ok(Proposal::Undecided(format!(
            "The primary checkout is on {branch}, which has no commit yet. Choose the project's \
             base branch explicitly."
        ))),
        Head::Unknown => Ok(Proposal::Undecided(
            "Git could not say which branch the primary checkout is on. Choose the project's base \
             branch explicitly."
                .to_string(),
        )),
    }
}

/// `HEAD` is on `branch`, which has a commit.
async fn attached(repo: &Path, repo_path: &str, branch: String) -> Result<Proposal, String> {
    if let Err(refused) = super::checked_base_branch(&branch) {
        return Ok(Proposal::Undecided(refused));
    }
    if super::looks_like_task_branch(&branch) {
        return Ok(Proposal::Undecided(format!(
            "The primary checkout is on {branch}, a task's own branch. Choose the project's base \
             branch explicitly."
        )));
    }
    // A malformed origin/HEAD is a problem the readiness report shows on its
    // own; it says nothing about which local branch is the base.
    if let Ok(Some(remote)) = resolve_remote_base(repo, repo_path).await {
        if remote.branch != branch {
            return Ok(Proposal::Undecided(format!(
                "The primary checkout is on {branch}, but origin's default branch is {}. Tasks \
                 start from {}@origin while it can be read; choose a local base branch \
                 explicitly if you want one for when it cannot.",
                remote.branch, remote.branch
            )));
        }
    }
    Ok(Proposal::Branch(branch))
}

/// A colocated JJ repository whose Git `HEAD` is not on a branch.
async fn jj_bookmark(repo: &Path) -> Result<Proposal, String> {
    if let Some(alias) = jj_trunk_alias(repo).await {
        let name = alias.strip_suffix("@origin").unwrap_or(&alias);
        if super::checked_base_branch(name).is_ok()
            && local_branch_commit(repo, name).await?.is_some()
        {
            return Ok(Proposal::Branch(name.to_string()));
        }
    }
    let mut candidates = Vec::new();
    for branch in vcs::local_branches(repo).await {
        if super::checked_base_branch(&branch).is_ok()
            && local_branch_commit(repo, &branch).await?.is_some()
        {
            candidates.push(branch);
        }
    }
    Ok(match candidates.len() {
        1 => {
            let branch = candidates.remove(0);
            Proposal::Suggested {
                why: format!(
                    "{branch} is this Jujutsu repository's only bookmark, but JJ's trunk() alias \
                     does not name it, so SlashIt will not assume it is the base. Confirm it, or \
                     choose another."
                ),
                branch,
            }
        }
        0 => Proposal::Undecided(
            "This Jujutsu repository has no bookmark to start tasks from. Create one (for \
             example `jj bookmark create <name> -r @-`), then choose it as the project's base."
                .to_string(),
        ),
        n => Proposal::Undecided(format!(
            "This Jujutsu repository has {n} bookmarks ({}) and JJ's trunk() alias does not name \
             one of them, so SlashIt cannot tell which is the project's base. Choose it \
             explicitly.",
            candidates.join(", ")
        )),
    })
}

/// Check that `branch` can be the project's base in the repository at
/// `repo_path`, for an explicit choice.
pub async fn validate_choice(repo_path: &str, branch: &str) -> Result<ProjectBase, String> {
    let repo = Path::new(repo_path);
    if let Some(refused) = vcs::detect(repo).await?.refusal(repo_path) {
        return Err(refused);
    }
    let branch = super::checked_base_branch(branch)?;
    if super::looks_like_task_branch(branch) {
        return Err(format!(
            "{branch} is a task's own branch, not a base for other tasks."
        ));
    }
    if local_branch_commit(repo, branch).await?.is_none() {
        return Err(format!(
            "{repo_path} has no local branch {branch} with a commit, so tasks cannot start there."
        ));
    }
    Ok(ProjectBase::LocalBranch {
        branch: branch.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn repo_on(branch: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", branch]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        (temp, repo)
    }

    async fn proposed(repo: &Path) -> Proposal {
        propose(repo.to_str().unwrap()).await.unwrap()
    }

    #[tokio::test]
    async fn an_attached_head_with_a_commit_is_proposed_whatever_its_name() {
        let (_temp, repo) = repo_on("trunk-xyz");
        assert_eq!(
            proposed(&repo).await,
            Proposal::Branch("trunk-xyz".to_string())
        );
    }

    #[tokio::test]
    async fn a_detached_head_or_no_commit_is_left_undecided() {
        let (_temp, repo) = repo_on("trunk-xyz");
        git(&repo, &["checkout", "-q", "--detach"]);
        assert!(
            matches!(proposed(&repo).await, Proposal::Undecided(why) if why.contains("detached"))
        );

        let temp = tempfile::TempDir::new().unwrap();
        git(temp.path(), &["init", "-q"]);
        assert!(
            matches!(proposed(temp.path()).await, Proposal::Undecided(why) if why.contains("no commits"))
        );

        let plain = tempfile::TempDir::new().unwrap();
        assert!(
            matches!(proposed(plain.path()).await, Proposal::Undecided(why) if why.contains("not under version control"))
        );
    }

    /// Checked out on a feature branch while origin's default branch is
    /// another: the two disagree, so nothing is captured.
    #[tokio::test]
    async fn a_checkout_that_disagrees_with_origin_head_is_left_undecided() {
        let (temp, repo) = repo_on("trunk");
        let origin = temp.path().join("origin.git");
        git(&repo, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&repo, &["push", "-q", "origin", "trunk"]);
        git(&repo, &["remote", "set-head", "origin", "trunk"]);
        assert_eq!(proposed(&repo).await, Proposal::Branch("trunk".to_string()));

        git(&repo, &["checkout", "-q", "-b", "feature"]);
        assert!(
            matches!(proposed(&repo).await, Proposal::Undecided(why) if why.contains("origin's default branch is trunk"))
        );
    }

    /// A colocated JJ repository (simulated: `.jj` beside `.git`) with a
    /// detached Git `HEAD` only *suggests* its only bookmark, and refuses to
    /// pick between several. Task branches never count.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_colocated_jj_repository_only_suggests_its_only_bookmark() {
        let _no_jj = crate::test_helpers::FakeProgram::without(&["jj"]).await;
        let (_temp, repo) = repo_on("trunk-xyz");
        std::fs::create_dir_all(repo.join(".jj")).unwrap();
        let tip = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["update-ref", "refs/heads/task-12345678", &tip]);
        git(&repo, &["checkout", "-q", "--detach"]);
        let only = proposed(&repo).await;
        assert!(
            matches!(&only, Proposal::Suggested { branch, .. } if branch == "trunk-xyz"),
            "{only:?}"
        );
        assert_eq!(only.into_base(), None, "a suggestion is never captured");

        git(&repo, &["update-ref", "refs/heads/other", &tip]);
        assert!(
            matches!(proposed(&repo).await, Proposal::Undecided(why) if why.contains("2 bookmarks"))
        );
    }

    /// JJ's own `trunk()` naming a local bookmark is captured.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_colocated_jj_repository_takes_the_bookmark_trunk_names() {
        let _jj =
            crate::test_helpers::FakeProgram::install("jj", "printf 'trunk-xyz@origin\\n'").await;
        let (_temp, repo) = repo_on("trunk-xyz");
        std::fs::create_dir_all(repo.join(".jj")).unwrap();
        let tip = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["update-ref", "refs/heads/other", &tip]);
        git(&repo, &["checkout", "-q", "--detach"]);
        assert_eq!(
            proposed(&repo).await,
            Proposal::Branch("trunk-xyz".to_string())
        );
    }

    #[tokio::test]
    async fn a_detached_head_with_no_branch_says_to_create_one() {
        let (_temp, repo) = repo_on("trunk-xyz");
        git(&repo, &["checkout", "-q", "--detach"]);
        git(&repo, &["branch", "-D", "trunk-xyz"]);
        assert!(
            matches!(proposed(&repo).await, Proposal::Undecided(why) if why.contains("git switch -c"))
        );
    }

    #[tokio::test]
    async fn an_explicit_choice_must_be_an_existing_local_branch() {
        let (_temp, repo) = repo_on("trunk-xyz");
        let path = repo.to_str().unwrap();
        assert_eq!(
            validate_choice(path, "trunk-xyz").await,
            Ok(ProjectBase::LocalBranch {
                branch: "trunk-xyz".to_string()
            })
        );
        assert!(validate_choice(path, "missing").await.is_err());
        assert!(validate_choice(path, "-evil").await.is_err());
        assert!(validate_choice(path, "task-12345678").await.is_err());
    }
}
