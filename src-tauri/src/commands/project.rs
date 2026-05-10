use crate::domain::{Project, AgentType, AgentConfig};
use crate::config::Storage;
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

type Projects = Arc<RwLock<HashMap<Uuid, Project>>>;

/// Helper function to persist projects to config file after mutation
fn persist_projects(storage: &Storage, projects: &HashMap<Uuid, Project>) {
    // Load current config
    let mut config = storage.load_config().unwrap_or_default();
    
    // Update projects in config (convert Uuid keys to String keys)
    config.projects = projects
        .iter()
        .map(|(id, project)| (id.to_string(), project.clone()))
        .collect();
    
    // Save config
    if let Err(e) = storage.save_config(&config) {
        eprintln!("Warning: Failed to persist projects: {}", e);
    }
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
        agent_type,
        agent_config,
        created_at: now,
        updated_at: now,
    };

    let mut projects = state.project.projects.write().await;
    projects.insert(id, project.clone());
    
    // Persist to disk
    persist_projects(&state.storage, &projects);
    
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
    let mut projects = state.project.projects.write().await;
    let removed = projects.remove(&id).is_some();
    
    // Persist to disk
    if removed {
        persist_projects(&state.storage, &projects);
        
        // Also delete associated tasks file
        if let Err(e) = state.storage.delete_project_tasks(id) {
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
    let mut projects = state.project.projects.write().await;

    if let Some(project) = projects.get_mut(&id) {
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
        let result = Some(project.clone());
        
        // Persist to disk
        persist_projects(&state.storage, &projects);
        
        Ok(result)
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
