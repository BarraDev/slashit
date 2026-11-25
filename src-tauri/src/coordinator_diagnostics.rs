// SPDX-License-Identifier: MPL-2.0
//! Content-free diagnostic lines for Coordinator turns.
//!
//! Each turn writes `[coordinator-context]` once after its context is built (or
//! once for a refusal), `[coordinator-invocation]` at every provider invocation
//! and every prompt refused before one, and `[coordinator-turn]` once at the end.
//! Lines carry only ids, counts, sizes, durations, enum values and timestamps:
//! never message text, prompts, projections, titles, summaries, Worker results,
//! read results, provider error text, paths or project names. The formatters
//! are pure, so they are tested without a provider.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::conversation_context::{ContextError, ContextSite, ContextSnapshot};
use crate::coordinator_reads::{DriveEvent, InvocationReport, Outcome, PromptSize};

/// What a turn was started for: the Human message entry, or the continuation's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    HumanMessage(Uuid),
    ReturnedAction(Uuid),
}

/// The identifiers every line of one turn shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Correlation {
    pub conversation_id: Uuid,
    pub conversation_revision: u64,
    pub site: ContextSite,
    pub trigger: Trigger,
}

/// Writes one diagnostic line to the instance log. Tests capture lines on their
/// own thread instead, so the turn's real early exits can be checked.
pub fn emit(line: &str) {
    #[cfg(test)]
    if capture::record(line) {
        return;
    }
    eprintln!("{line}");
}

#[cfg(test)]
pub mod capture {
    use std::cell::RefCell;

    thread_local! {
        static LINES: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    }

    /// Starts capturing this thread's diagnostic lines.
    pub fn start() {
        LINES.with(|lines| *lines.borrow_mut() = Some(Vec::new()));
    }

    /// Stops capturing and returns the lines written since `start`.
    pub fn finish() -> Vec<String> {
        LINES.with(|lines| lines.borrow_mut().take().unwrap_or_default())
    }

    pub(super) fn record(line: &str) -> bool {
        LINES.with(|lines| match lines.borrow_mut().as_mut() {
            Some(captured) => {
                captured.push(line.to_owned());
                true
            }
            None => false,
        })
    }
}

fn site_name(site: ContextSite) -> &'static str {
    match site {
        ContextSite::Ordinary => "ordinary",
        ContextSite::Continuation => "continuation",
    }
}

fn percent(used: usize, budget: usize) -> String {
    if budget == 0 {
        return "0.0".to_owned();
    }
    format!("{:.1}", used as f64 * 100.0 / budget as f64)
}

fn optional_time(time: Option<DateTime<Utc>>) -> String {
    time.map_or_else(|| "none".to_owned(), |time| time.to_rfc3339())
}

impl Correlation {
    fn fields(&self) -> String {
        let trigger = match self.trigger {
            Trigger::HumanMessage(id) => format!("message_entry_id={id}"),
            Trigger::ReturnedAction(id) => format!("action_id={id}"),
        };
        format!(
            "conversation_id={} conversation_revision={} site={} {trigger}",
            self.conversation_id,
            self.conversation_revision,
            site_name(self.site),
        )
    }
}

/// The `[coordinator-context]` line for a context that was built.
pub fn context_line(correlation: &Correlation, snapshot: &ContextSnapshot, construction_ms: u128) -> String {
    format!(
        "[coordinator-context] {} construction_ms={construction_ms} budget_chars={} fixed_chars={} reserved_chars={} \
         projection_chars={} used_chars={} utilization_percent={} included_entries={} omitted_count={} \
         omitted_first_created_at={} omitted_last_created_at={} shortened_fields={} \
         open_proposals_included={} open_proposals_total={} tasks_included={} tasks_total={}",
        correlation.fields(),
        snapshot.budget_chars,
        snapshot.fixed_chars,
        snapshot.reserved_chars,
        snapshot.projection_chars,
        snapshot.used_chars,
        percent(snapshot.used_chars, snapshot.budget_chars),
        snapshot.included_entry_ids.len(),
        snapshot.omitted_count,
        optional_time(snapshot.omitted_first_created_at),
        optional_time(snapshot.omitted_last_created_at),
        snapshot.shortened_fields,
        snapshot.open_proposals_included,
        snapshot.open_proposals_total,
        snapshot.tasks_included,
        snapshot.tasks_total,
    )
}

