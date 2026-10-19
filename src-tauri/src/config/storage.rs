use crate::config::paths::{AppPaths, ProjectKey};
use crate::domain::task::ExternalRef;
use crate::domain::{AgentConfig, Conversation, Project, Repository, Task};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use uuid::Uuid;

/// Task file name inside a project's own state directory.
const TASKS_FILE: &str = "tasks.toml";

/// Newest board layout this build can read and write back without losing
/// anything. A file declaring a later version was written by a newer SlashIt,
/// and saving over it would drop whatever that version added.
const TASKS_FILE_VERSION: u32 = 1;

/// A Task file that exists but cannot be used as a board.
///
/// Reading it as an empty board and saving that is how history gets lost, so
/// every save that would replace such a file refuses with this instead. The
/// file is never touched: its bytes are the only copy of the board.
#[derive(Debug)]
pub struct UnreadableTasksFile {
    pub path: PathBuf,
    reason: String,
}

impl std::fmt::Display for UnreadableTasksFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the task file {} cannot be used as a board ({}). SlashIt will not overwrite it, \
             so nothing was saved. Keep a copy of the file, then repair or restore it and \
             restart SlashIt.",
            self.path.display(),
            self.reason
        )
    }
}

impl std::error::Error for UnreadableTasksFile {}

/// What one Task file path holds. An absent file and an empty board are
/// different answers, and neither is the same as a file that cannot be read.
enum TasksFileState {
    Absent,
    Board(Vec<Task>),
    Unreadable(UnreadableTasksFile),
}

/// Structure for storing tasks per project in TOML files
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectTasksFile {
    pub version: u32,
    pub tasks: Vec<Task>,
}

/// A configuration-class write (`config.toml`, the workspace registry, the
/// feature flags) that did not complete, and what that means for the file.
///
/// The distinction is the contract. Every caller used to assume that an error
/// means the file is exactly as it was, which stops being true once a write
/// can fail *after* the replacement:
///
/// * [`Self::NotPublished`] -- the destination was not replaced. Memory that
///   was not changed agrees with disk, and the operation can be retried.
/// * [`Self::PublishedNotDurable`] -- the new content is what every reader of
///   the file now sees, but the final synchronization failed, so a crash may
///   bring the previous content back. Memory must follow the *visible* state
///   ([`adopt_published`]), the operation must not be repeated as if it had not
///   happened, no destructive step that depends on this commit may run until
///   [`Storage::confirm_config_durable`] succeeds, and the caller must say so
///   instead of reporting either success or "nothing was saved".
///
/// * [`Self::Unconfirmable`] -- only ever returned by
///   [`Storage::confirm_config_durable`]. The write itself succeeded, but the
///   filesystem cannot synchronize a directory
///   (`slashit_durable_fs::DirectorySync::Unsupported`), so nothing can prove
///   the replacement survives a power loss. An ordinary save is not refused on
///   such a filesystem; a destructive step that depends on the change sticking
///   is, and it keeps its retry records.
#[derive(Debug)]
pub enum ConfigWriteError {
    NotPublished(anyhow::Error),
    PublishedNotDurable(anyhow::Error),
    Unconfirmable(anyhow::Error),
}

impl ConfigWriteError {
    /// True when the new content is already visible on disk.
    pub fn is_published(&self) -> bool {
        matches!(self, Self::PublishedNotDurable(_))
    }

    fn inner(&self) -> &anyhow::Error {
        match self {
            Self::NotPublished(e) | Self::PublishedNotDurable(e) | Self::Unconfirmable(e) => e,
        }
    }

    /// A sentence for the person: what was being done and, if the change is
    /// already in place, that its durability is unconfirmed.
    pub fn describe(&self, action: &str) -> String {
        match self {
            Self::NotPublished(e) => format!("Failed to {action}: {e}"),
            Self::PublishedNotDurable(e) => format!(
                "The change was applied, but SlashIt could not confirm that it reached stable \
                 storage (while trying to {action}), so it may be lost if the computer stops \
                 before the next successful save. Do not repeat the action: {e:#}"
            ),
            Self::Unconfirmable(e) => format!(
                "Could not {action}: the change is saved, but this storage location cannot \
                 confirm that it survives a power loss, so SlashIt will not delete anything \
                 that depends on it: {e:#}"
            ),
        }
    }
}

impl std::fmt::Display for ConfigWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Only the outermost layer, as before: it names the stage that
            // failed ("Failed to load config before updating it"), and callers
            // render with `{e}`.
            Self::NotPublished(e) => write!(f, "{e}"),
            Self::PublishedNotDurable(e) => write!(
                f,
                "the change is in place but not confirmed durable (it may be lost if the \
                 computer stops before the next successful save): {e:#}"
            ),
            Self::Unconfirmable(e) => write!(f, "the change cannot be confirmed durable on this storage location: {e:#}"),
        }
    }
}

impl std::error::Error for ConfigWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.inner().as_ref())
    }
}

impl From<ConfigWriteError> for String {
    fn from(error: ConfigWriteError) -> Self {
        error.to_string()
    }
}

/// Make `current` follow a write's outcome: the proposed value replaces it
/// when the file now holds it, whether or not that was confirmed durable.
///
/// This is the one rule every caller that stages a change in a clone shares.
/// Leaving memory on the old value after [`ConfigWriteError::PublishedNotDurable`]
/// would let the next save write the old value back over a change the file
/// already shows, and would leave routes (rebuilt from the file) disagreeing
/// with the map they were derived for.
pub fn adopt_published<T>(
    current: &mut T,
    proposed: T,
    outcome: std::result::Result<(), ConfigWriteError>,
) -> std::result::Result<(), ConfigWriteError> {
    match outcome {
        Ok(()) => {
            *current = proposed;
            Ok(())
        }
        Err(error) if error.is_published() => {
            *current = proposed;
            Err(error)
        }
        Err(error) => Err(error),
    }
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
    /// The Queue settings. Absent from files written before they were
    /// persisted, which load as the defaults.
    #[serde(default)]
    pub queue: crate::config::QueueConfig,
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
            queue: crate::config::QueueConfig::default(),
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
        Some(ExternalRef::GithubPr { url: url.to_string(), number, repo, state: None , merge_commit: None})
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
    /// Projects whose Task file [`Self::load_all_tasks`] could not read, with
    /// that file. The board in memory for such a project was built without it,
    /// so it is empty or partial however the file looks later: a repair made
    /// while the app runs does not reach memory, and a save of that board would
    /// replace the repaired file with it. Keyed by project, not path, so a
    /// route change that moves the board does not lift the guard. Saves refuse
    /// until a load reads the project's file again.
    unloaded_boards: Arc<Mutex<HashMap<Uuid, PathBuf>>>,
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
    /// Set while the last configuration write replaced the file but could not
    /// confirm it durable (its directory sync failed, or the filesystem cannot
    /// do one at all), cleared by the next write whose directory sync
    /// succeeded. See [`Self::confirm_config_durable`].
    config_unconfirmed: Arc<std::sync::atomic::AtomicBool>,
}

/// One durable Task cleanup retry record: the Project whose board file
/// cleanup remains owed, and the board files captured for it while the
/// Project could still be routed to its state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTaskCleanupRecord {
    pub project_id: Uuid,
    pub board_files: Vec<PathBuf>,
}

impl Storage {
    /// Load a Project's primary Conversation, if one has been created.
    pub fn load_primary_conversation(&self, project_id: Uuid) -> Result<Option<Conversation>> {
        let pointer = self.paths.primary_conversation_file(project_id);
        if !pointer.is_file() {
            return Ok(None);
        }
        let id: Uuid = fs::read_to_string(&pointer)
            .with_context(|| format!("Failed to read primary Conversation pointer {pointer:?}"))?
            .trim()
            .parse()
            .context("Invalid primary Conversation identity")?;
        let path = self.paths.conversation_file(id);
        let bytes =
            fs::read(&path).with_context(|| format!("Failed to read Conversation {path:?}"))?;
        let conversation: Conversation = serde_json::from_slice(&bytes)
            .with_context(|| format!("Failed to parse Conversation {path:?}"))?;
        if conversation.id != id || conversation.project_id != project_id {
            anyhow::bail!("Primary Conversation identity does not match its Project pointer");
        }
        Ok(Some(conversation))
    }

    /// Create a Conversation with its own identity and atomically publish the
    /// Project's primary pointer only after the Conversation is durable.
    pub fn create_primary_conversation(&self, project_id: Uuid) -> Result<Conversation> {
        if let Some(existing) = self.load_primary_conversation(project_id)? {
            return Ok(existing);
        }
        let conversation = Conversation::new(project_id);
        self.save_conversation(&conversation)?;
        write_private_atomic(
            &self.paths.primary_conversation_file(project_id),
            conversation.id.to_string().as_bytes(),
        )
        .map_err(|e| write_failure(e, "persist primary Conversation pointer"))?;
        Ok(conversation)
    }

    /// Persist a Project Conversation with owner-only permissions and an
    /// atomic replacement, rejecting any identity/owner mismatch.
    pub fn save_conversation(&self, conversation: &Conversation) -> Result<()> {
        if conversation.id.is_nil() || conversation.project_id.is_nil() {
            anyhow::bail!("Conversation and Project identities must be valid");
        }
        let bytes =
            serde_json::to_vec_pretty(conversation).context("Failed to serialize Conversation")?;
        write_private_atomic(&self.paths.conversation_file(conversation.id), &bytes)
            .map(|_| ())
            .map_err(|e| write_failure(e, "atomically persist Conversation"))
    }

