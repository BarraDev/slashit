// SPDX-License-Identifier: MPL-2.0
// The channel parser is shared with the application, so the build and the app
// read the declaration with one implementation. See src/release_channel.rs.
#[path = "src/release_channel.rs"]
mod release_channel;

use release_channel::{channel_for, parse_declarations, PlainVersion};
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

    // The commit this binary was built from, for the About dialog. Taken at
    // build time only; the application never runs Git. Set
    // `SLASHIT_SOURCE_COMMIT` to supply it where there is no Git checkout (a
    // JJ workspace, a source tarball). Left unset when neither is available, and
    // the dialog then reports the commit as unknown.
    println!("cargo:rerun-if-env-changed=SLASHIT_SOURCE_COMMIT");
    let supplied = std::env::var("SLASHIT_SOURCE_COMMIT").ok().map(|c| c.trim().to_string());
    let commit = match supplied.filter(|c| !c.is_empty()) {
        Some(supplied) => Some(well_formed_commit(&supplied).unwrap_or_else(|| {
            panic!("SLASHIT_SOURCE_COMMIT={supplied} is not a hexadecimal commit id")
        })),
        None => git_head_commit(),
    };
    if let Some(commit) = commit {
        println!("cargo:rustc-env=SLASHIT_SOURCE_COMMIT={commit}");
    }
}

/// Watch the files that change when a commit lands on the current branch: the
/// `HEAD` file (which names the branch when attached) and the ref it names,
/// which is loose or in `packed-refs`. Paths come from `git rev-parse
/// --git-path`, so this also works in a linked worktree, where `.git` is a file.
fn watch_git_head() {
    for path in ["HEAD", "packed-refs"] {
        if let Some(resolved) = git_path(path) {
            println!("cargo:rerun-if-changed={resolved}");
        }
    }
    if let Some(branch) = git_output(&["rev-parse", "--symbolic-full-name", "HEAD"]) {
        if branch.starts_with("refs/") {
            if let Some(resolved) = git_path(&branch) {
                println!("cargo:rerun-if-changed={resolved}");
            }
        }
    }
}

fn git_path(path: &str) -> Option<String> {
    git_output(&["rev-parse", "--git-path", path])
}

/// Trimmed stdout of a Git command run in this package, or `None` outside a
/// checkout or when the command fails.
fn git_output(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(std::env::var("CARGO_MANIFEST_DIR").ok()?)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `HEAD` of the Git checkout this package sits in, or `None` outside one.
fn git_head_commit() -> Option<String> {
    watch_git_head();
    well_formed_commit(&git_output(&["rev-parse", "--verify", "--quiet", "HEAD"])?)
}

/// The commit id, when it is hexadecimal; anything else is not a commit.
fn well_formed_commit(text: &str) -> Option<String> {
    let well_formed = !text.is_empty() && text.bytes().all(|b| b.is_ascii_hexdigit());
    well_formed.then(|| text.to_string())
}
