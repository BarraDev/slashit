//! Replaying a task's own commits onto a new base, in the task's Git worktree.
//!
//! This is the Git half of restacking a stacked task whose parent has landed
//! on the default branch (see `commands::pr`, which decides whether a restack
//! is proven safe and records its result). Everything here takes object IDs
//! and fully qualified ref names only, never a name git would look up in
//! several namespaces, and never runs anything that could set work aside,
//! move another branch, or reach the remote.
//!
//! The one mutation is `git rebase --onto <onto> <fork point> <branch>`, run
//! in the worktree that has the branch checked out. Git moves the branch ref
//! only once every commit has been replayed, keeps the state of a rebase in
//! progress in the worktree's own Git directory, and restores the branch on
//! `git rebase --abort`. `ORIG_HEAD` is not a durable record of where the
//! branch was, so before the rebase the old tip is written to a backup ref
//! under `refs/slashit/restack-backup/`, which every worktree of the
//! repository shares and which jj neither imports nor rewrites.

use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

/// The backup ref that holds a task branch's tip from before a restack,
/// while the restack is in flight.
pub fn backup_ref(task_id: Uuid) -> String {
    format!("refs/slashit/restack-backup/{task_id}")
}

/// Whether `value` is a full object ID: exactly 40 (SHA-1) or 64 (SHA-256)
/// lowercase hexadecimal digits, which is all `git rev-parse` prints for one.
pub fn is_full_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A finished `git` run: its exit code, stdout and stderr, both trimmed.
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run(dir: &Path, args: &[&str], envs: &[(&str, String)]) -> Result<Ran, String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .envs(envs.iter().map(|(k, v)| (*k, v.as_str())))
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("Failed to run git: {e}"))?;
    Ok(Ran {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// `git <args>` in `dir`, its stdout on success and its stderr otherwise.
async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let ran = run(dir, args, &[]).await?;
    if ran.code != Some(0) {
        return Err(format!("git {} failed: {}", args.first().unwrap_or(&""), ran.stderr));
    }
    Ok(ran.stdout)
}

/// The object ID `refname` points at, or `None` when there is no ref of
/// exactly that name.
///
/// `for-each-ref` matches the full ref name only, never a branch or tag that
/// merely shares it. Its pattern also matches refs below `refname/`, so only
/// the line for the exact name is taken.
pub async fn exact_ref(dir: &Path, refname: &str) -> Result<Option<String>, String> {
    let listed = git(dir, &["for-each-ref", "--format=%(refname) %(objectname)", refname]).await?;
    Ok(listed.lines().find_map(|line| {
        let (name, oid) = line.split_once(' ')?;
        (name == refname && is_full_object_id(oid)).then(|| oid.to_string())
    }))
}

/// Whether the repository has the commit `oid`. Only for a full object ID.
pub async fn has_commit(dir: &Path, oid: &str) -> Result<bool, String> {
    if !is_full_object_id(oid) {
        return Ok(false);
    }
    let ran = run(dir, &["cat-file", "-e", &format!("{oid}^{{commit}}")], &[]).await?;
    Ok(ran.code == Some(0))
}

/// Whether commit `ancestor` is an ancestor of (or equal to) `descendant`,
/// both full object IDs. `merge-base --is-ancestor` answers with its exit
/// status: 0 is yes, 1 is no, and anything else is a failure, never a no.
pub async fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    if !is_full_object_id(ancestor) || !is_full_object_id(descendant) {
        return Err(format!(
            "{ancestor:?} and {descendant:?} must both be full object IDs to be compared"
        ));
    }
    let ran = run(dir, &["merge-base", "--is-ancestor", ancestor, descendant], &[]).await?;
    match ran.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "git could not tell whether {ancestor} is an ancestor of {descendant}: {}",
            ran.stderr
        )),
    }
}

/// The commits in `from..to`, oldest first.
async fn commits_between(dir: &Path, from: &str, to: &str) -> Result<Vec<String>, String> {
    let listed = git(dir, &["rev-list", "--reverse", &format!("{from}..{to}")]).await?;
    Ok(listed.lines().map(str::to_string).collect())
}

/// The merge commits in `from..to`.
pub async fn merges_between(dir: &Path, from: &str, to: &str) -> Result<Vec<String>, String> {
    let listed = git(dir, &["rev-list", "--merges", &format!("{from}..{to}")]).await?;
    Ok(listed.lines().map(str::to_string).collect())
}

/// The local branches, as full ref names, that contain commit `oid`.
pub async fn branches_containing(dir: &Path, oid: &str) -> Result<Vec<String>, String> {
    if !is_full_object_id(oid) {
        return Err(format!("{oid:?} is not a full object ID"));
    }
    let listed = git(dir, &["for-each-ref", "--format=%(refname)", "--contains", oid, "refs/heads/"]).await?;
    Ok(listed.lines().map(str::to_string).collect())
}