/// The `[coordinator-context]` line for a context the Builder refused.
pub fn context_refusal_line(correlation: &Correlation, error: &ContextError, construction_ms: u128) -> String {
    let ContextError::RequiredExceedsBudget {
        component,
        required_chars,
        budget_chars,
        fixed_chars,
        read_reserve_chars,
        turn_chars,
        structure_chars,
    } = error;
    format!(
        "[coordinator-context] {} outcome={} construction_ms={construction_ms} component={component} \
         required_chars={required_chars} budget_chars={budget_chars} fixed_chars={fixed_chars} \
         read_reserve_chars={read_reserve_chars} turn_chars={turn_chars} structure_chars={structure_chars}",
        correlation.fields(),
        Outcome::OverBudget.as_str(),
    )
}

fn size_fields(size: &PromptSize) -> String {
    format!(
        "prompt_chars={} base_prompt_chars={} read_evidence_chars={} measured_chars={} limit_chars={} \
         utilization_percent={} structured_reads={} refusal_notice_included={}",
        size.prompt_chars,
        size.base_prompt_chars,
        size.prompt_chars.saturating_sub(size.base_prompt_chars),
        size.measured_chars,
        size.limit_chars,
        percent(size.measured_chars, size.limit_chars),
        size.structured_reads,
        size.refusal_notice_included,
    )
}

fn invocation_fields(report: &InvocationReport) -> String {
    format!(
        "sequence={} outcome={} read_refused={} provider_ms={} {}",
        report.sequence,
        report.outcome.as_str(),
        report.read_refused,
        report.provider_ms,
        size_fields(&report.size),
    )
}

/// The `[coordinator-invocation]` line for one drive event: an invocation, or a
/// prompt refused before it was sent (which has no sequence number).
pub fn invocation_line(correlation: &Correlation, event: &DriveEvent) -> String {
    let fields = match event {
        DriveEvent::Invocation(report) => invocation_fields(report),
        DriveEvent::Refused(size) => format!("outcome={} {}", Outcome::OverBudget.as_str(), size_fields(size)),
    };
    format!("[coordinator-invocation] {} {fields}", correlation.fields())
}

/// Counts over one turn, folded from its drive events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnTally {
    pub invocations: usize,
    pub structured_reads: usize,
    pub read_budget_exhausted: bool,
    pub prompt_refused: bool,
    pub last_outcome: Option<Outcome>,
}

impl TurnTally {
    pub fn record(&mut self, event: &DriveEvent) {
        match event {
            DriveEvent::Refused(_) => self.prompt_refused = true,
            DriveEvent::Invocation(report) => {
                self.invocations += 1;
                if report.outcome == Outcome::ReadRequested && !report.read_refused {
                    self.structured_reads += 1;
                }
                self.read_budget_exhausted |= report.read_refused;
                self.last_outcome = Some(report.outcome);
            }
        }
    }

    /// The turn's outcome once its drive has ended. `answered` is whether the
    /// drive returned an output.
    pub fn final_outcome(&self, answered: bool) -> Outcome {
        if answered {
            Outcome::Answered
        } else if self.prompt_refused {
            Outcome::OverBudget
        } else if self.last_outcome == Some(Outcome::Cancelled) {
            Outcome::Cancelled
        } else {
            Outcome::Failed
        }
    }
}

