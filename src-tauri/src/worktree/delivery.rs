//! What a Task Checkout can prove about its work reaching the remote.
//!
//! Two questions, both answered from Git alone:
//!
//! - [`checkout_snapshot`]: what exactly is in the checkout right now, as a
//!   tree object. Two snapshots differ exactly when something changed in
//!   between, which is how an apply knows that one fix agent edited a file.
//! - [`RemoteBranch`]: where the pull request's branch is on `origin` after a
//!   refresh, and whether a given commit is contained in it.
//!
//! Neither one decides whether a review reply may be posted; that rule lives
//! with the PR review commands. Nothing here is called from rendering code:
//! [`RemoteBranch::refresh`] reaches the network, so only a user-started
//! command that already talks to the forge may call it.

use std::path::Path;

use super::restack;

/// The tree of everything in the checkout at `working_dir`: tracked changes,
/// untracked files and deletions, without touching the checkout's index, its
/// files or its branch.
///
/// A scratch index is filled from `HEAD` and then from the working tree, and
/// written out as a tree. Files Git ignores are not part of it, as they are
/// not part of a commit.
pub async fn checkout_snapshot(working_dir: &str) -> Result<String, String> {
    let scratch = std::env::temp_dir().join(format!("slashit-snapshot-{}.index", uuid::Uuid::new_v4()));
    let result = snapshot_with_index(working_dir, &scratch).await;
    let _ = tokio::fs::remove_file(&scratch).await;
    result
}

