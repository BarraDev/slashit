use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

const APP_NAME: &str = "slashit-app";
const MAX_SCROLLBACK_SIZE: usize = 100 * 1024; // 100KB per session

/// Metadata about a PTY session that persists across app restarts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: Uuid,
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    pub working_directory: Option<String>,
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
}

impl SessionStore {
    pub fn new() -> Result<Self> {
        let proj_dirs = ProjectDirs::from("com", "barradev", APP_NAME)
            .context("Failed to get project directories")?;

        let data_dir = proj_dirs.data_dir();
        fs::create_dir_all(data_dir)
            .context("Failed to create data directory")?;

        let store_path = data_dir.join("terminal_sessions.toml");

        Ok(Self { store_path })
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
        let mut sessions = self.load_sessions().unwrap_or_default();
        
        if let Some(existing) = sessions.iter_mut().find(|s| s.id == metadata.id) {
            *existing = metadata;
        } else {
            sessions.push(metadata);
        }

        self.save_sessions(&sessions)
    }

    /// Remove a session by ID
    pub fn remove_session(&self, session_id: Uuid) -> Result<()> {
        let mut sessions = self.load_sessions().unwrap_or_default();
        sessions.retain(|s| s.id != session_id);
        self.save_sessions(&sessions)
    }

    /// Mark all sessions as not alive (on startup)
    pub fn mark_all_dead(&self) -> Result<()> {
        let mut sessions = self.load_sessions().unwrap_or_default();
        for session in &mut sessions {
            session.is_alive = false;
        }
        self.save_sessions(&sessions)
    }

    /// Mark a session as alive
    pub fn mark_alive(&self, session_id: Uuid) -> Result<()> {
        let mut sessions = self.load_sessions().unwrap_or_default();
        if let Some(session) = sessions.iter_mut().find(|s| s.id == session_id) {
            session.is_alive = true;
            session.last_attached_at = chrono::Utc::now();
        }
        self.save_sessions(&sessions)
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new().expect("Failed to create session store")
    }
}
