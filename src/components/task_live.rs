//! The decisions behind the task drawer and the card activity line, kept free
//! of the DOM so they can be tested natively.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::models::{AgentEvent, AgentStatus, AttentionReason, LiveRun, TaskRunSnapshot, TaskStatus};

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
        // A run beginning or ending belongs to another moment than whatever
        // was shown; one that is running or stopping adds nothing.
        AgentEvent::RunState { status, .. } => match status {
            AgentStatus::Starting | AgentStatus::Stopped | AgentStatus::Failed(_) => Some(ActivityUpdate::Clear),
            AgentStatus::Running | AgentStatus::Stopping => None,
        },
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
    ) || matches!(
        event,
        // A run ending is when a stop settles the task, which announces
        // nothing else.
        AgentEvent::RunState { status: AgentStatus::Stopped | AgentStatus::Failed(_), .. }
    )
}

/// What a card or drawer says about a run SlashIt owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunBadge {
    /// Owned, process not started yet.
    Starting,
    /// Owned and running. The only state shown as Working.
    Working,
    /// A stop was asked for and the agent is still shutting down.
    Stopping,
}

impl RunBadge {
    /// `None` for a run that is over: it is not shown.
    pub fn from_status(status: &AgentStatus) -> Option<Self> {
        match status {
            AgentStatus::Starting => Some(Self::Starting),
            AgentStatus::Running => Some(Self::Working),
            AgentStatus::Stopping => Some(Self::Stopping),
            AgentStatus::Stopped | AgentStatus::Failed(_) => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Working => "Working",
            Self::Stopping => "Stopping",
        }
    }

    /// The drawer's activity line when the agent has said nothing yet.
    pub fn placeholder(self) -> &'static str {
        match self {
            Self::Starting => "Starting\u{2026}",
            Self::Working => "Working\u{2026}",
            Self::Stopping => "Stopping\u{2026}",
        }
    }
}

/// What a card or drawer shows for a task whose run is `run`.
///
/// A task that needs the person (`attention`) shows that, never a run badge:
/// the reason is the more useful thing to say.
pub fn project_run(run: Option<&AgentStatus>, attention: Option<AttentionReason>) -> Option<RunBadge> {
    if attention.is_some() {
        return None;
    }
    run.and_then(RunBadge::from_status)
}

/// The board's one record of the runs SlashIt owns.
///
/// Filled from `get_live_runs` when the board opens and kept current by
/// `run_state` events. The two overlap: the snapshot is requested after the
/// listener is registered, so an event can land while the answer is on its
/// way, and the answer may describe a moment before it. A task an event
/// spoke for while the request was out keeps what the event said.
///
/// Nothing here comes from a task's persisted status, so a restart starts
/// empty and a stale `InProgress` never shows as working.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LiveRuns {
    runs: HashMap<Uuid, AgentStatus>,
    /// Tasks an event spoke for since the snapshot was requested; `None`
    /// when no snapshot is in flight.
    spoken_for: Option<HashSet<Uuid>>,
}

impl LiveRuns {
    /// A snapshot is about to be requested.
    pub fn begin_hydration(&mut self) {
        self.spoken_for = Some(HashSet::new());
    }

    /// The snapshot answered. A failed read passes `None` and leaves what the
    /// events have said so far.
    pub fn finish_hydration(&mut self, snapshot: Option<Vec<LiveRun>>) {
        let spoken_for = self.spoken_for.take().unwrap_or_default();
        let Some(snapshot) = snapshot else { return };
        self.runs.retain(|id, _| spoken_for.contains(id));
        for run in snapshot {
            if !spoken_for.contains(&run.task_id) {
                self.runs.insert(run.task_id, run.status);
            }
        }
    }

    /// Follow a `run_state` event. Ending states remove the run.
    pub fn apply_event(&mut self, task_id: Uuid, status: AgentStatus) {
        if let Some(spoken_for) = &mut self.spoken_for {
            spoken_for.insert(task_id);
        }
        if RunBadge::from_status(&status).is_some() {
            self.runs.insert(task_id, status);
        } else {
            self.runs.remove(&task_id);
        }
    }

    /// The state of the run SlashIt owns for `task_id`, if it owns one.
    pub fn status(&self, task_id: &Uuid) -> Option<AgentStatus> {
        self.runs.get(task_id).cloned()
    }
}

/// Whether the board's "Running" count includes a task in `status`: coding,
/// or under AI review.
///
/// Read from the status alone, like the rest of the board. An AI review that
/// is waiting for a free slot is counted too; the record does not tell it
/// apart from one that has started.
pub fn counts_as_running(status: &TaskStatus) -> bool {
    matches!(status, TaskStatus::InProgress | TaskStatus::AiReview)
}

