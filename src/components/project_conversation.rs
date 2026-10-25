use crate::components::attention::AttentionChip;
use crate::components::task_activity::{compact_time, local_offset_at, OffsetAt};
use crate::models::{ConversationActionStatus, ConversationEntryKind, ConversationExecution, ConversationExecutionResult, ConversationExecutionState, ConversationFieldChange, ConversationHumanAction, ConversationProjectAction, ConversationProjectActionOutcome, ConversationProjectActionStatus, ConversationRole, ConversationSnapshot, ConversationTaskMutation};
use crate::services::conversation_service;
use chrono::{DateTime, Utc};
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::HashMap;
use uuid::Uuid;

/// Execution views refresh more slowly than the Coordinator and Worker
/// liveness poll, and only while a run can still change.
const EXECUTION_REFRESH_MS: u32 = 3_000;

/// Refreshes of an unchanged `Unconfirmed` run before the refresh stops, about
/// five minutes. Nothing settles such a run on its own, so polling it forever
/// would only repeat the same answer.
const UNCONFIRMED_STABLE_REFRESHES: u32 = 100;

fn should_poll_snapshot(busy: bool, coordinator_live: bool, worker_live: bool) -> bool {
    busy || coordinator_live || worker_live
}

/// How far above the end of the history the reader may be and still follow new
/// entries. The same slack the task drawer's output uses.
const FOLLOW_LATEST_SLACK_PX: i32 = 24;

/// Whether the history is scrolled to its newest entry, close enough to keep
/// following it as new entries arrive.
fn is_at_latest(scroll_top: i32, scroll_height: i32, client_height: i32) -> bool {
    scroll_top + client_height >= scroll_height - FOLLOW_LATEST_SLACK_PX
}

/// When an entry was saved, in the form the Task timeline uses. Nothing is shown
/// for a timestamp that does not parse, rather than a wrong one.
fn entry_time(created_at: &str, now: DateTime<Utc>, offset_at: OffsetAt<'_>) -> Option<String> {
    let at = DateTime::parse_from_rfc3339(created_at).ok()?.with_timezone(&Utc);
    Some(compact_time(at, offset_at, now))
}

fn has_unmediated_worker_result(snapshot: &ConversationSnapshot) -> bool {
    snapshot.conversation.actions.iter().any(|action| action.status == ConversationActionStatus::Returned && !action.coordinator_replied)
}

/// What a card for a retired direct delegation may offer. Direct delegation is
/// retired, so no card approves or starts a Worker. A proposal can be rejected;
/// an earlier approval can only be withdrawn, which changes nothing in the Task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetiredDelegationControls {
    note: &'static str,
    can_reject: bool,
    can_withdraw: bool,
}

/// `None` for every status that has no decision left to make on this card.
fn retired_delegation_controls(status: ConversationActionStatus) -> Option<RetiredDelegationControls> {
    match status {
        ConversationActionStatus::Proposed => Some(RetiredDelegationControls {
            note: "Direct delegation to a Task Worker is retired. Nothing will start from this proposal. Reject it to clear it.",
            can_reject: true,
            can_withdraw: false,
        }),
        ConversationActionStatus::Approved => Some(RetiredDelegationControls {
            note: "You approved this before direct delegation was retired. SlashIt will not start it. Withdraw the approval to clear it; the Task is not changed.",
            can_reject: false,
            can_withdraw: true,
        }),
        _ => None,
    }
}

/// What a Project action card shows the Human: exactly the fields that will be
/// created or changed, as labelled rows rather than raw JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MutationView {
    heading: &'static str,
    title: String,
    rows: Vec<(String, String)>,
}

fn words(value: &str) -> String {
    value.replace('_', " ")
}

/// The model policy an approval binds to. An explicit identifier is shown as
/// configured. An empty or `"default"` model means the provider chooses at
/// execution time, so the card does not name a concrete model it cannot know.
fn model_row(model: Option<&str>) -> (String, String) {
    let value = match model {
        None => "Not recorded for this proposal".to_string(),
        Some("" | "default") => "Provider default — selected at execution time; the concrete model may change.".to_string(),
        Some(model) => model.to_string(),
    };
    ("Model".to_string(), value)
}

fn mutation_view(mutation: &ConversationTaskMutation) -> MutationView {
    match mutation {
        ConversationTaskMutation::CreateTask { title, description, priority, category, .. } => {
            let mut rows = vec![
                ("Status".to_string(), "Backlog".to_string()),
                ("Priority".to_string(), words(priority)),
                ("Category".to_string(), words(category)),
            ];
            if let Some(description) = description {
                rows.push(("Description".to_string(), description.clone()));
            }
            MutationView { heading: "Create Task", title: title.clone(), rows }
        }
        ConversationTaskMutation::EditTask { target_task_id, target_task_title, changes } => {
            let mut rows = vec![("Task ID".to_string(), target_task_id.to_string())];
            rows.extend(changes.iter().map(|change| match change {
                ConversationFieldChange::Title { from, to } => ("Title".to_string(), format!("{from} → {to}")),
                ConversationFieldChange::Description { from, to } => ("Description".to_string(), format!("{} → {to}", from.as_deref().unwrap_or("(none)"))),
                ConversationFieldChange::Priority { from, to } => ("Priority".to_string(), format!("{} → {}", words(from), words(to))),
                ConversationFieldChange::Category { from, to } => ("Category".to_string(), format!("{} → {}", words(from), words(to))),
            }));
            MutationView { heading: "Edit Task", title: target_task_title.clone(), rows }
        }
        ConversationTaskMutation::EnqueueTask { target_task_id, target_task_title, from, input } => {
            let mut rows = vec![
                ("Task ID".to_string(), target_task_id.to_string()),
                ("Status".to_string(), format!("{} → queue", words(from))),
            ];
            if let Some(input) = input {
                rows.push(("Will run".to_string(), input.description.clone().unwrap_or_else(|| "(no description)".to_string())));
                if input.subtask_count > 0 {
                    rows.push(("Subtasks".to_string(), input.subtask_count.to_string()));
                }
                rows.push(("Input".to_string(), input.digest.chars().take(12).collect()));
            }
            rows.push(("Effect".to_string(), "Only queues the Task, and only if its definition is unchanged at the moment you approve. SlashIt's scheduler starts it when capacity allows, and the Task prepares its own checkout then".to_string()));
            MutationView { heading: "Queue Task", title: target_task_title.clone(), rows }
        }
        ConversationTaskMutation::ExecuteRequest { target_task_id, target_task_title, from, request, input, checkout } => {
            let mut rows = vec![
                ("Task ID".to_string(), target_task_id.to_string()),
                ("Status".to_string(), format!("{} → queue", words(from))),
                ("Request".to_string(), request.clone()),
                model_row(input.model.as_deref()),
                ("Input".to_string(), input.digest.chars().take(12).collect()),
            ];
            if let Some(checkout) = checkout {
                rows.push(("Checkout".to_string(), checkout.checkout_path.clone()));
                rows.push(("Branch".to_string(), checkout.branch.clone()));
                rows.push(("Head".to_string(), checkout.head.clone()));
                rows.push(("Base".to_string(), checkout.base_commit.clone()));
            }
            rows.push(("Effect".to_string(), if checkout.is_some() {
                "Queues one run in this Task's existing checkout with exactly this request, and only if the checkout and branch are unchanged at the moment you approve. Earlier commits are kept and stay in the Task diff. The Task description is not changed.".to_string()
            } else {
                "Queues one run with exactly this request, and only if the Task is unchanged at the moment you approve. The Task description is not changed. SlashIt's scheduler starts it when capacity allows, and the Task prepares its own checkout then".to_string()
            }));
            MutationView { heading: "Queue Task with a request", title: target_task_title.clone(), rows }
        }
        ConversationTaskMutation::MoveTask { target_task_id, target_task_title, from, to } => MutationView {
            heading: "Move Task",
            title: target_task_title.clone(),
            rows: vec![
                ("Task ID".to_string(), target_task_id.to_string()),
                ("Status".to_string(), format!("{} → {}", words(from), words(to))),
                ("Effect".to_string(), "Reorganizes the board only; no work is started".to_string()),
            ],
        },
    }
}

