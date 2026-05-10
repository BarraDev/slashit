use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "ref_type")]
pub enum ExternalRef {
    #[serde(rename = "github_issue")]
    GithubIssue { url: String, number: u32, repo: String, state: Option<String> },
    #[serde(rename = "github_pr")]
    GithubPr { url: String, number: u32, repo: String, state: Option<String> },
    #[serde(rename = "gitlab_issue")]
    GitlabIssue { url: String },
    #[serde(rename = "jira_ticket")]
    JiraTicket { key: String, project: String },
    #[serde(rename = "linear_ticket")]
    LinearTicket { id: String },
}

impl ExternalRef {
    pub fn label(&self) -> String {
        match self {
            Self::GithubIssue { number, .. } => format!("#{}", number),
            Self::GithubPr { number, .. } => format!("PR #{}", number),
            Self::GitlabIssue { url } => {
                url.rsplit('/').next().map(|n| format!("#{}", n)).unwrap_or("Issue".to_string())
            }
            Self::JiraTicket { key, .. } => key.clone(),
            Self::LinearTicket { id } => id.clone(),
        }
    }

    pub fn url(&self) -> Option<&str> {
        match self {
            Self::GithubIssue { url, .. } | Self::GithubPr { url, .. } | Self::GitlabIssue { url } => Some(url),
            Self::JiraTicket { .. } | Self::LinearTicket { .. } => None,
        }
    }

    pub fn is_pr(&self) -> bool {
        matches!(self, Self::GithubPr { .. })
    }

    pub fn is_issue(&self) -> bool {
        !self.is_pr()
    }
}

/// A task that can be persisted to TOML files.
/// Tasks are stored per-project in separate files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub project_id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub status: TaskStatus,
    pub model: String,
    pub planning_mode: bool,
    pub dependencies: Vec<Uuid>,
    pub workspace_id: Option<Uuid>,
    pub jj_change_id: Option<String>,

    pub category: TaskCategory,
    pub priority: TaskPriority,
    pub complexity: TaskComplexity,
    pub impact: TaskImpact,
    pub security_severity: SecuritySeverity,

    pub phase: TaskPhase,
    pub phase_progress: u8,
    pub overall_progress: u8,
    pub subtasks: Vec<Subtask>,
    pub sequence_number: u32,

    /// Position/order of the task within its column (lower = higher in list)
    #[serde(default)]
    pub position: i32,

    pub github_issue_url: Option<String>,
    pub gitlab_issue_url: Option<String>,
    pub linear_ticket_id: Option<String>,
    #[serde(default)]
    pub jira_issue_key: Option<String>,
    pub pr_url: Option<String>,

    #[serde(default)]
    pub external_refs: Vec<ExternalRef>,

    pub qa_signoff: Option<QaSignoff>,
    pub human_review: Option<HumanReview>,
    pub stuck_since: Option<chrono::DateTime<chrono::Utc>>,

    /// Last error message when task is in Error status
    #[serde(default)]
    pub error_message: Option<String>,

    /// Path to the git worktree for this task (isolates changes per task)
    #[serde(default)]
    pub worktree_path: Option<String>,
    /// Git branch name for this task's worktree
    #[serde(default)]
    pub branch_name: Option<String>,

    /// Last triage of PR review comments. Cached so reopening the modal does
    /// not re-run the LLM, and so post-apply state survives reloads.
    #[serde(default)]
    pub pr_review_plan: Option<PrReviewPlan>,

    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrReviewPlan {
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub pr_url: String,
    pub review_decision: Option<String>,
    pub comments: Vec<PrReviewComment>,
    pub items: Vec<PrReviewItem>,
    /// Raw model output, preserved for fallback display when JSON parsing fails.
    pub raw_plan: String,
    /// Result of the last apply, if any.
    #[serde(default)]
    pub last_apply: Option<PrReviewApplyResult>,
}

