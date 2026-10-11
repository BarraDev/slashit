//! The Git side of integrating a Task's approved commit into a local branch.
//!
//! Everything here is Git plumbing that never touches a working tree, an
//! index or the Task branch:
//!
//! - [`plan`] decides how a target branch would reach an approved commit and
//!   what the result would be, writing at most unreferenced objects. A
//!   merge is computed with `git merge-tree --write-tree`, so conflicts are
//!   found before anything is modified.
//! - [`advance`] moves the target with a compare-and-swap `update-ref`, from
//!   the exact commit the plan was made against. A branch that is checked
//!   out anywhere is never advanced this way: `update-ref` would leave that
//!   checkout's index and files at the old commit, so that its next commit
//!   would silently undo the integration.
//! - [`observe`] says, from the target branch alone, whether a plan was
//!   applied, which is what crash recovery rests on.
//!
//! Nothing here pushes, fetches or deletes a branch.

use super::manager::{ListedWorktree, WorktreeManager};
use super::registry_lock::{run_git_with, Input};
use super::restack::{exact_ref, has_commit, is_ancestor};
use crate::domain::IntegrationStrategy;
use std::path::Path;

/// Where a plan's merge commit is pinned until the target has moved, so
/// Git's garbage collection cannot take it between the plan and the move.
pub fn pin_ref(task_id: impl std::fmt::Display) -> String {
    format!("refs/slashit/integration/{task_id}")
}

const PIN_PREFIX: &str = "refs/slashit/integration/";

/// The marker that says SlashIt has integrated work into local branch
/// `branch` of this repository. It lives in the repository, so it survives
/// the loss of any Task or project record, and `default_base` consults it
/// before letting a remote's branch of the same name become a new Task's
/// base (see [`super::default_base::resolve_default_base`]).
pub fn marker_ref(branch: &str) -> String {
    format!("refs/slashit/integrated-base/{branch}")
}

/// The commits the plan relies on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub target_branch: String,
    pub target_old: String,
    pub approved_tip: String,
    /// What the target will name. For [`IntegrationStrategy::AlreadyContained`]
    /// this is the target's current commit and nothing moves.
    pub result: String,
    pub strategy: IntegrationStrategy,
}

/// Why no plan could be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The two lines changed the same paths incompatibly. Nothing was
    /// modified.
    Conflicts(Vec<String>),
    Refused(String),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflicts(paths) => write!(
                f,
                "Merging the approved changes into the target branch would conflict in: {}. \
                 Nothing was changed.",
                paths.join(", ")
            ),
            Self::Refused(why) => f.write_str(why),
        }
    }
}

