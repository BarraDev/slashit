// SPDX-License-Identifier: MPL-2.0
//! The answer to `GetPrCommentReview`: one Task's PR Comment Review as SlashIt
//! holds it, shaped for reading and export.
//!
//! The instance builds it from the Task's stored review plan and reply
//! records, plus one read of the pull request's current head
//! ([`PrCommentReviewHeadCheck`]), and the CLI renders it (JSON or Markdown)
//! from these types. Nothing here can analyze, start an agent, change an
//! approval or edit the plan.

use serde::{Deserialize, Serialize};

/// One Task's PR Comment Review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrCommentReview {
    pub task_id: String,
    pub task_title: String,
    /// The pull request the Task links, as SlashIt records it. `None` when
    /// the Task links none.
    pub task_pr_url: Option<String>,
    /// Whether the stored analysis still matches the pull request's head, as
    /// observed when this answer was built. Never stored.
    pub head_check: PrCommentReviewHeadCheck,
    /// The stored analysis. `None` when the Task has never been analyzed. Its
    /// `pr_url` is the pull request it read, which can differ from
    /// `task_pr_url` if the Task was linked to another one since.
    pub plan: Option<PrCommentReviewPlan>,
}

/// The analyzed head compared with the pull request head on the remote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrCommentReviewHeadCheck {
    pub state: PrCommentReviewHeadState,
    /// The head the remote reported. `None` unless the state is `current` or
    /// `stale`.
    pub observed_head_oid: Option<String>,
    /// Why no comparison was made, for `unavailable` and `unknown`.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrCommentReviewHeadState {
    /// The remote head is the analyzed head.
    Current,
    /// The remote head is another commit.
    Stale,
    /// The head of the analyzed pull request could not be observed: the Task
    /// links no pull request or another one, or the remote read failed.
    Unavailable,
    /// No analyzed head is stored: there is no plan, or it predates the
    /// recording of the head. The remote is not read.
    Unknown,
}

impl PrCommentReviewHeadState {
    /// The name used on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Stale => "stale",
            Self::Unavailable => "unavailable",
            Self::Unknown => "unknown",
        }
    }
}

/// The stored analysis, with every finding in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrCommentReviewPlan {
    /// The pull request the analysis read.
    pub pr_url: String,
    /// `owner/name` and number, when the URL is a GitHub pull request URL.
    pub repository: Option<String>,
    pub pr_number: Option<u64>,
    /// The pull request's head commit when it was analyzed. `None` for an
    /// analysis saved before it was recorded.
    pub analyzed_head_oid: Option<String>,
    /// The Task branch's tip in the checkout the analysis read.
    pub analyzed_worktree_oid: Option<String>,
    /// RFC 3339.
    pub generated_at: String,
    pub analyzed_at: Option<String>,
    /// GitHub's review decision when the analysis ran.
    pub review_decision: Option<String>,
    /// Counts the saved writes that changed a Human override.
    pub human_edit_revision: u64,
    pub findings: Vec<PrCommentReviewFinding>,
    /// The last Apply, where one ran.
    pub last_apply: Option<PrCommentReviewApply>,
}

/// One finding: a reviewer comment and what the analysis and the Human made
/// of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrCommentReviewFinding {
    /// Stable within the stored analysis: `<kind>:<comment id>` for a finding
    /// whose comment the analysis holds once, `comment:<id>` for an id it
    /// holds ambiguously or not at all, `item:<position>` (1-based) for a
    /// finding with no comment. A repeat gets `#<n>` appended. Reading the
    /// same analysis again yields the same ids.
    pub finding_id: String,
    pub comment_id: Option<u64>,
    /// The original reviewer comment, when the analysis holds exactly one
    /// comment with this id.
    pub comment: Option<PrCommentReviewComment>,
    /// The agent's summary and reasoning.
    pub summary: String,
    pub reasoning: String,
    /// `fix`, `skip` or `question`.
    pub decision: String,
    /// The analysis's own proposal, never changed by the Human.
    pub original_proposal: String,
    /// The Human's replacement for the proposal, when there is one.
    pub human_override: Option<String>,
    /// What a fix agent would be given: the override when it has text,
    /// otherwise the original proposal.
    pub effective_instruction: String,
    /// The Human's approval of this finding.
    pub approved: bool,
    /// Whether an Apply would fix it: a `fix` decision that is approved.
    pub selected_for_fix: bool,
    /// The Human's note on a question, when there is one.
    pub user_note: Option<String>,
    pub execution: PrCommentReviewExecution,
    pub publication: PrCommentReviewPublication,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrCommentReviewComment {
    /// `inline`, `review` or `conversation`.
    pub kind: String,
    pub author: String,
    pub author_association: Option<String>,
    pub body: String,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub url: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// What the fix agent did for one finding, as recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrCommentReviewExecution {
    pub fix_done: bool,
    pub fix_uncommitted: bool,
    pub fix_commit: Option<String>,
    /// The checkout's trees before and after the fix agent ran.
    pub fix_before_tree: Option<String>,
    pub fix_after_tree: Option<String>,
    pub last_agent_summary: Option<String>,
    pub last_error: Option<String>,
}

/// What was published to the pull request for one finding, as recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrCommentReviewPublication {
    pub reply_posted: bool,
    /// A PR-level reply whose POST may have reached GitHub.
    pub reply_uncertain: bool,
    pub reply_comment_id: Option<u64>,
    pub reply_text: Option<String>,
}

/// The last Apply's summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrCommentReviewApply {
    pub applied_at: String,
    pub dry_run: bool,
    pub agent_summary: String,
    pub fixed_ids: Vec<u64>,
    pub skipped_ids: Vec<u64>,
    pub failed_ids: Vec<u64>,
    pub fix_errors: Vec<String>,
    pub pushed: bool,
    pub push_branch: Option<String>,
    pub push_error: Option<String>,
    pub replies_posted: u32,
    pub reply_errors: Vec<String>,
}
