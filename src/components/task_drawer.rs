//! The task drawer: a side panel that answers what a Task is doing, what
//! happened if it failed, and what can be done about it now.
//!
//! One drawer instance serves exactly one task. Switching tasks unmounts it
//! and mounts a fresh one, so nothing it holds (its listener, its run
//! snapshot, a request still in flight) can outlive the task it was opened
//! for or be shown against another.

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::components::diff_viewer::DiffViewer;
use crate::components::task_live::{
    format_elapsed, output_provenance, DrawerActions, OutputProvenance, RefreshGate,
};
use crate::components::toast;
use crate::models::{LogLevel, QaStatus, Task, TaskPhase, TaskRunSnapshot, TaskStatus};
use crate::services::task_run_service::{
    get_task_run, listen_agent_events, requeue_task, stop_task_execution,
};
use crate::services::{get_task_diff, get_task_diff_stat};

/// The most output entries the drawer renders. The backend keeps more; this
/// is "recent output", not a transcript.
const VISIBLE_OUTPUT: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Output,
    Changes,
}

#[component]
pub fn TaskDrawer(
    task_id: Uuid,
    tasks: ReadSignal<Vec<Task>>,
    /// The agent's current one-line activity for this task, if any.
    #[prop(into)]
    activity: Signal<Option<String>>,
    on_close: Callback<()>,
    on_edit: Callback<Task>,
    /// Apply a task record a command returned, superseding older reads.
    apply_task: Callback<Task>,
    /// Read the task list again, for a change no event announces.
    refresh_tasks: Callback<()>,
) -> impl IntoView {
    let task = Memo::new(move |_| tasks.with(|all| all.iter().find(|t| t.id == task_id).cloned()));
    let status = Memo::new(move |_| task.with(|t| t.as_ref().map(|t| t.status.clone())));

    let run = RwSignal::new(TaskRunSnapshot::default());
    let run_loaded = RwSignal::new(false);
    let run_error = RwSignal::new(None::<String>);
    let gate = StoredValue::new(RefreshGate::default());

    // Re-read the run snapshot, coalescing requests that arrive while one is
    // in flight. Every write goes through `try_*`: a response that lands
    // after the drawer closed has nowhere to go and is dropped.
    let refresh_run = move || {
        if !gate.try_update_value(|g| g.request()).unwrap_or(false) {
            return;
        }
        spawn_local(async move {
            loop {
                match get_task_run(task_id.to_string()).await {
                    Ok(snapshot) => {
                        if run.try_set(snapshot).is_some() {
                            return;
                        }
                        run_loaded.try_set(true);
                        run_error.try_set(None);
                    }
                    Err(e) => {
                        if run_error.try_set(Some(e)).is_some() {
                            return;
                        }
                    }
                }
                if !gate.try_update_value(|g| g.finish()).unwrap_or(false) {
                    return;
                }
            }
        });
    };

    // This drawer's own live subscription: any event for this task means its
    // run changed, so read it again. Events for other tasks are ignored here
    // and never reach this drawer's state.
    let task_key = task_id.to_string();
    let listener = StoredValue::new_local(Some(listen_agent_events(move |event| {
        if event.task_id() == task_key {
            refresh_run();
        }
    })));
    on_cleanup(move || listener.dispose());

    // The record changing (a poll, a drag, a stop from elsewhere) can change
    // what is live without an event for this task; read the run again.
    Effect::new(move |_| {
        status.track();
        refresh_run();
    });

    // A clock for the elapsed time, owned by the drawer.
    let now = RwSignal::new(chrono::Utc::now());
    let ticker = StoredValue::new_local(Some(gloo_timers::callback::Interval::new(1_000, move || {
        now.try_set(chrono::Utc::now());
    })));
    on_cleanup(move || ticker.dispose());

    let actions = Memo::new(move |_| {
        status
            .get()
            .map(|s| run.with(|r| DrawerActions::for_task(&s, r)))
            .unwrap_or_default()
    });

    let stopping = RwSignal::new(false);
    let retrying = RwSignal::new(false);
    // The failure a retry was asked about, kept visible as the last attempt's
    // result until the next run changes the task.
    let previous_error = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        if status.get() != Some(TaskStatus::Queue) {
            previous_error.set(None);
        }
    });

    let on_stop = move |_| {
        if stopping.get_untracked() {
            return;
        }
        stopping.set(true);
        spawn_local(async move {
            // The backend ends the agent and settles the task before it
            // answers; what the task became is read back, never assumed.
            // A stop settles the task without an event of its own, so the
            // board is asked to read it again.
            if let Err(e) = stop_task_execution(task_id.to_string()).await {
                toast::error(format!("Could not stop the task: {e}"));
            }
            // The board may have gone while the stop was running.
            refresh_tasks.try_run(());
            stopping.try_set(false);
            refresh_run();
        });
    };

    let on_retry = move |_| {
        if retrying.get_untracked() {
            return;
        }
        let failure = task.with_untracked(|t| t.as_ref().and_then(|t| t.error_message.clone()));
        retrying.set(true);
        spawn_local(async move {
            match requeue_task(task_id.to_string()).await {
                Ok(Some(updated)) => {
                    apply_task.try_run(updated);
                    previous_error.try_set(failure);
                }
                Ok(None) => toast::error("This task no longer exists".to_string()),
                Err(e) => toast::error(format!("Could not retry the task: {e}")),
            }
            retrying.try_set(false);
            refresh_run();
        });
    };

    let tab = RwSignal::new(Tab::Output);
    // Loaded when the Changes tab is opened, and again if the task moves on
    // while it is open (an AI review fix changes the code).
    let changes = RwSignal::new(None::<Result<(String, String), String>>);
    let load_changes = move || {
        spawn_local(async move {
            let loaded = match get_task_diff(task_id.to_string()).await {
                Ok(diff) => {
                    let stat = get_task_diff_stat(task_id.to_string()).await.unwrap_or_default();
                    Ok((diff, stat))
                }
                Err(e) => Err(e),
            };
            changes.try_set(Some(loaded));
        });
    };
    let open_changes = move |_| {
        tab.set(Tab::Changes);
        if changes.get_untracked().is_none() {
            load_changes();
        }
    };
    Effect::new(move |previous: Option<Option<TaskStatus>>| {
        let current = status.get();
        if previous.is_some_and(|p| p != current) {
            changes.set(None);
            if tab.get_untracked() == Tab::Changes {
                load_changes();
            }
        }
        current
    });
    // A task that no longer has changes to show falls back to its output.
    Effect::new(move |_| {
        if !actions.get().changes && tab.get_untracked() == Tab::Changes {
            tab.set(Tab::Output);
        }
    });

    // Follow new output only while the reader is already at the bottom, so
    // scrolling back to read earlier output is not undone by the next line.
    let output_ref = NodeRef::<leptos::html::Div>::new();
    let follow_output = StoredValue::new(true);
    Effect::new(move |_| {
        run.track();
        if let Some(el) = output_ref.get() {
            if follow_output.get_value() {
                el.set_scroll_top(el.scroll_height());
            }
        }
    });
    let on_output_scroll = move |_| {
        if let Some(el) = output_ref.get_untracked() {
            let at_bottom = el.scroll_top() + el.client_height() >= el.scroll_height() - 24;
            follow_output.set_value(at_bottom);
        }
    };

    let close = move |_| on_close.run(());

    // Focus the drawer when it opens, so Escape closes it without a click.
    let drawer_ref = NodeRef::<leptos::html::Aside>::new();
    Effect::new(move |_| {
        if let Some(el) = drawer_ref.get() {
            let _ = el.focus();
        }
    });
    let on_keydown = move |e: leptos::ev::KeyboardEvent| {
        if e.key() == "Escape" {
            on_close.run(());
        }
    };

    view! {
        <div class="fixed inset-0 z-40 flex justify-end" data-testid="task-drawer-overlay">
            <div class="absolute inset-0 bg-black/40" on:click=close></div>
            <aside
                node_ref=drawer_ref
                data-testid="task-drawer"
                data-task-id=task_id.to_string()
                role="dialog"
                aria-modal="true"
                aria-label="Task details"
                tabindex="-1"
                on:keydown=on_keydown
                class="relative h-full w-full max-w-[520px] bg-[#101018] border-l border-white/10 shadow-2xl flex flex-col outline-none"
            >
                {move || match task.get() {
                    None => view! {
                        <div class="p-6 text-sm text-white/50">"This task no longer exists."</div>
                    }.into_any(),
                    Some(t) => view! {
                        <DrawerHeader task=t.clone() on_close=on_close />
                    }.into_any(),
                }}

                <div class="flex-1 min-h-0 overflow-y-auto px-5 py-4 space-y-4">
                    // Where the task is and what it is doing.
                    {move || task.get().map(|t| {
                        let live = run.with(|r| r.live);
                        let started = run.with(|r| {
                            r.last_execution.as_ref().filter(|_| output_provenance(r) == OutputProvenance::Live).map(|e| e.started_at)
                        });
                        view! {
                            <section class="space-y-2">
                                <div class="flex items-center gap-3 text-xs text-white/60">
                                    <span data-testid="task-drawer-phase">{phase_label(&t.phase)}</span>
                                    <div class="flex-1 h-1.5 bg-white/[0.06] rounded-full overflow-hidden">
                                        <div class="h-full bg-blue-500 rounded-full transition-all duration-500" style=format!("width: {}%", t.overall_progress)></div>
                                    </div>
                                    <span data-testid="task-drawer-progress">{format!("{}%", t.overall_progress)}</span>
                                    {started.map(|started| view! {
                                        <span data-testid="task-drawer-elapsed" class="tabular-nums">
                                            {move || format_elapsed((now.get() - started).num_seconds())}
                                        </span>
                                    })}
                                </div>
                                {live.then(|| view! {
                                    <div data-testid="task-drawer-activity" class="text-sm text-blue-200/90 truncate">
                                        {move || activity.get().unwrap_or_else(|| "Working…".to_string())}
                                    </div>
                                })}
                            </section>
                        }
                    })}

                    // What can be done now.
                    <section class="flex flex-wrap gap-2">
                        <Show when=move || actions.get().stop>
                            <button
                                data-testid="task-drawer-stop"
                                class="px-3 py-1.5 rounded-lg text-sm bg-red-500/15 text-red-300 hover:bg-red-500/25 disabled:opacity-50"
                                disabled=move || stopping.get()
                                on:click=on_stop
                            >
                                {move || if stopping.get() { "Stopping…" } else { "Stop" }}
                            </button>
                        </Show>
                        <Show when=move || actions.get().retry>
                            <button
                                data-testid="task-drawer-retry"
                                class="px-3 py-1.5 rounded-lg text-sm bg-blue-500/15 text-blue-300 hover:bg-blue-500/25 disabled:opacity-50"
                                disabled=move || retrying.get()
                                on:click=on_retry
                            >
                                {move || if retrying.get() { "Retrying…" } else { "Retry" }}
                            </button>
                        </Show>
                        <Show when=move || actions.get().edit>
                            <button
                                data-testid="task-drawer-edit"
                                class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/70 hover:bg-white/10"
                                on:click=move |_| {
                                    if let Some(t) = task.get_untracked() {
                                        on_edit.run(t);
                                    }
                                }
                            >
                                "Edit"
                            </button>
                        </Show>
                    </section>

                    // What happened, if it failed.
                    {move || task.get().filter(|t| t.status == TaskStatus::Error).map(|t| view! {
                        <section data-testid="task-drawer-failure" class="rounded-lg border border-red-500/20 bg-red-500/[0.06] p-3 space-y-2">
                            <h3 class="text-xs font-semibold uppercase tracking-wide text-red-300">"What went wrong"</h3>
                            <p data-testid="task-drawer-error" class="text-sm text-red-200 whitespace-pre-wrap break-words">
                                {t.error_message.clone().unwrap_or_else(|| "The run failed without recording a reason.".to_string())}
                            </p>
                            <p class="text-xs text-white/40">
                                "This is the result of the last attempt. Retry puts the task back in the queue, and the next run continues from the work already done."
                            </p>
                        </section>
                    })}
                    {move || previous_error.get().filter(|_| !run.with(|r| r.live)).map(|message| view! {
                        <section data-testid="task-drawer-previous-error" class="rounded-lg border border-white/10 bg-white/[0.03] p-3 space-y-2">
                            <h3 class="text-xs font-semibold uppercase tracking-wide text-white/50">"Last attempt failed"</h3>
                            <p class="text-sm text-white/60 whitespace-pre-wrap break-words">{message}</p>
                            <p class="text-xs text-white/40">"Queued to run again. This stays until the next run starts."</p>
                        </section>
                    })}

                    {move || task.get().map(|t| view! { <TaskSummary task=t /> })}

                    // Output and, where there are any, changes.
                    <section class="space-y-2">
                        <div role="tablist" class="flex items-center gap-1 border-b border-white/10">
                            <TabButton label="Output" testid="task-drawer-tab-output" active=Signal::derive(move || tab.get() == Tab::Output) on_click=Callback::new(move |_| tab.set(Tab::Output)) />
                            <Show when=move || actions.get().changes>
                                <TabButton label="Changes" testid="task-drawer-tab-changes" active=Signal::derive(move || tab.get() == Tab::Changes) on_click=Callback::new(open_changes) />
                            </Show>
                        </div>

                        <Show when=move || tab.get() == Tab::Output>
                            <p data-testid="task-drawer-output-provenance" class="text-xs text-white/40">
                                {move || {
                                    let queued = status.get() == Some(TaskStatus::Queue);
                                    if let Some(e) = run_error.get() {
                                        return format!("Could not read this task's run: {e}");
                                    }
                                    match run.with(output_provenance) {
                                        OutputProvenance::Live => "Live output from the running agent.",
                                        OutputProvenance::LastAttempt if queued => "Output from the last attempt. The next run replaces it.",
                                        OutputProvenance::LastAttempt => "Output from the last run.",
                                        OutputProvenance::None if !run_loaded.get() => "Loading…",
                                        OutputProvenance::None => "No output from this task since SlashIt started.",
                                    }
                                    .to_string()
                                }}
                            </p>
                            <div
                                node_ref=output_ref
                                on:scroll=on_output_scroll
                                data-testid="task-drawer-output"
                                class="max-h-[45vh] overflow-y-auto rounded-lg bg-black/40 p-3 font-mono text-xs space-y-1"
                            >
                                {move || run.with(|r| {
                                    let output = r.last_execution.as_ref().map(|e| e.output.as_slice()).unwrap_or(&[]);
                                    let skip = output.len().saturating_sub(VISIBLE_OUTPUT);
                                    output[skip..].iter().map(|entry| {
                                        let class = match entry.level {
                                            LogLevel::Error => "text-red-300",
                                            LogLevel::Warn => "text-yellow-300",
                                            _ => "text-white/70",
                                        };
                                        view! {
                                            <div data-testid="task-drawer-output-line" class=format!("whitespace-pre-wrap break-words {class}")>
                                                {entry.message.clone()}
                                            </div>
                                        }
                                    }).collect_view()
                                })}
                            </div>
                        </Show>

                        <Show when=move || tab.get() == Tab::Changes>
                            <div data-testid="task-drawer-changes">
                                {move || match changes.get() {
                                    None => view! { <p class="text-xs text-white/40">"Loading changes…"</p> }.into_any(),
                                    Some(Err(e)) => view! { <p class="text-xs text-red-300">{format!("Could not load the changes: {e}")}</p> }.into_any(),
                                    Some(Ok((diff, stat))) => view! { <DiffViewer diff=diff stat=stat /> }.into_any(),
                                }}
                            </div>
                        </Show>
                    </section>
                </div>
            </aside>
        </div>
    }
}

