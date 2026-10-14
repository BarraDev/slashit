//! A project's repository setup: whether it can run tasks, and the explicit
//! actions that make it able to.
//!
//! Every mutation here runs only because a person asked for it in the
//! project's repository settings (or in the Create Project form): choosing
//! the project's base branch, asking origin for its default branch, and
//! putting a folder under version control. None of them runs when a folder
//! is opened or a task is started. See `worktree::readiness` for what is
//! reported.

use crate::domain::{Project, ProjectBase};
use crate::worktree::resolve_default_base;
use crate::worktree::readiness::{readiness, Readiness};
use crate::worktree::vcs_init::{self, InitPreview, VcsInitKind};
use crate::worktree::{project_base, remote_head};
use uuid::Uuid;

/// The project `project_id`, and its repository's local path.
async fn project_and_path(
    state: &crate::AppState,
    project_id: Uuid,
) -> Result<(Project, String), String> {
    let project = state
        .project
        .projects
        .read()
        .await
        .get(&project_id)
        .cloned()
        .ok_or_else(|| format!("Project {project_id} not found"))?;
    let repository_id = project
        .repository_id
        .ok_or_else(|| format!("Project {} has no repository folder linked", project.name))?;
    let path = state
        .repository
        .repositories
        .read()
        .await
        .get(&repository_id)
        .map(|r| r.local_path.clone())
        .ok_or_else(|| format!("The repository of project {} is not known", project.name))?;
    Ok((project, path))
}

/// Record `base` as the project's local base, durably before memory: a
/// failed write leaves both as they were, and a write that replaced the file
/// without confirming it durable moves memory with the file.
pub(crate) async fn record_project_base(
    state: &crate::AppState,
    project_id: Uuid,
    base: Option<ProjectBase>,
) -> Result<(), String> {
    let mut projects = state.project.projects.write().await;
    let mut proposed = projects.clone();
    let project = proposed
        .get_mut(&project_id)
        .ok_or_else(|| format!("Project {project_id} not found"))?;
    project.base = base;
    project.updated_at = chrono::Utc::now();
    let outcome = crate::commands::project::try_persist_projects(&state.storage, &proposed);
    crate::config::adopt_published(&mut *projects, proposed, outcome)
        .map_err(|e| e.describe("save the project's base branch"))?;
    Ok(())
}

async fn report(state: &crate::AppState, project_id: Uuid) -> Result<Readiness, String> {
    let (project, path) = project_and_path(state, project_id).await?;
    readiness(&path, project.base.as_ref()).await
}

/// Whether the project can run tasks and deliver them, and why not.
#[tauri::command]
pub async fn get_project_readiness(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<Readiness, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    report(&state, project_id).await
}

/// Make the local branch `branch` the project's base, on explicit request.
#[tauri::command]
pub async fn set_project_base(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    branch: String,
) -> Result<Readiness, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    set_project_base_core(&state, project_id, &branch).await
}

pub(crate) async fn set_project_base_core(
    state: &crate::AppState,
    project_id: Uuid,
    branch: &str,
) -> Result<Readiness, String> {
    let (project, path) = project_and_path(state, project_id).await?;
    // Choosing another branch keeps the choice between it and a remote
    // default as the person made it.
    let prefer_local = project.base.as_ref().is_some_and(ProjectBase::prefers_local);
    let base = project_base::validate_choice(&path, branch).await?.choosing(prefer_local);
    record_project_base(state, project_id, Some(base)).await?;
    report(state, project_id).await
}

/// Choose where the project's new tasks start when the repository also has
/// a remote default branch: the project's local base branch
/// (`prefer_local`), or origin's default branch where the repository names
/// one. On explicit request only. Neither fetches, pushes nor changes the
/// repository; the choice is the project's, and survives a restart.
#[tauri::command]
pub async fn set_project_base_source(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    prefer_local: bool,
) -> Result<Readiness, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    set_project_base_source_core(&state, project_id, prefer_local).await
}

