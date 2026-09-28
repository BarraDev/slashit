//! The decisions behind the task drawer and the card activity line, kept free
//! of the DOM so they can be tested natively.

use crate::models::{AgentEvent, TaskRunSnapshot, TaskStatus};

/// The most characters of the agent's latest words a one-line activity shows.
const ACTIVITY_LIMIT: usize = 120;

/// How an event changes the one-line "what is the agent doing" summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityUpdate {
    Set(String),
    /// A run started or ended: whatever was shown belongs to another moment.
    Clear,
}

/// The activity an event implies, if any.
///
/// Only what the agent itself does counts: a tool it reaches for, or the last
/// thing it said. SlashIt's own `log` lines describe the run's plumbing
/// (worktrees, commits) and are deliberately not activity.
pub fn activity_from_event(event: &AgentEvent) -> Option<ActivityUpdate> {
    match event {
        AgentEvent::ToolUse { tool, .. } => Some(ActivityUpdate::Set(format!("Using {tool}"))),
        AgentEvent::Output { text, .. } => last_line(text).map(ActivityUpdate::Set),
        AgentEvent::PhaseChange { .. } | AgentEvent::Completed { .. } | AgentEvent::Error { .. } => {
            Some(ActivityUpdate::Clear)
        }
        AgentEvent::Log { .. } => None,
    }
}

/// The last non-blank line of `text`, cut to [`ACTIVITY_LIMIT`] characters.
fn last_line(text: &str) -> Option<String> {
    let line = text.lines().rev().map(str::trim).find(|l| !l.is_empty())?;
    if line.chars().count() <= ACTIVITY_LIMIT {
        Some(line.to_string())
    } else {
        Some(format!("{}…", line.chars().take(ACTIVITY_LIMIT).collect::<String>()))
    }
}

/// Whether an event changes the task record itself (status, phase, progress),
/// so the board should read the task list again rather than wait for its poll.
pub fn changes_task_record(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::PhaseChange { .. } | AgentEvent::Completed { .. } | AgentEvent::Error { .. }
    )
}

/// Whether the card should show a live activity line for a task in `status`.
pub fn shows_activity(status: &TaskStatus) -> bool {
    matches!(status, TaskStatus::InProgress | TaskStatus::AiReview)
}

/// What a person can do from the drawer, given the task and its run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrawerActions {
    /// An agent is live, so stopping has something to end.
    pub stop: bool,
    /// The task failed and nothing is running: re-queue it.
    pub retry: bool,
    /// The task is not running and its definition can still change.
    pub edit: bool,
    /// The task has produced changes worth reviewing.
    pub changes: bool,
}

impl DrawerActions {
    pub fn for_task(status: &TaskStatus, run: &TaskRunSnapshot) -> Self {
        let live = run.live;
        Self {
            stop: live,
            retry: *status == TaskStatus::Error && !live,
            edit: matches!(status, TaskStatus::Backlog | TaskStatus::Queue | TaskStatus::Error) && !live,
            changes: matches!(
                status,
                TaskStatus::AiReview | TaskStatus::HumanReview | TaskStatus::Done | TaskStatus::PrCreated
            ),
        }
    }
}

/// How the output section should describe what it is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputProvenance {
    /// The execution is running now.
    Live,
    /// The output belongs to an execution that has ended.
    LastAttempt,
    /// Nothing ran in this session.
    None,
}

pub fn output_provenance(run: &TaskRunSnapshot) -> OutputProvenance {
    match &run.last_execution {
        Some(execution) if run.live && execution.stopped_at.is_none() => OutputProvenance::Live,
        Some(_) => OutputProvenance::LastAttempt,
        None => OutputProvenance::None,
    }
}