/// Fetch `branch` from `origin` into `refs/remotes/origin/<branch>` and
/// return the object ID that ref then holds.
///
/// The refspec is spelled out, so nothing but that one remote-tracking ref
/// is updated and no tags come along. The result is read back by exact ref
/// name, not resolved as a revision. `branch` must already have passed
/// [`super::checked_task_branch`].
pub async fn fetch_remote_branch(dir: &Path, branch: &str) -> Result<String, String> {
    let tracking = format!("refs/remotes/origin/{branch}");
    let refspec = format!("+refs/heads/{branch}:{tracking}");
    git(dir, &["fetch", "--no-tags", "--quiet", "origin", &refspec])
        .await
        .map_err(|e| format!("Could not fetch {branch} from origin: {e}"))?;
    exact_ref(dir, &tracking)
        .await?
        .ok_or_else(|| format!("{tracking} does not exist after fetching {branch} from origin"))
}

/// What a task's worktree has in progress and uncommitted, as far as a
/// restack is concerned.
pub struct WorktreeState {
    /// The ref `HEAD` names, or `None` when it is detached.
    pub head_ref: Option<String>,
    /// The commit `HEAD` is at.
    pub head: String,
    /// A rebase, merge, cherry-pick or revert this worktree is in the middle
    /// of, by name.
    pub in_progress: Option<&'static str>,
    /// `git status --porcelain`, one entry per line, untracked files included.
    pub uncommitted: Vec<String>,
}

/// The path git keeps `name` at for the worktree `dir`: its own Git
/// directory for per-worktree state such as a rebase in progress.
async fn git_path(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(git(dir, &["rev-parse", "--git-path", name]).await?);
    Ok(if path.is_absolute() { path } else { dir.join(path) })
}

/// What operation the worktree at `dir` is in the middle of, if any.
async fn operation_in_progress(dir: &Path) -> Result<Option<&'static str>, String> {
    for (name, operation) in [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
    ] {
        if git_path(dir, name).await?.exists() {
            return Ok(Some(operation));
        }
    }
    Ok(None)
}

/// The [`WorktreeState`] of the worktree at `dir`, as its own `HEAD`, Git
/// directory and `git status` report it.
pub async fn worktree_state(dir: &Path) -> Result<WorktreeState, String> {
    let head_ref = run(dir, &["symbolic-ref", "-q", "HEAD"], &[]).await?;
    let head_ref = (head_ref.code == Some(0)).then_some(head_ref.stdout);
    let head = git(dir, &["rev-parse", "--verify", "HEAD^{commit}"]).await?;
    let in_progress = operation_in_progress(dir).await?;
    let uncommitted = git(dir, &["status", "--porcelain", "--untracked-files=all"])
        .await?
        .lines()
        .map(str::to_string)
        .collect();
    Ok(WorktreeState { head_ref, head, in_progress, uncommitted })
}

/// A rebase of `refs/heads/<branch>` left in progress in the worktree at
/// `dir`, as the branch tip it started from (its `orig-head`), or `None`.
/// A rebase of anything else is not reported here.
pub async fn interrupted_rebase_of(dir: &Path, branch: &str) -> Result<Option<String>, String> {
    let wanted = format!("refs/heads/{branch}");
    for name in ["rebase-merge", "rebase-apply"] {
        let state = git_path(dir, name).await?;
        let Ok(head_name) = std::fs::read_to_string(state.join("head-name")) else {
            continue;
        };
        if head_name.trim() == wanted {
            let orig_head = std::fs::read_to_string(state.join("orig-head")).unwrap_or_default();
            return Ok(Some(orig_head.trim().to_string()));
        }
    }
    Ok(None)
}

/// Abort the rebase in progress in the worktree at `dir`.
pub async fn abort_rebase(dir: &Path) -> Result<(), String> {
    git(dir, &["rebase", "--abort"]).await.map(|_| ())
}

/// Create the backup ref at `tip`, failing if it already exists.
pub async fn create_backup(dir: &Path, backup: &str, tip: &str) -> Result<(), String> {
    git(dir, &["update-ref", backup, tip, ""])
        .await
        .map(|_| ())
        .map_err(|e| format!("Could not write {backup}: {e}"))
}

/// Delete the backup ref, but only while it still holds `tip`.
pub async fn retire_backup(dir: &Path, backup: &str, tip: &str) -> Result<(), String> {
    git(dir, &["update-ref", "-d", backup, tip])
        .await
        .map(|_| ())
        .map_err(|e| format!("Could not delete {backup}: {e}"))
}

