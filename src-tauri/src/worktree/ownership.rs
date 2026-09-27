//! Whether a task branch can be given to a task at all, when another task
//! could claim the same name.
//!
//! A task's branch is named from only the first eight hex digits of its id
//! (see [`super::WorktreeManager::branch_for_task`]), so two tasks can be
//! given the same name. Git knows nothing about tasks: a branch of that name,
//! and any worktree git has registered for it, may be either task's. Every
//! way of acquiring a checkout for a task asks [`refuse_shared_task_branch`]
//! first, and a task is refused rather than handed a checkout that may be
//! another task's. Startup reconciliation, which re-points a recorded
//! checkout that moved without acquiring it, asks
//! [`tasks_sharing_a_recorded_branch`] and re-points neither task.
//!
//! The rule, for task `T` and branch `B` in one repository:
//!
//! - When `T` records `B` as its branch, `T` owns it unless another task
//!   records `B` too. A task that has recorded nothing does not compete with
//!   it: it would be refused by this same rule when it starts.
//! - When `T` records no branch, `B` is the one it would be given, and `T`
//!   is refused while another task records `B`, or records no branch and
//!   would be given `B` as well. Neither can then say which task a branch or
//!   worktree of that name belongs to.
//!
//! "One repository" is the repository git shares between checkouts: two
//! Projects whose repositories are the same directory, or linked worktrees
//! of one repository, share one set of branches. A task whose repository
//! cannot be resolved, or that git cannot answer for, is taken to share it.

use crate::domain::{Project, Repository, Task};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// How another task can claim a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    /// It records the branch as its own.
    Recorded,
    /// It records no branch, and would be given this one.
    Generated,
}

/// Another task that can claim the branch, and where its repository is.
struct Claimant {
    id: Uuid,
    title: String,
    claim: Claim,
    repo_path: Option<String>,
}

/// Refuse `branch` for `task_id` while another task in the same repository
/// can claim it; see the module documentation for the rule.
///
/// Nothing is created, adopted or changed either way. `repo_path` is the
/// repository `task_id`'s checkout would be acquired in.
pub async fn refuse_shared_task_branch(
    tasks: &Arc<RwLock<HashMap<Uuid, Task>>>,
    projects: &Arc<RwLock<HashMap<Uuid, Project>>>,
    repositories: &Arc<RwLock<HashMap<Uuid, Repository>>>,
    task_id: Uuid,
    branch: &str,
    repo_path: &str,
) -> Result<(), String> {
    // Each lock is read on its own and released before the next, as
    // everywhere else, so this never holds one while waiting for another.
    let (records_branch, others) = {
        let tasks = tasks.read().await;
        let records_branch =
            tasks.get(&task_id).and_then(|t| t.branch_name.as_deref()) == Some(branch);
        let others: Vec<(Uuid, String, Claim, Uuid)> = tasks
            .values()
            .filter(|t| t.id != task_id)
            .filter_map(|t| {
                let claim = match t.branch_name.as_deref() {
                    Some(recorded) if recorded == branch => Claim::Recorded,
                    None if !records_branch
                        && super::WorktreeManager::branch_for_task(t.id) == branch =>
                    {
                        Claim::Generated
                    }
                    _ => return None,
                };
                Some((t.id, t.title.clone(), claim, t.project_id))
            })
            .collect();
        (records_branch, others)
    };
    if others.is_empty() {
        return Ok(());
    }
    let repository_of: HashMap<Uuid, Uuid> = {
        let projects = projects.read().await;
        others
            .iter()
            .filter_map(|(_, _, _, project_id)| {
                Some((*project_id, projects.get(project_id)?.repository_id?))
            })
            .collect()
    };
    let claimants: Vec<Claimant> = {
        let repositories = repositories.read().await;
        others
            .into_iter()
            .map(|(id, title, claim, project_id)| Claimant {
                id,
                title,
                claim,
                repo_path: repository_of
                    .get(&project_id)
                    .and_then(|repository_id| repositories.get(repository_id))
                    .map(|repository| repository.local_path.clone()),
            })
            .collect()
    };

    let own = repository_identity(repo_path).await;
    for claimant in claimants {
        let theirs = match claimant.repo_path.as_deref() {
            Some(path) => repository_identity(path).await,
            None => None,
        };
        let same_repository = match (&own, &theirs) {
            (Some(own), Some(theirs)) => own == theirs,
            _ => true,
        };
        if same_repository {
            return Err(refusal(task_id, branch, records_branch, &claimant));
        }
    }
    Ok(())
}

