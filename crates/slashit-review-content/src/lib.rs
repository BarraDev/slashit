//! Which version of a review comment a fix was made from.
//!
//! GitHub keeps a comment's id when its text is edited, so the id says
//! nothing about whether the wording a fix addressed is still the wording on
//! the pull request. The plan therefore records a [`fingerprint`] of the
//! comment text the apply used, and a later analysis or backfill compares it
//! with the text now (see [`fixed_content_is_current`]).
//!
//! Timestamps never decide that for a comment with a recorded fingerprint.
//! For a fix recorded before fingerprints existed they count only when the
//! plan's copy of the comment is known to be what the apply was given (the
//! plan was generated at or before the apply); otherwise the fix is unproven.

use chrono::{DateTime, SubsecRound, Utc};
use sha2::{Digest, Sha256};

/// Recorded in place of a fingerprint when the text a fix was made from cannot
/// be established. No fingerprint equals it, so the fix never counts as current.
pub const UNPROVEN: &str = "unproven";

/// Names the algorithm and the normalization, so either can change later
/// without a stored fingerprint comparing equal to a different one.
const PREFIX: &str = "sha256-v1:";

/// A stable fingerprint of a review comment's text.
///
/// The text is normalized only where GitHub itself is inconsistent: line
/// endings (`\r\n` and `\r` become `\n`, as the REST and GraphQL APIs differ
/// on them) and leading and trailing whitespace (the fetch trims it). Nothing
/// inside the text is touched: whitespace in a code block is part of what the
/// reviewer wrote. The comment's path, line and timestamps are not part of it,
/// so a comment GitHub marks outdated after a push, whose position changes
/// but whose text does not, keeps its fingerprint.
pub fn fingerprint(body: &str) -> String {
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let digest = Sha256::digest(normalized.trim().as_bytes());
    let mut out = String::with_capacity(PREFIX.len() + digest.len() * 2);
    out.push_str(PREFIX);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Whether a comment last updated at `updated_at` may have been edited at or
/// after `applied_at`. The rule for a fix recorded without a fingerprint.
///
/// GitHub's `updated_at` has whole-second precision, while `applied_at`
/// almost never does. Both are rounded down to the second and compared
/// non-strictly, so a timestamp in the apply's own second counts as a
/// possible edit. A comment with no `updated_at` is taken as unchanged.
///
/// This cannot see an edit made between the analysis an apply used and the
/// moment the apply finished recording itself: that edit is older than
/// `applied_at`. It is why fingerprints exist.
pub fn edited_since(updated_at: Option<DateTime<Utc>>, applied_at: DateTime<Utc>) -> bool {
    updated_at.is_some_and(|updated| updated.trunc_subsecs(0) >= applied_at.trunc_subsecs(0))
}

/// Whether a fix made from `recorded` still covers the comment whose text is
/// `current_body`.
///
/// With a `recorded` fingerprint the answer is exactly whether the text is
/// the same, whenever it was edited and whatever its `updated_at` says.
///
/// Without one (a fix recorded before fingerprints existed) a timestamp may
/// stand in for proof only when `copy_is_apply_input` is true: the plan was
/// generated at or before the apply, so the comment text it holds is what the
/// agent was given, and the timestamp rule ([`edited_since`]) can then tell
/// whether the comment moved since. A plan generated after its apply may hold
/// text fetched later, and no timestamp ordering can show that text was
/// handled, so the answer is `false`.
pub fn fixed_content_is_current(
    recorded: Option<&str>,
    current_body: &str,
    updated_at: Option<DateTime<Utc>>,
    applied_at: DateTime<Utc>,
    copy_is_apply_input: bool,
) -> bool {
    match recorded {
        Some(recorded) => recorded == fingerprint(current_body),
        None => copy_is_apply_input && !edited_since(updated_at, applied_at),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fingerprint_is_a_known_value() {
        // Pinned: a change here orphans every fingerprint already stored.
        assert_eq!(
            fingerprint("use a slice here"),
            "sha256-v1:46c3aa51ac9908714dbf0f2e625489e6b2063e98eeab25bccc4c65b4749262cd",
        );
    }

    #[test]
    fn line_endings_and_outer_whitespace_do_not_change_it() {
        let lf = fingerprint("a\nb");
        assert_eq!(fingerprint("a\r\nb"), lf);
        assert_eq!(fingerprint("a\rb"), lf);
        assert_eq!(fingerprint("\n  a\nb \r\n"), lf);
    }

    #[test]
    fn inner_whitespace_and_case_do_matter() {
        assert_ne!(fingerprint("a  b"), fingerprint("a b"));
        assert_ne!(fingerprint("a\n\nb"), fingerprint("a\nb"));
        assert_ne!(fingerprint("A"), fingerprint("a"));
    }

    #[test]
    fn a_recorded_fingerprint_ignores_timestamps_both_ways() {
        let applied = "2024-06-01T00:10:00Z".parse().unwrap();
        let before = Some("2024-06-01T00:00:00Z".parse().unwrap());
        let after = Some("2024-06-01T00:20:00Z".parse().unwrap());
        let recorded = fingerprint("old");
        // Edited long before the apply finished: the text is what decides.
        assert!(!fixed_content_is_current(Some(&recorded), "new", before, applied, false));
        // Timestamp newer than the apply, text unchanged: still current.
        assert!(fixed_content_is_current(Some(&recorded), "old", after, applied, false));
    }

    #[test]
    fn without_a_fingerprint_the_timestamp_rule_applies() {
        let applied = "2024-06-01T00:10:00.500Z".parse().unwrap();
        let same_second = Some("2024-06-01T00:10:00Z".parse().unwrap());
        let earlier = Some("2024-06-01T00:09:59Z".parse().unwrap());
        assert!(!fixed_content_is_current(None, "x", same_second, applied, true));
        assert!(fixed_content_is_current(None, "x", earlier, applied, true));
        assert!(fixed_content_is_current(None, "x", None, applied, true));
    }

    #[test]
    fn without_a_fingerprint_a_copy_that_is_not_the_apply_input_is_never_current() {
        let applied = "2024-06-01T00:10:00Z".parse().unwrap();
        let earlier = Some("2024-06-01T00:05:00Z".parse().unwrap());
        assert!(!fixed_content_is_current(None, "x", earlier, applied, false));
        assert!(!fixed_content_is_current(None, "x", None, applied, false));
    }
}
