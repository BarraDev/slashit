use crate::domain::Repository;
use crate::config::Storage;
use crate::commands::file::is_git_repo_root;
use uuid::Uuid;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

type Repositories = Arc<RwLock<HashMap<Uuid, Repository>>>;

/// Persist repositories to the config file after mutation, reporting failure
/// to the caller.
///
/// Loading the current config is not allowed to fail silently into
/// `AppConfig::default()`: a transient read failure (permissions, a momentary
/// I/O error) must not be papered over by writing back a blank config, which
/// would destroy `jj_config`, `ui_preferences`, `agent_configs`, and
/// `projects`. The error is propagated instead so the caller can surface it
/// rather than silently "succeeding" at wiping the user's config.
///
/// The read and the write go through `Storage::update_config` so they are one
/// transaction. This is the other half of the pair described on
/// [`crate::commands::project::try_persist_projects`]: the `repositories`
/// guard held by the caller excludes other repository writers, but nothing
/// about it excludes a concurrent project save from rewriting the same file.
pub(crate) fn try_persist_repositories(
    storage: &Storage,
    repositories: &HashMap<Uuid, Repository>,
) -> anyhow::Result<()> {
    // See `try_persist_projects` for why this deliberately adds no context of
    // its own: callers render with `{e}`, so an outer wrapper would hide the
    // failing stage.
    storage.update_config(|config| {
        // Update repositories in config (convert Uuid keys to String keys)
        config.repositories = repositories
            .iter()
            .map(|(id, repository)| (id.to_string(), repository.clone()))
            .collect();
    })
}

#[derive(Clone)]
pub struct RepositoryState {
    pub repositories: Repositories,
}