/// Only an undecided or approved-but-unfinished action offers controls. A
/// finished one offers none, so a stale click cannot run it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProjectActionControls {
    approve_label: Option<&'static str>,
    can_reject: bool,
}

fn project_action_controls(status: ConversationProjectActionStatus) -> ProjectActionControls {
    match status {
        ConversationProjectActionStatus::Proposed => ProjectActionControls { approve_label: Some("Approve"), can_reject: true },
        ConversationProjectActionStatus::Approved => ProjectActionControls { approve_label: Some("Retry approved action"), can_reject: false },
        ConversationProjectActionStatus::Applied | ConversationProjectActionStatus::Rejected | ConversationProjectActionStatus::Refused => ProjectActionControls { approve_label: None, can_reject: false },
    }
}

/// Shown for a history event whose record is absent from the snapshot. Dropping
/// the event would leave a gap nobody can explain.
const MISSING_RECORD_TEXT: &str = "The record for this event is not in this Conversation, so its details cannot be shown.";

fn project_action_event_text(kind: &ConversationEntryKind, actions: &[ConversationProjectAction]) -> Option<String> {
    let find = |id: &Uuid| actions.iter().find(|action| action.id == *id);
    match kind {
        ConversationEntryKind::ProjectActionProposed { action_id } => find(action_id).map(|action| {
            let view = mutation_view(&action.mutation);
            format!("Proposed: {} \"{}\"", view.heading, view.title)
        }),
        ConversationEntryKind::ProjectActionDecision { approved, .. } => Some(if *approved { "You approved the proposal.".to_string() } else { "You rejected the proposal.".to_string() }),
        ConversationEntryKind::ProjectActionApplied { action_id, task_id } => find(action_id).map(|action| match &action.mutation {
            ConversationTaskMutation::CreateTask { title, .. } => format!("Task created in Backlog: {title} (Task {task_id})"),
            ConversationTaskMutation::EditTask { target_task_title, .. } => format!("Task updated: {target_task_title} (Task {task_id})"),
            ConversationTaskMutation::MoveTask { target_task_title, to, .. } => format!("Task moved to {}: {target_task_title} (Task {task_id})", words(to)),
            ConversationTaskMutation::ExecuteRequest { target_task_title, .. } => format!("Task queued with an additional request: {target_task_title} (Task {task_id})"),
            ConversationTaskMutation::EnqueueTask { target_task_title, .. } => match action.outcome {
                Some(ConversationProjectActionOutcome::AlreadyQueued { .. }) => format!("Task was already in the queue: {target_task_title} (Task {task_id}); SlashIt cannot tell whether this approval queued it"),
                _ => format!("Task queued: {target_task_title} (Task {task_id})"),
            },
        }),
        ConversationEntryKind::ProjectActionRefused { reason, .. } => Some(format!("Nothing was changed: {reason}")),
        _ => None,
    }
}

fn pending_project_actions(snapshot: &ConversationSnapshot) -> Vec<ConversationProjectAction> {
    snapshot.conversation.project_actions.iter()
        .filter(|action| project_action_controls(action.status).approve_label.is_some())
        .cloned().collect()
}

fn should_publish_snapshot(current: Option<(Uuid, u64)>, incoming: (Uuid, u64), request: u64, applied_request: u64) -> bool {
    match current {
        None => true,
        Some((current_id, _)) if current_id != incoming.0 => request > applied_request,
        Some((_, revision)) if incoming.1 > revision => true,
        Some((_, revision)) if incoming.1 == revision => request > applied_request,
        Some(_) => false,
    }
}

/// Whether the execution list still holds a run that can change without a
/// Human decision. Queued and running runs keep refreshing; an unconfirmed run
/// stops once it has been unchanged for a long time.
fn should_refresh_executions(executions: &[ConversationExecution], stable_refreshes: u32) -> bool {
    executions.iter().any(|execution| match execution.state {
        ConversationExecutionState::Unconfirmed => stable_refreshes < UNCONFIRMED_STABLE_REFRESHES,
        _ => execution.transitional,
    })
}

/// What a Human reads for each execution state. The wording never calls a
/// completed coding run delivered, and never says a run is alive from a
/// persisted status alone.
fn execution_state_text(state: ConversationExecutionState) -> &'static str {
    match state {
        ConversationExecutionState::PendingApproval => "Approved. No record shows the approval taking effect yet.",
        ConversationExecutionState::Queued => "Queued by this approval. SlashIt's scheduler decides when the run starts.",
        ConversationExecutionState::Running => "Running now in the Task Checkout.",
        ConversationExecutionState::Unconfirmed => "Started, but its outcome is not confirmed yet. It is not counted as completed or failed.",
        ConversationExecutionState::Completed => "The coding run completed. The Task may still be in review or delivery.",
        ConversationExecutionState::Failed => "The run failed.",
        ConversationExecutionState::RecoveryRequired => "The run's outcome needs your inspection before anything else runs.",
        ConversationExecutionState::Ambiguous => "SlashIt's records do not identify one run for this approval, so none is linked.",
        ConversationExecutionState::NoExecutionEvidence => "No run is recorded for this approval.",
        ConversationExecutionState::TaskUnavailable => "The Task is missing or no longer belongs to this Project.",
    }
}

/// The stable `data-state` value for an execution card, for tests and acceptance.
fn execution_state_key(state: ConversationExecutionState) -> &'static str {
    match state {
        ConversationExecutionState::PendingApproval => "pending_approval",
        ConversationExecutionState::Queued => "queued",
        ConversationExecutionState::Running => "running",
        ConversationExecutionState::Unconfirmed => "unconfirmed",
        ConversationExecutionState::Completed => "completed",
        ConversationExecutionState::Failed => "failed",
        ConversationExecutionState::RecoveryRequired => "recovery_required",
        ConversationExecutionState::Ambiguous => "ambiguous",
        ConversationExecutionState::NoExecutionEvidence => "no_execution_evidence",
        ConversationExecutionState::TaskUnavailable => "task_unavailable",
    }
}

fn execution_run_text(run: Option<u32>) -> String {
    match run {
        Some(run) => format!("Run {run}"),
        None => "Run not established yet".to_string(),
    }
}

/// The approval that started the run, named as the Human approved it.
fn approval_text(action: Option<&ConversationProjectAction>) -> &'static str {
    match action.map(|action| &action.mutation) {
        Some(ConversationTaskMutation::ExecuteRequest { .. }) => "Approved additional request",
        _ => "Approved queue request",
    }
}

/// What approving does, as the proposal card states it. A queue request queues a
/// run that SlashIt's scheduler starts, so it must not say that no work starts.
fn approval_effect_note(mutation: &ConversationTaskMutation) -> &'static str {
    match mutation {
        ConversationTaskMutation::EnqueueTask { .. } | ConversationTaskMutation::ExecuteRequest { .. } => "Approving queues this Task. SlashIt's scheduler decides when its run starts.",
        _ => "Approving only changes this Task's record. It does not start any work.",
    }
}

