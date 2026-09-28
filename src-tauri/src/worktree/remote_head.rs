//! Whether origin's default branch is known locally, and the one explicit
//! action that asks origin for it.
//!
//! Remote delivery (push, pull requests) is optional; running a task is not
//! blocked by anything here. [`detect_default_branch`] is the equivalent of
//! `git remote set-head origin --auto`, run only on a person's explicit
//! request: it reads origin's `HEAD` over the network and writes one local
//! ref, `refs/remotes/origin/HEAD`. It never fetches, pushes, or changes the
//! remote. A plain `git fetch` is not used as the repair because whether it
//! records `refs/remotes/origin/HEAD` depends on the Git version (2.48 and
//! later follow the remote's `HEAD` on fetch by default; 2.43 does not).
//! `set-head --auto` behaves the same on both: it refuses when origin's
//! `HEAD` is detached and ambiguous ("Multiple remote HEAD branches"), when
//! origin's `HEAD` names a branch with no commit ("Cannot determine remote
//! HEAD"), and when the branch it names has not been fetched ("Not a valid
//! ref"), writing nothing in each case.

use super::default_base::{origin_head, resolve_remote_base, OriginHead};
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

/// How long origin gets to answer.
const REMOTE_DEADLINE: Duration = Duration::from_secs(60);

/// What the repository knows locally about origin's default branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteState {
    /// No remote named `origin`. Pull requests are unavailable; tasks are not.
    NoOrigin,
    /// `refs/remotes/origin/HEAD` resolves to origin's `branch`.
    DefaultBranch { branch: String },
    /// `origin` exists, but its default branch is not recorded locally (or
    /// names a branch that has not been fetched).
    DefaultBranchUnknown,
    /// `refs/remotes/origin/HEAD` is there but unusable; why.
    DefaultBranchUnusable { reason: String },
}

/// Read what the repository at `repo_path` knows about origin, locally.
pub async fn remote_state(repo_path: &str) -> RemoteState {
    let repo = Path::new(repo_path);
    if !has_origin(repo).await {
        return RemoteState::NoOrigin;
    }
    match origin_head(repo).await {
        Ok(OriginHead::Missing) => return RemoteState::DefaultBranchUnknown,
        Ok(OriginHead::Elsewhere(target)) => {
            return RemoteState::DefaultBranchUnusable {
                reason: format!(
                    "refs/remotes/origin/HEAD points at {target}, not at a branch of origin."
                ),
            }
        }
        Ok(OriginHead::Branch(_)) => {}
        Err(e) => return RemoteState::DefaultBranchUnusable { reason: e },
    }
    // Without JJ's say: only origin/HEAD itself is reported here.
    match resolve_remote_base(repo, repo_path).await {
        Ok(Some(base)) => RemoteState::DefaultBranch {
            branch: base.branch,
        },
        Ok(None) => RemoteState::DefaultBranchUnknown,
        Err(reason) => RemoteState::DefaultBranchUnusable { reason },
    }
}

async fn has_origin(repo: &Path) -> bool {
    super::vcs::git_stdout(repo, &["config", "--get", "remote.origin.url"])
        .await
        .is_ok()
}

