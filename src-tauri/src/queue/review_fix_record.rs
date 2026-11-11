// SPDX-License-Identifier: MPL-2.0
//! Durable evidence that an AI Review fix run may have edited a Task Checkout.
//!
//! The record is written before the fix agent starts and removed only when the
//! checkout is known to match its baseline or the fix was committed. A record
//! that survives an interruption, a failure or a cancellation refuses every
//! later Worker commit until a Human decides (see
//! `docs/architecture/task-checkouts-and-stacks.md`, "Failed review-fix runs").
//!
//! It lives in the checkout's Git directory: Git never commits that directory,
//! older builds ignore the file, and the persisted Task gains no field.
//!
//! A Human decision is made against a preview. The preview names the paths the
//! fix changed, and a digest of its exact contents; a decision must send that
//! digest back, and is refused if the checkout has moved since.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const FILE_NAME: &str = "slashit-review-fix.json";
const VERSION: u32 = 1;

/// Why a Worker commit is refused while a record is unresolved.
pub const UNRESOLVED_REFUSAL: &str = "An AI Review fix run ended with its edits unresolved. \
    The Task Checkout may hold partial edits from that fix. They were preserved and not \
    committed; a Human decision is needed before the Task's work can be committed.";

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewFixRecord {
    pub version: u32,
    pub task_id: Uuid,
    /// Names the fix run that wrote the record. Only that run may clear it.
    pub run_id: Uuid,
    /// The checkout's content before the fix agent ran; see `CheckoutSnapshot::content`.
    pub baseline_content: String,
    pub started_at: DateTime<Utc>,
    /// Every uncommitted path before the fix ran, with its fingerprint. A path
    /// whose fingerprint has changed since is attributed to the fix.
    #[serde(default)]
    pub pre_fix: BTreeMap<String, String>,
    /// HEAD when the fix started. A fix that moved HEAD committed work, which
    /// discard and archive never touch.
    #[serde(default)]
    pub head: Option<String>,
    /// Every uncommitted path when the fix run settled. Written once, by the
    /// run that owns the record. A path changed after this is never attributed
    /// to the fix. Absent if the run never settled (for example, if SlashIt was
    /// killed): then every path that changed since `pre_fix` is attributed.
    #[serde(default)]
    pub post_fix: Option<BTreeMap<String, String>>,
}

/// The paths a fix is attributed to, shown to a Human before any decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewFixPreview {
    /// Paths the fix changed. Discard and archive act on these only.
    pub attributed: Vec<String>,
    /// Paths that were already uncommitted before the fix and are unchanged.
    /// Never touched.
    pub unrelated: Vec<String>,
    /// Paths already uncommitted before the fix that the fix also changed. They
    /// cannot be separated, so discard and archive refuse while any remain.
    pub blocked: Vec<String>,
    /// The record exists but cannot be read, so nothing can be attributed.
    pub record_unreadable: bool,
    /// HEAD moved since the fix started. Discard and archive refuse, because
    /// they touch only uncommitted paths and the fix's commit would stay.
    pub head_moved: bool,
    /// Confirms exactly this preview. A decision must send it back unchanged.
    pub digest: String,
}

/// A Human decision on an unresolved record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Remove the record. The checkout is not changed.
    Keep,
    /// Move the attributed paths into a stash, then remove the record.
    Archive,
    /// Remove the attributed paths, then remove the record.
    Discard,
}

async fn record_path(working_dir: &str) -> Result<PathBuf, String> {
    let output = crate::host_process::tokio_command("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(working_dir)
        .output()
        .await
        .map_err(|e| format!("could not locate the checkout's Git directory: {e}"))?;
    if !output.status.success() {
        return Err("could not locate the checkout's Git directory".to_string());
    }
    let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(git_dir).join(FILE_NAME))
}

