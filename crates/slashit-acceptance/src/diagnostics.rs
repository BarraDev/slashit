//! What to keep when an acceptance journey fails.
//!
//! A failed desktop test is expensive to reproduce, so the run captures the
//! evidence while the application is still alive: what the window was showing,
//! what the page actually contained, and what the provider logged. Nothing
//! here is allowed to fail the test — a diagnostic that panics destroys the
//! error it was supposed to explain.

use anyhow::Result;
use std::path::{Path, PathBuf};
use thirtyfour::prelude::*;

/// Page source is captured whole up to this size; beyond it the head is kept,
/// which is where a mount failure or an error page shows itself.
const MAX_SOURCE_BYTES: usize = 256 * 1024;

/// How many failed runs' artifact directories to keep before pruning the
/// oldest. Enough to compare a flake against its neighbours, few enough that
/// `target/` does not grow without bound.
const RETAINED_FAILURES: usize = 5;

/// A low-cardinality outcome label. The detailed assertion remains in the
/// test log; this label is safe to aggregate without copying page contents or
/// task data into a second artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    AssertionOrProduct,
    ApplicationCrash,
    DriverOrSession,
    Infrastructure,
    HarnessPanic,
}

/// Classify only on concrete diagnostic markers. Unknown errors default to
/// the test's own assertion/product path; the original error remains the
/// authoritative explanation in libtest output.
pub fn classify_failure(message: &str) -> FailureClass {
    let message = message.to_ascii_lowercase();
    if [
        "segmentation fault",
        "segfault",
        "signal 11",
        "renderer process crashed",
        "application crashed",
    ]
    .iter()
    .any(|marker| message.contains(marker))
    {
        FailureClass::ApplicationCrash
    } else if [
        "webdriver",
        "tauri-driver",
        "invalid session",
        "no such window",
        "connection refused",
    ]
    .iter()
    .any(|marker| message.contains(marker))
    {
        FailureClass::DriverOrSession
    } else if [
        "xvfb",
        "dbus",
        "not installed",
        "no application binary",
        "could not create",
        "permission denied",
        "port is already",
    ]
    .iter()
    .any(|marker| message.contains(marker))
    {
        FailureClass::Infrastructure
    } else {
        FailureClass::AssertionOrProduct
    }
}

/// Stable, content-free timing record written both beside failed evidence
/// and to the run-level JSONL index. The `session_logs` values are basenames,
/// never absolute paths.
#[derive(Debug)]
pub struct JourneyResult<'a> {
    pub schema_version: u8,
    pub journey: &'a str,
    pub outcome: &'a str,
    pub duration_ms: u64,
    pub failure_class: Option<FailureClass>,
    pub session_logs: &'a [String],
}

impl FailureClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::AssertionOrProduct => "assertion_or_product",
            Self::ApplicationCrash => "application_crash",
            Self::DriverOrSession => "driver_or_session",
            Self::Infrastructure => "infrastructure",
            Self::HarnessPanic => "harness_panic",
        }
    }
}

