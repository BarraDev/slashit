use crate::config::paths::{AppPaths, ProjectKey};
use crate::domain::task::ExternalRef;
use crate::domain::{AgentConfig, Project, Repository, Task};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
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
    /// Serializes every read-modify-write transaction over `config_file`.
    ///
    /// `config.toml` is one shared persistence unit: each writer replaces a
    /// single section but rewrites the whole file from its own snapshot. The
    /// in-memory domain locks do not help — a project save and a repository
    /// save hold *different* locks, so both could read the same config,
    /// update their own section, and write back, with the second write
    /// silently reverting the first. Both writes are individually atomic, so
    /// the file is never torn; the update is simply lost.
    ///
    /// The guard is taken by [`Self::update_config`] and [`Self::save_config`]
    /// and released before either returns, so it is strictly the innermost
    /// lock in every path that reaches it (state-location guard, then the
    /// domain map guard, then this) and cannot participate in a cycle. It is
    /// a `std::sync::Mutex` rather than a Tokio one because the transaction
    /// it protects is entirely synchronous file I/O with no `await` inside.
    config_tx: Arc<Mutex<()>>,
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
            config_tx: Arc::new(Mutex::new(())),
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

    /// Read the persisted config, salvaging what it can from a file that does
    /// not fully deserialize.
    ///
    /// This is the *recovery* reader. It exists so the app can still start
    /// against a config it cannot fully understand, which is the right answer
    /// for a read whose result is only displayed. It is the wrong answer for a
    /// read that is about to become the next authoritative file — see
    /// [`Self::read_config_strict`], which `update_config` uses instead.
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

    /// Read the persisted config with no recovery: a file that does not fully
    /// deserialize is an error, never a partially-defaulted value.
    ///
    /// [`Self::read_config`]'s salvage path starts from `AppConfig::default()`
    /// and only ever recovers `ui_preferences` and `jj_config`, so `projects`,
    /// `repositories`, `agent_configs` (including `api_key`) and `worktree`
    /// come back as defaults while the call still returns `Ok`. Writing that
    /// value back is what turns a file the parser merely could not read into
    /// permanent data loss, so the authoritative read-modify-write path must
    /// not accept it. One rejected `[worktree] placement` variant is enough to
    /// reach this, and a config written by a newer build and then read by an
    /// older one is the realistic way it happens.
    ///
    /// A missing file is still `Ok(AppConfig::default())`: there is nothing to
    /// lose, and first-run writers depend on it.
    ///
    /// The error is deliberately one flat message rather than a `context`
    /// chain, because every caller renders it with `{e}`, which shows only the
    /// outermost layer.
    fn read_config_strict(&self) -> Result<AppConfig> {
        if !self.config_file.exists() {
            return Ok(AppConfig::default());
        }

        let contents = fs::read_to_string(&self.config_file)
            .context("Failed to read config file")?;

        toml::from_str::<AppConfig>(&contents).map_err(|parse_error| {
            anyhow::anyhow!(
                "{} could not be parsed as a complete configuration ({parse_error}); \
                 refusing to save, because writing over it would replace every section \
                 that did not load with defaults. Repair or remove that file to continue",
                self.config_file.display()
            )
        })
    }

    /// Persist global config, replacing the whole file.
    ///
    /// This file carries `AgentConfig::api_key`, so it is written atomically
    /// and locked to owner-only permissions. It is also the reason config is
    /// never relocatable into a project directory — see `config::paths`.
    ///
    /// Callers that need to change one section of the *current* config must
    /// use [`Self::update_config`] instead: this method writes exactly the
    /// value it is handed, so building that value from a separately-loaded
    /// config reintroduces the lost-update window the transaction exists to
    /// close.
    pub fn save_config(&self, config: &AppConfig) -> Result<()> {
        let _tx = self.lock_config_tx();
        self.write_config(config)
    }

    /// Apply `mutate` to the persisted config as one atomic read-modify-write.
    ///
    /// The load, the mutation and the store all happen under
    /// [`Self::config_tx`], so two writers changing different sections cannot
    /// interleave and revert one another. This is the only correct way to
    /// change part of the config: every section the closure does not touch is
    /// carried over from the copy read inside this transaction, not from one
    /// the caller read at some earlier point.
    ///
    /// The read is [`Self::read_config_strict`], not the recovering
    /// `read_config`: carrying sections over is only meaningful if they were
    /// actually read, and a partially-recovered config carries defaults in
    /// place of the sections that failed to parse. A read failure of either
    /// kind propagates without writing anything, so the file on disk is left
    /// exactly as it was for the user to repair.
    ///
    /// `mutate` runs while the transaction guard is held, so it must not
    /// acquire any other lock or block.
    pub fn update_config<F>(&self, mutate: F) -> Result<()>
    where
        F: FnOnce(&mut AppConfig),
    {
        let _tx = self.lock_config_tx();
        // No wrapping `context` on purpose: callers render this with `{e}`,
        // which shows only the outermost layer, and the reader's own message
        // is the one that tells the user which file to repair and why.
        let mut config = self.read_config_strict()?;
        mutate(&mut config);
        self.write_config(&config)
    }

    /// Take the config transaction guard.
    ///
    /// A poisoned mutex means a previous writer panicked mid-transaction. The
    /// guard protects a file that is only ever replaced atomically, so there
    /// is no half-written state to inherit: recovering and carrying on is
    /// strictly better than turning every later save into a panic. This
    /// mirrors how `refresh_routes` handles its own poisoned lock.
    fn lock_config_tx(&self) -> std::sync::MutexGuard<'_, ()> {
        self.config_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Serialize and atomically replace the config file.
    ///
    /// Assumes [`Self::config_tx`] is already held by the caller.
    fn write_config(&self, config: &AppConfig) -> Result<()> {
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
                .project_state_dir(&key, project.id, root, project.state_location),
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

/// Per-process, monotonically increasing counter used to make temp file names
/// for atomic writes unique across concurrent callers.
static ATOMIC_WRITE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A sibling of `path` with a name no other in-flight atomic write can be
/// using: the process id rules out collisions across processes, and the
/// counter rules them out between concurrent writers inside this one. Kept in
/// the same directory as `path` so the final rename stays on one filesystem.
pub(crate) fn unique_temp_path(path: &Path) -> PathBuf {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    let counter = ATOMIC_WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!("{file_name}.{}-{counter}.tmp", std::process::id()))
}

