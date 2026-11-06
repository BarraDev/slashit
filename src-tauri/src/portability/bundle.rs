// SPDX-License-Identifier: MPL-2.0
//! The on-disk shape of a portable Project export, version 1.
//!
//! A bundle is one JSON object. The checksum covers exactly one thing: the
//! bytes of its `payload` member as the exporter wrote them (SHA-256, lowercase
//! hex). `manifest` must describe the payload and is checked against it.
//! `build` and `created_at` are informational.
//!
//! The checksum detects accidental damage and edits that did not also rewrite
//! the checksum. It does not authenticate the bundle: anyone who can write a
//! file can write a valid checksum over it.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{
    AgentType, CodingProvider, ExternalRef, HumanReviewDecision, HumanReviewEntry, ProjectBase,
    QaSignoff, RemoteType, ReviewMode, SecuritySeverity, Subtask, TaskCategory, TaskComplexity,
    TaskImpact, TaskPriority, TaskStatus,
};

pub const FORMAT: &str = "slashit.project-export";
pub const FORMAT_VERSION: u32 = 1;

/// Largest bundle the reader will open. One Project's record is far below
/// this; the bound stops a hostile or accidental multi-gigabyte file from
/// being read whole into memory.
pub const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;

/// What a v1 export never contains. The manifest repeats this list so a reader
/// can see the gaps without the code. A bundle whose manifest differs is
/// refused, so a future writer cannot silently widen v1.
pub const OMITTED: &[&str] = &[
    "conversations",
    "agent_config.command",
    "agent_config.args",
    "agent_config.env",
    "agent_config.api_key",
    "project.scope",
    "project.state_location",
    "repository.local_path",
    "task.worktree_path",
    "task.branch_name",
    "task.checkout_admission",
    "task.approved_execution",
    "task.executed_approvals",
    "task.new_attempt",
    "task.worker_handoff",
    "task.pending_republish",
    "task.pr_create_unconfirmed",
    "task.pr_publication",
    "task.local_integration",
    "task.cleanup_in_flight",
    "task.pr_review_plan",
    "task.human_review.pr_error",
    "activity.run_results",
    "activity.tool_calls",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleError {
    /// Not a SlashIt Project export at all.
    NotAnExport,
    /// A format version this build does not read.
    UnsupportedVersion(u32),
    TooLarge,
    /// Syntactically or structurally invalid, or the manifest disagrees.
    Malformed(String),
    /// Well-formed, but it names state an export never writes.
    Unsafe(String),
    ChecksumMismatch,
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnExport => f.write_str("the file is not a SlashIt Project export"),
            Self::UnsupportedVersion(v) => write!(
                f,
                "the export has format_version {v}, and this build reads format_version {FORMAT_VERSION} only"
            ),
            Self::TooLarge => write!(f, "the export is larger than {MAX_BUNDLE_BYTES} bytes"),
            Self::Malformed(why) => write!(f, "the export is malformed: {why}"),
            Self::Unsafe(why) => write!(f, "the export contains state an export never writes: {why}"),
            Self::ChecksumMismatch => {
                f.write_str("the checksum does not match the payload; the export was damaged or edited")
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: String,
    format_version: u32,
    build: String,
    created_at: DateTime<Utc>,
    manifest: Manifest,
    checksum: String,
    payload: Box<RawValue>,
}

/// Counts and gaps, derived from the payload. Checked on read, so it cannot
/// disagree with what was actually written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub repositories: u32,
    pub tasks: u32,
    pub dependencies: u32,
    pub activity_entries: u32,
    pub review_entries: u32,
    pub omitted: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payload {
    pub project: PortableProject,
    pub repository: Option<PortableRepository>,
    pub tasks: Vec<PortableTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableProject {
    pub id: Uuid,
    pub name: String,
    pub agent_type: AgentType,
    pub model: Option<String>,
    /// The local branch new Tasks start from, as a name. Not a commit and not
    /// a path: the importing repository must have a branch of this name.
    pub base: Option<ProjectBase>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableRepository {
    pub id: Uuid,
    pub remote_url: Option<String>,
    pub remote_type: Option<RemoteType>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableTask {
    pub id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub status: TaskStatus,
    pub model: String,
    pub provider: CodingProvider,
    pub review_mode: ReviewMode,
    pub reviewer_model: Option<String>,
    pub planning_mode: bool,
    pub dependencies: Vec<Uuid>,
    pub category: TaskCategory,
    pub priority: TaskPriority,
    pub complexity: TaskComplexity,
    pub impact: TaskImpact,
    pub security_severity: SecuritySeverity,
    pub subtasks: Vec<Subtask>,
    pub sequence_number: u32,
    pub position: i32,
    pub external_refs: Vec<ExternalRef>,
    pub github_issue_url: Option<String>,
    pub gitlab_issue_url: Option<String>,
    pub linear_ticket_id: Option<String>,
    pub jira_issue_key: Option<String>,
    pub pr_url: Option<String>,
    pub qa_signoff: Option<QaSignoff>,
    pub human_review: PortableHumanReview,
    /// The commit the Task branch started from, as text. Historical only: it
    /// names an object in the exporting repository and is never an input.
    pub base_commit: Option<String>,
    /// The Task timeline, minus raw run output and tool calls. Each value must
    /// parse as a `slashit_activity::Entry`.
    pub activity: Vec<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableHumanReview {
    pub arrivals: u32,
    pub entries: Vec<HumanReviewEntry>,
}

impl PortableHumanReview {
    /// Whether the decision made in the record's own arrival is an approval.
    /// True means the bundle, read as it stands, would present an approval as
    /// current. Import must withdraw it before anything can act on it.
    pub fn has_current_approval(&self) -> bool {
        self.entries.last().is_some_and(|e| {
            e.arrival == self.arrivals && e.decision == HumanReviewDecision::Approved
        })
    }
}

impl Payload {
    pub fn manifest(&self) -> Manifest {
        Manifest {
            repositories: u32::from(self.repository.is_some()),
            tasks: self.tasks.len() as u32,
            dependencies: self.tasks.iter().map(|t| t.dependencies.len() as u32).sum(),
            activity_entries: self.tasks.iter().map(|t| t.activity.len() as u32).sum(),
            review_entries: self
                .tasks
                .iter()
                .map(|t| t.human_review.entries.len() as u32)
                .sum(),
            omitted: OMITTED.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// A bundle that passed every check in [`decode`].
#[derive(Debug, Clone)]
pub struct Decoded {
    pub build: String,
    pub created_at: DateTime<Utc>,
    pub payload: Payload,
}

/// Serialize a payload into a complete bundle. Deterministic: the same payload,
/// `created_at` and `build` always give the same bytes.
pub fn encode(
    payload: &Payload,
    created_at: DateTime<Utc>,
    build: &str,
) -> Result<Vec<u8>, String> {
    let raw = serde_json::value::to_raw_value(payload).map_err(|e| e.to_string())?;
    let envelope = Envelope {
        format: FORMAT.to_string(),
        format_version: FORMAT_VERSION,
        build: build.to_string(),
        created_at,
        manifest: payload.manifest(),
        checksum: sha256_hex(raw.get().as_bytes()),
        payload: raw,
    };
    let mut bytes = serde_json::to_vec_pretty(&envelope).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Read and fully validate a bundle. Nothing is returned unless every check
/// passes; the checks run in the order that gives the most specific refusal.
pub fn decode(bytes: &[u8]) -> Result<Decoded, BundleError> {
    if bytes.len() as u64 > MAX_BUNDLE_BYTES {
        return Err(BundleError::TooLarge);
    }
    // The header is read before anything strict, so a newer version is named
    // as such rather than failing on a field this build does not know.
    #[derive(Deserialize)]
    struct Header {
        format: Option<String>,
        format_version: Option<u32>,
    }
    let header: Header =
        serde_json::from_slice(bytes).map_err(|e| BundleError::Malformed(e.to_string()))?;
    if header.format.as_deref() != Some(FORMAT) {
        return Err(BundleError::NotAnExport);
    }
    match header.format_version {
        Some(FORMAT_VERSION) => {}
        Some(other) => return Err(BundleError::UnsupportedVersion(other)),
        None => return Err(BundleError::Malformed("format_version is missing".into())),
    }

    let envelope: Envelope =
        serde_json::from_slice(bytes).map_err(|e| BundleError::Malformed(e.to_string()))?;
    if sha256_hex(envelope.payload.get().as_bytes()) != envelope.checksum {
        return Err(BundleError::ChecksumMismatch);
    }
    let payload: Payload = serde_json::from_str(envelope.payload.get())
        .map_err(|e| BundleError::Malformed(e.to_string()))?;
    if payload.manifest() != envelope.manifest {
        return Err(BundleError::Malformed(
            "the manifest does not describe the payload".into(),
        ));
    }
    validate(&payload)?;
    Ok(Decoded {
        build: envelope.build,
        created_at: envelope.created_at,
        payload,
    })
}

/// Structural rules a valid export satisfies by construction. A bundle that
/// breaks one was edited or produced by something else.
fn validate(payload: &Payload) -> Result<(), BundleError> {
    let malformed = |why: String| Err(BundleError::Malformed(why));
    let mut ids: HashSet<Uuid> = HashSet::new();
    for task in &payload.tasks {
        if !ids.insert(task.id) {
            return malformed(format!("task {} appears twice", task.id));
        }
    }
    for task in &payload.tasks {
        if matches!(
            task.status,
            TaskStatus::Queue | TaskStatus::InProgress | TaskStatus::AiReview
        ) {
            return Err(BundleError::Unsafe(format!(
                "task {} is in {:?}, a state an export refuses to write",
                task.id, task.status
            )));
        }
        for dependency in &task.dependencies {
            if *dependency == task.id {
                return malformed(format!("task {} depends on itself", task.id));
            }
            if !ids.contains(dependency) {
                return malformed(format!(
                    "task {} depends on {dependency}, which is not in the export",
                    task.id
                ));
            }
        }
        if has_duplicate(&task.dependencies) {
            return malformed(format!("task {} lists a dependency twice", task.id));
        }
        for value in &task.activity {
            let entry: slashit_activity::Entry = serde_json::from_value(value.clone())
                .map_err(|e| BundleError::Malformed(format!("task {} activity: {e}", task.id)))?;
            use slashit_activity::Kind;
            if matches!(
                entry.kind,
                Kind::RunResult { .. } | Kind::ToolUsed { .. } | Kind::ToolsOmitted { .. }
            ) {
                return Err(BundleError::Unsafe(format!(
                    "task {} carries run output",
                    task.id
                )));
            }
        }
        let review = &task.human_review;
        let mut previous = 0;
        for entry in &review.entries {
            if entry.sequence <= previous || entry.arrival > review.arrivals {
                return malformed(format!("task {} has a review entry out of order", task.id));
            }
            previous = entry.sequence;
        }
    }
    if let Err(cycle) = check_acyclic(&payload.tasks) {
        return malformed(cycle);
    }
    Ok(())
}

fn has_duplicate(values: &[Uuid]) -> bool {
    let mut seen = HashSet::new();
    values.iter().any(|v| !seen.insert(*v))
}

fn check_acyclic(tasks: &[PortableTask]) -> Result<(), String> {
    let edges: HashMap<Uuid, &[Uuid]> = tasks
        .iter()
        .map(|t| (t.id, t.dependencies.as_slice()))
        .collect();
    // Iterative depth-first search with an explicit colour map; a back edge is a cycle.
    let mut colour: HashMap<Uuid, u8> = HashMap::new();
    for start in tasks.iter().map(|t| t.id) {
        if colour.contains_key(&start) {
            continue;
        }
        let mut stack = vec![(start, 0usize)];
        colour.insert(start, 1);
        while let Some((node, next)) = stack.pop() {
            let deps = edges.get(&node).copied().unwrap_or(&[]);
            if next < deps.len() {
                stack.push((node, next + 1));
                let dep = deps[next];
                match colour.get(&dep).copied() {
                    Some(1) => return Err(format!("dependencies form a cycle through {dep}")),
                    Some(_) => {}
                    None => {
                        colour.insert(dep, 1);
                        stack.push((dep, 0));
                    }
                }
            } else {
                colour.insert(node, 2);
            }
        }
    }
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
