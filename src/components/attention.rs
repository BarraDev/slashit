//! "Needs you": which tasks cannot make progress without the user.
//!
//! Whether a task needs the user is `Task::needs_you`, which is the shared
//! `slashit_attention` rule; nothing here decides it again. What lives here
//! is how the answer is shown -- the card chip, the order the header walks
//! the board in -- and the one fact the task record cannot carry: that this
//! window is opening a task's pull request right now.

use std::collections::HashMap;

use leptos::prelude::*;
use uuid::Uuid;

use crate::models::{AttentionReason, Task, TaskStatus};

/// Pull requests this window is opening, and how many attempts it has made
/// for each task since it started.
///
/// Provided once for the whole app rather than by the drawer or the board,
/// because an attempt outlives both: closing the drawer or switching
/// projects does not stop it, and a board mounted again meanwhile still has
/// to know the task is waiting on SlashIt, not on the user. Never persisted:
/// an attempt ends with the process that runs it.
#[derive(Clone, Copy)]
pub struct Deliveries(RwSignal<HashMap<Uuid, DeliveryRecord>>);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeliveryRecord {
    /// Attempts started and not yet answered.
    pub in_flight: u32,
    /// Attempts this window has finished.
    pub finished: u32,
    /// When the last one finished, as local wall-clock time.
    pub last_finished_at: Option<String>,
}

impl Deliveries {
    pub fn provide() {
        provide_context(Self(RwSignal::new(HashMap::new())));
    }

    pub fn get() -> Option<Self> {
        use_context::<Self>()
    }

    /// Whether a pull request is being opened for `task_id`. Tracked.
    pub fn in_flight(self, task_id: Uuid) -> bool {
        self.0.with(|m| m.get(&task_id).is_some_and(|r| r.in_flight > 0))
    }

    /// [`Self::in_flight`], without subscribing to changes.
    pub fn in_flight_untracked(self, task_id: Uuid) -> bool {
        self.0.with_untracked(|m| m.get(&task_id).is_some_and(|r| r.in_flight > 0))
    }

    /// What this window knows about `task_id`'s attempts. Tracked.
    pub fn record(self, task_id: Uuid) -> Option<DeliveryRecord> {
        self.0.with(|m| m.get(&task_id).cloned())
    }

    pub fn begin(self, task_id: Uuid) {
        self.0.update(|m| m.entry(task_id).or_default().in_flight += 1);
    }

    /// The attempt was answered. Call it in the same tick as applying the
    /// answer's task record, so the card never shows the old record without
    /// the attempt in between.
    ///
    /// `attempted` is whether the answer says a pull request was actually
    /// tried; a refusal before that (the approval was not recorded, say) is
    /// not numbered as an attempt.
    pub fn finish(self, task_id: Uuid, attempted: bool) {
        let at = attempted.then(|| String::from(js_sys::Date::new_0().to_locale_time_string("en-US")));
        self.0.update(|m| {
            let record = m.entry(task_id).or_default();
            record.in_flight = record.in_flight.saturating_sub(1);
            if let Some(at) = at {
                record.finished += 1;
                record.last_finished_at = Some(at);
            }
        });
    }
}

/// Whether `task_id` is being delivered, for code that may run outside the
/// app's context (tests, detached views): no context means no attempts.
pub fn delivery_in_flight(task_id: Uuid) -> bool {
    Deliveries::get().is_some_and(|d| d.in_flight(task_id))
}

/// The attention count the open board computed for its project, so the
/// project rail shows exactly the number the header does for that project
/// instead of a read that may be a moment behind it.
#[derive(Clone, Copy)]
pub struct BoardAttention(pub RwSignal<Option<(Uuid, usize)>>);

impl BoardAttention {
    pub fn provide() {
        provide_context(Self(RwSignal::new(None)));
    }

    pub fn get() -> Option<Self> {
        use_context::<Self>()
    }
}

/// The tasks that need the user, in the order the board shows them: by
/// column, left to right, then top to bottom within a column.
pub fn attention_order(
    tasks: &[Task],
    column_rank: impl Fn(&TaskStatus) -> usize,
    in_flight: impl Fn(Uuid) -> bool,
) -> Vec<(Uuid, AttentionReason)> {
    let mut found: Vec<(usize, i32, Uuid, AttentionReason)> = tasks
        .iter()
        .filter_map(|t| {
            t.needs_you(in_flight(t.id))
                .map(|reason| (column_rank(&t.status), t.position, t.id, reason))
        })
        .collect();
    found.sort_by_key(|(column, position, id, _)| (*column, *position, *id));
    found.into_iter().map(|(_, _, id, reason)| (id, reason)).collect()
}

/// The task after `current` in `order`, wrapping around; the first one if
/// `current` is not in it any more.
pub fn next_after(order: &[Uuid], current: Option<Uuid>) -> Option<Uuid> {
    let at = current.and_then(|c| order.iter().position(|id| *id == c));
    match at {
        Some(i) => order.get((i + 1) % order.len()).copied(),
        None => order.first().copied(),
    }
}

