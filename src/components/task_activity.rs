//! The task drawer's Activity section: what happened to the task, oldest
//! first.
//!
//! Everything shown comes from the task record the drawer already has
//! (`activity`, `human_review`, `created_at`), so the section adds no request
//! and no poll of its own. It explains how the task got where it is; the
//! drawer's status, Needs You, review controls and actions stay the authority
//! on what can happen next.
//!
//! Details can hold text an agent or a person wrote (a command, review
//! feedback). They are rendered as text, never as markup.

use chrono::{DateTime, Datelike, FixedOffset, Utc};
use leptos::prelude::*;
use slashit_activity::Tone;

use crate::models::Task;

/// The most recent rows shown before "Show earlier" is asked for.
const VISIBLE_ROWS: usize = 40;

/// One timeline row, ready to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    pub key: String,
    pub kind: &'static str,
    /// When, compactly: a time today, a date and time otherwise.
    pub when: String,
    /// When, exactly, for the tooltip.
    pub exact: String,
    pub title: String,
    pub detail: Option<String>,
    pub tone: Tone,
    pub tool: bool,
}

/// The task's timeline as rows, in `offset`'s local time.
pub fn activity_rows(task: &Task, offset: FixedOffset, now: DateTime<Utc>) -> Vec<ActivityRow> {
    task.timeline()
        .iter()
        .map(|item| ActivityRow {
            key: item.key(),
            kind: item.kind_name(),
            when: compact_time(item.at, offset, now),
            exact: item.at.with_timezone(&offset).format("%Y-%m-%d %H:%M:%S").to_string(),
            title: item.title(),
            detail: item.detail(),
            tone: item.tone(),
            tool: item.is_tool(),
        })
        .collect()
}

