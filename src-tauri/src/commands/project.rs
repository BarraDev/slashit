use crate::domain::{Project, AgentType, AgentConfig};
use crate::config::Storage;
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

type Projects = Arc<RwLock<HashMap<Uuid, Project>>>;

/// Persist projects, reporting failure to the caller.
///
/// Most callers are content to log and continue, but a caller that has already
/// moved files on disk cannot: losing the write there would leave the project
/// pointing at the directory the data just left, and the user would open an
/// empty board with no indication that anything went wrong.
///
/// Loading the current config is not allowed to fail silently into
/// `AppConfig::default()`: a transient read failure (permissions, a momentary
/// I/O error) must not be papered over by writing back a blank config, which
/// would destroy `jj_config`, `ui_preferences`, `agent_configs`, and whichever
/// of projects/repositories this call was not updating. The error is
/// propagated instead so the caller can surface it rather than silently
/// "succeeding" at wiping the user's config.
///
/// The read and the write go through `Storage::update_config` so they are one
/// transaction. Loading the config here and saving it back as two separate
/// steps would let a concurrent repository save — which holds a different
/// in-memory lock and so is not excluded by the caller's `projects` guard —
/// land in between, and whichever write went second would revert the other's
/// section.
pub(crate) fn try_persist_projects(
    storage: &Storage,
    projects: &HashMap<Uuid, Project>,
) -> anyhow::Result<()> {
    // No extra `context` here on purpose: every caller formats this error
    // with `{e}`, which renders only the outermost context, so wrapping it
    // would replace the stage that actually failed ("Failed to load config
    // before updating it" / "Failed to write config file") with a tautology.
    // The callers already name the operation.
    storage.update_config(|config| {
        config.projects = projects
            .iter()
            .map(|(id, project)| (id.to_string(), project.clone()))
            .collect();
    })
}

#[derive(Clone)]
pub struct ProjectState {
    pub projects: Projects,
}