/// Classes for a reason's chip: red for a failure, violet for a review,
/// amber for a pull request that was not created.
pub fn chip_classes(reason: AttentionReason) -> &'static str {
    match reason {
        AttentionReason::Failed => "bg-red-500/15 text-red-200 border-red-500/40",
        AttentionReason::Review => "bg-violet-500/15 text-violet-200 border-violet-400/40",
        AttentionReason::PrNotCreated => "bg-amber-500/15 text-amber-200 border-amber-400/40",
    }
}

/// "Needs you · <reason>", on a card.
#[component]
pub fn AttentionChip(reason: AttentionReason) -> impl IntoView {
    view! {
        <span
            data-testid="task-card-attention"
            data-reason=reason.as_str()
            class=format!(
                "inline-flex items-center gap-1 px-1.5 py-0.5 rounded-md border text-[10px] font-semibold {}",
                chip_classes(reason)
            )
        >
            <svg class="w-3 h-3 flex-shrink-0" aria-hidden="true" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
            </svg>
            {format!("Needs you \u{00B7} {}", reason.label())}
        </span>
    }
}

/// "Creating PR…", on a card whose pull request is being opened.
#[component]
pub fn DeliveringChip() -> impl IntoView {
    view! {
        <span
            data-testid="task-card-delivering"
            role="status"
            class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded-md border border-sky-400/40 bg-sky-500/15 text-[10px] font-semibold text-sky-200"
        >
            <svg class="w-3 h-3 animate-spin flex-shrink-0" aria-hidden="true" fill="none" viewBox="0 0 24 24">
                <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
                <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"></path>
            </svg>
            "Creating PR\u{2026}"
        </span>
    }
}

/// Bring the card for `task_id` into view in both directions and give it
/// keyboard focus, without scrolling a second time for the focus.
pub fn reveal_card(task_id: Uuid) {
    use wasm_bindgen::JsCast;
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let selector = format!("[data-card-task-id=\"{task_id}\"]");
    let Ok(Some(card)) = document.query_selector(&selector) else {
        return;
    };
    let options = web_sys::ScrollIntoViewOptions::new();
    options.set_behavior(web_sys::ScrollBehavior::Smooth);
    options.set_block(web_sys::ScrollLogicalPosition::Center);
    options.set_inline(web_sys::ScrollLogicalPosition::Center);
    card.scroll_into_view_with_scroll_into_view_options(&options);
    if let Ok(card) = card.dyn_into::<web_sys::HtmlElement>() {
        let focus = web_sys::FocusOptions::new();
        focus.set_prevent_scroll(true);
        let _ = card.focus_with_options(&focus);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(status: &str, position: i32, id: u128) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::from_u128(id), "project_id": Uuid::from_u128(99),
            "title": "t", "description": null, "status": status, "model": "m",
            "planning_mode": false, "dependencies": [], "workspace_id": null, "jj_change_id": null,
            "category": "feature", "priority": "medium", "complexity": "moderate",
            "impact": "medium", "security_severity": "none", "phase": "idle",
            "phase_progress": 0, "overall_progress": 0, "subtasks": [], "sequence_number": 1,
            "position": position,
            "github_issue_url": null, "gitlab_issue_url": null, "linear_ticket_id": null,
            "pr_url": null, "qa_signoff": null, "human_review": { "arrivals": 1, "entries": [] },
            "stuck_since": null,
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap()
    }

    fn rank(status: &TaskStatus) -> usize {
        match status {
            TaskStatus::Error => 1,
            TaskStatus::HumanReview => 5,
            _ => 0,
        }
    }

    #[test]
    fn attention_walks_the_board_left_to_right_then_top_to_bottom() {
        let tasks = vec![
            task("human_review", 1, 1),
            task("backlog", 0, 2),
            task("human_review", 0, 3),
            task("error", 7, 4),
            task("in_progress", 0, 5),
        ];
        let order = attention_order(&tasks, rank, |_| false);
        assert_eq!(
            order,
            vec![
                (Uuid::from_u128(4), AttentionReason::Failed),
                (Uuid::from_u128(3), AttentionReason::Review),
                (Uuid::from_u128(1), AttentionReason::Review),
            ]
        );

        let delivering = Uuid::from_u128(3);
        let order = attention_order(&tasks, rank, |id| id == delivering);
        assert_eq!(order.len(), 2, "a task whose pull request is being opened waits on SlashIt");
    }

    #[test]
    fn the_header_cycles_through_the_tasks_and_restarts_when_one_is_gone() {
        let order = [Uuid::from_u128(1), Uuid::from_u128(2)];
        assert_eq!(next_after(&order, None), Some(order[0]));
        assert_eq!(next_after(&order, Some(order[0])), Some(order[1]));
        assert_eq!(next_after(&order, Some(order[1])), Some(order[0]));
        assert_eq!(next_after(&order, Some(Uuid::from_u128(9))), Some(order[0]));
        assert_eq!(next_after(&[], None), None);
    }

    #[test]
    fn each_reason_has_its_own_colour() {
        assert!(chip_classes(AttentionReason::Failed).contains("red"));
        assert!(chip_classes(AttentionReason::Review).contains("violet"));
        assert!(chip_classes(AttentionReason::PrNotCreated).contains("amber"));
    }
}