/// One Task run started by an approved action, shown beside that action's
/// event in the history. Its text is rendered as plain text only.
fn execution_card(execution: ConversationExecution, approval: &'static str) -> impl IntoView {
    let state = execution_state_key(execution.state);
    let result = match execution.result {
        ConversationExecutionResult::Available { text } | ConversationExecutionResult::Truncated { text } => {
            let note = text.truncated.then(|| format!("Showing the first {} of {} characters.", text.text.chars().count(), text.original_chars));
            view! {
                <div data-testid="conversation-execution-result" class="whitespace-pre-wrap text-sm text-white/90">{text.text}</div>
                {note.map(|note| view! { <p data-testid="conversation-execution-result-truncated" class="text-xs text-white/50">{note}</p> })}
            }.into_any()
        }
        ConversationExecutionResult::Unavailable { reason } => view! {
            <p data-testid="conversation-execution-result-unavailable" class="text-sm text-amber-100/80">{format!("The provider gave no final answer: {}", reason.text)}</p>
        }.into_any(),
        ConversationExecutionResult::NotRecorded => view! {
            <p data-testid="conversation-execution-result-missing" class="text-sm text-white/50">"No result was recorded for this run."</p>
        }.into_any(),
        ConversationExecutionResult::NotApplicable => ().into_any(),
    };
    let attention = execution.needs_human.then(|| match execution.attention {
        Some(reason) => view! { <div data-testid="conversation-execution-attention" class="flex items-center gap-2"><AttentionChip reason=reason /></div> }.into_any(),
        None => view! { <div data-testid="conversation-execution-attention" class="text-xs text-amber-100/90">"Needs you: inspect this run before another attempt."</div> }.into_any(),
    });
    let explanation = (!execution.explanation.text.is_empty()).then(|| execution.explanation.text.clone());
    view! {
        <article data-testid="conversation-execution" data-state=state class="ml-4 rounded-lg border border-sky-300/20 bg-sky-400/5 px-3 py-2">
            <div class="mb-1 text-xs font-medium text-white/50">"Task run"</div>
            <div data-testid="conversation-execution-task" class="text-sm font-medium text-white">{execution.task_title}</div>
            <div data-testid="conversation-execution-approval" class="text-xs text-white/50">{format!("{approval} · Task {}", execution.task_id.as_simple().to_string().chars().take(8).collect::<String>())}</div>
            <div data-testid="conversation-execution-run" class="mt-1 text-xs text-white/70">{execution_run_text(execution.run)}</div>
            <div data-testid="conversation-execution-state" class="mt-1 text-sm text-white/90">{execution_state_text(execution.state)}</div>
            <div class="mt-2 space-y-1">{result}</div>
            {attention}
            {explanation.map(|text| view! { <p data-testid="conversation-execution-explanation" class="mt-2 text-xs text-white/50">{text}</p> })}
        </article>
    }
}

