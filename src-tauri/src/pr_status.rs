//! What GitHub last said about the pull requests linked to tasks.
//!
//! One owner asks: [`PrStatuses::refresh`] is the only code that runs
//! `gh pr view` for a status, and both the executor's background poll and the
//! explicit refresh commands go through it. Every answer lands in one
//! in-memory cache keyed by repository and number, which the board reads
//! without reaching GitHub or the disk.
//!
//! Nothing here is persisted. Checks, reviews and mergeability change on
//! GitHub's schedule, not SlashIt's, and a restart simply asks again. The only
//! part of a pull request's status that is durable is its open/merged/closed
//! state, which the callers record on the task themselves.
//!
//! Tauri-free, like the executor that polls through it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsString;
use std::time::Duration;

/// How long one `gh pr view` may take before it is killed and reported as a
/// timeout.
///
/// Measured against GitHub on the development machine, the status query
/// takes about 1.3 s warm and took 9.8 s once, cold. Twenty seconds is about
/// twice that worst case, so a single slow answer is not mistaken for a hang.
/// The executor asks about a few pull requests at a time, so a pass with
/// hung `gh` processes is held up for at most this long per batch.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// How many failing check names a status keeps. The drawer shows these and
/// summarizes the rest as a count.
pub const FAILING_NAMES_KEPT: usize = 3;

/// The fields one status query asks for.
const FIELDS: &str = "state,statusCheckRollup,reviewDecision,mergeable";

/// A pull request, as GitHub identifies it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrKey {
    /// `owner/name`.
    pub repo: String,
    pub number: u32,
}

impl PrKey {
    pub fn new(repo: impl Into<String>, number: u32) -> Self {
        Self { repo: repo.into(), number }
    }
}

/// A pull request's status at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatus {
    pub state: PrState,
    pub checks: ChecksState,
    /// Names of failing checks, at most [`FAILING_NAMES_KEPT`], in the order
    /// GitHub listed them.
    pub failing_checks: Vec<String>,
    /// How many distinct checks are failing, including those not named.
    pub failing_check_count: u32,
    /// `None` when the repository requires no review.
    pub review_decision: Option<ReviewDecision>,
    /// `None` when GitHub reported nothing SlashIt recognizes.
    pub mergeable: Option<Mergeability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    Merged,
    Unknown,
}

impl PrState {
    fn from_gh(s: &str) -> Self {
        match s {
            "OPEN" => Self::Open,
            "CLOSED" => Self::Closed,
            "MERGED" => Self::Merged,
            _ => Self::Unknown,
        }
    }

    /// The spelling a task's pull request reference records, or `None` for a
    /// state that is not worth recording.
    pub fn as_recorded(self) -> Option<&'static str> {
        match self {
            Self::Open => Some("OPEN"),
            Self::Closed => Some("CLOSED"),
            Self::Merged => Some("MERGED"),
            Self::Unknown => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Merged | Self::Closed)
    }
}

/// What a pull request's checks add up to.
///
/// Only [`ChecksState::Passing`] is good news. No checks at all, and checks
/// SlashIt cannot read, are reported as such and never as passing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    /// Every reported check succeeded, or was neutral or skipped.
    Passing,
    /// At least one check finished and failed.
    Failing,
    /// None has failed and at least one has not finished.
    Pending,
    /// GitHub reported an empty list: nothing runs on this pull request.
    NoChecks,
    /// GitHub reported no list, or a value SlashIt does not recognize.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

