//! What version control a project's folder has, as far as a Task Checkout is
//! concerned.
//!
//! Every Task Checkout is a Git worktree added from the project's own
//! repository root (`WorktreeManager` runs `git worktree add` there), so a
//! folder is usable only when `git` itself finds a repository whose top level
//! is that folder. A Jujutsu repository qualifies only when it is colocated
//! with Git: one created with `jj git init --no-colocate` keeps its Git store
//! inside `.jj`, and plain `git` commands at its root find no repository.
//! Nothing here writes anything.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// The version control of a project folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Vcs {
    /// The folder is not there, or cannot be read.
    Missing,
    /// Neither Git nor Jujutsu.
    None,
    /// A plain subdirectory of the Git repository at `root`.
    InsideRepository { root: String },
    /// A Git repository root with no Jujutsu repository.
    Git,
    /// A Jujutsu repository colocated with Git at this root.
    JjColocated,
    /// A Jujutsu repository whose Git store is inside `.jj`, so `git` at the
    /// root finds no repository of its own.
    JjNotColocated,
}

impl Vcs {
    /// Whether a Task Checkout (a Git worktree) can be created from this
    /// folder at all, commits and base aside.
    pub fn supports_task_checkouts(&self) -> bool {
        matches!(self, Self::Git | Self::JjColocated)
    }

    /// Why a Task Checkout cannot be created here, in words a person can act
    /// on, or `None` when it can.
    pub fn refusal(&self, path: &str) -> Option<String> {
        match self {
            Self::Git | Self::JjColocated => None,
            Self::Missing => Some(format!(
                "The project folder {path} is not there or cannot be read."
            )),
            Self::None => Some(format!(
                "{path} is not under version control. SlashIt runs every task in its own Git \
                 worktree, so the project needs a Git or Jujutsu repository with at least one \
                 commit. Initialize one from Settings > Repository."
            )),
            Self::InsideRepository { root } => Some(format!(
                "{path} is a folder inside the Git repository at {root}, not its root. Open {root} \
                 as the project instead."
            )),
            Self::JjNotColocated => Some(format!(
                "{path} is a Jujutsu repository that is not colocated with Git: its Git store is \
                 inside .jj, and SlashIt's task checkouts are Git worktrees, which need a Git \
                 repository at the project root. Colocate it with `jj git colocation enable` \
                 (Jujutsu 0.35 or later) in {path}, then try again."
            )),
        }
    }
}

/// Classify `path`. Only errors when a program cannot be started at all.
pub async fn detect(path: &Path) -> Result<Vcs, String> {
    if !path.is_dir() {
        return Ok(Vcs::Missing);
    }
    let has_jj = path.join(".jj").is_dir();
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(path)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("Failed to run git: {e}"))?;
    let toplevel = output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()));
    let is_root = toplevel.as_ref().is_some_and(|top| same_path(top, path));
    Ok(match (has_jj, is_root, toplevel) {
        (true, true, _) => Vcs::JjColocated,
        (true, false, _) => Vcs::JjNotColocated,
        (false, true, _) => Vcs::Git,
        (false, false, Some(root)) => Vcs::InsideRepository {
            root: root
                .canonicalize()
                .unwrap_or(root)
                .to_string_lossy()
                .to_string(),
        },
        (false, false, None) => Vcs::None,
    })
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Where the primary checkout's `HEAD` is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Head {
    /// On the local branch `branch`, which has a commit.
    Branch { branch: String },
    /// On the local branch `branch`, which has no commit yet.
    Unborn { branch: String },
    /// At a commit, on no branch. Normal in a colocated Jujutsu repository.
    Detached,
    /// Git could not say.
    Unknown,
}