    /// Remove a Project's primary pointer and its document only after the
    /// document confirms both identities. Validation and cleanup failures
    /// preserve the pointer as the recovery reference. An already-absent
    /// document is a safe partial-cleanup state.
    pub fn delete_project_conversation(&self, project_id: Uuid) -> Result<()> {
        let pointer = self.paths.primary_conversation_file(project_id);
        match fs::symlink_metadata(&pointer) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => anyhow::bail!("Conversation pointer is not a regular file: {pointer:?}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // The Conversation document is written before its pointer, so
                // a crash during creation can leave a document-only orphan.
                // A pending Project-deletion marker authorizes a strict scan
                // for documents that prove their own id and Project owner.
                if self.pending_project_conversation_deletions()?.contains(&project_id) {
                    self.delete_unpointed_project_conversations(project_id)?;
                }
                return Ok(());
            }
            Err(error) => return Err(error).context("Failed to inspect Conversation pointer"),
        }
        let raw = fs::read_to_string(&pointer)
            .with_context(|| format!("Failed to read Conversation pointer {pointer:?}"))?;
        let conversation_id = raw.trim().parse::<Uuid>()
            .with_context(|| format!("Invalid Conversation identity in pointer {pointer:?}"))?;
        let document = self.paths.conversation_file(conversation_id);
        match fs::symlink_metadata(&document) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => anyhow::bail!("Conversation document is not a regular file: {document:?}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::remove_file(&pointer)
                    .with_context(|| format!("Failed to remove stale Conversation pointer {pointer:?}"))?;
                return Ok(());
            }
            Err(error) => return Err(error).context("Failed to inspect Conversation document"),
        }
        let bytes = fs::read(&document)
            .with_context(|| format!("Failed to read Conversation {document:?}"))?;
        let conversation: Conversation = serde_json::from_slice(&bytes)
            .with_context(|| format!("Failed to parse Conversation {document:?}"))?;
        if conversation.id != conversation_id || conversation.project_id != project_id {
            anyhow::bail!("Conversation identity or Project owner does not match pointer {pointer:?}");
        }
        fs::remove_file(&document)
            .with_context(|| format!("Failed to remove Project Conversation {document:?}"))?;
        fs::remove_file(&pointer)
            .with_context(|| format!("Failed to remove Conversation pointer {pointer:?}"))?;
        Ok(())
    }

    fn delete_unpointed_project_conversations(&self, project_id: Uuid) -> Result<()> {
        let directory = self.paths.conversations_dir();
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("Failed to scan unpointed Conversations"),
        };
        for entry in entries {
            let entry = entry.context("Failed to read Conversation directory entry")?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue; };
            let Some(id) = name.strip_suffix(".json") else { continue; };
            let conversation_id = id.parse::<Uuid>()
                .with_context(|| format!("Invalid Conversation filename {:?}", entry.path()))?;
            if name != format!("{conversation_id}.json") {
                anyhow::bail!("Non-canonical Conversation filename {:?}", entry.path());
            }
            let path = self.paths.conversation_file(conversation_id);
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("Failed to inspect Conversation {path:?}"))?;
            if !metadata.file_type().is_file() {
                anyhow::bail!("Conversation document is not a regular file: {path:?}");
            }
            let bytes = fs::read(&path)
                .with_context(|| format!("Failed to read Conversation {path:?}"))?;
            let conversation: Conversation = serde_json::from_slice(&bytes)
                .with_context(|| format!("Failed to parse Conversation {path:?}"))?;
            if conversation.id != conversation_id {
                anyhow::bail!("Conversation identity does not match filename {path:?}");
            }
            if conversation.project_id == project_id {
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to remove unpointed Project Conversation {path:?}"))?;
            }
        }
        Ok(())
    }

    /// Persist the retry reference before removing a Project from config.
    /// While the record exists, the Project's Conversation cleanup is owed:
    /// it also authorizes the strict scan for document-only orphans in
    /// [`Self::delete_project_conversation`].
    pub fn mark_project_conversation_deletion_pending(&self, project_id: Uuid) -> Result<()> {
        let path = self.paths.pending_project_conversation_deletion_file(project_id);
        write_private_atomic(&path, project_id.to_string().as_bytes())
            .map(|_| ())
            .map_err(|e| write_failure(e, "persist pending Project Conversation cleanup"))
    }

    /// Drop the retry reference. Call only once Conversation cleanup has
    /// finished, or once the Project is known to still exist in config
    /// because its removal never committed. Clearing an absent record is a
    /// no-op so recovery can be repeated.
    pub fn clear_project_conversation_deletion_pending(&self, project_id: Uuid) -> Result<()> {
        let path = self.paths.pending_project_conversation_deletion_file(project_id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("Failed to clear cleanup record {path:?}")),
        }
    }

    /// List valid durable cleanup records without discarding malformed ones.
    pub fn pending_project_conversation_deletions(&self) -> Result<Vec<Uuid>> {
        let directory = self.paths.conversations_dir();
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("Failed to list Conversation cleanup records"),
        };
        let mut pending = Vec::new();
        for entry in entries {
            let entry = entry.context("Failed to read Conversation cleanup directory entry")?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue; };
            let Some(id) = name.strip_suffix(".delete-pending") else { continue; };
            let project_id = id.parse::<Uuid>()
                .with_context(|| format!("Invalid Project id in cleanup record {:?}", entry.path()))?;
            let recorded_id: Uuid = fs::read_to_string(entry.path())
                .with_context(|| format!("Failed to read cleanup record {:?}", entry.path()))?
                .trim().parse()
                .with_context(|| format!("Invalid Project id in cleanup record {:?}", entry.path()))?;
            if recorded_id != project_id {
                anyhow::bail!("Project id does not match cleanup record {:?}", entry.path());
            }
            pending.push(project_id);
        }
        Ok(pending)
    }

    /// Persist the retry reference before removing a Project from config.
    ///
    /// While the record exists, the Project's Task cleanup is owed: it names
    /// the Project whose Task records are still on disk even after the
    /// Project itself is gone, and carries the board files captured while the
    /// Project could still be routed to them (see [`Self::tasks_board_files`]),
    /// because once the removal has persisted no routing recomputes them.
    pub fn mark_project_tasks_deletion_pending(
        &self,
        project_id: Uuid,
        board_files: &[PathBuf],
    ) -> Result<()> {
        self::Storage::check_cleanup_paths_losslessly_recordable(board_files)?;
        let path = self.paths.pending_project_tasks_deletion_file(project_id);
        let mut contents = project_id.to_string();
        for board_file in board_files {
            // Checked by `check_cleanup_paths_losslessly_recordable` above;
            // this stays `.to_str()` so a record line can never be a lossy
            // rendering of a path.
            contents.push('\n');
            contents.push_str(board_file.to_str().expect("checked above"));
        }
        write_private_atomic(&path, contents.as_bytes())
            .map(|_| ())
            .map_err(|e| write_failure(e, "persist pending Project Task cleanup"))
    }

    /// Whether the pending-cleanup record format can name these paths without
    /// loss.
    ///
    /// The record is one path per line, so a newline inside a path would
    /// corrupt it, and the file is read as text, so a non-UTF-8 path cannot be
    /// carried. Refusing is the removal decision this Process cannot carry
    /// out safely; the caller treats it as a refusal that deleted nothing.
    pub fn check_cleanup_paths_losslessly_recordable(board_files: &[PathBuf]) -> Result<()> {
        for board_file in board_files {
            let Some(text) = board_file.to_str() else {
                anyhow::bail!(
                    "the path {board_file:?} is not valid UTF-8, and a pending task cleanup \
                     record cannot carry it faithfully"
                );
            };
            if text.contains('\n') || text.contains('\r') {
                anyhow::bail!(
                    "the path {board_file:?} contains a line break, and a pending task cleanup \
                     record is one path per line"
                );
            }
        }
        Ok(())
    }

    /// Whether an unfinished earlier attempt left a Task cleanup record for
    /// this Project.
    pub fn has_pending_tasks_deletion_record(&self, project_id: Uuid) -> Result<bool> {
        match fs::metadata(self.paths.pending_project_tasks_deletion_file(project_id)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(anyhow::Error::new(error).context(
                "Failed to read pending Task cleanup record",
            )),
        }
    }

    /// Drop the retry reference. Call only once Task cleanup has finished, or
    /// once the Project is known to still exist in config because its removal
    /// never committed. Clearing an absent record is a no-op so recovery can
    /// be repeated.
    pub fn clear_project_tasks_deletion_pending(&self, project_id: Uuid) -> Result<()> {
        let path = self.paths.pending_project_tasks_deletion_file(project_id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("Failed to clear cleanup record {path:?}")),
        }
    }

    /// List valid durable Task cleanup records without discarding malformed
    /// ones. The first line of a record is the Project id; the remaining
    /// lines are the board files captured at deletion time.
    pub fn pending_project_tasks_deletions(&self) -> Result<Vec<ProjectTaskCleanupRecord>> {
        let entries = match fs::read_dir(self.paths.config_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("Failed to list Task cleanup records"),
        };
        let mut pending = Vec::new();
        for entry in entries {
            let entry = entry.context("Failed to read Task cleanup directory entry")?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(id) = name.strip_suffix(".tasks-delete-pending") else {
                continue;
            };
            let project_id = id.parse::<Uuid>().with_context(|| {
                format!(
                    "Invalid Project id in Task cleanup record {:?}",
                    entry.path()
                )
            })?;
            let raw = fs::read_to_string(entry.path())
                .with_context(|| format!("Failed to read Task cleanup record {:?}", entry.path()))?;
            let mut lines = raw.lines();
            let recorded_id: Uuid = lines
                .next()
                .unwrap_or_default()
                .trim()
                .parse()
                .with_context(|| {
                    format!(
                        "Invalid Project id in Task cleanup record {:?}",
                        entry.path()
                    )
                })?;
            if recorded_id != project_id {
                anyhow::bail!("Project id does not match Task cleanup record {:?}", entry.path());
            }
            let board_files = lines
                .map(PathBuf::from)
                .filter(|path| !path.as_os_str().is_empty())
                .collect();
            pending.push(ProjectTaskCleanupRecord {
                project_id,
                board_files,
            });
        }
        Ok(pending)
    }

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
            unloaded_boards: Arc::new(Mutex::new(HashMap::new())),
            config_tx: Arc::new(Mutex::new(())),
            config_unconfirmed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

    /// Load the complete persisted config without salvage, requiring an
    /// explicit `projects` section. Serde defaults that section for older or
    /// incomplete files, but that default cannot prove a Project was deleted.
    pub fn load_config_strict(&self) -> Result<AppConfig> {
        let contents = fs::read_to_string(&self.config_file)
            .context("Failed to read config for Project cleanup recovery")?;
        let value: toml::Value = toml::from_str(&contents)
            .context("Config is not valid TOML for Project cleanup recovery")?;
        if !value
            .as_table()
            .is_some_and(|table| table.contains_key("projects"))
        {
            anyhow::bail!("Config has no projects section; Project absence is not authoritative");
        }
        let config: AppConfig = toml::from_str(&contents)
            .context("Config is incomplete for Project cleanup recovery")?;
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
                        
                        // Try to recover the Queue settings: forgetting an
                        // `auto_promote = false` would start queued Tasks the
                        // person had paused.
                        if let Some(queue) = value.get("queue") {
                            if let Ok(queue_config) = queue.clone().try_into::<crate::config::QueueConfig>() {
                                config.queue = queue_config;
                                eprintln!("  Recovered: queue");
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
    pub fn save_config(&self, config: &AppConfig) -> std::result::Result<(), ConfigWriteError> {
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
    /// The error says whether the file was replaced: see [`ConfigWriteError`].
    ///
    /// `mutate` runs while the transaction guard is held, so it must not
    /// acquire any other lock or block.
    pub fn update_config<F>(&self, mutate: F) -> std::result::Result<(), ConfigWriteError>
    where
        F: FnOnce(&mut AppConfig),
    {
        let _tx = self.lock_config_tx();
        // No wrapping `context` on purpose: callers render this with `{e}`,
        // which shows only the outermost layer, and the reader's own message
        // is the one that tells the user which file to repair and why.
        let mut config = self.read_config_strict().map_err(ConfigWriteError::NotPublished)?;
        mutate(&mut config);
        self.write_config(&config)
    }

    /// Make sure the configuration on disk is confirmed durable, rewriting it
    /// if an earlier write replaced it without confirming.
    ///
    /// Anything destructive that depends on a configuration change having
    /// stuck (deleting a deleted Project's Task files and Conversation, say)
    /// calls this first and stops, keeping its retry records, when it fails.
    /// The rewrite is the full protocol over the file as it is now, so it is
    /// also the retry that can clear the uncertainty. A no-op when the last
    /// write was confirmed.
    ///
    /// A filesystem that cannot synchronize a directory never confirms: the
    /// rewrite succeeds, yet the result is [`ConfigWriteError::Unconfirmable`],
    /// because "the filesystem would not say" is not "the filesystem said yes".
    pub fn confirm_config_durable(&self) -> std::result::Result<(), ConfigWriteError> {
        if !self.config_unconfirmed.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        self.update_config(|_| {})?;
        self.require_config_confirmed()
    }

    fn require_config_confirmed(&self) -> std::result::Result<(), ConfigWriteError> {
        if self.config_unconfirmed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ConfigWriteError::Unconfirmable(anyhow::anyhow!(
                "the filesystem holding the configuration does not support synchronizing a directory"
            )));
        }
        Ok(())
    }

    /// [`Self::confirm_config_durable`] without trusting this process's memory
    /// of what it wrote: rewrite the configuration through the full protocol
    /// once. A new process cannot know that the file it loaded was left
    /// unconfirmed by the previous one, so startup uses this before it deletes
    /// anything on the strength of what the configuration says is absent.
    pub fn reconfirm_config_durable(&self) -> std::result::Result<(), ConfigWriteError> {
        // Unconfirmed until a write completes, so a failure here (including one
        // before publication) keeps every later barrier in this process closed.
        self.config_unconfirmed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.update_config(|_| {})?;
        self.require_config_confirmed()
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
    fn write_config(&self, config: &AppConfig) -> std::result::Result<(), ConfigWriteError> {
        let contents = toml::to_string_pretty(config)
            .context("Failed to serialize config")
            .map_err(ConfigWriteError::NotPublished)?;
        match write_private_atomic(&self.config_file, contents.as_bytes()) {
            Ok(committed) => {
                // `Unsupported` is a write that succeeded without anyone being
                // able to confirm the replacement itself: the barrier stays
                // closed, but an ordinary save is not refused for it.
                self.config_unconfirmed
                    .store(!committed.is_fully_durable(), std::sync::atomic::Ordering::SeqCst);
                self.refresh_routes(config);
                Ok(())
            }
            Err(error) if error.is_published() => {
                // The file holds this config now, so the routes derived from
                // it follow it before the failure is reported.
                self.config_unconfirmed.store(true, std::sync::atomic::Ordering::SeqCst);
                self.refresh_routes(config);
                Err(ConfigWriteError::PublishedNotDurable(
                    anyhow::Error::new(error).context("Failed to write config file"),
                ))
            }
            Err(error) => Err(ConfigWriteError::NotPublished(
                anyhow::Error::new(error).context("Failed to write config file"),
            )),
        }
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
        self.paths.legacy_tasks_dir()
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

/// The phrase every board-write error uses when the new content is already in
/// place. Callers that only have the rendered message (see `lifecycle::publish`)
/// use [`says_in_place_but_unconfirmed`] to tell this from a write that changed
/// nothing.
const NOT_CONFIRMED_DURABLE: &str = "in place but not confirmed durable";

pub fn says_in_place_but_unconfirmed(message: &str) -> bool {
    message.contains(NOT_CONFIRMED_DURABLE)
}

/// A durable-write failure as an `anyhow` error whose outermost message is
/// true. Callers render with `{e}`, which shows only that layer, so it must
/// itself say when the new content is already in place: "failed to write X"
/// for a write that replaced X would be a false report.
fn write_failure(error: slashit_durable_fs::WriteError, what: &str) -> anyhow::Error {
    let published = error.is_published();
    let error = anyhow::Error::new(error);
    if published {
        error.context(format!(
            "{what}: the new content is {NOT_CONFIRMED_DURABLE} (it may be lost if \
             the computer stops before the next successful save)"
        ))
    } else {
        error.context(format!("Failed to {what}"))
    }
}

/// Whether a failed [`Storage::save_project_tasks`] had already replaced the
/// board file: the new board is what every reader sees, but its durability is
/// unconfirmed ([`slashit_durable_fs::WriteError::PublishedNotDurable`]).
///
/// A caller that keeps the board in memory must then adopt what it tried to
/// save, the way [`adopt_published`] does for configuration. Leaving memory on
/// the previous board would let the next save, of any Task in that Project,
/// write the whole previous board back over a change the file already holds,
/// which undoes the operation silently.
pub fn board_write_published(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<slashit_durable_fs::WriteError>()
        .is_some_and(slashit_durable_fs::WriteError::is_published)
}

/// Write a file durably and atomically with owner-only permissions.
///
/// The protocol, its guarantee and its failure model live in
/// [`slashit_durable_fs`]. A failure is a [`slashit_durable_fs::WriteError`];
/// one that says the new content is already in place but not confirmed durable
/// is not a write that changed nothing.
fn write_private_atomic(
    path: &Path,
    bytes: &[u8],
) -> std::result::Result<slashit_durable_fs::Committed, slashit_durable_fs::WriteError> {
    slashit_durable_fs::write_atomic(path, bytes, slashit_durable_fs::Permissions::OwnerOnly)
}

impl Default for Storage {
    fn default() -> Self {
        Self::new().expect("Failed to create storage")
    }
}

impl Storage {
    /// Read one tasks file, applying reference migration.
    ///
    /// Only a missing file is `Absent`. Any other failure (permissions, bytes
    /// that are not UTF-8, TOML that does not parse into the current types, a
    /// layout version this build does not know) is `Unreadable`, because the
    /// file is there and holds something this run cannot account for.
    fn inspect_tasks_file(path: &Path) -> TasksFileState {
        let unreadable = |reason: String| {
            TasksFileState::Unreadable(UnreadableTasksFile {
                path: path.to_path_buf(),
                reason,
            })
        };
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return TasksFileState::Absent,
            Err(e) => return unreadable(format!("read failed: {e}")),
        };
        match toml::from_str::<ProjectTasksFile>(&contents) {
            Ok(file) if file.version > TASKS_FILE_VERSION => unreadable(format!(
                "layout version {} is newer than this SlashIt understands ({TASKS_FILE_VERSION})",
                file.version
            )),
            Ok(file) => {
                let mut tasks = file.tasks;
                for task in &mut tasks {
                    migrate_task_refs(task);
                }
                TasksFileState::Board(tasks)
            }
            Err(e) => unreadable(format!("parse failed: {e}")),
        }
    }

    /// Load every task across every project.
    ///
    /// A board that cannot be read is reported and skipped rather than
    /// propagated: one corrupt project file must not stop the other projects
    /// from loading. It is remembered, so a save never replaces it from a
    /// board built without it.
    ///
    /// Reads each routed project's own state directory, then sweeps the legacy
    /// global directory for projects that have not been routed or not yet
    /// migrated. The legacy sweep is what keeps an upgrade non-destructive:
    /// tasks stay visible until the project that owns them is saved once.
    pub fn load_all_tasks(&self) -> Result<Vec<Task>> {
        let mut all_tasks = Vec::new();
        let mut unloaded = match self.unloaded_boards.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for (owner, source) in self.task_files()? {
            match Self::inspect_tasks_file(&source) {
                TasksFileState::Board(tasks) => {
                    if let Some(owner) = owner {
                        unloaded.remove(&owner);
                    }
                    all_tasks.extend(tasks);
                }
                TasksFileState::Absent => {}
                TasksFileState::Unreadable(e) => {
                    eprintln!("Warning: {e}");
                    if let Some(owner) = owner {
                        unloaded.insert(owner, source);
                    }
                }
            }
        }
        Ok(all_tasks)
    }

    /// The board files [`Self::load_all_tasks`] reads that it cannot read or
    /// parse. It skips those with a warning, so a task in one is absent from
    /// every list built from it; whatever decides that no task owns
    /// something must ask first.
    pub fn unreadable_task_files(&self) -> Result<Vec<PathBuf>> {
        let mut unreadable = Vec::new();
        for (_, source) in self.task_files()? {
            if matches!(
                Self::inspect_tasks_file(&source),
                TasksFileState::Unreadable(_)
            ) {
                unreadable.push(source);
            }
        }
        Ok(unreadable)
    }

    /// Every file a task can be loaded from, each project's own first and
    /// the legacy ones no project has claimed after, with the project each
    /// belongs to when that is known (a legacy file is named for its project).
    fn task_files(&self) -> Result<Vec<(Option<Uuid>, PathBuf)>> {
        let mut files = Vec::new();
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

            if path.is_file() {
                files.push((Some(*project_id), path));
            } else if legacy.is_file() {
                // Not migrated yet: read from where the tasks still are, so
                // nothing disappears on upgrade.
                files.push((Some(*project_id), legacy));
            }
        }

        let legacy_dir = self.legacy_tasks_dir();
        if legacy_dir.is_dir() {
            for entry in fs::read_dir(&legacy_dir).context("Failed to read tasks directory")? {
                let path = entry?.path();
                if path.extension().is_some_and(|ext| ext == "toml")
                    && !seen_files.contains(&path)
                {
                    let owner = path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .and_then(|stem| Uuid::parse_str(stem).ok());
                    files.push((owner, path));
                }
            }
        }

        Ok(files)
    }

    /// The file that holds this project's board right now: its own
    /// `tasks.toml`, else the legacy file it has not been migrated out of.
    /// Absent carries the routed path, where a first save would write.
    fn project_board(&self, project_id: Uuid) -> (PathBuf, TasksFileState) {
        let path = self.tasks_path(project_id);
        let legacy = self.legacy_tasks_path(project_id);
        Self::board_at(path, &legacy)
    }

    fn board_at(path: PathBuf, legacy: &Path) -> (PathBuf, TasksFileState) {
        match Self::inspect_tasks_file(&path) {
            TasksFileState::Absent if legacy != path => {
                match Self::inspect_tasks_file(legacy) {
                    TasksFileState::Absent => (path, TasksFileState::Absent),
                    state => (legacy.to_path_buf(), state),
                }
            }
            state => (path, state),
        }
    }

    /// Load tasks for a specific project, falling back to the legacy location.
    ///
    /// A project with no file has no tasks. A project whose file exists but
    /// cannot be read is an error, not an empty board: callers that went on to
    /// save would otherwise replace the file with nothing.
    pub fn load_project_tasks(&self, project_id: Uuid) -> Result<Vec<Task>> {
        match self.project_board(project_id).1 {
            TasksFileState::Absent => Ok(Vec::new()),
            TasksFileState::Board(tasks) => Ok(tasks),
            TasksFileState::Unreadable(e) => Err(e.into()),
        }
    }

    /// Refuse when the file this save would replace cannot be read, or could
    /// not be when the board in memory was loaded. The check looks at the
    /// disk, so a stale or empty in-memory board cannot get past a file that
    /// is bad now; [`Self::unloaded_boards`] covers the one that is good now
    /// but was bad when memory was built.
    fn ensure_board_replaceable(
        &self,
        project_id: Uuid,
        path: &Path,
        legacy: &Path,
    ) -> Result<()> {
        let (_, state) = Self::board_at(path.to_path_buf(), legacy);
        if let TasksFileState::Unreadable(e) = state {
            return Err(e.into());
        }
        let skipped = match self.unloaded_boards.lock() {
            Ok(guard) => guard.get(&project_id).cloned(),
            Err(poisoned) => poisoned.into_inner().get(&project_id).cloned(),
        };
        match skipped {
            Some(skipped) => Err(UnreadableTasksFile {
                path: skipped,
                reason: "it could not be read when SlashIt started, so the board in memory \
                         does not include it"
                    .to_string(),
            }
            .into()),
            None => Ok(()),
        }
    }

    /// Whether the directory that holds this project's board can be
    /// synchronized here. `Unsupported` means a board write can succeed without
    /// surviving a power loss, so a caller that must not lose the write checks
    /// this first. It flushes the directory's entries and changes no content.
    pub fn board_directory_sync(&self, project_id: Uuid) -> Result<slashit_durable_fs::DirectorySync> {
        let path = self.tasks_path(project_id);
        let dir = path
            .parent()
            .context("the tasks file has no containing directory")?;
        slashit_durable_fs::sync_directory(dir).context("Failed to synchronize the tasks directory")
    }

    /// Save tasks for a specific project.
    ///
    /// Writes to the project's current state directory and then retires the
    /// legacy file, so the move happens exactly once and only after the new
    /// copy is safely on disk.
    ///
    /// Refuses with [`UnreadableTasksFile`] when the file being replaced (or,
    /// for an unmigrated project, the legacy file it would shadow) exists but
    /// cannot be read. The check runs before anything is written and again
    /// just before the rename. That narrows, but does not close, the window in
    /// which another process could swap in an unreadable file: it runs from the
    /// end of the second read, parse included, to the rename. SlashIt's own
    /// writers always replace the file atomically with a readable one, so they
    /// cannot open it; an outside process (an editor, a second instance, a
    /// restore) can, and only a cross-process lock would close it.
    pub fn save_project_tasks(&self, project_id: Uuid, tasks: &[Task]) -> Result<()> {
        let path = self.tasks_path(project_id);
        let legacy = self.legacy_tasks_path(project_id);
        self.ensure_board_replaceable(project_id, &path, &legacy)?;

        let tasks_file = ProjectTasksFile {
            version: TASKS_FILE_VERSION,
            tasks: tasks.to_vec(),
        };
        let contents = toml::to_string_pretty(&tasks_file).context("Failed to serialize tasks")?;

        let staged = slashit_durable_fs::stage(&path, contents.as_bytes(), slashit_durable_fs::Permissions::Preserve)
            .map_err(|e| write_failure(e, "write tasks file"))?;
        // The last look at the file being replaced, as close to the rename as
        // the protocol allows. Dropping `staged` removes the temporary file.
        self.ensure_board_replaceable(project_id, &path, &legacy)?;
        // A `PublishedNotDurable` error leaves the new board in place, so the
        // legacy copy below must not be retired on that path: it is retired
        // only after the new file's write completed and was confirmed. On a
        // filesystem that cannot synchronize a directory it is not retired at
        // all: removing the only other copy while the new file's entry is
        // unproven could lose the board. The routed file shadows the legacy
        // one, so keeping it costs nothing.
        let committed = staged.publish().map_err(|e| write_failure(e, "write tasks file"))?;

        if legacy != path && legacy.is_file() && committed.is_fully_durable() {
            // A legacy copy that cannot be read is the only record of
            // whatever it holds. The routed file already shadows it, so
            // leaving it costs nothing and deleting it could not be undone.
            if matches!(
                Self::inspect_tasks_file(&legacy),
                TasksFileState::Unreadable(_)
            ) {
                eprintln!("Warning: Keeping unreadable legacy tasks file {legacy:?}");
            } else if let Err(e) = fs::remove_file(&legacy) {
                eprintln!("Warning: Failed to remove legacy tasks file {legacy:?}: {e}");
            }
        }

        Ok(())
    }

    pub fn delete_task_files(&self, board_files: &[PathBuf]) -> Result<()> {
        for path in board_files {
            if path.exists() {
                fs::remove_file(path)
                    .with_context(|| format!("Failed to delete project tasks file {path:?}"))?;
            }
        }
        Ok(())
    }

    /// The files that may hold this project's board: its routed `tasks.toml`
    /// and the legacy one. Both are candidates, whichever exists.
    ///
    /// Capture the list *before* the Project leaves the config: the routing
    /// table is rebuilt from the config, so once the Project is gone this
    /// listing can only produce the legacy path and a routed project's board
    /// would be stranded forever. A Task cleanup record therefore carries the
    /// captured list, which is how a restart finishes the cleanup without the
    /// Project to route from.
    pub fn tasks_board_files(&self, project_id: Uuid) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Some(dir) = self.routed_state_dir(project_id) {
            files.push(dir.join(TASKS_FILE));
        }
        let legacy = self.legacy_tasks_path(project_id);
        if !files.contains(&legacy) {
            files.push(legacy);
        }
        files
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

    #[test]
    fn primary_project_conversation_has_its_own_identity_and_survives_reload() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let conversation = storage.create_primary_conversation(project_id).unwrap();
        assert_ne!(conversation.id, project_id);
        let mut changed = conversation.clone();
        changed.push(
            crate::domain::conversation::Role::Human,
            crate::domain::conversation::EntryKind::HumanMessage {
                text: "No tasks needed".into(),
            },
        );
        storage.save_conversation(&changed).unwrap();
        let restarted_storage = storage.clone();
        let restored = restarted_storage
            .load_primary_conversation(project_id)
            .unwrap()
            .unwrap();
        assert_eq!(restored.id, conversation.id);
        assert_eq!(restored.project_id, project_id);
        assert_eq!(restored.entries.len(), 1);
        assert!(restarted_storage
            .load_primary_conversation(Uuid::new_v4())
            .unwrap()
            .is_none());
    }

    #[test]
    fn deleting_project_conversation_removes_owned_pointer_and_document() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let conversation = storage.create_primary_conversation(project_id).unwrap();
        let document = storage.paths.conversation_file(conversation.id);
        let pointer = storage.paths.primary_conversation_file(project_id);
        storage.delete_project_conversation(project_id).unwrap();
        assert!(!pointer.exists());
        assert!(!document.exists());
    }

    #[test]
    fn malformed_conversation_pointer_is_preserved() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let pointer = storage.paths.primary_conversation_file(project_id);
        fs::create_dir_all(pointer.parent().unwrap()).unwrap();
        fs::write(&pointer, "not-a-uuid").unwrap();

        assert!(storage.delete_project_conversation(project_id).is_err());
        assert_eq!(fs::read_to_string(pointer).unwrap(), "not-a-uuid");
    }

    #[test]
    fn malformed_conversation_document_preserves_pointer_and_document() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let conversation_id = Uuid::new_v4();
        let pointer = storage.paths.primary_conversation_file(project_id);
        let document = storage.paths.conversation_file(conversation_id);
        fs::create_dir_all(document.parent().unwrap()).unwrap();
        fs::write(&pointer, conversation_id.to_string()).unwrap();
        fs::write(&document, "not-json").unwrap();

        assert!(storage.delete_project_conversation(project_id).is_err());
        assert!(pointer.exists());
        assert_eq!(fs::read_to_string(document).unwrap(), "not-json");
    }

    #[test]
    fn unreadable_conversation_document_preserves_pointer() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let conversation_id = Uuid::new_v4();
        let pointer = storage.paths.primary_conversation_file(project_id);
        let document = storage.paths.conversation_file(conversation_id);
        fs::create_dir_all(&document).unwrap();
        fs::write(&pointer, conversation_id.to_string()).unwrap();

        assert!(storage.delete_project_conversation(project_id).is_err());
        assert!(pointer.exists());
        assert!(document.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_conversation_document_is_not_treated_as_deleted() {
        use std::os::unix::fs::symlink;

        let (storage, temp) = create_test_storage();
        let project_id = Uuid::new_v4();
        let conversation = storage.create_primary_conversation(project_id).unwrap();
        let document = storage.paths.conversation_file(conversation.id);
        fs::remove_file(&document).unwrap();
        let target = temp.path().join("conversation-target.json");
        fs::write(&target, serde_json::to_vec(&conversation).unwrap()).unwrap();
        symlink(&target, &document).unwrap();
        let pointer = storage.paths.primary_conversation_file(project_id);

        assert!(storage.delete_project_conversation(project_id).is_err());
        assert!(pointer.exists());
        assert!(fs::symlink_metadata(document).unwrap().file_type().is_symlink());
        assert!(target.exists());
    }

    #[test]
    fn mismatched_conversation_pointer_never_deletes_another_projects_document() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let other_project_id = Uuid::new_v4();
        let other = storage
            .create_primary_conversation(other_project_id)
            .unwrap();
        let pointer = storage.paths.primary_conversation_file(project_id);
        write_private_atomic(&pointer, other.id.to_string().as_bytes()).unwrap();
        let document = storage.paths.conversation_file(other.id);
        assert!(storage.delete_project_conversation(project_id).is_err());
        assert!(pointer.exists());
        assert!(document.exists());
    }

    #[test]
    fn pointer_to_document_with_mismatched_identity_preserves_both_files() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let pointer_id = Uuid::new_v4();
        let mut conversation = Conversation::new(project_id);
        conversation.id = Uuid::new_v4();
        let pointer = storage.paths.primary_conversation_file(project_id);
        let document = storage.paths.conversation_file(pointer_id);
        fs::create_dir_all(document.parent().unwrap()).unwrap();
        fs::write(&pointer, pointer_id.to_string()).unwrap();
        fs::write(&document, serde_json::to_vec(&conversation).unwrap()).unwrap();

        assert!(storage.delete_project_conversation(project_id).is_err());
        assert!(pointer.exists());
        assert!(document.exists());
    }

    #[test]
    fn stale_owned_pointer_after_document_removal_is_safe_to_retry() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let conversation = storage.create_primary_conversation(project_id).unwrap();
        let document = storage.paths.conversation_file(conversation.id);
        let pointer = storage.paths.primary_conversation_file(project_id);
        fs::remove_file(document).unwrap();

        storage.delete_project_conversation(project_id).unwrap();
        storage.delete_project_conversation(project_id).unwrap();
        assert!(!pointer.exists());
    }

    #[test]
    fn pending_deletion_finds_document_orphaned_before_pointer_publication() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let conversation = storage.create_primary_conversation(project_id).unwrap();
        let document = storage.paths.conversation_file(conversation.id);
        let pointer = storage.paths.primary_conversation_file(project_id);
        fs::remove_file(&pointer).unwrap();
        let foreign = storage.create_primary_conversation(Uuid::new_v4()).unwrap();
        let foreign_document = storage.paths.conversation_file(foreign.id);
        fs::remove_file(storage.paths.primary_conversation_file(foreign.project_id)).unwrap();
        storage.mark_project_conversation_deletion_pending(project_id).unwrap();

        storage.delete_project_conversation(project_id).unwrap();
        storage.delete_project_conversation(project_id).unwrap();

        assert!(!document.exists());
        assert!(!pointer.exists());
        assert!(foreign_document.exists());
        assert!(storage.paths.pending_project_conversation_deletion_file(project_id).exists());
    }

    #[test]
    fn pending_project_conversation_cleanup_survives_storage_restart() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        storage.mark_project_conversation_deletion_pending(project_id).unwrap();
        let restarted = Storage::with_paths(storage.paths().clone());
        assert_eq!(restarted.pending_project_conversation_deletions().unwrap(), vec![project_id]);
    }

    #[test]
    fn a_board_that_cannot_be_parsed_is_reported_not_skipped_silently() {
        let (storage, _dir) = create_test_storage();
        assert!(storage.unreadable_task_files().unwrap().is_empty());
        let legacy = storage.legacy_tasks_dir();
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(format!("{}.toml", Uuid::new_v4())), "tasks = [ not toml").unwrap();
        fs::write(legacy.join(format!("{}.toml", Uuid::new_v4())), "version = 1\ntasks = []\n").unwrap();
        assert_eq!(storage.unreadable_task_files().unwrap().len(), 1);
        assert!(storage.load_all_tasks().unwrap().is_empty());
    }

    // ==================== Unreadable Task file tests ====================

    /// A board with one Pending local integration, as written by `save`.
    fn board_with_pending_integration(project_id: Uuid) -> Vec<Task> {
        let mut task = crate::test_helpers::create_test_task("Has a pending integration");
        task.project_id = project_id;
        task.local_integration = Some(crate::domain::LocalIntegration {
            state: crate::domain::IntegrationState::Pending,
            target_branch: "main".into(),
            target_old_oid: "a".repeat(40),
            approved_tip: "b".repeat(40),
            result_oid: "c".repeat(40),
            strategy: crate::domain::IntegrationStrategy::FastForward,
            checkout_path: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            completes_task: false,
        });
        vec![task]
    }

    /// Ways a Task file can exist and still not be a board, each as the bytes
    /// to leave on disk.
    fn unreadable_variants(valid: &str) -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("malformed TOML", b"tasks = [ not toml".to_vec()),
            (
                "unknown status",
                valid.replacen("status = \"backlog\"", "status = \"from_the_future\"", 1).into_bytes(),
            ),
            ("not UTF-8", vec![0xff, 0xfe, 0x00, 0x80]),
            (
                "newer layout version",
                valid.replacen("version = 1", "version = 2", 1).into_bytes(),
            ),
        ]
    }

    fn valid_board_text(project_id: Uuid) -> String {
        toml::to_string_pretty(&ProjectTasksFile {
            version: 1,
            tasks: board_with_pending_integration(project_id),
        })
        .unwrap()
    }

    #[test]
    fn a_missing_file_and_an_empty_board_are_both_loadable_but_different() {
        let (storage, _dir) = create_test_storage();
        let missing = Uuid::new_v4();
        assert!(matches!(storage.project_board(missing).1, TasksFileState::Absent));
        assert!(storage.load_project_tasks(missing).unwrap().is_empty());

        let empty = Uuid::new_v4();
        storage.save_project_tasks(empty, &[]).unwrap();
        assert!(matches!(
            storage.project_board(empty).1,
            TasksFileState::Board(ref tasks) if tasks.is_empty()
        ));
        assert!(storage.load_project_tasks(empty).unwrap().is_empty());
        // Both stay writable.
        storage.save_project_tasks(missing, &board_with_pending_integration(missing)).unwrap();
        storage.save_project_tasks(empty, &board_with_pending_integration(empty)).unwrap();
    }

    #[test]
    fn an_unreadable_board_is_refused_and_left_byte_for_byte() {
        for (what, bytes) in unreadable_variants(&valid_board_text(Uuid::new_v4())) {
            let (storage, _dir) = create_test_storage();
            let project_id = Uuid::new_v4();
            let path = storage.tasks_path(project_id);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &bytes).unwrap();

            let loaded = storage.load_project_tasks(project_id);
            let error = loaded.expect_err(what);
            assert!(error.downcast_ref::<UnreadableTasksFile>().is_some(), "{what}");

            // The ordinary create/update path: an empty in-memory board plus
            // one new Task, then a later update of it.
            let mut fresh = crate::test_helpers::create_test_task("Created after a failed load");
            fresh.project_id = project_id;
            for attempt in [vec![fresh.clone()], vec![]] {
                let error = storage
                    .save_project_tasks(project_id, &attempt)
                    .expect_err(what);
                let refusal = error.downcast_ref::<UnreadableTasksFile>().expect(what);
                assert_eq!(refusal.path, path, "{what}");
                assert!(error.to_string().contains("will not overwrite"), "{what}");
                assert_eq!(fs::read(&path).unwrap(), bytes, "{what}: bytes must not change");
            }

            // Nothing was left behind next to it, and a restart sees the same.
            let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(leftovers.len(), 1, "{what}: no temp file may remain: {leftovers:?}");
            let restarted = Storage::with_paths(storage.paths().clone());
            assert!(restarted.save_project_tasks(project_id, &[fresh]).is_err(), "{what}");
            assert_eq!(fs::read(&path).unwrap(), bytes, "{what}");
        }
    }

    #[test]
    fn an_unreadable_board_does_not_stop_healthy_projects() {
        let (storage, _dir) = create_test_storage();
        let broken = Uuid::new_v4();
        let healthy = Uuid::new_v4();
        storage.save_project_tasks(healthy, &board_with_pending_integration(healthy)).unwrap();
        let broken_path = storage.tasks_path(broken);
        fs::create_dir_all(broken_path.parent().unwrap()).unwrap();
        fs::write(&broken_path, b"tasks = [ not toml").unwrap();

        let loaded = storage.load_all_tasks().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(storage.unreadable_task_files().unwrap(), vec![broken_path.clone()]);

        let mut more = board_with_pending_integration(healthy);
        more.push(crate::test_helpers::create_test_task("Second"));
        storage.save_project_tasks(healthy, &more).unwrap();
        assert_eq!(storage.load_project_tasks(healthy).unwrap().len(), 2);
        assert_eq!(fs::read(&broken_path).unwrap(), b"tasks = [ not toml");
    }

    #[test]
    fn a_pending_integration_survives_an_unreadable_board_and_returns_once_repaired() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let valid = valid_board_text(project_id);
        let path = storage.tasks_path(project_id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let damaged = valid.replacen("status = \"backlog\"", "status = \"from_the_future\"", 1);
        fs::write(&path, &damaged).unwrap();

        assert!(storage.save_project_tasks(project_id, &[]).is_err());
        let kept = fs::read_to_string(&path).unwrap();
        assert!(kept.contains("[tasks.local_integration]"), "{kept}");
        assert!(kept.contains("state = \"pending\""), "{kept}");

        // The owner repairs it by hand; the same storage picks it straight up.
        fs::write(&path, &valid).unwrap();
        let tasks = storage.load_project_tasks(project_id).unwrap();
        assert!(tasks[0].local_integration.is_some());
        storage.save_project_tasks(project_id, &tasks).unwrap();
    }

    #[test]
    fn a_stale_board_cannot_overwrite_a_file_that_went_bad_after_it_was_loaded() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        storage.save_project_tasks(project_id, &board_with_pending_integration(project_id)).unwrap();
        let in_memory = storage.load_project_tasks(project_id).unwrap();

        let path = storage.tasks_path(project_id);
        fs::write(&path, b"tasks = [ not toml").unwrap();
        assert!(storage.save_project_tasks(project_id, &in_memory).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"tasks = [ not toml");
    }

    #[test]
    fn a_routed_project_refuses_over_its_own_file_and_leaves_others_alone() {
        let (storage, temp) = create_test_storage();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let project_id = Uuid::new_v4();
        route_project_to(&storage, project_id, &repo);

        storage.save_project_tasks(project_id, &board_with_pending_integration(project_id)).unwrap();
        let routed = storage.tasks_path(project_id);
        assert_ne!(routed, storage.legacy_tasks_path(project_id));
        fs::write(&routed, b"tasks = [ not toml").unwrap();

        assert!(storage.save_project_tasks(project_id, &[]).is_err());
        assert_eq!(fs::read(&routed).unwrap(), b"tasks = [ not toml");
        // A project with no routing is unaffected.
        let other = Uuid::new_v4();
        storage.save_project_tasks(other, &[]).unwrap();
    }

    #[test]
    fn an_unreadable_legacy_file_is_not_shadowed_or_deleted_by_a_migrating_save() {
        let (storage, temp) = create_test_storage();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let project_id = Uuid::new_v4();
        route_project_to(&storage, project_id, &repo);

        // Unmigrated: nothing routed yet, the only board is the unreadable
        // legacy one, so writing a routed file would hide it for good.
        let legacy = storage.legacy_tasks_path(project_id);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, b"tasks = [ not toml").unwrap();
        assert!(storage.load_project_tasks(project_id).is_err());
        assert!(storage.save_project_tasks(project_id, &[]).is_err());
        assert!(!storage.tasks_path(project_id).exists());
        assert_eq!(fs::read(&legacy).unwrap(), b"tasks = [ not toml");

        // Migrated, then a stale unreadable legacy copy: the routed save
        // proceeds but never deletes the unreadable bytes.
        let routed = storage.tasks_path(project_id);
        fs::create_dir_all(routed.parent().unwrap()).unwrap();
        fs::write(&routed, valid_board_text(project_id)).unwrap();
        storage.save_project_tasks(project_id, &board_with_pending_integration(project_id)).unwrap();
        assert_eq!(fs::read(&legacy).unwrap(), b"tasks = [ not toml");
    }

    #[test]
    fn a_readable_legacy_file_is_still_migrated_and_retired() {
        let (storage, temp) = create_test_storage();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let project_id = Uuid::new_v4();
        route_project_to(&storage, project_id, &repo);

        let legacy = storage.legacy_tasks_path(project_id);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, valid_board_text(project_id)).unwrap();
        let tasks = storage.load_project_tasks(project_id).unwrap();
        assert_eq!(tasks.len(), 1);
        storage.save_project_tasks(project_id, &tasks).unwrap();
        assert!(storage.tasks_path(project_id).is_file());
        assert!(!legacy.exists());
    }

    // ==================== Durable write semantics ====================

    fn write_error(error: &anyhow::Error) -> &slashit_durable_fs::WriteError {
        error
            .downcast_ref::<slashit_durable_fs::WriteError>()
            .unwrap_or_else(|| panic!("not a durable write failure: {error:#}"))
    }

    #[test]
    fn a_board_save_that_fails_before_publication_keeps_the_previous_board() {
        use slashit_durable_fs::{fault, Stage};
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let mut first = crate::test_helpers::create_test_task("first");
        first.project_id = project_id;
        storage.save_project_tasks(project_id, std::slice::from_ref(&first)).unwrap();
        let before = fs::read(storage.tasks_path(project_id)).unwrap();

        let mut second = crate::test_helpers::create_test_task("second");
        second.project_id = project_id;
        let _fault = fault::fail_at(Stage::SyncTemp, std::io::ErrorKind::Other);
        let error = storage.save_project_tasks(project_id, &[first, second]).unwrap_err();

        assert!(!write_error(&error).is_published(), "{error:#}");
        assert_eq!(fs::read(storage.tasks_path(project_id)).unwrap(), before);
        let strays: Vec<_> = fs::read_dir(storage.tasks_path(project_id).parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
    }

    #[test]
    fn a_board_that_is_published_but_not_durable_is_reported_and_keeps_the_legacy_copy() {
        use slashit_durable_fs::{fault, Stage};
        let (storage, temp) = create_test_storage();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let project_id = Uuid::new_v4();
        route_project_to(&storage, project_id, &repo);
        let legacy = storage.legacy_tasks_path(project_id);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, valid_board_text(project_id)).unwrap();
        let tasks = storage.load_project_tasks(project_id).unwrap();

        let _fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
        let error = storage.save_project_tasks(project_id, &tasks).unwrap_err();

        // The new board is what readers see, so this is not "nothing happened"...
        assert!(write_error(&error).is_published(), "{error:#}");
        assert!(storage.tasks_path(project_id).is_file());
        // ...but the move is not confirmed, so the old copy is not retired.
        assert!(legacy.exists(), "the legacy board must outlive an unconfirmed move");
        // A later successful save completes the move.
        storage.save_project_tasks(project_id, &tasks).unwrap();
        assert!(!legacy.exists());
    }

    #[test]
    fn the_board_guard_runs_after_the_temporary_file_is_written_and_removes_it() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let path = storage.tasks_path(project_id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"tasks = [ not toml").unwrap();
        assert!(storage.save_project_tasks(project_id, &[]).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"tasks = [ not toml");
        let strays = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(strays, 0);
    }

    #[test]
    fn a_config_published_but_not_durable_still_updates_routes_and_reports_it() {
        use slashit_durable_fs::{fault, Stage};
        let (storage, dir) = create_test_storage();
        let (donor, _donor_dir) = create_test_storage();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let project_id = Uuid::new_v4();
        let routed = route_project_to(&donor, project_id, &repo);

        let _fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
        let error = storage.update_config(|config| *config = routed.clone()).unwrap_err();

        assert!(error.is_published(), "{error:#}");
        assert!(storage.routed_state_dir(project_id).is_some(), "routes follow the visible config");
        assert!(storage.read_config_strict().unwrap().projects.contains_key(&project_id.to_string()));
    }

    #[test]
    fn an_unconfirmed_config_is_rewritten_by_the_barrier_and_only_then_confirmed() {
        use slashit_durable_fs::{fault, Stage};
        let (storage, _dir) = create_test_storage();
        storage.confirm_config_durable().expect("nothing to confirm is not a failure");

        let fault_a = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
        let error = storage
            .update_config(|config| config.ui_preferences.theme = "light".to_string())
            .unwrap_err();
        drop(fault_a);
        assert!(error.is_published());

        // Still failing: the barrier says so and the file keeps the change.
        let fault_b = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
        assert!(storage.confirm_config_durable().unwrap_err().is_published());
        drop(fault_b);
        assert_eq!(storage.load_config().unwrap().ui_preferences.theme, "light");

        // The rewrite that completes is the confirmation, and clears the state.
        storage.confirm_config_durable().expect("the retry completes");
        let trace = fault::trace();
        storage.confirm_config_durable().unwrap();
        assert!(trace.stages().is_empty(), "a confirmed config is not rewritten again");
    }

    /// A Conversation is read from disk at the start of every request, never held
    /// across requests, so a save that published without confirming is simply
    /// what the next request loads; there is no stale copy to write back.
    #[test]
    fn a_conversation_published_but_not_durable_is_what_the_next_request_loads_and_extends() {
        use slashit_durable_fs::{fault, Stage};
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let mut conversation = storage.create_primary_conversation(project_id).unwrap();
        conversation.push(
            crate::domain::Role::Human,
            crate::domain::EntryKind::HumanMessage { text: "first".into() },
        );

        let fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
        let error = storage.save_conversation(&conversation).unwrap_err();
        drop(fault);
        assert!(board_write_published(&error), "{error:#}");
        assert!(format!("{error}").contains("not confirmed durable"), "{error:#}");

        // The next request: load, extend, save. The earlier entry is still there.
        let mut reloaded = storage.load_primary_conversation(project_id).unwrap().unwrap();
        assert_eq!(reloaded.entries.len(), 1);
        reloaded.push(
            crate::domain::Role::Human,
            crate::domain::EntryKind::HumanMessage { text: "second".into() },
        );
        storage.save_conversation(&reloaded).expect("the next save completes");
        assert_eq!(storage.load_primary_conversation(project_id).unwrap().unwrap().entries.len(), 2);
    }

    #[test]
    fn the_rendered_board_error_says_whether_the_new_board_is_in_place() {
        use slashit_durable_fs::{fault, Stage};
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let published = {
            let _f = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
            storage.save_project_tasks(project_id, &[]).unwrap_err()
        };
        let unpublished = {
            let _f = fault::fail_at(Stage::Replace, std::io::ErrorKind::Other);
            storage.save_project_tasks(project_id, &[]).unwrap_err()
        };
        assert!(says_in_place_but_unconfirmed(&published.to_string()));
        assert!(!says_in_place_but_unconfirmed(&unpublished.to_string()));
    }

    #[test]
    fn a_filesystem_that_cannot_sync_directories_never_confirms_the_config() {
        use slashit_durable_fs::{fault, Stage};
        let (storage, _dir) = create_test_storage();
        storage.confirm_config_durable().unwrap();

        let fault = fault::fail_every(Stage::SyncDirectory, std::io::ErrorKind::Unsupported);
        // An ordinary save is not refused for it ...
        storage
            .update_config(|config| config.ui_preferences.theme = "light".to_string())
            .expect("the write itself succeeds");
        assert_eq!(storage.load_config().unwrap().ui_preferences.theme, "light");
        // ... but nothing destructive may lean on it, however often it is asked.
        for _ in 0..2 {
            let error = storage.confirm_config_durable().unwrap_err();
            assert!(matches!(error, ConfigWriteError::Unconfirmable(_)), "{error:?}");
            assert!(!error.is_published());
        }
        assert!(matches!(storage.reconfirm_config_durable().unwrap_err(), ConfigWriteError::Unconfirmable(_)));

        // The same storage on a filesystem that can answer clears it.
        drop(fault);
        storage.confirm_config_durable().expect("a synced rewrite confirms");
        storage.confirm_config_durable().unwrap();
    }

    #[test]
    fn a_legacy_board_is_kept_when_the_new_boards_directory_entry_is_unproven() {
        use slashit_durable_fs::{fault, Stage};
        let (storage, dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let legacy = storage.legacy_tasks_path(project_id);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        storage.save_project_tasks(project_id, &[]).unwrap();
        assert!(legacy.is_file());
        let (donor, _donor_dir) = create_test_storage();
        let routed = route_project_to(&donor, project_id, &repo);
        storage.update_config(|config| *config = routed.clone()).unwrap();
        assert_ne!(storage.tasks_path(project_id), legacy);

        let _fault = fault::fail_every(Stage::SyncDirectory, std::io::ErrorKind::Unsupported);
        storage.save_project_tasks(project_id, &[]).unwrap();
        assert!(storage.tasks_path(project_id).is_file());
        assert!(legacy.is_file(), "the only other copy stays while the new entry is unproven");
    }

    #[test]
    fn a_failed_reconfirmation_keeps_this_processs_barrier_closed() {
        use slashit_durable_fs::{fault, Stage};
        let (storage, _dir) = create_test_storage();
        storage.save_config(&AppConfig::default()).unwrap();
        storage.confirm_config_durable().unwrap();
        {
            let _fault = fault::fail_at(Stage::Replace, std::io::ErrorKind::Other);
            assert!(storage.reconfirm_config_durable().is_err());
        }
        // Not published, yet the barrier is closed until a write completes.
        let _trace = fault::trace();
        storage.confirm_config_durable().expect("the retry completes");
        assert!(_trace.stages().contains(&Stage::Replace), "the barrier rewrote the config");
    }

    #[test]
    fn a_config_that_fails_before_publication_changes_nothing() {
        use slashit_durable_fs::{fault, Stage};
        let (storage, _dir) = create_test_storage();
        storage.save_config(&AppConfig::default()).unwrap();
        let before = fs::read(&storage.config_file).unwrap();
        let _fault = fault::fail_at(Stage::Replace, std::io::ErrorKind::Other);
        let error = storage
            .update_config(|config| config.ui_preferences.theme = "light".to_string())
            .unwrap_err();
        assert!(!error.is_published(), "{error:#}");
        assert_eq!(fs::read(&storage.config_file).unwrap(), before);
    }

    #[test]
    fn a_conversation_is_synced_before_it_is_replaced_and_failures_are_typed() {
        use slashit_durable_fs::{fault, Stage};
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let conversation = Conversation::new(project_id);

        let trace = fault::trace();
        storage.save_conversation(&conversation).unwrap();
        let stages = trace.stages();
        let sync_temp = stages.iter().position(|s| *s == Stage::SyncTemp).unwrap();
        let replace = stages.iter().position(|s| *s == Stage::Replace).unwrap();
        let sync_dir = stages.iter().position(|s| *s == Stage::SyncDirectory).unwrap();
        assert!(sync_temp < replace && replace < sync_dir, "{stages:?}");
        drop(trace);

        let _fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
        let error = storage.save_conversation(&conversation).unwrap_err();
        assert!(write_error(&error).is_published(), "{error:#}");
    }

    #[test]
    fn a_primary_conversation_pointer_is_not_written_when_its_document_is_not_durable() {
        use slashit_durable_fs::{fault, Stage};
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let _fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Other);
        assert!(storage.create_primary_conversation(project_id).is_err());
        assert!(
            !storage.paths.primary_conversation_file(project_id).exists(),
            "the pointer must never name a document that is not confirmed durable"
        );
    }

    #[test]
    fn a_board_repaired_while_running_is_not_overwritten_by_the_memory_built_without_it() {
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let valid = valid_board_text(project_id);
        let path = storage.tasks_path(project_id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"tasks = [ not toml").unwrap();

        // Startup: the board is skipped, so memory holds nothing for it.
        assert!(storage.load_all_tasks().unwrap().is_empty());

        // The owner repairs the file while the app is running.
        fs::write(&path, &valid).unwrap();
        let error = storage
            .save_project_tasks(project_id, &[crate::test_helpers::create_test_task("New")])
            .expect_err("memory never saw the repaired board");
        assert!(error.downcast_ref::<UnreadableTasksFile>().is_some());
        assert_eq!(fs::read_to_string(&path).unwrap(), valid);

        // A restart loads it, after which saving is ordinary again.
        let restarted = Storage::with_paths(storage.paths().clone());
        let tasks = restarted.load_all_tasks().unwrap();
        assert_eq!(tasks.len(), 1);
        restarted.save_project_tasks(project_id, &tasks).unwrap();
    }

    #[test]
    fn deleting_a_project_still_removes_its_board_file_even_when_unreadable() {
        // Deliberate: deleting a project is an explicit instruction to discard
        // its tasks, and the orphan scan refuses to run while any unreadable
        // board exists. Recorded here so a change to it is a decision.
        let (storage, _dir) = create_test_storage();
        let project_id = Uuid::new_v4();
        let path = storage.tasks_path(project_id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"tasks = [ not toml").unwrap();
        storage.delete_task_files(&storage.tasks_board_files(project_id)).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn a_legacy_board_skipped_at_startup_still_guards_after_the_project_is_routed() {
        let (storage, temp) = create_test_storage();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let project_id = Uuid::new_v4();

        // Unrouted at startup: the legacy file is the board, and it is bad. It
        // holds a Pending integration record under a status this build does
        // not know.
        let legacy = storage.legacy_tasks_path(project_id);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let legacy_bytes = valid_board_text(project_id)
            .replacen("status = \"backlog\"", "status = \"from_the_future\"", 1)
            .into_bytes();
        fs::write(&legacy, &legacy_bytes).unwrap();
        assert!(storage.load_all_tasks().unwrap().is_empty());

        // The project is routed later and a readable routed board appears.
        route_project_to(&storage, project_id, &repo);
        let routed = storage.tasks_path(project_id);
        assert_ne!(routed, legacy);
        fs::create_dir_all(routed.parent().unwrap()).unwrap();
        let routed_text = valid_board_text(project_id);
        fs::write(&routed, &routed_text).unwrap();

        assert!(storage.save_project_tasks(project_id, &[]).is_err());
        assert_eq!(fs::read_to_string(&routed).unwrap(), routed_text);
        assert_eq!(fs::read(&legacy).unwrap(), legacy_bytes);
        assert!(String::from_utf8(legacy_bytes).unwrap().contains("[tasks.local_integration]"));
        // Neither a second attempt nor a restart-less retry changes anything.
        assert!(storage.save_project_tasks(project_id, &[]).is_err());
        assert_eq!(fs::read_to_string(&routed).unwrap(), routed_text);
    }

    #[test]
    fn a_board_skipped_at_startup_stays_guarded_when_the_route_moves_it() {
        let (storage, temp) = create_test_storage();
        let repo_a = temp.path().join("repo-a");
        let repo_b = temp.path().join("repo-b");
        fs::create_dir_all(&repo_a).unwrap();
        fs::create_dir_all(&repo_b).unwrap();
        let project_id = Uuid::new_v4();

        // Routed at startup, board unreadable.
        route_project_to(&storage, project_id, &repo_a);
        let first = storage.tasks_path(project_id);
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::write(&first, b"tasks = [ not toml").unwrap();
        assert!(storage.load_all_tasks().unwrap().is_empty());

        // The project is re-pointed; the bytes land at the new route, are
        // repaired there by hand, and a save must still not replace them.
        route_project_to(&storage, project_id, &repo_b);
        let second = storage.tasks_path(project_id);
        assert_ne!(first, second);
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        let repaired = valid_board_text(project_id);
        fs::write(&second, &repaired).unwrap();

        assert!(storage.save_project_tasks(project_id, &[]).is_err());
        assert_eq!(fs::read_to_string(&second).unwrap(), repaired);
        assert_eq!(fs::read(&first).unwrap(), b"tasks = [ not toml");
        // Other projects are unaffected.
        storage.save_project_tasks(Uuid::new_v4(), &[]).unwrap();
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
        let poisoned = valid.replace("placement = \"managed\"", "placement = \"shared_root\"");
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

    /// `placement = "auto"`, which every configuration written before
    /// SlashIt stopped delegating to Worktrunk carries, and `"managed"` both
    /// still pass the strict read, so an update keeps every other section.
    /// Neither chooses anything any more; a new configuration is written as
    /// `"managed"`.
    #[test]
    fn both_worktree_placement_spellings_still_pass_the_strict_read() {
        use crate::config::paths::WorktreePlacement;
        let mut seed = AppConfig::default();
        seed.jj_config.user_name = Some("Kept".to_string());
        let written = toml::to_string_pretty(&seed).expect("serialize");
        assert!(written.contains("placement = \"managed\""), "{written}");

        for (spelling, expected) in
            [("auto", WorktreePlacement::Auto), ("managed", WorktreePlacement::Managed)]
        {
            let (storage, _temp) = create_test_storage();
            let on_disk = written
                .replace("placement = \"managed\"", &format!("placement = \"{spelling}\""));
            fs::write(&storage.config_file, &on_disk).expect("write fixture");

            let read = storage.read_config_strict().expect(spelling);
            assert_eq!(read.worktree.placement, expected, "{spelling}");
            storage
                .update_config(|config| config.ui_preferences.theme = "light".to_string())
                .expect(spelling);
            let after = storage.read_config_strict().expect(spelling);
            assert_eq!(after.jj_config.user_name.as_deref(), Some("Kept"), "{spelling}");
            assert_eq!(after.worktree.placement, expected, "{spelling}: the spelling is kept");
        }
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
            base: None,
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
            base: None,
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
        storage.delete_task_files(&storage.tasks_board_files(project_id)).expect("Should delete");
        
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
                base: None,
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
            base: None,
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

        storage.delete_task_files(&storage.tasks_board_files(project_b)).unwrap();
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

    /// The cleanup record is one path per line read back as text, so a line
    /// break inside a path would silently corrupt it. Fail closed.
    #[test]
    fn a_board_path_with_a_line_break_is_refused_rather_than_encoded_lossily() {
        let (storage, _temp) = create_test_storage();
        let project_id = Uuid::new_v4();
        let broken = storage.legacy_tasks_path(project_id).with_file_name("line\nbreak.toml");

        let error = storage
            .mark_project_tasks_deletion_pending(project_id, std::slice::from_ref(&broken))
            .expect_err("a line break cannot survive the record format");
        assert!(
            error.to_string().contains("line break") || error.to_string().contains("one path per line"),
            "{error:?}"
        );
        assert!(
            !storage
                .paths()
                .pending_project_tasks_deletion_file(project_id)
                .exists(),
            "a refused record must not have been written"
        );
        assert!(!pending_list_contains(&storage, project_id));
    }

    /// A non-UTF-8 path byte cannot round-trip through the text record at
    /// all, so the record is not written and the caller refuses.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_board_path_is_refused_rather_than_encoded_lossily() {
        use std::os::unix::ffi::OsStringExt;

        let (storage, _temp) = create_test_storage();
        let project_id = Uuid::new_v4();
        let lossy_to_be = PathBuf::from(std::ffi::OsString::from_vec(
            b"state/\xff/repo".to_vec(),
        ));

        assert!(lossy_to_be.to_str().is_none(), "fixture must be non-UTF-8");
        let error = storage
            .mark_project_tasks_deletion_pending(
                project_id,
                std::slice::from_ref(&lossy_to_be),
            )
            .expect_err("a non-UTF-8 path cannot survive the record format");
        assert!(
            error.to_string().contains("UTF-8") || error.to_string().contains("faithfully"),
            "{error:?}"
        );
        assert!(
            !storage
                .paths()
                .pending_project_tasks_deletion_file(project_id)
                .exists()
        );
        assert!(!pending_list_contains(&storage, project_id));
    }

    /// `true` when the Project shows up in `pending_project_tasks_deletions`.
    fn pending_list_contains(storage: &Storage, project_id: Uuid) -> bool {
        storage
            .pending_project_tasks_deletions()
            .unwrap()
            .iter()
            .any(|record| record.project_id == project_id)
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

    // The temporary-file protocol (unique names, owner-only creation,
    // concurrent writers, fsync ordering) is tested in `slashit-durable-fs`.

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
            ExternalRef::GithubPr { number, repo, url, state, .. } => {
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
