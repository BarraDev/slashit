use crate::config::paths::{AppPaths, ProjectKey};
use crate::domain::task::ExternalRef;
use crate::domain::{AgentConfig, Project, Repository, Task};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use uuid::Uuid;

/// Task file name inside a project's own state directory.
const TASKS_FILE: &str = "tasks.toml";

/// Pre-`AppPaths` task location: one `<project-uuid>.toml` per project, all of
/// them under a single global directory. Still read, and still written for
/// projects that cannot be routed to a state directory of their own.
const LEGACY_TASKS_DIR: &str = "tasks";

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
    pub worktree: WorktreeConfig,
    #[serde(default)]
    pub ui_preferences: UiPreferences,
}

/// How SlashIt places the worktrees it creates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorktreeConfig {
    #[serde(default)]
    pub placement: crate::config::paths::WorktreePlacement,
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
            worktree: WorktreeConfig::default(),
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
    paths: Arc<AppPaths>,
    config_file: PathBuf,
    /// Project id to the directory holding that project's shareable state.
    ///
    /// Routing depends only on config content — a project's repository path
    /// and its `state_location`. Rebuilding the table wherever config enters or
    /// leaves the process is therefore sufficient to keep it correct, and there
    /// is no second invalidation path that could be forgotten. A project absent
    /// from the table (no repository attached, or config not loaded yet) falls
    /// back to the legacy global location rather than guessing.
    routes: Arc<RwLock<HashMap<Uuid, PathBuf>>>,
}

impl Storage {
    pub fn new() -> Result<Self> {
        let paths = AppPaths::new().context("Failed to resolve application directories")?;
        Ok(Self::with_paths(paths))
    }

