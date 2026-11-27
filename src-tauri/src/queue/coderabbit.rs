//! The CodeRabbit CLI's part of an AI Review: how it is invoked, how its
//! `--agent` output is read, and what counts as a review that did not complete.
//!
//! `--agent` prints one JSON object per line. The events read here are
//! `finding` and `unverified_finding`, `complete`, `error` and
//! `action_required`; every other type (`review_context`, `status`,
//! `heartbeat`, ...) is ignored. A run counts as completed only when the CLI
//! exits successfully after a `complete` event whose finding counts match what
//! was read. Anything else is a [`CodeRabbitReview::failure`], never an empty
//! clean result. Findings read before a failure are kept.
//!
//! Written against CodeRabbit CLI 0.9.7.

use crate::process_tree::TreeOwner;
use serde::Deserialize;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// The CLI documents reviews of up to 15 minutes; past this it is stopped.
pub(crate) const REVIEW_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// How much of the CLI's own error text a failure reason quotes.
const MAX_QUOTED_CHARS: usize = 200;

/// What one CodeRabbit run produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CodeRabbitReview {
    pub findings: Vec<Finding>,
    /// Why the review did not complete. `None` for a completed review, and for
    /// one that was cancelled or never asked for.
    pub failure: Option<String>,
}

impl CodeRabbitReview {
    pub(crate) fn failed(reason: impl Into<String>) -> Self {
        Self {
            findings: Vec::new(),
            failure: Some(reason.into()),
        }
    }

