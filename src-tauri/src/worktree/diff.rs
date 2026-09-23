//! One canonical task diff.
//!
//! A task needs exactly one answer to "what changed in this task", used by
//! both the UI's diff modal and the AI reviewer. Guessing the starting point
//! (previously: merge-base with `main`/`master`, falling back to `HEAD~1`) is
//! wrong because a stacked task's branch starts at its dependency's tip, not
//! at `main` -- that guess attributes the whole parent task's history to the
//! child. The starting point instead comes from `Task::base_commit`, recorded
//! once when the task's worktree is first created (see `queue::executor`) and
//! never re-derived, so retries and reattachment keep comparing against the
//! same boundary the task actually started from.
//!
//! A missing `base_commit` (a task persisted before this field existed, or
//! one attached to a branch SlashIt didn't create) is answered with
//! [`TaskDiffError::UnknownBoundary`] -- this is deliberately not the same
//! outcome as "no changes", and callers must not collapse the two.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Per-untracked-file content cap. `MAX_REQUEST_BYTES`
/// (`crates/slashit-ipc/src/protocol.rs`) already bounds the *entire* IPC
/// response carrying this diff's `patch` string at 1 MiB; letting a single
/// untracked file consume that whole budget would starve every other file in
/// the same task's diff (tracked or untracked), so this is a fraction of that
/// existing product limit, not a value copied from an unrelated tool.
const UNTRACKED_FILE_LIMIT_BYTES: usize = 256 * 1024;

