// SPDX-License-Identifier: MPL-2.0
//! What external agents asked a person to decide about a task, in the task
//! drawer. This is the only place such a request is decided: the control
//! channel can file one and read the answer, never give it.
//!
//! Each open request names the task, the full commit and the intended action,
//! and the decision sent back names that same commit.

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::models::Task;
use crate::services::agent_request_service::{
    decide_agent_request, list_agent_requests, AgentRequestView, RequestState, RequestedOperation,
};

/// How often the drawer asks again: a request arrives from outside the app, and
/// an open one expires or is left behind by the branch.
const POLL_MS: u32 = 3_000;

/// What the person is asked, in words.
pub fn intended_action(operation: RequestedOperation) -> &'static str {
    match operation {
        RequestedOperation::Approve => "approve the changes",
        RequestedOperation::OpenPr => "open a pull request",
    }
}

/// Whether the person may say yes. Approving changes needs them to be shown:
/// the Changes tab must be on exactly the requested commit.
pub fn may_authorize(operation: RequestedOperation, commit: &str, viewed_tip: Option<&str>) -> bool {
    match operation {
        RequestedOperation::Approve => viewed_tip == Some(commit),
        RequestedOperation::OpenPr => true,
    }
}

#[component]
pub fn AgentRequestsPanel(
    task_id: Uuid,
    /// Apply a task record a command returned.
    apply_task: Callback<Task>,
    /// The commit the Changes tab is showing, when it shows one.
    viewed_tip: RwSignal<Option<String>>,
) -> impl IntoView {
    let requests = RwSignal::new(Vec::<AgentRequestView>::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);

    let refresh = move || {
        spawn_local(async move {
            // A failed read keeps the last list: deciding is checked again by
            // the backend, which refuses a request that is no longer pending.
            if let Ok(list) = list_agent_requests(task_id.to_string()).await {
                if requests.try_get_untracked().is_some_and(|old| old != list) {
                    requests.try_set(list);
                }
            }
        });
    };
    refresh();
    let poll = StoredValue::new_local(Some(gloo_timers::callback::Interval::new(POLL_MS, refresh)));
    on_cleanup(move || poll.dispose());

    let decide = move |request: AgentRequestView, authorize: bool| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let answer = decide_agent_request(
                task_id.to_string(),
                request.id.to_string(),
                request.commit,
                authorize,
            )
            .await;
            busy.try_set(false);
            match answer {
                Ok(task) => apply_task.run(task),
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            refresh();
        });
    };

    // Keyed by request, so a click never lands on another request's button.
    let open = move || {
        requests
            .get()
            .into_iter()
            .filter(|r| matches!(r.state, RequestState::Pending | RequestState::Authorized))
            .collect::<Vec<_>>()
    };

    view! {
        <Show when=move || !open().is_empty() || error.with(Option::is_some)>
            <section data-testid="task-drawer-agent-requests" class="rounded-lg border border-amber-500/25 bg-amber-500/[0.06] p-3 space-y-3">
                <h3 class="text-xs font-semibold uppercase tracking-wide text-amber-300">"Agent requests"</h3>
                <For
                    each=open
                    key=|r| (r.id, r.state)
                    children=move |request| {
                        let operation = request.operation;
                        let commit = request.commit.clone();
                        let pending = request.state == RequestState::Pending;
                        let authorizable = Signal::derive({
                            let commit = commit.clone();
                            move || may_authorize(operation, &commit, viewed_tip.get().as_deref())
                        });
                        let (yes, no) = (request.clone(), request.clone());
                        view! {
                            <div data-testid="agent-request" class="space-y-1">
                                <p class="text-sm text-white/80 break-words">
                                    {format!(
                                        "An agent calling itself \u{201c}{}\u{201d} asks to {} for \u{201c}{}\u{201d}.",
                                        request.requested_by,
                                        intended_action(operation),
                                        request.task_title,
                                    )}
                                </p>
                                <p class="text-xs text-white/50">"Commit " <code data-testid="agent-request-commit" class="break-all">{commit}</code></p>
                                {if pending {
                                    view! {
                                        <div class="flex flex-wrap items-center gap-2">
                                            <button
                                                data-testid="agent-request-authorize"
                                                class="px-3 py-1.5 rounded-lg text-sm bg-green-500/15 text-green-300 hover:bg-green-500/25 disabled:opacity-50"
                                                disabled=move || busy.get() || !authorizable.get()
                                                on:click=move |_| decide(yes.clone(), true)
                                            >
                                                {match operation {
                                                    RequestedOperation::Approve => "Approve changes",
                                                    RequestedOperation::OpenPr => "Authorize once",
                                                }}
                                            </button>
                                            <button
                                                data-testid="agent-request-refuse"
                                                class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/70 hover:bg-white/10 disabled:opacity-50"
                                                disabled=move || busy.get()
                                                on:click=move |_| decide(no.clone(), false)
                                            >
                                                "Refuse"
                                            </button>
                                            <Show when=move || !authorizable.get()>
                                                <span class="text-xs text-white/40">"Open the Changes tab on this commit to approve it."</span>
                                            </Show>
                                        </div>
                                    }.into_any()
                                } else {
                                    view! {
                                        <p class="text-xs text-white/50">"Authorized. The agent can open the pull request once, until the authorization expires."</p>
                                    }.into_any()
                                }}
                            </div>
                        }
                    }
                />
                {move || error.get().map(|message| view! {
                    <p role="alert" data-testid="agent-request-error" class="text-sm text-red-300 break-words">{message}</p>
                })}
            </section>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approving_needs_the_changes_of_that_commit_on_screen() {
        let op = RequestedOperation::Approve;
        assert!(may_authorize(op, "abc", Some("abc")));
        assert!(!may_authorize(op, "abc", Some("def")));
        assert!(!may_authorize(op, "abc", None));
        assert!(may_authorize(RequestedOperation::OpenPr, "abc", None));
    }

    /// The backend sends the request flattened beside its derived state.
    #[test]
    fn a_backend_request_view_deserializes() {
        let view: AgentRequestView = serde_json::from_value(serde_json::json!({
            "task_id": Uuid::nil(), "task_title": "T", "state": "pending",
            "id": Uuid::nil(), "operation": "open_pr", "commit": "abc", "arrival": 1,
            "requested_by": "agent", "requested_at": "2026-01-01T00:00:00Z",
            "expires_at": "2026-01-01T00:30:00Z"
        }))
        .expect("the view deserializes");
        assert_eq!(view.state, RequestState::Pending);
        assert_eq!(view.operation, RequestedOperation::OpenPr);
    }
}
