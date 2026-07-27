//! Wire protocol: the command set, the envelope that carries it, and the
//! response shape.
//!
//! The protocol is deliberately separate from the transport that carries it
//! (see [`crate::transport`]) and from the framing that delimits it (see
//! [`crate::framing`]). A command means the same thing over a Unix socket, a
//! Windows named pipe, or a loopback TCP connection; only the access control
//! around it differs.

use serde::{Deserialize, Serialize};

/// Version of the request envelope understood by this build.
///
/// Bump this whenever the meaning of an existing field changes. Adding a new
/// optional field, or a new [`IpcRequest`] variant, does not require a bump:
/// an older server answers an unknown command with a clear error rather than
/// a decode failure.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest accepted request, in bytes.
///
/// The server reads one newline-delimited JSON object per connection. Without
/// a bound, a client that never sends a newline makes the server buffer until
/// it runs out of memory, so the read is capped and an oversized request is
/// rejected rather than absorbed.
pub const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

/// Request sent from a client to the running SlashIt instance.
///
/// Protocol: JSON-lines, one [`IpcEnvelope`] per line, newline-terminated.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum IpcRequest {
    /// Get overall app status (terminals, agents, queue)
    Status,
    /// List all projects
    ListProjects,
    /// List tasks, optionally filtered by project
    ListTasks { project_id: Option<String> },
    /// Create a new task
    CreateTask {
        project_id: String,
        title: String,
        description: Option<String>,
        priority: Option<String>,
    },
    /// Move a task to a different status
    MoveTask { task_id: String, status: String },
    /// Edit task properties
    EditTask {
        task_id: String,
        title: Option<String>,
        description: Option<String>,
        priority: Option<String>,
    },
    /// Delete a task
    DeleteTask { task_id: String },
    /// Get queue status
    QueueStatus,
    /// Add a task to the queue
    EnqueueTask { task_id: String },
    /// List active PTY terminal sessions
    ListTerminals,
    /// Bring the app window to front. A no-op with a clear message when the
    /// instance is a headless daemon.
    Show,
    /// Request graceful quit
    Quit,
    /// Report the resolved value and origin of every runtime feature flag.
    ///
    /// The daemon and the GUI resolve flags through the same code, so asking
    /// the running instance is the only way to see what is actually in force
    /// rather than what a config file suggests.
    Features,
    /// Report what kind of instance is answering, and on which endpoint.
    Ping,
}

impl IpcRequest {
    /// Whether this command can change state.
    ///
    /// Used for authorization and for the audit log. `CreateTask`,
    /// `MoveTask` and `EnqueueTask` are the ones that ultimately reach the
    /// queue executor, which spawns an agent — see
    /// `docs/architecture/ipc-security.md`.
    pub fn is_mutating(&self) -> bool {
        !matches!(
            self,
            Self::Status
                | Self::ListProjects
                | Self::ListTasks { .. }
                | Self::QueueStatus
                | Self::ListTerminals
                | Self::Features
                | Self::Ping
        )
    }

    /// Whether this command can cause code to run in a checkout.
    ///
    /// Moving a task into `in_progress` reaches the queue executor, which
    /// creates a worktree and spawns the configured agent with the task
    /// description as its prompt. These verbs are the code-execution surface
    /// and are denied to any non-local peer.
    pub fn spawns_agent(&self) -> bool {
        matches!(
            self,
            Self::CreateTask { .. } | Self::MoveTask { .. } | Self::EnqueueTask { .. }
        )
    }

    /// Stable name for logs and audit records.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::ListProjects => "list_projects",
            Self::ListTasks { .. } => "list_tasks",
            Self::CreateTask { .. } => "create_task",
            Self::MoveTask { .. } => "move_task",
            Self::EditTask { .. } => "edit_task",
            Self::DeleteTask { .. } => "delete_task",
            Self::QueueStatus => "queue_status",
            Self::EnqueueTask { .. } => "enqueue_task",
            Self::ListTerminals => "list_terminals",
            Self::Show => "show",
            Self::Quit => "quit",
            Self::Features => "features",
            Self::Ping => "ping",
        }
    }
}

/// What a client actually puts on the wire.
///
/// The envelope exists so the protocol can be versioned and so a transport
/// that is not self-authenticating (TCP) can carry a bearer token. Unix
/// sockets and named pipes are authenticated by the operating system and
/// leave `auth` empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcEnvelope {
    /// Protocol version the client speaks. Defaulted for forward tolerance so
    /// a missing field is a version mismatch with a clear message rather than
    /// a decode error.
    #[serde(default)]
    pub version: u32,

    /// Bearer token, required only on transports the OS does not access-control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,

    /// The command itself.
    pub request: IpcRequest,
}