/// The `[coordinator-turn]` line, written once when a turn ends.
pub fn turn_end_line(correlation: &Correlation, tally: &TurnTally, outcome: Outcome) -> String {
    format!(
        "[coordinator-turn] {} outcome={} invocations={} structured_reads={} read_budget_exhausted={}",
        correlation.fields(),
        outcome.as_str(),
        tally.invocations,
        tally.structured_reads,
        tally.read_budget_exhausted,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::TimeZone;

    use super::*;
    use crate::conversation_context::{build_coordinator_context, ContextBudget, ContextRequest, TurnInput};
    use crate::coordinator_reads::{drive, PromptLimits};
    use crate::domain::conversation::{Conversation, CoordinatorOutput};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    const SENTINEL: &str = "SENTINEL-do-not-log-7f3a";
    const ROOT: &str = "/srv/SENTINEL-root-7f3a/private-checkout";

    /// Splits a line into its tag and its `key=value` fields.
    fn parse(line: &str) -> (&str, BTreeMap<&str, &str>) {
        let mut parts = line.split_whitespace();
        let tag = parts.next().unwrap();
        let fields = parts.filter_map(|part| part.split_once('=')).collect();
        (tag, fields)
    }

    fn correlation(trigger: Trigger) -> Correlation {
        Correlation {
            conversation_id: Uuid::nil(),
            conversation_revision: 9,
            site: ContextSite::Ordinary,
            trigger,
        }
    }

    fn snapshot() -> ContextSnapshot {
        ContextSnapshot {
            conversation_id: Uuid::nil(),
            conversation_revision: 9,
            site: ContextSite::Ordinary,
            budget_chars: 1_000,
            fixed_chars: 200,
            reserved_chars: 300,
            projection_chars: 250,
            used_chars: 450,
            included_entry_ids: vec![Uuid::nil(), Uuid::nil()],
            omitted_count: 3,
            omitted_first_created_at: Some(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
            omitted_last_created_at: Some(Utc.with_ymd_and_hms(2026, 1, 3, 3, 4, 5).unwrap()),
            shortened_fields: 4,
            open_proposals_total: 5,
            open_proposals_included: 2,
            tasks_included: 6,
            tasks_total: 7,
        }
    }

    fn size() -> PromptSize {
        PromptSize {
            base_prompt_chars: 100,
            prompt_chars: 160,
            measured_chars: 170,
            limit_chars: 400,
            structured_reads: 2,
            refusal_notice_included: true,
        }
    }

    fn invocation(outcome: Outcome, read_refused: bool) -> DriveEvent {
        DriveEvent::Invocation(InvocationReport {
            sequence: 2,
            size: size(),
            read_refused,
            provider_ms: 31,
            outcome,
        })
    }

    #[test]
    fn context_line_has_every_field_with_its_value() {
        let line = context_line(&correlation(Trigger::HumanMessage(Uuid::nil())), &snapshot(), 12);
        let (tag, fields) = parse(&line);
        assert_eq!(tag, "[coordinator-context]");
        let nil = Uuid::nil().to_string();
        let expected = [
            ("conversation_id", nil.as_str()),
            ("conversation_revision", "9"),
            ("site", "ordinary"),
            ("message_entry_id", nil.as_str()),
            ("construction_ms", "12"),
            ("budget_chars", "1000"),
            ("fixed_chars", "200"),
            ("reserved_chars", "300"),
            ("projection_chars", "250"),
            ("used_chars", "450"),
            ("utilization_percent", "45.0"),
            ("included_entries", "2"),
            ("omitted_count", "3"),
            ("omitted_first_created_at", "2026-01-02T03:04:05+00:00"),
            ("omitted_last_created_at", "2026-01-03T03:04:05+00:00"),
            ("shortened_fields", "4"),
            ("open_proposals_included", "2"),
            ("open_proposals_total", "5"),
            ("tasks_included", "6"),
            ("tasks_total", "7"),
        ];
        assert_eq!(fields.len(), expected.len(), "{line}");
        for (key, value) in expected {
            assert_eq!(fields.get(key), Some(&value), "{key} in {line}");
        }
    }

    #[test]
    fn context_line_names_the_action_for_a_continuation_and_none_without_omissions() {
        let mut snap = snapshot();
        snap.site = ContextSite::Continuation;
        snap.omitted_count = 0;
        snap.omitted_first_created_at = None;
        snap.omitted_last_created_at = None;
        let action = Uuid::from_u128(7);
        let correlation = Correlation { site: ContextSite::Continuation, ..correlation(Trigger::ReturnedAction(action)) };
        let line = context_line(&correlation, &snap, 0);
        let (_, fields) = parse(&line);
        assert_eq!(fields.get("site"), Some(&"continuation"));
        assert_eq!(fields.get("action_id"), Some(&action.to_string().as_str()));
        assert!(!fields.contains_key("message_entry_id"));
        assert_eq!(fields.get("omitted_first_created_at"), Some(&"none"));
        assert_eq!(fields.get("omitted_last_created_at"), Some(&"none"));
    }

    #[test]
    fn context_refusal_line_has_every_numeric_field() {
        let error = ContextError::RequiredExceedsBudget {
            component: "current_message",
            required_chars: 2_000,
            budget_chars: 1_000,
            fixed_chars: 100,
            read_reserve_chars: 200,
            turn_chars: 1_500,
            structure_chars: 50,
        };
        let line = context_refusal_line(&correlation(Trigger::HumanMessage(Uuid::nil())), &error, 4);
        let (tag, fields) = parse(&line);
        assert_eq!(tag, "[coordinator-context]");
        assert_eq!(fields.get("outcome"), Some(&"over_budget"));
        assert_eq!(fields.get("construction_ms"), Some(&"4"));
        assert_eq!(fields.get("component"), Some(&"current_message"));
        assert_eq!(fields.get("required_chars"), Some(&"2000"));
        assert_eq!(fields.get("budget_chars"), Some(&"1000"));
        assert_eq!(fields.get("fixed_chars"), Some(&"100"));
        assert_eq!(fields.get("read_reserve_chars"), Some(&"200"));
        assert_eq!(fields.get("turn_chars"), Some(&"1500"));
        assert_eq!(fields.get("structure_chars"), Some(&"50"));
    }

    #[test]
    fn invocation_line_has_every_field_with_its_value() {
        let line = invocation_line(&correlation(Trigger::HumanMessage(Uuid::nil())), &invocation(Outcome::ReadRequested, false));
        let (tag, fields) = parse(&line);
        assert_eq!(tag, "[coordinator-invocation]");
        let expected = [
            ("sequence", "2"),
            ("outcome", "read_requested"),
            ("read_refused", "false"),
            ("provider_ms", "31"),
            ("prompt_chars", "160"),
            ("base_prompt_chars", "100"),
            ("read_evidence_chars", "60"),
            ("measured_chars", "170"),
            ("limit_chars", "400"),
            ("utilization_percent", "42.5"),
            ("structured_reads", "2"),
            ("refusal_notice_included", "true"),
        ];
        for (key, value) in expected {
            assert_eq!(fields.get(key), Some(&value), "{key} in {line}");
        }
        assert_eq!(fields.len(), expected.len() + 4, "correlation fields are four");
    }

    #[test]
    fn every_outcome_name_is_stable() {
        let names: Vec<&str> = [
            Outcome::Answered,
            Outcome::ReadRequested,
            Outcome::Failed,
            Outcome::Cancelled,
            Outcome::OverBudget,
        ]
        .map(Outcome::as_str)
        .into();
        assert_eq!(names, ["answered", "read_requested", "failed", "cancelled", "over_budget"]);
    }

    #[test]
    fn refused_prompt_line_has_no_sequence_and_is_over_budget() {
        let line = invocation_line(&correlation(Trigger::ReturnedAction(Uuid::nil())), &DriveEvent::Refused(size()));
        let (_, fields) = parse(&line);
        assert_eq!(fields.get("outcome"), Some(&"over_budget"));
        assert!(!fields.contains_key("sequence"));
        assert!(!fields.contains_key("provider_ms"));
        assert_eq!(fields.get("measured_chars"), Some(&"170"));
        assert_eq!(fields.get("limit_chars"), Some(&"400"));
        assert_eq!(fields.get("action_id"), Some(&Uuid::nil().to_string().as_str()));
    }

    #[test]
    fn turn_tally_counts_reads_and_exhaustion_and_maps_the_final_outcome() {
        let mut tally = TurnTally::default();
        tally.record(&invocation(Outcome::ReadRequested, false));
        tally.record(&invocation(Outcome::ReadRequested, false));
        tally.record(&invocation(Outcome::ReadRequested, true));
        tally.record(&invocation(Outcome::Answered, false));
        assert_eq!(tally.invocations, 4);
        assert_eq!(tally.structured_reads, 2);
        assert!(tally.read_budget_exhausted);
        assert_eq!(tally.final_outcome(true), Outcome::Answered);
        assert_eq!(tally.final_outcome(false), Outcome::Failed);

        let line = turn_end_line(&correlation(Trigger::HumanMessage(Uuid::nil())), &tally, Outcome::Answered);
        let (tag, fields) = parse(&line);
        assert_eq!(tag, "[coordinator-turn]");
        assert_eq!(fields.get("outcome"), Some(&"answered"));
        assert_eq!(fields.get("invocations"), Some(&"4"));
        assert_eq!(fields.get("structured_reads"), Some(&"2"));
        assert_eq!(fields.get("read_budget_exhausted"), Some(&"true"));
    }

    #[test]
    fn a_refused_or_cancelled_turn_with_no_invocation_says_so() {
        let mut refused = TurnTally::default();
        refused.record(&DriveEvent::Refused(size()));
        assert_eq!(refused.invocations, 0);
        assert_eq!(refused.final_outcome(false), Outcome::OverBudget);

        let mut cancelled = TurnTally::default();
        cancelled.record(&invocation(Outcome::Cancelled, false));
        assert_eq!(cancelled.final_outcome(false), Outcome::Cancelled);
        assert_eq!(cancelled.structured_reads, 0);
    }

    /// A conversation and one turn whose every free-text field carries the sentinel,
    /// with a project root that is a distinctive path.
    fn sentinel_conversation() -> (Conversation, Vec<Value>) {
        let mut conversation = Conversation::new(Uuid::new_v4());
        conversation.push(crate::domain::conversation::Role::Human, crate::domain::conversation::EntryKind::HumanMessage {
            text: format!("message {SENTINEL}"),
        });
        let task = json!({"id": Uuid::new_v4(), "title": format!("title {SENTINEL}"), "status": "backlog"});
        (conversation, vec![task])
    }

    #[tokio::test]
    async fn no_line_carries_message_title_worker_result_read_result_or_project_root() {
        let (conversation, tasks) = sentinel_conversation();
        let message_entry = conversation.entries.last().unwrap().id;
        let budget = ContextBudget::default();
        let started = std::time::Instant::now();
        let built = build_coordinator_context(&ContextRequest {
            conversation: &conversation,
            project_name: "Project named SENTINEL",
            project_root: Some(ROOT),
            turn: TurnInput::Ordinary { message: &format!("message {SENTINEL}"), tasks: &tasks },
            budget,
            fixed_chars: 0,
            read_reserve_chars: 0,
        })
        .unwrap();
        let correlation = Correlation {
            conversation_id: conversation.id,
            conversation_revision: conversation.revision,
            site: built.snapshot.site,
            trigger: Trigger::HumanMessage(message_entry),
        };
        let mut lines = vec![context_line(&correlation, &built.snapshot, started.elapsed().as_millis())];

        // The continuation site carries the Worker result and the returned action's text.
        let action = Uuid::new_v4();
        let continuation = build_coordinator_context(&ContextRequest {
            conversation: &conversation,
            project_name: "Project named SENTINEL",
            project_root: Some(ROOT),
            turn: TurnInput::Continuation(crate::conversation_context::ReturnedAction {
                action_id: action,
                target_task_id: Uuid::new_v4(),
                target_task_title: &format!("title {SENTINEL}"),
                explanation: &format!("explanation {SENTINEL}"),
                approved_request: Some(&format!("request {SENTINEL}")),
                worker_result: Some(&format!("result {SENTINEL}")),
            }),
            budget,
            fixed_chars: 0,
            read_reserve_chars: 0,
        })
        .unwrap();
        lines.push(context_line(
            &Correlation { site: continuation.snapshot.site, trigger: Trigger::ReturnedAction(action), ..correlation },
            &continuation.snapshot,
            0,
        ));

        // A read result carries the sentinel; the drive reports every invocation.
        let read_sentinel = format!(r#"{{"type":"inspect_task","target_task_id":"{}"}}"#, Uuid::new_v4());
        let script = Arc::new(Mutex::new(vec![read_sentinel, r#"{"type":"reply","text":"ok"}"#.to_owned()].into_iter()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let log = events.clone();
        let result = drive(
            "BASE",
            PromptLimits::UNBOUNDED,
            move |_prompt, _guard: Option<()>| {
                let next = script.lock().unwrap().next().unwrap();
                async move { Ok((Ok(next), ())) }
            },
            |output| async move {
                match output {
                    CoordinatorOutput::InspectTask { .. } => Ok(json!({"title": format!("read {SENTINEL}")})),
                    other => Err(other),
                }
            },
            move |event| log.lock().unwrap().push(event),
        )
        .await;
        assert!(result.is_ok());
        let mut tally = TurnTally::default();
        for event in events.lock().unwrap().iter() {
            tally.record(event);
            lines.push(invocation_line(&correlation, event));
        }
        lines.push(turn_end_line(&correlation, &tally, tally.final_outcome(true)));
        assert!(lines.len() >= 4);
        for line in &lines {
            assert!(line.starts_with('['), "{line}");
            assert!(!line.contains(SENTINEL), "sentinel leaked: {line}");
            assert!(!line.contains("SENTINEL-root"), "project root leaked: {line}");
            assert!(!line.contains("private-checkout"), "project root leaked: {line}");
            assert!(!line.contains("Project named"), "project name leaked: {line}");
        }
    }
}
