// SPDX-License-Identifier: MPL-2.0
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
    Ok(match commit(working_dir, branch, message, false, None).await? {
        Some(commit) => CheckoutCommit::Committed { commit },
        None => CheckoutCommit::NothingToCommit,
    })
}

/// [`commit_checkout`] for a caller that has checked the checkout's content as
/// `expected_tree`, a [`checkout_snapshot`] tree id. The staged index is compared
/// with it before the commit, so an edit made after the check and before staging
/// refuses the commit, and nothing is committed. Once staged, the commit takes
/// the index, so later edits to the working tree do not enter it.
pub async fn commit_checkout_verified(
    working_dir: &str,
    branch: &str,
    message: &str,
    expected_tree: &str,
) -> Result<CheckoutCommit, String> {
    Ok(match commit(working_dir, branch, message, false, Some(expected_tree)).await? {
        Some(commit) => CheckoutCommit::Committed { commit },
        None => CheckoutCommit::NothingToCommit,
    })
}

/// [`commit_checkout`], except that a checkout without changes still gets a
/// new, empty commit. Returns the new commit.
///
/// Run completion no longer asks for empty commits — a finished task branch
/// ends at the work itself ([`commit_checkout`], reading the tip it leaves
/// with [`checkout_head_commit`]) — so this variant has no production
/// caller and is compiled only for the tests that pin how [`commit`]
/// distinguishes an empty checkout when a caller does ask directly.
#[cfg(test)]
pub async fn commit_checkout_even_if_empty(
    working_dir: &str,
    branch: &str,
    message: &str,
) -> Result<String, String> {
    commit(working_dir, branch, message, true, None)
        .await?
        .ok_or_else(|| "git made no commit".to_string())
}

/// The commit the checkout's branch tip is at. For a run that was just told
/// there was nothing to commit, the work it has to record is exactly this
/// tip: the agent committed its own work, or changed nothing. The checkout
/// must be the top level of its own Git working tree with a commit reachable
/// through `HEAD` — which [`commit_checkout`]'s refusals have just
/// established for a caller that reads the tip right after a commit attempt
/// — and the call writes nothing.
pub async fn checkout_head_commit(working_dir: &str) -> Result<String, String> {
    git(working_dir, &["rev-parse", "--verify", "HEAD^{commit}"])
        .await
        .map_err(|e| format!("could not read the commit the checkout is at: {e}"))
}

/// Whether the checkout holds work that is not in its base yet: an edit that is
/// not committed, or a commit past `base`. A checkout that is unchanged, or
/// whose only commits are already in `base`, holds no work. Nothing is staged
/// or written.
pub async fn checkout_has_work(working_dir: &str, base: Option<&str>) -> Result<bool, String> {
    // `--untracked-files=normal` overrides `status.showUntrackedFiles=no`, which
    // would otherwise hide a new file that the run created.
    let status = git(working_dir, &["status", "--porcelain", "--untracked-files=normal"])
        .await
        .map_err(|e| format!("git status failed: {e}"))?;
    if !status.is_empty() {
        return Ok(true);
    }
    let Some(base) = base else {
        return Ok(false);
    };
    let ahead = git(working_dir, &["rev-list", "--count", &format!("{base}..HEAD")])
        .await
        .map_err(|e| format!("git rev-list failed: {e}"))?;
    let ahead: u64 = ahead
        .parse()
        .map_err(|_| format!("git rev-list printed an unexpected count: {ahead}"))?;
    Ok(ahead > 0)
}

