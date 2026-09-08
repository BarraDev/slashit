use crate::config::paths::{AppPaths, ProjectKey, WorktreePlacement};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

pub struct WorktreeManager {
    wt_available: bool,
    pub gs_available: bool,
    paths: Arc<AppPaths>,
    placement: WorktreePlacement,
}

pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
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
    pub fn new(paths: Arc<AppPaths>, placement: WorktreePlacement) -> Self {
        let wt_available = std::process::Command::new("which")
            .arg("wt")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        let gs_available = std::process::Command::new("which")
            .arg("git-spice")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if gs_available {
            println!("SlashIt: git-spice detected — available for stacked PRs");
        }

        let manager = Self {
            wt_available,
            gs_available,
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
            .or_else(|| Self::adopt_any_registered(branch, porcelain))
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
    pub fn adopt_any_registered(branch: &str, porcelain: &str) -> Option<String> {
        let registered = PathBuf::from(Self::worktree_for_branch(porcelain, branch)?);
        registered
            .is_dir()
            .then(|| registered.to_string_lossy().to_string())
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
    pub async fn create(&self, repo_path: &str, branch: &str) -> Result<WorktreeInfo, String> {
        if let Some(existing) = self.adoptable_path_live(repo_path, branch).await {
            return Ok(WorktreeInfo {
                path: existing.to_string_lossy().to_string(),
                branch: branch.to_string(),
            });
        }
        if self.delegates_to_wt() {
            self.create_with_wt(repo_path, branch).await
        } else {
            self.create_with_git(repo_path, branch).await
        }
    }

    /// Reattach to an existing branch (no -c flag). Used when re-queuing a task
    /// that already has a branch from a previous execution.
    pub async fn reattach(&self, repo_path: &str, branch: &str) -> Result<WorktreeInfo, String> {
        if let Some(existing) = self.adoptable_path_live(repo_path, branch).await {
            return Ok(WorktreeInfo {
                path: existing.to_string_lossy().to_string(),
                branch: branch.to_string(),
            });
        }

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

        let mut args = vec!["worktree", "add", dest];
        if create_branch {
            args.push("-b");
        }
        args.push(branch);

        let output = tokio::process::Command::new("git")
            .args(&args)
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

    /// Create a branch stacked on top of another branch.
    /// Uses git-spice when available, falls back to wt --base.
    pub async fn create_stacked_branch(
        &self,
        repo_path: &str,
        branch: &str,
        after_branch: &str,
    ) -> Result<WorktreeInfo, String> {
        if self.gs_available {
            // Use git-spice to create stacked branch
            let output = tokio::process::Command::new("git-spice")
                .args(["branch", "create", branch, "--insert-after", after_branch])
                .current_dir(repo_path)
                .output()
                .await
                .map_err(|e| format!("git-spice branch create failed: {}", e))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("git-spice branch create failed: {}", stderr));
            }

            // Now create a worktree for this branch
            if self.delegates_to_wt() {
                let output = tokio::process::Command::new("wt")
                    .args(["switch", branch, "--no-cd", "-y", "--no-verify"])
                    .current_dir(repo_path)
                    .output()
                    .await
                    .map_err(|e| format!("wt switch failed: {}", e))?;
                if output.status.success() {
                    return self.find_worktree_path(repo_path, branch).await;
                }
            }

            // Fallback: create worktree manually
            self.create_with_git(repo_path, branch).await
        } else if self.delegates_to_wt() {
            // Fallback: use wt with --base flag
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

            self.find_worktree_path(repo_path, branch).await
        } else {
            // Git-only fallback: create the branch from its parent, then
            // attach a worktree to it. Attach, not `-b` (`create_with_git`
            // always passes `-b`): the branch already exists from the line
            // above, so creating it again would fail.
            let _ = tokio::process::Command::new("git")
                .args(["branch", branch, after_branch])
                .current_dir(repo_path)
                .output()
                .await;
            let worktree_path = self.managed_path(repo_path, branch);
            self.git_worktree_add(repo_path, &worktree_path, branch, false)
                .await
        }
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

    /// Get the diff for a worktree (all changes since branching from main).
    pub async fn get_diff(&self, worktree_path: &str) -> Result<String, String> {
        // Find the merge-base with main, then diff from there
        let merge_base = tokio::process::Command::new("git")
            .args(["merge-base", "main", "HEAD"])
            .current_dir(worktree_path)
            .output()
            .await
            .map_err(|e| format!("Failed to find merge-base: {}", e))?;

        let base = if merge_base.status.success() {
            String::from_utf8_lossy(&merge_base.stdout).trim().to_string()
        } else {
            // If main doesn't exist, try master
            let master_base = tokio::process::Command::new("git")
                .args(["merge-base", "master", "HEAD"])
                .current_dir(worktree_path)
                .output()
                .await
                .map_err(|e| format!("Failed to find merge-base: {}", e))?;

            if master_base.status.success() {
                String::from_utf8_lossy(&master_base.stdout).trim().to_string()
            } else {
                // Fallback: diff against HEAD~1
                "HEAD~1".to_string()
            }
        };

        let output = tokio::process::Command::new("git")
            .args(["diff", &format!("{}..HEAD", base)])
            .current_dir(worktree_path)
            .output()
            .await
            .map_err(|e| format!("Failed to get diff: {}", e))?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(format!("git diff failed: {}", String::from_utf8_lossy(&output.stderr)))
        }
    }

    /// Get the diff stat for a worktree.
    pub async fn get_diff_stat(&self, worktree_path: &str) -> Result<String, String> {
        let merge_base = tokio::process::Command::new("git")
            .args(["merge-base", "main", "HEAD"])
            .current_dir(worktree_path)
            .output()
            .await
            .map_err(|e| format!("Failed to find merge-base: {}", e))?;

        let base = if merge_base.status.success() {
            String::from_utf8_lossy(&merge_base.stdout).trim().to_string()
        } else {
            "HEAD~1".to_string()
        };

        let output = tokio::process::Command::new("git")
            .args(["diff", "--stat", &format!("{}..HEAD", base)])
            .current_dir(worktree_path)
            .output()
            .await
            .map_err(|e| format!("Failed to get diff stat: {}", e))?;

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Check if a worktree directory exists on disk.
    pub fn exists(&self, worktree_path: &str) -> bool {
        Path::new(worktree_path).exists()
    }

    // --- Private: wt-based operations ---

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

    async fn remove_with_wt(&self, worktree_path: &str, repo_path: &str) -> Result<(), String> {
        if !self.exists(worktree_path) {
            // `wt` is spawned with `current_dir(worktree_path)`, so a missing
            // directory fails at spawn with `NotFound` before `wt` ever runs.
            // That is anti-convergent rather than merely unhelpful: a first
            // attempt that did remove the directory guarantees every later
            // attempt fails. A cleanup that removed the worktree but could not
            // save the board retains `worktree_path` on purpose, so the ~30s
            // retry pass would then retry that task forever -- and this is the
            // default backend whenever `wt` is installed, because
            // `WorktreePlacement::Auto` is the default.
            //
            // Handing this case to git rather than just returning `Ok(())` is
            // what makes it converge to a *usable* state. `wt` leaves the
            // registration behind when the directory disappears underneath it,
            // and a prunable registration is not inert: `wt switch <branch>`
            // then refuses with "Worktree directory missing", and
            // `wt switch -c <branch>` refuses because the branch still exists,
            // so the task could never get a worktree again. Nothing else in
            // SlashIt runs `git worktree prune`. `remove_with_git` prunes the
            // stale record, and like this backend it leaves the branch alone.
            return self.remove_with_git(worktree_path, repo_path).await;
        }

        let output = tokio::process::Command::new("wt")
            .args(["remove", "-y", "--no-verify"])
            .current_dir(worktree_path)
            .output()
            .await
            .map_err(|e| format!("Failed to run wt remove: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("wt remove failed: {}", stderr));
        }

        // A zero exit is not proof of removal. `wt remove` reports the removal
        // as happening in the background, and it exits zero even when the
        // directory survives -- an unwritable parent is enough to reproduce
        // it. The only trustworthy signal is the directory itself, which is
        // exactly why `remove_with_git` decides on `exists()` rather than on
        // git's exit status. Without this check the caller would durably clear
        // `worktree_path`, discarding the only handle back to a worktree that
        // still holds the user's work.
        if self.exists(worktree_path) {
            return Err(format!(
                "wt remove reported success but {worktree_path} is still on disk"
            ));
        }

        Ok(())
    }

    // --- Private: git-based fallback ---

    async fn create_with_git(&self, repo_path: &str, branch: &str) -> Result<WorktreeInfo, String> {
        let worktree_path = self.managed_path(repo_path, branch);
        self.git_worktree_add(repo_path, &worktree_path, branch, true)
            .await
    }

    async fn remove_with_git(&self, worktree_path: &str, repo_path: &str) -> Result<(), String> {
        // Try normal remove first
        let output = tokio::process::Command::new("git")
            .args(["worktree", "remove", worktree_path])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| format!("Failed to remove worktree: {}", e))?;

        if !output.status.success() {
            // Fallback to force if normal remove fails (e.g., uncommitted changes)
            let force_output = tokio::process::Command::new("git")
                .args(["worktree", "remove", "--force", worktree_path])
                .current_dir(repo_path)
                .output()
                .await
                .map_err(|e| format!("Failed to force-remove worktree: {}", e))?;

            if !force_output.status.success() {
                // Last resort: prune stale git metadata for worktrees whose
                // directory is already gone. It cannot remove a directory
                // that is still present, so it can never turn this into a
                // false success below.
                let _ = tokio::process::Command::new("git")
                    .args(["worktree", "prune"])
                    .current_dir(repo_path)
                    .output()
                    .await;
            }
        }

        // Every attempt above can fail with a non-zero exit — which is not a
        // Rust `Err` — without the directory actually being gone. The only
        // trustworthy signal that removal worked is checking for the
        // directory itself; reporting success otherwise would let a caller
        // clear `worktree_path` for a worktree that is still on disk, losing
        // the only handle back to it.
        if self.exists(worktree_path) {
            return Err(format!(
                "worktree at {} still exists after worktree remove, --force, and prune all ran",
                worktree_path
            ));
        }

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
            gs_available: false,
            paths: test_paths(),
            placement: WorktreePlacement::Auto,
        }
    }

    #[test]
    fn new_does_not_panic() {
        // Even if wt/git-spice are absent, construction must succeed.
        let mgr = WorktreeManager::new(test_paths(), WorktreePlacement::Auto);
        // wt_available and gs_available are booleans; just assert type.
        let _ = mgr.wt_available;
        let _ = mgr.gs_available;
    }

    #[tokio::test]
    async fn remove_converges_on_an_absent_directory_under_worktrunk_delegation() {
        // The retry pass PR #1 adds only terminates if a second removal of an
        // already-absent worktree succeeds. `remove_with_git` gets that from
        // its `exists()` gate; `remove_with_wt` spawns with
        // `current_dir(worktree_path)`, so without its own guard the retry
        // fails at spawn every ~30s forever. This runs on machines with and
        // without `wt` installed, because the guard returns before spawning.
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
    async fn integration_remove_under_worktrunk_delegation_prunes_a_stale_registration() {
        // Converging is not enough on its own: `wt` leaves the registration
        // behind when the directory goes missing, and a prunable registration
        // blocks `wt switch <branch>` ("Worktree directory missing") while the
        // surviving branch blocks `wt switch -c <branch>`, so the task could
        // never get a worktree again. Nothing else in SlashIt prunes. Needs no
        // `wt` binary: the delegation happens before anything is spawned.
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
            "the stale registration must be pruned, or the branch can never be checked out again"
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
            WorktreeManager::adopt_any_registered(branch, &porcelain),
            Some(registered_at.to_string_lossy().to_string())
        );
    }

    #[test]
    fn adopt_any_registered_still_requires_git_confirmation_for_the_exact_branch() {
        let branch = "task-abcd1234";

        // Nothing registered for this branch at all.
        assert!(WorktreeManager::adopt_any_registered(branch, "").is_none());

        // Registered, but for a different branch.
        let porcelain = "\
worktree /home/someone/code/my-app
HEAD 7777777777777777777777777777777777777777
branch refs/heads/some-other-branch
";
        assert!(WorktreeManager::adopt_any_registered(branch, porcelain).is_none());
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
        assert!(WorktreeManager::adopt_any_registered(branch, &porcelain).is_none());
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
        // case because it is the one a ~30s retry pass actually re-enters.
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
            gs_available: false,
            paths: test_paths(),
            placement: WorktreePlacement::Managed,
        };

        let info = mgr
            .create_stacked_branch(repo_path, "stacked-branch", "main")
            .await
            .expect("create_stacked_branch should fall back to git, not delegate to wt");
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
            gs_available: false,
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
    /// remove`, `--force`, and `prune` all fail to actually delete the
    /// directory (a permission-blocked, non-empty subdirectory defeats the
    /// recursive delete each of them relies on), so `remove_with_git` must
    /// report `Err` — never silently `Ok(())` for a worktree still on disk.
    ///
    /// Unix-only: relies on `std::os::unix::fs::PermissionsExt` and the
    /// `ipc` module's `current_uid()`, neither of which exist when compiling
    /// for Windows (`ipc` itself is `#[cfg(unix)]`-gated in `lib.rs`).
    #[cfg(unix)]
    #[tokio::test]
    async fn remove_with_git_reports_err_when_the_directory_survives_every_fallback() {
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
        // contents, so the directory itself can never become empty and no
        // fallback (normal remove, --force, prune) can fully delete it.
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

    #[tokio::test]
    async fn integration_remove_nonexistent_does_not_panic() {
        let mgr = test_manager();
        // Removing a non-existent worktree should not panic (may return Err, that is fine).
        let _ = mgr.remove("/tmp/slashit_no_such_wt", "/tmp").await;
    }

    #[tokio::test]
    async fn integration_get_diff_clean_worktree() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let info = mgr.create(repo_path, "diff-clean").await.expect("create failed");

        let diff = mgr.get_diff(&info.path).await.expect("get_diff failed");
        // No changes committed on the new branch beyond what main has -> empty diff
        assert!(diff.is_empty(), "expected empty diff on clean worktree, got: {}", diff);
    }

    #[tokio::test]
    async fn integration_get_diff_with_changes() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
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

        let diff = mgr.get_diff(&info.path).await.expect("get_diff failed");
        assert!(diff.contains("new_file.txt"), "diff should mention the new file");
    }

    #[tokio::test]
    async fn integration_get_diff_stat() {
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();

        let mgr = test_manager();
        let info = mgr.create(repo_path, "stat-branch").await.expect("create failed");

        std::fs::write(Path::new(&info.path).join("stat_file.txt"), "data").unwrap();
        std::process::Command::new("git")
            .args(["add", "stat_file.txt"])
            .current_dir(&info.path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add stat file"])
            .current_dir(&info.path)
            .output()
            .unwrap();

        let stat = mgr.get_diff_stat(&info.path).await.expect("get_diff_stat failed");
        assert!(stat.contains("stat_file.txt"), "stat should mention the file");
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

        // Create a stacked branch on top of the base. In the git-only
        // fallback path this runs `git branch stacked-branch base-branch`
        // then attaches a worktree to the already-created branch without
        // `-b` (`git_worktree_add(..., create_branch: false)`), so it must
        // succeed.
        let stacked = mgr
            .create_stacked_branch(repo_path, "stacked-branch", "base-branch")
            .await
            .expect("git-only stacked branch creation from a non-main base must succeed");

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
}
