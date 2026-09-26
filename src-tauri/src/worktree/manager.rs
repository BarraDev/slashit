use super::checked_task_branch;
use crate::config::paths::{AppPaths, ProjectKey, WorktreePlacement};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

pub struct WorktreeManager {
    wt_available: bool,
    paths: Arc<AppPaths>,
    placement: WorktreePlacement,
}

pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
}

/// A worktree [`WorktreeManager::create_stacked_branch`] produced, with the
/// dependency commit it verified the branch is stacked on.
pub struct StackedWorktree {
    pub info: WorktreeInfo,
    /// The dependency's tip as resolved and checked at the time. The task's
    /// diff starts here, rather than at the worktree's `HEAD`, which on a
    /// resumed branch already includes the task's own commits.
    pub dependency_tip: String,
    /// Whether an existing branch of the task's name was picked up rather
    /// than a new one created.
    pub resumed: bool,
}

/// The result of asking the filesystem whether a path is there, keeping the
/// distinction [`Path::exists`] collapses: "definitely not there" and
/// "could not be asked" are different answers, and every gate that decides
/// something destructive or something irreversible on the strength of a
/// path's presence needs to tell them apart rather than treat both as
/// `false`.
enum Presence {
    Present,
    Absent,
    Unverified(std::io::Error),
}

impl Presence {
    /// `metadata` follows symlinks, as [`Path::exists`] does: a link
    /// pointing at nothing is `Absent`, not `Present`, and a live link is
    /// `Present` regardless of what kind of file it names.
    fn of(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Ok(_) => Presence::Present,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Presence::Absent,
            Err(error) => Presence::Unverified(error),
        }
    }
}

/// The outcome of checking git for a task whose recorded worktree directory
/// has gone missing.
///
/// Startup used to have only two answers here, because the git listing it
/// consulted returned an empty string whether git had reported nothing or had
/// failed to run at all. `worktree_path` is the only persisted record of a
/// worktree, so answering "absent" out of a failed check discarded the only
/// handle back to a directory that may well still exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeRecovery {
    /// Git confirms a worktree registered for this branch at an adoptable
    /// path. The task should be re-pointed at it.
    Adopt(String),
    /// Git answered, and has no adoptable worktree for this branch. The
    /// recorded path is genuinely dead and may be cleared.
    ConfirmedAbsent,
    /// Git could not be consulted, so absence was never established. The
    /// reference must be kept for a later start to verify again.
    Unverified,
}