#[component]
fn DrawerHeader(task: Task, on_close: Callback<()>) -> impl IntoView {
    let (label, class) = status_badge(&task.status);
    let status_key = format!("{:?}", task.status).to_lowercase();
    view! {
        <header class="flex items-start gap-3 px-5 py-4 border-b border-white/10">
            <div class="flex-1 min-w-0">
                <h2 data-testid="task-drawer-title" class="text-base font-semibold text-white/90 break-words">{task.title.clone()}</h2>
                <span
                    data-testid="task-drawer-status"
                    data-status=status_key
                    class=format!("inline-block mt-1.5 px-2 py-0.5 rounded-md text-[11px] font-medium {class}")
                >
                    {label}
                </span>
            </div>
            <button
                data-testid="task-drawer-close"
                class="p-1.5 rounded-md text-white/50 hover:text-white/90 hover:bg-white/10"
                aria-label="Close"
                title="Close"
                on:click=move |_| on_close.run(())
            >
                <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                </svg>
            </button>
        </header>
    }
}

/// What a person needs to know about the task, without its version-control
/// plumbing.
#[component]
fn TaskSummary(task: Task) -> impl IntoView {
    let review = task.qa_signoff.as_ref().map(|qa| {
        let verdict = match qa.status {
            QaStatus::Approved => "AI review approved the changes.",
            QaStatus::FixesApplied => "AI review applied fixes to the changes.",
            QaStatus::Rejected => "AI review rejected the changes.",
        };
        (verdict, qa.issues_found.clone())
    });
    let awaiting_review = task.status == TaskStatus::HumanReview;
    view! {
        <section class="space-y-2 text-sm">
            {task.description.clone().filter(|d| !d.trim().is_empty()).map(|d| view! {
                <p data-testid="task-drawer-description" class="text-white/60 whitespace-pre-wrap break-words line-clamp-6">{d}</p>
            })}
            {review.map(|(verdict, issues)| view! {
                <div data-testid="task-drawer-review" class="text-white/60 space-y-1">
                    <p>{verdict}</p>
                    {(!issues.is_empty()).then(|| view! {
                        <ul class="list-disc pl-5 text-xs text-white/50 space-y-0.5">
                            {issues.into_iter().take(5).map(|issue| view! { <li>{issue}</li> }).collect_view()}
                        </ul>
                    })}
                </div>
            })}
            {awaiting_review.then(|| view! {
                <p class="text-xs text-purple-300/80">"Waiting for your review. The changes are in the Changes tab."</p>
            })}
        </section>
    }
}

