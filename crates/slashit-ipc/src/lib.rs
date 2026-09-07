use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Request sent from CLI to the running SlashIt app via Unix socket.
/// Protocol: JSON-lines (one JSON object per line, newline-terminated).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum IpcRequest {
    /// Get overall app status (terminals, agents, queue)
    Status,
    /// List all projects
    ListProjects,
    /// List tasks, optionally filtered by project
    ListTasks {
        project_id: Option<String>,
    },
    /// Create a new task
    CreateTask {
        project_id: String,
        title: String,
        description: Option<String>,
        priority: Option<String>,
    },
    /// Move a task to a different status
    MoveTask {
        task_id: String,
        status: String,
    },
    /// Edit task properties
    EditTask {
        task_id: String,
        title: Option<String>,
        description: Option<String>,
        priority: Option<String>,
    },
    /// Delete a task
    DeleteTask {
        task_id: String,
    },
    /// Get queue status
    QueueStatus,
    /// Add a task to the queue
    EnqueueTask {
        task_id: String,
    },
    /// List active PTY terminal sessions
    ListTerminals,
    /// Bring the app window to front
    Show,
    /// Request graceful quit
    Quit,
}

/// Response from the SlashIt app to the CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    pub data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl IpcResponse {
    pub fn success(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            data,
            error: None,
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: serde_json::Value::Null,
            error: Some(msg.into()),
        }
    }
}

/// Lightweight task summary for CLI display (no internal fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub project_id: String,
    pub project_name: String,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub phase: String,
    pub overall_progress: u8,
    pub created_at: String,
}

/// Lightweight project summary for CLI display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub path: Option<String>,
}

/// App-wide status snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppStatus {
    pub active_terminals: usize,
    pub running_agents: usize,
    pub queued_tasks: usize,
    pub in_progress_tasks: usize,
}

/// Terminal session summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSummary {
    pub id: String,
    pub name: String,
    pub cols: usize,
    pub rows: usize,
}

/// Queue status snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueStatusInfo {
    pub queued_count: usize,
    pub in_progress_count: usize,
    pub parallel_limit: u32,
    pub auto_promote: bool,
    pub fifo_ordering: bool,
}

/// Largest accepted request, in bytes.
///
/// The server reads one newline-delimited JSON object per connection. Without
/// a bound, a client that never sends a newline makes the server buffer until
/// it runs out of memory, so the read is capped and an oversized request is
/// rejected rather than absorbed.
pub const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

/// Directory holding the runtime socket.
///
/// `$XDG_RUNTIME_DIR` is already per-user and mode 0700. When it is unset the
/// fallback is `<tmp>/slashit-<uid>` rather than a fixed name directly in the
/// temp directory: `/tmp` is world-writable and shared between users, so a
/// predictable unqualified path there can be pre-created or squatted by
/// another local user. The caller is responsible for creating this directory
/// with owner-only permissions before binding — see `ipc::server::run`.
pub fn socket_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("slashit-app");
        }
    }
    std::env::temp_dir().join(format!("slashit-{}", current_uid()))
}

/// Returns the well-known Unix socket path used by both server and CLI.
pub fn socket_path() -> PathBuf {
    socket_dir().join("slashit.sock")
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // Safety: getuid is always successful and has no preconditions.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}