async fn commit(
    working_dir: &str,
    branch: &str,
    message: &str,
    allow_empty: bool,
    expected_tree: Option<&str>,
) -> Result<Option<String>, String> {
    refuse_unless_on_task_branch(working_dir, branch).await?;

    git(working_dir, &["add", "-A"])
        .await
        .map_err(|e| format!("git add failed: {e}"))?;

    if let Some(expected) = expected_tree {
        let staged = git(working_dir, &["write-tree"])
            .await
            .map_err(|e| format!("could not read the staged content: {e}"))?;
        if staged != expected {
            return Err(
                "the Task Checkout changed after its content was checked, so nothing was committed"
                    .to_string(),
            );
        }
    }

    if !allow_empty && !has_staged_changes(working_dir).await? {
        return Ok(None);
    }

    // The tip before this commit, so a commit that holds other content can be undone.
    let before = git(working_dir, &["rev-parse", "--verify", "HEAD"]).await.ok();

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
    if let Some(expected) = expected_tree {
        // Pre-commit hooks run after the check above and may stage files, so the
        // commit can record a tree other than the one that was checked.
        let holds_checked_tree = git(working_dir, &["rev-parse", "HEAD^{tree}"])
            .await
            .is_ok_and(|tree| tree == expected);
        if !holds_checked_tree {
            return Err(undo_commit(working_dir, before.as_deref(), &commit).await);
        }
    }
    Ok(Some(commit))
}

