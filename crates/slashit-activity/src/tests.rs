use super::*;
use chrono::TimeZone;

fn t(minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 29, 14, minute, 0).unwrap()
}

fn titles(items: &[Item<'_>]) -> Vec<String> {
    items.iter().map(Item::title).collect()
}

#[test]
fn rows_at_the_same_instant_keep_a_stored_order() {
    let mut entries = Vec::new();
    record(&mut entries, t(5), Kind::RunStarted { run: 1, addressing_feedback: false });
    record(&mut entries, t(5), Kind::RunCompleted { run: 1 });
    let decisions = [Decision { sequence: 1, at: t(5), approved: true, feedback: None }];

    let first = titles(&timeline(t(5), decisions, &entries));
    assert_eq!(
        first,
        ["Task created", "Coding started", "Coding finished", "You approved the changes"]
    );

    // The same record, read back in another order, gives the same timeline.
    let mut shuffled = entries.clone();
    shuffled.reverse();
    assert_eq!(titles(&timeline(t(5), decisions, &shuffled)), first);
}

#[test]
fn a_new_task_has_only_its_creation() {
    assert_eq!(titles(&timeline(t(0), [], &[])), ["Task created"]);
}

#[test]
fn a_start_reads_as_queue_then_run() {
    let mut entries = Vec::new();
    record(&mut entries, t(1), Kind::Moved { from: Column::Backlog, to: Column::Queue });
    let run = next_run(&entries);
    record(&mut entries, t(2), Kind::RunStarted { run, addressing_feedback: false });
    assert_eq!(
        titles(&timeline(t(0), [], &entries)),
        ["Task created", "Added to the queue", "Coding started"]
    );
}

#[test]
fn a_failure_stays_in_history_after_a_retry() {
    let mut entries = Vec::new();
    record(&mut entries, t(1), Kind::RunStarted { run: 1, addressing_feedback: false });
    record(&mut entries, t(2), Kind::RunFailed { run: Some(1), reason: "claude exited\nwith 1".into() });
    record(&mut entries, t(3), Kind::Moved { from: Column::Error, to: Column::Queue });
    let run = next_run(&entries);
    assert_eq!(run, 2);
    record(&mut entries, t(4), Kind::RunStarted { run, addressing_feedback: false });
    record(&mut entries, t(5), Kind::RunCompleted { run: 2 });

    let items = timeline(t(0), [], &entries);
    assert_eq!(
        titles(&items),
        [
            "Task created",
            "Coding started",
            "Coding failed",
            "Retried",
            "Coding started (attempt 2)",
            "Coding finished"
        ]
    );
    assert_eq!(items[2].detail().as_deref(), Some("claude exited with 1"));
    assert_eq!(items[2].tone(), Tone::Bad);
}

#[test]
fn each_ai_review_cycle_is_its_own() {
    let mut entries = Vec::new();
    for (minute, review) in [(1, next_review(&[])), (10, 2)] {
        record(&mut entries, t(minute), Kind::AiReviewStarted { review });
        record(&mut entries, t(minute + 1), Kind::AiReviewChangesRequested { review, issues: 2 });
        record(&mut entries, t(minute + 2), Kind::AiFixStarted { review });
        record(&mut entries, t(minute + 3), Kind::AiFixApplied { review });
    }
    assert_eq!(next_review(&entries), 3);
    record(&mut entries, t(20), Kind::AiReviewStarted { review: 3 });
    record(&mut entries, t(21), Kind::AiReviewApproved { review: 3 });
    let items = timeline(t(0), [], &entries);
    assert_eq!(
        titles(&items)[1..],
        [
            "AI review started",
            "AI review requested changes",
            "Fixing AI review findings",
            "AI review fixes applied",
            "AI review started (attempt 2)",
            "AI review requested changes",
            "Fixing AI review findings",
            "AI review fixes applied",
            "AI review started (attempt 3)",
            "AI review approved",
        ]
    );
    assert_eq!(items[2].detail().as_deref(), Some("2 issues found"));
}

/// A review skipped without starting still takes its number, so the next
/// review's verdict is its own and is not mistaken for a repeat.
#[test]
fn a_skipped_review_does_not_swallow_the_next_verdict() {
    let mut entries = Vec::new();
    let first = next_review(&entries);
    record(&mut entries, t(1), Kind::AiReviewSkipped { review: first, reason: "no changes to review".into() });
    let second = next_review(&entries);
    assert_eq!((first, second), (1, 2));
    record(&mut entries, t(2), Kind::AiReviewStarted { review: second });
    assert!(record(&mut entries, t(3), Kind::AiReviewApproved { review: second }));
    assert_eq!(
        titles(&timeline(t(0), [], &entries))[1..],
        ["AI review skipped", "AI review started (attempt 2)", "AI review approved"]
    );
}

#[test]
fn review_decisions_interleave_with_the_runs_they_caused() {
    let mut entries = Vec::new();
    record(&mut entries, t(1), Kind::ReadyForReview { arrival: 1 });
    record(&mut entries, t(3), Kind::RunStarted { run: 2, addressing_feedback: true });
    record(&mut entries, t(4), Kind::ReadyForReview { arrival: 2 });
    let decisions = [
        Decision { sequence: 1, at: t(2), approved: false, feedback: Some("handle empty input") },
        Decision { sequence: 2, at: t(5), approved: true, feedback: None },
    ];
    let items = timeline(t(0), decisions, &entries);
    assert_eq!(
        titles(&items),
        [
            "Task created",
            "Ready for your review",
            "You requested changes",
            "Coding started on your feedback",
            "Ready for your review",
            "You approved the changes"
        ]
    );
    assert_eq!(items[2].detail().as_deref(), Some("handle empty input"));
}

#[test]
fn delivery_retries_add_failures_and_one_pull_request_never_a_second_approval() {
    let mut entries = Vec::new();
    record(&mut entries, t(2), Kind::DeliveryFailed { reason: "gh: HTTP 422".into() });
    record(&mut entries, t(3), Kind::DeliveryFailed { reason: "gh: HTTP 422".into() });
    let url = "https://github.com/o/r/pull/92".to_string();
    assert!(record(&mut entries, t(4), Kind::PrLinked { url: url.clone(), number: Some(92) }));
    // Linking the same pull request again (a rediscovery, a refresh) is the
    // same milestone.
    assert!(!record(&mut entries, t(5), Kind::PrLinked { url, number: Some(92) }));

    let decisions = [Decision { sequence: 1, at: t(1), approved: true, feedback: None }];
    let items = timeline(t(0), decisions, &entries);
    assert_eq!(
        titles(&items),
        [
            "Task created",
            "You approved the changes",
            "Pull request not created",
            "Pull request not created",
            "Pull request linked"
        ]
    );
    assert_eq!(items[4].detail().as_deref(), Some("#92"));
}

#[test]
fn once_only_milestones_are_recorded_once() {
    let mut entries = Vec::new();
    assert!(record(&mut entries, t(1), Kind::RunCompleted { run: 1 }));
    assert!(!record(&mut entries, t(2), Kind::RunCompleted { run: 1 }));
    assert!(!record(&mut entries, t(2), Kind::RunFailed { run: Some(1), reason: "late".into() }));
    assert!(record(&mut entries, t(3), Kind::PrMerged { number: 7 }));
    assert!(!record(&mut entries, t(4), Kind::PrMerged { number: 7 }));
    assert!(record(&mut entries, t(5), Kind::ReadyForReview { arrival: 1 }));
    assert!(!record(&mut entries, t(6), Kind::ReadyForReview { arrival: 1 }));
    assert_eq!(entries.len(), 3);

    // Repeating kinds are recorded every time they happen.
    assert!(record(&mut entries, t(7), Kind::Moved { from: Column::Error, to: Column::Queue }));
    assert!(record(&mut entries, t(8), Kind::Moved { from: Column::Error, to: Column::Queue }));
    assert_eq!(entries.len(), 5);
}

#[test]
fn repeated_tool_calls_compact_and_each_run_is_capped() {
    let mut entries = Vec::new();
    record_tool(&mut entries, t(1), 1, "Bash", Some("cargo test -p slashit-ui"));
    record_tool(&mut entries, t(1), 1, "Bash", Some("cargo test -p slashit-ui"));
    record_tool(&mut entries, t(2), 1, "Read", Some("src/lib.rs"));
    let items = timeline(t(0), [], &entries);
    assert_eq!(titles(&items)[1..], ["Used Bash ×2", "Used Read"]);
    assert_eq!(items[1].detail().as_deref(), Some("cargo test -p slashit-ui"));
    assert!(items[1].is_tool());

    for i in 0..(MAX_TOOL_ENTRIES_PER_RUN + 5) {
        record_tool(&mut entries, t(3), 1, "Edit", Some(&format!("src/f{i}.rs")));
    }
    let listed = entries.iter().filter(|e| matches!(e.kind, Kind::ToolUsed { .. })).count();
    assert_eq!(listed, MAX_TOOL_ENTRIES_PER_RUN);
    assert!(entries.iter().any(|e| e.kind == Kind::ToolsOmitted { run: 1, count: 7 }));

    // A new run starts its own allowance.
    record_tool(&mut entries, t(4), 2, "Edit", Some("src/f0.rs"));
    assert!(matches!(entries.last().unwrap().kind, Kind::ToolUsed { run: 2, .. }));
}

#[test]
fn a_long_history_drops_tool_calls_before_milestones() {
    let mut entries = Vec::new();
    for run in 1..=15 {
        record(&mut entries, t(0), Kind::RunStarted { run, addressing_feedback: false });
        for i in 0..MAX_TOOL_ENTRIES_PER_RUN {
            record_tool(&mut entries, t(0), run, "Read", Some(&format!("f{i}")));
        }
        record(&mut entries, t(0), Kind::RunCompleted { run });
    }
    assert_eq!(entries.len(), MAX_ENTRIES);
    let milestones = entries.iter().filter(|e| !e.kind.is_tool()).count();
    assert_eq!(milestones, 30, "every run's start and end is kept");
    // Sequence numbers are never reused, even after drops.
    let mut seqs: Vec<u32> = entries.iter().map(|e| e.seq).collect();
    seqs.dedup();
    assert_eq!(seqs.len(), MAX_ENTRIES);
}

#[test]
fn reasons_are_one_bounded_line() {
    let mut entries = Vec::new();
    let long = format!("first line\n{}", "x".repeat(2 * MAX_REASON_CHARS));
    record(&mut entries, t(1), Kind::RunFailed { run: None, reason: long });
    let Kind::RunFailed { reason, .. } = &entries[0].kind else { unreachable!() };
    assert!(!reason.contains('\n'));
    assert_eq!(reason.chars().count(), MAX_REASON_CHARS);
    assert!(reason.starts_with("first line x") && reason.ends_with('…'));
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Holder {
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Vec::is_empty")]
    activity: Vec<Entry>,
}

#[test]
fn entries_round_trip_through_toml_and_json() {
    let mut entries = Vec::new();
    record(&mut entries, t(1), Kind::RunStarted { run: 1, addressing_feedback: true });
    record_tool(&mut entries, t(2), 1, "Bash", Some("say \"hi\""));
    record_tool(&mut entries, t(2), 1, "Glob", None);
    record(&mut entries, t(3), Kind::RunFailed { run: None, reason: "no worktree".into() });
    record(&mut entries, t(4), Kind::PrLinked { url: "https://x/pull/1".into(), number: None });
    record(&mut entries, t(5), Kind::Moved { from: Column::HumanReview, to: Column::Done });
    let holder = Holder { activity: entries };

    let text = toml::to_string_pretty(&holder).unwrap();
    assert_eq!(toml::from_str::<Holder>(&text).unwrap(), holder, "{text}");
    let json = serde_json::to_string(&holder).unwrap();
    assert_eq!(serde_json::from_str::<Holder>(&json).unwrap(), holder);
}

#[test]
fn a_record_without_activity_or_with_unknown_entries_still_loads() {
    assert_eq!(toml::from_str::<Holder>("").unwrap(), Holder { activity: vec![] });
    assert_eq!(serde_json::from_str::<Holder>("{}").unwrap(), Holder { activity: vec![] });
    assert_eq!(toml::to_string(&Holder { activity: vec![] }).unwrap(), "");

    let json = r#"{"activity":[
        {"seq":1,"at":"2026-09-29T14:01:00Z","kind":{"type":"from_a_newer_version","x":1}},
        {"seq":2,"at":"2026-09-29T14:02:00Z","kind":{"type":"run_completed","run":1}}
    ]}"#;
    let loaded: Holder = serde_json::from_str(json).unwrap();
    assert_eq!(loaded.activity.len(), 1);
    assert_eq!(loaded.activity[0].kind, Kind::RunCompleted { run: 1 });
}

