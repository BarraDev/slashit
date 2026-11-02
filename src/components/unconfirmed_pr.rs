// SPDX-License-Identifier: MPL-2.0
//! Pull request recovery: what a person can do about a request to open a
//! pull request whose outcome SlashIt never learned.
//!
//! Three states are told apart and never worded alike. *Unknown*: SlashIt
//! asked and never learned, so a pull request may exist. *Verified*: the
//! person linked a pull request and SlashIt checked it is this branch's own.
//! *Resolved by you*: the person confirmed that none exists; that is their
//! word, GitHub did not confirm it, and nothing was sent.
//!
//! The panel only asks and shows. The backend binds every answer to the exact
//! record shown here, refuses a stale one, checks a pull request before it
//! links it, and never pushes or creates anything. The rules the panel relies
//! on live in [`super::unconfirmed_pr_state`].

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use super::unconfirmed_pr_state::{
    answer_task, can_discard, found_url, looks_like_pull_request_url, outcome_note, recovery_state,
    still_current, Note, RecoveryState, Tone,
};
use crate::models::{
    Task, UnconfirmedOutcome, UnconfirmedPrCreate, UnconfirmedResolution,
};
use crate::services::pr_service::{get_unconfirmed_pr_repository, resolve_unconfirmed_pr_create};

/// Shown when the record changed while the confirmation was open.
const CHANGED_NOTICE: &str =
    "The unanswered request changed while you were confirming, so nothing was discarded. Review \
     it as it is now.";

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// What a person is doing in the panel right now.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    Idle,
    /// Typing the URL of the pull request they found.
    Linking,
    /// Confirming that none exists, for the attempt they were shown. Frozen
    /// when it opened: the answer names this record and no other.
    Discarding(UnconfirmedPrCreate),
}

fn tone_class(tone: Tone) -> &'static str {
    match tone {
        Tone::Good => "text-emerald-300",
        Tone::Warn => "text-amber-200",
        Tone::Info => "text-white/70",
    }
}

/// The recovery section for a task whose request to open its pull request was
/// never answered, and what became of it. Renders nothing for any other task.
#[component]
pub fn UnconfirmedPrPanel(
    task_id: Uuid,
    #[prop(into)] task: Signal<Option<Task>>,
    /// Apply a task record a command returned.
    apply_task: Callback<Task>,
) -> impl IntoView {
    let last = RwSignal::new(None::<UnconfirmedOutcome>);
    let note = RwSignal::new(None::<Note>);
    let mode = RwSignal::new(Mode::Idle);
    let url = RwSignal::new(String::new());
    let acknowledged = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let repository = RwSignal::new(None::<String>);

    let state = Memo::new(move |_| {
        task.with(|t| {
            t.as_ref()
                .and_then(|t| last.with(|l| recovery_state(t, l.as_ref())))
        })
    });
    let live_attempt = move || task.with(|t| t.as_ref().and_then(|t| t.pr_create_unconfirmed.clone()));

    // The repository is read once an unanswered request is shown.
    let asked_repository = StoredValue::new(false);
    Effect::new(move |_| {
        if matches!(state.get(), Some(RecoveryState::Unknown(_)))
            && !asked_repository.get_value()
        {
            asked_repository.set_value(true);
            spawn_local(async move {
                if let Ok(name) = get_unconfirmed_pr_repository(task_id.to_string()).await {
                    repository.try_set(name);
                }
            });
        }
    });

    // A confirmation that no longer describes the task's record closes.
    Effect::new(move |_| {
        let live = live_attempt();
        if busy.get_untracked() {
            return;
        }
        if let Mode::Discarding(shown) = mode.get_untracked() {
            if !still_current(&shown, live.as_ref()) {
                mode.set(Mode::Idle);
                acknowledged.set(false);
                note.set(Some(Note { tone: Tone::Warn, text: CHANGED_NOTICE.to_string() }));
            }
        }
    });

    let send = move |attempt: UnconfirmedPrCreate, resolution: UnconfirmedResolution| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let answer = resolve_unconfirmed_pr_create(task_id.to_string(), attempt, resolution).await;
            busy.try_set(false);
            match answer {
                Ok(answer) => {
                    if let Some(task) = answer_task(&answer) {
                        apply_task.try_run(task.clone());
                    }
                    note.try_set(Some(outcome_note(&answer.result)));
                    // A pull request GitHub listed is offered for linking.
                    match found_url(&answer.result) {
                        Some(found) => {
                            url.try_set(found.to_string());
                            mode.try_set(Mode::Linking);
                        }
                        None => {
                            mode.try_set(Mode::Idle);
                            url.try_set(String::new());
                        }
                    }
                    acknowledged.try_set(false);
                    last.try_set(Some(answer.result));
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
        });
    };

    view! {
        {move || state.get().map(|st| {
            let (kind, body) = match st {
                RecoveryState::Unknown(attempt) => ("unknown", unknown_view(attempt, repository, mode, url, acknowledged, busy, send)),
                RecoveryState::Verified { url } => ("verified", view! {
                    <p data-testid="unconfirmed-pr-verified" class="text-xs text-emerald-300">
                        {format!("The pull request {url} is linked. SlashIt checked that it is this branch's own, in this repository.")}
                    </p>
                }.into_any()),
                RecoveryState::ResolvedByPerson(attempt) => ("resolved_by_person", view! {
                    <p data-testid="unconfirmed-pr-resolved" class="text-xs text-white/70">
                        {format!(
                            "You confirmed that no pull request exists for {}, and the unanswered request made at {} was discarded. This was your decision; GitHub did not confirm it. Nothing was sent to GitHub.",
                            attempt.branch,
                            short(&attempt.commit),
                        )}
                    </p>
                }.into_any()),
            };
            view! {
                <section data-testid="unconfirmed-pr" data-state=kind class="rounded-lg border border-amber-500/20 bg-amber-500/[0.05] p-3 space-y-2">
                    <h3 class="text-xs font-semibold uppercase tracking-wide text-amber-300">"Pull request recovery"</h3>
                    {body}
                    {move || note.get().map(|n| view! {
                        <p data-testid="unconfirmed-pr-note" class=format!("text-xs whitespace-pre-wrap break-words {}", tone_class(n.tone))>{n.text}</p>
                    })}
                    {move || error.get().map(|e| view! {
                        <p data-testid="unconfirmed-pr-error" class="text-xs text-red-300 whitespace-pre-wrap break-words">{e}</p>
                    })}
                </section>
            }
        })}
    }
}