/// Moves the task branch back to `before` after a commit whose tree is not the
/// checked tree. A soft reset: the working tree and the index are untouched, so
/// every edit, forced ignored files included, stays visible to `git status`.
async fn undo_commit(working_dir: &str, before: Option<&str>, made: &str) -> String {
    const REFUSAL: &str = "a Git hook changed the staged content after it was checked";
    let Some(before) = before else {
        return format!("{REFUSAL}, and the commit {made} could not be undone, so it stays on the task branch");
    };
    match git(working_dir, &["reset", "-q", "--soft", before]).await {
        Ok(_) => format!("{REFUSAL}, so the commit was undone and nothing was committed"),
        Err(e) => format!("{REFUSAL}, and the commit {made} could not be undone, so it stays on the task branch: {e}"),
    }
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
    let output = crate::host_process::tokio_command("git")
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
    // A cancelled call stops its git process, so it cannot keep writing the index
    // after the record gate is released. Hook processes it started are not killed.
    let output = crate::host_process::tokio_command("git")
        .args(args)
        .current_dir(working_dir)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("could not run git: {e}"))?;
    if !output.status.success() {
        return Err(explain(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The content of a checkout at one moment, as Git tree ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutSnapshot {
    /// Committed and uncommitted content, tracked and untracked files alike,
    /// ignored untracked files left out. Equal ids mean equal content, however
    /// many commits or edits produced it.
    pub content: String,
    /// Whether the content is exactly HEAD's tree: no uncommitted change.
    pub clean: bool,
}

/// Snapshots the content of the checkout at `working_dir`, so that comparing a
/// snapshot from before an agent run with one from after shows whether the run
/// changed anything. The scratch index starts from HEAD, not from a copy of the
/// checkout's index: a copy gets a new file time, so Git can trust a cached
/// entry whose size and time match an edit made in the same moment and report
/// a stale blob. The checkout's own index is never written, and the scratch
/// index and its lock are removed afterwards.
pub async fn checkout_snapshot(working_dir: &str) -> Result<CheckoutSnapshot, String> {
    let git_dir = git(working_dir, &["rev-parse", "--absolute-git-dir"])
        .await
        .map_err(|e| format!("could not locate the checkout's Git directory: {e}"))?;
    let index = Path::new(&git_dir).join("slashit-content-snapshot.index");
    let lock = Path::new(&git_dir).join("slashit-content-snapshot.index.lock");
    let _ = std::fs::remove_file(&index);
    let _ = std::fs::remove_file(&lock);
    let result = async {
        // An unborn HEAD has no tree to read: the scratch index then starts empty.
        let _ = git_with_index(working_dir, &index, &["read-tree", "HEAD"]).await;
        // A file staged with `git add -f` despite an ignore rule is in the commit that
        // staging everything would make, but `add -A` skips it here. Its additions are
        // added explicitly. A path deleted since staging is left out, as `add -A` drops it.
        // `--no-renames`: a staged rename into an ignored name is an addition too.
        let staged = git(working_dir, &["diff", "--cached", "--no-renames", "--name-only", "--diff-filter=A", "-z"]).await?;
        let forced: Vec<&str> = staged
            .split('\0')
            .filter(|p| !p.is_empty() && Path::new(working_dir).join(p).symlink_metadata().is_ok())
            .collect();
        if !forced.is_empty() {
            // `:(literal)`: a name with glob characters must not match other, unstaged files.
            let literal: Vec<String> = forced.iter().map(|p| format!(":(literal){p}")).collect();
            let mut args = vec!["add", "-f", "--"];
            args.extend(literal.iter().map(String::as_str));
            git_with_index(working_dir, &index, &args).await?;
        }
        git_with_index(working_dir, &index, &["add", "-A"]).await?;
        let content = git_with_index(working_dir, &index, &["write-tree"]).await?;
        let head = git(working_dir, &["rev-parse", "HEAD^{tree}"]).await?;
        Ok(CheckoutSnapshot {
            clean: content == head,
            content,
        })
    }
    .await;
    let _ = std::fs::remove_file(&index);
    let _ = std::fs::remove_file(&lock);
    result
}

/// Runs one Git command against a temporary index file.
async fn git_with_index(working_dir: &str, index: &Path, args: &[&str]) -> Result<String, String> {
    let output = crate::host_process::tokio_command("git")
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .current_dir(working_dir)
        .output()
        .await
        .map_err(|e| format!("could not run git: {e}"))?;
    if !output.status.success() {
        return Err(format!("git {} failed: {}", args[0], explain(&output)));
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

    /// The tip of the branch, read back after an agent-style commit of its
    /// own, is what one run with nothing left to commit records as its head.
    #[tokio::test]
    async fn a_commit_checked_against_a_tree_refuses_content_changed_after_the_check() {
        let (_tmp, _repo, checkout) = repo_with_task_checkout();
        let dir = checkout.to_str().unwrap();
        std::fs::write(checkout.join("kept.txt"), "checked\n").unwrap();
        let checked = checkout_snapshot(dir).await.unwrap().content;
        let head = git_ok(&checkout, &["rev-parse", "task"]);

        std::fs::write(checkout.join("kept.txt"), "edited after the check\n").unwrap();
        let refused = commit_checkout_verified(dir, "task", "task: fix it", &checked).await;
        assert!(refused.is_err(), "a change after the check is refused: {refused:?}");
        assert_eq!(git_ok(&checkout, &["rev-parse", "task"]), head, "nothing was committed");

        std::fs::write(checkout.join("kept.txt"), "checked\n").unwrap();
        let outcome = commit_checkout_verified(dir, "task", "task: fix it", &checked)
            .await
            .expect("the checked content commits");
        assert!(matches!(outcome, CheckoutCommit::Committed { .. }));
        assert_eq!(git_ok(&checkout, &["show", "task:kept.txt"]), "checked");
    }

    #[tokio::test]
    async fn a_snapshot_matches_what_staging_everything_would_commit_in_each_shape() {
        let (_tmp, _repo, checkout) = repo_with_task_checkout();
        let dir = checkout.to_str().unwrap();
        std::fs::write(checkout.join("a.txt"), "aaa\n").unwrap();
        std::fs::write(checkout.join("b.txt"), "bbb\n").unwrap();
        std::fs::write(checkout.join("c.txt"), "ccc\n").unwrap();
        git_ok(&checkout, &["add", "-A"]);
        git_ok(&checkout, &["commit", "-q", "-m", "base files"]);

        // Each step is checked against what `add -A` then `write-tree` would record.
        let steps: Vec<(&str, Box<dyn Fn(&Path)>)> = vec![
            ("unstaged same-size edit", Box::new(|c: &Path| std::fs::write(c.join("a.txt"), "AAA\n").unwrap())),
            ("staged-only edit", Box::new(|c: &Path| {
                std::fs::write(c.join("b.txt"), "BBB\n").unwrap();
                git_ok(c, &["add", "b.txt"]);
            })),
            ("staged then unstaged edit", Box::new(|c: &Path| std::fs::write(c.join("b.txt"), "bbX\n").unwrap())),
            ("added and deleted files", Box::new(|c: &Path| {
                std::fs::write(c.join("new.txt"), "new\n").unwrap();
                std::fs::remove_file(c.join("c.txt")).unwrap();
            })),
        ];
        #[cfg(unix)]
        let steps = {
            let mut steps = steps;
            steps.push(("mode change and symlink", Box::new(|c: &Path| {
                use std::os::unix::fs::PermissionsExt;
                let mut perm = std::fs::metadata(c.join("a.txt")).unwrap().permissions();
                perm.set_mode(0o755);
                std::fs::set_permissions(c.join("a.txt"), perm).unwrap();
                std::os::unix::fs::symlink("a.txt", c.join("link")).unwrap();
            })));
            steps
        };
        for (name, edit) in steps {
            edit(&checkout);
            let snapshot = checkout_snapshot(dir).await.unwrap().content;
            git_ok(&checkout, &["add", "-A"]);
            let staged = git_ok(&checkout, &["write-tree"]);
            assert_eq!(snapshot, staged, "step: {name}");
        }
    }

    #[tokio::test]
    async fn a_snapshot_reads_an_edit_that_keeps_the_size_and_the_index_time() {
        let (_tmp, _repo, checkout) = repo_with_task_checkout();
        let dir = checkout.to_str().unwrap();
        // A staged blob that a refused commit left behind in the real index.
        std::fs::write(checkout.join("kept.txt"), "EIGHT\n").unwrap();
        git_ok(&checkout, &["add", "-A"]);
        let index = checkout.join(git_ok(&checkout, &["rev-parse", "--git-path", "index"]));
        let index_time = std::fs::metadata(&index).unwrap().modified().unwrap();
        // The same-size edit, stamped with the index's own time.
        let edited = checkout.join("kept.txt");
        std::fs::write(&edited, "eight\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&edited)
            .unwrap()
            .set_modified(index_time)
            .unwrap();

        let snapshot = checkout_snapshot(dir).await.unwrap().content;
        git_ok(&checkout, &["add", "-A"]);
        let staged = git_ok(&checkout, &["write-tree"]);
        assert_eq!(snapshot, staged, "the snapshot reads the file as it is on disk");
    }

    #[tokio::test]
    async fn checkout_head_commit_names_the_tip_the_commit_left_in_place() {
        let (_tmp, _repo, checkout) = repo_with_task_checkout();
        std::fs::write(checkout.join("own work.txt"), "committed by the agent\n").unwrap();
        git_ok(&checkout, &["add", "-A"]);
        git_ok(&checkout, &["commit", "-q", "-m", "agent: my own work"]);
        let head = git_ok(&checkout, &["rev-parse", "HEAD"]);

        let tip = checkout_head_commit(checkout.to_str().unwrap())
            .await
            .expect("tip");

        assert_eq!(tip, head);
    }

    /// The empty-commit variant still does what its contract says when a
    /// caller asks for it directly. The run-completion commit no longer does:
    /// the executor's `commit_changes` now uses `commit_checkout` and, when
    /// it answers `CheckoutCommit::NothingToCommit`, records the branch tip
    /// that `checkout_head_commit` returns, so a finished run adds a commit
    /// only when the checkout held uncommitted work.
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

    /// A file staged with `git add -f` despite an ignore rule is part of the commit
    /// that staging everything makes, so the checked snapshot must hold it too.
    #[tokio::test]
    async fn a_forced_staged_ignored_addition_is_in_the_snapshot() {
        let (_tmp, _repo, checkout) = repo_with_task_checkout();
        let dir = checkout.to_str().unwrap();
        std::fs::write(checkout.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(checkout.join("fixture.log"), "forced\n").unwrap();
        git_ok(&checkout, &["add", "-f", "fixture.log"]);
        let checked = checkout_snapshot(dir).await.unwrap().content;

        let outcome = commit_checkout_verified(dir, "task", "task: fix it", &checked)
            .await
            .expect("the checked content commits");

        assert!(matches!(outcome, CheckoutCommit::Committed { .. }));
        assert_eq!(git_ok(&checkout, &["show", "task:fixture.log"]), "forced");
    }

    /// A forced name with glob characters is added literally: an unstaged ignored file
    /// that the same pattern would match is neither in the snapshot nor in the commit.
    #[tokio::test]
    async fn a_forced_name_with_glob_characters_adds_only_itself() {
        let (_tmp, _repo, checkout) = repo_with_task_checkout();
        let dir = checkout.to_str().unwrap();
        std::fs::write(checkout.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(checkout.join("out[1].log"), "forced\n").unwrap();
        std::fs::write(checkout.join("out1.log"), "unstaged\n").unwrap();
        git_ok(&checkout, &["add", "-f", "--", ":(literal)out[1].log"]);
        let checked = checkout_snapshot(dir).await.unwrap().content;

        let outcome = commit_checkout_verified(dir, "task", "task: fix it", &checked)
            .await
            .expect("the checked content commits");

        assert!(matches!(outcome, CheckoutCommit::Committed { .. }));
        assert_eq!(git_ok(&checkout, &["show", "task:out[1].log"]), "forced");
        assert!(StdCommand::new("git")
            .args(["cat-file", "-e", "task:out1.log"])
            .current_dir(&checkout)
            .status()
            .unwrap()
            .code()
            != Some(0));
    }

    /// A staged rename into an ignored name is an addition the commit records, so the
    /// snapshot must hold it, not read it as a rename that drops it.
    #[tokio::test]
    async fn a_staged_rename_into_an_ignored_name_is_in_the_snapshot() {
        let (_tmp, _repo, checkout) = repo_with_task_checkout();
        let dir = checkout.to_str().unwrap();
        std::fs::write(checkout.join(".gitignore"), "*.log\n").unwrap();
        std::fs::rename(checkout.join("kept.txt"), checkout.join("renamed.log")).unwrap();
        git_ok(&checkout, &["rm", "-q", "--cached", "kept.txt"]);
        git_ok(&checkout, &["add", "-f", "renamed.log"]);
        let checked = checkout_snapshot(dir).await.unwrap().content;

        let outcome = commit_checkout_verified(dir, "task", "task: fix it", &checked)
            .await
            .expect("the checked content commits");

        assert!(matches!(outcome, CheckoutCommit::Committed { .. }));
        assert_eq!(git_ok(&checkout, &["show", "task:renamed.log"]), "kept");
    }

    /// A pre-commit hook that stages a file changes what the commit records. The
    /// commit is undone: the branch is where it was, and the edits stay on disk.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hook_that_stages_more_than_was_checked_is_undone() {
        let (_tmp, repo, checkout) = repo_with_task_checkout();
        // Published by `write_executable`'s short-lived child, as in the hook test below.
        crate::test_helpers::write_executable(
            &repo.join(".git/hooks/pre-commit"),
            "#!/bin/sh\necho extra > extra.txt && git add extra.txt\n",
        );
        std::fs::write(checkout.join("kept.txt"), "checked\n").unwrap();
        std::fs::write(checkout.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(checkout.join("forced.log"), "forced\n").unwrap();
        git_ok(&checkout, &["add", "-f", "--", ":(literal)forced.log"]);
        let dir = checkout.to_str().unwrap();
        let checked = checkout_snapshot(dir).await.unwrap().content;
        let before = git_ok(&checkout, &["rev-parse", "task"]);

        let error = commit_checkout_verified(dir, "task", "task: fix it", &checked)
            .await
            .expect_err("the commit holding the hook's file is refused");

        assert!(error.contains("a Git hook changed the staged content"), "{error}");
        assert!(error.contains("undone"), "{error}");
        assert_eq!(git_ok(&checkout, &["rev-parse", "task"]), before, "the commit was undone");
        assert_eq!(std::fs::read_to_string(checkout.join("kept.txt")).unwrap(), "checked\n");
        // The forced ignored file is still visible to status, so no clean check can
        // read the checkout as clean while it is there.
        let status = git_ok(&checkout, &["status", "--porcelain"]);
        assert!(status.contains("forced.log"), "{status}");
    }

    // The hook is a shell script made executable with Unix permissions.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_commit_a_hook_rejects_is_an_error_with_the_hook_s_reason() {
        let (_tmp, repo, checkout) = repo_with_task_checkout();
        let hook = repo.join(".git/hooks/pre-commit");
        // Published by `write_executable`'s short-lived child, not written by
        // this process, so git's exec of the hook cannot fail with ETXTBSY
        // while a fork still holds it open for writing.
        crate::test_helpers::write_executable(
            &hook,
            "#!/bin/sh\necho 'commits are refused here' >&2\nexit 1\n",
        );
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
