//! A pull request's status on the board: the one signal its card shows, and
//! the section the task drawer shows.
//!
//! Everything here reads the backend's in-memory cache (`list_pr_statuses`),
//! which the board's existing refresh reads again every few seconds. Nothing
//! here asks GitHub on its own schedule; the only requests it makes are the
//! explicit Refresh a person presses.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::components::task_activity::{compact_time, local_offset_at, OffsetAt};
use crate::components::toast;
use crate::models::{
    ChecksState, ExternalRef, Mergeability, PrKey, PrState, PrStatus, PrStatusEntry, ReviewDecision, Task,
};
use crate::services::{list_pr_statuses, refresh_pr_status};

/// How old a reading may be before it is shown as stale: four missed
/// background polls, which run every thirty seconds.
pub const STALE_AFTER_SECONDS: i64 = 120;

/// The board's copy of the backend cache, for one project.
///
/// Every write goes through [`WriteOrder`]: the periodic reload of the whole
/// cache and each explicit Refresh overlap, and an answer that arrives late
/// must not put back what a newer one replaced.
#[derive(Clone, Copy)]
pub struct PrStatusBoard {
    entries: RwSignal<HashMap<PrKey, ShownPr>>,
    order: StoredValue<WriteOrder>,
    project_id: StoredValue<String>,
}

/// One entry and whether it was stale when the board last looked.
///
/// Staleness is worked out when the board refreshes, not on a clock of its
/// own, so a card changes only when what it shows changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownPr {
    pub entry: PrStatusEntry,
    pub stale: bool,
}

impl ShownPr {
    fn at(entry: PrStatusEntry, now: DateTime<Utc>) -> Self {
        let stale = is_stale(&entry, now);
        Self { entry, stale }
    }
}

impl PrStatusBoard {
    /// Provide an empty board for `project_id`.
    pub fn provide(project_id: String) -> Self {
        let board = Self {
            entries: RwSignal::new(HashMap::new()),
            order: StoredValue::new(WriteOrder::default()),
            project_id: StoredValue::new(project_id),
        };
        provide_context(board);
        board
    }

    pub fn get() -> Option<Self> {
        use_context::<Self>()
    }

    /// What is known about `key`. Tracked.
    pub fn shown(self, key: &PrKey) -> Option<ShownPr> {
        self.entries.with(|m| m.get(key).cloned())
    }

    /// A ticket for a request about to be sent, whose answer is then given
    /// to [`Self::replace`] or [`Self::merge`]. Taken before the request, so
    /// the answer is ordered by when it was asked.
    pub fn issue(self) -> Option<u64> {
        self.order.try_update_value(WriteOrder::issue)
    }

    /// Read the backend cache again. Memory only on the backend side.
    pub fn reload(self) {
        let Some(project_id) = self.project_id.try_get_value() else {
            return;
        };
        if project_id.is_empty() {
            return;
        }
        let Some(ticket) = self.issue() else {
            return;
        };
        spawn_local(async move {
            if let Ok(entries) = list_pr_statuses(project_id).await {
                self.replace(ticket, entries, Utc::now());
            }
        });
    }

    /// Take `entries`, the answer to `ticket`, as the whole cache.
    pub fn replace(self, ticket: u64, entries: Vec<PrStatusEntry>, now: DateTime<Utc>) {
        self.write(|order, current| order.replace(ticket, current, entries, now));
    }

    /// Take `entries`, the answer to `ticket`, as some of the cache.
    pub fn merge(self, ticket: u64, entries: Vec<PrStatusEntry>, now: DateTime<Utc>) {
        self.write(|order, current| order.merge(ticket, current, entries, now));
    }

    /// Publish what `write` makes of the board, only if it is a real change.
    fn write(self, write: impl FnOnce(&mut WriteOrder, &HashMap<PrKey, ShownPr>) -> HashMap<PrKey, ShownPr>) {
        let Some(current) = self.entries.try_get_untracked() else {
            return;
        };
        let Some(next) = self.order.try_update_value(|order| write(order, &current)) else {
            return;
        };
        if next != current {
            self.entries.try_set(next);
        }
    }
}

/// Ask GitHub about `task_id`'s open pull requests now and show the answer on
/// `board`. The backend acts on it exactly as its background poll would: an
/// open pull request never moves the task or ends its work, and only a task
/// in a delivery column (Pull Request Created, Human Review, Done) is
/// finished by a merge or given a closure as its error.
///
/// Answers with the refreshed entries; a pull request GitHub could not be
/// asked about has its reason in its entry's `error`.
pub async fn refresh_task_prs(board: Option<PrStatusBoard>, task_id: String) -> Result<Vec<PrStatusEntry>, String> {
    let ticket = board.and_then(PrStatusBoard::issue);
    let entries = refresh_pr_status(task_id).await?;
    if let (Some(board), Some(ticket)) = (board, ticket) {
        board.merge(ticket, entries.clone(), Utc::now());
    }
    Ok(entries)
}

