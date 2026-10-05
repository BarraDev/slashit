//! Task Checkouts and task branches that no Task owns, found by a read-only
//! scan and reclaimed only one at a time, on explicit request.
//!
//! # What an orphan is
//!
//! An orphan is something SlashIt itself manages that no existing Task owns:
//!
//! - a **checkout orphan** is a worktree git has registered at a path under
//!   the Project's managed worktree root (`AppPaths::worktrees_root`) that no
//!   Task records as its `worktree_path` and whose checked-out branch no Task
//!   owns;
//! - a **branch orphan** is a local branch named the way SlashIt names task
//!   branches (see [`super::looks_like_task_branch`]) that no Task in the same
//!   repository owns.
//!
//! A Task owns a branch when it records it as its `branch_name`, when the
//! branch is the name its id would give it (`task-<id>`), or when it is the
//! name earlier versions gave it (`task-<first 8 hex digits of the id>`): the
//! last cannot be told from another task's, so any task whose id starts that
//! way keeps the branch from being an orphan, as the same rule keeps it from
//! being handed out (see [`super::refuse_shared_task_branch`]). A Project's
//! repository that cannot be told apart from another's shares its tasks.
//!
//! A worktree outside the managed root (the project's own checkout, one
//! the user made, one another tool placed and a task adopted) and a branch
//! with any other name are never orphans and never listed.
//!
//! # Safety
//!
//! The scan changes nothing: it lists, reads refs and runs
//! `git --no-optional-locks status`. Every question it cannot answer is
//! reported on the item as a refusal; none is ever read as "safe".
//!
//! Reclaiming reuses the policy of an ordinary cleanup rather than adding
//! one. A checkout is removed by [`WorktreeManager::remove`], which never
//! forces and refuses to make a detached checkout's commits unreachable,
//! after refusing a checkout with uncommitted changes or one that could not
//! be inspected. The checkout's branch is never touched by that. A branch is
//! deleted only when a durable ref other than a branch no Task owns
//! (another local branch, a remote-tracking branch or a tag) already reaches
//! its tip, never while any checkout has it checked out, and by a
//! compare-and-delete of the tip that was inspected. The scan may be stale,
//! so each reclaim asks the same questions again before acting.

use super::manager::{
    git_program, locked_registration_notice, resolved_path, CheckoutRegistration, ListedWorktree,
    WorktreeManager,
};
use super::ownership::repository_identity;
use crate::domain::{Project, Repository, Task};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// What the existing Tasks of a repository own.
#[derive(Debug, Clone, Default)]
pub struct Owners {
    /// The `worktree_path` of every task, whichever its repository.
    pub checkout_paths: Vec<String>,
    /// The `branch_name` of every task in the repository.
    pub branches: HashSet<String>,
    /// The id of every task in the repository.
    pub task_ids: Vec<Uuid>,
}

impl Owners {
    fn owns_path(&self, path: &str) -> bool {
        let wanted = resolved_path(Path::new(path));
        self.checkout_paths
            .iter()
            .any(|owned| owned == path || resolved_path(Path::new(owned)) == wanted)
    }

    fn owns_branch(&self, name: &str) -> bool {
        self.branches.contains(name)
            || self.task_ids.iter().any(|id| {
                let id = id.to_string();
                name == format!("task-{id}") || name == format!("task-{}", &id[..8])
            })
    }
}

/// What the tasks in the repository at `repo_path` own. A task whose Project
/// has no repository, or whose repository git cannot tell from this one,
/// is taken to share it, which only ever keeps a branch from being an orphan.
pub async fn owners_in_repository(
    tasks: &Arc<RwLock<HashMap<Uuid, Task>>>,
    projects: &Arc<RwLock<HashMap<Uuid, Project>>>,
    repositories: &Arc<RwLock<HashMap<Uuid, Repository>>>,
    repo_path: &str,
) -> Owners {
    let all: Vec<(Uuid, Uuid, Option<String>, Option<String>)> = tasks
        .read()
        .await
        .values()
        .map(|t| (t.id, t.project_id, t.worktree_path.clone(), t.branch_name.clone()))
        .collect();
    let repo_of_project: HashMap<Uuid, Option<String>> = {
        let projects = projects.read().await;
        let repositories = repositories.read().await;
        all.iter()
            .map(|(_, project_id, _, _)| {
                let path = projects
                    .get(project_id)
                    .and_then(|p| p.repository_id)
                    .and_then(|r| repositories.get(&r))
                    .map(|r| r.local_path.clone());
                (*project_id, path)
            })
            .collect()
    };
    let own = repository_identity(repo_path).await;
    let mut same: HashMap<Uuid, bool> = HashMap::new();
    for (project_id, path) in &repo_of_project {
        let theirs = match path {
            Some(path) => repository_identity(path).await,
            None => None,
        };
        let shared = match (&own, &theirs) {
            (Some(own), Some(theirs)) => own == theirs,
            _ => true,
        };
        same.insert(*project_id, shared);
    }
    let mut owners = Owners::default();
    for (id, project_id, worktree_path, branch_name) in all {
        owners.checkout_paths.extend(worktree_path);
        if same.get(&project_id).copied().unwrap_or(true) {
            owners.task_ids.push(id);
            owners.branches.extend(branch_name);
        }
    }
    owners
}

