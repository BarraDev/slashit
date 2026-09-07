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

    /// A four-byte character straddling the cap is the case that used to
    /// panic. Every offset across one is checked, so the boundary walk cannot
    /// be off by one in either direction.
    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // "😀" is four bytes; "é" is two; "中" is three. Mixing widths means a
        // cap can land inside any of them.
        let text = "aé中😀".repeat(64);
        assert!(text.len() > text.chars().count(), "the fixture must be wide");

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
        assert!(root
            .join(format!("run-{}", RETAINED_FAILURES + 2))
            .exists());

        std::fs::remove_dir_all(&root).expect("clean up");
    }
}