impl ProjectState {
    pub fn new() -> Self {
        Self {
            projects: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for ProjectState {
    fn default() -> Self {
        Self::new()
    }
}

fn get_default_agent_config(agent_type: &AgentType) -> AgentConfig {
    let (command, args) = match agent_type {
        AgentType::ClaudeCode => ("claude", vec!["--stdio"]),
        AgentType::Cursor => ("cursor-agent", vec!["--stdio"]),
        AgentType::Cody => ("cody-agent", vec!["--stdio"]),
        AgentType::Continue => ("continue-agent", vec!["--stdio"]),
        AgentType::Other(_) => ("unknown", vec![]),
    };

    AgentConfig {
        agent_type: agent_type.clone(),
        command: command.to_string(),
        args: args.into_iter().map(String::from).collect(),
        env: HashMap::new(),
        model: None,
        api_key: None,
    }
}

#[tauri::command]
pub async fn create_project(
    state: tauri::State<'_, crate::AppState>,
    name: String,
    repository_id: Option<String>,
    agent_type: AgentType,
) -> Result<Project, String> {
    let id = Uuid::new_v4();
    let repository_id = repository_id
        .and_then(|r| Uuid::parse_str(&r).ok());

    let agent_config = get_default_agent_config(&agent_type);
    let now = chrono::Utc::now();

    let project = Project {
        id,
        name,
        repository_id,
        scope: crate::domain::ProjectScope::Standalone,
        // New projects keep the repository pristine. Storing the board inside
        // the project is a deliberate opt-in, never the default.
        state_location: crate::config::paths::StateLocation::External,
        agent_type,
        agent_config,
        created_at: now,
        updated_at: now,
    };

    create_project_committed(&state.project.projects, &state.storage, project).await
}

/// Core of [`create_project`], taking the raw map and storage so tests can
/// call it without a `tauri::State`.
///
/// Inserts into a cloned map, persists the clone, and only replaces the
/// shared map on success — a failed persist never leaves memory holding a
/// project the caller was told did not get created.
async fn create_project_committed(
    projects: &RwLock<HashMap<Uuid, Project>>,
    storage: &Storage,
    project: Project,
) -> Result<Project, String> {
    let mut projects_w = projects.write().await;
    let mut proposed = projects_w.clone();
    proposed.insert(project.id, project.clone());

    try_persist_projects(storage, &proposed)
        .map_err(|e| format!("Failed to persist project: {e}"))?;

    *projects_w = proposed;
    Ok(project)
}

#[tauri::command]
pub async fn list_projects(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<Project>, String> {
    let projects = state.project.projects.read().await;
    Ok(projects.values().cloned().collect())
}

#[tauri::command]
pub async fn get_project(
    state: tauri::State<'_, crate::AppState>,
    id: String,
) -> Result<Option<Project>, String> {
    let id = Uuid::parse_str(&id).map_err(|e| e.to_string())?;
    let projects = state.project.projects.read().await;
    Ok(projects.get(&id).cloned())
}

#[tauri::command]
pub async fn delete_project(
    state: tauri::State<'_, crate::AppState>,
    id: String,
) -> Result<bool, String> {
    let id = Uuid::parse_str(&id).map_err(|e| e.to_string())?;
    delete_project_committed(&state.project.projects, &state.storage, id).await
}

/// Core of [`delete_project`]; see [`create_project_committed`] for why the
/// map is cloned first. The task files are only deleted once the persisted
/// map no longer references the project — a failed persist must leave both
/// memory and the on-disk task files untouched.
async fn delete_project_committed(
    projects: &RwLock<HashMap<Uuid, Project>>,
    storage: &Storage,
    id: Uuid,
) -> Result<bool, String> {
    let mut projects_w = projects.write().await;
    let mut proposed = projects_w.clone();
    let removed = proposed.remove(&id).is_some();

    if removed {
        try_persist_projects(storage, &proposed)
            .map_err(|e| format!("Failed to persist project deletion: {e}"))?;

        *projects_w = proposed;

        // Also delete associated tasks file
        if let Err(e) = storage.delete_project_tasks(id) {
            eprintln!("Warning: Failed to delete project tasks: {}", e);
        }
    }

    Ok(removed)
}

#[tauri::command]
pub async fn update_project(
    state: tauri::State<'_, crate::AppState>,
    id: String,
    name: Option<String>,
    repository_id: Option<String>,
    agent_type: Option<AgentType>,
) -> Result<Option<Project>, String> {
    let id = Uuid::parse_str(&id).map_err(|e| e.to_string())?;
    update_project_committed(&state.project.projects, &state.storage, id, name, repository_id, agent_type).await
}

/// Core of [`update_project`]; see [`create_project_committed`] for why the
/// map is cloned first. A failed persist never leaves memory holding an
/// update the caller was told did not apply.
async fn update_project_committed(
    projects: &RwLock<HashMap<Uuid, Project>>,
    storage: &Storage,
    id: Uuid,
    name: Option<String>,
    repository_id: Option<String>,
    agent_type: Option<AgentType>,
) -> Result<Option<Project>, String> {
    let mut projects_w = projects.write().await;
    let mut proposed = projects_w.clone();

    if let Some(project) = proposed.get_mut(&id) {
        if let Some(name) = name {
            project.name = name;
        }
        if let Some(repository_id) = repository_id {
            project.repository_id = Uuid::parse_str(&repository_id).ok();
        }
        if let Some(agent_type) = agent_type {
            project.agent_type = agent_type.clone();
            project.agent_config = get_default_agent_config(&agent_type);
        }
        project.updated_at = chrono::Utc::now();
        let result = project.clone();

        try_persist_projects(storage, &proposed)
            .map_err(|e| format!("Failed to persist project update: {e}"))?;

        *projects_w = proposed;
        Ok(Some(result))
    } else {
        Ok(None)
    }
}

/// Get the working directory path for a project by looking up its repository
#[tauri::command]
pub async fn get_project_path(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<Option<String>, String> {
    let id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    
    // Look up the project
    let projects = state.project.projects.read().await;
    let project = match projects.get(&id) {
        Some(p) => p.clone(),
        None => return Ok(None),
    };
    drop(projects);
    
    // If project has a repository_id, look up the repository's local_path
    if let Some(repo_id) = project.repository_id {
        let repositories = state.repository.repositories.read().await;
        if let Some(repo) = repositories.get(&repo_id) {
            return Ok(Some(repo.local_path.clone()));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::paths::{AppPaths, StateLocation};
    use crate::config::storage::AppConfig;
    use crate::domain::ProjectScope;
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

    fn make_test_project(id: Uuid) -> Project {
        Project {
            id,
            name: "Test Project".to_string(),
            repository_id: None,
            scope: ProjectScope::Standalone,
            state_location: StateLocation::External,
            agent_type: AgentType::ClaudeCode,
            agent_config: get_default_agent_config(&AgentType::ClaudeCode),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn try_persist_projects_refuses_to_reduce_a_config_it_cannot_fully_parse() {
        // End-to-end counterpart to the storage-level test: the production
        // writer must surface the refusal rather than committing a
        // partially-recovered config over the real one. Built by serializing a
        // real config and invalidating one enum variant, so every other
        // section keeps the shape the app actually writes.
        let (storage, _temp) = create_test_storage();

        let mut seeded = AppConfig::default();
        seeded.repositories.insert(
            "11111111-1111-1111-1111-111111111111".to_string(),
            crate::domain::Repository {
                id: Uuid::nil(),
                local_path: "/home/user/repo".to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        let valid = toml::to_string_pretty(&seeded).expect("serialize fixture");
        let poisoned = valid.replace("placement = \"auto\"", "placement = \"shared_root\"");
        assert_ne!(poisoned, valid, "fixture must invalidate the placement variant");
        std::fs::write(storage.paths().config_file(), &poisoned).expect("write fixture");

        let mut projects = HashMap::new();
        let id = Uuid::new_v4();
        projects.insert(id, make_test_project(id));

        let result = try_persist_projects(&storage, &projects);

        assert!(
            result.is_err(),
            "persisting must fail rather than write a config rebuilt from defaults"
        );
        let on_disk =
            std::fs::read_to_string(storage.paths().config_file()).expect("config still readable");
        assert_eq!(on_disk, poisoned, "the config on disk must be untouched");
        assert!(
            on_disk.contains("/home/user/repo"),
            "the repository section must survive the refused write"
        );
    }

    #[test]
    fn try_persist_projects_missing_config_file_initializes_from_defaults() {
        // No config.toml exists yet — this must behave exactly like today:
        // an `Ok` default config that the new projects are written into.
        let (storage, _temp) = create_test_storage();

        let mut projects = HashMap::new();
        let id = Uuid::new_v4();
        projects.insert(id, make_test_project(id));

        try_persist_projects(&storage, &projects).expect("should succeed against a missing config");

        let loaded = storage.load_config().expect("should load the freshly written config");
        assert_eq!(loaded.projects.len(), 1);
        assert!(loaded.projects.contains_key(&id.to_string()));
        // Everything else should still be plain defaults.
        assert_eq!(loaded.ui_preferences.theme, AppConfig::default().ui_preferences.theme);
    }

    #[cfg(unix)]
    #[test]
    fn try_persist_projects_propagates_read_failure_without_destroying_config() {
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

        let mut projects = HashMap::new();
        let id = Uuid::new_v4();
        projects.insert(id, make_test_project(id));

        let result = try_persist_projects(&storage, &projects);

        // Restore permissions so the file can be inspected and cleaned up.
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert!(result.is_err(), "a read failure must not be treated as success");
        let on_disk = std::fs::read(&config_path).unwrap();
        assert_eq!(on_disk, original_bytes, "config on disk must be untouched by a failed persist");
    }

    #[test]
    fn concurrent_project_and_repository_persists_never_lose_each_other() {
        // `config.toml` is a single file holding both sections, but a project
        // save and a repository save are serialized by two *different*
        // in-memory locks. Each helper replaces only its own section yet
        // rewrites the whole struct, so before `Storage::update_config` two
        // overlapping saves could both read the same config, both report
        // success, and leave whichever wrote second having reverted the
        // other's section. Every write is atomic, so nothing is ever torn —
        // the update is simply lost, which is why this needs a test rather
        // than showing up as corruption.
        //
        // Both sides start from a shared barrier so their read-modify-write
        // windows actually overlap. The assertion runs after *every* round,
        // not just at the end: each helper persists the full accumulated map,
        // so a section lost in one round would be rewritten by the next
        // round's save and never observed by a single final check.
        use crate::commands::repository::try_persist_repositories;
        use crate::domain::Repository;

        let (storage, _temp) = create_test_storage();

        const ROUNDS: usize = 40;
        let mut projects: HashMap<Uuid, Project> = HashMap::new();
        let mut repositories: HashMap<Uuid, Repository> = HashMap::new();

        for round in 1..=ROUNDS {
            let project_id = Uuid::new_v4();
            projects.insert(project_id, make_test_project(project_id));

            let repository_id = Uuid::new_v4();
            repositories.insert(
                repository_id,
                Repository {
                    id: repository_id,
                    local_path: format!("/repo/{round}"),
                    remote_url: None,
                    remote_type: None,
                    created_at: chrono::Utc::now(),
                },
            );

            let barrier = Arc::new(std::sync::Barrier::new(2));

            let project_side = {
                let storage = storage.clone();
                let projects = projects.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    try_persist_projects(&storage, &projects)
                })
            };

            let repository_side = {
                let storage = storage.clone();
                let repositories = repositories.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    try_persist_repositories(&storage, &repositories)
                })
            };

            project_side
                .join()
                .expect("project persist thread should not panic")
                .expect("project persist should succeed");
            repository_side
                .join()
                .expect("repository persist thread should not panic")
                .expect("repository persist should succeed");

            let on_disk = storage.load_config().expect("config should be readable");
            assert_eq!(
                on_disk.projects.len(),
                round,
                "round {round}: a concurrent repository save reverted the projects section"
            );
            assert_eq!(
                on_disk.repositories.len(),
                round,
                "round {round}: a concurrent project save reverted the repositories section"
            );
        }
    }

    /// Make `config.toml` exist but unreadable, so `try_persist_projects`
    /// fails deep inside its own `load_config()` call — the same forcing
    /// technique the read-failure test above uses, reused here to prove the
    /// *committed helpers* leave memory untouched on that failure, not just
    /// that the persistence function itself reports it.
    #[cfg(unix)]
    fn make_storage_with_unreadable_config() -> (Storage, TempDir) {
        use std::os::unix::fs::PermissionsExt;
        let (storage, temp) = create_test_storage();
        storage.save_config(&AppConfig::default()).expect("seed config");
        std::fs::set_permissions(storage.paths().config_file(), std::fs::Permissions::from_mode(0o000)).unwrap();
        (storage, temp)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_project_leaves_memory_unchanged_when_persistence_fails() {
        if crate::ipc::server::current_uid() == 0 {
            return;
        }
        let (storage, _temp) = make_storage_with_unreadable_config();
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::new());
        let new_project = make_test_project(Uuid::new_v4());

        let result = create_project_committed(&projects, &storage, new_project.clone()).await;

        assert!(result.is_err(), "a persistence failure must be reported, not swallowed");
        assert!(
            projects.read().await.is_empty(),
            "memory must not contain a project disk never recorded"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_project_leaves_memory_and_task_files_unchanged_when_persistence_fails() {
        if crate::ipc::server::current_uid() == 0 {
            return;
        }
        let (storage, _temp) = make_storage_with_unreadable_config();
        let existing = make_test_project(Uuid::new_v4());
        let id = existing.id;
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::from([(id, existing)]));

        let result = delete_project_committed(&projects, &storage, id).await;

        assert!(result.is_err(), "a persistence failure must be reported, not swallowed");
        assert!(
            projects.read().await.contains_key(&id),
            "memory must still contain the project a failed deletion could not persist"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn update_project_leaves_memory_unchanged_when_persistence_fails() {
        if crate::ipc::server::current_uid() == 0 {
            return;
        }
        let (storage, _temp) = make_storage_with_unreadable_config();
        let existing = make_test_project(Uuid::new_v4());
        let id = existing.id;
        let original_name = existing.name.clone();
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::from([(id, existing)]));

        let result = update_project_committed(
            &projects, &storage, id, Some("New Name".to_string()), None, None,
        ).await;

        assert!(result.is_err(), "a persistence failure must be reported, not swallowed");
        assert_eq!(
            projects.read().await.get(&id).unwrap().name,
            original_name,
            "memory must not contain an update disk never recorded"
        );
    }
}