/// Whether a checkout orphan's directory is there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutPresence {
    Present,
    /// Git registers it and its directory is gone.
    MissingRegistered,
    /// Git registers it locked and its directory is gone.
    MissingLocked,
    /// The filesystem or git could not say.
    Unverified,
}

/// A registered worktree under the managed root that no Task owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrphanCheckout {
    pub path: String,
    /// The branch checked out there, `None` when detached.
    pub branch: Option<String>,
    pub head: Option<String>,
    pub presence: CheckoutPresence,
    /// Whether it has uncommitted changes or untracked files. `None` when
    /// there is no directory to ask, or the question failed.
    pub dirty: Option<bool>,
    /// Why reclaiming it would be refused right now. `None` means every
    /// check passed, not that the reclaim is guaranteed.
    pub refusal: Option<String>,
}

/// A task branch that no Task owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrphanBranch {
    pub name: String,
    /// The commit it is at. `None` when it could not be read.
    pub tip: Option<String>,
    /// Whether a remote-tracking branch reaches its tip.
    pub pushed: Option<bool>,
    /// The durable refs that reach its tip.
    pub durable_refs: Vec<String>,
    /// Commits only this branch reaches. `None` when they could not be
    /// counted.
    pub unique_commits: Option<u32>,
    /// A checkout that has it checked out, if any.
    pub checked_out_at: Option<String>,
    /// Why reclaiming it would be refused right now.
    pub refusal: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OrphanScan {
    pub checkouts: Vec<OrphanCheckout>,
    pub branches: Vec<OrphanBranch>,
}

