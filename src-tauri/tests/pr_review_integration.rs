//! Integration tests for `address_pr_review_inner` — the apply pipeline
//! exercised end-to-end against mock `claude` and `gh` binaries on PATH.
//!
//! No real PR, no real LLM, no network. The mocks are tiny shell scripts
//! that record their argv to a log file and emit a canned response. Each
//! test sets PATH to a temp directory containing those mocks (prepended),
//! then runs the inner function.
//!
//! `jj` is invoked by the inner function but the test does NOT set up a
//! real jj repo; the inner function uses `let _ = run_cmd("jj", ...)` for
//! describe/export, so missing/failing jj is silently tolerated. We do not
//! assert on jj behavior here.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;

use slashit_app_lib::commands::pr::{
    address_pr_review_inner, discuss_pr_review_questions_inner, no_progress,
    AddressPrReviewOptions, PrReviewProgress, ProgressSink,
};
use slashit_app_lib::domain::task::{
    PrCommentKind, PrReviewComment, PrReviewDecision, PrReviewItem, PrReviewPlan,
};
use slashit_app_lib::test_helpers::{create_test_pr_review_setup, create_test_task};

// PATH is process-global; serialize tests that mutate it.
static PATH_LOCK: Mutex<()> = Mutex::new(());

struct MockEnv {
    _tmp: tempfile::TempDir,
    bin_dir: std::path::PathBuf,
    claude_log: std::path::PathBuf,
    gh_log: std::path::PathBuf,
    working_dir: std::path::PathBuf,
    saved_path: Option<String>,
}

impl MockEnv {
    fn setup(claude_result: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let working_dir = tmp.path().join("work");
        fs::create_dir_all(&working_dir).unwrap();

        let claude_log = tmp.path().join("claude.log");
        let gh_log = tmp.path().join("gh.log");

        // Mock `claude` — emits one stream-json `result` event so
        // extract_text_from_stream_json picks it up.
        let claude_script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> {log:?}; done\nprintf '%s\\n' '---END-ARGS---' >> {log:?}\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":{result:?}}}'\n",
            log = claude_log,
            result = claude_result,
        );
        write_executable(&bin_dir.join("claude"), &claude_script);

        // Mock `gh` — records argv, returns a successful empty JSON.
        let gh_script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> {log:?}; done\nprintf '%s\\n' '---END-ARGS---' >> {log:?}\nprintf '%s\\n' '{{}}'\n",
            log = gh_log,
        );
        write_executable(&bin_dir.join("gh"), &gh_script);

        let saved_path = std::env::var("PATH").ok();
        let new_path = match &saved_path {
            Some(p) => format!("{}:{}", bin_dir.display(), p),
            None => bin_dir.display().to_string(),
        };
        // Safety: serialized via PATH_LOCK; tests in this file are the only
        // mutators of PATH and we restore on Drop.
        unsafe {
            std::env::set_var("PATH", new_path);
        }

        MockEnv {
            _tmp: tmp,
            bin_dir,
            claude_log,
            gh_log,
            working_dir,
            saved_path,
        }
    }

    fn working_dir_str(&self) -> String {
        self.working_dir.display().to_string()
    }

    fn read_claude_log(&self) -> String {
        fs::read_to_string(&self.claude_log).unwrap_or_default()
    }

    fn read_gh_log(&self) -> String {
        fs::read_to_string(&self.gh_log).unwrap_or_default()
    }

    fn claude_invocations(&self) -> usize {
        self.read_claude_log().matches("---END-ARGS---").count()
    }

    fn gh_invocations(&self) -> usize {
        self.read_gh_log().matches("---END-ARGS---").count()
    }
}