/// What a person can do from the drawer, given the task and its run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrawerActions {
    /// The task is waiting in Backlog with nothing running: put it in the
    /// queue, where the scheduler starts it when there is capacity.
    pub start: bool,
    /// The task's execution is live, so stopping has something to end.
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
            start: *status == TaskStatus::Backlog && !live,
            // Only a running execution. Stopping an AI review would also
            // send the task back to Backlog, which is a decision this surface
            // does not offer yet.
            stop: live && *status == TaskStatus::InProgress,
            retry: *status == TaskStatus::Error && !live,
            edit: matches!(status, TaskStatus::Backlog | TaskStatus::Queue | TaskStatus::Error) && !live,
            changes: matches!(
                status,
                TaskStatus::AiReview | TaskStatus::HumanReview | TaskStatus::Done | TaskStatus::PrCreated
            ),
        }
    }
}

/// The drawer's Start request, from press to answer.
///
/// Start only enqueues. Whether and when the task then runs is the
/// scheduler's decision, so this tracks nothing past the enqueue's answer;
/// what the task became is read from the task itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StartRequest {
    #[default]
    Idle,
    /// The enqueue was sent and has not answered yet.
    Pending,
    /// The last enqueue failed, for this reason. Start can be pressed again.
    Failed(String),
}

impl StartRequest {
    /// Begin a request, or refuse to while one is in flight.
    ///
    /// A second enqueue is not harmless: by the time it lands the scheduler
    /// may already have started the task, and moving a running task back to
    /// the queue ends its agent.
    pub fn begin(&mut self) -> bool {
        if *self == Self::Pending {
            return false;
        }
        *self = Self::Pending;
        true
    }

    /// Settle on the enqueue's answer, handing back the task's current record
    /// to apply if there is one: the one just queued, or the task as it now
    /// is if it had already moved on.
    pub fn settle<T>(&mut self, outcome: Result<Option<T>, String>) -> Option<T> {
        match outcome {
            Ok(Some(task)) => {
                *self = Self::Idle;
                Some(task)
            }
            Ok(None) => {
                *self = Self::Failed("This task no longer exists.".to_string());
                None
            }
            Err(e) => {
                *self = Self::Failed(format!("Could not start the task: {e}"));
                None
            }
        }
    }

    pub fn is_pending(&self) -> bool {
        *self == Self::Pending
    }

    pub fn failure(&self) -> Option<&str> {
        match self {
            Self::Failed(reason) => Some(reason),
            _ => None,
        }
    }

