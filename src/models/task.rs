use serde::{Deserialize, Serialize};
pub use slashit_attention::AttentionReason;
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    #[serde(default)]
    pub human_review: HumanReviewRecord,
    pub stuck_since: Option<chrono::DateTime<chrono::Utc>>,

    #[serde(default)]
    pub error_message: Option<String>,

    #[serde(default)]
    pub worktree_path: Option<String>,
    #[serde(default)]
    pub branch_name: Option<String>,

    #[serde(default)]
    pub pr_review_plan: Option<PrReviewPlan>,

    /// Milestones the backend recorded, oldest first. See
    /// `slashit_activity`; the drawer's timeline reads them together with
    /// `created_at` and `human_review`.
    #[serde(default, deserialize_with = "slashit_activity::lenient")]
    pub activity: Vec<slashit_activity::Entry>,

    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Task {
    /// What happened to the task, oldest first.
    pub fn timeline(&self) -> Vec<slashit_activity::Item<'_>> {
        let decisions = self.human_review.entries.iter().map(|e| slashit_activity::Decision {
            sequence: e.sequence,
            at: e.decided_at,
            approved: e.decision == HumanReviewDecision::Approved,
            feedback: e.feedback.as_deref(),
        });
        slashit_activity::timeline(self.created_at, decisions, &self.activity)
    }

    /// Whether the task cannot make progress without the user right now, and
    /// why. The rule is `slashit_attention::Facts::needs_you`, the same one
    /// the backend answers with; this only copies the task's facts into it.
    ///
    /// `delivery_in_flight` is whether its pull request is being opened at
    /// this moment.
    pub fn needs_you(&self, delivery_in_flight: bool) -> Option<AttentionReason> {
        use slashit_attention::{Decision, Facts, Status};
        Facts {
            status: match self.status {
                TaskStatus::Error => Status::Error,
                TaskStatus::HumanReview => Status::HumanReview,
                TaskStatus::Backlog
                | TaskStatus::Queue
                | TaskStatus::InProgress
                | TaskStatus::AiReview
                | TaskStatus::PrCreated
                | TaskStatus::Done => Status::Other,
            },
            decision: self.human_review.current_decision().map(|e| match e.decision {
                HumanReviewDecision::Approved => Decision::Approved,
                HumanReviewDecision::ChangesRequested => Decision::ChangesRequested,
            }),
            pr_error_recorded: self.human_review.pr_error.is_some(),
            pr_linked: self.pr_url.is_some() || self.external_refs.iter().any(ExternalRef::is_pr),
            delivery_in_flight,
        }
        .needs_you()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrReviewPlan {
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub pr_url: String,
    pub review_decision: Option<String>,
    pub comments: Vec<PrReviewComment>,
    pub items: Vec<PrReviewItem>,
    pub raw_plan: String,
    #[serde(default)]
    pub last_apply: Option<PrReviewApplyResult>,
    /// Mirrors the backend field of the same name. It must survive a round
    /// trip through the frontend: the plan is sent back to the backend on
    /// apply, and dropping it would make every fix look recorded before
    /// fingerprints existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixed_content: Vec<FixedContent>,
}

/// Mirrors the backend type of the same name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FixedContent {
    pub comment_id: u64,
    pub fingerprint: String,
}

impl PrReviewPlan {
    /// Mirrors the backend rule of the same name, through the same shared
    /// `slashit_review_content` functions.
    pub fn fixed_content_is_current(
        &self,
        comment_id: u64,
        applied_at: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        let Some(comment) = self.comments.iter().find(|c| c.id == Some(comment_id)) else {
            return true;
        };
        let recorded = self.fixed_content.iter()
            .find(|f| f.comment_id == comment_id)
            .map(|f| f.fingerprint.as_str());
        slashit_review_content::fixed_content_is_current(
            recorded,
            &comment.body,
            comment.updated_at,
            applied_at,
            self.generated_at <= applied_at,
        )
    }