/// Runs git in the checkout. Pathspecs are literal, so a path is never a pattern.
async fn git(working_dir: &str, args: &[&str]) -> Result<Output, String> {
    crate::host_process::tokio_command("git")
        .args(args)
        .current_dir(working_dir)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .output()
        .await
        .map_err(|e| format!("git could not run: {e}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A digest of what the path holds now: its content, its link target, or `-`
/// when absent. Unreadable content is `?`, which never matches an earlier state.
fn content_digest(path: &Path) -> String {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return "-".to_string();
    };
    let bytes = if meta.file_type().is_symlink() {
        std::fs::read_link(path)
            .ok()
            .map(|target| target.to_string_lossy().into_owned().into_bytes())
    } else if meta.is_file() {
        std::fs::read(path).ok()
    } else {
        None
    };
    match bytes {
        Some(bytes) => hex(&Sha256::digest(&bytes)),
        None => "?".to_string(),
    }
}

/// Every uncommitted path with its fingerprint: the status pair and content.
async fn fingerprints(working_dir: &str) -> Result<BTreeMap<String, String>, String> {
    let output = git(
        working_dir,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--no-renames",
            "--untracked-files=all",
        ],
    )
    .await?;
    if !output.status.success() {
        return Err(format!(
            "the checkout's status could not be read: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut map = BTreeMap::new();
    for entry in output.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        if entry.len() < 4 {
            return Err("the checkout's status has an entry that cannot be read".to_string());
        }
        // A name that is not UTF-8 cannot be passed back to git or named in a
        // preview, so the checkout is refused rather than acted on partly.
        let status = std::str::from_utf8(&entry[..2])
            .map_err(|_| "a path in the checkout is not valid UTF-8".to_string())?;
        let path = std::str::from_utf8(&entry[3..])
            .map_err(|_| "a path in the checkout is not valid UTF-8".to_string())?;
        let digest = content_digest(&Path::new(working_dir).join(path));
        map.insert(path.to_string(), format!("{status} {digest}"));
    }
    Ok(map)
}

/// HEAD's commit id, or `None` where the checkout has no commit yet.
async fn current_head(working_dir: &str) -> Option<String> {
    let output = git(working_dir, &["rev-parse", "-q", "--verify", "HEAD"])
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Creates the record only if no file exists at `path`. The exclusive create is
/// the whole check-and-write, so two creates cannot both succeed, in this
/// process or another.
fn create_record(path: &Path, record: &ReviewFixRecord) -> Result<(), String> {
    let bytes = serde_json::to_vec(record).map_err(|e| e.to_string())?;
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(
                "an earlier AI Review fix run is still unresolved in this checkout".to_string(),
            )
        }
        Err(e) => return Err(format!("the record could not be made durable: {e}")),
    };
    let durable = file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| sync_parent(path));
    drop(file);
    if let Err(e) = durable {
        // The file is ours alone, so removing the partial write is safe. If
        // removal also fails, the unreadable file still fails closed.
        let _ = std::fs::remove_file(path);
        return Err(format!("the record could not be made durable: {e}"));
    }
    Ok(())
}

/// Makes the new directory entry durable, so a power loss cannot leave a fix
/// running with no record.
#[cfg(unix)]
fn sync_parent(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(dir) => std::fs::File::open(dir)?.sync_all(),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Writes the record before a fix agent may edit and returns the run id that
/// owns it. Refuses when a record already exists: an earlier unresolved fix's
/// baseline must never be overwritten.
pub async fn begin(
    working_dir: &str,
    task_id: Uuid,
    baseline_content: &str,
) -> Result<Uuid, String> {
    let path = record_path(working_dir).await?;
    let pre_fix = fingerprints(working_dir).await?;
    let head = current_head(working_dir).await;
    let run_id = Uuid::new_v4();
    create_record(
        &path,
        &ReviewFixRecord {
            version: VERSION,
            task_id,
            run_id,
            baseline_content: baseline_content.to_string(),
            started_at: Utc::now(),
            pre_fix,
            head,
            post_fix: None,
        },
    )?;
    Ok(run_id)
}

/// Records the checkout's uncommitted paths as the fix run leaves them. Only the
/// run that wrote the record may seal it. Called when the run settles, before
/// any Human decision can exist.
pub async fn seal(working_dir: &str, task_id: Uuid, run_id: Uuid) -> Result<(), String> {
    let path = record_path(working_dir).await?;
    let Some(mut record) = read_at(&path)? else {
        return Err("there is no review-fix record to seal".to_string());
    };
    if record.task_id != task_id || record.run_id != run_id {
        return Err("the review-fix record belongs to another run and was not sealed".to_string());
    }
    if record.post_fix.is_some() {
        return Err("the review-fix record is already sealed".to_string());
    }
    record.post_fix = Some(fingerprints(working_dir).await?);
    let bytes = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
    slashit_durable_fs::write_atomic(&path, &bytes, slashit_durable_fs::Permissions::Preserve)
        .map(|_| ())
        .map_err(|e| format!("the record could not be sealed: {e}"))
}

fn read_at(path: &Path) -> Result<Option<ReviewFixRecord>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("the review-fix record is unreadable: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("the review-fix record could not be read: {e}")),
    }
}

/// The unresolved record, if any. An unreadable record is an error, so commits
/// fail closed rather than proceed.
pub async fn pending(working_dir: &str) -> Result<Option<ReviewFixRecord>, String> {
    read_at(&record_path(working_dir).await?)
}

/// Whether a record file exists, readable or not.
pub async fn present(working_dir: &str) -> Result<bool, String> {
    record_path(working_dir)
        .await?
        .try_exists()
        .map_err(|e| format!("the review-fix record could not be checked: {e}"))
}

fn remove_at(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("the review-fix record could not be removed: {e}")),
    }
}

