//! The Git side of integrating a Task's approved commit into a local branch.
//!
//! Everything here is Git plumbing that never touches a working tree, an
//! index or the Task branch, except [`advance`] into a checkout the person
//! consented to:
//!
//! - [`plan`] decides how a target branch would reach an approved commit and
//!   what the result would be, writing at most unreferenced objects. A
//!   merge is computed with `git merge-tree --write-tree`, so conflicts are
//!   found before anything is modified.
//! - [`advance`] moves the target with a compare-and-swap `update-ref`, from
//!   the exact commit the plan was made against. A branch that is checked
//!   out anywhere is never advanced this way: `update-ref` would leave that
//!   checkout's index and files at the old commit, so that its next commit
//!   would silently undo the integration. A checked-out target moves only
//!   with the person's consent to that exact checkout, through Git's own
//!   `merge --ff-only` run in it, which updates the branch, the index and
//!   the files together (see [`inspect_checkout`]).
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
    /// For a merge computed without creating its commit (a preview), the
    /// tree the merge commit would have. A plan read back from a record has
    /// the commit and no tree.
    pub merge_tree: Option<String>,
}

impl Plan {
    /// What the target's files would be after the plan: its result, or the
    /// merge tree of a preview, or, for a fast-forward, the approved commit.
    fn resulting_tree_ish(&self) -> &str {
        if !self.result.is_empty() {
            &self.result
        } else {
            self.merge_tree.as_deref().unwrap_or(&self.approved_tip)
        }
    }
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
    run_env(dir, args, &[]).await
}

