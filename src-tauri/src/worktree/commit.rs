//! Recording the edits in a Task Checkout as a Git commit on its task branch.
//!
//! A Task Checkout is always a Git worktree, so Git alone decides whether
//! work is committed. jj is never run here: `jj describe` in a Git worktree
//! either finds no jj repository at all, or, for a checkout inside a jj
//! repository's working tree, finds that enclosing repository and rewrites
//! *its* current change, while the checkout's edits stay uncommitted. A
//! jj-colocated repository needs no help to see the new commit either: jj
//! imports the moved Git branch on its next command.

use std::path::Path;

/// What [`commit_checkout`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutCommit {
    /// A new commit is the tip of the checkout's branch.
    Committed { commit: String },
    /// The checkout had no changes, so nothing was committed.
    NothingToCommit,
}

/// Commit every change in the checkout at `working_dir` (tracked,
/// untracked and deleted files alike) with `message`, as a new commit on
/// the task branch `branch`. A checkout without changes is reported as
/// [`CheckoutCommit::NothingToCommit`].
///
/// The checkout must be the top level of its own Git working tree, have
/// `branch` checked out, and be in the middle of no rebase, merge,
/// cherry-pick, revert or bisect, with no unresolved conflicts. Anything
/// else is refused before a file is staged, with a message saying what to
/// do: an agent with a shell can leave its checkout detached, on another
/// branch or in a stopped rebase, and committing there would put the work
/// somewhere the task branch does not have it, or record conflict markers
/// as the fix. A directory that is not the top of its own working tree,
/// such as a checkout whose `.git` link has gone missing inside some other
/// repository, would otherwise be committed into whatever repository Git
/// finds above it.
///
/// Every Git step is checked, and a failure (a hook that rejects the commit,
/// a missing identity, an index that cannot be written) is returned with
/// Git's own explanation. Hooks run as they would for the user. Git is run
/// with an argument vector, never through a shell, and `message` is a single
/// argument.
pub async fn commit_checkout(
    working_dir: &str,
    branch: &str,
    message: &str,
) -> Result<CheckoutCommit, String> {
    Ok(match commit(working_dir, branch, message, false).await? {
        Some(commit) => CheckoutCommit::Committed { commit },
        None => CheckoutCommit::NothingToCommit,
    })
}

/// [`commit_checkout`], except that a checkout without changes still gets a
/// new, empty commit. Returns the new commit.
pub async fn commit_checkout_even_if_empty(
    working_dir: &str,
    branch: &str,
    message: &str,
) -> Result<String, String> {
    commit(working_dir, branch, message, true)
        .await?
        .ok_or_else(|| "git made no commit".to_string())
}

async fn commit(
    working_dir: &str,
    branch: &str,
    message: &str,
    allow_empty: bool,
) -> Result<Option<String>, String> {
    refuse_unless_on_task_branch(working_dir, branch).await?;

    git(working_dir, &["add", "-A"])
        .await
        .map_err(|e| format!("git add failed: {e}"))?;

    if !allow_empty && !has_staged_changes(working_dir).await? {
        return Ok(None);
    }

    let mut args = vec!["commit", "-q"];
    if allow_empty {
        args.push("--allow-empty");
    }
    args.extend(["-m", message]);
    git(working_dir, &args)
        .await
        .map_err(|e| format!("git commit failed: {e}"))?;

    let commit = git(working_dir, &["rev-parse", "HEAD"])
        .await
        .map_err(|e| format!("committed, but the new commit could not be read back: {e}"))?;
    Ok(Some(commit))
}

/// Prove Git registers `working_dir` under the Project repository on the Task's branch.
pub async fn validate_registered_task_checkout(repository: &str, working_dir: &str, branch: &str) -> Result<(), String> {
    refuse_unless_on_task_branch(working_dir, branch).await?;
    let listing = git(repository, &["worktree", "list", "--porcelain"])
        .await.map_err(|error| format!("could not verify Task Checkout ownership: {error}"))?;
    let expected_branch = format!("refs/heads/{branch}");
    for record in listing.split("\n\n") {
        let mut path = None;
        let mut registered_branch = None;
        for line in record.lines() {
            if let Some(value) = line.strip_prefix("worktree ") { path = Some(value); }
            if let Some(value) = line.strip_prefix("branch ") { registered_branch = Some(value); }
        }
        if path.is_some_and(|path| same_directory(Path::new(path), Path::new(working_dir)))
            && registered_branch == Some(expected_branch.as_str()) {
            return Ok(());
        }
    }
    Err(format!("{working_dir} is not registered by the Project repository on {branch}"))
}