/// How much of an oversized untracked file's `git diff --no-index` output is
/// actually read before giving up. This must be strictly more than
/// [`UNTRACKED_FILE_LIMIT_BYTES`]: the check is "did the stream exceed the
/// limit", decided by reading past it, not by `stat`-ing the file first and
/// then reading -- a file can grow between those two steps (TOCTOU), and a
/// single capped read has no such gap because there is no separate check.
const UNTRACKED_READ_CAP_BYTES: usize = UNTRACKED_FILE_LIMIT_BYTES * 2;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskDiff {
    pub patch: String,
    pub stat: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskDiffError {
    /// No durable starting point is recorded for this task. NOT equivalent to
    /// an empty diff -- an empty diff means "compared, nothing changed"; this
    /// means "cannot compare at all".
    UnknownBoundary,
    /// A subprocess failed or produced output that could not be interpreted.
    /// NOT equivalent to an empty diff either.
    Failed(String),
}

impl std::fmt::Display for TaskDiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskDiffError::UnknownBoundary => write!(
                f,
                "this task has no recorded starting commit, so its diff boundary is unknown"
            ),
            TaskDiffError::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

/// Compute the one canonical diff for a task: everything that has happened in
/// `working_dir` since `base_commit`, whether committed, staged, unstaged, or
/// untracked. Read-only: never stages, commits, or otherwise mutates the
/// repository it inspects.
pub async fn task_diff(working_dir: &str, base_commit: Option<&str>) -> Result<TaskDiff, TaskDiffError> {
    let Some(base) = base_commit else {
        return Err(TaskDiffError::UnknownBoundary);
    };

    // Try jj first: `commit_changes` runs `jj describe` (which edits the
    // commit message without advancing `@`) before falling back to `git
    // commit`, so on a genuinely jj-managed worktree `jj diff` would still
    // show the agent's change after a commit. Verified unreachable in
    // practice for every task worktree the current `WorktreeManager`
    // produces (plain `git worktree add`, never `jj workspace add` -- no
    // `.jj` directory exists to find), but a failed/inapplicable jj
    // invocation falls through to git for free, so trying it first costs
    // nothing if that ever changes. Only a *successful* jj exit is trusted;
    // an inapplicable jj (no repo found) must not be mistaken for "ran
    // successfully and found nothing".
    if let Some(diff) = jj_task_diff(working_dir).await {
        return Ok(diff);
    }

    git_task_diff(working_dir, base).await
}

async fn jj_task_diff(working_dir: &str) -> Option<TaskDiff> {
    // jj's own repo discovery walks up parent directories exactly like
    // git's does, so without this check a task worktree nested (however
    // unexpectedly -- e.g. under an ancestor a user's own `wt` placement
    // template happens to colocate with a jj-managed repo) could silently
    // diff that unrelated ancestor's workspace, completely bypassing
    // `base_commit`, instead of failing closed and falling through to the
    // git path below. Requiring `.jj` directly in `working_dir` -- not an
    // ancestor -- makes "this worktree is jj-managed" an explicit check
    // rather than an accident of where `WorktreeManager` happens to place
    // worktrees today.
    if !Path::new(working_dir).join(".jj").exists() {
        return None;
    }
    let patch = run_output("jj", &["diff", "--git"], working_dir).await.ok()?;
    let stat = run_output("jj", &["diff", "--stat"], working_dir).await.ok()?;
    Some(TaskDiff { patch, stat })
}

async fn git_task_diff(working_dir: &str, base: &str) -> Result<TaskDiff, TaskDiffError> {
    // A single base-to-working-tree comparison (not `base..HEAD` concatenated
    // with a separate `diff HEAD`) is what keeps a file that was committed
    // and then edited again from being reported twice: `git diff <base>`
    // (default form, no `..`) compares `base` directly against the current
    // index+working tree in one pass, so there is only ever one hunk set per
    // file no matter how many times it was committed since `base`.
    let patch = run_git(working_dir, &["diff", base]).await?;
    let stat = run_git(working_dir, &["diff", "--stat", base]).await?;

    let (untracked_patch, untracked_stat) = untracked_diff(working_dir, base).await?;

    let mut full_patch = patch;
    append_section(&mut full_patch, &untracked_patch);
    let mut full_stat = stat;
    append_section(&mut full_stat, &untracked_stat);

    Ok(TaskDiff { patch: full_patch, stat: full_stat })
}

fn append_section(into: &mut String, section: &str) {
    if section.is_empty() {
        return;
    }
    if !into.is_empty() && !into.ends_with('\n') {
        into.push('\n');
    }
    into.push_str(section);
}

async fn run_git(dir: &str, args: &[&str]) -> Result<String, TaskDiffError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .map_err(|e| TaskDiffError::Failed(format!("failed to run git {args:?}: {e}")))?;
    // A dirty/clean working tree relative to `base` is not a git error;
    // `git diff` exits 0 either way. Anything non-zero (unknown revision,
    // not a repository, etc.) is a genuine failure and must be surfaced as
    // one -- never silently turned into an empty string.
    if !output.status.success() {
        return Err(TaskDiffError::Failed(format!(
            "git {args:?} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn run_output(program: &str, args: &[&str], dir: &str) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn untracked_diff(working_dir: &str, base: &str) -> Result<(String, String), TaskDiffError> {
    let listing = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .current_dir(working_dir)
        .output()
        .await
        .map_err(|e| TaskDiffError::Failed(format!("failed to run git ls-files: {e}")))?;
    if !listing.status.success() {
        return Err(TaskDiffError::Failed(format!(
            "git ls-files exited with {}: {}",
            listing.status,
            String::from_utf8_lossy(&listing.stderr)
        )));
    }

    // A file `git rm --cached`'d but left on disk is both "deleted since
    // base" and, mechanically, untracked. The deletion side is the truthful
    // one to report; the on-disk leftover self-corrects the next time
    // `commit_changes` runs `git add -A`. Excluding it here avoids reporting
    // the same file as both removed and newly added.
    let deleted = run_git(working_dir, &["diff", "--name-only", "--diff-filter=D", base]).await?;
    let deleted_paths: HashSet<Vec<u8>> = deleted.lines().map(|l| l.as_bytes().to_vec()).collect();

    let mut patch = String::new();
    let mut stat = String::new();

    for raw_path in listing.stdout.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        if deleted_paths.contains(raw_path) {
            continue;
        }
        let display_path = escape_for_marker(&String::from_utf8_lossy(raw_path));
        let rel_path = os_string_from_bytes(raw_path);
        let full_path = Path::new(working_dir).join(&rel_path);

        match diff_one_untracked_file(working_dir, &full_path, &rel_path).await? {
            UntrackedFile::Diff(content) => {
                append_section(&mut patch, &content);
                let added = content.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).count();
                stat.push_str(&format!(" {display_path} | new file, {added} lines added\n"));
            }
            UntrackedFile::Empty => {
                stat.push_str(&format!(" {display_path} | new empty file\n"));
            }
            UntrackedFile::Binary => {
                let marker = format!("Binary file added: {display_path}\n");
                append_section(&mut patch, &marker);
                stat.push_str(&format!(" {display_path} | new binary file\n"));
            }
            UntrackedFile::Oversized => {
                let size_hint = tokio::fs::metadata(&full_path)
                    .await
                    .map(|m| format!("{} bytes", m.len()))
                    .unwrap_or_else(|_| "unknown size".to_string());
                let marker = format!(
                    "New file {display_path} ({size_hint}) exceeds the {UNTRACKED_FILE_LIMIT_BYTES}-byte diff \
                     content limit; content omitted.\n"
                );
                // Both the human/UI diff and the AI reviewer read `patch`;
                // only writing this into `stat` would leave the reviewer
                // with no trace the file exists at all.
                append_section(&mut patch, &marker);
                stat.push_str(&format!(
                    " {display_path} | new file, content omitted (exceeds {UNTRACKED_FILE_LIMIT_BYTES} bytes)\n"
                ));
            }
        }
    }

    Ok((patch, stat))
}

enum UntrackedFile {
    Diff(String),
    Empty,
    Binary,
    Oversized,
}

async fn diff_one_untracked_file(
    working_dir: &str,
    full_path: &Path,
    rel_path: &OsString,
) -> Result<UntrackedFile, TaskDiffError> {
    let null_device: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

    let mut child = Command::new("git")
        .arg("diff")
        .arg("--no-index")
        .arg("--")
        .arg(null_device)
        .arg(rel_path)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| TaskDiffError::Failed(format!("failed to spawn git diff --no-index: {e}")))?;

    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let mut oversized = false;
    loop {
        let n = stdout
            .read(&mut chunk)
            .await
            .map_err(|e| TaskDiffError::Failed(format!("failed to read git diff --no-index output: {e}")))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > UNTRACKED_READ_CAP_BYTES {
            oversized = true;
            break;
        }
    }

    if oversized {
        // Stop draining rather than let the child keep writing into a pipe
        // nobody is reading further; kill()+wait() together guarantee the
        // process is reaped either way, leaving no zombie.
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Ok(UntrackedFile::Oversized);
    }
    if buf.len() > UNTRACKED_FILE_LIMIT_BYTES {
        let _ = child.wait().await;
        return Ok(UntrackedFile::Oversized);
    }

    let status = child
        .wait()
        .await
        .map_err(|e| TaskDiffError::Failed(format!("failed to wait on git diff --no-index: {e}")))?;
    // `--no-index` exit codes: 0 = files identical (an empty new file vs
    // /dev/null), 1 = a difference was found (the ordinary case for any
    // non-empty new file), >=2 = a real error (unreadable file, etc).
    match status.code() {
        Some(0) | Some(1) => {}
        _ => {
            return Err(TaskDiffError::Failed(format!(
                "git diff --no-index exited with {status} for {}",
                full_path.display()
            )))
        }
    }

    let text = String::from_utf8_lossy(&buf).into_owned();
    if text.trim().is_empty() {
        return Ok(UntrackedFile::Empty);
    }
    // A hunk marker (`@@`) proves this is real text-diff content, checked
    // before looking for git's own "Binary files ... differ" line, so a text
    // file whose *content* happens to contain that exact phrase is not
    // misclassified as binary.
    if text.contains("@@") {
        return Ok(UntrackedFile::Diff(text));
    }
    if text.contains("Binary files ") {
        return Ok(UntrackedFile::Binary);
    }
    Ok(UntrackedFile::Diff(text))
}

#[cfg(unix)]
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(bytes).to_os_string()
}

