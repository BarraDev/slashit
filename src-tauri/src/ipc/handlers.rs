// SPDX-License-Identifier: MPL-2.0
//! One function per command.
//!
//! The handlers know nothing about transports, credentials or windows. Access
//! control has already happened by the time [`dispatch`] is called (see
//! [`super::server`]), and the two operations that differ between the desktop
//! app and the daemon — showing a window, quitting — go through
//! [`crate::instance::InstanceControl`]. That is why this module names no Tauri
//! type at all: it is the same code in both processes.

use slashit_ipc::{
    AppStatus, InstanceInfo, IpcRequest, IpcResponse, ProjectSummary, QueueStatusInfo, TaskSummary,
    TerminalSummary, PROTOCOL_VERSION,
};
use uuid::Uuid;

use crate::domain::{TaskPriority, TaskStatus};

use super::server::{IpcContext, PeerContext};

/// The outcome of one request.
///
/// A plain [`IpcResponse`] is not quite enough, because `Quit` has to happen
/// *after* its own reply has been written: asking a daemon to stop trips the
/// shutdown signal, and a client that asked politely should not be answered
/// with a closed connection.
pub struct Dispatch {
    pub response: IpcResponse,

    /// The instance was asked to stop. The server calls
    /// [`crate::instance::InstanceControl::request_quit`] once the response is
    /// on the wire.
    pub then_quit: bool,
}

impl Dispatch {
    /// An ordinary answer, with nothing deferred.
    pub fn reply(response: IpcResponse) -> Self {
        Self {
            response,
            then_quit: false,
        }
    }
}

/// Run one already-authorized request.
///
/// `peer` is carried this far only so `Ping` can report the endpoint the caller
/// actually reached; no handler makes an access-control decision of its own,
/// because a rule that lived in two places would eventually be applied in one.
pub async fn dispatch(req: IpcRequest, ctx: &IpcContext, peer: &PeerContext) -> Dispatch {
    match req {
        IpcRequest::Status => Dispatch::reply(handle_status(ctx).await),
        IpcRequest::ListProjects => Dispatch::reply(handle_list_projects(ctx).await),
        IpcRequest::ListTasks { project_id } => {
            Dispatch::reply(handle_list_tasks(ctx, project_id).await)
        }
        IpcRequest::GetTask { task_id } => Dispatch::reply(handle_get_task(ctx, task_id).await),
        IpcRequest::GetTaskEvidence { task_id } => {
            Dispatch::reply(handle_get_task_evidence(ctx, task_id).await)
        }
        IpcRequest::CreateTask {
            project_id,
            title,
            description,
            priority,
            review_mode,
        } => Dispatch::reply(
            handle_create_task(ctx, project_id, title, description, priority, review_mode).await,
        ),
        IpcRequest::MoveTask { task_id, status } => {
            Dispatch::reply(handle_move_task(ctx, task_id, status).await)
        }
        IpcRequest::EditTask {
            task_id,
            title,
            description,
            priority,
        } => Dispatch::reply(handle_edit_task(ctx, task_id, title, description, priority).await),
        IpcRequest::SetTaskModel { task_id, model } => {
            Dispatch::reply(handle_set_task_model(ctx, task_id, model).await)
        }
        IpcRequest::SetTaskReviewMode { task_id, review_mode } => {
            Dispatch::reply(handle_set_task_review_mode(ctx, task_id, review_mode).await)
        }
        IpcRequest::SetTaskDependencies { task_id, dependencies } => {
            Dispatch::reply(handle_set_task_dependencies(ctx, task_id, dependencies).await)
        }
        IpcRequest::DeleteTask { task_id } => {
            Dispatch::reply(handle_delete_task(ctx, task_id).await)
        }
        IpcRequest::QueueStatus => Dispatch::reply(handle_queue_status(ctx).await),
        IpcRequest::EnqueueTask { task_id } => {
            Dispatch::reply(handle_enqueue_task(ctx, task_id).await)
        }
        IpcRequest::ListTerminals => Dispatch::reply(handle_list_terminals(ctx).await),
        IpcRequest::Show => Dispatch::reply(handle_show(ctx)),
        IpcRequest::Quit => handle_quit(),
        IpcRequest::Features => Dispatch::reply(handle_features(ctx).await),
        IpcRequest::Ping => Dispatch::reply(handle_ping(ctx, peer)),
        IpcRequest::ExportProject { project_id, path } => {
            Dispatch::reply(handle_export_project(ctx, project_id, path).await)
        }
        IpcRequest::PreviewImport { path } => {
            Dispatch::reply(handle_preview_import(ctx, path).await)
        }
        IpcRequest::ApplyImport { path, checksum } => {
            Dispatch::reply(handle_apply_import(ctx, path, checksum).await)
        }
    }
}

/// The portable Project operations over this instance's shared state.
fn portable_live(ctx: &IpcContext) -> crate::portability::live::Live<'_> {
    crate::portability::live::Live {
        gate: &ctx.project_gate,
        leases: &ctx.task_lifecycle_locks,
        tasks: &ctx.tasks,
        projects: &ctx.projects,
        repositories: &ctx.repositories,
        storage: &ctx.storage,
    }
}

/// Write one Project's portable export to a new file.
async fn handle_export_project(ctx: &IpcContext, project_id: String, path: String) -> IpcResponse {
    match portable_live(ctx).export(&project_id, &path).await {
        Ok(outcome) => IpcResponse::success(serde_json::to_value(outcome).unwrap_or_default()),
        Err(e) => IpcResponse::error(e),
    }
}

/// Validate an export file and describe what importing it would do. Reads the
/// file and the instance's memory; nothing is written.
async fn handle_preview_import(ctx: &IpcContext, path: String) -> IpcResponse {
    match portable_live(ctx).preview(&path).await {
        Ok(report) => IpcResponse::success(serde_json::to_value(report).unwrap_or_default()),
        Err(e) => IpcResponse::error(e),
    }
}

/// Create the Project of an export file, when the confirmed checksum matches
/// its payload. The apply validates again under the locks it commits with.
async fn handle_apply_import(ctx: &IpcContext, path: String, checksum: String) -> IpcResponse {
    match portable_live(ctx).apply(&path, &checksum).await {
        Ok(report) => IpcResponse::success(serde_json::to_value(report).unwrap_or_default()),
        Err(e) => IpcResponse::error(e.to_string()),
    }
}

async fn handle_status(ctx: &IpcContext) -> IpcResponse {
    let active_terminals = ctx.pty.sessions.lock().await.len();

    let executor = ctx.executor.get().map(|executor| executor.as_ref());
    let running_agents =
        crate::commands::agent::active_agent_count(&ctx.executions, executor).await;

    let queue_mgr = ctx.queue_manager.read().await;
    let queued_tasks = queue_mgr.get_queued_tasks().await.len();
    let in_progress_tasks = queue_mgr.get_in_progress_count().await;

    let status = AppStatus {
        active_terminals,
        running_agents,
        queued_tasks,
        in_progress_tasks,
    };

    IpcResponse::success(serde_json::to_value(status).unwrap_or_default())
}

async fn handle_list_projects(ctx: &IpcContext) -> IpcResponse {
    let projects = ctx.projects.read().await;
    let repositories = ctx.repositories.read().await;
    let summaries: Vec<ProjectSummary> = projects
        .values()
        .map(|p| ProjectSummary {
            id: p.id.to_string(),
            name: p.name.clone(),
            path: p.repository_path(&repositories),
        })
        .collect();

    IpcResponse::success(serde_json::to_value(summaries).unwrap_or_default())
}

async fn handle_list_tasks(ctx: &IpcContext, project_id: Option<String>) -> IpcResponse {
    let filter_id = project_id.as_deref().and_then(|s| Uuid::parse_str(s).ok());

    let tasks = ctx.tasks.read().await;
    let projects = ctx.projects.read().await;
    let summaries: Vec<TaskSummary> = tasks
        .values()
        .filter(|t| match filter_id {
            Some(pid) => t.project_id == pid,
            None => true,
        })
        .map(|t| {
            let name = projects
                .get(&t.project_id)
                .map(|p| p.name.as_str())
                .unwrap_or("?");
            task_to_summary(t, name)
        })
        .collect();

    IpcResponse::success(serde_json::to_value(summaries).unwrap_or_default())
}

/// Read one Task as its full record, under the same field names the Task is
/// stored with. Answered from the in-memory Task store; nothing is written.
async fn handle_get_task(ctx: &IpcContext, task_id: String) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
    };
    let tasks = ctx.tasks.read().await;
    match tasks.get(&task_uuid) {
        Some(task) => match serde_json::to_value(task) {
            Ok(value) => IpcResponse::success(value),
            Err(e) => IpcResponse::error(format!("Could not encode task {task_id}: {e}")),
        },
        None => IpcResponse::error(format!("Task {task_id} not found")),
    }
}

// --- Task execution evidence -------------------------------------------------
//
// One read-only answer per Task. It assembles evidence that already exists —
// the Task's persisted record and Activity, which the executor writes with
// every lifecycle transition, and the live-run registry that
// `TaskExecutor::task_run` and `TaskExecutor::get_task_output` read for the
// desktop — so there is no second logging system, nothing is written here,
// and nothing streams: one request, one bounded answer. Bounded twice over:
// every echoed field has its own limit below, and their serialized sum is
// checked once against [`MAX_EVIDENCE_RESPONSE_BYTES`] before it is sent.

/// The persisted runs one evidence answer describes, newest first.
const MAX_EVIDENCE_RUNS: usize = 5;
/// The Activity milestones one answer echoes, newest first.
const MAX_EVIDENCE_ACTIVITY_ENTRIES: usize = 20;
/// Hard byte budget for the echoed Activity milestones. They are persisted
/// and sanitized already, but a long history of large provider results would
/// still make one answer grow past reason, so echoing stops here and what it
/// left out is counted.
const MAX_EVIDENCE_ACTIVITY_BYTES: usize = 20 * 1024;
/// The run-output entries one answer echoes, most recent first. The
/// executor's own buffer is larger; neither the buffer nor this bound is any
/// kind of persistence record, because output is session memory only.
const MAX_EVIDENCE_OUTPUT_ENTRIES: usize = 32;
/// Longest echoed run-output message, in characters.
const MAX_EVIDENCE_MESSAGE_CHARS: usize = 500;
/// Longest echoed provider result per run, in characters. The stored result
/// is already redacted and within `slashit_activity::MAX_RESULT_CHARS`; this
/// is the echo's own tighter bound.
const MAX_EVIDENCE_RESULT_CHARS: usize = 1_000;
/// Longest echoed failure reason, in characters. A reason on an Activity
/// entry is already one bounded line; a Task's `error_message` is not, so it
/// is redacted and cut here.
const MAX_EVIDENCE_FAILURE_CHARS: usize = 500;
/// Longest echoed Task title, in characters. A title is the person's own
/// wording, bounded by the request that created it but by nothing else; the
/// echo bounds it too and flags the cut.
const MAX_EVIDENCE_TITLE_CHARS: usize = 300;
/// Longest echoed model identifier, in characters. The configured model is
/// whatever the Task record names, and the latest observed one is whatever the
/// provider last reported, so neither is assumed well-shaped here. Identity is
/// passed through faithfully up to this bound, redaction included so a
/// credential-shaped word in a reported identifier obeys the same policy as
/// everything else, and the cut is flagged.
const MAX_EVIDENCE_MODEL_CHARS: usize = 200;
/// Hard byte ceiling for one serialized evidence answer.
///
/// Every echoed field is already bounded by a limit above. Their sum —
/// measured on the maximum combination of those bounds, in the test
/// `the_whole_answer_stays_within_the_wire_frame_limit` — lands far below this
/// number, which is itself under [`slashit_ipc::protocol::MAX_REQUEST_BYTES`],
/// the frame limit the client reads a response back through. The check that
/// applies this constant is the explicit aggregate bound: a record written by
/// an older build could carry text today's bounds never earned, so an answer
/// that would exceed the ceiling is refused once, here, rather than sent
/// whole and unreadable at the other end.
const MAX_EVIDENCE_RESPONSE_BYTES: usize = 512 * 1024;

/// What the run-output echo says whenever it carries entries.
const OUTPUT_MEMORY_ONLY_NOTE: &str = "The entries echoed here are held only in this instance's memory. Run output is not persisted: a restart loses all of it, and a client that asks again after one finds none of it. What survives is the Task's own record: its Activity milestones and the run outcomes echoed here.";

/// What the run-output echo says when a Task ran before, under some
/// instance, and this one holds none of the output.
const OUTPUT_NOT_HELD_NOTE: &str = "This instance holds no run output for this Task. Run output lives only in an instance's memory and is not persisted, so what a previous instance held was lost when that instance ended. The Task's own record survives: the runs, their outcomes and its Activity milestones.";

/// What the run-output echo says when nothing has ever run for the Task.
const OUTPUT_NEVER_RUN_NOTE: &str = "This instance holds no run output for this Task, because no run of it is recorded. Run output lives only in memory and is not persisted.";

/// One Task's recent execution evidence, read-only.
#[derive(Debug, serde::Serialize)]
struct TaskEvidence {
    task_id: String,
    /// The Task's own title, redacted, bounded to
    /// [`MAX_EVIDENCE_TITLE_CHARS`], with [`Self::title_truncated`] set when
    /// it was cut.
    title: String,
    /// Whether [`Self::title`] was cut to its echo bound.
    title_truncated: bool,
    /// Which coding Worker the Task runs on, as the Task record keeps it.
    provider: &'static str,
    /// The Task's saved Worker model, as a launch names it (`None` when the
    /// Task asks for the provider default). This is configuration, nothing
    /// more: it is never read back as evidence of what ran.
    configured_model: Option<String>,
    /// Whether [`Self::configured_model`] was cut to its echo bound.
    configured_model_truncated: bool,
    /// The Task's latest observed model: `last_observed_model` on the Task
    /// record, the model the provider last reported while actually running
    /// this Task. It is a Task-level identity, deliberately: SlashIt does not
    /// persist which model served each individual run, so nothing here may be
    /// attributed to one of [`Self::runs`] — a per-run observation would be
    /// invented history. `None` when no run ever reported one, and never
    /// inferred from the configured identifier: an alias is the model
    /// requested, not the model that reported itself.
    latest_observed_model: Option<String>,
    /// Whether [`Self::latest_observed_model`] was cut to its echo bound.
    latest_observed_model_truncated: bool,
    /// What this instance's executor holds for the Task right now. This is
    /// the only part of the answer that dies with the process, and the
    /// session's execution id is this session's evidence only: the run
    /// numbers on [`TaskEvidence::runs`] are the identity that persists.
    session: Option<SessionEvidence>,
    /// Where the Task's most recent run output lives, and what is echoed of
    /// it. Always present, because the answer must say what it does not hold.
    output: OutputEvidence,
    /// The Task's persisted runs, newest first, from its Activity record.
    runs: Vec<RunEvidence>,
    /// How many older persisted runs the answer did not describe.
    runs_omitted: usize,
    /// The latest persisted AI-review evidence. `None` when no review entry
    /// is on the record. A review a later run began after is still returned —
    /// it is the latest review on record — with
    /// [`AiReviewEvidence::superseded_by_run`] naming that newer run.
    ai_review: Option<AiReviewEvidence>,
    /// The Task's persisted review signoff, when one is kept: `approved`,
    /// `fixes_applied` or `rejected`.
    qa_signoff: Option<String>,
    /// Why the Task failed, from its own record or this process's run: the
    /// Task's saved error message first (the durable record), then this
    /// session's failed run. Each persisted run's own failure reason is on
    /// that run below. `None` when neither failed.
    failure_reason: Option<String>,
    /// Whether [`Self::failure_reason`] was cut to its echo bound.
    failure_reason_truncated: bool,
    /// The Task's persisted Activity milestones, newest first, as redacted
    /// echo copies: every string in them goes out under the current redaction
    /// policy, applied at read time, so a milestone written by an older build
    /// is echoed clean even when the stored record predates a masking rule.
    /// Each was redacted and bounded when it was recorded, and the copy here
    /// never changes the stored record.
    activity: Vec<serde_json::Value>,
    /// How many older milestones the answer did not echo, by count or by the
    /// byte budget.
    activity_omitted: usize,
}

