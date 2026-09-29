//! What happened to a task, in the order it happened.
//!
//! A task's timeline is assembled from two kinds of source, and never from
//! anything else:
//!
//! - facts the task record already keeps durably for its own reasons -- when
//!   it was created, and every Human Review decision with its time and
//!   feedback. These are read, never copied, so the timeline cannot disagree
//!   with the record that decides what the task does next.
//! - milestones nothing else keeps: runs starting and ending, AI reviews and
//!   their fixes, pull request delivery, a person moving the task. The
//!   backend appends an [`Entry`] for each in the same durable write as the
//!   transition it describes, so a milestone is on the timeline exactly when
//!   the transition is on disk.
//!
//! The timeline is history, not authority. Nothing decides what a task may do
//! from these entries; that stays with its status, phase and review record.
//!
//! Raw agent output is never an entry. The one kind of agent activity kept is
//! a tool call, reduced by the backend to the tool's name and a short detail
//! (a command's first line, a path relative to the task's checkout), with
//! repeats compacted and each run's share capped. Every detail and reason is
//! [`sanitize`]d on the way in.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mod sanitize;
pub use sanitize::{redact, sanitize, MASK};

/// The most entries one task keeps. When a new entry would exceed it, tool
/// calls are dropped first, oldest first, and milestones only when no tool
/// call is left.
pub const MAX_ENTRIES: usize = 200;

/// The most distinct tool-call entries one run keeps. Further calls are
/// counted in a single [`Kind::ToolsOmitted`] entry for the run.
pub const MAX_TOOL_ENTRIES_PER_RUN: usize = 20;

/// The longest reason (a failure, an AI review that could not run) an entry
/// keeps, in characters. The full text stays where it always was, on the
/// task's error or review record.
pub const MAX_REASON_CHARS: usize = 300;

/// The longest detail a tool call keeps, in characters.
pub const MAX_DETAIL_CHARS: usize = 120;

/// A task's column, as the timeline names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Backlog,
    Queue,
    InProgress,
    AiReview,
    HumanReview,
    Done,
    PrCreated,
    Error,
}

impl Column {
    pub fn label(self) -> &'static str {
        match self {
            Self::Backlog => "Backlog",
            Self::Queue => "Queue",
            Self::InProgress => "In Progress",
            Self::AiReview => "AI Review",
            Self::HumanReview => "Human Review",
            Self::Done => "Done",
            Self::PrCreated => "PR Created",
            Self::Error => "Failed",
        }
    }
}

/// One recorded milestone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Position in the task's history, starting at 1 and never reused, so two
    /// entries recorded at the same instant still have a fixed order.
    pub seq: u32,
    pub at: DateTime<Utc>,
    pub kind: Kind,
}

/// What happened. `run` numbers a task's coding runs and `review` its AI
/// reviews, each starting at 1, so a retry or a second review cycle reads as
/// a new one rather than a repeat of the last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    /// A person moved the task: Start, Retry, Add to Queue, a drag, Move to,
    /// or a close. Also written when a task reaches Done by any route.
    Moved { from: Column, to: Column },
    /// A coding run began. `addressing_feedback` is whether it started with
    /// Human Review change requests to answer.
    RunStarted {
        run: u32,
        #[serde(default)]
        addressing_feedback: bool,
    },
    /// The agent called a tool, `count` times in a row with the same detail.
    ToolUsed {
        run: u32,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default = "one")]
        count: u32,
    },
    /// Tool calls in `run` past [`MAX_TOOL_ENTRIES_PER_RUN`].
    ToolsOmitted { run: u32, count: u32 },
    /// The run finished and its work was committed.
    RunCompleted { run: u32 },
    /// The run failed, or with `run: None`, the task failed before a run
    /// could start.
    RunFailed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<u32>,
        reason: String,
    },
    /// A person stopped the task while it was in `from`.
    Stopped { from: Column },
    /// SlashIt started with the task left in `from` by an earlier session --
    /// running, or waiting to run or be reviewed -- and put it back in the
    /// queue. Says nothing about whether a run had begun.
    Interrupted { from: Column },
    AiReviewStarted { review: u32 },
    AiReviewApproved { review: u32 },
    /// The reviewer asked for changes; `issues` is how many it listed.
    AiReviewChangesRequested { review: u32, issues: u32 },
    /// The reviewer could not produce a verdict.
    AiReviewFailed { review: u32, reason: String },
    /// No AI review ran for these changes.
    AiReviewSkipped { review: u32, reason: String },
    AiFixStarted { review: u32 },
    AiFixApplied { review: u32 },
    /// The fix agent finished without changing any file.
    AiFixUnchanged { review: u32 },
    AiFixFailed { review: u32, reason: String },
    /// A run carried the task into Human Review; `arrival` is the Human
    /// Review record's arrival count after it did.
    ReadyForReview { arrival: u32 },
    /// Opening the pull request for approved changes failed.
    DeliveryFailed { reason: String },
    /// A pull request was recorded on the task: one SlashIt opened, or an
    /// open one it found for the task's branch.
    PrLinked {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        number: Option<u32>,
    },
    PrMerged { number: u32 },
    PrClosed { number: u32 },
}