fn refusal(task_id: Uuid, branch: &str, records_branch: bool, claimant: &Claimant) -> String {
    let how = match claimant.claim {
        Claim::Recorded => "records it as its branch",
        Claim::Generated => "would be given the same branch name",
    };
    let what_to_do = if records_branch {
        "Delete whichever of the two tasks does not own that branch, then start this task again."
    } else {
        "Create this task again, which gives it a new id and branch name, or delete the other \
         task if it is no longer needed."
    };
    format!(
        "branch {branch} cannot be used for task {task_id}: task {} (\"{}\") in the same \
         repository {how}. Task branch names use only the first 8 characters of a task's id, so \
         SlashIt cannot tell which of the two tasks that branch, or any checkout of it, belongs \
         to, and uses neither. {what_to_do}",
        claimant.id, claimant.title
    )
}

/// The tasks among `tasks` that record the same branch as another task in
/// the same repository, each mapped to one such other task.
///
/// For a caller that re-points a task's recorded checkout without acquiring
/// it, such as startup reconciliation: by the rule above, neither task owns a
/// branch both record. `repo_for_project` gives each Project's repository
/// path; a task whose Project has none is taken to share any repository.
pub async fn tasks_sharing_a_recorded_branch(
    tasks: &[Task],
    repo_for_project: &HashMap<Uuid, String>,
) -> HashMap<Uuid, Uuid> {
    let mut by_branch: HashMap<&str, Vec<&Task>> = HashMap::new();
    for task in tasks {
        if let Some(branch) = task.branch_name.as_deref() {
            by_branch.entry(branch).or_default().push(task);
        }
    }
    let mut identities: HashMap<&str, Option<PathBuf>> = HashMap::new();
    let mut shared = HashMap::new();
    for recorders in by_branch.values().filter(|recorders| recorders.len() > 1) {
        let mut repositories = Vec::with_capacity(recorders.len());
        for task in recorders {
            let identity = match repo_for_project.get(&task.project_id) {
                Some(path) => match identities.get(path.as_str()) {
                    Some(identity) => identity.clone(),
                    None => {
                        let identity = repository_identity(path).await;
                        identities.insert(path.as_str(), identity.clone());
                        identity
                    }
                },
                None => None,
            };
            repositories.push(identity);
        }
        for (i, task) in recorders.iter().enumerate() {
            let other = recorders.iter().enumerate().find(|(j, _)| {
                *j != i
                    && match (&repositories[i], &repositories[*j]) {
                        (Some(own), Some(theirs)) => own == theirs,
                        _ => true,
                    }
            });
            if let Some((_, other)) = other {
                shared.insert(task.id, other.id);
            }
        }
    }
    shared
}

