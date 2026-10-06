//! An opt-in Task Drawer surface; all workflow state is read from the backend.
use crate::services::coordination_service::{request, Snapshot};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{json, Value};
use uuid::Uuid;

fn revision(s: &Snapshot) -> u64 {
    s.conversation
        .as_ref()
        .and_then(|c| c["revision"].as_u64())
        .unwrap_or(0)
}
fn round(s: &Snapshot) -> Value {
    s.conversation
        .as_ref()
        .and_then(|c| c["rounds"].as_array())
        .and_then(|r| r.last())
        .cloned()
        .unwrap_or(Value::Null)
}
fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().into()
}

#[component]
pub fn TaskCoordination(task_id: Uuid) -> impl IntoView {
    let open = RwSignal::new(false);
    let snapshot = RwSignal::new(Snapshot::default());
    let loaded = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(String::new());
    let goal = RwSignal::new(String::new());
    let edited = RwSignal::new(String::new());
    let editing_id = RwSignal::new(String::new());
    let fetching = RwSignal::new(false);
    let accept = move |s: Snapshot| {
        let Some(current_revision) = snapshot.try_with_untracked(revision) else { return; };
        if revision(&s) < current_revision {
            return;
        }
        let d = round(&s)["delegation"].clone();
        let id = text(&d, "id");
        if Some(id.clone()) != editing_id.try_get_untracked() {
            editing_id.try_set(id);
            edited.try_set(text(&d, "proposed_request"));
        }
        snapshot.try_set(s);
        loaded.try_set(true);
    };
    let refresh = move || {
        if fetching.try_get_untracked().unwrap_or(true) {
            return;
        }
        fetching.set(true);
        spawn_local(async move {
            match request("get_coordination", json!({"taskId":task_id})).await {
                Ok(s) => accept(s),
                Err(e) => {
                    error.try_set(e);
                }
            }
            fetching.try_set(false);
        });
    };
    let act = Callback::new(move |action: Value| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(String::new());
        let revision = snapshot.with_untracked(revision);
        spawn_local(async move {
            match request(
                "act_on_coordination",
                json!({"taskId":task_id,"revision":revision,"action":action}),
            )
            .await
            {
                Ok(s) => accept(s),
                Err(e) => {
                    error.try_set(e);
                }
            }
            busy.try_set(false);
            refresh();
        });
    });
    // Observation only: timers never advance the workflow or start a Run.
    let ticker =
        StoredValue::new_local(Some(gloo_timers::callback::Interval::new(750, move || {
            if open.get_untracked() {
                refresh();
            }
        })));
    on_cleanup(move || ticker.dispose());
    let current = Memo::new(move |_| snapshot.with(round));
    let status = Memo::new(move |_| text(&current.get()["delegation"], "status"));
    let decision = Memo::new(move |_| text(&current.get(), "decision"));
    let idle = move || loaded.get() && !busy.get() && !snapshot.with(|s| s.live);
    view! {
        <section class="rounded-lg border border-white/10 p-3 space-y-3" data-testid="coordination">
            <button data-testid="coordination-open" on:click=move |_| { open.update(|o| *o = !*o); if open.get_untracked() { refresh(); } }>
                "Task coordination"
            </button>
            <Show when=move || open.get()>
                <p class="text-xs text-white/60">"One Coordinator proposes; you approve the exact Worker request. Work stays in this Task Checkout."
                </p>
                <p role="alert" class="text-red-300">{move || error.get()}</p>
                <p data-testid="coordination-live">{move || if snapshot.with(|s| s.live) { "Agent running" } else { "No live agent" }}</p>
                <Show when=move || snapshot.with(|s| s.live)>
                    <button data-testid="coordination-stop" on:click=move |_| {
                        spawn_local(async move {
                            if let Err(e) = crate::services::task_run_service::stop_task_execution(task_id.to_string()).await { error.try_set(e); }
                            refresh();
                        });
                    }>"Stop agent"</button>
                </Show>
                <Show when=move || current.get().is_null() || matches!(decision.get().as_str(), "Continue" | "Redirect")>
                    <label>"Human goal"
                        <textarea data-testid="coordination-goal" class="w-full bg-white/5 p-2" prop:value=move || goal.get()
                            on:input=move |e| goal.set(event_target_value(&e)) />
                    </label>
                    <button data-testid="coordination-submit" disabled=move || !idle() on:click=move |_| act.run(json!({"kind":"goal","goal":goal.get_untracked()}))>"Ask Coordinator"</button>
                </Show>
                <p class="whitespace-pre-wrap">{move || text(&current.get(), "goal")}</p>
                <p data-testid="coordination-explanation" class="whitespace-pre-wrap">{move || text(&current.get()["delegation"], "explanation")}</p>
                <p>"Delegation: "<span data-testid="coordination-status">{move || status.get()}</span></p>
                <pre data-testid="coordination-proposed" class="whitespace-pre-wrap">{move || text(&current.get()["delegation"], "proposed_request")}</pre>
                <Show when=move || status.get() == "Proposed">
                    <label>"Worker request (editable)"
                        <textarea data-testid="coordination-edit" class="w-full bg-white/5 p-2" prop:value=move || edited.get()
                            on:input=move |e| edited.set(event_target_value(&e)) />
                    </label>
                    <div class="flex gap-3">
                        <button data-testid="coordination-approve" disabled=move || !idle() on:click=move |_| act.run(json!({"kind":"approve","edited_request":null}))>"Approve exact proposal"</button>
                        <button data-testid="coordination-edit-approve" disabled=move || !idle() on:click=move |_| act.run(json!({"kind":"approve","edited_request":edited.get_untracked()}))>"Edit + approve"</button>
                        <button data-testid="coordination-reject" disabled=move || !idle() on:click=move |_| act.run(json!({"kind":"reject"}))>"Reject"</button>
                    </div>
                </Show>
                <pre data-testid="coordination-approved" class="whitespace-pre-wrap">{move || text(&current.get()["delegation"], "approved_request")}</pre>
                <pre data-testid="coordination-result" class="whitespace-pre-wrap">{move || text(&current.get()["delegation"], "result")}</pre>
                <pre data-testid="coordination-summary" class="whitespace-pre-wrap">{move || text(&current.get(), "summary")}</pre>
                <Show when=move || !current.get().is_null() && decision.get().is_empty() && status.get() != "Proposed" && text(&current.get(), "summary").is_empty() && idle()>
                    <p>"The previous attempt may have changed files. Inspect the Task Checkout before retrying. A fresh Run will use the saved approved request or returned result."</p>
                    <button data-testid="coordination-resume" on:click=move |_| act.run(json!({"kind":"resume"}))>"I inspected the checkout — continue fresh"</button>
                </Show>
                <Show when=move || (!text(&current.get(), "summary").is_empty() && decision.get().is_empty()) || status.get() == "Rejected">
                    <div class="flex gap-3">
                        <button data-testid="coordination-continue" disabled=move || !idle() on:click=move |_| act.run(json!({"kind":"decide","decision":"Continue"}))>"Continue"</button>
                        <button disabled=move || !idle() on:click=move |_| act.run(json!({"kind":"decide","decision":"Redirect"}))>"Redirect"</button>
                        <button disabled=move || !idle() on:click=move |_| act.run(json!({"kind":"decide","decision":"Reject"}))>"Reject result"</button>
                        <button data-testid="coordination-finish" disabled=move || !idle() on:click=move |_| act.run(json!({"kind":"decide","decision":"Finish"}))>"Approve / finish round"</button>
                    </div>
                    <p class="text-xs text-white/60">"This decision closes the coordination round. Task review and delivery remain separate."</p>
                </Show>
                <p data-testid="coordination-decision">{move || decision.get()}</p>
                <details><summary>"Saved rounds, decisions and projected context"</summary>
                    <pre class="whitespace-pre-wrap text-xs">{move || snapshot.with(|s| serde_json::to_string_pretty(&s.conversation).unwrap_or_default())}</pre>
                </details>
            </Show>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn latest_round_and_revision_come_only_from_backend() {
        let s = Snapshot {
            live: false,
            conversation: Some(json!({"revision":9,"rounds":[{"goal":"old"},{"goal":"new"}]})),
        };
        assert_eq!(revision(&s), 9);
        assert_eq!(text(&round(&s), "goal"), "new");
        assert!(round(&Snapshot::default()).is_null());
    }
}