impl IpcEnvelope {
    /// Wrap a request at the current protocol version with no credential.
    pub fn new(request: IpcRequest) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            auth: None,
            request,
        }
    }

    /// Wrap a request carrying a bearer token.
    pub fn with_auth(request: IpcRequest, token: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            auth: Some(token.into()),
            request,
        }
    }
}

/// Response from the SlashIt instance to a client.
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

/// One feature flag as the running instance resolved it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureFlagInfo {
    pub name: String,
    pub value: bool,
    pub default: bool,
    /// Which layer decided the value: `default`, `config`, `env` or `cli`.
    pub source: String,
    pub description: String,
}

/// What kind of instance answered, for `slashit ping`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceInfo {
    /// `gui` or `daemon`.
    pub mode: String,
    pub version: String,
    pub protocol_version: u32,
    pub pid: u32,
    /// Human-readable endpoint the answer came in on.
    pub endpoint: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrips() {
        let env = IpcEnvelope::new(IpcRequest::Status);
        let line = serde_json::to_string(&env).unwrap();
        let back: IpcEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back.version, PROTOCOL_VERSION);
        assert!(back.auth.is_none());
        assert!(matches!(back.request, IpcRequest::Status));
    }

    #[test]
    fn envelope_without_auth_omits_the_field() {
        let line = serde_json::to_string(&IpcEnvelope::new(IpcRequest::Status)).unwrap();
        assert!(
            !line.contains("auth"),
            "an unauthenticated envelope should not carry a null auth field: {line}"
        );
    }

    #[test]
    fn missing_version_defaults_to_zero_so_it_reads_as_a_mismatch() {
        // A pre-envelope client sends a bare request. Deserialising must not
        // silently succeed at the current version.
        let back: IpcEnvelope = serde_json::from_str(r#"{"request":{"cmd":"status"}}"#).unwrap();
        assert_eq!(back.version, 0);
        assert_ne!(back.version, PROTOCOL_VERSION);
    }

    #[test]
    fn legacy_bare_request_does_not_parse_as_an_envelope() {
        // The old wire format was a bare IpcRequest. It must fail rather than
        // be misread, so the server can answer with a version message.
        assert!(serde_json::from_str::<IpcEnvelope>(r#"{"cmd":"status"}"#).is_err());
    }

    #[test]
    fn agent_spawning_verbs_are_classified() {
        assert!(IpcRequest::CreateTask {
            project_id: String::new(),
            title: String::new(),
            description: None,
            priority: None,
        }
        .spawns_agent());
        assert!(IpcRequest::MoveTask {
            task_id: String::new(),
            status: "in_progress".into(),
        }
        .spawns_agent());
        assert!(IpcRequest::EnqueueTask {
            task_id: String::new(),
        }
        .spawns_agent());

        assert!(!IpcRequest::Status.spawns_agent());
        assert!(!IpcRequest::ListProjects.spawns_agent());
    }

    #[test]
    fn every_spawning_verb_is_also_mutating() {
        // A verb that spawns an agent but is classified read-only would slip
        // past any authorization rule written in terms of mutation.
        let all = [
            IpcRequest::Status,
            IpcRequest::ListProjects,
            IpcRequest::ListTasks { project_id: None },
            IpcRequest::CreateTask {
                project_id: String::new(),
                title: String::new(),
                description: None,
                priority: None,
            },
            IpcRequest::MoveTask {
                task_id: String::new(),
                status: String::new(),
            },
            IpcRequest::EditTask {
                task_id: String::new(),
                title: None,
                description: None,
                priority: None,
            },
            IpcRequest::DeleteTask {
                task_id: String::new(),
            },
            IpcRequest::QueueStatus,
            IpcRequest::EnqueueTask {
                task_id: String::new(),
            },
            IpcRequest::ListTerminals,
            IpcRequest::Show,
            IpcRequest::Quit,
            IpcRequest::Features,
            IpcRequest::Ping,
        ];
        for req in &all {
            if req.spawns_agent() {
                assert!(
                    req.is_mutating(),
                    "{} spawns but is not mutating",
                    req.verb()
                );
            }
        }
    }

    #[test]
    fn verbs_are_unique() {
        let all = [
            IpcRequest::Status.verb(),
            IpcRequest::ListProjects.verb(),
            IpcRequest::ListTasks { project_id: None }.verb(),
            IpcRequest::QueueStatus.verb(),
            IpcRequest::ListTerminals.verb(),
            IpcRequest::Show.verb(),
            IpcRequest::Quit.verb(),
            IpcRequest::Features.verb(),
            IpcRequest::Ping.verb(),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for v in all {
            assert!(seen.insert(v), "duplicate verb {v}");
        }
    }
}
