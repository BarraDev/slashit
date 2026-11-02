// SPDX-License-Identifier: MPL-2.0
// The channel parser is shared with the application, so the build and the app
// read the declaration with one implementation. See src/release_channel.rs.
#[path = "src/release_channel.rs"]
mod release_channel;
// The provenance rules are shared for the same reason. See src/source_provenance.rs.
#[path = "src/source_provenance.rs"]
mod source_provenance;

use release_channel::{channel_for, parse_declarations, PlainVersion};
use source_provenance::{classify, encode, is_commit_id, parse, tree_from_status, Provenance, Tree};
use std::path::PathBuf;

fn main() {
    tauri_build::build();

    // The build's own channel. A version the declaration does not name fails
    // the build, so no binary ever reports a channel it was not declared as.
    let version_text = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let path = PathBuf::from(manifest_dir).join("../.github/release-channels.txt");
    println!("cargo:rerun-if-changed={}", path.display());

    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let declarations =
        parse_declarations(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let version = PlainVersion::parse(&version_text)
        .unwrap_or_else(|| panic!("the package version {version_text} is not plain MAJOR.MINOR.PATCH"));
    let channel = channel_for(&declarations, version).unwrap_or_else(|| {
        panic!("{version} has no channel in {}; declare it there", path.display())
    });
    println!("cargo:rustc-env=SLASHIT_RELEASE_CHANNEL={}", channel.as_str());

    // Where this binary's source came from, for the About dialog. Taken at
    // build time only; the application never runs Git. Set
    // `SLASHIT_SOURCE_COMMIT` to supply the commit where there is no Git
    // checkout (a JJ workspace, a source tarball). A supplied commit is the
    // builder's assertion, so the tree state is reported as unverified, and a
    // value that is not a commit id fails the build, like an unreadable
    // channel declaration. The frontend assets in `../dist` are not part of
    // this check: Git ignores that directory, so a record cannot vouch for them.
    println!("cargo:rerun-if-env-changed=SLASHIT_SOURCE_COMMIT");
    watch_source_tree();
    let provenance = build_provenance();
    if let Some(recorded) = encode(&provenance) {
        // The application reads this value with the same parser, so a value it
        // could not read back is a build error, not a silent Unknown.
        assert_eq!(parse(&recorded), Some(provenance.clone()), "{recorded} does not read back");
        println!("cargo:rustc-env=SLASHIT_SOURCE_PROVENANCE={recorded}");
    }
}

/// What this build can say about its own source.
fn build_provenance() -> Provenance {
    let supplied = std::env::var("SLASHIT_SOURCE_COMMIT")
        .ok()
        .map(|commit| commit.trim().to_string())
        .filter(|commit| !commit.is_empty());
    match supplied {
        Some(supplied) => {
            if !is_commit_id(&supplied) {
                panic!("SLASHIT_SOURCE_COMMIT={supplied} is not a commit id");
            }
            classify(Some(&supplied), Tree::Unavailable)
        }
        None => {
            let commit = git_head_commit();
            // Untracked files are listed whatever the person's `status` config
            // says, and optional index refreshes are skipped, so the check
            // neither hides a modification nor rewrites the index it watches.
            let status = git_stdout(&[
                "--no-optional-locks",
                "status",
                "--porcelain",
                "--untracked-files=normal",
            ]);
            classify(commit.as_deref(), tree_from_status(status.as_deref()))
        }
    }
}

/// Asks Cargo to rerun this script when anything that changes the recorded
/// state changes: the commit (`HEAD` and the ref it names), the index, and
/// every tracked file. Without the tracked files, an edit after a build would
/// leave a record of an unmodified tree that no longer matches the source.
/// A new file that is neither tracked nor staged does not trigger a rerun.
fn watch_source_tree() {
    for path in ["HEAD", "packed-refs", "index"] {
        if let Some(resolved) = git_output(&["rev-parse", "--git-path", path]) {
            watch_if_present(&resolved);
        }
    }
    if let Some(branch) = git_output(&["rev-parse", "--symbolic-full-name", "HEAD"]) {
        if branch.starts_with("refs/") {
            if let Some(resolved) = git_output(&["rev-parse", "--git-path", &branch]) {
                watch_if_present(&resolved);
            }
        }
    }
    // Paths are relative to this package, which is where Cargo resolves them.
    // A path that is not valid UTF-8 is lossily converted, so it names no file
    // and Cargo reruns this script on every build: the failure is safe.
    if let Some(listing) = git_stdout(&["ls-files", "-z", "--", ":/"]) {
        for path in listing.split('\0').filter(|path| !path.is_empty()) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// Watches a Git control path only when it exists. Cargo treats a missing
/// watched path as always changed, which would rerun this script every time.
fn watch_if_present(resolved: &str) {
    if std::path::Path::new(resolved).exists() {
        println!("cargo:rerun-if-changed={resolved}");
    }
}

/// Stdout of a Git command run in this package, or `None` outside a checkout
/// or when the command fails. Non-UTF-8 bytes are replaced rather than dropped,
/// so output that is present is never mistaken for no output.
fn git_stdout(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(std::env::var("CARGO_MANIFEST_DIR").ok()?)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Trimmed stdout of a Git command, or `None` when it is empty or fails.
fn git_output(args: &[&str]) -> Option<String> {
    let text = git_stdout(args)?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `HEAD` of the Git checkout this package sits in, or `None` outside one.
fn git_head_commit() -> Option<String> {
    git_output(&["rev-parse", "--verify", "--quiet", "HEAD"]).filter(|commit| is_commit_id(commit))
}