#[test]
fn a_rows_kind_name_is_its_stored_type() {
    let kinds = [
        Kind::Moved { from: Column::Backlog, to: Column::Queue },
        Kind::RunStarted { run: 1, addressing_feedback: false },
        Kind::ToolUsed { run: 1, tool: "Bash".into(), detail: None, count: 1 },
        Kind::ToolsOmitted { run: 1, count: 1 },
        Kind::RunCompleted { run: 1 },
        Kind::RunFailed { run: None, reason: String::new() },
        Kind::Stopped { from: Column::InProgress },
        Kind::Interrupted { from: Column::InProgress },
        Kind::AiReviewStarted { review: 1 },
        Kind::AiReviewApproved { review: 1 },
        Kind::AiReviewChangesRequested { review: 1, issues: 0 },
        Kind::AiReviewFailed { review: 1, reason: String::new() },
        Kind::AiReviewSkipped { review: 1, reason: String::new() },
        Kind::AiFixStarted { review: 1 },
        Kind::AiFixApplied { review: 1 },
        Kind::AiFixUnchanged { review: 1 },
        Kind::AiFixFailed { review: 1, reason: String::new() },
        Kind::ReadyForReview { arrival: 1 },
        Kind::DeliveryFailed { reason: String::new() },
        Kind::PrLinked { url: String::new(), number: None },
        Kind::PrMerged { number: 1 },
        Kind::PrClosed { number: 1 },
    ];
    let entries: Vec<Entry> =
        kinds.iter().enumerate().map(|(i, kind)| Entry { seq: i as u32 + 1, at: t(1), kind: kind.clone() }).collect();
    for item in timeline(t(0), [], &entries).iter().skip(1) {
        let Row::Recorded(kind) = &item.row else { unreachable!() };
        let tag = serde_json::to_value(kind).unwrap()["type"].as_str().unwrap().to_string();
        assert_eq!(item.kind_name(), tag);
    }
    let keys: Vec<String> = timeline(t(0), [], &entries).iter().map(Item::key).collect();
    assert_eq!(keys[..3], ["created", "entry-1", "entry-2"]);
}