fn unknown_view(
    attempt: UnconfirmedPrCreate,
    repository: RwSignal<Option<String>>,
    mode: RwSignal<Mode>,
    url: RwSignal<String>,
    acknowledged: RwSignal<bool>,
    busy: RwSignal<bool>,
    send: impl Fn(UnconfirmedPrCreate, UnconfirmedResolution) + Copy + Send + Sync + 'static,
) -> AnyView {
    let when = attempt.started_at.format("%Y-%m-%d %H:%M UTC").to_string();
    let for_link = attempt.clone();
    let for_open = attempt.clone();
    view! {
        <div data-testid="unconfirmed-pr-unknown" class="space-y-2">
            <p class="text-xs font-semibold text-amber-200">"Pull request status unknown"</p>
            <p class="text-xs text-white/70">
                "SlashIt asked GitHub to open a pull request for this branch and never learned the outcome. A pull request may exist. GitHub not listing one does not prove that none was opened, so SlashIt will not open another until this is settled."
            </p>
            <dl data-testid="unconfirmed-pr-attempt" class="text-xs text-white/60 space-y-0.5">
                <div>"Repository: "<span data-testid="unconfirmed-pr-repository">{move || repository.get().unwrap_or_else(|| "not known".to_string())}</span></div>
                <div>"Branch: "<span data-testid="unconfirmed-pr-branch">{attempt.branch.clone()}</span></div>
                <div>"Commit: "<code data-testid="unconfirmed-pr-commit">{attempt.commit.clone()}</code></div>
                <div>"Requested: "<span data-testid="unconfirmed-pr-started">{when}</span></div>
            </dl>
            <Show when=move || mode.get() == Mode::Idle>
                <div class="flex flex-wrap gap-2">
                    <button
                        type="button"
                        data-testid="unconfirmed-pr-link-start"
                        class="px-3 py-1.5 rounded-lg text-sm bg-blue-500/15 text-blue-200 hover:bg-blue-500/25 disabled:opacity-50"
                        disabled=move || busy.get()
                        on:click=move |_| mode.set(Mode::Linking)
                    >
                        "I found the pull request"
                    </button>
                    <button
                        type="button"
                        data-testid="unconfirmed-pr-discard-start"
                        class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/70 hover:bg-white/10 disabled:opacity-50"
                        disabled=move || busy.get()
                        on:click={
                            let shown = for_open.clone();
                            move |_| {
                                acknowledged.set(false);
                                mode.set(Mode::Discarding(shown.clone()));
                            }
                        }
                    >
                        "No pull request exists..."
                    </button>
                </div>
            </Show>
            <Show when=move || mode.get() == Mode::Linking>
                <div data-testid="unconfirmed-pr-link" class="space-y-2">
                    <p class="text-xs text-white/60">
                        "Paste the pull request's URL. SlashIt links it only after checking that it is this branch's own pull request, in this repository, at the work this request was made for."
                    </p>
                    <input
                        type="text"
                        data-testid="unconfirmed-pr-url"
                        class="w-full rounded-md bg-black/30 border border-white/10 px-2 py-1 text-sm text-white"
                        placeholder="https://github.com/owner/repo/pull/123"
                        prop:value=move || url.get()
                        on:input=move |ev| url.set(event_target_value(&ev))
                    />
                    <div class="flex gap-2">
                        <button
                            type="button"
                            data-testid="unconfirmed-pr-link-confirm"
                            class="px-3 py-1.5 rounded-lg text-sm bg-blue-500/15 text-blue-200 hover:bg-blue-500/25 disabled:opacity-50"
                            disabled=move || busy.get() || !looks_like_pull_request_url(&url.get())
                            on:click={
                                let shown = for_link.clone();
                                move |_| send(shown.clone(), UnconfirmedResolution::LinkExisting { pr_url: url.get_untracked().trim().to_string() })
                            }
                        >
                            "Check and link"
                        </button>
                        <button
                            type="button"
                            class="px-3 py-1.5 rounded-lg text-sm text-white/60 hover:text-white"
                            disabled=move || busy.get()
                            on:click=move |_| mode.set(Mode::Idle)
                        >
                            "Cancel"
                        </button>
                    </div>
                </div>
            </Show>
            <Show when=move || matches!(mode.get(), Mode::Discarding(_))>
                <div data-testid="unconfirmed-pr-discard" class="space-y-2 rounded-md border border-white/15 p-2">
                    <p class="text-xs text-white/80">
                        "Discarding tells SlashIt that no pull request exists for this branch. Only you can know that: check the repository's pull requests on GitHub yourself first. If one exists, creating another later makes a duplicate."
                    </p>
                    <p class="text-xs text-white/60">
                        "This sends nothing to GitHub and creates nothing. Creating the pull request again afterwards goes through the usual checks and approval."
                    </p>
                    <label class="flex items-start gap-2 text-xs text-white/80">
                        <input
                            type="checkbox"
                            data-testid="unconfirmed-pr-ack"
                            prop:checked=move || acknowledged.get()
                            on:change=move |ev| acknowledged.set(event_target_checked(&ev))
                        />
                        <span>"I checked GitHub myself, found no pull request for this branch, and understand a second one may be created."</span>
                    </label>
                    <div class="flex gap-2">
                        <button
                            type="button"
                            data-testid="unconfirmed-pr-discard-confirm"
                            class="px-3 py-1.5 rounded-lg text-sm bg-amber-500/15 text-amber-100 hover:bg-amber-500/25 disabled:opacity-50"
                            disabled=move || !can_discard(acknowledged.get(), busy.get())
                            on:click=move |_| {
                                // The attempt frozen when the confirmation opened, never
                                // the record read now.
                                if let Mode::Discarding(frozen) = mode.get_untracked() {
                                    send(frozen, UnconfirmedResolution::ConfirmNoPullRequest { acknowledged: acknowledged.get_untracked() });
                                }
                            }
                        >
                            "Discard the unanswered request"
                        </button>
                        <button
                            type="button"
                            class="px-3 py-1.5 rounded-lg text-sm text-white/60 hover:text-white"
                            disabled=move || busy.get()
                            on:click=move |_| {
                                mode.set(Mode::Idle);
                                acknowledged.set(false);
                            }
                        >
                            "Cancel"
                        </button>
                    </div>
                </div>
            </Show>
        </div>
    }
    .into_any()
}