fn one() -> u32 {
    1
}

impl Kind {
    /// What makes this milestone the same one if it is recorded again, for
    /// kinds that can only happen once: a run ends once, a pull request is
    /// linked once. `None` for kinds that truthfully repeat, such as a person
    /// retrying or a delivery failing again.
    fn identity(&self) -> Option<Identity<'_>> {
        Some(match self {
            Self::RunStarted { run, .. } => Identity::RunStarted(*run),
            Self::RunCompleted { run } | Self::RunFailed { run: Some(run), .. } => {
                Identity::RunEnded(*run)
            }
            Self::AiReviewStarted { review } => Identity::ReviewStarted(*review),
            Self::AiReviewApproved { review }
            | Self::AiReviewChangesRequested { review, .. }
            | Self::AiReviewFailed { review, .. }
            | Self::AiReviewSkipped { review, .. } => Identity::ReviewVerdict(*review),
            Self::AiFixStarted { review } => Identity::FixStarted(*review),
            Self::AiFixApplied { review } | Self::AiFixUnchanged { review } | Self::AiFixFailed { review, .. } => {
                Identity::FixEnded(*review)
            }
            Self::ReadyForReview { arrival } => Identity::Arrival(*arrival),
            Self::PrLinked { url, .. } => Identity::PrLinked(url),
            Self::PrMerged { number } => Identity::PrMerged(*number),
            Self::PrClosed { number } => Identity::PrClosed(*number),
            Self::Moved { .. }
            | Self::ToolUsed { .. }
            | Self::ToolsOmitted { .. }
            | Self::RunFailed { run: None, .. }
            | Self::Stopped { .. }
            | Self::Interrupted { .. }
            | Self::DeliveryFailed { .. } => return None,
        })
    }

    fn is_tool(&self) -> bool {
        matches!(self, Self::ToolUsed { .. } | Self::ToolsOmitted { .. })
    }
}

#[derive(PartialEq, Eq)]
enum Identity<'a> {
    RunStarted(u32),
    RunEnded(u32),
    ReviewStarted(u32),
    ReviewVerdict(u32),
    FixStarted(u32),
    FixEnded(u32),
    Arrival(u32),
    PrLinked(&'a str),
    PrMerged(u32),
    PrClosed(u32),
}

/// Append `kind` at `at`, unless it is a once-only milestone already
/// recorded. Returns whether anything was added.
///
/// Reasons, tool details and pull request URLs are [`sanitize`]d here, and
/// reasons cut to one bounded line, so no caller can store a credential or a
/// transcript by passing one in.
pub fn record(entries: &mut Vec<Entry>, at: DateTime<Utc>, mut kind: Kind) -> bool {
    sanitize_text(&mut kind);
    if let Some(identity) = kind.identity() {
        if entries.iter().any(|e| e.kind.identity().as_ref() == Some(&identity)) {
            return false;
        }
    }
    push(entries, at, kind);
    true
}

/// Record one tool call in `run`: a repeat of the run's last call (same tool,
/// same detail) adds to its count, and calls past the run's cap are counted
/// rather than listed. The detail is [`sanitize`]d first.
pub fn record_tool(entries: &mut Vec<Entry>, at: DateTime<Utc>, run: u32, tool: &str, detail: Option<&str>) {
    let detail = detail.map(|d| sanitize(d, MAX_DETAIL_CHARS)).filter(|d| !d.is_empty());
    let detail = detail.as_deref();
    let last_tool = entries.iter_mut().rev().find(|e| match &e.kind {
        Kind::ToolUsed { run: r, .. } => *r == run,
        _ => false,
    });
    if let Some(Entry { kind: Kind::ToolUsed { tool: t, detail: d, count, .. }, .. }) = last_tool {
        if t == tool && d.as_deref() == detail {
            *count = count.saturating_add(1);
            return;
        }
    }
    let listed = entries
        .iter()
        .filter(|e| matches!(&e.kind, Kind::ToolUsed { run: r, .. } if *r == run))
        .count();
    if listed >= MAX_TOOL_ENTRIES_PER_RUN {
        let omitted = entries.iter_mut().find_map(|e| match &mut e.kind {
            Kind::ToolsOmitted { run: r, count } if *r == run => Some(count),
            _ => None,
        });
        match omitted {
            Some(count) => *count = count.saturating_add(1),
            None => push(entries, at, Kind::ToolsOmitted { run, count: 1 }),
        }
        return;
    }
    push(
        entries,
        at,
        Kind::ToolUsed { run, tool: tool.to_string(), detail: detail.map(str::to_string), count: 1 },
    );
}