pub(crate) async fn set_project_base_source_core(
    state: &crate::AppState,
    project_id: Uuid,
    prefer_local: bool,
) -> Result<Readiness, String> {
    let (project, path) = project_and_path(state, project_id).await?;
    let current = project
        .base
        .ok_or("This project has no local base branch yet. Choose one in Settings > Repository first.")?;
    let chosen = if prefer_local {
        // The branch must still be one tasks can start from.
        project_base::validate_choice(&path, current.branch()).await?.choosing(true)
    } else {
        current.choosing(false)
    };
    // A choice that leaves tasks without a usable base is refused here, not
    // discovered when the next task starts.
    resolve_default_base(&path, Some(&chosen))
        .await
        .map_err(|why| format!("The choice was not saved: {why}"))?;
    record_project_base(state, project_id, Some(chosen)).await?;
    report(state, project_id).await
}

/// Ask origin for its default branch and record it locally
/// (`git remote set-head origin --auto`), on explicit request.
#[tauri::command]
pub async fn detect_remote_default_branch(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<Readiness, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    detect_remote_default_branch_core(&state, project_id).await
}

pub(crate) async fn detect_remote_default_branch_core(
    state: &crate::AppState,
    project_id: Uuid,
) -> Result<Readiness, String> {
    let (_, path) = project_and_path(state, project_id).await?;
    remote_head::detect_default_branch(&path).await?;
    report(state, project_id).await
}

/// What initializing version control in `path` would do. Writes nothing.
#[tauri::command]
pub async fn preview_vcs_initialization(path: String) -> Result<InitPreview, String> {
    vcs_init::preview(&path).await
}

/// Put the project's folder under version control with a first commit (or
/// give its commit-less Git repository one), then capture the branch the
/// tool created as the project's base. On explicit request only.
#[tauri::command]
pub async fn initialize_project_vcs(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    kind: VcsInitKind,
    expected_files: Option<usize>,
) -> Result<Readiness, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    initialize_project_vcs_core(&state, project_id, kind, expected_files).await
}

pub(crate) async fn initialize_project_vcs_core(
    state: &crate::AppState,
    project_id: Uuid,
    kind: VcsInitKind,
    expected_files: Option<usize>,
) -> Result<Readiness, String> {
    let (_, path) = project_and_path(state, project_id).await?;
    let outcome = vcs_init::initialize(&path, kind, expected_files).await?;
    record_project_base(
        state,
        project_id,
        Some(ProjectBase::LocalBranch {
            branch: outcome.branch,
        }),
    )
    .await
    .map_err(|e| format!("Version control was initialized in {path}, but {e}"))?;
    report(state, project_id).await
}