impl WorktreeManager {
    /// Whether `name` is an executable on `PATH`.
    ///
    /// A directory walk rather than `which`, because `which` is a subprocess
    /// and this runs during construction. A forked child inherits every open
    /// descriptor until it reaches `exec`, and the single-instance ownership
    /// lease is an `flock` held on one of them, so a probe that forks while
    /// that lease is being released holds it open past the release -- long
    /// enough for the next acquisition to be told, wrongly, that another
    /// instance is already listening. Measured: with the two `which` calls in
    /// place the IPC ownership regression failed 9 runs in 12 against parallel
    /// construction, and 0 in 12 with them gone.
    ///
    /// Answering it directly is also simply the smaller thing to do: two
    /// process spawns at startup, to read a variable this process already has.
    fn on_path(name: &str) -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(name);
            std::fs::metadata(&candidate).is_ok_and(|meta| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    meta.is_file() && meta.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    meta.is_file()
                }
            })
        })
    }

    pub fn new(paths: Arc<AppPaths>, placement: WorktreePlacement) -> Self {
        let wt_available = Self::on_path("wt");

        let manager = Self {
            wt_available,
            paths,
            placement,
        };

        if manager.delegates_to_wt() {
            println!("SlashIt: Worktrunk (wt) detected — delegating worktree placement to it");
        } else {
            println!(
                "SlashIt: managing worktrees under {}",
                manager.paths.data_dir().join("worktrees").display()
            );
        }

        manager
    }

    /// Whether worktree placement is handed to worktrunk.
    ///
    /// `wt switch` accepts no target path, so delegating means SlashIt does not
    /// choose the directory — worktrunk's own `worktree-path` template does.
    /// That is deliberate under [`WorktreePlacement::Auto`]: it is the user's
    /// tool and their hooks. [`WorktreePlacement::Managed`] takes the decision
    /// back.
    fn delegates_to_wt(&self) -> bool {
        self.wt_available && matches!(self.placement, WorktreePlacement::Auto)
    }

    /// Where SlashIt would place this branch's worktree.
    fn managed_path(&self, repo_path: &str, branch: &str) -> PathBuf {
        let key = ProjectKey::for_path(Path::new(repo_path)).key;
        self.paths.worktree_path(&key, branch)
    }

    /// The pre-`AppPaths` location: a sibling of the repository named
    /// `<repo>.<branch>`.
    ///
    /// Still consulted so that a worktree created by an older version is
    /// adopted rather than orphaned, which would otherwise strand the branch
    /// and any uncommitted work in it.
    fn legacy_path(repo_path: &str, branch: &str) -> Option<PathBuf> {
        let repo_dir = Path::new(repo_path);
        let repo_name = repo_dir.file_name()?.to_str()?;
        let parent = repo_dir.parent()?;
        Some(parent.join(format!("{repo_name}.{branch}")))
    }

    /// Find a worktree for `branch` that already exists on disk and that git
    /// itself confirms is registered for that exact branch.
    ///
    /// Used at startup to re-point a task whose recorded worktree path no
    /// longer resolves, before the reference is discarded as stale.
    ///
    /// `porcelain` is the output of `git worktree list --porcelain` for
    /// `repo_path`. A caller adopting many branches from the same repo in a
    /// loop (e.g. one task per branch at startup) should fetch it once via
    /// [`Self::worktree_list_porcelain`] and reuse it, rather than shelling
    /// out per task.
    pub fn adopt_existing(&self, repo_path: &str, branch: &str, porcelain: &str) -> Option<String> {
        self.adoptable_path(repo_path, branch, porcelain)
            .map(|p| p.to_string_lossy().to_string())
    }

    /// Run `git worktree list --porcelain` for `repo_path`, synchronously.
    ///
    /// Exists for callers outside an async context (startup adoption runs
    /// before the Tauri/Tokio runtime is driving anything) that still need
    /// to verify a candidate worktree path against git's own bookkeeping.
    ///
    /// `None` means git could not be consulted at all — it failed to spawn
    /// (no `git` on `PATH`, `repo_path` unreadable or gone) or exited
    /// non-zero (`repo_path` is not a repository). `Some` is git's own
    /// answer, and an empty listing inside it genuinely means nothing is
    /// registered.
    ///
    /// The distinction matters because a caller cannot treat the two the
    /// same way in both directions. Not adopting on an unusable listing is
    /// right — adopting blind is exactly what `adoptable_path` refuses to do.
    /// But *discarding* a recorded worktree reference needs positive proof of
    /// absence, and this function returning a bare empty string used to
    /// supply that proof out of a failed `git` invocation.
    pub fn worktree_list_porcelain(repo_path: &str) -> Option<String> {
        let output = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(repo_path)
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// What startup should do with a task whose recorded worktree directory
    /// is no longer on disk.
    ///
    /// `porcelain` is the result of [`Self::worktree_list_porcelain`] for the
    /// task's repository, so `None` carries "git could not tell us" through
    /// to the decision instead of collapsing it into "nothing is registered".
    ///
    /// Adoption stays fail-closed: a listing that cannot be obtained never
    /// adopts anything. For any listing git did produce the adoption decision
    /// is unchanged; the exit-status check in
    /// [`Self::worktree_list_porcelain`] makes adoption strictly narrower, not
    /// wider, since a non-zero exit whose stdout happened to parse can no
    /// longer be adopted from. What changes materially is the discard
    /// decision, and only where absence was never actually established.
    pub fn classify_missing_worktree(
        &self,
        repo_path: &str,
        branch: &str,
        porcelain: Option<&str>,
    ) -> WorktreeRecovery {
        let Some(porcelain) = porcelain else {
            return WorktreeRecovery::Unverified;
        };

        // `adopt_existing` only matches this app's own managed/legacy path
        // conventions. `adopt_any_registered` is the fallback for a worktree
        // git still has registered for this branch at some other path — for
        // example one `wt` placed under its own convention, which happens
        // whenever `WorktreePlacement::Auto` (the default) delegates to it.
        // Git's confirmation is already the trust boundary, so a worktree it
        // vouches for is exactly as real as one sitting where SlashIt would
        // itself have put it. Only once BOTH miss has git positively said
        // there is nothing to adopt — which is what `ConfirmedAbsent` means,
        // and the only basis on which a reference may be discarded.
        match self
            .adopt_existing(repo_path, branch, porcelain)
            .or_else(|| Self::adopt_any_registered(repo_path, branch, porcelain))
        {
            Some(path) => WorktreeRecovery::Adopt(path),
            None => WorktreeRecovery::ConfirmedAbsent,
        }
    }

    /// An existing worktree for this branch that SlashIt should reuse.
    ///
    /// Checked before creating anything, so an upgrade never abandons a
    /// worktree the user still has work in.
    ///
    /// A directory existing at the deterministic managed/legacy path is not
    /// enough on its own — it could be stale, pruned, or unrelated debris
    /// left behind on disk. The path is only trusted once `porcelain`
    /// confirms git itself has that exact path registered for `branch`.
    fn adoptable_path(&self, repo_path: &str, branch: &str, porcelain: &str) -> Option<PathBuf> {
        let registered = PathBuf::from(Self::worktree_for_branch(porcelain, branch)?);
        if !registered.is_dir() {
            return None;
        }

        let managed = self.managed_path(repo_path, branch);
        if registered == managed {
            return Some(managed);
        }

        if Self::legacy_path(repo_path, branch).as_deref() == Some(registered.as_path()) {
            return Some(registered);
        }

        None
    }

    /// The path git currently has registered for `branch`, whether or not
    /// anything is there.
    ///
    /// Unlike [`Self::adopt_any_registered`] this does not ask whether the
    /// directory exists, and that is the entire point of it. Startup uses it to
    /// decide whether an interrupted cleanup finished: a removal deletes the
    /// checkout's contents and takes the registration down last, so a
    /// registration that is still there is proof the removal did not reach its
    /// end, however empty the directory looks. Adoption must not use it, for
    /// the same reason -- a surviving registration is exactly the case where
    /// there is nothing safe to adopt.
    pub fn registration_for_branch(porcelain: &str, branch: &str) -> Option<String> {
        Self::worktree_for_branch(porcelain, branch)
    }

    /// Any worktree git currently has registered for `branch`, regardless of
    /// whether its path matches this app's managed or legacy conventions.
    ///
    /// Startup reconciliation uses this as a fallback after
    /// [`Self::adopt_existing`]: git-confirmation is already the trust
    /// boundary — see that method's doc comment — and a worktree placed by
    /// an external tool such as `wt` under [`WorktreePlacement::Auto`] (the
    /// default) never matches either convention, but is exactly as real as
    /// one that does. Reusing a worktree when *creating* one, by contrast,
    /// only makes sense at a path the app would itself create at, which is
    /// why [`Self::adopt_existing`] stays scoped to those two conventions
    /// and this method is not used there.
    ///
    /// `repo_path` is never itself adoptable: the primary checkout is a
    /// registered worktree for whatever branch it has checked out, and
    /// without this check a task branch checked out there would get
    /// repointed at the user's own working copy.
    pub fn adopt_any_registered(repo_path: &str, branch: &str, porcelain: &str) -> Option<String> {
        let registered = PathBuf::from(Self::worktree_for_branch(porcelain, branch)?);
        if !registered.is_dir() {
            return None;
        }
        // The primary checkout is itself a registered worktree for whatever
        // branch it currently has checked out. Adopting it would repoint the
        // task at the user's own working copy, and the executor would then run
        // an agent there and target it for cleanup.
        // `canonicalize` resolves symlinks so a `..`-relative or
        // symlink-aliased primary path still compares equal; a canonicalize
        // failure (a path git listed but that no longer resolves) falls back
        // to the literal comparison rather than treating it as a match --
        // failing closed, since an unresolvable primary path is not proof the
        // candidate is a distinct, safe checkout either.
        let same_as_primary = match (registered.canonicalize(), Path::new(repo_path).canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => registered == Path::new(repo_path),
        };
        if same_as_primary {
            return None;
        }
        Some(registered.to_string_lossy().to_string())
    }

    /// Async counterpart of [`Self::adoptable_path`] for callers already
    /// running on the Tokio runtime: fetches the porcelain listing itself
    /// rather than requiring the caller to supply one.
    async fn adoptable_path_live(&self, repo_path: &str, branch: &str) -> Option<PathBuf> {
        let output = tokio::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(repo_path)
            .output()
            .await
            .ok()?;
        let porcelain = String::from_utf8_lossy(&output.stdout);
        self.adoptable_path(repo_path, branch, &porcelain)
    }

    /// Generate a branch name from a task UUID (first 8 chars).
    pub fn branch_for_task(task_id: Uuid) -> String {
        format!("task-{}", &task_id.to_string()[..8])
    }

    /// Create a worktree for a task. Returns the worktree path and branch name.
    ///
    /// Callers pass [`Self::branch_for_task`] here today, but the name is
    /// checked all the same, so that every way of acquiring a worktree holds
    /// its branch to the same contract before any process sees it.
    pub async fn create(&self, repo_path: &str, branch: &str) -> Result<WorktreeInfo, String> {
        self.create_or_adopt(repo_path, branch).await.map(|(info, _)| info)
    }

    /// [`Self::create`], also saying whether the worktree was adopted: an
    /// existing worktree of `branch` reused rather than a new branch
    /// created. Where an adopted branch started is not known here.
    pub async fn create_or_adopt(
        &self,
        repo_path: &str,
        branch: &str,
    ) -> Result<(WorktreeInfo, bool), String> {
        let branch = checked_task_branch(branch)?;
        if let Some(existing) = self.adoptable_path_live(repo_path, branch).await {
            let info = WorktreeInfo {
                path: existing.to_string_lossy().to_string(),
                branch: branch.to_string(),
            };
            return Ok((info, true));
        }
        let info = if self.delegates_to_wt() {
            self.create_with_wt(repo_path, branch).await?
        } else {
            self.create_with_git(repo_path, branch).await?
        };
        Ok((info, false))
    }

    /// Reattach to an existing branch (no -c flag). Used when re-queuing a task
    /// that already has a branch from a previous execution.
    ///
    /// `branch` is the task's recorded `branch_name`, read back from a board
    /// file that may be kept in the project and so may say anything; it is
    /// refused, before `wt` or `git` is run, unless it is a plain branch name.
    pub async fn reattach(&self, repo_path: &str, branch: &str) -> Result<WorktreeInfo, String> {
        let branch = checked_task_branch(branch)?;
        if let Some(existing) = self.adoptable_path_live(repo_path, branch).await {
            return Ok(WorktreeInfo {
                path: existing.to_string_lossy().to_string(),
                branch: branch.to_string(),
            });
        }
        // Attaching by name alone is not enough: with no local branch of
        // that name, `git worktree add` reads a full object ID, `FETCH_HEAD`
        // or `ORIG_HEAD` as a commit and checks it out detached.
        Self::local_branch_tip(repo_path, branch).await?;

        if self.delegates_to_wt() {
            // wt switch to existing branch (no -c)
            let output = tokio::process::Command::new("wt")
                .args(["switch", branch, "--no-cd", "-y", "--no-verify"])
                .current_dir(repo_path)
                .output()
                .await
                .map_err(|e| format!("Failed to run wt switch: {}", e))?;
            if !output.status.success() {
                return Err(format!(
                    "wt switch failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            self.find_worktree_path(repo_path, branch).await
        } else {
            // git worktree add without -b (attach to an existing branch)
            let worktree_path = self.managed_path(repo_path, branch);
            self.git_worktree_add(repo_path, &worktree_path, branch, false)
                .await
        }
    }

    /// Run `git worktree add`, creating the external worktree root first.
    ///
    /// `create_branch` selects `-b <branch>` (a new branch) over `<branch>`
    /// (attach to an existing one).
    async fn git_worktree_add(
        &self,
        repo_path: &str,
        worktree_path: &Path,
        branch: &str,
        create_branch: bool,
    ) -> Result<WorktreeInfo, String> {
        if let Some(parent) = worktree_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create worktree root {}: {e}", parent.display()))?;
        }

        let dest = worktree_path
            .to_str()
            .ok_or_else(|| "Worktree path is not valid UTF-8".to_string())?;

        let output = tokio::process::Command::new("git")
            .args(Self::git_worktree_add_args(dest, branch, create_branch))
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| format!("Failed to create git worktree: {}", e))?;

        if !output.status.success() {
            return Err(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(WorktreeInfo {
            path: worktree_path.to_string_lossy().to_string(),
            branch: branch.to_string(),
        })
    }

    /// The arguments for `git worktree add`, with `--` before the positional
    /// arguments so that neither the destination nor an attached branch can
    /// be read as an option. `-b` has to come before the `--`; after it, git
    /// would take `-b` as the path. A revision such as `HEAD~1` is still a
    /// valid `<commit-ish>` after the `--`, which is why callers also check
    /// the branch with [`checked_task_branch`].
    fn git_worktree_add_args<'a>(dest: &'a str, branch: &'a str, create_branch: bool) -> Vec<&'a str> {
        if create_branch {
            vec!["worktree", "add", "-b", branch, "--", dest]
        } else {
            vec!["worktree", "add", "--", dest, branch]
        }
    }

    /// The commit the local branch `branch` points at, or an error if there
    /// is no such branch.
    ///
    /// Asks for `refs/heads/<branch>` in full because the short name can
    /// mean something else: a full object ID, `FETCH_HEAD` or `ORIG_HEAD`
    /// when no branch has that name, or a tag of the same name.
    async fn local_branch_tip(repo_path: &str, branch: &str) -> Result<String, String> {
        let output = tokio::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}^{{commit}}"))
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| format!("Failed to run git rev-parse: {e}"))?;
        if !output.status.success() {
            return Err(format!("There is no local branch {branch:?}"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Whether the ref `refs/heads/<branch>` exists in `repo_path`.
    ///
    /// `Ok(false)` only when git listed the local branches cleanly and this
    /// one is not among them. A name that is not a plain branch name, a ref
    /// git warns it cannot read, or a git that could not be asked is an
    /// error, never an absence. A ref that names a missing object is
    /// present: it is there, and whatever uses it next refuses it.
    ///
    /// `for-each-ref` rather than `rev-parse --verify` or `show-ref
    /// --verify --quiet`: both of those exit 1 in silence for a ref that is
    /// there but broken (a missing object, or unreadable contents), exactly
    /// as they do for one that is not there at all.
    pub async fn local_branch_exists(repo_path: &str, branch: &str) -> Result<bool, String> {
        let branch = checked_task_branch(branch)?;
        let refname = format!("refs/heads/{branch}");
        let output = tokio::process::Command::new("git")
            .args(["for-each-ref", "--format=%(refname)", &refname])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| format!("Failed to run git for-each-ref: {e}"))?;
        if !output.status.success() || !output.stderr.is_empty() {
            return Err(format!(
                "Could not check for local branch {branch:?}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        // The pattern also matches every ref below `refs/heads/<branch>/`,
        // which is not this branch.
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line == refname))
    }

    /// Whether `ancestor` is reachable from the local branch `branch`.
    async fn branch_contains(repo_path: &str, branch: &str, ancestor: &str) -> Result<bool, String> {
        let output = tokio::process::Command::new("git")
            .args(["merge-base", "--is-ancestor", ancestor])
            .arg(format!("refs/heads/{branch}"))
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| format!("Failed to run git merge-base: {e}"))?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(format!(
                "Could not check whether branch {branch} contains {ancestor}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
        }
    }

    /// Create the local branch `branch` at exactly `commit`, failing if the
    /// branch already exists. `git update-ref` takes the object ID as it is,
    /// where `git branch` would prefer a ref that happens to share its name.
    async fn create_branch_at(repo_path: &str, branch: &str, commit: &str) -> Result<(), String> {
        let output = tokio::process::Command::new("git")
            .args(["update-ref", &format!("refs/heads/{branch}"), commit, ""])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| format!("Failed to run git update-ref: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "Could not create branch {branch} at {commit}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Delete the branch [`Self::create_branch_at`] created, but only while
    /// it still points at `commit` and no worktree has it checked out. If
    /// anything has moved it since, it is no longer only this operation's;
    /// and `git worktree add` can fail after checking it out (a failing
    /// `post-checkout` hook does that), where deleting it would leave that
    /// worktree on a branch that no longer exists. Either way it is kept and
    /// reported, as it is when git cannot list its worktrees.
    async fn discard_created_branch(
        repo_path: &str,
        branch: &str,
        commit: &str,
    ) -> Result<(), String> {
        match Self::worktree_list_porcelain(repo_path) {
            None => {
                return Err(format!(
                    "branch {branch} was kept: git could not list its worktrees"
                ))
            }
            Some(porcelain) => {
                if let Some(path) = Self::registration_for_branch(&porcelain, branch) {
                    return Err(format!(
                        "branch {branch} was kept: it is checked out at {path}"
                    ));
                }
            }
        }
        let output = tokio::process::Command::new("git")
            .args(["update-ref", "-d", &format!("refs/heads/{branch}"), commit])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| format!("Failed to run git update-ref -d: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "branch {branch} was kept: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Create a branch stacked on top of another branch.
    ///
    /// Under worktrunk delegation this is `wt switch -c --base`; otherwise
    /// SlashIt creates the branch at the dependency's tip itself and attaches
    /// a worktree to it. No other stacking tool is consulted, so what a task
    /// is stacked on does not depend on what happens to be installed. A
    /// branch of this name that already contains the dependency's tip is
    /// reattached instead, so an attempt that did not finish does not block
    /// the next; one that does not contain it is refused.
    ///
    /// `after_branch` is the dependency's recorded `branch_name`, which is as
    /// untrusted as the task's own, so both names are checked before `wt` or
    /// `git` is run.
    pub async fn create_stacked_branch(
        &self,
        repo_path: &str,
        branch: &str,
        after_branch: &str,
    ) -> Result<StackedWorktree, String> {
        let branch = checked_task_branch(branch)?;
        let after_branch = checked_task_branch(after_branch)
            .map_err(|e| format!("Cannot stack on the dependency's branch: {e}"))?;
        // Every tool below would read a name that is not a local branch as
        // the commit it resolves to, and stack on that instead.
        let dependency_tip = Self::local_branch_tip(repo_path, after_branch)
            .await
            .map_err(|e| format!("Cannot stack on the dependency's branch: {e}"))?;
        // A branch of this name left by an earlier attempt that did not
        // finish -- a worktree whose checkout hook failed, a start that died
        // before the task recorded its branch, a `wt switch` whose worktree
        // could not be found afterwards -- is picked up again rather than
        // failing every retry on "already exists". Only while it still holds
        // the dependency's work: a branch that does not is somebody else's,
        // and is refused and left exactly as it is.
        if Self::local_branch_exists(repo_path, branch).await? {
            if !Self::branch_contains(repo_path, branch, &dependency_tip).await? {
                return Err(format!(
                    "branch {branch} already exists and does not contain the dependency's \
                     branch {after_branch} (at {dependency_tip}); it was left as it is"
                ));
            }
            let info = self.reattach(repo_path, branch).await?;
            return Ok(StackedWorktree { info, dependency_tip, resumed: true });
        }
        let info = if self.delegates_to_wt() {
            // Worktrunk places the worktree and creates the branch from the
            // dependency.
            let output = tokio::process::Command::new("wt")
                .args(["switch", "-c", branch, "--base", after_branch, "--no-cd", "-y", "--no-verify"])
                .current_dir(repo_path)
                .output()
                .await
                .map_err(|e| format!("wt switch failed: {}", e))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("wt switch with base failed: {}", stderr));
            }

            self.find_worktree_path(repo_path, branch).await?
        } else {
            // Git path: create the branch at the dependency's tip,
            // then attach a worktree to it. Attach, not `-b` (`create_with_git`
            // always passes `-b`): the branch already exists from the line
            // above, so creating it again would fail. A branch that could not
            // be created stops here: attaching to whatever already has that
            // name would report a stack that is not stacked on anything. A
            // worktree that could not be attached takes the new branch with
            // it, or every retry would fail on the branch it left behind.
            Self::create_branch_at(repo_path, branch, &dependency_tip).await?;
            let worktree_path = self.managed_path(repo_path, branch);
            match self
                .git_worktree_add(repo_path, &worktree_path, branch, false)
                .await
            {
                Ok(info) => info,
                Err(e) => {
                    return match Self::discard_created_branch(repo_path, branch, &dependency_tip)
                        .await
                    {
                        Ok(()) => Err(e),
                        Err(kept) => Err(format!("{e}; {kept}")),
                    };
                }
            }
        };
        Ok(StackedWorktree { info, dependency_tip, resumed: false })
    }

    /// Remove a task's worktree -- the disposable checkout, and nothing else.
    ///
    /// Takes no branch, on purpose. A worktree is a second checkout of a
    /// branch that already exists without it, so removing one is not a reason
    /// to remove the other, and the branch is routinely the only ref naming
    /// the commits the task produced. Nothing this method can observe
    /// distinguishes work that is safe elsewhere from work that exists
    /// nowhere else, so it does not decide; callers that genuinely want a
    /// branch gone need their own contract for saying so, and today none
    /// does.
    pub async fn remove(&self, worktree_path: &str, repo_path: &str) -> Result<(), String> {
        if self.delegates_to_wt() {
            self.remove_with_wt(worktree_path, repo_path).await
        } else {
            self.remove_with_git(worktree_path, repo_path).await
        }
    }

    /// Check if a worktree directory exists on disk.
    pub fn exists(&self, worktree_path: &str) -> bool {
        Path::new(worktree_path).exists()
    }

    /// Whether `path` is proven gone, as opposed to merely unreadable.
    ///
    /// [`Self::exists`] answers `false` for two different things: a path
    /// that genuinely is not there, and a path `metadata` could not be asked
    /// about at all (a parent directory closed off, a transient I/O error).
    /// A removal's success gate needs positive evidence of absence rather
    /// than an absence of evidence, so only the first of those answers
    /// `true` here.
    ///
    /// `metadata` follows symlinks, as [`Path::exists`] does, and the two
    /// have to agree: a link pointing at nothing is a path with no worktree
    /// at it, and refusing to call that absent would withhold convergence
    /// from a removal that really did finish, leaving a caller retrying a
    /// worktree that is already gone.
    fn proven_absent(path: &str) -> bool {
        matches!(Presence::of(Path::new(path)), Presence::Absent)
    }

    // --- Private: wt-based operations ---

    /// The exact `wt remove` invocation this backend runs for every real
    /// removal. Kept as a named constant, rather than inlined at the one
    /// call site, so the branch-preservation and synchronicity contract it
    /// encodes -- `--foreground`, `--no-delete-branch` -- can be asserted
    /// deterministically (see
    /// [`tests::wt_remove_args_run_in_the_foreground_and_keep_the_branch`])
    /// without spawning `wt` at all.
    ///
    /// `--no-hooks` is the current spelling for what used to be
    /// `--no-verify`; `wt` still accepts the old name but warns it is
    /// deprecated.
    const WT_REMOVE_ARGS: [&'static str; 5] =
        ["remove", "-y", "--no-hooks", "--foreground", "--no-delete-branch"];

    async fn create_with_wt(&self, repo_path: &str, branch: &str) -> Result<WorktreeInfo, String> {
        let output = tokio::process::Command::new("wt")
            .args(["switch", "-c", branch, "--no-cd", "-y", "--no-verify"])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| format!("Failed to run wt switch: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("wt switch failed: {}", stderr));
        }

        // wt creates worktree as sibling: ../repo.branch
        // Find it via git worktree list
        self.find_worktree_path(repo_path, branch).await
    }

    /// Decide a removal's outcome on positive proof of absence, never on
    /// whatever exit status the attempt itself reported, and put git's own
    /// registration back if the attempt cost the worktree its record without
    /// actually removing it.
    ///
    /// Both backends share this contract exactly: `git worktree remove`
    /// (and `--force`) can fail with a non-zero exit without the directory
    /// actually being gone, and can also exit zero while leaving it behind;
    /// `wt remove` runs its own removal in the background and exits zero
    /// even when the directory survives, and its documented sequence --
    /// rename into its own trash, prune git's registration, delete the
    /// branch, a detached `rm -rf` -- can have the registration-pruning step
    /// land before an earlier or later step in that same sequence fails,
    /// whatever exit status that failure surfaces as. Neither tool's exit
    /// code is the signal to decide on; `Self::proven_absent` is.
    ///
    /// `record` is `None` when [`WorktreeRecord::save`] found nothing to
    /// preserve, which is not itself a failure -- a removal that never had a
    /// record to lose behaves exactly as it did before this existed.
    /// `failure` is the caller's own backend-specific description of what
    /// went wrong, used only when the worktree could not be proven gone; the
    /// wording stays entirely with each backend so this never has to guess
    /// which tool ran or why.
    fn finish_removal(
        worktree_path: &str,
        record: Option<WorktreeRecord>,
        mut failure: String,
    ) -> Result<(), String> {
        if Self::proven_absent(worktree_path) {
            return Ok(());
        }

        // A removal that cost the worktree its record without finishing has
        // also cost the retry this `Err` schedules any way to act: putting
        // it back is what makes the next attempt an ordinary removal rather
        // than an impossible one.
        if let Some(record) = &record {
            if let Err(error) = record.restore() {
                failure.push_str(&format!("; its git record could not be put back: {error}"));
            }
        }
        Err(failure)
    }

    async fn remove_with_wt(&self, worktree_path: &str, repo_path: &str) -> Result<(), String> {
        if !self.exists(worktree_path) {
            // `wt` is spawned with `current_dir(worktree_path)`, so a missing
            // directory fails at spawn with `NotFound` before `wt` ever runs.
            // That is anti-convergent rather than merely unhelpful: a first
            // attempt that did remove the directory guarantees every later
            // attempt fails. A cleanup that removed the worktree but could not
            // save the board retains `worktree_path` on purpose, so every
            // explicit retry the user asks for would fail on a worktree that is
            // already gone -- and this is the default backend whenever `wt` is
            // installed, because `WorktreePlacement::Auto` is the default.
            //
            // Handing this case to git rather than just returning `Ok(())` is
            // what makes it converge to a *usable* state. `wt` leaves the
            // registration behind when the directory disappears underneath it,
            // and a prunable registration is not inert: `wt switch <branch>`
            // then refuses with "Worktree directory missing", and
            // `wt switch -c <branch>` refuses because the branch still exists,
            // so the task could never get a worktree again. `git worktree
            // remove` clears a record whose directory is gone by itself, which
            // is what `remove_with_git` is being handed this case for, and
            // like this backend it leaves the branch alone.
            return self.remove_with_git(worktree_path, repo_path).await;
        }

        // Saved before anything runs, for the same reason `remove_with_git`
        // saves one: confirmed by hand against a real `wt`, its "Worktree
        // directory missing; pruned" is not proof the directory is gone --
        // only that `wt` could not stat it -- and it prunes git's
        // registration on that basis regardless.
        let record = WorktreeRecord::save(worktree_path, repo_path).await;

        // `--no-delete-branch`, in `Self::WT_REMOVE_ARGS` below, is what keeps
        // this backend's contract the same as the git one's. Measured against
        // worktrunk without it (v0.29.0 originally, and confirmed again on
        // v0.68.0): `wt remove` deletes the task branch whenever the branch
        // sits on the same commit as main -- it prints "Removing <branch>
        // worktree & branch (same commit as main)" and the branch is gone
        // afterwards. That is not an exotic state; it is the ordinary end of a
        // task, reached by every task whose agent committed nothing and by
        // every task whose work has already landed. The task branch is durable
        // committed task state -- it is what `create_pr` pushes and what
        // `reattach` checks out again -- so it has to survive cleanup.
        let output = tokio::process::Command::new("wt")
            .args(Self::WT_REMOVE_ARGS)
            .current_dir(worktree_path)
            .output()
            .await
            .map_err(|e| format!("Failed to run wt remove: {}", e))?;

        // `stderr` is kept only to explain a failure in the message below,
        // never to decide one -- see `finish_removal` for why exit status
        // is not that signal for this backend either.
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        // `wt remove` defaults to deleting the actual worktree in a detached
        // background job and returning as soon as that job is launched, but
        // `--foreground` opts this call out of that: confirmed by hand
        // against a real, installed `wt`, the checkout is already gone --
        // no sleep needed to observe it -- immediately after a `--foreground`
        // call returns, including through the cross-filesystem fallback to
        // plain `git worktree remove` `wt` documents for that mode. There is
        // therefore nothing to wait out here; `proven_absent` below is
        // evaluated exactly once, right after this call returns.
        //
        // That still is not the same as trusting the exit status itself: a
        // pre-remove hook failing, or `wt` hitting trouble partway through
        // its own removal (the scenario
        // `remove_with_wt_preserves_and_restores_the_registration_when_its_own_removal_cannot_finish`
        // exercises), can each surface as success or failure independently
        // of whether the checkout actually survived. `finish_removal` (via
        // `proven_absent`) is still the only thing that decides.
        let failure = if output.status.success() {
            format!("wt remove reported success but {worktree_path} could not be proven gone")
        } else if stderr.is_empty() {
            format!(
                "wt remove failed and {worktree_path} could not be proven gone, which gave no \
                 reason"
            )
        } else {
            format!("wt remove failed and {worktree_path} could not be proven gone: {stderr}")
        };
        Self::finish_removal(worktree_path, record, failure)
    }

    // --- Private: git-based fallback ---

    async fn create_with_git(&self, repo_path: &str, branch: &str) -> Result<WorktreeInfo, String> {
        let worktree_path = self.managed_path(repo_path, branch);
        self.git_worktree_add(repo_path, &worktree_path, branch, true)
            .await
    }

    async fn remove_with_git(&self, worktree_path: &str, repo_path: &str) -> Result<(), String> {
        // Saved before anything runs, because there is no asking for it back
        // afterwards: git tears its own record down at the end of a removal it
        // could not finish, and a checkout it no longer registers is one no
        // `git worktree` command will touch again.
        let record = WorktreeRecord::save(worktree_path, repo_path).await;

        // The ordinary removal is the only removal. There is deliberately no
        // `--force` fallback and no other destructive retry: `git worktree
        // remove` refuses precisely when the checkout holds content no commit
        // names -- a modified tracked file, a staged change, a non-ignored
        // untracked file -- and that refusal is the last thing standing
        // between an automatic cleanup and work that exists nowhere else.
        // Treating it as a signal to re-run with `--force` deleted exactly the
        // content the refusal was protecting, and then reported `Ok(())`, on
        // which the caller durably clears `worktree_path`. Git's own notion of
        // dirty is the whole policy here, so nothing below parses stderr to
        // second-guess it; ignored build output is not dirty to git and this
        // removal takes the checkout away build output and all.
        let output = tokio::process::Command::new("git")
            .args(["worktree", "remove", worktree_path])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| format!("Failed to remove worktree: {}", e))?;

        // Kept for the failure message below. A refusal git explains in
        // stderr is the only thing that can tell the user *which* file is
        // holding the cleanup back, and a non-zero exit on its own cannot.
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        // Git's own words are carried through verbatim, because this string
        // is what surfaces to the user and "cleanup failed" is not something
        // anyone can act on. "contains modified or untracked files, use
        // --force to delete it" names the fix a human may legitimately
        // choose by hand; SlashIt just will not choose it for them.
        let failure = if stderr.is_empty() {
            format!(
                "worktree at {worktree_path} could not be proven gone after `git worktree \
                 remove`, which gave no reason"
            )
        } else {
            format!(
                "worktree at {worktree_path} could not be proven gone after `git worktree \
                 remove`: {stderr}"
            )
        };
        Self::finish_removal(worktree_path, record, failure)?;

        // The branch is not touched here, and that is the whole point of this
        // function's contract.
        //
        // This used to run `git branch -D` whenever a `git worktree remove`
        // had exited 0. `-D` force-deletes regardless of merge state and
        // leaves no branch reflog, so for the ordinary case -- a task whose
        // agent committed its work on the task branch and pushed nothing --
        // the branch was the only ref naming those commits, and deleting it
        // left them reachable from nothing at all. `git fsck` can still find
        // the objects until they are collected; the product cannot find them
        // at any point, which is the part that matters. Both destructive
        // paths reach here, so one drag onto Done or one context-menu delete
        // was enough.
        //
        // Nothing about a removed checkout implies the branch is expendable.
        // The rest of SlashIt agrees and always has: every caller keeps
        // `branch_name` on the task explicitly "for PR creation", `create_pr`
        // pushes exactly that branch, and a re-queued task reattaches to it
        // with `git worktree add <path> <branch>`. Deleting it broke both,
        // and left `branch_name` pointing at a ref that no longer existed.
        //
        // What remains is an uncollected local branch per finished task.
        // That is the intended trade: a leaked ref is visible, inspectable
        // and deletable by hand, and destroyed work is none of those.

        Ok(())
    }

    /// Find worktree path by branch name using git worktree list.
    async fn find_worktree_path(&self, repo_path: &str, branch: &str) -> Result<WorktreeInfo, String> {
        let output = tokio::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| format!("Failed to list worktrees: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(path) = Self::worktree_for_branch(&stdout, branch) {
            return Ok(WorktreeInfo {
                path,
                branch: branch.to_string(),
            });
        }

        // Not registered with git: fall back to the paths we would have used,
        // newest scheme first. (`worktree_for_branch` already checked this
        // same listing above and found no match, so `adoptable_path`'s own
        // git-registration check can only agree — kept for symmetry with
        // `create`/`reattach` and in case that matching logic ever diverges.)
        if let Some(path) = self.adoptable_path(repo_path, branch, &stdout) {
            return Ok(WorktreeInfo {
                path: path.to_string_lossy().to_string(),
                branch: branch.to_string(),
            });
        }

        Err(format!("Worktree for branch '{}' not found", branch))
    }

    /// Extract the worktree path for `branch` from `git worktree list --porcelain`.
    ///
    /// Matching is on the exact `branch refs/heads/<name>` record, not a
    /// substring of the whole block. A substring test matches the `worktree
    /// <path>` line too, so a branch whose name appears anywhere in the main
    /// checkout's path — branch `app` against `/home/u/app` — used to resolve to
    /// the user's primary working copy, and an agent would then have run there.
    fn worktree_for_branch(porcelain: &str, branch: &str) -> Option<String> {
        let wanted = format!("refs/heads/{branch}");
        let mut current_path: Option<&str> = None;

        for line in porcelain.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                current_path = Some(path);
            } else if let Some(reference) = line.strip_prefix("branch ") {
                if reference == wanted {
                    return current_path.map(str::to_string);
                }
            } else if line.is_empty() {
                current_path = None;
            }
        }

        None
    }
}

/// The record git keeps for a linked worktree, saved so that a removal git
/// abandons partway can still be retried.
///
/// `git worktree remove` deletes the checkout's contents first and tears down
/// `<git dir>/worktrees/<id>` afterwards, and it does the second part whether
/// or not the first one finished. What that leaves behind, when it did not, is
/// a directory that is plainly still there and that git no longer knows
/// anything about -- and git has no way back to it. `remove` answers "is not a
/// working tree" however the path is spelled, `repair` declines because the
/// `.git` file now points at nothing, and `add` refuses a path that exists.
/// The record is the only thing that makes a checkout removable, so losing it
/// turns an unfinished removal into an impossible one: no later attempt, however
/// many times a person asks for it, could ever get anywhere.
///
/// What keeps this from reaching a worktree it has no business in is the
/// record's own absence, checked at the moment of putting it back rather than
/// assumed from the path. Something can take the path in the meantime -- a
/// removal that empties a directory but cannot delete the directory itself
/// leaves an empty one behind, and `git worktree add` accepts an empty
/// directory -- but a worktree created there gets its record under this same
/// name, so the name is occupied and nothing is put back. Only a name git
/// holds nothing under is filled in, and only with the bytes git itself had
/// under it moments earlier in this same call.
///
/// Nothing here decides anything about deleting the directory. That was git's
/// to do and still is; the next attempt is an ordinary `git worktree remove`
/// that succeeds once whatever blocked the deletion is gone.
struct WorktreeRecord {
    /// `<git common dir>/worktrees/<id>`, where git keeps the record.
    admin: PathBuf,
    /// A copy of it, beside it rather than in system temp so that putting it
    /// back is a rename on one filesystem, and outside `worktrees/` so that
    /// git does not read the copy as a worktree of its own.
    saved: PathBuf,
    /// `<worktree>/.git`, the file pointing back at `admin`, with what it
    /// held. It lives in the checkout, so it is one of the things git deletes
    /// on its way through, and a record whose worktree has no `.git` file
    /// fails validation just as an absent record does.
    gitlink: PathBuf,
    gitlink_contents: Vec<u8>,
    /// Whether the copy has to outlive this call. It does exactly when it
    /// could not be put back, because a call that finds no record has nothing
    /// to copy and this is then the only way back there is.
    keep: Cell<bool>,
}

impl WorktreeRecord {
    /// Save the record for `worktree_path`, or nothing if there is not one to
    /// save.
    ///
    /// Returning `None` is not a failure. A path git does not register is one
    /// this cannot restore anything for, and a removal then behaves exactly
    /// as it did before.
    ///
    /// The worktree-side pointer at `<worktree_path>/.git` is tried first: it
    /// holds the exact bytes git itself wrote, and preserving those verbatim
    /// is worth doing whenever they can be read. [`Self::locate_admin`] is
    /// not a lesser fallback for when they can't -- it is what still answers
    /// in exactly the case that matters here, a worktree whose own parent has
    /// closed off, because it never reads anything under `worktree_path` at
    /// all.
    async fn save(worktree_path: &str, repo_path: &str) -> Option<Self> {
        let common = Self::common_dir(repo_path).await?;
        let worktrees_dir = common.join("worktrees");
        let gitlink = Path::new(worktree_path).join(".git");

        let (admin, gitlink_contents) = match std::fs::read(&gitlink) {
            Ok(contents) => {
                let admin = PathBuf::from(
                    std::str::from_utf8(&contents).ok()?.strip_prefix("gitdir:")?.trim(),
                );
                (admin, contents)
            }
            Err(_) => {
                let admin = Self::locate_admin(&worktrees_dir, worktree_path)?;
                // Git's own record format for the reverse pointer -- proven
                // against a real `git worktree add` in
                // `synthesized_gitlink_matches_what_git_itself_writes` below.
                let gitlink_contents = format!("gitdir: {}\n", admin.display()).into_bytes();
                (admin, gitlink_contents)
            }
        };

        // Wherever the pointer came from, where it leads is checked against
        // what git says about this repository rather than taken on trust:
        // only this repository's own worktree records are ever copied.
        if admin.parent() != Some(worktrees_dir.as_path()) {
            return None;
        }

        // Named after the record rather than at random, so that a copy an
        // earlier attempt could not put back is the one this attempt finds.
        let saved = common.join(format!(
            "slashit-saved-worktree-record-{}",
            admin.file_name()?.to_str()?
        ));

        if admin.is_dir() {
            // Any copy still lying about is from an attempt that has since
            // been overtaken by git's own record.
            let _ = std::fs::remove_dir_all(&saved);
            copy_tree(&admin, &saved).ok()?;
        } else if !saved.is_dir() {
            // No record, and no copy of one: there is nothing to put back.
            return None;
        }

        Some(Self {
            admin,
            saved,
            gitlink,
            gitlink_contents,
            keep: Cell::new(false),
        })
    }

    /// Find `worktree_path`'s administrative record directly under
    /// `worktrees_dir`, without reading anything under `worktree_path`
    /// itself.
    ///
    /// Git's own reverse pointer -- `<worktrees_dir>/<id>/gitdir` -- holds
    /// exactly `<worktree_path>/.git\n` and nothing else (confirmed against a
    /// real `git worktree add` in
    /// `synthesized_gitlink_matches_what_git_itself_writes` below), so the
    /// record whose `gitdir` names this exact path is this worktree's, and
    /// the only place that pointer has to be read from is the repository
    /// side -- which a blocked worktree parent never affects.
    ///
    /// Fails closed on anything it cannot be sure of: a record whose
    /// `gitdir` cannot be read is skipped rather than guessed at, and this
    /// reports nothing at all, rather than pick one, if more than one record
    /// claims the same path -- restoring the wrong one would be worse than
    /// restoring none.
    fn locate_admin(worktrees_dir: &Path, worktree_path: &str) -> Option<PathBuf> {
        let wanted = Path::new(worktree_path).join(".git");
        let mut found: Option<PathBuf> = None;

        for entry in std::fs::read_dir(worktrees_dir).ok()?.flatten() {
            let admin = entry.path();
            if !admin.is_dir() {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(admin.join("gitdir")) else {
                continue;
            };
            if Path::new(contents.trim()) != wanted {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(admin);
        }

        found
    }

    /// Put the record back, if git dropped one it did not finish acting on.
    fn restore(&self) -> Result<(), String> {
        let outcome = self.put_back();
        self.keep.set(outcome.is_err());
        outcome
    }

    fn put_back(&self) -> Result<(), String> {
        match Presence::of(&self.admin) {
            Presence::Present => {}
            Presence::Absent => {
                // Git deletes `worktrees/` itself once it holds nothing.
                if let Some(parent) = self.admin.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("could not recreate {}: {e}", parent.display()))?;
                }
                std::fs::rename(&self.saved, &self.admin)
                    .map_err(|e| format!("could not put {} back: {e}", self.admin.display()))?;
            }
            // Not knowing is not license to overwrite: the record could
            // still be exactly where it belongs, and blindly renaming over
            // it would be destructive on a guess. Reported as failure so the
            // caller keeps the saved copy for a later attempt instead.
            Presence::Unverified(error) => {
                return Err(format!(
                    "could not tell whether {} still exists: {error}",
                    self.admin.display()
                ));
            }
        }

        // Asked separately, because the record and the file pointing at it are
        // two things git deletes separately, and a worktree with no `.git`
        // file fails validation exactly as a missing record does. A worktree
        // that is really there always has one, so this cannot reach a live
        // checkout.
        match Presence::of(&self.gitlink) {
            Presence::Present => {}
            Presence::Absent => {
                std::fs::write(&self.gitlink, &self.gitlink_contents)
                    .map_err(|e| format!("could not put {} back: {e}", self.gitlink.display()))?;
            }
            Presence::Unverified(error) => {
                return Err(format!(
                    "could not tell whether {} still exists: {error}",
                    self.gitlink.display()
                ));
            }
        }
        Ok(())
    }

    async fn common_dir(repo_path: &str) -> Option<PathBuf> {
        let output = tokio::process::Command::new("git")
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .current_dir(repo_path)
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
        (!path.is_empty()).then(|| PathBuf::from(path))
    }
}

impl Drop for WorktreeRecord {
    fn drop(&mut self) {
        if self.keep.get() {
            // Deleting it here would take away the only way back: the record
            // is gone, so the next attempt has nothing of its own to copy.
            return;
        }
        // Restored, and so renamed away already; or never needed.
        let _ = std::fs::remove_dir_all(&self.saved);
    }
}

/// Copy `from` to `to`, contents and all.
///
/// Only ever used on a worktree record, which holds git's own bookkeeping and
/// no user content.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // -------------------------------------------------------
    // Unit tests (no external tools required)
    // -------------------------------------------------------

    /// Roots under a unique temp path. Nothing is created on disk — these
    /// tests only exercise path computation.
    fn test_paths() -> Arc<AppPaths> {
        let root = std::env::temp_dir().join(format!("slashit-wt-test-{}", Uuid::new_v4()));
        Arc::new(AppPaths::with_roots(
            root.join("config"),
            root.join("data"),
            root.join("cache"),
            root.join("runtime"),
        ))
    }

    /// A manager with external tools forced off, so tests never depend on
    /// whether `wt` happens to be installed on the machine running them.
    fn test_manager() -> WorktreeManager {
        WorktreeManager {
            wt_available: false,
            paths: test_paths(),
            placement: WorktreePlacement::Auto,
        }
    }

    #[test]
    fn new_does_not_panic() {
        // Even if wt is absent, construction must succeed.
        let mgr = WorktreeManager::new(test_paths(), WorktreePlacement::Auto);
        let _ = mgr.wt_available;
    }

    #[tokio::test]
    async fn remove_converges_on_an_absent_directory_under_worktrunk_delegation() {
        // A second removal of an already-absent worktree has to succeed, or
        // the state is one nothing can ever finish: `remove_with_git` gets that
        // from its `exists()` gate; `remove_with_wt` spawns with
        // `current_dir(worktree_path)`, so without its own guard it fails at
        // spawn every time it is asked, however many times the user asks. This
        // runs on machines with and without `wt` installed, because the guard
        // returns before spawning.
        let mut mgr = test_manager();
        mgr.wt_available = true;
        mgr.placement = WorktreePlacement::Auto;
        assert!(mgr.delegates_to_wt(), "this test must exercise the wt backend");

        // A private temp root, so parallel test threads cannot race on the name.
        let temp = tempfile::TempDir::new().expect("tempdir");
        let repo = create_temp_git_repo();
        let absent = temp.path().join("worktree-that-does-not-exist");

        let result = mgr
            .remove(absent.to_str().unwrap(), repo.path().to_str().unwrap())
            .await;

        assert!(
            result.is_ok(),
            "an already-absent worktree is a converged removal, not a failure: {result:?}"
        );
    }

    #[tokio::test]
    async fn integration_remove_under_worktrunk_delegation_clears_a_stale_registration() {
        // Converging is not enough on its own: `wt` leaves the registration
        // behind when the directory goes missing, and a stale registration
        // blocks `wt switch <branch>` ("Worktree directory missing") while the
        // surviving branch blocks `wt switch -c <branch>`, so the task could
        // never get a worktree again. `git worktree remove` clears a record
        // whose directory is gone, which is the whole reason this case is
        // handed to the git backend. Needs no `wt` binary: the delegation
        // happens before anything is spawned.
        let repo = create_temp_git_repo();
        let repo_path = repo.path().to_str().unwrap().to_string();

        let mut mgr = test_manager();
        mgr.placement = WorktreePlacement::Managed;
        let info = mgr
            .create(&repo_path, "task-abcd1234")
            .await
            .expect("create failed");

        // Exactly the state `wt` leaves behind: directory gone, git record kept.
        std::fs::remove_dir_all(&info.path).expect("remove worktree dir");
        let before = WorktreeManager::worktree_list_porcelain(&repo_path).expect("git listing");
        assert!(
            before.contains(&info.path),
            "the stale registration must still be there before the removal"
        );

        mgr.wt_available = true;
        mgr.placement = WorktreePlacement::Auto;
        assert!(mgr.delegates_to_wt(), "this test must exercise the wt backend");

        mgr.remove(&info.path, &repo_path)
            .await
            .expect("an already-absent worktree is a converged removal");

        let after = WorktreeManager::worktree_list_porcelain(&repo_path).expect("git listing");
        assert!(
            !after.contains(&info.path),
            "the stale registration must be cleared, or the branch can never be checked out \
             again"
        );
        assert!(
            branch_exists(&repo_path, "task-abcd1234"),
            "the two backends must not differ on data loss: `wt remove` never deletes a branch, \
             and the git path it delegates to must not either"
        );
    }

    #[test]
    fn delegates_to_wt_respects_managed_placement_regardless_of_wt_availability() {
        let mut mgr = test_manager();
        mgr.wt_available = true;

        mgr.placement = WorktreePlacement::Auto;
        assert!(mgr.delegates_to_wt(), "Auto with wt installed should delegate");

        mgr.placement = WorktreePlacement::Managed;
        assert!(
            !mgr.delegates_to_wt(),
            "Managed must take placement back even when wt is installed"
        );
    }

    #[test]
    fn managed_worktrees_live_outside_the_repository() {
        let mgr = test_manager();
        let repo = "/home/someone/code/my-app";
        let path = mgr.managed_path(repo, "task-1a2b3c4d");

        assert!(
            !path.starts_with(repo),
            "worktree {} must not be inside the repository",
            path.display()
        );
        assert!(path.starts_with(mgr.paths.data_dir()));
        assert!(path.ends_with("task-1a2b3c4d"));
    }

    #[test]
    fn managed_paths_do_not_collide_between_same_named_repos() {
        let mgr = test_manager();
        let a = mgr.managed_path("/home/someone/a/my-app", "feature");
        let b = mgr.managed_path("/home/someone/b/my-app", "feature");
        assert_ne!(a, b, "same repo name in different places must not collide");
    }

    #[test]
    fn worktree_for_branch_matches_the_exact_branch_record() {
        // The main checkout's path contains the branch name "app". A substring
        // match would return the user's primary working copy here.
        let porcelain = "\
worktree /home/someone/app
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /home/someone/.local/share/slashit-app/worktrees/app-abcd1234/app
HEAD 2222222222222222222222222222222222222222
branch refs/heads/app
";
        let found = WorktreeManager::worktree_for_branch(porcelain, "app");
        assert_eq!(
            found.as_deref(),
            Some("/home/someone/.local/share/slashit-app/worktrees/app-abcd1234/app")
        );
    }

    #[test]
    fn worktree_for_branch_ignores_a_prefix_match() {
        let porcelain = "\
worktree /home/someone/app
HEAD 1111111111111111111111111111111111111111
branch refs/heads/feature-two
";
        assert_eq!(WorktreeManager::worktree_for_branch(porcelain, "feature"), None);
    }

    #[test]
    fn worktree_for_branch_handles_a_detached_head_block() {
        let porcelain = "\
worktree /home/someone/app
HEAD 1111111111111111111111111111111111111111
detached

worktree /home/someone/wt/fix
HEAD 2222222222222222222222222222222222222222
branch refs/heads/fix
";
        assert_eq!(
            WorktreeManager::worktree_for_branch(porcelain, "fix").as_deref(),
            Some("/home/someone/wt/fix")
        );
    }

    #[test]
    fn legacy_sibling_path_is_still_computable_for_adoption() {
        let legacy = WorktreeManager::legacy_path("/home/someone/code/my-app", "task-1a2b3c4d")
            .expect("legacy path");
        assert_eq!(
            legacy,
            PathBuf::from("/home/someone/code/my-app.task-1a2b3c4d")
        );
    }

    // -------------------------------------------------------
    // adoptable_path: a directory existing on disk is not enough — it must
    // also be confirmed by git's own worktree list for this exact branch.
    // -------------------------------------------------------

    #[test]
    fn adoptable_path_rejects_a_directory_that_exists_but_is_not_a_registered_worktree() {
        let mgr = test_manager();
        let repo = "/home/someone/code/my-app";
        let branch = "task-stale0001";
        let managed = mgr.managed_path(repo, branch);
        std::fs::create_dir_all(&managed).expect("failed to create candidate dir");

        // Git knows nothing about this branch at all -- e.g. the worktree
        // was pruned, or the directory is unrelated debris that happens to
        // sit at the deterministic path.
        let porcelain = "\
worktree /home/someone/code/my-app
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main
";

        assert!(
            mgr.adoptable_path(repo, branch, porcelain).is_none(),
            "a directory that exists on disk but isn't registered with git must not be adopted"
        );

        let _ = std::fs::remove_dir_all(managed.parent().unwrap());
    }

    #[test]
    fn adoptable_path_accepts_a_directory_git_confirms_is_registered_for_the_branch() {
        let mgr = test_manager();
        let repo = "/home/someone/code/my-app";
        let branch = "task-abcd1234";
        let managed = mgr.managed_path(repo, branch);
        std::fs::create_dir_all(&managed).expect("failed to create candidate dir");

        let porcelain = format!(
            "worktree {}\nHEAD 2222222222222222222222222222222222222222\nbranch refs/heads/{}\n",
            managed.display(),
            branch
        );

        let result = mgr.adoptable_path(repo, branch, &porcelain);
        assert_eq!(result.as_deref(), Some(managed.as_path()));

        let _ = std::fs::remove_dir_all(managed.parent().unwrap());
    }

    #[test]
    fn adoptable_path_rejects_a_registration_for_a_different_branch() {
        let mgr = test_manager();
        let repo = "/home/someone/code/my-app";
        let branch = "task-abcd1234";
        let managed = mgr.managed_path(repo, branch);
        std::fs::create_dir_all(&managed).expect("failed to create candidate dir");

        // Git has a worktree registered at our exact candidate path, but for
        // a different branch -- adopting it would hand this task someone
        // else's branch.
        let porcelain = format!(
            "worktree {}\nHEAD 3333333333333333333333333333333333333333\nbranch refs/heads/some-other-branch\n",
            managed.display()
        );

        assert!(mgr.adoptable_path(repo, branch, &porcelain).is_none());

        let _ = std::fs::remove_dir_all(managed.parent().unwrap());
    }

    #[test]
    fn adoptable_path_rejects_branch_registered_at_an_unrelated_path() {
        let mgr = test_manager();
        let repo = "/home/someone/code/my-app";
        let branch = "task-abcd1234";
        let managed = mgr.managed_path(repo, branch);
        std::fs::create_dir_all(&managed).expect("failed to create candidate dir");

        // Git confirms `branch` is registered, but at some other worktree
        // entirely (e.g. the user's primary checkout) -- not at either
        // deterministic path SlashIt would place it at, so it must not be
        // adopted even though our own candidate directory also exists.
        let porcelain = format!(
            "worktree /home/someone/code/my-app\nHEAD 4444444444444444444444444444444444444444\nbranch refs/heads/{}\n",
            branch
        );

        assert!(mgr.adoptable_path(repo, branch, &porcelain).is_none());

        let _ = std::fs::remove_dir_all(managed.parent().unwrap());
    }

    #[test]
    fn adopt_existing_returns_the_string_path_when_git_confirms_registration() {
        let mgr = test_manager();
        let repo = "/home/someone/code/my-app";
        let branch = "task-abcd1234";
        let managed = mgr.managed_path(repo, branch);
        std::fs::create_dir_all(&managed).expect("failed to create candidate dir");

        let porcelain = format!(
            "worktree {}\nHEAD 5555555555555555555555555555555555555555\nbranch refs/heads/{}\n",
            managed.display(),
            branch
        );

        assert_eq!(
            mgr.adopt_existing(repo, branch, &porcelain),
            Some(managed.to_string_lossy().to_string())
        );

        let _ = std::fs::remove_dir_all(managed.parent().unwrap());
    }

    #[test]
    fn adopt_any_registered_accepts_a_path_adopt_existing_would_reject() {
        // The exact scenario `adoptable_path_rejects_branch_registered_at_an_unrelated_path`
        // above proves `adopt_existing` refuses: git confirms `branch` is
        // registered, but not at either of SlashIt's own conventions (for
        // example, a worktree `wt` itself placed under its own naming
        // scheme). `adopt_any_registered` is the startup-reconciliation
        // fallback that treats git's confirmation alone as sufficient,
        // rather than stranding a live worktree just because of where it
        // happens to sit.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let branch = "task-abcd1234";
        let registered_at = tmp.path().join("wherever-wt-put-it");
        std::fs::create_dir_all(&registered_at).expect("failed to create candidate dir");

        let porcelain = format!(
            "worktree {}\nHEAD 6666666666666666666666666666666666666666\nbranch refs/heads/{}\n",
            registered_at.display(),
            branch
        );

        assert_eq!(
            WorktreeManager::adopt_any_registered(
                tmp.path().to_str().unwrap(),
                branch,
                &porcelain
            ),
            Some(registered_at.to_string_lossy().to_string())
        );
    }

    /// The exact scenario a review traced: the user has the task branch
    /// checked out in the repository's own primary checkout, and
    /// `worktree_for_branch` matches that record as readily as any other.
    /// Adopting it would repoint the task at the user's own working copy,
    /// and the executor would then run an agent there.
    #[test]
    fn adopt_any_registered_refuses_the_primary_checkout() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let repo = tmp.path();
        let branch = "task-abcd1234";

        let porcelain = format!(
            "worktree {}\nHEAD 9999999999999999999999999999999999999999\nbranch refs/heads/{}\n",
            repo.display(),
            branch
        );

        assert!(
            WorktreeManager::adopt_any_registered(repo.to_str().unwrap(), branch, &porcelain)
                .is_none(),
            "the primary checkout must never be adopted as a task's worktree"
        );
    }

    /// Adversarial: git can list the primary checkout's registration through
    /// a symlinked or `..`-relative path spelling that differs byte-for-byte
    /// from `repo_path`. A literal string comparison would miss this; the
    /// canonicalizing comparison must not.
    #[cfg(unix)]
    #[test]
    fn adopt_any_registered_refuses_a_symlinked_alias_of_the_primary_checkout() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let repo = tmp.path().join("primary-checkout");
        std::fs::create_dir_all(&repo).expect("create primary checkout dir");
        let alias = tmp.path().join("alias-to-primary");
        std::os::unix::fs::symlink(&repo, &alias).expect("create symlink alias");
        let branch = "task-abcd1234";

        // git lists the worktree at the symlink path, not the canonical one --
        // a realistic case, since git records whatever path the checkout was
        // opened through.
        let porcelain = format!(
            "worktree {}\nHEAD 4444444444444444444444444444444444444444\nbranch refs/heads/{}\n",
            alias.display(),
            branch
        );

        assert!(
            WorktreeManager::adopt_any_registered(repo.to_str().unwrap(), branch, &porcelain)
                .is_none(),
            "a symlinked alias of the primary checkout must resolve to the same canonical path \
             and be refused just like the literal path"
        );
    }

    #[test]
    fn adopt_any_registered_still_requires_git_confirmation_for_the_exact_branch() {
        let branch = "task-abcd1234";

        // Nothing registered for this branch at all.
        assert!(
            WorktreeManager::adopt_any_registered("/home/someone/code/other-repo", branch, "")
                .is_none()
        );

        // Registered, but for a different branch.
        let porcelain = "\
worktree /home/someone/code/my-app
HEAD 7777777777777777777777777777777777777777
branch refs/heads/some-other-branch
";
        assert!(WorktreeManager::adopt_any_registered(
            "/home/someone/code/other-repo",
            branch,
            porcelain
        )
        .is_none());
    }

    #[test]
    fn adopt_any_registered_rejects_a_registration_whose_directory_is_gone() {
        // Git can still list a registration for a worktree whose directory
        // was already removed by hand; `is_dir()` is what keeps that from
        // being adopted as if it were live.
        let branch = "task-abcd1234";
        let porcelain = format!(
            "worktree /nonexistent/definitely-not-here-{}\nHEAD 8888888888888888888888888888888888888888\nbranch refs/heads/{}\n",
            Uuid::new_v4(),
            branch
        );
        assert!(WorktreeManager::adopt_any_registered(
            "/home/someone/code/other-repo",
            branch,
            &porcelain
        )
        .is_none());
    }

    #[test]
    fn branch_for_task_format() {
        let id = Uuid::parse_str("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap();
        let branch = WorktreeManager::branch_for_task(id);
        assert_eq!(branch, "task-a1b2c3d4");
    }

    #[test]
    fn branch_for_task_uses_first_8_chars() {
        let id = Uuid::new_v4();
        let branch = WorktreeManager::branch_for_task(id);
        assert!(branch.starts_with("task-"));
        // 5 chars for "task-" + 8 hex chars = 13
        assert_eq!(branch.len(), 13);
    }

    #[test]
    fn branch_for_task_deterministic() {
        let id = Uuid::new_v4();
        let a = WorktreeManager::branch_for_task(id);
        let b = WorktreeManager::branch_for_task(id);
        assert_eq!(a, b);
    }

    #[test]
    fn exists_returns_false_for_nonexistent_path() {
        let mgr = test_manager();
        assert!(!mgr.exists("/tmp/slashit_nonexistent_worktree_12345"));
    }

    #[test]
    fn exists_returns_true_for_existing_dir() {
        let mgr = test_manager();
        // /tmp always exists on Linux
        assert!(mgr.exists("/tmp"));
    }

    #[test]
    fn worktree_path_construction_sibling_format() {
        // Verify the expected path format: parent/repo_name.branch
        let repo_path = PathBuf::from("/home/user/projects/my-repo");
        let branch = "feature-xyz";
        let repo_name = repo_path.file_name().unwrap().to_str().unwrap();
        let parent = repo_path.parent().unwrap();
        let expected = parent.join(format!("{}.{}", repo_name, branch));
        assert_eq!(
            expected,
            PathBuf::from("/home/user/projects/my-repo.feature-xyz")
        );
    }

    #[test]
    fn worktree_path_construction_root_repo_fallback() {
        // When repo is at root ("/repo"), parent is "/", path should be "/repo.branch"
        let repo_path = PathBuf::from("/repo");
        let branch = "fix-bug";
        let repo_name = repo_path.file_name().unwrap().to_str().unwrap();
        let parent = repo_path.parent().unwrap();
        let expected = parent.join(format!("{}.{}", repo_name, branch));
        assert_eq!(expected, PathBuf::from("/repo.fix-bug"));
    }

    #[test]
    fn worktree_path_with_slashes_in_branch() {
        // Branch names can contain slashes (e.g. "feature/foo")
        let repo_path = PathBuf::from("/home/user/my-repo");
        let branch = "feature/foo";
        let repo_name = repo_path.file_name().unwrap().to_str().unwrap();
        let parent = repo_path.parent().unwrap();
        let expected = parent.join(format!("{}.{}", repo_name, branch));
        // PathBuf normalizes this: the "feature" part becomes a directory component
        // This is the actual behavior of the code -- it will create a nested path.
        assert_eq!(
            expected.to_string_lossy(),
            "/home/user/my-repo.feature/foo"
        );
    }

    // -------------------------------------------------------
    // Integration tests (require git)
    // -------------------------------------------------------

    /// Helper: create a temporary git repository and return its path.
    fn create_temp_git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let repo = dir.path();

        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .expect("git init failed");

        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(repo)
            .output()
            .expect("git config email failed");

        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(repo)
            .output()
            .expect("git config name failed");

        // Create an initial commit so HEAD exists
        std::fs::write(repo.join("README.md"), "# test").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .expect("git add failed");
        std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .output()
            .expect("git commit failed");

        // Ensure we are on a branch called "main"
        std::process::Command::new("git")
            .args(["branch", "-M", "main"])
            .current_dir(repo)
            .output()
            .expect("git branch -M main failed");

        dir
    }

    #[tokio::test]
    async fn integration_create_and_exists() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();

        let info = mgr.create(repo_path, "test-branch").await.expect("create failed");
        assert!(Path::new(&info.path).exists(), "worktree dir should exist");
        assert_eq!(info.branch, "test-branch");
        assert!(mgr.exists(&info.path));
    }

    #[tokio::test]
    async fn integration_remove_leaves_the_work_in_the_worktree_reachable() {
        // The invariant the whole contract exists for, stated the way a user
        // would lose it: an agent produced a commit, that commit is on the
        // task branch and on nothing else, and removing the checkout must not
        // take it away. Asserted on refs rather than on `git show`, because
        // the objects survive a `branch -D` for as long as it takes gc to run
        // -- a commit no ref can name is already lost to the product.
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        let mgr = test_manager();

        let info = mgr
            .create(repo_path, "task-removeme")
            .await
            .expect("create failed");

        let work = commit_work(&info.path, "agent-work.txt");
        assert_eq!(
            refs_reaching(repo_path, &work),
            vec!["refs/heads/task-removeme".to_string()],
            "precondition: the branch must be the only thing naming this commit, or the test \
             could pass on some other ref holding it"
        );

        mgr.remove(&info.path, repo_path)
            .await
            .expect("removal of a live worktree should succeed");

        assert!(!Path::new(&info.path).exists(), "worktree dir should be gone");
        assert_eq!(
            refs_reaching(repo_path, &work),
            vec!["refs/heads/task-removeme".to_string()],
            "removing the checkout must leave the commit reachable; an empty list here is the \
             data loss this contract forbids"
        );
    }

    #[tokio::test]
    async fn integration_remove_keeps_the_branch_it_removed_the_worktree_for() {
        // The branch survives a removal it *did* own, not just one it did
        // not. This is the case that used to force-delete: `git worktree
        // remove` exits 0, and that used to be read as permission to run
        // `git branch -D`.
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        let mgr = test_manager();

        let info = mgr
            .create(repo_path, "task-removeme")
            .await
            .expect("create failed");
        assert!(branch_exists(repo_path, "task-removeme"), "precondition");

        mgr.remove(&info.path, repo_path)
            .await
            .expect("removal of a live worktree should succeed");

        assert!(!Path::new(&info.path).exists(), "worktree dir should be gone");
        assert!(
            branch_exists(repo_path, "task-removeme"),
            "removal takes the checkout only: the branch is what a later PR push and a later \
             reattach both need"
        );
    }

    #[tokio::test]
    async fn integration_a_removed_worktree_can_be_reattached_from_its_branch() {
        // What the retained branch is worth: `reattach` is the path a
        // re-queued task takes, and with the branch gone its
        // `git worktree add <path> <branch>` fails and the executor falls
        // back to running the agent in the user's own repository.
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        let mgr = test_manager();

        let info = mgr
            .create(repo_path, "task-reattach")
            .await
            .expect("create failed");
        let work = commit_work(&info.path, "agent-work.txt");

        mgr.remove(&info.path, repo_path).await.expect("remove failed");

        let again = mgr
            .reattach(repo_path, "task-reattach")
            .await
            .expect("a removed worktree must be recreatable from the branch it left behind");

        assert_eq!(
            std::fs::read_to_string(Path::new(&again.path).join("agent-work.txt")).ok(),
            Some(WORK.to_string()),
            "the reattached worktree must hold the work the first one produced"
        );
        assert!(
            refs_reaching(repo_path, &work).contains(&"refs/heads/task-reattach".to_string()),
            "and the commit must still be named by the branch it was reattached from"
        );
    }

    #[tokio::test]
    async fn integration_remove_keeps_the_branch_when_it_removed_nothing() {
        // Convergence without collateral damage: a retry sees the path
        // already absent, both `git worktree remove` invocations exit
        // non-zero, and nothing was removed by this call. Reporting `Ok` is
        // right — the worktree is gone — and the branch, which this call has
        // no claim on whatsoever, must be exactly as it was. Kept as its own
        // case because it is the one an explicit retry actually re-enters:
        // asking again for a cleanup that already removed the directory must
        // converge rather than fail forever.
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        let mgr = test_manager();

        std::process::Command::new("git")
            .args(["branch", "task-keepme"])
            .current_dir(repo_path)
            .output()
            .expect("failed to create branch");
        assert!(branch_exists(repo_path, "task-keepme"), "precondition");

        let absent = tmp.path().join("never-existed");
        mgr.remove(absent.to_str().unwrap(), repo_path)
            .await
            .expect("an already-absent worktree is not a failure");

        assert!(
            branch_exists(repo_path, "task-keepme"),
            "no git removal succeeded, so the branch must not be force-deleted"
        );
    }

    /// Automatic cleanup must not be able to destroy work that exists only in
    /// the checkout it is deleting.
    ///
    /// A worktree is disposable exactly to the extent that everything in it
    /// is recoverable from somewhere else, and not-yet-committed content is
    /// precisely what is not: no commit, and therefore no branch, names it.
    /// `remove` is reached automatically -- one drag onto Done is enough --
    /// so there is no moment at which anybody is asked whether this particular
    /// checkout is expendable.
    ///
    /// Three classes of content have that property and are run as one table
    /// rather than three near-identical tests, because the contract is one
    /// contract: a tracked file edited in place, a change staged but not
    /// committed, and an ordinary untracked file. Each is what an interrupted
    /// agent run routinely leaves behind. For each, the removal must refuse
    /// and say so: report `Err`, leave the checkout on disk holding its exact
    /// bytes, leave the registration git needs in order to address that
    /// checkout again, and leave the task branch.
    ///
    /// The behaviour this pins down is that `remove_with_git` makes exactly
    /// one removal attempt, the ordinary `git worktree remove`, and reports
    /// git's refusal as an `Err`. It used to read that refusal as a signal to
    /// re-run with `--force`, which deleted the content and then returned
    /// `Ok(())`; the caller durably clears `worktree_path` on that `Ok`, so
    /// the work was gone and nothing recorded that it had ever existed.
    ///
    /// The error text is asserted on too, and against git's own words rather
    /// than a phrase invented here. "Cleanup failed" tells a user nothing
    /// they can act on, whereas git names the file that is holding the
    /// removal back, and that is the whole difference between a dead end and
    /// a next step.
    #[tokio::test]
    async fn a_dirty_worktree_survives_automatic_cleanup() {
        /// One class of content that lives nowhere but in the checkout.
        struct DirtyCase {
            /// Named in every assertion, so a failure says which class broke.
            class: &'static str,
            /// The file the class is expressed in, relative to the checkout.
            file: &'static str,
            /// Its exact bytes, which are what must still be there afterwards.
            contents: &'static str,
            /// Whether the change is `git add`ed before cleanup runs.
            staged: bool,
        }

        const CASES: &[DirtyCase] = &[
            DirtyCase {
                class: "tracked file modified but not committed",
                // Tracked because `create_temp_git_repo` committed it.
                file: "README.md",
                contents: "# test\nedited in the worktree, never committed\n",
                staged: false,
            },
            DirtyCase {
                class: "change staged but not committed",
                file: "staged-in-the-worktree.txt",
                contents: "staged in the worktree, never committed\n",
                staged: true,
            },
            DirtyCase {
                class: "non-ignored untracked file created",
                file: "untracked-in-the-worktree.txt",
                contents: "written in the worktree, never added\n",
                staged: false,
            },
        ];

        // Every violation of every case is collected rather than asserted on
        // the spot, so one run names all three classes and everything each of
        // them lost. A fail-fast table would report only the first class and
        // hide whether the other two behave the same way.
        let mut violations: Vec<String> = Vec::new();

        for case in CASES {
            let tmp = create_temp_git_repo();
            let repo_path = tmp.path().to_str().unwrap();

            // `Managed` is a placement users actually select, not a harness
            // fiction, and it forces the git backend even where `wt` is
            // installed. `wt_available` is therefore set to the value that
            // would otherwise delegate, which makes the assertion below a
            // real statement about the gate rather than about the host: this
            // test exercises `remove_with_git` on every machine, and says so.
            let mut mgr = test_manager();
            mgr.wt_available = true;
            mgr.placement = WorktreePlacement::Managed;
            assert!(
                !mgr.delegates_to_wt(),
                "[{}] this test must exercise the git backend whatever is installed on the host",
                case.class
            );

            let info = mgr
                .create(repo_path, "task-dirty")
                .await
                .expect("create failed");
            let file = Path::new(&info.path).join(case.file);
            std::fs::write(&file, case.contents).expect("write uncommitted content");
            if case.staged {
                run_git(&info.path, &["add", case.file]);
            }
            assert!(
                !run_git(&info.path, &["status", "--porcelain"]).is_empty(),
                "precondition [{}]: git itself must agree the checkout holds content no commit \
                 names, or the case under test was never set up",
                case.class
            );

            // Git's refusal is read from git rather than written down here,
            // so the assertion below stays true across git versions and
            // across the wording of the advice they give. Running the plain
            // removal to get it is safe: it refuses at validation and touches
            // neither the checkout nor its registration, which is the very
            // property the rest of this test is about.
            let refusal = git_removal_refusal(repo_path, &info.path);
            assert!(
                !refusal.is_empty(),
                "precondition [{}]: git itself must refuse to remove this checkout, or there is \
                 no refusal for cleanup to relay",
                case.class
            );

            let result = mgr.remove(&info.path, repo_path).await;

            let class = case.class;
            match &result {
                Ok(()) => violations.push(format!(
                    "[{class}] cleanup reported success for a checkout holding content no commit \
                     names; the caller clears worktree_path on that answer, so the last handle \
                     to that content goes too"
                )),
                Err(error) if !error.contains(&refusal) => violations.push(format!(
                    "[{class}] the failure surfaced to the user does not carry git's reason, so \
                     nobody can tell which file is holding cleanup back: cleanup said {error:?}, \
                     git said {refusal:?}"
                )),
                Err(_) => {}
            }
            if !Path::new(&info.path).exists() {
                violations.push(format!(
                    "[{class}] the checkout at {} was deleted, and the uncommitted content went \
                     with it",
                    info.path
                ));
            }
            let survived = std::fs::read_to_string(&file).ok();
            if survived.as_deref() != Some(case.contents) {
                violations.push(format!(
                    "[{class}] {} no longer holds the bytes that existed only in this checkout: \
                     {survived:?}",
                    file.display()
                ));
            }
            let listing = WorktreeManager::worktree_list_porcelain(repo_path).expect("git listing");
            if !listing.contains(&info.path) {
                violations.push(format!(
                    "[{class}] the checkout is no longer registered with git, so no `git \
                     worktree` command can address it again and the retry has nothing to retry; \
                     listing was: {listing}"
                ));
            }
            if !branch_exists(repo_path, "task-dirty") {
                violations.push(format!(
                    "[{class}] the task branch was destroyed along with the checkout"
                ));
            }
        }

        assert!(
            violations.is_empty(),
            "automatic cleanup destroyed content that existed nowhere else:\n{}",
            violations.join("\n")
        );
    }

    /// `remove` never reaches for `--force`, stated as the consequence that
    /// distinguishes running it from not running it.
    ///
    /// The direct structural proof would be a `git` shim first on `PATH` that
    /// logs its own argv, the way `tests/task_terminal_cleanup_lifecycle.rs`
    /// does it. That is not available in this module: installing a shim means
    /// mutating `PATH` with `std::env::set_var`, which is process-global and
    /// unsound while any other thread may be reading the environment, and
    /// these tests run as `#[tokio::test]` functions inside the crate's one
    /// parallel test binary. It is the same objection that keeps
    /// `integration_remove_worktree_uses_repo_path_not_process_cwd` away from
    /// `std::env::set_current_dir`, and a separate integration binary is
    /// where a shim belongs.
    ///
    /// So the outcome carries the proof. `git worktree remove --force` on a
    /// checkout holding a modified tracked file deletes that checkout and
    /// exits zero -- asserted here as a positive control at the end, against
    /// this exact checkout, so it is a measured fact about this arrangement
    /// and not a claim about git in general. Finding the checkout, the
    /// modification and the registration all intact after `remove` returned
    /// is therefore only explicable by no `--force` having run.
    ///
    /// Weaker than an argv log in one respect: it cannot see a `--force` that
    /// was spawned and then failed for an unrelated reason. Stronger in
    /// another: it is stated in terms of what a user loses rather than in
    /// terms of an argument string, so a destructive fallback reintroduced
    /// under any other spelling -- `-f`, a recursive delete in Rust, a helper
    /// that hides the flag -- fails it just the same.
    #[tokio::test]
    async fn remove_never_falls_back_to_a_forced_deletion() {
        // Tracked because `create_temp_git_repo` committed README.md, so
        // these bytes are a modification git can see and refuse over.
        const UNCOMMITTED: &str = "# test\nedited in the worktree, never committed\n";

        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        // `Managed` forces the git backend even on a machine with `wt`
        // installed, so this test is about `remove_with_git` everywhere.
        let mut mgr = test_manager();
        mgr.wt_available = true;
        mgr.placement = WorktreePlacement::Managed;
        assert!(
            !mgr.delegates_to_wt(),
            "this test must exercise the git backend whatever is installed on the host"
        );

        let info = mgr
            .create(repo_path, "task-no-force")
            .await
            .expect("create failed");
        let readme = Path::new(&info.path).join("README.md");
        std::fs::write(&readme, UNCOMMITTED).expect("write uncommitted content");

        let error = mgr
            .remove(&info.path, repo_path)
            .await
            .expect_err("a checkout git refuses to remove has not been cleaned up");

        assert!(
            Path::new(&info.path).exists(),
            "the checkout is still there, which is the entire point: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&readme).ok().as_deref(),
            Some(UNCOMMITTED),
            "and it still holds the bytes that exist in no commit anywhere"
        );
        let listing = WorktreeManager::worktree_list_porcelain(repo_path).expect("git listing");
        assert!(
            listing.contains(&info.path),
            "the registration survives too, or the retry has nothing to address; listing was: \
             {listing}"
        );
        assert!(
            branch_exists(repo_path, "task-no-force"),
            "and no removal, refused or otherwise, touches the branch"
        );

        // The positive control. `--force` is run here, by the test, against
        // this same checkout: it succeeds and the directory goes. Everything
        // asserted above is therefore a state `--force` cannot leave behind.
        run_git(repo_path, &["worktree", "remove", "--force", &info.path]);
        assert!(
            !Path::new(&info.path).exists(),
            "positive control: `--force` deletes this exact checkout, so the survival asserted \
             above is only reachable if remove never ran it"
        );
    }

    /// A clean checkout stays removable even when its branch carries a commit
    /// that `main` does not.
    ///
    /// This is the guard rail for the fix that
    /// `a_dirty_worktree_survives_automatic_cleanup` asks for. That refusal
    /// has to stay scoped to content no commit names; widening it into "never
    /// remove a worktree whose branch is unmerged" is an easy way to make the
    /// dirty cases pass and would leave every finished task's checkout on
    /// disk forever, since a task branch is unmerged for as long as its PR is
    /// open.
    ///
    /// `integration_a_removed_worktree_can_be_reattached_from_its_branch`
    /// already removes a worktree whose branch carries a commit, so the delta
    /// here is deliberately small: the unmerged-relative-to-`main` state is
    /// asserted as a precondition rather than merely arranged, and the
    /// registration is asserted to be gone as well as the directory. A fix
    /// that starts consulting merge state then fails here, under a name that
    /// says what it broke.
    #[tokio::test]
    async fn a_clean_worktree_whose_branch_has_unmerged_commits_is_still_removable() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        let mgr = test_manager();

        let info = mgr
            .create(repo_path, "task-unmerged")
            .await
            .expect("create failed");
        let work = commit_work(&info.path, "agent-work.txt");

        assert!(
            run_git(&info.path, &["status", "--porcelain"]).is_empty(),
            "precondition: the checkout must be clean, or this would be testing a dirty case"
        );
        assert!(
            !run_git(
                repo_path,
                &["branch", "--merged", "main", "--format=%(refname)"]
            )
            .lines()
            .any(|refname| refname == "refs/heads/task-unmerged"),
            "precondition: the branch must be unmerged relative to main, or the state this test \
             is named for was never reached"
        );

        mgr.remove(&info.path, repo_path)
            .await
            .expect("a clean checkout stays removable however its branch relates to main");

        assert!(
            !Path::new(&info.path).exists(),
            "the checkout must go: finished tasks are how the disk is reclaimed"
        );
        let listing = WorktreeManager::worktree_list_porcelain(repo_path).expect("git listing");
        assert!(
            !listing.contains(&info.path),
            "the registration must go with it, or the branch can never be checked out again; \
             listing was: {listing}"
        );
        assert!(
            branch_exists(repo_path, "task-unmerged"),
            "the branch survives every removal, and this one is not an exception"
        );
        assert!(
            refs_reaching(repo_path, &work).contains(&"refs/heads/task-unmerged".to_string()),
            "and the commit that made it unmerged is still named by it"
        );
    }

    /// Content git itself ignores is not work, and cleanup treats it the way
    /// git does.
    ///
    /// SlashIt invents no policy here. `git worktree remove` deletes a
    /// checkout whose only non-committed content is git-ignored, and an
    /// agent's build output and editor droppings are exactly that. So the
    /// refusal `a_dirty_worktree_survives_automatic_cleanup` asks for has to
    /// be keyed on the same notion of dirty that `git status` reports, not on
    /// "anything no commit names" -- otherwise the first build a task runs
    /// would pin its checkout to disk permanently.
    #[tokio::test]
    async fn a_worktree_whose_only_uncommitted_content_is_ignored_is_still_removed() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        let mgr = test_manager();

        let info = mgr
            .create(repo_path, "task-ignored")
            .await
            .expect("create failed");
        std::fs::write(Path::new(&info.path).join(".gitignore"), "build/\n")
            .expect("write .gitignore");
        run_git(&info.path, &["add", ".gitignore"]);
        run_git(
            &info.path,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                "ignore build output",
            ],
        );
        std::fs::create_dir(Path::new(&info.path).join("build")).expect("create build dir");
        std::fs::write(
            Path::new(&info.path).join("build").join("artifact.o"),
            "build output\n",
        )
        .expect("write build output");

        assert!(
            run_git(&info.path, &["status", "--porcelain"]).is_empty(),
            "precondition: git must consider the checkout clean, since that is the notion of \
             dirty this policy defers to"
        );
        assert!(
            run_git(&info.path, &["status", "--porcelain", "--ignored"]).contains("build/"),
            "precondition: and something must actually be ignored, or this test proves nothing"
        );

        mgr.remove(&info.path, repo_path)
            .await
            .expect("ignored content is not a reason to keep a checkout alive");

        assert!(
            !Path::new(&info.path).exists(),
            "ordinary git semantics: the checkout goes, build output and all"
        );
        assert!(
            branch_exists(repo_path, "task-ignored"),
            "and the branch is left alone, as it is by every other removal"
        );
    }

    /// Cleanup under worktrunk must not delete the task branch.
    ///
    /// `wt remove` deletes the branch by design whenever it decides that
    /// merging it would add nothing. The first and cheapest of its five
    /// checks is "branch HEAD equals the default branch", which is the state
    /// of every task whose agent committed nothing and of every task whose
    /// work has already landed. Measured against worktrunk (v0.29.0
    /// originally, and confirmed again on v0.68.0): without
    /// `--no-delete-branch` it prints "Removing <branch> worktree & branch
    /// [in background] (same commit as main)" and the branch is gone
    /// afterwards; with `--no-delete-branch` it prints "Branch integrated
    /// (same commit as main); retained with --no-delete-branch" and the
    /// branch survives.
    ///
    /// `remove_with_wt` therefore passes `--no-delete-branch`, and this test
    /// is what holds that flag in place. Without it the two backends disagree
    /// about the one thing `remove`'s contract is entirely about:
    /// `remove_with_git` stopped running `git branch -D` precisely because a
    /// task branch is routinely the only ref naming the commits a task
    /// produced, while the backend that is default on any machine with `wt`
    /// installed would still delete it. `branch_name` is what `create_pr`
    /// pushes and what `reattach` re-checks-out, so both break.
    ///
    /// Ignored by default, like every other real-`wt` test in this file, for
    /// needing the `wt` binary on PATH -- following the PTY tests that are
    /// ignored for needing to spawn real processes. Run it with
    /// `cargo test -- --ignored`.
    ///
    /// The checkout is made with plain `git worktree add` rather than through
    /// `mgr.create`, deliberately. `wt switch` is the only worktrunk
    /// subcommand that consults the `worktree-path` template, and
    /// `remove_with_wt` passes no `--config`, so creating through the
    /// delegating backend would drop a worktree into whichever global root
    /// the developer running the test has configured -- for a real user, a
    /// directory full of their own work. `wt remove` acts on the checkout it
    /// is invoked in, so driving it against a checkout git made under a temp
    /// dir exercises exactly the code under test and can write nowhere else.
    #[tokio::test]
    #[ignore] // Ignore by default as it requires the `wt` binary on PATH
    async fn integration_worktrunk_cleanup_keeps_the_task_branch() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap().to_string();
        let checkout_root = tempfile::TempDir::new().expect("tempdir");
        let checkout = checkout_root
            .path()
            .join("task-checkout")
            .to_string_lossy()
            .to_string();

        run_git(
            &repo_path,
            &[
                "worktree",
                "add",
                "-b",
                "task-worktrunk",
                checkout.as_str(),
                "main",
            ],
        );
        assert_eq!(
            run_git(&repo_path, &["rev-parse", "refs/heads/task-worktrunk"]),
            run_git(&repo_path, &["rev-parse", "refs/heads/main"]),
            "precondition: the branch must sit on main's commit, which is the state wt reads as \
             safe to delete and the state a task that committed nothing is in"
        );

        let mgr = WorktreeManager {
            wt_available: true,
            paths: test_paths(),
            placement: WorktreePlacement::Auto,
        };
        assert!(
            mgr.delegates_to_wt(),
            "this test must exercise the wt backend"
        );

        let result = mgr.remove(&checkout, &repo_path).await;

        // `--no-delete-branch` alone left `wt remove`'s own removal running in
        // a detached background job that could still be mid-flight -- deleting
        // the branch as its tail step -- the instant this call returned, which
        // is why this test used to poll for the job to settle before judging
        // the branch. `remove_with_wt` also passes `--foreground` now, which
        // blocks until that removal, branch decision included, has actually
        // finished; `result` is therefore already the final outcome.
        assert!(result.is_ok(), "remove_with_wt must converge on this checkout: {result:?}");
        assert!(!Path::new(&checkout).exists(), "the checkout must actually be gone");
        assert!(
            branch_exists(&repo_path, "task-worktrunk"),
            "wt cleanup deleted the task branch, which is what create_pr pushes and what \
             reattach checks out again; remove_with_wt must keep passing --no-delete-branch. \
             remove returned {result:?}"
        );
    }

    /// What a run of the fake agent leaves behind, so a test can recognise
    /// its own work rather than trust that something was written.
    const WORK: &str = "work produced in the worktree\n";

    /// Write [`WORK`] into `worktree_path` and commit it, returning the sha.
    ///
    /// Mirrors what the executor does with whatever an agent produced: the
    /// run's output becomes a real commit on the task's real branch, which is
    /// what makes the branch the only thing naming it.
    fn commit_work(worktree_path: &str, file: &str) -> String {
        std::fs::write(Path::new(worktree_path).join(file), WORK).expect("write work");
        run_git(worktree_path, &["add", "-A"]);
        run_git(
            worktree_path,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                "agent work",
            ],
        );
        run_git(worktree_path, &["rev-parse", "HEAD"])
    }

    /// Every ref in `repo_path` from which `commit` can still be reached.
    ///
    /// Empty means the commit is unreachable: still in the object database
    /// until git collects it, but named by nothing, and so gone as far as
    /// anything in SlashIt is concerned.
    fn refs_reaching(repo_path: &str, commit: &str) -> Vec<String> {
        run_git(
            repo_path,
            &["for-each-ref", "--contains", commit, "--format=%(refname)"],
        )
        .lines()
        .map(str::to_string)
        .collect()
    }

    fn run_git(dir: &str, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// What `git worktree remove <worktree_path>` says when it refuses,
    /// trimmed the way `remove_with_git` trims it, or the empty string if git
    /// did not refuse at all.
    ///
    /// Read out of git rather than written down as a literal, because the
    /// exact wording ("contains modified or untracked files, use --force to
    /// delete it") is git's to change and nothing here has an opinion about
    /// it. Provoking a refusal costs nothing: git validates before it
    /// deletes, so a refused removal leaves the checkout and its registration
    /// exactly as they were.
    fn git_removal_refusal(repo_path: &str, worktree_path: &str) -> String {
        let output = std::process::Command::new("git")
            .args(["worktree", "remove", worktree_path])
            .current_dir(repo_path)
            .output()
            .unwrap_or_else(|e| panic!("git worktree remove failed to spawn: {e}"));
        if output.status.success() {
            return String::new();
        }
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    }

    fn branch_exists(repo_path: &str, branch: &str) -> bool {
        std::process::Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
            .current_dir(repo_path)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn worktree_list_porcelain_returns_git_output_for_a_real_repo() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let porcelain = WorktreeManager::worktree_list_porcelain(repo_path)
            .expect("git should have answered for a real repository");
        assert!(
            porcelain.contains("branch refs/heads/main"),
            "expected the main branch in the listing, got: {porcelain}"
        );
    }

    #[test]
    fn worktree_list_porcelain_returns_none_when_git_cannot_be_spawned() {
        // A path that does not exist fails at `chdir`, before git even runs.
        // This used to come back as an empty listing, indistinguishable from
        // git reporting that nothing is registered.
        assert_eq!(
            WorktreeManager::worktree_list_porcelain("/tmp/slashit_not_a_repo_at_all"),
            None,
            "an unusable repo path must not be reported as an authoritative empty listing"
        );
    }

    #[test]
    fn worktree_list_porcelain_returns_none_when_git_exits_non_zero() {
        // A real directory that is not a repository: git spawns fine and
        // exits 128 with empty stdout. Without checking the exit status this
        // is the most dangerous case, because it looks exactly like success.
        let tmp = tempfile::tempdir().expect("temp dir");
        let not_a_repo = tmp.path().to_str().unwrap();

        assert_eq!(
            WorktreeManager::worktree_list_porcelain(not_a_repo),
            None,
            "a non-zero git exit must not be reported as an authoritative empty listing"
        );
    }

    // classify_missing_worktree: the three answers startup needs to tell
    // apart before it decides whether to spend a task's only worktree
    // reference.

    #[test]
    fn classify_missing_worktree_adopts_a_registered_relocated_worktree() {
        let mgr = test_manager();
        let repo = "/home/u/app";
        let branch = "task-abcd1234";

        let managed = mgr.managed_path(repo, branch);
        std::fs::create_dir_all(&managed).expect("create managed worktree dir");
        let porcelain = format!(
            "worktree {}\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/{}\n",
            managed.display(),
            branch
        );

        let result = mgr.classify_missing_worktree(repo, branch, Some(&porcelain));

        let _ = std::fs::remove_dir_all(managed.parent().unwrap());

        assert_eq!(
            result,
            WorktreeRecovery::Adopt(managed.to_string_lossy().to_string()),
            "a worktree git confirms at the managed path must be adopted"
        );
    }

    /// Integration Scenario A: a task branch registered at an external,
    /// non-managed worktree (e.g. one `wt` placed under its own convention)
    /// is still adopted through the `adopt_any_registered` fallback.
    #[test]
    fn classify_missing_worktree_adopts_an_externally_placed_worktree() {
        let mgr = test_manager();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let repo = tmp.path().join("primary-checkout");
        std::fs::create_dir_all(&repo).expect("create primary checkout dir");
        let external = tmp.path().join("wherever-wt-put-it");
        std::fs::create_dir_all(&external).expect("create external worktree dir");
        let branch = "task-abcd1234";

        let porcelain = format!(
            "worktree {}\nHEAD 2222222222222222222222222222222222222222\nbranch refs/heads/{}\n",
            external.display(),
            branch
        );

        let result = mgr.classify_missing_worktree(
            repo.to_str().unwrap(),
            branch,
            Some(&porcelain),
        );

        assert_eq!(
            result,
            WorktreeRecovery::Adopt(external.to_string_lossy().to_string()),
            "a genuine external registered worktree must still be adoptable"
        );
    }

    /// Integration Scenario B: the only registered worktree for the branch is
    /// the primary checkout itself. Adoption must refuse it and report
    /// confirmed absence rather than handing the primary path downstream to
    /// an agent or VCS command.
    #[test]
    fn classify_missing_worktree_refuses_to_adopt_the_primary_checkout() {
        let mgr = test_manager();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let repo = tmp.path().join("primary-checkout");
        std::fs::create_dir_all(&repo).expect("create primary checkout dir");
        let branch = "task-abcd1234";

        let porcelain = format!(
            "worktree {}\nHEAD 3333333333333333333333333333333333333333\nbranch refs/heads/{}\n",
            repo.display(),
            branch
        );

        let result = mgr.classify_missing_worktree(
            repo.to_str().unwrap(),
            branch,
            Some(&porcelain),
        );

        assert_eq!(
            result,
            WorktreeRecovery::ConfirmedAbsent,
            "the primary checkout must never be handed back as an adoptable task worktree"
        );
    }

    #[test]
    fn classify_missing_worktree_confirms_absence_when_git_lists_nothing() {
        let mgr = test_manager();

        assert_eq!(
            mgr.classify_missing_worktree("/home/u/app", "task-abcd1234", Some("")),
            WorktreeRecovery::ConfirmedAbsent,
            "git answering with no registration is positive proof the reference is dead"
        );
    }

    #[test]
    fn classify_missing_worktree_reports_unverified_when_git_could_not_be_consulted() {
        let mgr = test_manager();

        assert_eq!(
            mgr.classify_missing_worktree("/home/u/app", "task-abcd1234", None),
            WorktreeRecovery::Unverified,
            "a failed git lookup must not be reported as confirmed absence"
        );
    }

    #[tokio::test]
    async fn integration_create_stacked_branch_falls_back_to_git_when_placement_is_managed() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        // `wt_available` is forced true without an actual `wt` binary on
        // PATH: under `Managed` placement this must never be consulted, so
        // if the fix regresses to checking `wt_available` directly, this
        // test fails by trying (and failing) to run a nonexistent `wt`.
        let mgr = WorktreeManager {
            wt_available: true,
            paths: test_paths(),
            placement: WorktreePlacement::Managed,
        };

        let info = mgr
            .create_stacked_branch(repo_path, "stacked-branch", "main")
            .await
            .expect("create_stacked_branch should fall back to git, not delegate to wt").info;
        assert!(Path::new(&info.path).exists(), "worktree dir should exist");
        assert_eq!(info.branch, "stacked-branch");
    }

    #[tokio::test]
    async fn integration_remove_falls_back_to_git_when_placement_is_managed() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        // Mirrors `integration_create_stacked_branch_falls_back_to_git_when_placement_is_managed`:
        // `wt_available` is forced true without an actual `wt` binary on
        // PATH. `remove()` must gate on `delegates_to_wt()` (false here,
        // since placement is `Managed`), not raw `wt_available`. If the gate
        // regresses to checking `wt_available` directly, this test fails by
        // trying (and failing) to run a nonexistent `wt remove`.
        let mgr = WorktreeManager {
            wt_available: true,
            paths: test_paths(),
            placement: WorktreePlacement::Managed,
        };

        let info = mgr
            .create(repo_path, "managed-remove")
            .await
            .expect("create failed");
        assert!(Path::new(&info.path).exists());

        let result = mgr.remove(&info.path, repo_path).await;
        assert!(
            result.is_ok(),
            "remove should use the git-managed path, not remove_with_wt: {:?}",
            result.err()
        );
        assert!(!Path::new(&info.path).exists(), "worktree directory should be removed");
    }

    #[tokio::test]
    async fn integration_exists_nonexistent() {
        let mgr = test_manager();
        assert!(!mgr.exists("/tmp/slashit_does_not_exist_999"));
    }

    #[tokio::test]
    async fn integration_remove_worktree() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();

        let info = mgr.create(repo_path, "remove-me").await.expect("create failed");
        assert!(Path::new(&info.path).exists());

        // `remove_with_git` now takes `repo_path` explicitly and sets
        // `.current_dir` on every git invocation, so this no longer needs to
        // mutate the process-wide CWD (which was also unsound under
        // parallel test execution).
        let result = mgr.remove(&info.path, repo_path).await;
        assert!(result.is_ok(), "remove should succeed: {:?}", result.err());
        assert!(
            !Path::new(&info.path).exists(),
            "worktree directory should be removed"
        );

        assert!(
            branch_exists(repo_path, "remove-me"),
            "the branch is not the worktree and is not removed with it"
        );
    }

    #[tokio::test]
    async fn integration_remove_worktree_uses_repo_path_not_process_cwd() {
        // Two unrelated repos, neither of which is the process's own ambient
        // CWD (wherever `cargo test` happens to run from — this crate's own
        // checkout, never one of these temp repos). `remove_with_git` sets
        // `.current_dir(repo_path)` on every git invocation, so passing
        // `target_repo_path` explicitly must succeed regardless of that
        // ambient CWD. Proving this does not require mutating the
        // process-wide CWD via `std::env::set_current_dir`, which would be
        // unsound here: it is process-global, not per-thread, and other
        // `#[tokio::test]` functions can be running concurrently in the same
        // process.
        let target_repo = create_temp_git_repo();
        let target_repo_path = target_repo.path().to_str().unwrap().to_string();
        let unrelated_repo = create_temp_git_repo();

        assert_ne!(
            std::env::current_dir().unwrap(),
            target_repo.path(),
            "the test's own CWD must not coincidentally be the target repo, or this test would \
             not actually exercise CWD independence"
        );

        let mgr = test_manager();
        let info = mgr
            .create(&target_repo_path, "cwd-independent")
            .await
            .expect("create failed");

        let result = mgr.remove(&info.path, &target_repo_path).await;

        assert!(result.is_ok(), "remove should succeed: {:?}", result.err());
        assert!(
            !Path::new(&info.path).exists(),
            "worktree directory should be removed from the target repo"
        );

        // The observable proof that git ran against `target_repo_path` and
        // not the ambient CWD is the registration, since the branch is no
        // longer touched by a removal at all.
        let listing = WorktreeManager::worktree_list_porcelain(&target_repo_path)
            .expect("git should have answered for the target repo");
        assert!(
            !listing.contains(&info.path),
            "the registration should be gone from the target repo"
        );

        // The unrelated repo must never have been touched by any of this.
        assert!(
            !branch_exists(unrelated_repo.path().to_str().unwrap(), "cwd-independent"),
            "the unrelated repo must never see a branch created in the target repo"
        );
    }

    /// Deterministic reproduction of the false-success bug: `git worktree
    /// remove` cannot actually delete the directory (a permission-blocked,
    /// non-empty subdirectory defeats the recursive delete it relies on), so
    /// `remove_with_git` must report `Err` — never silently `Ok(())` for a
    /// worktree still on disk.
    ///
    /// This is the case the physical-existence check exists for and the one
    /// no exit status can decide, which is why it stays interesting now that
    /// there is only ever one attempt: the obstacle here is the filesystem
    /// rather than anything git has an opinion about, so a removal can get
    /// most of the way through and still leave a directory behind.
    ///
    /// Unix-only: relies on `std::os::unix::fs::PermissionsExt` and the
    /// `ipc` module's `current_uid()`, neither of which exist when compiling
    /// for Windows (`ipc` itself is `#[cfg(unix)]`-gated in `lib.rs`).
    #[cfg(unix)]
    #[tokio::test]
    async fn remove_with_git_reports_err_when_the_directory_survives_the_removal() {
        use std::os::unix::fs::PermissionsExt;

        if crate::ipc::server::current_uid() == 0 {
            return; // root ignores the permission bits this test relies on
        }

        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let info = mgr
            .create(repo_path, "undeletable")
            .await
            .expect("create failed");

        let blocked = Path::new(&info.path).join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("file.txt"), b"content").unwrap();
        // No read/write/execute: git cannot list or unlink this directory's
        // contents, so the directory itself can never become empty and the
        // removal cannot fully delete it.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = mgr.remove(&info.path, repo_path).await;

        // Restore permissions so the temp dir can be cleaned up on drop.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            result.is_err(),
            "removal that cannot actually delete the directory must report Err"
        );
        assert!(
            Path::new(&info.path).exists(),
            "the worktree directory must still be there when removal is reported as failed, so \
             a caller retains worktree_path for a future retry"
        );
    }

    /// The distinction [`WorktreeManager::exists`] cannot make, and the
    /// reason a removal's success gate is decided on `proven_absent` instead
    /// of on it.
    ///
    /// The tests above chmod something *inside* the worktree -- a
    /// subdirectory, or the worktree directory itself -- which still lets
    /// `metadata` on the worktree path resolve (or answer `NotFound` once
    /// the whole thing is gone). This targets the worktree's *parent*
    /// instead, so `metadata(worktree_path)` itself fails with a permission
    /// error rather than `NotFound` -- the one case `exists()` cannot tell
    /// apart from genuine absence.
    #[cfg(unix)]
    #[test]
    fn proven_absent_separates_not_there_from_could_not_look() {
        use std::os::unix::fs::PermissionsExt;

        if crate::ipc::server::current_uid() == 0 {
            return; // root ignores the permission bits this test relies on
        }

        let tmp = create_temp_git_repo();
        let closed = tmp.path().join("closed");
        std::fs::create_dir(&closed).expect("create the unreadable parent");
        let hidden = closed.join("worktree");
        std::fs::create_dir(&hidden).expect("create the worktree inside it");
        let hidden = hidden.to_str().unwrap().to_string();
        let never = tmp.path().join("never-existed");
        let never = never.to_str().unwrap().to_string();

        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000))
            .expect("close off the parent");
        let exists_says = test_manager().exists(&hidden);
        let proven_says = WorktreeManager::proven_absent(&hidden);
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755))
            .expect("reopen the parent");

        assert!(
            !exists_says,
            "`exists` reports a path it was not allowed to look at as absent -- that is the \
             answer a removal's success gate must not inherit"
        );
        assert!(
            !proven_says,
            "a path that could not be looked at is not proven absent, and a removal must not \
             report success on that basis"
        );
        assert!(
            WorktreeManager::proven_absent(&never),
            "a path that is genuinely not there is the case the success gate exists for"
        );

        // A link pointing at nothing has no worktree at it, and `exists`
        // already says so. Disagreeing here would strand every caller of a
        // removal that really did finish: the success gate would report
        // `Err` for a worktree that is already gone, and no retry could ever
        // converge on it.
        let dangling = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("nothing-here"), &dangling)
            .expect("create the dangling link");
        let dangling = dangling.to_str().unwrap().to_string();
        assert!(
            !test_manager().exists(&dangling),
            "`exists` follows the link and finds nothing"
        );
        assert!(
            WorktreeManager::proven_absent(&dangling),
            "and this must agree with it, or a removal that really finished would be reported \
             as one that did not"
        );
    }

    /// The branch-preservation and synchronicity contract `remove_with_wt`
    /// depends on, proven against the exact argument list it spawns rather
    /// than against real `wt` output -- so it runs on every machine, with or
    /// without the `wt` binary on `PATH`, and needs no process at all.
    ///
    /// `remove_with_wt_preserves_and_restores_the_registration_when_its_own_removal_cannot_finish`
    /// is the real-`wt` counterpart that exercises what these flags actually
    /// do; this test is what keeps that contract from silently regressing on
    /// every other run, since that one is `#[ignore]`d.
    #[test]
    fn wt_remove_args_run_in_the_foreground_and_keep_the_branch() {
        let args = WorktreeManager::WT_REMOVE_ARGS;
        assert!(
            args.contains(&"--foreground"),
            "without this, `wt remove` returns before the removal it started has finished, and \
             the caller has nothing to evaluate `proven_absent` against yet: {args:?}"
        );
        assert!(
            args.contains(&"--no-delete-branch"),
            "without this, Worktrunk decides for itself whether the task's branch survives \
             removal -- SlashIt must be the one requiring that it does: {args:?}"
        );
    }

    /// The exact parent-blocked scenario the `exists()`-gated success check
    /// this module used to have could misclassify as removed: both `git
    /// worktree remove` and the caller's own filesystem check answer "not
    /// there" for a directory the caller was simply never allowed to look
    /// at.
    ///
    /// `git worktree remove` itself exits `0` here without touching the
    /// directory at all -- it cannot even stat the path through a
    /// closed-off parent, and answers that the same way it would answer a
    /// worktree that is genuinely gone. That is `?`-mapped to a spawn/exit
    /// failure in the tests above, where the obstacle is inside the
    /// worktree rather than in its parent; here it is git's own exit status
    /// that is misleading, which is exactly why the success gate this test
    /// is about does not trust it either. The gate itself is proven
    /// directly against `proven_absent` in the unit test above; this is the
    /// same scenario at the level `remove()` callers observe.
    #[cfg(unix)]
    #[tokio::test]
    async fn remove_reports_err_rather_than_false_success_when_the_worktree_cannot_be_looked_at() {
        use std::os::unix::fs::PermissionsExt;

        if crate::ipc::server::current_uid() == 0 {
            return; // root ignores the permission bits this test relies on
        }

        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let info = mgr
            .create(repo_path, "task-parent-blocked")
            .await
            .expect("create failed");

        let parent = Path::new(&info.path)
            .parent()
            .expect("a worktree path has a parent")
            .to_path_buf();
        let restore_mode = std::fs::metadata(&parent)
            .expect("read the parent's mode")
            .permissions()
            .mode();

        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000))
            .expect("close off the parent");
        let result = mgr.remove(&info.path, repo_path).await;
        // Reopened before asserting, so a failure still leaves a removable
        // temp dir behind and so the worktree can be confirmed still present
        // below.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(restore_mode))
            .expect("reopen the parent");

        assert!(
            result.is_err(),
            "a removal that could not prove the worktree gone must report Err, not silently \
             `Ok(())`, which would let the caller discard the only handle back to a worktree \
             that may still be fully present: {result:?}"
        );
        assert!(
            Path::new(&info.path).exists(),
            "the worktree was never actually touched by this removal attempt, so it must still \
             be there once its parent is reopened"
        );
    }

    /// `WorktreeRecord`'s own reverse lookup, proven directly rather than
    /// only through a full `remove()` call.
    ///
    /// A synthetic `worktrees_dir` is built by hand so the ambiguous and
    /// malformed cases can be constructed at all -- git itself would never
    /// produce them, which is exactly why the lookup has to fail closed on
    /// them rather than assume they cannot occur.
    #[test]
    fn locate_admin_matches_the_exact_gitdir_pointer_and_nothing_else() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).expect("create worktrees dir");

        let mine = tmp.path().join("checkouts").join("mine");
        let theirs = tmp.path().join("checkouts").join("theirs");

        let mine_admin = worktrees_dir.join("mine");
        std::fs::create_dir(&mine_admin).expect("create mine's record");
        std::fs::write(mine_admin.join("gitdir"), format!("{}\n", mine.join(".git").display()))
            .expect("write mine's gitdir");

        let theirs_admin = worktrees_dir.join("theirs");
        std::fs::create_dir(&theirs_admin).expect("create theirs' record");
        std::fs::write(theirs_admin.join("gitdir"), format!("{}\n", theirs.join(".git").display()))
            .expect("write theirs' gitdir");

        assert_eq!(
            WorktreeRecord::locate_admin(&worktrees_dir, mine.to_str().unwrap()),
            Some(mine_admin),
            "must find the record whose gitdir names exactly this path"
        );
        assert_eq!(
            WorktreeRecord::locate_admin(&worktrees_dir, theirs.to_str().unwrap()),
            Some(theirs_admin),
            "an unrelated record must resolve to itself, not to whichever one is listed first"
        );

        let never_registered = tmp.path().join("checkouts").join("never-registered");
        assert_eq!(
            WorktreeRecord::locate_admin(&worktrees_dir, never_registered.to_str().unwrap()),
            None,
            "no record names this path, so there is nothing to find"
        );
    }

    #[test]
    fn locate_admin_fails_closed_on_an_ambiguous_match() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).expect("create worktrees dir");

        let shared = tmp.path().join("checkouts").join("shared");
        for name in ["a", "b"] {
            let admin = worktrees_dir.join(name);
            std::fs::create_dir(&admin).expect("create a record");
            std::fs::write(admin.join("gitdir"), format!("{}\n", shared.join(".git").display()))
                .expect("write its gitdir");
        }

        assert_eq!(
            WorktreeRecord::locate_admin(&worktrees_dir, shared.to_str().unwrap()),
            None,
            "two records claiming the same worktree path cannot be arbitrated -- restoring the \
             wrong one is worse than restoring none"
        );
    }

    #[test]
    fn locate_admin_skips_a_record_it_cannot_read() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let worktrees_dir = tmp.path().join("worktrees");
        std::fs::create_dir(&worktrees_dir).expect("create worktrees dir");

        // No `gitdir` file at all: not something git itself would leave, but
        // exactly the shape a record this cannot make sense of takes.
        std::fs::create_dir(worktrees_dir.join("malformed")).expect("create the bad record");

        let mine = tmp.path().join("checkouts").join("mine");
        let mine_admin = worktrees_dir.join("mine");
        std::fs::create_dir(&mine_admin).expect("create mine's record");
        std::fs::write(mine_admin.join("gitdir"), format!("{}\n", mine.join(".git").display()))
            .expect("write mine's gitdir");

        assert_eq!(
            WorktreeRecord::locate_admin(&worktrees_dir, mine.to_str().unwrap()),
            Some(mine_admin),
            "a record this cannot read must be skipped, not treated as a match or as fatal"
        );
    }

    /// The exact bytes [`WorktreeRecord::save`] synthesizes for a `.git`
    /// pointer it could not read must be indistinguishable from what git
    /// itself writes -- a restore that handed git a file in the wrong shape
    /// would just trade a lost registration for a corrupted one.
    #[tokio::test]
    async fn synthesized_gitlink_matches_what_git_itself_writes() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let info = mgr
            .create(repo_path, "task-gitlink-format")
            .await
            .expect("create failed");

        let real_gitlink =
            std::fs::read(Path::new(&info.path).join(".git")).expect("read the real .git file");
        let admin = admin_record_of(&info.path);
        let synthesized = format!("gitdir: {}\n", admin.display()).into_bytes();

        assert_eq!(
            real_gitlink, synthesized,
            "the reconstructed pointer must match byte-for-byte what git itself writes, or a \
             restore would hand git a file in a shape it does not recognize"
        );
    }

    /// The full failure-and-recovery sequence for the worktree-parent-blocked
    /// scenario, not just the immediate `Err`: a removal that could not look
    /// at the worktree at all must leave enough behind -- both the directory
    /// and git's own registration for it -- that a later, unobstructed
    /// removal can still finish the job.
    ///
    /// A real `git worktree remove` against this exact obstacle exits `0`
    /// and deletes `<common>/worktrees/<id>` for a path it cannot stat at
    /// all, without ever touching the directory (confirmed by hand against a
    /// real repository before writing this test). `WorktreeRecord::save`'s
    /// repository-side lookup is what survives that: it never reads anything
    /// under the worktree's own blocked parent, so it can copy the record
    /// before git's own attempt destroys it.
    #[cfg(unix)]
    #[tokio::test]
    async fn remove_preserves_and_restores_the_registration_when_the_worktree_cannot_be_looked_at()
    {
        use std::os::unix::fs::PermissionsExt;

        if crate::ipc::server::current_uid() == 0 {
            return; // root ignores the permission bits this test relies on
        }

        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let info = mgr
            .create(repo_path, "task-registration-blocked")
            .await
            .expect("create failed");

        let registered_before =
            WorktreeManager::worktree_list_porcelain(repo_path).expect("git listing");
        assert!(
            registered_before.contains(&info.path),
            "the worktree must be registered before anything happens to it"
        );

        let parent = Path::new(&info.path)
            .parent()
            .expect("a worktree path has a parent")
            .to_path_buf();
        let restore_mode = std::fs::metadata(&parent)
            .expect("read the parent's mode")
            .permissions()
            .mode();

        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000))
            .expect("close off the parent");
        let blocked = mgr.remove(&info.path, repo_path).await;
        // Reopened before asserting, so the outcome can actually be inspected
        // and so a later attempt in this same test has something to act on.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(restore_mode))
            .expect("reopen the parent");

        assert!(
            blocked.is_err(),
            "a removal that could not prove the worktree gone must report Err: {blocked:?}"
        );
        assert!(
            Path::new(&info.path).exists(),
            "the worktree was never actually touched by this removal attempt"
        );

        let registered_after =
            WorktreeManager::worktree_list_porcelain(repo_path).expect("git listing");
        assert!(
            registered_after.contains(&info.path),
            "git's own registration must survive or be restored, not be discarded alongside the \
             wrongly-assumed removal -- without it nothing can ever act on this worktree again"
        );

        // Git's registration surviving is only useful if a real removal can
        // still use it: a later legitimate cleanup, once access is restored,
        // has to actually converge.
        mgr.remove(&info.path, repo_path)
            .await
            .expect("the attempt made once the obstacle is gone is the one that has to converge");
        assert!(
            !Path::new(&info.path).exists(),
            "the retry has to actually remove the worktree, not merely stop failing"
        );
        assert!(
            branch_exists(repo_path, "task-registration-blocked"),
            "the branch must survive cleanup, as it does for every other removal path"
        );
        assert!(
            saved_records(repo_path).is_empty(),
            "no saved-record debris should remain once the worktree has actually converged"
        );
    }

    /// The same parent-blocked scenario, reached through `remove_with_wt`'s
    /// delegation rather than by calling the git backend directly.
    ///
    /// `remove_with_wt`'s own entry guard treats a worktree it cannot see as
    /// one to hand to `remove_with_git` -- that guard is `!self.exists(...)`,
    /// which is `true` for a blocked parent exactly as it is for a genuinely
    /// missing directory, so this case reaches the same repository-side
    /// preservation mechanism proven directly above rather than a separate,
    /// unprotected path. Needs no `wt` binary: the delegation happens before
    /// anything is spawned.
    #[cfg(unix)]
    #[tokio::test]
    async fn remove_with_wt_delegation_reaches_the_same_registration_preservation() {
        use std::os::unix::fs::PermissionsExt;

        if crate::ipc::server::current_uid() == 0 {
            return; // root ignores the permission bits this test relies on
        }

        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mut mgr = test_manager();
        mgr.placement = WorktreePlacement::Managed;
        let info = mgr
            .create(repo_path, "task-wt-registration-blocked")
            .await
            .expect("create failed");

        let parent = Path::new(&info.path)
            .parent()
            .expect("a worktree path has a parent")
            .to_path_buf();
        let restore_mode = std::fs::metadata(&parent)
            .expect("read the parent's mode")
            .permissions()
            .mode();

        mgr.wt_available = true;
        mgr.placement = WorktreePlacement::Auto;
        assert!(mgr.delegates_to_wt(), "this test must exercise the wt backend's delegation");

        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000))
            .expect("close off the parent");
        let blocked = mgr.remove(&info.path, repo_path).await;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(restore_mode))
            .expect("reopen the parent");

        assert!(
            blocked.is_err(),
            "delegation must not turn a blocked-parent removal into a false success: {blocked:?}"
        );
        let registered_after =
            WorktreeManager::worktree_list_porcelain(repo_path).expect("git listing");
        assert!(
            registered_after.contains(&info.path),
            "the registration must survive delegation through remove_with_wt exactly as it does \
             for the git backend called directly"
        );

        // The retry converging is already proven, in full, against the git
        // backend directly above. Routing it back through real `wt` here
        // would test worktrunk's own ability to act on a worktree it never
        // created (this one came from `mgr.create`, not `wt switch`), which
        // is a different, unrelated concern -- so the retry is pointed at
        // the git backend, the same one that actually did the preserving.
        mgr.placement = WorktreePlacement::Managed;
        mgr.remove(&info.path, repo_path)
            .await
            .expect("the retry once access is restored has to converge");
        assert!(!Path::new(&info.path).exists(), "the retry has to actually remove the worktree");
    }

    /// The shared decision point both backends funnel through
    /// (`WorktreeManager::finish_removal`), exercised directly against the
    /// exact post-state a removal tool that could not finish leaves: the
    /// checkout is still there, but the registration it pointed at is
    /// already gone.
    ///
    /// This is the deterministic seam for a scenario `remove_with_wt`'s real
    /// invocation cannot be driven through without either a timing race or
    /// the real `wt` binary. `remove_with_wt_preserves_and_restores_the_registration_when_its_own_removal_cannot_finish`
    /// below proves the identical scenario against real `wt`, gated behind
    /// `#[ignore]` for the same reason every other real-`wt` test in this
    /// file is; this test proves the recovery logic itself, unconditionally,
    /// on every run.
    #[tokio::test]
    async fn finish_removal_restores_a_registration_the_removal_tool_pruned() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let info = mgr
            .create(repo_path, "task-tool-pruned-registration")
            .await
            .expect("create failed");

        // Saved exactly as both backends save it: before anything
        // destructive runs.
        let record = WorktreeRecord::save(&info.path, repo_path).await;
        assert!(record.is_some(), "a real, freshly created worktree always has a record to save");

        // Exactly the state a removal tool leaves when it prunes git's own
        // registration without finishing the physical removal -- confirmed
        // by hand against both a bare `git worktree remove` and a real `wt
        // remove` whose parent directory could not be written to.
        forget_worktree_record(repo_path, &info.path);
        assert!(
            Path::new(&info.path).exists(),
            "precondition: the checkout itself is untouched"
        );
        assert!(
            !WorktreeManager::worktree_list_porcelain(repo_path)
                .unwrap()
                .contains(&info.path),
            "precondition: git no longer registers it"
        );

        let outcome = WorktreeManager::finish_removal(
            &info.path,
            record,
            "the removal tool reported success but left the checkout behind".to_string(),
        );

        assert!(
            outcome.is_err(),
            "a checkout that is not proven gone must report Err: {outcome:?}"
        );
        let registered_after =
            WorktreeManager::worktree_list_porcelain(repo_path).expect("git listing");
        assert!(
            registered_after.contains(&info.path),
            "the registration must be restored, not left pruned alongside a checkout that is \
             still fully present"
        );

        // A later legitimate cleanup, once whatever tool ran has nothing
        // left to stand in its way, still has to converge.
        mgr.remove(&info.path, repo_path)
            .await
            .expect("the retry has to actually finish once the registration is back");
        assert!(!Path::new(&info.path).exists(), "the retry has to actually remove the worktree");
        assert!(
            branch_exists(repo_path, "task-tool-pruned-registration"),
            "the branch must survive, as it does for every other removal path"
        );
        assert!(
            saved_records(repo_path).is_empty(),
            "no saved-record debris should remain once the worktree has actually converged"
        );
    }

    /// The real-`wt` counterpart to
    /// `finish_removal_restores_a_registration_the_removal_tool_pruned`
    /// above: the same scenario, but driven through `remove_with_wt`'s
    /// actual invocation of the installed `wt` binary rather than
    /// fabricated by hand.
    ///
    /// The obstacle is the worktree's *parent* directory losing write
    /// permission (not read or execute, which stay intact) before `remove`
    /// is called -- a static precondition set once, not a race.
    /// `Path::exists` only needs execute permission on the parent to
    /// succeed, so the entry guard above still sees the worktree as present
    /// and lets `wt remove` actually run; `wt`'s own internal rename then
    /// needs *write* permission on that same parent and cannot get it,
    /// deleting the worktree's `.git` pointer and pruning git's
    /// registration before failing on the directory itself. Confirmed by
    /// hand against a real, installed `wt` before writing this test, and
    /// stable immediately after the `--foreground` call returns -- checked
    /// with no sleep, and again a second later, with the same result both
    /// times.
    ///
    /// The retry is not asserted to fully remove the checkout, and that is
    /// deliberate, not a weaker test. `wt`'s own partial-removal attempt here
    /// deletes tracked file content on its way to failing (confirmed by
    /// hand: the checkout comes back from this obstacle missing files
    /// `git worktree add` had put there, not merely missing its `.git`
    /// pointer), and [`WorktreeRecord`] was never meant to cover that -- its
    /// own doc comment scopes it to git's bookkeeping, not user content. A
    /// forceless retry against a checkout git now considers dirty correctly
    /// keeps refusing, exactly as `remove_with_git` does for real
    /// uncommitted changes and exactly as this project's own removal of a
    /// destructive `--force` fallback intends: this pass does not add one to
    /// the `wt` backend either. So the only thing asserted about the retry
    /// is the same invariant proven above -- it is never a silent `Ok(())`
    /// while the checkout is still there.
    ///
    /// The checkout is made with plain `git worktree add` rather than
    /// through `mgr.create` or `wt switch`: `wt switch` is the only
    /// worktrunk subcommand that consults the `worktree-path` template, and
    /// creating through it would drop a worktree into whichever global root
    /// the developer running this test has configured -- for a real user, a
    /// directory full of their own work. The branch is created at the same
    /// commit as `main` (no commits follow `git worktree add`), which is
    /// exactly the case Worktrunk's own merge-detection considers safe to
    /// delete on an ordinary `wt remove` -- so the branch surviving below is
    /// proof of `--no-delete-branch` overriding that heuristic, not an
    /// accident of the branch already being unmerged.
    ///
    /// Ignored by default because it is the only other test in this file
    /// that needs the `wt` binary on `PATH`. Run it with
    /// `cargo test -- --ignored`.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore] // Ignore by default as it requires the `wt` binary on PATH
    async fn remove_with_wt_preserves_and_restores_the_registration_when_its_own_removal_cannot_finish()
    {
        use std::os::unix::fs::PermissionsExt;

        if crate::ipc::server::current_uid() == 0 {
            return; // root ignores the permission bits this test relies on
        }

        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap().to_string();
        let checkout_root = tempfile::TempDir::new().expect("tempdir");
        let checkout = checkout_root
            .path()
            .join("task-checkout")
            .to_string_lossy()
            .to_string();

        run_git(
            &repo_path,
            &["worktree", "add", "-b", "task-wt-parent-blocked", checkout.as_str(), "main"],
        );

        let mut mgr = test_manager();
        mgr.wt_available = true;
        mgr.placement = WorktreePlacement::Auto;
        assert!(mgr.delegates_to_wt(), "this test must exercise the wt backend");

        let parent = checkout_root.path().to_path_buf();
        let restore_mode = std::fs::metadata(&parent)
            .expect("read the parent's mode")
            .permissions()
            .mode();

        // Read and traverse survive; write does not -- `Path::exists` needs
        // only the former, so the entry guard still lets `wt remove` run.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500))
            .expect("close write access to the parent");
        let blocked = mgr.remove(&checkout, &repo_path).await;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(restore_mode))
            .expect("reopen the parent");

        assert!(
            blocked.is_err(),
            "a removal wt could not finish must report Err, not silently Ok(()): {blocked:?}"
        );
        assert!(Path::new(&checkout).exists(), "the checkout was never actually removed");
        let registered_after =
            WorktreeManager::worktree_list_porcelain(&repo_path).expect("git listing");
        assert!(
            registered_after.contains(&checkout),
            "git's own registration must survive or be restored, not be discarded alongside \
             wt's own partial removal"
        );

        // `--foreground` makes this deterministic rather than a race to
        // catch a background job before it lands, so a single retry, not
        // several spaced by sleeps, is enough to observe the stable outcome.
        // It is not asserted to converge: `wt`'s own partial-removal attempt
        // above deletes tracked file content on its way to failing (see the
        // doc comment above), leaving the checkout dirty, and a forceless
        // retry against a dirty checkout correctly keeps refusing.
        let retried = mgr.remove(&checkout, &repo_path).await;
        let converged = !Path::new(&checkout).exists();
        assert!(
            retried.is_ok() == converged,
            "remove() must never report Ok(()) while the checkout is still present, nor Err \
             once it is actually gone: {retried:?}, converged={converged}"
        );

        // Branch preservation is unconditional here, unlike the checkout
        // itself: `--no-delete-branch` is on every `wt remove` call this
        // backend makes, including the blocked one above, so Worktrunk never
        // reaches its own merge-detection for this branch regardless of
        // whether the retry actually converged.
        assert!(
            branch_exists(&repo_path, "task-wt-parent-blocked"),
            "SlashIt requires --no-delete-branch on every wt remove call, so the branch must \
             survive whether or not the checkout itself converged"
        );

        if converged {
            assert!(
                saved_records(&repo_path).is_empty(),
                "no saved-record debris should remain once the worktree has actually converged"
            );
        }
    }

    /// The retry contract, at the layer that has to honour it: a removal that
    /// could not run must leave the worktree in a state a later removal can
    /// still act on, and that later removal must actually finish.
    ///
    /// The obstacle is the worktree directory itself, unreadable. Git cannot
    /// reach `<path>/.git` through it, so both removes refuse at validation
    /// without touching anything -- which is exactly the situation a retry is
    /// for, since the worktree is still whole and a later attempt has
    /// something to succeed at.
    ///
    /// The registration assertion is the regression. A repository-wide `git
    /// worktree prune` used to run here, and it decides a record is stale by
    /// whether it can read that same `.git`, so it read this worktree as gone
    /// and dropped the record for a directory still fully present. Nothing
    /// can remove a path git no longer registers: every later attempt then
    /// failed with "is not a working tree", `exists` stayed true, and the
    /// no later attempt, however many times the user asked, could ever
    /// finish. The prune is gone now, and the assertion holds all the same --
    /// it is about the record surviving, not about what might destroy it.
    ///
    /// Unix-only for the same reason as the test above.
    #[cfg(unix)]
    #[tokio::test]
    async fn integration_a_removal_that_could_not_run_converges_once_the_obstacle_is_gone() {
        use std::os::unix::fs::PermissionsExt;

        if crate::ipc::server::current_uid() == 0 {
            return; // root ignores the permission bits the obstacle is made of
        }

        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let info = mgr
            .create(repo_path, "task-blocked1")
            .await
            .expect("create failed");
        let restore = std::fs::metadata(&info.path)
            .expect("read the worktree mode")
            .permissions()
            .mode();

        std::fs::set_permissions(&info.path, std::fs::Permissions::from_mode(0o000))
            .expect("close off the worktree");
        let blocked = mgr.remove(&info.path, repo_path).await;
        let registered = WorktreeManager::worktree_list_porcelain(repo_path).expect("git listing");
        // Reopened before asserting, so a failure still leaves a removable
        // temp dir behind.
        std::fs::set_permissions(&info.path, std::fs::Permissions::from_mode(restore))
            .expect("reopen the worktree");

        assert!(
            blocked.is_err(),
            "a removal that could not delete the directory must report Err: {blocked:?}"
        );
        assert!(
            registered.contains(&info.path),
            "the registration must survive a removal that did nothing, or nothing can ever \
             remove the directory again and no later attempt can converge"
        );

        mgr.remove(&info.path, repo_path)
            .await
            .expect("the attempt made once the obstacle is gone is the one that has to converge");
        assert!(
            !Path::new(&info.path).exists(),
            "the retry has to actually remove the worktree, not merely stop failing"
        );
    }

    /// The other way a removal fails, and the one git does not walk away
    /// from cleanly.
    ///
    /// The obstacle here is inside the worktree rather than the worktree
    /// itself, so git can read `<path>/.git`, validates the checkout, accepts
    /// the removal and starts deleting -- and only then reaches a directory
    /// whose entries it may not unlink. Git tears the worktree's record down
    /// on its way out regardless, which is documented behaviour rather than an
    /// accident, so what is left is a directory that is still there and that
    /// git no longer registers.
    ///
    /// That state is terminal on its own: `git worktree remove` answers "is
    /// not a working tree" however the path is spelled, `git worktree repair`
    /// declines because the `.git` file points at a record that is gone, and
    /// `git worktree add` refuses a path that exists. The record has to be put
    /// back, or the removal is not merely unfinished but impossible: every
    /// later attempt, explicit or not, would fail on a record that is gone.
    ///
    /// The obstacle is git-ignored, and it has to be. `remove_with_git` makes
    /// one non-forcing attempt, so anything `git status` can see makes git
    /// refuse at validation and delete nothing at all -- a different failure,
    /// covered by `a_dirty_worktree_survives_automatic_cleanup`, and one that
    /// never reaches the code this test is about. Ignored content is the only
    /// kind git will accept a removal over and then trip on, which makes it
    /// the only way in to the abandoned-partway state now that nothing forces
    /// its way past a refusal. Measured: with a non-ignored obstacle git says
    /// "contains modified or untracked files" and the checkout is untouched;
    /// with an ignored one it says "failed to delete: Permission denied",
    /// having already removed part of the checkout and its own record.
    ///
    /// Unix-only for the same reason as the tests above.
    #[cfg(unix)]
    #[tokio::test]
    async fn integration_a_removal_git_abandons_partway_can_still_be_retried() {
        use std::os::unix::fs::PermissionsExt;

        if crate::ipc::server::current_uid() == 0 {
            return; // root ignores the permission bits the obstacle is made of
        }

        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let info = mgr
            .create(repo_path, "task-abandoned")
            .await
            .expect("create failed");

        // Committed first, so that `blocked/` is ignored rather than
        // untracked by the time it exists: an untracked directory is content
        // the single non-forcing attempt refuses over, and refusing is not
        // the failure this test is about.
        std::fs::write(Path::new(&info.path).join(".gitignore"), "blocked/\n")
            .expect("write .gitignore");
        run_git(&info.path, &["add", ".gitignore"]);
        run_git(
            &info.path,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                "ignore the obstacle",
            ],
        );

        // Mode 0o500, not 0o000: git has to be able to see that there is
        // something inside to delete, so that it fails on the unlink rather
        // than refusing at validation the way the test above arranges.
        let blocked = Path::new(&info.path).join("blocked");
        std::fs::create_dir(&blocked).expect("create the obstacle");
        std::fs::write(blocked.join("held.txt"), b"held").expect("fill the obstacle");
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o500))
            .expect("close off the obstacle");

        assert!(
            run_git(&info.path, &["status", "--porcelain"]).is_empty(),
            "precondition: git must see nothing to refuse over, or the removal never gets far \
             enough to be abandoned partway"
        );

        let abandoned = mgr.remove(&info.path, repo_path).await;
        let registered = WorktreeManager::worktree_list_porcelain(repo_path).expect("git listing");
        // Reopened before asserting, so a failure still leaves a removable
        // temp dir behind.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755))
            .expect("reopen the obstacle");

        assert!(
            abandoned.is_err(),
            "a removal that could not delete the directory must report Err: {abandoned:?}"
        );
        assert!(
            Path::new(&info.path).exists(),
            "the worktree directory must still be there when removal is reported as failed"
        );
        assert!(
            registered.contains(&info.path),
            "git drops the record of a worktree whose removal it abandoned, and nothing can \
             remove a checkout git no longer registers: the record has to survive the attempt \
             or every later one is already impossible"
        );

        assert!(
            saved_records(repo_path).is_empty(),
            "a record that was put back has been renamed into place, not left lying beside it"
        );

        // What git deleted on its way out is not always nothing. It walks the
        // checkout in readdir order and stops where the obstacle is, so
        // whether it had already unlinked a tracked file by then is the
        // filesystem's answer and not this test's -- measured: none of them
        // here, some of them on the hosted ubuntu-22.04 runner, from the same
        // fixture and the same git invocation.
        //
        // A checkout missing tracked files is dirty, and `remove_with_git`
        // refuses dirt with "contains modified or untracked files" no matter
        // who made it, git included. That refusal is the contract working, not
        // the failure this test is about, and the removal is deliberately not
        // the thing that repairs it: putting the checkout back in order is the
        // user's own `git checkout`, which the acceptance journey for this
        // same abandonment spells out as the step before asking again. Running
        // it unconditionally is what lets the assertion below mean the one
        // thing this test exists for -- that the restored record makes a later
        // attempt possible at all -- under either ordering.
        run_git(&info.path, &["checkout", "--", "."]);
        assert!(
            run_git(&info.path, &["status", "--porcelain"]).is_empty(),
            "the checkout has to be back in order before the retry, or what the retry measures \
             is the ordinary dirty refusal and not the record that was put back"
        );

        mgr.remove(&info.path, repo_path)
            .await
            .expect("the attempt made once the obstacle is gone is the one that has to converge");
        assert!(
            !Path::new(&info.path).exists(),
            "the retry has to actually remove the worktree, not merely stop failing"
        );
        assert!(
            branch_exists(repo_path, "task-abandoned"),
            "and it must not take the branch, and the work on it, with it"
        );
        assert!(
            saved_records(repo_path).is_empty(),
            "and a removal that finished leaves nothing of its own behind"
        );
    }

    /// The other half of that: an attempt that could not put the record back.
    ///
    /// Putting it back is two filesystem operations against `.git`, and either
    /// can fail -- a read-only remount, a full disk. If the copy were thrown
    /// away when that happened the task would be stuck exactly as it was
    /// before this existed, because an attempt that finds no record has
    /// nothing of its own to copy. So the copy outlives an attempt that could
    /// not use it, under a name derived from the record rather than a fresh
    /// one, and the next attempt is the one that puts it back.
    ///
    /// The state is built here rather than provoked, because provoking it
    /// means taking write access to `.git` away between two operations inside
    /// a single call, and git would not have got as far as dropping the record
    /// without it either.
    #[cfg(unix)]
    #[tokio::test]
    async fn integration_a_record_an_attempt_could_not_put_back_is_used_by_the_next_one() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let info = mgr
            .create(repo_path, "task-stranded")
            .await
            .expect("create failed");

        let admin = admin_record_of(&info.path);
        let saved = Path::new(repo_path).join(".git").join(format!(
            "slashit-saved-worktree-record-{}",
            admin.file_name().unwrap().to_str().unwrap()
        ));
        copy_tree(&admin, &saved).expect("copy the record aside");
        std::fs::remove_dir_all(&admin).expect("drop the record the way git does");

        let listing = WorktreeManager::worktree_list_porcelain(repo_path).expect("git listing");
        assert!(
            !listing.contains(&info.path),
            "the worktree must start out unregistered, or this proves nothing"
        );

        let stuck = mgr.remove(&info.path, repo_path).await;
        assert!(
            stuck.is_err(),
            "nothing can remove a checkout git does not register: {stuck:?}"
        );
        let listing = WorktreeManager::worktree_list_porcelain(repo_path).expect("git listing");
        assert!(
            listing.contains(&info.path),
            "the copy left behind by an attempt that could not put the record back has to be              found by the next attempt, or the task is stuck for good"
        );

        mgr.remove(&info.path, repo_path)
            .await
            .expect("and with the record back, an ordinary removal has to converge");
        assert!(
            !Path::new(&info.path).exists(),
            "the worktree has to actually go"
        );
        assert!(
            branch_exists(repo_path, "task-stranded"),
            "and the branch has to stay"
        );
    }

    /// The record git keeps for the worktree at `worktree_path`.
    fn admin_record_of(worktree_path: &str) -> PathBuf {
        let gitlink = std::fs::read_to_string(Path::new(worktree_path).join(".git"))
            .expect("the worktree has a .git file");
        PathBuf::from(
            gitlink
                .strip_prefix("gitdir:")
                .expect("the .git file points at a record")
                .trim(),
        )
    }

    /// Copies of worktree records left beside git's own.
    fn saved_records(repo_path: &str) -> Vec<PathBuf> {
        std::fs::read_dir(Path::new(repo_path).join(".git"))
            .expect("read the git directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("slashit-saved-worktree-record-"))
            })
            .collect()
    }

    /// Cleaning up one task is an operation on that task's worktree.
    ///
    /// It used to end in a repository-wide `git worktree prune`, which takes
    /// no path and decides what is stale by whether it can read each
    /// registered worktree's `.git`. A worktree it may not look inside reads
    /// exactly like one that is gone, so a second worktree that is entirely
    /// present -- on an unmounted drive, or behind a permission, as here --
    /// lost the record it needs while its files sat untouched on disk. Nothing
    /// about it took part in the cleanup that destroyed it.
    ///
    /// The state that reached that prune is the one git leaves behind when it
    /// abandons a removal and the leftover directory is cleared afterwards:
    /// the path is gone and unregistered, so both removes answer "is not a
    /// working tree" and there is nothing of this task's left to clear.
    ///
    /// Unix-only for the same reason as the tests above.
    #[cfg(unix)]
    #[tokio::test]
    async fn integration_a_cleanup_leaves_another_worktree_registered() {
        use std::os::unix::fs::PermissionsExt;

        if crate::ipc::server::current_uid() == 0 {
            return; // root ignores the permission bits this test relies on
        }

        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let bystander = mgr
            .create(repo_path, "task-bystander")
            .await
            .expect("create failed");
        let cleaned = mgr
            .create(repo_path, "task-cleaned")
            .await
            .expect("create failed");

        // Exactly what git leaves when it abandons a removal and the leftover
        // is cleared away afterwards: no directory, and no record either.
        forget_worktree_record(repo_path, &cleaned.path);
        std::fs::remove_dir_all(&cleaned.path).expect("clear the leftover directory");

        // Unreadable, not absent: everything it holds is still on disk.
        let restore = std::fs::metadata(&bystander.path)
            .expect("read the bystander's mode")
            .permissions()
            .mode();
        std::fs::set_permissions(&bystander.path, std::fs::Permissions::from_mode(0o000))
            .expect("close off the bystander");

        let converged = mgr.remove(&cleaned.path, repo_path).await;
        let registered = WorktreeManager::worktree_list_porcelain(repo_path).expect("git listing");
        std::fs::set_permissions(&bystander.path, std::fs::Permissions::from_mode(restore))
            .expect("reopen the bystander");

        assert!(
            converged.is_ok(),
            "a worktree that is not there is a converged removal: {converged:?}"
        );
        assert!(
            registered.contains(&bystander.path),
            "cleaning up {} destroyed the record of {}, which took no part in it -- the \
             directory is still there, so nothing can remove it and nothing can check its \
             branch out again",
            cleaned.path,
            bystander.path
        );
        assert!(
            Path::new(&bystander.path).join("README.md").is_file(),
            "and the bystander must still hold what it held"
        );
    }

    /// Delete the record git keeps for the worktree at `worktree_path`,
    /// leaving the checkout unregistered exactly as an abandoned removal does.
    fn forget_worktree_record(repo_path: &str, worktree_path: &str) {
        let admin = admin_record_of(worktree_path);
        assert!(
            admin.starts_with(Path::new(repo_path).join(".git").join("worktrees")),
            "the record must be this repository's own, found {}",
            admin.display()
        );
        std::fs::remove_dir_all(&admin).expect("delete the record");
    }

    #[tokio::test]
    async fn integration_remove_nonexistent_does_not_panic() {
        let mgr = test_manager();
        // Removing a non-existent worktree should not panic (may return Err, that is fine).
        let _ = mgr.remove("/tmp/slashit_no_such_wt", "/tmp").await;
    }

    /// `WorktreeManager` no longer owns diff computation itself --
    /// `crate::worktree::task_diff` is the one canonical implementation, used
    /// by both the UI and the AI reviewer (see `worktree::diff`'s own test
    /// suite for its unit coverage). These integration tests confirm the
    /// canonical diff produces a correct, real result against a worktree
    /// this manager actually created, not just a bare temp repo.
    #[tokio::test]
    async fn integration_task_diff_clean_worktree() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let base = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_path)
            .output()
            .unwrap();
        let base = String::from_utf8_lossy(&base.stdout).trim().to_string();
        let info = mgr.create(repo_path, "diff-clean").await.expect("create failed");

        let diff = crate::worktree::task_diff(&info.path, Some(&base)).await.expect("task_diff failed");
        assert!(diff.patch.is_empty(), "expected empty diff on clean worktree, got: {}", diff.patch);
    }

    #[tokio::test]
    async fn integration_task_diff_with_changes() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let base = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_path)
            .output()
            .unwrap();
        let base = String::from_utf8_lossy(&base.stdout).trim().to_string();
        let info = mgr.create(repo_path, "diff-change").await.expect("create failed");

        // Make a change and commit it in the worktree
        std::fs::write(Path::new(&info.path).join("new_file.txt"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "new_file.txt"])
            .current_dir(&info.path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add file"])
            .current_dir(&info.path)
            .output()
            .unwrap();

        let diff = crate::worktree::task_diff(&info.path, Some(&base)).await.expect("task_diff failed");
        assert!(diff.patch.contains("new_file.txt"), "diff should mention the new file");
        assert!(diff.stat.contains("new_file.txt"), "stat should mention the new file");
    }

    #[tokio::test]
    async fn integration_task_diff_unknown_boundary_for_worktree_with_no_recorded_base() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let info = mgr.create(repo_path, "stat-branch").await.expect("create failed");

        let result = crate::worktree::task_diff(&info.path, None).await;
        assert_eq!(result, Err(crate::worktree::TaskDiffError::UnknownBoundary));
    }

    #[tokio::test]
    async fn integration_create_stacked_branch() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();

        // Create a base branch via git (not as a worktree, just a branch)
        std::process::Command::new("git")
            .args(["branch", "base-branch"])
            .current_dir(repo_path)
            .output()
            .expect("git branch failed");

        // Create a stacked branch on top of the base. The git path creates
        // `stacked-branch` at `base-branch`'s tip, then attaches a worktree to the already-created branch without
        // `-b` (`git_worktree_add(..., create_branch: false)`), so it must
        // succeed.
        let stacked = mgr
            .create_stacked_branch(repo_path, "stacked-branch", "base-branch")
            .await
            .expect("git-only stacked branch creation from a non-main base must succeed").info;

        assert!(Path::new(&stacked.path).exists());
        assert_eq!(stacked.branch, "stacked-branch");
    }

    #[tokio::test]
    async fn integration_create_empty_branch_name() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        // Git rejects empty branch names, so this should fail.
        let result = mgr.create(repo_path, "").await;
        assert!(result.is_err(), "creating worktree with empty branch should fail");
    }

    #[tokio::test]
    async fn integration_create_branch_with_special_chars() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        // Spaces in branch names are invalid in git
        let result = mgr.create(repo_path, "branch with spaces").await;
        assert!(result.is_err(), "branch with spaces should fail");
    }

    #[tokio::test]
    async fn integration_reattach_existing_branch() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();

        // Create a worktree, then remove the worktree (keep the branch)
        let info = mgr.create(repo_path, "reattach-me").await.expect("create failed");
        let wt_path = info.path.clone();

        // Remove worktree only (not the branch)
        std::process::Command::new("git")
            .args(["worktree", "remove", "--force", &wt_path])
            .current_dir(repo_path)
            .output()
            .expect("worktree remove failed");

        // Reattach to the existing branch
        let reattached = mgr.reattach(repo_path, "reattach-me").await.expect("reattach failed");
        assert!(Path::new(&reattached.path).exists(), "reattached worktree should exist");
        assert_eq!(reattached.branch, "reattach-me");
    }

    /// Every ref in the repository with the commit it names, so a test can
    /// show a refused operation left all of them exactly as they were.
    fn all_refs(repo_path: &str) -> String {
        run_git(repo_path, &["for-each-ref", "--format=%(refname) %(objectname)"])
    }

    /// The worktrees git has registered, the primary checkout included.
    fn registered_worktrees(repo_path: &str) -> Vec<String> {
        run_git(repo_path, &["worktree", "list", "--porcelain"])
            .lines()
            .filter_map(|line| line.strip_prefix("worktree ").map(str::to_string))
            .collect()
    }

    /// A repository whose `main` has moved one commit past an unrelated
    /// branch `victim`, so resetting `victim` to `HEAD` is observable.
    fn repo_with_victim_branch() -> tempfile::TempDir {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        run_git(repo_path, &["branch", "victim"]);
        run_git(repo_path, &["commit", "--allow-empty", "-m", "second"]);
        tmp
    }

    /// `task.branch_name` is read back from `tasks.toml`, which a board kept
    /// in the project takes from whatever the last commit wrote. Reattaching
    /// used to hand it to `git worktree add <dest> <branch>` as it was, and
    /// `-Bvictim` there is the option that force-resets `victim` to `HEAD`.
    #[tokio::test]
    async fn reattach_refuses_an_option_shaped_branch_before_git_sees_it() {
        let tmp = repo_with_victim_branch();
        let repo_path = tmp.path().to_str().unwrap();
        let refs_before = all_refs(repo_path);

        let mgr = test_manager();
        let result = mgr.reattach(repo_path, "-Bvictim").await;

        assert_eq!(all_refs(repo_path), refs_before, "no ref may move, `victim` above all");
        assert!(result.is_err(), "a branch shaped like an option must be refused");
        assert_eq!(registered_worktrees(repo_path).len(), 1, "no worktree may be created");
        assert_eq!(run_git(repo_path, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert!(!mgr.managed_path(repo_path, "-Bvictim").exists());
    }

    /// A revision is not a branch: `git worktree add <dest> HEAD~1` checks
    /// out a detached worktree at whatever commit that names.
    #[tokio::test]
    async fn reattach_refuses_a_revision_shaped_branch() {
        let tmp = repo_with_victim_branch();
        let repo_path = tmp.path().to_str().unwrap();
        let refs_before = all_refs(repo_path);

        let mgr = test_manager();
        let result = mgr.reattach(repo_path, "HEAD~1").await;

        assert!(result.is_err(), "a revision must be refused where a branch is expected");
        assert_eq!(registered_worktrees(repo_path).len(), 1, "no detached worktree may appear");
        assert_eq!(all_refs(repo_path), refs_before);
    }

    /// The dependency's `branch_name` is persisted the same way as the
    /// task's own. On the git-only stacked path it went to
    /// `git branch <new> <dependency>`, where `-m` renames the branch the
    /// user has checked out in their own repository.
    #[tokio::test]
    async fn stacked_git_fallback_refuses_an_option_shaped_dependency() {
        let tmp = repo_with_victim_branch();
        let repo_path = tmp.path().to_str().unwrap();
        let refs_before = all_refs(repo_path);

        let mgr = test_manager();
        let result = mgr.create_stacked_branch(repo_path, "task-1234abcd", "-m").await;

        assert_eq!(run_git(repo_path, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(all_refs(repo_path), refs_before, "no ref may be created, moved or renamed");
        assert!(result.is_err(), "a dependency shaped like an option must be refused");
        assert!(!branch_exists(repo_path, "task-1234abcd"));
        assert_eq!(registered_worktrees(repo_path).len(), 1);
    }

    /// A revision-shaped dependency, or a poisoned name for the new branch,
    /// is refused before any branch is created.
    #[tokio::test]
    async fn stacked_git_fallback_refuses_revision_or_option_shaped_names() {
        let tmp = repo_with_victim_branch();
        let repo_path = tmp.path().to_str().unwrap();
        let refs_before = all_refs(repo_path);

        let mgr = test_manager();
        for (branch, after) in [
            ("task-1234abcd", "HEAD~1"),
            ("task-1234abcd", "victim@{1}"),
            ("-Bvictim", "main"),
            ("HEAD~1", "main"),
        ] {
            let result = mgr.create_stacked_branch(repo_path, branch, after).await;
            assert!(result.is_err(), "{branch:?} on {after:?} must be refused");
            assert_eq!(all_refs(repo_path), refs_before, "{branch:?} on {after:?}");
            assert_eq!(registered_worktrees(repo_path).len(), 1, "{branch:?} on {after:?}");
        }
    }

    /// The git stacked path used to ignore `git branch`'s exit status. When
    /// the new branch could not be created from the dependency -- here
    /// because a branch of that name already exists elsewhere -- it went on
    /// to attach a worktree to whatever that name already was, and reported
    /// a stacked branch that was not stacked on anything. An existing branch
    /// that does not contain the dependency's work is refused, named, and
    /// left where it is.
    #[tokio::test]
    async fn stacked_git_fallback_reports_a_branch_it_could_not_create() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        run_git(repo_path, &["branch", "task-1234abcd"]);
        run_git(repo_path, &["checkout", "-q", "-b", "task-5678abcd"]);
        run_git(repo_path, &["commit", "--allow-empty", "-m", "dependency work"]);
        run_git(repo_path, &["checkout", "-q", "main"]);
        let refs_before = all_refs(repo_path);

        let mgr = test_manager();
        let result = mgr
            .create_stacked_branch(repo_path, "task-1234abcd", "task-5678abcd")
            .await;

        let error = result.err().expect("a branch that could not be created must not be reported as stacked");
        assert!(
            error.contains("task-1234abcd") && error.contains("task-5678abcd"),
            "the refusal has to name both branches: {error}"
        );
        assert_eq!(all_refs(repo_path), refs_before);
        assert_eq!(registered_worktrees(repo_path).len(), 1);
    }

    #[test]
    fn git_worktree_add_args_put_every_positional_after_the_terminator() {
        assert_eq!(
            WorktreeManager::git_worktree_add_args("/wt/dest", "task-1234abcd", true),
            ["worktree", "add", "-b", "task-1234abcd", "--", "/wt/dest"]
        );
        assert_eq!(
            WorktreeManager::git_worktree_add_args("/wt/dest", "task-1234abcd", false),
            ["worktree", "add", "--", "/wt/dest", "task-1234abcd"]
        );
    }

    /// The `--` holds on its own, without the check in front of it: git is
    /// handed `-Bvictim` directly and still reads it as a (missing) branch,
    /// not as the option that resets `victim`.
    #[tokio::test]
    async fn git_worktree_add_reads_an_option_shaped_branch_as_a_branch() {
        let tmp = repo_with_victim_branch();
        let repo_path = tmp.path().to_str().unwrap();
        let refs_before = all_refs(repo_path);

        let mgr = test_manager();
        let dest = mgr.managed_path(repo_path, "unchecked");
        let result = mgr.git_worktree_add(repo_path, &dest, "-Bvictim", false).await;

        assert_eq!(all_refs(repo_path), refs_before, "`victim` must not be reset");
        assert!(result.is_err());
        assert_eq!(registered_worktrees(repo_path).len(), 1);
    }

    /// The git stacked path creates its branch at the exact commit
    /// it resolved, never over an existing branch, and not at a branch that
    /// happens to be named by that commit's hex, which `git branch` would
    /// prefer.
    #[tokio::test]
    async fn create_branch_at_takes_the_commit_literally_and_never_overwrites() {
        let tmp = repo_with_victim_branch();
        let repo_path = tmp.path().to_str().unwrap();
        let commit = run_git(repo_path, &["rev-parse", "victim"]);
        run_git(repo_path, &["branch", "--", &commit, "main"]);

        WorktreeManager::create_branch_at(repo_path, "task-1234abcd", &commit)
            .await
            .expect("create");
        assert_eq!(run_git(repo_path, &["rev-parse", "refs/heads/task-1234abcd"]), commit);

        let refs_before = all_refs(repo_path);
        let main = run_git(repo_path, &["rev-parse", "main"]);
        let result = WorktreeManager::create_branch_at(repo_path, "task-1234abcd", &main).await;
        assert!(result.is_err(), "an existing branch must not be moved");
        assert_eq!(all_refs(repo_path), refs_before);
    }

    /// A repository in which the commit before `main`'s tip is also named by
    /// things that are not local branches but have a branch name's shape:
    /// its full object ID, and `FETCH_HEAD` and `ORIG_HEAD` as a fetch or a
    /// reset leaves them. Returns the repository and that commit.
    fn repo_with_branch_shaped_revisions() -> (tempfile::TempDir, String) {
        let tmp = repo_with_victim_branch();
        let repo_path = tmp.path().to_str().unwrap();
        let commit = run_git(repo_path, &["rev-parse", "HEAD~1"]);
        run_git(repo_path, &["update-ref", "ORIG_HEAD", &commit]);
        std::fs::write(
            tmp.path().join(".git/FETCH_HEAD"),
            format!("{commit}\t\tbranch 'main' of elsewhere\n"),
        )
        .unwrap();
        for name in [commit.as_str(), "FETCH_HEAD", "ORIG_HEAD"] {
            assert_eq!(run_git(repo_path, &["rev-parse", name]), commit, "{name} must resolve");
        }
        (tmp, commit)
    }

    /// `checked_task_branch` judges a name by its shape, and a full object
    /// ID, `FETCH_HEAD` and `ORIG_HEAD` all pass it. With no local branch of
    /// that name, `git worktree add <dest> <name>` still succeeds, with a
    /// detached `HEAD` at whatever the name resolves to, so the agent's
    /// commits would land on no branch at all.
    #[tokio::test]
    async fn reattach_refuses_a_name_that_is_not_a_local_branch() {
        let (tmp, commit) = repo_with_branch_shaped_revisions();
        let repo_path = tmp.path().to_str().unwrap();
        let refs_before = all_refs(repo_path);

        let mgr = test_manager();
        for name in [commit.as_str(), "FETCH_HEAD", "ORIG_HEAD"] {
            let result = mgr.reattach(repo_path, name).await;
            assert!(result.is_err(), "{name:?} is not a local branch and must be refused");
            assert_eq!(registered_worktrees(repo_path).len(), 1, "{name:?}: no detached worktree");
            assert!(!mgr.managed_path(repo_path, name).exists(), "{name:?}");
            assert_eq!(all_refs(repo_path), refs_before, "{name:?}");
        }
    }

    /// The same names as a dependency: `git branch <new> <name>` takes them
    /// as a start point, and would stack the task on a commit that is not
    /// the dependency's branch.
    #[tokio::test]
    async fn stacked_git_fallback_refuses_a_dependency_that_is_not_a_local_branch() {
        let (tmp, commit) = repo_with_branch_shaped_revisions();
        let repo_path = tmp.path().to_str().unwrap();
        let refs_before = all_refs(repo_path);

        let mgr = test_manager();
        for name in [commit.as_str(), "FETCH_HEAD", "ORIG_HEAD"] {
            let result = mgr.create_stacked_branch(repo_path, "task-1234abcd", name).await;
            assert!(result.is_err(), "{name:?} is not a local branch and must be refused");
            assert!(!branch_exists(repo_path, "task-1234abcd"), "{name:?}");
            assert_eq!(all_refs(repo_path), refs_before, "{name:?}");
            assert_eq!(registered_worktrees(repo_path).len(), 1, "{name:?}");
        }
    }

    /// The git-only stacked path creates the branch, then attaches a
    /// worktree to it. When the attach failed -- here because something is
    /// already in the way at the worktree's path -- the branch stayed, and
    /// every retry then failed on it: the stacked path because the branch
    /// already exists, and the executor's plain `create` for the same reason.
    #[tokio::test]
    async fn stacked_git_fallback_that_cannot_attach_leaves_no_branch_behind() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        run_git(repo_path, &["checkout", "-q", "-b", "task-5678abcd"]);
        run_git(repo_path, &["commit", "--allow-empty", "-m", "dependency work"]);
        run_git(repo_path, &["checkout", "-q", "main"]);
        let refs_before = all_refs(repo_path);

        let mgr = test_manager();
        let obstacle = mgr.managed_path(repo_path, "task-1234abcd");
        std::fs::create_dir_all(&obstacle).unwrap();
        std::fs::write(obstacle.join("in-the-way"), "x").unwrap();

        let result = mgr
            .create_stacked_branch(repo_path, "task-1234abcd", "task-5678abcd")
            .await;
        assert!(result.is_err(), "the worktree could not be attached");
        assert!(!branch_exists(repo_path, "task-1234abcd"), "the branch it created must go");
        assert_eq!(all_refs(repo_path), refs_before);
        assert_eq!(registered_worktrees(repo_path).len(), 1);

        std::fs::remove_dir_all(&obstacle).unwrap();
        let stacked = mgr
            .create_stacked_branch(repo_path, "task-1234abcd", "task-5678abcd")
            .await
            .expect("a retry once the obstacle is gone").info;
        assert_eq!(run_git(&stacked.path, &["symbolic-ref", "--short", "HEAD"]), "task-1234abcd");
        assert_eq!(
            run_git(&stacked.path, &["rev-parse", "HEAD"]),
            run_git(repo_path, &["rev-parse", "task-5678abcd"])
        );
    }

    /// `git worktree add` can report failure after the worktree is already
    /// registered and checked out, as it does when a `post-checkout` hook
    /// fails. Deleting the branch then would leave that worktree on a branch
    /// that no longer exists, so the branch is kept.
    #[cfg(unix)]
    #[tokio::test]
    async fn stacked_git_fallback_keeps_a_branch_a_failed_attach_checked_out() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        run_git(repo_path, &["branch", "task-5678abcd"]);
        let hook = tmp.path().join(".git/hooks/post-checkout");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mgr = test_manager();
        let result = mgr
            .create_stacked_branch(repo_path, "task-1234abcd", "task-5678abcd")
            .await;

        assert!(result.is_err(), "git reported the attach as failed");
        let porcelain = run_git(repo_path, &["worktree", "list", "--porcelain"]);
        let registered = WorktreeManager::registration_for_branch(&porcelain, "task-1234abcd")
            .expect("git registered the worktree before the hook failed");
        assert!(branch_exists(repo_path, "task-1234abcd"), "the checked-out branch must be kept");
        assert_eq!(
            run_git(&registered, &["rev-parse", "HEAD"]),
            run_git(repo_path, &["rev-parse", "task-5678abcd"])
        );
    }

    /// What a failed checkout hook leaves behind -- the branch, and the
    /// worktree git registered before the hook ran -- is picked up by the
    /// next attempt, rather than failing it, and every one after, on
    /// "already exists".
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stacked_attempt_a_failed_hook_left_behind_is_picked_up_on_retry() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        run_git(repo_path, &["checkout", "-q", "-b", "task-5678abcd"]);
        run_git(repo_path, &["commit", "--allow-empty", "-m", "dependency work"]);
        run_git(repo_path, &["checkout", "-q", "main"]);
        let dependency_tip = run_git(repo_path, &["rev-parse", "task-5678abcd"]);
        let hook = tmp.path().join(".git/hooks/post-checkout");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mgr = test_manager();
        let first = mgr
            .create_stacked_branch(repo_path, "task-1234abcd", "task-5678abcd")
            .await;
        assert!(first.is_err(), "the hook fails the first attempt");
        std::fs::remove_file(&hook).unwrap();

        let retried = mgr
            .create_stacked_branch(repo_path, "task-1234abcd", "task-5678abcd")
            .await
            .expect("the retry picks up what the first attempt left");
        assert!(retried.resumed);
        assert_eq!(retried.dependency_tip, dependency_tip);
        let retried = retried.info;
        assert_eq!(run_git(&retried.path, &["symbolic-ref", "--short", "HEAD"]), "task-1234abcd");
        assert_eq!(run_git(&retried.path, &["rev-parse", "HEAD"]), dependency_tip);
        assert_eq!(run_git(repo_path, &["rev-parse", "refs/heads/task-1234abcd"]), dependency_tip);
        assert_eq!(registered_worktrees(repo_path).len(), 2, "no second worktree for the branch");
    }

    /// A branch an earlier attempt created and recorded nowhere -- the start
    /// died before the task saved it, and its worktree is gone -- is
    /// attached to again as it is, work on top of the dependency included.
    #[tokio::test]
    async fn a_stacked_branch_left_without_a_worktree_is_attached_on_retry() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        run_git(repo_path, &["checkout", "-q", "-b", "task-5678abcd"]);
        run_git(repo_path, &["commit", "--allow-empty", "-m", "dependency work"]);
        run_git(repo_path, &["checkout", "-q", "-b", "task-1234abcd"]);
        run_git(repo_path, &["commit", "--allow-empty", "-m", "the task's own work"]);
        run_git(repo_path, &["checkout", "-q", "main"]);
        let task_tip = run_git(repo_path, &["rev-parse", "task-1234abcd"]);
        let refs_before = all_refs(repo_path);

        let mgr = test_manager();
        let attached = mgr
            .create_stacked_branch(repo_path, "task-1234abcd", "task-5678abcd")
            .await
            .expect("an existing branch holding the dependency's work is attached");
        assert!(attached.resumed);
        assert_eq!(
            attached.dependency_tip,
            run_git(repo_path, &["rev-parse", "task-5678abcd"]),
            "the stack is reported against the dependency, not the branch's own tip"
        );
        let attached = attached.info;
        assert_eq!(run_git(&attached.path, &["symbolic-ref", "--short", "HEAD"]), "task-1234abcd");
        assert_eq!(run_git(&attached.path, &["rev-parse", "HEAD"]), task_tip);
        assert_eq!(all_refs(repo_path), refs_before, "the branch is attached, never moved");
    }

    /// The cleanup deletes the branch only while it still names the commit
    /// it was created at. Anything that moved it in between made it someone
    /// else's, and it is kept.
    #[tokio::test]
    async fn discarding_a_created_branch_keeps_it_once_it_has_moved() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        let created_at = run_git(repo_path, &["rev-parse", "HEAD"]);
        run_git(repo_path, &["branch", "task-1234abcd"]);
        run_git(repo_path, &["commit", "--allow-empty", "-m", "moved"]);
        run_git(repo_path, &["branch", "-f", "task-1234abcd", "main"]);
        let moved_to = run_git(repo_path, &["rev-parse", "task-1234abcd"]);

        let result =
            WorktreeManager::discard_created_branch(repo_path, "task-1234abcd", &created_at).await;
        assert!(result.is_err(), "a branch that was kept must be reported");
        assert_eq!(run_git(repo_path, &["rev-parse", "refs/heads/task-1234abcd"]), moved_to);

        WorktreeManager::discard_created_branch(repo_path, "task-1234abcd", &moved_to)
            .await
            .expect("a branch still where it was created is deleted");
        assert!(!branch_exists(repo_path, "task-1234abcd"));
    }

    /// `task-<full uuid>`, which versions before the 8-hex prefix wrote, is
    /// a local branch like any other and still reattaches and stacks.
    #[tokio::test]
    async fn legacy_full_uuid_task_branches_reattach_and_stack_through_git() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        let legacy = format!("task-{}", Uuid::new_v4());
        let newer = format!("task-{}", Uuid::new_v4());
        run_git(repo_path, &["branch", &legacy]);
        let mgr = test_manager();

        let reattached = mgr.reattach(repo_path, &legacy).await.expect("reattach");
        assert_eq!(run_git(&reattached.path, &["symbolic-ref", "--short", "HEAD"]), legacy);
        let dependency_tip = commit_work(&reattached.path, "dependency.txt");
        run_git(repo_path, &["worktree", "remove", &reattached.path]);

        let stacked = mgr
            .create_stacked_branch(repo_path, &newer, &legacy)
            .await
            .expect("stacked").info;
        assert_eq!(run_git(&stacked.path, &["symbolic-ref", "--short", "HEAD"]), newer);
        assert_eq!(run_git(&stacked.path, &["rev-parse", "HEAD"]), dependency_tip);
    }

    /// The names SlashIt generates keep working on every git-only path.
    #[tokio::test]
    async fn generated_task_branches_create_reattach_and_stack_through_git() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        let mgr = test_manager();

        let created = mgr.create(repo_path, "task-1234abcd").await.expect("create");
        assert_eq!(run_git(&created.path, &["symbolic-ref", "--short", "HEAD"]), "task-1234abcd");
        let dependency_tip = commit_work(&created.path, "dependency.txt");

        run_git(repo_path, &["worktree", "remove", &created.path]);
        let reattached = mgr.reattach(repo_path, "task-1234abcd").await.expect("reattach");
        assert_eq!(run_git(&reattached.path, &["symbolic-ref", "--short", "HEAD"]), "task-1234abcd");
        run_git(repo_path, &["worktree", "remove", &reattached.path]);

        let stacked = mgr
            .create_stacked_branch(repo_path, "task-5678abcd", "task-1234abcd")
            .await
            .expect("stacked").info;
        assert_eq!(run_git(&stacked.path, &["symbolic-ref", "--short", "HEAD"]), "task-5678abcd");
        assert_eq!(run_git(&stacked.path, &["rev-parse", "HEAD"]), dependency_tip);
        assert_eq!(run_git(repo_path, &["symbolic-ref", "--short", "HEAD"]), "main");
    }

    /// A `git-spice` executable installed at the front of `PATH` for as long
    /// as this value lives, recording every invocation in `log`.
    ///
    /// Holds [`crate::test_helpers::PATH_LOCK`] itself, so `PATH` is restored
    /// before the lock is released no matter how the test ends.
    #[cfg(unix)]
    struct FakeGitSpice {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        log: PathBuf,
        saved_path: Option<std::ffi::OsString>,
    }

    #[cfg(unix)]
    impl FakeGitSpice {
        /// `body` runs after the invocation has been logged, in whatever
        /// directory the caller spawned it from.
        async fn install(body: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let lock = crate::test_helpers::PATH_LOCK.lock().await;
            let dir = tempfile::tempdir().expect("tempdir");
            let log = dir.path().join("git-spice.log");
            let program = dir.path().join("git-spice");
            std::fs::write(
                &program,
                format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log:?}\n{body}\n"),
            )
            .expect("write fake git-spice");
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake git-spice");

            let saved_path = std::env::var_os("PATH");
            let mut entries = vec![dir.path().to_path_buf()];
            if let Some(path) = &saved_path {
                entries.extend(std::env::split_paths(path));
            }
            let new_path = std::env::join_paths(entries).expect("join PATH");
            // Safety: serialized via PATH_LOCK, held by this value and
            // released only after `Drop` has restored PATH.
            unsafe {
                std::env::set_var("PATH", new_path);
            }
            FakeGitSpice { _lock: lock, _dir: dir, log, saved_path }
        }

        fn invocations(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    #[cfg(unix)]
    impl Drop for FakeGitSpice {
        fn drop(&mut self) {
            // Safety: PATH_LOCK is still held; `_lock` drops after this.
            unsafe {
                match &self.saved_path {
                    Some(path) => std::env::set_var("PATH", path),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }

    /// Stack `stacked` on a `dependency` branch one commit ahead of `main`,
    /// with a manager constructed while `fake` is on `PATH`, and check the
    /// result is exactly what the git-only path produces: the new branch at
    /// the dependency's tip, a worktree attached to it, and the primary
    /// checkout untouched.
    #[cfg(unix)]
    async fn assert_stacks_on_the_dependency_tip_with(fake: &FakeGitSpice) {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        run_git(repo_path, &["branch", "dependency"]);
        let dependency = fixture_worktree(repo_path, "dependency");
        let dependency_tip = commit_work(&dependency, "dependency.txt");
        run_git(repo_path, &["worktree", "remove", &dependency]);
        let main_tip = run_git(repo_path, &["rev-parse", "main"]);
        assert_ne!(dependency_tip, main_tip, "the fixture must tell the two bases apart");

        let mgr = WorktreeManager::new(test_paths(), WorktreePlacement::Managed);
        let stacked = mgr
            .create_stacked_branch(repo_path, "stacked", "dependency")
            .await
            .expect("stacking on a local branch must succeed whatever is on PATH").info;

        assert_eq!(fake.invocations(), "", "git-spice must never be run");
        assert_eq!(run_git(repo_path, &["rev-parse", "refs/heads/stacked"]), dependency_tip);
        assert_eq!(run_git(&stacked.path, &["symbolic-ref", "--short", "HEAD"]), "stacked");
        assert_eq!(run_git(&stacked.path, &["rev-parse", "HEAD"]), dependency_tip);
        assert!(
            registered_worktrees(repo_path)
                .iter()
                .any(|w| Path::new(w) == Path::new(&stacked.path)),
            "the stacked worktree must be registered with git"
        );
        assert_eq!(run_git(repo_path, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(run_git(repo_path, &["rev-parse", "main"]), main_tip);
    }

    /// A worktree for an existing `branch`, made with git directly so the
    /// fixture does not depend on the manager under test.
    fn fixture_worktree(repo_path: &str, branch: &str) -> String {
        let path = std::env::temp_dir().join(format!("slashit-wt-fixture-{}", Uuid::new_v4()));
        let path = path.to_str().unwrap().to_string();
        run_git(repo_path, &["worktree", "add", "--", &path, branch]);
        path
    }

    /// A `git-spice` that accepts the call behaves the way `branch create`
    /// does: it creates the branch from whatever the primary checkout has
    /// checked out, and switches that checkout to it. Stacking must not be
    /// handed to it, or the task starts from `main` instead of its
    /// dependency and the user's own checkout is switched under them.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_git_spice_that_succeeds_does_not_change_the_stacked_branch() {
        let fake = FakeGitSpice::install("exec git checkout -q -b \"$3\"").await;
        assert_stacks_on_the_dependency_tip_with(&fake).await;
    }

    /// git-spice 0.29 refuses `--insert-after` outright (`unknown flag
    /// --insert-after`, exit 1). Its presence on `PATH` must not turn a
    /// stack the git path can build into an error.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_git_spice_that_refuses_does_not_change_the_stacked_branch() {
        let fake = FakeGitSpice::install(
            "echo 'FTL git-spice: unknown flag --insert-after' >&2\nexit 1",
        )
        .await;
        assert_stacks_on_the_dependency_tip_with(&fake).await;
    }

    /// `local_branch_exists` says "no" only when git answered: a missing
    /// branch is `Ok(false)`, but an invalid name or a directory git cannot
    /// read as a repository is an error, never an absence.
    #[tokio::test]
    async fn local_branch_exists_separates_absent_from_unanswerable() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        assert_eq!(WorktreeManager::local_branch_exists(repo_path, "main").await, Ok(true));
        assert_eq!(WorktreeManager::local_branch_exists(repo_path, "gone").await, Ok(false));
        assert!(WorktreeManager::local_branch_exists(repo_path, "-Bvictim").await.is_err());

        // Present but broken is not absent: a ref naming a missing object,
        // which `rev-parse --verify --quiet` reports exactly like no ref at
        // all, and a ref with unreadable contents.
        std::fs::write(
            tmp.path().join(".git/refs/heads/missing-object"),
            "1234567890123456789012345678901234567890\n",
        )
        .unwrap();
        assert_eq!(
            WorktreeManager::local_branch_exists(repo_path, "missing-object").await,
            Ok(true)
        );
        std::fs::write(tmp.path().join(".git/refs/heads/garbage"), "garbage\n").unwrap();
        assert!(WorktreeManager::local_branch_exists(repo_path, "garbage").await.is_err());
        // Refs below `refs/heads/<name>/` are not the branch `<name>`.
        run_git(repo_path, &["branch", "nested/child"]);
        assert_eq!(WorktreeManager::local_branch_exists(repo_path, "nested").await, Ok(false));

        let not_a_repo = tempfile::tempdir().expect("tempdir");
        assert!(
            WorktreeManager::local_branch_exists(not_a_repo.path().to_str().unwrap(), "main")
                .await
                .is_err()
        );
    }
}