    /// The findings as the fix agent reads them, one `- ISSUE:` line each (the
    /// shape the review's issue count reads) with the comment indented below,
    /// so no line of a comment can pass for another issue. Empty when there
    /// are none.
    pub(crate) fn findings_text(&self) -> String {
        let mut text = String::new();
        for finding in &self.findings {
            let severity = if finding.unverified {
                "unverified"
            } else {
                finding.severity.as_deref().unwrap_or("finding")
            };
            let title = finding
                .title
                .as_deref()
                .or_else(|| finding.comment.lines().find(|l| !l.trim().is_empty()))
                .unwrap_or("Review finding");
            text.push_str(&format!(
                "- ISSUE: [CodeRabbit {}] {}:{}-{} - {}\n",
                one_line(severity),
                one_line(&finding.file_name),
                finding.start_line,
                finding.end_line,
                one_line(title),
            ));
            for line in finding.comment.lines() {
                text.push_str("  ");
                text.push_str(line);
                text.push('\n');
            }
        }
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Finding {
    pub file_name: String,
    pub start_line: i64,
    pub end_line: i64,
    pub comment: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    /// Set from the event type.
    #[serde(skip)]
    pub unverified: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Complete {
    status: String,
    findings: usize,
    #[serde(default)]
    unverified_findings: usize,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    unreviewed_file_count: usize,
    #[serde(default)]
    unreviewed_files: Vec<String>,
}

/// The arguments for a review of everything in `working_dir` since
/// `base_commit`: the same boundary as the Task's diff, committed or not.
///
/// `--fresh` makes each review cover that whole range instead of resuming from
/// the CLI's previous local checkpoint. `--use-credits` is deliberately absent:
/// SlashIt never authorizes paid usage.
fn review_args<'a>(working_dir: &'a str, base_commit: &'a str) -> [&'a str; 8] {
    [
        "review",
        "--agent",
        "--fresh",
        "--include-untracked",
        "--base-commit",
        base_commit,
        "--dir",
        working_dir,
    ]
}

/// Run the CodeRabbit CLI found on `PATH` and read its result.
///
/// The run's process tree is owned by a [`TreeOwner`] (a process group on
/// Unix, a Job Object on Windows), and every process in it is ended when the
/// run completes (so a descendant cannot outlive the review), times out, is
/// cancelled, or the future is dropped. A run whose tree cannot be owned is not
/// started. A cancelled run returns an empty result, which the caller discards.
pub(crate) async fn run_review(
    working_dir: &str,
    base_commit: &str,
    cancelled: &mut tokio::sync::watch::Receiver<bool>,
    timeout: Duration,
) -> CodeRabbitReview {
    if *cancelled.borrow() {
        return CodeRabbitReview::default();
    }
    // Many tests drive a review with the default settings, which enable
    // CodeRabbit. None may reach a developer's installed CLI.
    #[cfg(test)]
    if !path_resolves_to_a_fixture() {
        return CodeRabbitReview::failed("unit tests run only a fixture CodeRabbit CLI");
    }
    let mut command = tokio::process::Command::new("coderabbit");
    command
        .args(review_args(working_dir, base_commit))
        .current_dir(working_dir)
        // A prompt for input must fail, not wait.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Covers the paths where this future is simply dropped (an aborted
        // task, a runtime shutdown) for the leader itself.
        .kill_on_drop(true);
    // Spawned with its tree owned (see `TreeOwner::spawn`). Declared after the
    // child it owns is not enough on its own: the tuple binds `child` first, so
    // the owner drops first, while the leader is unreaped.
    let (mut child, group) = match TreeOwner::spawn(&mut command) {
        Ok(spawned) => spawned,
        Err(e) => {
            return CodeRabbitReview::failed(format!(
                "the CodeRabbit CLI could not be started with its processes owned: {e}"
            ))
        }
    };

    let (Some(mut out), Some(mut err)) = (child.stdout.take(), child.stderr.take()) else {
        group.stop(&mut child).await;
        return CodeRabbitReview::failed("the CodeRabbit CLI's output could not be captured");
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    // The pipes are drained while waiting: a CLI that fills one would
    // otherwise never exit.
    let finished = {
        let drain =
            async { tokio::join!(out.read_to_end(&mut stdout), err.read_to_end(&mut stderr)) };
        // Once the leader has exited, whatever it left running is ended too.
        // That also closes the pipes a descendant would otherwise hold open.
        // The exit is awaited without reaping, so the tree is still owned.
        let end_group = async {
            group.wait_for_leader_exit().await;
            group.signal();
        };
        let run = async { tokio::join!(drain, end_group) };
        tokio::select! {
            biased;
            _ = cancelled.changed() => None,
            finished = tokio::time::timeout(timeout, run) => Some(finished),
        }
    };

    let Some(finished) = finished else {
        group.stop(&mut child).await;
        return CodeRabbitReview::default();
    };
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);

    match finished {
        Ok(((out_read, err_read), ())) => {
            let status = child.wait().await;
            group.reaped();
            let mut review = match status {
                Ok(status) => parse_output(&stdout, &stderr, status.success(), status.code()),
                Err(e) => CodeRabbitReview::failed(format!(
                    "the CodeRabbit CLI's exit status could not be read: {e}"
                )),
            };
            if let Err(e) = out_read.and(err_read) {
                review.failure.get_or_insert_with(|| {
                    format!("the CodeRabbit CLI's output could not be read: {e}")
                });
            }
            review
        }
        Err(_) => {
            group.stop(&mut child).await;
            // Whatever it reported before stalling is still worth keeping.
            let mut review = parse_output(&stdout, &stderr, false, None);
            review.failure = Some(format!(
                "the CodeRabbit CLI did not finish within {} seconds and was stopped",
                timeout.as_secs()
            ));
            review
        }
    }
}

/// Whether the first `coderabbit` on `PATH` is under the temporary directory.
#[cfg(test)]
fn path_resolves_to_a_fixture() -> bool {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("coderabbit"))
                .find(|program| program.is_file())
        })
        .is_some_and(|program| program.starts_with(std::env::temp_dir()))
}

