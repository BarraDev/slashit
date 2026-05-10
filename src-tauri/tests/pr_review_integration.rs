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

use slashit_app_lib::commands::pr::{address_pr_review_inner, AddressPrReviewOptions};
use slashit_app_lib::test_helpers::create_test_pr_review_setup;

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
        address_pr_review_inner(task, env.working_dir_str(), plan, opts)
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
    assert_eq!(result.fixed_ids, vec![101]);
    assert_eq!(result.skipped_ids, vec![102]);
    assert!(
        result.agent_summary.contains("DRY RUN"),
        "agent_summary should carry the mock claude output, got: {:?}",
        result.agent_summary
    );

    // Plan returned to caller has last_apply set with this run's result.
    let stamped = updated_plan.last_apply.expect("last_apply set");
    assert_eq!(stamped.fixed_ids, vec![101]);

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

    let (result, _updated_plan) =
        address_pr_review_inner(task, env.working_dir_str(), plan, opts)
            .await
            .expect("full apply succeeds");

    assert_eq!(env.claude_invocations(), 1, "claude called exactly once");
    assert!(!result.pushed, "auto_push=false → not pushed");
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

    let err = address_pr_review_inner(task, env.working_dir_str(), plan, opts)
        .await
        .expect_err("should error when no items are approved");
    assert!(err.contains("No approved fix items"), "got: {err}");
    assert_eq!(env.claude_invocations(), 0, "claude must not run when there is nothing to do");
    assert_eq!(env.gh_invocations(), 0);
}
