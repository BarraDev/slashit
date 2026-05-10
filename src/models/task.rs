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
    pub human_review: Option<HumanReview>,
    pub stuck_since: Option<chrono::DateTime<chrono::Utc>>,

    #[serde(default)]
    pub error_message: Option<String>,

    #[serde(default)]
    pub worktree_path: Option<String>,
    #[serde(default)]
    pub branch_name: Option<String>,

    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HumanReview {
    pub approved: bool,
    pub approver: Option<String>,
    pub timestamp: Option<chrono::DateTime<chrono::Utc>>,
    pub feedback: Option<String>,
    pub spec_hash: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatus {
    pub state: PrState,
    pub checks_passing: Option<bool>,
    pub review_decision: Option<ReviewDecision>,
    pub mergeable: Option<Mergeability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    Merged,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mergeability {
    Mergeable,
    Conflicting,
    Unknown,
}
