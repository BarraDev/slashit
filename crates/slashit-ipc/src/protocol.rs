// SPDX-License-Identifier: MPL-2.0
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
    /// Read one task as its full stored record. Never writes.
    GetTask { task_id: String },
    /// Read one Task's recent execution evidence. Never writes.
    ///
    /// The answer is built from evidence that already exists: the Task's
    /// persisted Activity and its Task record (run outcomes, review outcomes,
    /// the configured model and the Task's latest observed model), plus what
    /// the running instance still holds in memory for the Task's latest run —
    /// the same data the desktop's `get_task_run` and `get_task_output`
    /// commands serve the UI. The in-memory part exists only for this
    /// instance's lifetime and is gone after a restart; the answer says so.
    /// The observed model is a Task-level identity, not attributed to any
    /// single run — per-run model history is not persisted — and it is never
    /// inferred from the configured alias. The number of runs described and
    /// the bytes of the answer are bounded by the instance.
    GetTaskEvidence { task_id: String },
    /// Read one Task's review: decisions, whether the approval still covers the
    /// branch, and agent requests. Never writes.
    GetReviewState { task_id: String },
    /// Read one Task's delivery: its pull request, failures and agent requests
    /// to open one. Never writes.
    GetDeliveryState { task_id: String },
    /// File a request for a person to decide in the desktop app. `operation` is
    /// `approve` or `open_pr`, `commit` the full object ID it is about, and
    /// `actor` the name the agent gives itself. Approves, authorizes and
    /// publishes nothing. There is deliberately no verb that decides one.
    RequestOperation {
        task_id: String,
        operation: String,
        commit: String,
        actor: String,
    },
    /// Open the pull request a person authorized with request `request_id`.
    /// The authorization names the commit and is used at most once.
    OpenPr {
        task_id: String,
        request_id: String,
        actor: String,
    },
    /// Create a new task
    CreateTask {
        project_id: String,
        title: String,
        description: Option<String>,
        priority: Option<String>,
        /// `manual` or `automatic`; absent keeps the default, automatic.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        review_mode: Option<String>,
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
    /// Set the coding model a Task's runs are started on.
    ///
    /// The identifier is saved exactly as it is given: nothing is substituted,
    /// so the model a run is started on is the model that was requested. The
    /// instance refuses an identifier no model picker would offer (or, for an
    /// OpenCode Task, one that is not a `provider/model` id).
    SetTaskModel { task_id: String, model: String },
    /// Set how a Task's finished coding runs are reviewed: `manual` or
    /// `automatic`. A change affects only runs that have not started.
    SetTaskReviewMode {
        task_id: String,
        review_mode: String,
    },
    /// Replace a Task's structured dependencies. An empty list clears them.
    ///
    /// Validated by the instance with the same rules the desktop applies: at
    /// most one Task of the same Project, no self-dependency, no cycle, and
    /// only while the Task holds no checkout and nothing is unfinished.
    SetTaskDependencies {
        task_id: String,
        dependencies: Vec<String>,
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
    /// Close executor admission for `operation` and report what is still in
    /// flight, in one step. After an acquired reply no start, Task, Project
    /// Run or Queue promotion is admitted until `Resume` with the same
    /// operation. Repeating the request with the same operation is idempotent,
    /// so a lost reply can be retried. A refusal is a reply with
    /// `acquired: false`, never an error, and it never reveals the holder.
    Quiesce { operation: String },
    /// Reopen admission closed by `Quiesce` for the same `operation`. Any other
    /// operation, and any shutdown fence, is refused as a reply, not reopened.
    Resume { operation: String },
    /// Report the resolved value and origin of every runtime feature flag.
    ///
    /// The daemon and the GUI resolve flags through the same code, so asking
    /// the running instance is the only way to see what is actually in force
    /// rather than what a config file suggests.
    Features,
    /// Report what kind of instance is answering, and on which endpoint.
    Ping,
    /// Write one Project's portable export to a new file. `path` must be
    /// absolute. Refused while a Task of the Project is queued, running or in
    /// automatic AI Review, or has an unsettled lifecycle record.
    ExportProject { project_id: String, path: String },
    /// Validate an export file and describe what importing it would do. Reads
    /// the file and the instance's memory, and writes nothing.
    PreviewImport { path: String },
    /// Create the Project described by an export file, as an inert Project.
    /// `checksum` is the payload checksum a preview reported for this file; the
    /// apply refuses any other payload. Writes the Project and its Tasks.
    ApplyImport { path: String, checksum: String },
}

impl IpcRequest {
    /// Whether this command can change state.
    ///
    /// Used for authorization and for the audit log. `CreateTask`,
    /// `MoveTask`, `EnqueueTask` and `EditTask` are the ones that ultimately
    /// reach the queue executor, which spawns an agent — see
    /// `docs/architecture/ipc-security.md`.
    pub fn is_mutating(&self) -> bool {
        !matches!(
            self,
            Self::Status
                | Self::ListProjects
                | Self::ListTasks { .. }
                | Self::GetTask { .. }
                | Self::GetTaskEvidence { .. }
                | Self::GetReviewState { .. }
                | Self::GetDeliveryState { .. }
                | Self::QueueStatus
                | Self::ListTerminals
                | Self::Features
                | Self::Ping
                | Self::PreviewImport { .. }
        )
    }

    /// Whether this command can cause code to run in a checkout.
    ///
    /// Moving a task into `in_progress` reaches the queue executor, which
    /// creates a worktree and spawns the configured agent with the task's
    /// live title and description as its prompt — read at execution time, not
    /// snapshotted at `CreateTask`/`MoveTask`/`EnqueueTask` time. `EditTask`
    /// can rewrite that title and description on a task already queued or
    /// in progress, so it reaches the same primitive one step later and must
    /// be denied to any non-local peer exactly like the verbs that start it.
    /// These verbs are the code-execution surface.
    /// Verbs that create work a person can run (the agent verbs above, and an
    /// import, whose Tasks can be queued the same way), a preview, which reads
    /// a file at a path the client chooses, and an export, which writes a new
    /// file at a path the client chooses, a request that puts a decision in
    /// front of a person, and opening a pull request, which publishes. Denied
    /// to any peer the operating system has not identified.
    pub fn needs_verified_peer(&self) -> bool {
        self.spawns_agent()
            || matches!(
                self,
                Self::ApplyImport { .. }
                    | Self::PreviewImport { .. }
                    | Self::ExportProject { .. }
                    | Self::Quiesce { .. }
                    | Self::Resume { .. }
                    | Self::RequestOperation { .. }
                    | Self::OpenPr { .. }
            )
    }

    pub fn spawns_agent(&self) -> bool {
        matches!(
            self,
            Self::CreateTask { .. }
                | Self::MoveTask { .. }
                | Self::EnqueueTask { .. }
                | Self::EditTask { .. }
        )
    }

    /// Stable name for logs and audit records.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::ListProjects => "list_projects",
            Self::ListTasks { .. } => "list_tasks",
            Self::GetTask { .. } => "get_task",
            Self::GetTaskEvidence { .. } => "get_task_evidence",
            Self::GetReviewState { .. } => "get_review_state",
            Self::GetDeliveryState { .. } => "get_delivery_state",
            Self::RequestOperation { .. } => "request_operation",
            Self::OpenPr { .. } => "open_pr",
            Self::CreateTask { .. } => "create_task",
            Self::MoveTask { .. } => "move_task",
            Self::EditTask { .. } => "edit_task",
            Self::SetTaskModel { .. } => "set_task_model",
            Self::SetTaskReviewMode { .. } => "set_task_review_mode",
            Self::SetTaskDependencies { .. } => "set_task_dependencies",
            Self::DeleteTask { .. } => "delete_task",
            Self::QueueStatus => "queue_status",
            Self::EnqueueTask { .. } => "enqueue_task",
            Self::ListTerminals => "list_terminals",
            Self::Show => "show",
            Self::Quit => "quit",
            Self::Quiesce { .. } => "quiesce",
            Self::Resume { .. } => "resume",
            Self::Features => "features",
            Self::Ping => "ping",
            Self::ExportProject { .. } => "export_project",
            Self::PreviewImport { .. } => "preview_import",
            Self::ApplyImport { .. } => "apply_import",
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
    /// Whether this instance supports `Quiesce` and `Resume`. An instance
    /// built before the field existed omits it, which reads as unsupported.
    #[serde(default)]
    pub admission_control: bool,
}

/// Whether `id` is a well-formed quiesce operation id: 1 to 64 ASCII letters,
/// digits or hyphens. Checked at the IPC boundary before any state is touched.
pub fn is_valid_operation_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Reply to an acquired `Quiesce`: admission is closed for `operation` and these
/// are the counts a restart would interrupt. `admitting` counts starts still
/// inside their admission sequence; `owned_work` counts running Task owners and
/// Project Runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuiesceInfo {
    pub acquired: bool,
    pub operation: String,
    pub admitting: usize,
    pub owned_work: usize,
    #[serde(flatten)]
    pub status: AppStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_built_before_admission_quiesce_reads_as_unsupported() {
        let old =
            r#"{"mode":"gui","version":"1.0.0","protocol_version":1,"pid":7,"endpoint":"unix:/x"}"#;
        let info: InstanceInfo = serde_json::from_str(old).unwrap();
        assert!(
            !info.admission_control,
            "an answer without the field must fail closed"
        );
        let new = r#"{"mode":"gui","version":"1.0.0","protocol_version":1,"pid":7,"endpoint":"unix:/x","admission_control":true}"#;
        assert!(
            serde_json::from_str::<InstanceInfo>(new)
                .unwrap()
                .admission_control
        );
    }

    #[test]
    fn operation_ids_are_bounded_and_plain() {
        assert!(is_valid_operation_id("sup-42-0f8c3b1e-aaaa"));
        assert!(!is_valid_operation_id(""));
        assert!(!is_valid_operation_id(&"a".repeat(65)));
        assert!(!is_valid_operation_id("a b"));
        assert!(!is_valid_operation_id("a/../b"));
    }

    #[test]
    fn quiesce_and_resume_are_mutating_requests() {
        let quiesce = IpcRequest::Quiesce {
            operation: "op".into(),
        };
        let resume = IpcRequest::Resume {
            operation: "op".into(),
        };
        assert!(quiesce.is_mutating());
        assert!(resume.is_mutating());
    }

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
            review_mode: None,
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
        assert!(IpcRequest::EditTask {
            task_id: String::new(),
            title: None,
            description: None,
            priority: None,
        }
        .spawns_agent());

        assert!(!IpcRequest::Status.spawns_agent());
        assert!(!IpcRequest::ListProjects.spawns_agent());
        assert!(!IpcRequest::DeleteTask {
            task_id: String::new(),
        }
        .spawns_agent());
        assert!(!IpcRequest::Show.spawns_agent());
        assert!(!IpcRequest::Quit.spawns_agent());
    }

    #[test]
    fn an_import_creates_work_so_it_needs_a_verified_peer() {
        assert!(IpcRequest::ApplyImport {
            path: String::new(),
            checksum: String::new(),
        }
        .needs_verified_peer());
        // A preview reads a caller-chosen path, so it is not a read-only status verb.
        assert!(IpcRequest::PreviewImport {
            path: String::new(),
        }
        .needs_verified_peer());
        // An export writes a file at a path the client names.
        assert!(IpcRequest::ExportProject {
            project_id: String::new(),
            path: String::new(),
        }
        .needs_verified_peer());
        assert!(!IpcRequest::Status.needs_verified_peer());
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
                review_mode: None,
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

    /// A read that serves one Task's execution evidence is a status verb: it
    /// changes nothing, spawns nothing and needs no verified peer.
    #[test]
    fn task_evidence_is_a_read_only_status_verb() {
        let req = IpcRequest::GetTaskEvidence {
            task_id: String::new(),
        };
        assert!(!req.is_mutating(), "{} changes nothing", req.verb());
        assert!(!req.spawns_agent());
        assert!(!req.needs_verified_peer());
    }

    #[test]
    fn review_and_delivery_verbs_are_classified() {
        let task_id = String::new();
        for read in [
            IpcRequest::GetReviewState { task_id: task_id.clone() },
            IpcRequest::GetDeliveryState { task_id: task_id.clone() },
        ] {
            assert!(!read.is_mutating(), "{} changes nothing", read.verb());
            assert!(!read.needs_verified_peer());
        }
        for write in [
            IpcRequest::RequestOperation {
                task_id: task_id.clone(),
                operation: "open_pr".into(),
                commit: String::new(),
                actor: String::new(),
            },
            IpcRequest::OpenPr {
                task_id: task_id.clone(),
                request_id: String::new(),
                actor: String::new(),
            },
        ] {
            assert!(write.is_mutating(), "{}", write.verb());
            assert!(write.needs_verified_peer(), "{}", write.verb());
        }
        // No verb records a decision.
        for cmd in ["approve_task", "approve", "decide_agent_request", "authorize_request"] {
            let line = format!(r#"{{"cmd":"{cmd}","task_id":"t"}}"#);
            assert!(serde_json::from_str::<IpcRequest>(&line).is_err(), "{cmd}");
        }
    }

    #[test]
    fn a_task_evidence_request_round_trips_on_the_wire() {
        let env = IpcEnvelope::new(IpcRequest::GetTaskEvidence {
            task_id: "6f96fefa-0000-0000-0000-000000000003".into(),
        });
        let line = serde_json::to_string(&env).unwrap();
        let back: IpcEnvelope = serde_json::from_str(&line).unwrap();
        match back.request {
            IpcRequest::GetTaskEvidence { task_id } => {
                assert!(task_id.ends_with("0003"));
            }
            other => panic!("decoded as {other:?}"),
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

    /// Task configuration writes carry no agent-launching reach — they only
    /// change what a *later* run is started with — but they do change state, so
    /// both classifications are pinned here, as
    /// `every_spawning_verb_is_also_mutating` pins the spawning verbs'.
    #[test]
    fn task_configuration_writes_are_mutating_and_spawn_nothing() {
        let all = [
            IpcRequest::SetTaskModel {
                task_id: String::new(),
                model: String::new(),
            },
            IpcRequest::SetTaskReviewMode {
                task_id: String::new(),
                review_mode: String::new(),
            },
            IpcRequest::SetTaskDependencies {
                task_id: String::new(),
                dependencies: Vec::new(),
            },
        ];
        let mut seen = std::collections::BTreeSet::new();
        for req in &all {
            assert!(
                req.is_mutating(),
                "{} changes state and must be classified as mutating",
                req.verb()
            );
            assert!(
                !req.spawns_agent(),
                "{} must not be classified as a code-execution verb",
                req.verb()
            );
            assert!(seen.insert(req.verb()), "duplicate verb {}", req.verb());
        }
    }

    #[test]
    fn a_task_configuration_write_round_trips_on_the_wire() {
        let env = IpcEnvelope::new(IpcRequest::SetTaskDependencies {
            task_id: "6f96fefa-0000-0000-0000-000000000001".into(),
            dependencies: vec!["6f96fefa-0000-0000-0000-000000000002".into()],
        });
        let line = serde_json::to_string(&env).unwrap();
        let back: IpcEnvelope = serde_json::from_str(&line).unwrap();
        match back.request {
            IpcRequest::SetTaskDependencies {
                task_id,
                dependencies,
            } => {
                assert!(task_id.ends_with("0001"));
                assert_eq!(dependencies.len(), 1);
            }
            other => panic!("decoded as {other:?}"),
        }
    }
}
