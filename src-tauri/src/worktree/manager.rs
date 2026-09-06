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

        match self.adopt_existing(repo_path, branch, porcelain) {
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

    /// Remove a worktree for a task.
    pub async fn remove(&self, worktree_path: &str, branch: &str, repo_path: &str) -> Result<(), String> {
        if self.delegates_to_wt() {
            self.remove_with_wt(worktree_path).await
        } else {
            self.remove_with_git(worktree_path, branch, repo_path).await
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

    async fn remove_with_wt(&self, worktree_path: &str) -> Result<(), String> {
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

        Ok(())
    }

    // --- Private: git-based fallback ---

    async fn create_with_git(&self, repo_path: &str, branch: &str) -> Result<WorktreeInfo, String> {
        let worktree_path = self.managed_path(repo_path, branch);
        self.git_worktree_add(repo_path, &worktree_path, branch, true)
            .await
    }

    async fn remove_with_git(&self, worktree_path: &str, branch: &str, repo_path: &str) -> Result<(), String> {
        // Try normal remove first
        let output = tokio::process::Command::new("git")
            .args(["worktree", "remove", worktree_path])
            .current_dir(repo_path)
            .output()
            .await
            .map_err(|e| format!("Failed to remove worktree: {}", e))?;

        let mut removed_by_git = output.status.success();

        if !removed_by_git {
            // Fallback to force if normal remove fails (e.g., uncommitted changes)
            let force_output = tokio::process::Command::new("git")
                .args(["worktree", "remove", "--force", worktree_path])
                .current_dir(repo_path)
                .output()
                .await
                .map_err(|e| format!("Failed to force-remove worktree: {}", e))?;

            removed_by_git = force_output.status.success();

            if !removed_by_git {
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

        // Deleting the branch requires more than observing that the directory
        // is gone. An absent directory is `Ok` above precisely because it may
        // have been removed by an earlier attempt or by the user, and the
        // cleanup retry pass re-enters this function every ~30s — so "the path
        // is absent" is also exactly what a retry that removed nothing sees.
        // `git branch -D` force-deletes regardless of merge state and leaves no
        // branch reflog, and branch names are deterministic from the task id,
        // so acting on that inference can destroy commits on a branch this call
        // has no claim to.
        //
        // A successful `git worktree remove` means git *had a registration for
        // this path* — note it also exits 0 for a record whose directory
        // something else already deleted, which is the case worth keeping. It
        // is an ownership proxy, not proof that this call did the removing, and
        // that is the property wanted here. The check stays after the `exists`
        // check for the original reason: deleting the branch while the worktree
        // is still checked out on it would leave a retry unable to recreate the
        // worktree at all.
        if removed_by_git {
            let _ = tokio::process::Command::new("git")
                .args(["branch", "-D", branch])
                .current_dir(repo_path)
                .output()
                .await;
        } else {
            // Reported rather than silent: this leaves a branch behind, and
            // nothing else collects it.
            eprintln!(
                "Note: worktree {worktree_path} was already absent and git had no registration \
                 for it, so branch {branch} was left in place rather than force-deleted"
            );
        }

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
    async fn integration_remove_deletes_the_branch_it_actually_removed() {
        // The ordinary path must be unchanged: a real removal still tidies up
        // the branch the worktree was checked out on.
        let tmp = create_temp_git_repo();
        let repo_path = tmp.path().to_str().unwrap();
        let mgr = test_manager();

        let info = mgr
            .create(repo_path, "task-removeme")
            .await
            .expect("create failed");

        mgr.remove(&info.path, "task-removeme", repo_path)
            .await
            .expect("removal of a live worktree should succeed");

        assert!(!Path::new(&info.path).exists(), "worktree dir should be gone");
        assert!(
            !branch_exists(repo_path, "task-removeme"),
            "a confirmed removal should still delete the branch"
        );
    }

    #[tokio::test]
    async fn integration_remove_keeps_the_branch_when_it_removed_nothing() {
        // A retry sees exactly this: the path is already absent, so both
        // `git worktree remove` invocations exit non-zero and nothing was
        // removed by this call. Reporting `Ok` is right — the worktree is gone
        // — but inferring from that that the branch is expendable is not.
        // `git branch -D` force-deletes regardless of merge state and leaves
        // no reflog, and branch names are deterministic from the task id, so
        // the branch reachable here may belong to a later run.
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
        mgr.remove(absent.to_str().unwrap(), "task-keepme", repo_path)
            .await
            .expect("an already-absent worktree is not a failure");

        assert!(
            branch_exists(repo_path, "task-keepme"),
            "no git removal succeeded, so the branch must not be force-deleted"
        );
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

        let result = mgr.remove(&info.path, "managed-remove", repo_path).await;
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
        let result = mgr.remove(&info.path, "remove-me", repo_path).await;
        assert!(result.is_ok(), "remove should succeed: {:?}", result.err());
        assert!(
            !Path::new(&info.path).exists(),
            "worktree directory should be removed"
        );

        // Verify the branch was also deleted
        let branch_check = std::process::Command::new("git")
            .args(["branch", "--list", "remove-me"])
            .current_dir(repo_path)
            .output()
            .expect("git branch list failed");
        let branches = String::from_utf8_lossy(&branch_check.stdout);
        assert!(
            !branches.contains("remove-me"),
            "branch should be deleted after remove"
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

        let result = mgr.remove(&info.path, "cwd-independent", &target_repo_path).await;

        assert!(result.is_ok(), "remove should succeed: {:?}", result.err());
        assert!(
            !Path::new(&info.path).exists(),
            "worktree directory should be removed from the target repo"
        );

        let branch_check = std::process::Command::new("git")
            .args(["branch", "--list", "cwd-independent"])
            .current_dir(&target_repo_path)
            .output()
            .expect("git branch list failed");
        assert!(
            !String::from_utf8_lossy(&branch_check.stdout).contains("cwd-independent"),
            "branch should be deleted from the target repo"
        );

        // The unrelated repo must never have been touched by any of this.
        let unrelated_branch_check = std::process::Command::new("git")
            .args(["branch", "--list", "cwd-independent"])
            .current_dir(unrelated_repo.path())
            .output()
            .expect("git branch list failed");
        assert!(
            String::from_utf8_lossy(&unrelated_branch_check.stdout).trim().is_empty(),
            "the unrelated repo must never see a branch created/removed in the target repo"
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

        let result = mgr.remove(&info.path, "undeletable", repo_path).await;

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
        let _ = mgr.remove("/tmp/slashit_no_such_wt", "no-branch", "/tmp").await;
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