/// The base a newly registered project starts with: its repository's
/// unambiguous base branch, if it has one (see `worktree::project_base`).
pub(crate) async fn captured_base(repository_path: Option<&str>) -> Option<ProjectBase> {
    let path = repository_path?;
    project_base::propose(path).await.ok()?.into_base()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::readiness::{BaseComparison, BaseStanding};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    struct Fx {
        tmp: tempfile::TempDir,
        paths: std::sync::Arc<crate::config::paths::AppPaths>,
        state: crate::AppState,
        repo: PathBuf,
        project_id: Uuid,
        #[cfg(unix)]
        _no_jj: crate::test_helpers::FakeProgram,
    }

    fn paths_in(root: &Path) -> std::sync::Arc<crate::config::paths::AppPaths> {
        std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
            root.join("config"),
            root.join("data"),
            root.join("cache"),
            root.join("runtime"),
        ))
    }

    /// A project on `trunk` whose repository has a bare `origin` with the
    /// same commit and `origin/HEAD` set, as `git clone` leaves it, and
    /// whose base is the legacy kind: nobody has chosen between them.
    async fn fixture() -> Fx {
        // Readiness asks whether `jj` exists; a fake `jj` another test put on
        // the shared PATH must not see that.
        #[cfg(unix)]
        let no_jj = crate::test_helpers::FakeProgram::without(&["jj"]).await;
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let (state, _) = crate::app_core::build_state_with_paths(paths.clone()).await.unwrap();
        let repo = tmp.path().join("repo");
        let origin = tmp.path().join("origin.git");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "trunk"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        git(&repo, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&repo, &["push", "-q", "origin", "trunk"]);
        git(&repo, &["remote", "set-head", "origin", "trunk"]);

        let repository_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        state.repository.repositories.write().await.insert(
            repository_id,
            crate::domain::Repository {
                id: repository_id,
                local_path: repo.to_str().unwrap().to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        state.project.projects.write().await.insert(
            project_id,
            Project {
                id: project_id,
                name: "p".to_string(),
                repository_id: Some(repository_id),
                scope: crate::domain::project::ProjectScope::Standalone,
                state_location: crate::config::paths::StateLocation::External,
                base: Some(ProjectBase::LocalBranch { branch: "trunk".to_string() }),
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
            },
        );
        Fx {
            tmp,
            paths,
            state,
            repo,
            project_id,
            #[cfg(unix)]
            _no_jj: no_jj,
        }
    }

    fn standing(report: &Readiness) -> &BaseStanding {
        report.standing.as_ref().expect("both branches exist")
    }

    #[tokio::test]
    async fn a_project_that_has_not_chosen_reads_as_not_preferring_local_and_follows_origin() {
        let fx = fixture().await;
        let report = report(&fx.state, fx.project_id).await.unwrap();
        assert!(!report.prefers_local);
        assert_eq!(report.project_base.as_deref(), Some("trunk"));
        assert_eq!(standing(&report).comparison, BaseComparison::Same);
        assert_eq!(standing(&report).remote_branch, "origin/trunk");
        assert_eq!(format!("{:?}", report.base.unwrap().source), "Remote");
    }

    fn source_of(report: &Readiness) -> String {
        format!("{:?}", report.base.as_ref().expect("a base").source)
    }

    /// A second clone of origin, to make origin move without touching the
    /// project's repository.
    fn push_from_elsewhere(fx: &Fx, message: &str) {
        let other = fx.tmp.path().join("other");
        if !other.exists() {
            git(fx.tmp.path(), &["clone", "-q", "-b", "trunk", fx.tmp.path().join("origin.git").to_str().unwrap(), other.to_str().unwrap()]);
        }
        git(&other, &["pull", "-q", "--ff-only", "origin", "trunk"]);
        git(&other, &["commit", "-q", "--allow-empty", "-m", message]);
        git(&other, &["push", "-q", "origin", "HEAD:trunk"]);
        git(&fx.repo, &["fetch", "-q", "origin"]);
    }

    #[tokio::test]
    async fn choosing_the_local_branch_is_saved_survives_a_restart_and_a_remote_moving() {
        let fx = fixture().await;
        git(&fx.repo, &["commit", "-q", "--allow-empty", "-m", "local work"]);
        let local_tip = git(&fx.repo, &["rev-parse", "refs/heads/trunk"]);
        let origin_tip = git(&fx.repo, &["rev-parse", "refs/remotes/origin/trunk"]);

        // Unchosen: origin's default branch, and the local work is not where tasks start.
        let before = report(&fx.state, fx.project_id).await.unwrap();
        assert_eq!(source_of(&before), "Remote");
        assert_eq!(before.base.as_ref().unwrap().commit, origin_tip);
        assert_eq!(standing(&before).comparison, BaseComparison::LocalAhead { commits: 1 });

        let chosen = set_project_base_source_core(&fx.state, fx.project_id, true).await.unwrap();
        assert!(chosen.prefers_local);
        assert_eq!(source_of(&chosen), "Local");
        assert_eq!(chosen.base.as_ref().unwrap().commit, local_tip, "the exact local commit");

        // Restart: the choice is in the stored project, not in memory.
        let (restarted, _) = crate::app_core::build_state_with_paths(fx.paths.clone()).await.unwrap();
        let stored = restarted.project.projects.read().await.get(&fx.project_id).cloned().expect("stored project");
        assert_eq!(stored.base, Some(ProjectBase::PreferredLocalBranch { branch: "trunk".into() }));

        // Origin moves and is fetched: remote ahead, then diverged. The
        // choice stands; nothing silently switches to origin.
        push_from_elsewhere(&fx, "remote only");
        let diverged = report(&fx.state, fx.project_id).await.unwrap();
        assert_eq!(standing(&diverged).comparison, BaseComparison::Diverged { local_only: 1, remote_only: 1 });
        assert_eq!(source_of(&diverged), "Local");
        assert_eq!(diverged.base.as_ref().unwrap().commit, local_tip);
        git(&fx.repo, &["reset", "-q", "--hard", &origin_tip]);
        let behind = report(&fx.state, fx.project_id).await.unwrap();
        assert_eq!(standing(&behind).comparison, BaseComparison::RemoteAhead { commits: 1 });
        assert_eq!(source_of(&behind), "Local");
        assert_eq!(behind.base.as_ref().unwrap().commit, origin_tip);

        // Choosing another local branch keeps the choice.
        git(&fx.repo, &["branch", "release"]);
        let other = set_project_base_core(&fx.state, fx.project_id, "release").await.unwrap();
        assert!(other.prefers_local);
        assert_eq!(other.project_base.as_deref(), Some("release"));
    }

    #[tokio::test]
    async fn the_choice_can_be_withdrawn_and_origin_is_used_again() {
        let fx = fixture().await;
        git(&fx.repo, &["commit", "-q", "--allow-empty", "-m", "local work"]);
        let origin_tip = git(&fx.repo, &["rev-parse", "refs/remotes/origin/trunk"]);
        set_project_base_source_core(&fx.state, fx.project_id, true).await.unwrap();
        let back = set_project_base_source_core(&fx.state, fx.project_id, false).await.unwrap();
        assert!(!back.prefers_local);
        assert_eq!(source_of(&back), "Remote");
        assert_eq!(back.base.unwrap().commit, origin_tip);
        let stored = fx.state.project.projects.read().await.get(&fx.project_id).cloned().unwrap();
        assert_eq!(stored.base, Some(ProjectBase::LocalBranch { branch: "trunk".into() }), "the branch is kept");
    }

    #[tokio::test]
    async fn a_choice_that_leaves_no_usable_base_is_refused_and_nothing_changes() {
        let fx = fixture().await;
        let stored = |fx: &Fx| {
            let projects = fx.state.project.projects.try_read().unwrap();
            projects.get(&fx.project_id).unwrap().base.clone()
        };

        // The local branch is gone.
        git(&fx.repo, &["checkout", "-q", "--detach"]);
        git(&fx.repo, &["branch", "-q", "-D", "trunk"]);
        let refused = set_project_base_source_core(&fx.state, fx.project_id, true).await.unwrap_err();
        assert!(refused.contains("trunk"), "{refused}");
        assert_eq!(stored(&fx), Some(ProjectBase::LocalBranch { branch: "trunk".into() }));
        git(&fx.repo, &["branch", "-q", "trunk", "refs/remotes/origin/trunk"]);

        // A project with no base branch has nothing to choose.
        fx.state.project.projects.write().await.get_mut(&fx.project_id).unwrap().base = None;
        let refused = set_project_base_source_core(&fx.state, fx.project_id, true).await.unwrap_err();
        assert!(refused.contains("no local base branch"), "{refused}");
        fx.state.project.projects.write().await.get_mut(&fx.project_id).unwrap().base =
            Some(ProjectBase::PreferredLocalBranch { branch: "trunk".into() });

        // Origin's default is unusable: going back to it is refused, and the
        // person keeps the local choice.
        git(&fx.repo, &["update-ref", "refs/remotes/upstream/x", "refs/heads/trunk"]);
        git(&fx.repo, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/upstream/x"]);
        let refused = set_project_base_source_core(&fx.state, fx.project_id, false).await.unwrap_err();
        assert!(refused.contains("The choice was not saved"), "{refused}");
        assert_eq!(stored(&fx), Some(ProjectBase::PreferredLocalBranch { branch: "trunk".into() }));
        // The chosen local branch still works with that origin/HEAD.
        assert_eq!(source_of(&report(&fx.state, fx.project_id).await.unwrap()), "Local");
    }
}