/// Whether the worktree at `dir` is back exactly where a restack found it:
/// on `refs/heads/<branch>`, which is at `old_tip`, with no rebase, merge,
/// cherry-pick or revert in progress. `Err` says what is not.
pub async fn verify_restored(dir: &Path, branch: &str, old_tip: &str) -> Result<(), String> {
    let branch_ref = format!("refs/heads/{branch}");
    let tip = exact_ref(dir, &branch_ref).await?;
    if tip.as_deref() != Some(old_tip) {
        return Err(format!("{branch_ref} is at {tip:?}, not {old_tip}"));
    }
    let state = worktree_state(dir).await?;
    if state.head_ref.as_deref() != Some(branch_ref.as_str()) || state.head != old_tip {
        return Err(format!(
            "the worktree's HEAD is {} at {}, not {branch_ref} at {old_tip}",
            state.head_ref.as_deref().unwrap_or("detached"),
            state.head
        ));
    }
    if let Some(operation) = state.in_progress {
        return Err(format!("the worktree is still in the middle of a {operation}"));
    }
    Ok(())
}

/// One restack: the task's commits `fork_point..old_tip` on
/// `refs/heads/<branch>`, replayed onto `onto` in the worktree `worktree`.
pub struct Restack<'a> {
    pub worktree: &'a Path,
    pub branch: &'a str,
    pub fork_point: &'a str,
    pub old_tip: &'a str,
    pub onto: &'a str,
    pub backup: &'a str,
}

/// Why [`Restack::replay`] did not produce a verified restacked branch.
pub enum RestackFailure {
    /// The branch and the worktree were verified to be back at the old tip,
    /// and the backup ref was retired (or, if deleting it failed, that is
    /// part of the reason). The `String` says what went wrong.
    Restored(String),
    /// What went wrong, and why the branch could not be verified as
    /// restored. The backup ref is kept, holding the old tip.
    NotRestored(String),
}

