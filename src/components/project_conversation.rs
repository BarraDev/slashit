use crate::models::{ConversationActionStatus, ConversationEntryKind, ConversationFieldChange, ConversationHumanAction, ConversationProjectAction, ConversationProjectActionOutcome, ConversationProjectActionStatus, ConversationRole, ConversationSnapshot, ConversationTaskMutation};
use crate::services::conversation_service;
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::HashMap;
use uuid::Uuid;

fn should_poll_snapshot(busy: bool, coordinator_live: bool, worker_live: bool) -> bool {
    busy || coordinator_live || worker_live
}

fn has_unmediated_worker_result(snapshot: &ConversationSnapshot) -> bool {
    snapshot.conversation.actions.iter().any(|action| action.status == ConversationActionStatus::Returned && !action.coordinator_replied)
}

/// The request the Human sees: an approved action always shows the persisted
/// `approved_request`, never a leftover local edit.
fn shown_request(approved: bool, edit: Option<String>, current: String) -> String {
    if approved { current } else { edit.unwrap_or(current) }
}

/// The request plain Approve must send so the approved text is the visible text.
/// An already approved action carries its request, so nothing is resent.
fn plain_approval_request(approved: bool, edit: Option<String>) -> Option<String> {
    if approved { None } else { edit }
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

#[component]
pub fn ProjectConversation(project_id: String) -> impl IntoView {
    let (snapshot, set_snapshot) = signal(None::<ConversationSnapshot>);
    let (draft, set_draft) = signal(String::new());
    let (edits, set_edits) = signal(HashMap::<String, String>::new());
    let (busy, set_busy) = signal(false);
    let (error, set_error) = signal(None::<String>);
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
        let current = snapshot.get_untracked();
        let current_version = current.as_ref().map(|current| (current.conversation.id, current.conversation.revision));
        let incoming_version = (value.conversation.id, value.conversation.revision);
        if should_publish_snapshot(current_version, incoming_version, request, applied_snapshot_request.get_untracked()) {
            set_snapshot.set(Some(value));
            set_applied_snapshot_request.set(request);
        }
    });

    let edits_snapshot = snapshot;
    Effect::new(move |_| {
        let active_ids = edits_snapshot.get().map(|value| value.conversation.actions.into_iter()
            .filter(|action| matches!(action.status, ConversationActionStatus::Proposed | ConversationActionStatus::Approved))
            .map(|action| action.id.to_string()).collect::<Vec<_>>()).unwrap_or_default();
        set_edits.update(|edits| edits.retain(|id, _| active_ids.contains(id)));
    });

    let initial_id = project_id.clone();
    let initial_begin_request = begin_snapshot_request.clone();
    let initial_publish = publish_snapshot.clone();
    Effect::new(move |_| {
        if initial_id.is_empty() { return; }
        let id = initial_id.clone();
        let request = initial_begin_request.run(());
        let publish = initial_publish.clone();
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
                Err(error) => set_error.set(Some(error)),
            }
        });
    });
    let poll_id = project_id.clone();
    let poll_begin_request = begin_snapshot_request.clone();
    let poll_publish = publish_snapshot.clone();
    let timer = StoredValue::new_local(Some(gloo_timers::callback::Interval::new(800, move || {
        let active = snapshot.get_untracked().is_some_and(|value| {
            should_poll_snapshot(busy.get_untracked(), value.coordinator_live, value.worker_live)
        });
        if !active { return; }
        let id = poll_id.clone();
        let request = poll_begin_request.run(());
        let publish = poll_publish.clone();
        spawn_local(async move { if let Ok(value) = conversation_service::get_project_conversation(id).await { publish.run((request, value)); } });
    })));
    on_cleanup(move || { timer.update_value(|value| { value.take(); }); });

    let send_id = project_id.clone();
    let send_begin_request = begin_snapshot_request.clone();
    let send_publish = publish_snapshot.clone();
    let on_send = move || {
        let message = draft.get_untracked().trim().to_string();
        if message.is_empty() || busy.get_untracked() { return; }
        set_draft.set(String::new()); set_busy.set(true); set_error.set(None);
        let id = send_id.clone();
        let request = send_begin_request.run(());
        let publish = send_publish.clone();
        spawn_local(async move {
            match conversation_service::send_project_message(id, message).await {
                Ok(value) => publish.run((request, value)),
                Err(error) => set_error.set(Some(error)),
            }
            set_busy.set(false);
        });
    };
    let stop_id = StoredValue::new_local(project_id.clone());
    let approve_project_id = project_id.clone();
    let approve_begin_request = begin_snapshot_request.clone();
    let approve_publish = publish_snapshot.clone();
    let approve_action = Callback::new(move |(action_id, request): (String, Option<String>)| {
        let Some(current) = snapshot.get_untracked() else { return; };
        set_busy.set(true);
        let id = approve_project_id.clone();
        let refresh_id = approve_project_id.clone();
        let request_id = approve_begin_request.run(());
        let publish = approve_publish.clone();
        let refresh_begin_request = approve_begin_request.clone();
        let refresh_publish = approve_publish.clone();
        spawn_local(async move {
            match conversation_service::act_on_project_conversation(id, current.conversation.id.to_string(), current.conversation.revision, action_id, ConversationHumanAction::Approve { request }).await {
                Ok(value) => publish.run((request_id, value)), Err(error) => { set_error.set(Some(error)); let refresh_request = refresh_begin_request.run(()); if let Ok(value) = conversation_service::get_project_conversation(refresh_id).await { refresh_publish.run((refresh_request, value)); } },
            }
            set_busy.set(false);
        });
    });
    let reject_project_id = project_id.clone();
    let reject_begin_request = begin_snapshot_request.clone();
    let reject_publish = publish_snapshot.clone();
    let reject_action = Callback::new(move |action_id: String| {
        let Some(current) = snapshot.get_untracked() else { return; };
        set_busy.set(true);
        let id = reject_project_id.clone();
        let refresh_id = reject_project_id.clone();
        let request_id = reject_begin_request.run(());
        let publish = reject_publish.clone();
        let refresh_begin_request = reject_begin_request.clone();
        let refresh_publish = reject_publish.clone();
        spawn_local(async move {
            match conversation_service::act_on_project_conversation(id, current.conversation.id.to_string(), current.conversation.revision, action_id, ConversationHumanAction::Reject).await {
                Ok(value) => publish.run((request_id, value)), Err(error) => { set_error.set(Some(error)); let refresh_request = refresh_begin_request.run(()); if let Ok(value) = conversation_service::get_project_conversation(refresh_id).await { refresh_publish.run((refresh_request, value)); } },
            }
            set_busy.set(false);
        });
    });
    let retry_project_id = project_id.clone();
    let retry_begin_request = begin_snapshot_request.clone();
    let retry_publish = publish_snapshot.clone();
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
        let publish = retry_publish.clone();
        spawn_local(async move {
            match conversation_service::retry_project_conversation_continuation(
                id, current.conversation.id.to_string(), current.conversation.revision, action_id,
            ).await {
                Ok(value) => publish.run((request, value)),
                Err(error) => set_error.set(Some(error)),
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
                <div class="mb-3 min-h-0 flex-1 space-y-2 overflow-y-auto" data-testid="conversation-history">
                    {move || snapshot.get().map(|value| {
                        let actions = value.conversation.actions;
                        let project_actions = value.conversation.project_actions;
                        value.conversation.entries.into_iter().filter_map(|entry| {
                            let (testid, text) = match entry.kind {
                                ConversationEntryKind::HumanMessage { text } | ConversationEntryKind::CoordinatorReply { text } => ("conversation-message", Some(text)),
                                ConversationEntryKind::ActionProposed { action_id } => ("conversation-action-event", actions.iter().find(|action| action.id == action_id).map(|action| format!("Proposed work for {} (Task {}): {}", action.target_task_title, action.target_task_id, action.request))),
                                ConversationEntryKind::ActionDecision { approved, request, .. } => ("conversation-action-event", Some(if approved { format!("You approved the Worker request: {}", request.unwrap_or_default()) } else { "You rejected the proposed Task work.".to_string() })),
                                ConversationEntryKind::WorkerStarted { action_id } => ("conversation-action-event", actions.iter().find(|action| action.id == action_id).map(|action| format!("Worker started in Task Checkout: {}", action.target_task_title))),
                                ConversationEntryKind::WorkerResult { result, .. } => ("conversation-worker-result", Some(result)),
                                ConversationEntryKind::RunFailed { message, .. } => ("conversation-run-failed", Some(message)),
                                ref kind @ (ConversationEntryKind::ProjectActionProposed { .. } | ConversationEntryKind::ProjectActionDecision { .. } | ConversationEntryKind::ProjectActionApplied { .. } | ConversationEntryKind::ProjectActionRefused { .. }) => ("conversation-action-event", project_action_event_text(kind, &project_actions)),
                            };
                            let role = match entry.role { ConversationRole::Human => "You", ConversationRole::Coordinator => "Coordinator", ConversationRole::Worker => "Worker" };
                            text.map(|text| view! { <article data-testid=testid class="rounded-lg bg-black/25 px-3 py-2"><div class="mb-1 text-xs font-medium text-white/50">{role}</div><div class="whitespace-pre-wrap text-sm text-white/90">{text}</div></article> })
                        }).collect_view()
                    })}
                </div>
                <div class="mb-3 max-h-[40%] shrink-0 space-y-2 overflow-y-auto">
                    <For
                        each=move || snapshot.get().map(|value| value.conversation.actions.into_iter().filter(|action| matches!(action.status, ConversationActionStatus::Proposed | ConversationActionStatus::Approved)).collect::<Vec<_>>()).unwrap_or_default()
                        key=|action| action.id
                        children=move |action| {
                        let action_uuid = action.id;
                        let id = action_uuid.to_string(); let input_id = id.clone(); let approve_id = id.clone(); let edit_approve_id = id.clone(); let reject_id = id.clone();
                        let original = action.approved_request.clone().unwrap_or_else(|| action.request.clone()); let value_fallback = original.clone(); let value_id = id.clone();
                        let current_request = Signal::derive(move || snapshot.get().and_then(|value| value.conversation.actions.into_iter().find(|item| item.id == action_uuid)).map(|item| item.approved_request.unwrap_or(item.request)).unwrap_or_else(|| value_fallback.clone()));
                        let is_approved = Signal::derive(move || snapshot.get().and_then(|value| value.conversation.actions.into_iter().find(|item| item.id == action_uuid)).is_some_and(|item| item.status == ConversationActionStatus::Approved));
                        let approve_action = approve_action.clone(); let edit_approve_action = approve_action.clone(); let reject_action = reject_action.clone();
                        view! {
                            <article data-testid="conversation-action-proposal" class="rounded-lg border border-amber-300/30 bg-amber-400/5 p-3">
                                <div class="text-xs uppercase tracking-wide text-amber-200">"Coordinator proposes Task work"</div>
                                <div class="mt-1 font-medium text-white">{action.target_task_title}</div>
                                <div class="mt-1 break-all font-mono text-xs text-white/55">{format!("Task ID: {}", action.target_task_id)}</div>
                                <p class="mt-2 whitespace-pre-wrap text-sm text-white/75">{action.explanation}</p>
                                <label class="mt-2 block text-xs text-white/50">"Worker request"<textarea data-testid="conversation-action-request" class="mt-1 min-h-20 w-full rounded bg-black/30 p-2 text-sm text-white" readonly=move || is_approved.get() prop:value=move || shown_request(is_approved.get(), edits.with(|edits| edits.get(&value_id).cloned()), current_request.get()) on:input=move |event| set_edits.update(|edits| { edits.insert(input_id.clone(), event_target_value(&event)); })></textarea></label>
                                <div class="mt-2 flex gap-2">
                                    <button data-testid="conversation-action-approve" class="rounded bg-emerald-700 px-3 py-1 text-sm" disabled=move || busy.get() on:click=move |_| { let request = plain_approval_request(is_approved.get_untracked(), edits.get_untracked().get(&approve_id).cloned()); approve_action.run((approve_id.clone(), request)); }>{move || if is_approved.get() { "Start approved Worker" } else { "Approve exact request" }}</button>
                                    <button data-testid="conversation-action-edit-approve" class="rounded bg-emerald-700/70 px-3 py-1 text-sm" disabled=move || busy.get() || is_approved.get() on:click=move |_| { let edited = edits.get_untracked().get(&id).cloned().unwrap_or(original.clone()); edit_approve_action.run((edit_approve_id.clone(), Some(edited))); } >"Edit + approve"</button>
                                    <button data-testid="conversation-action-reject" class="rounded border border-white/20 px-3 py-1 text-sm" disabled=move || busy.get() || is_approved.get() on:click=move |_| reject_action.run(reject_id.clone())>"Reject"</button>
                                </div>
                            </article>
                        }
                        }
                    />
                </div>
                <div class="mb-3 max-h-[40%] shrink-0 space-y-2 overflow-y-auto" data-testid="project-actions">
                    <For
                        each=move || snapshot.get().map(|value| pending_project_actions(&value)).unwrap_or_default()
                        key=|action| (action.id, action.status)
                        children=move |action| {
                            let view = mutation_view(&action.mutation);
                            let controls = project_action_controls(action.status);
                            let approve_id = action.id.to_string(); let reject_id = approve_id.clone();
                            let approve_action = approve_action.clone(); let reject_action = reject_action.clone();
                            let status = format!("{:?}", action.status).to_lowercase();
                            view! {
                                <article data-testid="project-action-proposal" data-status=status class="rounded-lg border border-amber-300/30 bg-amber-400/5 p-3">
                                    <div class="text-xs uppercase tracking-wide text-amber-200">{format!("Coordinator proposes: {}", view.heading)}</div>
                                    <div class="mt-1 font-medium text-white" data-testid="project-action-title">{view.title}</div>
                                    <p class="mt-2 whitespace-pre-wrap text-sm text-white/75">{action.explanation}</p>
                                    <dl class="mt-2 space-y-1 text-sm" data-testid="project-action-details">
                                        {view.rows.into_iter().map(|(label, value)| view! { <div class="flex gap-2"><dt class="w-24 shrink-0 text-xs text-white/50">{label}</dt><dd class="whitespace-pre-wrap break-words text-white/90">{value}</dd></div> }).collect_view()}
                                    </dl>
                                    <p class="mt-2 text-xs text-white/50">"Approving only changes this Task's record. It does not start any work."</p>
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
                                let retry = retry_coordinator.clone();
                                view! {
                                    <div data-testid="conversation-continuation-error" class="mb-3 rounded border border-amber-300/30 bg-amber-400/5 p-3 text-sm text-amber-100">
                                        <p>{value.continuation_error.unwrap_or_else(|| "The Worker result is saved and awaits a Coordinator response.".into())}</p>
                                        <button data-testid="conversation-retry-coordinator" class="mt-2 rounded border border-amber-200/30 px-3 py-1" disabled=move || busy.get() on:click=move |_| retry.run(())>"Retry Coordinator response"</button>
                                    </div>
                                }
                            })
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
    fn plain_approval_sends_the_edited_request_the_textarea_shows() {
        let shown = shown_request(false, Some("B".into()), "A".into());
        assert_eq!(shown, "B");
        assert_eq!(plain_approval_request(false, Some("B".into())), Some(shown));
        assert_eq!(plain_approval_request(false, None), None);
    }

    #[test]
    fn an_approved_action_shows_its_persisted_request_not_a_stale_edit() {
        assert_eq!(shown_request(true, Some("B".into()), "A".into()), "A");
        assert_eq!(plain_approval_request(true, Some("B".into())), None);
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
}