#[component]
fn TabButton(
    label: &'static str,
    testid: &'static str,
    active: Signal<bool>,
    on_click: Callback<leptos::ev::MouseEvent>,
) -> impl IntoView {
    view! {
        <button
            data-testid=testid
            role="tab"
            aria-selected=move || if active.get() { "true" } else { "false" }
            class=move || format!(
                "px-3 py-1.5 text-xs font-medium border-b-2 -mb-px transition-colors {}",
                if active.get() { "border-blue-400 text-white/90" } else { "border-transparent text-white/40 hover:text-white/70" }
            )
            on:click=move |e| on_click.run(e)
        >
            {label}
        </button>
    }
}

fn status_badge(status: &TaskStatus) -> (&'static str, &'static str) {
    match status {
        TaskStatus::Backlog => ("Backlog", "bg-white/10 text-white/60"),
        TaskStatus::Queue => ("Queued", "bg-slate-500/20 text-slate-300"),
        TaskStatus::InProgress => ("Running", "bg-blue-500/20 text-blue-300"),
        TaskStatus::AiReview => ("AI Review", "bg-purple-500/20 text-purple-300"),
        TaskStatus::HumanReview => ("Human Review", "bg-purple-500/20 text-purple-300"),
        TaskStatus::Done => ("Done", "bg-emerald-500/20 text-emerald-300"),
        TaskStatus::PrCreated => ("PR Created", "bg-emerald-500/20 text-emerald-300"),
        TaskStatus::Error => ("Failed", "bg-red-500/20 text-red-300"),
    }
}

fn phase_label(phase: &TaskPhase) -> &'static str {
    match phase {
        TaskPhase::Idle => "Idle",
        TaskPhase::Planning => "Planning",
        TaskPhase::Coding => "Coding",
        TaskPhase::QaReview => "QA Review",
        TaskPhase::QaFixing => "Fixing",
        TaskPhase::Complete => "Complete",
        TaskPhase::Failed => "Failed",
    }
}