impl Restack<'_> {
    /// Replay the task's commits onto `onto` and verify the result.
    ///
    /// The rebase names every revision exactly: the new base and the fork
    /// point as object IDs, the branch by its exact name, and it turns off
    /// everything configuration could turn on behind the caller's back:
    /// setting uncommitted work aside (`--no-autostash`), moving other
    /// branches that point into the replayed range (`--no-update-refs`),
    /// guessing another fork point from the reflog (`--no-fork-point`),
    /// recreating merges, and reordering `fixup!` commits. The caller has
    /// already checked that the worktree is clean and has the branch
    /// checked out, and has written the backup ref.
    ///
    /// Git needs a committer identity to write the replayed commits. Where
    /// the repository has none, the committer of the branch's tip is used,
    /// the same way SlashIt's author rewrite keeps the tip's own committer
    /// rather than inventing one.
    ///
    /// Success is not taken from the exit status alone. The new tip must
    /// contain `onto`, `onto..<new tip>` must hold as many commits as
    /// `fork_point..old_tip`, and each replayed commit must change the same
    /// lines as its original, compared as `git patch-id --stable` over diffs
    /// with no context lines, so that a change `onto` made next to a task's
    /// line does not count as a difference while a dropped or altered change
    /// does. A commit that became empty against `onto` is dropped by the
    /// rebase and fails the count. Anything that does not verify is rolled
    /// back to the old tip.
    pub async fn replay(&self) -> Result<String, RestackFailure> {
        let envs = match committer_env(self.worktree, self.old_tip).await {
            Ok(envs) => envs,
            Err(e) => return Err(self.restore_now(format!("the rebase could not be started: {e}")).await),
        };
        let rebased = run(
            self.worktree,
            &[
                "rebase",
                "--no-autostash",
                "--no-update-refs",
                "--no-fork-point",
                "--no-rebase-merges",
                "--no-autosquash",
                "--quiet",
                "--onto",
                self.onto,
                self.fork_point,
                self.branch,
            ],
            &envs,
        )
        .await;
        let rebased = match rebased {
            Ok(ran) => ran,
            Err(e) => return Err(self.restore_now(format!("git rebase could not be run: {e}")).await),
        };
        if rebased.code != Some(0) {
            let conflicted = git(self.worktree, &["diff", "--name-only", "--diff-filter=U"])
                .await
                .unwrap_or_default();
            let what = if conflicted.is_empty() {
                format!("git rebase failed: {}", rebased.stderr)
            } else {
                format!("it stopped on a conflict in {}", conflicted.lines().collect::<Vec<_>>().join(", "))
            };
            return Err(self.restore_now(what).await);
        }

        match self.verify().await {
            Ok(new_tip) => Ok(new_tip),
            Err(why) => Err(self.restore_now(format!("the rebased branch did not verify: {why}")).await),
        }
    }

    /// The new tip, when the rebased branch holds exactly the task's commits
    /// on top of `onto`.
    async fn verify(&self) -> Result<String, String> {
        let branch_ref = format!("refs/heads/{}", self.branch);
        let new_tip = exact_ref(self.worktree, &branch_ref)
            .await?
            .ok_or_else(|| format!("{branch_ref} is gone"))?;
        if !is_ancestor(self.worktree, self.onto, &new_tip).await? {
            return Err(format!("{new_tip} does not contain {}", self.onto));
        }
        let before = commits_between(self.worktree, self.fork_point, self.old_tip).await?;
        let after = commits_between(self.worktree, self.onto, &new_tip).await?;
        if before.len() != after.len() {
            return Err(format!(
                "it holds {} commits on top of {} where the task had {}",
                after.len(),
                self.onto,
                before.len()
            ));
        }
        for (original, replayed) in before.iter().zip(&after) {
            if patch_id(self.worktree, original).await? != patch_id(self.worktree, replayed).await? {
                return Err(format!("{replayed} does not make the same change as {original}"));
            }
        }
        Ok(new_tip)
    }

    /// Put the branch back at the old tip after a rebase that ran, verify
    /// it, and say which [`RestackFailure`] that makes `what`.
    ///
    /// A rebase still in progress is aborted, which is how git itself
    /// restores the branch. A rebase that finished moved the branch, and
    /// `git reset --keep` in the worktree moves it back: the worktree was
    /// clean before the rebase and a finished rebase leaves it clean, and
    /// `--keep` refuses rather than discard anything if that is somehow no
    /// longer true. Only a verified restoration retires the backup ref.
    async fn restore_now(&self, what: String) -> RestackFailure {
        let undo = async {
            if operation_in_progress(self.worktree).await? == Some("rebase") {
                abort_rebase(self.worktree).await?;
            }
            let branch_ref = format!("refs/heads/{}", self.branch);
            if exact_ref(self.worktree, &branch_ref).await?.as_deref() != Some(self.old_tip) {
                git(self.worktree, &["reset", "--quiet", "--keep", self.old_tip]).await?;
            }
            verify_restored(self.worktree, self.branch, self.old_tip).await
        };
        if let Err(why) = undo.await {
            return RestackFailure::NotRestored(format!(
                "{what}, and the branch could not be verified as restored: {why}"
            ));
        }
        match retire_backup(self.worktree, self.backup, self.old_tip).await {
            Ok(()) => RestackFailure::Restored(what),
            Err(e) => RestackFailure::Restored(format!("{what} ({e})")),
        }
    }

    /// Roll a verified restack back, for a caller that could not record it:
    /// the branch goes back to the old tip, and the backup ref is retired
    /// once that is verified.
    pub async fn roll_back(&self, what: String) -> RestackFailure {
        self.restore_now(what).await
    }
}

/// The committer identity to give the rebase: nothing when git already has
/// one, and otherwise the committer of `tip`.
async fn committer_env(dir: &Path, tip: &str) -> Result<Vec<(&'static str, String)>, String> {
    let ident = run(dir, &["var", "GIT_COMMITTER_IDENT"], &[]).await?;
    if ident.code == Some(0) {
        return Ok(Vec::new());
    }
    let who = git(dir, &["show", "-s", "--format=%cn%x00%ce", tip]).await?;
    let (name, email) = who
        .split_once('\0')
        .ok_or_else(|| format!("could not read the committer of {tip}"))?;
    Ok(vec![("GIT_COMMITTER_NAME", name.to_string()), ("GIT_COMMITTER_EMAIL", email.to_string())])
}

/// `git patch-id --stable` of `commit`'s own change with no context lines,
/// or the empty string for a commit that changes nothing.
async fn patch_id(dir: &Path, commit: &str) -> Result<String, String> {
    let diff = tokio::process::Command::new("git")
        .args(["diff-tree", "-p", "-U0", "--no-color", "--no-ext-diff", "--no-renames", commit])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("Failed to run git diff-tree: {e}"))?;
    if !diff.status.success() {
        return Err(format!(
            "git diff-tree {commit} failed: {}",
            String::from_utf8_lossy(&diff.stderr).trim()
        ));
    }
    let mut child = tokio::process::Command::new("git")
        .args(["patch-id", "--stable"])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start git patch-id: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&diff.stdout)
            .await
            .map_err(|e| format!("Failed to write to git patch-id: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("Failed to finish git patch-id: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git patch-id failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}
