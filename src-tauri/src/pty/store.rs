use crate::config::paths::AppPaths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

const MAX_SCROLLBACK_SIZE: usize = 100 * 1024; // 100KB per session

/// Metadata about a PTY session that persists across app restarts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: Uuid,
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    pub working_directory: Option<String>,
    /// The Project the terminal was opened in. Files written before this field
    /// existed load with `None`.
    #[serde(default)]
    pub project_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_attached_at: chrono::DateTime<chrono::Utc>,
    pub is_alive: bool,
}

/// Scrollback buffer for terminal output
#[derive(Debug, Clone, Default)]
pub struct ScrollbackBuffer {
    data: Vec<u8>,
    max_size: usize,
}

impl ScrollbackBuffer {
    pub fn new(max_size: usize) -> Self {
        Self {
            data: Vec::with_capacity(max_size.min(8192)),
            max_size,
        }
    }

    /// Append data to the scrollback buffer, trimming if needed
    pub fn append(&mut self, new_data: &[u8]) {
        // If adding this data would exceed max, trim from the front
        let total_len = self.data.len() + new_data.len();
        if total_len > self.max_size {
            let excess = total_len - self.max_size;
            if excess >= self.data.len() {
                // New data is larger than buffer, just keep end of new data
                self.data.clear();
                let start = new_data.len().saturating_sub(self.max_size);
                self.data.extend_from_slice(&new_data[start..]);
            } else {
                // Trim from front of existing data
                self.data.drain(0..excess);
                self.data.extend_from_slice(new_data);
            }
        } else {
            self.data.extend_from_slice(new_data);
        }
    }

    /// Get all scrollback data
    pub fn get_data(&self) -> &[u8] {
        &self.data
    }
}

/// Manages scrollback buffers for all sessions in memory
#[derive(Clone)]
pub struct ScrollbackManager {
    buffers: Arc<Mutex<HashMap<Uuid, ScrollbackBuffer>>>,
}