impl ReviewDecision {
    fn from_gh(s: &str) -> Option<Self> {
        match s {
            "APPROVED" => Some(Self::Approved),
            "CHANGES_REQUESTED" => Some(Self::ChangesRequested),
            "REVIEW_REQUIRED" => Some(Self::ReviewRequired),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mergeability {
    Mergeable,
    Conflicting,
    /// GitHub has not finished computing it.
    Unknown,
}

impl Mergeability {
    fn from_gh(s: &str) -> Option<Self> {
        match s {
            "MERGEABLE" => Some(Self::Mergeable),
            "CONFLICTING" => Some(Self::Conflicting),
            "UNKNOWN" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// What one entry of `statusCheckRollup` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckOutcome {
    Passed,
    Failed,
    Running,
    Unrecognized,
}

/// Read one rollup entry. GitHub mixes two kinds: a `CheckRun` (Actions and
/// most apps) with a `status` and, once completed, a `conclusion`; and a
/// legacy commit `StatusContext` with a single `state`.
fn classify_check(check: &Value) -> CheckOutcome {
    let text = |key: &str| check.get(key).and_then(Value::as_str);
    let is_status_context = match text("__typename") {
        Some("StatusContext") => true,
        Some("CheckRun") => false,
        Some(_) => return CheckOutcome::Unrecognized,
        None => check.get("state").is_some() && check.get("status").is_none(),
    };

    if is_status_context {
        return match text("state") {
            Some("SUCCESS") => CheckOutcome::Passed,
            Some("FAILURE" | "ERROR") => CheckOutcome::Failed,
            Some("PENDING" | "EXPECTED") => CheckOutcome::Running,
            _ => CheckOutcome::Unrecognized,
        };
    }

    match text("status") {
        Some("COMPLETED") => match text("conclusion") {
            Some("SUCCESS" | "NEUTRAL" | "SKIPPED") => CheckOutcome::Passed,
            Some("FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE") => {
                CheckOutcome::Failed
            }
            _ => CheckOutcome::Unrecognized,
        },
        Some("QUEUED" | "IN_PROGRESS" | "WAITING" | "PENDING" | "REQUESTED") => CheckOutcome::Running,
        _ => CheckOutcome::Unrecognized,
    }
}

/// The name a person knows a check by.
fn check_name(check: &Value) -> Option<String> {
    ["name", "context"]
        .iter()
        .find_map(|key| check.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// What a whole rollup adds up to, and the failing checks' names.
///
/// A failure outranks everything: it is finished and it is bad news. An
/// entry SlashIt cannot read outranks one still running, because it may be a
/// failure spelled in a way SlashIt does not know yet, and "running" would
/// hide that. Passing needs every entry to have passed.
fn summarize_checks(rollup: Option<&Value>) -> (ChecksState, Vec<String>, u32) {
    let Some(checks) = rollup.and_then(Value::as_array) else {
        return (ChecksState::Unknown, Vec::new(), 0);
    };
    if checks.is_empty() {
        return (ChecksState::NoChecks, Vec::new(), 0);
    }

    let mut failing: Vec<String> = Vec::new();
    let mut unnamed_failures = 0u32;
    let (mut running, mut unrecognized) = (false, false);
    for check in checks {
        match classify_check(check) {
            CheckOutcome::Passed => {}
            CheckOutcome::Failed => match check_name(check) {
                Some(name) if !failing.contains(&name) => failing.push(name),
                Some(_) => {}
                None => unnamed_failures += 1,
            },
            CheckOutcome::Running => running = true,
            CheckOutcome::Unrecognized => unrecognized = true,
        }
    }

    let count = failing.len() as u32 + unnamed_failures;
    let state = if count > 0 {
        ChecksState::Failing
    } else if unrecognized {
        ChecksState::Unknown
    } else if running {
        ChecksState::Pending
    } else {
        ChecksState::Passing
    };
    failing.truncate(FAILING_NAMES_KEPT);
    (state, failing, count)
}

/// Read what `gh pr view --json state,statusCheckRollup,reviewDecision,mergeable`
/// printed.
pub fn parse_pr_status(json: &Value) -> PrStatus {
    let (checks, failing_checks, failing_check_count) = summarize_checks(json.get("statusCheckRollup"));
    PrStatus {
        state: PrState::from_gh(json.get("state").and_then(Value::as_str).unwrap_or("")),
        checks,
        failing_checks,
        failing_check_count,
        review_decision: json.get("reviewDecision").and_then(Value::as_str).and_then(ReviewDecision::from_gh),
        mergeable: json.get("mergeable").and_then(Value::as_str).and_then(Mergeability::from_gh),
    }
}

/// Why a status could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrFetchError {
    pub kind: PrFetchErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrFetchErrorKind {
    /// `gh` did not answer within [`FETCH_TIMEOUT`] and was stopped.
    Timeout,
    /// `gh` is not signed in to GitHub.
    Auth,
    /// There is no `gh` to run.
    NotInstalled,
    /// Anything else: GitHub refused, the network failed, the answer was
    /// unreadable.
    Failed,
}

impl std::fmt::Display for PrFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl PrFetchError {
    fn new(kind: PrFetchErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }

    /// Classify a `gh` that exited unsuccessfully. `gh` exits with 4 when it
    /// needs a sign-in.
    fn from_exit(code: Option<i32>, stderr: &str) -> Self {
        let said = stderr.trim();
        let lower = said.to_lowercase();
        let auth = code == Some(4)
            || lower.contains("gh auth login")
            || lower.contains("not logged in")
            || lower.contains("http 401")
            || lower.contains("bad credentials");
        if auth {
            return Self::new(
                PrFetchErrorKind::Auth,
                "GitHub CLI is not signed in. Run `gh auth login`, then refresh.",
            );
        }
        let first_line = said.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        let message = if first_line.is_empty() {
            "GitHub CLI failed without saying why".to_string()
        } else {
            let mut line: String = first_line.chars().take(300).collect();
            if first_line.chars().count() > 300 {
                line.push('…');
            }
            format!("GitHub CLI failed: {line}")
        };
        Self::new(PrFetchErrorKind::Failed, message)
    }
}

/// One pull request's cache entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatusEntry {
    pub repo: String,
    pub number: u32,
    /// The last status read successfully. A later failure never clears it.
    pub status: Option<PrStatus>,
    /// When `status` was read.
    pub fetched_at: Option<DateTime<Utc>>,
    /// When the latest recorded attempt, successful or not, started.
    pub attempted_at: DateTime<Utc>,
    /// Why the latest attempt failed; `None` when it succeeded.
    pub error: Option<PrFetchError>,
}

/// The single owner of pull request status: the program it runs, and the
/// cache every answer lands in.
pub struct PrStatuses {
    program: OsString,
    timeout: Duration,
    entries: std::sync::RwLock<HashMap<PrKey, PrStatusEntry>>,
}

impl PrStatuses {
    /// Ask GitHub through the `gh` on `PATH`.
    pub fn github() -> Self {
        Self::with_program("gh", FETCH_TIMEOUT)
    }

    /// Ask through `program` instead, which answers like `gh`.
    pub fn with_program(program: impl Into<OsString>, timeout: Duration) -> Self {
        Self { program: program.into(), timeout, entries: std::sync::RwLock::new(HashMap::new()) }
    }

    /// Ask GitHub for `key`'s status now and record the answer.
    ///
    /// A failure is recorded beside the last good status rather than in place
    /// of it, and returned.
    pub async fn refresh(&self, key: &PrKey) -> Result<PrStatus, PrFetchError> {
        let started = Utc::now();
        let result = self.fetch(key).await;
        self.record(key, started, Utc::now(), &result);
        result
    }

    /// The cached entry for `key`, without asking anyone.
    pub fn get(&self, key: &PrKey) -> Option<PrStatusEntry> {
        self.entries.read().unwrap_or_else(|e| e.into_inner()).get(key).cloned()
    }

    /// The cached entries among `keys`, without asking anyone.
    pub fn entries_for<'a>(&self, keys: impl IntoIterator<Item = &'a PrKey>) -> Vec<PrStatusEntry> {
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        let mut found: Vec<PrStatusEntry> = keys.into_iter().filter_map(|k| entries.get(k).cloned()).collect();
        found.sort_by(|a, b| (&a.repo, a.number).cmp(&(&b.repo, b.number)));
        found.dedup_by(|a, b| a.repo == b.repo && a.number == b.number);
        found
    }

    /// Store `status` as just read, for tests elsewhere in the crate.
    #[cfg(test)]
    pub fn remember(&self, key: &PrKey, status: PrStatus) {
        let now = Utc::now();
        self.record(key, now, now, &Ok(status));
    }

    /// Store an attempt that started at `started` and ended at `finished`.
    ///
    /// An answer to an attempt older than the one already recorded is
    /// dropped: a slow poll finishing after a quick manual refresh must not
    /// put the older reading back.
    fn record(
        &self,
        key: &PrKey,
        started: DateTime<Utc>,
        finished: DateTime<Utc>,
        result: &Result<PrStatus, PrFetchError>,
    ) {
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        let entry = entries.entry(key.clone()).or_insert_with(|| PrStatusEntry {
            repo: key.repo.clone(),
            number: key.number,
            status: None,
            fetched_at: None,
            attempted_at: started,
            error: None,
        });
        if started < entry.attempted_at {
            return;
        }
        entry.attempted_at = started;
        match result {
            Ok(status) => {
                entry.status = Some(status.clone());
                entry.fetched_at = Some(finished);
                entry.error = None;
            }
            Err(e) => entry.error = Some(e.clone()),
        }
    }

    async fn fetch(&self, key: &PrKey) -> Result<PrStatus, PrFetchError> {
        let number = key.number.to_string();
        let args = ["pr", "view", number.as_str(), "--repo", key.repo.as_str(), "--json", FIELDS];
        let stdout = run_bounded(&self.program, &args, self.timeout).await?;
        let json: Value = serde_json::from_slice(&stdout).map_err(|e| {
            PrFetchError::new(PrFetchErrorKind::Failed, format!("GitHub's answer could not be read: {e}"))
        })?;
        Ok(parse_pr_status(&json))
    }
}

/// Run `program` with `args` and return its standard output, or stop it once
/// `timeout` has passed.
///
/// The child leads its own process group, and a timeout kills the whole group
/// before reaping the leader, so nothing it started is left behind.
async fn run_bounded(program: &OsString, args: &[&str], timeout: Duration) -> Result<Vec<u8>, PrFetchError> {
    use tokio::io::AsyncReadExt;

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Never wait on a person: no prompts, no update check.
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            PrFetchError::new(
                PrFetchErrorKind::NotInstalled,
                "GitHub CLI (gh) is not installed or not on PATH",
            )
        } else {
            PrFetchError::new(PrFetchErrorKind::Failed, format!("GitHub CLI could not be started: {e}"))
        }
    })?;
    let pid = child.id();
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let finished = tokio::time::timeout(timeout, async {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let read_out = async {
            if let Some(pipe) = stdout_pipe.as_mut() {
                let _ = pipe.read_to_end(&mut stdout).await;
            }
        };
        let read_err = async {
            if let Some(pipe) = stderr_pipe.as_mut() {
                let _ = pipe.read_to_end(&mut stderr).await;
            }
        };
        tokio::join!(read_out, read_err);
        let status = child.wait().await;
        (status, stdout, stderr)
    })
    .await;

    match finished {
        Ok((Ok(status), stdout, _)) if status.success() => Ok(stdout),
        Ok((Ok(status), _, stderr)) => {
            Err(PrFetchError::from_exit(status.code(), &String::from_utf8_lossy(&stderr)))
        }
        Ok((Err(e), _, _)) => Err(PrFetchError::new(
            PrFetchErrorKind::Failed,
            format!("GitHub CLI could not be waited for: {e}"),
        )),
        Err(_) => {
            // The leader has not been reaped (the wait above never
            // completed), so its pid is still this group's id and cannot
            // have been reused.
            #[cfg(unix)]
            if let Some(pid) = pid {
                // Safety: `pid` leads a process group this function created
                // with `process_group(0)`, and it is not yet reaped.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
            #[cfg(not(unix))]
            let _ = pid;
            let _ = child.kill().await;
            Err(PrFetchError::new(
                PrFetchErrorKind::Timeout,
                format!("GitHub did not answer within {} seconds", timeout.as_secs()),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(status: &str, conclusion: Option<&str>) -> Value {
        json!({ "__typename": "CheckRun", "name": format!("{status}-{conclusion:?}"), "status": status, "conclusion": conclusion })
    }

    fn context(state: &str) -> Value {
        json!({ "__typename": "StatusContext", "context": format!("ctx-{state}"), "state": state })
    }

    fn state_of(checks: Vec<Value>) -> ChecksState {
        summarize_checks(Some(&Value::Array(checks))).0
    }


    #[test]
    fn pr_state_from_gh_known_values() {
        assert_eq!(PrState::from_gh("OPEN"), PrState::Open);
        assert_eq!(PrState::from_gh("CLOSED"), PrState::Closed);
        assert_eq!(PrState::from_gh("MERGED"), PrState::Merged);
        assert_eq!(PrState::from_gh("garbage"), PrState::Unknown);
    }

    #[test]
    fn review_decision_from_gh_known_values() {
        assert_eq!(ReviewDecision::from_gh("APPROVED"), Some(ReviewDecision::Approved));
        assert_eq!(ReviewDecision::from_gh("CHANGES_REQUESTED"), Some(ReviewDecision::ChangesRequested));
        assert_eq!(ReviewDecision::from_gh("REVIEW_REQUIRED"), Some(ReviewDecision::ReviewRequired));
        assert_eq!(ReviewDecision::from_gh("OTHER"), None);
    }

    #[test]
    fn mergeability_from_gh_known_values() {
        assert_eq!(Mergeability::from_gh("MERGEABLE"), Some(Mergeability::Mergeable));
        assert_eq!(Mergeability::from_gh("CONFLICTING"), Some(Mergeability::Conflicting));
        assert_eq!(Mergeability::from_gh("UNKNOWN"), Some(Mergeability::Unknown));
        assert_eq!(Mergeability::from_gh("other"), None);
    }

    #[test]
    fn each_check_run_result_maps_to_one_outcome() {
        let table = [
            ("COMPLETED", Some("SUCCESS"), CheckOutcome::Passed),
            ("COMPLETED", Some("NEUTRAL"), CheckOutcome::Passed),
            ("COMPLETED", Some("SKIPPED"), CheckOutcome::Passed),
            ("COMPLETED", Some("FAILURE"), CheckOutcome::Failed),
            ("COMPLETED", Some("CANCELLED"), CheckOutcome::Failed),
            ("COMPLETED", Some("TIMED_OUT"), CheckOutcome::Failed),
            ("COMPLETED", Some("ACTION_REQUIRED"), CheckOutcome::Failed),
            ("COMPLETED", Some("STARTUP_FAILURE"), CheckOutcome::Failed),
            ("COMPLETED", Some("STALE"), CheckOutcome::Unrecognized),
            ("COMPLETED", None, CheckOutcome::Unrecognized),
            ("QUEUED", None, CheckOutcome::Running),
            ("IN_PROGRESS", None, CheckOutcome::Running),
            ("WAITING", None, CheckOutcome::Running),
            ("PENDING", None, CheckOutcome::Running),
            ("REQUESTED", None, CheckOutcome::Running),
            ("SOMETHING_NEW", None, CheckOutcome::Unrecognized),
        ];
        for (status, conclusion, expected) in table {
            assert_eq!(classify_check(&run(status, conclusion)), expected, "{status} {conclusion:?}");
        }
    }

    #[test]
    fn each_legacy_status_maps_to_one_outcome() {
        let table = [
            ("SUCCESS", CheckOutcome::Passed),
            ("FAILURE", CheckOutcome::Failed),
            ("ERROR", CheckOutcome::Failed),
            ("PENDING", CheckOutcome::Running),
            ("EXPECTED", CheckOutcome::Running),
            ("WHATEVER", CheckOutcome::Unrecognized),
        ];
        for (state, expected) in table {
            assert_eq!(classify_check(&context(state)), expected, "{state}");
        }
        // Without a type name, a lone `state` is a legacy status.
        assert_eq!(classify_check(&json!({ "context": "ci", "state": "SUCCESS" })), CheckOutcome::Passed);
        assert_eq!(classify_check(&json!({ "__typename": "Mystery", "state": "SUCCESS" })), CheckOutcome::Unrecognized);
    }

    #[test]
    fn a_rollup_adds_up_with_failure_first() {
        let passed = || run("COMPLETED", Some("SUCCESS"));
        let failed = || run("COMPLETED", Some("FAILURE"));
        let running = || run("IN_PROGRESS", None);
        let odd = || run("COMPLETED", Some("STALE"));

        assert_eq!(summarize_checks(None).0, ChecksState::Unknown);
        assert_eq!(summarize_checks(Some(&Value::Null)).0, ChecksState::Unknown);
        assert_eq!(summarize_checks(Some(&json!("nope"))).0, ChecksState::Unknown);
        assert_eq!(state_of(vec![]), ChecksState::NoChecks);
        assert_eq!(state_of(vec![passed(), passed()]), ChecksState::Passing);
        assert_eq!(state_of(vec![passed(), context("SUCCESS")]), ChecksState::Passing);
        assert_eq!(state_of(vec![run("COMPLETED", Some("SKIPPED")), run("COMPLETED", Some("NEUTRAL"))]), ChecksState::Passing);
        assert_eq!(state_of(vec![passed(), running()]), ChecksState::Pending);
        assert_eq!(state_of(vec![passed(), context("PENDING")]), ChecksState::Pending);
        assert_eq!(state_of(vec![passed(), context("EXPECTED")]), ChecksState::Pending);
        assert_eq!(state_of(vec![running(), failed()]), ChecksState::Failing);
        assert_eq!(state_of(vec![failed(), odd()]), ChecksState::Failing);
        assert_eq!(state_of(vec![context("ERROR"), running()]), ChecksState::Failing);
        assert_eq!(state_of(vec![passed(), odd()]), ChecksState::Unknown);
        assert_eq!(state_of(vec![running(), odd()]), ChecksState::Unknown);
    }

    #[test]
    fn failing_names_are_kept_distinct_and_capped_but_all_counted() {
        let fail = |name: &str| json!({ "__typename": "CheckRun", "name": name, "status": "COMPLETED", "conclusion": "FAILURE" });
        let (state, names, count) = summarize_checks(Some(&json!([
            fail("lint"),
            { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS" },
            fail("test"),
            fail("lint"),
            { "__typename": "StatusContext", "context": "coverage", "state": "ERROR" },
            fail("docs"),
            fail(""),
        ])));
        assert_eq!(state, ChecksState::Failing);
        assert_eq!(names, vec!["lint", "test", "coverage"]);
        assert_eq!(count, 5, "lint, test, coverage, docs and one unnamed");
    }

    #[test]
    fn a_whole_answer_parses_into_one_status() {
        let status = parse_pr_status(&json!({
            "state": "OPEN",
            "reviewDecision": "CHANGES_REQUESTED",
            "mergeable": "CONFLICTING",
            "statusCheckRollup": [
                { "__typename": "CheckRun", "name": "check", "status": "COMPLETED", "conclusion": "TIMED_OUT" }
            ],
        }));
        assert_eq!(
            status,
            PrStatus {
                state: PrState::Open,
                checks: ChecksState::Failing,
                failing_checks: vec!["check".to_string()],
                failing_check_count: 1,
                review_decision: Some(ReviewDecision::ChangesRequested),
                mergeable: Some(Mergeability::Conflicting),
            }
        );

        let bare = parse_pr_status(&json!({ "state": "MERGED", "reviewDecision": "", "mergeable": "UNKNOWN" }));
        assert_eq!(bare.state, PrState::Merged);
        assert_eq!(bare.checks, ChecksState::Unknown, "no rollup at all is not 'no checks'");
        assert_eq!(bare.review_decision, None);
        assert_eq!(bare.mergeable, Some(Mergeability::Unknown));

        let odd = parse_pr_status(&json!({ "state": "DRAFTISH", "reviewDecision": "MAYBE", "mergeable": "SOMETIMES", "statusCheckRollup": null }));
        assert_eq!(odd.state, PrState::Unknown);
        assert_eq!(odd.checks, ChecksState::Unknown);
        assert_eq!(odd.review_decision, None);
        assert_eq!(odd.mergeable, None);
    }

    /// The wire shape the frontend's mirror in `src/models/task.rs` reads.
    /// Both sides test against this same literal.
    #[test]
    fn an_entry_serializes_to_the_shape_the_frontend_reads() {
        let at = DateTime::parse_from_rfc3339("2026-09-30T12:00:00Z").unwrap().with_timezone(&Utc);
        let entry = PrStatusEntry {
            repo: "o/r".to_string(),
            number: 7,
            status: Some(PrStatus {
                state: PrState::Open,
                checks: ChecksState::NoChecks,
                failing_checks: vec![],
                failing_check_count: 0,
                review_decision: Some(ReviewDecision::ReviewRequired),
                mergeable: Some(Mergeability::Mergeable),
            }),
            fetched_at: Some(at),
            attempted_at: at,
            error: Some(PrFetchError::new(PrFetchErrorKind::Timeout, "slow")),
        };
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            json!({
                "repo": "o/r",
                "number": 7,
                "status": {
                    "state": "open",
                    "checks": "no_checks",
                    "failing_checks": [],
                    "failing_check_count": 0,
                    "review_decision": "review_required",
                    "mergeable": "mergeable"
                },
                "fetched_at": "2026-09-30T12:00:00Z",
                "attempted_at": "2026-09-30T12:00:00Z",
                "error": { "kind": "timeout", "message": "slow" }
            })
        );
    }

    #[test]
    fn a_failed_exit_is_classified() {
        assert_eq!(PrFetchError::from_exit(Some(4), "").kind, PrFetchErrorKind::Auth);
        let auth = PrFetchError::from_exit(Some(1), "To get started with GitHub CLI, please run:  gh auth login");
        assert_eq!(auth.kind, PrFetchErrorKind::Auth);
        assert!(auth.message.contains("gh auth login"));
        let other = PrFetchError::from_exit(Some(1), "\nGraphQL: Could not resolve to a PullRequest\nmore");
        assert_eq!(other.kind, PrFetchErrorKind::Failed);
        assert_eq!(other.message, "GitHub CLI failed: GraphQL: Could not resolve to a PullRequest");
        assert_eq!(PrFetchError::from_exit(Some(1), "  ").message, "GitHub CLI failed without saying why");
    }

    fn status(state: PrState, checks: ChecksState) -> PrStatus {
        PrStatus { state, checks, failing_checks: vec![], failing_check_count: 0, review_decision: None, mergeable: None }
    }

    #[test]
    fn a_failure_keeps_the_last_good_status_and_a_success_clears_the_error() {
        let cache = PrStatuses::with_program("unused", FETCH_TIMEOUT);
        let key = PrKey::new("o/r", 1);
        let t = |s: i64| DateTime::from_timestamp(1_800_000_000 + s, 0).unwrap();
        let good = status(PrState::Open, ChecksState::Passing);

        cache.record(&key, t(0), t(1), &Ok(good.clone()));
        let fail = PrFetchError::new(PrFetchErrorKind::Failed, "down");
        cache.record(&key, t(30), t(31), &Err(fail.clone()));
        let entry = cache.get(&key).unwrap();
        assert_eq!(entry.status, Some(good), "a failed refresh never forgets what was known");
        assert_eq!(entry.fetched_at, Some(t(1)));
        assert_eq!(entry.attempted_at, t(30));
        assert_eq!(entry.error, Some(fail));

        let newer = status(PrState::Open, ChecksState::Failing);
        cache.record(&key, t(60), t(61), &Ok(newer.clone()));
        let entry = cache.get(&key).unwrap();
        assert_eq!((entry.status, entry.fetched_at, entry.error), (Some(newer.clone()), Some(t(61)), None));

        // An older attempt answering late does not put its reading back.
        cache.record(&key, t(45), t(90), &Ok(good_again()));
        assert_eq!(cache.get(&key).unwrap().status, Some(newer));

        fn good_again() -> PrStatus {
            status(PrState::Open, ChecksState::Passing)
        }
    }

    #[test]
    fn a_first_failure_has_an_entry_with_no_status() {
        let cache = PrStatuses::with_program("unused", FETCH_TIMEOUT);
        let key = PrKey::new("o/r", 2);
        let now = Utc::now();
        cache.record(&key, now, now, &Err(PrFetchError::new(PrFetchErrorKind::Auth, "sign in")));
        let entry = cache.get(&key).unwrap();
        assert_eq!(entry.status, None);
        assert_eq!(entry.fetched_at, None);
        assert_eq!(entry.error.map(|e| e.kind), Some(PrFetchErrorKind::Auth));
        assert!(cache.get(&PrKey::new("o/r", 3)).is_none());
        assert_eq!(cache.entries_for([&key, &key, &PrKey::new("o/r", 3)]).len(), 1);
    }

    #[cfg(unix)]
    mod process {
        use super::super::*;
        use std::os::unix::fs::PermissionsExt;

        /// A stand-in `gh` written into `dir`.
        pub fn script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
            let path = dir.join("gh");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn alive(pid: i32) -> bool {
            // Signal 0 checks existence. A zombie still exists, so also read
            // its state: an unreaped zombie of ours is not "left running".
            if unsafe { libc::kill(pid, 0) } != 0 {
                return false;
            }
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(stat) => !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'),
                Err(_) => false,
            }
        }

        #[tokio::test]
        async fn a_successful_answer_is_parsed_and_cached() {
            let dir = tempfile::tempdir().unwrap();
            let gh = script(
                dir.path(),
                r#"echo "$@" > "$(dirname "$0")/args"; printf '{"state":"OPEN","statusCheckRollup":[],"reviewDecision":"APPROVED","mergeable":"MERGEABLE"}'"#,
            );
            let statuses = PrStatuses::with_program(gh, Duration::from_secs(10));
            let key = PrKey::new("owner/repo", 42);
            let status = statuses.refresh(&key).await.unwrap();
            assert_eq!(status.checks, ChecksState::NoChecks);
            assert_eq!(status.review_decision, Some(ReviewDecision::Approved));
            assert_eq!(statuses.get(&key).unwrap().status, Some(status));
            let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
            assert_eq!(args.trim(), format!("pr view 42 --repo owner/repo --json {FIELDS}"));
        }

        #[tokio::test]
        async fn an_auth_failure_is_recorded_as_one() {
            let dir = tempfile::tempdir().unwrap();
            let gh = script(dir.path(), "echo 'You are not logged into any GitHub hosts. Run gh auth login' >&2; exit 4");
            let statuses = PrStatuses::with_program(gh, Duration::from_secs(10));
            let key = PrKey::new("o/r", 1);
            let err = statuses.refresh(&key).await.unwrap_err();
            assert_eq!(err.kind, PrFetchErrorKind::Auth);
            assert_eq!(statuses.get(&key).unwrap().error, Some(err));
        }

        #[tokio::test]
        async fn a_missing_gh_is_reported_as_not_installed() {
            let statuses = PrStatuses::with_program("/nonexistent/slashit-no-gh", Duration::from_secs(1));
            let err = statuses.refresh(&PrKey::new("o/r", 1)).await.unwrap_err();
            assert_eq!(err.kind, PrFetchErrorKind::NotInstalled);
        }

        #[tokio::test]
        async fn a_hung_gh_is_stopped_with_everything_it_started() {
            let dir = tempfile::tempdir().unwrap();
            // The script starts a grandchild that would outlive it, records
            // both pids, and then hangs.
            let gh = script(
                dir.path(),
                r#"d="$(dirname "$0")"; sleep 300 & echo $! > "$d/grandchild"; echo $$ > "$d/leader"; wait"#,
            );
            let statuses = PrStatuses::with_program(gh, Duration::from_millis(700));
            let key = PrKey::new("o/r", 9);
            let good = super::status(PrState::Open, ChecksState::Passing);
            let t0 = Utc::now() - chrono::Duration::seconds(60);
            statuses.record(&key, t0, t0, &Ok(good.clone()));

            let began = std::time::Instant::now();
            let err = statuses.refresh(&key).await.unwrap_err();
            assert_eq!(err.kind, PrFetchErrorKind::Timeout);
            assert!(began.elapsed() < Duration::from_secs(5), "took {:?}", began.elapsed());

            let entry = statuses.get(&key).unwrap();
            assert_eq!(entry.status, Some(good), "a timeout keeps the last good status");
            assert_eq!(entry.error.map(|e| e.kind), Some(PrFetchErrorKind::Timeout));

            let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap().trim().parse::<i32>().unwrap();
            let (leader, grandchild) = (read("leader"), read("grandchild"));
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while (alive(leader) || alive(grandchild)) && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(!alive(leader), "the timed-out gh is still running");
            assert!(!alive(grandchild), "a process the timed-out gh started is still running");
        }
    }
}