    /// Derive per-item `fix_done` / `reply_posted` from a persisted
    /// `last_apply`. Mirrors the backend helper of the same name so the
    /// frontend can backfill the cached plan on modal open without a round
    /// trip — old plans created before lifecycle tracking surface their
    /// badges immediately.
    /// A comment whose text is no longer the text its fix was made from
    /// regains neither flag.
    pub fn backfill_lifecycle_from_last_apply(&mut self) {
        let Some(last) = self.last_apply.clone() else { return; };
        if last.dry_run { return; }
        let failed_reply_ids: std::collections::HashSet<u64> = last.reply_errors.iter()
            .filter_map(|s| {
                let rest = s.strip_prefix("comment ")?;
                let (id, _) = rest.split_once(':')?;
                id.trim().parse::<u64>().ok()
            })
            .collect();
        for idx in 0..self.items.len() {
            let Some(cid) = self.items[idx].comment_id else { continue; };
            let current = last.fixed_ids.contains(&cid)
                && self.fixed_content_is_current(cid, last.applied_at);
            let item = &mut self.items[idx];
            if current {
                if !item.fix_done {
                    item.fix_done = true;
                }
                // Replies wait for delivery: see the backend helper.
                if last.auto_reply == Some(true)
                    && !item.reply_posted
                    && !item.fix_uncommitted
                    && item.fix_commit.is_none()
                    && !failed_reply_ids.contains(&cid)
                {
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
    #[serde(default)]
    pub author_association: Option<String>,
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

impl PrReviewComment {
    /// Mirrors the backend rule of the same name: only an owner, member or
    /// collaborator's Fix items may start out approved.
    pub fn author_is_collaborator(&self) -> bool {
        matches!(
            self.author_association.as_deref(),
            Some("OWNER" | "MEMBER" | "COLLABORATOR")
        )
    }
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
    pub decision: PrReviewDecisionKind,
    pub reasoning: String,
    pub proposed_change: String,
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub user_note: String,
    #[serde(default)]
    pub fix_done: bool,
    /// Mirrors the backend field: a fix not yet committed and pushed.
    #[serde(default)]
    pub fix_uncommitted: bool,
    /// Mirrors the backend field: the commit carrying this item's fix. It
    /// must survive a round trip through the frontend, or every fix would
    /// look like one saved before commits were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_commit: Option<String>,
    #[serde(default)]
    pub reply_posted: bool,
    #[serde(default)]
    pub last_agent_summary: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub pr_reply_text: Option<String>,
    #[serde(default)]
    pub reply_comment_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrReviewDecisionKind {
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
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub failed_ids: Vec<u64>,
    #[serde(default)]
    pub fix_errors: Vec<String>,
    #[serde(default)]
    pub push_error: Option<String>,
    /// Whether this apply ran with `auto_reply=true` — `None` for a result
    /// persisted before this field existed. Mirrors the backend field
    /// exactly, including the tri-state: backfill must not treat a missing
    /// historical value as `false`, and must not use `replies_posted` to
    /// guess which individual item it belongs to. See the backend
    /// `PrReviewApplyResult::auto_reply` doc for the full reasoning.
    #[serde(default)]
    pub auto_reply: Option<bool>,
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

impl std::fmt::Display for TaskCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Feature => write!(f, "Feature"),
            Self::BugFix => write!(f, "Bug Fix"),
            Self::Refactoring => write!(f, "Refactoring"),
            Self::Documentation => write!(f, "Documentation"),
            Self::Security => write!(f, "Security"),
            Self::Performance => write!(f, "Performance"),
            Self::UiUx => write!(f, "UI/UX"),
            Self::Infrastructure => write!(f, "Infrastructure"),
            Self::Testing => write!(f, "Testing"),
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Subtask {
    pub id: Uuid,
    pub title: String,
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

/// Mirrors the backend `HumanReviewRecord`: decisions made at Human Review,
/// each tied to the Human Review arrival it was made in.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HumanReviewRecord {
    #[serde(default)]
    pub arrivals: u32,
    #[serde(default)]
    pub entries: Vec<HumanReviewEntry>,
    /// Why the last attempt to open a pull request for the approved changes
    /// failed.
    #[serde(default)]
    pub pr_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HumanReviewEntry {
    pub sequence: u32,
    pub arrival: u32,
    pub decision: HumanReviewDecision,
    #[serde(default)]
    pub feedback: Option<String>,
    pub decided_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HumanReviewDecision {
    Approved,
    ChangesRequested,
}

impl HumanReviewRecord {
    /// The decision made in the current arrival, if one was. Same rule as
    /// the backend.
    pub fn current_decision(&self) -> Option<&HumanReviewEntry> {
        self.entries.last().filter(|e| e.arrival == self.arrivals)
    }

    pub fn is_approved(&self) -> bool {
        self.current_decision()
            .is_some_and(|e| e.decision == HumanReviewDecision::Approved)
    }

    /// Changes requested in earlier reviews, oldest first: the feedback the
    /// changes now under review were made to answer.
    pub fn earlier_requests(&self) -> Vec<&HumanReviewEntry> {
        self.entries
            .iter()
            .filter(|e| e.arrival < self.arrivals)
            .filter(|e| e.decision == HumanReviewDecision::ChangesRequested)
            .collect()
    }
}

/// The AI review's findings, ready to show: list markers and `ISSUE:`
/// prefixes removed, blank lines dropped, and exact repeats shown once, in
/// the order they were first reported.
pub fn review_findings(issues: &[String]) -> Vec<String> {
    let mut findings: Vec<String> = Vec::new();
    for issue in issues {
        let text = strip_finding_prefixes(issue);
        if !text.is_empty() && !findings.iter().any(|f| f == text) {
            findings.push(text.to_string());
        }
    }
    findings
}

fn strip_finding_prefixes(issue: &str) -> &str {
    let mut text = issue.trim();
    loop {
        let before = text;
        for marker in ["- ", "* "] {
            if let Some(rest) = text.strip_prefix(marker) {
                text = rest.trim_start();
            }
        }
        if text.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("issue:")) {
            text = text[6..].trim_start();
        }
        if text == before {
            return text;
        }
    }
}




#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueConfig {
    pub parallel_task_limit: u32,
    pub auto_promote: bool,
    pub fifo_ordering: bool,
    pub use_coderabbit: bool,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            parallel_task_limit: 3,
            auto_promote: true,
            fifo_ordering: true,
            use_coderabbit: true,
        }
    }
}

/// A pull request, as GitHub identifies it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PrKey {
    pub repo: String,
    pub number: u32,
}

/// What GitHub last said about one pull request, as the backend's
/// `pr_status::PrStatusEntry` holds it. Mirrored by hand: both sides test
/// against the same JSON literal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatusEntry {
    pub repo: String,
    pub number: u32,
    /// The last status read successfully. A later failure never clears it.
    pub status: Option<PrStatus>,
    /// When `status` was read.
    pub fetched_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When the latest attempt started.
    pub attempted_at: chrono::DateTime<chrono::Utc>,
    /// Why the latest attempt failed, if it did.
    pub error: Option<PrFetchError>,
}

impl PrStatusEntry {
    pub fn key(&self) -> PrKey {
        PrKey { repo: self.repo.clone(), number: self.number }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatus {
    pub state: PrState,
    pub checks: ChecksState,
    pub failing_checks: Vec<String>,
    pub failing_check_count: u32,
    pub review_decision: Option<ReviewDecision>,
    pub mergeable: Option<Mergeability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    Merged,
    Unknown,
}

/// What a pull request's checks add up to. Only `Passing` is good news.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    Passing,
    Failing,
    Pending,
    NoChecks,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mergeability {
    Mergeable,
    Conflicting,
    /// GitHub has not finished computing it.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrFetchError {
    pub kind: PrFetchErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrFetchErrorKind {
    Timeout,
    Auth,
    NotInstalled,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same literal the backend's `pr_status` tests serialize to.
    #[test]
    fn a_pull_request_status_entry_reads_the_backend_shape() {
        let entry: PrStatusEntry = serde_json::from_value(serde_json::json!({
            "repo": "o/r",
            "number": 7,
            "status": {
                "state": "open",
                "checks": "no_checks",
                "failing_checks": [],
                "failing_check_count": 0,
                "review_decision": "review_required",
                "mergeable": "mergeable"
            },
            "fetched_at": "2026-09-30T12:00:00Z",
            "attempted_at": "2026-09-30T12:00:00Z",
            "error": { "kind": "timeout", "message": "slow" }
        }))
        .expect("the backend's shape");
        let status = entry.status.clone().unwrap();
        assert_eq!(status.checks, ChecksState::NoChecks);
        assert_eq!(status.review_decision, Some(ReviewDecision::ReviewRequired));
        assert_eq!(status.mergeable, Some(Mergeability::Mergeable));
        assert_eq!(entry.error.as_ref().map(|e| e.kind), Some(PrFetchErrorKind::Timeout));
        assert_eq!(entry.key(), PrKey { repo: "o/r".to_string(), number: 7 });
    }

    /// Two Fix items, comments 1 and 2, fixed and answered by an apply at
    /// `applied_at` whose lifecycle flags are not set yet.
    fn plan_applied_at(applied_at: &str, updated: [Option<&str>; 2]) -> PrReviewPlan {
        let comment = |id: u64, updated_at: Option<&str>| {
            serde_json::json!({
                "id": id, "kind": "inline", "author": "r", "body": "b",
                "updated_at": updated_at,
            })
        };
        let item = |id: u64| {
            serde_json::json!({
                "comment_id": id, "summary": "s", "decision": "fix",
                "reasoning": "r", "proposed_change": "c", "approved": true,
            })
        };
        serde_json::from_value(serde_json::json!({
            "generated_at": "2026-09-30T12:00:00Z",
            "pr_url": "https://github.com/o/r/pull/1",
            "review_decision": null,
            "comments": [comment(1, updated[0]), comment(2, updated[1])],
            "items": [item(1), item(2)],
            "raw_plan": "",
            "last_apply": {
                "applied_at": applied_at,
                "agent_summary": "",
                "fixed_ids": [1, 2],
                "skipped_ids": [],
                "auto_reply": true,
            },
        }))
        .expect("a plan in the backend's shape")
    }

    #[test]
    fn backfill_skips_a_comment_edited_after_the_last_apply() {
        let mut plan = plan_applied_at(
            "2026-09-30T12:00:30.500Z",
            [Some("2026-09-30T12:05:00Z"), Some("2026-09-30T11:00:00Z")],
        );
        plan.items[0].fix_uncommitted = true;

        plan.backfill_lifecycle_from_last_apply();

        assert!(!plan.items[0].fix_done && !plan.items[0].reply_posted);
        assert!(plan.items[0].fix_uncommitted, "the checkout fact survives the edit");
        assert!(plan.items[1].fix_done && plan.items[1].reply_posted, "unchanged comments keep the backfill");
    }

    #[test]
    fn backfill_treats_a_same_second_update_as_a_possible_edit() {
        let mut plan = plan_applied_at(
            "2026-09-30T12:00:30.700Z",
            [Some("2026-09-30T12:00:30Z"), Some("2026-09-30T12:00:29Z")],
        );

        plan.backfill_lifecycle_from_last_apply();

        assert!(!plan.items[0].fix_done && !plan.items[0].reply_posted);
        assert!(plan.items[1].fix_done && plan.items[1].reply_posted);
    }

    // The backend asserts the same table in `commands::pr` tests.
    const BACKFILL_CASES: [(Option<&str>, &str, Option<&str>, bool); 7] = [
        (Some("a"), "a", Some("2024-06-05T00:00:00Z"), true),
        (Some("a"), "b", Some("2024-05-01T00:00:00Z"), false),
        (Some("a"), "a\r\n", None, true),
        (Some("a"), "b", None, false),
        (None, "b", Some("2024-06-01T00:00:29Z"), true),
        (None, "b", Some("2024-06-01T00:00:30Z"), false),
        (None, "b", None, true),
    ];

    #[test]
    fn backfill_follows_the_recorded_text_and_falls_back_to_timestamps_only_without_one() {
        for (recorded, current, updated_at, restored) in BACKFILL_CASES {
            let fixed: Vec<serde_json::Value> = recorded
                .map(|r| serde_json::json!({
                    "comment_id": 1,
                    "fingerprint": slashit_review_content::fingerprint(r),
                }))
                .into_iter()
                .collect();
            let mut plan: PrReviewPlan = serde_json::from_value(serde_json::json!({
                "generated_at": "2024-05-31T00:00:00Z",
                "pr_url": "https://github.com/o/r/pull/1",
                "review_decision": null,
                "comments": [{
                    "id": 1, "kind": "inline", "author": "r",
                    "body": current, "updated_at": updated_at,
                }],
                "items": [{
                    "comment_id": 1, "summary": "s", "decision": "fix",
                    "reasoning": "r", "proposed_change": "c", "approved": true,
                }],
                "raw_plan": "",
                "last_apply": {
                    "applied_at": "2024-06-01T00:00:30.500Z",
                    "agent_summary": "", "fixed_ids": [1], "skipped_ids": [],
                    "auto_reply": true,
                },
                "fixed_content": fixed,
            }))
            .expect("a plan in the backend's shape");

            plan.backfill_lifecycle_from_last_apply();

            let case = (recorded, current, updated_at);
            assert_eq!(plan.items[0].fix_done, restored, "{case:?}");
            assert_eq!(plan.items[0].reply_posted, restored, "{case:?}");
        }
    }

    #[test]
    fn backfill_does_not_restore_a_legacy_plan_generated_after_its_apply() {
        let mut plan: PrReviewPlan = serde_json::from_value(serde_json::json!({
            "generated_at": "2024-06-01T00:30:00Z",
            "pr_url": "https://github.com/o/r/pull/1",
            "review_decision": null,
            "comments": [{
                "id": 42, "kind": "inline", "author": "r",
                "body": "B", "updated_at": "2024-06-01T00:05:00Z",
            }],
            "items": [{
                "comment_id": 42, "summary": "s", "decision": "fix",
                "reasoning": "r", "proposed_change": "c", "approved": true,
            }],
            "raw_plan": "",
            "last_apply": {
                "applied_at": "2024-06-01T00:10:00Z",
                "agent_summary": "", "fixed_ids": [42], "skipped_ids": [],
                "auto_reply": true,
            },
        }))
        .expect("a plan in the backend's shape");
        plan.items[0].fix_uncommitted = true;

        plan.backfill_lifecycle_from_last_apply();

        assert!(!plan.items[0].fix_done && !plan.items[0].reply_posted);
        assert!(plan.items[0].fix_uncommitted);
    }

    #[test]
    fn the_recorded_text_survives_a_round_trip_through_the_frontend() {
        let plan = plan_applied_at("2024-06-01T00:00:30Z", [None, None]);
        assert!(plan.fixed_content.is_empty());
        let mut recorded = plan;
        recorded.fixed_content.push(FixedContent { comment_id: 1, fingerprint: "sha256-v1:x".to_string() });
        let back: PrReviewPlan =
            serde_json::from_str(&serde_json::to_string(&recorded).unwrap()).unwrap();
        assert_eq!(back.fixed_content, recorded.fixed_content);
        let none = plan_applied_at("2024-06-01T00:00:30Z", [None, None]);
        assert!(serde_json::to_value(&none).unwrap().get("fixed_content").is_none());
    }

    #[test]
    fn backfill_keeps_a_comment_without_an_update_time_as_unchanged() {
        let mut plan = plan_applied_at("2026-09-30T12:00:30Z", [None, None]);

        plan.backfill_lifecycle_from_last_apply();

        assert!(plan.items.iter().all(|i| i.fix_done && i.reply_posted));
    }

    fn issues(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn findings_lose_their_prefixes_and_repeats() {
        let found = review_findings(&issues(&[
            "- ISSUE: [high] src/lib.rs:4 - panics on empty input",
            "- - ISSUE: ISSUE: [high] src/lib.rs:4 - panics on empty input",
            "ISSUE: [low] README.md:1 - typo",
            "issue: [low] README.md:1 - typo",
            "   ",
            "* [medium] src/main.rs:9 - flag ignored",
            "The review fixes could not be committed: git said no.",
        ]));
        assert_eq!(
            found,
            vec![
                "[high] src/lib.rs:4 - panics on empty input",
                "[low] README.md:1 - typo",
                "[medium] src/main.rs:9 - flag ignored",
                "The review fixes could not be committed: git said no.",
            ]
        );
    }

    #[test]
    fn findings_that_only_look_like_prefixes_are_kept_intact() {
        assert_eq!(review_findings(&issues(&["Issues found in parser"])), vec!["Issues found in parser"]);
        assert_eq!(review_findings(&issues(&["ÉISSUE: accent"])), vec!["ÉISSUE: accent"]);
        assert_eq!(review_findings(&issues(&["ISSUE:"])), Vec::<String>::new());
    }

    fn entry(sequence: u32, arrival: u32, decision: HumanReviewDecision, feedback: Option<&str>) -> HumanReviewEntry {
        HumanReviewEntry {
            sequence,
            arrival,
            decision,
            feedback: feedback.map(str::to_string),
            decided_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn the_current_decision_and_earlier_requests_follow_the_arrivals() {
        let mut review = HumanReviewRecord { arrivals: 1, ..Default::default() };
        assert!(review.current_decision().is_none());
        review.entries.push(entry(1, 1, HumanReviewDecision::ChangesRequested, Some("fix it")));
        assert!(review.earlier_requests().is_empty(), "not earlier yet: it is this review's");

        review.arrivals = 2;
        assert!(review.current_decision().is_none());
        assert_eq!(review.earlier_requests().len(), 1);

        review.entries.push(entry(2, 2, HumanReviewDecision::Approved, None));
        assert!(review.is_approved());
    }

    /// A task exactly as the backend sends it over IPC.
    fn task_json(status: &str, review: serde_json::Value, pr_url: Option<&str>) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "project_id": "22222222-2222-2222-2222-222222222222",
            "title": "t", "description": null, "status": status, "model": "m",
            "planning_mode": false, "dependencies": [], "workspace_id": null, "jj_change_id": null,
            "category": "feature", "priority": "medium", "complexity": "moderate",
            "impact": "medium", "security_severity": "none", "phase": "idle",
            "phase_progress": 0, "overall_progress": 0, "subtasks": [], "sequence_number": 1,
            "github_issue_url": null, "gitlab_issue_url": null, "linear_ticket_id": null,
            "pr_url": pr_url, "qa_signoff": null, "human_review": review, "stuck_since": null,
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap()
    }

    fn decided(decision: &str, pr_error: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "arrivals": 1,
            "entries": [{ "sequence": 1, "arrival": 1, "decision": decision, "decided_at": "2026-01-01T00:00:00Z" }],
            "pr_error": pr_error,
        })
    }

    #[test]
    fn attention_reads_the_record_the_backend_sends() {
        let undecided = serde_json::json!({ "arrivals": 1, "entries": [] });
        assert_eq!(task_json("error", undecided.clone(), None).needs_you(false), Some(AttentionReason::Failed));
        assert_eq!(task_json("human_review", undecided.clone(), None).needs_you(false), Some(AttentionReason::Review));
        assert_eq!(task_json("human_review", undecided.clone(), None).needs_you(true), None);
        assert_eq!(task_json("human_review", decided("approved", None), None).needs_you(false), None);
        assert_eq!(
            task_json("human_review", decided("approved", Some("gh failed")), None).needs_you(false),
            Some(AttentionReason::PrNotCreated)
        );
        assert_eq!(
            task_json("human_review", decided("approved", Some("gh failed")), Some("https://x/pull/1")).needs_you(false),
            None
        );
        assert_eq!(task_json("human_review", decided("changes_requested", None), None).needs_you(false), None);
        for status in ["backlog", "queue", "in_progress", "ai_review", "pr_created", "done"] {
            assert_eq!(task_json(status, undecided.clone(), None).needs_you(false), None, "{status}");
        }
    }

    /// The backend sends `human_review` as a table; a task from before it
    /// existed sends none at all.
    #[test]
    fn a_task_without_a_review_record_deserializes_as_unreviewed() {
        let json = serde_json::json!({ "arrivals": 0 });
        let review: HumanReviewRecord = serde_json::from_value(json).unwrap();
        assert_eq!(review, HumanReviewRecord::default());
    }
}

/// What the pull request section offers for a stacked task whose branch is
/// published, as the backend's `commands::pr::RepublishStatus` serializes it.
/// Mirrored by hand: both sides test against the same JSON literal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepublishStatus {
    /// The parent landed and the task can be moved onto it. `rewrites` is
    /// false when only the pull request's base has to change.
    NeedsRestack {
        parent_branch: String,
        parent_pr: u64,
        default_branch: String,
        /// `None` for a branch pushed without a pull request.
        pr_number: Option<u64>,
        rewrites: bool,
    },
    /// The parent landed, but a restack is not possible as things are.
    Blocked { reason: String },
    /// A restack was started and did not finish. `discard_blocked` says why
    /// Discard would be refused, when it already is known to be.
    Interrupted { rewritten: bool, detail: String, discard_blocked: Option<String> },
}

/// What a finished restack did, as `commands::pr::RepublishOutcome` serializes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepublishOutcome {
    pub rewritten: bool,
    pub base: String,
    pub previous_tip: String,
    pub new_tip: String,
}
