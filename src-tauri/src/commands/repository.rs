use crate::domain::Repository;
use crate::config::Storage;
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

type Repositories = Arc<RwLock<HashMap<Uuid, Repository>>>;

/// Helper function to persist repositories to config file after mutation
fn persist_repositories(storage: &Storage, repositories: &HashMap<Uuid, Repository>) {
    // Load current config
    let mut config = storage.load_config().unwrap_or_default();
    
    // Update repositories in config (convert Uuid keys to String keys)
    config.repositories = repositories
        .iter()
        .map(|(id, repository)| (id.to_string(), repository.clone()))
        .collect();
    
    // Save config
    if let Err(e) = storage.save_config(&config) {
        eprintln!("Warning: Failed to persist repositories: {}", e);
    }
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

    let mut repositories = state.repository.repositories.write().await;
    repositories.insert(id, repository.clone());
    
    // Persist to disk
    persist_repositories(&state.storage, &repositories);
    
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