/// Refuse a checkout that is not exactly where the task's work belongs: see
/// [`commit_checkout`]. Nothing has been staged when this refuses.
async fn refuse_unless_on_task_branch(working_dir: &str, branch: &str) -> Result<(), String> {
    let top_level = git(working_dir, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|e| format!("{working_dir} is not a Git checkout: {e}"))?;
    if !same_directory(Path::new(&top_level), Path::new(working_dir)) {
        return Err(format!(
            "{working_dir} is not the top of its own Git working tree (Git found {top_level}), \
             so nothing was committed"
        ));
    }

    let state = super::restack::worktree_state(Path::new(working_dir))
        .await
        .map_err(|e| format!("could not read the state of the checkout at {working_dir}: {e}"))?;
    if let Some(operation) = state.in_progress {
        return Err(format!(
            "the checkout at {working_dir} is in the middle of a {operation}, so nothing was \
             committed. Finish or abort the {operation} there and check out {branch}"
        ));
    }
    let expected = format!("refs/heads/{branch}");
    if state.head_ref.as_deref() != Some(expected.as_str()) {
        let found = match &state.head_ref {
            Some(other) => format!("branch {}", other.strip_prefix("refs/heads/").unwrap_or(other)),
            None => format!("a detached HEAD at {}", state.head),
        };
        return Err(format!(
            "the checkout at {working_dir} has {found} checked out, not the task branch \
             {branch}, so nothing was committed. Check out {branch} there, bringing the changes \
             with you"
        ));
    }
    let conflicted: Vec<&str> = state
        .uncommitted
        .iter()
        .filter(|line| is_unmerged(line))
        .map(|line| line.get(3..).unwrap_or(line))
        .collect();
    if !conflicted.is_empty() {
        return Err(format!(
            "the checkout at {working_dir} has unresolved conflicts ({}), so nothing was \
             committed. Resolve them there",
            conflicted.join(", ")
        ));
    }
    Ok(())
}

/// Whether a `git status --porcelain` line is an unmerged path.
fn is_unmerged(line: &str) -> bool {
    matches!(line.get(..2), Some("DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU"))
}

/// Whether the index differs from `HEAD`. `git diff --cached --quiet` exits
/// 1 for a difference and 0 for none; anything else is a failure.
async fn has_staged_changes(working_dir: &str) -> Result<bool, String> {
    let output = tokio::process::Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(working_dir)
        .output()
        .await
        .map_err(|e| format!("could not run git: {e}"))?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(format!("git diff --cached failed: {}", explain(&output))),
    }
}