/// Read the primary checkout's `HEAD` in the Git repository at `repo`.
pub async fn head(repo: &Path) -> Head {
    let Ok(symbolic) = git_stdout(repo, &["symbolic-ref", "-q", "HEAD"]).await else {
        return match git_stdout(repo, &["rev-parse", "--verify", "-q", "HEAD^{commit}"]).await {
            Ok(_) => Head::Detached,
            Err(_) => Head::Unknown,
        };
    };
    let Some(branch) = symbolic.strip_prefix("refs/heads/") else {
        return Head::Unknown;
    };
    match super::restack::exact_ref(repo, &symbolic).await {
        Ok(Some(_)) => Head::Branch {
            branch: branch.to_string(),
        },
        Ok(None) => Head::Unborn {
            branch: branch.to_string(),
        },
        Err(_) => Head::Unknown,
    }
}

/// Whether the repository at `repo` has any commit reachable from any ref.
pub async fn has_commits(repo: &Path) -> bool {
    git_stdout(repo, &["rev-list", "-n", "1", "--all"])
        .await
        .is_ok_and(|out| !out.is_empty())
}

/// Every local branch, by name, sorted, leaving out task branches SlashIt
/// created (see [`super::looks_like_task_branch`]).
pub async fn local_branches(repo: &Path) -> Vec<String> {
    let Ok(listed) = git_stdout(
        repo,
        &["for-each-ref", "--format=%(refname)", "refs/heads/"],
    )
    .await
    else {
        return Vec::new();
    };
    let mut branches: Vec<String> = listed
        .lines()
        .filter_map(|line| line.strip_prefix("refs/heads/"))
        .filter(|name| !super::looks_like_task_branch(name))
        .map(str::to_string)
        .collect();
    branches.sort();
    branches
}

/// Whether a `jj` program can be started.
pub async fn jj_available() -> bool {
    tokio::process::Command::new("jj")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .is_ok_and(|output| output.status.success())
}

/// `git <args>` in `dir`, its trimmed stdout on success, its stderr otherwise.
pub(super) async fn git_stdout(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = super::registry_lock::run_git_with(
        "git".into(),
        &dir.to_string_lossy(),
        args,
        &[("GIT_TERMINAL_PROMPT", "0".to_string())],
        super::registry_lock::Input::None,
    )
    .await
    .map_err(|e| format!("Failed to run git: {e}"))?
    .output;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
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
    }

    #[tokio::test]
    async fn folders_are_classified_by_what_git_finds_at_their_root() {
        let temp = tempfile::TempDir::new().unwrap();
        let plain = temp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(detect(&plain).await, Ok(Vcs::None));
        assert_eq!(detect(&temp.path().join("gone")).await, Ok(Vcs::Missing));

        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        git(&repo, &["init", "-q"]);
        assert_eq!(detect(&repo).await, Ok(Vcs::Git));
        assert!(matches!(
            detect(&repo.join("sub")).await,
            Ok(Vcs::InsideRepository { .. })
        ));

        // A `.jj` beside a real `.git` is colocated; a `.jj` alone is not.
        std::fs::create_dir_all(repo.join(".jj")).unwrap();
        assert_eq!(detect(&repo).await, Ok(Vcs::JjColocated));
        std::fs::create_dir_all(plain.join(".jj")).unwrap();
        assert_eq!(detect(&plain).await, Ok(Vcs::JjNotColocated));
        assert!(Vcs::JjNotColocated
            .refusal("/p")
            .unwrap()
            .contains("jj git colocation enable"));
    }

    #[tokio::test]
    async fn head_says_attached_unborn_or_detached() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path();
        git(repo, &["init", "-q", "-b", "trunk-xyz"]);
        assert_eq!(
            head(repo).await,
            Head::Unborn {
                branch: "trunk-xyz".to_string()
            }
        );
        assert!(!has_commits(repo).await);
        git(repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        assert_eq!(
            head(repo).await,
            Head::Branch {
                branch: "trunk-xyz".to_string()
            }
        );
        assert!(has_commits(repo).await);
        git(repo, &["checkout", "-q", "--detach"]);
        assert_eq!(head(repo).await, Head::Detached);
    }
}