/// The repository `path` is a checkout of, as the directory git keeps its
/// shared state in (`--git-common-dir`), canonicalized. `None` when git
/// cannot say.
async fn repository_identity(path: &str) -> Option<PathBuf> {
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(path)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let common = String::from_utf8(output.stdout).ok()?;
    let common = Path::new(common.trim_end_matches(['\n', '\r']));
    // Relative to `path` when git answers relatively.
    Path::new(path).join(common).canonicalize().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskStatus;

    const A: Uuid = Uuid::from_u128(0x12345678_0000_4000_8000_000000000001);
    const B: Uuid = Uuid::from_u128(0x12345678_0000_4000_8000_000000000002);
    const BRANCH: &str = "task-12345678";

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }

    fn new_repository(dir: &Path) -> String {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["commit", "-q", "--allow-empty", "-m", "first"]);
        dir.to_string_lossy().to_string()
    }

    /// Task maps where each task is in a Project of its own, with a
    /// Repository record of its own at the path given (none for `None`).
    #[derive(Default)]
    struct Board {
        tasks: Arc<RwLock<HashMap<Uuid, Task>>>,
        projects: Arc<RwLock<HashMap<Uuid, Project>>>,
        repositories: Arc<RwLock<HashMap<Uuid, Repository>>>,
    }

    impl Board {
        async fn add(&self, id: Uuid, branch: Option<&str>, repo_path: Option<&str>) {
            let project_id = Uuid::new_v4();
            let repository_id = repo_path.map(|path| {
                let id = Uuid::new_v4();
                (id, path.to_string())
            });
            if let Some((repository_id, local_path)) = &repository_id {
                self.repositories.write().await.insert(
                    *repository_id,
                    Repository {
                        id: *repository_id,
                        local_path: local_path.clone(),
                        remote_url: None,
                        remote_type: None,
                        created_at: chrono::Utc::now(),
                    },
                );
                let project = Project {
                    id: project_id,
                    name: "project".to_string(),
                    repository_id: Some(*repository_id),
                    scope: crate::domain::ProjectScope::Standalone,
                    state_location: crate::config::paths::StateLocation::External,
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
                };
                self.projects.write().await.insert(project_id, project);
            }
            let mut task = crate::test_helpers::create_test_task_full("t", project_id, TaskStatus::Queue, 0);
            task.id = id;
            task.branch_name = branch.map(str::to_string);
            self.tasks.write().await.insert(id, task);
        }

        async fn refuse(&self, task_id: Uuid, repo_path: &str) -> Result<(), String> {
            refuse_shared_task_branch(&self.tasks, &self.projects, &self.repositories, task_id, BRANCH, repo_path)
                .await
        }
    }

    /// The same git repository, reached through another Project and another
    /// Repository record, whether at the same directory or through a linked
    /// worktree of it, shares one set of branches.
    #[tokio::test]
    async fn another_project_on_the_same_git_repository_is_the_same_repository() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = new_repository(&temp.path().join("repo"));
        let linked = temp.path().join("linked");
        git(Path::new(&repo), &["worktree", "add", "-q", "-b", "other", "--", linked.to_str().unwrap()]);

        for other_path in [repo.clone(), format!("{repo}/"), linked.to_string_lossy().to_string()] {
            let board = Board::default();
            board.add(A, Some(BRANCH), Some(&other_path)).await;
            board.add(B, None, Some(&repo)).await;

            let refused = board.refuse(B, &repo).await.expect_err(&other_path);
            assert!(refused.contains(&A.to_string()), "{refused}");
        }
    }

    /// The same branch name in another git repository is not a claim.
    #[tokio::test]
    async fn the_same_branch_name_in_another_repository_is_no_claim() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = new_repository(&temp.path().join("repo"));
        let elsewhere = new_repository(&temp.path().join("elsewhere"));
        let board = Board::default();
        board.add(A, Some(BRANCH), Some(&elsewhere)).await;
        board.add(B, None, Some(&repo)).await;

        assert_eq!(board.refuse(B, &repo).await, Ok(()));
    }

    /// A claimant whose repository cannot be resolved, or is not a git
    /// repository git can answer for, may be in the same one.
    #[tokio::test]
    async fn a_claimant_whose_repository_cannot_be_resolved_is_taken_to_share_it() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = new_repository(&temp.path().join("repo"));
        let not_a_repository = temp.path().join("plain");
        std::fs::create_dir_all(&not_a_repository).unwrap();
        for other_path in [None, Some(not_a_repository.to_string_lossy().to_string())] {
            let board = Board::default();
            board.add(A, Some(BRANCH), other_path.as_deref()).await;
            board.add(B, None, Some(&repo)).await;

            assert!(board.refuse(B, &repo).await.is_err(), "{other_path:?}");
        }
    }

    /// The task asking is never its own competitor, and a task recording a
    /// different branch does not claim the one it would otherwise be given.
    #[tokio::test]
    async fn only_other_tasks_claiming_the_branch_compete() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = new_repository(&temp.path().join("repo"));
        let board = Board::default();
        board.add(A, None, Some(&repo)).await;
        board.add(B, Some("task-recorded-elsewhere"), Some(&repo)).await;

        assert_eq!(board.refuse(A, &repo).await, Ok(()));
    }

    /// Two tasks both recording the branch both lose it: neither record
    /// says which one created it.
    #[tokio::test]
    async fn two_tasks_recording_the_same_branch_are_both_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = new_repository(&temp.path().join("repo"));
        let board = Board::default();
        board.add(A, Some(BRANCH), Some(&repo)).await;
        board.add(B, Some(BRANCH), Some(&repo)).await;

        for (task, other) in [(A, B), (B, A)] {
            let refused = board.refuse(task, &repo).await.expect_err("refused");
            assert!(refused.contains(&other.to_string()), "{refused}");
            assert!(refused.contains("Delete whichever"), "{refused}");
        }
    }
}
