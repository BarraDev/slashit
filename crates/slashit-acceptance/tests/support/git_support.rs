//! Deterministic Git command helpers for acceptance fixtures.

use anyhow::{bail, Context, Result};
use std::path::Path;

/// `git <args>` for the repository at `dir`, reading no global configuration
/// and none of the variables that would point Git somewhere else: run from a
/// Git hook, the environment names the hook's repository, and `GIT_DIR`
/// overrides repository discovery from `dir`.
pub(crate) fn git_command(dir: &Path, args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        command.env_remove(variable);
    }
    command
}

/// `git <args>` in `dir`, trimmed stdout. Reads no global configuration.
pub(crate) fn git_out(dir: &Path, args: &[&str]) -> Result<String> {
    let output = git_command(dir, args)
        .output()
        .with_context(|| format!("could not run git {args:?} in {}", dir.display()))?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Whether `ancestor` is an ancestor of `descendant`.
///
/// `git merge-base --is-ancestor` exits 0 for yes and 1 for no; any other
/// outcome (an unknown revision, a broken repository) is Git failing, not an
/// answer.
pub(crate) fn git_is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    git_predicate(
        dir,
        &["merge-base", "--is-ancestor", ancestor, descendant],
        1,
    )
}

/// Whether the commit `revision` names exists in the repository.
///
/// `git rev-parse --verify --quiet` exits 0 when it resolves and 1 when it
/// does not. `cat-file -e` would not do: it exits 128 for a missing
/// `<sha>^{commit}`, the same status as a real failure.
pub(crate) fn git_commit_exists(dir: &Path, revision: &str) -> Result<bool> {
    git_predicate(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{revision}^{{commit}}"),
        ],
        1,
    )
}

/// Runs a Git command that answers a yes/no question by exit status: 0 is
/// yes, `negative_status` is no, and every other outcome is an error carrying
/// Git's stderr, so an assertion cannot pass because Git itself failed.
fn git_predicate(dir: &Path, args: &[&str], negative_status: i32) -> Result<bool> {
    let output = git_command(dir, args)
        .output()
        .with_context(|| format!("could not run git {args:?} in {}", dir.display()))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(code) if code == negative_status => Ok(false),
        status => bail!(
            "git {args:?} failed unexpectedly in {} (status {status:?}): {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}