    /// The failure to show for a task now in `status`, if any.
    ///
    /// A failed Start is about the Backlog task it was pressed for. The task
    /// can leave Backlog while the request is still in flight, and a failure
    /// that lands after that no longer describes where the task is.
    pub fn failure_for(&self, status: Option<&TaskStatus>) -> Option<&str> {
        self.failure().filter(|_| status == Some(&TaskStatus::Backlog))
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

    #[test]
    fn running_counts_coding_and_ai_review() {
        assert!(counts_as_running(&TaskStatus::InProgress));
        assert!(counts_as_running(&TaskStatus::AiReview));
        for status in [
            TaskStatus::Backlog,
            TaskStatus::Queue,
            TaskStatus::HumanReview,
            TaskStatus::PrCreated,
            TaskStatus::Done,
            TaskStatus::Error,
        ] {
            assert!(!counts_as_running(&status), "{status:?}");
        }
    }

    fn tool(tool: &str) -> AgentEvent {
        AgentEvent::ToolUse { task_id: "t".into(), tool: tool.into() }
    }

    fn run(live: bool, execution: Option<bool>) -> TaskRunSnapshot {
        TaskRunSnapshot {
            live,
            status: live.then_some(AgentStatus::Running),
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
    fn stop_is_offered_only_while_an_execution_is_live() {
        assert!(DrawerActions::for_task(&TaskStatus::InProgress, &run(true, Some(false))).stop);
        assert!(!DrawerActions::for_task(&TaskStatus::InProgress, &run(false, Some(true))).stop);
        // An AI review is live, but stopping it is not offered here.
        assert!(!DrawerActions::for_task(&TaskStatus::AiReview, &run(true, Some(true))).stop);
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
        assert_eq!(actions, DrawerActions { start: false, stop: false, retry: false, edit: false, changes: true });
    }

    #[test]
    fn start_is_offered_only_for_a_backlog_task_with_nothing_running() {
        assert!(DrawerActions::for_task(&TaskStatus::Backlog, &run(false, None)).start);
        assert!(DrawerActions::for_task(&TaskStatus::Backlog, &run(false, Some(true))).start);
        // A stop settles the task in Backlog; its agent may not be gone yet.
        assert!(!DrawerActions::for_task(&TaskStatus::Backlog, &run(true, Some(false))).start);
        for status in [
            TaskStatus::Queue,
            TaskStatus::InProgress,
            TaskStatus::AiReview,
            TaskStatus::HumanReview,
            TaskStatus::Done,
            TaskStatus::PrCreated,
            TaskStatus::Error,
        ] {
            assert!(!DrawerActions::for_task(&status, &run(false, None)).start, "{status:?}");
        }
    }

    #[test]
    fn a_start_in_flight_refuses_a_second_press() {
        let mut start = StartRequest::default();
        assert!(start.begin());
        assert!(start.is_pending());
        assert!(!start.begin(), "a second press while the first is in flight must not enqueue again");
        assert_eq!(start.settle(Ok(Some(7))), Some(7));
        assert_eq!(start, StartRequest::Idle);
    }

    #[test]
    fn a_failed_start_keeps_its_reason_and_can_be_pressed_again() {
        let mut start = StartRequest::default();
        assert!(start.begin());
        assert_eq!(start.settle::<()>(Err("the board could not be saved".into())), None);
        assert_eq!(start.failure(), Some("Could not start the task: the board could not be saved"));
        assert!(!start.is_pending());

        // Retrying clears the old reason while the new request is in flight.
        assert!(start.begin());
        assert_eq!(start.failure(), None);
        assert_eq!(start.settle(Ok(Some(()))), Some(()));
        assert_eq!(start.failure(), None);
    }

    #[test]
    fn a_failed_start_is_shown_only_while_the_task_is_in_backlog() {
        let mut start = StartRequest::default();
        assert!(start.begin());
        // The task moved on while the request was in flight, then it failed.
        assert_eq!(start.settle::<()>(Err("refused".into())), None);
        assert_eq!(start.failure_for(Some(&TaskStatus::Queue)), None);
        assert_eq!(start.failure_for(Some(&TaskStatus::InProgress)), None);
        assert_eq!(start.failure_for(None), None);
        assert_eq!(
            start.failure_for(Some(&TaskStatus::Backlog)),
            Some("Could not start the task: refused")
        );
    }

    #[test]
    fn a_start_for_a_task_that_is_gone_says_so() {
        let mut start = StartRequest::default();
        assert!(start.begin());
        assert_eq!(start.settle::<()>(Ok(None)), None);
        assert_eq!(start.failure(), Some("This task no longer exists."));
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

    fn run_state(task: Uuid, status: AgentStatus) -> AgentEvent {
        AgentEvent::RunState { task_id: task.to_string(), status }
    }

    fn working(runs: &LiveRuns, task: &Uuid) -> Option<RunBadge> {
        project_run(runs.status(task).as_ref(), None)
    }

    // matrix 1, 2, 9, 10: nothing is working until the backend says it owns a run
    #[test]
    fn a_task_with_no_owned_run_is_not_working_whatever_its_status_says() {
        let runs = LiveRuns::default();
        let task = Uuid::from_u128(1);
        assert_eq!(working(&runs, &task), None);
        // The status alone never contributes: there is no input for it.
        assert_eq!(runs.status(&task), None);
    }

    // matrix 3, 4, 8, 6
    #[test]
    fn a_run_goes_from_starting_to_working_to_stopping_and_then_is_gone() {
        let mut runs = LiveRuns::default();
        let task = Uuid::from_u128(1);
        runs.apply_event(task, AgentStatus::Starting);
        assert_eq!(working(&runs, &task), Some(RunBadge::Starting));
        runs.apply_event(task, AgentStatus::Running);
        assert_eq!(working(&runs, &task), Some(RunBadge::Working));
        runs.apply_event(task, AgentStatus::Stopping);
        assert_eq!(working(&runs, &task), Some(RunBadge::Stopping));
        runs.apply_event(task, AgentStatus::Stopped);
        assert_eq!(working(&runs, &task), None, "no stale Working after the run ends");
    }

    // matrix 6, 7
    #[test]
    fn a_failed_run_is_not_shown_as_working() {
        let mut runs = LiveRuns::default();
        let task = Uuid::from_u128(1);
        runs.apply_event(task, AgentStatus::Running);
        runs.apply_event(task, AgentStatus::Failed("boom".into()));
        assert_eq!(working(&runs, &task), None);
        assert_eq!(RunBadge::from_status(&AgentStatus::Failed("x".into())), None);
    }

    // matrix 5: what the agent says moves the activity line, never the run
    #[test]
    fn activity_changes_do_not_change_whether_the_run_is_working() {
        let mut runs = LiveRuns::default();
        let task = Uuid::from_u128(1);
        runs.apply_event(task, AgentStatus::Running);
        let before = runs.clone();
        for event in [
            tool("Edit"),
            AgentEvent::Output { task_id: task.to_string(), text: "next step".into() },
            tool("Bash"),
        ] {
            // Only `run_state` events are fed to the registry.
            assert!(!matches!(event, AgentEvent::RunState { .. }));
            assert!(activity_from_event(&event).is_some());
        }
        assert_eq!(runs, before);
        assert_eq!(working(&runs, &task), Some(RunBadge::Working));
    }

    // matrix 12
    #[test]
    fn there_is_no_waiting_for_input_state_to_show() {
        // Exhaustive on purpose: a new status must be placed here, and
        // waiting for input needs a provider signal that does not exist.
        for status in [
            AgentStatus::Starting,
            AgentStatus::Running,
            AgentStatus::Stopping,
            AgentStatus::Stopped,
            AgentStatus::Failed(String::new()),
        ] {
            match status {
                AgentStatus::Starting | AgentStatus::Running | AgentStatus::Stopping => {
                    assert!(RunBadge::from_status(&status).is_some())
                }
                AgentStatus::Stopped | AgentStatus::Failed(_) => assert!(RunBadge::from_status(&status).is_none()),
            }
        }
    }

    // matrix 11
    #[test]
    fn a_task_that_needs_the_person_shows_that_not_a_run_badge() {
        let mut runs = LiveRuns::default();
        let task = Uuid::from_u128(1);
        runs.apply_event(task, AgentStatus::Running);
        assert_eq!(project_run(runs.status(&task).as_ref(), Some(AttentionReason::Review)), None);
        assert_eq!(project_run(runs.status(&task).as_ref(), Some(AttentionReason::Failed)), None);
        assert_eq!(project_run(runs.status(&task).as_ref(), None), Some(RunBadge::Working));
    }

    // matrix 13
    #[test]
    fn parallel_runs_do_not_cross_associate() {
        let mut runs = LiveRuns::default();
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        runs.apply_event(a, AgentStatus::Running);
        runs.apply_event(b, AgentStatus::Starting);
        runs.apply_event(a, AgentStatus::Stopped);
        assert_eq!(working(&runs, &a), None);
        assert_eq!(working(&runs, &b), Some(RunBadge::Starting));
    }

    // matrix 14
    #[test]
    fn a_board_that_opens_while_a_run_is_active_starts_from_the_backend_snapshot() {
        let mut runs = LiveRuns::default();
        let task = Uuid::from_u128(1);
        runs.begin_hydration();
        runs.finish_hydration(Some(vec![LiveRun { task_id: task, status: AgentStatus::Running }]));
        assert_eq!(working(&runs, &task), Some(RunBadge::Working));
    }

    #[test]
    fn an_event_during_hydration_beats_the_older_snapshot_for_its_own_task() {
        let mut runs = LiveRuns::default();
        let (ended, begun, quiet) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        runs.begin_hydration();
        // The snapshot was read before these happened.
        runs.apply_event(ended, AgentStatus::Stopped);
        runs.apply_event(begun, AgentStatus::Starting);
        runs.finish_hydration(Some(vec![
            LiveRun { task_id: ended, status: AgentStatus::Running },
            LiveRun { task_id: quiet, status: AgentStatus::Running },
        ]));
        assert_eq!(working(&runs, &ended), None, "the run ended after the snapshot; no phantom");
        assert_eq!(working(&runs, &begun), Some(RunBadge::Starting));
        assert_eq!(working(&runs, &quiet), Some(RunBadge::Working));
    }

    #[test]
    fn a_failed_snapshot_read_leaves_only_what_events_said() {
        let mut runs = LiveRuns::default();
        let task = Uuid::from_u128(1);
        runs.begin_hydration();
        runs.apply_event(task, AgentStatus::Running);
        runs.finish_hydration(None);
        assert_eq!(working(&runs, &task), Some(RunBadge::Working));
        // And an event after hydration is applied as ever.
        runs.apply_event(task, AgentStatus::Stopped);
        assert_eq!(working(&runs, &task), None);
    }

    #[test]
    fn a_run_ending_announces_a_task_record_change_and_clears_activity() {
        let task = Uuid::from_u128(1);
        assert!(changes_task_record(&run_state(task, AgentStatus::Stopped)));
        assert!(changes_task_record(&run_state(task, AgentStatus::Failed("x".into()))));
        assert!(!changes_task_record(&run_state(task, AgentStatus::Running)));
        assert_eq!(activity_from_event(&run_state(task, AgentStatus::Stopped)), Some(ActivityUpdate::Clear));
        assert_eq!(activity_from_event(&run_state(task, AgentStatus::Running)), None);
    }

}