/// What this instance's executor holds about the Task's run, if anything.
#[derive(Debug, serde::Serialize)]
struct SessionEvidence {
    /// Whether the executor owns a run for the Task right now — exactly when
    /// stopping it has something to end.
    live: bool,
    /// The owned run's state (`starting`, `running`, `stopping`), or, once
    /// the run is gone, how this session's latest execution for the Task
    /// ended (`stopped`, or `failed` with its reason — the failure text is
    /// externally influenced, so it is redacted and bounded like any echoed
    /// free text). `None` when nothing has run in this session. How the Task
    /// looks after a restart is visible here: absent.
    status: Option<crate::domain::AgentStatus>,
    /// This session's execution identity, when one is recorded. Never
    /// persisted, and never inferred from a run's persisted number.
    execution_id: Option<String>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    stopped_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// One persisted run's evidence, read from the Task's Activity.
#[derive(Debug, serde::Serialize)]
struct RunEvidence {
    /// The run's number: the identity the Activity record gives it (starting
    /// at 1), the same number the Task's timeline and its review records show.
    run: u32,
    /// `started`, `completed`, `failed` or `recovery_required` — the run's
    /// latest recorded end, or `started` while the record says it never
    /// ended. The record is read newest first, so an older end event never
    /// overrules what a newer one decided.
    status: &'static str,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When the record's latest end for this run happened.
    ended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The bounded, one-line reason the record keeps for a failed or
    /// recovered run, redacted under the current policy at read time: a
    /// record written by an older build, before a masking rule existed, is
    /// echoed clean anyway.
    failure_reason: Option<String>,
    /// What the provider reported as the run's final result, re-redacted and
    /// re-bounded for this echo: already redacted and bounded on the record,
    /// and sanitized again at read time so a result a policy predates is
    /// echoed clean. `None` when no result is persisted for the run, which is
    /// also the shape of every run recorded before results were.
    outcome: Option<OutcomeEvidence>,
}

/// One persisted run result, re-bounded for this echo.
#[derive(Debug, serde::Serialize)]
struct OutcomeEvidence {
    /// `available`, `truncated` or `unavailable`, as the record keeps it.
    kind: &'static str,
    /// The recorded redacted text, re-redacted at read time and cut to the
    /// echo's own tighter bound here.
    text: Option<String>,
    /// The full length the record itself says the result had, when the
    /// record cut it (that is `slashit_activity::MAX_RESULT_CHARS`).
    original_chars: Option<u32>,
    /// Characters of the recorded text that this echo left out, when the
    /// echo cut further.
    echo_chars_omitted: Option<u32>,
    /// Why the record has no result, when it recorded one.
    reason: Option<String>,
}

/// The latest persisted AI-review evidence.
#[derive(Debug, serde::Serialize)]
struct AiReviewEvidence {
    /// The review ordinal the Activity record uses.
    review: u32,
    /// `started`, `approved`, `changes_requested`, `fix_failed`, `failed`,
    /// `skipped` or `awaiting_verdict` (the review began and recorded no
    /// verdict yet). A fix pass that ended in an error is `fix_failed` with
    /// its bounded reason — it is not the verdict itself, which was still the
    /// changes request the fix answered, so the two are distinguishable here:
    /// an ordinary request for changes is `changes_requested`.
    outcome: &'static str,
    /// How many issues a changes-requested verdict listed.
    issues: Option<u32>,
    /// The bounded reason a failed or skipped review recorded.
    reason: Option<String>,
    /// The run that began after this review was recorded, when one did. The
    /// review is still the latest one on record, but it predates that run:
    /// it is not evidence about it. `None` while the review is the newest
    /// thing the record holds.
    superseded_by_run: Option<u32>,
}

/// The Task's most recent run output as this instance holds it.
#[derive(Debug, serde::Serialize)]
struct OutputEvidence {
    /// Whether there are entries to see at all.
    available: bool,
    /// Where the held output lives: only this instance's memory. Run output
    /// is never persisted, so the answer carries this sentence on every read.
    memory_only: bool,
    /// The availability in one sentence, so no client has to guess which of
    /// "held here", "an earlier instance lost it" and "nothing ever ran".
    note: &'static str,
    /// How many older entries the echo left out of the executor's buffer.
    entries_omitted: usize,
    /// Whether any echoed message was cut to its own bound.
    truncated: bool,
    /// The echoed entries, most recent first. Each message is already
    /// redacted and cut for this echo; a cut is flagged on the entry.
    entries: Vec<serde_json::Value>,
}

/// `text` cut to `max` characters, and whether the cut happened.
fn evidence_cut(text: &str, max: usize) -> (String, bool) {
    let chars = text.chars().count();
    if chars > max {
        (text.chars().take(max).collect(), true)
    } else {
        (text.to_string(), false)
    }
}

/// Free text the answer echoes, redacted first and cut after, so a mask is
/// never cut in half. `redact` is idempotent, so already-redacted activity
/// text passes through unchanged.
fn evidence_text(text: &str, max: usize) -> (String, bool) {
    evidence_cut(&slashit_activity::redact(text), max)
}

/// One failure reason, redacted and cut to the failure-echo bound. A reason
/// on an Activity entry was redacted and bounded to
/// `slashit_activity::MAX_REASON_CHARS` when it was recorded; a Task's
/// `error_message` was not, so every echoed reason passes through here.
fn evidence_failure_reason(reason: &str) -> (String, bool) {
    evidence_text(reason, MAX_EVIDENCE_FAILURE_CHARS)
}

/// Echo the run output this instance holds, bounded, most recent first.
///
/// Always an answer, never silence: what this read *did not* hold is evidence
/// too, and the note says it in one sentence.
fn evidence_output(
    entries: Vec<crate::domain::AgentLogEntry>,
    persisted_evidence: bool,
) -> OutputEvidence {
    let total = entries.len();
    let hold = entries
        .into_iter()
        .rev()
        .take(MAX_EVIDENCE_OUTPUT_ENTRIES)
        .collect::<Vec<_>>();
    let mut truncated = false;
    let mut echoed = Vec::with_capacity(hold.len());
    for entry in hold {
        let (message, cut) = evidence_text(&entry.message, MAX_EVIDENCE_MESSAGE_CHARS);
        truncated |= cut;
        echoed.push(serde_json::json!({
            "timestamp": entry.timestamp,
            "level": entry.level,
            "message": message,
            "message_truncated": cut,
        }));
    }

    let note = if total > 0 {
        OUTPUT_MEMORY_ONLY_NOTE
    } else if persisted_evidence {
        OUTPUT_NOT_HELD_NOTE
    } else {
        OUTPUT_NEVER_RUN_NOTE
    };
    OutputEvidence {
        available: total > 0,
        memory_only: true,
        note,
        entries_omitted: total.saturating_sub(MAX_EVIDENCE_OUTPUT_ENTRIES),
        truncated,
        entries: echoed,
    }
}

/// The Task's failure, for the answer. The Task's own saved error message
/// comes first — it is the durable, Task-failure record — then this session's
/// failed run. Persisted run failures are not folded in here: each run's own
/// reason is on its [`RunEvidence`], which names the run it belongs to.
fn evidence_failure(
    task: &crate::domain::Task,
    snapshot: Option<&TaskExecutorSnapshot>,
) -> (Option<String>, bool) {
    if let Some(message) = &task.error_message {
        let (text, cut) = evidence_failure_reason(message);
        return (Some(text), cut);
    }
    if let Some(reason) = snapshot.and_then(|s| match &s.status {
        Some(crate::domain::AgentStatus::Failed(reason)) => Some(reason.as_str()),
        _ => None,
    }) {
        let (text, cut) = evidence_failure_reason(reason);
        return (Some(text), cut);
    }
    (None, false)
}

/// The session's run state, as the evidence echoes it. A `failed` state
/// carries the run's recorded error message — externally influenced text,
/// since a provider error shaped it — so it is redacted and cut to the same
/// bound a persisted failure gets; the other states are fixed identifiers
/// and pass through.
fn evidence_session_status(
    status: Option<crate::domain::AgentStatus>,
) -> Option<crate::domain::AgentStatus> {
    status.map(|status| match status {
        crate::domain::AgentStatus::Failed(reason) => {
            let (reason, _) = evidence_failure_reason(&reason);
            crate::domain::AgentStatus::Failed(reason)
        }
        other => other,
    })
}

/// The persisted runs, newest first, bounded, from the Activity record.
fn evidence_runs(activity: &[slashit_activity::Entry]) -> (Vec<RunEvidence>, usize) {
    use slashit_activity::Kind;

    // The runs the record knows, ascending: a run exists once anything names it.
    let mut numbers: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    for entry in activity {
        match &entry.kind {
            Kind::RunStarted { run, .. }
            | Kind::ToolUsed { run, .. }
            | Kind::ToolsOmitted { run, .. }
            | Kind::RunCompleted { run }
            | Kind::RunResult { run, .. }
            | Kind::NewAttemptAuthorized { run } => {
                numbers.insert(*run);
            }
            Kind::RunFailed { run: Some(run), .. }
            | Kind::RecoveryRequired { run: Some(run), .. } => {
                numbers.insert(*run);
            }
            _ => {}
        }
    }

    let total = numbers.len();
    let mut runs = Vec::with_capacity(total.min(MAX_EVIDENCE_RUNS));
    for run in numbers.iter().rev().take(MAX_EVIDENCE_RUNS) {
        let mut evidence = RunEvidence {
            run: *run,
            status: "started",
            started_at: None,
            ended_at: None,
            failure_reason: None,
            outcome: None,
        };
        // The Activity is read newest first here, so the first valid end
        // event matched below is the run's latest one, and once an end has
        // settled, every older one is skipped: the run's status, its
        // `ended_at` and its failure reason all come from that one, latest
        // end event, and an older end of a different kind — or another
        // run's events, which never match the guard — cannot override it.
        let mut ended = false;
        for entry in activity.iter().rev() {
            match &entry.kind {
                Kind::RunStarted { run: r, .. } if r == run => {
                    if evidence.started_at.is_none() {
                        evidence.started_at = Some(entry.at);
                    }
                }
                Kind::RunCompleted { run: r } if r == run && !ended => {
                    ended = true;
                    evidence.status = "completed";
                    evidence.ended_at = Some(entry.at);
                }
                Kind::RunFailed {
                    run: Some(r),
                    reason,
                } if r == run && !ended => {
                    ended = true;
                    evidence.status = "failed";
                    evidence.ended_at = Some(entry.at);
                    // A persisted reason was redacted when it was recorded;
                    // the echo re-redacts it so a reason a policy predates is
                    // never echoed as it was stored.
                    evidence.failure_reason = Some(evidence_failure_reason(reason).0);
                }
                Kind::RecoveryRequired {
                    run: Some(r),
                    reason,
                } if r == run && !ended => {
                    ended = true;
                    evidence.status = "recovery_required";
                    evidence.ended_at = Some(entry.at);
                    evidence.failure_reason = Some(evidence_failure_reason(reason).0);
                }
                Kind::RunResult { run: r, result } if r == run => {
                    evidence
                        .outcome
                        .get_or_insert_with(|| evidence_outcome(result));
                }
                _ => {}
            }
        }
        runs.push(evidence);
    }
    (runs, total.saturating_sub(MAX_EVIDENCE_RUNS))
}

/// One persisted run result, re-bounded and re-redacted for this echo.
fn evidence_outcome(result: &slashit_activity::RunResult) -> OutcomeEvidence {
    match result {
        slashit_activity::RunResult::Available { text } => OutcomeEvidence {
            kind: "available",
            ..evidence_excerpt(text)
        },
        slashit_activity::RunResult::Truncated {
            text,
            original_chars,
        } => OutcomeEvidence {
            kind: "truncated",
            original_chars: Some(*original_chars),
            ..evidence_excerpt(text)
        },
        slashit_activity::RunResult::Unavailable { reason } => OutcomeEvidence {
            kind: "unavailable",
            text: None,
            original_chars: None,
            echo_chars_omitted: None,
            // A reason in an `Unavailable` result is provider text too. It was
            // redacted and bounded when it was recorded; a record written
            // before that policy existed is sanitized again on the way out.
            reason: Some(evidence_failure_reason(reason).0),
        },
    }
}

/// `text`, redacted again at read time, cut to the echo's bound.
///
/// A result was redacted when it was recorded, but the stored record is
/// historical: a result written by an older build — before a masking rule
/// existed, or before it was this strong — is echoed under today's policy,
/// not the writer's. Redaction comes before the cut, so a mask is never cut
/// in half.
fn evidence_excerpt(text: &str) -> OutcomeEvidence {
    let redacted = slashit_activity::redact(text);
    let chars = redacted.chars().count();
    let (shown, omitted) = if chars <= MAX_EVIDENCE_RESULT_CHARS {
        (redacted, None)
    } else {
        (
            redacted.chars().take(MAX_EVIDENCE_RESULT_CHARS).collect(),
            Some(u32::try_from(chars - MAX_EVIDENCE_RESULT_CHARS).unwrap_or(u32::MAX)),
        )
    };
    OutcomeEvidence {
        kind: "",
        text: Some(shown),
        original_chars: None,
        echo_chars_omitted: omitted,
        reason: None,
    }
}

/// The latest persisted AI-review evidence, from the Activity record.
///
/// Scanning newest first stops at the first review or fix event: a run that
/// later began does not erase the review, but marks it
/// [`AiReviewEvidence::superseded_by_run`] — the review is still the latest
/// on record and is returned, with the run that followed it named, never
/// attributed to that run. `None` only when no review record exists at all.
fn evidence_ai_review(activity: &[slashit_activity::Entry]) -> Option<AiReviewEvidence> {
    use slashit_activity::Kind;

    // The run that began most recently and still precedes the first review
    // event the scan reaches: it started after that review was recorded.
    // Run starts that follow the review event in the record (they are older
    // than it) are not reached, because the scan stops there.
    let mut superseded_by_run: Option<u32> = None;

    for entry in activity.iter().rev() {
        match &entry.kind {
            Kind::RunStarted { run, .. } => {
                superseded_by_run.get_or_insert(*run);
            }
            Kind::AiReviewStarted { review } => {
                return Some(AiReviewEvidence {
                    review: *review,
                    outcome: "awaiting_verdict",
                    issues: None,
                    reason: None,
                    superseded_by_run,
                });
            }
            Kind::AiReviewApproved { review } => {
                return Some(AiReviewEvidence {
                    review: *review,
                    outcome: "approved",
                    issues: None,
                    reason: None,
                    superseded_by_run,
                });
            }
            Kind::AiReviewChangesRequested { review, issues } => {
                return Some(AiReviewEvidence {
                    review: *review,
                    outcome: "changes_requested",
                    issues: Some(*issues),
                    reason: None,
                    superseded_by_run,
                });
            }
            Kind::AiReviewFailed { review, reason } => {
                return Some(AiReviewEvidence {
                    review: *review,
                    outcome: "failed",
                    issues: None,
                    // Re-bounded here: the record keeps at most
                    // `MAX_REASON_CHARS`, within the failure bound, so the
                    // cut flag the bound would set cannot fire.
                    reason: Some(evidence_failure_reason(reason).0),
                    superseded_by_run,
                });
            }
            Kind::AiReviewSkipped { review, reason } => {
                return Some(AiReviewEvidence {
                    review: *review,
                    outcome: "skipped",
                    issues: None,
                    reason: Some(evidence_failure_reason(reason).0),
                    superseded_by_run,
                });
            }
            // A fix pass is the follow-up of a changes-requested verdict, so
            // a fix under way or finished without incident leaves the verdict
            // to report as that one.
            Kind::AiFixStarted { review }
            | Kind::AiFixApplied { review }
            | Kind::AiFixUnchanged { review } => {
                return Some(AiReviewEvidence {
                    review: *review,
                    outcome: "changes_requested",
                    issues: None,
                    reason: None,
                    superseded_by_run,
                });
            }
            // A fix pass that itself ended in an error is reported as such,
            // with its bounded reason, rather than blurred into the verdict
            // it tried to answer: the reviewer asked for changes, the fix
            // could not deliver them, and the persistence keeps both facts.
            Kind::AiFixFailed { review, reason } => {
                return Some(AiReviewEvidence {
                    review: *review,
                    outcome: "fix_failed",
                    issues: None,
                    // Re-bounded here, exactly like a failed review's reason:
                    // the record keeps at most `MAX_REASON_CHARS`, within the
                    // failure bound, so the cut flag the bound would set
                    // cannot fire.
                    reason: Some(evidence_failure_reason(reason).0),
                    superseded_by_run,
                });
            }
            _ => {}
        }
    }
    None
}

/// The echoed Activity milestones, newest first, within both bounds.
///
/// Each milestone is a *copy* of the stored entry, redacted again on the way
/// out. Entries were sanitized when they were written, but the stored record
/// is historical: one written by an older build — before a masking rule
/// existed, or before it was this strong — is echoed under today's policy.
/// The stored record itself is never touched.
fn evidence_activity(activity: &[slashit_activity::Entry]) -> (Vec<serde_json::Value>, usize) {
    let mut echoed = Vec::new();
    let mut bytes = 0usize;
    let mut taken = 0usize;
    for entry in activity.iter().rev() {
        if taken == MAX_EVIDENCE_ACTIVITY_ENTRIES {
            break;
        }
        let Some(mut value) = serde_json::to_value(entry).ok() else {
            continue;
        };
        evidence_redact_value(&mut value);
        // A stored entry is bounded, so no single entry can reasonably blow
        // the budget; the one-decision cut is here, on what is already held.
        let cost = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0);
        if !echoed.is_empty() && bytes + cost > MAX_EVIDENCE_ACTIVITY_BYTES {
            break;
        }
        bytes += cost;
        taken += 1;
        echoed.push(value);
    }
    (echoed, activity.len().saturating_sub(taken))
}

/// The redaction policy, applied to every string `value` carries, in place.
///
/// `redact` is idempotent, so text a compliant record already holds passes
/// through unchanged; a credential that a historical record kept intact is
/// masked here, on the echo copy only.
fn evidence_redact_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = slashit_activity::redact(text),
        serde_json::Value::Array(items) => {
            for item in items {
                evidence_redact_value(item);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                evidence_redact_value(item);
            }
        }
        _ => {}
    }
}

/// The IPC-side instance snapshot this evidence reads run state from.
type TaskExecutorSnapshot = crate::queue::executor::TaskRunSnapshot;

/// Read one Task's recent execution evidence. Read-only.
///
/// The Task record and its Activity come from the Task store; the run state
/// and the output come from the executor's own registry, the same reads the
/// desktop's `get_task_run` and `get_task_output` commands make. What is
/// answered, how much of it, and which part dies with this process are all
/// explicit in the answer.
async fn handle_get_task_evidence(ctx: &IpcContext, task_id: String) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
    };

    // Read the Task briefly and work on the clone: the persistently-sourced
    // half of the answer must not race with a lifecycle write that is still
    // holding the store.
    let task = {
        let tasks = ctx.tasks.read().await;
        match tasks.get(&task_uuid) {
            Some(task) => task.clone(),
            None => return IpcResponse::error(format!("Task {task_id} not found")),
        }
    };

    // The executor's registry — the same reads the desktop makes.
    let executor = ctx.executor.get();
    let (snapshot, output_entries) = match executor {
        Some(executor) => {
            let snapshot = executor.task_run(task_uuid).await;
            let output = executor.get_task_output(task_uuid).await;
            (Some(snapshot), output)
        }
        None => (None, Vec::new()),
    };

    let session = snapshot.as_ref().map(|snapshot| SessionEvidence {
        live: snapshot.live,
        // A failed state names the run's error, so it goes out as any echoed
        // free text does: redacted and bounded.
        status: evidence_session_status(snapshot.status.clone()),
        execution_id: snapshot.last_execution.as_ref().map(|e| e.id.to_string()),
        started_at: snapshot.last_execution.as_ref().map(|e| e.started_at),
        stopped_at: snapshot.last_execution.as_ref().and_then(|e| e.stopped_at),
    });

    let (runs, runs_omitted) = evidence_runs(&task.activity);
    let ai_review = evidence_ai_review(&task.activity);
    let (failure_reason, failure_reason_truncated) = evidence_failure(&task, snapshot.as_ref());

    // The title and the model identifiers are echoed through the same
    // redaction and bound the rest of the answer obeys; each cut is flagged
    // rather than silent.
    let (title, title_truncated) = evidence_text(&task.title, MAX_EVIDENCE_TITLE_CHARS);
    let (configured_model, configured_model_truncated) = match task.configured_model() {
        Some(model) => {
            let (model, cut) = evidence_text(model, MAX_EVIDENCE_MODEL_CHARS);
            (Some(model), cut)
        }
        None => (None, false),
    };
    let (latest_observed_model, latest_observed_model_truncated) = match &task.last_observed_model {
        Some(model) => {
            let (model, cut) = evidence_text(model, MAX_EVIDENCE_MODEL_CHARS);
            (Some(model), cut)
        }
        None => (None, false),
    };

    let qa_signoff = task.qa_signoff.as_ref().map(|s| match s.status {
        crate::domain::QaStatus::Approved => "approved".to_string(),
        crate::domain::QaStatus::FixesApplied => "fixes_applied".to_string(),
        crate::domain::QaStatus::Rejected => "rejected".to_string(),
    });

    let output = evidence_output(output_entries, !runs.is_empty());
    let (activity, activity_omitted) = evidence_activity(&task.activity);

    let answer = TaskEvidence {
        task_id: task.id.to_string(),
        title,
        title_truncated,
        provider: task.provider.label(),
        configured_model,
        configured_model_truncated,
        latest_observed_model,
        latest_observed_model_truncated,
        session,
        output,
        runs,
        runs_omitted,
        ai_review,
        qa_signoff,
        failure_reason,
        failure_reason_truncated,
        activity,
        activity_omitted,
    };

    evidence_reply(&task_id, answer)
}

/// Serialize one assembled answer, apply the aggregate bound and wrap it.
///
/// The aggregate bound ([`MAX_EVIDENCE_RESPONSE_BYTES`]) applies here, to the
/// whole response rather than to any field: individual bounds could stay
/// within reason field by field and still blow the client's frame limit
/// together. Too large is refused whole — trimming silently could make a
/// client mistake what is missing for evidence that it does not exist.
fn evidence_reply(task_id: &str, answer: TaskEvidence) -> IpcResponse {
    match serde_json::to_string(&answer) {
        Ok(text) if text.len() <= MAX_EVIDENCE_RESPONSE_BYTES => {
            match serde_json::from_str(&text) {
                Ok(value) => IpcResponse::success(value),
                Err(e) => {
                    IpcResponse::error(format!("Could not encode the evidence for {task_id}: {e}"))
                }
            }
        }
        Ok(_) => IpcResponse::error(format!(
            "The evidence answer for {task_id} would exceed the response bound and was not sent"
        )),
        Err(e) => IpcResponse::error(format!("Could not encode the evidence for {task_id}: {e}")),
    }
}

async fn handle_create_task(
    ctx: &IpcContext,
    project_id: String,
    title: String,
    description: Option<String>,
    priority: Option<String>,
    review_mode: Option<String>,
) -> IpcResponse {
    let project_uuid = match Uuid::parse_str(&project_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid project_id: {project_id}")),
    };
    let review_mode = match review_mode.as_deref() {
        None | Some("automatic") => crate::domain::task::ReviewMode::Automatic,
        Some("manual") => crate::domain::task::ReviewMode::Manual,
        Some(other) => {
            return IpcResponse::error(format!(
                "Invalid review mode: {other}. Use manual or automatic."
            ))
        }
    };

    // Verify project exists and get its name
    let project_name = {
        let projects = ctx.projects.read().await;
        match projects.get(&project_uuid) {
            Some(p) => p.name.clone(),
            None => return IpcResponse::error(format!("Project {project_id} not found")),
        }
    };

    let priority = parse_priority(priority.as_deref());
    let new = crate::domain::NewTask {
        id: Uuid::new_v4(),
        project_id: project_uuid,
        title,
        description,
        model: "default".to_string(),
        // No reviewer is chosen by this command; a person sets one on the Task.
        reviewer_model: None,
        planning_mode: false,
        dependencies: Vec::new(),
        category: Default::default(),
        priority,
        complexity: Default::default(),
        impact: Default::default(),
        security_severity: Default::default(),
        github_issue_url: None,
        gitlab_issue_url: None,
        linear_ticket_id: None,
    };
    let result = crate::lifecycle::create(
        &ctx.project_gate,
        &ctx.tasks,
        &ctx.projects,
        &ctx.storage,
        project_uuid,
        move |existing| {
            let mut task = crate::domain::Task::new_backlog(existing, new);
            task.review_mode = review_mode;
            task
        },
    )
    .await;

    match result {
        Ok(task) => {
            let summary = task_to_summary(&task, &project_name);
            IpcResponse::success(serde_json::to_value(summary).unwrap_or_default())
        }
        Err(e) => IpcResponse::error(e),
    }
}

/// What the command line is told when asked to move or queue a Task that is in
/// Human Review. Leaving Human Review (to Done, Queue, or any other column) is a
/// human decision recorded in the application, and a typed command cannot
/// confirm it, so the command line makes no such move at all.
pub(crate) const CLI_FROM_HUMAN_REVIEW_REFUSAL: &str =
    "This task is in Human Review, awaiting an explicit human review decision, so it was left \
     unchanged. The command line does not move or queue it. Make that decision in the SlashIt \
     application's Human Review panel, where it is recorded.";

/// Move a task to another column from the CLI.
///
/// A move into `Done` is the same terminal claim the desktop app makes, and
/// takes the same route: [`crate::lifecycle::terminalize`] removes the worktree
/// first and commits the status only if that succeeded. This handler used to
/// assign the status directly and had no worktree manager at all, so
/// `slashit move <task> done` finished tasks on the board and left their
/// checkouts behind for a periodic sweep to notice.
///
/// Every other move takes the task's lifecycle lease, so it cannot land in the
/// middle of a cleanup and be overwritten by the commit that cleanup is about
/// to make.
async fn handle_move_task(ctx: &IpcContext, task_id: String, status: String) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
    };

    let new_status = match parse_status(&status) {
        Some(s) => s,
        None => return IpcResponse::error(format!("Invalid status: {status}")),
    };

    if matches!(new_status, crate::domain::TaskStatus::Done) {
        // Taken under the lease, and decided on the status read under it, as the
        // desktop's close is, so the refusal comes before terminalization begins.
        let _lease = match ctx.task_lifecycle_locks.acquire(task_uuid).await {
            Ok(lease) => lease,
            Err(e) => return IpcResponse::error(e),
        };
        let current = ctx.tasks.read().await.get(&task_uuid).map(|t| t.status.clone());
        if current == Some(crate::domain::TaskStatus::HumanReview) {
            return IpcResponse::error(CLI_FROM_HUMAN_REVIEW_REFUSAL.to_string());
        }
        return match crate::lifecycle::terminalize_leased(
            terminalize_ctx(ctx),
            task_uuid,
            crate::lifecycle::Origin::User,
            crate::lifecycle::TerminalizeRequest::new(new_status),
        )
        .await
        {
            Ok(task) => {
                let projects = ctx.projects.read().await;
                let pname = projects
                    .get(&task.project_id)
                    .map(|p| p.name.as_str())
                    .unwrap_or("?");
                let summary = task_to_summary(&task, pname);
                IpcResponse::success(serde_json::to_value(summary).unwrap_or_default())
            }
            Err(crate::lifecycle::TerminalizeRefusal::TaskNotFound) => {
                IpcResponse::error(format!("Task {task_id} not found"))
            }
            Err(refusal) => IpcResponse::error(refusal.to_string()),
        };
    }

    let _lease = match ctx.task_lifecycle_locks.acquire(task_uuid).await {
        Ok(lease) => lease,
        Err(e) => return IpcResponse::error(e),
    };

    let (old_status, unfinished_integration, queue_refusal, repeated_queue) = match ctx
        .tasks
        .read()
        .await
        .get(&task_uuid)
        .map(|t| {
            (
                t.status.clone(),
                t.integration_unfinished_refusal(),
                crate::lifecycle::queue_move_refusal(t, &new_status),
                crate::lifecycle::is_repeated_queue_move(t, &new_status),
            )
        })
    {
        Some(found) => found,
        None => return IpcResponse::error(format!("Task {task_id} not found")),
    };

    // A Task in Human Review leaves that column only through the application's
    // own decision (approve, request changes, close, or the desktop queue
    // action). A typed move out of it would discard the pending decision, so it
    // is refused for every target, before an owner is ended or anything is
    // written. The same move from the desktop is not refused here.
    if old_status == crate::domain::TaskStatus::HumanReview && new_status != old_status {
        return IpcResponse::error(CLI_FROM_HUMAN_REVIEW_REFUSAL.to_string());
    }

    // The same answer as the desktop `update_task_status`: a request for the
    // column a queued Task already holds returns the Task as it is, and writes
    // nothing, so its error and recovery evidence survives.
    if repeated_queue {
        let current = ctx.tasks.read().await.get(&task_uuid).cloned();
        return match current {
            Some(task) => task_summary_reply(ctx, &task).await,
            None => IpcResponse::error(format!("Task {task_id} not found")),
        };
    }

    // Same as the desktop `update_task_status`: a pending local integration
    // keeps the task in the column it is recovered from.
    if old_status != new_status {
        if let Some(refusal) = unfinished_integration {
            return IpcResponse::error(refusal);
        }
    }

    // Same as the desktop `update_task_status`: a move into Queue is refused
    // for the same reasons as every other queue entry, before an owner is
    // ended or anything is written.
    if let Some(refusal) = queue_refusal {
        return IpcResponse::error(refusal);
    }

    // Same as the desktop `update_task_status`: a move into In Progress is
    // refused while new work is paused for disk space, before anything else.
    if let Err(e) = ctx.start_guard.check_move(&old_status, &new_status).await {
        return IpcResponse::error(e);
    }

    // Same rule as the desktop `update_task_status`: any status this handler
    // reaches actually changes the task's lifecycle column (`Done` returned
    // above, through the same `terminalize` that already refuses under an
    // active owner), so a previous owner must be ended before the durable
    // write below, under the lease this handler already holds -- or the
    // whole move is refused, exactly like a durable-write failure is.
    if old_status != new_status {
        let running = ctx
            .executor
            .get()
            .map(|e| e.as_ref() as &dyn crate::lifecycle::ExecutionOwnership);
        if let Err(e) = crate::lifecycle::end_active_ownership(running, task_uuid).await {
            return IpcResponse::error(e);
        }
        // Ending the owner can leave a Worker in recovery, so a move into
        // Queue is checked again under the lease before the write.
        let recorded = ctx
            .tasks
            .read()
            .await
            .get(&task_uuid)
            .and_then(|t| crate::lifecycle::queue_move_refusal(t, &new_status));
        if let Some(refusal) = recorded {
            return IpcResponse::error(refusal);
        }
    }

    // A move from Queue into In Progress is a promotion: it takes an admission
    // permit before its write, as the scheduler's own promotion does.
    if old_status == crate::domain::TaskStatus::Queue
        && new_status == crate::domain::TaskStatus::InProgress
    {
        let Some(executor) = ctx.executor.get() else {
            return IpcResponse::error(crate::commands::queue::NO_EXECUTOR_PROMOTION.to_string());
        };
        let record_move = move |staged: &mut std::collections::HashMap<Uuid, crate::domain::Task>,
                                promoted_id: Uuid| {
            if let Some(task) = staged.get_mut(&promoted_id) {
                task.record_move(&crate::domain::TaskStatus::Queue);
            }
        };
        return match executor
            .promote_queued(crate::queue::PromotionChoice::Exact(task_uuid), record_move)
            .await
        {
            Ok(crate::queue::PromotionOutcome::Promoted(task)) => task_summary_reply(ctx, &task).await,
            Ok(refused) => IpcResponse::error(
                executor.queued_promotion_refusal(Some(task_uuid), &refused).await,
            ),
            Err(e) => IpcResponse::error(e),
        };
    }

    // The same classifier the desktop app uses, through the same
    // stage/persist/publish primitive: this handler used to assign
    // `task.status` directly, which left a task moved e.g. `Failed ->
    // InProgress` with its stale `phase = Failed` and `error_message`
    // intact, so it said "running" while failing every readiness check.
    let new_status_for_amend = new_status.clone();
    let effect = crate::lifecycle::classify_status_transition(&old_status, &new_status_for_amend);
    let amend = move |staged: &mut std::collections::HashMap<Uuid, crate::domain::Task>| {
        if let Some(task) = staged.get_mut(&task_uuid) {
            let from = task.status.clone();
            task.status = new_status_for_amend.clone();
            task.record_move(&from);
            if let crate::lifecycle::StatusTransitionEffect::ResetExecutionState = effect {
                task.reset_execution_state();
            }
        }
    };

    match crate::lifecycle::commit_task(&ctx.tasks, &ctx.storage, task_uuid, &amend).await {
        Ok(Some(task)) => task_summary_reply(ctx, &task).await,
        Ok(None) => IpcResponse::error(format!("Task {task_id} not found")),
        Err(e) => IpcResponse::error(e),
    }
}