impl JourneyResult<'_> {
    pub fn json_value(&self) -> serde_json::Value {
        let mut fields = serde_json::Map::new();
        fields.insert("schema_version".into(), self.schema_version.into());
        fields.insert("journey".into(), self.journey.into());
        fields.insert("outcome".into(), self.outcome.into());
        fields.insert("duration_ms".into(), self.duration_ms.into());
        fields.insert(
            "failure_class".into(),
            self.failure_class
                .map(|class| serde_json::Value::String(class.as_str().into()))
                .unwrap_or(serde_json::Value::Null),
        );
        fields.insert(
            "session_logs".into(),
            serde_json::Value::Array(
                self.session_logs
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        serde_json::Value::Object(fields)
    }
}

/// Save a per-journey JSON result and append it to a JSONL run index.
/// Diagnostics must not replace a product failure, so I/O errors are returned
/// for tests but callers in the harness report and suppress them.
pub fn write_journey_result(
    artifact_dir: &Path,
    index_path: &Path,
    result: &JourneyResult<'_>,
) -> Result<()> {
    if result.outcome != "passed" {
        std::fs::create_dir_all(artifact_dir)?;
        let json = serde_json::to_vec_pretty(&result.json_value())?;
        std::fs::write(artifact_dir.join("journey-result.json"), &json)?;
    }
    let line = serde_json::to_vec(&result.json_value())?;
    use std::io::Write;
    let mut index = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(index_path)?;
    index.write_all(&line)?;
    index.write_all(b"\n")?;
    Ok(())
}

/// Capture everything the live session can still tell us, into `dir`.
///
/// Returns a human-readable summary for the panic message; individual capture
/// failures are reported inside it rather than propagated.
pub async fn capture(driver: &WebDriver, dir: &Path, label: &str) -> String {
    let mut notes = Vec::new();

    match driver.current_url().await {
        Ok(url) => notes.push(format!("url: {url}")),
        Err(e) => notes.push(format!("url: unavailable ({e})")),
    }
    match driver.title().await {
        Ok(title) => notes.push(format!("title: {title:?}")),
        Err(e) => notes.push(format!("title: unavailable ({e})")),
    }

    let screenshot = dir.join(format!("{label}-screenshot.png"));
    match driver.screenshot(&screenshot).await {
        Ok(()) => notes.push(format!("screenshot: {}", screenshot.display())),
        Err(e) => notes.push(format!("screenshot: unavailable ({e})")),
    }

    let source_path = dir.join(format!("{label}-source.html"));
    match driver.source().await {
        Ok(source) => {
            let truncated = head_within(&source, MAX_SOURCE_BYTES);
            match std::fs::write(&source_path, truncated) {
                Ok(()) => notes.push(format!(
                    "source: {} ({} of {} bytes)",
                    source_path.display(),
                    truncated.len(),
                    source.len()
                )),
                Err(e) => notes.push(format!("source: could not be written ({e})")),
            }
        }
        Err(e) => notes.push(format!("source: unavailable ({e})")),
    }

    notes.join("\n  ")
}

/// The longest prefix of `text` that fits in `limit` bytes and is still valid
/// UTF-8.
///
/// Slicing a `String` at an arbitrary byte index panics when that index falls
/// inside a multi-byte code point, and a page source large enough to be
/// truncated is exactly the kind that contains one. A panic here would be
/// especially destructive: capture runs *because* a journey already failed,
/// so it would replace the real error with an unrelated one and throw the
/// evidence away. Walking back to the nearest boundary costs at most three
/// bytes.
fn head_within(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Drop the oldest artifact directories under `root`, keeping the most recent
/// [`RETAINED_FAILURES`].
///
/// Retention is by modification time rather than by name so a directory that
/// was written to during a long run is treated as recent.
pub fn prune(root: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(());
    };

    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();

    if dirs.len() <= RETAINED_FAILURES {
        return Ok(());
    }

    dirs.sort_by_key(|(modified, _)| *modified);
    let doomed = dirs.len() - RETAINED_FAILURES;
    for (_, path) in dirs.into_iter().take(doomed) {
        // Housekeeping for somebody else's leftovers, so a failure here must
        // not fail the run that is merely trying to start: a stale directory
        // nobody can delete would otherwise turn every later journey red for
        // a reason unrelated to the product. It does have to be audible,
        // because silently keeping everything lets the retention bound drift
        // without anybody noticing.
        //
        // A directory that is already gone is the expected outcome of two
        // runs starting at once and choosing the same oldest victim, so that
        // one is silent: warning about it would train everybody to ignore the
        // warning that matters.
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "[acceptance] could not prune the old artifact directory {}: {error}",
                path.display()
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_classes_have_stable_markers() {
        assert_eq!(
            classify_failure("assertion failed: title mismatch"),
            FailureClass::AssertionOrProduct
        );
        assert_eq!(
            classify_failure("renderer process crashed with signal 11"),
            FailureClass::ApplicationCrash
        );
        assert_eq!(
            classify_failure("invalid WebDriver session"),
            FailureClass::DriverOrSession
        );
        assert_eq!(
            classify_failure("Xvfb is not installed"),
            FailureClass::Infrastructure
        );
    }

    #[test]
    fn journey_result_is_written_as_json_and_jsonl_without_absolute_paths() {
        let root = std::env::temp_dir().join(format!("slashit-diagnostics-{}", std::process::id()));
        let artifact = root.join("failed-journey");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&artifact).expect("artifact directory");
        let logs = ["0-project-provider.log".to_string()];
        let result = JourneyResult {
            schema_version: 1,
            journey: "sample_journey",
            outcome: "failed",
            duration_ms: 1234,
            failure_class: Some(FailureClass::DriverOrSession),
            session_logs: &logs,
        };
        let index = root.join("journey-timings.jsonl");
        write_journey_result(&artifact, &index, &result).expect("write result");
        let parsed: serde_json::Value = serde_json::from_slice(
            &std::fs::read(artifact.join("journey-result.json")).expect("result file"),
        )
        .expect("valid result JSON");
        assert_eq!(parsed["duration_ms"], 1234);
        assert_eq!(parsed["failure_class"], "driver_or_session");
        assert_eq!(parsed["session_logs"][0], "0-project-provider.log");
        let line = std::fs::read_to_string(index).expect("JSONL index");
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains("sample_journey"));

        let passed_artifact = root.join("passed-journey");
        let passed = JourneyResult {
            schema_version: 1,
            journey: "passed_journey",
            outcome: "passed",
            duration_ms: 9,
            failure_class: None,
            session_logs: &[],
        };
        let index = root.join("journey-timings.jsonl");
        write_journey_result(&passed_artifact, &index, &passed).expect("write passed result");
        assert!(
            !passed_artifact.exists(),
            "green runs leave no artifact directory"
        );
        assert_eq!(
            std::fs::read_to_string(index)
                .expect("updated JSONL index")
                .lines()
                .count(),
            2
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    /// A four-byte character straddling the cap is the case that used to
    /// panic. Every offset across one is checked, so the boundary walk cannot
    /// be off by one in either direction.
    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // "😀" is four bytes; "é" is two; "中" is three. Mixing widths means a
        // cap can land inside any of them.
        let text = "aé中😀".repeat(64);
        assert!(
            text.len() > text.chars().count(),
            "the fixture must be wide"
        );

        for limit in 0..text.len() + 8 {
            // The point of the test: this must not panic for any limit.
            let head = head_within(&text, limit);
            assert!(head.len() <= limit, "limit {limit} was exceeded");
            assert!(
                text.starts_with(head),
                "limit {limit} produced something that is not a prefix"
            );
            // Nothing was lost beyond the partial character at the cut.
            assert!(
                limit >= text.len() || head.len() + 4 > limit,
                "limit {limit} discarded more than one character's worth"
            );
        }
    }

    #[test]
    fn text_shorter_than_the_cap_is_returned_whole() {
        let text = "😀 unchanged";
        assert_eq!(head_within(text, MAX_SOURCE_BYTES), text);
    }

    #[test]
    fn prune_keeps_the_newest_and_tolerates_a_missing_root() {
        let root = std::env::temp_dir().join(format!("slashit-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        prune(&root).expect("a missing root is not an error");

        std::fs::create_dir_all(&root).expect("create root");
        for index in 0..RETAINED_FAILURES + 3 {
            let dir = root.join(format!("run-{index}"));
            std::fs::create_dir_all(&dir).expect("create run dir");
            // Distinct modification times, so ordering is not left to chance.
            std::thread::sleep(std::time::Duration::from_millis(15));
        }

        prune(&root).expect("prune");
        let remaining = std::fs::read_dir(&root).expect("read root").count();
        assert_eq!(remaining, RETAINED_FAILURES);
        // The newest must be among the survivors.
        assert!(root.join(format!("run-{}", RETAINED_FAILURES + 2)).exists());

        std::fs::remove_dir_all(&root).expect("clean up");
    }
}
