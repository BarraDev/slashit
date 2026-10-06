//! Replaying a task's own commits onto a new base, in the task's Git worktree.
//!
//! This is the Git half of restacking a stacked task whose parent has landed
//! on the default branch (see `commands::pr`, which decides whether a restack
//! is proven safe and records its result). Everything here takes object IDs
//! and fully qualified ref names only, never a name git would look up in
//! several namespaces, and never runs anything that could set work aside,
//! move another branch, or reach the remote. The one step that discards
//! worktree state, `git read-tree --reset -u` when a stopped rebase is
//! undone, runs only after [`changes_beyond_the_stop`] found nothing there
//! but what the stop itself left.
//!
//! The restack itself is `git rebase --onto <onto> <fork point> <branch>`,
//! run in the worktree that has the branch checked out. Git moves the branch
//! ref only once every commit has been replayed, and keeps the state of a
//! rebase in progress in the worktree's own Git directory. `ORIG_HEAD` is
//! not a durable record of where the branch was, so before the rebase the
//! old tip is written to a backup ref under `refs/slashit/restack-backup/`
//! (created only if absent, and deleted only while it still holds the old
//! tip), which every worktree of the repository shares and which jj neither
//! imports nor rewrites. Undoing a restack writes nothing else but:
//!
//! - for a rebase that stopped part way, which never moved the branch,
//!   `git rebase --quit`, `git symbolic-ref HEAD refs/heads/<branch>` and
//!   `git read-tree --reset -u <old tip>`, none of which writes the branch;
//! - for a rebase that finished, a compare-and-swap of the branch ref from
//!   the tip the rebase produced back to the old tip (`git update-ref`),
//!   then `git read-tree -m -u` to bring the index and files along.
//!
//! SlashIt's own flows are kept out of the worktree while a restack runs, but
//! the user is not: a commit from the task's terminal, or another tool, can
//! move the branch or the worktree's `HEAD` at any moment. So the rebase
//! marks every ref update it makes with a reflog action unique to this
//! restack (`slashit-restack/<nonce>`, see [`REFLOG_ACTION_PREFIX`]), and a
//! rollback only ever undoes a state that the reflogs prove this restack
//! produced and nothing moved since. A branch or `HEAD` that moved otherwise
//! is left exactly as it is, with the backup ref kept
//! ([`RestackFailure::BranchMoved`]).
//!
//! Hooks the rebase runs (`post-checkout`, `post-rewrite` and so on) inherit
//! its `GIT_REFLOG_ACTION`, so a hook of the user's that moves `HEAD` or the
//! branch during the rebase is logged under the marker and taken as the
//! restack's own.

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

/// One `git` run, under the repository's registration lock (see
/// [`super::registry_lock`]) except network queries (`fetch` and
/// `ls-remote`), which never read the registrations and may take long on the
/// network. Each helper here runs one `git` and returns; none is called while
/// another holds the lock.
async fn run_unless_fetch(
    dir: &Path,
    args: &[&str],
    envs: &[(&str, String)],
) -> std::io::Result<std::process::Output> {
    if matches!(args.first(), Some(&"fetch") | Some(&"ls-remote")) {
        return tokio::process::Command::new("git")
            .args(args)
            .envs(envs.iter().map(|(k, v)| (*k, v.as_str())))
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .output()
            .await;
    }
    super::registry_lock::run_git_with(
        "git".into(),
        &dir.to_string_lossy(),
        args,
        envs,
        super::registry_lock::Input::None,
    )
    .await
    .map(|ran| ran.output)
}

