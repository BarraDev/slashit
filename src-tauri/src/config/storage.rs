use crate::domain::{Project, AgentConfig, Task, Repository};
use crate::domain::task::ExternalRef;
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

const APP_NAME: &str = "slashit-app";

/// Structure for storing tasks per project in TOML files
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectTasksFile {
    pub version: u32,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub projects: HashMap<String, Project>,
    #[serde(default)]
    pub repositories: HashMap<String, Repository>,
    #[serde(default)]
    pub agent_configs: HashMap<String, AgentConfig>,
    #[serde(default)]
    pub jj_config: JjConfig,
    #[serde(default)]
    pub ui_preferences: UiPreferences,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JjConfig {
    #[serde(default)]
    pub user_name: Option<String>,
    #[serde(default)]
    pub user_email: Option<String>,
    #[serde(default = "default_branch")]
    pub default_branch: String,
}

fn default_branch() -> String {
    "main".to_string()
}

impl Default for JjConfig {
    fn default() -> Self {
        Self {
            user_name: None,
            user_email: None,
            default_branch: default_branch(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiPreferences {
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: u32,
}

fn default_theme() -> String {
    "dark".to_string()
}

fn default_sidebar_width() -> u32 {
    300
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            sidebar_width: default_sidebar_width(),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            projects: HashMap::new(),
            repositories: HashMap::new(),
            agent_configs: HashMap::new(),
            jj_config: JjConfig {
                user_name: None,
                user_email: None,
                default_branch: "main".to_string(),
            },
            ui_preferences: UiPreferences {
                theme: "dark".to_string(),
                sidebar_width: 300,
            },
        }
    }
}

fn parse_github_url(url: &str, is_pr: bool) -> Option<ExternalRef> {
    // Expected format: https://github.com/owner/repo/issues/123 or .../pull/123
    let segment = if is_pr { "/pull/" } else { "/issues/" };
    let number_str = url.split(segment).last()?;
    let number = number_str.parse::<u32>().ok()?;
    let repo_part = url.split("github.com/").last()?;
    let repo = repo_part.split(segment).next()?.to_string();

    if is_pr {
        Some(ExternalRef::GithubPr { url: url.to_string(), number, repo, state: None })
    } else {
        Some(ExternalRef::GithubIssue { url: url.to_string(), number, repo, state: None })
    }
}

fn migrate_task_refs(task: &mut Task) {
    if !task.external_refs.is_empty() { return; }

    if let Some(url) = task.github_issue_url.as_ref() {
        if let Some(r) = parse_github_url(url, false) { task.external_refs.push(r); }
    }
    if let Some(url) = task.pr_url.as_ref() {
        if let Some(r) = parse_github_url(url, true) { task.external_refs.push(r); }
    }
    if let Some(key) = task.jira_issue_key.as_ref() {
        let project = key.split('-').next().unwrap_or("").to_string();
        task.external_refs.push(ExternalRef::JiraTicket { key: key.clone(), project });
    }
    if let Some(id) = task.linear_ticket_id.as_ref() {
        task.external_refs.push(ExternalRef::LinearTicket { id: id.clone() });
    }
    if let Some(url) = task.gitlab_issue_url.as_ref() {
        task.external_refs.push(ExternalRef::GitlabIssue { url: url.clone() });
    }
}

#[derive(Clone)]
pub struct Storage {
    config_dir: PathBuf,
    config_file: PathBuf,
}

impl Storage {
    pub fn new() -> Result<Self> {
        let proj_dirs = ProjectDirs::from("com", "barradev", APP_NAME)
            .context("Failed to get project directories")?;

        let config_dir = proj_dirs.config_dir().to_path_buf();
        let config_file = config_dir.join("config.toml");

        fs::create_dir_all(&config_dir)
            .context("Failed to create config directory")?;

        Ok(Self {
            config_dir,
            config_file,
        })
    }

    pub fn load_config(&self) -> Result<AppConfig> {
        if self.config_file.exists() {
            let contents = fs::read_to_string(&self.config_file)
                .context("Failed to read config file")?;
            
            // Try to parse the config file
            match toml::from_str::<AppConfig>(&contents) {
                Ok(config) => Ok(config),
                Err(parse_error) => {
                    // Log detailed error information
                    eprintln!("Config parse error details:");
                    eprintln!("  Error: {}", parse_error);
                    if let Some(span) = parse_error.span() {
                        eprintln!("  Location: bytes {}..{}", span.start, span.end);
                        // Try to show the problematic section
                        if span.start < contents.len() {
                            let context_start = span.start.saturating_sub(50);
                            let context_end = (span.end + 50).min(contents.len());
                            eprintln!("  Context: ...{}...", &contents[context_start..context_end]);
                        }
                    }
                    
                    // Try partial recovery: parse as generic TOML value first
                    if let Ok(value) = toml::from_str::<toml::Value>(&contents) {
                        eprintln!("  Config file is valid TOML but doesn't match AppConfig structure");
                        eprintln!("  Top-level keys: {:?}", value.as_table().map(|t| t.keys().collect::<Vec<_>>()));
                        
                        // Attempt to extract what we can
                        let mut config = AppConfig::default();
                        
                        // Try to recover UI preferences
                        if let Some(ui) = value.get("ui_preferences") {
                            if let Ok(ui_prefs) = ui.clone().try_into::<UiPreferences>() {
                                config.ui_preferences = ui_prefs;
                                eprintln!("  Recovered: ui_preferences");
                            }
                        }
                        
                        // Try to recover JJ config
                        if let Some(jj) = value.get("jj_config") {
                            if let Ok(jj_config) = jj.clone().try_into::<JjConfig>() {
                                config.jj_config = jj_config;
                                eprintln!("  Recovered: jj_config");
                            }
                        }
                        
                        eprintln!("  Using partial recovery with defaults for unrecoverable sections");
                        
                        // Backup the old config before we potentially overwrite it
                        let backup_path = self.config_file.with_extension("toml.backup");
                        if let Err(e) = fs::copy(&self.config_file, &backup_path) {
                            eprintln!("  Warning: Failed to backup old config: {}", e);
                        } else {
                            eprintln!("  Backed up old config to: {:?}", backup_path);
                        }
                        
                        return Ok(config);
                    }
                    
                    // Config file is corrupted/invalid TOML - backup and reset
                    eprintln!("  Config file is not valid TOML, backing up and resetting");
                    let backup_path = self.config_file.with_extension("toml.corrupted");
                    if let Err(e) = fs::rename(&self.config_file, &backup_path) {
                        eprintln!("  Warning: Failed to backup corrupted config: {}", e);
                    } else {
                        eprintln!("  Moved corrupted config to: {:?}", backup_path);
                    }
                    
                    Ok(AppConfig::default())
                }
            }
        } else {
            Ok(AppConfig::default())
        }
    }

    pub fn save_config(&self, config: &AppConfig) -> Result<()> {
        let contents = toml::to_string_pretty(config)
            .context("Failed to serialize config")?;
        fs::write(&self.config_file, contents)
            .context("Failed to write config file")?;
        Ok(())
    }

    pub fn config_dir(&self) -> &PathBuf {
        &self.config_dir
    }
}

impl Default for Storage {
    fn default() -> Self {
        Self::new().expect("Failed to create storage")
    }
}

impl Storage {
    /// Get the directory for storing task files
    pub fn tasks_dir(&self) -> PathBuf {
        self.config_dir.join("tasks")
    }

    /// Get the path to a project's tasks file
    fn project_tasks_path(&self, project_id: Uuid) -> PathBuf {
        self.tasks_dir().join(format!("{}.toml", project_id))
    }

    /// Load all tasks from all project files
    pub fn load_all_tasks(&self) -> Result<Vec<Task>> {
        let tasks_dir = self.tasks_dir();
        if !tasks_dir.exists() {
            return Ok(Vec::new());
        }

        let mut all_tasks = Vec::new();
        
        for entry in fs::read_dir(&tasks_dir).context("Failed to read tasks directory")? {
            let entry = entry?;
            let path = entry.path();
            
            if path.extension().is_some_and(|ext| ext == "toml") {
                match fs::read_to_string(&path) {
                    Ok(contents) => {
                        match toml::from_str::<ProjectTasksFile>(&contents) {
                            Ok(tasks_file) => {
                                let mut tasks = tasks_file.tasks;
                                for task in &mut tasks {
                                    migrate_task_refs(task);
                                }
                                all_tasks.extend(tasks);
                            }
                            Err(e) => {
                                eprintln!("Warning: Failed to parse tasks file {:?}: {}", path, e);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Warning: Failed to read tasks file {:?}: {}", path, e);
                    }
                }
            }
        }

        Ok(all_tasks)
    }

    /// Load tasks for a specific project
    pub fn load_project_tasks(&self, project_id: Uuid) -> Result<Vec<Task>> {
        let path = self.project_tasks_path(project_id);
        
        if !path.exists() {
            return Ok(Vec::new());
        }

        let contents = fs::read_to_string(&path)
            .context("Failed to read project tasks file")?;
        
        let tasks_file: ProjectTasksFile = toml::from_str(&contents)
            .context("Failed to parse project tasks file")?;

        let mut tasks = tasks_file.tasks;
        for task in &mut tasks {
            migrate_task_refs(task);
        }
        Ok(tasks)
    }

    /// Save tasks for a specific project
    pub fn save_project_tasks(&self, project_id: Uuid, tasks: &[Task]) -> Result<()> {
        let tasks_dir = self.tasks_dir();
        fs::create_dir_all(&tasks_dir)
            .context("Failed to create tasks directory")?;

        let path = self.project_tasks_path(project_id);
        
        let tasks_file = ProjectTasksFile {
            version: 1,
            tasks: tasks.to_vec(),
        };

        let contents = toml::to_string_pretty(&tasks_file)
            .context("Failed to serialize tasks")?;
        
        fs::write(&path, contents)
            .context("Failed to write tasks file")?;

        Ok(())
    }

    /// Delete tasks file for a project
    pub fn delete_project_tasks(&self, project_id: Uuid) -> Result<()> {
        let path = self.project_tasks_path(project_id);
        if path.exists() {
            fs::remove_file(&path)
                .context("Failed to delete project tasks file")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create a test storage instance with a temporary directory
    fn create_test_storage() -> (Storage, TempDir) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_dir = temp_dir.path().to_path_buf();
        let config_file = config_dir.join("config.toml");
        
        let storage = Storage {
            config_dir,
            config_file,
        };
        
        (storage, temp_dir)
    }

    // ==================== AppConfig Serde Default Tests ====================

    #[test]
    fn test_appconfig_default_values() {
        let config = AppConfig::default();
        
        assert!(config.projects.is_empty());
        assert!(config.repositories.is_empty());
        assert!(config.agent_configs.is_empty());
        assert_eq!(config.jj_config.default_branch, "main");
        assert_eq!(config.ui_preferences.theme, "dark");
        assert_eq!(config.ui_preferences.sidebar_width, 300);
    }

    #[test]
    fn test_parse_empty_toml_uses_defaults() {
        // Empty TOML should use all defaults
        let toml_str = "";
        let config: AppConfig = toml::from_str(toml_str).expect("Should parse empty TOML");
        
        assert!(config.projects.is_empty());
        assert!(config.repositories.is_empty());
        assert_eq!(config.jj_config.default_branch, "main");
        assert_eq!(config.ui_preferences.theme, "dark");
    }

    #[test]
    fn test_parse_toml_with_missing_fields() {
        // TOML with only some fields - others should use defaults
        let toml_str = r#"
[ui_preferences]
theme = "light"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Should parse partial TOML");
        
        // Specified values should be used
        assert_eq!(config.ui_preferences.theme, "light");
        // Missing sidebar_width should use default
        assert_eq!(config.ui_preferences.sidebar_width, 300);
        // Missing sections should use defaults
        assert!(config.projects.is_empty());
        assert!(config.repositories.is_empty());
        assert_eq!(config.jj_config.default_branch, "main");
    }

    #[test]
    fn test_parse_toml_with_only_jj_config() {
        let toml_str = r#"
[jj_config]
user_name = "Test User"
user_email = "test@example.com"
default_branch = "develop"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Should parse jj_config only");
        
        assert_eq!(config.jj_config.user_name, Some("Test User".to_string()));
        assert_eq!(config.jj_config.user_email, Some("test@example.com".to_string()));
        assert_eq!(config.jj_config.default_branch, "develop");
        // Other sections should have defaults
        assert!(config.projects.is_empty());
        assert_eq!(config.ui_preferences.theme, "dark");
    }

    #[test]
    fn test_parse_toml_without_repositories_field() {
        // This simulates an old config file that was created before the repositories field existed
        let toml_str = r#"
[ui_preferences]
theme = "dark"
sidebar_width = 350

[jj_config]
default_branch = "main"
"#;
        let config: AppConfig = toml::from_str(toml_str).expect("Should parse without repositories");
        
        // repositories should default to empty HashMap
        assert!(config.repositories.is_empty());
        assert!(config.projects.is_empty());
        assert_eq!(config.ui_preferences.sidebar_width, 350);
    }

    // ==================== Storage Load/Save Tests ====================

    #[test]
    fn test_load_config_no_file_returns_default() {
        let (storage, _temp) = create_test_storage();
        
        let config = storage.load_config().expect("Should load default config");
        
        assert!(config.projects.is_empty());
        assert_eq!(config.ui_preferences.theme, "dark");
    }

    #[test]
    fn test_save_and_load_config_roundtrip() {
        let (storage, _temp) = create_test_storage();
        
        // Create a config with some data
        let mut config = AppConfig::default();
        config.ui_preferences.theme = "custom-theme".to_string();
        config.ui_preferences.sidebar_width = 400;
        config.jj_config.user_name = Some("Test User".to_string());
        config.jj_config.default_branch = "develop".to_string();
        
        // Save it
        storage.save_config(&config).expect("Should save config");
        
        // Load it back
        let loaded = storage.load_config().expect("Should load config");
        
        assert_eq!(loaded.ui_preferences.theme, "custom-theme");
        assert_eq!(loaded.ui_preferences.sidebar_width, 400);
        assert_eq!(loaded.jj_config.user_name, Some("Test User".to_string()));
        assert_eq!(loaded.jj_config.default_branch, "develop");
    }

    #[test]
    fn test_load_config_with_valid_toml_wrong_structure() {
        let (storage, _temp) = create_test_storage();
        
        // Create a TOML file with valid TOML but unexpected structure
        // This simulates a config from a different version of the app
        let weird_toml = r#"
[ui_preferences]
theme = "light"
sidebar_width = 250

[some_unknown_section]
foo = "bar"
baz = 123
"#;
        fs::write(&storage.config_file, weird_toml).expect("Should write test file");
        
        // Should still load successfully, using defaults for missing/invalid parts
        let config = storage.load_config().expect("Should load with recovery");
        
        // The valid parts should be recovered
        assert_eq!(config.ui_preferences.theme, "light");
        assert_eq!(config.ui_preferences.sidebar_width, 250);
    }

    #[test]
    fn test_load_config_corrupted_toml_returns_default() {
        let (storage, _temp) = create_test_storage();
        
        // Write completely invalid TOML
        let corrupted = "this is not valid TOML { [ ] } @#$%";
        fs::write(&storage.config_file, corrupted).expect("Should write test file");
        
        // Should return default config (after backing up the corrupted file)
        let config = storage.load_config().expect("Should handle corrupted config");
        
        assert!(config.projects.is_empty());
        assert_eq!(config.ui_preferences.theme, "dark");
        
        // Check that a backup was created
        let backup_path = storage.config_file.with_extension("toml.corrupted");
        assert!(backup_path.exists(), "Corrupted config should be backed up");
    }

    // ==================== Project Persistence Tests ====================

    #[test]
    fn test_project_persistence_in_config() {
        use crate::domain::{Project, AgentType, AgentConfig};
        use std::collections::HashMap;
        
        let (storage, _temp) = create_test_storage();
        
        // Create a config with projects
        let mut config = AppConfig::default();
        let project_id = Uuid::new_v4();
        let project = Project {
            id: project_id,
            name: "Test Project".to_string(),
            repository_id: None,
            agent_type: AgentType::ClaudeCode,
            agent_config: AgentConfig {
                agent_type: AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: vec![],
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        config.projects.insert(project_id.to_string(), project);
        
        // Save and reload
        storage.save_config(&config).expect("Should save config");
        let loaded = storage.load_config().expect("Should load config");
        
        assert_eq!(loaded.projects.len(), 1);
        assert!(loaded.projects.contains_key(&project_id.to_string()));
        assert_eq!(loaded.projects.get(&project_id.to_string()).unwrap().name, "Test Project");
    }

    // ==================== Repository Persistence Tests ====================

    #[test]
    fn test_repository_persistence_in_config() {
        use crate::domain::{Repository, RemoteType};
        
        let (storage, _temp) = create_test_storage();
        
        // Create a config with repositories
        let mut config = AppConfig::default();
        let repo_id = Uuid::new_v4();
        let repo = Repository {
            id: repo_id,
            local_path: "/path/to/repo".to_string(),
            remote_url: Some("https://github.com/test/repo".to_string()),
            remote_type: Some(RemoteType::GitHub),
            created_at: chrono::Utc::now(),
        };
        config.repositories.insert(repo_id.to_string(), repo);
        
        // Save and reload
        storage.save_config(&config).expect("Should save config");
        let loaded = storage.load_config().expect("Should load config");
        
        assert_eq!(loaded.repositories.len(), 1);
        assert!(loaded.repositories.contains_key(&repo_id.to_string()));
        assert_eq!(loaded.repositories.get(&repo_id.to_string()).unwrap().local_path, "/path/to/repo");
    }

    // ==================== JjConfig Tests ====================

    #[test]
    fn test_jjconfig_default() {
        let config = JjConfig::default();
        
        assert!(config.user_name.is_none());
        assert!(config.user_email.is_none());
        assert_eq!(config.default_branch, "main");
    }

    #[test]
    fn test_jjconfig_serde_defaults() {
        // Parse JjConfig with only some fields
        let toml_str = r#"
user_name = "Test"
"#;
        let config: JjConfig = toml::from_str(toml_str).expect("Should parse");
        
        assert_eq!(config.user_name, Some("Test".to_string()));
        assert!(config.user_email.is_none());
        assert_eq!(config.default_branch, "main"); // default
    }

    // ==================== UiPreferences Tests ====================

    #[test]
    fn test_uipreferences_default() {
        let prefs = UiPreferences::default();
        
        assert_eq!(prefs.theme, "dark");
        assert_eq!(prefs.sidebar_width, 300);
    }

    #[test]
    fn test_uipreferences_serde_defaults() {
        // Parse with only theme specified
        let toml_str = r#"theme = "light""#;
        let prefs: UiPreferences = toml::from_str(toml_str).expect("Should parse");
        
        assert_eq!(prefs.theme, "light");
        assert_eq!(prefs.sidebar_width, 300); // default
    }

    // ==================== Task Persistence Tests ====================

    #[test]
    fn test_save_and_load_project_tasks() {
        let (storage, _temp) = create_test_storage();
        
        let project_id = Uuid::new_v4();
        let task = crate::test_helpers::create_test_task("Test Task");
        let mut task = task;
        task.project_id = project_id;
        
        let tasks = vec![task.clone()];
        
        // Save tasks
        storage.save_project_tasks(project_id, &tasks).expect("Should save tasks");
        
        // Load tasks
        let loaded = storage.load_project_tasks(project_id).expect("Should load tasks");
        
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].title, "Test Task");
        assert_eq!(loaded[0].project_id, project_id);
    }

    #[test]
    fn test_load_nonexistent_project_tasks_returns_empty() {
        let (storage, _temp) = create_test_storage();
        
        let project_id = Uuid::new_v4();
        let tasks = storage.load_project_tasks(project_id).expect("Should return empty vec");
        
        assert!(tasks.is_empty());
    }

    #[test]
    fn test_delete_project_tasks() {
        let (storage, _temp) = create_test_storage();
        
        let project_id = Uuid::new_v4();
        let task = crate::test_helpers::create_test_task("Test Task");
        let tasks = vec![task];
        
        // Save tasks
        storage.save_project_tasks(project_id, &tasks).expect("Should save");
        
        // Verify file exists
        let path = storage.tasks_dir().join(format!("{}.toml", project_id));
        assert!(path.exists());
        
        // Delete tasks
        storage.delete_project_tasks(project_id).expect("Should delete");
        
        // Verify file is gone
        assert!(!path.exists());
    }

    #[test]
    fn test_load_all_tasks_from_multiple_projects() {
        let (storage, _temp) = create_test_storage();

        // Create tasks for two projects
        let project1_id = Uuid::new_v4();
        let project2_id = Uuid::new_v4();

        let mut task1 = crate::test_helpers::create_test_task("Task 1");
        task1.project_id = project1_id;

        let mut task2 = crate::test_helpers::create_test_task("Task 2");
        task2.project_id = project2_id;

        let mut task3 = crate::test_helpers::create_test_task("Task 3");
        task3.project_id = project1_id;

        // Save tasks
        storage.save_project_tasks(project1_id, &[task1, task3]).expect("Should save");
        storage.save_project_tasks(project2_id, &[task2]).expect("Should save");

        // Load all tasks
        let all_tasks = storage.load_all_tasks().expect("Should load all");

        assert_eq!(all_tasks.len(), 3);
    }

    // ==================== parse_github_url Tests ====================

    #[test]
    fn test_parse_github_url_valid_issue() {
        let result = parse_github_url("https://github.com/owner/repo/issues/123", false);
        assert!(result.is_some());
        match result.unwrap() {
            ExternalRef::GithubIssue { number, repo, url, state } => {
                assert_eq!(number, 123);
                assert_eq!(repo, "owner/repo");
                assert_eq!(url, "https://github.com/owner/repo/issues/123");
                assert!(state.is_none());
            }
            _ => panic!("Expected GithubIssue variant"),
        }
    }

    #[test]
    fn test_parse_github_url_valid_pr() {
        let result = parse_github_url("https://github.com/owner/repo/pull/456", true);
        assert!(result.is_some());
        match result.unwrap() {
            ExternalRef::GithubPr { number, repo, url, state } => {
                assert_eq!(number, 456);
                assert_eq!(repo, "owner/repo");
                assert_eq!(url, "https://github.com/owner/repo/pull/456");
                assert!(state.is_none());
            }
            _ => panic!("Expected GithubPr variant"),
        }
    }

    #[test]
    fn test_parse_github_url_trailing_slash() {
        // Trailing slash means the "number" part is empty string after split, which fails parse
        let result = parse_github_url("https://github.com/owner/repo/issues/123/", false);
        // The last segment after "/issues/" is "123/" which won't parse as u32
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_github_url_without_number() {
        let result = parse_github_url("https://github.com/owner/repo/issues/", false);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_github_url_non_numeric_number() {
        let result = parse_github_url("https://github.com/owner/repo/issues/abc", false);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_github_url_empty_string() {
        let result = parse_github_url("", false);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_github_url_completely_invalid() {
        let result = parse_github_url("https://example.com/not-github", false);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_github_url_nested_org_repo() {
        let result = parse_github_url("https://github.com/org/sub-repo/issues/1", false);
        assert!(result.is_some());
        match result.unwrap() {
            ExternalRef::GithubIssue { number, repo, .. } => {
                assert_eq!(number, 1);
                assert_eq!(repo, "org/sub-repo");
            }
            _ => panic!("Expected GithubIssue variant"),
        }
    }

    #[test]
    fn test_parse_github_url_large_number() {
        let result = parse_github_url("https://github.com/owner/repo/issues/999999", false);
        assert!(result.is_some());
        match result.unwrap() {
            ExternalRef::GithubIssue { number, .. } => {
                assert_eq!(number, 999999);
            }
            _ => panic!("Expected GithubIssue variant"),
        }
    }

    // ==================== migrate_task_refs Tests ====================

    #[test]
    fn test_migrate_task_refs_already_migrated_skipped() {
        let mut task = crate::test_helpers::create_test_task("Already Migrated");
        task.external_refs.push(ExternalRef::LinearTicket { id: "existing".to_string() });
        task.github_issue_url = Some("https://github.com/owner/repo/issues/1".to_string());

        migrate_task_refs(&mut task);

        // Should still have only the original ref, migration was skipped
        assert_eq!(task.external_refs.len(), 1);
        match &task.external_refs[0] {
            ExternalRef::LinearTicket { id } => assert_eq!(id, "existing"),
            _ => panic!("Expected original LinearTicket ref"),
        }
    }

    #[test]
    fn test_migrate_task_refs_github_issue_url() {
        let mut task = crate::test_helpers::create_test_task("GH Issue");
        task.github_issue_url = Some("https://github.com/owner/repo/issues/42".to_string());

        migrate_task_refs(&mut task);

        assert_eq!(task.external_refs.len(), 1);
        match &task.external_refs[0] {
            ExternalRef::GithubIssue { number, repo, .. } => {
                assert_eq!(*number, 42);
                assert_eq!(repo, "owner/repo");
            }
            _ => panic!("Expected GithubIssue"),
        }
    }

    #[test]
    fn test_migrate_task_refs_pr_url() {
        let mut task = crate::test_helpers::create_test_task("PR Task");
        task.pr_url = Some("https://github.com/owner/repo/pull/99".to_string());

        migrate_task_refs(&mut task);

        assert_eq!(task.external_refs.len(), 1);
        match &task.external_refs[0] {
            ExternalRef::GithubPr { number, repo, .. } => {
                assert_eq!(*number, 99);
                assert_eq!(repo, "owner/repo");
            }
            _ => panic!("Expected GithubPr"),
        }
    }

    #[test]
    fn test_migrate_task_refs_jira_issue_key() {
        let mut task = crate::test_helpers::create_test_task("Jira Task");
        task.jira_issue_key = Some("PROJ-123".to_string());

        migrate_task_refs(&mut task);

        assert_eq!(task.external_refs.len(), 1);
        match &task.external_refs[0] {
            ExternalRef::JiraTicket { key, project } => {
                assert_eq!(key, "PROJ-123");
                assert_eq!(project, "PROJ");
            }
            _ => panic!("Expected JiraTicket"),
        }
    }

    #[test]
    fn test_migrate_task_refs_linear_ticket_id() {
        let mut task = crate::test_helpers::create_test_task("Linear Task");
        task.linear_ticket_id = Some("LIN-456".to_string());

        migrate_task_refs(&mut task);

        assert_eq!(task.external_refs.len(), 1);
        match &task.external_refs[0] {
            ExternalRef::LinearTicket { id } => assert_eq!(id, "LIN-456"),
            _ => panic!("Expected LinearTicket"),
        }
    }

    #[test]
    fn test_migrate_task_refs_gitlab_issue_url() {
        let mut task = crate::test_helpers::create_test_task("GitLab Task");
        task.gitlab_issue_url = Some("https://gitlab.com/group/project/-/issues/77".to_string());

        migrate_task_refs(&mut task);

        assert_eq!(task.external_refs.len(), 1);
        match &task.external_refs[0] {
            ExternalRef::GitlabIssue { url } => {
                assert_eq!(url, "https://gitlab.com/group/project/-/issues/77");
            }
            _ => panic!("Expected GitlabIssue"),
        }
    }

    #[test]
    fn test_migrate_task_refs_all_legacy_fields() {
        let mut task = crate::test_helpers::create_test_task("All Fields");
        task.github_issue_url = Some("https://github.com/owner/repo/issues/10".to_string());
        task.pr_url = Some("https://github.com/owner/repo/pull/20".to_string());
        task.jira_issue_key = Some("PROJ-30".to_string());
        task.linear_ticket_id = Some("LIN-40".to_string());
        task.gitlab_issue_url = Some("https://gitlab.com/g/p/-/issues/50".to_string());

        migrate_task_refs(&mut task);

        assert_eq!(task.external_refs.len(), 5);
        // Verify order: GithubIssue, GithubPr, JiraTicket, LinearTicket, GitlabIssue
        assert!(matches!(&task.external_refs[0], ExternalRef::GithubIssue { number: 10, .. }));
        assert!(matches!(&task.external_refs[1], ExternalRef::GithubPr { number: 20, .. }));
        assert!(matches!(&task.external_refs[2], ExternalRef::JiraTicket { .. }));
        assert!(matches!(&task.external_refs[3], ExternalRef::LinearTicket { .. }));
        assert!(matches!(&task.external_refs[4], ExternalRef::GitlabIssue { .. }));
    }

    #[test]
    fn test_migrate_task_refs_no_legacy_fields() {
        let mut task = crate::test_helpers::create_test_task("Empty Task");

        migrate_task_refs(&mut task);

        assert!(task.external_refs.is_empty());
    }

    #[test]
    fn test_migrate_task_refs_malformed_github_url_skipped_others_migrate() {
        let mut task = crate::test_helpers::create_test_task("Malformed GH");
        task.github_issue_url = Some("not-a-valid-url".to_string());
        task.jira_issue_key = Some("PROJ-99".to_string());
        task.linear_ticket_id = Some("LIN-88".to_string());

        migrate_task_refs(&mut task);

        // Malformed github URL is skipped, but jira and linear still migrate
        assert_eq!(task.external_refs.len(), 2);
        assert!(matches!(&task.external_refs[0], ExternalRef::JiraTicket { .. }));
        assert!(matches!(&task.external_refs[1], ExternalRef::LinearTicket { .. }));
    }

    #[test]
    fn test_migrate_task_refs_jira_key_without_dash() {
        let mut task = crate::test_helpers::create_test_task("Jira No Dash");
        task.jira_issue_key = Some("SINGLE".to_string());

        migrate_task_refs(&mut task);

        assert_eq!(task.external_refs.len(), 1);
        match &task.external_refs[0] {
            ExternalRef::JiraTicket { key, project } => {
                assert_eq!(key, "SINGLE");
                assert_eq!(project, "SINGLE");
            }
            _ => panic!("Expected JiraTicket"),
        }
    }

    #[test]
    fn test_migrate_task_refs_jira_key_multiple_dashes() {
        let mut task = crate::test_helpers::create_test_task("Jira Multi Dash");
        task.jira_issue_key = Some("MY-PROJ-123".to_string());

        migrate_task_refs(&mut task);

        assert_eq!(task.external_refs.len(), 1);
        match &task.external_refs[0] {
            ExternalRef::JiraTicket { key, project } => {
                assert_eq!(key, "MY-PROJ-123");
                assert_eq!(project, "MY");
            }
            _ => panic!("Expected JiraTicket"),
        }
    }
}