/// `14:32` today, `Sep 28 14:32` earlier this year, `2025-09-28 14:32`
/// before that.
pub fn compact_time(at: DateTime<Utc>, offset: FixedOffset, now: DateTime<Utc>) -> String {
    let local = at.with_timezone(&offset);
    let today = now.with_timezone(&offset);
    if local.date_naive() == today.date_naive() {
        local.format("%H:%M").to_string()
    } else if local.year() == today.year() {
        local.format("%b %-d %H:%M").to_string()
    } else {
        local.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// The rows to render: all of them when `expanded`, otherwise the most recent
/// [`VISIBLE_ROWS`]. Also returns how many earlier rows are hidden.
pub fn visible_rows(rows: &[ActivityRow], expanded: bool) -> (&[ActivityRow], usize) {
    let hidden = if expanded { 0 } else { rows.len().saturating_sub(VISIBLE_ROWS) };
    (&rows[hidden..], hidden)
}

/// The user's local offset from UTC, as the webview reports it.
fn local_offset() -> FixedOffset {
    // `getTimezoneOffset` is UTC minus local, in minutes.
    let minutes = js_sys::Date::new_0().get_timezone_offset();
    FixedOffset::west_opt((minutes * 60.0) as i32).unwrap_or_else(|| FixedOffset::east_opt(0).unwrap())
}

fn tone_class(tone: Tone) -> &'static str {
    match tone {
        Tone::Good => "bg-emerald-400",
        Tone::Bad => "bg-red-400",
        Tone::Attention => "bg-purple-400",
        Tone::Neutral => "bg-white/40",
        Tone::Quiet => "bg-white/15",
    }
}

#[component]
pub fn TaskActivity(task: Memo<Option<Task>>) -> impl IntoView {
    let expanded = RwSignal::new(false);
    let offset = local_offset();
    // Recomputed only when the task record changes, not on a clock.
    let rows = Memo::new(move |_| {
        task.with(|t| t.as_ref().map(|t| activity_rows(t, offset, Utc::now())).unwrap_or_default())
    });

    view! {
        <section data-testid="task-drawer-timeline" class="space-y-2">
            <h3 class="text-xs font-semibold uppercase tracking-wide text-white/50">"Activity"</h3>
            {move || rows.with(|rows| {
                let (shown, hidden) = visible_rows(rows, expanded.get());
                let earlier = (hidden > 0).then(|| view! {
                    <button
                        data-testid="task-drawer-timeline-earlier"
                        class="text-xs text-blue-300 hover:text-blue-200"
                        on:click=move |_| expanded.set(true)
                    >
                        {format!("Show {hidden} earlier")}
                    </button>
                });
                let items = shown.iter().cloned().map(|row| view! { <ActivityItem row=row /> }).collect_view();
                view! {
                    {earlier}
                    <ol class="space-y-1.5">{items}</ol>
                }
            })}
        </section>
    }
}

#[component]
fn ActivityItem(row: ActivityRow) -> impl IntoView {
    let title_class = if row.tool { "text-xs text-white/50" } else { "text-sm text-white/80" };
    let detail_class = if row.tool {
        "text-xs text-white/40 font-mono break-all line-clamp-2"
    } else {
        "text-xs text-white/50 whitespace-pre-wrap break-words line-clamp-4"
    };
    view! {
        <li
            data-testid="task-drawer-timeline-row"
            data-kind=row.kind
            data-key=row.key.clone()
            class="flex gap-3"
        >
            <time
                class="w-[88px] shrink-0 text-right text-[11px] tabular-nums text-white/40 pt-0.5"
                title=row.exact.clone()
            >
                {row.when.clone()}
            </time>
            <span class=format!("mt-1.5 h-1.5 w-1.5 shrink-0 rounded-full {}", tone_class(row.tone))></span>
            <div class="min-w-0 flex-1">
                <p data-testid="task-drawer-timeline-title" class=title_class>{row.title.clone()}</p>
                {row.detail.clone().map(|d| view! {
                    <p data-testid="task-drawer-timeline-detail" class=detail_class>{d}</p>
                })}
            </div>
        </li>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use slashit_activity::{Column, Kind};

    fn task(activity: serde_json::Value, review: serde_json::Value) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "project_id": "22222222-2222-2222-2222-222222222222",
            "title": "t", "description": null, "status": "human_review", "model": "m",
            "planning_mode": false, "dependencies": [], "workspace_id": null, "jj_change_id": null,
            "category": "feature", "priority": "medium", "complexity": "moderate",
            "impact": "medium", "security_severity": "none", "phase": "complete",
            "phase_progress": 0, "overall_progress": 0, "subtasks": [], "sequence_number": 1,
            "github_issue_url": null, "gitlab_issue_url": null, "linear_ticket_id": null,
            "pr_url": null, "qa_signoff": null, "human_review": review, "stuck_since": null,
            "activity": activity,
            "created_at": "2026-09-29T14:00:00Z", "updated_at": "2026-09-29T15:00:00Z"
        }))
        .unwrap()
    }

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 29, 18, 0, 0).unwrap()
    }

    #[test]
    fn a_task_as_the_backend_sends_it_reads_as_its_history() {
        let activity = serde_json::json!([
            { "seq": 1, "at": "2026-09-29T14:32:00Z", "kind": { "type": "run_started", "run": 1 } },
            { "seq": 2, "at": "2026-09-29T14:34:00Z",
              "kind": { "type": "tool_used", "run": 1, "tool": "Bash", "detail": "cargo test -p slashit-ui", "count": 1 } },
            { "seq": 3, "at": "2026-09-29T14:38:00Z", "kind": { "type": "run_completed", "run": 1 } },
            { "seq": 4, "at": "2026-09-29T14:39:00Z", "kind": { "type": "ai_review_started", "review": 1 } },
            { "seq": 5, "at": "2026-09-29T14:46:00Z", "kind": { "type": "ai_review_approved", "review": 1 } },
            { "seq": 6, "at": "2026-09-29T14:50:00Z", "kind": { "type": "ready_for_review", "arrival": 1 } },
            { "seq": 7, "at": "2026-09-29T15:04:00Z",
              "kind": { "type": "pr_linked", "url": "https://github.com/o/r/pull/92", "number": 92 } }
        ]);
        let review = serde_json::json!({
            "arrivals": 1,
            "entries": [{ "sequence": 1, "arrival": 1, "decision": "approved", "decided_at": "2026-09-29T15:03:00Z" }]
        });
        let rows = activity_rows(&task(activity, review), utc(), now());
        let lines: Vec<(String, String, Option<String>)> =
            rows.iter().map(|r| (r.when.clone(), r.title.clone(), r.detail.clone())).collect();
        assert_eq!(
            lines,
            vec![
                ("14:00".into(), "Task created".into(), None),
                ("14:32".into(), "Coding started".into(), None),
                ("14:34".into(), "Used Bash".into(), Some("cargo test -p slashit-ui".into())),
                ("14:38".into(), "Coding finished".into(), None),
                ("14:39".into(), "AI review started".into(), None),
                ("14:46".into(), "AI review approved".into(), None),
                ("14:50".into(), "Ready for your review".into(), None),
                ("15:03".into(), "You approved the changes".into(), None),
                ("15:04".into(), "Pull request linked".into(), Some("#92".into())),
            ]
        );
        assert!(rows[2].tool && !rows[1].tool);
        assert_eq!(rows[7].kind, "approved");
        assert_eq!(rows[8].exact, "2026-09-29 15:04:00");
    }

    #[test]
    fn a_task_from_before_activity_existed_shows_what_its_record_proves() {
        let review = serde_json::json!({
            "arrivals": 2,
            "entries": [
                { "sequence": 1, "arrival": 1, "decision": "changes_requested", "feedback": "add tests", "decided_at": "2026-09-29T15:00:00Z" }
            ]
        });
        let mut json = serde_json::to_value(task(serde_json::json!([]), review)).unwrap();
        json.as_object_mut().unwrap().remove("activity");
        let old: Task = serde_json::from_value(json).unwrap();
        let rows = activity_rows(&old, utc(), now());
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, ["Task created", "You requested changes"]);
        assert_eq!(rows[1].detail.as_deref(), Some("add tests"));
    }

    #[test]
    fn times_are_local_and_compact() {
        let at = Utc.with_ymd_and_hms(2026, 9, 29, 23, 30, 0).unwrap();
        let lisbon = FixedOffset::east_opt(3600).unwrap();
        // 00:30 the next day in Lisbon, which is "today" there at 01:00.
        let later = Utc.with_ymd_and_hms(2026, 9, 30, 0, 0, 0).unwrap();
        assert_eq!(compact_time(at, lisbon, later), "00:30");
        assert_eq!(compact_time(at, utc(), later), "Sep 29 23:30");
        let next_year = Utc.with_ymd_and_hms(2027, 1, 2, 0, 0, 0).unwrap();
        assert_eq!(compact_time(at, utc(), next_year), "2026-09-29 23:30");
    }

    #[test]
    fn a_long_history_shows_its_latest_rows_until_asked_for_more() {
        let mut entries = Vec::new();
        for run in 1..=30 {
            let at = Utc.with_ymd_and_hms(2026, 9, 29, 14, 0, 0).unwrap() + chrono::Duration::minutes(run.into());
            slashit_activity::record(&mut entries, at, Kind::RunStarted { run, addressing_feedback: false });
            slashit_activity::record(&mut entries, at, Kind::RunFailed { run: Some(run), reason: "x".into() });
            slashit_activity::record(&mut entries, at, Kind::Moved { from: Column::Error, to: Column::Queue });
        }
        let rows = activity_rows(
            &task(serde_json::to_value(&entries).unwrap(), serde_json::json!({ "arrivals": 0 })),
            utc(),
            now(),
        );
        assert_eq!(rows.len(), 91);

        let (shown, hidden) = visible_rows(&rows, false);
        assert_eq!((shown.len(), hidden), (VISIBLE_ROWS, 91 - VISIBLE_ROWS));
        assert_eq!(shown.last().unwrap().title, "Retried", "the newest row is always shown");

        let (shown, hidden) = visible_rows(&rows, true);
        assert_eq!((shown.len(), hidden), (91, 0));
        assert_eq!(shown[0].title, "Task created");
    }
}