/// Write a file atomically with owner-only permissions.
///
/// The temp file is created in the destination directory, under a name unique
/// to this call (see [`unique_temp_path`]) so concurrent writers never race on
/// the same temp path, and is opened already restricted to owner-only
/// permissions so the secret it carries is never briefly world-readable, at
/// the temporary path or the final one.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = unique_temp_path(path);
    create_owner_only_file(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// Write a file atomically, leaving permissions at the platform default.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = unique_temp_path(path);
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// Create `path` containing `bytes`, restricted to owner-only permissions
/// from the moment it exists. On Unix this opens the file with mode `0600`
/// set at creation time rather than writing at the platform default and
/// tightening permissions afterward, which would leave a window where the
/// file is briefly world-readable.
#[cfg(unix)]
fn create_owner_only_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn create_owner_only_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Windows inherits the parent ACL, which is already user-scoped under
    // %APPDATA%. There is no portable mode bit to set here.
    fs::write(path, bytes)
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
    fn test_load_config_ignores_unknown_sections() {
        // Renamed from `..._with_valid_toml_wrong_structure`: `AppConfig` sets
        // no `deny_unknown_fields`, so this fixture deserializes cleanly and
        // never reaches the recovery branch. What it actually pins is that an
        // unknown section is tolerated. The recovery branch is covered by
        // `load_config_still_recovers_from_a_config_it_cannot_fully_parse`.
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

    // ============ Strict read-modify-write vs. recovery reads ============

    /// A config that is valid TOML and fully populated, but that `AppConfig`'s
    /// own deserializer rejects.
    ///
    /// Built by serializing a real config and then invalidating exactly one
    /// enum variant, so every other section keeps the shape the app actually
    /// writes rather than a hand-typed approximation. `WorktreePlacement` has
    /// only `auto` and `managed`, so `shared_root` is rejected — this is the
    /// shape of a config written by a newer build and read by an older one.
    fn config_that_fails_to_deserialize() -> String {
        let mut config = AppConfig::default();
        config.jj_config.user_name = Some("Recoverable".to_string());
        config.ui_preferences.theme = "light".to_string();
        config.repositories.insert(
            "11111111-1111-1111-1111-111111111111".to_string(),
            Repository {
                id: uuid::Uuid::nil(),
                local_path: "/home/user/repo".to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        config.agent_configs.insert(
            "claude_code".to_string(),
            AgentConfig {
                agent_type: crate::domain::AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: vec!["--stdio".to_string()],
                env: HashMap::new(),
                model: Some("opus".to_string()),
                api_key: Some("sk-must-not-be-lost".to_string()),
            },
        );

        let valid = toml::to_string_pretty(&config).expect("fixture should serialize");
        let poisoned = valid.replace("placement = \"auto\"", "placement = \"shared_root\"");
        assert_ne!(
            poisoned, valid,
            "fixture must actually invalidate the placement variant"
        );

        // The fixture is only meaningful if it really lands on the recovery
        // branch: valid TOML, invalid AppConfig.
        assert!(
            toml::from_str::<toml::Value>(&poisoned).is_ok(),
            "fixture must stay valid TOML"
        );
        assert!(
            toml::from_str::<AppConfig>(&poisoned).is_err(),
            "fixture must fail to deserialize as AppConfig"
        );

        poisoned
    }

    #[test]
    fn update_config_refuses_to_overwrite_a_config_it_cannot_fully_parse() {
        let (storage, _temp) = create_test_storage();
        let poisoned = config_that_fails_to_deserialize();
        fs::write(&storage.config_file, &poisoned).expect("write fixture");

        let result = storage.update_config(|config| {
            config.projects.insert("new".to_string(), unreachable_project());
        });

        assert!(
            result.is_err(),
            "a config that did not fully parse must not be used as the basis of a write"
        );

        let on_disk = fs::read_to_string(&storage.config_file).expect("config still readable");
        assert_eq!(
            on_disk, poisoned,
            "the unparsable config must be left untouched for the user to repair"
        );
        assert!(
            on_disk.contains("sk-must-not-be-lost"),
            "the api key must still be on disk"
        );
        assert!(
            on_disk.contains("/home/user/repo"),
            "the repository must still be on disk"
        );
    }

    #[test]
    fn update_config_preserves_sections_it_does_not_touch() {
        // The other half of the invariant: refusing an unparsable config must
        // not come at the cost of the normal carry-over behaviour.
        let (storage, _temp) = create_test_storage();

        let mut seed = AppConfig::default();
        seed.jj_config.user_name = Some("Keep Me".to_string());
        seed.ui_preferences.theme = "light".to_string();
        seed.agent_configs.insert(
            "claude_code".to_string(),
            AgentConfig {
                agent_type: crate::domain::AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: vec![],
                env: HashMap::new(),
                model: None,
                api_key: Some("sk-keep".to_string()),
            },
        );
        storage.save_config(&seed).expect("seed config");

        storage
            .update_config(|config| {
                config.projects.insert("new".to_string(), unreachable_project());
            })
            .expect("a fully parsable config must still be updatable");

        let loaded = storage.load_config().expect("load back");
        assert_eq!(loaded.projects.len(), 1, "the update must be applied");
        assert_eq!(loaded.jj_config.user_name.as_deref(), Some("Keep Me"));
        assert_eq!(loaded.ui_preferences.theme, "light");
        assert_eq!(
            loaded.agent_configs["claude_code"].api_key.as_deref(),
            Some("sk-keep")
        );
    }

    #[test]
    fn load_config_still_recovers_from_a_config_it_cannot_fully_parse() {
        // Startup salvage is intentionally retained: the strict reader is only
        // for the authoritative write path. The app must still start and show
        // whatever survived, and the original file must still be backed up.
        let (storage, _temp) = create_test_storage();
        fs::write(&storage.config_file, config_that_fails_to_deserialize()).expect("write fixture");

        let recovered = storage.load_config().expect("startup must not fail");

        assert_eq!(recovered.ui_preferences.theme, "light", "salvaged");
        assert_eq!(
            recovered.jj_config.user_name.as_deref(),
            Some("Recoverable"),
            "salvaged"
        );
        assert!(
            recovered.repositories.is_empty() && recovered.agent_configs.is_empty(),
            "this is exactly the reduction that must never be written back"
        );
        assert!(
            storage.config_file.with_extension("toml.backup").exists(),
            "recovery must leave a backup of the original"
        );
    }

    /// A minimal project for tests that only care that *some* mutation was
    /// attempted, never about the project's contents.
    fn unreachable_project() -> crate::domain::Project {
        use crate::config::paths::StateLocation;
        use crate::domain::{AgentType, ProjectScope};
        crate::domain::Project {
            id: uuid::Uuid::nil(),
            name: "test".to_string(),
            repository_id: None,
            scope: ProjectScope::Standalone,
            state_location: StateLocation::External,
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
        }
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
    fn routed_projects_sharing_a_repository_keep_separate_task_state() {
        use crate::domain::{AgentConfig, AgentType, Project, Repository};

        let (storage, temp) = create_test_storage();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();

        let repository_id = Uuid::new_v4();
        let project_a = Uuid::new_v4();
        let project_b = Uuid::new_v4();
        let project = |id, name: &str| Project {
            id,
            name: name.to_string(),
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
        };

        let mut config = AppConfig::default();
        config.repositories.insert(
            repository_id.to_string(),
            Repository {
                id: repository_id,
                local_path: repo.to_string_lossy().to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        config
            .projects
            .insert(project_a.to_string(), project(project_a, "A"));
        config
            .projects
            .insert(project_b.to_string(), project(project_b, "B"));
        storage.save_config(&config).unwrap();

        let mut task_a = crate::test_helpers::create_test_task("A task");
        task_a.project_id = project_a;
        let mut task_b = crate::test_helpers::create_test_task("B task");
        task_b.project_id = project_b;

        storage.save_project_tasks(project_a, &[task_a]).unwrap();
        storage.save_project_tasks(project_b, &[task_b]).unwrap();

        assert_ne!(storage.tasks_path(project_a), storage.tasks_path(project_b));
        assert_eq!(
            storage.load_project_tasks(project_a).unwrap()[0].title,
            "A task"
        );
        assert_eq!(
            storage.load_project_tasks(project_b).unwrap()[0].title,
            "B task"
        );

        storage.delete_project_tasks(project_b).unwrap();
        assert_eq!(
            storage.load_project_tasks(project_a).unwrap()[0].title,
            "A task"
        );
        assert!(storage.load_project_tasks(project_b).unwrap().is_empty());
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

    #[cfg(unix)]
    #[test]
    fn private_temp_file_is_owner_only_from_creation() {
        // The temp file itself (not just the final path) must never be
        // briefly world-readable, since save_config carries api_key.
        let (storage, _temp) = create_test_storage();
        storage.save_config(&AppConfig::default()).unwrap();

        use std::os::unix::fs::PermissionsExt;
        let tmp = unique_temp_path(&storage.config_file);
        create_owner_only_file(&tmp, b"secret").unwrap();
        let mode = fs::metadata(&tmp).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "temp file must be owner-only at creation");
        fs::remove_file(&tmp).unwrap();
    }

    // ==================== Atomic write temp-name uniqueness Tests ====================

    #[test]
    fn unique_temp_path_never_collides_across_calls() {
        let path = Path::new("/tmp/example-dir/config.toml");
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let tmp = unique_temp_path(path);
            assert!(seen.insert(tmp.clone()), "temp path collided: {tmp:?}");
            assert_eq!(tmp.parent(), path.parent(), "temp file must stay a sibling of the destination");
        }
    }

    #[test]
    fn concurrent_atomic_writes_to_same_destination_do_not_corrupt() {
        // Two writers targeting the same destination used to share one fixed
        // temp name and race on it. With per-call unique temp names, each
        // writer's rename is independent, so the final file must be exactly
        // one writer's complete payload — never a truncated or interleaved
        // mix of two — and no stray temp files should be left behind.
        use std::sync::Barrier;
        use std::thread;

        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let path = Arc::new(temp_dir.path().join("shared.toml"));

        let writer_count = 8usize;
        let barrier = Arc::new(Barrier::new(writer_count));

        let handles: Vec<_> = (0..writer_count)
            .map(|i| {
                let barrier = Arc::clone(&barrier);
                let path = Arc::clone(&path);
                thread::spawn(move || {
                    let payload = format!("writer-{i}-").repeat(2048);
                    barrier.wait();
                    write_atomic(&path, payload.as_bytes()).expect("write should succeed");
                    payload
                })
            })
            .collect();

        let payloads: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let final_contents = fs::read_to_string(&*path).expect("final file should be readable");
        assert!(
            payloads.iter().any(|p| p == &final_contents),
            "final file must be exactly one writer's complete payload, not a mix"
        );

        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path() != *path)
            .collect();
        assert!(leftovers.is_empty(), "unexpected leftover temp files: {leftovers:?}");
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