/// Removes the record only if `run_id` wrote it for `task_id`. Another run's
/// record, and an unreadable one whose owner cannot be shown, stay in place.
pub async fn clear(working_dir: &str, task_id: Uuid, run_id: Uuid) -> Result<(), String> {
    let path = record_path(working_dir).await?;
    match read_at(&path)? {
        None => Ok(()),
        Some(record) if record.task_id == task_id && record.run_id == run_id => remove_at(&path),
        Some(_) => {
            Err("the review-fix record belongs to another run and was left in place".to_string())
        }
    }
}

/// The preview a decision must confirm: the paths the fix changed, the paths
/// left alone, and a digest of exactly that state.
pub async fn preview(working_dir: &str) -> Result<ReviewFixPreview, String> {
    let now = fingerprints(working_dir).await?;
    let path = record_path(working_dir).await?;
    let mut attributed = Vec::new();
    let mut unrelated = Vec::new();
    let mut blocked = Vec::new();
    let mut head_moved = false;
    let (run, record_unreadable) = match read_at(&path) {
        Ok(Some(record)) => {
            head_moved = current_head(working_dir).await != record.head;
            for (changed, fingerprint) in &now {
                let before = record.pre_fix.get(changed);
                match &record.post_fix {
                    // Sealed: the fix's paths are those it left dirty and changed
                    // from the start. Anything dirty only now was edited later.
                    Some(after) => match after.get(changed) {
                        // Uncommitted before the fix and untouched by it: never the fix's.
                        Some(sealed) if before == Some(sealed) => unrelated.push(changed.clone()),
                        // Uncommitted before the fix and changed by it: cannot be separated.
                        Some(_) if before.is_some() => blocked.push(changed.clone()),
                        // Dirty only because of the fix: attributed while still as sealed.
                        Some(sealed) if sealed == fingerprint => attributed.push(changed.clone()),
                        Some(_) => blocked.push(changed.clone()),
                        // Clean when the run settled: changed later by someone else.
                        None => unrelated.push(changed.clone()),
                    },
                    // Not sealed (the run never settled): every path that changed
                    // since the fix started is attributed, so the preview shows it.
                    None => match before {
                        None => attributed.push(changed.clone()),
                        Some(before) if before == fingerprint => unrelated.push(changed.clone()),
                        Some(_) => blocked.push(changed.clone()),
                    },
                }
            }
            // Uncommitted before the fix and clean now: the fix may have reverted or
            // deleted that edit, so it is blocked rather than dropped from view.
            for before in record.pre_fix.keys() {
                if !now.contains_key(before) {
                    blocked.push(before.clone());
                }
            }
            (record.run_id.to_string(), false)
        }
        Ok(None) => return Err("No AI Review fix is unresolved in this checkout.".to_string()),
        Err(_) => {
            blocked.extend(now.keys().cloned());
            ("unreadable".to_string(), true)
        }
    };
    let mut lines = vec![format!("run {run}"), format!("head_moved {head_moved}")];
    for (tag, paths) in [
        ("attributed", &attributed),
        ("unrelated", &unrelated),
        ("blocked", &blocked),
    ] {
        for changed in paths {
            let state = now.get(changed).map_or("gone", String::as_str);
            lines.push(format!("{tag} {changed} {state}"));
        }
    }
    Ok(ReviewFixPreview {
        attributed,
        unrelated,
        blocked,
        record_unreadable,
        head_moved,
        digest: hex(&Sha256::digest(lines.join("\n").as_bytes())),
    })
}