/// Append entries recorded elsewhere -- a run's tool calls, buffered until
/// the run ends -- keeping their times and giving them the next positions.
/// Their text is [`sanitize`]d again, and once-only milestones among them are
/// skipped if already recorded.
pub fn append(entries: &mut Vec<Entry>, recorded: Vec<Entry>) {
    for mut entry in recorded {
        if entry.kind.is_tool() {
            sanitize_text(&mut entry.kind);
            push(entries, entry.at, entry.kind);
        } else {
            record(entries, entry.at, entry.kind);
        }
    }
}

fn push(entries: &mut Vec<Entry>, at: DateTime<Utc>, kind: Kind) {
    let seq = entries.iter().map(|e| e.seq).max().unwrap_or(0).saturating_add(1);
    entries.push(Entry { seq, at, kind });
    while entries.len() > MAX_ENTRIES {
        let oldest = entries.iter().position(|e| e.kind.is_tool()).unwrap_or(0);
        entries.remove(oldest);
    }
}

/// Every piece of free text `kind` carries, [`sanitize`]d.
fn sanitize_text(kind: &mut Kind) {
    match kind {
        Kind::RunFailed { reason, .. }
        | Kind::AiReviewFailed { reason, .. }
        | Kind::AiReviewSkipped { reason, .. }
        | Kind::AiFixFailed { reason, .. }
        | Kind::DeliveryFailed { reason } => *reason = sanitize(reason, MAX_REASON_CHARS),
        Kind::ToolUsed { detail: Some(detail), .. } => *detail = sanitize(detail, MAX_DETAIL_CHARS),
        Kind::PrLinked { url, .. } => *url = redact(url),
        _ => {}
    }
}

