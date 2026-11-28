// SPDX-License-Identifier: MPL-2.0
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
use crate::components::task_dependency::DependencyList;
use crate::components::task_issue_links::{draft_is_unsaved, TaskIssueLinks};
use crate::components::new_work_pause::NewWorkPauseNotice;
use crate::components::pr_status::PullRequestSection;
use crate::components::task_activity::TaskActivity;
use crate::components::task_live::{
    format_elapsed, output_provenance, project_run, DrawerActions, OutputProvenance, RefreshGate,
    RunBadge, StartRequest,
};
use crate::components::pr_publication::PublishToPrPanel;
use crate::components::task_review::{AiReviewSection, HumanReviewPanel};
use crate::components::toast;
use crate::models::{AgentStatus, LogLevel, Task, TaskPhase, TaskRunSnapshot, TaskStatus};
use crate::services::task_run_service::{get_task_run, listen_agent_events, stop_task_execution};
use crate::services::human_review_service::{get_task_review_changes, ReviewChanges};
use crate::services::{enqueue_task, get_task_diff, get_task_diff_stat, promoted_message, run_queued_task, EnqueueOutcome, RunNowOutcome};

/// The most output entries the drawer renders. The backend keeps more; this
/// is "recent output", not a transcript.
const VISIBLE_OUTPUT: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Output,
    Changes,
}

/// One step through the Human Review column.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReviewStep {
    Previous,
    Next,
}

/// What a step through the Human Review column does from the open Task.
#[derive(Debug, PartialEq, Eq)]
enum StepOutcome {
    /// Open this Task in the drawer.
    Open(Uuid),
    /// A Task lies that way, but the drawer holds text that moving would lose.
    Unsaved,
    /// No Task lies that way: the edge of the column, or the open Task is not in it.
    Unavailable,
}

/// The Human Review column in the order the board shows it.
fn review_column(tasks: &[Task]) -> Vec<Uuid> {
    let mut column: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::HumanReview)
        .collect();
    column.sort_by_key(|t| t.position);
    column.into_iter().map(|t| t.id).collect()
}

/// The Task one step from `current` in the Human Review column, if any.
fn review_neighbour(tasks: &[Task], current: Uuid, step: ReviewStep) -> Option<Uuid> {
    let column = review_column(tasks);
    let at = column.iter().position(|id| *id == current)?;
    match step {
        ReviewStep::Previous => at.checked_sub(1).map(|i| column[i]),
        ReviewStep::Next => column.get(at + 1).copied(),
    }
}

/// Whether the drawer holds unsent text that a step to another Task would
/// discard: the Request Changes draft or the GitHub issue address being typed.
fn drawer_holds_unsaved_text(review_draft: bool, issue_address: &str) -> bool {
    review_draft || draft_is_unsaved(issue_address)
}

fn review_step(tasks: &[Task], current: Uuid, step: ReviewStep, unsaved: bool) -> StepOutcome {
    match review_neighbour(tasks, current, step) {
        None => StepOutcome::Unavailable,
        Some(_) if unsaved => StepOutcome::Unsaved,
        Some(id) => StepOutcome::Open(id),
    }
}

/// Previous and next Human Review Tasks, reached without closing the drawer.
/// Shown only while the open Task is in that column.
#[component]
fn ReviewStepper(
    task_id: Uuid,
    tasks: ReadSignal<Vec<Task>>,
    on_open: Callback<Uuid>,
    /// Whether the drawer holds text (an unsent Request Changes draft or a
    /// GitHub issue address) that moving to another Task would discard.
    unsaved: Signal<bool>,
) -> impl IntoView {
    let in_column = Memo::new(move |_| tasks.with(|all| review_column(all).contains(&task_id)));
    let position = Memo::new(move |_| {
        tasks.with(|all| {
            let column = review_column(all);
            column.iter().position(|id| *id == task_id).map(|at| (at + 1, column.len()))
        })
    });
    let blocked = RwSignal::new(false);
    let step = move |step: ReviewStep| {
        let outcome = tasks.with_untracked(|all| review_step(all, task_id, step, unsaved.get_untracked()));
        match outcome {
            StepOutcome::Open(id) => {
                blocked.set(false);
                on_open.run(id);
            }
            StepOutcome::Unsaved => blocked.set(true),
            StepOutcome::Unavailable => {}
        }
    };
    let can_step = move |step: ReviewStep| tasks.with(|all| review_neighbour(all, task_id, step).is_some());

    view! {
        <Show when=move || in_column.get()>
            <div data-testid="task-drawer-review-stepper" class="px-5 pt-3 space-y-2">
                <div class="flex items-center gap-2 text-xs text-white/60">
                    <button
                        data-testid="task-drawer-review-previous"
                        class="px-2 py-1 rounded-md hover:bg-white/10 disabled:opacity-40 disabled:hover:bg-transparent"
                        aria-label="Previous Human Review task"
                        title="Previous Human Review task"
                        disabled=move || !can_step(ReviewStep::Previous)
                        on:click=move |_| step(ReviewStep::Previous)
                    >
                        "← Previous"
                    </button>
                    <span data-testid="task-drawer-review-position" class="flex-1 text-center tabular-nums">
                        {move || position.get().map(|(at, total)| format!("{at} of {total} in Human Review"))}
                    </span>
                    <button
                        data-testid="task-drawer-review-next"
                        class="px-2 py-1 rounded-md hover:bg-white/10 disabled:opacity-40 disabled:hover:bg-transparent"
                        aria-label="Next Human Review task"
                        title="Next Human Review task"
                        disabled=move || !can_step(ReviewStep::Next)
                        on:click=move |_| step(ReviewStep::Next)
                    >
                        "Next →"
                    </button>
                </div>
                <Show when=move || blocked.get() && unsaved.get()>
                    <p data-testid="task-drawer-review-unsaved" role="alert" class="text-xs text-amber-200">
                        "Text here is not sent yet (a Request Changes draft or a GitHub issue address). Send it or clear it to move to another Task; nothing was discarded."
                    </p>
                </Show>
            </div>
        </Show>
    }
}

