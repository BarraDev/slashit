use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use slashit_attention as attention;
pub use slashit_attention::AttentionReason;
use uuid::Uuid;

pub use slashit_activity::{Column as ActivityColumn, Entry as ActivityEntry, Kind as ActivityKind};

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
    /// Per-task git worktree. Renamed from `workspace_id`. Without the alias
    /// a pre-rename record still deserializes (unknown fields are ignored),
    /// but silently drops the reference — `worktree_id` defaults to `None`
    /// since the old `workspace_id` key is never read. The alias makes the
    /// legacy key populate this field instead of being discarded.
    #[serde(alias = "workspace_id", default)]
    pub worktree_id: Option<Uuid>,
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
    /// Decisions made at Human Review. See [`HumanReviewRecord`].
    #[serde(default)]
    pub human_review: HumanReviewRecord,
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

    /// The commit the task's worktree started from: the exact commit of the
    /// default branch on `origin`, of the project's local base branch when
    /// there is no remote default (see [`BranchOrigin::LocalBase`]), or of
    /// the parent branch's tip, for a stacked task, resolved once, at the moment the branch is created, and never
    /// re-derived afterward -- a retry reattaches to the same branch and
    /// must keep comparing against the same starting point, not wherever
    /// the branch tip has since moved to. `None` for a task persisted
    /// before this field existed, or one attached to a branch SlashIt
    /// didn't create, including an adopted worktree: there is no reliable
    /// way to recover a boundary for
    /// those after the fact, so their task diff is truthfully "unknown",
    /// never guessed via `merge-base`/`HEAD~1`.
    ///
    /// For a stacked task this is also the fork point: `base_commit..<tip>`
    /// is exactly the task's own commits. It changes only when SlashIt
    /// itself moves the branch onto another base, which today is one
    /// operation: restacking an unpublished stacked branch onto the default
    /// branch after its parent was merged there, before its first pull
    /// request is opened (see `commands::pr::restack_onto_landed_parent`).
    /// That sets it to the exact commit the branch was replayed onto, in the
    /// same durable write that sets [`Self::branch_origin`].
    #[serde(default)]
    pub base_commit: Option<String>,

    /// What the task's branch currently starts from, as far as SlashIt
    /// knows: its ancestry and base as of now, not a history of where it
    /// once started. A pull request for the task is opened against what
    /// this names, not against whatever the task's dependencies look like
    /// by then.
    ///
    /// Recorded when SlashIt creates the branch (or resumes one an
    /// unfinished start left), and never inferred later from mutable task
    /// state such as the task's dependencies or the branch's tip. It changes
    /// afterwards only when SlashIt itself deliberately rewrites the
    /// branch's history onto another base: restacking an unpublished
    /// stacked branch onto the default branch its parent was merged into
    /// turns [`BranchOrigin::Stacked`] into [`BranchOrigin::DefaultBase`],
    /// together with [`Self::base_commit`]. Where the branch started before
    /// that is not kept.
    ///
    /// `None` for a branch created before this field existed, or one SlashIt
    /// reattached without creating it: where those started cannot be
    /// recovered after the fact. `None` too for an ordinary branch created
    /// by a version that could not prove its start was on the default base.
    #[serde(default)]
    pub branch_origin: Option<BranchOrigin>,

    /// A restack of this task's already published branch that has begun and
    /// not finished (see `commands::pr::republish`). `None` when none is
    /// under way, which is what every record written before this field
    /// existed deserializes to.
    ///
    /// While it is set, the pull request on GitHub and the remote branch may
    /// still be what they were before the restack: this record is the only
    /// durable statement that the local branch is ahead of them on purpose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_republish: Option<PendingRepublish>,

    /// A destructive worktree cleanup was started for this task and this
    /// process has not yet durably recorded its outcome.
    ///
    /// Deliberately not a `TaskStatus`: it says nothing about how far the work
    /// got, only that `worktree_path` may name a directory a removal is in the
    /// middle of taking apart. The distinction matters because the two facts
    /// have different lifetimes -- a task can be interrupted mid-cleanup out of
    /// any status -- and because a public status would have to be rendered,
    /// filtered and dragged like the others.
    ///
    /// It exists because nothing else on disk can tell the two cases apart. A
    /// removal deletes the checkout's contents depth-first and takes git's
    /// registration down last, so a crash partway through leaves `status`,
    /// `worktree_path`, the directory and the registration all exactly as a
    /// healthy task's would be. The only remaining difference is which files
    /// inside are already gone, and that is unattributable: an agent that ran
    /// `rm -rf` on its own checkout produces the same shape. Writing the
    /// intent down before the subprocess exists is what makes the interrupted
    /// case observable at all.
    ///
    /// `true` at startup means the recorded worktree is untrusted: see
    /// `app_core::build_state_with_paths`, which quarantines the task rather
    /// than adopting or re-removing it.
    #[serde(default)]
    pub cleanup_in_flight: bool,

    /// Last triage of PR review comments. Cached so reopening the modal does
    /// not re-run the LLM, and so post-apply state survives reloads.
    #[serde(default)]
    pub pr_review_plan: Option<PrReviewPlan>,

    /// Milestones nothing else on the record keeps, oldest first: runs, AI
    /// reviews, delivery, moves. Each is appended in the durable write of the
    /// transition it describes. History only -- nothing decides what the
    /// task may do from it. See [`slashit_activity`].
    ///
    /// A task written before this existed has none, and its timeline shows
    /// only what the rest of its record proves. An entry a newer version
    /// wrote is skipped rather than failing the load.
    #[serde(default, deserialize_with = "slashit_activity::lenient", skip_serializing_if = "Vec::is_empty")]
    pub activity: Vec<ActivityEntry>,

    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// `text` with the user's home directory written as `~`, for text the task
/// file keeps that may name a path on this machine.
pub(crate) fn without_home_dir(text: &str) -> String {
    match dirs::home_dir().and_then(|h| h.into_os_string().into_string().ok()) {
        Some(home) => with_home_as_tilde(text, &home),
        None => text.to_string(),
    }
}