fn parse_output(stdout: &str, stderr: &str, success: bool, code: Option<i32>) -> CodeRabbitReview {
    let mut findings = Vec::new();
    let mut complete: Option<Complete> = None;
    let mut reported: Option<String> = None;
    let mut unreadable: Option<String> = None;

    for line in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            unreadable.get_or_insert_with(|| format!("not a JSON event: {}", quote(line)));
            continue;
        };
        let text = |key: &str| event.get(key).and_then(|v| v.as_str());
        match text("type") {
            Some(kind @ ("finding" | "unverified_finding")) => match Finding::deserialize(&event) {
                Ok(mut finding) => {
                    finding.unverified = kind == "unverified_finding";
                    findings.push(finding);
                }
                Err(e) => {
                    unreadable.get_or_insert_with(|| format!("a malformed finding: {e}"));
                }
            },
            Some("complete") => match Complete::deserialize(&event) {
                Ok(event) => complete = Some(event),
                Err(e) => {
                    unreadable.get_or_insert_with(|| format!("a malformed completion: {e}"));
                }
            },
            Some("error") => {
                let kind = text("errorType").unwrap_or("unknown");
                let message = quote(text("message").unwrap_or("no message"));
                reported.get_or_insert_with(|| {
                    format!("CodeRabbit reported an error ({kind}): {message}")
                });
            }
            Some("action_required") => {
                // Takes precedence over the error beside it: it says what a
                // person would have to decide.
                reported = Some(if text("action") == Some("rerun_with_use_credits") {
                    "CodeRabbit's included review limit is reached and it asked to spend usage \
                     credits. SlashIt does not authorize paid usage"
                        .to_string()
                } else {
                    format!(
                        "CodeRabbit needs a person to act: {}",
                        quote(
                            text("message")
                                .or_else(|| text("action"))
                                .unwrap_or("no detail")
                        )
                    )
                });
            }
            Some(_) => {}
            None => {
                unreadable.get_or_insert_with(|| format!("an event with no type: {}", quote(line)));
            }
        }
    }

    let exit = || {
        let status = match code {
            Some(code) => format!("exited with code {code}"),
            None => "was terminated".to_string(),
        };
        match stderr.trim() {
            "" => format!("the CodeRabbit CLI {status}"),
            text => format!("the CodeRabbit CLI {status}: {}", quote(text)),
        }
    };

    let failure = if let Some(reported) = reported {
        Some(reported)
    } else if !success {
        Some(exit())
    } else if let Some(unreadable) = unreadable {
        Some(format!(
            "CodeRabbit's output could not be read, {unreadable}"
        ))
    } else {
        match &complete {
            None => Some(
                "CodeRabbit's output could not be read, it ended without a completion event"
                    .to_string(),
            ),
            Some(done) => incomplete_reason(done, &findings),
        }
    };

    CodeRabbitReview { findings, failure }
}

