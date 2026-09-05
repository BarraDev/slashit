use crate::domain::Repository;
use crate::config::Storage;
use anyhow::Context;
use uuid::Uuid;
use std::collections::HashMap;
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
fn try_persist_repositories(
    storage: &Storage,
    repositories: &HashMap<Uuid, Repository>,
) -> anyhow::Result<()> {
    let mut config = storage
        .load_config()
        .context("Failed to load config before persisting repositories")?;

    // Update repositories in config (convert Uuid keys to String keys)
    config.repositories = repositories
        .iter()
        .map(|(id, repository)| (id.to_string(), repository.clone()))
        .collect();

    storage.save_config(&config)
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

#[tauri::command]
pub async fn create_repository(
    state: tauri::State<'_, crate::AppState>,
    local_path: String,
    remote_url: Option<String>,
) -> Result<Repository, String> {
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
        local_path,
        remote_url,
        remote_type,
        created_at: chrono::Utc::now(),
    };

    create_repository_committed(&state.repository.repositories, &state.storage, repository).await
}

/// Core of [`create_repository`], taking the raw map and storage so tests
/// can call it without a `tauri::State`.
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

        if unsafe { libc_geteuid() } == 0 {
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
        if unsafe { libc_geteuid() } == 0 {
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

    #[cfg(unix)]
    extern "C" {
        #[link_name = "geteuid"]
        fn libc_geteuid() -> u32;
    }
}