/// `1h 02m`, `3m 07s` or `42s`.
pub fn format_elapsed(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Orders responses to requests that can overlap.
///
/// The board reads the task list from a poll, after a live event and after a
/// Stop or Retry. Those requests race; answered out of order, an older
/// snapshot would overwrite a newer one. Every request takes a ticket, and a
/// response is applied only if no later ticket has been applied already.
#[derive(Debug, Default, Clone, Copy)]
pub struct ResponseOrder {
    issued: u64,
    applied: u64,
}

impl ResponseOrder {
    pub fn issue(&mut self) -> u64 {
        self.issued += 1;
        self.issued
    }

    /// Whether the response to `ticket` may be applied. Applying it makes
    /// every earlier ticket stale.
    pub fn accept(&mut self, ticket: u64) -> bool {
        if ticket > self.applied {
            self.applied = ticket;
            true
        } else {
            false
        }
    }

    /// Treat everything issued so far as stale, because a newer truth (a
    /// command's own answer) has just been applied directly.
    pub fn supersede_pending(&mut self) {
        self.applied = self.issued;
    }
}

/// Coalesces "read the run again" requests while one is in flight.
///
/// Live events can arrive faster than the snapshot can be read. One read at a
/// time, plus at most one more after it if anything asked meanwhile, keeps the
/// drawer current without a request per event.
#[derive(Debug, Default, Clone, Copy)]
pub struct RefreshGate {
    in_flight: bool,
    dirty: bool,
}

impl RefreshGate {
    /// Ask for a refresh. `true` means start one now.
    pub fn request(&mut self) -> bool {
        if self.in_flight {
            self.dirty = true;
            false
        } else {
            self.in_flight = true;
            true
        }
    }

    /// A refresh finished. `true` means start another, because something
    /// asked while it was running.
    pub fn finish(&mut self) -> bool {
        if self.dirty {
            self.dirty = false;
            true
        } else {
            self.in_flight = false;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ExecutionSnapshot, TaskPhase};

    fn tool(tool: &str) -> AgentEvent {
        AgentEvent::ToolUse { task_id: "t".into(), tool: tool.into() }
    }

    fn run(live: bool, execution: Option<bool>) -> TaskRunSnapshot {
        TaskRunSnapshot {
            live,
            last_execution: execution.map(|stopped| ExecutionSnapshot {
                started_at: chrono::Utc::now(),
                stopped_at: stopped.then(chrono::Utc::now),
                output: Vec::new(),
            }),
        }
    }

    #[test]
    fn activity_follows_what_the_agent_does_and_ignores_plumbing() {
        assert_eq!(activity_from_event(&tool("Edit")), Some(ActivityUpdate::Set("Using Edit".into())));
        assert_eq!(
            activity_from_event(&AgentEvent::Output { task_id: "t".into(), text: "first\n  second  \n\n".into() }),
            Some(ActivityUpdate::Set("second".into()))
        );
        assert_eq!(activity_from_event(&AgentEvent::Output { task_id: "t".into(), text: " \n ".into() }), None);
        assert_eq!(
            activity_from_event(&AgentEvent::Log {
                task_id: "t".into(),
                level: crate::models::LogLevel::Info,
                message: "Starting Claude agent in /some/worktree".into(),
            }),
            None,
            "executor bookkeeping (paths) is not the agent's activity"
        );
        assert_eq!(
            activity_from_event(&AgentEvent::PhaseChange { task_id: "t".into(), phase: TaskPhase::Coding, progress: 10 }),
            Some(ActivityUpdate::Clear)
        );
    }

    #[test]
    fn long_activity_is_cut_to_one_short_line() {
        let long = "x".repeat(500);
        let Some(ActivityUpdate::Set(shown)) =
            activity_from_event(&AgentEvent::Output { task_id: "t".into(), text: long })
        else {
            panic!("expected an activity");
        };
        assert_eq!(shown.chars().count(), ACTIVITY_LIMIT + 1);
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn stop_is_offered_only_while_an_agent_is_live() {
        for status in [TaskStatus::InProgress, TaskStatus::AiReview] {
            assert!(DrawerActions::for_task(&status, &run(true, Some(false))).stop);
            assert!(!DrawerActions::for_task(&status, &run(false, Some(true))).stop);
        }
        for status in [TaskStatus::Backlog, TaskStatus::Error, TaskStatus::HumanReview] {
            assert!(!DrawerActions::for_task(&status, &run(false, None)).stop);
        }
    }

    #[test]
    fn retry_is_offered_only_for_a_failed_task_with_nothing_running() {
        assert!(DrawerActions::for_task(&TaskStatus::Error, &run(false, Some(true))).retry);
        assert!(DrawerActions::for_task(&TaskStatus::Error, &run(false, None)).retry);
        assert!(!DrawerActions::for_task(&TaskStatus::Error, &run(true, Some(false))).retry);
        for status in [
            TaskStatus::Backlog,
            TaskStatus::Queue,
            TaskStatus::InProgress,
            TaskStatus::AiReview,
            TaskStatus::HumanReview,
            TaskStatus::Done,
            TaskStatus::PrCreated,
        ] {
            assert!(!DrawerActions::for_task(&status, &run(false, Some(true))).retry, "{status:?}");
        }
    }

    #[test]
    fn human_review_is_read_only_apart_from_its_changes() {
        let actions = DrawerActions::for_task(&TaskStatus::HumanReview, &run(false, Some(true)));
        assert_eq!(actions, DrawerActions { stop: false, retry: false, edit: false, changes: true });
    }

    #[test]
    fn output_is_labelled_by_where_it_came_from() {
        assert_eq!(output_provenance(&run(true, Some(false))), OutputProvenance::Live);
        assert_eq!(output_provenance(&run(false, Some(true))), OutputProvenance::LastAttempt);
        // An AI review is live, but the coding execution it follows has ended.
        assert_eq!(output_provenance(&run(true, Some(true))), OutputProvenance::LastAttempt);
        assert_eq!(output_provenance(&run(false, None)), OutputProvenance::None);
    }

    #[test]
    fn elapsed_reads_naturally() {
        assert_eq!(format_elapsed(-3), "0s");
        assert_eq!(format_elapsed(42), "42s");
        assert_eq!(format_elapsed(187), "3m 07s");
        assert_eq!(format_elapsed(3720), "1h 02m");
    }

    #[test]
    fn an_older_response_never_overwrites_a_newer_one() {
        let mut order = ResponseOrder::default();
        let poll = order.issue();
        let after_event = order.issue();
        assert!(order.accept(after_event));
        assert!(!order.accept(poll), "the poll was answered late and is stale");

        let in_flight = order.issue();
        order.supersede_pending();
        assert!(!order.accept(in_flight), "a command's own answer superseded it");
        let fresh = order.issue();
        assert!(order.accept(fresh));
    }

    #[test]
    fn refreshes_coalesce_while_one_is_in_flight() {
        let mut gate = RefreshGate::default();
        assert!(gate.request());
        assert!(!gate.request());
        assert!(!gate.request());
        assert!(gate.finish(), "one more read covers everything that asked meanwhile");
        assert!(!gate.finish());
        assert!(gate.request(), "idle again");
    }

    #[test]
    fn only_state_changing_events_refresh_the_task_record() {
        assert!(changes_task_record(&AgentEvent::Completed { task_id: "t".into(), success: true, message: None }));
        assert!(changes_task_record(&AgentEvent::Error { task_id: "t".into(), message: "m".into() }));
        assert!(!changes_task_record(&tool("Read")));
    }
}