/// `text` as one line of at most `max` characters: whitespace runs, line
/// breaks included, become one space, and a cut is marked with an ellipsis.
pub fn one_line(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= max {
        return joined;
    }
    let mut cut: String = joined.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The number the next coding run gets: one past any run recorded, so a
/// run whose start was dropped to keep the timeline bounded still has its
/// number.
pub fn next_run(entries: &[Entry]) -> u32 {
    entries
        .iter()
        .filter_map(|e| match e.kind {
            Kind::RunStarted { run, .. }
            | Kind::RunCompleted { run }
            | Kind::RunFailed { run: Some(run), .. }
            | Kind::ToolUsed { run, .. }
            | Kind::ToolsOmitted { run, .. } => Some(run),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

/// The number the next AI review gets: one past any review recorded, begun,
/// skipped or decided, so a review that was skipped without starting still
/// has its number.
pub fn next_review(entries: &[Entry]) -> u32 {
    entries
        .iter()
        .filter_map(|e| match e.kind {
            Kind::AiReviewStarted { review }
            | Kind::AiReviewApproved { review }
            | Kind::AiReviewChangesRequested { review, .. }
            | Kind::AiReviewFailed { review, .. }
            | Kind::AiReviewSkipped { review, .. }
            | Kind::AiFixStarted { review }
            | Kind::AiFixApplied { review }
            | Kind::AiFixUnchanged { review }
            | Kind::AiFixFailed { review, .. } => Some(review),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

/// Read a task's entries leniently: an entry this version does not
/// understand -- one a newer SlashIt wrote -- is skipped instead of failing
/// the whole task, and a missing list is empty. A skipped entry is gone
/// once this version writes the task back; losing history after a
/// downgrade is accepted over refusing to load the board. For
/// `#[serde(default, deserialize_with = "slashit_activity::lenient")]`.
pub fn lenient<'de, D>(deserializer: D) -> Result<Vec<Entry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Maybe {
        Known(Entry),
        Unknown(serde::de::IgnoredAny),
    }
    let items: Vec<Maybe> = Vec::deserialize(deserializer)?;
    Ok(items
        .into_iter()
        .filter_map(|m| match m {
            Maybe::Known(entry) => Some(entry),
            Maybe::Unknown(_) => None,
        })
        .collect())
}

/// A Human Review decision, as the task's review record keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision<'a> {
    pub sequence: u32,
    pub at: DateTime<Utc>,
    pub approved: bool,
    pub feedback: Option<&'a str>,
}

/// One line of the timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row<'a> {
    Created,
    Decision { approved: bool, feedback: Option<&'a str> },
    Recorded(&'a Kind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item<'a> {
    pub at: DateTime<Utc>,
    pub row: Row<'a>,
    /// Where the row came from and its position there. Orders rows recorded
    /// at the same instant; see [`timeline`].
    order: (u8, u32),
}

/// How a row reads at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Good,
    Bad,
    /// Waiting on the user.
    Attention,
    /// A tool call: detail, not a milestone.
    Quiet,
}

/// A task's timeline, oldest first.
///
/// Ordered by time, then, for rows at the same instant, creation first, then
/// recorded entries by `seq`, then decisions by `sequence`. Every key is
/// stored, so the order is the same on every read and after every restart.
pub fn timeline<'a>(
    created_at: DateTime<Utc>,
    decisions: impl IntoIterator<Item = Decision<'a>>,
    entries: &'a [Entry],
) -> Vec<Item<'a>> {
    let mut items = vec![Item { at: created_at, row: Row::Created, order: (0, 0) }];
    items.extend(entries.iter().map(|e| Item { at: e.at, row: Row::Recorded(&e.kind), order: (1, e.seq) }));
    items.extend(decisions.into_iter().map(|d| Item {
        at: d.at,
        row: Row::Decision { approved: d.approved, feedback: d.feedback },
        order: (2, d.sequence),
    }));
    items.sort_by_key(|item| (item.at, item.order));
    items
}

impl Item<'_> {
    /// The line a person reads.
    pub fn title(&self) -> String {
        match &self.row {
            Row::Created => "Task created".to_string(),
            Row::Decision { approved: true, .. } => "You approved the changes".to_string(),
            Row::Decision { approved: false, .. } => "You requested changes".to_string(),
            Row::Recorded(kind) => kind_title(kind),
        }
    }

    /// A second line, when there is more to say.
    pub fn detail(&self) -> Option<String> {
        match &self.row {
            Row::Created => None,
            Row::Decision { feedback, .. } => feedback.map(str::to_string),
            Row::Recorded(kind) => kind_detail(kind),
        }
    }

    pub fn tone(&self) -> Tone {
        match &self.row {
            Row::Created => Tone::Neutral,
            Row::Decision { approved: true, .. } => Tone::Good,
            Row::Decision { approved: false, .. } => Tone::Neutral,
            Row::Recorded(kind) => match kind {
                Kind::ToolUsed { .. } | Kind::ToolsOmitted { .. } => Tone::Quiet,
                Kind::RunFailed { .. }
                | Kind::AiReviewFailed { .. }
                | Kind::AiFixFailed { .. }
                | Kind::DeliveryFailed { .. }
                | Kind::PrClosed { .. } => Tone::Bad,
                Kind::RunCompleted { .. }
                | Kind::AiReviewApproved { .. }
                | Kind::AiFixApplied { .. }
                | Kind::PrLinked { .. }
                | Kind::PrMerged { .. } => Tone::Good,
                Kind::ReadyForReview { .. } => Tone::Attention,
                Kind::Moved { to: Column::Done, .. } => Tone::Good,
                _ => Tone::Neutral,
            },
        }
    }

    /// Whether this row is a tool call rather than a milestone.
    pub fn is_tool(&self) -> bool {
        matches!(self.row, Row::Recorded(kind) if kind.is_tool())
    }

    /// A key that names this row and no other in the task's timeline, and
    /// stays the same as more rows are added.
    pub fn key(&self) -> String {
        match self.order {
            (0, _) => "created".to_string(),
            (1, seq) => format!("entry-{seq}"),
            (_, sequence) => format!("decision-{sequence}"),
        }
    }

    /// What kind of row this is, as the stored entry names it: `created`,
    /// `approved`, `changes_requested`, or an entry's `type`.
    pub fn kind_name(&self) -> &'static str {
        match &self.row {
            Row::Created => "created",
            Row::Decision { approved: true, .. } => "approved",
            Row::Decision { approved: false, .. } => "changes_requested",
            Row::Recorded(kind) => match kind {
                Kind::Moved { .. } => "moved",
                Kind::RunStarted { .. } => "run_started",
                Kind::ToolUsed { .. } => "tool_used",
                Kind::ToolsOmitted { .. } => "tools_omitted",
                Kind::RunCompleted { .. } => "run_completed",
                Kind::RunFailed { .. } => "run_failed",
                Kind::Stopped { .. } => "stopped",
                Kind::Interrupted { .. } => "interrupted",
                Kind::AiReviewStarted { .. } => "ai_review_started",
                Kind::AiReviewApproved { .. } => "ai_review_approved",
                Kind::AiReviewChangesRequested { .. } => "ai_review_changes_requested",
                Kind::AiReviewFailed { .. } => "ai_review_failed",
                Kind::AiReviewSkipped { .. } => "ai_review_skipped",
                Kind::AiFixStarted { .. } => "ai_fix_started",
                Kind::AiFixApplied { .. } => "ai_fix_applied",
                Kind::AiFixUnchanged { .. } => "ai_fix_unchanged",
                Kind::AiFixFailed { .. } => "ai_fix_failed",
                Kind::ReadyForReview { .. } => "ready_for_review",
                Kind::DeliveryFailed { .. } => "delivery_failed",
                Kind::PrLinked { .. } => "pr_linked",
                Kind::PrMerged { .. } => "pr_merged",
                Kind::PrClosed { .. } => "pr_closed",
            },
        }
    }
}

fn nth(label: &str, n: u32) -> String {
    if n > 1 {
        format!("{label} (attempt {n})")
    } else {
        label.to_string()
    }
}

fn kind_title(kind: &Kind) -> String {
    match kind {
        Kind::Moved { from: Column::Error, to: Column::Queue } => "Retried".to_string(),
        Kind::Moved { from: Column::Backlog, to: Column::Queue } => "Added to the queue".to_string(),
        Kind::Moved { to: Column::Done, .. } => "Moved to Done".to_string(),
        Kind::Moved { to, .. } => format!("Moved to {}", to.label()),
        Kind::RunStarted { addressing_feedback: true, .. } => "Coding started on your feedback".to_string(),
        Kind::RunStarted { run, .. } => nth("Coding started", *run),
        Kind::ToolUsed { tool, count, .. } if *count > 1 => format!("Used {tool} ×{count}"),
        Kind::ToolUsed { tool, .. } => format!("Used {tool}"),
        Kind::ToolsOmitted { count, .. } => {
            format!("{count} more tool call{}", if *count == 1 { "" } else { "s" })
        }
        Kind::RunCompleted { .. } => "Coding finished".to_string(),
        Kind::RunFailed { run: Some(_), .. } => "Coding failed".to_string(),
        Kind::RunFailed { run: None, .. } => "Could not start".to_string(),
        Kind::Stopped { from } => format!("Stopped during {}", from.label()),
        Kind::Interrupted { from } => format!("Returned to the queue from {} when SlashIt restarted", from.label()),
        Kind::AiReviewStarted { review } => nth("AI review started", *review),
        Kind::AiReviewApproved { .. } => "AI review approved".to_string(),
        Kind::AiReviewChangesRequested { .. } => "AI review requested changes".to_string(),
        Kind::AiReviewFailed { .. } => "AI review failed".to_string(),
        Kind::AiReviewSkipped { .. } => "AI review skipped".to_string(),
        Kind::AiFixStarted { .. } => "Fixing AI review findings".to_string(),
        Kind::AiFixApplied { .. } => "AI review fixes applied".to_string(),
        Kind::AiFixUnchanged { .. } => "Fix agent changed nothing".to_string(),
        Kind::AiFixFailed { .. } => "AI review fixes failed".to_string(),
        Kind::ReadyForReview { .. } => "Ready for your review".to_string(),
        Kind::DeliveryFailed { .. } => "Pull request not created".to_string(),
        Kind::PrLinked { .. } => "Pull request linked".to_string(),
        Kind::PrMerged { .. } => "Pull request merged".to_string(),
        Kind::PrClosed { .. } => "Pull request closed without merge".to_string(),
    }
}

fn kind_detail(kind: &Kind) -> Option<String> {
    match kind {
        Kind::ToolUsed { detail, .. } => detail.clone(),
        Kind::RunFailed { reason, .. }
        | Kind::AiReviewFailed { reason, .. }
        | Kind::AiReviewSkipped { reason, .. }
        | Kind::AiFixFailed { reason, .. }
        | Kind::DeliveryFailed { reason } => Some(reason.clone()),
        Kind::AiReviewChangesRequested { issues, .. } if *issues > 0 => {
            Some(format!("{issues} issue{} found", if *issues == 1 { "" } else { "s" }))
        }
        Kind::PrLinked { number: Some(number), .. } => Some(format!("#{number}")),
        Kind::PrMerged { number } | Kind::PrClosed { number } => Some(format!("#{number}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