/// What a Refresh that answered `entries` tells the person who pressed it.
pub fn refresh_outcome(entries: &[PrStatusEntry]) -> Result<&'static str, String> {
    if let Some(error) = entries.iter().find_map(|e| e.error.as_ref()) {
        return Err(error.message.clone());
    }
    Ok(if entries.is_empty() { "No open pull request to refresh" } else { "PR state refreshed" })
}

/// Orders the answers written to the board.
///
/// Two things are ordered, each by what can actually tell:
///
/// - Which reading of a pull request is newer is the backend's to say. Its
///   cache never stores an older attempt over a newer one, so an entry with
///   a later `attempted_at` is newer whenever it was asked for, and one with
///   an earlier one is older, even in the answer to a later request.
/// - Whether a pull request is on the board at all is said by the answers
///   themselves: a full snapshot that leaves one out removes it, and any
///   answer that includes one keeps it. Those are ordered by ticket, taken
///   when the request was sent, so a snapshot asked for before a newer
///   answer spoke about a pull request neither removes it nor brings back
///   one that newer answer removed.
///
/// A ticket orders when a request was sent, not when the backend read its
/// cache. So a pull request's very first reading, from a Refresh sent just
/// before a reload that read the cache before that reading landed, can be
/// removed by the reload's answer; the next reload shows it again.
#[derive(Debug, Default)]
pub struct WriteOrder {
    issued: u64,
    /// For each pull request any answer has spoken about, the latest ticket
    /// among them, including one that removed it.
    spoken: HashMap<PrKey, u64>,
}

impl WriteOrder {
    pub fn issue(&mut self) -> u64 {
        self.issued += 1;
        self.issued
    }

    /// Whether the answer to `ticket` is the latest word on whether `key` is
    /// on the board; if so, it becomes that.
    fn speaks(&mut self, key: &PrKey, ticket: u64) -> bool {
        let spoken = self.spoken.entry(key.clone()).or_default();
        let latest = ticket > *spoken;
        *spoken = (*spoken).max(ticket);
        latest
    }

    /// The board after `entries`, the answer to `ticket`, which is the whole
    /// cache.
    pub fn replace(
        &mut self,
        ticket: u64,
        current: &HashMap<PrKey, ShownPr>,
        entries: Vec<PrStatusEntry>,
        now: DateTime<Utc>,
    ) -> HashMap<PrKey, ShownPr> {
        let mut incoming: HashMap<PrKey, PrStatusEntry> = entries.into_iter().map(|e| (e.key(), e)).collect();
        let mut next = HashMap::new();
        for (key, shown) in current {
            match incoming.remove(key) {
                Some(entry) => {
                    self.speaks(key, ticket);
                    next.insert(key.clone(), newer(shown, entry, now));
                }
                // Left out: removed, unless a later answer has spoken about it.
                None => {
                    if !self.speaks(key, ticket) {
                        next.insert(key.clone(), ShownPr::at(shown.entry.clone(), now));
                    }
                }
            }
        }
        for (key, entry) in incoming {
            // New to the board: added, unless a later answer removed it.
            if self.speaks(&key, ticket) {
                next.insert(key, ShownPr::at(entry, now));
            }
        }
        next
    }

    /// The board after `entries`, the answer to `ticket`, which is some of
    /// the cache.
    pub fn merge(
        &mut self,
        ticket: u64,
        current: &HashMap<PrKey, ShownPr>,
        entries: Vec<PrStatusEntry>,
        now: DateTime<Utc>,
    ) -> HashMap<PrKey, ShownPr> {
        let mut next = current.clone();
        for entry in entries {
            let key = entry.key();
            let latest = self.speaks(&key, ticket);
            match current.get(&key) {
                Some(shown) => {
                    next.insert(key, newer(shown, entry, now));
                }
                None if latest => {
                    next.insert(key, ShownPr::at(entry, now));
                }
                None => {}
            }
        }
        next
    }
}

/// Whichever of what is shown and `entry` the backend read later.
fn newer(shown: &ShownPr, entry: PrStatusEntry, now: DateTime<Utc>) -> ShownPr {
    if entry.attempted_at > shown.entry.attempted_at {
        ShownPr::at(entry, now)
    } else {
        ShownPr::at(shown.entry.clone(), now)
    }
}

/// Whether the last good reading is too old, or the latest attempt failed.
pub fn is_stale(entry: &PrStatusEntry, now: DateTime<Utc>) -> bool {
    entry.error.is_some()
        || entry.fetched_at.is_none_or(|at| (now - at).num_seconds() > STALE_AFTER_SECONDS)
}

/// The one thing a card says about its pull request, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardSignal {
    Merged,
    Closed,
    /// Nothing read yet: the card shows only the number.
    Unread,
    CiFailing,
    Conflict,
    ChangesRequested,
    CiRunning,
    ReadyToMerge,
    CiPassed,
    Open,
    CiUnknown,
}