impl RepositoryState {
    pub fn new() -> Self {
        Self {
            repositories: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for RepositoryState {
    fn default() -> Self {
        Self::new()
    }
}

/// Initialize a git repository at `path` if the caller asked for it and the
/// folder is not already a git repository at the moment of execution.
///
/// The frontend's own git-detection runs when the folder is picked, which
/// can be stale by the time the user submits the form (the folder could have
/// been git-init'd externally, or a `.git` could have appeared or vanished
/// in between). Re-checking here — right before acting — is what makes the
/// check authoritative instead of decorative.
///
/// Deliberately does not force an initial branch name: that would override
/// the user's own `init.defaultBranch` git config, which this feature has no
/// reason to second-guess.
///
/// Returns whether `git init` actually ran, so the caller can tell a fresh
/// initialization apart from a folder that was already a repository — the
/// two need different error framing if persistence fails afterward.
async fn ensure_git_initialized(path: &Path) -> Result<bool, String> {
    if is_git_repo_root(path) {
        // Already a repo — leave its history/config untouched.
        return Ok(false);
    }

    let output = tokio::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .output()
        .await
        .map_err(|e| format!("Failed to run git init: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    if !is_git_repo_root(path) {
        return Err(format!(
            "git init reported success but '{}' is still not a git repository",
            path.display()
        ));
    }

    Ok(true)
}

#[tauri::command]
pub async fn create_repository(
    state: tauri::State<'_, crate::AppState>,
    local_path: String,
    remote_url: Option<String>,
    initialize_git: bool,
) -> Result<Repository, String> {
    create_repository_core(
        &state.repository.repositories,
        &state.storage,
        local_path,
        remote_url,
        initialize_git,
    )
    .await
}

/// Core of [`create_repository`], taking the raw map and storage so tests
/// can call it without a `tauri::State`.
///
/// Ordering is deliberate: validate the path, then (if asked) initialize
/// git, then verify git actually took, then persist — only after all of
/// that succeeds is anything reported to the caller as done.
async fn create_repository_core(
    repositories: &RwLock<HashMap<Uuid, Repository>>,
    storage: &Storage,
    local_path: String,
    remote_url: Option<String>,
    initialize_git: bool,
) -> Result<Repository, String> {
    let path = std::path::PathBuf::from(&local_path);
    if !path.is_dir() {
        return Err(format!("'{}' is not a directory", local_path));
    }

    let git_freshly_initialized = if initialize_git {
        ensure_git_initialized(&path).await?
    } else {
        false
    };

    let id = Uuid::new_v4();
    let remote_type = remote_url.as_ref().and_then(|url| {
        if url.contains("github.com") {
            Some(crate::domain::RemoteType::GitHub)
        } else if url.contains("gitlab.com") {
            Some(crate::domain::RemoteType::GitLab)
        } else if url.contains("bitbucket.org") {
            Some(crate::domain::RemoteType::Bitbucket)
        } else {
            None
        }
    });

    let repository = Repository {
        id,
        local_path: local_path.clone(),
        remote_url,
        remote_type,
        created_at: chrono::Utc::now(),
    };

    create_repository_committed(repositories, storage, repository)
        .await
        .map_err(|e| {
            if git_freshly_initialized {
                // The filesystem side effect already happened and is not
                // undone here: deleting `.git` on a persistence failure
                // would destroy real repository state to manufacture a
                // rollback that was never atomic to begin with.
                format!(
                    "Git repository was initialized at '{local_path}', but registering the project failed: {e}"
                )
            } else {
                e
            }
        })
}

/// Persists a fully-built [`Repository`], called by [`create_repository_core`]
/// after any requested git initialization has already succeeded.
///
/// Inserts into a cloned map, persists the clone, and only replaces the
/// shared map on success — a failed persist never leaves memory holding a
/// repository disk never recorded (which a later successful persist could
/// otherwise write).
async fn create_repository_committed(
    repositories: &RwLock<HashMap<Uuid, Repository>>,
    storage: &Storage,
    repository: Repository,
) -> Result<Repository, String> {
    let mut repositories_w = repositories.write().await;
    let mut proposed = repositories_w.clone();
    proposed.insert(repository.id, repository.clone());

    try_persist_repositories(storage, &proposed)
        .map_err(|e| format!("Failed to persist repository: {e}"))?;

    *repositories_w = proposed;
    Ok(repository)
}

#[tauri::command]
pub async fn list_repositories(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<Repository>, String> {
    let repositories = state.repository.repositories.read().await;
    Ok(repositories.values().cloned().collect())
}

#[tauri::command]
pub async fn get_repository(
    state: tauri::State<'_, crate::AppState>,
    id: String,
) -> Result<Option<Repository>, String> {
    let id = Uuid::parse_str(&id).map_err(|e| e.to_string())?;
    let repositories = state.repository.repositories.read().await;
    Ok(repositories.get(&id).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::paths::AppPaths;
    use crate::config::storage::AppConfig;
    use tempfile::TempDir;

    fn create_test_storage() -> (Storage, TempDir) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = temp_dir.path();
        std::fs::create_dir_all(root.join("config")).expect("Failed to create config dir");
        std::fs::create_dir_all(root.join("data")).expect("Failed to create data dir");

        let storage = Storage::with_paths(AppPaths::with_roots(
            root.join("config"),
            root.join("data"),
            root.join("cache"),
            root.join("runtime"),
        ));

        (storage, temp_dir)
    }

    fn make_test_repository(id: Uuid) -> Repository {
        Repository {
            id,
            local_path: "/path/to/repo".to_string(),
            remote_url: None,
            remote_type: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn try_persist_repositories_missing_config_file_initializes_from_defaults() {
        // No config.toml exists yet — this must behave exactly like today:
        // an `Ok` default config that the new repository is written into.
        let (storage, _temp) = create_test_storage();

        let mut repositories = HashMap::new();
        let id = Uuid::new_v4();
        repositories.insert(id, make_test_repository(id));

        try_persist_repositories(&storage, &repositories)
            .expect("should succeed against a missing config");

        let loaded = storage.load_config().expect("should load the freshly written config");
        assert_eq!(loaded.repositories.len(), 1);
        assert!(loaded.repositories.contains_key(&id.to_string()));
        assert_eq!(loaded.ui_preferences.theme, AppConfig::default().ui_preferences.theme);
    }

    #[cfg(unix)]
    #[test]
    fn try_persist_repositories_propagates_read_failure_without_destroying_config() {
        use std::os::unix::fs::PermissionsExt;

        if crate::ipc::server::current_uid() == 0 {
            return; // root ignores permission bits, so chmod 0o000 would not force a read failure
        }

        let (storage, _temp) = create_test_storage();

        // Seed a real config with data that must survive a failed persist.
        let mut seed = AppConfig::default();
        seed.jj_config.user_name = Some("Keep Me".to_string());
        storage.save_config(&seed).expect("seed config");

        let config_path = storage.paths().config_file();
        let original_bytes = std::fs::read(&config_path).unwrap();

        // Force load_config() to return Err by making the existing file unreadable.
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut repositories = HashMap::new();
        let id = Uuid::new_v4();
        repositories.insert(id, make_test_repository(id));

        let result = try_persist_repositories(&storage, &repositories);

        // Restore permissions so the file can be inspected and cleaned up.
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert!(result.is_err(), "a read failure must not be treated as success");
        let on_disk = std::fs::read(&config_path).unwrap();
        assert_eq!(on_disk, original_bytes, "config on disk must be untouched by a failed persist");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_repository_leaves_memory_unchanged_when_persistence_fails() {
        use std::os::unix::fs::PermissionsExt;
        if crate::ipc::server::current_uid() == 0 {
            return;
        }

        let (storage, _temp) = create_test_storage();
        storage.save_config(&AppConfig::default()).expect("seed config");
        std::fs::set_permissions(storage.paths().config_file(), std::fs::Permissions::from_mode(0o000)).unwrap();

        let repositories: RwLock<HashMap<Uuid, Repository>> = RwLock::new(HashMap::new());
        let new_repo = make_test_repository(Uuid::new_v4());

        let result = create_repository_committed(&repositories, &storage, new_repo).await;

        assert!(result.is_err(), "a persistence failure must be reported, not swallowed");
        assert!(
            repositories.read().await.is_empty(),
            "memory must not contain a repository disk never recorded"
        );
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be on PATH for these tests")
    }

    #[tokio::test]
    async fn checked_on_a_non_git_folder_creates_a_real_git_repository() {
        let (storage, temp) = create_test_storage();
        let folder = temp.path().join("project");
        std::fs::create_dir_all(&folder).unwrap();
        let repositories: RwLock<HashMap<Uuid, Repository>> = RwLock::new(HashMap::new());

        let result = create_repository_core(
            &repositories,
            &storage,
            folder.to_string_lossy().to_string(),
            None,
            true,
        )
        .await;

        assert!(result.is_ok(), "expected success, got {:?}", result.err());
        assert!(is_git_repo_root(&folder), "folder must be a real git repository");
        let toplevel = git(&folder, &["rev-parse", "--is-inside-work-tree"]);
        assert!(toplevel.status.success(), "git must recognize the folder as a work tree");
    }

    #[tokio::test]
    async fn unchecked_on_a_non_git_folder_never_touches_git() {
        let (storage, temp) = create_test_storage();
        let folder = temp.path().join("project");
        std::fs::create_dir_all(&folder).unwrap();
        let repositories: RwLock<HashMap<Uuid, Repository>> = RwLock::new(HashMap::new());

        let result = create_repository_core(
            &repositories,
            &storage,
            folder.to_string_lossy().to_string(),
            None,
            false,
        )
        .await;

        assert!(result.is_ok(), "expected success, got {:?}", result.err());
        assert!(!folder.join(".git").exists(), "no git initialization must occur when unchecked");
    }

    #[tokio::test]
    async fn checked_on_an_existing_git_repo_does_not_reinitialize_it() {
        let (storage, temp) = create_test_storage();
        let folder = temp.path().join("project");
        std::fs::create_dir_all(&folder).unwrap();
        assert!(git(&folder, &["init", "-q"]).status.success());
        git(&folder, &["config", "user.email", "test@example.com"]);
        git(&folder, &["config", "user.name", "Test"]);
        std::fs::write(folder.join("README.md"), "hello").unwrap();
        git(&folder, &["add", "README.md"]);
        assert!(git(&folder, &["commit", "-q", "-m", "init"]).status.success());
        let head_before = git(&folder, &["rev-parse", "HEAD"]).stdout;

        let repositories: RwLock<HashMap<Uuid, Repository>> = RwLock::new(HashMap::new());
        let result = create_repository_core(
            &repositories,
            &storage,
            folder.to_string_lossy().to_string(),
            None,
            true,
        )
        .await;

        assert!(result.is_ok(), "expected success, got {:?}", result.err());
        let head_after = git(&folder, &["rev-parse", "HEAD"]).stdout;
        assert_eq!(head_before, head_after, "existing repository history must survive untouched");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_init_failure_is_not_reported_as_project_creation_success() {
        use std::os::unix::fs::PermissionsExt;
        if crate::ipc::server::current_uid() == 0 {
            return; // root ignores permission bits
        }

        let (storage, temp) = create_test_storage();
        let folder = temp.path().join("project");
        std::fs::create_dir_all(&folder).unwrap();
        // A directory git cannot write into makes `git init` fail deterministically.
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o500)).unwrap();

        let repositories: RwLock<HashMap<Uuid, Repository>> = RwLock::new(HashMap::new());
        let result = create_repository_core(
            &repositories,
            &storage,
            folder.to_string_lossy().to_string(),
            None,
            true,
        )
        .await;

        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err(), "a failed git init must not be reported as success");
        assert!(
            repositories.read().await.is_empty(),
            "no repository must be registered when git init failed"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persistence_failure_after_successful_git_init_reports_partial_result_and_keeps_git() {
        use std::os::unix::fs::PermissionsExt;
        if crate::ipc::server::current_uid() == 0 {
            return;
        }

        let (storage, temp) = create_test_storage();
        storage.save_config(&AppConfig::default()).expect("seed config");
        let folder = temp.path().join("project");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::set_permissions(storage.paths().config_file(), std::fs::Permissions::from_mode(0o000)).unwrap();

        let repositories: RwLock<HashMap<Uuid, Repository>> = RwLock::new(HashMap::new());
        let result = create_repository_core(
            &repositories,
            &storage,
            folder.to_string_lossy().to_string(),
            None,
            true,
        )
        .await;

        std::fs::set_permissions(storage.paths().config_file(), std::fs::Permissions::from_mode(0o600)).unwrap();

        let err = result.expect_err("persistence failure must be reported, not swallowed as success");
        assert!(
            err.contains("Git repository was initialized"),
            "error must make clear git init already happened: {err}"
        );
        assert!(
            is_git_repo_root(&folder),
            "a failed registration must not delete the git repository it cannot undo"
        );
        assert!(
            repositories.read().await.is_empty(),
            "memory must not contain a repository disk never recorded"
        );
    }
}