impl PrReviewPlan {
    /// Derive per-item `fix_done` and `reply_posted` from the persisted
    /// `last_apply`. Used to upgrade plans that pre-date the lifecycle fields
    /// so badges show immediately for items the user already addressed, and
    /// so a re-apply on the same plan correctly skips items whose work landed
    /// in a prior run.
    ///
    /// Only flips flags from `false` to `true` — never undoes user-visible
    /// state. Dry-run results are ignored on purpose.
    pub fn backfill_lifecycle_from_last_apply(&mut self) {
        let Some(last) = self.last_apply.clone() else { return; };
        if last.dry_run { return; }
        // `reply_errors` come back as `"comment <id>: <msg>"`. Lift the ids out
        // so we know which fixed items missed the reply step.
        let failed_reply_ids: std::collections::HashSet<u64> = last.reply_errors.iter()
            .filter_map(|s| {
                let rest = s.strip_prefix("comment ")?;
                let (id, _) = rest.split_once(':')?;
                id.trim().parse::<u64>().ok()
            })
            .collect();
        for item in self.items.iter_mut() {
            let Some(cid) = item.comment_id else { continue; };
            if last.fixed_ids.contains(&cid) {
                if !item.fix_done {
                    item.fix_done = true;
                }
                if !item.reply_posted && !failed_reply_ids.contains(&cid) {
                    item.reply_posted = true;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrReviewComment {
    pub id: Option<u64>,
    pub kind: PrCommentKind,
    pub author: String,
    pub body: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<i64>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrCommentKind {
    Inline,
    Review,
    Conversation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrReviewItem {
    #[serde(default)]
    pub comment_id: Option<u64>,
    pub summary: String,
    pub decision: PrReviewDecision,
    pub reasoning: String,
    pub proposed_change: String,
    #[serde(default)]
    pub approved: bool,
    /// User-provided note for a Question item, fed to the agent on the next
    /// `discuss_pr_review_questions` round. Cleared once the round completes.
    #[serde(default)]
    pub user_note: String,
    /// True once the agent successfully edited code for this item. Survives
    /// across modal reopens. A re-run with `fix_done=true` skips the agent.
    #[serde(default)]
    pub fix_done: bool,
    /// True once a reply (inline or fallback PR comment) was posted on GitHub
    /// for this item. Decoupled from `fix_done` so a successful fix with a
    /// failed reply leaves the item visibly pending in the "Sync replies" path.
    #[serde(default)]
    pub reply_posted: bool,
    /// Agent's per-item report from the run that set `fix_done=true`. Reused
    /// as the body of a deferred reply when the agent does not need to run
    /// again (already fixed, only the reply is missing).
    #[serde(default)]
    pub last_agent_summary: Option<String>,
    /// Last per-item error message, surfaced as the failed-badge tooltip and
    /// kept across reopens so the user remembers which items still need work.
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrReviewDecision {
    Fix,
    Skip,
    Question,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrReviewApplyResult {
    pub applied_at: chrono::DateTime<chrono::Utc>,
    pub agent_summary: String,
    pub fixed_ids: Vec<u64>,
    pub skipped_ids: Vec<u64>,
    #[serde(default)]
    pub pushed: bool,
    #[serde(default)]
    pub push_branch: Option<String>,
    #[serde(default)]
    pub replies_posted: u32,
    #[serde(default)]
    pub reply_errors: Vec<String>,
    /// True when this result came from a read-only dry run — no edits, push,
    /// or replies actually happened. Lets the UI label the summary as a preview
    /// instead of a real apply.
    #[serde(default)]
    pub dry_run: bool,
    /// Items the agent attempted but failed (e.g. claude exited non-zero for
    /// that item). Distinct from `skipped_ids` (user-marked skip/not approved).
    #[serde(default)]
    pub failed_ids: Vec<u64>,
    /// One human-readable error per failed item, in the form
    /// `"comment <id>: <error>"`. Surfaced in the modal alongside the agent
    /// summary so the user knows which items need manual attention.
    #[serde(default)]
    pub fix_errors: Vec<String>,
    /// Set when `auto_push=true` and the push failed AFTER at least one fix
    /// was applied. The fixes are still on disk; this records why the branch
    /// did not reach the remote.
    #[serde(default)]
    pub push_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Backlog,
    Queue,
    InProgress,
    AiReview,
    HumanReview,
    Done,
    PrCreated,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TaskCategory {
    #[default]
    Feature,
    BugFix,
    Refactoring,
    Documentation,
    Security,
    Performance,
    UiUx,
    Infrastructure,
    Testing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TaskPriority {
    Urgent,
    High,
    #[default]
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TaskComplexity {
    Minimal,
    #[default]
    Moderate,
    Complex,
    Advanced,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TaskImpact {
    Low,
    #[default]
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SecuritySeverity {
    #[default]
    None,
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TaskPhase {
    #[default]
    Idle,
    Planning,
    Coding,
    QaReview,
    QaFixing,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subtask {
    pub id: Uuid,
    pub title: String,
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QaSignoff {
    pub status: QaStatus,
    pub issues_found: Vec<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub session_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QaStatus {
    Approved,
    FixesApplied,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanReview {
    pub approved: bool,
    pub approver: Option<String>,
    pub timestamp: Option<chrono::DateTime<chrono::Utc>>,
    pub feedback: Option<String>,
    pub spec_hash: Option<String>,
}