async fn git(working_dir: &str, args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(working_dir)
        .output()
        .await
        .map_err(|e| format!("could not run git: {e}"))?;
    if !output.status.success() {
        return Err(explain(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Git's explanation of a failure on one bounded line: its stderr, or its
/// stdout when stderr is empty (some hooks write there), or the exit status.
fn explain(output: &std::process::Output) -> String {
    const LIMIT: usize = 500;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|t| !t.is_empty())
        .map(|t| t.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" / "))
        .unwrap_or_else(|| format!("exited with {}", output.status));
    match text.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text,
    }
}

fn same_directory(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command as StdCommand;

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let output = StdCommand::new("git").args(args).current_dir(dir).output().expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A repository with one commit and a task branch checked out in a Git
    /// worktree beside it. The identity is repository-local: CI has none.
    fn repo_with_task_checkout() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_ok(&repo, &["init", "-q", "-b", "main"]);
        git_ok(&repo, &["config", "user.email", "t@example.com"]);
        git_ok(&repo, &["config", "user.name", "T"]);
        std::fs::write(repo.join("kept.txt"), "kept\n").unwrap();
        std::fs::write(repo.join("gone.txt"), "gone\n").unwrap();
        git_ok(&repo, &["add", "-A"]);
        git_ok(&repo, &["commit", "-q", "-m", "seed"]);
        let checkout = tmp.path().join("checkout");
        git_ok(&repo, &["worktree", "add", "-q", "-b", "task", checkout.to_str().unwrap()]);
        (tmp, repo, checkout)
    }

    #[tokio::test]
    async fn every_change_becomes_one_commit_on_the_task_branch() {
        let (_tmp, _repo, checkout) = repo_with_task_checkout();
        std::fs::write(checkout.join("kept.txt"), "edited\n").unwrap();
        std::fs::remove_file(checkout.join("gone.txt")).unwrap();
        std::fs::write(checkout.join("new.txt"), "new\n").unwrap();

        let outcome = commit_checkout(checkout.to_str().unwrap(), "task", "task: fix it")
            .await
            .expect("commit");

        let tip = git_ok(&checkout, &["rev-parse", "task"]);
        assert_eq!(outcome, CheckoutCommit::Committed { commit: tip });
        assert_eq!(git_ok(&checkout, &["status", "--porcelain"]), "");
        assert_eq!(git_ok(&checkout, &["log", "-1", "--format=%s", "task"]), "task: fix it");
        assert_eq!(
            git_ok(&checkout, &["diff", "--name-status", "main", "task"]),
            "D\tgone.txt\nM\tkept.txt\nA\tnew.txt",
        );
    }

    #[tokio::test]
    async fn a_clean_checkout_has_nothing_to_commit_unless_an_empty_commit_is_asked_for() {
        let (_tmp, _repo, checkout) = repo_with_task_checkout();
        let before = git_ok(&checkout, &["rev-parse", "task"]);

        let outcome = commit_checkout(checkout.to_str().unwrap(), "task", "task: nothing").await;
        assert_eq!(outcome, Ok(CheckoutCommit::NothingToCommit));
        assert_eq!(git_ok(&checkout, &["rev-parse", "task"]), before);

        let commit = commit_checkout_even_if_empty(checkout.to_str().unwrap(), "task", "task: empty")
            .await
            .expect("empty commit");
        let tip = git_ok(&checkout, &["rev-parse", "task"]);
        assert_ne!(tip, before);
        assert_eq!(commit, tip);
    }

    // The hook is a shell script made executable with Unix permissions.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_commit_a_hook_rejects_is_an_error_with_the_hook_s_reason() {
        let (_tmp, repo, checkout) = repo_with_task_checkout();
        let hook = repo.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho 'commits are refused here' >&2\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(checkout.join("new.txt"), "new\n").unwrap();
        let before = git_ok(&checkout, &["rev-parse", "task"]);

        let error = commit_checkout(checkout.to_str().unwrap(), "task", "task: fix it")
            .await
            .expect_err("the hook refuses the commit");

        assert!(error.starts_with("git commit failed"), "{error}");
        assert!(error.contains("commits are refused here"), "{error}");
        assert_eq!(git_ok(&checkout, &["rev-parse", "task"]), before);
    }

    #[tokio::test]
    async fn an_index_git_cannot_lock_is_an_error() {
        let (_tmp, repo, checkout) = repo_with_task_checkout();
        std::fs::write(checkout.join("new.txt"), "new\n").unwrap();
        let admin = PathBuf::from(git_ok(&checkout, &["rev-parse", "--absolute-git-dir"]));
        std::fs::write(admin.join("index.lock"), b"").unwrap();

        let error = commit_checkout(checkout.to_str().unwrap(), "task", "task: fix it")
            .await
            .expect_err("a locked index cannot be written");

        assert!(error.starts_with("git add failed"), "{error}");
        assert!(error.contains("index.lock"), "{error}");
        assert_eq!(git_ok(&repo, &["rev-parse", "main"]), git_ok(&checkout, &["rev-parse", "task"]));
    }

    /// A directory whose `.git` link is gone is not a checkout of its own;
    /// Git would find the repository around it and commit there.
    #[tokio::test]
    async fn a_directory_inside_another_repository_is_refused() {
        let (_tmp, repo, _checkout) = repo_with_task_checkout();
        let stray = repo.join("stray");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("new.txt"), "new\n").unwrap();
        let before = git_ok(&repo, &["rev-parse", "main"]);

        let error = commit_checkout(stray.to_str().unwrap(), "main", "task: fix it")
            .await
            .expect_err("not a checkout of its own");

        assert!(error.contains("not the top of its own Git working tree"), "{error}");
        assert_eq!(git_ok(&repo, &["rev-parse", "main"]), before);
        assert_eq!(git_ok(&repo, &["status", "--porcelain"]), "?? stray/");
    }

    #[tokio::test]
    async fn a_directory_outside_any_repository_is_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let error = commit_checkout(tmp.path().to_str().unwrap(), "task", "task: fix it")
            .await
            .expect_err("not a Git checkout");
        assert!(error.contains("is not a Git checkout"), "{error}");
    }

    /// Where the fix agent may have left its checkout, each refused before
    /// anything is staged: every ref and the index are as they were.
    async fn assert_refused(checkout: &Path, repo: &Path, reason: &str) {
        std::fs::write(checkout.join("new.txt"), "new\n").unwrap();
        let refs = git_ok(repo, &["for-each-ref", "--format=%(refname) %(objectname)"]);
        let head = git_ok(checkout, &["rev-parse", "HEAD"]);

        let error = commit_checkout(checkout.to_str().unwrap(), "task", "task: fix it")
            .await
            .expect_err("refused");

        assert!(error.contains(reason), "{error}");
        assert!(error.contains("nothing was committed"), "{error}");
        assert_eq!(git_ok(repo, &["for-each-ref", "--format=%(refname) %(objectname)"]), refs);
        assert_eq!(git_ok(checkout, &["rev-parse", "HEAD"]), head);
        assert!(git_ok(checkout, &["status", "--porcelain"]).contains("?? new.txt"), "nothing is staged");
    }

    #[tokio::test]
    async fn a_detached_checkout_is_refused() {
        let (_tmp, repo, checkout) = repo_with_task_checkout();
        git_ok(&checkout, &["checkout", "-q", "--detach"]);
        assert_refused(&checkout, &repo, "a detached HEAD at").await;
    }

    #[tokio::test]
    async fn a_checkout_on_another_branch_is_refused() {
        let (_tmp, repo, checkout) = repo_with_task_checkout();
        git_ok(&checkout, &["checkout", "-q", "-b", "elsewhere"]);
        assert_refused(&checkout, &repo, "branch elsewhere checked out, not the task branch task").await;
    }

    #[tokio::test]
    async fn a_checkout_stopped_in_a_conflicted_rebase_is_refused() {
        let (_tmp, repo, checkout) = repo_with_task_checkout();
        std::fs::write(checkout.join("kept.txt"), "task side\n").unwrap();
        git_ok(&checkout, &["commit", "-q", "-am", "task side"]);
        git_ok(&checkout, &["checkout", "-q", "-b", "upstream", "main"]);
        std::fs::write(checkout.join("kept.txt"), "upstream side\n").unwrap();
        git_ok(&checkout, &["commit", "-q", "-am", "upstream side"]);
        git_ok(&checkout, &["checkout", "-q", "task"]);
        let rebase = StdCommand::new("git").args(["rebase", "-q", "upstream"]).current_dir(&checkout).output().unwrap();
        assert!(!rebase.status.success(), "the rebase stops on its conflict");
        assert_refused(&checkout, &repo, "in the middle of a rebase").await;
    }

    #[tokio::test]
    async fn a_checkout_in_the_middle_of_a_bisect_is_refused() {
        let (_tmp, repo, checkout) = repo_with_task_checkout();
        git_ok(&checkout, &["bisect", "start"]);
        assert_refused(&checkout, &repo, "in the middle of a bisect").await;
    }

    #[tokio::test]
    async fn unresolved_conflicts_are_refused() {
        let (_tmp, repo, checkout) = repo_with_task_checkout();
        std::fs::write(checkout.join("kept.txt"), "stashed side\n").unwrap();
        git_ok(&checkout, &["stash", "-q"]);
        std::fs::write(checkout.join("kept.txt"), "task side\n").unwrap();
        git_ok(&checkout, &["commit", "-q", "-am", "task side"]);
        let pop = StdCommand::new("git").args(["stash", "pop", "-q"]).current_dir(&checkout).output().unwrap();
        assert!(!pop.status.success(), "the stash conflicts");
        assert_refused(&checkout, &repo, "unresolved conflicts (kept.txt)").await;
    }
}