/// The reply that carries a Task's summary, for a move that wrote the Task or
/// left it as it was.
async fn task_summary_reply(ctx: &IpcContext, task: &crate::domain::Task) -> IpcResponse {
    let projects = ctx.projects.read().await;
    let pname = projects
        .get(&task.project_id)
        .map(|p| p.name.as_str())
        .unwrap_or("?");
    let summary = task_to_summary(task, pname);
    IpcResponse::success(serde_json::to_value(summary).unwrap_or_default())
}

/// The handles [`crate::lifecycle::terminalize`] needs, out of an
/// [`IpcContext`].
fn terminalize_ctx(ctx: &IpcContext) -> crate::lifecycle::TerminalizeCtx<'_> {
    crate::lifecycle::TerminalizeCtx {
        tasks: &ctx.tasks,
        projects: &ctx.projects,
        repositories: &ctx.repositories,
        worktree_manager: &ctx.worktree_manager,
        storage: &ctx.storage,
        authority: &ctx.task_lifecycle_locks,
        running: ctx
            .executor
            .get()
            .map(|e| e.as_ref() as &dyn crate::lifecycle::ExecutionOwnership),
    }
}

async fn handle_edit_task(
    ctx: &IpcContext,
    task_id: String,
    title: Option<String>,
    description: Option<String>,
    priority: Option<String>,
) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
    };

    let amend = move |staged: &mut std::collections::HashMap<Uuid, crate::domain::Task>| {
        if let Some(task) = staged.get_mut(&task_uuid) {
            if let Some(t) = title.clone() {
                task.title = t;
            }
            if let Some(d) = description.clone() {
                task.description = Some(d);
            }
            if let Some(p) = priority.clone() {
                task.priority = parse_priority(Some(&p));
            }
        }
    };

    match crate::lifecycle::commit_task(&ctx.tasks, &ctx.storage, task_uuid, &amend).await {
        Ok(Some(task)) => {
            let projects = ctx.projects.read().await;
            let pname = projects
                .get(&task.project_id)
                .map(|p| p.name.as_str())
                .unwrap_or("?");
            let summary = task_to_summary(&task, pname);
            IpcResponse::success(serde_json::to_value(summary).unwrap_or_default())
        }
        Ok(None) => IpcResponse::error(format!("Task {task_id} not found")),
        Err(e) => IpcResponse::error(e),
    }
}

/// Delete a task through the same lifecycle-aware operation the desktop uses.
///
/// This used to be a bare `HashMap::remove` plus a save: no lease, no execution
/// check, no cleanup. `slashit delete` on a task with a live checkout removed
/// the only record naming that directory and that branch, left the directory on
/// disk, and reported success -- and, holding no lease, it could do it while a
/// cleanup was mid-removal, overwriting the `cleanup_in_flight` stamp that the
/// next start relies on. There is one delete now, and both front doors call it.
async fn handle_delete_task(ctx: &IpcContext, task_id: String) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
    };

    match crate::lifecycle::delete(terminalize_ctx(ctx), task_uuid).await {
        Ok(true) => IpcResponse::success(serde_json::json!({"deleted": task_id})),
        Ok(false) => IpcResponse::error(format!("Task {task_id} not found")),
        Err(refusal) => IpcResponse::error(refusal.to_string()),
    }
}

/// Set a Task's Worker model from the command line.
///
/// The field is written through the desktop's own `update_task` boundary
/// (`commands::task::set_task_model_and_review_mode`), so the persisted board
/// ends in the same state as that command's for the same input. The
/// identifier is checked by `domain::task::parse_worker_model`, the same rule
/// the desktop write applies, and an identifier that is refused leaves the
/// Task and its board as they were.
async fn handle_set_task_model(ctx: &IpcContext, task_id: String, model: String) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
    };
    let provider = match ctx.tasks.read().await.get(&task_uuid) {
        Some(task) => task.provider,
        None => return IpcResponse::error(format!("Task {task_id} not found")),
    };
    let model = match crate::domain::task::parse_worker_model(provider, &model) {
        Ok(model) => model,
        Err(refusal) => return IpcResponse::error(refusal),
    };
    match crate::commands::task::set_task_model_and_review_mode(
        &ctx.tasks,
        &ctx.storage,
        task_uuid,
        Some(model),
        None,
    )
    .await
    {
        Ok(Some(task)) => task_summary_reply(ctx, &task).await,
        Ok(None) => IpcResponse::error(format!("Task {task_id} not found")),
        Err(e) => IpcResponse::error(e),
    }
}

/// Set how a Task's finished coding runs are reviewed from the command line.
///
/// Written through `commands::task::set_task_model_and_review_mode`, as the
/// desktop's `update_task` stores the field, so a run that already started
/// keeps the mode it started with and the write adopts the refusals that
/// boundary gives. The request may carry only the two modes the desktop names,
/// in the same words.
async fn handle_set_task_review_mode(
    ctx: &IpcContext,
    task_id: String,
    review_mode: String,
) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
    };
    let mode = match review_mode.as_str() {
        "automatic" => crate::domain::task::ReviewMode::Automatic,
        "manual" => crate::domain::task::ReviewMode::Manual,
        other => {
            return IpcResponse::error(format!(
                "Invalid review mode: {other}. Use manual or automatic."
            ))
        }
    };
    match crate::commands::task::set_task_model_and_review_mode(
        &ctx.tasks,
        &ctx.storage,
        task_uuid,
        None,
        Some(mode),
    )
    .await
    {
        Ok(Some(task)) => task_summary_reply(ctx, &task).await,
        Ok(None) => IpcResponse::error(format!("Task {task_id} not found")),
        Err(e) => IpcResponse::error(e),
    }
}

/// Replace a Task's structured dependencies from the command line.
///
/// The write is the desktop command's own
/// (`commands::task::set_task_dependencies_via`) -- the same parser, the same
/// lifecycle lease and the same refusal messages -- so a cycle, a
/// cross-Project dependency, a malformed identifier, a second dependency, a
/// Task that holds a checkout or one that is awaiting a decision changes
/// nothing and answers with the refusal the desktop gives.
async fn handle_set_task_dependencies(
    ctx: &IpcContext,
    task_id: String,
    dependencies: Vec<String>,
) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
    };
    match crate::commands::task::set_task_dependencies_via(
        &ctx.tasks,
        &ctx.storage,
        &ctx.task_lifecycle_locks,
        task_uuid,
        dependencies,
    )
    .await
    {
        Ok(Some(task)) => task_summary_reply(ctx, &task).await,
        Ok(None) => IpcResponse::error(format!("Task {task_id} not found")),
        Err(e) => IpcResponse::error(e),
    }
}

async fn handle_queue_status(ctx: &IpcContext) -> IpcResponse {
    let queue_mgr = ctx.queue_manager.read().await;
    let queued_count = queue_mgr.get_queued_tasks().await.len();
    let in_progress_count = queue_mgr.get_in_progress_count().await;
    let config = queue_mgr.config();

    let info = QueueStatusInfo {
        queued_count,
        in_progress_count,
        parallel_limit: config.parallel_task_limit,
        auto_promote: config.auto_promote,
        fifo_ordering: config.fifo_ordering,
    };

    IpcResponse::success(serde_json::to_value(info).unwrap_or_default())
}

/// Queue a Task from the CLI. The same entry point as the desktop's Add to
/// Queue, so the two cannot leave a Task in different states. See
/// `lifecycle::enqueue_current`.
async fn handle_enqueue_task(ctx: &IpcContext, task_id: String) -> IpcResponse {
    let task_uuid = match Uuid::parse_str(&task_id) {
        Ok(id) => id,
        Err(_) => return IpcResponse::error(format!("Invalid task_id: {task_id}")),
    };

    let running = ctx
        .executor
        .get()
        .map(|e| e.as_ref() as &dyn crate::lifecycle::ExecutionOwnership);
    // A Task in Human Review is not queued from the command line: that would
    // leave the review without its decision and withdraw its approval.
    match crate::lifecycle::enqueue_current_refusing_human_review(
        &ctx.tasks,
        &ctx.storage,
        &ctx.task_lifecycle_locks,
        running,
        task_uuid,
        CLI_FROM_HUMAN_REVIEW_REFUSAL,
    )
    .await
    {
        Ok(verdict) => match verdict.refusal(task_uuid) {
            Some(refusal) => IpcResponse::error(refusal),
            None => IpcResponse::success(serde_json::json!({"enqueued": task_id})),
        },
        Err(e) => IpcResponse::error(e),
    }
}

async fn handle_list_terminals(ctx: &IpcContext) -> IpcResponse {
    let sessions = ctx.pty.sessions.lock().await;
    let summaries: Vec<TerminalSummary> = sessions
        .values()
        .map(|s| TerminalSummary {
            id: s.id.to_string(),
            name: s.name.clone(),
            cols: s.cols as usize,
            rows: s.rows as usize,
        })
        .collect();

    IpcResponse::success(serde_json::to_value(summaries).unwrap_or_default())
}

/// Bring the window forward, if this instance has one.
///
/// A daemon reports the failure instead of quietly succeeding: `slashit show`
/// against a headless instance should say there is no window, not claim it
/// raised one.
fn handle_show(ctx: &IpcContext) -> IpcResponse {
    match ctx.control.show_window() {
        Ok(()) => IpcResponse::success(serde_json::json!({"shown": true})),
        Err(e) => IpcResponse::error(e),
    }
}

/// Ask the instance to stop.
///
/// The quit itself is deferred to the server, which performs it only after this
/// response has been written; see [`Dispatch::then_quit`].
fn handle_quit() -> Dispatch {
    Dispatch {
        response: IpcResponse::success(serde_json::json!({"quit": true})),
        then_quit: true,
    }
}

/// Every feature flag with the value in force and the layer that decided it.
///
/// A daemon started with `--feature` overrides serves its startup snapshot,
/// which is the only place the CLI layer is still visible — see
/// [`IpcContext::feature_diagnostics`]. Otherwise this resolves live through
/// the same code the settings UI calls, so `slashit features` and the toggle
/// in the app cannot disagree about which layer won.
async fn handle_features(ctx: &IpcContext) -> IpcResponse {
    let flags = match &ctx.feature_diagnostics {
        Some(cached) => cached.clone(),
        None => ctx.features.read().await.diagnostics(),
    };
    IpcResponse::success(serde_json::to_value(flags).unwrap_or_default())
}

/// Report what kind of instance answered, and where.
///
/// The endpoint comes from the connection rather than from configuration: with
/// several listeners bound, the useful answer is the one the caller actually
/// reached.
fn handle_ping(ctx: &IpcContext, peer: &PeerContext) -> IpcResponse {
    let info = InstanceInfo {
        mode: ctx.control.mode().as_str().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: PROTOCOL_VERSION,
        pid: std::process::id(),
        endpoint: peer.endpoint.to_string(),
    };
    IpcResponse::success(serde_json::to_value(info).unwrap_or_default())
}

// --- Helper functions ---

fn task_to_summary(task: &crate::domain::Task, project_name: &str) -> TaskSummary {
    TaskSummary {
        id: task.id.to_string(),
        project_id: task.project_id.to_string(),
        project_name: project_name.to_string(),
        title: task.title.clone(),
        status: format!("{:?}", task.status),
        priority: format!("{:?}", task.priority),
        phase: format!("{:?}", task.phase),
        overall_progress: task.overall_progress,
        created_at: task.created_at.to_rfc3339(),
    }
}

fn parse_priority(s: Option<&str>) -> TaskPriority {
    match s {
        Some("urgent") => TaskPriority::Urgent,
        Some("high") => TaskPriority::High,
        Some("medium") => TaskPriority::Medium,
        Some("low") => TaskPriority::Low,
        _ => TaskPriority::Medium,
    }
}

fn parse_status(s: &str) -> Option<TaskStatus> {
    match s {
        "backlog" => Some(TaskStatus::Backlog),
        "queue" => Some(TaskStatus::Queue),
        "in_progress" => Some(TaskStatus::InProgress),
        "ai_review" => Some(TaskStatus::AiReview),
        "human_review" => Some(TaskStatus::HumanReview),
        "done" => Some(TaskStatus::Done),
        "pr_created" => Some(TaskStatus::PrCreated),
        "error" => Some(TaskStatus::Error),
        _ => None,
    }
}

#[cfg(test)]
pub(super) mod tests {
    //! The delete handler's own policy, proven against the same conditions the
    //! desktop delete is held to in `crate::lifecycle::tests`.
    //!
    //! Driven through [`dispatch`] rather than over a socket: the transport is
    //! already covered by `tests/ipc_integration.rs`, and what needs proving
    //! here is that the daemon's front door reaches the one shared operation
    //! rather than a second copy of the rules. A delete that is safe through
    //! the desktop and unsafe through the CLI is not a safe delete.

    use super::*;
    use crate::domain::{Project, Repository};
    use crate::test_helpers::{create_test_task_full, ipc_test_context};
    use slashit_ipc::Endpoint;
    use std::path::Path;
    use std::sync::Arc;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn peer() -> PeerContext {
        PeerContext {
            endpoint: Endpoint::Unix {
                path: std::path::PathBuf::from("/nonexistent-for-tests"),
            },
            os_verified: true,
        }
    }