/// Why a `complete` event still does not stand for a whole review.
fn incomplete_reason(done: &Complete, findings: &[Finding]) -> Option<String> {
    let message = done
        .message
        .as_deref()
        .map(|m| format!(": {}", quote(m)))
        .unwrap_or_default();
    if done.status != "review_completed" {
        return Some(format!(
            "CodeRabbit did not review the changes ({}){message}",
            quote(&done.status)
        ));
    }
    if done.outcome.as_deref() == Some("failed") {
        return Some(format!("CodeRabbit's review failed{message}"));
    }
    let unverified = findings.iter().filter(|f| f.unverified).count();
    if done.findings != findings.len() - unverified || done.unverified_findings != unverified {
        return Some(format!(
            "CodeRabbit's output could not be read, it counted {} findings and {} were read",
            done.findings.saturating_add(done.unverified_findings),
            findings.len()
        ));
    }
    let unreviewed = done.unreviewed_file_count.max(done.unreviewed_files.len());
    (unreviewed > 0).then(|| format!("CodeRabbit left {unreviewed} file(s) unreviewed{message}"))
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The CLI's own words, on one line and bounded.
fn quote(text: &str) -> String {
    slashit_activity::one_line(text, MAX_QUOTED_CHARS)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::test_helpers::FakeProgram;

    const FINDING: &str = r#"{"type":"finding","severity":"major","title":"Unchecked index","fileName":"src/a.rs","startLine":3,"endLine":4,"comment":"This panics.\n- ISSUE: not a second one","codegenInstructions":"x","suggestions":[]}"#;
    const DONE_ONE: &str = r#"{"type":"complete","status":"review_completed","findings":1}"#;
    const DONE_NONE: &str = r#"{"type":"complete","status":"review_completed","findings":0}"#;

    /// A fixture `coderabbit` first on `PATH`; the real CLI is never reached.
    struct Fixture {
        dir: tempfile::TempDir,
        _program: FakeProgram,
    }

    impl Fixture {
        /// `body` is shell run after the fixture records its pid, arguments
        /// and working directory.
        async fn install(body: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let here = dir.path();
            let program = FakeProgram::install(
                "coderabbit",
                &format!(
                    "here={here:?}\n\
                     printf '%s\\n' \"$@\" > \"$here/args\"\n\
                     pwd -P > \"$here/cwd\"\n\
                     printf '%s\\n' \"$$\" > \"$here/pid\"\n\
                     {body}\n"
                ),
            )
            .await;
            Fixture {
                dir,
                _program: program,
            }
        }

        /// A fixture that prints `lines`, writes `stderr` and exits `code`.
        async fn printing(lines: &[&str], stderr: &str, code: i32) -> Self {
            let mut body = String::new();
            for line in lines {
                body.push_str(&format!("cat <<'EVENT'\n{line}\nEVENT\n"));
            }
            body.push_str(&format!(
                "cat >&2 <<'ERRORS'\n{stderr}\nERRORS\nexit {code}"
            ));
            Self::install(&body).await
        }

        async fn run(&self) -> CodeRabbitReview {
            let (_cancel, mut cancelled) = tokio::sync::watch::channel(false);
            self.run_with(&mut cancelled, Duration::from_secs(30)).await
        }

        async fn run_with(
            &self,
            cancelled: &mut tokio::sync::watch::Receiver<bool>,
            timeout: Duration,
        ) -> CodeRabbitReview {
            run_review(
                self.dir.path().to_str().unwrap(),
                "abc123",
                cancelled,
                timeout,
            )
            .await
        }

        fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
        }

        /// The pid the fixture recorded in `name` once it started.
        async fn recorded(&self, name: &str) -> u32 {
            for _ in 0..400 {
                if let Ok(pid) = self.read(name).trim().parse() {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("the fixture never recorded {name}");
        }

        async fn pid(&self) -> u32 {
            self.recorded("pid").await
        }
    }

    /// Gone, or a zombie awaiting the runtime's reaper.
    fn is_running(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|stat| !stat.contains(") Z "))
    }

    fn failure(review: &CodeRabbitReview) -> &str {
        review.failure.as_deref().expect("a failed review")
    }

    #[tokio::test]
    async fn the_cli_is_invoked_for_the_task_range_in_the_checkout_without_paid_usage() {
        let fixture = Fixture::printing(&[DONE_NONE], "", 0).await;
        let review = fixture.run().await;

        assert_eq!(review, CodeRabbitReview::default());
        let dir = fixture.dir.path().to_str().unwrap();
        assert_eq!(
            fixture.read("args").lines().collect::<Vec<_>>(),
            [
                "review",
                "--agent",
                "--fresh",
                "--include-untracked",
                "--base-commit",
                "abc123",
                "--dir",
                dir
            ]
        );
        assert_eq!(
            std::path::Path::new(fixture.read("cwd").trim()),
            fixture.dir.path().canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn findings_are_read_and_progress_events_ignored() {
        let unverified = r#"{"type":"unverified_finding","verificationUnavailable":true,"fileName":"b.rs","startLine":1,"endLine":1,"comment":"Maybe wrong."}"#;
        let fixture = Fixture::printing(
            &[
                r#"{"type":"review_context","reviewType":"all"}"#,
                r#"{"type":"status","phase":"setup","status":"preparing"}"#,
                r#"{"type":"heartbeat","status":"reviewing"}"#,
                FINDING,
                unverified,
                r#"{"type":"complete","status":"review_completed","findings":1,"unverifiedFindings":1}"#,
            ],
            "",
            0,
        )
        .await;
        let review = fixture.run().await;

        assert_eq!(review.failure, None);
        assert_eq!(
            review.findings_text(),
            "- ISSUE: [CodeRabbit major] src/a.rs:3-4 - Unchecked index\n  This panics.\n  \
             - ISSUE: not a second one\n- ISSUE: [CodeRabbit unverified] b.rs:1-1 - Maybe \
             wrong.\n  Maybe wrong.\n"
        );
    }

    #[tokio::test]
    async fn a_rejected_option_reports_the_exit_status_and_error_text() {
        let fixture = Fixture::printing(&[], "error: unknown option '--prompt-only'", 1).await;
        let review = fixture.run().await;

        assert_eq!(
            failure(&review),
            "the CodeRabbit CLI exited with code 1: error: unknown option '--prompt-only'"
        );
        assert!(review.findings.is_empty());
    }

    #[tokio::test]
    async fn a_failing_exit_keeps_the_findings_read_before_it() {
        let fixture = Fixture::printing(&[FINDING, DONE_ONE], "", 1).await;
        let review = fixture.run().await;

        assert_eq!(failure(&review), "the CodeRabbit CLI exited with code 1");
        assert_eq!(review.findings.len(), 1);
    }

    #[tokio::test]
    async fn a_rate_limit_reports_the_clis_message() {
        let fixture = Fixture::printing(
            &[r#"{"type":"error","errorType":"rate_limit","message":"Rate limit exceeded, try again in 12 minutes","recoverable":true}"#],
            "",
            1,
        )
        .await;

        assert_eq!(
            failure(&fixture.run().await),
            "CodeRabbit reported an error (rate_limit): Rate limit exceeded, try again in 12 \
             minutes"
        );
    }

    #[tokio::test]
    async fn a_request_to_spend_credits_is_reported_and_never_accepted() {
        let fixture = Fixture::printing(
            &[
                r#"{"type":"action_required","action":"rerun_with_use_credits","command":"coderabbit review --use-credits"}"#,
                r#"{"type":"error","errorType":"rate_limit","message":"Rate limit exceeded"}"#,
            ],
            "",
            1,
        )
        .await;
        let review = fixture.run().await;

        // The later error does not replace what a person has to decide.
        assert!(failure(&review).contains("does not authorize paid usage"));
        assert!(!fixture.read("args").contains("--use-credits"));
    }

    #[tokio::test]
    async fn output_that_is_not_a_completed_review_is_a_failure() {
        let cases: &[(&[&str], &str)] = &[
            (&[], "without a completion event"),
            (&["Review complete, no issues!"], "not a JSON event"),
            (&["{\"findings\":[]}", DONE_NONE], "no type"),
            (
                &[r#"{"type":"finding","fileName":"a.rs"}"#, DONE_NONE],
                "malformed finding",
            ),
            (&[r#"{"type":"complete"}"#], "malformed completion"),
            (&[DONE_ONE], "counted 1 findings and 0 were read"),
            (
                &[
                    r#"{"type":"complete","status":"review_skipped","findings":0,"message":"No files"}"#,
                ],
                "did not review the changes (review_skipped): No files",
            ),
            (
                &[
                    r#"{"type":"complete","status":"review_completed","findings":0,"outcome":"failed"}"#,
                ],
                "review failed",
            ),
            (
                &[
                    r#"{"type":"complete","status":"review_completed","findings":0,"unreviewedFiles":["a"]}"#,
                ],
                "1 file(s) unreviewed",
            ),
        ];
        for (lines, expected) in cases {
            let fixture = Fixture::printing(lines, "", 0).await;
            let review = fixture.run().await;
            assert!(
                failure(&review).contains(expected),
                "{lines:?} -> {review:?}"
            );
        }
    }

    #[tokio::test]
    async fn output_larger_than_a_pipe_buffer_does_not_stall_the_run() {
        let fixture = Fixture::install(&format!(
            "i=0; while [ $i -lt 3000 ]; do \
             echo '{{\"type\":\"heartbeat\",\"status\":\"reviewing\"}}'; \
             echo 'noise noise noise noise noise noise noise noise' >&2; i=$((i+1)); done\n\
             echo '{DONE_NONE}'"
        ))
        .await;

        assert_eq!(fixture.run().await, CodeRabbitReview::default());
    }

    #[tokio::test]
    async fn a_run_past_its_timeout_is_killed_and_reported() {
        let fixture = Fixture::install(&format!("echo '{FINDING}'\nexec sleep 300")).await;
        let (_cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let review = fixture
            .run_with(&mut cancelled, Duration::from_secs(2))
            .await;

        assert!(failure(&review).contains("did not finish within 2 seconds"));
        assert_eq!(review.findings.len(), 1);
        assert!(!is_running(fixture.pid().await));
    }

    #[tokio::test]
    async fn a_cancelled_run_is_killed_and_reaped_with_no_result() {
        let fixture = Fixture::install("exec sleep 300").await;
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let (review, pid) = tokio::join!(
            fixture.run_with(&mut cancelled, Duration::from_secs(60)),
            async {
                let pid = fixture.pid().await;
                cancel.send(true).unwrap();
                pid
            }
        );

        assert_eq!(review, CodeRabbitReview::default());
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    /// Shell that leaves a `sleep` running in the background, records its pid
    /// as `descendant`, then becomes the leader's own foreground `sleep`.
    const WITH_DESCENDANT: &str = "sleep 300 &\necho $! > \"$here/descendant\"\nsleep 300";

    /// A process the run never started. It must survive every cleanup below.
    struct Unrelated(std::process::Child);

    impl Unrelated {
        fn start() -> Self {
            Unrelated(
                std::process::Command::new("sleep")
                    .arg("300")
                    .spawn()
                    .expect("sleep"),
            )
        }
    }

    impl Drop for Unrelated {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    async fn a_timed_out_run_ends_the_descendants_it_started() {
        let fixture = Fixture::install(WITH_DESCENDANT).await;
        let unrelated = Unrelated::start();
        let (_cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let review = fixture
            .run_with(&mut cancelled, Duration::from_secs(2))
            .await;

        assert!(failure(&review).contains("did not finish within 2 seconds"));
        assert!(!is_running(fixture.pid().await));
        assert!(!is_running(fixture.recorded("descendant").await));
        assert!(
            is_running(unrelated.0.id()),
            "an unrelated process was ended"
        );
    }

    #[tokio::test]
    async fn a_cancelled_run_ends_the_descendants_it_started() {
        let fixture = Fixture::install(WITH_DESCENDANT).await;
        let unrelated = Unrelated::start();
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let (review, (leader, descendant)) = tokio::join!(
            fixture.run_with(&mut cancelled, Duration::from_secs(60)),
            async {
                let leader = fixture.pid().await;
                let descendant = fixture.recorded("descendant").await;
                cancel.send(true).unwrap();
                (leader, descendant)
            }
        );

        assert_eq!(review, CodeRabbitReview::default());
        assert!(!is_running(leader));
        assert!(!is_running(descendant));
        assert!(
            is_running(unrelated.0.id()),
            "an unrelated process was ended"
        );
    }

    #[tokio::test]
    async fn dropping_the_run_ends_the_descendants_it_started() {
        let fixture = Fixture::install(WITH_DESCENDANT).await;
        let unrelated = Unrelated::start();
        let (_cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let (leader, descendant) = {
            let run = fixture.run_with(&mut cancelled, Duration::from_secs(60));
            tokio::pin!(run);
            tokio::select! {
                _ = &mut run => panic!("the fixture never exits"),
                ids = async {
                    (fixture.pid().await, fixture.recorded("descendant").await)
                } => ids,
            }
        };

        for _ in 0..400 {
            if !is_running(leader) && !is_running(descendant) {
                assert!(
                    is_running(unrelated.0.id()),
                    "an unrelated process was ended"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the dropped run left its group running");
    }

    #[tokio::test]
    async fn a_descendant_outliving_a_completed_review_is_ended() {
        let fixture = Fixture::install(&format!(
            "sleep 300 &\necho $! > \"$here/descendant\"\ncat <<'EVENT'\n{DONE_NONE}\nEVENT\nexit 0"
        ))
        .await;
        let review = tokio::time::timeout(Duration::from_secs(20), fixture.run())
            .await
            .expect("a descendant held the review open");

        assert_eq!(review, CodeRabbitReview::default());
        assert!(!is_running(fixture.recorded("descendant").await));
    }

    #[tokio::test]
    async fn a_run_already_cancelled_starts_nothing() {
        let fixture = Fixture::printing(&[DONE_NONE], "", 0).await;
        let (_cancel, mut cancelled) = tokio::sync::watch::channel(true);
        let review = fixture
            .run_with(&mut cancelled, Duration::from_secs(5))
            .await;

        assert_eq!(review, CodeRabbitReview::default());
        assert_eq!(fixture.read("pid"), "");
    }

    #[tokio::test]
    async fn dropping_the_run_kills_the_cli() {
        let fixture = Fixture::install("exec sleep 300").await;
        let (_cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let pid = {
            let run = fixture.run_with(&mut cancelled, Duration::from_secs(60));
            tokio::pin!(run);
            tokio::select! {
                _ = &mut run => panic!("the fixture never exits"),
                pid = fixture.pid() => pid,
            }
        };

        for _ in 0..400 {
            if !is_running(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the dropped run left the CLI running");
    }
}