/// `text` with every path under `home` starting `~` instead.
///
/// Matched the way Windows compares paths as well as exactly -- ignoring
/// ASCII case, with `/` and `\` alike -- because a tool may write the
/// profile as `C:/Users/me` or `c:\users\me`. Only a whole path component
/// matches, so `/home/me` leaves `/home/meg` alone. A home at a filesystem
/// or drive root is left alone rather than turning every path into `~`.
fn with_home_as_tilde(text: &str, home: &str) -> String {
    let home = home.strip_prefix(r"\\?\").unwrap_or(home).trim_end_matches(['/', '\\']);
    if home.is_empty() || home.len() == 2 && home.ends_with(':') {
        return text.to_string();
    }
    let (bytes, needle) = (text.as_bytes(), home.as_bytes());
    let same = |a: u8, b: u8| a.eq_ignore_ascii_case(&b) || matches!(a, b'/' | b'\\') && matches!(b, b'/' | b'\\');
    let mut out = String::with_capacity(text.len());
    let (mut at, mut copied) = (0, 0);
    // A match starts on `needle`'s first byte and ends on its last, both
    // character boundaries, so the slices below never split a character.
    while at + needle.len() <= bytes.len() {
        let end = at + needle.len();
        let whole = bytes.get(end).is_none_or(|&c| !(c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.')));
        if whole && bytes[at..end].iter().zip(needle).all(|(&a, &b)| same(a, b)) {
            out.push_str(&text[copied..at]);
            out.push('~');
            (at, copied) = (end, end);
        } else {
            at += 1;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// The caller-chosen fields of a new task; everything else starts at its
/// initial value in [`Task::new_backlog`].
#[derive(Debug, Clone)]
pub struct NewTask {
    pub id: Uuid,
    pub project_id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub model: String,
    pub planning_mode: bool,
    pub dependencies: Vec<Uuid>,
    pub category: TaskCategory,
    pub priority: TaskPriority,
    pub complexity: TaskComplexity,
    pub impact: TaskImpact,
    pub security_severity: SecuritySeverity,
    pub github_issue_url: Option<String>,
    pub gitlab_issue_url: Option<String>,
    pub linear_ticket_id: Option<String>,
}

impl Task {
    /// Why nothing but the restack's own recovery may change this task's
    /// branch or checkout right now, or `None`.
    ///
    /// While [`Self::pending_republish`] exists the task is in an exclusive
    /// recovery state: the record is a proof of one exact transaction (the
    /// approved remote tip, the fork point, the target base and the verified
    /// rewritten tip), and anything that advanced the branch would make
    /// resuming or discarding it ambiguous. An agent run, an AI review or
    /// fix, a pull request helper, and pull request creation all ask this
    /// first. Reading the task, and Resume and Discard themselves, do not.
    pub fn republish_pending_refusal(&self) -> Option<String> {
        self.pending_republish.as_ref().map(|_| {
            "A restack of this task's published branch is unfinished, so nothing else may change \
             its branch or checkout until it is resumed or discarded. Resume or discard it in the \
             task's pull request section."
                .to_string()
        })
    }

    /// Return the task to a state work can start from again, keeping the work
    /// it has already produced.
    ///
    /// Phase, progress and any recorded error describe a run that is no longer
    /// happening, so leaving them behind would have the board report a run
    /// nothing is driving. `worktree_path` and `branch_name` deliberately
    /// survive: the next execution reattaches to that branch and continues
    /// from what is already there. Discarding a worktree is an explicit
    /// destructive action, never a side effect of a run ending.
    ///
    /// An approval of the changes under review stops being current too: the
    /// task is going back to work, and whatever the next run commits is not
    /// what was approved. See [`HumanReviewRecord::withdraw_approval`].
    pub fn reset_execution_state(&mut self) {
        self.phase = TaskPhase::Idle;
        self.phase_progress = 0;
        self.overall_progress = 0;
        self.error_message = None;
        self.human_review.withdraw_approval();
    }

    /// A brand-new Backlog task: no checkout, no run, no progress.
    ///
    /// The one place a task's initial state is spelled out, shared by the
    /// desktop command, the CLI handler and the Project Coordinator, so a task
    /// is identical whichever front door created it.
    pub fn new_backlog(existing: &HashMap<Uuid, Task>, new: NewTask) -> Task {
        let now = chrono::Utc::now();
        Task {
            id: new.id,
            project_id: new.project_id,
            title: new.title,
            description: new.description,
            status: TaskStatus::Backlog,
            model: new.model,
            planning_mode: new.planning_mode,
            dependencies: new.dependencies,
            worktree_id: None,
            jj_change_id: None,
            category: new.category,
            priority: new.priority,
            complexity: new.complexity,
            impact: new.impact,
            security_severity: new.security_severity,
            phase: TaskPhase::Idle,
            phase_progress: 0,
            overall_progress: 0,
            subtasks: Vec::new(),
            sequence_number: 0,
            position: Task::next_backlog_position(existing, new.project_id),
            github_issue_url: new.github_issue_url,
            gitlab_issue_url: new.gitlab_issue_url,
            linear_ticket_id: new.linear_ticket_id,
            jira_issue_key: None,
            pr_url: None,
            external_refs: Vec::new(),
            qa_signoff: None,
            human_review: Default::default(),
            stuck_since: None,
            error_message: None,
            worktree_path: None,
            branch_name: None,
            base_commit: None,
            branch_origin: None,
            pending_republish: None,
            cleanup_in_flight: false,
            pr_review_plan: None,
            activity: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// The position a newly created task should take in `project_id`'s
    /// Backlog column: one past the highest position already used there, or
    /// the first slot if the column is empty.
    ///
    /// Shared by every front door that creates a task, so a task created
    /// through the CLI lands at the end of the column like one created in the
    /// app, rather than at a hardcoded position that collides with whatever
    /// is already there.
    pub fn next_backlog_position(existing: &HashMap<Uuid, Task>, project_id: Uuid) -> i32 {
        existing
            .values()
            .filter(|t| t.project_id == project_id && t.status == TaskStatus::Backlog)
            .map(|t| t.position)
            .max()
            .map_or(0, |max| max + 1)
    }

    /// Whether the poller should start this task on its next pass.
    ///
    /// `InProgress` alone does not mean "running": it is also what a task
    /// promoted out of the queue looks like in the instant before an agent
    /// exists for it. The idle phase is what distinguishes the two, because
    /// spawning execution moves the task to a working phase before it spawns
    /// anything.
    ///
    /// A cleanup this or an earlier process started and never recorded the
    /// outcome of leaves the recorded checkout untrustworthy, and an agent
    /// started against it would be working inside a directory a removal may
    /// still be taking apart -- so a quarantined task is never ready, no
    /// matter what its status and phase say.
    ///
    /// This is readiness, not capacity and not queue eligibility: it says
    /// nothing about how many tasks may run at once, and nothing about
    /// whether a `Queue` task may be promoted. The executor and the
    /// regression tests that pin its behavior both call this rather than
    /// each keeping their own copy of the condition, so the two cannot drift
    /// the way they once did.
    pub fn is_ready_to_execute(&self) -> bool {
        self.status == TaskStatus::InProgress
            && self.phase == TaskPhase::Idle
            && !self.cleanup_in_flight
            && self.pending_republish.is_none()
    }

    /// Record a milestone on the task's timeline now, unless it is a
    /// once-only milestone already there. Returns whether it was added.
    pub fn record_activity(&mut self, kind: ActivityKind) -> bool {
        self.record_activity_at(chrono::Utc::now(), kind)
    }

    /// [`Self::record_activity`] for a milestone that happened at `at`.
    ///
    /// A reason is subprocess or agent text, so the home directory in it is
    /// written as `~` here; the timeline masks credentials in it on the way
    /// in (see [`slashit_activity::sanitize`]).
    pub fn record_activity_at(&mut self, at: chrono::DateTime<chrono::Utc>, mut kind: ActivityKind) -> bool {
        match &mut kind {
            ActivityKind::RunFailed { reason, .. }
            | ActivityKind::AiReviewFailed { reason, .. }
            | ActivityKind::AiReviewSkipped { reason, .. }
            | ActivityKind::AiFixFailed { reason, .. }
            | ActivityKind::DeliveryFailed { reason } => *reason = without_home_dir(reason),
            _ => {}
        }
        slashit_activity::record(&mut self.activity, at, kind)
    }

    /// Record that the task moved from `from` to its current status, when
    /// that is a move at all.
    pub fn record_move(&mut self, from: &TaskStatus) {
        if *from != self.status {
            let kind = ActivityKind::Moved { from: from.column(), to: self.status.column() };
            self.record_activity(kind);
        }
    }

    /// Record what a pull request's remote state says happened to it, when
    /// that is a milestone: merged, or closed without merging.
    pub fn record_pr_state(&mut self, number: u32, state: &str) {
        if state.eq_ignore_ascii_case("MERGED") {
            self.record_activity(ActivityKind::PrMerged { number });
        } else if state.eq_ignore_ascii_case("CLOSED") {
            self.record_activity(ActivityKind::PrClosed { number });
        }
    }

    /// The number the task's next coding run gets.
    pub fn next_run(&self) -> u32 {
        slashit_activity::next_run(&self.activity)
    }

    /// The number the task's next AI review gets.
    pub fn next_review(&self) -> u32 {
        slashit_activity::next_review(&self.activity)
    }

    /// Whether the task cannot make progress without the user right now, and
    /// why. The rule is [`slashit_attention::Facts::needs_you`]; this only
    /// copies the task's facts into it.
    ///
    /// `delivery_in_flight` is whether its pull request is being opened at
    /// this moment, which the record cannot say.
    pub fn needs_you(&self, delivery_in_flight: bool) -> Option<AttentionReason> {
        attention::Facts {
            status: match self.status {
                TaskStatus::Error => attention::Status::Error,
                TaskStatus::HumanReview => attention::Status::HumanReview,
                TaskStatus::Backlog
                | TaskStatus::Queue
                | TaskStatus::InProgress
                | TaskStatus::AiReview
                | TaskStatus::PrCreated
                | TaskStatus::Done => attention::Status::Other,
            },
            decision: self.human_review.current_decision().map(|e| match e.decision {
                HumanReviewDecision::Approved => attention::Decision::Approved,
                HumanReviewDecision::ChangesRequested => attention::Decision::ChangesRequested,
            }),
            pr_error_recorded: self.human_review.pr_error.is_some(),
            pr_linked: self.pr_url.is_some() || self.external_refs.iter().any(ExternalRef::is_pr),
            delivery_in_flight,
        }
        .needs_you()
    }
}

/// The durable record of one restack of a published stacked branch, from the
/// moment it is planned until its pull request has been retargeted.
///
/// Everything a retry needs is here, so that no step has to be recovered from
/// incidental observations: the tip the user approved and that the remote must
/// still have for the update to go through (`previous_tip`), the commit the
/// branch is replayed onto (`onto`), and, once the replay is verified and the
/// task's base is recorded, the tip it produced (`rewritten_tip`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingRepublish {
    /// The branch the task was stacked on, as `BranchOrigin::Stacked` names it.
    pub parent_branch: String,
    /// The parent's merged pull request.
    pub parent_pr: u64,
    /// The task's own pull request, which is retargeted last. `None` for a
    /// branch that is on origin with no pull request yet: there is nothing to
    /// retarget, and the pull request is created afterwards against the
    /// default branch.
    #[serde(default)]
    pub pr_number: Option<u64>,
    /// The default branch the parent landed on, which the pull request is
    /// retargeted to.
    pub default_branch: String,
    /// `Task::base_commit` before the restack: the parent's tip the branch
    /// was created at.
    pub fork_point: String,
    /// The branch's tip, local and on the remote, when the restack was
    /// approved. The guarded push expects the remote to be exactly here.
    pub previous_tip: String,
    /// The commit the branch is replayed onto.
    pub onto: String,
    /// The tip the replay produced, set in the same write that moves the
    /// task's `base_commit` to `onto` and its origin to the default branch.
    /// `None` until then.
    #[serde(default)]
    pub rewritten_tip: Option<String>,
}

/// What a task's branch currently starts from, as recorded on
/// [`Task::branch_origin`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BranchOrigin {
    /// Starts from the repository's default base.
    ///
    /// `branch` is the default branch `D` the branch was started from or
    /// replayed onto, as SlashIt resolved it then: an ordinary branch it
    /// creates starts at the exact commit `refs/remotes/origin/<D>` named
    /// (see `worktree::default_base::resolve_default_base`), and a restacked one was
    /// replayed onto the default branch its parent was merged into. Its pull
    /// request targets `D` explicitly.
    ///
    /// `None` is what every record written before the branch was kept
    /// deserializes to, and serializes exactly as those records were
    /// written (`kind = "default_base"` and nothing else). Where such a
    /// branch was started is not re-derived: its pull request targets the
    /// repository's default branch as GitHub reports it, as it always did.
    DefaultBase {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
    /// Created at the tip of a dependency's branch, `parent_branch`, so that
    /// it builds on that work, and not moved off it by SlashIt since. Its
    /// pull request targets `parent_branch` while the parent is still open.
    Stacked { parent_branch: String },
    /// Starts from the project's local base branch `branch`
    /// (`domain::ProjectBase`), at the exact commit `refs/heads/<branch>`
    /// named when the branch was created, because the repository had no
    /// usable remote default branch then.
    ///
    /// Nothing about a remote is claimed: a pull request for it targets
    /// `branch` on `origin` only once `origin` has that branch and it
    /// contains the commit the task started from, so that the pull request
    /// carries only the task's own commits.
    LocalBase { branch: String },
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
    /// The comment text each fix was made from, one entry per comment id.
    /// Empty for every plan saved before it existed; see
    /// [`PrReviewPlan::fixed_content_is_current`] for what that means.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixed_content: Vec<FixedContent>,
}

/// The fingerprint ([`slashit_review_content::fingerprint`]) of the text of
/// comment `comment_id` that a fix agent was given, recorded when the fix
/// succeeded. It is what a later analysis compares the comment's current text
/// with, instead of guessing from timestamps whether it was edited.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FixedContent {
    pub comment_id: u64,
    pub fingerprint: String,
}

impl PrReviewPlan {
    /// Record that a fix for `comment_id` was just made from the text this
    /// plan holds for it. A comment the plan does not hold is not recorded:
    /// nothing is known about what the agent saw, and the entry stays absent
    /// (legacy behavior) rather than guessed.
    pub fn record_fixed_content(&mut self, comment_id: u64) {
        let Some(fingerprint) = self.comments.iter()
            .find(|c| c.id == Some(comment_id))
            .map(PrReviewComment::fingerprint)
        else { return; };
        self.fixed_content.retain(|f| f.comment_id != comment_id);
        self.fixed_content.push(FixedContent { comment_id, fingerprint });
    }

    /// The fingerprint recorded for the text fixed for `comment_id`, if any.
    pub fn fixed_fingerprint(&self, comment_id: u64) -> Option<&str> {
        self.fixed_content.iter()
            .find(|f| f.comment_id == comment_id)
            .map(|f| f.fingerprint.as_str())
    }

    /// Whether the fix recorded for `comment_id` still covers the comment as
    /// this plan holds it, judged against an apply made at `applied_at`.
    ///
    /// With a recorded fingerprint: whether the text is the same, whenever it
    /// was edited. A comment edited and then put back to exactly the text
    /// that was fixed is current again: that fix and its reply already
    /// address it. Without one (a fix recorded before fingerprints existed):
    /// the timestamp rule, and only if the plan was generated at or before
    /// the apply, so that its copy of the comment is what the agent was
    /// given; a plan generated after its apply is unproven and restores
    /// nothing. A comment the plan does not hold is current,
    /// as it always was: there is no text to have changed.
    pub fn fixed_content_is_current(
        &self,
        comment_id: u64,
        applied_at: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        let Some(comment) = self.comments.iter().find(|c| c.id == Some(comment_id)) else {
            return true;
        };
        slashit_review_content::fixed_content_is_current(
            self.fixed_fingerprint(comment_id),
            &comment.body,
            comment.updated_at,
            applied_at,
            self.generated_at <= applied_at,
        )
    }

    /// Derive per-item `fix_done` and `reply_posted` from the persisted
    /// `last_apply`. Used to upgrade plans that pre-date the lifecycle fields
    /// so badges show immediately for items the user already addressed, and
    /// so a re-apply on the same plan correctly skips items whose work landed
    /// in a prior run.
    ///
    /// Only flips flags from `false` to `true` — never undoes user-visible
    /// state. Dry-run results are ignored on purpose. An item whose comment
    /// no longer has the text its fix was made from
    /// ([`PrReviewPlan::fixed_content_is_current`]) gets neither flag back:
    /// the apply fixed and answered the old wording, so the next Apply
    /// processes the comment afresh. `fix_uncommitted` is not touched; it
    /// describes the checkout, not the comment.
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
        for idx in 0..self.items.len() {
            let Some(cid) = self.items[idx].comment_id else { continue; };
            let current = last.fixed_ids.contains(&cid)
                && self.fixed_content_is_current(cid, last.applied_at);
            let item = &mut self.items[idx];
            if current {
                if !item.fix_done {
                    item.fix_done = true;
                }
                // A fix not yet delivered (`fix_uncommitted`) had no reply
                // attempted -- replies wait for delivery -- so its absence
                // from `reply_errors` says nothing about a reply being posted.
                // Neither does it for any fix with a recorded commit: whether
                // its reply was posted is recorded on the item itself, and
                // only a plan from before delivery was tracked needs this
                // inference.
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
    /// GitHub's `author_association` for the comment's author (`OWNER`,
    /// `MEMBER`, `COLLABORATOR`, `CONTRIBUTOR`, `NONE`, ...). `None` when it
    /// was not fetched, which includes every plan saved before it was.
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
    /// The fingerprint of the comment's text: which version of it a fix was
    /// made from. See [`slashit_review_content::fingerprint`].
    pub fn fingerprint(&self) -> String {
        slashit_review_content::fingerprint(&self.body)
    }

    /// Whether a Fix triaged from this comment may start out approved.
    ///
    /// Only the repository's owner, its organization's members and invited
    /// collaborators qualify. Everyone else -- contributors, first-time
    /// contributors, anyone with no association, and apps such as review
    /// bots, which GitHub reports as `NONE` -- can still be read and triaged,
    /// but their Fix items wait for the user to approve them one by one. A
    /// bot is not an exception: what a review bot posts can be steered by
    /// whoever talks to it on the PR. An unknown association is not trusted.
    ///
    /// This decides eligibility for pre-approval only. The comment's text is
    /// untrusted data whoever wrote it.
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
    /// Bookkeeping for the UI and for what an apply still owes, never
    /// evidence: true from the moment this item's fix agent succeeds until an
    /// apply or a sync has seen the fix delivered (see
    /// [`PrReviewItem::fix_commit`] and [`PrReviewItem::fix_effect`]), and set
    /// again if it is no longer. Clearing it proves nothing, and a plan
    /// persisted before commits were recorded has it cleared without any proof
    /// (#96). Whether a reply may say the fix is done is decided only by
    /// delivery as the remote shows it at that moment.
    ///
    /// What is owed follows from the other two fields, not from this flag: a
    /// commit is owed to a fix with a `fix_effect` and no `fix_commit`; a push
    /// is owed to one with a `fix_commit` the remote does not hold; a fix with
    /// neither is unproven and owes nothing (see
    /// [`PrReviewItem::fix_has_no_provenance`]). A commit or push that fails,
    /// is cancelled, or is withheld because another fix agent failed in the
    /// same apply leaves the flag set. It survives an edit to the item's
    /// comment.
    #[serde(default)]
    pub fix_uncommitted: bool,
    /// The commit that carries this item's fix: recorded when an apply's
    /// commit succeeds after the item's fix agent changed the checkout. One
    /// commit may carry several items' fixes. `None` for a fix not yet
    /// committed and for every plan saved before this field existed, which
    /// therefore proves nothing about delivery.
    ///
    /// This is a different question from the one `PrReviewPlan::fixed_content`
    /// answers (#109): that records which version of the comment's text the
    /// fix was made from; this records which commit holds the fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_commit: Option<String>,
    /// What this item's fix agent changed in the checkout: the trees before
    /// and after its run. It is the evidence behind every claim that the fix
    /// is delivered, so it is kept for as long as the item stands behind a
    /// commit: a commit becomes `fix_commit` only if the change is still in
    /// it, and a reply is allowed only if the change is also still in the
    /// remote branch as refreshed at that moment (a later commit may have
    /// overwritten or reverted it, whatever the ancestry says). The change is
    /// found by its content, not its path, so later unrelated edits keep the
    /// proof, including edits to other parts of the same file. An overlapping
    /// edit removes it, and so does one that merely moves the hunk, such as a
    /// line inserted above it: the change must still sit where it was made.
    /// A false negative is acceptable and a false positive is not. The trees
    /// are ordinary unreferenced Git objects, so once Git prunes them the
    /// proof is unavailable and the fix is made again.
    ///
    /// It is dropped when no commit can carry it: a commit or a finding of
    /// nothing to commit weighed it and it was not there, or the remote
    /// branch no longer has it. `None` then, when the agent changed nothing,
    /// and for every plan saved before this existed. Independent of #109's
    /// `fixed_content`, which identifies the comment text, not the change.
    ///
    /// Scope boundary: the effect is everything that changed in the checkout
    /// during the agent's run (a rewritten `Cargo.lock`, a manifest, a
    /// workflow file, generated state), because a legitimate fix may be any
    /// of those. It means "this concrete repository effect was attributed to
    /// this fix agent's run". It does not mean SlashIt checked that the
    /// change satisfies the review comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_effect: Option<FixEffect>,
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
    /// First-person reply text produced by the agent during the fix step.
    /// Used verbatim as the PR reply body — no signature, no labels.
    #[serde(default)]
    pub pr_reply_text: Option<String>,
    /// GitHub comment ID of the reply we posted. Persisted so a future Sync
    /// can PATCH the existing comment instead of duplicating it.
    #[serde(default)]
    pub reply_comment_id: Option<u64>,
}

/// The checkout's tree before and after one fix agent ran.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FixEffect {
    pub before_tree: String,
    pub after_tree: String,
}

impl PrReviewItem {
    /// A fix recorded as done with no change on record to prove it by, with
    /// or without a commit named: every fix a build from before #96 saved, an
    /// agent that changed nothing, and a fix whose change was found missing
    /// from the commit that took the checkout. Nothing can prove it reached
    /// the pull request, so it never gets a reply; an Apply makes it again
    /// once, rather than leaving it stuck.
    pub fn fix_has_no_provenance(&self) -> bool {
        self.fix_done && self.fix_effect.is_none()
    }
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
    /// Set when at least one fix was applied but the branch did not reach
    /// the remote as it should have: the fixes could not be committed (and
    /// so nothing was pushed), the apply was cancelled before committing, or
    /// `auto_push=true` and the push itself failed. The fixes are still on
    /// disk; this records why.
    #[serde(default)]
    pub push_error: Option<String>,
    /// Whether this apply ran with `auto_reply=true` — `Some(true)`/`Some(false)`
    /// for any apply recorded since this field existed, `None` for a result
    /// persisted before it did. Needed to distinguish "reply attempted and
    /// succeeded" from "reply intentionally never attempted" when backfilling
    /// `reply_posted` from `fixed_ids` — collapsing the missing-historical-value
    /// case to `false` would make backfill treat "we don't actually know" the
    /// same as "definitely not attempted", when in fact a genuinely old apply
    /// may well have posted replies. `replies_posted > 0` cannot resolve that
    /// ambiguity either: with multiple fixed items it does not say *which*
    /// item's reply succeeded, so backfill must not use it to flip any
    /// individual item's `reply_posted`. The unknown case is left alone here;
    /// `Sync` is the path that may resolve it, using concrete per-comment
    /// evidence (`in_reply_to_id`) rather than a guess.
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

impl TaskStatus {
    /// The column, as the activity timeline names it.
    pub fn column(&self) -> ActivityColumn {
        match self {
            Self::Backlog => ActivityColumn::Backlog,
            Self::Queue => ActivityColumn::Queue,
            Self::InProgress => ActivityColumn::InProgress,
            Self::AiReview => ActivityColumn::AiReview,
            Self::HumanReview => ActivityColumn::HumanReview,
            Self::Done => ActivityColumn::Done,
            Self::PrCreated => ActivityColumn::PrCreated,
            Self::Error => ActivityColumn::Error,
        }
    }
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

/// What people decided about a task at Human Review, in the order they
/// decided it.
///
/// A decision is about the changes a run produced, so each one is tied to the
/// Human Review *arrival* it was made in: [`Self::arrivals`] counts the times
/// the executor has carried the task into Human Review after a run, and an
/// entry records the count at the moment it was made. The current decision is
/// therefore derived, never stored: it is the last entry made in the current
/// arrival, if any. A new run that reaches Human Review again starts a new
/// arrival, so an approval of the previous changes is not mistaken for an
/// approval of these.
///
/// Approval is not delivery. It never moves the task to another column,
/// never merges anything and never touches the checkout; opening a pull
/// request is a separate step whose failure is recorded in
/// [`Self::pr_error`] without undoing the approval.
///
/// A task written before this existed has no `human_review` table, or one in
/// an earlier, never-populated shape whose keys are ignored, and loads as an
/// empty record: no arrivals, no decisions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HumanReviewRecord {
    /// How many times a run has carried the task into Human Review, plus
    /// the times an approval was withdrawn because the task went back to
    /// work (see [`Self::withdraw_approval`]).
    #[serde(default)]
    pub arrivals: u32,
    /// Every decision, oldest first.
    #[serde(default)]
    pub entries: Vec<HumanReviewEntry>,
    /// Why the last attempt to open a pull request for the approved changes
    /// failed. Cleared when a later attempt succeeds, and when the task goes
    /// back for another run.
    #[serde(default)]
    pub pr_error: Option<String>,
}

/// One decision a person made at Human Review.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HumanReviewEntry {
    /// Position in the task's review history, starting at 1.
    pub sequence: u32,
    /// The [`HumanReviewRecord::arrivals`] count when the decision was made.
    pub arrival: u32,
    pub decision: HumanReviewDecision,
    /// What the reviewer asked for. Present exactly when
    /// `decision` is [`HumanReviewDecision::ChangesRequested`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// The decision made in the current arrival, if one was.
    pub fn current_decision(&self) -> Option<&HumanReviewEntry> {
        self.entries.last().filter(|e| e.arrival == self.arrivals)
    }

    pub fn is_approved(&self) -> bool {
        self.current_decision()
            .is_some_and(|e| e.decision == HumanReviewDecision::Approved)
    }

    /// The feedback the next run has to address: every change request made
    /// in the current arrival, oldest first. Empty once a run has carried the
    /// task back into Human Review, because that run was the answer to it.
    pub fn pending_feedback(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|e| e.arrival == self.arrivals)
            .filter(|e| e.decision == HumanReviewDecision::ChangesRequested)
            .filter_map(|e| e.feedback.as_deref())
            .collect()
    }

    /// Record that a run carried the task into Human Review again.
    pub fn record_arrival(&mut self) {
        self.arrivals = self.arrivals.saturating_add(1);
        self.pr_error = None;
    }

    /// The task is going back to work after its changes were approved, so
    /// the approval no longer describes what the branch will hold. Closing
    /// the current arrival is what retires it: the entry stays in the
    /// history, and nothing is current until a run brings the task back.
    ///
    /// A change request is left alone. It is meant for the run the task is
    /// going back to, which [`Self::pending_feedback`] reads.
    pub fn withdraw_approval(&mut self) {
        if self.is_approved() {
            self.record_arrival();
        }
    }

    /// Append a decision made now, in the current arrival.
    pub fn push(
        &mut self,
        decision: HumanReviewDecision,
        feedback: Option<String>,
        decided_at: chrono::DateTime<chrono::Utc>,
    ) {
        let sequence = self.entries.last().map_or(1, |e| e.sequence.saturating_add(1));
        self.entries.push(HumanReviewEntry {
            sequence,
            arrival: self.arrivals,
            decision,
            feedback,
            decided_at,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pre-rename persisted task record: `workspace_id` (the old field
    /// name), no `worktree_id` at all. Loaded by `Storage::load_project_tasks`
    /// / `load_all_tasks` from an on-disk `tasks.toml` written before the
    /// `worktree_id` rename shipped.
    const LEGACY_TASK_TOML: &str = r#"
        id = "11111111-1111-1111-1111-111111111111"
        project_id = "22222222-2222-2222-2222-222222222222"
        title = "Legacy task"
        status = "backlog"
        model = "test-model"
        planning_mode = false
        dependencies = []
        workspace_id = "33333333-3333-3333-3333-333333333333"
        category = "feature"
        priority = "medium"
        complexity = "moderate"
        impact = "medium"
        security_severity = "none"
        phase = "planning"
        phase_progress = 0
        overall_progress = 0
        subtasks = []
        sequence_number = 0
        created_at = "2024-01-01T00:00:00Z"
        updated_at = "2024-01-01T00:00:00Z"
    "#;

    /// A task the poller may start: `InProgress`, idle phase, no cleanup
    /// pending.
    fn startable_task() -> Task {
        let mut task = crate::test_helpers::create_test_task_full(
            "Startable",
            Uuid::new_v4(),
            TaskStatus::InProgress,
            0,
        );
        task.phase = TaskPhase::Idle;
        task.cleanup_in_flight = false;
        task
    }

    #[test]
    fn is_ready_to_execute_is_true_for_an_ordinary_in_progress_idle_task() {
        assert!(startable_task().is_ready_to_execute());
    }

    #[test]
    fn is_ready_to_execute_is_false_for_a_queued_or_completed_task() {
        let mut queued = startable_task();
        queued.status = TaskStatus::Queue;
        assert!(!queued.is_ready_to_execute());

        let mut done = startable_task();
        done.status = TaskStatus::Done;
        assert!(!done.is_ready_to_execute());
    }

    #[test]
    fn is_ready_to_execute_is_false_while_an_earlier_phase_has_not_reached_idle() {
        let mut mid_run = startable_task();
        mid_run.phase = TaskPhase::Coding;
        assert!(!mid_run.is_ready_to_execute());
    }

    /// The mutation this unit closed: a task whose interrupted cleanup was
    /// never resolved must never be reported ready, even though its status
    /// and phase alone look exactly like an ordinary startable task. Before
    /// this fix, the regression test that was supposed to pin this
    /// (`executor_would_start` in `task_terminal_cleanup_lifecycle.rs`)
    /// duplicated the condition by hand and had already drifted -- it omitted
    /// this exact clause and so could not have caught its own removal. This
    /// test calls the production predicate directly, so it cannot drift the
    /// same way: deleting the `!cleanup_in_flight` clause from
    /// `is_ready_to_execute` fails this test.
    #[test]
    fn is_ready_to_execute_is_false_for_a_quarantined_task_even_though_status_and_phase_say_go() {
        let mut quarantined = startable_task();
        quarantined.cleanup_in_flight = true;
        assert!(
            !quarantined.is_ready_to_execute(),
            "a task with an unresolved cleanup must never be reported ready to execute"
        );
    }

    /// A task whose published branch has an unfinished restack is not started:
    /// the restack owns it (see `Task::republish_pending_refusal`).
    #[test]
    fn is_ready_to_execute_is_false_while_a_restack_of_its_branch_is_pending() {
        let mut pending = startable_task();
        pending.pending_republish = Some(PendingRepublish {
            parent_branch: "p".to_string(),
            parent_pr: 1,
            pr_number: Some(2),
            default_branch: "main".to_string(),
            fork_point: "a".repeat(40),
            previous_tip: "b".repeat(40),
            onto: "c".repeat(40),
            rewritten_tip: None,
        });
        assert!(!pending.is_ready_to_execute());
        assert!(pending.republish_pending_refusal().is_some());
        assert!(startable_task().republish_pending_refusal().is_none());
    }

    #[test]
    fn task_deserializes_legacy_workspace_id_into_worktree_id() {
        let task: Task = toml::from_str(LEGACY_TASK_TOML)
            .expect("legacy `workspace_id` record must still deserialize via the alias");
        assert_eq!(
            task.worktree_id,
            Some(Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap()),
            "worktree_id should be populated from the legacy `workspace_id` key",
        );
    }

    #[test]
    fn task_deserializes_without_workspace_id_or_worktree_id() {
        // Belt-and-suspenders: a record with neither key present (the normal
        // case for any task that never had a worktree) must still load, with
        // worktree_id defaulting to None.
        let toml_str = LEGACY_TASK_TOML.replace(
            "workspace_id = \"33333333-3333-3333-3333-333333333333\"\n",
            "",
        );
        let task: Task = toml::from_str(&toml_str)
            .expect("record with no worktree reference at all must still deserialize");
        assert_eq!(task.worktree_id, None);
    }

    /// A board written by a build that predates `cleanup_in_flight` loads with
    /// no interrupted cleanup recorded.
    ///
    /// The default has to be `false` and not merely present: `true` is the
    /// quarantine, so a default that leaned the other way would hold back every
    /// task on every existing installation the first time it started.
    #[test]
    fn a_task_written_before_the_field_existed_has_no_cleanup_in_flight() {
        assert!(
            !LEGACY_TASK_TOML.contains("cleanup_in_flight"),
            "this fixture only proves anything while it predates the field"
        );
        let task: Task = toml::from_str(LEGACY_TASK_TOML)
            .expect("a record written before the field existed must still deserialize");
        assert!(!task.cleanup_in_flight);
    }

    /// A board written before `branch_origin` existed loads with no origin
    /// recorded, rather than failing or claiming one it cannot know.
    #[test]
    fn a_task_written_before_branch_origin_existed_has_none() {
        assert!(
            !LEGACY_TASK_TOML.contains("branch_origin"),
            "this fixture only proves anything while it predates the field"
        );
        let task: Task = toml::from_str(LEGACY_TASK_TOML)
            .expect("a record written before the field existed must still deserialize");
        assert_eq!(task.branch_origin, None);

        let json = serde_json::to_value(&task).unwrap();
        let mut object = json.as_object().unwrap().clone();
        object.remove("branch_origin");
        let task: Task = serde_json::from_value(serde_json::Value::Object(object))
            .expect("a JSON task without the field must still deserialize");
        assert_eq!(task.branch_origin, None);
    }

    /// A recorded branch is read back and written out exactly as it was,
    /// whatever form an earlier version gave it, and a record with none
    /// stays without one: the recorded name is what a task is reattached by,
    /// so loading a board never renames a task's branch or gives it one.
    #[test]
    fn a_recorded_branch_of_any_form_reads_and_writes_unchanged() {
        assert!(
            !LEGACY_TASK_TOML.contains("branch_name"),
            "this fixture only proves anything while it records no branch"
        );
        let task: Task = toml::from_str(LEGACY_TASK_TOML).expect("a record with no branch");
        assert_eq!(task.branch_name, None);

        for branch in [
            "task-11111111",
            "task-11111111-1111-1111-1111-111111111111",
            "feature/login",
        ] {
            let toml_text = format!("{LEGACY_TASK_TOML}\nbranch_name = \"{branch}\"\n");
            let task: Task = toml::from_str(&toml_text).expect(branch);
            assert_eq!(task.branch_name.as_deref(), Some(branch));
            let written = toml::to_string(&task).unwrap();
            let reread: Task = toml::from_str(&written).expect(branch);
            assert_eq!(reread.branch_name.as_deref(), Some(branch));
        }
    }

    /// A record written before the default branch was kept, in TOML and
    /// in JSON, reads as a default base with no branch; and such an origin
    /// is written back exactly as it was read, so a board file an older
    /// version shares is not rewritten into something it cannot parse.
    #[test]
    fn a_legacy_default_base_reads_and_writes_as_it_always_did() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Holder {
            origin: BranchOrigin,
        }
        let legacy = BranchOrigin::DefaultBase { branch: None };

        let toml_text = "[origin]\nkind = \"default_base\"\n";
        let read: Holder = toml::from_str(toml_text).expect(toml_text);
        assert_eq!(read.origin, legacy);
        assert_eq!(toml::to_string(&read).unwrap(), toml_text);

        let json_text = r#"{"origin":{"kind":"default_base"}}"#;
        let read: Holder = serde_json::from_str(json_text).expect(json_text);
        assert_eq!(read.origin, legacy);
        assert_eq!(serde_json::to_string(&read).unwrap(), json_text);

        let resolved = Holder {
            origin: BranchOrigin::DefaultBase { branch: Some("main".to_string()) },
        };
        assert_eq!(
            serde_json::to_string(&resolved).unwrap(),
            r#"{"origin":{"kind":"default_base","branch":"main"}}"#
        );
        let toml_text = toml::to_string(&resolved).unwrap();
        assert_eq!(toml_text, "[origin]\nkind = \"default_base\"\nbranch = \"main\"\n");
        assert_eq!(toml::from_str::<Holder>(&toml_text).unwrap(), resolved);
    }

    /// A board written before decisions were recorded -- with no
    /// `human_review` at all, or with the earlier shape of that table that
    /// nothing ever populated -- loads as a task nobody has reviewed yet.
    #[test]
    fn a_task_written_before_review_decisions_existed_has_an_empty_review_record() {
        assert!(!LEGACY_TASK_TOML.contains("human_review"));
        let task: Task = toml::from_str(LEGACY_TASK_TOML).expect("no human_review table");
        assert_eq!(task.human_review, HumanReviewRecord::default());

        let earlier_shape = format!(
            "{LEGACY_TASK_TOML}\n[human_review]\napproved = true\napprover = \"someone\"\nfeedback = \"old\"\n"
        );
        let task: Task = toml::from_str(&earlier_shape).expect("the earlier, unused shape");
        assert_eq!(task.human_review, HumanReviewRecord::default());
        assert!(task.human_review.current_decision().is_none());
        assert!(task.human_review.pending_feedback().is_empty());
    }

    #[test]
    fn review_history_round_trips_through_the_task_file_and_ipc() {
        #[derive(Serialize, Deserialize)]
        struct File {
            tasks: Vec<Task>,
        }
        let mut task = startable_task();
        task.human_review.record_arrival();
        task.human_review.push(
            HumanReviewDecision::ChangesRequested,
            Some("handle \"quotes\"\nand newlines".to_string()),
            chrono::Utc::now(),
        );
        task.human_review.record_arrival();
        task.human_review.push(HumanReviewDecision::Approved, None, chrono::Utc::now());
        task.human_review.pr_error = Some("gh failed".to_string());

        let toml_text = toml::to_string_pretty(&File { tasks: vec![task.clone()] }).unwrap();
        let loaded: File = toml::from_str(&toml_text).expect(&toml_text);
        assert_eq!(loaded.tasks[0].human_review, task.human_review, "{toml_text}");

        let json = serde_json::to_string(&task).unwrap();
        let loaded: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.human_review, task.human_review);
    }

    #[test]
    fn the_current_decision_belongs_to_the_current_arrival_only() {
        let mut review = HumanReviewRecord::default();
        review.record_arrival();
        assert!(review.current_decision().is_none());

        review.push(HumanReviewDecision::ChangesRequested, Some("fix it".into()), chrono::Utc::now());
        assert_eq!(review.pending_feedback(), vec!["fix it"]);
        assert!(!review.is_approved());

        // The rerun that answered the request brings the task back.
        review.record_arrival();
        assert!(review.current_decision().is_none());
        assert!(review.pending_feedback().is_empty());

        review.push(HumanReviewDecision::Approved, None, chrono::Utc::now());
        assert!(review.is_approved());
        assert_eq!(
            review.entries.iter().map(|e| (e.sequence, e.arrival)).collect::<Vec<_>>(),
            vec![(1, 1), (2, 2)]
        );

        // An approval never carries over to changes a later run produced.
        review.pr_error = Some("stale".into());
        review.record_arrival();
        assert!(!review.is_approved());
        assert_eq!(review.pr_error, None);
    }

    /// A task a run has just carried into Human Review.
    fn arrived_in_review() -> Task {
        let mut task = startable_task();
        task.status = TaskStatus::HumanReview;
        task.human_review.record_arrival();
        task
    }

    #[test]
    fn attention_follows_the_review_through_every_decision() {
        let mut task = arrived_in_review();
        assert_eq!(task.needs_you(false), Some(AttentionReason::Review));

        // Request Changes records the request and queues the task in one write.
        task.human_review.push(HumanReviewDecision::ChangesRequested, Some("again".into()), chrono::Utc::now());
        task.status = TaskStatus::Queue;
        assert_eq!(task.needs_you(false), None, "queued");
        task.status = TaskStatus::InProgress;
        assert_eq!(task.needs_you(false), None, "running");
        task.status = TaskStatus::AiReview;
        assert_eq!(task.needs_you(false), None, "under AI review");

        // The next run brings it back: a new arrival, undecided again.
        task.status = TaskStatus::HumanReview;
        task.human_review.record_arrival();
        assert_eq!(task.needs_you(false), Some(AttentionReason::Review));

        task.human_review.push(HumanReviewDecision::Approved, None, chrono::Utc::now());
        assert_eq!(task.needs_you(false), None, "approved, nothing failed");

        task.human_review.pr_error = Some("gh: HTTP 422".into());
        assert_eq!(task.needs_you(false), Some(AttentionReason::PrNotCreated));
        assert_eq!(task.needs_you(true), None, "while a retry is opening the pull request");

        task.pr_url = Some("https://github.com/o/r/pull/1".into());
        assert_eq!(task.needs_you(false), None, "a linked pull request outranks the old failure");
    }

    /// A change request made on a task that was then moved back into Human
    /// Review by hand is not an undecided review.
    #[test]
    fn human_review_with_changes_requested_is_not_an_undecided_review() {
        let mut task = arrived_in_review();
        task.human_review.push(HumanReviewDecision::ChangesRequested, Some("again".into()), chrono::Utc::now());
        assert_eq!(task.needs_you(false), None);
    }

    #[test]
    fn only_error_among_the_other_statuses_needs_the_user() {
        for status in [
            TaskStatus::Backlog,
            TaskStatus::Queue,
            TaskStatus::InProgress,
            TaskStatus::AiReview,
            TaskStatus::PrCreated,
            TaskStatus::Done,
        ] {
            let mut task = startable_task();
            task.status = status.clone();
            assert_eq!(task.needs_you(false), None, "{status:?}");
        }
        let mut failed = startable_task();
        failed.status = TaskStatus::Error;
        assert_eq!(failed.needs_you(false), Some(AttentionReason::Failed));
    }

    /// Nothing about attention is stored, so a restart derives the same
    /// answers from the task file alone.
    #[test]
    fn attention_is_derived_again_from_the_task_file_after_a_restart() {
        #[derive(Serialize, Deserialize)]
        struct File {
            tasks: Vec<Task>,
        }
        let mut failed = startable_task();
        failed.status = TaskStatus::Error;
        let undecided = arrived_in_review();
        let mut pr_failed = arrived_in_review();
        pr_failed.human_review.push(HumanReviewDecision::Approved, None, chrono::Utc::now());
        pr_failed.human_review.pr_error = Some("gh failed".into());

        let tasks = vec![failed, undecided, pr_failed];
        let toml_text = toml::to_string_pretty(&File { tasks: tasks.clone() }).unwrap();
        assert!(!toml_text.contains("attention") && !toml_text.contains("needs_you"), "{toml_text}");
        let loaded: File = toml::from_str(&toml_text).unwrap();
        let reasons: Vec<_> = loaded.tasks.iter().map(|t| t.needs_you(false)).collect();
        assert_eq!(
            reasons,
            vec![
                Some(AttentionReason::Failed),
                Some(AttentionReason::Review),
                Some(AttentionReason::PrNotCreated)
            ]
        );
    }

    /// A board written before activity was recorded loads with none, and is
    /// written back without an `activity` key until something happens.
    #[test]
    fn a_task_written_before_activity_existed_loads_with_none() {
        assert!(!LEGACY_TASK_TOML.contains("activity"));
        let task: Task = toml::from_str(LEGACY_TASK_TOML).expect("no activity");
        assert!(task.activity.is_empty());
        assert!(!toml::to_string(&task).unwrap().contains("activity"));
    }

    /// Recorded milestones are what the task file holds, and read back the
    /// same after a restart, in TOML and over IPC.
    #[test]
    fn a_home_directory_is_found_however_the_path_is_written() {
        for (home, text, expected) in [
            ("/home/me", "in /home/me/src and /home/meg/x", "in ~/src and /home/meg/x"),
            ("/home/me/", "cd /home/me", "cd ~"),
            (r"C:\Users\Me", r"at C:\Users\Me\repo", r"at ~\repo"),
            (r"C:\Users\Me", "at C:/Users/Me/repo and c:\\users\\me\\x", "at ~/repo and ~\\x"),
            (r"\\?\C:\Users\Me", r"at C:\Users\Me\repo", r"at ~\repo"),
            (r"C:\", r"C:\Program Files and ABC:", r"C:\Program Files and ABC:"),
            ("/", "/etc/hosts", "/etc/hosts"),
            ("/home/mé", "in /home/mé/ü", "in ~/ü"),
        ] {
            assert_eq!(with_home_as_tilde(text, home), expected, "{home}: {text}");
        }
    }

    /// A failure reason names no path under the user's home and keeps no
    /// credential, whichever producer wrote it.
    #[test]
    fn failure_reasons_do_not_record_the_home_directory_or_credentials() {
        let Some(home) = dirs::home_dir()
            .and_then(|h| h.into_os_string().into_string().ok())
            .map(|h| h.trim_end_matches(['/', '\\']).to_string())
            .filter(|h| h.contains(['/', '\\']))
        else {
            return;
        };
        let mut task = crate::test_helpers::create_test_task("t");
        task.record_activity(ActivityKind::DeliveryFailed {
            reason: format!(
                "git push failed in {home}/.local/share/slashit/worktrees/t: \
                 https://oauth2:glpat-0123456789abcdefghij@gitlab.com/o/r.git"
            ),
        });
        assert_eq!(
            task.activity[0].kind,
            ActivityKind::DeliveryFailed {
                reason: "git push failed in ~/.local/share/slashit/worktrees/t: https://***@gitlab.com/o/r.git".into()
            }
        );
    }

    #[test]
    fn activity_round_trips_through_the_task_file_and_ipc() {
        #[derive(Serialize, Deserialize)]
        struct File {
            tasks: Vec<Task>,
        }
        let mut task = startable_task();
        let run = task.next_run();
        task.record_activity(ActivityKind::RunStarted { run, addressing_feedback: false });
        task.record_activity(ActivityKind::RunFailed { run: Some(run), reason: "exit 1".into() });
        let from = task.status.clone();
        task.status = TaskStatus::Queue;
        task.record_move(&from);
        assert_eq!(task.next_run(), 2);

        let toml_text = toml::to_string_pretty(&File { tasks: vec![task.clone()] }).unwrap();
        let loaded: File = toml::from_str(&toml_text).expect(&toml_text);
        assert_eq!(loaded.tasks[0].activity, task.activity, "{toml_text}");
        let json = serde_json::to_string(&task).unwrap();
        let loaded: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.activity, task.activity);
    }

    #[test]
    fn a_move_is_recorded_only_when_the_column_changes() {
        let mut task = startable_task();
        task.record_move(&TaskStatus::InProgress);
        assert!(task.activity.is_empty());
        task.status = TaskStatus::Queue;
        task.record_move(&TaskStatus::Error);
        assert_eq!(
            task.activity[0].kind,
            ActivityKind::Moved { from: ActivityColumn::Error, to: ActivityColumn::Queue }
        );
    }

    /// A merged or closed pull request is a milestone once, however often
    /// its state is observed; an open one is none.
    #[test]
    fn pull_request_states_are_recorded_once() {
        let mut task = startable_task();
        for _ in 0..3 {
            task.record_pr_state(7, "OPEN");
            task.record_pr_state(7, "MERGED");
            task.record_pr_state(7, "merged");
        }
        assert_eq!(task.activity.len(), 1);
        assert_eq!(task.activity[0].kind, ActivityKind::PrMerged { number: 7 });
    }

    /// Every origin survives the task file (TOML) and IPC (JSON) unchanged.
    #[test]
    fn branch_origin_round_trips_through_toml_and_json() {
        #[derive(Serialize, Deserialize)]
        struct File {
            tasks: Vec<Task>,
        }
        for origin in [
            BranchOrigin::DefaultBase { branch: None },
            BranchOrigin::DefaultBase { branch: Some("main".to_string()) },
            BranchOrigin::Stacked { parent_branch: "task-parent".to_string() },
            BranchOrigin::LocalBase { branch: "trunk".to_string() },
        ] {
            let mut task = startable_task();
            task.branch_origin = Some(origin.clone());

            let toml_text = toml::to_string_pretty(&File { tasks: vec![task.clone()] }).unwrap();
            let loaded: File = toml::from_str(&toml_text).expect(&toml_text);
            assert_eq!(loaded.tasks[0].branch_origin.as_ref(), Some(&origin), "{toml_text}");

            let json = serde_json::to_string(&task).unwrap();
            let loaded: Task = serde_json::from_str(&json).unwrap();
            assert_eq!(loaded.branch_origin.as_ref(), Some(&origin), "{json}");
        }
    }
}


