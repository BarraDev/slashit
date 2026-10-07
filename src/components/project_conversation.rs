use crate::models::{ConversationActionStatus, ConversationEntryKind, ConversationHumanAction, ConversationRole, ConversationSnapshot};
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
            match conversation_service::get_project_conversation(id).await {
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
        set_busy.set(true);
        set_error.set(None);
        let id = retry_project_id.clone();
        let request = retry_begin_request.run(());
        let publish = retry_publish.clone();
        spawn_local(async move {
            match conversation_service::get_project_conversation(id).await {
                Ok(value) => publish.run((request, value)),
                Err(error) => set_error.set(Some(error)),
            }
            set_busy.set(false);
        });
    });
    view! {
            <section data-testid="project-conversation" class="mb-6 rounded-xl border border-white/10 bg-zinc-900/70 p-4">
                <header class="mb-3 flex items-center justify-between">
                    <div><h2 class="text-lg font-semibold text-white">"Project Conversation"</h2><p class="text-xs text-white/50">"Talk with the Project Coordinator. Task work always asks for approval."</p></div>
                    <Show when=move || snapshot.get().is_some_and(|s| s.coordinator_live || s.worker_live)>
                        <button data-testid="conversation-stop" class="rounded-md border border-red-400/40 px-3 py-1 text-sm text-red-200" on:click=move |_| { let id = stop_id.get_value(); spawn_local(async move { if let Err(error) = conversation_service::stop_project_conversation(id).await { set_error.set(Some(error)); } }); }>"Stop run"</button>
                    </Show>
                </header>
                <div class="mb-3 max-h-72 space-y-2 overflow-y-auto" data-testid="conversation-history">
                    {move || snapshot.get().map(|value| {
                        let actions = value.conversation.actions;
                        value.conversation.entries.into_iter().filter_map(|entry| {
                            let (testid, text) = match entry.kind {
                                ConversationEntryKind::HumanMessage { text } | ConversationEntryKind::CoordinatorReply { text } => ("conversation-message", Some(text)),
                                ConversationEntryKind::ActionProposed { action_id } => ("conversation-action-event", actions.iter().find(|action| action.id == action_id).map(|action| format!("Proposed work for {} (Task {}): {}", action.target_task_title, action.target_task_id, action.request))),
                                ConversationEntryKind::ActionDecision { approved, request, .. } => ("conversation-action-event", Some(if approved { format!("You approved the Worker request: {}", request.unwrap_or_default()) } else { "You rejected the proposed Task work.".to_string() })),
                                ConversationEntryKind::WorkerStarted { action_id } => ("conversation-action-event", actions.iter().find(|action| action.id == action_id).map(|action| format!("Worker started in Task Checkout: {}", action.target_task_title))),
                                ConversationEntryKind::WorkerResult { result, .. } => ("conversation-worker-result", Some(result)),
                                ConversationEntryKind::RunFailed { message, .. } => ("conversation-run-failed", Some(message)),
                            };
                            let role = match entry.role { ConversationRole::Human => "You", ConversationRole::Coordinator => "Coordinator", ConversationRole::Worker => "Worker" };
                            text.map(|text| view! { <article data-testid=testid class="rounded-lg bg-black/25 px-3 py-2"><div class="mb-1 text-xs font-medium text-white/50">{role}</div><div class="whitespace-pre-wrap text-sm text-white/90">{text}</div></article> })
                        }).collect_view()
                    })}
                </div>
                <div class="mb-3 space-y-2">
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
                                <label class="mt-2 block text-xs text-white/50">"Worker request"<textarea data-testid="conversation-action-request" class="mt-1 min-h-20 w-full rounded bg-black/30 p-2 text-sm text-white" prop:value=move || edits.with(|edits| edits.get(&value_id).cloned().unwrap_or_else(|| current_request.get())) on:input=move |event| set_edits.update(|edits| { edits.insert(input_id.clone(), event_target_value(&event)); })></textarea></label>
                                <div class="mt-2 flex gap-2">
                                    <button data-testid="conversation-action-approve" class="rounded bg-emerald-700 px-3 py-1 text-sm" disabled=move || busy.get() on:click=move |_| approve_action.run((approve_id.clone(), None))>{move || if is_approved.get() { "Start approved Worker" } else { "Approve exact request" }}</button>
                                    <button data-testid="conversation-action-edit-approve" class="rounded bg-emerald-700/70 px-3 py-1 text-sm" disabled=move || busy.get() || is_approved.get() on:click=move |_| { let edited = edits.get_untracked().get(&id).cloned().unwrap_or(original.clone()); edit_approve_action.run((edit_approve_id.clone(), Some(edited))); } >"Edit + approve"</button>
                                    <button data-testid="conversation-action-reject" class="rounded border border-white/20 px-3 py-1 text-sm" disabled=move || busy.get() || is_approved.get() on:click=move |_| reject_action.run(reject_id.clone())>"Reject"</button>
                                </div>
                            </article>
                        }
                        }
                    />
                </div>
                {move || snapshot.get().map(|value| {
                    let status = if value.worker_live { "Worker is running in the Task Checkout" } else if value.coordinator_live { "Coordinator is responding" } else { "Conversation is ready" };
                    view! { <div data-testid="conversation-run-state" class="mb-2 text-xs text-white/50">{status}</div> }
                })}
                {move || snapshot.get().and_then(|value| value.continuation_error.map(|message| {
                    let retry = retry_coordinator.clone();
                    view! {
                        <div data-testid="conversation-continuation-error" class="mb-3 rounded border border-amber-300/30 bg-amber-400/5 p-3 text-sm text-amber-100">
                            <p>{message}</p>
                            <button data-testid="conversation-retry-coordinator" class="mt-2 rounded border border-amber-200/30 px-3 py-1" disabled=move || busy.get() on:click=move |_| retry.run(())>"Retry Coordinator response"</button>
                        </div>
                    }
                }))}
                {move || snapshot.get().is_some_and(|value| has_unmediated_worker_result(&value)).then(|| view! {
                    <p data-testid="conversation-awaiting-mediation" class="mb-2 text-xs text-amber-100/80">"A saved Worker result must be reviewed before you send another message."</p>
                })}
                <form class="flex items-end gap-2" on:submit=move |event: web_sys::SubmitEvent| { event.prevent_default(); on_send(); }>
                    <textarea data-testid="conversation-message-input" class="min-h-16 flex-1 rounded-lg border border-white/10 bg-black/30 p-3 text-sm text-white" placeholder="Ask about this Project…" prop:value=draft disabled=move || snapshot.get().is_some_and(|value| has_unmediated_worker_result(&value)) on:input=move |event| set_draft.set(event_target_value(&event))></textarea>
                    <button data-testid="conversation-send" type="submit" class="rounded-lg bg-indigo-600 px-4 py-2 text-sm font-medium" disabled=move || busy.get() || draft.get().trim().is_empty() || snapshot.get().is_some_and(|value| has_unmediated_worker_result(&value))>"Send"</button>
                </form>
                {move || error.get().map(|message| view! { <p data-testid="conversation-error" class="mt-2 text-sm text-red-300">{message}</p> })}
            </section>
    }
}

#[cfg(test)]
mod tests {
    use super::{should_poll_snapshot, should_publish_snapshot};
    use uuid::Uuid;

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
}