/// Ask origin which branch is its default and record it as
/// `refs/remotes/origin/HEAD`, returning that branch. See the module
/// documentation for what this does and does not touch.
pub async fn detect_default_branch(repo_path: &str) -> Result<String, String> {
    let repo = Path::new(repo_path);
    if let Some(refused) = super::vcs::detect(repo).await?.refusal(repo_path) {
        return Err(refused);
    }
    if !has_origin(repo).await {
        return Err(format!(
            "{repo_path} has no remote named origin, so there is no default branch to detect. \
             Tasks do not need one."
        ));
    }
    let mut command = tokio::process::Command::new("git");
    command
        .args(["remote", "set-head", "origin", "--auto"])
        .current_dir(repo)
        .stdin(std::process::Stdio::null())
        // Never wait on a credential prompt nobody can see.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .kill_on_drop(true);
    let output = tokio::time::timeout(REMOTE_DEADLINE, command.output())
        .await
        .map_err(|_| {
            format!(
                "origin did not answer within {} seconds; nothing was changed.",
                REMOTE_DEADLINE.as_secs()
            )
        })?
        .map_err(|e| format!("Failed to run git remote set-head: {e}"))?;
    if !output.status.success() {
        return Err(explain_failure(
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    match resolve_remote_base(repo, repo_path).await? {
        Some(base) => Ok(base.branch),
        None => Err(
            "git recorded origin's default branch, but it still cannot be read back as a branch \
             of origin with a commit. Fetch origin and try again."
                .to_string(),
        ),
    }
}

/// What `git remote set-head origin --auto`'s refusal means, in words a
/// person can act on. Git's own message is kept at the end.
fn explain_failure(stderr: &str) -> String {
    let what = if stderr.contains("Multiple remote HEAD branches") {
        "origin does not say which branch is its default (its HEAD is detached and matches more \
         than one branch), so SlashIt will not guess. Set it with `git remote set-head origin \
         <branch>`, or choose a local base branch."
            .to_string()
    } else if stderr.contains("Cannot determine remote HEAD") {
        "origin's HEAD does not name a branch with a commit, so it has no default branch to \
         record. Choose a local base branch instead."
            .to_string()
    } else if let Some(refname) = stderr
        .split("Not a valid ref: ")
        .nth(1)
        .map(|rest| rest.lines().next().unwrap_or(rest).trim())
    {
        format!(
            "origin's default branch is {}, but it has not been fetched ({refname} is not \
             there). Fetch origin (`git fetch origin`), then detect the default branch again.",
            refname
                .strip_prefix("refs/remotes/origin/")
                .unwrap_or(refname)
        )
    } else {
        "origin could not be asked for its default branch.".to_string()
    };
    format!("{what} Nothing was changed. (git: {stderr})")
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

    /// Everything a remote holds that a mutation could change.
    fn remote_fingerprint(remote: &Path) -> String {
        let refs = git(
            remote,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname) %(symref)",
            ],
        );
        let head = std::fs::read_to_string(remote.join("HEAD")).unwrap();
        format!("{refs}\n{head}")
    }

    /// A repository on `trunk` and `other` (same commit), pushed to a bare
    /// `origin` whose HEAD names `trunk`, fetched, with no
    /// `refs/remotes/origin/HEAD` recorded.
    fn without_origin_head() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        let origin = temp.path().join("origin.git");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "trunk"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        git(&repo, &["branch", "other"]);
        git(
            &repo,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "trunk",
                origin.to_str().unwrap(),
            ],
        );
        git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&repo, &["push", "-q", "origin", "trunk", "other"]);
        git(&repo, &["fetch", "-q", "origin"]);
        let _ = std::process::Command::new("git")
            .args(["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"])
            .current_dir(&repo)
            .output();
        (temp, repo, origin)
    }

    #[tokio::test]
    async fn detecting_records_origin_head_and_changes_nothing_on_origin() {
        let (_temp, repo, origin) = without_origin_head();
        let path = repo.to_str().unwrap();
        assert_eq!(remote_state(path).await, RemoteState::DefaultBranchUnknown);
        let before = remote_fingerprint(&origin);

        assert_eq!(detect_default_branch(path).await, Ok("trunk".to_string()));

        assert_eq!(
            remote_state(path).await,
            RemoteState::DefaultBranch {
                branch: "trunk".to_string()
            }
        );
        assert_eq!(remote_fingerprint(&origin), before);
    }

    /// Origin's HEAD detached at a commit two branches share: refused, and
    /// nothing is written locally or on origin.
    #[tokio::test]
    async fn an_ambiguous_origin_is_refused_without_writing_anything() {
        let (_temp, repo, origin) = without_origin_head();
        let tip = git(&repo, &["rev-parse", "HEAD"]);
        git(&origin, &["update-ref", "--no-deref", "HEAD", &tip]);
        let before = remote_fingerprint(&origin);

        let refused = detect_default_branch(repo.to_str().unwrap())
            .await
            .expect_err("ambiguous");
        assert!(refused.contains("will not guess"), "{refused}");
        assert_eq!(
            remote_state(repo.to_str().unwrap()).await,
            RemoteState::DefaultBranchUnknown
        );
        assert_eq!(remote_fingerprint(&origin), before);
    }

    #[tokio::test]
    async fn an_unfetched_default_branch_is_explained() {
        let (_temp, repo, _origin) = without_origin_head();
        git(&repo, &["update-ref", "-d", "refs/remotes/origin/trunk"]);
        let refused = detect_default_branch(repo.to_str().unwrap())
            .await
            .expect_err("unfetched");
        assert!(
            refused.contains("origin's default branch is trunk, but it has not been fetched"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn a_repository_without_origin_has_nothing_to_detect() {
        let temp = tempfile::TempDir::new().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(
            temp.path(),
            &["commit", "-q", "--allow-empty", "-m", "first"],
        );
        let path = temp.path().to_str().unwrap();
        assert_eq!(remote_state(path).await, RemoteState::NoOrigin);
        assert!(detect_default_branch(path)
            .await
            .unwrap_err()
            .contains("no remote named origin"));
    }
}