/// [`run`] with more environment for the one `git`. The Git configuration
/// the process inherits is the user's; a test that must not depend on it
/// passes its own here.
async fn run_env(dir: &Path, args: &[&str], envs: &[(&str, String)]) -> Result<Ran, String> {
    let mut all = vec![("GIT_TERMINAL_PROMPT", "0".to_string())];
    all.extend(envs.iter().map(|(k, v)| (*k, v.clone())));
    let output = run_git_with("git".into(), &dir.to_string_lossy(), args, &all, Input::None)
    .await
    .map_err(|e| format!("Failed to run git: {e}"))?
    .output;
    Ok(Ran {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// `git`'s standard output exactly as written, for output that is parsed
/// (`-z` lists), where trimming could change the first or last name.
async fn git_raw(dir: &Path, args: &[&str]) -> Result<String, String> {
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
    if output.status.code() != Some(0) {
        return Err(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
    let records = ListedWorktree::parse(&listing);
    if let Some(record) = records.iter().find(|record| record.branch == Some(wanted.as_str())) {
        return Ok(Some(record.path.to_string()));
    }
    // Git reports a checkout in the middle of a rebase or bisect as detached,
    // though the branch is still that checkout's: moving the branch under it
    // would break the operation. Such a checkout counts as having it.
    for record in records.iter().filter(|record| record.branch.is_none()) {
        // A registration whose directory is gone (not yet pruned, or on an
        // unmounted volume) has no operation under way that could be broken.
        if !Path::new(record.path).is_dir() {
            continue;
        }
        if holds_branch_under_way(Path::new(record.path), &wanted).await? {
            return Ok(Some(record.path.to_string()));
        }
    }
    Ok(None)
}

/// Whether the detached checkout at `dir` is rebasing or bisecting `wanted`.
async fn holds_branch_under_way(dir: &Path, wanted: &str) -> Result<bool, String> {
    for name in ["rebase-merge/head-name", "rebase-apply/head-name", "BISECT_START"] {
        let located = git(dir, &["rev-parse", "--git-path", name]).await?;
        let located = if Path::new(&located).is_absolute() { Path::new(&located).to_path_buf() } else { dir.join(&located) };
        let Ok(contents) = std::fs::read_to_string(&located) else { continue };
        let named = contents.trim();
        if named == wanted || named == wanted.trim_start_matches("refs/heads/") {
            return Ok(true);
        }
    }
    Ok(false)
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
    plan_with(repo, target_branch, approved_tip, message, write_commit, &[]).await
}

/// [`plan`] with `envs` added to the environment of the `git` that creates
/// the merge commit, which is the one run that needs a Git identity.
pub async fn plan_with(
    repo: &Path,
    target_branch: &str,
    approved_tip: &str,
    message: &str,
    write_commit: bool,
    envs: &[(&str, String)],
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
        merge_tree: None,
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
        return Ok(Plan { merge_tree: Some(tree), ..plan(String::new(), IntegrationStrategy::MergeCommit) });
    }
    let commit = run_env(
        repo,
        &["commit-tree", &tree, "-p", &target_old, "-p", approved_tip, "-m", message],
        envs,
    )
    .await?;
    if commit.code != Some(0) || !super::restack::is_full_object_id(&commit.stdout) {
        return Err(PlanError::Refused(format!(
            "The merge commit could not be created: {}. Set user.name and user.email for this \
             repository (git config user.name and git config user.email), then try again. \
             Nothing was changed.",
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

/// `text` as one word of a POSIX shell command line, for the commands
/// SlashIt suggests a person run. Words made only of characters no shell
/// treats specially are left as they are.
pub fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '+' | '=' | '@' | '%' | ','));
    if plain {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Move the target branch to the plan's result, only if it still names the
/// commit the plan was made against.
///
/// A branch checked out nowhere moves with a compare-and-swap `update-ref`.
/// One checked out in a checkout moves only when `consented_checkout` is
/// that checkout's path, which is what a person confirmed after being shown
/// it; then it moves with Git's own fast-forward in that checkout, so the
/// branch, the index and the files change together (see
/// [`advance_checked_out`]). Any other checked-out branch is refused,
/// whatever the plan. An [`IntegrationStrategy::AlreadyContained`] plan
/// moves nothing.
pub async fn advance(repo_path: &str, plan: &Plan, consented_checkout: Option<&str>) -> Result<(), String> {
    if plan.strategy == IntegrationStrategy::AlreadyContained {
        return Ok(());
    }
    if let Some(path) = checked_out_at(repo_path, &plan.target_branch).await? {
        if consented_checkout != Some(path.as_str()) {
            return Err(checked_out_message(&plan.target_branch, &path, &manual_merge(plan.strategy, &plan.approved_tip)));
        }
        return advance_checked_out(&path, plan).await;
    }
    update_ref(Path::new(repo_path), plan).await
}

async fn update_ref(repo: &Path, plan: &Plan) -> Result<(), String> {
    let message = format!("slashit: integrate {} into {}", short(&plan.approved_tip), plan.target_branch);
    git(repo, &["update-ref", "-m", &message, &head_ref(&plan.target_branch), &plan.result, &plan.target_old])
        .await
        .map(|_| ())
        .map_err(|e| format!("The branch {} was not moved: {e}", plan.target_branch))
}

/// What a checkout that has the target branch checked out allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutReadiness {
    /// Clean, at the commit the plan was made against, nothing under way:
    /// Git can fast-forward it.
    Ready,
    /// A previous attempt got Git to the point of updating the index and the
    /// files to the plan's result, and stopped before moving the branch.
    /// Moving the branch is all that is left, and loses nothing, since the
    /// checkout already holds exactly the result.
    FilesAlreadyUpdated,
    /// It cannot be integrated into as it is; each entry says why in words a
    /// person can act on.
    Blocked(Vec<String>),
}

/// The most paths named in one message.
const LISTED_PATHS: usize = 8;

fn listed(paths: &[String]) -> String {
    let mut text = paths.iter().take(LISTED_PATHS).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > LISTED_PATHS {
        text.push_str(&format!(" and {} more", paths.len() - LISTED_PATHS));
    }
    text
}

/// States of a repository in which `git merge` must not be started.
const UNDER_WAY: &[(&str, &str)] = &[
    ("MERGE_HEAD", "a merge is in progress"),
    ("CHERRY_PICK_HEAD", "a cherry-pick is in progress"),
    ("REVERT_HEAD", "a revert is in progress"),
    ("rebase-merge", "a rebase is in progress"),
    ("rebase-apply", "a rebase or patch application is in progress"),
    ("sequencer", "a cherry-pick or revert sequence is in progress"),
    ("BISECT_LOG", "a bisect is in progress"),
    ("index.lock", "another Git command is using this checkout (index.lock exists)"),
];

/// Whether the checkout at `path`, which has the plan's target branch
/// checked out, can have the plan applied to it, from Git alone. Changes
/// nothing.
///
/// Everything the plan relied on must still hold: `HEAD` is attached to the
/// target branch and at the commit the plan was made against, no Git
/// operation is under way, the index and the tracked files are clean, and no
/// untracked or ignored file stands where the integration would create one.
/// Untracked files that are not in the way are left alone and are no reason
/// to refuse.
pub async fn inspect_checkout(path: &str, plan: &Plan) -> Result<CheckoutReadiness, String> {
    let dir = Path::new(path);
    let mut blocked = Vec::new();

    let attached = run(dir, &["symbolic-ref", "-q", "HEAD"]).await?;
    if attached.stdout != head_ref(&plan.target_branch) {
        return Ok(CheckoutReadiness::Blocked(vec![format!(
            "{path} no longer has {} checked out.",
            plan.target_branch
        )]));
    }
    let head = git(dir, &["rev-parse", "HEAD"]).await?;
    if head != plan.target_old {
        return Ok(CheckoutReadiness::Blocked(vec![format!(
            "{path} is at {} now, not at {}, where the integration was planned from.",
            short(&head),
            short(&plan.target_old)
        )]));
    }
    for (name, meaning) in UNDER_WAY {
        let located = git(dir, &["rev-parse", "--git-path", name]).await?;
        let located = if Path::new(&located).is_absolute() { Path::new(&located).to_path_buf() } else { dir.join(&located) };
        if located.exists() {
            blocked.push(format!("In {path}, {meaning}. Finish or abort it first."));
        }
    }
    if !blocked.is_empty() {
        return Ok(CheckoutReadiness::Blocked(blocked));
    }

    // Every tracked path that differs from `HEAD`, in the index or in the
    // files, including paths with an unresolved conflict.
    let differing = git_raw(dir, &["diff", "--name-only", "-z", "--ignore-submodules=none", "HEAD"]).await?;
    let changed: Vec<String> = differing.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect();
    if !changed.is_empty() {
        let result_tree_in_index =
            run(dir, &["diff", "--cached", "--quiet", plan.resulting_tree_ish()]).await?.code == Some(0);
        let files_match_index = run(dir, &["diff", "--quiet"]).await?.code == Some(0);
        if result_tree_in_index && files_match_index && !plan.result.is_empty() && plan.result != plan.target_old {
            return Ok(CheckoutReadiness::FilesAlreadyUpdated);
        }
        return Ok(CheckoutReadiness::Blocked(vec![format!(
            "{path} has uncommitted changes to tracked files ({}). Commit or stash them first; \
             SlashIt will not touch them.",
            listed(&changed)
        )]));
    }

    let added = git_raw(dir, &["diff", "--no-renames", "--name-only", "-z", "--diff-filter=A", &plan.target_old, plan.resulting_tree_ish()]).await?;
    let mut in_the_way: Vec<String> = Vec::new();
    for path in added.split('\0').filter(|p| !p.is_empty()) {
        if is_in_the_way(dir, path).await? {
            in_the_way.push(path.to_string());
        }
    }
    if !in_the_way.is_empty() {
        return Ok(CheckoutReadiness::Blocked(vec![format!(
            "{path} has files that are not tracked where the integration would create files: {}. \
             Move or remove them first; SlashIt will not overwrite them.",
            listed(&in_the_way)
        )]));
    }
    Ok(CheckoutReadiness::Ready)
}

/// Whether something that is not tracked exists at `relative` in `checkout`,
/// or an untracked file stands where one of its directories would be
/// created. What is tracked is the task's own file-to-directory or
/// directory-to-file replacement, which Git makes cleanly.
async fn is_in_the_way(checkout: &Path, relative: &str) -> Result<bool, String> {
    let full = checkout.join(relative);
    let mut candidates = Vec::new();
    if std::fs::symlink_metadata(&full).is_ok() {
        candidates.push(relative.to_string());
    }
    for ancestor in full.ancestors().skip(1) {
        if ancestor == checkout || !ancestor.starts_with(checkout) {
            break;
        }
        if std::fs::symlink_metadata(ancestor).is_ok_and(|m| !m.is_dir()) {
            if let Ok(rel) = ancestor.strip_prefix(checkout) {
                candidates.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    for candidate in candidates {
        let pathspec = format!(":(literal){candidate}");
        let tracked = git_raw(checkout, &["ls-files", "-z", "--", &pathspec]).await?;
        if tracked.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Apply the plan in the checkout that has its target branch checked out.
///
/// Git does it: `merge --ff-only` to the plan's result, which for a merge
/// plan is the merge commit already made. Git moves the branch, the index
/// and the files itself, refuses to overwrite a change it was not asked to,
/// and SlashIt never resets, forces or discards anything. Everything the
/// plan relied on is checked first, as [`inspect_checkout`] says, and Git's
/// answer is checked after.
///
/// If Git is interrupted after updating the files and before moving the
/// branch, the checkout holds the result with the branch still at the old
/// commit. The next attempt recognises exactly that and finishes by moving
/// the branch. Any other half-done state is a refusal that names the
/// changes, and nothing is overwritten.
async fn advance_checked_out(path: &str, plan: &Plan) -> Result<(), String> {
    let dir = Path::new(path);
    match inspect_checkout(path, plan).await? {
        CheckoutReadiness::Blocked(why) => return Err(format!("{} Nothing was changed.", why.join(" "))),
        CheckoutReadiness::FilesAlreadyUpdated => return update_ref(dir, plan).await,
        CheckoutReadiness::Ready => {}
    }
    let merged = run_env(
        dir,
        &["merge", "--ff-only", "--no-edit", "--quiet", &plan.result],
        &[("GIT_MERGE_AUTOEDIT", "no".to_string())],
    )
    .await?;
    // What matters is where the branch is, not Git's exit code: a hook that
    // fails after the move does not undo it.
    if branch_tip(Path::new(path), &plan.target_branch).await?.as_deref() == Some(plan.result.as_str()) {
        return Ok(());
    }
    Err(format!(
        "Git did not fast-forward {} in {path}: {}. If it left changes in that checkout, they are \
         still there and were not discarded; run git status in it to see them.",
        plan.target_branch,
        if merged.stderr.is_empty() { "it reported no reason" } else { merged.stderr.as_str() }
    ))
}

/// The command line that runs `command` (see [`manual_merge`]) in the
/// checkout at `path`, quoted for a POSIX shell.
pub fn manual_command(path: &str, command: &str) -> String {
    format!("git -C {} {command}", shell_quote(path))
}

/// `command` is the `git merge` invocation, with its arguments, to run in
/// the checkout that has the branch (see [`manual_merge`]).
pub fn checked_out_message(branch: &str, path: &str, command: &str) -> String {
    format!(
        "The branch {branch} is checked out in {path}, so SlashIt does not move it unless you \
         confirm that checkout. Nothing was changed. To integrate it yourself, run: {}",
        manual_command(path, command)
    )
}

/// The merge a person can run themselves in the checkout that has the target:
/// a fast-forward onto `commit` when `strategy` is one (`--ff-only` would
/// fail for a diverged branch), otherwise a merge of it.
pub fn manual_merge(strategy: IntegrationStrategy, commit: &str) -> String {
    let commit = shell_quote(commit);
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

/// Whether the repository has a remote named origin. Nothing is contacted.
pub async fn has_origin(repo: &Path) -> bool {
    run(repo, &["remote", "get-url", "origin"]).await.is_ok_and(|ran| ran.code == Some(0))
}

/// What `origin/<branch>` named when the repository last saw it, if it does.
pub async fn remote_tracking_tip(repo: &Path, branch: &str) -> Result<Option<String>, String> {
    exact_ref(repo, &format!("refs/remotes/origin/{branch}")).await
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

/// The paths `to` changes relative to `from`, each as `<status> <path>`
/// (`A` added, `M` modified, `D` deleted), at most `limit` of them.
pub async fn changed_files(repo: &Path, from: &str, to: &str, limit: usize) -> Result<Vec<String>, String> {
    let listed = git(repo, &["diff", "--no-renames", "--name-status", from, to]).await?;
    Ok(listed.lines().take(limit).map(|line| line.replacen('\t', " ", 1)).collect())
}

/// How many commits `to` has that `from` does not.
pub async fn count_between(repo: &Path, from: &str, to: &str) -> Result<u32, String> {
    let n = git(repo, &["rev-list", "--count", &format!("{from}..{to}")]).await?;
    n.parse().map_err(|_| format!("git rev-list --count answered {n:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Git with none of the developer's configuration, so a test says what it
    /// depends on.
    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Text that every shell treats specially in some way.
    const AWKWARD: &[&str] = &[
        "plain",
        "with space",
        "it's",
        "a;b",
        "$(touch pwned)",
        "`touch pwned`",
        "a&&b|c>d<e",
        "*?[x]~#!",
        "tab\there",
        "quote\"s",
        "back\\slash",
        "new\nline",
        "",
    ];

    #[test]
    fn quoted_text_reaches_a_shell_as_exactly_one_word() {
        let dir = tempfile::tempdir().unwrap();
        for text in AWKWARD {
            let quoted = shell_quote(text);
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {quoted}"))
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), *text, "{text:?} quoted as {quoted}");
        }
        assert!(!dir.path().join("pwned").exists(), "no quoted word was run");
    }

    #[test]
    fn plain_words_are_left_alone() {
        assert_eq!(shell_quote("/home/me/repo-1.0/a_b"), "/home/me/repo-1.0/a_b");
        assert_eq!(shell_quote("deadbeef"), "deadbeef");
        assert_eq!(shell_quote(""), "''");
    }

    /// The command SlashIt suggests for a checked-out target runs, as given,
    /// in a shell, for a checkout whose path has spaces and metacharacters.
    #[tokio::test]
    async fn the_suggested_command_works_for_a_path_with_spaces_and_metacharacters() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("my repo; $(touch pwned) 'x' `y` & z");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q", "-b", "main"]);
        let commit = |msg: &str| {
            git_in(&repo, &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "--allow-empty", "-m", msg]);
            git_in(&repo, &["rev-parse", "HEAD"])
        };
        commit("init");
        git_in(&repo, &["checkout", "-q", "-b", "task"]);
        let tip = commit("work");
        git_in(&repo, &["checkout", "-q", "main"]);

        let message = checked_out_message("main", repo.to_str().unwrap(), &manual_merge(IntegrationStrategy::FastForward, &tip));
        let command = message.rsplit_once("run: ").expect("the message ends with the command").1.to_string();
        assert!(command.starts_with("git -C '"), "{command}");

        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{command}: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(git_in(&repo, &["rev-parse", "main"]), tip, "the command merged the work");
        assert!(!tmp.path().join("pwned").exists() && !repo.join("pwned").exists(), "the path was not executed");
    }

    /// A merge commit needs an identity. With none configured, in an
    /// environment that has no global or system Git configuration, planning
    /// is refused with what to do, and nothing is created or moved.
    #[tokio::test]
    async fn a_merge_commit_without_a_git_identity_is_refused_and_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q", "-b", "main"]);
        // Never guess an identity from the host name or the account.
        git_in(&repo, &["config", "user.useConfigOnly", "true"]);
        let commit = |file: &str| {
            std::fs::write(repo.join(file), file).unwrap();
            git_in(&repo, &["add", "."]);
            git_in(&repo, &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", file]);
            git_in(&repo, &["rev-parse", "HEAD"])
        };
        let base = commit("base.txt");
        git_in(&repo, &["checkout", "-q", "-b", "task"]);
        let task = commit("task.txt");
        git_in(&repo, &["checkout", "-q", "main"]);
        let main = commit("main.txt");
        git_in(&repo, &["checkout", "-q", "--detach"]);

        // The identity may also come from the environment; this run has none,
        // and no global or system configuration to supply one either.
        let isolated = [
            ("GIT_CONFIG_GLOBAL", "/dev/null".to_string()),
            ("GIT_CONFIG_NOSYSTEM", "1".to_string()),
            ("HOME", tmp.path().to_string_lossy().to_string()),
            ("GIT_AUTHOR_NAME", String::new()),
            ("GIT_AUTHOR_EMAIL", String::new()),
            ("GIT_COMMITTER_NAME", String::new()),
            ("GIT_COMMITTER_EMAIL", String::new()),
        ];
        let refused = plan_with(&repo, "main", &task, "merge", true, &isolated).await.unwrap_err();
        let PlanError::Refused(why) = refused else { panic!("expected a refusal, not a conflict") };
        assert!(why.contains("merge commit could not be created"), "{why}");
        assert!(why.contains("user.name") && why.contains("user.email") && why.contains("Nothing was changed"), "{why}");

        assert_eq!(git_in(&repo, &["rev-parse", "main"]), main, "the target did not move");
        assert_eq!(git_in(&repo, &["rev-parse", "task"]), task);
        // The merge's tree may remain as an unreferenced object; a commit may not.
        let dangling = git_in(&repo, &["fsck", "--no-reflogs", "--lost-found"]);
        assert!(!dangling.contains("dangling commit"), "no commit was created: {dangling}");
        let _ = base;

        // A preview needs no identity: it creates no commit.
        let preview = plan_with(&repo, "main", &task, "merge", false, &isolated).await.unwrap();
        assert_eq!(preview.strategy, IntegrationStrategy::MergeCommit);
        assert!(preview.result.is_empty() && preview.merge_tree.is_some());

        // With an identity the same plan is made.
        git_in(&repo, &["config", "user.name", "Test"]);
        git_in(&repo, &["config", "user.email", "test@example.com"]);
        let configured = &isolated[..3];
        let made = plan_with(&repo, "main", &task, "merge", true, configured).await.unwrap();
        assert!(!made.result.is_empty());
    }
}