async fn git(repo: &str, args: &[&str]) -> Result<String, String> {
    let output = super::registry_lock::output(git_program(), repo, args)
        .await
        .map_err(|e| format!("`git {}` could not be run in {repo}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "`git {}` failed in {repo}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether the checkout at `path` has uncommitted changes or untracked
/// files, asked without taking any lock.
async fn is_dirty(path: &str) -> Result<bool, String> {
    let status = git(path, &["--no-optional-locks", "status", "--porcelain=v1", "-z", "--untracked-files=all"]).await?;
    Ok(!status.is_empty())
}

/// Every ref of `repo` under `refs/heads`, `refs/remotes` and `refs/tags`.
async fn durable_candidates(repo: &str) -> Result<Vec<String>, String> {
    let listed = git(repo, &["for-each-ref", "--format=%(refname)", "refs/heads", "refs/remotes", "refs/tags"]).await?;
    Ok(listed.lines().map(str::to_string).collect())
}

impl WorktreeManager {
    /// List the Task Checkouts and task branches of the repository at
    /// `repo_path` that no Task owns. Changes nothing.
    ///
    /// Fails, rather than listing less, when git cannot list worktrees or
    /// branches at all; a question that fails for one item is reported on
    /// that item.
    pub async fn scan_orphans(&self, repo_path: &str, owners: &Owners) -> Result<OrphanScan, String> {
        let listing = Self::worktree_listing(repo_path).await?;
        let records = ListedWorktree::parse(&listing);
        let root = resolved_path(&self.managed_root(repo_path));
        let primary = resolved_path(Path::new(repo_path));

        let mut checkouts = Vec::new();
        for record in &records {
            let resolved = resolved_path(Path::new(record.path));
            if resolved == primary || resolved == root || !resolved.starts_with(&root) {
                continue;
            }
            let branch = record.branch.map(|b| b.trim_start_matches("refs/heads/").to_string());
            if owners.owns_path(record.path) || branch.as_deref().is_some_and(|b| owners.owns_branch(b)) {
                continue;
            }
            checkouts.push(inspect_checkout(repo_path, &listing, record, branch).await);
        }

        let refs = durable_candidates(repo_path).await?;
        let orphan_names: Vec<String> = refs
            .iter()
            .filter_map(|r| r.strip_prefix("refs/heads/"))
            .filter(|name| super::looks_like_task_branch(name) && !owners.owns_branch(name))
            .map(str::to_string)
            .collect();
        // Another orphan is not a durable home: it may be reclaimed too.
        let excluded: HashSet<String> = orphan_names.iter().map(|n| format!("refs/heads/{n}")).collect();
        let durable: Vec<String> = refs.into_iter().filter(|r| !excluded.contains(r)).collect();
        let mut branches = Vec::new();
        for name in orphan_names {
            let checked_out_at = records
                .iter()
                .find(|r| r.branch == Some(format!("refs/heads/{name}").as_str()))
                .map(|r| r.path.to_string());
            branches.push(inspect_branch(repo_path, &name, &durable, checked_out_at).await);
        }
        checkouts.sort_by(|a, b| a.path.cmp(&b.path));
        branches.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(OrphanScan { checkouts, branches })
    }

    /// Remove the orphaned checkout at `path`, on explicit request.
    ///
    /// The scan is taken again first: a checkout that is owned now, or gone,
    /// or that fails any check, is refused with the reason. The removal is
    /// [`Self::remove`] itself. The checkout's branch is left as it is.
    pub async fn reclaim_orphan_checkout(
        &self,
        repo_path: &str,
        owners: &Owners,
        path: &str,
    ) -> Result<(), String> {
        let scan = self.scan_orphans(repo_path, owners).await?;
        let wanted = resolved_path(Path::new(path));
        let Some(orphan) = scan
            .checkouts
            .iter()
            .find(|c| c.path == path || resolved_path(Path::new(&c.path)) == wanted)
        else {
            return Err(format!(
                "{path} is not an orphaned Task Checkout now: a task owns it, git no longer \
                 registers it, or it is outside SlashIt's managed directory. Nothing was changed."
            ));
        };
        if let Some(refusal) = &orphan.refusal {
            return Err(refusal.clone());
        }
        self.remove(&orphan.path, repo_path, orphan.branch.as_deref()).await
    }

    /// Delete the orphaned task branch `name`, on explicit request.
    ///
    /// Refused unless a durable ref already reaches its tip and no checkout
    /// has it checked out. The deletion names the tip that was inspected, so
    /// a branch that moved in the meantime is not deleted.
    pub async fn reclaim_orphan_branch(
        &self,
        repo_path: &str,
        owners: &Owners,
        name: &str,
    ) -> Result<(), String> {
        let scan = self.scan_orphans(repo_path, owners).await?;
        let Some(orphan) = scan.branches.iter().find(|b| b.name == name) else {
            return Err(format!(
                "{name} is not an orphaned task branch now: a task owns it, it is gone, or it is \
                 not named like a task branch. Nothing was changed."
            ));
        };
        if let Some(refusal) = &orphan.refusal {
            return Err(refusal.clone());
        }
        let tip = orphan.tip.as_deref().ok_or("the branch's tip is not known")?;
        let name = super::checked_task_branch(name)?;
        git(repo_path, &["update-ref", "-d", &format!("refs/heads/{name}"), tip])
            .await
            .map(|_| ())
            .map_err(|why| format!("{name} was not deleted: {why}"))
    }
}

async fn inspect_checkout(
    repo_path: &str,
    listing: &str,
    record: &ListedWorktree<'_>,
    branch: Option<String>,
) -> OrphanCheckout {
    let path = record.path.to_string();
    let mut out = OrphanCheckout {
        path: path.clone(),
        branch: branch.clone(),
        head: record.head.map(str::to_string),
        presence: CheckoutPresence::Unverified,
        dirty: None,
        refusal: None,
    };
    match CheckoutRegistration::of_path(Ok(listing), &path) {
        CheckoutRegistration::Present { .. } if record.locked.is_some() => {
            out.presence = CheckoutPresence::Present;
            out.refusal = Some(format!(
                "git holds {path} locked, and `git worktree remove` refuses a locked checkout. \
                 If it is not needed, run `git worktree unlock {path}` in {repo_path}, then scan again."
            ));
        }
        CheckoutRegistration::Present { .. } => {
            out.presence = CheckoutPresence::Present;
            match is_dirty(&path).await {
                Ok(true) => {
                    out.dirty = Some(true);
                    out.refusal = Some(format!(
                        "{path} has uncommitted changes or untracked files, so it was not removed. \
                         Commit or discard them in the checkout yourself first."
                    ));
                }
                Ok(false) => out.dirty = Some(false),
                Err(why) => {
                    out.refusal = Some(format!(
                        "{path} could not be inspected ({why}), so it is not known whether it holds \
                         uncommitted work. Nothing was changed."
                    ));
                }
            }
        }
        CheckoutRegistration::MissingPrunable { .. } => out.presence = CheckoutPresence::MissingRegistered,
        CheckoutRegistration::MissingLocked { path: p, branch: b, reason } => {
            out.presence = CheckoutPresence::MissingLocked;
            out.refusal = Some(locked_registration_notice(repo_path, &p, b.as_deref(), reason.as_deref()));
        }
        CheckoutRegistration::Unknown(why) => out.refusal = Some(format!("{path} could not be inspected: {why}")),
        CheckoutRegistration::NotRegistered => {
            out.refusal = Some(format!("git does not register {path}, so it cannot be reclaimed here"));
        }
    }
    // The same refusal an ordinary cleanup makes for a detached checkout.
    if out.refusal.is_none() {
        if let Err(why) = WorktreeManager::refuse_to_drop_detached_commit(&path, repo_path).await {
            out.refusal = Some(why);
        }
    }
    out
}

async fn inspect_branch(
    repo_path: &str,
    name: &str,
    durable: &[String],
    checked_out_at: Option<String>,
) -> OrphanBranch {
    let mut out = OrphanBranch {
        name: name.to_string(),
        tip: None,
        pushed: None,
        durable_refs: Vec::new(),
        unique_commits: None,
        checked_out_at,
        refusal: None,
    };
    let tip = match git(repo_path, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}^{{commit}}")]).await {
        Ok(tip) => tip.trim().to_string(),
        Err(why) => {
            out.refusal = Some(format!("{name}'s tip could not be read ({why}), so nothing was changed."));
            return out;
        }
    };
    out.tip = Some(tip.clone());
    let reaching = match git(
        repo_path,
        &["for-each-ref", "--contains", &tip, "--format=%(refname)", "refs/heads", "refs/remotes", "refs/tags"],
    )
    .await
    {
        Ok(listed) => listed.lines().filter(|r| durable.iter().any(|d| d == r)).map(str::to_string).collect::<Vec<_>>(),
        Err(why) => {
            out.refusal = Some(format!(
                "which refs reach {name} could not be asked ({why}), so it is not known whether \
                 deleting it would lose work. Nothing was changed."
            ));
            return out;
        }
    };
    out.pushed = Some(reaching.iter().any(|r| r.starts_with("refs/remotes/")));
    if reaching.is_empty() {
        out.unique_commits = unique_commits(repo_path, &tip, durable).await.ok();
    } else {
        out.unique_commits = Some(0);
    }
    out.durable_refs = reaching;
    out.refusal = if let Some(path) = &out.checked_out_at {
        Some(format!(
            "{name} is checked out at {path}. Remove that checkout first; the branch is not deleted \
             while a checkout has it."
        ))
    } else if out.durable_refs.is_empty() {
        Some(match out.unique_commits {
            Some(n) => format!(
                "{name} has {n} commit(s) that no other branch, remote-tracking branch or tag \
                 reaches, so it was not deleted. Push it, or keep the work with \
                 `git branch <name> {tip}`, then delete it yourself."
            ),
            None => format!(
                "Whether {name}'s commits exist anywhere else could not be established, so it was \
                 not deleted."
            ),
        })
    } else {
        None
    };
    out
}

/// How many commits `tip` reaches that none of `durable` does.
async fn unique_commits(repo: &str, tip: &str, durable: &[String]) -> Result<u32, String> {
    use tokio::io::AsyncWriteExt;
    let mut input = format!("{tip}\n");
    for r in durable {
        input.push('^');
        input.push_str(r);
        input.push('\n');
    }
    let mut child = tokio::process::Command::new(git_program())
        .args(["rev-list", "--count", "--stdin"])
        .current_dir(repo)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("`git rev-list` could not be run in {repo}: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("`git rev-list` has no stdin")?;
    let writing = async move { stdin.write_all(input.as_bytes()).await };
    let (written, output) = tokio::join!(writing, child.wait_with_output());
    let output = output.map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    written.map_err(|e| e.to_string())?;
    String::from_utf8_lossy(&output.stdout).trim().parse().map_err(|e: std::num::ParseIntError| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::paths::AppPaths;
    use std::path::PathBuf;

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct Fixture {
        _root: tempfile::TempDir,
        repo: PathBuf,
        manager: WorktreeManager,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::Builder::new().prefix("slashit-orphans-").tempdir().unwrap();
            let repo = root.path().join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            git_in(&repo, &["init", "-q", "-b", "main"]);
            git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
            let paths = AppPaths::with_roots(
                root.path().join("config"),
                root.path().join("data"),
                root.path().join("cache"),
                root.path().join("runtime"),
            );
            let manager = WorktreeManager::new(Arc::new(paths));
            Self { _root: root, repo, manager }
        }

        fn repo(&self) -> &str {
            self.repo.to_str().unwrap()
        }

        /// A branch named like a task's, and a checkout of it under the
        /// managed root, as a task would have been given.
        fn managed_checkout(&self, id: Uuid) -> (String, PathBuf) {
            let branch = WorktreeManager::branch_for_task(id);
            let dir = self.manager.managed_root(self.repo()).join(&branch);
            std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
            git_in(&self.repo, &["worktree", "add", "-q", "-b", &branch, dir.to_str().unwrap()]);
            (branch, dir)
        }

        fn commit_in(&self, dir: &Path, file: &str) -> String {
            std::fs::write(dir.join(file), file).unwrap();
            git_in(dir, &["add", file]);
            git_in(dir, &["commit", "-q", "-m", file]);
            git_in(dir, &["rev-parse", "HEAD"])
        }

        async fn scan(&self, owners: &Owners) -> OrphanScan {
            self.manager.scan_orphans(self.repo(), owners).await.expect("scan")
        }
    }

    fn owning(id: Uuid, branch: Option<&str>, path: Option<&Path>) -> Owners {
        Owners {
            checkout_paths: path.map(|p| p.to_string_lossy().to_string()).into_iter().collect(),
            branches: branch.map(str::to_string).into_iter().collect(),
            task_ids: vec![id],
        }
    }

    #[tokio::test]
    async fn an_owned_checkout_and_its_branch_are_not_orphans() {
        let f = Fixture::new();
        let id = Uuid::new_v4();
        let (branch, dir) = f.managed_checkout(id);
        let scan = f.scan(&owning(id, Some(&branch), Some(&dir))).await;
        assert_eq!(scan, OrphanScan::default());
        // Owned through the id alone, without a recorded path or branch.
        let scan = f.scan(&owning(id, None, None)).await;
        assert_eq!(scan, OrphanScan::default());
    }

    #[tokio::test]
    async fn an_owned_branch_without_a_checkout_is_not_an_orphan() {
        let f = Fixture::new();
        let id = Uuid::new_v4();
        let branch = WorktreeManager::branch_for_task(id);
        git_in(&f.repo, &["branch", &branch]);
        assert_eq!(f.scan(&owning(Uuid::new_v4(), Some(&branch), None)).await, OrphanScan::default());
        // The name earlier versions gave the task is kept by its id's prefix.
        let early = format!("task-{}", &id.to_string()[..8]);
        git_in(&f.repo, &["branch", &early]);
        let scan = f.scan(&owning(id, None, None)).await;
        assert!(scan.branches.is_empty());
    }

    #[tokio::test]
    async fn an_orphan_checkout_and_an_orphan_branch_are_listed_and_clean_ones_reclaimable() {
        let f = Fixture::new();
        let (branch, dir) = f.managed_checkout(Uuid::new_v4());
        let scan = f.scan(&Owners::default()).await;
        assert_eq!(scan.checkouts.len(), 1);
        let c = &scan.checkouts[0];
        assert_eq!(c.branch.as_deref(), Some(branch.as_str()));
        assert_eq!(c.presence, CheckoutPresence::Present);
        assert_eq!(c.dirty, Some(false));
        assert_eq!(c.refusal, None);
        // The branch is checked out there, so it is listed and not deletable yet.
        assert_eq!(scan.branches.len(), 1);
        assert!(scan.branches[0].refusal.as_deref().unwrap().contains("checked out at"));
        assert!(dir.exists());
    }

    #[tokio::test]
    async fn worktrees_outside_the_managed_root_are_never_listed() {
        let f = Fixture::new();
        let outside = f._root.path().join("elsewhere");
        git_in(&f.repo, &["worktree", "add", "-q", "-b", "feature", outside.to_str().unwrap()]);
        let named_like_a_task = f._root.path().join("other");
        let branch = WorktreeManager::branch_for_task(Uuid::new_v4());
        git_in(&f.repo, &["worktree", "add", "-q", "-b", &branch, named_like_a_task.to_str().unwrap()]);
        let scan = f.scan(&Owners::default()).await;
        assert!(scan.checkouts.is_empty(), "{scan:?}");
        // Its task-shaped branch is an orphan branch, but only as a branch
        // that is checked out elsewhere and so is refused.
        assert_eq!(scan.branches.len(), 1);
        assert!(scan.branches[0].refusal.is_some());
        assert!(f.repo.exists());
    }

    #[tokio::test]
    async fn a_dirty_orphan_checkout_is_flagged_and_refused() {
        let f = Fixture::new();
        let (_, dir) = f.managed_checkout(Uuid::new_v4());
        std::fs::write(dir.join("notes.txt"), "mine").unwrap();
        let scan = f.scan(&Owners::default()).await;
        assert_eq!(scan.checkouts[0].dirty, Some(true));
        let path = scan.checkouts[0].path.clone();
        let refused = f.manager.reclaim_orphan_checkout(f.repo(), &Owners::default(), &path).await;
        assert!(refused.unwrap_err().contains("uncommitted"));
        assert!(dir.join("notes.txt").exists());
    }

    #[tokio::test]
    async fn a_branch_with_commits_only_it_has_is_refused_and_kept() {
        let f = Fixture::new();
        let branch = WorktreeManager::branch_for_task(Uuid::new_v4());
        let dir = f._root.path().join("scratch");
        git_in(&f.repo, &["worktree", "add", "-q", "-b", &branch, dir.to_str().unwrap()]);
        f.commit_in(&dir, "a.txt");
        f.commit_in(&dir, "b.txt");
        git_in(&f.repo, &["worktree", "remove", dir.to_str().unwrap()]);
        let scan = f.scan(&Owners::default()).await;
        let b = &scan.branches[0];
        assert_eq!(b.unique_commits, Some(2));
        assert_eq!(b.pushed, Some(false));
        assert!(b.refusal.as_deref().unwrap().contains("2 commit(s)"));
        let refused = f.manager.reclaim_orphan_branch(f.repo(), &Owners::default(), &branch).await;
        assert!(refused.is_err());
        git_in(&f.repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}")]);
    }

    #[tokio::test]
    async fn a_branch_another_ref_reaches_is_deleted() {
        let f = Fixture::new();
        let merged = WorktreeManager::branch_for_task(Uuid::new_v4());
        git_in(&f.repo, &["branch", &merged]);
        let scan = f.scan(&Owners::default()).await;
        assert_eq!(scan.branches[0].refusal, None);
        assert!(scan.branches[0].durable_refs.contains(&"refs/heads/main".to_string()));
        f.manager.reclaim_orphan_branch(f.repo(), &Owners::default(), &merged).await.expect("reclaim");
        assert!(f.scan(&Owners::default()).await.branches.is_empty());
        let listed = git_in(&f.repo, &["branch", "--list", &merged]);
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn two_orphan_branches_holding_the_same_work_do_not_vouch_for_each_other() {
        let f = Fixture::new();
        let a = WorktreeManager::branch_for_task(Uuid::new_v4());
        let b = WorktreeManager::branch_for_task(Uuid::new_v4());
        let dir = f._root.path().join("scratch");
        git_in(&f.repo, &["worktree", "add", "-q", "-b", &a, dir.to_str().unwrap()]);
        f.commit_in(&dir, "a.txt");
        git_in(&f.repo, &["worktree", "remove", dir.to_str().unwrap()]);
        git_in(&f.repo, &["branch", &b, &a]);
        let scan = f.scan(&Owners::default()).await;
        assert_eq!(scan.branches.len(), 2);
        assert!(scan.branches.iter().all(|b| b.refusal.is_some()), "{scan:?}");
    }

    #[tokio::test]
    async fn a_registered_checkout_whose_directory_is_gone_is_listed_and_reclaimed() {
        let f = Fixture::new();
        let (branch, dir) = f.managed_checkout(Uuid::new_v4());
        std::fs::remove_dir_all(&dir).unwrap();
        let scan = f.scan(&Owners::default()).await;
        assert_eq!(scan.checkouts[0].presence, CheckoutPresence::MissingRegistered);
        assert_eq!(scan.checkouts[0].dirty, None);
        assert_eq!(scan.checkouts[0].refusal, None);
        f.manager
            .reclaim_orphan_checkout(f.repo(), &Owners::default(), dir.to_str().unwrap())
            .await
            .expect("reclaim");
        let scan = f.scan(&Owners::default()).await;
        assert!(scan.checkouts.is_empty());
        // The checkout's branch is a separate decision and is still there.
        assert_eq!(scan.branches.iter().map(|b| b.name.clone()).collect::<Vec<_>>(), vec![branch]);
    }

    #[tokio::test]
    async fn a_checkout_that_cannot_be_inspected_is_refused_not_called_safe() {
        let f = Fixture::new();
        let (_, dir) = f.managed_checkout(Uuid::new_v4());
        std::fs::write(dir.join(".git"), "gitdir: /nonexistent/nowhere\n").unwrap();
        let scan = f.scan(&Owners::default()).await;
        let c = &scan.checkouts[0];
        assert_eq!(c.dirty, None);
        assert!(c.refusal.as_deref().unwrap().contains("could not be inspected"), "{c:?}");
        let refused = f.manager.reclaim_orphan_checkout(f.repo(), &Owners::default(), &c.path).await;
        assert!(refused.is_err());
        assert!(dir.exists());
    }

    #[tokio::test]
    async fn a_scan_that_git_cannot_answer_is_an_error_not_an_empty_list() {
        let f = Fixture::new();
        let missing = f._root.path().join("not-a-repo");
        std::fs::create_dir_all(&missing).unwrap();
        assert!(f.manager.scan_orphans(missing.to_str().unwrap(), &Owners::default()).await.is_err());
    }

    #[tokio::test]
    async fn a_clean_checkout_is_reclaimed_and_its_branch_stays() {
        let f = Fixture::new();
        let (branch, dir) = f.managed_checkout(Uuid::new_v4());
        f.manager
            .reclaim_orphan_checkout(f.repo(), &Owners::default(), dir.to_str().unwrap())
            .await
            .expect("reclaim");
        assert!(!dir.exists());
        git_in(&f.repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}")]);
    }

    /// Work only a detached `HEAD` names is protected by the same refusal an
    /// ordinary cleanup makes.
    #[tokio::test]
    async fn work_only_a_detached_head_names_is_refused_through_the_cleanup_policy() {
        let f = Fixture::new();
        let (_, dir) = f.managed_checkout(Uuid::new_v4());
        git_in(&dir, &["checkout", "-q", "--detach"]);
        let commit = f.commit_in(&dir, "lost.txt");
        let scan = f.scan(&Owners::default()).await;
        let c = &scan.checkouts[0];
        assert_eq!(c.dirty, Some(false));
        assert!(c.refusal.as_deref().unwrap().contains(&commit), "{c:?}");
        let refused = f.manager.reclaim_orphan_checkout(f.repo(), &Owners::default(), &c.path).await;
        assert!(refused.unwrap_err().contains("unreachable"));
        assert!(dir.join("lost.txt").exists());
    }

    #[tokio::test]
    async fn a_failed_reclaim_leaves_the_other_orphan_and_its_own_record_alone() {
        let f = Fixture::new();
        let (_, dirty) = f.managed_checkout(Uuid::new_v4());
        let (_, clean) = f.managed_checkout(Uuid::new_v4());
        std::fs::write(dirty.join("wip.txt"), "x").unwrap();
        let none = Owners::default();
        assert!(f.manager.reclaim_orphan_checkout(f.repo(), &none, dirty.to_str().unwrap()).await.is_err());
        f.manager.reclaim_orphan_checkout(f.repo(), &none, clean.to_str().unwrap()).await.expect("clean");
        assert!(dirty.join("wip.txt").exists());
        assert!(!clean.exists());
        let scan = f.scan(&none).await;
        assert_eq!(scan.checkouts.len(), 1);
        assert_eq!(scan.checkouts[0].dirty, Some(true));
    }

    #[tokio::test]
    async fn a_stale_scan_cannot_reclaim_what_a_task_owns_now() {
        let f = Fixture::new();
        let id = Uuid::new_v4();
        let (branch, dir) = f.managed_checkout(id);
        let owners = owning(id, Some(&branch), Some(&dir));
        let refused = f.manager.reclaim_orphan_checkout(f.repo(), &owners, dir.to_str().unwrap()).await;
        assert!(refused.unwrap_err().contains("not an orphaned"));
        assert!(dir.exists());
        let refused = f.manager.reclaim_orphan_branch(f.repo(), &owners, &branch).await;
        assert!(refused.unwrap_err().contains("not an orphaned"));
    }

    #[tokio::test]
    async fn a_branch_is_never_deleted_while_a_checkout_has_it() {
        let f = Fixture::new();
        let (branch, dir) = f.managed_checkout(Uuid::new_v4());
        let refused = f.manager.reclaim_orphan_branch(f.repo(), &Owners::default(), &branch).await;
        assert!(refused.unwrap_err().contains("checked out at"));
        assert!(dir.exists());
    }

    /// Tasks of the repository, and of one that cannot be told apart from it,
    /// own; tasks of another repository do not.
    #[tokio::test(flavor = "multi_thread")]
    async fn ownership_is_read_from_the_tasks_of_the_same_repository() {
        use crate::domain::TaskStatus;
        let f = Fixture::new();
        let other = f._root.path().join("other-repo");
        std::fs::create_dir_all(&other).unwrap();
        git_in(&other, &["init", "-q", "-b", "main"]);
        let tasks = Arc::new(RwLock::new(HashMap::new()));
        let projects = Arc::new(RwLock::new(HashMap::new()));
        let repositories = Arc::new(RwLock::new(HashMap::new()));
        let add = |repo: Option<&Path>, branch: &str| {
            let project_id = Uuid::new_v4();
            let repository_id = repo.map(|path| {
                let id = Uuid::new_v4();
                futures_lite_block(repositories.write()).insert(id, Repository {
                    id,
                    local_path: path.to_string_lossy().to_string(),
                    remote_url: None,
                    remote_type: None,
                    created_at: chrono::Utc::now(),
                });
                id
            });
            let mut task = crate::test_helpers::create_test_task_full("t", project_id, TaskStatus::Queue, 0);
            task.branch_name = Some(branch.to_string());
            task.worktree_path = Some(format!("/somewhere/{branch}"));
            futures_lite_block(projects.write()).insert(project_id, Project {
                id: project_id,
                name: "p".to_string(),
                repository_id,
                scope: crate::domain::ProjectScope::Standalone,
                state_location: crate::config::paths::StateLocation::External,
                base: None,
                agent_type: crate::domain::AgentType::ClaudeCode,
                agent_config: crate::domain::AgentConfig {
                    agent_type: crate::domain::AgentType::ClaudeCode,
                    command: "claude".to_string(),
                    args: Vec::new(),
                    env: HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            });
            futures_lite_block(tasks.write()).insert(task.id, task);
        };
        add(Some(&f.repo), "mine");
        add(Some(&other), "theirs");
        add(None, "unknown-repository");
        let owners = owners_in_repository(&tasks, &projects, &repositories, f.repo()).await;
        assert!(owners.branches.contains("mine"));
        assert!(owners.branches.contains("unknown-repository"));
        assert!(!owners.branches.contains("theirs"));
        assert_eq!(owners.checkout_paths.len(), 3);
    }

    fn futures_lite_block<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
    }

    #[tokio::test]
    async fn a_locked_checkout_is_explained_and_not_removed() {
        let f = Fixture::new();
        let (_, dir) = f.managed_checkout(Uuid::new_v4());
        git_in(&f.repo, &["worktree", "lock", dir.to_str().unwrap()]);
        let scan = f.scan(&Owners::default()).await;
        assert!(scan.checkouts[0].refusal.as_deref().unwrap().contains("locked"));
        let refused = f.manager.reclaim_orphan_checkout(f.repo(), &Owners::default(), &scan.checkouts[0].path).await;
        assert!(refused.is_err());
        assert!(dir.exists());
    }
}