impl ScrollbackManager {
    pub fn new() -> Self {
        Self {
            buffers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a new scrollback buffer for a session
    pub async fn create(&self, session_id: Uuid) {
        let mut buffers = self.buffers.lock().await;
        buffers.insert(session_id, ScrollbackBuffer::new(MAX_SCROLLBACK_SIZE));
    }

    /// Append data to a session's scrollback buffer
    pub async fn append(&self, session_id: Uuid, data: &[u8]) {
        let mut buffers = self.buffers.lock().await;
        if let Some(buffer) = buffers.get_mut(&session_id) {
            buffer.append(data);
        }
    }

    /// Get scrollback data for a session
    pub async fn get(&self, session_id: Uuid) -> Option<Vec<u8>> {
        let buffers = self.buffers.lock().await;
        buffers.get(&session_id).map(|b| b.get_data().to_vec())
    }

    /// Remove a session's scrollback buffer
    pub async fn remove(&self, session_id: Uuid) {
        let mut buffers = self.buffers.lock().await;
        buffers.remove(&session_id);
    }
}

impl Default for ScrollbackManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Persists session metadata to disk
pub struct SessionStore {
    store_path: PathBuf,
    write_lock: std::sync::Mutex<()>,
}

impl SessionStore {
    pub fn new() -> Result<Self> {
        let paths = AppPaths::new().context("Failed to resolve application directories")?;
        Self::with_paths(&paths)
    }

    /// Build against explicit roots. Terminal scrollback is machine-local, so
    /// it always lives under `data_dir` and is never relocatable into a project.
    pub fn with_paths(paths: &AppPaths) -> Result<Self> {
        let data_dir = paths.data_dir();
        fs::create_dir_all(data_dir).context("Failed to create data directory")?;

        Ok(Self {
            store_path: paths.terminal_sessions_file(),
            write_lock: std::sync::Mutex::new(()),
        })
    }

    #[cfg(test)]
    pub(crate) fn at_path(store_path: PathBuf) -> Self {
        Self {
            store_path,
            write_lock: std::sync::Mutex::new(()),
        }
    }

    /// Load all session metadata from disk
    pub fn load_sessions(&self) -> Result<Vec<SessionMetadata>> {
        if !self.store_path.exists() {
            return Ok(Vec::new());
        }

        let contents = fs::read_to_string(&self.store_path)
            .context("Failed to read sessions file")?;

        #[derive(Deserialize)]
        struct SessionsFile {
            sessions: Vec<SessionMetadata>,
        }

        let file: SessionsFile = toml::from_str(&contents)
            .context("Failed to parse sessions file")?;

        Ok(file.sessions)
    }

    /// Save all session metadata to disk
    pub fn save_sessions(&self, sessions: &[SessionMetadata]) -> Result<()> {
        #[derive(Serialize)]
        struct SessionsFile<'a> {
            sessions: &'a [SessionMetadata],
        }

        let file = SessionsFile { sessions };
        let contents = toml::to_string_pretty(&file)
            .context("Failed to serialize sessions")?;

        fs::write(&self.store_path, contents)
            .context("Failed to write sessions file")?;

        Ok(())
    }

    /// Add or update a session
    pub fn upsert_session(&self, metadata: SessionMetadata) -> Result<()> {
        self.update(|sessions| match sessions.iter_mut().find(|s| s.id == metadata.id) {
            Some(existing) => *existing = metadata,
            None => sessions.push(metadata),
        })
    }

    /// Remove a session by ID
    pub fn remove_session(&self, session_id: Uuid) -> Result<()> {
        self.update(|sessions| sessions.retain(|s| s.id != session_id))
    }

    /// Mark all sessions as not alive (on startup)
    pub fn mark_all_dead(&self) -> Result<()> {
        self.update(|sessions| {
            for session in sessions.iter_mut() {
                session.is_alive = false;
            }
        })
    }

    /// Mark a session as alive
    pub fn mark_alive(&self, session_id: Uuid) -> Result<()> {
        self.update(|sessions| {
            if let Some(session) = sessions.iter_mut().find(|s| s.id == session_id) {
                session.is_alive = true;
                session.last_attached_at = chrono::Utc::now();
            }
        })
    }

    /// Read, change and rewrite the file as one step. The lock matters: two
    /// writers that load before either saves would otherwise drop one record.
    fn update(&self, change: impl FnOnce(&mut Vec<SessionMetadata>)) -> Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut sessions = self.load_sessions().unwrap_or_default();
        change(&mut sessions);
        self.save_sessions(&sessions)
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new().expect("Failed to create session store")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(project_id: Option<&str>) -> SessionMetadata {
        SessionMetadata {
            id: Uuid::new_v4(),
            name: "Terminal".into(),
            cols: 80,
            rows: 24,
            working_directory: None,
            project_id: project_id.map(str::to_owned),
            created_at: chrono::Utc::now(),
            last_attached_at: chrono::Utc::now(),
            is_alive: true,
        }
    }

    #[test]
    fn a_record_keeps_its_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::at_path(dir.path().join("terminal-sessions.toml"));
        store.upsert_session(record(Some("project-a"))).expect("write");
        let loaded = store.load_sessions().expect("read");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].project_id.as_deref(), Some("project-a"));
    }

    #[test]
    fn a_file_written_before_the_project_field_loads_without_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("terminal-sessions.toml");
        let old = record(None);
        let contents = format!(
            "[[sessions]]\nid = \"{}\"\nname = \"Terminal\"\ncols = 80\nrows = 24\ncreated_at = \"{}\"\nlast_attached_at = \"{}\"\nis_alive = true\n",
            old.id,
            old.created_at.to_rfc3339(),
            old.last_attached_at.to_rfc3339(),
        );
        fs::write(&path, contents).expect("write");
        let loaded = SessionStore::at_path(path).load_sessions().expect("read");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].project_id, None);
    }

    /// Two writers racing on one file must keep both sets of records. Without
    /// the write lock, a save can overwrite the other writer's load-modify-save.
    #[test]
    fn concurrent_writers_keep_every_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = std::sync::Arc::new(SessionStore::at_path(dir.path().join("terminal-sessions.toml")));
        let writers: Vec<_> = (0..2)
            .map(|_| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        store.upsert_session(record(None)).expect("write");
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer");
        }
        assert_eq!(store.load_sessions().expect("read").len(), 50);
    }

    /// A record that cannot be written must be reported, never swallowed: the
    /// caller decides whether the terminal may still start.
    #[test]
    fn a_record_that_cannot_be_written_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The store path is a directory, so the write must fail.
        let store = SessionStore::at_path(dir.path().to_path_buf());
        assert!(store.upsert_session(record(Some("project-a"))).is_err());
    }
}