impl From<String> for PlanError {
    fn from(why: String) -> Self {
        Self::Refused(why)
    }
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run(dir: &Path, args: &[&str]) -> Result<Ran, String> {
    let output = run_git_with(
        "git".into(),
        &dir.to_string_lossy(),
        args,
        &[("GIT_TERMINAL_PROMPT", "0".to_string())],
        Input::None,
    )
    .await
    .map_err(|e| format!("Failed to run git: {e}"))?
    .output;
    Ok(Ran {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let ran = run(dir, args).await?;
    if ran.code != Some(0) {
        return Err(format!("git {} failed: {}", args.first().unwrap_or(&""), ran.stderr));
    }
    Ok(ran.stdout)
}

fn head_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

/// The path of the checkout that has `branch` checked out, if any. A
/// Jujutsu-colocated repository, whose Git `HEAD` is normally detached, has
/// none.
pub async fn checked_out_at(repo_path: &str, branch: &str) -> Result<Option<String>, String> {
    let listing = WorktreeManager::worktree_listing(repo_path).await?;
    let wanted = head_ref(branch);
    Ok(ListedWorktree::parse(&listing)
        .into_iter()
        .find(|record| record.branch == Some(wanted.as_str()))
        .map(|record| record.path.to_string()))
}

/// The tip of local branch `branch`, by exact ref name.
pub async fn branch_tip(repo: &Path, branch: &str) -> Result<Option<String>, String> {
    exact_ref(repo, &head_ref(branch)).await
}

/// Plan how local branch `target_branch` reaches `approved_tip`.
///
/// `message` is the merge commit's message, used only when a merge commit is
/// needed. It is made with the repository's configured identity; a
/// repository with none is refused, never given an invented one.
///
/// With `write_commit` false the merge is still computed, and its conflicts
/// reported, but no commit object is created and a merge plan's `result` is
/// empty: that is a preview, which must not leave objects behind each time
/// it is looked at.
pub async fn plan(
    repo: &Path,
    target_branch: &str,
    approved_tip: &str,
    message: &str,
    write_commit: bool,
) -> Result<Plan, PlanError> {
    super::checked_base_branch(target_branch).map_err(PlanError::Refused)?;
    let target_old = branch_tip(repo, target_branch).await?.ok_or_else(|| {
        PlanError::Refused(format!("The target branch {target_branch} does not exist in this repository."))
    })?;
    if !has_commit(repo, approved_tip).await? {
        return Err(PlanError::Refused(format!(
            "The approved commit {approved_tip} is not in this repository any more."
        )));
    }
    let plan = |result: String, strategy| Plan {
        target_branch: target_branch.to_string(),
        target_old: target_old.clone(),
        approved_tip: approved_tip.to_string(),
        result,
        strategy,
    };
    if is_ancestor(repo, approved_tip, &target_old).await? {
        return Ok(plan(target_old.clone(), IntegrationStrategy::AlreadyContained));
    }
    if is_ancestor(repo, &target_old, approved_tip).await? {
        return Ok(plan(approved_tip.to_string(), IntegrationStrategy::FastForward));
    }

    let merged = run(repo, &["merge-tree", "--write-tree", "--name-only", &target_old, approved_tip]).await?;
    let tree = match merged.code {
        Some(0) => merged.stdout.lines().next().unwrap_or_default().to_string(),
        Some(1) => {
            // The tree id, then the conflicted paths, then a blank line and
            // Git's own messages.
            let paths: Vec<String> = merged
                .stdout
                .lines()
                .skip(1)
                .take_while(|line| !line.is_empty())
                .map(str::to_string)
                .collect();
            return Err(PlanError::Conflicts(paths));
        }
        _ => {
            return Err(PlanError::Refused(format!(
                "Git could not compute the merge ({}). Integrating needs Git 2.38 or newer, and \
                 branches that share history.",
                merged.stderr
            )))
        }
    };
    if !super::restack::is_full_object_id(&tree) {
        return Err(PlanError::Refused(format!("Git returned {tree:?} instead of a merge tree.")));
    }
    if !write_commit {
        return Ok(plan(String::new(), IntegrationStrategy::MergeCommit));
    }
    let commit = run(
        repo,
        &["commit-tree", &tree, "-p", &target_old, "-p", approved_tip, "-m", message],
    )
    .await?;
    if commit.code != Some(0) || !super::restack::is_full_object_id(&commit.stdout) {
        return Err(PlanError::Refused(format!(
            "The merge commit could not be created: {}. Set user.name and user.email for this \
             repository, then try again.",
            commit.stderr
        )));
    }
    Ok(plan(commit.stdout, IntegrationStrategy::MergeCommit))
}

/// The branch of origin's default (`D` of `origin/D`), when the repository
/// names one, for a project that has no local base branch of its own.
pub async fn remote_default_branch(repo_path: &str) -> Result<Option<String>, String> {
    Ok(super::default_base::resolve_remote_base(Path::new(repo_path), repo_path)
        .await?
        .map(|base| base.branch))
}

/// Pin `result` for task `task_id` so the commit outlives garbage collection.
pub async fn pin(repo: &Path, task_id: impl std::fmt::Display, result: &str) -> Result<(), String> {
    git(repo, &["update-ref", &pin_ref(task_id), result]).await.map(|_| ())
}

/// Remove the pin for `task_id`. A pin that is not there is not an error.
pub async fn unpin(repo: &Path, task_id: impl std::fmt::Display) -> Result<(), String> {
    let name = pin_ref(task_id);
    if exact_ref(repo, &name).await?.is_some() {
        git(repo, &["update-ref", "-d", &name]).await?;
    }
    Ok(())
}

/// The task ids that have a pin in this repository.
pub async fn pinned(repo: &Path) -> Result<Vec<String>, String> {
    let listed = git(repo, &["for-each-ref", "--format=%(refname)", PIN_PREFIX]).await?;
    Ok(listed.lines().filter_map(|line| line.strip_prefix(PIN_PREFIX)).map(str::to_string).collect())
}

/// Record that SlashIt integrates into local branch `branch` here. Never
/// overwrites an existing marker.
pub async fn mark_base(repo: &Path, branch: &str, at: &str) -> Result<(), String> {
    let name = marker_ref(branch);
    if exact_ref(repo, &name).await?.is_none() {
        git(repo, &["update-ref", &name, at]).await?;
    }
    Ok(())
}

pub async fn has_marker(repo: &Path, branch: &str) -> Result<bool, String> {
    Ok(exact_ref(repo, &marker_ref(branch)).await?.is_some())
}

/// Move the target branch to the plan's result, only if it still names the
/// commit the plan was made against. Refuses when the branch is checked out
/// anywhere. An [`IntegrationStrategy::AlreadyContained`] plan moves
/// nothing.
pub async fn advance(repo_path: &str, plan: &Plan) -> Result<(), String> {
    if plan.strategy == IntegrationStrategy::AlreadyContained {
        return Ok(());
    }
    if let Some(path) = checked_out_at(repo_path, &plan.target_branch).await? {
        return Err(checked_out_message(&plan.target_branch, &path, &manual_merge(plan.strategy, &plan.result)));
    }
    let message = format!("slashit: integrate {} into {}", short(&plan.approved_tip), plan.target_branch);
    git(
        Path::new(repo_path),
        &["update-ref", "-m", &message, &head_ref(&plan.target_branch), &plan.result, &plan.target_old],
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("The branch {} was not moved: {e}", plan.target_branch))
}

/// `command` is the `git merge` invocation, with its arguments, to run in
/// the checkout that has the branch (see [`manual_merge`]).
pub fn checked_out_message(branch: &str, path: &str, command: &str) -> String {
    format!(
        "The branch {branch} is checked out in {path}, and SlashIt does not change a checked-out \
         branch. Nothing was changed. To integrate it yourself, run: git -C {path} {command}"
    )
}

/// The merge a person can run themselves in the checkout that has the target:
/// a fast-forward onto `commit` when `strategy` is one (`--ff-only` would
/// fail for a diverged branch), otherwise a merge of it.
pub fn manual_merge(strategy: IntegrationStrategy, commit: &str) -> String {
    match strategy {
        IntegrationStrategy::MergeCommit => format!("merge --no-ff {commit}"),
        _ => format!("merge --ff-only {commit}"),
    }
}

/// What the target branch says about a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    /// The target holds the plan's result (or something that contains it).
    Applied,
    /// The target is exactly where the plan found it.
    Unmoved,
    /// The target is somewhere else, and does not hold the result.
    Moved,
    /// The target branch is gone.
    Missing,
}

pub async fn observe(repo: &Path, plan: &Plan) -> Result<Observed, String> {
    let Some(now) = branch_tip(repo, &plan.target_branch).await? else {
        return Ok(Observed::Missing);
    };
    let held = match plan.strategy {
        IntegrationStrategy::AlreadyContained => is_ancestor(repo, &plan.approved_tip, &now).await?,
        _ => has_commit(repo, &plan.result).await? && is_ancestor(repo, &plan.result, &now).await?,
    };
    Ok(if held {
        Observed::Applied
    } else if now == plan.target_old {
        Observed::Unmoved
    } else {
        Observed::Moved
    })
}

/// Local `branch` against `origin/<branch>`: `Some(why)` when integrating
/// into it would build on a line that is behind or has diverged from
/// origin's. A repository with no such remote branch has nothing to compare.
pub async fn not_behind_origin(repo: &Path, branch: &str) -> Result<Option<String>, String> {
    let Some(local) = branch_tip(repo, branch).await? else { return Ok(None) };
    let Some(remote) = exact_ref(repo, &format!("refs/remotes/origin/{branch}")).await? else {
        return Ok(None);
    };
    if is_ancestor(repo, &remote, &local).await? {
        return Ok(None);
    }
    Ok(Some(format!(
        "The branch {branch} does not contain everything origin/{branch} has, so integrating into \
         it would build on an out-of-date line. Update {branch} first (for example `git fetch` and \
         `git merge --ff-only origin/{branch}` where it is checked out); SlashIt does not do that \
         for you."
    )))
}

/// The first twelve characters of an object id, for messages.
pub fn short(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

/// The subject lines of the commits in `from..to`, oldest first, at most
/// `limit` of them.
pub async fn subjects(repo: &Path, from: &str, to: &str, limit: usize) -> Result<Vec<String>, String> {
    let range = format!("{from}..{to}");
    let listed = git(repo, &["log", "--reverse", "--format=%h %s", &format!("-n{limit}"), &range]).await?;
    Ok(listed.lines().map(str::to_string).collect())
}

/// How many commits `to` has that `from` does not.
pub async fn count_between(repo: &Path, from: &str, to: &str) -> Result<u32, String> {
    let n = git(repo, &["rev-list", "--count", &format!("{from}..{to}")]).await?;
    n.parse().map_err(|_| format!("git rev-list --count answered {n:?}"))
}