    /// A context whose worktrees are SlashIt's own, plus a real repository, a
    /// project pointing at it, and one task holding a real checkout.
    async fn world(dirty: bool) -> (tempfile::TempDir, IpcContext, Uuid, String) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (ctx, task_id, checkout) = world_at(tmp.path(), dirty).await;
        (tmp, ctx, task_id, checkout)
    }

    /// [`world`] rooted at a directory the caller owns, so that another
    /// process can be told where it is.
    pub(in crate::ipc) async fn world_at(root: &Path, dirty: bool) -> (IpcContext, Uuid, String) {
        let tmp = root;
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.join("config"),
            tmp.join("data"),
            tmp.join("cache"),
            tmp.join("runtime"),
        ));

        let mut ctx = ipc_test_context(paths.clone());
        ctx.worktree_manager = Arc::new(crate::worktree::WorktreeManager::new(paths));

        let repo_dir = tmp.join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        git(&repo_dir, &["init", "--initial-branch=main"]);
        git(&repo_dir, &["config", "user.email", "test@example.com"]);
        git(&repo_dir, &["config", "user.name", "Test"]);
        std::fs::write(repo_dir.join("README.md"), "seed\n").unwrap();
        git(&repo_dir, &["add", "."]);
        git(&repo_dir, &["commit", "-m", "seed"]);
        // A bare `origin` whose `HEAD` is recorded locally, so that the task
        // checkout has a default base to start from.
        let origin = tmp.join("origin.git");
        git(&repo_dir, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(&repo_dir, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&repo_dir, &["push", "-q", "origin", "main"]);
        git(&repo_dir, &["remote", "set-head", "origin", "main"]);
        let repo_path = repo_dir.to_string_lossy().to_string();

        let repository_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        ctx.repositories.write().await.insert(
            repository_id,
            Repository {
                id: repository_id,
                local_path: repo_path.clone(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        ctx.projects.write().await.insert(
            project_id,
            Project {
                id: project_id,
                name: "ipc-delete-test".to_string(),
                repository_id: Some(repository_id),
                scope: crate::domain::project::ProjectScope::Standalone,
                state_location: crate::config::paths::StateLocation::External,
                base: None,
                agent_type: crate::domain::AgentType::ClaudeCode,
                agent_config: crate::domain::project::AgentConfig {
                    agent_type: crate::domain::AgentType::ClaudeCode,
                    command: "claude".to_string(),
                    args: Vec::new(),
                    env: std::collections::HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );

        let info = ctx
            .worktree_manager
            .create(&repo_path, "task-abcd1234")
            .await
            .expect("worktree creation");
        let checkout = std::path::PathBuf::from(&info.path);
        std::fs::write(checkout.join("work.txt"), "work\n").unwrap();
        git(&checkout, &["config", "user.email", "test@example.com"]);
        git(&checkout, &["config", "user.name", "Test"]);
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-m", "work"]);
        if dirty {
            // Uncommitted and not ignored: git refuses this checkout through
            // the ordinary production path, which is the refusal under test.
            std::fs::write(checkout.join("unsaved.txt"), "not committed\n").unwrap();
        }

        let mut task = create_test_task_full("subject", project_id, TaskStatus::InProgress, 0);
        task.worktree_path = Some(info.path.clone());
        task.branch_name = Some("task-abcd1234".to_string());
        let task_id = task.id;
        ctx.tasks.write().await.insert(task_id, task.clone());
        ctx.storage
            .save_project_tasks(project_id, &[task])
            .expect("seed the board");

        (ctx, task_id, info.path)
    }

    #[tokio::test]
    async fn ipc_delete_refuses_a_checkout_git_will_not_remove_and_keeps_the_record() {
        let (_tmp, ctx, task_id, checkout) = world(true).await;

        let answer = dispatch(
            IpcRequest::DeleteTask {
                task_id: task_id.to_string(),
            },
            &ctx,
            &peer(),
        )
        .await
        .response;

        assert!(
            !answer.ok,
            "the handler answered success for a delete whose cleanup git refused: {answer:?}"
        );

        // The record is the only thing naming this checkout and this branch.
        // Removing it after a refusal is what strands the directory.
        let task = ctx
            .tasks
            .read()
            .await
            .get(&task_id)
            .cloned()
            .expect("the record has to survive a refused delete");
        assert_eq!(task.worktree_path.as_deref(), Some(checkout.as_str()));
        assert!(
            Path::new(&checkout).join("unsaved.txt").exists(),
            "and the work the refusal was protecting must survive with it"
        );
    }

    #[tokio::test]
    async fn ipc_delete_removes_the_task_once_its_checkout_is_safely_gone() {
        let (_tmp, ctx, task_id, checkout) = world(false).await;

        let answer = dispatch(
            IpcRequest::DeleteTask {
                task_id: task_id.to_string(),
            },
            &ctx,
            &peer(),
        )
        .await
        .response;

        assert!(answer.ok, "a clean checkout must delete: {answer:?}");
        assert!(!ctx.tasks.read().await.contains_key(&task_id));
        assert!(
            !Path::new(&checkout).exists(),
            "the checkout goes with the record it belonged to"
        );
    }

    /// Front-door parity: `MoveTask` used to write `task.status` directly for
    /// every non-`Done` transition, so a card moved `Failed -> InProgress`
    /// kept its stale `phase = Failed` and `error_message` -- the board said
    /// "running" while the task failed its own readiness check. This drives
    /// the real handler, not a copy of its logic, and checks the same
    /// production predicate the executor gates on.
    #[tokio::test]
    async fn ipc_move_task_out_of_failed_resets_stale_execution_state_not_just_the_status() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);

        let project_id = Uuid::new_v4();
        ctx.projects.write().await.insert(
            project_id,
            Project {
                id: project_id,
                name: "ipc-move-test".to_string(),
                repository_id: None,
                scope: crate::domain::project::ProjectScope::Standalone,
                state_location: crate::config::paths::StateLocation::External,
                base: None,
                agent_type: crate::domain::AgentType::ClaudeCode,
                agent_config: crate::domain::project::AgentConfig {
                    agent_type: crate::domain::AgentType::ClaudeCode,
                    command: "claude".to_string(),
                    args: Vec::new(),
                    env: std::collections::HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );

        let mut task = create_test_task_full("subject", project_id, TaskStatus::Error, 0);
        task.phase = crate::domain::TaskPhase::Failed;
        task.phase_progress = 40;
        task.overall_progress = 30;
        task.error_message = Some("the agent reported a failure".to_string());
        let task_id = task.id;
        ctx.tasks.write().await.insert(task_id, task.clone());
        ctx.storage
            .save_project_tasks(project_id, &[task])
            .expect("seed the board");

        let answer = dispatch(
            IpcRequest::MoveTask {
                task_id: task_id.to_string(),
                status: "in_progress".to_string(),
            },
            &ctx,
            &peer(),
        )
        .await
        .response;

        assert!(answer.ok, "the move must succeed: {answer:?}");

        let updated = ctx
            .tasks
            .read()
            .await
            .get(&task_id)
            .cloned()
            .expect("the task is still on the board");
        assert_eq!(updated.status, TaskStatus::InProgress);
        assert_eq!(
            updated.phase,
            crate::domain::TaskPhase::Idle,
            "a stale Failed phase must not survive a move out of Error"
        );
        assert!(updated.error_message.is_none(), "the old failure must not linger");
        assert!(
            updated.is_ready_to_execute(),
            "the production readiness predicate must agree this task can run"
        );

        // Durability: the disk must agree with memory, not just the reply.
        let persisted = ctx
            .storage
            .load_project_tasks(project_id)
            .expect("read the board back");
        let persisted_task = persisted
            .iter()
            .find(|t| t.id == task_id)
            .expect("the task must still be recorded");
        assert_eq!(persisted_task.status, TaskStatus::InProgress);
        assert_eq!(persisted_task.phase, crate::domain::TaskPhase::Idle);
        assert!(persisted_task.error_message.is_none());
    }

    /// The command-line client reaches the same lifecycle as the desktop: a
    /// task whose local integration was started and not finished stays in the
    /// column its Resume and Discard are offered in, whatever is asked.
    #[tokio::test]
    async fn ipc_moves_and_enqueues_leave_a_task_with_an_unfinished_integration_alone() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);

        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("integrating", project_id, TaskStatus::Done, 0);
        task.local_integration = Some(crate::domain::LocalIntegration {
            state: crate::domain::IntegrationState::Pending,
            target_branch: "main".to_string(),
            target_old_oid: "a".repeat(40),
            approved_tip: "b".repeat(40),
            result_oid: "b".repeat(40),
            strategy: crate::domain::IntegrationStrategy::FastForward,
            checkout_path: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            completes_task: false,
        });
        task.record_activity(crate::domain::task::ActivityKind::CompletedLocally {});
        let task_id = task.id;
        ctx.tasks.write().await.insert(task_id, task.clone());
        ctx.storage.save_project_tasks(project_id, &[task.clone()]).expect("seed the board");

        let mut requests = vec![IpcRequest::EnqueueTask { task_id: task_id.to_string() }];
        for status in ["backlog", "queue", "in_progress", "done"] {
            requests.push(IpcRequest::MoveTask { task_id: task_id.to_string(), status: status.to_string() });
        }
        for request in requests {
            let label = format!("{request:?}");
            let answer = dispatch(request, &ctx, &peer()).await.response;
            assert!(!answer.ok, "{label}: must be refused: {answer:?}");
            assert!(
                format!("{answer:?}").contains(crate::domain::task::INTEGRATION_UNFINISHED_REFUSAL),
                "{label}: {answer:?}"
            );
            let board = ctx.tasks.read().await[&task_id].clone();
            assert_eq!(
                (board.status, board.updated_at, board.activity.len()),
                (TaskStatus::Done, task.updated_at, 1),
                "{label}: the board is unchanged"
            );
            assert_eq!(board.local_integration, task.local_integration, "{label}");
            let persisted = ctx.storage.load_project_tasks(project_id).expect("read back");
            assert_eq!(persisted[0].status, TaskStatus::Done, "{label}: the file is unchanged");
            assert!(persisted[0].local_integration.as_ref().is_some_and(|r| r.is_pending()), "{label}");
        }
    }

    /// Front-door parity: `CreateTask` used to hardcode `position: 0` for
    /// every new task, so every task created through the CLI collided with
    /// whatever else already sat at the top of Backlog. Desktop's
    /// `create_task` command and this handler now share
    /// `Task::next_backlog_position`, so a task created here lands at the end
    /// of the column exactly like one created in the app.
    #[tokio::test]
    async fn ipc_create_task_places_each_new_task_at_the_end_of_the_backlog_column() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);

        let project_id = Uuid::new_v4();
        ctx.projects.write().await.insert(
            project_id,
            Project {
                id: project_id,
                name: "ipc-create-position-test".to_string(),
                repository_id: None,
                scope: crate::domain::project::ProjectScope::Standalone,
                state_location: crate::config::paths::StateLocation::External,
                base: None,
                agent_type: crate::domain::AgentType::ClaudeCode,
                agent_config: crate::domain::project::AgentConfig {
                    agent_type: crate::domain::AgentType::ClaudeCode,
                    command: "claude".to_string(),
                    args: Vec::new(),
                    env: std::collections::HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );

        for i in 0..3 {
            let answer = dispatch(
                IpcRequest::CreateTask {
                    project_id: project_id.to_string(),
                    title: format!("task {i}"),
                    description: None,
                    priority: None,
                    review_mode: None,
                },
                &ctx,
                &peer(),
            )
            .await
            .response;
            assert!(answer.ok, "create {i} must succeed: {answer:?}");
        }

        let mut positions: Vec<i32> = ctx
            .tasks
            .read()
            .await
            .values()
            .filter(|t| t.project_id == project_id)
            .map(|t| t.position)
            .collect();
        positions.sort();
        assert_eq!(
            positions,
            vec![0, 1, 2],
            "each task must land after the ones already there, not all collapse onto position 0"
        );
    }

    /// Front-door parity: `ListProjects` used to hardcode `path: None` for
    /// every project, even one with a resolvable repository. Both this
    /// handler and desktop's `get_project_path` command now resolve through
    /// `Project::repository_path`, so the CLI reports the same fact the app
    /// does -- truthfully absent for a project with no repository, not
    /// invented for one that has one.
    #[tokio::test]
    async fn ipc_list_projects_reports_the_same_path_facts_as_the_desktop_command() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);

        let repository_id = Uuid::new_v4();
        ctx.repositories.write().await.insert(
            repository_id,
            Repository {
                id: repository_id,
                local_path: "/repo/with-a-path".to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );

        let with_repo_id = Uuid::new_v4();
        let without_repo_id = Uuid::new_v4();
        let mk_project = |id: Uuid, name: &str, repository_id: Option<Uuid>| Project {
            id,
            name: name.to_string(),
            repository_id,
            scope: crate::domain::project::ProjectScope::Standalone,
            state_location: crate::config::paths::StateLocation::External,
            base: None,
            agent_type: crate::domain::AgentType::ClaudeCode,
            agent_config: crate::domain::project::AgentConfig {
                agent_type: crate::domain::AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: std::collections::HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        ctx.projects.write().await.insert(
            with_repo_id,
            mk_project(with_repo_id, "has-a-repo", Some(repository_id)),
        );
        ctx.projects
            .write()
            .await
            .insert(without_repo_id, mk_project(without_repo_id, "standalone", None));

        let answer = dispatch(IpcRequest::ListProjects, &ctx, &peer()).await.response;
        assert!(answer.ok, "{answer:?}");
        let summaries: Vec<slashit_ipc::ProjectSummary> =
            serde_json::from_value(answer.data).expect("valid summaries");

        let with_repo = summaries
            .iter()
            .find(|s| s.id == with_repo_id.to_string())
            .expect("the repo-backed project must be listed");
        assert_eq!(
            with_repo.path.as_deref(),
            Some("/repo/with-a-path"),
            "a project with a resolvable repository must report its real path"
        );

        let without_repo = summaries
            .iter()
            .find(|s| s.id == without_repo_id.to_string())
            .expect("the standalone project must be listed");
        assert_eq!(
            without_repo.path, None,
            "a project with no repository must report an honest absence, not an invented path"
        );
    }
}

/// U3a: `slashit move <task> done` must not close a Task that is awaiting a
/// Human Review decision. Driven through [`dispatch`], the path a CLI request
/// takes, not through a helper.
#[cfg(test)]
mod human_review_close {
    use super::*;
    use crate::test_helpers::{create_test_task_full, ipc_test_context};
    use slashit_ipc::Endpoint;
    use std::sync::Arc;

    fn peer() -> PeerContext {
        PeerContext {
            endpoint: Endpoint::Unix {
                path: std::path::PathBuf::from("/nonexistent-for-tests"),
            },
            os_verified: true,
        }
    }

    async fn test_ctx() -> (tempfile::TempDir, IpcContext) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        (tmp, ipc_test_context(paths))
    }

    /// Seeds one Task on its board and returns its ID and its Project's ID.
    async fn seed(ctx: &IpcContext, status: TaskStatus) -> (Uuid, Uuid) {
        let project_id = Uuid::new_v4();
        let task = create_test_task_full("under test", project_id, status, 0);
        let task_id = task.id;
        ctx.tasks.write().await.insert(task_id, task.clone());
        ctx.storage
            .save_project_tasks(project_id, &[task])
            .expect("seed the board");
        (task_id, project_id)
    }

    async fn move_to(ctx: &IpcContext, task_id: Uuid, status: &str) -> IpcResponse {
        dispatch(
            IpcRequest::MoveTask { task_id: task_id.to_string(), status: status.to_string() },
            ctx,
            &peer(),
        )
        .await
        .response
    }

    #[tokio::test]
    async fn cli_done_refuses_a_task_awaiting_human_review_and_changes_nothing() {
        let (_tmp, ctx) = test_ctx().await;
        let (task_id, project_id) = seed(&ctx, TaskStatus::HumanReview).await;
        let before = serde_json::to_value(ctx.tasks.read().await.get(&task_id).cloned().unwrap())
            .expect("a task serializes");

        let first = move_to(&ctx, task_id, "done").await;
        let second = move_to(&ctx, task_id, "done").await;

        assert!(!first.ok, "a Task in Human Review must not be closed from the CLI: {first:?}");
        let message = first.error.clone().unwrap_or_default();
        assert!(
            message.contains("Human Review") && message.contains("application"),
            "the refusal must say the decision is made in the application: {message}"
        );
        assert_eq!(second.error, first.error, "a repeated request gives the same refusal");

        let in_memory = ctx.tasks.read().await.get(&task_id).cloned().unwrap();
        assert_eq!(in_memory.status, TaskStatus::HumanReview);
        assert_eq!(serde_json::to_value(&in_memory).unwrap(), before);
        let on_disk = ctx
            .storage
            .load_project_tasks(project_id)
            .expect("read the board back")
            .into_iter()
            .find(|t| t.id == task_id)
            .expect("the task is on disk");
        assert_eq!(on_disk.status, TaskStatus::HumanReview);
    }

    /// Every typed move out of Human Review is refused, not only the close: the
    /// two-step route (Human Review, then Queue, then Done) must not exist from
    /// the command line. Each refused request leaves the task and the board as
    /// they were.
    #[tokio::test]
    async fn cli_moves_out_of_human_review_are_refused_for_every_target() {
        let (_tmp, ctx) = test_ctx().await;
        let (task_id, project_id) = seed(&ctx, TaskStatus::HumanReview).await;
        let before = serde_json::to_value(ctx.tasks.read().await.get(&task_id).cloned().unwrap())
            .expect("a task serializes");

        for target in ["queue", "in_progress", "backlog", "done"] {
            let answer = move_to(&ctx, task_id, target).await;
            assert!(!answer.ok, "a move to {target} must be refused: {answer:?}");
            assert!(
                answer.error.unwrap_or_default().contains("Human Review"),
                "the refusal for {target} must name Human Review"
            );
        }

        assert_eq!(serde_json::to_value(ctx.tasks.read().await.get(&task_id).cloned().unwrap()).unwrap(), before);
        let on_disk = ctx
            .storage
            .load_project_tasks(project_id)
            .expect("read the board back")
            .into_iter()
            .find(|t| t.id == task_id)
            .expect("the task is on disk");
        assert_eq!(on_disk.status, TaskStatus::HumanReview);
    }

    /// A queue request from the command line is refused for a Task in Human
    /// Review, and the approval it holds is kept. Queueing would withdraw it.
    #[tokio::test]
    async fn cli_enqueue_refuses_a_human_review_task_and_keeps_its_approval() {
        let (_tmp, ctx) = test_ctx().await;
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("under test", project_id, TaskStatus::HumanReview, 0);
        task.human_review.approve(Some("a".repeat(40)), chrono::Utc::now());
        let task_id = task.id;
        ctx.tasks.write().await.insert(task_id, task.clone());
        ctx.storage.save_project_tasks(project_id, &[task]).expect("seed the board");

        let answer = dispatch(IpcRequest::EnqueueTask { task_id: task_id.to_string() }, &ctx, &peer())
            .await
            .response;

        assert!(!answer.ok, "a Human Review task must not be queued from the CLI: {answer:?}");
        let in_memory = ctx.tasks.read().await.get(&task_id).cloned().unwrap();
        assert_eq!(in_memory.status, TaskStatus::HumanReview);
        assert!(in_memory.human_review.is_approved(), "the approval is kept");
        let on_disk = ctx
            .storage
            .load_project_tasks(project_id)
            .expect("read the board back")
            .into_iter()
            .find(|t| t.id == task_id)
            .expect("the task is on disk");
        assert_eq!(on_disk.status, TaskStatus::HumanReview);
        assert!(on_disk.human_review.is_approved());
    }

    #[tokio::test]
    async fn cli_done_still_finishes_a_task_outside_human_review() {
        let (_tmp, ctx) = test_ctx().await;
        let (task_id, _project_id) = seed(&ctx, TaskStatus::Backlog).await;

        let answer = move_to(&ctx, task_id, "done").await;

        assert!(answer.ok, "{answer:?}");
        assert_eq!(ctx.tasks.read().await.get(&task_id).unwrap().status, TaskStatus::Done);
    }
}

/// The IPC-side half of Unit 5C2's corrective-pass front-door parity: every
/// scenario here is the exact one pinned against the desktop command in
/// `crate::commands::task::lifecycle_ownership`, over the same test-only fake
/// owners, driven through the real `handle_move_task`/`dispatch` -- not a
/// copy of its logic -- so `slashit move` and dragging a card in the app are
/// held to the same ownership contract rather than each trusted separately.
#[cfg(test)]
mod lifecycle_ownership {
    use super::*;
    use crate::test_helpers::{attach_test_executor_ipc, create_test_task_full, ipc_test_context};
    use slashit_ipc::Endpoint;
    use std::sync::Arc;

    fn peer() -> PeerContext {
        PeerContext {
            endpoint: Endpoint::Unix {
                path: std::path::PathBuf::from("/nonexistent-for-tests"),
            },
            os_verified: true,
        }
    }

    async fn test_ctx() -> (tempfile::TempDir, IpcContext) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        (tmp, ipc_test_context(paths))
    }

    async fn seed(ctx: &IpcContext, status: TaskStatus) -> Uuid {
        let project_id = Uuid::new_v4();
        let task = create_test_task_full("under test", project_id, status, 0);
        let task_id = task.id;
        ctx.tasks.write().await.insert(task_id, task.clone());
        ctx.storage
            .save_project_tasks(project_id, &[task])
            .expect("seed the board");
        task_id
    }

    #[tokio::test]
    async fn ipc_move_task_ends_a_running_execution_before_moving_the_task() {
        let (_tmp, ctx) = test_ctx().await;
        let executor = attach_test_executor_ipc(&ctx).await;
        let task_id = seed(&ctx, TaskStatus::InProgress).await;
        let cleaned_up = executor.register_fake_running_execution_for_test(task_id).await;

        let answer = dispatch(
            IpcRequest::MoveTask { task_id: task_id.to_string(), status: "backlog".to_string() },
            &ctx,
            &peer(),
        )
        .await
        .response;

        assert!(answer.ok, "{answer:?}");
        assert!(
            cleaned_up.load(std::sync::atomic::Ordering::SeqCst),
            "the previous execution owner must be driven to its own cleanup before the \
             IPC move is reported as done"
        );
        assert_eq!(executor.running_task_count().await, 0);
    }

    #[tokio::test]
    async fn ipc_move_task_ends_a_running_review_before_moving_the_task() {
        let (_tmp, ctx) = test_ctx().await;
        let executor = attach_test_executor_ipc(&ctx).await;
        let task_id = seed(&ctx, TaskStatus::AiReview).await;
        let cleaned_up = executor.register_fake_reviewing_owner_for_test(task_id).await;

        let answer = dispatch(
            IpcRequest::MoveTask { task_id: task_id.to_string(), status: "backlog".to_string() },
            &ctx,
            &peer(),
        )
        .await
        .response;

        assert!(answer.ok, "{answer:?}");
        assert!(cleaned_up.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(executor.running_task_count().await, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn ipc_move_task_refuses_when_ownership_cannot_be_ended_in_time() {
        let (_tmp, ctx) = test_ctx().await;
        let executor = attach_test_executor_ipc(&ctx).await;
        let task_id = seed(&ctx, TaskStatus::InProgress).await;
        executor.register_unkillable_running_execution_for_test(task_id).await;

        let answer = dispatch(
            IpcRequest::MoveTask { task_id: task_id.to_string(), status: "backlog".to_string() },
            &ctx,
            &peer(),
        )
        .await
        .response;

        assert!(
            !answer.ok,
            "an owner that cannot be ended within the bounded shutdown must refuse the \
             move, not silently proceed: {answer:?}"
        );
        assert_eq!(
            ctx.tasks.read().await.get(&task_id).unwrap().status,
            TaskStatus::InProgress,
            "the previous durable status must survive a refused move"
        );
        assert!(executor.is_task_running(task_id).await);
    }

    #[tokio::test]
    async fn ipc_move_task_same_status_does_not_touch_a_running_execution() {
        // The IPC protocol has no positional reorder, so its analogue of
        // "same-column reorder" is a move whose requested status is the
        // status the task already has.
        let (_tmp, ctx) = test_ctx().await;
        let executor = attach_test_executor_ipc(&ctx).await;
        let task_id = seed(&ctx, TaskStatus::InProgress).await;
        let cleaned_up = executor.register_fake_running_execution_for_test(task_id).await;

        let answer = dispatch(
            IpcRequest::MoveTask { task_id: task_id.to_string(), status: "in_progress".to_string() },
            &ctx,
            &peer(),
        )
        .await
        .response;

        assert!(answer.ok, "{answer:?}");
        assert!(
            !cleaned_up.load(std::sync::atomic::Ordering::SeqCst),
            "a move that does not change the task's status must never touch the agent \
             working on it"
        );
        assert!(executor.is_task_running(task_id).await);
    }

    /// Stores `task` on the board and in memory, as the desktop would have.
    async fn seed_task(ctx: &IpcContext, task: crate::domain::Task) -> Uuid {
        let project_id = task.project_id;
        let task_id = task.id;
        ctx.tasks.write().await.insert(task_id, task.clone());
        ctx.storage
            .save_project_tasks(project_id, &[task])
            .expect("seed the board");
        task_id
    }

    fn persisted(ctx: &IpcContext, project_id: Uuid, task_id: Uuid) -> crate::domain::Task {
        ctx.storage
            .load_project_tasks(project_id)
            .expect("the board must be readable")
            .into_iter()
            .find(|t| t.id == task_id)
            .expect("the task must be on disk")
    }

    fn move_to_queue(task_id: Uuid) -> IpcRequest {
        IpcRequest::MoveTask { task_id: task_id.to_string(), status: "queue".to_string() }
    }

    #[tokio::test]
    async fn a_move_into_queue_refuses_unfinished_cleanup_before_ending_an_owner() {
        let (_tmp, ctx) = test_ctx().await;
        let executor = attach_test_executor_ipc(&ctx).await;
        let project_id = Uuid::new_v4();
        // A Done cleanup that stamped its flag, then failed before its status
        // write: the Task is still in Human Review with the flag set.
        let mut task = create_test_task_full("cleaning up", project_id, TaskStatus::HumanReview, 0);
        task.cleanup_in_flight = true;
        let task_id = seed_task(&ctx, task).await;
        let cleaned_up = executor.register_fake_running_execution_for_test(task_id).await;

        let answer = dispatch(move_to_queue(task_id), &ctx, &peer()).await.response;

        // The command line refuses any move out of Human Review before the
        // cleanup refusal is reached, and before an owner is ended.
        assert!(!answer.ok, "{answer:?}");
        assert!(format!("{answer:?}").contains("Human Review"), "{answer:?}");
        assert!(
            !cleaned_up.load(std::sync::atomic::Ordering::SeqCst),
            "a move that will be refused must not end the owner"
        );
        assert!(executor.is_task_running(task_id).await);
        assert_eq!(ctx.tasks.read().await[&task_id].status, TaskStatus::HumanReview);
        assert_eq!(persisted(&ctx, project_id, task_id).status, TaskStatus::HumanReview);
    }

    #[tokio::test]
    async fn a_move_into_queue_refuses_a_live_worker_without_stopping_it() {
        let (_tmp, ctx) = test_ctx().await;
        let executor = attach_test_executor_ipc(&ctx).await;
        let project_id = Uuid::new_v4();
        // A Worker is running: its handoff is recorded as Started and the
        // Task is In Progress.
        let mut task = create_test_task_full("running", project_id, TaskStatus::InProgress, 0);
        task.worker_handoff = Some(crate::domain::WorkerHandoff {
            review_mode: Default::default(),
            task_id: task.id,
            project_id,
            repository_id: None,
            run: Some(1),
            branch: None,
            start_head: None,
            stage: crate::domain::WorkerHandoffStage::Started {},
            acceptance: None,
            review_mode_unproven: false,
            reviewer_model: None,
        });
        let task_id = seed_task(&ctx, task).await;
        let cleaned_up = executor.register_fake_running_execution_for_test(task_id).await;

        let answer = dispatch(move_to_queue(task_id), &ctx, &peer()).await.response;

        assert!(!answer.ok, "{answer:?}");
        assert!(
            !cleaned_up.load(std::sync::atomic::Ordering::SeqCst),
            "a queue request that cannot be written must leave the live Worker running"
        );
        assert!(executor.is_task_running(task_id).await);
        assert_eq!(ctx.tasks.read().await[&task_id].status, TaskStatus::InProgress);
        assert_eq!(persisted(&ctx, project_id, task_id).status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn a_move_into_queue_that_is_not_refused_is_written_once() {
        let (_tmp, ctx) = test_ctx().await;
        let project_id = Uuid::new_v4();
        let task = create_test_task_full("ready", project_id, TaskStatus::Backlog, 0);
        let task_id = seed_task(&ctx, task).await;

        let answer = dispatch(move_to_queue(task_id), &ctx, &peer()).await.response;
        assert!(answer.ok, "{answer:?}");
        let written = persisted(&ctx, project_id, task_id);
        assert_eq!(written.status, TaskStatus::Queue);
        let activity = written.activity.len();

        // A repeated request changes nothing, and records no second move.
        let answer = dispatch(move_to_queue(task_id), &ctx, &peer()).await.response;
        assert!(answer.ok, "{answer:?}");
        let repeated = persisted(&ctx, project_id, task_id);
        assert_eq!(repeated.status, TaskStatus::Queue);
        assert_eq!(repeated.activity.len(), activity);
    }

    /// A repeated request for Queue, from a Task already in Queue, leaves the
    /// Task exactly as it was: its error, approval and recovery evidence
    /// survive, nothing is recorded, and the board is not rewritten. The
    /// evidence is given once without recovery evidence, where a reset would
    /// otherwise be written, and once with it, where the board refuses any
    /// write.
    #[tokio::test]
    async fn a_repeated_move_into_queue_keeps_the_evidence_a_queued_task_carries() {
        let (_tmp, ctx) = test_ctx().await;
        let executor = attach_test_executor_ipc(&ctx).await;
        let project_id = Uuid::new_v4();
        for with_recovery in [false, true] {
            let mut task = create_test_task_full("queued with evidence", project_id, TaskStatus::Queue, 0);
            task.error_message = Some("the last run failed".into());
            task.human_review.approve(Some("a".repeat(40)), chrono::Utc::now());
            if with_recovery {
                task.worker_handoff = Some(crate::domain::WorkerHandoff {
                    review_mode: Default::default(),
                    task_id: task.id,
                    project_id,
                    repository_id: None,
                    run: Some(1),
                    branch: None,
                    start_head: None,
                    stage: crate::domain::WorkerHandoffStage::RecoveryRequired {
                        reason: "the run was interrupted".into(),
                        observed_commit: None,
                    },
                    acceptance: None,
                    review_mode_unproven: false,
                    reviewer_model: None,
                });
            }
            // A fixed, old timestamp: any rewrite of the record moves it.
            task.updated_at = chrono::DateTime::from_timestamp(0, 0).expect("the epoch");
            let task_id = task.id;
            let board = {
                let mut tasks = ctx.tasks.write().await;
                tasks.insert(task_id, task);
                tasks.values().filter(|t| t.project_id == project_id).cloned().collect::<Vec<_>>()
            };
            ctx.storage.save_project_tasks(project_id, &board).expect("seed the board");
            let cleaned_up = executor.register_fake_running_execution_for_test(task_id).await;
            let memory_before = serde_json::to_value(ctx.tasks.read().await[&task_id].clone()).unwrap();
            let disk_before = serde_json::to_value(persisted(&ctx, project_id, task_id)).unwrap();

            let answer = dispatch(move_to_queue(task_id), &ctx, &peer()).await.response;

            assert!(answer.ok, "with recovery {with_recovery}: {answer:?}");
            assert!(
                !cleaned_up.load(std::sync::atomic::Ordering::SeqCst),
                "a request that changes nothing must not end the owner"
            );
            assert!(executor.is_task_running(task_id).await);
            let memory_after = serde_json::to_value(ctx.tasks.read().await[&task_id].clone()).unwrap();
            let disk_after = serde_json::to_value(persisted(&ctx, project_id, task_id)).unwrap();
            assert_eq!(memory_after, memory_before, "with recovery {with_recovery}: memory is unchanged");
            assert_eq!(disk_after, disk_before, "with recovery {with_recovery}: the record is not rewritten");
            let written = persisted(&ctx, project_id, task_id);
            assert_eq!(written.error_message.as_deref(), Some("the last run failed"));
            assert!(written.human_review.is_approved());
            assert_eq!(written.worker_handoff.is_some(), with_recovery);
            if with_recovery {
                assert!(matches!(
                    written.worker_handoff.as_ref().map(|h| &h.stage),
                    Some(crate::domain::WorkerHandoffStage::RecoveryRequired { .. })
                ));
            }
        }
    }

    #[tokio::test]
    async fn ipc_enqueue_task_ends_a_running_execution_before_requeuing_the_task() {
        let (_tmp, ctx) = test_ctx().await;
        let executor = attach_test_executor_ipc(&ctx).await;
        let task_id = seed(&ctx, TaskStatus::InProgress).await;
        let cleaned_up = executor.register_fake_running_execution_for_test(task_id).await;

        let answer = dispatch(IpcRequest::EnqueueTask { task_id: task_id.to_string() }, &ctx, &peer())
            .await
            .response;

        assert!(answer.ok, "{answer:?}");
        assert!(
            cleaned_up.load(std::sync::atomic::Ordering::SeqCst),
            "`slashit enqueue` must end a task's previous owner before overwriting its \
             status with Queue"
        );
        assert_eq!(
            ctx.tasks.read().await.get(&task_id).unwrap().status,
            TaskStatus::Queue
        );
    }
}

/// `slashit move <task> in_progress` is held to the same disk-pressure pause
/// as the desktop front doors.
#[cfg(test)]
mod disk_pressure {
    use super::*;
    use crate::domain::storage_usage::GIB;
    use crate::test_helpers::{create_test_task_full, ipc_test_context, FakeDisk};
    use slashit_ipc::Endpoint;
    use std::sync::Arc;

    #[tokio::test]
    async fn ipc_move_into_in_progress_is_refused_on_a_critical_disk() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let disk = FakeDisk::with_available(10 * GIB);
        let mut ctx = ipc_test_context(paths);
        ctx.start_guard = disk.guard();
        let project_id = Uuid::new_v4();
        let task = create_test_task_full("under test", project_id, TaskStatus::Queue, 0);
        let task_id = task.id;
        ctx.tasks.write().await.insert(task_id, task.clone());
        ctx.storage.save_project_tasks(project_id, &[task]).expect("seed the board");
        // This platform's own local endpoint -- a Unix socket or a named
        // pipe -- so the test describes a caller that can exist here. The
        // handler reads it only to answer `Ping`.
        let peer = PeerContext { endpoint: Endpoint::local_default(), os_verified: true };
        let move_to = |status: &str| IpcRequest::MoveTask { task_id: task_id.to_string(), status: status.to_string() };

        let answer = dispatch(move_to("in_progress"), &ctx, &peer).await.response;
        assert!(!answer.ok, "{answer:?}");
        assert!(format!("{answer:?}").contains("New work paused"), "{answer:?}");
        assert_eq!(ctx.tasks.read().await[&task_id].status, TaskStatus::Queue);

        // Other moves are not starts.
        let answer = dispatch(move_to("backlog"), &ctx, &peer).await.response;
        assert!(answer.ok, "{answer:?}");

        disk.set_available(300 * GIB);
        let answer = dispatch(move_to("in_progress"), &ctx, &peer).await.response;
        assert!(answer.ok, "{answer:?}");
        assert_eq!(ctx.tasks.read().await[&task_id].status, TaskStatus::InProgress);
    }
}

#[cfg(test)]
mod enqueue_from_the_cli {
    use super::*;
    use crate::domain::{TaskPhase, TaskStatus};
    use crate::test_helpers::{create_test_task_full, ipc_test_context};

    /// `slashit enqueue` on a failed Task clears the failed run exactly as the
    /// desktop's Add to Queue does, instead of queueing it with the stale phase
    /// and error still on the record.
    #[tokio::test]
    async fn enqueue_clears_a_failed_run_on_the_record_it_writes() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("failed", project_id, TaskStatus::Error, 0);
        task.phase = TaskPhase::Coding;
        task.error_message = Some("the last run failed".into());
        let id = task.id;
        ctx.tasks.write().await.insert(id, task.clone());
        ctx.storage
            .save_project_tasks(project_id, &[task])
            .expect("seed the board");

        let response = handle_enqueue_task(&ctx, id.to_string()).await;
        assert!(response.ok, "{response:?}");

        let on_disk = ctx
            .storage
            .load_project_tasks(project_id)
            .expect("the board must be readable")
            .into_iter()
            .find(|t| t.id == id)
            .expect("the task must be on disk");
        assert_eq!(on_disk.status, TaskStatus::Queue);
        assert_eq!(on_disk.phase, TaskPhase::Idle);
        assert_eq!(on_disk.error_message, None);
    }
}

/// `GetTask`: the full Task record, read-only.
#[cfg(test)]
mod get_task {
    use super::*;
    use crate::domain::task::{
        CodingProvider, HumanReviewDecision, HumanReviewEntry, ReviewMode,
    };
    use crate::domain::publication::{PrPublication, PublicationState};
    use crate::test_helpers::{create_test_task_full, ipc_test_context};
    use slashit_ipc::Endpoint;
    use std::sync::Arc;

    fn peer() -> PeerContext {
        PeerContext {
            endpoint: Endpoint::Unix {
                path: std::path::PathBuf::from("/nonexistent-for-tests"),
            },
            os_verified: true,
        }
    }

    async fn get(ctx: &IpcContext, id: &str) -> IpcResponse {
        dispatch(IpcRequest::GetTask { task_id: id.to_string() }, ctx, &peer())
            .await
            .response
    }

    fn files_under(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        crate::test_helpers::files_under(root)
    }

    async fn rich_world() -> (tempfile::TempDir, IpcContext, Uuid) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        let project_id = Uuid::new_v4();
        let dependency = Uuid::new_v4();
        let mut task = create_test_task_full("Inspect me", project_id, TaskStatus::HumanReview, 0);
        task.model = "claude-sonnet-5-5".to_string();
        task.provider = CodingProvider::OpenCode;
        task.review_mode = ReviewMode::Manual;
        task.reviewer_model = Some("claude-opus-5-5".to_string());
        task.dependencies = vec![dependency];
        task.pr_url = Some("https://github.com/o/r/pull/7".to_string());
        task.human_review.entries.push(HumanReviewEntry {
            sequence: 1,
            arrival: 1,
            decision: HumanReviewDecision::Approved,
            feedback: None,
            decided_at: chrono::Utc::now(),
            approved_tip: Some("a".repeat(40)),
            tip_reconfirmed: false,
        });
        task.pr_publication = Some(PrPublication {
            request_id: Uuid::new_v4(),
            commit: "a".repeat(40),
            branch: "task-1".to_string(),
            pr_url: "https://github.com/o/r/pull/7".to_string(),
            remote_before: None,
            state: PublicationState::Published {},
            requested_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        });
        let id = task.id;
        ctx.tasks.write().await.insert(id, task.clone());
        ctx.storage.save_project_tasks(project_id, &[task]).expect("seed the board");
        (tmp, ctx, id)
    }

    #[tokio::test]
    async fn returns_every_field_group_under_the_task_record_names() {
        let (_tmp, ctx, id) = rich_world().await;
        let reply = get(&ctx, &id.to_string()).await;
        assert!(reply.ok, "{:?}", reply.error);
        let d = reply.data;

        assert_eq!(d["id"], id.to_string());
        assert_eq!(d["title"], "Inspect me");
        assert_eq!(d["status"], serde_json::to_value(TaskStatus::HumanReview).unwrap());
        assert!(d.get("phase").is_some());
        assert_eq!(d["model"], "claude-sonnet-5-5");
        assert_eq!(d["provider"], "opencode");
        assert_eq!(d["review_mode"], "manual");
        assert_eq!(d["reviewer_model"], "claude-opus-5-5");
        assert_eq!(d["dependencies"].as_array().unwrap().len(), 1);
        assert_eq!(d["human_review"]["entries"][0]["decision"], "approved");
        assert_eq!(d["human_review"]["entries"][0]["approved_tip"], "a".repeat(40));
        assert_eq!(d["pr_url"], "https://github.com/o/r/pull/7");
        assert_eq!(d["pr_publication"]["commit"], "a".repeat(40));
        assert_eq!(d["pr_publication"]["state"]["kind"], "published");

        // Same shape as the stored record: it round-trips into a Task.
        let task: crate::domain::Task = serde_json::from_value(d).expect("a Task record");
        assert_eq!(task.id, id);
    }

    #[tokio::test]
    async fn makes_no_write() {
        let (tmp, ctx, id) = rich_world().await;
        let before = files_under(tmp.path());
        assert!(before.iter().any(|(p, b)| p.extension().is_some_and(|e| e == "toml") && String::from_utf8_lossy(b).contains("Inspect me")), "the seeded board is on disk");
        let reply = get(&ctx, &id.to_string()).await;
        assert!(reply.ok);
        assert_eq!(files_under(tmp.path()), before, "the Task store must be byte-identical");
    }

    #[tokio::test]
    async fn unknown_or_malformed_id_is_an_error() {
        let (_tmp, ctx, _id) = rich_world().await;
        let missing = get(&ctx, &Uuid::new_v4().to_string()).await;
        assert!(!missing.ok);
        assert!(missing.error.unwrap().contains("not found"));
        let bad = get(&ctx, "nope").await;
        assert!(!bad.ok);
        assert!(bad.error.unwrap().contains("Invalid task_id"));
    }
}

/// The Task configuration writes: `SetTaskModel`, `SetTaskReviewMode` and
/// `SetTaskDependencies` change what a Task's runs are later started with, and
/// are held here to the desktop commands' own contract. For the same input the
/// persisted Task field and the refusal messages must match what
/// `update_task` and `set_task_dependencies` produce, a refused request must
/// leave memory and the board file byte-for-byte as they were, and `GetTask`
/// (the slice 1 read verb) must confirm what was written.
#[cfg(test)]
mod task_configuration_writes {
    use super::*;
    use crate::commands::gui_admission::GuiMutation;
    use crate::config::paths::{AppPaths, StateLocation};
    use crate::domain::task::ReviewMode;
    use crate::domain::{AgentConfig, AgentType, Project, ProjectScope};
    use crate::test_helpers::{create_test_task_full, ipc_test_context};
    use slashit_ipc::Endpoint;
    use std::sync::Arc;
    use tauri::Manager;

    type App = tauri::App<tauri::test::MockRuntime>;

    fn peer() -> PeerContext {
        PeerContext {
            endpoint: Endpoint::Unix {
                path: std::path::PathBuf::from("/nonexistent-for-tests"),
            },
            os_verified: true,
        }
    }

    async fn test_ctx() -> (tempfile::TempDir, IpcContext) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        (tmp, ipc_test_context(paths))
    }

    /// Seeds one Task on its board and answers (task id, project id).
    async fn seed(ctx: &IpcContext, task: crate::domain::Task) -> (Uuid, Uuid) {
        let project_id = task.project_id;
        let task_id = task.id;
        ctx.tasks.write().await.insert(task_id, task.clone());
        ctx.storage
            .save_project_tasks(project_id, &[task])
            .expect("seed the board");
        (task_id, project_id)
    }

    fn backlog_task(project_id: Uuid, title: &str) -> crate::domain::Task {
        create_test_task_full(title, project_id, TaskStatus::Backlog, 0)
    }

    fn persisted(ctx: &IpcContext, project_id: Uuid, task_id: Uuid) -> crate::domain::Task {
        ctx.storage
            .load_project_tasks(project_id)
            .expect("the board must be readable")
            .into_iter()
            .find(|t| t.id == task_id)
            .expect("the task must be on disk")
    }

    /// The Task's whole record as the board file holds it.
    fn persisted_json(ctx: &IpcContext, project_id: Uuid, task_id: Uuid) -> serde_json::Value {
        serde_json::to_value(persisted(ctx, project_id, task_id)).unwrap()
    }

    /// Read one value back through `GetTask`, the slice 1 read verb.
    async fn read_back(ctx: &IpcContext, task_id: Uuid) -> serde_json::Value {
        let answer = dispatch(
            IpcRequest::GetTask {
                task_id: task_id.to_string(),
            },
            ctx,
            &peer(),
        )
        .await
        .response;
        assert!(answer.ok, "{answer:?}");
        answer.data
    }

    fn project_for(id: Uuid) -> Project {
        Project {
            id,
            name: "Configuration Project".to_string(),
            repository_id: None,
            scope: ProjectScope::Standalone,
            state_location: StateLocation::External,
            base: None,
            agent_type: AgentType::ClaudeCode,
            agent_config: AgentConfig {
                agent_type: AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: std::collections::HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// The desktop half of a parity test: a running application over its own
    /// state root, holding the Project and the Tasks the caller seeded.
    async fn gui_app(
        project_id: Uuid,
        tasks: Vec<crate::domain::Task>,
    ) -> (tempfile::TempDir, App) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let (state, _report) = crate::app_core::build_state_with_paths(paths)
            .await
            .expect("state should build under a fresh tempdir");
        state.project.projects.write().await.insert(project_id, project_for(project_id));
        {
            let mut memory = state.task.tasks.write().await;
            for task in &tasks {
                memory.insert(task.id, task.clone());
            }
            let board: Vec<crate::domain::Task> =
                memory.values().filter(|t| t.project_id == project_id).cloned().collect();
            state.storage.save_project_tasks(project_id, &board).expect("seed the board");
        }
        let app = tauri::test::mock_app();
        app.manage(state);
        (tmp, app)
    }

    fn new_params(task_id: Uuid, model: Option<String>, review_mode: Option<ReviewMode>) -> crate::commands::task::UpdateTaskParams {
        crate::commands::task::UpdateTaskParams {
            task_id: task_id.to_string(),
            title: None,
            description: None,
            category: None,
            priority: None,
            complexity: None,
            impact: None,
            security_severity: None,
            model,
            reviewer_model: None,
            provider: None,
            review_mode,
            planning_mode: None,
            dependencies: None,
        }
    }

    /// A Worker model identifier the model picker actually offers, so both
    /// paths take the same, accepted input whatever the environment installs.
    async fn an_offered_model() -> String {
        crate::agents::model_catalog::claude_models()
            .into_iter()
            .map(|m| m.id)
            .find(|id| id != "default")
            .unwrap_or_else(|| "default".to_string())
    }

    /// Acceptance: for the same input, the CLI write and the desktop's own
    /// `update_task` leave the field the board file persists in the same
    /// state, and `GetTask` reads the CLI's value back.
    #[tokio::test]
    async fn set_model_persists_what_the_desktop_update_task_persists() {
        let chosen = an_offered_model().await;

        let gui_project_id = Uuid::new_v4();
        let gui_task = backlog_task(gui_project_id, "desktop");
        let gui_task_id = gui_task.id;
        let (_gui_tmp, gui_app) = gui_app(gui_project_id, vec![gui_task]).await;
        crate::commands::task::update_task(
            GuiMutation::untracked(),
            gui_app.state(),
            new_params(gui_task_id, Some(chosen.clone()), None),
        )
        .await
        .expect("the desktop write is stored")
        .expect("the task exists");
        let gui_state = gui_app.state::<crate::AppState>();
        let gui_on_disk = gui_state
            .storage
            .load_project_tasks(gui_project_id)
            .expect("read the board back")
            .into_iter()
            .find(|t| t.id == gui_task_id)
            .expect("on disk");

        let (_tmp, ctx) = test_ctx().await;
        let project_id = Uuid::new_v4();
        let (task_id, ipc_project_id) = seed(&ctx, backlog_task(project_id, "cli")).await;
        let answer = dispatch(
            IpcRequest::SetTaskModel {
                task_id: task_id.to_string(),
                model: chosen.clone(),
            },
            &ctx,
            &peer(),
        )
        .await
        .response;
        assert!(answer.ok, "{answer:?}");
        assert_eq!(
            persisted(&ctx, ipc_project_id, task_id).model,
            chosen,
            "the model is stored exactly as requested, not substituted"
        );
        assert_eq!(
            gui_on_disk.model, chosen,
            "the desktop write reaches the same value for the same input"
        );

        let shown = read_back(&ctx, task_id).await;
        assert_eq!(shown["model"], chosen, "the slice 1 read verb confirms it");
    }

    /// A malformed identifier is refused and nothing is written: the record, in
    /// memory and on the board file, keeps the value it had.
    #[tokio::test]
    async fn set_model_refuses_a_malformed_identifier_and_changes_nothing() {
        let (_tmp, ctx) = test_ctx().await;
        let project_id = Uuid::new_v4();
        let mut task = backlog_task(project_id, "under test");
        task.model = "default".to_string();
        let (task_id, project_id) = seed(&ctx, task).await;
        let memory_before = serde_json::to_value(ctx.tasks.read().await[&task_id].clone()).unwrap();
        let disk_before = persisted_json(&ctx, project_id, task_id);

        let answer = dispatch(
            IpcRequest::SetTaskModel {
                task_id: task_id.to_string(),
                model: "son net".to_string(),
            },
            &ctx,
            &peer(),
        )
        .await
        .response;

        assert!(!answer.ok, "a malformed model must be refused: {answer:?}");
        let message = answer.error.unwrap_or_default();
        assert!(message.contains("son net"), "{message}");
        assert!(message.contains("was not saved"), "{message}");
        assert_eq!(
            serde_json::to_value(ctx.tasks.read().await[&task_id].clone()).unwrap(),
            memory_before,
            "the refused model must not be written"
        );
        assert_eq!(persisted_json(&ctx, project_id, task_id), disk_before);
        let shown = read_back(&ctx, task_id).await;
        assert_eq!(shown["model"], "default", "the read-back still answers the old value");
    }

    /// The same identifier gets the same answer from the desktop write and from
    /// the control channel, for each Worker. An accepted one is stored as given,
    /// and a refused one changes nothing. No discovery is consulted: a model the
    /// catalog does not list is accepted when it is well formed.
    #[tokio::test]
    async fn a_worker_model_gets_the_same_answer_from_the_form_and_the_control_channel() {
        use crate::domain::task::CodingProvider;
        let cases = [
            (CodingProvider::Claude, "sonnet", true),
            (CodingProvider::Claude, "claude-sonnet-4-5-20250929", true),
            (CodingProvider::OpenCode, "ollama/llama3.2:3b", true),
            (CodingProvider::Claude, "", false),
            (CodingProvider::OpenCode, "", false),
            (CodingProvider::Claude, "son net", false),
            (CodingProvider::Claude, "ollama/llama3.2:3b", false),
            (CodingProvider::OpenCode, "sonnet", false),
        ];
        for (provider, model, accepted) in cases {
            let project_id = Uuid::new_v4();
            let mut task = backlog_task(project_id, "parity");
            task.provider = provider;
            let task_id = task.id;
            let (_gui_tmp, gui_app) = gui_app(project_id, vec![task.clone()]).await;
            let gui = crate::commands::task::update_task(
                GuiMutation::untracked(),
                gui_app.state(),
                new_params(task_id, Some(model.to_string()), None),
            )
            .await;

            let (_tmp, ctx) = test_ctx().await;
            let (ipc_task_id, ipc_project_id) = seed(&ctx, task).await;
            let answer = dispatch(
                IpcRequest::SetTaskModel {
                    task_id: ipc_task_id.to_string(),
                    model: model.to_string(),
                },
                &ctx,
                &peer(),
            )
            .await
            .response;

            assert_eq!(matches!(gui, Ok(Some(_))), accepted, "desktop write, {model:?}: {gui:?}");
            assert_eq!(answer.ok, accepted, "control channel, {model:?}: {answer:?}");
            if accepted {
                let stored = gui_app.state::<crate::AppState>().task.tasks.read().await[&task_id]
                    .model
                    .clone();
                assert_eq!(stored, model, "desktop stores the identifier as given");
                assert_eq!(persisted(&ctx, ipc_project_id, ipc_task_id).model, model);
            } else {
                let desktop_refusal = gui.err().unwrap_or_default();
                assert!(desktop_refusal.contains("was not saved") || desktop_refusal.contains("No model"), "{desktop_refusal}");
                let channel_refusal = answer.error.unwrap_or_default();
                assert!(channel_refusal.contains("was not saved") || channel_refusal.contains("No model"), "{channel_refusal}");
            }
        }
    }

    /// Changing a Task's provider without naming a model re-checks the stored
    /// model: a Claude alias cannot become an OpenCode Worker's model, and a
    /// provider/model id cannot stay on a Claude Worker. Refused, nothing changes.
    #[tokio::test]
    async fn a_provider_change_rechecks_the_stored_model() {
        use crate::domain::task::CodingProvider;
        let project_id = Uuid::new_v4();
        let mut task = backlog_task(project_id, "provider change");
        task.model = "sonnet".to_string();
        let task_id = task.id;
        let (_gui_tmp, gui_app) = gui_app(project_id, vec![task]).await;
        let mut params = new_params(task_id, None, None);
        params.provider = Some(CodingProvider::OpenCode);
        let refused = crate::commands::task::update_task(
            GuiMutation::untracked(),
            gui_app.state(),
            params,
        )
        .await;
        let message = refused.expect_err("a Claude alias must not become an OpenCode model");
        assert!(message.contains("provider/model"), "{message}");
        let stored = gui_app.state::<crate::AppState>().task.tasks.read().await[&task_id].clone();
        assert_eq!(stored.provider, CodingProvider::Claude);
        assert_eq!(stored.model, "sonnet");
    }

    /// Same parity, for the review mode the run's finish is handed in.
    #[tokio::test]
    async fn set_review_mode_persists_what_the_desktop_update_task_persists() {
        let gui_project_id = Uuid::new_v4();
        let gui_task = backlog_task(gui_project_id, "desktop");
        let gui_task_id = gui_task.id;
        let (_gui_tmp, gui_app) = gui_app(gui_project_id, vec![gui_task]).await;
        crate::commands::task::update_task(
            GuiMutation::untracked(),
            gui_app.state(),
            new_params(gui_task_id, None, Some(ReviewMode::Manual)),
        )
        .await
        .expect("the desktop write is stored")
        .expect("the task exists");
        let gui_state = gui_app.state::<crate::AppState>();
        let gui_on_disk = gui_state
            .storage
            .load_project_tasks(gui_project_id)
            .expect("read the board back")
            .into_iter()
            .find(|t| t.id == gui_task_id)
            .expect("on disk");

        let (_tmp, ctx) = test_ctx().await;
        let project_id = Uuid::new_v4();
        let (task_id, ipc_project_id) = seed(&ctx, backlog_task(project_id, "cli")).await;
        let answer = dispatch(
            IpcRequest::SetTaskReviewMode {
                task_id: task_id.to_string(),
                review_mode: "manual".to_string(),
            },
            &ctx,
            &peer(),
        )
        .await
        .response;
        assert!(answer.ok, "{answer:?}");
        assert_eq!(
            persisted(&ctx, ipc_project_id, task_id).review_mode,
            ReviewMode::Manual,
        );
        assert_eq!(
            gui_on_disk.review_mode,
            ReviewMode::Manual,
            "the desktop write reaches the same value for the same input"
        );

        let shown = read_back(&ctx, task_id).await;
        assert_eq!(shown["review_mode"], "manual", "the slice 1 read verb confirms it");
    }

    #[tokio::test]
    async fn set_review_mode_refuses_an_unknown_mode_and_changes_nothing() {
        let (_tmp, ctx) = test_ctx().await;
        let project_id = Uuid::new_v4();
        let (task_id, project_id) = seed(&ctx, backlog_task(project_id, "under test")).await;
        let memory_before = serde_json::to_value(ctx.tasks.read().await[&task_id].clone()).unwrap();
        let disk_before = persisted_json(&ctx, project_id, task_id);

        let answer = dispatch(
            IpcRequest::SetTaskReviewMode {
                task_id: task_id.to_string(),
                review_mode: "sometimes".to_string(),
            },
            &ctx,
            &peer(),
        )
        .await
        .response;

        assert!(!answer.ok, "{answer:?}");
        let message = answer.error.unwrap_or_default();
        assert!(message.contains("Invalid review mode"), "{message}");
        assert_eq!(
            serde_json::to_value(ctx.tasks.read().await[&task_id].clone()).unwrap(),
            memory_before,
        );
        assert_eq!(persisted_json(&ctx, project_id, task_id), disk_before);
    }

    /// Same parity, for the structured dependency. Clearing the dependency is
    /// the empty request, and that is written too.
    #[tokio::test]
    async fn set_dependencies_persists_what_the_desktop_set_task_dependencies_persists() {
        let gui_project_id = Uuid::new_v4();
        let gui_parent = backlog_task(gui_project_id, "parent");
        let gui_child = backlog_task(gui_project_id, "child");
        let (gui_parent_id, gui_child_id) = (gui_parent.id, gui_child.id);
        let (_gui_tmp, gui_app) = gui_app(gui_project_id, vec![gui_parent, gui_child]).await;
        let written = crate::commands::task::set_task_dependencies(
            GuiMutation::untracked(),
            gui_app.state(),
            gui_child_id.to_string(),
            vec![gui_parent_id.to_string()],
        )
        .await
        .expect("the desktop write is stored")
        .expect("the task exists");
        assert_eq!(written.dependencies, vec![gui_parent_id]);
        let gui_state = gui_app.state::<crate::AppState>();
        let gui_on_disk = gui_state
            .storage
            .load_project_tasks(gui_project_id)
            .expect("read the board back")
            .into_iter()
            .find(|t| t.id == gui_child_id)
            .expect("on disk");
        assert_eq!(gui_on_disk.dependencies, vec![gui_parent_id]);

        let (_tmp, ctx) = test_ctx().await;
        let project_id = Uuid::new_v4();
        let parent = backlog_task(project_id, "parent");
        let child = backlog_task(project_id, "child");
        let parent_id = parent.id;
        let (_, ipc_project_id) = seed(&ctx, parent).await;
        let (task_id, _) = seed(&ctx, child).await;
        let answer = dispatch(
            IpcRequest::SetTaskDependencies {
                task_id: task_id.to_string(),
                dependencies: vec![parent_id.to_string()],
            },
            &ctx,
            &peer(),
        )
        .await
        .response;
        assert!(answer.ok, "{answer:?}");
        assert_eq!(
            persisted(&ctx, ipc_project_id, task_id).dependencies,
            vec![parent_id],
            "the desktop write reaches the same value for the same input"
        );

        let shown = read_back(&ctx, task_id).await;
        assert_eq!(
            shown["dependencies"][0],
            parent_id.to_string(),
            "the slice 1 read verb confirms it"
        );

        // An empty request is a clearing, and that is persisted as well.
        let answer = dispatch(
            IpcRequest::SetTaskDependencies { task_id: task_id.to_string(), dependencies: Vec::new() },
            &ctx,
            &peer(),
        )
        .await
        .response;
        assert!(answer.ok, "{answer:?}");
        assert_eq!(persisted(&ctx, ipc_project_id, task_id).dependencies, Vec::<uuid::Uuid>::new());
    }

    /// A request that closes a dependency cycle is refused with the desktop's
    /// own message and nothing is written — in memory, on the board file, and
    /// in what `GetTask` answers.
    #[tokio::test]
    async fn set_dependencies_rejects_a_cycle_and_writes_nothing() {
        let (_tmp, ctx) = test_ctx().await;
        let project_id = Uuid::new_v4();
        let lower = backlog_task(project_id, "lower");
        let mut upper = backlog_task(project_id, "upper");
        // `upper` already runs after `lower`; pointing `lower` at `upper` would
        // close a cycle.
        upper.dependencies = vec![lower.id];
        let (lower_id, upper_id) = (lower.id, upper.id);
        {
            let mut tasks = ctx.tasks.write().await;
            tasks.insert(lower_id, lower);
            tasks.insert(upper_id, upper.clone());
        }
        let board = ctx
            .tasks
            .read()
            .await
            .values()
            .filter(|t| t.project_id == project_id)
            .cloned()
            .collect::<Vec<_>>();
        ctx.storage.save_project_tasks(project_id, &board).expect("seed the board");
        let memory_before = serde_json::to_value(ctx.tasks.read().await[&lower_id].clone()).unwrap();
        let disk_before = persisted_json(&ctx, project_id, lower_id);

        let answer = dispatch(
            IpcRequest::SetTaskDependencies {
                task_id: lower_id.to_string(),
                dependencies: vec![upper_id.to_string()],
            },
            &ctx,
            &peer(),
        )
        .await
        .response;

        assert!(!answer.ok, "a cycle must be refused: {answer:?}");
        let message = answer.error.unwrap_or_default();
        assert!(message.contains("depend on itself"), "{message}");
        assert_eq!(
            serde_json::to_value(ctx.tasks.read().await[&lower_id].clone()).unwrap(),
            memory_before,
            "the refused request must not be written"
        );
        assert_eq!(persisted_json(&ctx, project_id, lower_id), disk_before);
        let shown = read_back(&ctx, lower_id).await;
        assert_eq!(
            shown["dependencies"].as_array().map(Vec::len),
            Some(0),
            "the Task keeps its empty dependency set"
        );
    }

    /// A live run's Task refuses a configuration write, with the desktop's own
    /// refusal, and the board keeps everything it held — the model and review
    /// mode through `commit_task`'s recovery refusal, the dependency through
    /// the checkout rule.
    #[tokio::test]
    async fn a_task_awaiting_a_decision_or_holding_a_checkout_refuses_every_write() {
        let (_tmp, ctx) = test_ctx().await;
        let project_id = Uuid::new_v4();
        let mut running = backlog_task(project_id, "running");
        running.status = TaskStatus::InProgress;
        // The durable start of a run is the evidence the desktop's own edit
        // boundary refuses on.
        running.worker_handoff = Some(crate::domain::WorkerHandoff {
            review_mode: Default::default(),
            task_id: running.id,
            project_id,
            repository_id: None,
            run: Some(1),
            branch: None,
            start_head: None,
            stage: crate::domain::WorkerHandoffStage::Started {},
            acceptance: None,
            review_mode_unproven: false,
            reviewer_model: None,
        });
        let running_id = running.id;

        let mut checked_out = backlog_task(project_id, "checked out");
        checked_out.branch_name = Some("slashit/checked-out".to_string());
        let checked_out_id = checked_out.id;
        {
            let mut tasks = ctx.tasks.write().await;
            tasks.insert(running_id, running.clone());
            tasks.insert(checked_out_id, checked_out.clone());
        }
        let board = ctx
            .tasks
            .read()
            .await
            .values()
            .filter(|t| t.project_id == project_id)
            .cloned()
            .collect::<Vec<_>>();
        ctx.storage.save_project_tasks(project_id, &board).expect("seed the board");
        let running_memory =
            serde_json::to_value(ctx.tasks.read().await[&running_id].clone()).unwrap();
        let running_disk = persisted_json(&ctx, project_id, running_id);
        let checked_out_memory =
            serde_json::to_value(ctx.tasks.read().await[&checked_out_id].clone()).unwrap();
        let checked_out_disk = persisted_json(&ctx, project_id, checked_out_id);

        let model = dispatch(
            IpcRequest::SetTaskModel {
                task_id: running_id.to_string(),
                model: "default".to_string(),
            },
            &ctx,
            &peer(),
        )
        .await
        .response;
        assert!(!model.ok, "a running Task must refuse a model edit: {model:?}");
        assert!(
            model
                .error
                .clone()
                .unwrap_or_default()
                .contains("unresolved Worker recovery evidence"),
            "{model:?}"
        );

        let review_mode = dispatch(
            IpcRequest::SetTaskReviewMode {
                task_id: running_id.to_string(),
                review_mode: "manual".to_string(),
            },
            &ctx,
            &peer(),
        )
        .await
        .response;
        assert!(!review_mode.ok, "a running Task must refuse a review mode edit: {review_mode:?}");

        let dependencies = dispatch(
            IpcRequest::SetTaskDependencies {
                task_id: checked_out_id.to_string(),
                dependencies: vec![running_id.to_string()],
            },
            &ctx,
            &peer(),
        )
        .await
        .response;
        assert!(!dependencies.ok, "a checkout must refuse a dependency edit: {dependencies:?}");
        assert!(
            dependencies.error.clone().unwrap_or_default().contains("checkout"),
            "{dependencies:?}"
        );

        assert_eq!(
            serde_json::to_value(ctx.tasks.read().await[&running_id].clone()).unwrap(),
            running_memory,
            "the refused model and review mode edits changed nothing"
        );
        assert_eq!(persisted_json(&ctx, project_id, running_id), running_disk);
        assert_eq!(
            serde_json::to_value(ctx.tasks.read().await[&checked_out_id].clone()).unwrap(),
            checked_out_memory,
            "the refused dependency edit changed nothing"
        );
        assert_eq!(persisted_json(&ctx, project_id, checked_out_id), checked_out_disk);
    }

    #[tokio::test]
    async fn the_configuration_verbs_answer_a_task_that_is_not_on_the_board() {
        let (_tmp, ctx) = test_ctx().await;
        let missing = Uuid::new_v4().to_string();
        for request in [
            IpcRequest::SetTaskModel { task_id: missing.clone(), model: "default".to_string() },
            IpcRequest::SetTaskReviewMode {
                task_id: missing.clone(),
                review_mode: "manual".to_string(),
            },
            IpcRequest::SetTaskDependencies { task_id: missing.clone(), dependencies: Vec::new() },
        ] {
            let answer = dispatch(request, &ctx, &peer()).await.response;
            assert!(!answer.ok, "a task that is not on the board cannot be changed: {answer:?}");
            assert!(
                answer.error.clone().unwrap_or_default().contains("not found"),
                "{answer:?}"
            );
        }
    }
}

/// The lock order of the Project portability handlers: Tasks, then Projects,
/// then Repositories, the order Apply and Task creation use.
///
/// A handler that takes Projects before Tasks can hold Projects while it waits
/// for Tasks. An Apply that holds Tasks and waits for Projects then closes the
/// cycle. The test is deterministic: the runtime is single-threaded and the test
/// holds the Projects write lock, which parks each handler where it first needs
/// Projects. A handler that honors the order is parked holding Tasks.
#[cfg(test)]
mod import_lock_order {
    use super::*;
    use crate::config::paths::{AppPaths, StateLocation};
    use crate::domain::{AgentConfig, AgentType, Project, ProjectScope};
    use crate::test_helpers::ipc_test_context;
    use std::sync::Arc;

    fn test_ctx() -> (tempfile::TempDir, IpcContext) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        (tmp, ipc_test_context(paths))
    }

    fn project_for(id: Uuid) -> Project {
        Project {
            id,
            name: "Lock Order Project".to_string(),
            repository_id: None,
            scope: ProjectScope::Standalone,
            state_location: StateLocation::External,
            base: None,
            agent_type: AgentType::ClaudeCode,
            agent_config: AgentConfig {
                agent_type: AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: std::collections::HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// Runs `handler` with the Projects write lock held by the test, so the
    /// handler parks at the first point where it needs Projects. Three yields
    /// let the single-threaded runtime poll the handler until it is parked; a
    /// parked handler stays parked, so the probe then reads a fixed state. The
    /// handler must already hold Tasks there, so a Tasks writer cannot get in.
    async fn assert_tasks_held_before_projects<F>(ctx: &IpcContext, handler: F)
    where
        F: std::future::Future<Output = IpcResponse>,
    {
        let projects = ctx.projects.write().await;
        let probe = async {
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            let tasks_held = ctx.tasks.try_write().is_err();
            drop(projects);
            tasks_held
        };
        let (response, tasks_held) = tokio::join!(handler, probe);
        assert!(
            tasks_held,
            "the handler waits for Projects without holding Tasks: an Apply holding Tasks and \
             waiting for Projects deadlocks against it"
        );
        assert!(response.ok, "the handler must finish once the lock is free: {response:?}");
    }

    #[tokio::test]
    async fn export_takes_tasks_before_projects() {
        let (tmp, ctx) = test_ctx();
        let project_id = Uuid::new_v4();
        ctx.projects.write().await.insert(project_id, project_for(project_id));
        let destination = tmp.path().join("export.slashit-project");
        let handler = handle_export_project(
            &ctx,
            project_id.to_string(),
            destination.to_string_lossy().into_owned(),
        );
        assert_tasks_held_before_projects(&ctx, handler).await;
    }

    #[tokio::test]
    async fn preview_takes_tasks_before_projects() {
        let (tmp, ctx) = test_ctx();
        let project_id = Uuid::new_v4();
        ctx.projects.write().await.insert(project_id, project_for(project_id));
        let destination = tmp.path().join("export.slashit-project");
        let exported = handle_export_project(
            &ctx,
            project_id.to_string(),
            destination.to_string_lossy().into_owned(),
        )
        .await;
        assert!(exported.ok, "{exported:?}");
        let handler = handle_preview_import(&ctx, destination.to_string_lossy().into_owned());
        assert_tasks_held_before_projects(&ctx, handler).await;
    }
}

/// `GetTaskEvidence`: one Task's recent execution evidence, read-only.
#[cfg(test)]
mod get_task_evidence {
    use super::*;
    use crate::domain::task::{CodingProvider, QaSignoff, QaStatus};
    use crate::test_helpers::{attach_test_executor_ipc, create_test_task_full, ipc_test_context};
    use slashit_activity::{Kind, RunResult};
    use std::sync::Arc;

    async fn evidence(ctx: &IpcContext, id: &str) -> serde_json::Value {
        let reply = dispatch(
            IpcRequest::GetTaskEvidence {
                task_id: id.to_string(),
            },
            ctx,
            &peer(),
        )
        .await
        .response;
        assert!(reply.ok, "{:?}", reply.error);
        reply.data
    }

    fn peer() -> PeerContext {
        PeerContext {
            endpoint: slashit_ipc::Endpoint::Unix {
                path: std::path::PathBuf::from("/nonexistent-for-tests"),
            },
            os_verified: true,
        }
    }

    /// A Task whose record shows everything this read reports on: two
    /// completed runs with results, a failed latest one, one AI review, and
    /// a configured-versus-observed model pair.
    async fn rich_world() -> (tempfile::TempDir, IpcContext, Uuid) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("Evidence test", project_id, TaskStatus::Error, 0);
        task.model = "claude-sonnet-5-5".to_string();
        task.provider = CodingProvider::OpenCode;
        task.last_observed_model = Some("claude-opus-4-0-20250514".to_string());
        task.error_message = Some("the last run failed: worker exited".to_string());
        task.qa_signoff = Some(QaSignoff {
            status: QaStatus::Approved,
            issues_found: Vec::new(),
            timestamp: chrono::Utc::now(),
            session_id: Uuid::new_v4(),
        });
        let at = chrono::Utc::now() - chrono::Duration::hours(1);
        task.record_activity_at(
            at,
            Kind::RunStarted {
                run: 1,
                addressing_feedback: false,
            },
        );
        task.record_activity_at(
            at,
            Kind::ToolUsed {
                run: 1,
                tool: "Bash".into(),
                detail: Some("ls".into()),
                count: 1,
            },
        );
        task.record_activity_at(at, Kind::RunCompleted { run: 1 });
        task.record_activity_at(
            at,
            Kind::RunResult {
                run: 1,
                result: RunResult::Available {
                    text: "did the work".into(),
                },
            },
        );
        task.record_activity_at(
            at,
            Kind::RunStarted {
                run: 2,
                addressing_feedback: true,
            },
        );
        task.record_activity_at(at, Kind::RunCompleted { run: 2 });
        task.record_activity_at(
            at,
            Kind::RunResult {
                run: 2,
                result: RunResult::Truncated {
                    text: "partial answer".into(),
                    original_chars: 9_999,
                },
            },
        );
        task.record_activity_at(at, Kind::AiReviewStarted { review: 3 });
        task.record_activity_at(at, Kind::AiReviewApproved { review: 3 });
        assert!(task.record_activity_at(
            at,
            Kind::RunFailed {
                run: Some(4),
                reason: "provider unreachable".into()
            }
        ));

        let id = task.id;
        ctx.tasks.write().await.insert(id, task.clone());
        ctx.storage
            .save_project_tasks(project_id, &[task])
            .expect("seed the board");
        (tmp, ctx, id)
    }

    fn files_under(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        crate::test_helpers::files_under(root)
    }

    #[tokio::test]
    async fn reports_the_persisted_evidence_the_record_holds() {
        let (_tmp, ctx, id) = rich_world().await;
        let d = evidence(&ctx, &id.to_string()).await;

        assert_eq!(d["task_id"], id.to_string());
        assert_eq!(d["title"], "Evidence test");
        assert_eq!(d["provider"], "OpenCode");
        assert_eq!(d["configured_model"], "claude-sonnet-5-5");
        assert_eq!(d["latest_observed_model"], "claude-opus-4-0-20250514");
        // The observed identity is the Task's latest, kept whole on the Task
        // record — never broken out per run below.
        assert_eq!(
            d["runs"]
                .as_array()
                .expect("runs")
                .iter()
                .filter(|run| run.get("observed_model").is_some()
                    || run.get("latest_observed_model").is_some())
                .count(),
            0,
            "no run carries a model identity"
        );

        // Persisted runs, newest first, with the record's own outcome. Runs
        // 1, 2 and 4 ran; the reviews are 3, and are not runs.
        let runs = d["runs"].as_array().expect("runs");
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0]["run"], 4);
        assert_eq!(runs[0]["status"], "failed");
        assert_eq!(runs[0]["failure_reason"], "provider unreachable");
        assert!(runs[0]["outcome"].is_null());
        assert_eq!(runs[2]["run"], 1);
        assert_eq!(runs[2]["status"], "completed");
        assert_eq!(runs[2]["outcome"]["kind"], "available");
        assert_eq!(runs[2]["outcome"]["text"], "did the work");
        assert_eq!(runs[1]["run"], 2);
        assert_eq!(runs[1]["status"], "completed");
        assert_eq!(runs[1]["outcome"]["kind"], "truncated");
        assert_eq!(runs[1]["outcome"]["original_chars"], 9_999);
        for run in runs {
            assert!(run["outcome"].is_null() || run["outcome"]["echo_chars_omitted"].is_null());
            assert!(
                run.get("observed_model").is_none() && run.get("latest_observed_model").is_none(),
                "no run carries a model identity: the latest observed model is a Task-level \
                 fact, and attributing it to a run would be invented history: {run}"
            );
        }

        assert_eq!(d["ai_review"]["review"], 3);
        assert_eq!(d["ai_review"]["outcome"], "approved");
        assert_eq!(d["qa_signoff"], "approved");

        assert_eq!(d["failure_reason"], "the last run failed: worker exited");
        assert_eq!(d["failure_reason_truncated"], false);

        // Newest first, and the run milestones the runs above describe.
        let activity = d["activity"].as_array().expect("activity");
        assert!(!activity.is_empty());
        assert!(activity.len() <= MAX_EVIDENCE_ACTIVITY_ENTRIES);
        let first = activity[0]["kind"]
            .as_object()
            .expect("a kind")
            .keys()
            .next();
        // Ordered newest first, so the oldest-looking milestone is last.
        assert_eq!(
            activity.last().unwrap()["kind"]["type"],
            "run_started",
            "{first:?}"
        );

        // No executor is installed here, so there is nothing this process
        // supervised: the session half is null (the executor-backed shape is
        // the subject of the next tests).
        assert!(d["session"].is_null());

        // And the answer always says what it does not hold.
        assert_eq!(d["output"]["available"], false);
        assert_eq!(d["output"]["memory_only"], true);
        let note = d["output"]["note"].as_str().expect("note");
        assert!(note.contains("memory"), "{note}");
    }

    #[tokio::test]
    async fn an_observed_model_is_only_ever_a_recorded_one() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("alias only", project_id, TaskStatus::Backlog, 0);
        // A configured alias is the model requested. No run has ever reported
        // one, and none may be invented from the alias.
        task.model = "haiku".to_string();
        assert_eq!(task.last_observed_model, None);
        let id = task.id;
        ctx.tasks.write().await.insert(id, task.clone());

        let d = evidence(&ctx, &id.to_string()).await;
        assert_eq!(d["configured_model"], "haiku");
        assert!(
            d["latest_observed_model"].is_null(),
            "an observed model was invented from a configured alias: {d}"
        );
    }

    #[tokio::test]
    async fn the_latest_run_describes_what_this_instance_supervised() {
        let (_tmp, ctx, id) = rich_world().await;
        let executor = attach_test_executor_ipc(&ctx).await;
        let execution = executor
            .record_ended_execution_for_test(
                id,
                crate::domain::AgentStatus::Stopped,
                &[
                    "Session started: 11111111-1111-1111-1111-111111111111 (model: claude-opus-4-0-20250514)",
                    "Using tool: Bash",
                    "wrote ghp_abcdefghij123456 into the patch",
                ],
            )
            .await;

        let d = evidence(&ctx, &id.to_string()).await;

        let session = &d["session"];
        assert_eq!(session["live"], false);
        assert_eq!(session["status"], "stopped");
        assert_eq!(session["execution_id"], execution.to_string());
        assert!(session["started_at"].is_string());
        assert!(session["stopped_at"].is_string());

        let output = &d["output"];
        assert_eq!(output["available"], true);
        assert_eq!(output["memory_only"], true);
        let note = output["note"].as_str().expect("note");
        assert!(note.contains("restart"), "{note}");
        let entries = output["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 3);
        // A credential-shaped value in the run's own output is masked here, as
        // it is everywhere else the record repeats agent text. The newest
        // entry is first.
        let newest = entries[0]["message"].as_str().expect("msg");
        assert!(newest.contains("***"), "{newest}");
        assert_eq!(entries[1]["message"], "Using tool: Bash");
        assert_eq!(entries[1]["message_truncated"], false);
    }

    /// The restart case. The Task's persisted evidence — its record, its
    /// Activity, the run outcomes and the Task's latest observed model —
    /// survives, while everything this session held (the output, the
    /// execution id, the run state) is gone, and the answer says so in one
    /// sentence.
    #[tokio::test]
    async fn after_a_restart_the_persisted_evidence_is_all_there_is() {
        let (_tmp, first, id) = rich_world().await;
        let task = first.tasks.read().await.get(&id).expect("seeded").clone();
        let executor = attach_test_executor_ipc(&first).await;
        executor
            .record_ended_execution_for_test(
                id,
                crate::domain::AgentStatus::Stopped,
                &["this session saw output"],
            )
            .await;

        // A second instance over the same storage: the task is re-read from
        // the board, and this new process's executor has its own empty maps.
        let before = evidence(&first, &id.to_string()).await;
        assert_eq!(
            before["output"]["available"], true,
            "the first instance held output"
        );

        let ctx = ipc_test_context(first.paths.clone());
        let project_id = task.project_id;
        let reloaded = ctx
            .storage
            .load_project_tasks(project_id)
            .expect("the board is readable")
            .into_iter()
            .find(|t| t.id == id)
            .expect("the task is on disk");
        ctx.tasks.write().await.insert(id, reloaded);
        attach_test_executor_ipc(&ctx).await;

        let after = evidence(&ctx, &id.to_string()).await;

        // What was durable is still there.
        assert_eq!(after["task_id"], id.to_string());
        assert_eq!(after["latest_observed_model"], "claude-opus-4-0-20250514");
        assert_eq!(after["runs"].as_array().map(Vec::len).unwrap(), 3);
        assert_eq!(after["runs"][0]["failure_reason"], "provider unreachable");

        // What was only in memory is not: and the answer says what happened.
        let session = &after["session"];
        assert_eq!(session["live"], false);
        assert!(session["status"].is_null());
        assert!(session["execution_id"].is_null());
        let output = &after["output"];
        assert_eq!(output["available"], false);
        let note = output["note"].as_str().expect("note");
        assert!(
            note.contains("lost when that instance ended"),
            "the answer must say the output died with the earlier instance: {note}"
        );
    }

    #[tokio::test]
    async fn the_number_of_runs_described_is_bounded() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("many runs", project_id, TaskStatus::Error, 0);
        let at = chrono::Utc::now();
        for run in 1..=7 {
            task.record_activity_at(
                at,
                Kind::RunStarted {
                    run,
                    addressing_feedback: false,
                },
            );
            // A large result per run, so the byte bound is exercised too.
            task.record_activity_at(
                at,
                Kind::RunResult {
                    run,
                    result: RunResult::Available {
                        text: "x".repeat(4_000),
                    },
                },
            );
            task.record_activity_at(
                at,
                Kind::RunFailed {
                    run: Some(run),
                    reason: format!("run {run} failed"),
                },
            );
        }
        let id = task.id;
        ctx.tasks.write().await.insert(id, task.clone());

        let d = evidence(&ctx, &id.to_string()).await;
        let runs = d["runs"].as_array().expect("runs");
        assert_eq!(runs.len(), MAX_EVIDENCE_RUNS, "newest first");
        assert_eq!(runs[0]["run"], 7);
        assert_eq!(runs[4]["run"], 3);
        assert_eq!(d["runs_omitted"], 2);
        // Every echoed result is within the echo's own bound; the record set
        // no truncation, so the flag names what this echo cut.
        for run in runs {
            assert_eq!(run["outcome"]["kind"], "available");
            let text = run["outcome"]["text"].as_str().expect("text");
            assert_eq!(text.chars().count(), MAX_EVIDENCE_RESULT_CHARS);
            assert_eq!(
                run["outcome"]["echo_chars_omitted"].as_u64().unwrap(),
                3_000
            );
        }
    }

    #[tokio::test]
    async fn the_activity_echo_is_bounded_in_entries_and_bytes() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full("long history", project_id, TaskStatus::Error, 0);
        let at = chrono::Utc::now();
        for run in 1..=30 {
            task.record_activity_at(
                at,
                Kind::RunStarted {
                    run,
                    addressing_feedback: false,
                },
            );
            task.record_activity_at(at, Kind::RunCompleted { run });
            task.record_activity_at(
                at,
                Kind::RunResult {
                    run,
                    result: RunResult::Available {
                        text: "y".repeat(4_000),
                    },
                },
            );
        }
        let id = task.id;
        ctx.tasks.write().await.insert(id, task.clone());

        let d = evidence(&ctx, &id.to_string()).await;
        let activity = d["activity"].as_array().expect("activity");
        assert!(activity.len() <= MAX_EVIDENCE_ACTIVITY_ENTRIES);
        let bytes: usize = activity
            .iter()
            .map(|v| serde_json::to_string(v).unwrap().len())
            .sum();
        assert!(bytes <= MAX_EVIDENCE_ACTIVITY_BYTES, "{bytes}");
        assert_eq!(
            d["activity_omitted"].as_u64().unwrap() + activity.len() as u64,
            90
        );
    }

    #[tokio::test]
    async fn makes_no_write() {
        let (tmp, ctx, id) = rich_world().await;
        let before = files_under(tmp.path());
        let _ = evidence(&ctx, &id.to_string()).await;
        assert_eq!(
            files_under(tmp.path()),
            before,
            "an evidence read must leave the store byte-identical"
        );
    }

    #[tokio::test]
    async fn unknown_or_malformed_request_fails_closed() {
        let (_tmp, ctx, _id) = rich_world().await;
        let bad = dispatch(
            IpcRequest::GetTaskEvidence {
                task_id: "nope".into(),
            },
            &ctx,
            &PeerContext {
                endpoint: slashit_ipc::Endpoint::Unix {
                    path: std::path::PathBuf::from("/nonexistent-for-tests"),
                },
                os_verified: true,
            },
        )
        .await
        .response;
        assert!(!bad.ok);
        assert!(bad.error.unwrap().contains("Invalid task_id"));

        let missing = evidence_or_error(&ctx, &Uuid::new_v4().to_string()).await;
        assert!(!missing.ok);
        assert!(missing.error.unwrap().contains("not found"));
    }

    async fn evidence_or_error(ctx: &IpcContext, id: &str) -> IpcResponse {
        dispatch(
            IpcRequest::GetTaskEvidence {
                task_id: id.to_string(),
            },
            ctx,
            &PeerContext {
                endpoint: slashit_ipc::Endpoint::Unix {
                    path: std::path::PathBuf::from("/nonexistent-for-tests"),
                },
                os_verified: true,
            },
        )
        .await
        .response
    }

    /// A review a newer run began after is not erased from the evidence: it
    /// is the latest review on record, reported with that run named so it is
    /// never passed off as a verdict about the run that followed it.
    #[tokio::test]
    async fn a_review_the_newest_run_started_after_is_reported_superseded() {
        let (_tmp, ctx, id) = rich_world().await;
        // The last thing rich_world's record holds is the failed run 4; a
        // later run starts after its latest review (review 3).
        {
            let mut tasks = ctx.tasks.write().await;
            let task = tasks.get_mut(&id).expect("seeded");
            task.record_activity_at(
                chrono::Utc::now(),
                Kind::RunStarted {
                    run: 5,
                    addressing_feedback: false,
                },
            );
        }

        let d = evidence(&ctx, &id.to_string()).await;
        let review = &d["ai_review"];
        assert_eq!(review["outcome"], "approved", "{review}");
        assert_eq!(review["review"], 3, "{review}");
        assert_eq!(
            review["superseded_by_run"], 5,
            "the review must say which run it predates: {review}"
        );

        // The rendered report carries the same honesty, bound to the run.
        let text = slashit::output::run_report_text(&d);
        assert!(
            text.contains("AI review:        approved (review 3, signoff: approved)\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "superseded by run 5: recorded before run 5 began, not evidence about run 5\n"
            ),
            "{text}"
        );
    }

    /// A fix attempt is still the changes-requested verdict it answered when
    /// a later run begins; the report may keep it, but only as a verdict the
    /// newer run has not yet earned.
    #[tokio::test]
    async fn a_fix_attempt_followed_by_a_new_run_is_reported_superseded() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        let mut task =
            create_test_task_full("fix then rerun", Uuid::new_v4(), TaskStatus::AiReview, 0);
        let at = chrono::Utc::now();
        task.record_activity_at(at, Kind::AiReviewStarted { review: 1 });
        task.record_activity_at(
            at,
            Kind::AiReviewChangesRequested {
                review: 1,
                issues: 2,
            },
        );
        task.record_activity_at(at, Kind::AiFixApplied { review: 1 });
        task.record_activity_at(
            at,
            Kind::RunStarted {
                run: 2,
                addressing_feedback: true,
            },
        );

        let id = task.id;
        ctx.tasks.write().await.insert(id, task);

        let d = evidence(&ctx, &id.to_string()).await;
        assert_eq!(d["ai_review"]["outcome"], "changes_requested", "{d}");
        assert_eq!(d["ai_review"]["review"], 1, "{d}");
        assert_eq!(
            d["ai_review"]["superseded_by_run"], 2,
            "a fix attempt before run 2 began is a verdict about run 1's work: {d}"
        );
    }

    /// A record with runs and no review anywhere reports no review at all —
    /// the absence is not the loss of one.
    #[tokio::test]
    async fn no_persisted_review_is_no_review() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        let mut task =
            create_test_task_full("never reviewed", Uuid::new_v4(), TaskStatus::Error, 0);
        let at = chrono::Utc::now();
        task.record_activity_at(
            at,
            Kind::RunStarted {
                run: 1,
                addressing_feedback: false,
            },
        );
        task.record_activity_at(at, Kind::RunCompleted { run: 1 });
        task.record_activity_at(
            at,
            Kind::RunFailed {
                run: Some(1),
                reason: "the provider dropped".into(),
            },
        );

        let id = task.id;
        ctx.tasks.write().await.insert(id, task);

        let d = evidence(&ctx, &id.to_string()).await;
        assert!(
            d["ai_review"].is_null(),
            "no review record exists, so no review is reported: {d}"
        );
    }

    #[tokio::test]
    async fn a_fix_failure_is_distinguished_from_an_ordinary_changes_request() {
        // A review asked for changes, the fix pass answered, and the fix pass
        // itself ended in an error: the persistence keeps all four facts, and
        // the answer reports the fix failure — with its bounded reason —
        // rather than blurring it into the changes-requested verdict it tried
        // to answer.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        let mut task = create_test_task_full("fix failed", Uuid::new_v4(), TaskStatus::AiReview, 0);
        let at = chrono::Utc::now();
        task.record_activity_at(at, Kind::AiReviewStarted { review: 1 });
        task.record_activity_at(
            at,
            Kind::AiReviewChangesRequested {
                review: 1,
                issues: 3,
            },
        );
        task.record_activity_at(at, Kind::AiFixStarted { review: 1 });
        task.record_activity_at(
            at,
            Kind::AiFixFailed {
                review: 1,
                reason: format!(
                    "the fix agent could not write the checkout, {SECRET_SHAPED}; {}",
                    "e".repeat(700)
                ),
            },
        );
        let id = task.id;
        ctx.tasks.write().await.insert(id, task);

        let d = evidence(&ctx, &id.to_string()).await;
        let review = &d["ai_review"];
        assert_eq!(review["review"], 1, "{review}");
        assert_eq!(
            review["outcome"], "fix_failed",
            "a failed fix pass is its own outcome, not a changes request: {review}"
        );
        // The fix's own failure explanation is kept, redacted and within the
        // bound every echoed reason obeys.
        assert!(!serde_json::to_string(&d).unwrap().contains(SECRET_SHAPED));
        let reason = review["reason"].as_str().expect("reason");
        assert!(
            reason.chars().count() <= MAX_EVIDENCE_FAILURE_CHARS,
            "{reason}"
        );
    }

    /// A credential-shaped word versus which nothing in the answer may survive
    /// whole.
    const SECRET_SHAPED: &str = "ghp_theOtherToken9876543210";

    #[tokio::test]
    async fn a_fix_under_way_or_finished_without_incident_keeps_the_verdict() {
        for kind in [
            Kind::AiFixStarted { review: 1 },
            Kind::AiFixApplied { review: 1 },
            Kind::AiFixUnchanged { review: 1 },
        ] {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
                tmp.path().join("config"),
                tmp.path().join("data"),
                tmp.path().join("cache"),
                tmp.path().join("runtime"),
            ));
            let ctx = ipc_test_context(paths);
            let mut task =
                create_test_task_full("fix under way", Uuid::new_v4(), TaskStatus::AiReview, 0);
            let at = chrono::Utc::now();
            task.record_activity_at(at, Kind::AiReviewStarted { review: 1 });
            task.record_activity_at(
                at,
                Kind::AiReviewChangesRequested {
                    review: 1,
                    issues: 2,
                },
            );
            task.record_activity_at(at, kind);
            let id = task.id;
            ctx.tasks.write().await.insert(id, task);

            let d = evidence(&ctx, &id.to_string()).await;
            assert_eq!(
                d["ai_review"]["outcome"], "changes_requested",
                "the verdict to report is still the changes request: {d}"
            );
            assert!(d["ai_review"]["reason"].is_null(), "{d}");
        }
    }

    /// The whole answer, largest legal combination of everything at once: a
    /// bounding-needle title and models, a bounding failure reason, a failed
    /// session with a bounding reason, five runs each carrying a bounding
    /// outcome and reason, a bounding output buffer of long lines, and an
    /// Activity history big enough to spend the echo's byte budget. The
    /// serialized answer is the thing measured — this is how the constants
    /// above are verified to add up under the documented wire frame limit
    /// (`MAX_REQUEST_BYTES`, which the read the client makes is bounded by)
    /// and under this read's own aggregate ceiling.
    ///
    /// The answer is not merely *within* every bound: each bound is the one
    /// that decided — the runs and the echoed output entries are capped at
    /// their own constants with older ones counted in the omitted fields, and
    /// the Activity echo stops on its byte budget, not on its entry cap. A
    /// test that only checked an upper bound could pass while an under-built
    /// answer echoed half of what the constants allow.
    #[tokio::test]
    async fn the_whole_answer_stays_within_the_wire_frame_limit() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        let project_id = Uuid::new_v4();
        let mut task = create_test_task_full(
            &format!("edge{}", "t".repeat(MAX_EVIDENCE_TITLE_CHARS + 100)),
            project_id,
            TaskStatus::Error,
            0,
        );
        // Four-byte characters everywhere, so character bounds are also byte
        // bounds; quotes and newlines so JSON escaping is spent too.
        let fat = format!("✩{}", "ζ̈\\\"\n".repeat(300).repeat(2));
        task.model = fat.clone();
        task.last_observed_model = Some(fat.clone());
        task.error_message = Some(fat.clone());
        let at = chrono::Utc::now();
        for run in 1..=40 {
            task.record_activity_at(
                at,
                Kind::RunStarted {
                    run,
                    addressing_feedback: false,
                },
            );
            task.record_activity_at(at, Kind::RunCompleted { run });
            task.record_activity_at(
                at,
                Kind::RunResult {
                    run,
                    result: RunResult::Available {
                        text: format!("r{run} ✩{}", "result ζ̈\\\"\n".repeat(500).repeat(3)),
                    },
                },
            );
            task.record_activity_at(
                at,
                Kind::RunFailed {
                    run: Some(run),
                    reason: format!("run {run} ✩{}", "failed ζ̈\\\"\n".repeat(500).repeat(2)),
                },
            );
        }

        let id = task.id;
        ctx.tasks.write().await.insert(id, task.clone());
        let executor = attach_test_executor_ipc(&ctx).await;
        // More output lines than the echo keeps, so the echo's own cap is
        // what bounds the answer, not however many were recorded.
        let mut lines = Vec::new();
        for i in 0..(MAX_EVIDENCE_OUTPUT_ENTRIES + 8) {
            lines.push(format!(
                "line {i} ✩{}",
                "output ζ̈\\\"\n".repeat(500).repeat(2)
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        executor
            .record_ended_execution_for_test(
                id,
                crate::domain::AgentStatus::Failed(fat.clone()),
                &refs,
            )
            .await;

        let reply = handle_get_task_evidence(&ctx, id.to_string()).await;
        assert!(reply.ok, "{:?}", reply.error);
        let d = &reply.data;

        // The intended caps are reached, not merely satisfied. Nothing here
        // just stayed under an upper bound: each bound below is the one that
        // decided.
        // runs — five described, newest first, the other thirty-five counted;
        // every outcome is cut to the echo's own result bound.
        let runs = d["runs"].as_array().expect("runs");
        assert_eq!(runs.len(), MAX_EVIDENCE_RUNS);
        assert_eq!(runs[0]["run"], 40);
        assert_eq!(runs[MAX_EVIDENCE_RUNS - 1]["run"], 36);
        assert_eq!(
            d["runs_omitted"].as_u64(),
            Some((40 - MAX_EVIDENCE_RUNS) as u64)
        );
        for run in runs {
            let outcome = &run["outcome"];
            // The record cut each result to its own bound (`truncated`, with
            // the full length it kept), and the echo cut further still.
            assert!(
                outcome["kind"] == "truncated",
                "a result this size is recorded cut, with the full length \
                 named: {run:?}"
            );
            let text = outcome["text"].as_str().expect("text");
            assert_eq!(
                text.chars().count(),
                MAX_EVIDENCE_RESULT_CHARS,
                "the echo's own result bound is what the outcome was cut to: {run:?}"
            );
            assert!(
                outcome["echo_chars_omitted"].as_u64().unwrap_or(0) > 0,
                "the echo genuinely cut further than the record did: {run:?}"
            );
        }
        // output — thirty-two long entries echoed, the eight older ones
        // counted, each message cut to its own bound.
        let output = &d["output"];
        let entries = output["entries"].as_array().expect("output entries");
        assert_eq!(entries.len(), MAX_EVIDENCE_OUTPUT_ENTRIES);
        assert_eq!(
            output["entries_omitted"].as_u64(),
            Some((lines.len() - MAX_EVIDENCE_OUTPUT_ENTRIES) as u64)
        );
        for entry in entries {
            assert_eq!(
                entry["message"].as_str().map(|m| m.chars().count()),
                Some(MAX_EVIDENCE_MESSAGE_CHARS),
                "every echoed line is at the echo's own message bound: {entry:?}"
            );
            assert_eq!(entry["message_truncated"], true, "{entry:?}");
        }
        // Activity — the richest the bounds allow, and the byte budget is the
        // cap that fired: the echoed milestones sit within the budget, and
        // echoing the next stored milestone would not have fit within it,
        // which is exactly why echoing stopped where it did. The whole
        // history is much longer than the entry cap, so this is the budget's
        // doing, not a run of shorter history.
        let activity = d["activity"].as_array().expect("activity");
        let activity_bytes: usize = activity
            .iter()
            .map(|value| serde_json::to_string(value).unwrap().len())
            .sum();
        assert!(
            activity_bytes <= MAX_EVIDENCE_ACTIVITY_BYTES,
            "the echoed milestones total {activity_bytes} bytes"
        );
        assert!(
            activity.len() < MAX_EVIDENCE_ACTIVITY_ENTRIES,
            "the byte budget, not the entry cap, is what stopped the echo: \
             {} milestones fit",
            activity.len()
        );
        assert!(d["activity_omitted"].as_u64().unwrap_or(0) > 0);
        {
            // The next stored milestone the echo declined, newest first, and
            // what it would have cost — the same measurement the echo makes.
            let next = &task.activity[task.activity.len() - 1 - activity.len()];
            let mut next_value = serde_json::to_value(next).expect("serializable");
            evidence_redact_value(&mut next_value);
            let next_cost = serde_json::to_string(&next_value).unwrap().len();
            assert!(
                activity_bytes + next_cost > MAX_EVIDENCE_ACTIVITY_BYTES,
                "the byte budget was genuinely reached: {activity_bytes} bytes \
                 echoed, and holding the next milestone would only have cost \
                 {next_cost} more"
            );
        }

        let bytes = serde_json::to_string(d).unwrap().len();
        // The response the handler built is within this read's own aggregate
        // ceiling ...
        assert!(
            bytes <= MAX_EVIDENCE_RESPONSE_BYTES,
            "the best-effort worst case measured {bytes} bytes"
        );
        // ... and within the wire frame limit, so the client the CLI makes
        // can read the answer whole.
        assert!(
            u32::try_from(bytes)
                .is_ok_and(|b| b as u64 <= slashit_ipc::protocol::MAX_REQUEST_BYTES),
            "the best-effort worst case measured {bytes} bytes"
        );
    }

    /// The aggregate bound fails closed: an answer past
    /// [`MAX_EVIDENCE_RESPONSE_BYTES`] is refused whole, with a reason that
    /// names the bound, and nothing oversized goes out with fields silently
    /// trimmed.
    #[test]
    fn an_answer_past_the_aggregate_bound_is_refused_whole() {
        let oversized = TaskEvidence {
            task_id: "id".into(),
            title: "m".repeat(MAX_EVIDENCE_RESPONSE_BYTES),
            title_truncated: true,
            provider: "OpenCode",
            configured_model: None,
            configured_model_truncated: false,
            latest_observed_model: None,
            latest_observed_model_truncated: false,
            session: None,
            output: OutputEvidence {
                available: false,
                memory_only: true,
                note: OUTPUT_NEVER_RUN_NOTE,
                entries_omitted: 0,
                truncated: false,
                entries: Vec::new(),
            },
            runs: Vec::new(),
            runs_omitted: 0,
            ai_review: None,
            qa_signoff: None,
            failure_reason: None,
            failure_reason_truncated: false,
            activity: Vec::new(),
            activity_omitted: 0,
        };
        let reply = evidence_reply("39bd237c-0000-0000-0000-000000000009", oversized);
        assert!(!reply.ok);
        let error = reply.error.expect("an error");
        assert!(error.contains("response bound"), "{error}");
        assert!(error.contains("was not sent"), "{error}");
    }

    /// The CLI contract: the typed answer the actual handler produces,
    /// serialized the way production serializes it, rendered through the
    /// CLI's own report function. Schema and rendering are validated together,
    /// so neither side can drift from the other: the handler renames a field,
    /// this fails; the report loses a line, this fails.
    #[tokio::test]
    async fn the_cli_report_renders_the_handler_s_typed_answer() {
        let (_tmp, ctx, id) = rich_world().await;
        let executor = attach_test_executor_ipc(&ctx).await;
        executor
            .record_ended_execution_for_test(
                id,
                crate::domain::AgentStatus::Stopped,
                &["Session started: e7e1f7d0 (model: claude-opus-4-0-20250514)"],
            )
            .await;
        let reply = handle_get_task_evidence(&ctx, id.to_string()).await;
        assert!(reply.ok, "{:?}", reply.error);

        // Schema compatibility: every field the CLI report reads is present
        // in the handler's serialized answer, under the exact key the CLI
        // reads.
        for key in [
            "task_id",
            "title",
            "title_truncated",
            "provider",
            "configured_model",
            "latest_observed_model",
            "session",
            "output",
            "runs",
            "runs_omitted",
            "ai_review",
            "qa_signoff",
            "failure_reason",
            "activity",
            "activity_omitted",
        ] {
            assert!(
                reply.data.get(key).is_some(),
                "the handler's answer is missing the key the CLI report reads: {key}"
            );
        }

        let text = slashit::output::run_report_text(&reply.data);
        assert!(text.contains("Evidence test"), "the title: {text}");
        assert!(text.contains("provider OpenCode"), "the provider: {text}");
        assert!(
            text.contains("Model configured: claude-sonnet-5-5"),
            "the configured model, verbatim: {text}"
        );
        // The observed model is named as the Task's latest, not attributed to
        // any run, and never to the configured alias instead.
        assert!(
            text.contains("latest observed"),
            "the Task-level identity: {text}"
        );
        assert!(
            text.contains("claude-opus-4-0-20250514"),
            "the model: {text}"
        );
        assert!(
            text.contains("run 4 · failed · provider unreachable"),
            "the newest run, its status and its reason: {text}"
        );
        assert!(
            text.contains("run 2 · completed") && text.contains("partial answer"),
            "a completed run with its recorded outcome: {text}"
        );
        assert!(text.contains("run 1 · completed"), "an older run: {text}");
        assert!(text.contains("review 3"), "the review ordinal: {text}");
        assert!(
            text.contains("signoff: approved"),
            "the review signoff: {text}"
        );
        assert!(
            text.contains("the last run failed: worker exited"),
            "the Task's failure reason: {text}"
        );
        assert!(
            text.contains("held only in this instance's memory"),
            "the memory-only hold on run output: {text}"
        );
        assert!(
            text.contains("[info] Session started: e7e1f7d0"),
            "an echoed output line: {text}"
        );
        // Nothing was degraded on the way through: no placeholder survived.
        assert!(!text.contains("(untitled)"), "the title rendered: {text}");
    }
}

/// What a run's recorded end events decide for it. Regression tests for the
/// reconstruction rule: the record is scanned newest first, the FIRST valid
/// end event that matches a run settles that run's `status` and `ended_at`,
/// and neither an older end event of a different kind nor any event naming
/// another run can override the decision. The entries are built directly (a
/// record could arrive this way from an older version of the app or a bug),
/// which is exactly the shape the rule must survive.
#[cfg(test)]
mod evidence_run_end_events {
    use super::*;
    use slashit_activity::{Entry, Kind, RunResult};

    fn entry(seq: u32, kind: Kind) -> Entry {
        Entry {
            seq,
            at: chrono::DateTime::from_timestamp(1_700_000_000 + i64::from(seq), 0)
                .expect("an instant"),
            kind,
        }
    }

    fn start(run: u32) -> Kind {
        Kind::RunStarted {
            run,
            addressing_feedback: false,
        }
    }

    fn started(run: u32, entries: &[Entry]) -> Vec<Entry> {
        let mut entries = entries.to_vec();
        entries.insert(0, entry(0, start(run)));
        entries
    }

    fn run_of(entries: &[Entry]) -> RunEvidence {
        evidence_runs(entries)
            .0
            .into_iter()
            .next()
            .expect("a run was named")
    }

    #[test]
    fn a_completion_recorded_after_a_failure_stays_completed() {
        let mut entries = started(
            1,
            &[entry(
                2,
                Kind::RunFailed {
                    run: Some(1),
                    reason: "provider dropped".into(),
                },
            )],
        );
        entries.push(entry(3, Kind::RunCompleted { run: 1 }));

        let run = run_of(&entries);
        assert_eq!(run.status, "completed");
        assert!(
            run.failure_reason.is_none(),
            "a completed run carries no failure"
        );
        assert_eq!(run.started_at, Some(entries[0].at));
        assert_eq!(run.ended_at, Some(entries[2].at), "the latest end's time");
    }

    #[test]
    fn a_failure_recorded_after_a_completion_reads_failed() {
        let mut entries = started(1, &[entry(2, Kind::RunCompleted { run: 1 })]);
        entries.push(entry(
            3,
            Kind::RunFailed {
                run: Some(1),
                reason: "crashed at the end".into(),
            },
        ));

        let run = run_of(&entries);
        assert_eq!(run.status, "failed");
        assert_eq!(run.failure_reason.as_deref(), Some("crashed at the end"));
        assert_eq!(run.ended_at, Some(entries[2].at));
    }

    #[test]
    fn runs_keep_their_own_events_when_they_are_interleaved() {
        let entries = started(
            1,
            &[
                entry(2, start(2)),
                entry(
                    3,
                    Kind::RunFailed {
                        run: Some(2),
                        reason: "run 2 out of memory".into(),
                    },
                ),
                entry(
                    4,
                    Kind::RunResult {
                        run: 1,
                        result: RunResult::Available {
                            text: "run 1's answer".into(),
                        },
                    },
                ),
                entry(5, Kind::RunCompleted { run: 1 }),
            ],
        );

        let (runs, omitted) = evidence_runs(&entries);
        assert_eq!(omitted, 0);
        assert_eq!(runs.len(), 2);
        // Newest run first, each with only its own history: run 2's failure
        // never touches run 1, and run 1's completion never rewrites run 2.
        assert_eq!(runs[0].run, 2);
        assert_eq!(runs[0].status, "failed");
        assert_eq!(
            runs[0].failure_reason.as_deref(),
            Some("run 2 out of memory")
        );
        assert!(runs[0].outcome.is_none());
        assert_eq!(runs[1].run, 1);
        assert_eq!(runs[1].status, "completed");
        assert!(runs[1].failure_reason.is_none());
        assert_eq!(
            runs[1].outcome.as_ref().and_then(|o| o.text.as_deref()),
            Some("run 1's answer"),
        );
    }

    #[test]
    fn a_run_the_record_never_ended_reads_started() {
        let entries = started(
            1,
            &[entry(
                1,
                Kind::ToolUsed {
                    run: 1,
                    tool: "Bash".into(),
                    detail: None,
                    count: 1,
                },
            )],
        );

        let run = run_of(&entries);
        assert_eq!(run.status, "started");
        assert!(run.failure_reason.is_none());
        assert!(run.ended_at.is_none());
        assert_eq!(run.started_at, Some(entries[0].at));
    }

    #[test]
    fn a_recovery_is_earlier_history_of_a_later_completion() {
        let mut entries = started(
            1,
            &[
                entry(
                    2,
                    Kind::RecoveryRequired {
                        run: Some(1),
                        reason: "the run ended uncleanly".into(),
                    },
                ),
                entry(3, Kind::NewAttemptAuthorized { run: 1 }),
            ],
        );
        entries.push(entry(4, Kind::RunCompleted { run: 1 }));

        let run = run_of(&entries);
        assert_eq!(run.status, "completed");
        assert!(
            run.failure_reason.is_none(),
            "the deciding end is the completion"
        );
        assert_eq!(run.ended_at, Some(entries[3].at));
    }
}

/// What a review's evidence says when a run follows it. The latest persisted
/// review is returned even though a newer `RunStarted` exists on the record —
/// with that run named as `superseded_by_run`, never attributed to it — and
/// `None` only when no review record exists at all. The entries are built
/// directly, as a record could arrive from an older version of the app.
#[cfg(test)]
mod evidence_ai_review_supersession {
    use super::*;
    use slashit_activity::{Entry, Kind};

    fn entry(seq: u32, kind: Kind) -> Entry {
        Entry {
            seq,
            at: chrono::DateTime::from_timestamp(1_700_000_000 + i64::from(seq), 0)
                .expect("an instant"),
            kind,
        }
    }

    fn review_of(entries: &[Entry]) -> AiReviewEvidence {
        evidence_ai_review(entries).expect("a review was on the record")
    }

    /// A completed review followed by a run that has no review yet: the old
    /// verdict is reported, saying which run it predates.
    #[test]
    fn a_completed_review_followed_by_a_run_is_superseded_by_that_run() {
        let entries = [
            entry(
                1,
                Kind::RunStarted {
                    run: 1,
                    addressing_feedback: false,
                },
            ),
            entry(2, Kind::RunCompleted { run: 1 }),
            entry(3, Kind::AiReviewStarted { review: 1 }),
            entry(4, Kind::AiReviewApproved { review: 1 }),
            entry(
                5,
                Kind::RunStarted {
                    run: 2,
                    addressing_feedback: false,
                },
            ),
        ];

        let review = review_of(&entries);
        assert_eq!(review.review, 1);
        assert_eq!(review.outcome, "approved");
        assert_eq!(review.superseded_by_run, Some(2));
    }

    /// A review that asked for changes and a fix attempt after it, then a
    /// run: the verdict the fix answered stays the reported outcome, marked
    /// superseded by the later run — not silently dropped, and not passed off
    /// as anything about that run.
    #[test]
    fn a_fix_attempt_followed_by_a_run_is_superseded_by_that_run() {
        let entries = [
            entry(1, Kind::AiReviewStarted { review: 1 }),
            entry(
                2,
                Kind::AiReviewChangesRequested {
                    review: 1,
                    issues: 2,
                },
            ),
            entry(3, Kind::AiFixStarted { review: 1 }),
            entry(
                4,
                Kind::AiFixFailed {
                    review: 1,
                    reason: "the fix could not deliver".into(),
                },
            ),
            entry(
                5,
                Kind::RunStarted {
                    run: 2,
                    addressing_feedback: true,
                },
            ),
        ];

        let review = review_of(&entries);
        assert_eq!(review.review, 1);
        assert_eq!(review.outcome, "fix_failed");
        assert_eq!(review.superseded_by_run, Some(2));
    }

    /// A run that began while the latest review was still being asked for
    /// marks that review superseded too: the review started after the run, and
    /// it is still the latest decision on record.
    #[test]
    fn an_unverdicted_review_after_a_run_is_superseded_the_same_way() {
        let entries = [
            entry(
                1,
                Kind::RunStarted {
                    run: 1,
                    addressing_feedback: false,
                },
            ),
            entry(2, Kind::RunCompleted { run: 1 }),
            entry(3, Kind::AiReviewStarted { review: 1 }),
            entry(
                4,
                Kind::RunStarted {
                    run: 2,
                    addressing_feedback: false,
                },
            ),
        ];

        let review = review_of(&entries);
        assert_eq!(review.review, 1);
        assert_eq!(review.outcome, "awaiting_verdict");
        assert_eq!(review.superseded_by_run, Some(2));
    }

    /// Run starts from before the review cycle — older entries the scan never
    /// reaches, because the review event settles first — name no successor.
    #[test]
    fn a_review_nothing_started_after_is_not_superseded() {
        let entries = [
            entry(
                1,
                Kind::RunStarted {
                    run: 1,
                    addressing_feedback: false,
                },
            ),
            entry(2, Kind::RunCompleted { run: 1 }),
            entry(
                3,
                Kind::RunStarted {
                    run: 2,
                    addressing_feedback: false,
                },
            ),
            entry(
                4,
                Kind::RunStarted {
                    run: 3,
                    addressing_feedback: false,
                },
            ),
            entry(5, Kind::AiReviewStarted { review: 2 }),
            entry(6, Kind::AiReviewApproved { review: 2 }),
        ];

        let review = review_of(&entries);
        assert_eq!(review.review, 2);
        assert_eq!(review.outcome, "approved");
        assert_eq!(review.superseded_by_run, None);
    }

    /// Other run events — a result, a failure, a recovery — are not run
    /// starts, and do not mark a review superseded on their own.
    #[test]
    fn run_events_that_name_no_new_start_supersede_nothing() {
        let entries = [
            entry(1, Kind::AiReviewStarted { review: 1 }),
            entry(2, Kind::AiReviewApproved { review: 1 }),
            entry(
                3,
                Kind::RunResult {
                    run: 1,
                    result: slashit_activity::RunResult::Available {
                        text: "the answer".into(),
                    },
                },
            ),
            entry(
                4,
                Kind::RunFailed {
                    run: Some(1),
                    reason: "in the ledger late".into(),
                },
            ),
            entry(5, Kind::NewAttemptAuthorized { run: 1 }),
        ];

        let review = review_of(&entries);
        assert_eq!(review.review, 1);
        assert_eq!(review.outcome, "approved");
        assert_eq!(review.superseded_by_run, None);
    }

    /// Runs without any review on the record answer no review at all — this
    /// is the shape `None` is reserved for.
    #[test]
    fn a_record_with_runs_and_no_review_answers_no_review() {
        let entries = [
            entry(
                1,
                Kind::RunStarted {
                    run: 1,
                    addressing_feedback: false,
                },
            ),
            entry(2, Kind::RunCompleted { run: 1 }),
            entry(
                3,
                Kind::RunStarted {
                    run: 2,
                    addressing_feedback: false,
                },
            ),
        ];

        assert!(
            evidence_ai_review(&entries).is_none(),
            "there is no review to report, not a lost one"
        );
    }

    /// Of two run starts after the review, the newest one is what the review
    /// predates.
    #[test]
    fn the_newest_starting_run_is_the_review_s_successor() {
        let entries = [
            entry(1, Kind::AiReviewStarted { review: 1 }),
            entry(2, Kind::AiReviewApproved { review: 1 }),
            entry(
                3,
                Kind::RunStarted {
                    run: 2,
                    addressing_feedback: false,
                },
            ),
            entry(
                4,
                Kind::RunStarted {
                    run: 3,
                    addressing_feedback: false,
                },
            ),
        ];

        let review = review_of(&entries);
        assert_eq!(review.superseded_by_run, Some(3));
    }
}

/// Every echoed field obeys the same redaction and bound. Regression tests
/// with representative secrets: a credential-shaped value that reaches any
/// echo — through the Task's error message, a failed session state, an
/// output line, an AI-review reason or a run result — goes out masked, and
/// every variable-length field is cut with its flag set.
///
/// The redaction is applied at read time as well: the later tests in this
/// module push raw Activity entries past `record_activity_at`'s write-time
/// sanitization, the shape a record written by a version of the app that
/// lagged behind the current policy can arrive in, and require the echo to
/// mask what the stored record kept intact.
#[cfg(test)]
mod evidence_bounds_and_redaction {
    use super::*;
    use crate::test_helpers::{attach_test_executor_ipc, create_test_task_full, ipc_test_context};
    use slashit_activity::{Kind, RunResult};
    use std::sync::Arc;

    /// One call: a Github-shaped token versus which nothing in the answer may
    /// survive whole.
    const SECRET: &str = "ghp_theTokenToFind0123456789";

    async fn read_evidence(ctx: &IpcContext, id: &str) -> serde_json::Value {
        let reply = dispatch(
            IpcRequest::GetTaskEvidence {
                task_id: id.to_string(),
            },
            ctx,
            &PeerContext {
                endpoint: slashit_ipc::Endpoint::Unix {
                    path: std::path::PathBuf::from("/nonexistent-for-tests"),
                },
                os_verified: true,
            },
        )
        .await
        .response;
        assert!(reply.ok, "{:?}", reply.error);
        reply.data
    }

    async fn echo(task: crate::domain::Task) -> serde_json::Value {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        let id = task.id;
        ctx.tasks.write().await.insert(id, task);
        let d = read_evidence(&ctx, &id.to_string()).await;
        // The tempdir is dropped here; the answer was read before that.
        drop(tmp);
        d
    }

    fn task_with(title: &str, error_message: Option<String>) -> crate::domain::Task {
        let mut task = create_test_task_full(title, Uuid::new_v4(), TaskStatus::Error, 0);
        task.error_message = error_message;
        task
    }

    fn field_leaks(d: &serde_json::Value, secret: &str) {
        assert!(
            !serde_json::to_string(d).unwrap().contains(secret),
            "the secret reached the response unmasked: {d}"
        );
        assert!(
            serde_json::to_string(d)
                .unwrap()
                .contains(slashit_activity::MASK),
            "no mask anywhere: the redaction was not exercised: {d}"
        );
    }

    #[tokio::test]
    async fn an_oversized_error_message_is_redacted_cut_and_flagged() {
        let reason = format!(
            "the provider refused the run; token {SECRET}; {}",
            "x".repeat(1_200),
        );
        let d = echo(task_with("failed task", Some(reason))).await;

        field_leaks(&d, SECRET);
        assert_eq!(d["failure_reason_truncated"], true);
        let echoed = d["failure_reason"].as_str().expect("reason");
        assert_eq!(echoed.chars().count(), MAX_EVIDENCE_FAILURE_CHARS);
    }

    #[tokio::test]
    async fn a_failed_session_state_names_a_redacted_reason() {
        let task = task_with("failed in this session", None);
        let id = task.id;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        ctx.tasks.write().await.insert(id, task);
        let executor = attach_test_executor_ipc(&ctx).await;
        executor
            .record_ended_execution_for_test(
                id,
                crate::domain::AgentStatus::Failed(format!(
                    "the provider replied 401 to token {SECRET}"
                )),
                &[],
            )
            .await;

        let d = read_evidence(&ctx, &id.to_string()).await;
        field_leaks(&d, SECRET);
        let status = d["session"]["status"].as_object().expect("a failed state");
        assert!(status.contains_key("failed"), "{status:?}");
        let reason = status["failed"].as_str().expect("the failure text");
        assert!(reason.chars().count() <= MAX_EVIDENCE_FAILURE_CHARS);
    }

    #[tokio::test]
    async fn an_oversized_output_line_is_redacted_cut_and_flagged() {
        let task = task_with("chatty failure", None);
        let id = task.id;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let paths = Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let ctx = ipc_test_context(paths);
        ctx.tasks.write().await.insert(id, task);
        let executor = attach_test_executor_ipc(&ctx).await;
        let line = format!(
            "the run printed token {SECRET} and then {}",
            "y".repeat(900)
        );
        executor
            .record_ended_execution_for_test(id, crate::domain::AgentStatus::Stopped, &[&line])
            .await;

        let d = read_evidence(&ctx, &id.to_string()).await;
        field_leaks(&d, SECRET);
        let entry = &d["output"]["entries"][0];
        assert_eq!(entry["message_truncated"], true);
        let message = entry["message"].as_str().expect("message");
        assert!(message.chars().count() <= MAX_EVIDENCE_MESSAGE_CHARS);
        assert_eq!(d["output"]["truncated"], true);
    }

    #[tokio::test]
    async fn a_recorded_run_failure_named_a_secret_is_masked() {
        let mut task = task_with("failed loudly", None);
        let at = chrono::Utc::now();
        task.record_activity_at(
            at,
            Kind::RunStarted {
                run: 1,
                addressing_feedback: false,
            },
        );
        task.record_activity_at(
            at,
            Kind::RunFailed {
                run: Some(1),
                reason: format!("the run's own last line was token {SECRET}"),
            },
        );

        let d = echo(task).await;
        field_leaks(&d, SECRET);
        let reason = d["runs"][0]["failure_reason"].as_str().expect("reason");
        assert!(reason.chars().count() <= slashit_activity::MAX_REASON_CHARS);
    }

    #[tokio::test]
    async fn an_ai_review_failure_reason_is_redacted_and_bounded() {
        let mut task = task_with("review failed", None);
        task.record_activity_at(
            chrono::Utc::now(),
            Kind::AiReviewFailed {
                review: 1,
                reason: format!(
                    "the reviewer could not read its credentials, {SECRET}; {}",
                    "z".repeat(2_000)
                ),
            },
        );

        let d = echo(task).await;
        field_leaks(&d, SECRET);
        let reason = d["ai_review"]["reason"].as_str().expect("reason");
        assert!(
            reason.chars().count() <= slashit_activity::MAX_REASON_CHARS,
            "{reason}"
        );
    }

    #[tokio::test]
    async fn a_run_result_keeps_a_secret_masked_and_within_the_echo_bound() {
        let mut task = task_with("big result", None);
        let at = chrono::Utc::now();
        task.record_activity_at(
            at,
            Kind::RunStarted {
                run: 1,
                addressing_feedback: false,
            },
        );
        task.record_activity_at(
            at,
            Kind::RunResult {
                run: 1,
                result: RunResult::Available {
                    text: format!("the patch holds {SECRET} and {}", "w".repeat(3_000)),
                },
            },
        );

        let d = echo(task).await;
        field_leaks(&d, SECRET);
        let outcome = &d["runs"][0]["outcome"];
        // The record stored the whole (redacted) answer within its own bound,
        // so the echo's tighter bound is what cut it, and the flag says so.
        assert_eq!(
            outcome["text"].as_str().unwrap().chars().count(),
            MAX_EVIDENCE_RESULT_CHARS,
        );
        assert!(outcome["echo_chars_omitted"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn the_title_and_the_model_identifiers_are_bounded_and_flagged() {
        let mut task = task_with(&format!("t{}", "m".repeat(2_000)), None);
        task.model = "n".repeat(500);
        task.last_observed_model = Some("o".repeat(500));

        let d = echo(task).await;
        assert_eq!(d["title_truncated"], true);
        assert_eq!(
            d["title"].as_str().unwrap().chars().count(),
            MAX_EVIDENCE_TITLE_CHARS
        );
        assert_eq!(d["configured_model_truncated"], true);
        assert_eq!(
            d["configured_model"].as_str().unwrap().chars().count(),
            MAX_EVIDENCE_MODEL_CHARS,
        );
        assert_eq!(d["latest_observed_model_truncated"], true);
        assert_eq!(
            d["latest_observed_model"].as_str().unwrap().chars().count(),
            MAX_EVIDENCE_MODEL_CHARS,
        );
    }

    #[tokio::test]
    async fn a_untouched_title_and_models_carry_no_flag() {
        let mut task = task_with("clean", None);
        task.model = "claude-sonnet-5-5".to_string();
        task.last_observed_model = Some("a-provider/model-name".to_string());

        let d = echo(task).await;
        assert_eq!(d["title_truncated"], false);
        assert_eq!(d["configured_model_truncated"], false);
        assert_eq!(d["latest_observed_model_truncated"], false);
        assert_eq!(d["configured_model"], "claude-sonnet-5-5");
    }

    // --- Historical records, read-time redaction ------------------------------
    //
    // Each test below pushes raw entries straight onto `task.activity`,
    // bypassing the write-time sanitization `record_activity_at` applies, so
    // the stored record is the shape an older build could have written: a
    // credential intact, in a run failure reason, a run result, an
    // `Unavailable` reason, a tool detail or an AI-review failure reason.
    // Nothing in the answer — the IPC response or the human-readable CLI
    // report — may repeat it.

    /// A raw entry, pushed past `record_activity_at`'s write-time
    /// sanitization: the shape a record written before the current policy
    /// existed can arrive in. `seq` keeps the pushed position intact.
    fn paste(task: &mut crate::domain::Task, seq: u32, kind: Kind) {
        task.activity.push(slashit_activity::Entry {
            seq,
            at: chrono::DateTime::from_timestamp(1_700_000_000 + i64::from(seq), 0)
                .expect("an instant"),
            kind,
        });
    }

    /// The same guarantee for the human-readable report: whatever the answer
    /// echoes, the CLI report built from it must stay clean too.
    fn report_leaks(d: &serde_json::Value, secret: &str) {
        let text = slashit::output::run_report_text(d);
        assert!(
            !text.contains(secret),
            "the secret reached the rendered report unmasked: {text}"
        );
    }

    #[tokio::test]
    async fn a_historical_run_failure_reason_stored_raw_is_masked_at_read_time() {
        let mut task = task_with("failed long ago", None);
        paste(
            &mut task,
            1,
            Kind::RunStarted {
                run: 1,
                addressing_feedback: false,
            },
        );
        paste(
            &mut task,
            2,
            Kind::RunFailed {
                run: Some(1),
                reason: format!("the run's last line carried token {SECRET}"),
            },
        );

        let d = echo(task).await;
        field_leaks(&d, SECRET);
        report_leaks(&d, SECRET);
        let reason = d["runs"][0]["failure_reason"].as_str().expect("reason");
        assert!(
            reason.contains(slashit_activity::MASK),
            "the credential is masked on the echo: {reason}"
        );
    }

    #[tokio::test]
    async fn a_historical_run_result_stored_raw_is_masked_at_read_time() {
        let mut task = task_with("resulted long ago", None);
        paste(
            &mut task,
            1,
            Kind::RunStarted {
                run: 1,
                addressing_feedback: false,
            },
        );
        paste(
            &mut task,
            2,
            Kind::RunResult {
                run: 1,
                result: RunResult::Available {
                    text: format!("the patch includes {SECRET}, honestly"),
                },
            },
        );

        let d = echo(task).await;
        field_leaks(&d, SECRET);
        report_leaks(&d, SECRET);
        let text = d["runs"][0]["outcome"]["text"].as_str().expect("text");
        assert!(
            text.contains(slashit_activity::MASK),
            "the credential is masked on the echo: {text}"
        );
    }

    #[tokio::test]
    async fn a_historical_unavailable_reason_stored_raw_is_masked_at_read_time() {
        let mut task = task_with("no result long ago", None);
        paste(
            &mut task,
            1,
            Kind::RunStarted {
                run: 1,
                addressing_feedback: false,
            },
        );
        paste(
            &mut task,
            2,
            Kind::RunResult {
                run: 1,
                result: RunResult::Unavailable {
                    reason: format!("the provider was reached with {SECRET} and said nothing"),
                },
            },
        );

        let d = echo(task).await;
        field_leaks(&d, SECRET);
        report_leaks(&d, SECRET);
        let reason = d["runs"][0]["outcome"]["reason"].as_str().expect("reason");
        assert!(
            reason.contains(slashit_activity::MASK),
            "the credential is masked on the echo: {reason}"
        );
    }

    #[tokio::test]
    async fn a_historical_tool_detail_stored_raw_is_masked_in_the_activity_echo() {
        let mut task = task_with("tooled long ago", None);
        paste(
            &mut task,
            1,
            Kind::RunStarted {
                run: 1,
                addressing_feedback: false,
            },
        );
        paste(
            &mut task,
            2,
            Kind::ToolUsed {
                run: 1,
                tool: "Bash".into(),
                detail: Some(format!("export API_KEY={SECRET} && ./deploy.sh")),
                count: 1,
            },
        );

        let d = echo(task).await;
        field_leaks(&d, SECRET);
        report_leaks(&d, SECRET);
        let activity = d["activity"].as_array().expect("activity");
        let tool = activity
            .iter()
            .find(|entry| entry["kind"]["type"] == "tool_used")
            .expect("the tool milestone is echoed");
        let detail = tool["kind"]["detail"].as_str().expect("detail");
        assert!(
            detail.contains(slashit_activity::MASK),
            "the credential is masked on the echo: {detail}"
        );
    }

    #[tokio::test]
    async fn a_historical_ai_review_failure_reason_stored_raw_is_masked_at_read_time() {
        let mut task = task_with("reviewed long ago", None);
        paste(
            &mut task,
            1,
            Kind::AiReviewFailed {
                review: 1,
                reason: format!("the reviewer could not read {SECRET}"),
            },
        );

        let d = echo(task).await;
        field_leaks(&d, SECRET);
        report_leaks(&d, SECRET);
        let reason = d["ai_review"]["reason"].as_str().expect("reason");
        assert!(
            reason.contains(slashit_activity::MASK),
            "the credential is masked on the echo: {reason}"
        );
    }
}