/// Applies a Human decision. `confirm_digest` must equal the digest of the
/// preview computed now; a checkout that moved since is refused and untouched.
pub async fn resolve(
    working_dir: &str,
    decision: Decision,
    confirm_digest: &str,
) -> Result<(), String> {
    let preview = preview(working_dir).await?;
    if preview.digest != confirm_digest {
        return Err(
            "The checkout changed after the preview, or the confirmation does not match it. \
             Nothing was changed; preview again and confirm the new digest."
                .to_string(),
        );
    }
    let path = record_path(working_dir).await?;
    if decision == Decision::Keep {
        return remove_at(&path);
    }
    if preview.record_unreadable {
        return Err(
            "The review-fix record is unreadable, so no edit can be attributed to the fix. \
             Nothing was changed; keep it for review or leave it."
                .to_string(),
        );
    }
    if preview.head_moved {
        return Err(
            "The fix moved HEAD, so it committed work. Discard and archive touch only uncommitted \
             paths and would leave that commit in place. Nothing was changed; keep it for review."
                .to_string(),
        );
    }
    if !preview.blocked.is_empty() {
        return Err(format!(
            "These paths had edits before the fix and the fix changed them too, so they cannot \
             be separated: {}. Nothing was changed.",
            preview.blocked.join(", ")
        ));
    }
    if !preview.attributed.is_empty() {
        match decision {
            Decision::Archive => {
                archive_paths(working_dir, &preview.attributed, confirm_digest).await?
            }
            Decision::Discard => remove_paths(working_dir, &preview.attributed).await?,
            Decision::Keep => {}
        }
    }
    remove_at(&path)
}