#[component]
pub fn TaskDrawer(
    task_id: Uuid,
    tasks: ReadSignal<Vec<Task>>,
    /// The agent's current one-line activity for this task, if any.
    #[prop(into)]
    activity: Signal<Option<String>>,
    /// The run SlashIt owns for this task, from the board's one record of
    /// runs, so the drawer and the card cannot disagree about it.
    #[prop(into)]
    run_status: Signal<Option<AgentStatus>>,
    on_close: Callback<()>,
    on_edit: Callback<Task>,
    /// Open another Task, from the dependency list.
    on_open: Callback<Uuid>,
    /// Apply a task record a command returned, superseding older reads.
    apply_task: Callback<Task>,
    /// Read the task list again, for a change no event announces.
    refresh_tasks: Callback<()>,
) -> impl IntoView {
    let task = Memo::new(move |_| tasks.with(|all| all.iter().find(|t| t.id == task_id).cloned()));
    let status = Memo::new(move |_| task.with(|t| t.as_ref().map(|t| t.status.clone())));

    // What the drawer calls working: the owned run, unless the task needs the
    // person, in which case that is what it says.
    let badge = Memo::new(move |_| {
        let attention = task.with(|t| {
            t.as_ref().and_then(|t| {
                t.needs_you(crate::components::attention::delivery_in_flight(task_id))
            })
        });
        run_status.with(|run| project_run(run.as_ref(), attention))
    });

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
    let ticker = StoredValue::new_local(Some(gloo_timers::callback::Interval::new(
        1_000,
        move || {
            now.try_set(chrono::Utc::now());
        },
    )));
    on_cleanup(move || ticker.dispose());

    let actions = Memo::new(move |_| {
        task.get()
            .map(|task| {
                run.with(|r| {
                    DrawerActions::for_task_with_recovery(
                        &task.status,
                        r,
                        task.worker_handoff.as_ref().is_some_and(|handoff| {
                            matches!(
                                &handoff.stage,
                                crate::models::WorkerHandoffStage::RecoveryRequired { .. }
                            )
                        }),
                    )
                })
            })
            .unwrap_or_default()
    });

    let start = RwSignal::new(StartRequest::default());
    // A failed Start is about the Backlog task it was pressed for; once the
    // task is somewhere else, its reason no longer describes anything. The
    // alert is also gated on Backlog (`failure_for`), because a failure can
    // land after the task has already left it.
    Effect::new(move |_| {
        if status.get() != Some(TaskStatus::Backlog)
            && start.with_untracked(|s| s.failure().is_some())
        {
            start.set(StartRequest::Idle);
        }
    });

    let stopping = RwSignal::new(false);
    // The Run now request for a queued task, from press to answer. Like
    // Start, a second press while one is in flight is refused: the first
    // promotion may already have left the queue.
    let (run_now_pending, set_run_now_pending) = signal(false);
    let retrying = RwSignal::new(false);
    // The failure a retry was asked about, kept visible as the last attempt's
    // result until the next run changes the task.
    let previous_error = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        if status.get() != Some(TaskStatus::Queue) {
            previous_error.set(None);
        }
    });

    // Start enqueues, exactly as the card menu's "Add to Queue" does; the
    // scheduler runs the task when there is capacity. The drawer shows what
    // the task actually became, never a Running it assumed. Start is only
    // offered in Backlog, so that is the column it expects: if the task has
    // left it meanwhile (the scheduler started it), the backend changes
    // nothing and the drawer shows where the task really is.
    let on_start = move |_| {
        if !start.try_update(|s| s.begin()).unwrap_or(false) {
            return;
        }
        spawn_local(async move {
            let outcome = enqueue_task(task_id.to_string(), TaskStatus::Backlog)
                .await
                .map(|outcome| match outcome {
                    EnqueueOutcome::Queued(task) | EnqueueOutcome::AlreadyMoved(task) => Some(task),
                    EnqueueOutcome::Missing => None,
                });
            match start.try_update(|s| s.settle(outcome)) {
                // Applying the record also reads the task list again, since
                // the scheduler may already have moved the task on.
                Some(Some(updated)) => apply_task.try_run(updated),
                // After a failure the drawer must show where the task really
                // is.
                Some(None) => refresh_tasks.try_run(()),
                // The drawer closed meanwhile.
                None => return,
            };
            refresh_run();
        });
    };

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

    // Run now promotes a queued task out of the queue, through the same
    // lifecycle the scheduler's promotion uses; the executor then starts its
    // run. The queue column is the one this expects: a behind view (the
    // promotion already happened, or the task was moved) is answered with the
    // task as it now is, not with a second promotion.
    let on_run_now = move |_| {
        if run_now_pending.get_untracked() {
            return;
        }
        set_run_now_pending.set(true);
        spawn_local(async move {
            match run_queued_task(task_id.to_string(), TaskStatus::Queue).await {
                Ok(RunNowOutcome::Promoted(task)) => {
                    toast::success(promoted_message(&task.title));
                    apply_task.try_run(task);
                    refresh_tasks.try_run(());
                }
                // Nothing was changed: show the task where it really is.
                Ok(RunNowOutcome::NotQueued(current)) => {
                    apply_task.try_run(current);
                    refresh_tasks.try_run(());
                }
                Ok(RunNowOutcome::Missing) => {
                    toast::error("This task no longer exists".to_string())
                }
                Err(e) => toast::error(format!("Could not run the task: {e}")),
            }
            set_run_now_pending.try_set(false);
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
            match enqueue_task(task_id.to_string(), TaskStatus::Error).await {
                Ok(EnqueueOutcome::Queued(updated)) => {
                    apply_task.try_run(updated);
                    previous_error.try_set(failure);
                }
                // No longer failed (already retried elsewhere): nothing was
                // changed; show the task as it is.
                Ok(EnqueueOutcome::AlreadyMoved(current)) => {
                    apply_task.try_run(current);
                }
                Ok(EnqueueOutcome::Missing) => {
                    toast::error("This task no longer exists".to_string())
                }
                Err(e) => toast::error(format!("Could not retry the task: {e}")),
            }
            retrying.try_set(false);
            refresh_run();
        });
    };

    let tab = RwSignal::new(Tab::Output);
    // Loaded when the Changes tab is opened, and again if the task moves on
    // while it is open (an AI review fix changes the code).
    let changes = RwSignal::new(None::<Result<ChangesView, String>>);
    // The commit the loaded changes are for, shared with the decision panel so
    // it can refuse to approve a commit other than the one on screen.
    let viewed_tip = RwSignal::new(None::<String>);
    // Only the latest question's answer is kept: a load that was overtaken (the
    // task moved on, or the panel asked for a fresh one) must not put its older
    // picture, or the end of its guard, back.
    let generation = StoredValue::new(0u32);
    let load_changes = move || {
        let reviewing = status.get_untracked() == Some(TaskStatus::HumanReview);
        let mine = generation.try_update_value(|n| { *n += 1; *n }).unwrap_or(0);
        spawn_local(async move {
            let current = move || generation.try_get_value() == Some(mine);
            let review = if reviewing {
                match get_task_review_changes(task_id.to_string()).await {
                    Ok(review) => review,
                    Err(e) => {
                        if current() {
                            changes.try_set(Some(Err(e)));
                        }
                        return;
                    }
                }
            } else {
                None
            };
            let loaded = match review {
                Some(review) => Ok(ChangesView {
                    diff: review.patch.clone(),
                    stat: review.stat.clone(),
                    review: Some(review),
                }),
                None => match get_task_diff(task_id.to_string()).await {
                    Ok(diff) => {
                        let stat = get_task_diff_stat(task_id.to_string())
                            .await
                            .unwrap_or_default();
                        Ok(ChangesView { diff, stat, review: None })
                    }
                    Err(e) => Err(e),
                },
            };
            if current() {
                changes.try_set(Some(loaded));
            }
        });
    };
    // A commit counts as seen only once its diff is on the open Changes tab.
    Effect::new(move |_| {
        let on_screen = tab.get() == Tab::Changes;
        let seen = changes.with(|c| {
            let loaded = c.as_ref().and_then(|c| c.as_ref().ok());
            let tip = loaded.and_then(|v| v.review.as_ref().map(|r| r.tip.as_str()));
            seen_tip(viewed_tip.get_untracked(), on_screen, tip)
        });
        if viewed_tip.with_untracked(|v| *v != seen) {
            viewed_tip.set(seen);
        }
    });
    // Drops the picture, and with it what was seen. While the Changes tab is open
    // it is read again.
    let invalidate_changes = move || {
        viewed_tip.set(None);
        changes.set(None);
        if tab.get_untracked() == Tab::Changes {
            load_changes();
        } else {
            generation.update_value(|n| *n += 1);
        }
    };
    // The decision panel found the tab out of date. Open the Changes tab on the
    // branch's current commit. Nothing can be approved until it is on screen.
    let reload_changes = Callback::new(move |()| {
        viewed_tip.set(None);
        changes.set(None);
        tab.set(Tab::Changes);
        load_changes();
    });
    let open_changes = move |_| {
        tab.set(Tab::Changes);
        if changes.get_untracked().is_none() {
            load_changes();
        }
    };
    Effect::new(move |previous: Option<Option<TaskStatus>>| {
        let current = status.get();
        if previous.is_some_and(|p| p != current) {
            invalidate_changes();
        }
        current
    });
    // Approval needs the committed changes on screen, so a Human Review task opens
    // its Changes tab and loads them. The tab is switched only while nothing is
    // loaded, so a failed load stays an error and is not retried in a loop.
    Effect::new(move |_| {
        if status.get() == Some(TaskStatus::HumanReview) && changes.with(Option::is_none) {
            tab.set(Tab::Changes);
            load_changes();
        }
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

    // Unsent text a step to another Task would discard: the Human Review
    // draft and the GitHub issue address being typed. A step is blocked while
    // either holds text.
    let review_unsaved = RwSignal::new(false);
    let issue_address = RwSignal::new(String::new());
    let unsaved = Signal::derive(move || {
        issue_address.with(|address| drawer_holds_unsaved_text(review_unsaved.get(), address))
    });

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
                        <DrawerHeader task=t.clone() badge=badge on_close=on_close />
                    }.into_any(),
                }}

                <ReviewStepper task_id=task_id tasks=tasks on_open=on_open unsaved=unsaved />

                <div class="flex-1 min-h-0 overflow-y-auto px-5 py-4 space-y-4">
                    // Where the task is and what it is doing.
                    {move || task.get().map(|t| {
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
                                {move || badge.get().map(|badge| view! {
                                    <div data-testid="task-drawer-activity" data-run=badge.label().to_lowercase() class="text-sm text-blue-200/90 truncate">
                                        {if badge == RunBadge::Working {
                                            activity.get().unwrap_or_else(|| badge.placeholder().to_string())
                                        } else {
                                            badge.placeholder().to_string()
                                        }}
                                    </div>
                                })}
                            </section>
                        }
                    })}

                    // What can be done now.
                    <section class="flex flex-wrap gap-2">
                        <Show when=move || actions.get().start || start.with(StartRequest::is_pending)>
                            <button
                                data-testid="task-drawer-start"
                                class="px-3 py-1.5 rounded-lg text-sm bg-blue-500 text-white hover:bg-blue-400 disabled:opacity-50"
                                title="Add this task to the queue. It runs when there is capacity."
                                disabled=move || start.with(StartRequest::is_pending)
                                aria-busy=move || if start.with(StartRequest::is_pending) { "true" } else { "false" }
                                on:click=on_start
                            >
                                {move || if start.with(StartRequest::is_pending) { "Starting…" } else { "Start" }}
                            </button>
                        </Show>
                        // Announced, not only shown: a disabled button's new
                        // label is easy for a screen reader to miss.
                        <span data-testid="task-drawer-start-status" role="status" aria-live="polite" class="sr-only">
                            {move || if start.with(StartRequest::is_pending) { "Starting…" } else { "" }}
                        </span>
                        <Show when=move || actions.get().run_now || run_now_pending.get()>
                            <button
                                data-testid="task-drawer-run-now"
                                class="px-3 py-1.5 rounded-lg text-sm bg-blue-500/15 text-blue-300 hover:bg-blue-500/25 disabled:opacity-50"
                                title="Promote this queued task now; its run starts without waiting for the queue's order."
                                disabled=move || run_now_pending.get()
                                aria-busy=move || if run_now_pending.get() { "true" } else { "false" }
                                on:click=on_run_now
                            >
                                {move || if run_now_pending.get() { "Running…" } else { "Run now" }}
                            </button>
                        </Show>
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

                    // Waiting to start: say so when the reason is disk space.
                    <Show when=move || task.with(|t| t.as_ref().is_some_and(|t| {
                        t.status == TaskStatus::Queue
                            || (t.status == TaskStatus::InProgress && t.phase == TaskPhase::Idle)
                    }))>
                        <NewWorkPauseNotice testid="task-drawer-new-work-paused"/>
                    </Show>

                    {move || start.with(|s| s.failure_for(status.get().as_ref()).map(str::to_string)).map(|reason| view! {
                        <p data-testid="task-drawer-start-error" role="alert" class="text-sm text-red-300 whitespace-pre-wrap break-words">
                            {reason}
                        </p>
                    })}

                    // What happened, if it failed.
                    {move || task.get().filter(|t| t.status == TaskStatus::Error || t.worker_handoff.as_ref().is_some_and(|handoff| matches!(&handoff.stage, crate::models::WorkerHandoffStage::RecoveryRequired { .. }))).map(|t| view! {
                        <section data-testid="task-drawer-failure" class="rounded-lg border border-red-500/20 bg-red-500/[0.06] p-3 space-y-2">
                            <h3 class="text-xs font-semibold uppercase tracking-wide text-red-300">
                                {if t.worker_handoff.as_ref().is_some_and(|handoff| matches!(&handoff.stage, crate::models::WorkerHandoffStage::RecoveryRequired { .. })) { "Worker recovery required" } else { "What went wrong" }}
                            </h3>
                            <p data-testid="task-drawer-error" class="text-sm text-red-200 whitespace-pre-wrap break-words">
                                {t.error_message.clone().unwrap_or_else(|| "The run failed without recording a reason.".to_string())}
                            </p>
                            {t.worker_handoff.as_ref().and_then(|handoff| matches!(&handoff.stage, crate::models::WorkerHandoffStage::RecoveryRequired { .. }).then_some(handoff)).map(|handoff| view! {
                                <p data-testid="task-drawer-recovery-guidance" class="text-xs text-white/50">
                                    {format!("Task Checkout preserved at {} on branch {}. Use Changes and Activity to inspect it. No new Worker can start until this recovery is resolved.", t.worktree_path.as_deref().unwrap_or("an unrecorded path"), handoff.branch.as_deref().unwrap_or("unknown"))}
                                </p>
                            }).unwrap_or_else(|| view! {
                                <p data-testid="task-drawer-retry-note" class="text-xs text-white/40">{retry_note(&t).to_string()}</p>
                            })}
                        </section>
                    })}
                    <crate::components::worker_recovery::WorkerRecoveryPanel task_id=task_id task=task apply_task=apply_task />
                    <crate::components::review_fix_recovery::ReviewFixRecoveryPanel task_id=task_id task=task apply_task=apply_task />
                    <crate::components::new_attempt::NewAttemptPanel task_id=task_id task=task apply_task=apply_task />
                    {move || previous_error.get().filter(|_| !run.with(|r| r.live)).map(|message| view! {
                        <section data-testid="task-drawer-previous-error" class="rounded-lg border border-white/10 bg-white/[0.03] p-3 space-y-2">
                            <h3 class="text-xs font-semibold uppercase tracking-wide text-white/50">"Last attempt failed"</h3>
                            <p class="text-sm text-white/60 whitespace-pre-wrap break-words">{message}</p>
                            <p class="text-xs text-white/40">"Queued to run again. This stays until the next run starts."</p>
                        </section>
                    })}

                    {move || task.get().map(|t| view! { <TaskSummary task=t /> })}

                    {move || task.get().filter(|t| !t.dependencies.is_empty()).map(|t| view! {
                        <DependencyList task=t tasks=tasks on_open=on_open />
                    })}

                    // The GitHub Issues this task links. Kept mounted while the task exists, so a typed address survives a poll.
                    <Show when=move || task.with(|t| t.is_some())>
                        <TaskIssueLinks task_id=task_id task=task apply_task=apply_task address=issue_address />
                    </Show>

                    // The decision on these changes, and what became of it.
                    <Show when=move || {
                        task.with(|t| t.as_ref().is_some_and(|t| {
                            t.status == TaskStatus::HumanReview
                                || (t.status == TaskStatus::PrCreated && t.human_review.is_approved())
                                || t.completed_locally()
                        }))
                    }>
                        <HumanReviewPanel task_id=task_id task=task apply_task=apply_task viewed_tip=viewed_tip on_stale=reload_changes unsaved=review_unsaved />
                    </Show>

                    // What an external agent asked a person to decide.
                    <crate::components::agent_requests::AgentRequestsPanel task_id=task_id apply_task=apply_task viewed_tip=viewed_tip />

                    // A request to open the pull request that was never answered, and
                    // what the person did about it.
                    <crate::components::unconfirmed_pr::UnconfirmedPrPanel task_id=task_id task=task apply_task=apply_task />

                    // Its pull request, when it has one.
                    <PullRequestSection task_id=task_id task=task now=now refresh_tasks=refresh_tasks />

                    // Publishing a commit to that pull request: its own decision, never an approval.
                    <Show when=move || task.with(|t| t.as_ref().is_some_and(|t| t.pr_url.is_some() && t.branch_name.is_some()))>
                        <PublishToPrPanel task_id=task_id task=task refresh_tasks=refresh_tasks />
                    </Show>

                    // How the task got here.
                    <TaskActivity task=task />

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
                                    Some(Ok(ChangesView { diff, stat, review })) => view! {
                                        {review.map(|r| view! { <ReviewScope review=r /> })}
                                        <DiffViewer diff=diff stat=stat />
                                    }.into_any(),
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
fn DrawerHeader(
    task: Task,
    badge: Memo<Option<RunBadge>>,
    on_close: Callback<()>,
) -> impl IntoView {
    let (status_label, class) = status_badge(&task.status);
    // A task in the In Progress column says what its run is only while
    // SlashIt owns one; the column alone does not say anything is running.
    let in_progress = task.status == TaskStatus::InProgress;
    let label = move || match badge.get() {
        Some(badge) if in_progress => badge.label(),
        _ => status_label,
    };
    let status_key = format!("{:?}", task.status).to_lowercase();
    view! {
        <header class="flex items-start gap-3 px-5 py-4 border-b border-white/10">
            <div class="flex-1 min-w-0">
                <h2 data-testid="task-drawer-title" class="text-base font-semibold text-white/90 break-words">{task.title.clone()}</h2>
                <div class="mt-1"><crate::components::task_card::TaskIdCopy task_id=task.id /></div>
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
    view! {
        <section class="space-y-2 text-sm">
            {task.description.clone().filter(|d| !d.trim().is_empty()).map(|d| {
                let expanded = RwSignal::new(false);
                view! {
                    <div>
                        <p
                            data-testid="task-drawer-description"
                            class=move || if expanded.get() {
                                "text-white/60 whitespace-pre-wrap break-words"
                            } else {
                                "text-white/60 whitespace-pre-wrap break-words line-clamp-6"
                            }
                        >{d}</p>
                        {view! {
                            <button
                                type="button"
                                data-testid="task-drawer-description-toggle"
                                class="mt-1 text-xs text-blue-300 hover:text-blue-200"
                                aria-expanded=move || expanded.get().to_string()
                                on:click=move |_| expanded.update(|v| *v = !*v)
                            >
                                {move || if expanded.get() { "Show less" } else { "Show full description" }}
                            </button>
                        }}
                    </div>
                }
            })}
            {task.qa_signoff.clone().map(|signoff| view! { <AiReviewSection signoff=signoff /> })}
            {(task.status == crate::models::TaskStatus::HumanReview && task.qa_signoff.is_none()).then(|| {
                let note = no_ai_review_note(&task);
                view! {
                    <p data-testid="task-drawer-no-ai-review" class="text-white/60">{note}</p>
                }
            })}
        </section>
    }
}

/// What the drawer says when a Human Review has no AI verdict. A run whose
/// review mode could not be confirmed (its record was rebuilt) is not
/// described as Manual: Manual was never chosen for that run. The mode is
/// read from the run's own record, because the Task's configured mode can be
/// edited after the run started and then applies only to the next run. When
/// the record names a skipped or failed review, its reason is said as well.
/// The closing sentence comes from the same recorded approval the Human Review
/// panel reads, so it never denies an approval the Task holds.
fn no_ai_review_note(task: &Task) -> String {
    let handoff = task.worker_handoff.as_ref();
    let unconfirmed = handoff.is_some_and(|handoff| handoff.review_mode_unproven);
    let manual = handoff.is_some_and(|handoff| {
        handoff.review_mode == crate::models::task::ReviewMode::Manual
    });
    let body = if unconfirmed {
        "No AI Review ran for these changes. Their review mode could not be confirmed, so AI Review was withheld.".to_string()
    } else if manual {
        "No AI Review ran: Manual review mode is selected. Inspect the changes yourself.".to_string()
    } else if let Some(summary) =
        crate::components::task_review::no_verdict_summary(None, &task.activity)
    {
        summary
    } else {
        "No AI Review ran for these changes.".to_string()
    };
    format!("{body} {}", approval_sentence(task))
}

/// Who approved the changes, as the Human Review record says. A recorded Human
/// approval is named whether or not it still covers the branch: the panel
/// says that, and this line must not deny the approval.
fn approval_sentence(task: &Task) -> &'static str {
    if task.human_review.is_approved() {
        "A person approved these changes. Human Review shows whether that approval still covers the branch."
    } else {
        "Nothing has been approved by AI."
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

/// What Retry will do for a failed task.
///
/// The next run continues in the task's recorded checkout. A task that
/// failed before a checkout was ever attached to it has none, so there is no
/// earlier work for the next run to continue from.
fn retry_note(task: &Task) -> &'static str {
    if task.worktree_path.is_some() {
        "This is the result of the last attempt. Retry puts the task back in the queue, and the next run continues from the work already done."
    } else {
        "This is the result of the last attempt. Retry puts the task back in the queue."
    }
}

fn status_badge(status: &TaskStatus) -> (&'static str, &'static str) {
    match status {
        TaskStatus::Backlog => ("Backlog", "bg-white/10 text-white/60"),
        TaskStatus::Queue => ("Queued", "bg-slate-500/20 text-slate-300"),
        TaskStatus::InProgress => ("In Progress", "bg-blue-500/20 text-blue-300"),
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

/// What the Changes tab shows: the diff, and, in Human Review, the commit it is for.
#[derive(Clone)]
struct ChangesView {
    diff: String,
    stat: String,
    review: Option<ReviewChanges>,
}

/// The commit a person has been shown, given the one seen before, whether the
/// Changes tab is open, and the commit of the diff that is loaded. A diff that
/// is still loading, failed, or arrived behind another tab shows nothing.
fn seen_tip(previous: Option<String>, on_screen: bool, loaded: Option<&str>) -> Option<String> {
    let loaded = loaded?;
    if on_screen {
        Some(loaded.to_string())
    } else {
        previous.filter(|seen| seen == loaded)
    }
}

/// Names the commit the diff below is, and lists edits the checkout holds that
/// it does not contain. The person approves the commit, not those edits.
#[component]
fn ReviewScope(review: ReviewChanges) -> impl IntoView {
    let summary = pending_summary(&review.pending_patch, &review.pending_stat);
    let short: String = review.tip.chars().take(12).collect();
    view! {
        <div data-testid="review-changes-scope" class="mb-2 space-y-2">
            <p class="text-xs text-white/60">
                "Showing the changes up to commit "
                <code data-testid="review-changes-commit" class="font-mono text-white/80">{short}</code>
                ", the commit an approval records."
            </p>
            {summary.map(|summary| view! {
                <details data-testid="review-changes-pending" class="rounded-md border border-amber-500/20 bg-amber-500/[0.06] p-2">
                    <summary class="text-xs text-amber-200 cursor-pointer">{summary}</summary>
                    <div class="mt-2">
                        <DiffViewer diff=review.pending_patch.clone() stat=review.pending_stat.clone() />
                    </div>
                </details>
            })}
        </div>
    }
}

/// The notice for edits the checkout holds that the commit does not. Empty,
/// binary and oversized untracked files leave a stat line or a marker but no
/// `diff --git` header, so either part counts as something to list.
pub fn pending_summary(pending_patch: &str, pending_stat: &str) -> Option<String> {
    let files = pending_patch.lines().filter(|l| l.starts_with("diff --git ")).count();
    if files == 0 && pending_patch.trim().is_empty() && pending_stat.trim().is_empty() {
        return None;
    }
    let what = match files {
        0 => "Uncommitted changes in the checkout".to_string(),
        1 => "1 file with uncommitted changes in the checkout".to_string(),
        n => format!("{n} files with uncommitted changes in the checkout"),
    };
    Some(format!("{what}. They are not part of this commit and are not approved."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed_task(worktree: Option<&str>) -> Task {
        let mut task: Task = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "project_id": Uuid::new_v4(),
            "title": "t",
            "description": null,
            "status": "error",
            "model": "default",
            "planning_mode": false,
            "dependencies": [],
            "workspace_id": null,
            "jj_change_id": null,
            "category": "feature",
            "priority": "medium",
            "complexity": "moderate",
            "impact": "medium",
            "security_severity": "none",
            "phase": "failed",
            "phase_progress": 0,
            "overall_progress": 0,
            "subtasks": [],
            "sequence_number": 0,
            "github_issue_url": null,
            "gitlab_issue_url": null,
            "linear_ticket_id": null,
            "pr_url": null,
            "qa_signoff": null,
            "stuck_since": null,
            "created_at": chrono::Utc::now(),
            "updated_at": chrono::Utc::now(),
        }))
        .expect("a failed task");
        task.worktree_path = worktree.map(str::to_string);
        task
    }

    #[test]
    fn a_commit_is_seen_only_once_its_diff_is_on_the_changes_tab() {
        let seen = || Some("abc".to_string());
        // Loading or failed: nothing, whatever was seen before.
        assert_eq!(seen_tip(seen(), true, None), None);
        // Loaded behind another tab: not seen until the tab is opened.
        assert_eq!(seen_tip(None, false, Some("abc")), None);
        assert_eq!(seen_tip(seen(), false, Some("def")), None);
        assert_eq!(seen_tip(None, true, Some("abc")), seen());
        // Switching away afterwards does not unsee it.
        assert_eq!(seen_tip(seen(), false, Some("abc")), seen());
    }

    #[test]
    fn retry_promises_continuation_only_when_there_is_a_checkout_to_continue_in() {
        assert!(retry_note(&failed_task(Some("/tmp/wt/task-1")))
            .contains("continues from the work already done"));
        let note = retry_note(&failed_task(None));
        assert!(!note.contains("continues"), "{note}");
        assert!(note.contains("Retry puts the task back in the queue."));
    }

    fn in_column(status: TaskStatus, position: i32) -> Task {
        let mut task = failed_task(None);
        task.status = status;
        task.position = position;
        task
    }

    #[test]
    fn steps_follow_the_board_order_of_the_human_review_column_only() {
        // Positions are out of insertion order, and an AI Review Task sits
        // between two Human Review Tasks; neither may appear in the walk.
        let third = in_column(TaskStatus::HumanReview, 2);
        let other = in_column(TaskStatus::AiReview, 1);
        let first = in_column(TaskStatus::HumanReview, 0);
        let second = in_column(TaskStatus::HumanReview, 1);
        let tasks = vec![third.clone(), other, first.clone(), second.clone()];

        assert_eq!(review_column(&tasks), vec![first.id, second.id, third.id]);
        assert_eq!(review_neighbour(&tasks, second.id, ReviewStep::Previous), Some(first.id));
        assert_eq!(review_neighbour(&tasks, second.id, ReviewStep::Next), Some(third.id));
        assert_eq!(review_step(&tasks, second.id, ReviewStep::Next, false), StepOutcome::Open(third.id));
    }

    #[test]
    fn the_first_and_last_task_have_no_step_past_the_edge() {
        let first = in_column(TaskStatus::HumanReview, 0);
        let last = in_column(TaskStatus::HumanReview, 1);
        let tasks = vec![first.clone(), last.clone()];

        assert_eq!(review_step(&tasks, first.id, ReviewStep::Previous, false), StepOutcome::Unavailable);
        assert_eq!(review_step(&tasks, last.id, ReviewStep::Next, false), StepOutcome::Unavailable);
    }

    #[test]
    fn a_task_alone_in_its_column_or_no_longer_in_it_has_nowhere_to_step() {
        let alone = in_column(TaskStatus::HumanReview, 0);
        let left = in_column(TaskStatus::InProgress, 0);
        let gone = failed_task(None).id;
        let tasks = vec![alone.clone(), left.clone()];

        for step in [ReviewStep::Previous, ReviewStep::Next] {
            assert_eq!(review_step(&tasks, alone.id, step, false), StepOutcome::Unavailable);
            assert_eq!(review_step(&tasks, left.id, step, false), StepOutcome::Unavailable);
            assert_eq!(review_step(&tasks, gone, step, false), StepOutcome::Unavailable);
        }
        assert_eq!(review_step(&[], gone, ReviewStep::Next, false), StepOutcome::Unavailable);
    }

    #[test]
    fn an_unsent_draft_blocks_a_step_that_would_leave_it_but_not_a_step_with_no_target() {
        let first = in_column(TaskStatus::HumanReview, 0);
        let second = in_column(TaskStatus::HumanReview, 1);
        let tasks = vec![first.clone(), second.clone()];

        assert_eq!(review_step(&tasks, first.id, ReviewStep::Next, true), StepOutcome::Unsaved);
        assert_eq!(review_step(&tasks, second.id, ReviewStep::Previous, true), StepOutcome::Unsaved);
        // The edge stays unavailable even with a draft: nothing is offered to move to.
        assert_eq!(review_step(&tasks, first.id, ReviewStep::Previous, true), StepOutcome::Unavailable);
    }

    #[test]
    fn either_unsent_draft_holds_the_drawer() {
        assert!(!drawer_holds_unsaved_text(false, ""));
        assert!(!drawer_holds_unsaved_text(false, "  "));
        assert!(drawer_holds_unsaved_text(true, ""));
        assert!(drawer_holds_unsaved_text(false, "o/r"));
    }

    /// A Human Review Task whose run record says `run_mode`, configured as
    /// `task_mode` now. The two differ after an edit made during or after a run.
    fn human_review_task(
        task_mode: crate::models::task::ReviewMode,
        run_mode: crate::models::task::ReviewMode,
        unproven: bool,
    ) -> Task {
        use crate::models::task::{WorkerHandoff, WorkerHandoffStage};
        let mut task = failed_task(None);
        task.status = TaskStatus::HumanReview;
        task.review_mode = task_mode;
        task.worker_handoff = Some(WorkerHandoff {
            task_id: task.id,
            project_id: task.project_id,
            repository_id: None,
            run: Some(1),
            branch: None,
            start_head: None,
            review_mode: run_mode,
            review_mode_unproven: unproven,
            reviewer_model: None,
            stage: WorkerHandoffStage::Started,
            acceptance: None,
        });
        task
    }

    #[test]
    fn an_unconfirmed_run_is_not_described_as_manual_even_when_the_task_is_manual() {
        use crate::models::task::ReviewMode;
        let note = no_ai_review_note(&human_review_task(
            ReviewMode::Manual,
            ReviewMode::Manual,
            true,
        ));
        assert!(note.contains("could not be confirmed"), "{note}");
        assert!(!note.contains("Manual"), "{note}");
        assert!(!note.contains("selected"), "{note}");
    }

    /// An approval of the current arrival, as the record holds it.
    fn approval_entry(task: &Task) -> crate::models::task::HumanReviewEntry {
        use crate::models::task::{HumanReviewDecision, HumanReviewEntry};
        HumanReviewEntry {
            sequence: 1,
            arrival: task.human_review.arrivals,
            decision: HumanReviewDecision::Approved,
            feedback: None,
            approved_tip: Some("a".repeat(40)),
            decided_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn a_recorded_human_approval_is_not_denied_by_the_ai_note() {
        use crate::models::task::ReviewMode;
        let mut task = human_review_task(ReviewMode::Manual, ReviewMode::Manual, false);
        task.human_review.entries.push(approval_entry(&task));
        let note = no_ai_review_note(&task);
        assert!(note.contains("A person approved these changes"), "{note}");
        assert!(!note.contains("Nothing has been approved"), "{note}");
        assert!(!note.contains("nothing has been approved"), "{note}");
    }

    #[test]
    fn a_stale_human_approval_is_still_named_as_one() {
        use crate::models::task::ReviewMode;
        let mut task = human_review_task(ReviewMode::Automatic, ReviewMode::Automatic, false);
        task.human_review.entries.push(approval_entry(&task));
        // The branch may have moved past the approved commit: the note still names
        // the approval and leaves the question of coverage to Human Review.
        let note = no_ai_review_note(&task);
        assert!(note.contains("still covers the branch"), "{note}");
    }

    #[test]
    fn a_confirmed_manual_run_still_says_manual_was_selected() {
        use crate::models::task::ReviewMode;
        let note = no_ai_review_note(&human_review_task(
            ReviewMode::Manual,
            ReviewMode::Manual,
            false,
        ));
        assert!(note.contains("Manual review mode is selected"), "{note}");
    }

    #[test]
    fn an_automatic_run_is_not_described_as_manual_after_the_task_is_edited_to_manual() {
        use crate::models::task::ReviewMode;
        let note = no_ai_review_note(&human_review_task(
            ReviewMode::Manual,
            ReviewMode::Automatic,
            false,
        ));
        assert_eq!(note, "No AI Review ran for these changes. Nothing has been approved by AI.");
    }

    #[test]
    fn a_manual_run_is_still_described_as_manual_after_the_task_is_edited_to_automatic() {
        use crate::models::task::ReviewMode;
        let note = no_ai_review_note(&human_review_task(
            ReviewMode::Automatic,
            ReviewMode::Manual,
            false,
        ));
        assert!(note.contains("Manual review mode is selected"), "{note}");
    }

    #[test]
    fn a_task_without_a_run_record_says_no_ai_review_ran() {
        use crate::models::task::ReviewMode;
        let mut task = human_review_task(ReviewMode::Manual, ReviewMode::Manual, false);
        task.worker_handoff = None;
        let note = no_ai_review_note(&task);
        assert_eq!(note, "No AI Review ran for these changes. Nothing has been approved by AI.");
    }

    #[test]
    fn uncommitted_edits_are_noticed_even_when_they_leave_no_diff_header() {
        assert_eq!(pending_summary("", ""), None);
        assert_eq!(pending_summary("\n", " \n"), None);
        let one = pending_summary("diff --git a/x b/x\n+1\n", "").unwrap();
        assert!(one.starts_with("1 file with uncommitted") && one.contains("not approved"), "{one}");
        let two = "diff --git a/x b/x\n+1\ndiff --git a/y b/y\n+2\n+diff --git not a header\n";
        assert!(pending_summary(two, "").unwrap().starts_with("2 files with"));
        // An empty untracked file: a stat line, no header.
        let empty = pending_summary("", " empty.txt | 0\n").unwrap();
        assert!(empty.starts_with("Uncommitted changes in the checkout"), "{empty}");
    }
}
