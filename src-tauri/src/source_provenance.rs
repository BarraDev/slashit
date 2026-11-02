// SPDX-License-Identifier: MPL-2.0
//! Where a build's source identity came from, and how far it can be trusted.
//!
//! A build records the commit it was made from and whether its tree had
//! uncommitted modifications. A commit alone is not an exact snapshot: a build
//! from a modified tree carries the commit it started from, not the code it
//! compiled. A clean tree is not enough for an exact claim either: the frontend
//! assets packaged into the build come from `dist/`, which Git ignores, so no
//! record of the source can vouch for them. This module decides which of those
//! facts a build can honestly claim.
//!
//! This module uses only `std`, because `build.rs` compiles it as well. The
//! application reads the recorded value and never runs Git.

/// What is known about the source a build was compiled from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// The commit is known, and the build tree had uncommitted modifications.
    Modified { commit: String },
    /// The commit is known, but the build's complete contents, including the
    /// frontend assets, cannot be tied to it. This is the most a build can
    /// claim, whether or not its own tree was clean.
    Unverified { commit: String },
    /// Neither the commit nor the state of the tree could be established.
    Unknown,
}

/// The state of a build tree, as far as it could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tree {
    Clean,
    Modified,
    /// Git did not answer, so the state is not known.
    Unavailable,
}

/// Reads the output of `git status --porcelain`. Empty output is a clean tree.
/// Any entry, tracked or untracked, is a modification. Files that Git ignores,
/// such as `target/`, never appear in that output, so ordinary build output
/// does not make a tree look modified.
pub fn tree_from_status(porcelain: Option<&str>) -> Tree {
    match porcelain {
        None => Tree::Unavailable,
        Some(text) if text.trim().is_empty() => Tree::Clean,
        Some(_) => Tree::Modified,
    }
}

/// Combines the commit with the state of the tree. Without a commit nothing
/// is known, whatever the tree looks like. A modified tree is reported as
/// modified; any other tree is unverified, because the frontend assets are not
/// part of what Git can check.
pub fn classify(commit: Option<&str>, tree: Tree) -> Provenance {
    match (commit, tree) {
        (None, _) => Provenance::Unknown,
        (Some(commit), Tree::Modified) => Provenance::Modified {
            commit: commit.to_string(),
        },
        (Some(commit), Tree::Clean | Tree::Unavailable) => Provenance::Unverified {
            commit: commit.to_string(),
        },
    }
}

/// The value `build.rs` records, or `None` when nothing is known, in which
/// case nothing is recorded and the application reports `Unknown`.
pub fn encode(provenance: &Provenance) -> Option<String> {
    match provenance {
        Provenance::Modified { commit } => Some(format!("modified:{commit}")),
        Provenance::Unverified { commit } => Some(format!("unverified:{commit}")),
        Provenance::Unknown => None,
    }
}

/// Reads a recorded value. Anything `encode` did not produce is rejected.
pub fn parse(text: &str) -> Option<Provenance> {
    let (state, commit) = text.split_once(':')?;
    if !is_commit_id(commit) {
        return None;
    }
    let commit = commit.to_string();
    match state {
        "modified" => Some(Provenance::Modified { commit }),
        "unverified" => Some(Provenance::Unverified { commit }),
        _ => None,
    }
}

/// A commit id: hexadecimal, and long enough to name a commit unambiguously.
pub fn is_commit_id(text: &str) -> bool {
    (7..=64).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "14f170b13b0f864a6322576f4512dadfb18482a2";

    #[test]
    fn a_clean_tree_at_a_known_commit_is_still_only_unverified() {
        // The frontend assets are not part of what Git can check, so a clean
        // tree never supports an exact claim.
        assert_eq!(
            classify(Some(COMMIT), tree_from_status(Some(""))),
            Provenance::Unverified { commit: COMMIT.to_string() }
        );
    }

    #[test]
    fn a_modified_tree_is_modified_even_though_its_commit_is_known() {
        let status = Some(" M src-tauri/src/lib.rs\n");
        assert_eq!(
            classify(Some(COMMIT), tree_from_status(status)),
            Provenance::Modified { commit: COMMIT.to_string() }
        );
    }

    #[test]
    fn an_untracked_file_counts_as_a_modification() {
        let status = Some("?? notes.md\n");
        assert_eq!(tree_from_status(status), Tree::Modified);
    }

    #[test]
    fn a_tree_that_git_could_not_read_is_unverified_not_clean() {
        assert_eq!(tree_from_status(None), Tree::Unavailable);
        assert_eq!(
            classify(Some(COMMIT), tree_from_status(None)),
            Provenance::Unverified { commit: COMMIT.to_string() }
        );
    }

    #[test]
    fn no_commit_is_unknown_whatever_the_tree_says() {
        for tree in [Tree::Clean, Tree::Modified, Tree::Unavailable] {
            assert_eq!(classify(None, tree), Provenance::Unknown, "{tree:?}");
        }
    }

    #[test]
    fn whitespace_only_status_output_is_a_clean_tree() {
        assert_eq!(tree_from_status(Some("\n")), Tree::Clean);
    }

    #[test]
    fn each_state_survives_the_recorded_form() {
        for provenance in [
            classify(Some(COMMIT), Tree::Clean),
            classify(Some(COMMIT), Tree::Modified),
            classify(Some(COMMIT), Tree::Unavailable),
        ] {
            assert_ne!(provenance, Provenance::Unknown);
            let recorded = encode(&provenance).expect("a known commit is recorded");
            assert_eq!(parse(&recorded), Some(provenance), "{recorded}");
        }
    }

    #[test]
    fn unknown_is_never_recorded() {
        assert_eq!(encode(&Provenance::Unknown), None);
    }

    #[test]
    fn a_value_not_produced_by_encode_is_rejected() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("modified"), None);
        assert_eq!(parse(&format!("dirty:{COMMIT}")), None);
        assert_eq!(parse(&format!("clean:{COMMIT}")), None, "clean is never a recorded state");
        assert_eq!(parse("modified:"), None);
        assert_eq!(parse("modified:not-a-commit"), None);
        assert_eq!(parse("unverified:abc"), None, "too short to name a commit");
    }
}
