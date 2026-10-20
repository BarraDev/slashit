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

/// What the backend's Task board reads: no `lenient`, so nothing is skipped.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct StrictHolder {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    activity: Vec<Entry>,
}

#[test]
fn a_strict_list_keeps_every_entry_in_order_and_refuses_what_it_cannot_keep() {
    let known = r#"activity = [
        { seq = 1, at = "2026-09-29T14:01:00Z", kind = { type = "run_completed", run = 1 } },
        { seq = 2, at = "2026-09-29T14:02:00Z", kind = { type = "completed_locally" } },
        { seq = 3, at = "2026-09-29T14:03:00Z", kind = { type = "tool_used", run = 1, tool = "Read" } },
    ]"#;
    let loaded: StrictHolder = toml::from_str(known).unwrap();
    assert_eq!(
        loaded.activity.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [1, 2, 3],
        "nothing removed or reordered"
    );
    assert_eq!(loaded.activity[1].kind, Kind::CompletedLocally {});
    assert_eq!(loaded.activity[2].kind, Kind::ToolUsed { run: 1, tool: "Read".into(), detail: None, count: 1 });
    let text = toml::to_string(&loaded).unwrap();
    assert_eq!(toml::from_str::<StrictHolder>(&text).unwrap(), loaded);

    for (what, entry) in [
        ("an unknown kind", r#"kind = { type = "from_a_newer_version" }"#),
        ("an unknown field on an entry", r#"kind = { type = "run_completed", run = 1 }, future = true"#),
        ("an unknown field on a kind", r#"kind = { type = "run_completed", run = 1, future = 1 }"#),
        ("an unknown field on a kind with no fields", r#"kind = { type = "completed_locally", future = false }"#),
        (
            "an unknown field on a nested result",
            r#"kind = { type = "run_result", run = 1, result = { outcome = "available", text = "x", future = [] } }"#,
        ),
    ] {
        let text = format!(
            "activity = [{{ seq = 1, at = \"2026-09-29T14:01:00Z\", {entry} }}]"
        );
        assert!(toml::from_str::<StrictHolder>(&text).is_err(), "{what} must be refused");
    }
}

#[test]
fn a_lenient_list_still_skips_what_it_does_not_know() {
    // The Task board reads strictly; `lenient` serves readers that only
    // display, which would rather show less than nothing. An entry with a field
    // it does not declare is an entry it does not fully understand, so it is
    // skipped like an unknown kind instead of being shown with the field lost.
    let json = r#"{"activity":[
        {"seq":1,"at":"2026-09-29T14:01:00Z","kind":{"type":"completed_locally","future":1}},
        {"seq":2,"at":"2026-09-29T14:02:00Z","kind":{"type":"completed_locally"}}
    ]}"#;
    let loaded: Holder = serde_json::from_str(json).unwrap();
    assert_eq!(loaded.activity.len(), 1);
    assert_eq!(loaded.activity[0].seq, 2);
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
        Kind::CompletedLocally {},
        Kind::IntegratedLocally { target: "main".into(), strategy: "fast_forward".into(), commit: "a".repeat(40) },
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

#[test]
fn reasons_keep_no_credential() {
    let reasons = [
        Kind::RunFailed {
            run: Some(1),
            reason: "Exit code 128 — fatal: unable to access \
                     'https://x-access-token:ghs_0123456789abcdefghij@github.com/o/r.git/': 403"
                .into(),
        },
        Kind::DeliveryFailed { reason: "gh: HTTP 401 (Authorization: token gho_0123456789abcdefghij)".into() },
        Kind::AiReviewFailed { review: 1, reason: "API error: invalid x-api-key sk-ant-0123456789abcdef".into() },
        Kind::AiFixFailed { review: 1, reason: "Fix agent failed: curl -H Authorization:Bearer eyJhbGciOiJIUzI1NiJ9".into() },
        Kind::AiReviewSkipped {
            review: 2,
            reason: "the task's changes could not be read: GET https://h/x?X-Amz-Signature=deadbeef1234 failed".into(),
        },
    ];
    let mut entries = Vec::new();
    for kind in reasons {
        record(&mut entries, t(1), kind);
    }
    let written = serde_json::to_string(&entries).unwrap();
    let fragments = ["ghs_", "gho_", "sk-ant", "eyJhbGci", "deadbeef1234", "X-Amz-Signature"];
    let kept: Vec<usize> = (0..fragments.len()).filter(|&i| written.contains(fragments[i])).collect();
    assert!(kept.is_empty(), "fragments at {kept:?} were kept");
    assert!(written.contains("https://***@github.com/o/r.git/"), "{written}");
    assert!(written.contains("GET https://h/x failed"), "{written}");
}

#[test]
fn tool_details_keep_no_credential() {
    let mut entries = Vec::new();
    record_tool(&mut entries, t(1), 1, "WebSearch", Some("why is ghp_0123456789abcdefghij rejected"));
    record_tool(&mut entries, t(1), 1, "Bash", Some("curl -H \"Authorization:Bearer abc\" https://x"));
    let details: Vec<_> = entries
        .iter()
        .map(|e| match &e.kind {
            Kind::ToolUsed { detail, .. } => detail.clone().unwrap(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(details, ["why is *** rejected", "curl -H \"Authorization:*** ***\" https://x"]);
}

#[test]
fn sanitizing_twice_changes_nothing_more() {
    for text in [
        "curl -H 'Authorization: Bearer abc' -u me:pw https://u:p@h/p?q=1",
        "TOKEN=\"a b c\" run",
        "rust token parser basic usage",
    ] {
        let once = sanitize(text, 300);
        assert_eq!(sanitize(&once, 300), once, "{text}");
    }
    assert_eq!(sanitize("TOKEN=\"a b c\" run", 300), "TOKEN=\"*** *** ***\" run");
    assert_eq!(sanitize("TOKEN=\"abc\" run", 300), "TOKEN=\"***\" run");
}

#[test]
fn a_value_after_any_flag_or_assignment_is_masked_too() {
    for (text, expected) in [
        ("wget --header=X-Api-Key:abc123 https://api", "wget --header=X-Api-Key:*** https://api"),
        ("curl --header=Authorization:Bearer abc123 https://x", "curl --header=Authorization:*** *** https://x"),
        ("app --endpoint=https://h/cb?access_token=abc123", "app --endpoint=https://h/cb"),
        ("gh api /x --raw-field=password=hunter2", "gh api /x --raw-field=password=***"),
        ("tool --header=Authorization:ghp_0123456789abcdefghij", "tool --header=Authorization:***"),
        ("CALLBACK=https://u:p@h/x run", "CALLBACK=https://***@h/x run"),
        ("DB_PASS=hunter2 ./run", "DB_PASS=*** ./run"),
        ("gpg --passphrase hunter2 -d f", "gpg --passphrase *** -d f"),
        ("OPENAI_KEY=abc123 MY_SERVICE_KEY=abc", "OPENAI_KEY=*** MY_SERVICE_KEY=***"),
        ("curl -H \"Ocp-Apim-Subscription-Key: abc\" x", "curl -H \"Ocp-Apim-Subscription-Key: ***\" x"),
        ("curl -ualice:hunter2 https://x", "curl -u*** https://x"),
        ("curl --user=alice:hunter2 https://x", "curl --user=*** https://x"),
        ("git log --author=rui --since=2.weeks", "git log --author=rui --since=2.weeks"),
    ] {
        assert_eq!(redact(text), expected, "{text}");
    }
}

#[test]
fn every_url_in_a_word_loses_its_credentials() {
    assert_eq!(
        redact("[\"https://a.com\",\"https://u:tok@b.com/p\"]"),
        "[\"https://a.com\",\"https://***@b.com/p\"]"
    );
    assert_eq!(redact("https://a.com,https://u:tok@b.com"), "https://a.com,https://***@b.com");
}

#[test]
fn a_token_cut_short_by_a_truncation_is_still_masked() {
    assert_eq!(redact("error: bad credentials ghp_abcdefgh…"), "error: bad credentials ***");
    // Without the cut, as short a word is only a name.
    assert_eq!(redact("see ghp_abcd"), "see ghp_abcd");
}

#[test]
fn names_that_only_start_like_a_token_are_kept() {
    for text in ["npm_config_cache=/tmp npm ci", "hf_hub_download(repo)", "ASIA-Pacific-region", "sk-learn"] {
        assert_eq!(redact(text), text);
    }
}

#[test]
fn appended_tool_rows_and_linked_urls_are_sanitized_too() {
    let mut entries = Vec::new();
    let raw = Entry {
        seq: 1,
        at: t(1),
        kind: Kind::ToolUsed { run: 1, tool: "Bash".into(), detail: Some("TOKEN=abc run".into()), count: 1 },
    };
    append(&mut entries, vec![raw]);
    record(&mut entries, t(2), Kind::PrLinked { url: "https://u:p@github.com/o/r/pull/1?t=x".into(), number: Some(1) });
    assert_eq!(
        entries.iter().map(|e| e.kind.clone()).collect::<Vec<_>>(),
        [
            Kind::ToolUsed { run: 1, tool: "Bash".into(), detail: Some("TOKEN=*** run".into()), count: 1 },
            Kind::PrLinked { url: "https://***@github.com/o/r/pull/1".into(), number: Some(1) },
        ]
    );
}

#[test]
fn the_next_run_counts_past_a_start_dropped_from_a_full_timeline() {
    let entries = vec![Entry { seq: 1, at: t(1), kind: Kind::RunFailed { run: Some(7), reason: "x".into() } }];
    assert_eq!(next_run(&entries), 8);
}

#[test]
fn a_local_completion_is_recorded_once_and_read_back() {
    let mut entries = Vec::new();
    assert!(!completed_locally(&entries));
    assert!(record(&mut entries, t(1), Kind::CompletedLocally {}));
    assert!(completed_locally(&entries));

    // Asked again, it is the same milestone.
    assert!(!record(&mut entries, t(2), Kind::CompletedLocally {}));
    assert_eq!(entries.len(), 1);

    let items = timeline(t(0), [], &entries);
    let last = items.last().expect("a row");
    assert_eq!(last.title(), "Completed locally, not merged");
    assert_eq!(last.kind_name(), "completed_locally");
    assert_eq!(last.tone(), Tone::Good);
}

#[test]
fn an_integration_is_recorded_once_per_commit_and_read_back() {
    let kind = |commit: &str| Kind::IntegratedLocally {
        target: "main".into(),
        strategy: "merge_commit".into(),
        commit: commit.into(),
    };
    let mut entries = Vec::new();
    assert!(record(&mut entries, t(1), kind("aaaa")));
    assert!(!record(&mut entries, t(2), kind("aaaa")), "the same result is the same milestone");
    assert!(record(&mut entries, t(3), kind("bbbb")), "a later integration of newer work is its own entry");
    assert_eq!(entries.len(), 2);

    let items = timeline(t(0), [], &entries);
    let last = items.last().expect("a row");
    assert_eq!(last.title(), "Integrated into main");
    assert_eq!(last.kind_name(), "integrated_locally");
    assert_eq!(last.tone(), Tone::Good);
}

#[test]
fn a_provider_answer_is_redacted_and_bounded_before_it_is_stored() {
    let secret = RunResult::from_provider(Ok("Done. GITHUB_TOKEN=ghp_0123456789abcdef".into()));
    let RunResult::Available { text } = &secret else {
        panic!("a short answer is available: {secret:?}");
    };
    assert!(!text.contains("ghp_"), "{text}");

    let long = RunResult::from_provider(Ok("word ".repeat(1_000)));
    let RunResult::Truncated {
        text,
        original_chars,
    } = &long
    else {
        panic!("an answer past the limit is truncated: {long:?}");
    };
    assert!(text.chars().count() <= MAX_RESULT_CHARS);
    assert_eq!(*original_chars, 5_000);

    // Storing a truncated result again changes nothing: the limit holds and
    // the original length is kept.
    let mut entries = Vec::new();
    record(
        &mut entries,
        t(1),
        Kind::RunResult {
            run: 1,
            result: long.clone(),
        },
    );
    assert_eq!(
        entries[0].kind,
        Kind::RunResult {
            run: 1,
            result: long
        }
    );
}

#[test]
fn a_result_is_recorded_once_per_run_and_read_back_by_run() {
    let mut entries = Vec::new();
    let answer = RunResult::Available {
        text: "first answer".into(),
    };
    assert!(record(
        &mut entries,
        t(1),
        Kind::RunResult {
            run: 1,
            result: answer.clone()
        }
    ));
    assert!(!record(
        &mut entries,
        t(2),
        Kind::RunResult {
            run: 1,
            result: RunResult::Unavailable {
                reason: "later".into()
            }
        },
    ));
    assert!(record(
        &mut entries,
        t(3),
        Kind::RunResult {
            run: 2,
            result: RunResult::Unavailable {
                reason: "second run".into()
            }
        },
    ));
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].kind,
        Kind::RunResult {
            run: 1,
            result: answer
        }
    );
    assert_eq!(next_run(&entries), 3);
}

#[test]
fn a_result_serializes_with_its_outcome_and_reads_back() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Stored {
        activity: Vec<Entry>,
    }
    let stored = Stored {
        activity: vec![
            Entry {
                seq: 1,
                at: t(1),
                kind: Kind::RunResult {
                    run: 1,
                    result: RunResult::Available { text: "ok".into() },
                },
            },
            Entry {
                seq: 2,
                at: t(2),
                kind: Kind::RunResult {
                    run: 2,
                    result: RunResult::Truncated {
                        text: "cut".into(),
                        original_chars: 9_000,
                    },
                },
            },
            Entry {
                seq: 3,
                at: t(3),
                kind: Kind::RunResult {
                    run: 3,
                    result: RunResult::Unavailable {
                        reason: "no answer".into(),
                    },
                },
            },
        ],
    };
    let toml_text = toml::to_string(&stored).unwrap();
    assert_eq!(
        toml::from_str::<Stored>(&toml_text).unwrap(),
        stored,
        "{toml_text}"
    );
    let json_text = serde_json::to_string(&stored).unwrap();
    assert_eq!(serde_json::from_str::<Stored>(&json_text).unwrap(), stored);
}

#[test]
fn a_result_row_names_its_run_and_its_outcome() {
    let mut entries = Vec::new();
    record(
        &mut entries,
        t(1),
        Kind::RunResult {
            run: 1,
            result: RunResult::Unavailable {
                reason: "none".into(),
            },
        },
    );
    let timeline = timeline(t(0), [], &entries);
    let row = timeline.last().unwrap();
    assert_eq!(row.kind_name(), "run_result");
    assert_eq!(row.title(), "Coding result unavailable");
    assert_eq!(row.run_result().map(|(run, _)| run), Some(1));
    assert!(!row.is_tool());
}