impl CardSignal {
    pub fn label(self) -> &'static str {
        match self {
            Self::Merged => "Merged",
            Self::Closed => "Closed",
            Self::Unread => "",
            Self::CiFailing => "CI failing",
            Self::Conflict => "Conflict",
            Self::ChangesRequested => "Changes requested",
            Self::CiRunning => "CI running",
            Self::ReadyToMerge => "Ready to merge",
            Self::CiPassed => "CI passed",
            Self::Open => "Open",
            Self::CiUnknown => "CI ?",
        }
    }

    /// Stable name for tests and styling hooks.
    pub fn key(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Unread => "unread",
            Self::CiFailing => "ci_failing",
            Self::Conflict => "conflict",
            Self::ChangesRequested => "changes_requested",
            Self::CiRunning => "ci_running",
            Self::ReadyToMerge => "ready_to_merge",
            Self::CiPassed => "ci_passed",
            Self::Open => "open",
            Self::CiUnknown => "ci_unknown",
        }
    }

    fn class(self) -> &'static str {
        match self {
            Self::Merged => "bg-purple-500/10 text-purple-300",
            Self::Closed => "bg-white/[0.06] text-white/50",
            Self::Unread | Self::Open | Self::CiUnknown => "bg-white/[0.06] text-white/60",
            Self::CiFailing | Self::Conflict => "bg-red-500/15 text-red-300",
            Self::ChangesRequested => "bg-amber-500/15 text-amber-300",
            Self::CiRunning => "bg-sky-500/15 text-sky-300",
            Self::ReadyToMerge | Self::CiPassed => "bg-emerald-500/15 text-emerald-300",
        }
    }

    /// Whether the signal can go stale. Merged and closed are final, however
    /// they were learned, and nothing asks about them again; not having read
    /// anything yet has nothing to dim.
    fn can_go_stale(self) -> bool {
        !matches!(self, Self::Unread | Self::Merged | Self::Closed)
    }
}

/// The card's signal. `recorded` is the state the task records for the pull
/// request; `status` is the last good reading, if any.
pub fn card_signal(recorded: Option<&str>, status: Option<&PrStatus>) -> CardSignal {
    match recorded {
        Some(s) if s.eq_ignore_ascii_case("MERGED") => return CardSignal::Merged,
        Some(s) if s.eq_ignore_ascii_case("CLOSED") => return CardSignal::Closed,
        _ => {}
    }
    let Some(status) = status else {
        return CardSignal::Unread;
    };
    match status.state {
        PrState::Merged => return CardSignal::Merged,
        PrState::Closed => return CardSignal::Closed,
        PrState::Open | PrState::Unknown => {}
    }
    if status.checks == ChecksState::Failing {
        CardSignal::CiFailing
    } else if status.mergeable == Some(Mergeability::Conflicting) {
        CardSignal::Conflict
    } else if status.review_decision == Some(ReviewDecision::ChangesRequested) {
        CardSignal::ChangesRequested
    } else if status.checks == ChecksState::Pending {
        CardSignal::CiRunning
    } else if status.checks == ChecksState::Passing
        && status.review_decision == Some(ReviewDecision::Approved)
        && status.mergeable == Some(Mergeability::Mergeable)
    {
        CardSignal::ReadyToMerge
    } else if status.checks == ChecksState::Passing {
        CardSignal::CiPassed
    } else if status.checks == ChecksState::NoChecks {
        CardSignal::Open
    } else {
        CardSignal::CiUnknown
    }
}

/// Whether a card dims its signal: only a signal read from the cache, when
/// that reading is stale.
fn card_stale(recorded: Option<&str>, shown: Option<&ShownPr>) -> bool {
    let terminal = recorded.is_some_and(|s| s.eq_ignore_ascii_case("MERGED") || s.eq_ignore_ascii_case("CLOSED"));
    !terminal
        && shown.is_some_and(|s| {
            s.stale && card_signal(recorded, s.entry.status.as_ref()).can_go_stale()
        })
}

pub fn state_label(state: PrState) -> &'static str {
    match state {
        PrState::Open => "Open",
        PrState::Merged => "Merged",
        PrState::Closed => "Closed",
        PrState::Unknown => "Unknown",
    }
}

pub fn checks_label(checks: ChecksState) -> &'static str {
    match checks {
        ChecksState::Passing => "Passing",
        ChecksState::Failing => "Failing",
        ChecksState::Pending => "Running",
        ChecksState::NoChecks => "No checks reported",
        ChecksState::Unknown => "Unknown",
    }
}

pub fn review_label(review: Option<ReviewDecision>) -> &'static str {
    match review {
        Some(ReviewDecision::Approved) => "Approved",
        Some(ReviewDecision::ChangesRequested) => "Changes requested",
        Some(ReviewDecision::ReviewRequired) => "Review required",
        None => "No review required",
    }
}

pub fn merge_label(mergeable: Option<Mergeability>) -> &'static str {
    match mergeable {
        Some(Mergeability::Mergeable) => "Mergeable",
        Some(Mergeability::Conflicting) => "Conflicts with the base branch",
        Some(Mergeability::Unknown) => "GitHub is still checking",
        None => "Unknown",
    }
}

/// The failing checks to name, and how many more there are, if any.
pub fn failing_summary(status: &PrStatus) -> (Vec<String>, Option<String>) {
    let shown = status.failing_checks.iter().take(3).cloned().collect::<Vec<_>>();
    let more = (status.failing_check_count as usize).saturating_sub(shown.len());
    let rest = (more > 0).then(|| if more == 1 { "and 1 more".to_string() } else { format!("and {more} more") });
    (shown, rest)
}