/// Stashes exactly the attributed paths, so the edits stay recoverable.
async fn archive_paths(working_dir: &str, paths: &[String], digest: &str) -> Result<(), String> {
    // Checked before any stash exists: git saves a stash and then fails on a
    // pathspec that names a deletion, which would leave the stash behind.
    for changed in paths {
        if std::fs::symlink_metadata(Path::new(working_dir).join(changed)).is_err() {
            return Err(format!(
                "{changed} is deleted, and archive cannot stash a deletion. Nothing was changed; \
                 discard it, or keep the record."
            ));
        }
    }
    let message = format!(
        "SlashIt: archived AI Review fix edits (preview {})",
        &digest[..12]
    );
    let mut args = vec![
        "stash",
        "push",
        "--include-untracked",
        "-m",
        message.as_str(),
        "--",
    ];
    args.extend(paths.iter().map(String::as_str));
    let output = git(working_dir, &args).await?;
    if !output.status.success() {
        return Err(format!(
            "the fix's edits could not be archived, so nothing was changed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Returns each attributed path to HEAD, or deletes it when git does not track
/// it. Refuses on the first failure; paths already handled stay handled.
async fn remove_paths(working_dir: &str, paths: &[String]) -> Result<(), String> {
    for changed in paths {
        let in_head = git(working_dir, &["cat-file", "-e", &format!("HEAD:{changed}")])
            .await?
            .status
            .success();
        let in_index = git(working_dir, &["ls-files", "--error-unmatch", "--", changed])
            .await?
            .status
            .success();
        // Restoring from HEAD covers modified, deleted and staged-deleted paths
        // alike, and resets the index entry too. A path HEAD does not know is
        // removed from the index (if staged) and from the worktree. Only a path
        // git lists as untracked is deleted as a plain file.
        let output = if in_head {
            git(
                working_dir,
                &[
                    "restore",
                    "--source=HEAD",
                    "--staged",
                    "--worktree",
                    "--",
                    changed,
                ],
            )
            .await?
        } else if in_index {
            git(working_dir, &["rm", "-q", "-f", "--", changed]).await?
        } else {
            std::fs::remove_file(Path::new(working_dir).join(changed))
                .map_err(|e| format!("{changed} could not be removed: {e}"))?;
            continue;
        };
        if !output.status.success() {
            return Err(format!(
                "{changed} could not be restored: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Runs git and returns its output without judging it.
    fn git_raw(dir: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs")
    }

    /// Runs git and requires success.
    fn git_ok(dir: &Path, args: &[&str]) -> Output {
        let output = git_raw(dir, args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git_ok(dir.path(), &["init", "-q"]);
        git_ok(dir.path(), &["config", "user.name", "SlashIt Test"]);
        git_ok(
            dir.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        dir
    }

    /// A repository with one commit, so stash and worktrees have a HEAD.
    fn repo_with_commit() -> tempfile::TempDir {
        let dir = repo();
        std::fs::write(dir.path().join("tracked.txt"), "base\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        git_ok(dir.path(), &["add", "."]);
        git_ok(dir.path(), &["commit", "-q", "-m", "base"]);
        dir
    }

    #[tokio::test]
    async fn a_record_round_trips_and_is_cleared() {
        let dir = repo();
        let path = dir.path().to_str().unwrap();
        let task = Uuid::new_v4();
        assert_eq!(pending(path).await.unwrap(), None);
        let run = begin(path, task, "content-a").await.unwrap();
        let record = pending(path).await.unwrap().expect("record present");
        assert_eq!(record.task_id, task);
        assert_eq!(record.run_id, run);
        assert_eq!(record.baseline_content, "content-a");
        clear(path, task, run).await.unwrap();
        assert_eq!(pending(path).await.unwrap(), None);
    }

    #[tokio::test]
    async fn begin_never_overwrites_an_unresolved_baseline() {
        let dir = repo();
        let path = dir.path().to_str().unwrap();
        begin(path, Uuid::new_v4(), "first").await.unwrap();
        assert!(begin(path, Uuid::new_v4(), "second").await.is_err());
        assert_eq!(
            pending(path).await.unwrap().unwrap().baseline_content,
            "first"
        );
    }

    /// Two creates released together at a barrier, the feared interleaving for a
    /// check followed by a separate write. Exactly one may succeed, and the file
    /// must hold the winner's baseline.
    #[test]
    fn two_racing_creates_leave_exactly_one_record() {
        for round in 0..16 {
            let dir = repo();
            let path = dir.path().join(FILE_NAME);
            let barrier = std::sync::Barrier::new(2);
            let (a, b) = std::thread::scope(|scope| {
                let spawn = |baseline: &'static str| {
                    let (path, barrier) = (&path, &barrier);
                    scope.spawn(move || {
                        let record = ReviewFixRecord {
                            version: VERSION,
                            task_id: Uuid::new_v4(),
                            run_id: Uuid::new_v4(),
                            baseline_content: baseline.to_string(),
                            started_at: Utc::now(),
                            pre_fix: BTreeMap::new(),
                            head: None,
                            post_fix: None,
                        };
                        barrier.wait();
                        create_record(path, &record).map(|()| baseline)
                    })
                };
                let (a, b) = (spawn("first"), spawn("second"));
                (a.join().unwrap(), b.join().unwrap())
            });
            assert_eq!(
                a.is_ok() as u8 + b.is_ok() as u8,
                1,
                "round {round}: {a:?} {b:?}"
            );
            let winner = a.or(b).unwrap();
            let stored: ReviewFixRecord =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(stored.baseline_content, winner, "round {round}");
        }
    }

    #[tokio::test]
    async fn clear_refuses_a_record_another_run_wrote() {
        let dir = repo();
        let path = dir.path().to_str().unwrap();
        let task = Uuid::new_v4();
        let run = begin(path, task, "x").await.unwrap();
        assert!(clear(path, task, Uuid::new_v4()).await.is_err());
        assert!(clear(path, Uuid::new_v4(), run).await.is_err());
        assert!(
            pending(path).await.unwrap().is_some(),
            "the record was removed"
        );
        clear(path, task, run).await.unwrap();
        assert_eq!(pending(path).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_unreadable_record_fails_closed() {
        let dir = repo();
        let path = dir.path().to_str().unwrap();
        begin(path, Uuid::new_v4(), "x").await.unwrap();
        let file = record_path(path).await.unwrap();
        std::fs::write(&file, b"not json").unwrap();
        assert!(pending(path).await.is_err());
        assert!(clear(path, Uuid::new_v4(), Uuid::new_v4()).await.is_err());
        assert!(file.exists(), "an unreadable record was removed");
    }

    /// The fix's edits, then a user's edit made before the fix: only the fix's
    /// edits are attributed.
    async fn fix_over_user_edits(dir: &Path) -> String {
        let path = dir.to_str().unwrap();
        std::fs::write(dir.join("user.txt"), "mine\n").unwrap();
        begin(path, Uuid::new_v4(), "x").await.unwrap();
        std::fs::write(dir.join("review-fix.txt"), "partial\n").unwrap();
        std::fs::write(dir.join("tracked.txt"), "fixed\n").unwrap();
        preview(path).await.unwrap().digest
    }

    #[tokio::test]
    async fn the_preview_attributes_only_what_the_fix_changed() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("user.txt"), "mine\n").unwrap();
        begin(path, Uuid::new_v4(), "x").await.unwrap();
        std::fs::write(dir.path().join("review-fix.txt"), "partial\n").unwrap();
        let preview = preview(path).await.unwrap();
        assert_eq!(preview.attributed, vec!["review-fix.txt".to_string()]);
        assert_eq!(preview.unrelated, vec!["user.txt".to_string()]);
        assert!(preview.blocked.is_empty());
        assert!(!preview.record_unreadable);
    }

    #[tokio::test]
    async fn keep_removes_the_record_and_changes_nothing_else() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        let digest = {
            begin(path, Uuid::new_v4(), "x").await.unwrap();
            std::fs::write(dir.path().join("review-fix.txt"), "partial\n").unwrap();
            preview(path).await.unwrap().digest
        };
        let head = git_ok(dir.path(), &["rev-parse", "HEAD"]).stdout;
        resolve(path, Decision::Keep, &digest).await.unwrap();
        assert_eq!(pending(path).await.unwrap(), None);
        assert!(dir.path().join("review-fix.txt").exists());
        assert_eq!(git_ok(dir.path(), &["rev-parse", "HEAD"]).stdout, head);
    }

    #[tokio::test]
    async fn a_decision_with_a_mismatched_digest_changes_nothing() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        fix_over_user_edits(dir.path()).await;
        assert!(resolve(path, Decision::Keep, "not-the-digest")
            .await
            .is_err());
        assert!(resolve(path, Decision::Discard, "not-the-digest")
            .await
            .is_err());
        assert!(pending(path).await.unwrap().is_some());
        assert!(dir.path().join("review-fix.txt").exists());
        assert!(dir.path().join("user.txt").exists());
    }

    /// Unrelated edits survive a discard, and the fix's edits are reverted.
    #[tokio::test]
    async fn discard_removes_only_the_fixes_edits() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        let digest = fix_over_user_edits(dir.path()).await;
        resolve(path, Decision::Discard, &digest).await.unwrap();
        assert_eq!(pending(path).await.unwrap(), None);
        assert!(!dir.path().join("review-fix.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "base\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("user.txt")).unwrap(),
            "mine\n"
        );
    }

    /// A checkout that changed after the preview is refused, and nothing moves.
    #[tokio::test]
    async fn discard_refuses_a_checkout_that_changed_after_the_preview() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        let digest = fix_over_user_edits(dir.path()).await;
        std::fs::write(dir.path().join("review-fix.txt"), "more\n").unwrap();
        assert!(resolve(path, Decision::Discard, &digest).await.is_err());
        assert!(pending(path).await.unwrap().is_some());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("review-fix.txt")).unwrap(),
            "more\n"
        );
    }

    /// A path the user had already edited and the fix edited again cannot be
    /// separated, so discard refuses and leaves it as it is.
    #[tokio::test]
    async fn discard_refuses_a_path_that_also_had_unrelated_edits() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("user.txt"), "mine\n").unwrap();
        begin(path, Uuid::new_v4(), "x").await.unwrap();
        std::fs::write(dir.path().join("user.txt"), "fixed over mine\n").unwrap();
        let digest = preview(path).await.unwrap();
        assert_eq!(digest.blocked, vec!["user.txt".to_string()]);
        let error = resolve(path, Decision::Discard, &digest.digest)
            .await
            .unwrap_err();
        assert!(error.contains("user.txt"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("user.txt")).unwrap(),
            "fixed over mine\n"
        );
        assert!(pending(path).await.unwrap().is_some());
    }

    /// A fix that deleted a tracked file with `git rm` leaves a staged deletion.
    /// Discard must restore it, not leave the deletion staged for the next commit.
    #[tokio::test]
    async fn discard_restores_a_staged_deletion_by_the_fix() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        begin(path, Uuid::new_v4(), "x").await.unwrap();
        git_ok(dir.path(), &["rm", "-q", "tracked.txt"]);
        let digest = preview(path).await.unwrap().digest;
        resolve(path, Decision::Discard, &digest).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "base\n"
        );
        let staged = git_ok(dir.path(), &["diff", "--cached", "--name-only"]).stdout;
        assert!(staged.is_empty(), "a deletion stayed staged: {staged:?}");
    }

    /// A fix that moved HEAD committed work. Discard and archive touch only
    /// uncommitted paths, so they refuse and leave the commit in place.
    #[tokio::test]
    async fn discard_refuses_when_the_fix_moved_head() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        begin(path, Uuid::new_v4(), "x").await.unwrap();
        std::fs::write(dir.path().join("review-fix.txt"), "committed\n").unwrap();
        git_ok(dir.path(), &["add", "review-fix.txt"]);
        git_ok(dir.path(), &["commit", "-q", "-m", "fix"]);
        std::fs::write(dir.path().join("more.txt"), "edit\n").unwrap();
        let digest = preview(path).await.unwrap();
        assert!(digest.head_moved);
        let error = resolve(path, Decision::Discard, &digest.digest)
            .await
            .unwrap_err();
        assert!(error.contains("moved HEAD"), "{error}");
        assert!(dir.path().join("more.txt").exists());
        assert!(pending(path).await.unwrap().is_some());
    }

    /// Once the run settles and is sealed, an edit made later by anyone else is
    /// not attributed to the fix: it is left alone. Changing the fix's own file
    /// after the seal blocks the decision rather than discarding it silently.
    #[tokio::test]
    async fn edits_made_after_the_fix_settled_are_not_attributed_to_it() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        let task = Uuid::new_v4();
        let run = begin(path, task, "x").await.unwrap();
        std::fs::write(dir.path().join("review-fix.txt"), "partial\n").unwrap();
        seal(path, task, run).await.unwrap();
        std::fs::write(dir.path().join("later.txt"), "a human edit\n").unwrap();
        let preview = preview(path).await.unwrap();
        assert_eq!(preview.attributed, vec!["review-fix.txt".to_string()]);
        assert_eq!(preview.unrelated, vec!["later.txt".to_string()]);
        assert!(preview.blocked.is_empty());
        std::fs::write(dir.path().join("review-fix.txt"), "changed later\n").unwrap();
        let after = super::preview(path).await.unwrap();
        assert_eq!(after.blocked, vec!["review-fix.txt".to_string()]);
    }

    /// Only the run that wrote the record may seal it.
    #[tokio::test]
    async fn another_run_cannot_seal_the_record() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        let task = Uuid::new_v4();
        begin(path, task, "x").await.unwrap();
        assert!(seal(path, task, Uuid::new_v4()).await.is_err());
        assert!(pending(path).await.unwrap().unwrap().post_fix.is_none());
    }

    /// Archive cannot stash a staged deletion. It must refuse before creating
    /// any stash, and leave the deletion as the fix staged it.
    #[tokio::test]
    async fn archive_refuses_a_staged_deletion_without_a_stray_stash() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        begin(path, Uuid::new_v4(), "x").await.unwrap();
        git_ok(dir.path(), &["rm", "-q", "tracked.txt"]);
        let digest = preview(path).await.unwrap().digest;
        assert!(resolve(path, Decision::Archive, &digest).await.is_err());
        let stashes = git_ok(dir.path(), &["stash", "list"]).stdout;
        assert!(stashes.is_empty(), "a stray stash was left: {stashes:?}");
        assert!(pending(path).await.unwrap().is_some());
    }

    /// A user's untracked file that the fix deletes is not in the checkout any
    /// more, so it must be blocked instead of silently dropping out of view.
    #[tokio::test]
    async fn a_pre_existing_file_the_fix_deletes_is_blocked() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "mine\n").unwrap();
        begin(path, Uuid::new_v4(), "x").await.unwrap();
        std::fs::remove_file(dir.path().join("notes.txt")).unwrap();
        let preview = preview(path).await.unwrap();
        assert_eq!(preview.blocked, vec!["notes.txt".to_string()]);
        assert!(preview.attributed.is_empty());
        let error = resolve(path, Decision::Discard, &preview.digest)
            .await
            .unwrap_err();
        assert!(error.contains("notes.txt"), "{error}");
    }

    /// Once sealed, an uncommitted edit the fix did not touch is the user's,
    /// even if the user edits it again after the seal. It is left alone, not
    /// blocked and not attributed.
    #[tokio::test]
    async fn a_pre_existing_file_the_fix_did_not_touch_stays_unrelated_after_the_seal() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        let task = Uuid::new_v4();
        std::fs::write(dir.path().join("user.txt"), "mine\n").unwrap();
        let run = begin(path, task, "x").await.unwrap();
        std::fs::write(dir.path().join("review-fix.txt"), "partial\n").unwrap();
        seal(path, task, run).await.unwrap();
        std::fs::write(dir.path().join("user.txt"), "mine, edited again\n").unwrap();
        let preview = preview(path).await.unwrap();
        assert_eq!(preview.attributed, vec!["review-fix.txt".to_string()]);
        assert_eq!(preview.unrelated, vec!["user.txt".to_string()]);
        assert!(preview.blocked.is_empty());
    }

    /// A record is sealed once. A second seal would move the fix's boundary.
    #[tokio::test]
    async fn a_record_cannot_be_sealed_twice() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        let task = Uuid::new_v4();
        let run = begin(path, task, "x").await.unwrap();
        seal(path, task, run).await.unwrap();
        assert!(seal(path, task, run).await.is_err());
    }

    /// Archive moves only the fix's paths into a recoverable stash.
    #[tokio::test]
    async fn archive_stashes_only_the_fixes_edits() {
        let dir = repo_with_commit();
        let path = dir.path().to_str().unwrap();
        let digest = fix_over_user_edits(dir.path()).await;
        resolve(path, Decision::Archive, &digest).await.unwrap();
        assert_eq!(pending(path).await.unwrap(), None);
        assert!(!dir.path().join("review-fix.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("user.txt")).unwrap(),
            "mine\n"
        );
        let stashes =
            String::from_utf8_lossy(&git_ok(dir.path(), &["stash", "list"]).stdout).to_string();
        assert!(
            stashes.contains("archived AI Review fix edits"),
            "{stashes}"
        );
    }

    /// An archive that cannot be taken (no commit, so no HEAD) must leave the
    /// record and the edits exactly as they were.
    #[tokio::test]
    async fn an_archive_that_cannot_stash_keeps_the_record_and_edits() {
        let dir = repo();
        let path = dir.path().to_str().unwrap();
        begin(path, Uuid::new_v4(), "x").await.unwrap();
        std::fs::write(dir.path().join("review-fix.txt"), "partial\n").unwrap();
        let digest = preview(path).await.unwrap().digest;
        assert!(resolve(path, Decision::Archive, &digest).await.is_err());
        assert!(pending(path).await.unwrap().is_some());
        assert!(dir.path().join("review-fix.txt").exists());
    }

    /// Production cleanup removes a checkout with `git worktree remove`, without
    /// `--force`. A dirty checkout is refused, so its record and its edits both
    /// survive.
    #[tokio::test]
    async fn a_dirty_checkout_is_refused_by_worktree_removal_with_its_record() {
        let repo_dir = repo_with_commit();
        let parent = tempfile::tempdir().unwrap();
        let wt = parent.path().join("wt");
        let wt_str = wt.to_str().unwrap();
        git_ok(
            repo_dir.path(),
            &["worktree", "add", "-q", "-b", "wt-dirty", wt_str],
        );
        begin(wt_str, Uuid::new_v4(), "x").await.unwrap();
        std::fs::write(wt.join("review-fix.txt"), "partial\n").unwrap();
        let removal = git_raw(repo_dir.path(), &["worktree", "remove", wt_str]);
        assert!(!removal.status.success(), "a dirty checkout was removed");
        assert!(wt.join("review-fix.txt").exists());
        assert!(pending(wt_str).await.unwrap().is_some());
    }

    /// The record lives in the worktree's admin directory. Ignored-only edits do
    /// not make the checkout dirty, so removal succeeds and takes the record
    /// with it. The record is therefore not evidence for ignored output.
    #[tokio::test]
    async fn a_checkout_with_only_ignored_edits_is_removed_with_its_record() {
        let repo_dir = repo_with_commit();
        let parent = tempfile::tempdir().unwrap();
        let wt = parent.path().join("wt");
        let wt_str = wt.to_str().unwrap();
        git_ok(
            repo_dir.path(),
            &["worktree", "add", "-q", "-b", "wt-ignored", wt_str],
        );
        begin(wt_str, Uuid::new_v4(), "x").await.unwrap();
        let record_file = record_path(wt_str).await.unwrap();
        assert!(record_file.exists());
        std::fs::write(wt.join("ignored.txt"), "build output\n").unwrap();
        let removal = git_raw(repo_dir.path(), &["worktree", "remove", wt_str]);
        assert!(
            removal.status.success(),
            "{}",
            String::from_utf8_lossy(&removal.stderr)
        );
        assert!(!record_file.exists(), "the record outlived its worktree");
    }
}