#[component]
pub fn ProjectConversation(project_id: String) -> impl IntoView {
    let (snapshot, set_snapshot) = signal(None::<ConversationSnapshot>);
    let (draft, set_draft) = signal(String::new());
    let (busy, set_busy) = signal(false);
    let (error, set_error) = signal(None::<String>);
    let (executions, set_executions) = signal(Vec::<ConversationExecution>::new());
    let (next_snapshot_request, set_next_snapshot_request) = signal(0u64);
    let (applied_snapshot_request, set_applied_snapshot_request) = signal(0u64);
    let begin_snapshot_request = Callback::new(move |()| {
        let mut request = 0;
        set_next_snapshot_request.update(|latest| {
            *latest = latest.saturating_add(1);
            request = *latest;
        });
        request
    });
    let publish_snapshot = Callback::new(move |(request, value): (u64, ConversationSnapshot)| {
        // A response can arrive after a Project switch has disposed this view.
        // The accessors below report that nothing is left, instead of panicking.
        let Some(applied) = applied_snapshot_request.try_get_untracked() else { return; };
        let current = snapshot.try_get_untracked().flatten();
        let current_version = current.as_ref().map(|current| (current.conversation.id, current.conversation.revision));
        let incoming_version = (value.conversation.id, value.conversation.revision);
        if should_publish_snapshot(current_version, incoming_version, request, applied) {
            // The liveness poll returns the whole history every 800 ms. Setting
            // the signal re-renders every entry, so an unchanged poll must not set it.
            if current.as_ref() != Some(&value) {
                let _ = set_executions.try_set(value.executions.clone());
                let _ = set_snapshot.try_set(Some(value));
            }
            let _ = set_applied_snapshot_request.try_set(request);
        }
    });

    // The history follows its newest entry until the Human scrolls up to read
    // earlier ones. "Jump to latest" and sending a message bring it back.
    let history_ref = NodeRef::<leptos::html::Div>::new();
    let (following_latest, set_following_latest) = signal(true);
    let scroll_to_latest = move || {
        if let Some(el) = history_ref.try_get_untracked().flatten() {
            el.set_scroll_top(el.scroll_height());
        }
    };
    // The entries render from the same snapshot, so the scroll waits one tick for the new height.
    Effect::new(move |_| {
        snapshot.track();
        if following_latest.get_untracked() {
            gloo_timers::callback::Timeout::new(0, scroll_to_latest).forget();
        }
    });
    let on_history_scroll = move |_| {
        if let Some(el) = history_ref.try_get_untracked().flatten() {
            set_following_latest.set(is_at_latest(el.scroll_top(), el.scroll_height(), el.client_height()));
        }
    };


    let initial_id = project_id.clone();
    let initial_begin_request = begin_snapshot_request;
    let initial_publish = publish_snapshot;
    Effect::new(move |_| {
        if initial_id.is_empty() { return; }
        let id = initial_id.clone();
        let request = initial_begin_request.run(());
        let publish = initial_publish;
        spawn_local(async move {
            // Opening takes the Project lock, which a live run holds, so a page
            // remounted mid-run reads first and only opens a Conversation that
            // has never been opened.
            let loaded = match conversation_service::get_project_conversation(id.clone()).await {
                Ok(value) => Ok(value),
                Err(_) => conversation_service::open_project_conversation(id).await,
            };
            match loaded {
                Ok(value) => publish.run((request, value)),
                Err(error) => { let _ = set_error.try_set(Some(error)); }
            }
        });
    });
    let poll_id = project_id.clone();
    let poll_begin_request = begin_snapshot_request;
    let poll_publish = publish_snapshot;
    let timer = StoredValue::new_local(Some(gloo_timers::callback::Interval::new(800, move || {
        let active = snapshot.get_untracked().is_some_and(|value| {
            should_poll_snapshot(busy.get_untracked(), value.coordinator_live, value.worker_live)
        });
        if !active { return; }
        let id = poll_id.clone();
        let request = poll_begin_request.run(());
        let publish = poll_publish;
        spawn_local(async move { if let Ok(value) = conversation_service::get_project_conversation(id).await { publish.run((request, value)); } });
    })));
    on_cleanup(move || { timer.update_value(|value| { value.take(); }); });

    // Execution views refresh on their own, slower clock. The liveness poll
    // above stays scoped to the Coordinator and the legacy Conversation Worker.
    // This read writes nothing, and it runs only while a shown run can still change.
    let execution_id = project_id.clone();
    let stable_refreshes = StoredValue::new_local(0u32);
    let seen_executions = StoredValue::new_local(Vec::<ConversationExecution>::new());
    let execution_timer = StoredValue::new_local(Some(gloo_timers::callback::Interval::new(EXECUTION_REFRESH_MS, move || {
        if snapshot.get_untracked().is_none() { return; }
        let current = executions.get_untracked();
        // Any change to the list, from a refresh or from a published snapshot, restarts the unconfirmed budget.
        if seen_executions.with_value(|seen| *seen != current) {
            seen_executions.set_value(current.clone());
            stable_refreshes.set_value(0);
        }
        if !should_refresh_executions(&current, stable_refreshes.get_value()) { return; }
        let id = execution_id.clone();
        spawn_local(async move {
            if let Ok(value) = conversation_service::get_project_conversation_executions(id).await {
                // The view may have been disposed while the request was in flight, so the late
                // response uses the non-panicking accessors and is dropped when nothing is left to update.
                if value == current {
                    stable_refreshes.try_update_value(|count| *count = count.saturating_add(1));
                } else if executions.try_get_untracked().is_some_and(|now| now == current) {
                    // Apply the response only if nothing newer arrived while it was in flight. A newer snapshot is never overwritten by an older read; the next tick refreshes again when needed.
                    let _ = set_executions.try_set(value);
                }
            }
        });
    })));
    on_cleanup(move || { execution_timer.update_value(|value| { value.take(); }); });

    let send_id = project_id.clone();
    let send_begin_request = begin_snapshot_request;
    let send_publish = publish_snapshot;
    let on_send = move || {
        let message = draft.get_untracked().trim().to_string();
        if message.is_empty() || busy.get_untracked() { return; }
        let revision_before = snapshot.get_untracked().map(|value| value.conversation.revision);
        set_draft.set(String::new()); set_busy.set(true); set_error.set(None);
        set_following_latest.set(true);
        let id = send_id.clone();
        let request = send_begin_request.run(());
        let publish = send_publish;
        let refresh_id = send_id.clone();
        let refresh_begin_request = send_begin_request;
        let refresh_publish = send_publish;
        spawn_local(async move {
            match conversation_service::send_project_message(id, message.clone()).await {
                Ok(value) => publish.run((request, value)),
                Err(error) => {
                    set_error.set(Some(error));
                    // A failed send can still have saved the Human message. Reload first, and
                    // restore the draft only when the Conversation did not change, so the
                    // same message is never typed and saved twice.
                    let refresh_request = refresh_begin_request.run(());
                    let maybe_saved = match conversation_service::get_project_conversation(refresh_id).await {
                        Ok(value) => {
                            let changed = revision_before.is_none_or(|before| value.conversation.revision != before);
                            refresh_publish.run((refresh_request, value));
                            changed
                        }
                        Err(_) => true,
                    };
                    if !maybe_saved && draft.get_untracked().is_empty() { set_draft.set(message); }
                }
            }
            set_busy.set(false);
        });
    };
    let stop_id = StoredValue::new_local(project_id.clone());
    let approve_project_id = project_id.clone();
    let approve_begin_request = begin_snapshot_request;
    let approve_publish = publish_snapshot;
    let approve_action = Callback::new(move |(action_id, request): (String, Option<String>)| {
        let Some(current) = snapshot.get_untracked() else { return; };
        set_busy.set(true);
        let id = approve_project_id.clone();
        let refresh_id = approve_project_id.clone();
        let request_id = approve_begin_request.run(());
        let publish = approve_publish;
        let refresh_begin_request = approve_begin_request;
        let refresh_publish = approve_publish;
        spawn_local(async move {
            match conversation_service::act_on_project_conversation(id, current.conversation.id.to_string(), current.conversation.revision, action_id, ConversationHumanAction::Approve { request }).await {
                Ok(value) => publish.run((request_id, value)), Err(error) => { set_error.set(Some(error)); let refresh_request = refresh_begin_request.run(()); if let Ok(value) = conversation_service::get_project_conversation(refresh_id).await { refresh_publish.run((refresh_request, value)); } },
            }
            set_busy.set(false);
        });
    });
    let reject_project_id = project_id.clone();
    let reject_begin_request = begin_snapshot_request;
    let reject_publish = publish_snapshot;
    let reject_action = Callback::new(move |action_id: String| {
        let Some(current) = snapshot.get_untracked() else { return; };
        set_busy.set(true);
        let id = reject_project_id.clone();
        let refresh_id = reject_project_id.clone();
        let request_id = reject_begin_request.run(());
        let publish = reject_publish;
        let refresh_begin_request = reject_begin_request;
        let refresh_publish = reject_publish;
        spawn_local(async move {
            match conversation_service::act_on_project_conversation(id, current.conversation.id.to_string(), current.conversation.revision, action_id, ConversationHumanAction::Reject).await {
                Ok(value) => publish.run((request_id, value)), Err(error) => { set_error.set(Some(error)); let refresh_request = refresh_begin_request.run(()); if let Ok(value) = conversation_service::get_project_conversation(refresh_id).await { refresh_publish.run((refresh_request, value)); } },
            }
            set_busy.set(false);
        });
    });
    let withdraw_project_id = project_id.clone();
    let withdraw_begin_request = begin_snapshot_request;
    let withdraw_publish = publish_snapshot;
    let withdraw_action = Callback::new(move |action_id: String| {
        let Some(current) = snapshot.get_untracked() else { return; };
        set_busy.set(true);
        let id = withdraw_project_id.clone();
        let refresh_id = withdraw_project_id.clone();
        let request_id = withdraw_begin_request.run(());
        let publish = withdraw_publish;
        let refresh_begin_request = withdraw_begin_request;
        let refresh_publish = withdraw_publish;
        spawn_local(async move {
            match conversation_service::act_on_project_conversation(id, current.conversation.id.to_string(), current.conversation.revision, action_id, ConversationHumanAction::Withdraw).await {
                Ok(value) => publish.run((request_id, value)), Err(error) => { set_error.set(Some(error)); let refresh_request = refresh_begin_request.run(()); if let Ok(value) = conversation_service::get_project_conversation(refresh_id).await { refresh_publish.run((refresh_request, value)); } },
            }
            set_busy.set(false);
        });
    });
    let retry_project_id = project_id.clone();
    let retry_begin_request = begin_snapshot_request;
    let retry_publish = publish_snapshot;
    let retry_coordinator = Callback::new(move |()| {
        if busy.get_untracked() { return; }
        let Some(current) = snapshot.get_untracked() else { return; };
        let Some(action_id) = current.conversation.actions.iter()
            .find(|action| action.status == ConversationActionStatus::Returned && !action.coordinator_replied)
            .map(|action| action.id.to_string()) else { return; };
        set_busy.set(true);
        set_error.set(None);
        let id = retry_project_id.clone();
        let request = retry_begin_request.run(());
        let publish = retry_publish;
        spawn_local(async move {
            match conversation_service::retry_project_conversation_continuation(
                id, current.conversation.id.to_string(), current.conversation.revision, action_id,
            ).await {
                Ok(value) => publish.run((request, value)),
                Err(error) => { let _ = set_error.try_set(Some(error)); }
            }
            set_busy.set(false);
        });
    });
    let turn_project_id = project_id.clone();
    let turn_begin_request = begin_snapshot_request;
    let turn_publish = publish_snapshot;
    let retry_turn = Callback::new(move |()| {
        if busy.get_untracked() { return; }
        let Some(current) = snapshot.get_untracked() else { return; };
        set_busy.set(true);
        set_error.set(None);
        let id = turn_project_id.clone();
        let refresh_id = turn_project_id.clone();
        let request = turn_begin_request.run(());
        let publish = turn_publish;
        let refresh_begin_request = turn_begin_request;
        let refresh_publish = turn_publish;
        spawn_local(async move {
            match conversation_service::retry_project_message(id, current.conversation.id.to_string(), current.conversation.revision).await {
                Ok(value) => publish.run((request, value)),
                Err(error) => {
                    // A refused retry means the Conversation moved on, so reload it.
                    set_error.set(Some(error));
                    let refresh_request = refresh_begin_request.run(());
                    if let Ok(value) = conversation_service::get_project_conversation(refresh_id).await {
                        refresh_publish.run((refresh_request, value));
                    }
                }
            }
            set_busy.set(false);
        });
    });
    view! {
            <section data-testid="project-conversation" class="flex h-full min-h-0 flex-col rounded-xl border border-white/10 bg-zinc-900/70 p-4">
                <header class="mb-3 flex shrink-0 items-center justify-between">
                    <div><h2 class="text-lg font-semibold text-white">"Project Conversation"</h2><p class="text-xs text-white/50">"Talk with the Project Coordinator. Task work always asks for approval."</p></div>
                    <Show when=move || snapshot.get().is_some_and(|s| s.coordinator_live || s.worker_live)>
                        <button data-testid="conversation-stop" class="rounded-md border border-red-400/40 px-3 py-1 text-sm text-red-200" on:click=move |_| { let id = stop_id.get_value(); spawn_local(async move { if let Err(error) = conversation_service::stop_project_conversation(id).await { set_error.set(Some(error)); } }); }>"Stop run"</button>
                    </Show>
                </header>
                <div class="mb-3 min-h-0 flex-1 space-y-2 overflow-y-auto" data-testid="conversation-history" node_ref=history_ref on:scroll=on_history_scroll>
                    {move || snapshot.get().is_some_and(|value| value.conversation.entries.is_empty()).then(|| view! {
                        <p data-testid="conversation-empty" class="text-sm text-white/50">"No messages yet. Ask the Coordinator about this Project or its Tasks. It can propose changes, and you approve each one before SlashIt applies it."</p>
                    })}
                    {move || snapshot.get().map(|value| {
                        let actions = value.conversation.actions;
                        let project_actions = value.conversation.project_actions;
                        let executions = executions.get();
                        let now = Utc::now();
                        value.conversation.entries.into_iter().flat_map(|entry| {
                            let when = entry_time(&entry.created_at, now, &local_offset_at);
                            let applied = match &entry.kind { ConversationEntryKind::ProjectActionApplied { action_id, .. } => Some(*action_id), _ => None };
                            let (testid, text) = match entry.kind {
                                ConversationEntryKind::HumanMessage { text } | ConversationEntryKind::CoordinatorReply { text } => ("conversation-message", Some(text)),
                                ConversationEntryKind::ActionProposed { action_id } => ("conversation-action-event", Some(actions.iter().find(|action| action.id == action_id).map(|action| format!("Proposed work for {} (Task {}): {}", action.target_task_title, action.target_task_id, action.request)).unwrap_or_else(|| MISSING_RECORD_TEXT.to_string()))),
                                ConversationEntryKind::ActionDecision { approved, request, .. } => ("conversation-action-event", Some(if approved { format!("You approved the Worker request: {}", request.unwrap_or_default()) } else { "You rejected the proposed Task work.".to_string() })),
                                ConversationEntryKind::WorkerStarted { action_id } => ("conversation-action-event", Some(actions.iter().find(|action| action.id == action_id).map(|action| format!("Worker started in Task Checkout: {}", action.target_task_title)).unwrap_or_else(|| MISSING_RECORD_TEXT.to_string()))),
                                ConversationEntryKind::WorkerResult { result, .. } => ("conversation-worker-result", Some(result)),
                                ConversationEntryKind::ActionWithdrawn { .. } => ("conversation-action-event", Some("You withdrew an earlier approval of a retired direct delegation. Nothing was started.".to_string())),
                                ConversationEntryKind::RunFailed { message, .. } => ("conversation-run-failed", Some(message)),
                                ref kind @ (ConversationEntryKind::ProjectActionProposed { .. } | ConversationEntryKind::ProjectActionDecision { .. } | ConversationEntryKind::ProjectActionApplied { .. } | ConversationEntryKind::ProjectActionRefused { .. }) => ("conversation-action-event", Some(project_action_event_text(kind, &project_actions).unwrap_or_else(|| MISSING_RECORD_TEXT.to_string()))),
                            };
                            let role = match entry.role { ConversationRole::Human => "You", ConversationRole::Coordinator => "Coordinator", ConversationRole::Worker => "Worker" };
                            let event = text.map(|text| view! { <article data-testid=testid class="rounded-lg bg-black/25 px-3 py-2"><div class="mb-1 flex items-baseline justify-between gap-2 text-xs font-medium text-white/50"><span>{role}</span><span data-testid="conversation-entry-time" class="font-normal text-white/40">{when}</span></div><div class="whitespace-pre-wrap text-sm text-white/90">{text}</div></article> }.into_any());
                            // The run an approval started appears beside the event that applied it, so the history keeps it after the proposal card is gone.
                            let card = applied.and_then(|action_id| executions.iter().find(|execution| execution.action_id == action_id).cloned()).map(|execution| {
                                let approval = approval_text(project_actions.iter().find(|action| action.id == execution.action_id));
                                execution_card(execution, approval).into_any()
                            });
                            event.into_iter().chain(card).collect::<Vec<_>>()
                        }).collect_view()
                    })}
                </div>
                {move || {
                    (!following_latest.get() && snapshot.get().is_some_and(|value| !value.conversation.entries.is_empty())).then(|| view! {
                        <div class="mb-2 flex shrink-0 justify-center">
                            <button data-testid="conversation-jump-latest" class="rounded-full border border-white/20 bg-zinc-900 px-3 py-1 text-xs text-white/80" on:click=move |_| { set_following_latest.set(true); scroll_to_latest(); }>"Jump to latest"</button>
                        </div>
                    })
                }}
                <div class="mb-3 max-h-[40%] shrink-0 space-y-2 overflow-y-auto">
                    <For
                        each=move || snapshot.get().map(|value| value.conversation.actions.into_iter().filter(|action| retired_delegation_controls(action.status).is_some()).collect::<Vec<_>>()).unwrap_or_default()
                        key=|action| action.id
                        children=move |action| {
                        let action_uuid = action.id;
                        let id = action_uuid.to_string(); let reject_id = id.clone(); let withdraw_id = id;
                        let recorded = action.approved_request.clone().unwrap_or_else(|| action.request.clone());
                        let recorded_request = Signal::derive(move || snapshot.get().and_then(|value| value.conversation.actions.into_iter().find(|item| item.id == action_uuid)).map(|item| item.approved_request.unwrap_or(item.request)).unwrap_or_else(|| recorded.clone()));
                        let status = Signal::derive(move || snapshot.get().and_then(|value| value.conversation.actions.into_iter().find(|item| item.id == action_uuid)).map(|item| item.status));
                        let controls = Signal::derive(move || status.get().and_then(retired_delegation_controls));
                        let reject_action = reject_action; let withdraw_action = withdraw_action;
                        view! {
                            <article data-testid="conversation-action-proposal" class="rounded-lg border border-amber-300/30 bg-amber-400/5 p-3">
                                <div class="text-xs uppercase tracking-wide text-amber-200">"Retired direct delegation"</div>
                                <div class="mt-1 font-medium text-white">{action.target_task_title}</div>
                                <div class="mt-1 break-all font-mono text-xs text-white/55">{format!("Task ID: {}", action.target_task_id)}</div>
                                <p class="mt-2 whitespace-pre-wrap text-sm text-white/75">{action.explanation}</p>
                                <p data-testid="conversation-retired-note" class="mt-2 text-sm text-amber-100/80">{move || controls.get().map(|c| c.note).unwrap_or_default()}</p>
                                <label class="mt-2 block text-xs text-white/50">"Recorded request"<textarea data-testid="conversation-action-request" class="mt-1 min-h-20 w-full rounded bg-black/30 p-2 text-sm text-white" readonly=true prop:value=move || recorded_request.get()></textarea></label>
                                <div class="mt-2 flex gap-2">
                                    {move || controls.get().is_some_and(|c| c.can_reject).then(|| {
                                        let reject_id = reject_id.clone();
                                        view! { <button data-testid="conversation-action-reject" class="rounded border border-white/20 px-3 py-1 text-sm" disabled=move || busy.get() on:click=move |_| reject_action.run(reject_id.clone())>"Reject"</button> }
                                    })}
                                    {move || controls.get().is_some_and(|c| c.can_withdraw).then(|| {
                                        let withdraw_id = withdraw_id.clone();
                                        view! { <button data-testid="conversation-action-withdraw" class="rounded border border-amber-200/40 px-3 py-1 text-sm text-amber-100" disabled=move || busy.get() on:click=move |_| withdraw_action.run(withdraw_id.clone())>"Withdraw approval"</button> }
                                    })}
                                </div>
                            </article>
                        }
                        }
                    />
                </div>
                <div class="mb-3 shrink-0 space-y-2" data-testid="project-actions">
                    <For
                        each=move || snapshot.get().map(|value| pending_project_actions(&value)).unwrap_or_default()
                        key=|action| (action.id, action.status)
                        children=move |action| {
                            let view = mutation_view(&action.mutation);
                            let controls = project_action_controls(action.status);
                            let approve_id = action.id.to_string(); let reject_id = approve_id.clone();
                            let approve_action = approve_action; let reject_action = reject_action;
                            let status = format!("{:?}", action.status).to_lowercase();
                            let action_id = action.id;
                            let note = approval_effect_note(&action.mutation);
                            let approval = approval_text(Some(&action));
                            view! {
                                <article data-testid="project-action-proposal" data-status=status class="rounded-lg border border-amber-300/30 bg-amber-400/5 p-3">
                                    <div class="text-xs uppercase tracking-wide text-amber-200">{format!("Coordinator proposes: {}", view.heading)}</div>
                                    <div class="mt-1 font-medium text-white" data-testid="project-action-title">{view.title}</div>
                                    <p class="mt-2 whitespace-pre-wrap text-sm text-white/75">{action.explanation}</p>
                                    <dl class="mt-2 space-y-1 text-sm" data-testid="project-action-details">
                                        {view.rows.into_iter().map(|(label, value)| view! { <div class="flex gap-2"><dt class="w-24 shrink-0 text-xs text-white/50">{label}</dt><dd class="whitespace-pre-wrap break-words text-white/90">{value}</dd></div> }).collect_view()}
                                    </dl>
                                    <p class="mt-2 text-xs text-white/50">{note}</p>
                                    // An approved action whose apply has not finished may already have started a run: show what the records say, under the proposal.
                                    {move || executions.get().into_iter().find(|execution| execution.action_id == action_id).map(|execution| execution_card(execution, approval))}
                                    <div class="mt-2 flex gap-2">
                                        {controls.approve_label.map(|label| view! { <button data-testid="project-action-approve" class="rounded bg-emerald-700 px-3 py-1 text-sm" disabled=move || busy.get() on:click=move |_| approve_action.run((approve_id.clone(), None))>{label}</button> })}
                                        {controls.can_reject.then(|| view! { <button data-testid="project-action-reject" class="rounded border border-white/20 px-3 py-1 text-sm" disabled=move || busy.get() on:click=move |_| reject_action.run(reject_id.clone())>"Reject"</button> })}
                                    </div>
                                </article>
                            }
                        }
                    />
                </div>
                {move || snapshot.get().map(|value| {
                    let status = if value.worker_live { "Worker is running in the Task Checkout" } else if value.coordinator_live { "Coordinator is responding" } else { "Conversation is ready" };
                    view! { <div data-testid="conversation-run-state" class="mb-2 shrink-0 text-xs text-white/50">{status}</div> }
                })}
                {move || {
                    snapshot.get().and_then(|value| {
                        value.conversation.actions.iter()
                            .find(|action| action.status == ConversationActionStatus::Returned && !action.coordinator_replied)
                            .map(|_| {
                                let retry = retry_coordinator;
                                view! {
                                    <div data-testid="conversation-continuation-error" class="mb-3 rounded border border-amber-300/30 bg-amber-400/5 p-3 text-sm text-amber-100">
                                        <p>{value.continuation_error.unwrap_or_else(|| "The Worker result is saved and awaits a Coordinator response.".into())}</p>
                                        <button data-testid="conversation-retry-coordinator" class="mt-2 rounded border border-amber-200/30 px-3 py-1" disabled=move || busy.get() on:click=move |_| retry.run(())>"Retry Coordinator response"</button>
                                    </div>
                                }
                            })
                    })
                }}
                {move || {
                    snapshot.get().filter(|value| value.turn_retry_available).map(|_| {
                        let retry = retry_turn;
                        view! {
                            <div data-testid="conversation-turn-retry" class="mb-3 flex shrink-0 items-center justify-between gap-3 rounded border border-amber-300/30 bg-amber-400/5 p-3 text-sm text-amber-100">
                                <p>"Your last message has no reply yet. Retry asks the Coordinator again for that message, and it is not saved twice. Sending a new message ends this Retry: the earlier message stays unanswered, and only the new one is answered."</p>
                                <button data-testid="conversation-retry-turn" class="shrink-0 rounded border border-amber-200/30 px-3 py-1" disabled=move || busy.get() on:click=move |_| retry.run(())>"Retry"</button>
                            </div>
                        }
                    })
                }}
                {move || snapshot.get().is_some_and(|value| has_unmediated_worker_result(&value)).then(|| view! {
                    <p data-testid="conversation-awaiting-mediation" class="mb-2 shrink-0 text-xs text-amber-100/80">"A saved Worker result must be reviewed before you send another message."</p>
                })}
                <form class="flex shrink-0 items-end gap-2" on:submit=move |event: web_sys::SubmitEvent| { event.prevent_default(); on_send(); }>
                    <textarea data-testid="conversation-message-input" class="min-h-16 flex-1 rounded-lg border border-white/10 bg-black/30 p-3 text-sm text-white" placeholder="Ask about this Project…" prop:value=draft disabled=move || snapshot.get().is_some_and(|value| has_unmediated_worker_result(&value)) on:input=move |event| set_draft.set(event_target_value(&event))></textarea>
                    <button data-testid="conversation-send" type="submit" class="rounded-lg bg-indigo-600 px-4 py-2 text-sm font-medium" disabled=move || busy.get() || draft.get().trim().is_empty() || snapshot.get().is_some_and(|value| has_unmediated_worker_result(&value))>"Send"</button>
                </form>
                {move || error.get().map(|message| view! { <p data-testid="conversation-error" class="mt-2 text-sm text-red-300">{message}</p> })}
            </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polling_continues_until_a_live_run_is_absent_from_the_snapshot() {
        assert!(should_poll_snapshot(false, true, false));
        assert!(should_poll_snapshot(false, false, true));
        assert!(should_poll_snapshot(true, false, false));
        assert!(!should_poll_snapshot(false, false, false));
    }

    #[test]
    fn the_history_keeps_following_the_newest_entry_only_near_its_end() {
        assert!(is_at_latest(0, 500, 500));
        assert!(is_at_latest(100, 500, 380));
        assert!(!is_at_latest(0, 500, 400));
    }

    #[test]
    fn an_entry_time_is_shown_in_the_timeline_form_or_not_at_all() {
        let utc = |_: DateTime<Utc>| chrono::FixedOffset::east_opt(0).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-10-25T12:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(entry_time("2026-10-25T08:30:00.123456789Z", now, &utc).as_deref(), Some("08:30"));
        assert_eq!(entry_time("2026-10-24T09:05:00Z", now, &utc).as_deref(), Some("Oct 24 09:05"));
        assert_eq!(entry_time("", now, &utc), None);
    }

    #[test]
    fn an_older_revision_never_replaces_a_newer_conversation_snapshot() {
        let conversation = Uuid::new_v4();
        assert!(!should_publish_snapshot(Some((conversation, 9)), (conversation, 8), 12, 11));
    }

    #[test]
    fn a_newer_revision_can_arrive_from_an_earlier_request() {
        let conversation = Uuid::new_v4();
        assert!(should_publish_snapshot(Some((conversation, 8)), (conversation, 9), 4, 10));
    }

    #[test]
    fn equal_revision_snapshots_follow_request_order() {
        let conversation = Uuid::new_v4();
        assert!(!should_publish_snapshot(Some((conversation, 9)), (conversation, 9), 4, 5));
        assert!(should_publish_snapshot(Some((conversation, 9)), (conversation, 9), 6, 5));
    }

    #[test]
    fn conversation_identity_changes_follow_request_order() {
        let current = Uuid::new_v4();
        let incoming = Uuid::new_v4();
        assert!(!should_publish_snapshot(Some((current, 20)), (incoming, 0), 1, 10));
        assert!(should_publish_snapshot(Some((current, 20)), (incoming, 0), 11, 10));
    }

    #[test]
    fn a_retired_proposal_can_only_be_rejected() {
        use ConversationActionStatus::*;
        let controls = retired_delegation_controls(Proposed).expect("a proposal still has a decision");
        assert!(controls.can_reject);
        assert!(!controls.can_withdraw);
        assert!(controls.note.contains("retired"));
    }

    #[test]
    fn a_retired_approval_can_only_be_withdrawn_and_no_card_starts_a_worker() {
        use ConversationActionStatus::*;
        let controls = retired_delegation_controls(Approved).expect("an approval can be withdrawn");
        assert!(controls.can_withdraw);
        assert!(!controls.can_reject);
        assert!(controls.note.contains("will not start it"));
        for finished in [Running, Returned, Rejected, Interrupted, Failed, Withdrawn] {
            assert_eq!(retired_delegation_controls(finished), None, "{finished:?}");
        }
    }

    #[test]
    fn a_create_proposal_shows_exactly_what_will_be_created() {
        let view = mutation_view(&ConversationTaskMutation::CreateTask { task_id: Uuid::new_v4(), title: "Add dark mode".into(), description: Some("Support a dark theme".into()), priority: "high".into(), category: "ui_ux".into() });
        assert_eq!((view.heading, view.title.as_str()), ("Create Task", "Add dark mode"));
        assert_eq!(view.rows, vec![
            ("Status".to_string(), "Backlog".to_string()), ("Priority".to_string(), "high".to_string()),
            ("Category".to_string(), "ui ux".to_string()), ("Description".to_string(), "Support a dark theme".to_string()),
        ]);
    }

    #[test]
    fn an_edit_proposal_shows_each_approved_change_from_its_observed_value() {
        let id = Uuid::new_v4();
        let view = mutation_view(&ConversationTaskMutation::EditTask { target_task_id: id, target_task_title: "Old".into(), changes: vec![
            ConversationFieldChange::Title { from: "Old".into(), to: "New".into() },
            ConversationFieldChange::Description { from: None, to: "Why".into() },
            ConversationFieldChange::Priority { from: "medium".into(), to: "urgent".into() },
        ] });
        assert_eq!(view.heading, "Edit Task");
        assert_eq!(view.rows, vec![
            ("Task ID".to_string(), id.to_string()), ("Title".to_string(), "Old → New".to_string()),
            ("Description".to_string(), "(none) → Why".to_string()), ("Priority".to_string(), "medium → urgent".to_string()),
        ]);
    }

    #[test]
    fn a_move_proposal_states_the_status_change_and_that_no_work_starts() {
        let id = Uuid::new_v4();
        let mutation = ConversationTaskMutation::MoveTask { target_task_id: id, target_task_title: "Old".into(), from: "queue".into(), to: "backlog".into() };
        let view = mutation_view(&mutation);
        assert_eq!(view.heading, "Move Task");
        assert_eq!(view.rows, vec![
            ("Task ID".to_string(), id.to_string()), ("Status".to_string(), "queue → backlog".to_string()),
            ("Effect".to_string(), "Reorganizes the board only; no work is started".to_string()),
        ]);
        let wire = serde_json::json!({"kind":"move_task","target_task_id":id,"target_task_title":"Old","from":"queue","to":"backlog"});
        assert_eq!(serde_json::from_value::<ConversationTaskMutation>(wire).unwrap(), mutation);
    }

    #[test]
    fn approve_and_reject_serialize_to_the_commands_the_backend_accepts() {
        // The card sends `Approve { request: None }`: Project actions are not
        // editable, so the Human approves exactly the proposal on screen.
        assert_eq!(serde_json::to_value(ConversationHumanAction::Approve { request: None }).unwrap(), serde_json::json!({"action":"approve","request":null}));
        assert_eq!(serde_json::to_value(ConversationHumanAction::Reject).unwrap(), serde_json::json!({"action":"reject"}));
        assert_eq!(serde_json::to_value(ConversationHumanAction::Withdraw).unwrap(), serde_json::json!({"action":"withdraw"}));
    }

    #[test]
    fn only_unfinished_actions_offer_controls() {
        use ConversationProjectActionStatus::*;
        assert_eq!(project_action_controls(Proposed), ProjectActionControls { approve_label: Some("Approve"), can_reject: true });
        assert_eq!(project_action_controls(Approved), ProjectActionControls { approve_label: Some("Retry approved action"), can_reject: false });
        for finished in [Applied, Rejected, Refused] {
            assert_eq!(project_action_controls(finished), ProjectActionControls { approve_label: None, can_reject: false });
        }
    }

    #[test]
    fn completed_actions_report_their_result_and_leave_no_pending_card() {
        let task_id = Uuid::new_v4();
        let action = ConversationProjectAction { id: Uuid::new_v4(), explanation: "why".into(), status: ConversationProjectActionStatus::Applied,
            mutation: ConversationTaskMutation::CreateTask { task_id, title: "Add dark mode".into(), description: None, priority: "medium".into(), category: "feature".into() },
            outcome: Some(ConversationProjectActionOutcome::Applied { task_id, title: "Add dark mode".into() }), created_at: String::new() };
        let text = project_action_event_text(&ConversationEntryKind::ProjectActionApplied { action_id: action.id, task_id }, std::slice::from_ref(&action)).unwrap();
        assert_eq!(text, format!("Task created in Backlog: Add dark mode (Task {task_id})"));
        let refused = project_action_event_text(&ConversationEntryKind::ProjectActionRefused { action_id: action.id, reason: "The Task changed after this edit was proposed; nothing was changed".into() }, &[]).unwrap();
        assert!(refused.starts_with("Nothing was changed:"));
    }

    #[test]
    fn a_queue_proposal_shows_the_input_it_binds_the_approval_to() {
        use crate::models::conversation::ExecutionInput;
        let mutation = ConversationTaskMutation::EnqueueTask {
            target_task_id: Uuid::new_v4(), target_task_title: "Build it".into(), from: "backlog".into(),
            input: Some(ExecutionInput { digest: "0123456789abcdef".into(), description: Some("Add a word counter".into()), subtask_count: 2, model: Some("opus".into()) }),
        };
        let view = mutation_view(&mutation);
        assert!(view.rows.iter().any(|(name, value)| name == "Will run" && value == "Add a word counter"));
        assert!(view.rows.iter().any(|(name, value)| name == "Subtasks" && value == "2"));
        assert!(view.rows.iter().any(|(name, value)| name == "Input" && value == "0123456789ab"));
        // A proposal saved before the input was recorded still deserializes.
        let legacy: ConversationTaskMutation = serde_json::from_value(serde_json::json!({"kind":"enqueue_task","target_task_id":Uuid::new_v4(),"target_task_title":"Old","from":"backlog"})).unwrap();
        assert!(matches!(legacy, ConversationTaskMutation::EnqueueTask { input: None, .. }));
    }

    fn execute_request_rows(model: Option<&str>) -> Vec<(String, String)> {
        use crate::models::conversation::ExecutionInput;
        let mutation = ConversationTaskMutation::ExecuteRequest {
            target_task_id: Uuid::new_v4(), target_task_title: "Build it".into(), from: "backlog".into(), request: "Add a changelog entry".into(),
            input: ExecutionInput { digest: "0123456789abcdef".into(), description: None, subtask_count: 0, model: model.map(str::to_string) },
            checkout: None,
        };
        mutation_view(&mutation).rows
    }

    #[test]
    fn an_execute_request_card_shows_the_checkout_it_reuses() {
        use crate::models::conversation::{ExecutionInput, ExistingCheckout};
        let mutation = ConversationTaskMutation::ExecuteRequest {
            target_task_id: Uuid::new_v4(), target_task_title: "Build it".into(), from: "backlog".into(), request: "Add a changelog entry".into(),
            input: ExecutionInput { digest: "0123456789abcdef".into(), description: None, subtask_count: 0, model: None },
            checkout: Some(ExistingCheckout {
                checkout_path: "/work/checkout".into(), branch: "slashit/task-1".into(), head: "aaaa1111".into(), base_commit: "bbbb2222".into(),
            }),
        };
        let rows = mutation_view(&mutation).rows;
        let value = |name: &str| rows.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
        assert_eq!(value("Branch").as_deref(), Some("slashit/task-1"));
        assert_eq!(value("Head").as_deref(), Some("aaaa1111"));
        assert_eq!(value("Base").as_deref(), Some("bbbb2222"));
        assert!(value("Effect").unwrap().contains("existing checkout"));
    }

    fn model_value(rows: &[(String, String)]) -> String {
        rows.iter().find(|(name, _)| name == "Model").map(|(_, value)| value.clone()).expect("the card shows a Model row")
    }

    #[test]
    fn an_execute_request_card_shows_an_explicit_model_as_configured() {
        assert_eq!(model_value(&execute_request_rows(Some("claude-opus-4-0-20250514"))), "claude-opus-4-0-20250514");
    }

    #[test]
    fn an_execute_request_card_discloses_provider_default_selection_without_naming_a_model() {
        for model in [Some(""), Some("default")] {
            let value = model_value(&execute_request_rows(model));
            assert_eq!(value, "Provider default — selected at execution time; the concrete model may change.");
        }
    }

    #[test]
    fn an_execute_request_card_does_not_claim_a_model_for_a_proposal_that_never_recorded_one() {
        let rows = execute_request_rows(None);
        assert_eq!(model_value(&rows), "Not recorded for this proposal");
        assert!(!rows.iter().any(|(_, value)| value.contains("Provider default")));
    }

    #[test]
    fn a_queue_proposal_says_it_only_queues_and_a_found_task_is_not_claimed() {
        let task_id = Uuid::new_v4();
        let mut action = ConversationProjectAction { id: Uuid::new_v4(), explanation: "why".into(), status: ConversationProjectActionStatus::Applied,
            mutation: ConversationTaskMutation::EnqueueTask { target_task_id: task_id, target_task_title: "Build it".into(), from: "backlog".into(), input: None },
            outcome: Some(ConversationProjectActionOutcome::Applied { task_id, title: "Build it".into() }), created_at: String::new() };
        let view = mutation_view(&action.mutation);
        assert_eq!(view.heading, "Queue Task");
        assert!(view.rows.iter().any(|(name, value)| name == "Status" && value == "backlog → queue"));
        assert!(view.rows.iter().any(|(name, value)| name == "Effect" && value.starts_with("Only queues the Task")));
        let applied = ConversationEntryKind::ProjectActionApplied { action_id: action.id, task_id };
        assert_eq!(project_action_event_text(&applied, std::slice::from_ref(&action)).unwrap(), format!("Task queued: Build it (Task {task_id})"));
        action.outcome = Some(ConversationProjectActionOutcome::AlreadyQueued { task_id, title: "Build it".into() });
        let found = project_action_event_text(&applied, std::slice::from_ref(&action)).unwrap();
        assert!(found.starts_with("Task was already in the queue") && found.contains("cannot tell"), "{found}");
    }

    #[test]
    fn project_action_snapshots_decode_from_the_backend_wire_shape() {
        let task_id = Uuid::new_v4();
        let wire = serde_json::json!({"conversation":{"id":Uuid::new_v4(),"project_id":Uuid::new_v4(),"revision":2,"created_at":"","updated_at":"","entries":[],"actions":[],
            "project_actions":[{"id":Uuid::new_v4(),"explanation":"e","status":"proposed","created_at":"",
                "mutation":{"kind":"create_task","task_id":task_id,"title":"T","description":null,"priority":"medium","category":"feature"}}]},
            "coordinator_live":false,"worker_live":false,"run_status":null});
        let snapshot: ConversationSnapshot = serde_json::from_value(wire).unwrap();
        assert_eq!(pending_project_actions(&snapshot).len(), 1);
        let legacy = serde_json::json!({"conversation":{"id":Uuid::new_v4(),"project_id":Uuid::new_v4(),"revision":0,"created_at":"","updated_at":"","entries":[],"actions":[]},"coordinator_live":false,"worker_live":false,"run_status":null});
        assert!(pending_project_actions(&serde_json::from_value(legacy).unwrap()).is_empty());
    }

    fn execution(state: ConversationExecutionState, transitional: bool) -> ConversationExecution {
        ConversationExecution {
            action_id: Uuid::from_u128(1),
            task_id: Uuid::from_u128(2),
            task_title: "Task".into(),
            state,
            transitional,
            run: Some(1),
            result: ConversationExecutionResult::NotApplicable,
            needs_human: false,
            attention: None,
            explanation: crate::models::ConversationBoundedText { text: String::new(), truncated: false, original_chars: 0 },
        }
    }

    #[test]
    fn execution_refresh_follows_only_runs_that_can_still_change() {
        use ConversationExecutionState as S;
        assert!(should_refresh_executions(&[execution(S::Queued, true)], 0));
        assert!(should_refresh_executions(&[execution(S::Running, true)], 10_000));
        assert!(!should_refresh_executions(&[execution(S::Completed, false)], 0));
        assert!(!should_refresh_executions(&[execution(S::Failed, false), execution(S::NoExecutionEvidence, false)], 0));
        assert!(!should_refresh_executions(&[], 0));
        // An unconfirmed run keeps refreshing until it has been unchanged for the budget.
        assert!(should_refresh_executions(&[execution(S::Unconfirmed, true)], UNCONFIRMED_STABLE_REFRESHES - 1));
        assert!(!should_refresh_executions(&[execution(S::Unconfirmed, true)], UNCONFIRMED_STABLE_REFRESHES));
    }

    #[test]
    fn each_execution_state_has_its_own_key_and_wording() {
        use ConversationExecutionState as S;
        let states = [S::PendingApproval, S::Queued, S::Running, S::Unconfirmed, S::Completed, S::Failed, S::RecoveryRequired, S::Ambiguous, S::NoExecutionEvidence, S::TaskUnavailable];
        let keys: std::collections::HashSet<_> = states.iter().map(|state| execution_state_key(*state)).collect();
        let texts: std::collections::HashSet<_> = states.iter().map(|state| execution_state_text(*state)).collect();
        assert_eq!(keys.len(), states.len());
        assert_eq!(texts.len(), states.len());
        // A completed coding run is never presented as a delivered Task.
        assert!(execution_state_text(S::Completed).contains("may still be in review"));
        assert!(execution_state_text(S::Unconfirmed).contains("not counted as completed or failed"));
    }

    #[test]
    fn an_execution_names_the_approval_that_started_it() {
        let queue = ConversationProjectAction {
            id: Uuid::from_u128(1),
            explanation: String::new(),
            mutation: ConversationTaskMutation::EnqueueTask { target_task_id: Uuid::from_u128(2), target_task_title: "T".into(), from: "backlog".into(), input: None },
            status: ConversationProjectActionStatus::Applied,
            outcome: None,
            created_at: String::new(),
        };
        assert_eq!(approval_text(Some(&queue)), "Approved queue request");
        assert_eq!(approval_text(None), "Approved queue request");
        assert_eq!(execution_run_text(None), "Run not established yet");
        assert_eq!(execution_run_text(Some(3)), "Run 3");
    }

    #[test]
    fn approval_wording_matches_what_approval_does() {
        let enqueue = ConversationTaskMutation::EnqueueTask { target_task_id: Uuid::from_u128(2), target_task_title: "T".into(), from: "backlog".into(), input: None };
        assert!(approval_effect_note(&enqueue).contains("queues this Task"));
        assert!(!approval_effect_note(&enqueue).contains("does not start any work"));
        let edit = ConversationTaskMutation::EditTask { target_task_id: Uuid::from_u128(2), target_task_title: "T".into(), changes: vec![] };
        assert!(approval_effect_note(&edit).contains("does not start any work"));
    }

    #[test]
    fn a_refresh_that_returns_after_disposal_finds_nothing_to_update() {
        // The refresh response is applied through these accessors. After the view is disposed they report
        // that nothing is left, instead of panicking as `get_untracked` and `set` do.
        let owner = leptos::reactive::owner::Owner::new();
        let (executions, set_executions) = owner.with(|| signal(Vec::<ConversationExecution>::new()));
        let stable_refreshes = owner.with(|| StoredValue::new_local(0u32));
        owner.cleanup();
        assert!(executions.try_get_untracked().is_none());
        assert!(set_executions.try_set(Vec::new()).is_some());
        assert!(stable_refreshes.try_update_value(|count| *count += 1).is_none());
    }
}