async fn snapshot_with_index(working_dir: &str, index: &Path) -> Result<String, String> {
    let run = |args: &'static [&'static str]| {
        let index = index.to_path_buf();
        async move {
            let output = tokio::process::Command::new("git")
                .args(args)
                .env("GIT_INDEX_FILE", &index)
                .current_dir(working_dir)
                .output()
                .await
                .map_err(|e| format!("could not run git: {e}"))?;
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                Err(format!(
                    "git {} failed: {}",
                    args.first().unwrap_or(&""),
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
        }
    };
    // A repository without a commit yet has no HEAD to read: start empty.
    let _ = run(&["read-tree", "HEAD"]).await;
    run(&["add", "-A"]).await?;
    run(&["write-tree"]).await
}

/// Whether the change between the trees `before` and `after` (what one fix
/// agent did) is still in `commit`: the change, applied backwards to the
/// commit's tree, must fit. Fixes sharing a file are told apart by their
/// hunks, not their paths. A change an overlapping edit overwrote, or that
/// was discarded, no longer fits; an empty change, an object this repository
/// lost, or any Git failure answers `false`, because a missing proof is never
/// read as a present one.
pub async fn effect_survives_in(working_dir: &str, before: &str, after: &str, commit: &str) -> bool {
    let run = |args: Vec<String>, index: Option<std::path::PathBuf>| async move {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(&args).current_dir(working_dir);
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd.output().await.ok().filter(|o| o.status.success())
    };
    let Some(diff) = run(
        ["diff", "--binary", "--full-index", before, after].map(String::from).to_vec(),
        None,
    )
    .await
    else {
        return false;
    };
    if diff.stdout.is_empty() {
        return false;
    }
    let id = uuid::Uuid::new_v4();
    let index = std::env::temp_dir().join(format!("slashit-effect-{id}.index"));
    let patch = std::env::temp_dir().join(format!("slashit-effect-{id}.patch"));
    let survives = async {
        tokio::fs::write(&patch, &diff.stdout).await.ok()?;
        run(["read-tree", commit].map(String::from).to_vec(), Some(index.clone())).await?;
        run(
            vec![
                "apply".into(), "--cached".into(), "--reverse".into(), "--check".into(),
                patch.display().to_string(),
            ],
            Some(index.clone()),
        )
        .await
        .map(|_| ())
    }
    .await
    .is_some();
    let _ = tokio::fs::remove_file(&index).await;
    let _ = tokio::fs::remove_file(&patch).await;
    survives
}

/// Where `origin` held a pull request's branch when it was last refreshed.
pub struct RemoteBranch {
    dir: std::path::PathBuf,
    tip: Result<String, String>,
}

impl RemoteBranch {
    /// Ask `origin` for `branch` now and record where it is. A branch that
    /// cannot be fetched (no network, no such branch, a refused name) leaves
    /// nothing proven: [`RemoteBranch::contains`] is then `false` for every
    /// commit, and [`RemoteBranch::unavailable`] says why. There is no
    /// fallback to a remote-tracking ref from an earlier run, which a
    /// force-push may have made stale.
    pub async fn refresh(working_dir: &str, branch: Option<&str>) -> Self {
        let dir = std::path::PathBuf::from(working_dir);
        let tip = match branch {
            None => Err("this task records no branch".to_string()),
            Some(branch) => match super::checked_task_branch(branch) {
                Ok(branch) => restack::fetch_remote_branch(&dir, branch).await,
                Err(e) => Err(e.to_string()),
            },
        };
        Self { dir, tip }
    }

    /// Why the branch could not be refreshed, if it could not.
    pub fn unavailable(&self) -> Option<&str> {
        self.tip.as_ref().err().map(String::as_str)
    }

    /// Whether `commit` is the remote branch's tip or one of its ancestors.
    /// Contains exactly this commit: an equal change under another commit ID
    /// is not it. A commit this repository does not have is not contained.
    pub async fn contains(&self, commit: &str) -> bool {
        let Ok(tip) = &self.tip else { return false };
        matches!(restack::is_ancestor(&self.dir, commit, tip).await, Ok(true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(dir).output().expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A checkout of branch `task` with one commit that `origin` (a bare
    /// repository beside it) already holds.
    fn checkout_with_origin() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let origin = tmp.path().join("origin.git");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(tmp.path(), &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(&work, &["init", "-q", "-b", "task"]);
        git(&work, &["config", "user.email", "t@example.com"]);
        git(&work, &["config", "user.name", "T"]);
        std::fs::write(work.join("a.txt"), "a\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "base"]);
        git(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&work, &["push", "-q", "-u", "origin", "task"]);
        (tmp, work)
    }

    #[tokio::test]
    async fn a_snapshot_changes_exactly_when_the_checkout_does() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        let clean = checkout_snapshot(dir).await.unwrap();
        assert_eq!(checkout_snapshot(dir).await.unwrap(), clean, "a snapshot is repeatable");

        std::fs::write(work.join("b.txt"), "new\n").unwrap();
        let with_new = checkout_snapshot(dir).await.unwrap();
        assert_ne!(with_new, clean, "an untracked file counts");
        std::fs::write(work.join("a.txt"), "edited\n").unwrap();
        assert_ne!(checkout_snapshot(dir).await.unwrap(), with_new, "an edit counts");
        std::fs::remove_file(work.join("b.txt")).unwrap();
        std::fs::write(work.join("a.txt"), "a\n").unwrap();
        assert_eq!(checkout_snapshot(dir).await.unwrap(), clean, "putting everything back is no change");

        assert_eq!(git(&work, &["status", "--porcelain"]), "", "the checkout's own index is untouched");
        assert_eq!(git(&work, &["diff", "--cached", "--name-only"]), "");
    }

    #[tokio::test]
    async fn the_remote_branch_contains_a_commit_it_was_pushed_but_not_one_it_never_saw() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        let pushed = git(&work, &["rev-parse", "HEAD"]);
        std::fs::write(work.join("fix.txt"), "fix\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "fix"]);
        let local_only = git(&work, &["rev-parse", "HEAD"]);

        let remote = RemoteBranch::refresh(dir, Some("task")).await;
        assert_eq!(remote.unavailable(), None);
        assert!(remote.contains(&pushed).await);
        assert!(!remote.contains(&local_only).await, "committed locally is not delivered");
        assert!(!remote.contains(&"0".repeat(40)).await, "an unknown commit is not contained");
        assert!(!remote.contains("not-an-object-id").await);

        git(&work, &["push", "-q", "origin", "task"]);
        assert!(RemoteBranch::refresh(dir, Some("task")).await.contains(&local_only).await);
    }

    #[tokio::test]
    async fn an_identical_change_under_another_commit_id_is_not_contained() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        let base = git(&work, &["rev-parse", "HEAD"]);
        std::fs::write(work.join("fix.txt"), "fix\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "fix"]);
        let fix = git(&work, &["rev-parse", "HEAD"]);
        let twin = git(&work, &["commit-tree", &format!("{fix}^{{tree}}"), "-p", &base, "-m", "twin"]);
        git(&work, &["push", "-q", "origin", &format!("{twin}:refs/heads/task")]);

        let remote = RemoteBranch::refresh(dir, Some("task")).await;
        assert!(remote.contains(&twin).await);
        assert!(!remote.contains(&fix).await, "the same tree and parent under another ID is another commit");
    }

    #[tokio::test]
    async fn a_force_push_that_drops_a_commit_is_seen_at_the_next_refresh() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        let base = git(&work, &["rev-parse", "HEAD"]);
        std::fs::write(work.join("fix.txt"), "fix\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "fix"]);
        let fix = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["push", "-q", "origin", "task"]);
        assert!(RemoteBranch::refresh(dir, Some("task")).await.contains(&fix).await);

        // Someone else rewrites the branch on the server.
        let origin = git(&work, &["remote", "get-url", "origin"]);
        git(&work, &["--git-dir", &origin, "update-ref", "refs/heads/task", &base]);
        // The tracking ref still says the fix is there until it is refreshed.
        assert_eq!(git(&work, &["rev-parse", "origin/task"]), fix);
        assert!(!RemoteBranch::refresh(dir, Some("task")).await.contains(&fix).await);
    }

    #[tokio::test]
    async fn a_remote_that_cannot_be_asked_proves_nothing_even_with_a_cached_tracking_ref() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        let pushed = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["remote", "set-url", "origin", "/nonexistent/remote.git"]);

        let remote = RemoteBranch::refresh(dir, Some("task")).await;
        assert!(remote.unavailable().is_some());
        assert!(!remote.contains(&pushed).await);
        assert!(RemoteBranch::refresh(dir, None).await.unavailable().is_some(), "no branch, no proof");
    }
}