#[cfg(not(unix))]
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    // Git on non-Unix platforms works with UTF-8/UTF-16 paths already; lossy
    // decoding here only matters for the (practically unreachable, since git
    // itself wouldn't have produced it) case of genuinely invalid bytes.
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Escape characters that could let an untracked filename break out of the
/// synthesized marker lines above. Scoped to those hand-built lines only --
/// it does not and cannot sanitize `git diff`'s own raw output (the ordinary
/// per-file patch/tracked-file diff content elsewhere in this module), which
/// carries the same latent risk of breaking the ` ```diff ` fence it gets
/// embedded in (`queue::prompt::build_review_prompt`) that any git diff
/// containing a literal ` ``` ` sequence already has, independent of this
/// feature; hardening that fence is a `queue::prompt` prompt-construction
/// concern, not a task-diff-boundary one.
fn escape_for_marker(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('`', "'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn git(dir: &Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git command failed to spawn");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
    }

    fn commit_all(dir: &Path, msg: &str) {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", msg, "--allow-empty"]);
    }

    fn head(dir: &Path) -> String {
        let out = StdCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn unknown_boundary_is_not_an_empty_diff() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_all(dir.path(), "base");

        let result = task_diff(dir.path().to_str().unwrap(), None).await;
        assert_eq!(result, Err(TaskDiffError::UnknownBoundary));
    }

    #[tokio::test]
    async fn empty_diff_is_a_real_success_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_all(dir.path(), "base");
        let base = head(dir.path());

        let diff = task_diff(dir.path().to_str().unwrap(), Some(&base)).await.unwrap();
        assert!(diff.patch.is_empty());
        assert!(diff.stat.is_empty());
    }

    #[tokio::test]
    async fn committed_task_change_is_visible() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        commit_all(dir.path(), "base");
        let base = head(dir.path());

        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        commit_all(dir.path(), "task change");

        let diff = task_diff(dir.path().to_str().unwrap(), Some(&base)).await.unwrap();
        assert!(diff.patch.contains("+two"), "patch was: {}", diff.patch);
    }

    #[tokio::test]
    async fn file_committed_then_edited_again_appears_once() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        commit_all(dir.path(), "base");
        let base = head(dir.path());

        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        commit_all(dir.path(), "first edit");
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        // second edit left uncommitted on purpose

        let diff = task_diff(dir.path().to_str().unwrap(), Some(&base)).await.unwrap();
        let header_count = diff.patch.matches("diff --git a/a.txt b/a.txt").count();
        assert_eq!(header_count, 1, "a.txt must appear exactly once: {}", diff.patch);
        assert!(diff.patch.contains("+two"));
        assert!(diff.patch.contains("+three"));
    }

    #[tokio::test]
    async fn untracked_file_is_visible_without_staging() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_all(dir.path(), "base");
        let base = head(dir.path());

        std::fs::write(dir.path().join("new.txt"), "hello\n").unwrap();

        let diff = task_diff(dir.path().to_str().unwrap(), Some(&base)).await.unwrap();
        assert!(diff.patch.contains("new.txt"), "patch was: {}", diff.patch);
        assert!(diff.patch.contains("+hello"), "patch was: {}", diff.patch);
        assert!(diff.stat.contains("new.txt"));

        // Must remain read-only: the file is still untracked afterward.
        let status = StdCommand::new("git")
            .args(["status", "--porcelain"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let status_text = String::from_utf8_lossy(&status.stdout);
        assert!(status_text.contains("?? new.txt"), "status was: {status_text}");
    }

    #[tokio::test]
    async fn oversized_untracked_file_is_a_bounded_marker_not_full_content() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_all(dir.path(), "base");
        let base = head(dir.path());

        let big = "x".repeat(UNTRACKED_FILE_LIMIT_BYTES + 1024);
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();

        let diff = task_diff(dir.path().to_str().unwrap(), Some(&base)).await.unwrap();
        assert!(diff.patch.contains("big.txt"));
        assert!(diff.patch.contains("exceeds"));
        assert!(diff.patch.len() < big.len(), "content should have been omitted, not embedded");
        assert!(diff.stat.contains("content omitted"));
    }

    #[tokio::test]
    async fn ignored_untracked_file_is_excluded() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        commit_all(dir.path(), "base");
        let base = head(dir.path());

        std::fs::write(dir.path().join("ignored.txt"), "secret\n").unwrap();

        let diff = task_diff(dir.path().to_str().unwrap(), Some(&base)).await.unwrap();
        assert!(!diff.patch.contains("ignored.txt"));
        assert!(!diff.stat.contains("ignored.txt"));
    }

    #[tokio::test]
    async fn nonexistent_base_commit_is_a_failure_not_an_empty_diff() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_all(dir.path(), "base");

        let result = task_diff(dir.path().to_str().unwrap(), Some("0000000000000000000000000000000000000000")).await;
        assert!(matches!(result, Err(TaskDiffError::Failed(_))), "got: {result:?}");
    }

    #[tokio::test]
    async fn parent_branch_work_is_excluded_from_child_task_diff() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_all(dir.path(), "root");

        // Parent task's own work, committed on a branch.
        git(dir.path(), &["checkout", "-q", "-b", "parent-task"]);
        std::fs::write(dir.path().join("parent.txt"), "parent work\n").unwrap();
        commit_all(dir.path(), "parent task work");
        let child_base = head(dir.path());

        // Child task stacks on top of the parent's tip -- this IS its base.
        git(dir.path(), &["checkout", "-q", "-b", "child-task"]);
        std::fs::write(dir.path().join("child.txt"), "child work\n").unwrap();
        commit_all(dir.path(), "child task work");

        let diff = task_diff(dir.path().to_str().unwrap(), Some(&child_base)).await.unwrap();
        assert!(diff.patch.contains("child.txt"));
        assert!(!diff.patch.contains("parent.txt"), "parent work leaked into child diff: {}", diff.patch);
    }
}