impl Drop for MockEnv {
    fn drop(&mut self) {
        // Restore PATH so other test binaries are unaffected.
        unsafe {
            match &self.saved_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        let _ = &self.bin_dir; // touch field to silence unused warnings
    }
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod");
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_invokes_claude_only_no_gh_no_push() {
    let _guard = PATH_LOCK.lock().unwrap();
    let env = MockEnv::setup("DRY RUN — 1 item verified, would edit 1 file.");
    let (task, plan) = create_test_pr_review_setup();

    let opts = AddressPrReviewOptions {
        auto_push: true,   // must be ignored in dry-run
        auto_reply: true,  // must be ignored in dry-run
        dry_run: true,
    };

    let (result, updated_plan) =
        address_pr_review_inner(task, env.working_dir_str(), plan, opts, no_progress())
            .await
            .expect("dry-run apply succeeds");

    assert_eq!(env.claude_invocations(), 1, "claude called exactly once");
    assert_eq!(
        env.gh_invocations(),
        0,
        "gh must not be called during a dry run, got log:\n{}",
        env.read_gh_log()
    );
    assert!(!result.pushed, "dry-run never pushes");
    assert_eq!(result.replies_posted, 0, "dry-run never replies");
    assert!(result.reply_errors.is_empty());
    assert!(result.dry_run, "dry-run flag must be set on the result");
    assert_eq!(result.fixed_ids, vec![101]);
    assert_eq!(result.skipped_ids, vec![102]);
    assert!(
        result.agent_summary.contains("DRY RUN"),
        "agent_summary should carry the mock claude output, got: {:?}",
        result.agent_summary
    );

    // Dry-runs are session-local previews: the plan returned to the caller
    // (and persisted on the task) MUST NOT advance `last_apply`, otherwise a
    // stale dry-run would re-appear the next time the modal opens and the
    // "only new" filter would treat it as real progress.
    assert!(
        updated_plan.last_apply.is_none(),
        "dry-run must not persist on the plan; got {:?}",
        updated_plan.last_apply,
    );

    // Sanity: the prompt sent to claude flagged read-only mode.
    let claude_log = env.read_claude_log();
    assert!(
        claude_log.contains("--allowedTools") && claude_log.contains("Read,Glob,Grep"),
        "dry-run claude invocation should pass read-only allowedTools, got:\n{}",
        claude_log
    );
    assert!(
        !claude_log.contains("Read,Edit,Write,Bash"),
        "dry-run must not enable Edit/Write tools, got:\n{}",
        claude_log
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn full_apply_with_auto_reply_calls_gh_per_fix_item() {
    let _guard = PATH_LOCK.lock().unwrap();
    let env = MockEnv::setup("FIXED: item #0\nDONE");
    let (task, plan) = create_test_pr_review_setup();

    let opts = AddressPrReviewOptions {
        auto_push: false,  // skip push (no remote in test)
        auto_reply: true,
        dry_run: false,
    };

    let (result, updated_plan) =
        address_pr_review_inner(task, env.working_dir_str(), plan, opts, no_progress())
            .await
            .expect("full apply succeeds");
    // Real applies (dry_run=false) must persist on the plan so the next modal
    // open knows when the last real apply happened (used by `show_only_new`).
    let persisted = updated_plan.last_apply.expect("real apply persists last_apply");
    assert_eq!(persisted.fixed_ids, vec![101]);
    assert!(!persisted.dry_run);

    assert_eq!(env.claude_invocations(), 1, "claude called exactly once");
    assert!(!result.pushed, "auto_push=false → not pushed");
    assert!(!result.dry_run, "full apply must not set the dry_run flag");
    assert_eq!(result.fixed_ids, vec![101]);
    assert_eq!(result.skipped_ids, vec![102]);

    // gh should have been called once per approved Fix item (here, 1 fix).
    // The mock returns ok on the inline `/replies` endpoint, so we expect
    // exactly 1 invocation and no fallback to `pr comment`.
    assert_eq!(
        env.gh_invocations(),
        1,
        "gh should be called once per Fix item, got log:\n{}",
        env.read_gh_log()
    );
    assert_eq!(result.replies_posted, 1);
    assert!(
        result.reply_errors.is_empty(),
        "no reply errors expected, got: {:?}",
        result.reply_errors
    );

    // Verify the gh call hit the `/replies` endpoint with the right comment id.
    let gh_log = env.read_gh_log();
    assert!(
        gh_log.contains("repos/test-org/test-repo/pulls/42/comments/101/replies"),
        "expected inline reply endpoint for comment 101, got:\n{}",
        gh_log
    );

    // Claude got the full edit toolset on the apply path.
    let claude_log = env.read_claude_log();
    assert!(
        claude_log.contains("Read,Edit,Write,Bash"),
        "apply path must enable Edit/Write/Bash tools, got:\n{}",
        claude_log
    );
}

/// Build a plan with three items keyed to comment ids 201/202/203:
/// two Question items (with notes) plus a Skip item that must survive untouched.
fn create_test_discuss_setup() -> (slashit_app_lib::domain::Task, PrReviewPlan) {
    let mut task = create_test_task("Discuss PR review questions");
    task.pr_url = Some("https://github.com/test-org/test-repo/pull/42".to_string());
    task.branch_name = Some("test-branch".to_string());

    let comments = vec![
        PrReviewComment {
            id: Some(201),
            kind: PrCommentKind::Inline,
            author: "reviewer".to_string(),
            body: "Should we retry on failure?".to_string(),
            path: Some("src/lib.rs".to_string()),
            line: Some(10),
            url: None,
            created_at: None,
            updated_at: None,
        },
        PrReviewComment {
            id: Some(202),
            kind: PrCommentKind::Inline,
            author: "reviewer".to_string(),
            body: "Timeout seems off.".to_string(),
            path: Some("src/lib.rs".to_string()),
            line: Some(20),
            url: None,
            created_at: None,
            updated_at: None,
        },
        PrReviewComment {
            id: Some(203),
            kind: PrCommentKind::Inline,
            author: "reviewer".to_string(),
            body: "Nit: rename later.".to_string(),
            path: Some("src/lib.rs".to_string()),
            line: Some(30),
            url: None,
            created_at: None,
            updated_at: None,
        },
    ];

    let items = vec![
        PrReviewItem {
            comment_id: Some(201),
            summary: "Maybe add retry".to_string(),
            decision: PrReviewDecision::Question,
            reasoning: "Unsure whether retry is desired.".to_string(),
            proposed_change: String::new(),
            approved: false,
            user_note: "yes, please add retry with backoff".to_string(),
        },
        PrReviewItem {
            comment_id: Some(202),
            summary: "Timeout value".to_string(),
            decision: PrReviewDecision::Question,
            reasoning: "Need a target timeout from the user.".to_string(),
            proposed_change: String::new(),
            approved: false,
            user_note: "what timeout should we use?".to_string(),
        },
        PrReviewItem {
            comment_id: Some(203),
            summary: "Rename (skipped)".to_string(),
            decision: PrReviewDecision::Skip,
            reasoning: "Out of scope.".to_string(),
            proposed_change: String::new(),
            approved: false,
            user_note: String::new(),
        },
    ];

    let plan = PrReviewPlan {
        generated_at: chrono::Utc::now(),
        pr_url: task.pr_url.clone().unwrap(),
        review_decision: None,
        comments,
        items,
        raw_plan: String::new(),
        last_apply: None,
    };

    (task, plan)
}

#[tokio::test(flavor = "multi_thread")]
async fn discuss_round_merges_updates_without_reordering_or_touching_skip() {
    let _guard = PATH_LOCK.lock().unwrap();
    // Agent converts 201 → Fix, keeps 202 as Question with fresh reasoning,
    // and (correctly) does not return an entry for the Skip item 203.
    let claude_json = r#"{"items":[{"comment_id":201,"summary":"Add retry with backoff","decision":"fix","reasoning":"User confirmed; will wrap call in retry.","proposed_change":"Wrap http_call in retry_with_backoff(3)."},{"comment_id":202,"summary":"Timeout value","decision":"question","reasoning":"Which timeout (ms) do you want — 5000 or 10000?","proposed_change":""}]}"#;
    let env = MockEnv::setup(claude_json);
    let (task, plan) = create_test_discuss_setup();

    let merged = discuss_pr_review_questions_inner(task, env.working_dir_str(), plan)
        .await
        .expect("discuss merges successfully");

    assert_eq!(env.claude_invocations(), 1, "claude called exactly once");
    assert_eq!(env.gh_invocations(), 0, "discuss never calls gh");

    // Items still in input order (201, 202, 203).
    let ids: Vec<Option<u64>> = merged.items.iter().map(|i| i.comment_id).collect();
    assert_eq!(ids, vec![Some(201), Some(202), Some(203)], "merge must not reorder");

    // 201 converted to Fix, user_note cleared, approved flipped to true.
    let converted = &merged.items[0];
    assert!(matches!(converted.decision, PrReviewDecision::Fix));
    assert!(converted.approved, "Fix items are auto-approved by parse_review_items");
    assert!(converted.user_note.is_empty(), "user_note must be cleared after merge");
    assert_eq!(converted.summary, "Add retry with backoff");
    assert!(converted.reasoning.contains("User confirmed"));
    assert!(converted.proposed_change.contains("retry_with_backoff"));

    // 202 still Question, reasoning refreshed, user_note cleared, not approved.
    let still_question = &merged.items[1];
    assert!(matches!(still_question.decision, PrReviewDecision::Question));
    assert!(!still_question.approved);
    assert!(still_question.user_note.is_empty(), "user_note cleared even when still Question");
    assert!(
        still_question.reasoning.contains("5000 or 10000"),
        "reasoning should carry the agent's follow-up, got: {:?}",
        still_question.reasoning,
    );

    // 203 (Skip) completely untouched.
    let skip = &merged.items[2];
    assert!(matches!(skip.decision, PrReviewDecision::Skip));
    assert_eq!(skip.summary, "Rename (skipped)");
    assert_eq!(skip.reasoning, "Out of scope.");
    assert!(!skip.approved);
    assert!(skip.user_note.is_empty());

    // Discuss path is read-only: prompt to claude must use the Read-only toolset.
    let claude_log = env.read_claude_log();
    assert!(
        claude_log.contains("--allowedTools") && claude_log.contains("Read,Glob,Grep"),
        "discuss must run claude read-only, got:\n{}",
        claude_log
    );
    assert!(
        !claude_log.contains("Read,Edit,Write,Bash"),
        "discuss must not enable Edit/Write tools, got:\n{}",
        claude_log
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discuss_without_pending_questions_errors_before_calling_claude() {
    let _guard = PATH_LOCK.lock().unwrap();
    let env = MockEnv::setup("unused");
    // Default 2-item plan has 1 Fix + 1 Skip — no Question items with notes.
    let (task, plan) = create_test_pr_review_setup();

    let err = discuss_pr_review_questions_inner(task, env.working_dir_str(), plan)
        .await
        .expect_err("should error when nothing to discuss");
    assert!(err.contains("No Question items with notes"), "got: {err}");
    assert_eq!(env.claude_invocations(), 0);
    assert_eq!(env.gh_invocations(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_approved_set_returns_error_and_does_not_call_claude() {
    let _guard = PATH_LOCK.lock().unwrap();
    let env = MockEnv::setup("unused");
    let (task, mut plan) = create_test_pr_review_setup();
    // Demote the only Fix item so nothing is approved.
    for item in plan.items.iter_mut() {
        item.approved = false;
    }

    let opts = AddressPrReviewOptions {
        auto_push: false,
        auto_reply: false,
        dry_run: false,
    };

    let err = address_pr_review_inner(task, env.working_dir_str(), plan, opts, no_progress())
        .await
        .expect_err("should error when no items are approved");
    assert!(err.contains("No approved fix items"), "got: {err}");
    assert_eq!(env.claude_invocations(), 0, "claude must not run when there is nothing to do");
    assert_eq!(env.gh_invocations(), 0);
}

/// Build a 2-Fix-item plan exercising the per-item iteration.
fn create_test_two_fix_setup() -> (slashit_app_lib::domain::Task, PrReviewPlan) {
    use slashit_app_lib::test_helpers::create_test_task;

    let mut task = create_test_task("Two fixes");
    task.pr_url = Some("https://github.com/test-org/test-repo/pull/42".to_string());
    task.branch_name = Some("test-branch".to_string());

    let comments = vec![
        PrReviewComment {
            id: Some(301),
            kind: PrCommentKind::Inline,
            author: "reviewer".to_string(),
            body: "First issue".to_string(),
            path: Some("src/a.rs".to_string()),
            line: Some(1),
            url: None,
            created_at: None,
            updated_at: None,
        },
        PrReviewComment {
            id: Some(302),
            kind: PrCommentKind::Inline,
            author: "reviewer".to_string(),
            body: "Second issue".to_string(),
            path: Some("src/b.rs".to_string()),
            line: Some(2),
            url: None,
            created_at: None,
            updated_at: None,
        },
    ];

    let items = vec![
        PrReviewItem {
            comment_id: Some(301),
            summary: "Fix first".to_string(),
            decision: PrReviewDecision::Fix,
            reasoning: "needs fix".to_string(),
            proposed_change: "change a".to_string(),
            approved: true,
            user_note: String::new(),
        },
        PrReviewItem {
            comment_id: Some(302),
            summary: "Fix second".to_string(),
            decision: PrReviewDecision::Fix,
            reasoning: "needs fix".to_string(),
            proposed_change: "change b".to_string(),
            approved: true,
            user_note: String::new(),
        },
    ];

    let plan = PrReviewPlan {
        generated_at: chrono::Utc::now(),
        pr_url: task.pr_url.clone().unwrap(),
        review_decision: None,
        comments,
        items,
        raw_plan: String::new(),
        last_apply: None,
    };

    (task, plan)
}

#[tokio::test(flavor = "multi_thread")]
async fn per_item_apply_invokes_claude_per_fix_and_emits_progress_events() {
    use std::sync::Mutex as StdMutex;
    let _guard = PATH_LOCK.lock().unwrap();
    let env = MockEnv::setup("OK fixed");
    let (task, plan) = create_test_two_fix_setup();

    let opts = AddressPrReviewOptions {
        auto_push: false,
        auto_reply: false,
        dry_run: false,
    };

    let captured: std::sync::Arc<StdMutex<Vec<PrReviewProgress>>> =
        std::sync::Arc::new(StdMutex::new(Vec::new()));
    let captured_clone = std::sync::Arc::clone(&captured);
    let sink: ProgressSink = std::sync::Arc::new(move |ev| {
        captured_clone.lock().unwrap().push(ev);
    });

    let (result, _plan) = address_pr_review_inner(task, env.working_dir_str(), plan, opts, sink)
        .await
        .expect("two-fix apply succeeds");

    // Per-item loop: one claude invocation per approved Fix item.
    assert_eq!(
        env.claude_invocations(),
        2,
        "claude must be called once per approved Fix item, got log:\n{}",
        env.read_claude_log(),
    );
    assert_eq!(result.fixed_ids, vec![301, 302]);
    assert!(result.failed_ids.is_empty(), "no failures expected");
    assert!(result.fix_errors.is_empty());

    // agent_summary aggregates per-item headers; each comment id appears once.
    assert!(
        result.agent_summary.contains("comment 301") && result.agent_summary.contains("comment 302"),
        "agent_summary must include per-item headers, got: {}",
        result.agent_summary,
    );

    // Progress events: 2 item_started, 2 item_succeeded, 1 all_done. No
    // push/reply events because auto_push and auto_reply are false.
    let events = captured.lock().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["item_started", "item_succeeded", "item_started", "item_succeeded", "all_done"],
        "unexpected progress sequence: {:?}",
        kinds,
    );
    // First pair targets comment 301; second pair targets 302; counters match.
    assert_eq!(events[0].comment_id, Some(301));
    assert_eq!(events[0].current, Some(1));
    assert_eq!(events[0].total, Some(2));
    assert_eq!(events[2].comment_id, Some(302));
    assert_eq!(events[2].current, Some(2));
    assert_eq!(events[2].total, Some(2));
}