/// How long ago, roughly.
pub fn age_label(seconds: i64) -> String {
    match seconds.max(0) {
        0..=9 => "just now".to_string(),
        s @ 10..=59 => format!("{s}s ago"),
        s @ 60..=3599 => format!("{}m ago", s / 60),
        s @ 3600..=86_399 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// Everything the card's signal leaves out, for its tooltip.
pub fn card_tooltip(
    number: u32,
    recorded: Option<&str>,
    shown: Option<&ShownPr>,
    now: DateTime<Utc>,
    offset_at: OffsetAt<'_>,
) -> String {
    let mut lines = vec![format!("Pull request #{number}")];
    let entry = shown.map(|s| &s.entry);
    match entry.and_then(|e| e.status.as_ref()) {
        Some(status) => {
            lines.push(format!("State: {}", state_label(status.state)));
            let mut checks = format!("Checks: {}", checks_label(status.checks));
            let (names, rest) = failing_summary(status);
            if !names.is_empty() {
                checks.push_str(&format!(" ({}", names.join(", ")));
                if let Some(rest) = rest {
                    checks.push_str(&format!(", {rest}"));
                }
                checks.push(')');
            }
            lines.push(checks);
            lines.push(format!("Review: {}", review_label(status.review_decision)));
            lines.push(format!("Merge: {}", merge_label(status.mergeable)));
        }
        None => {
            if let Some(state) = recorded {
                lines.push(format!("State: {}", state.to_lowercase()));
            }
            lines.push("Not checked yet".to_string());
        }
    }
    if let Some(at) = entry.and_then(|e| e.fetched_at) {
        let stale = if card_stale(recorded, shown) { " (stale)" } else { "" };
        lines.push(format!(
            "Checked {}{stale}, at {}",
            age_label((now - at).num_seconds()),
            compact_time(at, offset_at, now)
        ));
    }
    if let Some(error) = entry.and_then(|e| e.error.as_ref()) {
        lines.push(format!("Last refresh failed: {}", error.message));
    }
    lines.join("\n")
}

/// The pull request badge on a card.
#[component]
pub fn PrBadge(url: String, number: u32, repo: String, state: Option<String>) -> impl IntoView {
    let key = PrKey { repo, number };
    let board = PrStatusBoard::get();
    let shown = Memo::new(move |_| board.and_then(|b| b.shown(&key)));
    let recorded = StoredValue::new(state);

    let signal = Memo::new(move |_| {
        recorded.with_value(|r| shown.with(|s| card_signal(r.as_deref(), s.as_ref().and_then(|s| s.entry.status.as_ref()))))
    });
    let stale = Memo::new(move |_| recorded.with_value(|r| shown.with(|s| card_stale(r.as_deref(), s.as_ref()))));

    view! {
        <a
            href=url
            target="_blank"
            data-testid="task-card-pr"
            data-signal=move || signal.get().key()
            data-stale=move || if stale.get() { "true" } else { "false" }
            title=move || recorded.with_value(|r| shown.with(|s| card_tooltip(number, r.as_deref(), s.as_ref(), Utc::now(), &local_offset_at)))
            class=move || format!(
                "flex items-center gap-1 px-2 py-1 text-[10px] rounded-md transition-colors hover:brightness-125 {}{}",
                signal.get().class(),
                if stale.get() { " opacity-50" } else { "" },
            )
            on:click=move |e: web_sys::MouseEvent| e.stop_propagation()
        >
            <svg class="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 9l3 3-3 3m5 0h3M5 20h14a2 2 0 002-2V6a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z" />
            </svg>
            {format!("PR #{number}")}
            {move || {
                let s = signal.get();
                (s != CardSignal::Unread).then(|| view! {
                    <span data-testid="task-card-pr-signal" class="font-medium">{format!("\u{00B7} {}", s.label())}</span>
                })
            }}
        </a>
    }
}

/// The pull requests linked to `task` that it does not record as merged or
/// closed.
pub fn open_prs(task: &Task) -> Vec<PrKey> {
    task.external_refs
        .iter()
        .filter_map(|r| match r {
            ExternalRef::GithubPr { number, repo, state, .. }
                if !state.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("MERGED") || s.eq_ignore_ascii_case("CLOSED")) =>
            {
                Some(PrKey { repo: repo.clone(), number: *number })
            }
            _ => None,
        })
        .collect()
}

/// The first GitHub pull request linked to `task`, with its recorded state.
fn linked_pr(task: &Task) -> Option<(String, PrKey, Option<String>)> {
    task.external_refs.iter().find_map(|r| match r {
        ExternalRef::GithubPr { url, number, repo, state } => {
            Some((url.clone(), PrKey { repo: repo.clone(), number: *number }, state.clone()))
        }
        _ => None,
    })
}

/// The drawer's Pull request section, shown when the task has one.
#[component]
pub fn PullRequestSection(
    task_id: Uuid,
    task: Memo<Option<Task>>,
    /// The drawer's clock.
    now: RwSignal<DateTime<Utc>>,
    /// Read the task list again after a refresh, which may have finished the
    /// task.
    refresh_tasks: Callback<()>,
) -> impl IntoView {
    let board = PrStatusBoard::get();
    let pr = Memo::new(move |_| task.with(|t| t.as_ref().and_then(linked_pr)));
    let refreshing = RwSignal::new(false);
    // Bumped after a refresh, so the restack notice looks at the parent again.
    let restack_reload = RwSignal::new(0u32);
    let branch = Memo::new(move |_| task.with(|t| t.as_ref().and_then(|t| t.branch_name.clone())));

    let on_refresh = move |_| {
        if refreshing.get_untracked() {
            return;
        }
        refreshing.set(true);
        spawn_local(async move {
            if let Err(e) = refresh_task_prs(board, task_id.to_string()).await {
                toast::error(format!("Could not refresh the pull request: {e}"));
            }
            refresh_tasks.try_run(());
            restack_reload.try_update(|n| *n += 1);
            refreshing.try_set(false);
        });
    };

    move || {
        pr.get().map(|(url, key, recorded)| {
            let number = key.number;
            let shown = Memo::new(move |_| board.and_then(|b| b.shown(&key)));
            let recorded_terminal = recorded
                .as_deref()
                .filter(|s| s.eq_ignore_ascii_case("MERGED") || s.eq_ignore_ascii_case("CLOSED"))
                .map(|s| if s.eq_ignore_ascii_case("MERGED") { PrState::Merged } else { PrState::Closed });
            let recorded_open = recorded.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("OPEN"));
            let state = Memo::new(move |_| {
                recorded_terminal
                    .or_else(|| shown.with(|s| s.as_ref().and_then(|s| s.entry.status.as_ref().map(|st| st.state))))
                    .unwrap_or(if recorded_open { PrState::Open } else { PrState::Unknown })
            });
            let terminal = move || matches!(state.get(), PrState::Merged | PrState::Closed);

            view! {
                <section data-testid="task-drawer-pr" class="rounded-lg border border-white/10 bg-white/[0.03] p-3 space-y-2">
                    <div class="flex items-center gap-2">
                        <h3 class="text-xs font-semibold uppercase tracking-wide text-white/50">
                            {format!("Pull request #{number}")}
                        </h3>
                        <a
                            href=url
                            target="_blank"
                            data-testid="task-drawer-pr-link"
                            class="text-xs text-sky-300 hover:underline"
                            title="Open on GitHub"
                        >
                            "Open on GitHub"
                        </a>
                        <span
                            data-testid="task-drawer-pr-state"
                            data-state=move || format!("{:?}", state.get()).to_lowercase()
                            class="ml-auto px-2 py-0.5 rounded-md text-[11px] bg-white/[0.06] text-white/70"
                        >
                            {move || state_label(state.get())}
                        </span>
                    </div>

                    <Show when=move || !terminal()>
                        <crate::components::restack_published::RestackNotice
                            task_id=task_id
                            branch=branch
                            reload=restack_reload
                            refresh=Callback::new(move |_| {
                                refresh_tasks.try_run(());
                                restack_reload.try_update(|n| *n += 1);
                            })
                        />
                        {move || shown.get().map(|s| (s.entry, s.stale)).map(|(entry, stale)| match entry.status.clone() {
                            Some(status) => view! {
                                <StatusRows status=status stale=stale />
                                {entry.error.clone().map(|e| view! {
                                    <p data-testid="task-drawer-pr-error" class="text-xs text-amber-300 break-words">
                                        {format!("Showing the last good reading. The latest refresh failed: {}", e.message)}
                                    </p>
                                })}
                            }.into_any(),
                            None => view! {
                                <p data-testid="task-drawer-pr-error" role="alert" class="text-sm text-red-300 break-words">
                                    {entry.error.map(|e| format!("Could not read this pull request from GitHub: {}", e.message))
                                        .unwrap_or_else(|| "Not checked yet.".to_string())}
                                </p>
                            }.into_any(),
                        }).unwrap_or_else(|| view! {
                            <p data-testid="task-drawer-pr-unchecked" class="text-xs text-white/40">"Not checked yet."</p>
                        }.into_any())}

                        <div class="flex items-center gap-2 pt-1">
                            <span data-testid="task-drawer-pr-checked" class="text-xs text-white/40">
                                {move || shown.with(|s| {
                                    match s.as_ref().and_then(|s| s.entry.fetched_at) {
                                        Some(at) => {
                                            let age = age_label((now.get() - at).num_seconds());
                                            if s.as_ref().is_some_and(|s| s.stale) { format!("As of {age}") } else { format!("Checked {age}") }
                                        }
                                        None => String::new(),
                                    }
                                })}
                            </span>
                            <button
                                data-testid="task-drawer-pr-refresh"
                                class="ml-auto px-2 py-1 rounded-md text-xs bg-white/5 text-white/70 hover:bg-white/10 disabled:opacity-50"
                                disabled=move || refreshing.get()
                                title="Ask GitHub for this pull request's status now"
                                on:click=on_refresh
                            >
                                {move || if refreshing.get() { "Refreshing…" } else { "Refresh" }}
                            </button>
                        </div>
                    </Show>
                </section>
            }
        })
    }
}