async fn run(dir: &Path, args: &[&str], envs: &[(&str, String)]) -> Result<Ran, String> {
    let output = run_unless_fetch(dir, args, envs)
        .await
        .map_err(|e| format!("Failed to run git: {e}"))?;
    Ok(Ran {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// `git <args>` in `dir` for a NUL-separated listing (`-z`), its entries on
/// success and its stderr otherwise. The output is not trimmed, since a
/// path may start or end with whitespace; only the empty entry after the
/// final NUL is dropped.
async fn git_entries(dir: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    let output = run_unless_fetch(dir, args, &[])
        .await
        .map_err(|e| format!("Failed to run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let listed = String::from_utf8_lossy(&output.stdout);
    let mut entries: Vec<String> = listed.split('\0').map(str::to_string).collect();
    if entries.last().is_some_and(String::is_empty) {
        entries.pop();
    }
    Ok(entries)
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

/// Whether `origin` currently has exactly this branch. An empty successful
/// `ls-remote` result is a definite absence; a failed request remains an
/// unavailable remote rather than being mistaken for an absent branch.
pub async fn remote_branch_exists(dir: &Path, branch: &str) -> Result<bool, String> {
    let refname = format!("refs/heads/{branch}");
    let ran = run(dir, &["ls-remote", "--heads", "origin", &refname], &[]).await?;
    if ran.code != Some(0) {
        return Err(format!("git ls-remote failed: {}", ran.stderr));
    }
    Ok(ran.stdout.lines().any(|line| line.ends_with(&format!("\t{refname}"))))
}

/// What a task's worktree has in progress and uncommitted, as far as a
/// restack is concerned.
pub struct WorktreeState {
    /// The ref `HEAD` names, or `None` when it is detached.
    pub head_ref: Option<String>,
    /// The commit `HEAD` is at.
    pub head: String,
    /// A rebase, merge, cherry-pick, revert or bisect this worktree is in
    /// the middle of, by name.
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
        ("BISECT_LOG", "bisect"),
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

/// A rebase of a branch left in progress in a worktree, as git's own state
/// files for it record it.
pub struct InterruptedRebase {
    /// The branch tip it started from (its `orig-head`).
    pub orig_head: String,
    /// The commit it replays onto (its `onto`).
    pub onto: String,
}

/// A rebase of `refs/heads/<branch>` left in progress in the worktree at
/// `dir`, or `None`. A rebase of anything else is not reported here. Both
/// backends, `rebase-merge/` and `rebase-apply/`, keep `head-name`,
/// `orig-head` and `onto` files. A restack always runs the merge backend,
/// but an apply-backend rebase left by anything else is still reported, so
/// that it goes through the same ownership checks rather than being
/// overlooked.
pub async fn interrupted_rebase_of(dir: &Path, branch: &str) -> Result<Option<InterruptedRebase>, String> {
    let wanted = format!("refs/heads/{branch}");
    for name in ["rebase-merge", "rebase-apply"] {
        let state = git_path(dir, name).await?;
        let Ok(head_name) = std::fs::read_to_string(state.join("head-name")) else {
            continue;
        };
        if head_name.trim() == wanted {
            let read = |file: &str| std::fs::read_to_string(state.join(file)).unwrap_or_default().trim().to_string();
            return Ok(Some(InterruptedRebase { orig_head: read("orig-head"), onto: read("onto") }));
        }
    }
    Ok(None)
}

/// The prefix of the reflog action a restack runs its rebase under. The
/// full action, its marker, is this prefix and a fresh UUID, so that every
/// ref update the rebase makes is logged as `<marker> (start)`,
/// `<marker> (pick)`, `<marker> (finish)` and so on, and nothing else writes
/// that marker.
pub const REFLOG_ACTION_PREFIX: &str = "slashit-restack/";

/// The entries of `refname`'s reflog in the worktree at `dir`, newest first,
/// as the object ID each one set and its message, at most `limit` of them.
/// `HEAD` is the worktree's own. A ref with no reflog has no entries.
async fn reflog(dir: &Path, refname: &str, limit: usize) -> Result<Vec<(String, String)>, String> {
    let limit = format!("--max-count={limit}");
    let ran = run(
        dir,
        &["log", "-g", "--no-show-signature", &limit, "--format=%H%x00%gs", refname, "--"],
        &[],
    )
    .await?;
    if ran.code != Some(0) {
        return Err(format!("git could not read the reflog of {refname}: {}", ran.stderr));
    }
    Ok(ran
        .stdout
        .lines()
        .filter_map(|line| line.split_once('\0'))
        .map(|(oid, message)| (oid.to_string(), message.to_string()))
        .collect())
}

/// The tip the rebase marked `marker` produced for `refs/heads/<branch>`,
/// when that is still the latest update of the branch: the newest entry of
/// its reflog is the rebase's `(finish)`. `None` when anything else moved
/// the branch since, or the rebase never finished.
async fn produced_tip(dir: &Path, branch: &str, marker: &str) -> Result<Option<String>, String> {
    let entries = reflog(dir, &format!("refs/heads/{branch}"), 1).await?;
    let finish = format!("{marker} (finish):");
    Ok(entries
        .into_iter()
        .next()
        .filter(|(oid, message)| message.starts_with(&finish) && is_full_object_id(oid))
        .map(|(oid, _)| oid))
}

/// How far back `HEAD`'s reflog is searched for a rebase's `(start)`: one
/// entry per replayed commit and a few more, so far more than any task
/// branch SlashIt restacks. A `(start)` further back is not found, and the
/// moves are then not taken as the rebase's.
const HEAD_REFLOG_LIMIT: usize = 10_000;

/// Whether every move of the worktree's `HEAD` since the rebase marked
/// `marker` started was that rebase's (or its rollback's): walking `HEAD`'s
/// reflog from the newest entry, each one carries the marker, down to its
/// `(start)`, and `HEAD` is where the newest entry left it. A commit, a
/// checkout or a rebase from anywhere else, or no `(start)` at all, is not.
async fn head_moved_only_by(dir: &Path, marker: &str) -> Result<bool, String> {
    let entries = reflog(dir, "HEAD", HEAD_REFLOG_LIMIT).await?;
    let Some((newest, _)) = entries.first() else {
        return Ok(false);
    };
    if &git(dir, &["rev-parse", "--verify", "HEAD^{commit}"]).await? != newest {
        return Ok(false);
    }
    let start = format!("{marker} (start)");
    let own = format!("{marker} ");
    for (_, message) in &entries {
        if message.starts_with(&start) {
            return Ok(true);
        }
        if !message.starts_with(&own) {
            return Ok(false);
        }
    }
    Ok(false)
}

/// The marker of the one restack whose rebase alone has moved the worktree's
/// `HEAD` since it started, whichever restack that was, or `None`: the
/// newest entry of
/// `HEAD`'s reflog carries a marker under [`REFLOG_ACTION_PREFIX`], and every
/// entry down to that marker's `(start)` carries the same one. A rebase run
/// or continued by hand, or a commit from the terminal, is logged under
/// another action and makes this `None`.
pub async fn head_moved_only_by_a_restack(dir: &Path) -> Result<Option<String>, String> {
    let entries = reflog(dir, "HEAD", HEAD_REFLOG_LIMIT).await?;
    let marker = entries.first().and_then(|(_, message)| {
        let marker = message.split(' ').next()?;
        let nonce = marker.strip_prefix(REFLOG_ACTION_PREFIX)?;
        Uuid::parse_str(nonce).is_ok().then(|| marker.to_string())
    });
    match marker {
        Some(marker) if head_moved_only_by(dir, &marker).await? => Ok(Some(marker)),
        _ => Ok(None),
    }
}

/// Why the rebase stopped in the worktree at `dir` no longer holds only what
/// the stop itself left, or `None` when it does.
///
/// A rebase that stops on a conflict leaves the conflicted paths unmerged,
/// stages its clean merge of the other paths the stopped commit changes
/// (`REBASE_HEAD`, against its parent), and the worktree was clean before it
/// started. So a stop with no unmerged paths left (resolved by hand, or a
/// stop that was not a conflict), a change to any other tracked path that is
/// not staged, a path staged against `HEAD` that the stopped commit does not
/// change, or an untracked file are someone else's work, which undoing the
/// stop would discard. Without a `REBASE_HEAD` that has a parent, the stop
/// cannot be checked, and is refused. A user's staged edit on a path the
/// stopped commit does change cannot be told from git's clean merge there by
/// this check, and would be discarded with it; recovery, where that can
/// happen, adds [`staged_resolutions_in_the_stop`]. The other way round, when `onto` renamed a path
/// the stopped commit changes, git stages that change at the new path, which
/// the commit itself does not name, and the undo is refused though nothing
/// is the user's (failing closed). What
/// cannot be told apart: an edit inside a path that is still unmerged looks
/// the same as the conflict markers the stop wrote there, and a nested
/// repository created at an unmerged path is removed with it. Neither can a file
/// ignored at the old tip that `onto`'s `.gitignore` no longer ignores: at
/// the stop it shows as untracked, and the undo is refused, failing closed.
///
/// Changes inside submodules are left out of the comparison
/// (`--ignore-submodules=all`): the rebase moves a submodule's recorded
/// commit without updating its checkout, so one that `onto` moved always
/// shows as changed at the stop, and the undo never recurses into a
/// submodule's checkout (`--no-recurse-submodules`, whatever
/// `submodule.recurse` says), so nothing in it can be lost. `=dirty` with a
/// check of each gitlink would only separate cases that the undo treats
/// alike. A type change is not left out: a submodule replaced by a file or a
/// link, or the other way round, is listed on its own
/// (`--diff-filter=T`), and refuses the undo, which would otherwise remove
/// it.
pub async fn changes_beyond_the_stop(dir: &Path) -> Result<Option<String>, String> {
    let paths = |listed: Vec<String>| -> std::collections::BTreeSet<String> { listed.into_iter().collect() };
    let unmerged = paths(git_entries(dir, &["diff", "-z", "--name-only", "--diff-filter=U"]).await?);
    if unmerged.is_empty() {
        return Ok(Some("no conflicted paths are left, so they were resolved by hand".to_string()));
    }
    let mut unstaged = paths(git_entries(dir, &["diff", "-z", "--name-only", "--ignore-submodules=all"]).await?);
    unstaged.extend(paths(
        git_entries(dir, &["diff", "-z", "--name-only", "--diff-filter=T", "--ignore-submodules=none"]).await?,
    ));
    let others: Vec<String> = unstaged.difference(&unmerged).cloned().collect();
    if !others.is_empty() {
        return Ok(Some(format!("{} changed besides the conflict", others.join(", "))));
    }
    let Some(stopped) = run(dir, &["rev-parse", "-q", "--verify", "REBASE_HEAD^{commit}"], &[])
        .await?
        .stdout
        .lines()
        .next()
        .filter(|oid| is_full_object_id(oid))
        .map(str::to_string)
    else {
        return Ok(Some("git does not name the commit it stopped on (REBASE_HEAD)".to_string()));
    };
    let parent = run(dir, &["rev-parse", "-q", "--verify", &format!("{stopped}^1")], &[]).await?;
    if parent.code != Some(0) || !is_full_object_id(&parent.stdout) {
        return Ok(Some(format!("the commit it stopped on, {stopped}, has no parent to compare it with")));
    }
    let own = paths(git_entries(dir, &["diff-tree", "-r", "-z", "--name-only", "--no-renames", &parent.stdout, &stopped]).await?);
    let staged = paths(
        git_entries(dir, &["diff", "--cached", "-z", "--name-only", "--no-renames", "--ignore-submodules=none", "HEAD"]).await?,
    );
    let foreign: Vec<String> = staged.difference(&unmerged).filter(|p| !own.contains(*p)).cloned().collect();
    if !foreign.is_empty() {
        return Ok(Some(format!(
            "{} staged, which the commit it stopped on does not change (or git staged its change \
             there because the new base renamed the path)",
            foreign.join(", ")
        )));
    }
    let untracked = paths(git_entries(dir, &["ls-files", "-z", "--others", "--exclude-standard"]).await?);
    if !untracked.is_empty() {
        return Ok(Some(format!(
            "it has untracked files ({})",
            untracked.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(None)
}

/// Why a stopped rebase's staged, no longer conflicted paths may hold a
/// resolution someone made, or `None` when they cannot. For recovery after
/// SlashIt stopped part way, where time has passed since the stop and the
/// user may have resolved some conflicts and left others.
///
/// [`changes_beyond_the_stop`] accepts a staged path the stopped commit
/// changes, since that is where git stages its clean merge. Here each such
/// path must hold, in the index, exactly what `HEAD` or the stopped commit
/// (`REBASE_HEAD`) holds there, mode and object alike, a missing entry
/// counting as a value of its own. Anything else was resolved or edited and
/// staged, and is refused. So is a clean merge of a path both sides changed,
/// which git stages as a third version: recovery fails closed there.
pub async fn staged_resolutions_in_the_stop(dir: &Path) -> Result<Option<String>, String> {
    let paths = |listed: Vec<String>| -> std::collections::BTreeSet<String> { listed.into_iter().collect() };
    let stopped = git(dir, &["rev-parse", "-q", "--verify", "REBASE_HEAD^{commit}"]).await;
    let Some(stopped) = stopped.ok().filter(|oid| is_full_object_id(oid)) else {
        return Ok(Some("git does not name the commit it stopped on (REBASE_HEAD)".to_string()));
    };
    let parent = run(dir, &["rev-parse", "-q", "--verify", &format!("{stopped}^1")], &[]).await?;
    if parent.code != Some(0) || !is_full_object_id(&parent.stdout) {
        return Ok(Some(format!("the commit it stopped on, {stopped}, has no parent to compare it with")));
    }
    let unmerged = paths(git_entries(dir, &["diff", "-z", "--name-only", "--diff-filter=U"]).await?);
    let own = paths(git_entries(dir, &["diff-tree", "-r", "-z", "--name-only", "--no-renames", &parent.stdout, &stopped]).await?);
    let staged = paths(
        git_entries(dir, &["diff", "--cached", "-z", "--name-only", "--no-renames", "--ignore-submodules=none", "HEAD"]).await?,
    );
    let candidates: Vec<&str> = staged
        .iter()
        .filter(|p| own.contains(*p) && !unmerged.contains(*p))
        .map(String::as_str)
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    // `<mode> <object>` per path, from `ls-files -s` (`<mode> <object>
    // <stage>\t<path>`) or `ls-tree` (`<mode> <type> <object>\t<path>`).
    // Paths are passed as literal pathspecs, never globs.
    let entries = |listed: Vec<String>, tree: bool| -> std::collections::BTreeMap<String, String> {
        listed
            .iter()
            .filter_map(|entry| {
                let (meta, path) = entry.split_once('\t')?;
                let fields: Vec<&str> = meta.split(' ').collect();
                let (mode, object) = match (tree, fields.as_slice()) {
                    (false, [mode, object, _stage]) => (mode, object),
                    (true, [mode, _type, object]) => (mode, object),
                    _ => return None,
                };
                Some((path.to_string(), format!("{mode} {object}")))
            })
            .collect()
    };
    let listing = |args: Vec<&str>| {
        let mut all = vec!["--literal-pathspecs"];
        all.extend(args);
        all.push("--");
        all.extend(candidates.iter().copied());
        all.into_iter().map(str::to_string).collect::<Vec<String>>()
    };
    let list = |args: Vec<String>| async move {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        git_entries(dir, &args).await
    };
    let index = entries(list(listing(vec!["ls-files", "-s", "-z"])).await?, false);
    let head = entries(list(listing(vec!["ls-tree", "-r", "-z", "--full-tree", "HEAD"])).await?, true);
    let rebase_head = entries(list(listing(vec!["ls-tree", "-r", "-z", "--full-tree", &stopped])).await?, true);
    let resolved: Vec<&str> = candidates
        .into_iter()
        .filter(|path| {
            let at = |listed: &std::collections::BTreeMap<String, String>| listed.get(*path).cloned();
            at(&index) != at(&head) && at(&index) != at(&rebase_head)
        })
        .collect();
    if resolved.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "{} staged with content that neither the new base nor the commit it stopped on has, as a \
         resolved conflict is",
        resolved.join(", ")
    )))
}

/// Undo a rebase of `refs/heads/<branch>` stopped part way in the worktree
/// at `dir`, back to `old_tip`, without writing the branch ref: a rebase
/// that stopped never moved it, and `git rebase --abort` would write it back
/// to where the rebase started, over anything that moved it since.
///
/// `git rebase --quit` drops the rebase's state and leaves `HEAD` detached
/// where it stopped, `git symbolic-ref` puts `HEAD` back on the branch
/// (logged as `message`), and `git read-tree --reset -u <old tip>` makes the
/// index and the files the old tip's, removing what the stop added. It never
/// goes into a submodule's checkout (`--no-recurse-submodules`), whatever
/// `submodule.recurse` says, so a submodule checkout the rebase moved (a git
/// that honors `submodule.recurse` in a rebase does) is not moved back:
/// `git submodule update` does that.
/// Untracked files at paths the old tip does not track are left; ignored
/// files at paths it does track are overwritten. The caller has checked
/// that the rebase is the one it means to undo and that the worktree holds
/// nothing else ([`changes_beyond_the_stop`]). `read-tree --reset` never
/// refuses, so an edit or untracked file made after that check and before
/// the reset is destroyed. The result is verified with
/// [`verify_restored`]; if the branch was moved in the meantime, that fails
/// and nothing it was moved to is lost.
pub async fn undo_stopped_rebase(dir: &Path, branch: &str, old_tip: &str, message: &str) -> Result<(), String> {
    let branch_ref = format!("refs/heads/{branch}");
    git(dir, &["rebase", "--quit"]).await?;
    git(dir, &["symbolic-ref", "-m", message, "HEAD", &branch_ref]).await?;
    git(dir, &["read-tree", "--reset", "-u", "--no-recurse-submodules", old_tip]).await?;
    verify_restored(dir, branch, old_tip).await
}

/// Whether `new_tip` is exactly `fork_point..old_tip` replayed onto `onto`:
/// it contains `onto`, holds as many commits on top of it as the task had,
/// and each makes the same change as its original, compared as `git patch-id
/// --stable` over diffs with no context lines, so that a change `onto` made
/// next to a task's line does not count as a difference while a dropped or
/// altered change does.
///
/// A property of the commits alone, so it holds the same in the process that
/// ran the rebase and in a later one that finds its result.
pub async fn replay_matches(
    dir: &Path,
    fork_point: &str,
    old_tip: &str,
    onto: &str,
    new_tip: &str,
) -> Result<(), String> {
    if !is_ancestor(dir, onto, new_tip).await? {
        return Err(format!("{new_tip} does not contain {onto}"));
    }
    let before = commits_between(dir, fork_point, old_tip).await?;
    let after = commits_between(dir, onto, new_tip).await?;
    if before.len() != after.len() {
        return Err(format!(
            "it holds {} commits on top of {onto} where the task had {}",
            after.len(),
            before.len()
        ));
    }
    for (original, replayed) in before.iter().zip(&after) {
        if patch_id(dir, original).await? != patch_id(dir, replayed).await? {
            return Err(format!("{replayed} does not make the same change as {original}"));
        }
    }
    Ok(())
}

/// [`replay_matches`], and in addition every replayed commit carries the same
/// full message and the same author name and email as its original.
///
/// For finding a replay after a crash, where the question is not only "does
/// this make the same changes" but "is this the replay SlashIt was authorized
/// to make": a rebase rewrites parents, commit IDs and committer, and nothing
/// else, so a commit that differs in message or author was changed by someone
/// else. The unpublished restack's own verification stays content-only.
pub async fn replay_matches_exactly(
    dir: &Path,
    fork_point: &str,
    old_tip: &str,
    onto: &str,
    new_tip: &str,
) -> Result<(), String> {
    replay_matches(dir, fork_point, old_tip, onto, new_tip).await?;
    let before = commits_between(dir, fork_point, old_tip).await?;
    let after = commits_between(dir, onto, new_tip).await?;
    for (original, replayed) in before.iter().zip(&after) {
        if authored_message(dir, original).await? != authored_message(dir, replayed).await? {
            return Err(format!(
                "{replayed} has a different message or author than {original}, which a replay does not change"
            ));
        }
    }
    Ok(())
}

/// A commit's author name, author email and full raw message, untrimmed.
async fn authored_message(dir: &Path, commit: &str) -> Result<(String, String, String), String> {
    let mut parts = git_entries(dir, &["show", "-s", "-z", "--format=%an%x00%ae%x00%B", commit])
        .await?
        .into_iter();
    let name = parts.next().ok_or_else(|| format!("could not read the author of {commit}"))?;
    let email = parts.next().ok_or_else(|| format!("could not read the author of {commit}"))?;
    Ok((name, email, parts.next().unwrap_or_default()))
}

/// The backup ref that holds a published task branch's tip from before it was
/// restacked (see `commands::pr::republish`). Its own name, not
/// [`backup_ref`]'s: an unpublished restack's recovery deletes or refuses the
/// refs under that prefix, and must never take this one for its own.
pub fn republish_backup_ref(task_id: Uuid) -> String {
    format!("refs/slashit/republish-backup/{task_id}")
}

/// Move `refs/heads/<branch>` back from `from` to `to`, and the index and
/// files of the worktree that has it checked out along with it, but only while
/// the branch is still exactly at `from`.
///
/// The ref is moved with a compare-and-swap, and the files with `git read-tree
/// -m -u`, which refuses rather than overwrite a local change. The caller has
/// checked that the worktree is on the branch at `from`, clean and in the
/// middle of nothing. Verified afterwards by [`verify_restored`].
pub async fn move_branch_back(dir: &Path, branch: &str, from: &str, to: &str) -> Result<(), String> {
    let branch_ref = format!("refs/heads/{branch}");
    git(dir, &["update-ref", &branch_ref, to, from])
        .await
        .map_err(|e| format!("could not move {branch_ref} from {from} back to {to}: {e}"))?;
    git(dir, &["update-index", "-q", "--refresh"]).await?;
    git(dir, &["read-tree", "-m", "-u", "--no-recurse-submodules", from, to])
        .await
        .map_err(|e| format!("{branch_ref} is back at {to}, but the worktree could not follow: {e}"))?;
    verify_restored(dir, branch, to).await
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
        return Err(format!("{branch_ref} is at {}, not {old_tip}", tip.as_deref().unwrap_or("nothing (it is missing)")));
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
    /// The reflog action this restack's rebase runs under, unique to it.
    marker: String,
    /// Run once, in tests, between the checks that a stopped rebase is this
    /// restack's and undoing it, to move things in that window.
    #[cfg(test)]
    before_undoing_a_stop: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
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
    /// What went wrong, and how the branch or the worktree's `HEAD` was
    /// found moved by something other than this restack: a commit from the
    /// task's terminal, say, or another tool. The branch was not written and
    /// nothing was deleted, so whatever moved it is kept, and the backup ref
    /// is kept, holding the old tip. When the branch moved while a stopped
    /// rebase was being undone, the worktree was already put back on the
    /// branch with the old tip's files, as the `String` says.
    BranchMoved(String),
}

/// Why a rollback did not put the branch back.
enum NotUndone {
    /// Something other than this restack moved the branch or `HEAD`.
    Moved(String),
    /// Anything else; the branch could not be verified as restored.
    Failed(String),
}

impl From<String> for NotUndone {
    fn from(why: String) -> Self {
        NotUndone::Failed(why)
    }
}

impl<'a> Restack<'a> {
    /// A restack of `refs/heads/<branch>` in `worktree`, with a reflog
    /// marker of its own.
    pub fn new(
        worktree: &'a Path,
        branch: &'a str,
        fork_point: &'a str,
        old_tip: &'a str,
        onto: &'a str,
        backup: &'a str,
    ) -> Self {
        let marker = format!("{REFLOG_ACTION_PREFIX}{}", Uuid::new_v4());
        Restack {
            worktree,
            branch,
            fork_point,
            old_tip,
            onto,
            backup,
            marker,
            #[cfg(test)]
            before_undoing_a_stop: std::sync::Mutex::new(None),
        }
    }

    /// Replay the task's commits onto `onto` and verify the result.
    ///
    /// The rebase runs git's merge backend (`--merge`), whatever
    /// `rebase.backend` says: what the undo checks (the marker's reflog
    /// entries, `REBASE_HEAD`, the stop's state in `rebase-merge/`) is
    /// written against it.
    ///
    /// The rebase names every revision exactly: the new base and the fork
    /// point as object IDs, the branch by its exact name, and it turns off
    /// everything configuration could turn on behind the caller's back:
    /// setting uncommitted work aside (`--no-autostash`), moving other
    /// branches that point into the replayed range (`--no-update-refs`, on
    /// a git new enough to have it and so `rebase.updateRefs`),
    /// guessing another fork point from the reflog (`--no-fork-point`),
    /// recreating merges, and reordering `fixup!` commits. The caller has
    /// already checked that the worktree is clean and has the branch
    /// checked out, and has written the backup ref. The branch is passed by
    /// its short name, since `refs/heads/<branch>` would make git rebase a
    /// detached `HEAD` instead of the branch.
    ///
    /// The rebase runs under this restack's reflog marker, with every ref
    /// update logged (`core.logAllRefUpdates=always`), so that the branch's
    /// and the worktree's reflogs tell its moves from anybody else's. It runs
    /// with rerere off (`rerere.enabled=false`): a resolution recorded
    /// earlier is not replayed into the commits the restack verifies, and a
    /// stop is left as the conflict git made, which is how an undo tells it
    /// from the user's own resolution.
    ///
    /// Git needs a committer identity to write the replayed commits. Where
    /// the repository has none, the committer of the branch's tip is used,
    /// the same way SlashIt's author rewrite keeps the tip's own committer
    /// rather than inventing one.
    ///
    /// Success is not taken from the exit status alone. The branch must be
    /// exactly at the tip the rebase produced, per its reflog, with the
    /// worktree's `HEAD` on it and moved by nothing else. The new tip must
    /// contain `onto`, `onto..<new tip>` must hold as many commits as
    /// `fork_point..old_tip`, and each replayed commit must change the same
    /// lines as its original, compared as `git patch-id --stable` over diffs
    /// with no context lines, so that a change `onto` made next to a task's
    /// line does not count as a difference while a dropped or altered change
    /// does. A commit that became empty against `onto` is dropped by the
    /// rebase and fails the count. Anything that does not verify is rolled
    /// back to the old tip, as far as [`Restack::roll_back`] allows.
    pub async fn replay(&self) -> Result<String, RestackFailure> {
        let rebased = self.rebase().await?;
        self.settle(rebased).await
    }

    /// Run the rebase. An `Err` is a rebase that never started, already
    /// settled.
    async fn rebase(&self) -> Result<Ran, RestackFailure> {
        let mut envs = match committer_env(self.worktree, self.old_tip).await {
            Ok(envs) => envs,
            Err(e) => {
                return Err(self.restore(false, format!("the rebase could not be started: {e}")).await)
            }
        };
        envs.push(("GIT_REFLOG_ACTION", self.marker.clone()));
        let version = match git(self.worktree, &["version"]).await {
            Ok(version) => version,
            Err(e) => {
                return Err(self.restore(false, format!("the rebase could not be started: {e}")).await)
            }
        };
        let mut args = vec![
            "-c",
            "core.logAllRefUpdates=always",
            "-c",
            "rerere.enabled=false",
            "rebase",
            "--merge",
            "--no-autostash",
        ];
        if knows_no_update_refs(&version) {
            args.push("--no-update-refs");
        }
        args.extend([
            "--no-fork-point",
            "--no-rebase-merges",
            "--no-autosquash",
            "--quiet",
            "--onto",
            self.onto,
            self.fork_point,
            self.branch,
        ]);
        let rebased = run(self.worktree, &args, &envs).await;
        match rebased {
            Ok(ran) => Ok(ran),
            Err(e) => Err(self.restore(false, format!("git rebase could not be run: {e}")).await),
        }
    }

    /// Verify what the rebase left, or roll it back.
    async fn settle(&self, rebased: Ran) -> Result<String, RestackFailure> {
        if rebased.code != Some(0) {
            let conflicted = git(self.worktree, &["diff", "--name-only", "--diff-filter=U"])
                .await
                .unwrap_or_default();
            let what = if conflicted.is_empty() {
                format!("git rebase failed: {}", rebased.stderr)
            } else {
                format!("it stopped on a conflict in {}", conflicted.lines().collect::<Vec<_>>().join(", "))
            };
            return Err(self.restore(true, what).await);
        }

        match self.verify().await {
            Ok(new_tip) => Ok(new_tip),
            Err(why) => Err(self.restore(true, format!("the rebased branch did not verify: {why}")).await),
        }
    }

    /// The new tip, when the branch is exactly where this restack's rebase
    /// left it and holds exactly the task's commits on top of `onto`.
    async fn verify(&self) -> Result<String, String> {
        let branch_ref = format!("refs/heads/{}", self.branch);
        let new_tip = produced_tip(self.worktree, self.branch, &self.marker)
            .await?
            .ok_or_else(|| format!("the latest update of {branch_ref} is not this restack's rebase"))?;
        let tip = exact_ref(self.worktree, &branch_ref).await?;
        if tip.as_deref() != Some(new_tip.as_str()) {
            return Err(format!(
                "{branch_ref} is at {}, not at {new_tip}, which the rebase produced",
                tip.as_deref().unwrap_or("nothing (it is missing)")
            ));
        }
        if !head_moved_only_by(self.worktree, &self.marker).await? {
            return Err("the worktree's HEAD was moved by something other than this restack".to_string());
        }
        let state = worktree_state(self.worktree).await?;
        if state.head_ref.as_deref() != Some(branch_ref.as_str()) || state.head != new_tip {
            return Err(format!("the worktree's HEAD is not {branch_ref} at {new_tip}"));
        }
        if let Some(operation) = state.in_progress {
            return Err(format!("the worktree is in the middle of a {operation}"));
        }
        replay_matches(self.worktree, self.fork_point, self.old_tip, self.onto, &new_tip).await?;
        Ok(new_tip)
    }

    /// Put the branch back at the old tip, but only a state this restack
    /// produced and nothing moved since, verify it, and say which
    /// [`RestackFailure`] that makes `what`. `ran` says whether the rebase
    /// was run at all.
    ///
    /// - A rebase that never started changed nothing, and nothing is done:
    ///   the branch and the worktree are only verified at the old tip.
    /// - A rebase stopped part way is undone only while it is provably this
    ///   one: a rebase of `refs/heads/<branch>` from the old tip onto `onto`,
    ///   the branch still at the old tip, `HEAD` moved by nothing but this
    ///   rebase, and the worktree holding nothing but the conflict the stop
    ///   left ([`changes_beyond_the_stop`]). It is undone with
    ///   [`undo_stopped_rebase`], which never writes the branch ref, never
    ///   with `git rebase --abort`, which writes it back to where the rebase
    ///   started. A branch moved after the checks keeps what it was moved to;
    ///   a `HEAD` moved in that instant is put back on the branch, and what
    ///   it was moved to stays in its reflog. The reset of the index and
    ///   files never refuses: an edit or untracked file made in that instant
    ///   is destroyed, and ignored files at paths the old tip tracks are
    ///   overwritten.
    /// - A rebase that finished moved the branch. It is moved back only
    ///   while it is still at the tip the rebase produced, per its reflog,
    ///   with the worktree's `HEAD` on it, moved by nothing else and in the
    ///   middle of nothing. The ref is moved back with a compare-and-swap
    ///   (`git update-ref <branch> <old tip> <produced tip>`), which changes
    ///   nothing if it moved in the meantime; only then are the index and
    ///   the files brought back with `git read-tree -m -u`, which refuses
    ///   rather than overwrite local changes or untracked files (ignored
    ///   files it does overwrite, as `git checkout` would), and never goes
    ///   into a submodule's checkout. A commit
    ///   made in the instant between the swap and the read-tree is not
    ///   detected before it, and fails the verification after it.
    /// - A branch already at the old tip with no rebase in progress is
    ///   only verified.
    ///
    /// Anything else was moved by someone other than this restack, and is
    /// left exactly as it is ([`RestackFailure::BranchMoved`]). Only a
    /// verified restoration retires the backup ref.
    async fn restore(&self, ran: bool, what: String) -> RestackFailure {
        let undone = if ran { self.undo().await } else { self.check_untouched().await };
        match undone {
            Ok(()) => match retire_backup(self.worktree, self.backup, self.old_tip).await {
                Ok(()) => RestackFailure::Restored(what),
                Err(e) => RestackFailure::Restored(format!("{what} ({e})")),
            },
            Err(NotUndone::Moved(how)) => RestackFailure::BranchMoved(format!("{what}; meanwhile {how}")),
            Err(NotUndone::Failed(why)) => RestackFailure::NotRestored(format!(
                "{what}, and the branch could not be verified as restored: {why}"
            )),
        }
    }

    /// After a rebase that never started: nothing to undo, and anything not
    /// at the old tip was moved by someone else.
    async fn check_untouched(&self) -> Result<(), NotUndone> {
        verify_restored(self.worktree, self.branch, self.old_tip)
            .await
            .map_err(|why| NotUndone::Moved(format!("{why}, although SlashIt had not changed anything yet")))
    }

    /// After a rebase that ran: undo what it provably did, and nothing else.
    async fn undo(&self) -> Result<(), NotUndone> {
        let branch_ref = format!("refs/heads/{}", self.branch);
        let tip = exact_ref(self.worktree, &branch_ref).await?;
        let shown = |tip: &Option<String>| tip.clone().unwrap_or_else(|| "gone".to_string());

        if operation_in_progress(self.worktree).await? == Some("rebase") {
            let rebase = interrupted_rebase_of(self.worktree, self.branch).await?;
            let own_rebase = rebase.is_some_and(|r| r.orig_head == self.old_tip && r.onto == self.onto);
            if !own_rebase {
                return Err(NotUndone::Moved(format!(
                    "the rebase in progress in the worktree is not this restack's; {branch_ref} is at {}",
                    shown(&tip)
                )));
            }
            if tip.as_deref() != Some(self.old_tip) {
                return Err(NotUndone::Moved(format!("{branch_ref} was moved to {}", shown(&tip))));
            }
            if !head_moved_only_by(self.worktree, &self.marker).await? {
                let head = git(self.worktree, &["rev-parse", "--verify", "HEAD"]).await?;
                return Err(NotUndone::Moved(format!(
                    "the worktree's HEAD was moved to {head} by something other than this restack, \
                     while its rebase was stopped; {branch_ref} is at {}",
                    shown(&tip)
                )));
            }
            if let Some(why) = changes_beyond_the_stop(self.worktree).await? {
                return Err(NotUndone::Moved(format!(
                    "the worktree holds changes SlashIt did not make while its rebase was stopped \
                     ({why}); {branch_ref} is at {}",
                    shown(&tip)
                )));
            }
            #[cfg(test)]
            if let Some(hook) = self.before_undoing_a_stop.lock().unwrap().take() {
                hook();
            }
            let message = format!("{} (rollback)", self.marker);
            if let Err(why) = undo_stopped_rebase(self.worktree, self.branch, self.old_tip, &message).await {
                let now = exact_ref(self.worktree, &branch_ref).await?;
                if now.as_deref() != Some(self.old_tip) {
                    return Err(NotUndone::Moved(format!(
                        "{branch_ref} was moved to {} while the stopped rebase was being undone. \
                         The branch was not written; the worktree is back on it, but its index and \
                         files hold {}'s version, which `git read-tree -m -u {} HEAD` there brings \
                         to the branch's tip",
                        shown(&now),
                        self.old_tip,
                        self.old_tip
                    )));
                }
                return Err(NotUndone::Failed(why));
            }
            return Ok(());
        }

        if tip.as_deref() == Some(self.old_tip) {
            return Ok(verify_restored(self.worktree, self.branch, self.old_tip).await?);
        }

        let produced = produced_tip(self.worktree, self.branch, &self.marker).await?;
        let Some(produced) = produced.filter(|p| tip.as_deref() == Some(p.as_str())) else {
            return Err(NotUndone::Moved(format!(
                "{branch_ref} was moved to {} by something other than this restack",
                shown(&tip)
            )));
        };
        if !head_moved_only_by(self.worktree, &self.marker).await? {
            let head = git(self.worktree, &["rev-parse", "--verify", "HEAD"]).await?;
            return Err(NotUndone::Moved(format!(
                "the worktree's HEAD was moved to {head} by something other than this restack; \
                 {branch_ref} is at {produced}"
            )));
        }
        let state = worktree_state(self.worktree).await?;
        if state.head_ref.as_deref() != Some(branch_ref.as_str())
            || state.head != produced
            || state.in_progress.is_some()
        {
            return Err(NotUndone::Moved(format!(
                "the worktree is no longer on {branch_ref} at {produced} with nothing in progress: \
                 its HEAD is {} at {}{}",
                state.head_ref.as_deref().unwrap_or("detached"),
                state.head,
                state.in_progress.map(|op| format!(", in the middle of a {op}")).unwrap_or_default()
            )));
        }

        let message = format!("{} (rollback)", self.marker);
        let swapped = run(
            self.worktree,
            &["update-ref", "-m", &message, &branch_ref, self.old_tip, &produced],
            &[],
        )
        .await?;
        if swapped.code != Some(0) {
            let now = exact_ref(self.worktree, &branch_ref).await?;
            if now.as_deref() != Some(produced.as_str()) {
                return Err(NotUndone::Moved(format!(
                    "{branch_ref} was moved to {} before it could be put back",
                    shown(&now)
                )));
            }
            return Err(NotUndone::Failed(format!(
                "{branch_ref} could not be moved back from {produced}: {}",
                swapped.stderr
            )));
        }
        run(self.worktree, &["update-index", "-q", "--refresh"], &[]).await?;
        let files = run(
            self.worktree,
            &["read-tree", "-m", "-u", "--no-recurse-submodules", &produced, self.old_tip],
            &[],
        )
        .await?;
        if files.code != Some(0) {
            return Err(NotUndone::Failed(format!(
                "{branch_ref} is back at {}, but the worktree's index and files still hold the \
                 restacked {produced}: git read-tree could not update them ({}). To finish, run \
                 `git status` in the worktree, deal with what it reports, then run `git read-tree \
                 -m -u {produced} {}` there",
                self.old_tip, files.stderr, self.old_tip
            )));
        }
        Ok(verify_restored(self.worktree, self.branch, self.old_tip).await?)
    }

    /// Roll a verified restack back, for a caller that could not record it:
    /// the branch goes back to the old tip while it is still exactly where
    /// the rebase left it, and the backup ref is retired once that is
    /// verified. A branch or `HEAD` that moved since is left as it is
    /// ([`RestackFailure::BranchMoved`]).
    pub async fn roll_back(&self, what: String) -> RestackFailure {
        self.restore(true, what).await
    }
}

/// Whether the git that printed `version` (`git version` output, such as
/// `git version 2.39.5 (Apple Git-154)` or `git version 2.45.1.windows.1`)
/// knows `git rebase --no-update-refs`, which came with 2.38. An older git
/// rejects the option, and has no `rebase.updateRefs` for it to turn off
/// either. A version that cannot be read is taken to know it.
fn knows_no_update_refs(version: &str) -> bool {
    let parsed = version.trim().strip_prefix("git version ").and_then(|v| {
        let mut numbers = v.split(|c: char| !c.is_ascii_digit());
        let major: u32 = numbers.next()?.parse().ok()?;
        let minor: u32 = numbers.next()?.parse().ok()?;
        Some((major, minor))
    });
    parsed.is_none_or(|version| version >= (2, 38))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let output = StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env_remove("GIT_REFLOG_ACTION")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A repository with `main` (F, then M) pushed to a bare `origin`, and
    /// `task` (B1, B2 on b.txt) forked from F, never pushed and checked out
    /// in a worktree of its own. With `conflict`, M edits b.txt so replaying
    /// B1 onto it stops on a conflict. The backup ref is written as the
    /// caller of a restack writes it.
    struct Fixture {
        _tmp: tempfile::TempDir,
        origin: PathBuf,
        worktree: PathBuf,
        fork_point: String,
        old_tip: String,
        onto: String,
        backup: String,
    }

    impl Fixture {
        fn new(conflict: bool) -> Self {
            Self::build(conflict, false, false)
        }

        /// A restack whose first commit replays cleanly and whose second,
        /// B2, stops on a conflict in f.txt, which B2 and M both change.
        fn stopping_on_the_second_commit() -> Self {
            Self::build(false, false, true)
        }

        /// [`Fixture::new`] with a conflict, where the repository also has
        /// a submodule `sub` (at S1 on F and on the task's branch, and moved
        /// to S2 by M), checked out in the task's worktree.
        fn with_submodule() -> Self {
            Self::build(true, true, false)
        }

        fn build(conflict: bool, submodule: bool, second: bool) -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let origin = tmp.path().join("origin.git");
            let repo = tmp.path().join("repo");
            let worktree = tmp.path().join("worktree");
            git_in(tmp.path(), &["init", "-q", "--bare", origin.to_str().unwrap()]);
            git_in(tmp.path(), &["init", "-q", "-b", "main", repo.to_str().unwrap()]);
            git_in(&repo, &["config", "user.name", "Test"]);
            git_in(&repo, &["config", "user.email", "test@example.com"]);
            git_in(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
            let sub_commits = submodule.then(|| {
                let sub = tmp.path().join("sub");
                git_in(tmp.path(), &["init", "-q", "-b", "main", sub.to_str().unwrap()]);
                let s1 = commit(&sub, "s.txt", "s1\n", "S1");
                let s2 = commit(&sub, "s.txt", "s2\n", "S2");
                let url = sub.to_str().unwrap();
                git_in(&repo, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", url, "sub"]);
                git_in(&repo.join("sub"), &["checkout", "-q", &s1]);
                (s1, s2)
            });
            commit(&repo, "f.txt", "f\n", "F");
            let fork_point = git_in(&repo, &["rev-parse", "HEAD"]);
            git_in(&repo, &["checkout", "-q", "-b", "task"]);
            commit(&repo, "b.txt", "b1\n", "B1");
            if second {
                std::fs::write(repo.join("f.txt"), "the task's f\n").unwrap();
            }
            commit(&repo, "b.txt", "b2\n", "B2");
            let old_tip = git_in(&repo, &["rev-parse", "HEAD"]);
            git_in(&repo, &["checkout", "-q", "main"]);
            if let Some((_, s2)) = &sub_commits {
                git_in(&repo.join("sub"), &["checkout", "-q", s2]);
            }
            if second {
                commit(&repo, "f.txt", "main's own f\n", "M");
            } else if conflict {
                commit(&repo, "b.txt", "main's own b\n", "M");
            } else {
                commit(&repo, "m.txt", "m\n", "M");
            }
            let onto = git_in(&repo, &["rev-parse", "HEAD"]);
            git_in(&repo, &["push", "-q", "origin", "main"]);
            git_in(&repo, &["worktree", "add", "-q", worktree.to_str().unwrap(), "task"]);
            if submodule {
                git_in(&worktree, &["-c", "protocol.file.allow=always", "submodule", "update", "-q", "--init"]);
            }
            let backup = backup_ref(Uuid::new_v4());
            git_in(&repo, &["update-ref", &backup, &old_tip, ""]);
            Fixture { _tmp: tmp, origin, worktree, fork_point, old_tip, onto, backup }
        }

        fn restack(&self) -> Restack<'_> {
            Restack::new(&self.worktree, "task", &self.fork_point, &self.old_tip, &self.onto, &self.backup)
        }

        fn tip(&self) -> Option<String> {
            ref_in(&self.worktree, "refs/heads/task")
        }

        fn backup_at(&self) -> Option<String> {
            ref_in(&self.worktree, &self.backup)
        }

        fn head(&self) -> String {
            git_in(&self.worktree, &["rev-parse", "HEAD"])
        }

        fn rebase_in_progress(&self) -> bool {
            ["rebase-merge", "rebase-apply"].iter().any(|name| {
                let path = git_in(&self.worktree, &["rev-parse", "--git-path", name]);
                self.worktree.join(path).exists()
            })
        }

        /// Nothing reached `origin` but `main`.
        fn assert_nothing_pushed(&self) {
            assert_eq!(ref_in(&self.origin, "refs/heads/task"), None, "nothing may be pushed");
        }

        /// The ordinary outcome of a rollback: the branch and the worktree
        /// exactly at the old tip, clean, and the backup retired.
        fn assert_restored(&self, name: &str) {
            assert_eq!(self.tip().as_deref(), Some(self.old_tip.as_str()), "{name}");
            assert_eq!(git_in(&self.worktree, &["symbolic-ref", "HEAD"]), "refs/heads/task", "{name}");
            assert_eq!(self.head(), self.old_tip, "{name}");
            assert!(!self.rebase_in_progress(), "{name}");
            assert_eq!(git_in(&self.worktree, &["status", "--porcelain", "--untracked-files=all"]), "", "{name}");
            assert_eq!(self.backup_at(), None, "{name}: the backup is retired");
            self.assert_nothing_pushed();
        }
    }

    fn commit(dir: &Path, file: &str, content: &str, subject: &str) -> String {
        std::fs::write(dir.join(file), content).unwrap();
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", subject]);
        git_in(dir, &["rev-parse", "HEAD"])
    }

    fn ref_in(dir: &Path, refname: &str) -> Option<String> {
        git_in(dir, &["for-each-ref", "--format=%(refname) %(objectname)", refname])
            .lines()
            .find_map(|line| {
                let (name, oid) = line.split_once(' ')?;
                (name == refname).then(|| oid.to_string())
            })
    }

    fn describe(failure: &RestackFailure) -> &str {
        match failure {
            RestackFailure::Restored(why) | RestackFailure::NotRestored(why) | RestackFailure::BranchMoved(why) => why,
        }
    }

    /// A restack that completed and verified is rolled back because its
    /// caller could not record it, but a commit was made on the branch from
    /// the task's terminal in between. The branch is not SlashIt's to move
    /// any more: the commit stays its tip, the backup is kept, and the
    /// failure says so.
    #[tokio::test]
    async fn a_rollback_after_a_terminal_commit_leaves_the_branch_alone() {
        let fixture = Fixture::new(false);
        let restack = fixture.restack();
        let new_tip = restack.replay().await.unwrap_or_else(|_| panic!("the replay verifies"));
        assert_eq!(fixture.tip().as_deref(), Some(new_tip.as_str()));
        let user = commit(&fixture.worktree, "u.txt", "u\n", "U");

        let failure = restack.roll_back("the task could not record the restack".to_string()).await;

        assert!(
            matches!(&failure, RestackFailure::BranchMoved(why) if why.contains(&user)),
            "{}",
            describe(&failure)
        );
        assert_eq!(fixture.tip().as_deref(), Some(user.as_str()), "the terminal commit stays the tip");
        assert_eq!(fixture.backup_at().as_deref(), Some(fixture.old_tip.as_str()), "the backup is kept");
        fixture.assert_nothing_pushed();
    }

    /// A completed restack whose worktree was moved off the branch before
    /// the rollback, without the branch itself moving, is not rolled back
    /// either: the ref would move under a `HEAD` SlashIt no longer owns.
    #[tokio::test]
    async fn a_rollback_after_the_worktree_left_the_branch_leaves_it_alone() {
        let fixture = Fixture::new(false);
        let restack = fixture.restack();
        let new_tip = restack.replay().await.unwrap_or_else(|f| panic!("{}", describe(&f)));
        git_in(&fixture.worktree, &["checkout", "-q", "--detach"]);

        let failure = restack.roll_back("the task could not record the restack".to_string()).await;

        assert!(matches!(failure, RestackFailure::BranchMoved(_)), "{}", describe(&failure));
        assert_eq!(fixture.tip().as_deref(), Some(new_tip.as_str()));
        assert!(git_in(&fixture.worktree, &["branch", "--show-current"]).is_empty(), "still detached");
        assert_eq!(fixture.backup_at().as_deref(), Some(fixture.old_tip.as_str()));
        fixture.assert_nothing_pushed();
    }

    /// Moving the files back never overwrites a change made in the worktree:
    /// when git refuses to, the branch ref is back at the old tip, the change
    /// is still there, the backup is kept, and the failure says what to run.
    #[tokio::test]
    async fn a_rollback_that_would_overwrite_a_local_change_keeps_it() {
        let fixture = Fixture::new(false);
        let restack = fixture.restack();
        restack.replay().await.unwrap_or_else(|f| panic!("{}", describe(&f)));
        std::fs::write(fixture.worktree.join("m.txt"), "edited\n").unwrap();

        let failure = restack.roll_back("the task could not record the restack".to_string()).await;

        assert!(
            matches!(&failure, RestackFailure::NotRestored(why) if why.contains("read-tree")),
            "{}",
            describe(&failure)
        );
        assert_eq!(fixture.tip().as_deref(), Some(fixture.old_tip.as_str()));
        assert_eq!(std::fs::read_to_string(fixture.worktree.join("m.txt")).unwrap(), "edited\n");
        assert_eq!(fixture.backup_at().as_deref(), Some(fixture.old_tip.as_str()));
        fixture.assert_nothing_pushed();
    }

    /// A rebase that stopped on a conflict is aborted only while it is still
    /// exactly as SlashIt's rebase left it. A commit made from the terminal
    /// on the detached `HEAD`, or a branch moved by another tool, leaves the
    /// rebase and the branch alone and the backup kept.
    #[tokio::test]
    async fn a_conflicted_restack_that_the_user_touched_is_not_aborted() {
        for moved_branch in [false, true] {
            let fixture = Fixture::new(true);
            let restack = fixture.restack();
            let ran = restack.rebase().await.unwrap_or_else(|f| panic!("{}", describe(&f)));
            assert_ne!(ran.code, Some(0));
            assert!(fixture.rebase_in_progress());
            let (expected_tip, expected_head) = if moved_branch {
                let tree = git_in(&fixture.worktree, &["rev-parse", &format!("{}^{{tree}}", fixture.old_tip)]);
                let moved = git_in(&fixture.worktree, &["commit-tree", "-p", &fixture.old_tip, "-m", "U", &tree]);
                git_in(&fixture.worktree, &["update-ref", "refs/heads/task", &moved, &fixture.old_tip]);
                (moved, fixture.head())
            } else {
                std::fs::write(fixture.worktree.join("b.txt"), "resolved\n").unwrap();
                git_in(&fixture.worktree, &["add", "b.txt"]);
                git_in(&fixture.worktree, &["commit", "-q", "-m", "U"]);
                (fixture.old_tip.clone(), fixture.head())
            };

            let failure = restack.settle(ran).await.expect_err("not restacked");

            assert!(
                matches!(&failure, RestackFailure::BranchMoved(why) if why.contains("moved")),
                "moved_branch={moved_branch}: {}",
                describe(&failure)
            );
            assert!(fixture.rebase_in_progress(), "moved_branch={moved_branch}: not aborted");
            assert_eq!(fixture.head(), expected_head, "moved_branch={moved_branch}");
            assert_eq!(fixture.tip().as_deref(), Some(expected_tip.as_str()), "moved_branch={moved_branch}");
            assert_eq!(fixture.backup_at().as_deref(), Some(fixture.old_tip.as_str()));
            fixture.assert_nothing_pushed();
        }
    }

    /// Undoing a stopped rebase never writes the branch: a rebase that
    /// stopped never moved it. A branch moved in the window after the checks
    /// that the stop is this restack's, and before it is undone, keeps what
    /// it was moved to, and the backup is kept.
    #[tokio::test]
    async fn undoing_a_stopped_restack_never_moves_the_branch_back() {
        let fixture = Fixture::new(true);
        let restack = fixture.restack();
        let ran = restack.rebase().await.unwrap_or_else(|f| panic!("{}", describe(&f)));
        assert_ne!(ran.code, Some(0));
        let tree = git_in(&fixture.worktree, &["rev-parse", &format!("{}^{{tree}}", fixture.old_tip)]);
        let user = git_in(&fixture.worktree, &["commit-tree", "-p", &fixture.old_tip, "-m", "U", &tree]);
        *restack.before_undoing_a_stop.lock().unwrap() = Some(Box::new({
            let worktree = fixture.worktree.clone();
            let (user, old_tip) = (user.clone(), fixture.old_tip.clone());
            move || {
                git_in(&worktree, &["update-ref", "refs/heads/task", &user, &old_tip]);
            }
        }));

        let failure = restack.settle(ran).await.expect_err("not restacked");

        assert_eq!(fixture.tip().as_deref(), Some(user.as_str()), "the branch keeps what it was moved to");
        assert!(!matches!(failure, RestackFailure::Restored(_)), "{}", describe(&failure));
        assert_eq!(fixture.backup_at().as_deref(), Some(fixture.old_tip.as_str()));
        fixture.assert_nothing_pushed();
    }

    #[test]
    fn no_update_refs_is_passed_only_to_a_git_that_knows_it() {
        for (version, knows) in [
            ("git version 2.55.0", true),
            ("git version 2.38.0", true),
            ("git version 2.37.7", false),
            ("git version 2.39.5 (Apple Git-154)", true),
            ("git version 2.45.1.windows.1", true),
            ("git version 2.34.1.windows.2", false),
            ("git version 1.9.5", false),
            ("git version 3.0.0", true),
            ("git version 2.40.0-rc1\n", true),
            ("something else", true),
            ("git version", true),
            ("git version abc", true),
        ] {
            assert_eq!(knows_no_update_refs(version), knows, "{version:?}");
        }
    }

    /// A conflict rerere has a recorded resolution for is still SlashIt's
    /// own stop, not work of the user's: the restack runs with rerere off,
    /// so the stop is left as git made it and undone, and no recorded
    /// resolution is replayed into what the restack verifies.
    #[tokio::test]
    async fn a_stop_rerere_could_resolve_is_undone_as_the_restacks_own() {
        let fixture = Fixture::new(true);
        let wt = &fixture.worktree;
        git_in(wt, &["config", "rerere.enabled", "true"]);
        git_in(wt, &["config", "rerere.autoUpdate", "true"]);
        let stopped = StdCommand::new("git")
            .args(["rebase", "-q", "--onto", &fixture.onto, &fixture.fork_point, "task"])
            .current_dir(wt)
            .output()
            .expect("run git");
        assert!(!stopped.status.success(), "the rebase must stop on the conflict");
        std::fs::write(wt.join("b.txt"), "resolved\n").unwrap();
        git_in(wt, &["rerere"]);
        git_in(wt, &["rebase", "--abort"]);
        assert_eq!(fixture.tip().as_deref(), Some(fixture.old_tip.as_str()));

        let failure = fixture.restack().replay().await.expect_err("conflict");

        assert!(matches!(failure, RestackFailure::Restored(_)), "{}", describe(&failure));
        fixture.assert_restored("rerere");
    }

    /// A submodule whose pointer `onto` moved shows as changed at the stop,
    /// since the rebase does not update its checkout. That is not the
    /// user's work, and undoing the stop does not touch the submodule.
    #[tokio::test]
    async fn a_stop_with_a_moved_submodule_is_undone_as_the_restacks_own() {
        let fixture = Fixture::with_submodule();

        let failure = fixture.restack().replay().await.expect_err("conflict");

        assert!(matches!(failure, RestackFailure::Restored(_)), "{}", describe(&failure));
        fixture.assert_restored("submodule");
    }

    /// Whatever `submodule.recurse` says, undoing a stop never goes into a
    /// submodule's checkout: an edit made there during the stop is kept.
    #[tokio::test]
    async fn undoing_a_stop_keeps_an_edit_inside_a_submodule() {
        let fixture = Fixture::with_submodule();
        git_in(&fixture.worktree, &["config", "submodule.recurse", "true"]);
        let restack = fixture.restack();
        let ran = restack.rebase().await.unwrap_or_else(|f| panic!("{}", describe(&f)));
        assert_ne!(ran.code, Some(0));
        let edited = fixture.worktree.join("sub").join("s.txt");
        std::fs::write(&edited, "edited in the submodule\n").unwrap();

        let failure = restack.settle(ran).await.expect_err("not restacked");

        assert_eq!(
            std::fs::read_to_string(&edited).unwrap(),
            "edited in the submodule\n",
            "{}",
            describe(&failure)
        );
        assert_eq!(fixture.tip().as_deref(), Some(fixture.old_tip.as_str()));
        fixture.assert_nothing_pushed();
    }

    /// A submodule the user replaced with a regular file during the stop is
    /// a type change, not the rebase's moved pointer: the undo is refused
    /// and the file is kept.
    #[tokio::test]
    async fn undoing_a_stop_refuses_a_submodule_replaced_by_a_file() {
        let fixture = Fixture::with_submodule();
        let restack = fixture.restack();
        let ran = restack.rebase().await.unwrap_or_else(|f| panic!("{}", describe(&f)));
        assert_ne!(ran.code, Some(0));
        let sub = fixture.worktree.join("sub");
        std::fs::remove_dir_all(&sub).unwrap();
        std::fs::write(&sub, "a file now\n").unwrap();

        let failure = restack.settle(ran).await.expect_err("not restacked");

        assert!(matches!(failure, RestackFailure::BranchMoved(_)), "{}", describe(&failure));
        assert_eq!(std::fs::read_to_string(&sub).unwrap(), "a file now\n");
        assert!(fixture.rebase_in_progress(), "the stop is left as it is");
        assert_eq!(fixture.backup_at().as_deref(), Some(fixture.old_tip.as_str()));
        fixture.assert_nothing_pushed();
    }

    /// A submodule replaced by a file is refused even where configuration
    /// tells `git diff` to ignore that submodule.
    #[tokio::test]
    async fn undoing_a_stop_refuses_a_replaced_submodule_that_configuration_ignores() {
        for (key, value) in [("submodule.sub.ignore", "all"), ("diff.ignoreSubmodules", "all")] {
            let fixture = Fixture::with_submodule();
            git_in(&fixture.worktree, &["config", key, value]);
            let restack = fixture.restack();
            let ran = restack.rebase().await.unwrap_or_else(|f| panic!("{}", describe(&f)));
            let sub = fixture.worktree.join("sub");
            std::fs::remove_dir_all(&sub).unwrap();
            std::fs::write(&sub, "a file now\n").unwrap();

            let failure = restack.settle(ran).await.expect_err("not restacked");

            assert!(matches!(failure, RestackFailure::BranchMoved(_)), "{key}: {}", describe(&failure));
            assert_eq!(std::fs::read_to_string(&sub).unwrap(), "a file now\n", "{key}");
            assert_eq!(fixture.backup_at().as_deref(), Some(fixture.old_tip.as_str()), "{key}");
        }
    }

    /// Work staged during the stop on a path the stopped commit does not
    /// change, an edit or a new file, is the user's: the undo is refused and
    /// the work kept.
    #[tokio::test]
    async fn undoing_a_stop_refuses_staged_work_beside_it() {
        for new_file in [false, true] {
            let fixture = Fixture::new(true);
            let restack = fixture.restack();
            let ran = restack.rebase().await.unwrap_or_else(|f| panic!("{}", describe(&f)));
            let file = if new_file { "n.txt" } else { "f.txt" };
            std::fs::write(fixture.worktree.join(file), "staged by the user\n").unwrap();
            git_in(&fixture.worktree, &["add", file]);

            let failure = restack.settle(ran).await.expect_err("not restacked");

            assert!(matches!(failure, RestackFailure::BranchMoved(_)), "{file}: {}", describe(&failure));
            assert_eq!(
                std::fs::read_to_string(fixture.worktree.join(file)).unwrap(),
                "staged by the user\n",
                "{file}"
            );
            assert!(fixture.rebase_in_progress(), "{file}: the stop is left as it is");
            assert_eq!(fixture.backup_at().as_deref(), Some(fixture.old_tip.as_str()), "{file}");
        }
    }

    /// A stop after earlier commits replayed cleanly is still the
    /// restack's own, and is undone to the old tip exactly.
    #[tokio::test]
    async fn a_stop_on_the_second_commit_is_undone_as_the_restacks_own() {
        let fixture = Fixture::stopping_on_the_second_commit();

        let failure = fixture.restack().replay().await.expect_err("conflict");

        assert!(
            matches!(&failure, RestackFailure::Restored(why) if why.contains("f.txt")),
            "{}",
            describe(&failure)
        );
        fixture.assert_restored("second commit");
    }

    /// NUL-separated listings are read as git writes them. A resolution
    /// staged at a path whose name starts with a space, so that it comes
    /// first in every listing, is still found.
    #[tokio::test]
    async fn a_staged_resolution_at_a_path_with_leading_space_is_found() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let worktree = tmp.path().join("worktree");
        git_in(tmp.path(), &["init", "-q", "-b", "main", repo.to_str().unwrap()]);
        let fork_point = commit(&repo, "f.txt", "f\n", "F");
        git_in(&repo, &["checkout", "-q", "-b", "task"]);
        std::fs::write(repo.join(" lead.txt"), "task\n").unwrap();
        let old_tip = commit(&repo, "z.txt", "task\n", "B1");
        git_in(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join(" lead.txt"), "main\n").unwrap();
        let onto = commit(&repo, "z.txt", "main\n", "M");
        git_in(&repo, &["worktree", "add", "-q", worktree.to_str().unwrap(), "task"]);
        let backup = backup_ref(Uuid::new_v4());
        let restack = Restack::new(&worktree, "task", &fork_point, &old_tip, &onto, &backup);
        let ran = restack.rebase().await.unwrap_or_else(|f| panic!("{}", describe(&f)));
        assert_ne!(ran.code, Some(0));
        assert_eq!(git_in(&worktree, &["diff", "--name-only", "--diff-filter=U"]).lines().count(), 2, "both conflict");
        std::fs::write(worktree.join(" lead.txt"), "resolved\n").unwrap();
        git_in(&worktree, &["add", " lead.txt"]);

        let found = staged_resolutions_in_the_stop(&worktree).await.unwrap();

        assert!(found.as_deref().is_some_and(|why| why.contains(" lead.txt")), "{found:?}");
    }

    /// The restack runs git's merge backend whatever `rebase.backend` says:
    /// the reflog and stopped-state checks are written against it. A stop
    /// keeps its state in `rebase-merge/`, never `rebase-apply/`, and is
    /// undone as the restack's own.
    #[tokio::test]
    async fn a_restack_stops_with_the_merge_backend_whatever_the_configuration() {
        let fixture = Fixture::new(true);
        git_in(&fixture.worktree, &["config", "rebase.backend", "apply"]);
        let restack = fixture.restack();
        let ran = restack.rebase().await.unwrap_or_else(|f| panic!("{}", describe(&f)));
        assert_ne!(ran.code, Some(0));
        let state = |name: &str| fixture.worktree.join(git_in(&fixture.worktree, &["rev-parse", "--git-path", name]));
        assert!(state("rebase-merge").is_dir(), "the stop is the merge backend's");
        assert!(!state("rebase-apply").exists(), "not the apply backend's");

        let failure = restack.settle(ran).await.expect_err("conflict");

        assert!(matches!(failure, RestackFailure::Restored(_)), "{}", describe(&failure));
        fixture.assert_restored("apply configured, conflict");
    }

    /// A clean restack under `rebase.backend=apply` still runs as the merge
    /// backend, and verifies. The two backends log the same reflog messages
    /// on current git, so a `post-rewrite` hook records which backend's
    /// state directory the rebase is running with.
    #[tokio::test]
    async fn a_clean_restack_uses_the_merge_backend_whatever_the_configuration() {
        let fixture = Fixture::new(false);
        git_in(&fixture.worktree, &["config", "rebase.backend", "apply"]);
        let hooks = fixture._tmp.path().join("hooks");
        let seen = fixture._tmp.path().join("backend-seen");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("post-rewrite");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\ncat >/dev/null\nif [ -d \"$(git rev-parse --git-path rebase-merge)\" ]; then echo merge; \
                 else echo other; fi > {seen:?}\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git_in(&fixture.worktree, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
        let restack = fixture.restack();

        let new_tip = restack.replay().await.unwrap_or_else(|f| panic!("{}", describe(&f)));

        assert_eq!(std::fs::read_to_string(&seen).unwrap().trim(), "merge");
        assert_eq!(fixture.tip().as_deref(), Some(new_tip.as_str()));
        assert_ne!(new_tip, fixture.old_tip);
    }

    /// When the rebase never started, SlashIt changed nothing, so it has
    /// nothing to undo: a branch that moved meanwhile is not reset.
    #[tokio::test]
    async fn a_restack_that_never_started_does_not_reset_a_moved_branch() {
        let fixture = Fixture::new(false);
        let restack = fixture.restack();
        let user = commit(&fixture.worktree, "u.txt", "u\n", "U");

        let failure = restack.restore(false, "the rebase could not be started: no identity".to_string()).await;

        assert!(matches!(&failure, RestackFailure::BranchMoved(why) if why.contains(&user)), "{}", describe(&failure));
        assert_eq!(fixture.tip().as_deref(), Some(user.as_str()));
        assert_eq!(fixture.head(), user);
        assert_eq!(fixture.backup_at().as_deref(), Some(fixture.old_tip.as_str()));
        fixture.assert_nothing_pushed();
    }

    /// With nobody else in the worktree, both rollbacks still restore the old
    /// tip exactly: after a conflict, and after a completed replay.
    #[tokio::test]
    async fn an_untouched_restack_is_rolled_back_exactly() {
        let fixture = Fixture::new(true);
        let failure = fixture.restack().replay().await.expect_err("conflict");
        assert!(matches!(failure, RestackFailure::Restored(_)));
        fixture.assert_restored("conflict");

        let fixture = Fixture::new(false);
        let restack = fixture.restack();
        let new_tip = restack.replay().await.unwrap_or_else(|_| panic!("the replay verifies"));
        assert_ne!(new_tip, fixture.old_tip);
        let failure = restack.roll_back("the task could not record the restack".to_string()).await;
        assert!(matches!(failure, RestackFailure::Restored(_)));
        fixture.assert_restored("completed");
    }
}