    /// Build against explicit roots. Used by tests to stay inside a tempdir.
    pub fn with_paths(paths: AppPaths) -> Self {
        let config_file = paths.config_file();
        Self {
            paths: Arc::new(paths),
            config_file,
            routes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// The resolved application directories. The single source of truth for
    /// every path; no caller may derive a state path independently.
    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn load_config(&self) -> Result<AppConfig> {
        let config = self.read_config()?;
        self.refresh_routes(&config);
        Ok(config)
    }

    fn read_config(&self) -> Result<AppConfig> {
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

    /// Persist global config.
    ///
    /// This file carries `AgentConfig::api_key`, so it is written atomically
    /// and locked to owner-only permissions. It is also the reason config is
    /// never relocatable into a project directory — see `config::paths`.
    pub fn save_config(&self, config: &AppConfig) -> Result<()> {
        let contents = toml::to_string_pretty(config)
            .context("Failed to serialize config")?;
        write_private_atomic(&self.config_file, contents.as_bytes())
            .context("Failed to write config file")?;
        self.refresh_routes(config);
        Ok(())
    }

    // --- Project state routing --------------------------------------------

    /// Recompute the project to state-directory table from `config`.
    fn refresh_routes(&self, config: &AppConfig) {
        let mut next = HashMap::with_capacity(config.projects.len());
        for (id_str, project) in &config.projects {
            let Ok(id) = Uuid::parse_str(id_str) else {
                continue;
            };
            if let Some(dir) = self.state_dir_for(config, project) {
                next.insert(id, dir);
            }
        }
        // A poisoned lock means another thread panicked mid-update. The table
        // is derived data, so recovering it and carrying on is strictly better
        // than propagating the panic into every subsequent save.
        match self.routes.write() {
            Ok(mut routes) => *routes = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// Where one project's shareable state belongs, or `None` when the project
    /// has no repository path to key off.
    fn state_dir_for(&self, config: &AppConfig, project: &Project) -> Option<PathBuf> {
        let repository_id = project.repository_id?;
        let repository = config.repositories.get(&repository_id.to_string())?;
        let root = Path::new(&repository.local_path);
        let key = ProjectKey::for_path(root).key;
        Some(
            self.paths
                .project_state_dir(&key, root, project.state_location),
        )
    }

    /// The routed state directory for a project, if it has one.
    fn routed_state_dir(&self, project_id: Uuid) -> Option<PathBuf> {
        let routes = match self.routes.read() {
            Ok(routes) => routes,
            Err(poisoned) => poisoned.into_inner(),
        };
        routes.get(&project_id).cloned()
    }

    fn legacy_tasks_dir(&self) -> PathBuf {
        self.paths.config_dir().join(LEGACY_TASKS_DIR)
    }

    fn legacy_tasks_path(&self, project_id: Uuid) -> PathBuf {
        self.legacy_tasks_dir().join(format!("{project_id}.toml"))
    }

    /// Where this project's tasks are written.
    ///
    /// Routed projects get `tasks.toml` inside their own state directory.
    /// Unroutable ones keep the legacy `<uuid>.toml` name under the shared
    /// directory, because a shared directory plus a shared file name would
    /// make every unroutable project overwrite the same file.
    fn tasks_path(&self, project_id: Uuid) -> PathBuf {
        match self.routed_state_dir(project_id) {
            Some(dir) => dir.join(TASKS_FILE),
            None => self.legacy_tasks_path(project_id),
        }
    }
}

/// Write a file atomically with owner-only permissions.
///
/// The temp file is created in the destination directory so the rename stays
/// on one filesystem, and permissions are set on the temp file *before* the
/// rename so the secret is never briefly world-readable at its final path.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    set_owner_only(&tmp)?;
    fs::rename(&tmp, path)
}

/// Write a file atomically, leaving permissions at the platform default.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> std::io::Result<()> {
    // Windows inherits the parent ACL, which is already user-scoped under
    // %APPDATA%. There is no portable mode bit to set here.
    Ok(())
}

impl Default for Storage {
    fn default() -> Self {
        Self::new().expect("Failed to create storage")
    }
}

impl Storage {
    /// Read and parse one tasks file, applying reference migration.
    ///
    /// A parse failure is reported and skipped rather than propagated: one
    /// corrupt project file must not stop the other projects from loading.
    fn read_tasks_file(path: &Path) -> Option<Vec<Task>> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(e) => {
                eprintln!("Warning: Failed to read tasks file {path:?}: {e}");
                return None;
            }
        };
        match toml::from_str::<ProjectTasksFile>(&contents) {
            Ok(file) => {
                let mut tasks = file.tasks;
                for task in &mut tasks {
                    migrate_task_refs(task);
                }
                Some(tasks)
            }
            Err(e) => {
                eprintln!("Warning: Failed to parse tasks file {path:?}: {e}");
                None
            }
        }
    }

    /// Load every task across every project.
    ///
    /// Reads each routed project's own state directory, then sweeps the legacy
    /// global directory for projects that have not been routed or not yet
    /// migrated. The legacy sweep is what keeps an upgrade non-destructive:
    /// tasks stay visible until the project that owns them is saved once.
    pub fn load_all_tasks(&self) -> Result<Vec<Task>> {
        let mut all_tasks = Vec::new();
        let mut seen_files = std::collections::HashSet::new();

        let routes = {
            let guard = match self.routes.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.clone()
        };

        for (project_id, dir) in &routes {
            let path = dir.join(TASKS_FILE);
            let legacy = self.legacy_tasks_path(*project_id);

            // Claim both locations for this project before reading either.
            //
            // Marking only the file we read would let the legacy sweep below
            // pick up a stale copy that survived — a crash between writing the
            // new file and deleting the old one leaves both on disk. That copy
            // is older and is appended last, so it would overwrite the current
            // board when the caller collects tasks by id.
            seen_files.insert(path.clone());
            seen_files.insert(legacy.clone());

            let source = if path.is_file() {
                Some(path)
            } else if legacy.is_file() {
                // Not migrated yet: read from where the tasks still are, so
                // nothing disappears on upgrade.
                Some(legacy)
            } else {
                None
            };

            if let Some(source) = source {
                if let Some(tasks) = Self::read_tasks_file(&source) {
                    all_tasks.extend(tasks);
                }
            }
        }

        let legacy_dir = self.legacy_tasks_dir();
        if legacy_dir.is_dir() {
            for entry in fs::read_dir(&legacy_dir).context("Failed to read tasks directory")? {
                let path = entry?.path();
                if path.extension().is_some_and(|ext| ext == "toml")
                    && !seen_files.contains(&path)
                {
                    if let Some(tasks) = Self::read_tasks_file(&path) {
                        all_tasks.extend(tasks);
                    }
                }
            }
        }

        Ok(all_tasks)
    }

    /// Load tasks for a specific project, falling back to the legacy location.
    pub fn load_project_tasks(&self, project_id: Uuid) -> Result<Vec<Task>> {
        let path = self.tasks_path(project_id);
        if path.is_file() {
            return Ok(Self::read_tasks_file(&path).unwrap_or_default());
        }

        let legacy = self.legacy_tasks_path(project_id);
        if legacy != path && legacy.is_file() {
            return Ok(Self::read_tasks_file(&legacy).unwrap_or_default());
        }

        Ok(Vec::new())
    }

    /// Save tasks for a specific project.
    ///
    /// Writes to the project's current state directory and then retires the
    /// legacy file, so the move happens exactly once and only after the new
    /// copy is safely on disk.
    pub fn save_project_tasks(&self, project_id: Uuid, tasks: &[Task]) -> Result<()> {
        let path = self.tasks_path(project_id);

        let tasks_file = ProjectTasksFile {
            version: 1,
            tasks: tasks.to_vec(),
        };
        let contents = toml::to_string_pretty(&tasks_file).context("Failed to serialize tasks")?;
        write_atomic(&path, contents.as_bytes()).context("Failed to write tasks file")?;

        let legacy = self.legacy_tasks_path(project_id);
        if legacy != path && legacy.is_file() {
            if let Err(e) = fs::remove_file(&legacy) {
                eprintln!("Warning: Failed to remove legacy tasks file {legacy:?}: {e}");
            }
        }

        Ok(())
    }

    /// Delete a project's tasks from wherever they live.
    pub fn delete_project_tasks(&self, project_id: Uuid) -> Result<()> {
        let path = self.tasks_path(project_id);
        if path.exists() {
            fs::remove_file(&path).context("Failed to delete project tasks file")?;
        }

        let legacy = self.legacy_tasks_path(project_id);
        if legacy != path && legacy.exists() {
            fs::remove_file(&legacy).context("Failed to delete legacy project tasks file")?;
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
        let root = temp_dir.path();
        // `AppPaths::new()` creates these in production; `with_roots` does not,
        // so the fixture stands in for that setup.
        fs::create_dir_all(root.join("config")).expect("Failed to create config dir");
        fs::create_dir_all(root.join("data")).expect("Failed to create data dir");

        let storage = Storage::with_paths(AppPaths::with_roots(
            root.join("config"),
            root.join("data"),
            root.join("cache"),
            root.join("runtime"),
        ));

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
            scope: crate::domain::ProjectScope::Standalone,
            state_location: crate::config::paths::StateLocation::External,
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
        
        // An unroutable project (no config loaded) keeps the legacy filename.
        let path = storage.legacy_tasks_path(project_id);
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

    // ==================== State routing / migration Tests ====================

    /// Build a config whose single project routes to `state_dir`, and load it
    /// so the routing table is populated.
    fn route_project_to(
        storage: &Storage,
        project_id: Uuid,
        repo_path: &std::path::Path,
    ) -> AppConfig {
        use crate::domain::{AgentConfig, AgentType, Project, Repository};

        let repository_id = Uuid::new_v4();
        let mut config = AppConfig::default();
        config.repositories.insert(
            repository_id.to_string(),
            Repository {
                id: repository_id,
                local_path: repo_path.to_string_lossy().to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        config.projects.insert(
            project_id.to_string(),
            Project {
                id: project_id,
                name: "Routed".to_string(),
                repository_id: Some(repository_id),
                scope: crate::domain::ProjectScope::Standalone,
                state_location: crate::config::paths::StateLocation::External,
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
            },
        );

        storage.save_config(&config).expect("Should save config");
        config
    }

    #[test]
    fn routed_project_writes_into_its_own_state_dir() {
        let (storage, temp) = create_test_storage();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();

        let project_id = Uuid::new_v4();
        route_project_to(&storage, project_id, &repo);

        let task = crate::test_helpers::create_test_task("Routed task");
        storage
            .save_project_tasks(project_id, &[task])
            .expect("Should save");

        // Outside the repository, and not in the legacy global directory.
        let written = storage.tasks_path(project_id);
        assert!(written.is_file());
        assert!(!written.starts_with(&repo));
        assert!(!storage.legacy_tasks_path(project_id).exists());
        assert_eq!(storage.load_project_tasks(project_id).unwrap().len(), 1);
    }

    #[test]
    fn tasks_left_in_the_legacy_location_are_still_loaded() {
        let (storage, temp) = create_test_storage();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();

        // Write a task the old way: routed project, but only the legacy file.
        let project_id = Uuid::new_v4();
        let task = crate::test_helpers::create_test_task("Pre-upgrade task");
        storage
            .save_project_tasks(project_id, &[task])
            .expect("Should save");
        assert!(storage.legacy_tasks_path(project_id).is_file());

        // Now the project gains a route, as it would on upgrade.
        route_project_to(&storage, project_id, &repo);

        assert_eq!(storage.load_all_tasks().unwrap().len(), 1);
        assert_eq!(storage.load_project_tasks(project_id).unwrap().len(), 1);
    }

    #[test]
    fn a_surviving_legacy_file_does_not_resurrect_an_old_board() {
        // A crash between writing the new file and deleting the old one leaves
        // both on disk. The stale copy must not be loaded: it is older, and
        // being appended last it would overwrite the current board when the
        // caller collects tasks by id.
        let (storage, temp) = create_test_storage();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();

        let project_id = Uuid::new_v4();
        route_project_to(&storage, project_id, &repo);

        let current = crate::test_helpers::create_test_task("Current");
        storage
            .save_project_tasks(project_id, &[current])
            .expect("Should save");

        // Re-create the legacy file behind the code's back.
        let legacy = storage.legacy_tasks_path(project_id);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let stale = ProjectTasksFile {
            version: 1,
            tasks: vec![
                crate::test_helpers::create_test_task("Stale one"),
                crate::test_helpers::create_test_task("Stale two"),
            ],
        };
        fs::write(&legacy, toml::to_string_pretty(&stale).unwrap()).unwrap();

        let loaded = storage.load_all_tasks().expect("Should load");
        assert_eq!(loaded.len(), 1, "stale legacy copy must be ignored");
        assert_eq!(loaded[0].title, "Current");
    }

    #[test]
    fn unroutable_projects_keep_separate_legacy_files() {
        // Without a route there is no per-project directory, so the per-uuid
        // filename is what stops two projects sharing one tasks.toml.
        let (storage, _temp) = create_test_storage();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        storage
            .save_project_tasks(a, &[crate::test_helpers::create_test_task("A")])
            .unwrap();
        storage
            .save_project_tasks(b, &[crate::test_helpers::create_test_task("B")])
            .unwrap();

        assert_ne!(storage.tasks_path(a), storage.tasks_path(b));
        assert_eq!(storage.load_all_tasks().unwrap().len(), 2);
    }

    #[test]
    fn config_file_is_not_world_readable() {
        // config.toml carries AgentConfig::api_key.
        let (storage, _temp) = create_test_storage();
        storage.save_config(&AppConfig::default()).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&storage.config_file).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "config.toml must not be group/world readable");
        }
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