/// Checks, review and merge for an open pull request.
#[component]
fn StatusRows(status: PrStatus, stale: bool) -> impl IntoView {
    let (names, rest) = failing_summary(&status);
    let checks_class = match status.checks {
        ChecksState::Failing => "text-red-300",
        ChecksState::Passing => "text-emerald-300",
        ChecksState::Pending => "text-sky-300",
        ChecksState::NoChecks | ChecksState::Unknown => "text-white/60",
    };
    view! {
        <dl data-testid="task-drawer-pr-status" data-stale=if stale { "true" } else { "false" } class=format!("grid grid-cols-[5rem_1fr] gap-x-3 gap-y-1 text-sm {}", if stale { "opacity-60" } else { "" })>
            <dt class="text-white/40">"Checks"</dt>
            <dd data-testid="task-drawer-pr-checks" data-checks=format!("{:?}", status.checks).to_lowercase() class=checks_class>
                {checks_label(status.checks)}
                {(!names.is_empty()).then(|| view! {
                    <ul class="mt-1 space-y-0.5 text-xs text-red-200/90">
                        {names.into_iter().map(|name| view! {
                            <li data-testid="task-drawer-pr-failing-check" class="break-words">{name}</li>
                        }).collect_view()}
                        {rest.map(|rest| view! { <li data-testid="task-drawer-pr-failing-more" class="text-white/40">{rest}</li> })}
                    </ul>
                })}
            </dd>
            <dt class="text-white/40">"Review"</dt>
            <dd data-testid="task-drawer-pr-review" class="text-white/70">{review_label(status.review_decision)}</dd>
            <dt class="text-white/40">"Merge"</dt>
            <dd data-testid="task-drawer-pr-merge" class="text-white/70">{merge_label(status.mergeable)}</dd>
        </dl>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{PrFetchError, PrFetchErrorKind};

    fn status(checks: ChecksState, review: Option<ReviewDecision>, merge: Option<Mergeability>) -> PrStatus {
        PrStatus {
            state: PrState::Open,
            checks,
            failing_checks: vec![],
            failing_check_count: 0,
            review_decision: review,
            mergeable: merge,
        }
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).unwrap()
    }

    fn entry(status: Option<PrStatus>, fetched: Option<i64>, error: bool) -> PrStatusEntry {
        PrStatusEntry {
            repo: "o/r".to_string(),
            number: 1,
            status,
            fetched_at: fetched.map(at),
            attempted_at: at(fetched.unwrap_or(0)),
            error: error.then(|| PrFetchError { kind: PrFetchErrorKind::Failed, message: "GitHub is down".to_string() }),
        }
    }

    /// Pull request `number`, as the backend read it at `read`, with `checks`.
    fn read(number: u32, read: i64, checks: ChecksState) -> PrStatusEntry {
        PrStatusEntry { number, ..entry(Some(status(checks, None, None)), Some(read), false) }
    }

    fn checks_of(board: &HashMap<PrKey, ShownPr>, number: u32) -> Option<ChecksState> {
        board
            .get(&PrKey { repo: "o/r".to_string(), number })
            .and_then(|s| s.entry.status.as_ref())
            .map(|s| s.checks)
    }

    #[test]
    fn a_reload_answered_after_a_newer_refresh_does_not_undo_it() {
        let mut order = WriteOrder::default();
        let now = at(100);
        let first = order.issue();
        let board = order.replace(first, &HashMap::new(), vec![read(1, 0, ChecksState::Pending)], now);

        // 1. The periodic reload is sent.
        let reload = order.issue();
        // 2. A Refresh is sent after it and answered first, with a newer
        //    reading of #1 and #2, which the cache did not have yet.
        let refresh = order.issue();
        let board = order.merge(
            refresh,
            &board,
            vec![read(1, 50, ChecksState::Failing), read(2, 50, ChecksState::Passing)],
            now,
        );
        // 3. The reload arrives with the cache as it was before the Refresh.
        let board = order.replace(reload, &board, vec![read(1, 0, ChecksState::Pending)], now);

        // 4. The Refresh's answer is still what the board shows.
        assert_eq!(checks_of(&board, 1), Some(ChecksState::Failing), "an older value does not replace a newer one");
        assert_eq!(checks_of(&board, 2), Some(ChecksState::Passing), "an older snapshot does not remove a newer key");
    }

    #[test]
    fn a_reload_sent_during_a_refresh_does_not_undo_it_either() {
        let mut order = WriteOrder::default();
        let now = at(100);
        let first = order.issue();
        let board = order.replace(first, &HashMap::new(), vec![read(1, 0, ChecksState::Pending)], now);

        // The Refresh is sent first; the reload after it, but it reads the
        // cache before the Refresh's answer lands there.
        let refresh = order.issue();
        let reload = order.issue();
        let board = order.merge(refresh, &board, vec![read(1, 50, ChecksState::Failing)], now);
        let board = order.replace(reload, &board, vec![read(1, 0, ChecksState::Pending)], now);
        assert_eq!(checks_of(&board, 1), Some(ChecksState::Failing), "the backend's reading time decides");

        // Answered in the other order, the Refresh's newer reading still wins.
        let refresh = order.issue();
        let reload = order.issue();
        let board = order.replace(reload, &board, vec![read(1, 50, ChecksState::Failing)], now);
        let board = order.merge(refresh, &board, vec![read(1, 80, ChecksState::Passing)], now);
        assert_eq!(checks_of(&board, 1), Some(ChecksState::Passing));
    }

    #[test]
    fn which_pull_requests_are_on_the_board_follows_the_latest_answer() {
        let mut order = WriteOrder::default();
        let now = at(100);
        // Sent first, answered last.
        let late = order.issue();
        let first = order.issue();
        let board = order.replace(first, &HashMap::new(), vec![read(1, 0, ChecksState::Pending)], now);
        // Since then #1 was unlinked and #2 linked.
        let later = order.issue();
        let board = order.replace(later, &board, vec![read(2, 10, ChecksState::Passing)], now);
        assert_eq!((checks_of(&board, 1), checks_of(&board, 2)), (None, Some(ChecksState::Passing)));

        let board = order.replace(late, &board, vec![read(1, 0, ChecksState::Pending)], now);
        assert_eq!(checks_of(&board, 1), None, "a late snapshot does not bring back what a newer one removed");
        assert_eq!(checks_of(&board, 2), Some(ChecksState::Passing), "nor remove what a newer one added");
        let board = order.merge(late, &board, vec![read(1, 90, ChecksState::Failing)], now);
        assert_eq!(checks_of(&board, 1), None, "nor does a late Refresh answer");

        let newest = order.issue();
        let board = order.merge(newest, &board, vec![read(1, 95, ChecksState::Failing)], now);
        assert_eq!(checks_of(&board, 1), Some(ChecksState::Failing), "a newer answer does");
    }

    #[test]
    fn a_refresh_says_whether_github_answered() {
        assert_eq!(refresh_outcome(&[]), Ok("No open pull request to refresh"));
        assert_eq!(refresh_outcome(&[read(1, 0, ChecksState::Passing)]), Ok("PR state refreshed"));
        let failed = entry(Some(status(ChecksState::Passing, None, None)), Some(0), true);
        assert_eq!(refresh_outcome(&[read(2, 0, ChecksState::Passing), failed]), Err("GitHub is down".to_string()));
    }

    #[test]
    fn the_card_shows_the_most_urgent_signal() {
        use ChecksState::*;
        use Mergeability::*;
        use ReviewDecision::*;
        let open = Some("OPEN");
        let failing_everything = status(Failing, Some(ChangesRequested), Some(Conflicting));
        let table: Vec<(Option<&str>, Option<PrStatus>, CardSignal)> = vec![
            (Some("MERGED"), Some(failing_everything.clone()), CardSignal::Merged),
            (Some("merged"), None, CardSignal::Merged),
            (Some("CLOSED"), Some(failing_everything.clone()), CardSignal::Closed),
            (open, None, CardSignal::Unread),
            (None, None, CardSignal::Unread),
            (open, Some(PrStatus { state: PrState::Merged, ..failing_everything.clone() }), CardSignal::Merged),
            (open, Some(PrStatus { state: PrState::Closed, ..failing_everything.clone() }), CardSignal::Closed),
            (open, Some(failing_everything.clone()), CardSignal::CiFailing),
            (open, Some(status(Pending, Some(ChangesRequested), Some(Conflicting))), CardSignal::Conflict),
            (open, Some(status(Pending, Some(ChangesRequested), Some(Mergeable))), CardSignal::ChangesRequested),
            (open, Some(status(Pending, Some(Approved), Some(Mergeable))), CardSignal::CiRunning),
            (open, Some(status(Passing, Some(Approved), Some(Mergeable))), CardSignal::ReadyToMerge),
            (open, Some(status(Passing, Some(Approved), Some(Mergeability::Unknown))), CardSignal::CiPassed),
            (open, Some(status(Passing, Some(ReviewRequired), Some(Mergeable))), CardSignal::CiPassed),
            (open, Some(status(Passing, None, Some(Mergeable))), CardSignal::CiPassed),
            (open, Some(status(NoChecks, Some(Approved), Some(Mergeable))), CardSignal::Open),
            (open, Some(status(ChecksState::Unknown, Some(Approved), Some(Mergeable))), CardSignal::CiUnknown),
            (open, Some(PrStatus { state: PrState::Unknown, ..status(Passing, None, None) }), CardSignal::CiPassed),
        ];
        for (recorded, status, expected) in table {
            assert_eq!(card_signal(recorded, status.as_ref()), expected, "{recorded:?} {status:?}");
        }
    }

    #[test]
    fn only_passing_is_shown_as_good_news() {
        let green = |s: CardSignal| s.class().contains("emerald");
        for checks in [ChecksState::NoChecks, ChecksState::Unknown, ChecksState::Pending] {
            let signal = card_signal(Some("OPEN"), Some(&status(checks, Some(ReviewDecision::Approved), Some(Mergeability::Mergeable))));
            assert!(!green(signal), "{checks:?} shows as {signal:?}");
        }
        let pending = card_signal(Some("OPEN"), Some(&status(ChecksState::Pending, None, None)));
        assert!(!pending.class().contains("red"), "running is never failing");
        // Review required, or no review policy at all, has no chip of its own.
        for review in [Some(ReviewDecision::ReviewRequired), None] {
            let signal = card_signal(Some("OPEN"), Some(&status(ChecksState::Passing, review, None)));
            assert_eq!(signal, CardSignal::CiPassed, "{review:?}");
        }
    }

    #[test]
    fn a_reading_goes_stale_after_four_missed_polls_or_a_failed_refresh() {
        let passing = Some(status(ChecksState::Passing, None, None));
        assert!(!is_stale(&entry(passing.clone(), Some(0), false), at(STALE_AFTER_SECONDS)));
        assert!(is_stale(&entry(passing.clone(), Some(0), false), at(STALE_AFTER_SECONDS + 1)));
        assert!(is_stale(&entry(passing.clone(), Some(0), true), at(1)));
        assert!(is_stale(&entry(None, None, true), at(1)));
    }

    #[test]
    fn a_stale_card_keeps_its_signal_but_a_final_one_never_dims() {
        let failing = Some(status(ChecksState::Failing, None, None));
        let shown = ShownPr { entry: entry(failing.clone(), Some(0), true), stale: true };
        assert_eq!(card_signal(Some("OPEN"), shown.entry.status.as_ref()), CardSignal::CiFailing);
        assert!(card_stale(Some("OPEN"), Some(&shown)));
        assert!(!card_stale(Some("MERGED"), Some(&shown)));
        // Merged, heard from GitHub rather than recorded, is final too.
        let merged = ShownPr {
            entry: entry(Some(PrStatus { state: PrState::Merged, ..status(ChecksState::Passing, None, None) }), Some(0), false),
            stale: true,
        };
        assert!(!card_stale(Some("OPEN"), Some(&merged)));
        let fresh = ShownPr { entry: entry(failing, Some(0), false), stale: false };
        assert!(!card_stale(Some("OPEN"), Some(&fresh)));
        assert!(!card_stale(Some("OPEN"), None));

        let utc = |_| chrono::FixedOffset::east_opt(0).unwrap();
        let tip = card_tooltip(1, Some("OPEN"), Some(&shown), at(300), &utc);
        assert!(tip.contains("Checks: Failing"), "{tip}");
        assert!(tip.contains("Review: No review required"), "{tip}");
        assert!(tip.contains("Merge: Unknown"), "{tip}");
        assert!(tip.contains("Checked 5m ago (stale)"), "{tip}");
        assert!(tip.contains("Last refresh failed: GitHub is down"), "{tip}");
        let unread = card_tooltip(1, Some("OPEN"), None, at(0), &utc);
        assert!(unread.contains("Not checked yet"), "{unread}");
    }

    #[test]
    fn every_drawer_state_has_its_words() {
        assert_eq!(
            [PrState::Open, PrState::Merged, PrState::Closed, PrState::Unknown].map(state_label),
            ["Open", "Merged", "Closed", "Unknown"]
        );
        assert_eq!(
            [ChecksState::Passing, ChecksState::Failing, ChecksState::Pending, ChecksState::NoChecks, ChecksState::Unknown]
                .map(checks_label),
            ["Passing", "Failing", "Running", "No checks reported", "Unknown"]
        );
        assert_eq!(
            [Some(ReviewDecision::Approved), Some(ReviewDecision::ChangesRequested), Some(ReviewDecision::ReviewRequired), None]
                .map(review_label),
            ["Approved", "Changes requested", "Review required", "No review required"]
        );
        assert_eq!(
            [Some(Mergeability::Mergeable), Some(Mergeability::Conflicting), Some(Mergeability::Unknown), None].map(merge_label),
            ["Mergeable", "Conflicts with the base branch", "GitHub is still checking", "Unknown"]
        );
    }

    #[test]
    fn failing_checks_are_named_and_the_rest_counted() {
        let mut s = status(ChecksState::Failing, None, None);
        s.failing_checks = vec!["lint".into(), "test".into(), "docs".into()];
        s.failing_check_count = 5;
        assert_eq!(failing_summary(&s), (vec!["lint".into(), "test".into(), "docs".into()], Some("and 2 more".into())));
        s.failing_check_count = 4;
        assert_eq!(failing_summary(&s).1.as_deref(), Some("and 1 more"));
        s.failing_checks = vec!["lint".into()];
        s.failing_check_count = 1;
        assert_eq!(failing_summary(&s), (vec!["lint".into()], None));
        // Failing checks with no names are still counted.
        s.failing_checks = vec![];
        s.failing_check_count = 2;
        assert_eq!(failing_summary(&s), (vec![], Some("and 2 more".into())));
    }

    #[test]
    fn ages_read_roughly() {
        assert_eq!(age_label(-3), "just now");
        assert_eq!(age_label(5), "just now");
        assert_eq!(age_label(42), "42s ago");
        assert_eq!(age_label(125), "2m ago");
        assert_eq!(age_label(7200), "2h ago");
        assert_eq!(age_label(200_000), "2d ago");
    }
}
