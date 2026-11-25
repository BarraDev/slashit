// SPDX-License-Identifier: MPL-2.0
//! Content-aware fingerprint of a Task Checkout's uncommitted work.
//!
//! A human-authorized new attempt builds on the work already in the checkout,
//! so its decision must notice any change to what the person inspected, even
//! a change to a file that was already modified and whose `git status` line
//! has not changed. This module gathers evidence only: read-only Git commands
//! with optional locks disabled, and direct reads of the files. It never
//! writes to the repository or to the checkout.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::registry_lock::{run_git_with, Input};

/// Largest regular file whose contents are hashed. A larger file is refused,
/// never skipped, because skipping it would leave the change undetected.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Largest total of file contents hashed for one fingerprint.
pub const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// The budget for one fingerprint. Tests use smaller values than the defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_FILE_BYTES,
            max_total_bytes: MAX_TOTAL_BYTES,
        }
    }
}

/// The state of a checkout at one moment, as far as a decision depends on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutFingerprint {
    /// The full commit that HEAD named when the fingerprint was taken.
    pub head: String,
    /// The symbolic branch ref, or `None` when HEAD is detached.
    pub branch_ref: Option<String>,
    /// The uncommitted paths, in the order the digest uses.
    pub dirty_paths: Vec<String>,
    /// `sha256:` followed by lowercase hexadecimal.
    pub digest: String,
}

/// Why no fingerprint was produced. Each variant refuses the decision, so
/// none of them is ever read as "unchanged".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FingerprintError {
    /// Git could not run or failed. The text is Git's own message.
    Git(String),
    /// HEAD does not name a commit, as in an empty repository.
    NoHead,
    /// The directory is inside a Git repository but is not that repository's
    /// root. Git reports paths relative to the root, so the fingerprint would
    /// read the wrong files.
    NotCheckoutRoot { path: String },
    /// Git is mid-operation (a merge, rebase, cherry-pick, revert, bisect, or
    /// a held index lock). The named marker is the reason.
    InProgress(String),
    /// A path has conflict stages, so its contents are not one thing.
    Unmerged(String),
    /// A path is not valid UTF-8, so it cannot be recorded losslessly.
    NonUtf8Path,
    /// A path is a kind of entry this fingerprint does not cover, such as a
    /// nested repository, a submodule, or a device.
    Unsupported { path: String, reason: &'static str },
    /// A path could not be read.
    Unreadable { path: String, reason: String },
    /// A file is larger than [`Limits::max_file_bytes`].
    TooLarge { path: String, limit: u64 },
    /// The contents hashed so far would exceed [`Limits::max_total_bytes`].
    TooMuchData { limit: u64 },
}

impl fmt::Display for FingerprintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(message) => write!(f, "Git could not describe the checkout: {message}"),
            Self::NoHead => write!(f, "The checkout has no commit to inspect."),
            Self::NotCheckoutRoot { path } => write!(
                f,
                "{path} is not the root of its Git checkout, so its changes cannot be inspected."
            ),
            Self::InProgress(marker) => write!(
                f,
                "Git is in the middle of an operation ({marker}). Finish or abort it first."
            ),
            Self::Unmerged(path) => write!(f, "{path} has unresolved conflicts."),
            Self::NonUtf8Path => write!(f, "A changed path is not valid UTF-8."),
            Self::Unsupported { path, reason } => write!(f, "{path} cannot be inspected: {reason}."),
            Self::Unreadable { path, reason } => write!(f, "{path} could not be read: {reason}."),
            Self::TooLarge { path, limit } => {
                write!(f, "{path} is larger than the {limit}-byte inspection limit.")
            }
            Self::TooMuchData { limit } => write!(
                f,
                "The changed files exceed the {limit}-byte inspection limit in total."
            ),
        }
    }
}

/// Markers whose presence means Git is partway through an operation, or is
/// holding the index. Each is read from the checkout's own Git directory.
const IN_PROGRESS_MARKERS: [&str; 9] = [
    "MERGE_HEAD",
    "REBASE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "rebase-merge",
    "rebase-apply",
    "sequencer",
    "index.lock",
];

/// Fingerprints `checkout` with the default [`Limits`].
pub async fn fingerprint(checkout: &Path) -> Result<CheckoutFingerprint, FingerprintError> {
    fingerprint_with(checkout, Limits::default()).await
}

/// Fingerprints `checkout` within `limits`. Refuses rather than returning a
/// partial result.
pub async fn fingerprint_with(
    checkout: &Path,
    limits: Limits,
) -> Result<CheckoutFingerprint, FingerprintError> {
    let dir = checkout.to_string_lossy().into_owned();

    // `--quiet` makes an unborn HEAD fail silently. Any other failure, such as
    // a directory that is not a repository, keeps Git's own message.
    let head = git(&dir, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .await
        .map_err(|e| match e {
            FingerprintError::Git(message) if message.is_empty() => FingerprintError::NoHead,
            other => other,
        })?;
    let head = String::from_utf8_lossy(&head).trim().to_string();

    // Status paths are relative to the repository root. Reading them from a
    // subdirectory of a larger repository would hash other files, so the
    // checkout must be the root itself.
    let root = git(&dir, &["rev-parse", "--show-toplevel"]).await?;
    let root = PathBuf::from(String::from_utf8_lossy(&root).trim_end_matches(['\n', '\r']));
    let same_root = match (fs::canonicalize(&root), fs::canonicalize(checkout)) {
        (Ok(root), Ok(checkout)) => root == checkout,
        _ => false,
    };
    if !same_root {
        return Err(FingerprintError::NotCheckoutRoot { path: dir });
    }

    // A detached HEAD makes `symbolic-ref` fail. HEAD resolved above, so this
    // is the only reason it fails here.
    // `-q` exits 1 with no message for a detached HEAD, which is a valid state.
    // Any other failure is refused rather than read as detached.
    let branch_ref = match git(&dir, &["symbolic-ref", "-q", "HEAD"]).await {
        Ok(out) => Some(String::from_utf8_lossy(&out).trim().to_string()),
        Err(FingerprintError::Git(message)) if message.is_empty() => None,
        Err(other) => return Err(other),
    };

    for marker in IN_PROGRESS_MARKERS {
        let out = git(&dir, &["rev-parse", "--git-path", marker]).await?;
        let raw = PathBuf::from(String::from_utf8_lossy(&out).trim_end().to_string());
        let full = if raw.is_absolute() { raw } else { checkout.join(raw) };
        if full.exists() {
            return Err(FingerprintError::InProgress(marker.to_string()));
        }
    }

    let status = git(
        &dir,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await?;
    let entries = parse_status(&status)?;

    let index = git(&dir, &["ls-files", "--stage", "-z"]).await?;
    let index = index_records(&index)?;

    let mut digest = Sha256::new();
    feed(&mut digest, b"slashit.checkout-fingerprint.v1");
    feed(&mut digest, head.as_bytes());
    feed(&mut digest, branch_ref.as_deref().unwrap_or("").as_bytes());

    let mut total = 0u64;
    let mut dirty_paths = Vec::with_capacity(entries.len());
    for (path, xy) in &entries {
        let name = String::from_utf8(path.clone()).map_err(|_| FingerprintError::NonUtf8Path)?;
        feed(&mut digest, path);
        feed(&mut digest, xy);
        match index.get(path) {
            Some(record) => feed(&mut digest, record.as_bytes()),
            None => feed(&mut digest, b""),
        }
        describe_worktree(checkout, &name, limits, &mut total, &mut digest)?;
        dirty_paths.push(name);
    }

    Ok(CheckoutFingerprint {
        head,
        branch_ref,
        dirty_paths,
        digest: format!("sha256:{}", hex::encode(digest.finalize())),
    })
}

/// Runs one read-only Git command in `dir` and returns its standard output.
/// Optional locks are disabled so that `status` never refreshes the index.
async fn git(dir: &str, args: &[&str]) -> Result<Vec<u8>, FingerprintError> {
    let envs = [
        ("GIT_OPTIONAL_LOCKS", "0".to_string()),
        ("GIT_TERMINAL_PROMPT", "0".to_string()),
    ];
    let ran = run_git_with("git".into(), dir, args, &envs, Input::None)
        .await
        .map_err(|e| FingerprintError::Git(format!("failed to run git: {e}")))?;
    if !ran.output.status.success() {
        let stderr = String::from_utf8_lossy(&ran.output.stderr).trim().to_string();
        return Err(FingerprintError::Git(stderr));
    }
    Ok(ran.output.stdout)
}

/// Parses `status --porcelain=v1 -z` into paths and their two status letters,
/// sorted by raw bytes. A rename or copy names its source in the next field.
/// Conflicts and nested repositories are refused.
fn parse_status(out: &[u8]) -> Result<BTreeMap<Vec<u8>, [u8; 2]>, FingerprintError> {
    let mut fields = out.split(|b| *b == 0).filter(|field| !field.is_empty());
    let mut entries = BTreeMap::new();
    while let Some(field) = fields.next() {
        if field.len() < 4 || field[2] != b' ' {
            return Err(FingerprintError::Git(
                "status output had an unexpected shape".into(),
            ));
        }
        let xy = [field[0], field[1]];
        let path = field[3..].to_vec();
        let text = String::from_utf8_lossy(&path).into_owned();
        if xy[0] == b'U' || xy[1] == b'U' || xy == *b"DD" || xy == *b"AA" {
            return Err(FingerprintError::Unmerged(text));
        }
        if path.ends_with(b"/") {
            return Err(FingerprintError::Unsupported {
                path: text,
                reason: "it is a nested repository or submodule",
            });
        }
        entries.insert(path, xy);
        if matches!(xy[0], b'R' | b'C') || matches!(xy[1], b'R' | b'C') {
            let source = fields.next().ok_or_else(|| {
                FingerprintError::Git("a rename had no source path".into())
            })?;
            // The source may also be listed with its own status. Keep that
            // entry rather than replacing it with the rename's status.
            entries.entry(source.to_vec()).or_insert(xy);
        }
    }
    Ok(entries)
}

/// The index record `mode blob stage` for each tracked path, keyed by path.
fn index_records(out: &[u8]) -> Result<HashMap<Vec<u8>, String>, FingerprintError> {
    let mut records = HashMap::new();
    for entry in out.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
        let tab = entry
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| FingerprintError::Git("ls-files output had no tab".into()))?;
        let meta = String::from_utf8_lossy(&entry[..tab]).into_owned();
        records.insert(entry[tab + 1..].to_vec(), meta);
    }
    Ok(records)
}

/// Feeds one path's worktree state into the digest. A regular file contributes
/// its mode, size, and SHA-256 of its contents. A symbolic link contributes its
/// target text and is never followed. An absent path is recorded as absent.
fn describe_worktree(
    checkout: &Path,
    path: &str,
    limits: Limits,
    total: &mut u64,
    digest: &mut Sha256,
) -> Result<(), FingerprintError> {
    let full = checkout.join(path);
    // A parent directory that is a link would move the read outside the
    // checkout, so the path is refused before anything is read.
    let mut parent = checkout.to_path_buf();
    let mut parts = Path::new(path).components().peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            break;
        }
        parent.push(part);
        if fs::symlink_metadata(&parent).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(FingerprintError::Unsupported {
                path: path.to_string(),
                reason: "a parent directory is a symbolic link",
            });
        }
    }
    let unreadable = |e: std::io::Error| FingerprintError::Unreadable {
        path: path.to_string(),
        reason: e.to_string(),
    };
    let meta = match fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            feed(digest, b"missing");
            return Ok(());
        }
        Err(e) => return Err(unreadable(e)),
    };

    if meta.file_type().is_symlink() {
        let target = fs::read_link(&full).map_err(unreadable)?;
        feed(digest, b"symlink");
        feed(digest, &os_bytes(target.as_os_str()));
        return Ok(());
    }

    if !meta.is_file() {
        return Err(FingerprintError::Unsupported {
            path: path.to_string(),
            reason: "it is not a regular file, a symbolic link, or a missing path",
        });
    }

    let remaining = limits.max_total_bytes.saturating_sub(*total);
    if meta.len() > limits.max_file_bytes {
        return Err(FingerprintError::TooLarge {
            path: path.to_string(),
            limit: limits.max_file_bytes,
        });
    }
    if meta.len() > remaining {
        return Err(FingerprintError::TooMuchData {
            limit: limits.max_total_bytes,
        });
    }

    let mut file = fs::File::open(&full).map_err(unreadable)?;
    let mut contents = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut read_total = 0u64;
    loop {
        let n = file.read(&mut buffer).map_err(unreadable)?;
        if n == 0 {
            break;
        }
        read_total += n as u64;
        // The file may have grown since its metadata was read, so the budget
        // is enforced on what is actually read, not on what was reported.
        if read_total > limits.max_file_bytes {
            return Err(FingerprintError::TooLarge {
                path: path.to_string(),
                limit: limits.max_file_bytes,
            });
        }
        if read_total > remaining {
            return Err(FingerprintError::TooMuchData {
                limit: limits.max_total_bytes,
            });
        }
        contents.update(&buffer[..n]);
    }
    *total += read_total;

    feed(digest, b"file");
    feed(digest, &mode_bytes(&meta));
    feed(digest, &read_total.to_be_bytes());
    feed(digest, &contents.finalize());
    Ok(())
}

#[cfg(unix)]
fn mode_bytes(meta: &fs::Metadata) -> Vec<u8> {
    use std::os::unix::fs::PermissionsExt;
    (meta.permissions().mode() & 0o7777).to_be_bytes().to_vec()
}

#[cfg(not(unix))]
fn mode_bytes(_meta: &fs::Metadata) -> Vec<u8> {
    Vec::new()
}

#[cfg(unix)]
fn os_bytes(text: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    text.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_bytes(text: &std::ffi::OsStr) -> Vec<u8> {
    text.to_string_lossy().into_owned().into_bytes()
}

/// Length-prefixed, so that no two different sequences of fields can produce
/// the same bytes.
fn feed(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct Repo {
        _root: tempfile::TempDir,
        path: PathBuf,
        config: PathBuf,
    }

    fn git_in(repo: &Repo, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(&repo.path)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", &repo.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    fn repo() -> Repo {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("checkout");
        fs::create_dir(&path).expect("checkout dir");
        let config = root.path().join("gitconfig");
        fs::write(&config, "").expect("empty global config");
        let repo = Repo {
            _root: root,
            path,
            config,
        };
        git_in(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.path.join("a.txt"), "one\n").expect("tracked file");
        git_in(&repo, &["add", "a.txt"]);
        git_in(&repo, &["commit", "-q", "-m", "base"]);
        repo
    }

    fn write(repo: &Repo, name: &str, bytes: &[u8]) {
        fs::write(repo.path.join(name), bytes).expect("write file");
    }

    #[tokio::test]
    async fn a_clean_checkout_fingerprints_its_head_and_branch_deterministically() {
        let repo = repo();
        let first = fingerprint(&repo.path).await.expect("clean fingerprint");
        let second = fingerprint(&repo.path).await.expect("repeat fingerprint");
        assert_eq!(first, second);
        assert!(first.dirty_paths.is_empty());
        assert_eq!(first.branch_ref.as_deref(), Some("refs/heads/main"));
        assert!(first.digest.starts_with("sha256:"));
    }

    #[tokio::test]
    async fn the_same_status_line_over_different_contents_changes_the_digest() {
        let repo = repo();
        write(&repo, "a.txt", b"two\n");
        let before = fingerprint(&repo.path).await.expect("fingerprint");
        write(&repo, "a.txt", b"three\n");
        let after = fingerprint(&repo.path).await.expect("fingerprint");
        assert_eq!(before.dirty_paths, after.dirty_paths);
        assert_ne!(before.digest, after.digest);
    }

    #[tokio::test]
    async fn staged_content_is_part_of_the_fingerprint() {
        let repo = repo();
        write(&repo, "a.txt", b"staged\n");
        git_in(&repo, &["add", "a.txt"]);
        write(&repo, "a.txt", b"staged then edited\n");
        let staged_then_edited = fingerprint(&repo.path).await.expect("fingerprint");
        write(&repo, "a.txt", b"staged\n");
        let staged_and_clean = fingerprint(&repo.path).await.expect("fingerprint");
        assert_ne!(staged_then_edited.digest, staged_and_clean.digest);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_tracked_path_under_a_directory_replaced_by_a_link_is_refused() {
        let repo = repo();
        fs::create_dir(repo.path.join("vendor")).expect("dir");
        write(&repo, "vendor/lib.txt", b"inside");
        git_in(&repo, &["add", "vendor/lib.txt"]);
        git_in(&repo, &["commit", "-q", "-m", "vendor"]);
        let outside = repo.path.parent().expect("parent").join("outside");
        fs::create_dir(&outside).expect("outside dir");
        fs::write(outside.join("lib.txt"), b"outside").expect("outside file");
        fs::remove_dir_all(repo.path.join("vendor")).expect("remove dir");
        std::os::unix::fs::symlink(&outside, repo.path.join("vendor")).expect("symlink");
        let err = fingerprint(&repo.path).await.expect_err("refused");
        assert!(matches!(err, FingerprintError::Unsupported { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn an_untracked_file_is_fingerprinted_by_its_contents() {
        let repo = repo();
        write(&repo, "b.txt", b"x");
        let first = fingerprint(&repo.path).await.expect("fingerprint");
        write(&repo, "b.txt", b"y");
        let second = fingerprint(&repo.path).await.expect("fingerprint");
        assert_eq!(first.dirty_paths, vec!["b.txt".to_string()]);
        assert_ne!(first.digest, second.digest);
    }

    #[tokio::test]
    async fn binary_contents_are_hashed_byte_for_byte() {
        let repo = repo();
        write(&repo, "blob.bin", &[0, 159, 255]);
        let first = fingerprint(&repo.path).await.expect("fingerprint");
        write(&repo, "blob.bin", &[0, 159, 254]);
        let second = fingerprint(&repo.path).await.expect("fingerprint");
        assert_ne!(first.digest, second.digest);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symbolic_link_is_recorded_by_its_target_and_never_followed() {
        let repo = repo();
        std::os::unix::fs::symlink("a.txt", repo.path.join("link")).expect("symlink");
        let first = fingerprint(&repo.path).await.expect("fingerprint");
        fs::remove_file(repo.path.join("link")).expect("remove link");
        std::os::unix::fs::symlink("elsewhere.txt", repo.path.join("link")).expect("symlink");
        let second = fingerprint(&repo.path).await.expect("fingerprint");
        assert_ne!(first.digest, second.digest);
    }

    #[tokio::test]
    async fn a_merge_in_progress_is_refused_and_nothing_is_written() {
        let repo = repo();
        write(&repo, "a.txt", b"edited\n");
        fs::write(repo.path.join(".git").join("MERGE_HEAD"), "0".repeat(40))
            .expect("marker");
        let file_before = fs::read(repo.path.join("a.txt")).expect("read");
        let index_before = fs::read(repo.path.join(".git").join("index")).expect("read");
        let err = fingerprint(&repo.path).await.expect_err("refused");
        assert_eq!(err, FingerprintError::InProgress("MERGE_HEAD".into()));
        assert_eq!(fs::read(repo.path.join("a.txt")).expect("read"), file_before);
        assert_eq!(
            fs::read(repo.path.join(".git").join("index")).expect("read"),
            index_before
        );
    }

    #[tokio::test]
    async fn an_unmerged_path_is_refused() {
        let repo = repo();
        git_in(&repo, &["checkout", "-q", "-b", "side"]);
        write(&repo, "a.txt", b"side\n");
        git_in(&repo, &["commit", "-q", "-am", "side"]);
        git_in(&repo, &["checkout", "-q", "main"]);
        write(&repo, "a.txt", b"main\n");
        git_in(&repo, &["commit", "-q", "-am", "main"]);
        // The merge is expected to conflict, leaving the path unmerged.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo.path)
            .args(["merge", "--no-edit", "side"])
            .env("GIT_CONFIG_GLOBAL", &repo.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output();
        // A conflicted merge leaves MERGE_HEAD behind. Remove it so the refusal
        // under test is the unmerged path itself, not the operation marker.
        fs::remove_file(repo.path.join(".git").join("MERGE_HEAD")).expect("remove marker");
        let err = fingerprint(&repo.path).await.expect_err("refused");
        assert_eq!(err, FingerprintError::Unmerged("a.txt".into()));
    }

    #[tokio::test]
    async fn a_nested_repository_is_refused_as_unsupported() {
        let repo = repo();
        let nested = repo.path.join("nested");
        fs::create_dir(&nested).expect("nested dir");
        Command::new("git")
            .arg("-C")
            .arg(&nested)
            .args(["init", "-q"])
            .env("GIT_CONFIG_GLOBAL", &repo.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("git init");
        write(&repo, "nested/inner.txt", b"inner");
        let err = fingerprint(&repo.path).await.expect_err("refused");
        assert!(matches!(err, FingerprintError::Unsupported { .. }));
    }

    #[tokio::test]
    async fn a_file_over_the_per_file_limit_is_refused_not_skipped() {
        let repo = repo();
        write(&repo, "big.txt", b"12345");
        let limits = Limits {
            max_file_bytes: 4,
            max_total_bytes: 1024,
        };
        let err = fingerprint_with(&repo.path, limits).await.expect_err("refused");
        assert!(matches!(err, FingerprintError::TooLarge { .. }));
    }

    #[tokio::test]
    async fn the_total_budget_is_enforced_across_files() {
        let repo = repo();
        write(&repo, "one.txt", b"abc");
        write(&repo, "two.txt", b"def");
        let limits = Limits {
            max_file_bytes: 1024,
            max_total_bytes: 4,
        };
        let err = fingerprint_with(&repo.path, limits).await.expect_err("refused");
        assert_eq!(err, FingerprintError::TooMuchData { limit: 4 });
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_non_utf8_path_is_refused_because_it_cannot_be_recorded() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let repo = repo();
        let name = OsStr::from_bytes(b"bad-\xff.txt");
        fs::write(repo.path.join(name), b"x").expect("write");
        let err = fingerprint(&repo.path).await.expect_err("refused");
        assert_eq!(err, FingerprintError::NonUtf8Path);
    }

    #[tokio::test]
    async fn fingerprinting_changes_no_bytes_in_the_checkout_or_its_git_directory() {
        let repo = repo();
        write(&repo, "a.txt", b"edited\n");
        write(&repo, "b.txt", b"new\n");
        let snapshot = |path: &Path| fs::read(path).expect("read");
        let tracked = snapshot(&repo.path.join("a.txt"));
        let untracked = snapshot(&repo.path.join("b.txt"));
        let index = snapshot(&repo.path.join(".git").join("index"));
        let head = snapshot(&repo.path.join(".git").join("HEAD"));
        fingerprint(&repo.path).await.expect("fingerprint");
        assert_eq!(snapshot(&repo.path.join("a.txt")), tracked);
        assert_eq!(snapshot(&repo.path.join("b.txt")), untracked);
        assert_eq!(snapshot(&repo.path.join(".git").join("index")), index);
        assert_eq!(snapshot(&repo.path.join(".git").join("HEAD")), head);
    }

    #[tokio::test]
    async fn a_detached_head_has_no_branch_and_an_empty_repository_has_no_head() {
        let repo = repo();
        git_in(&repo, &["checkout", "-q", "--detach"]);
        let detached = fingerprint(&repo.path).await.expect("fingerprint");
        assert_eq!(detached.branch_ref, None);

        let empty_root = tempfile::tempdir().expect("temp dir");
        let empty = empty_root.path().join("empty");
        fs::create_dir(&empty).expect("dir");
        Command::new("git")
            .arg("-C")
            .arg(&empty)
            .args(["init", "-q"])
            .env("GIT_CONFIG_GLOBAL", &repo.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("git init");
        let err = fingerprint(&empty).await.expect_err("no head");
        assert_eq!(err, FingerprintError::NoHead);
    }

    #[tokio::test]
    async fn a_subdirectory_of_a_repository_is_refused_rather_than_misread() {
        let repo = repo();
        let sub = repo.path.join("sub");
        fs::create_dir(&sub).expect("subdirectory");
        write(&repo, "sub/inner.txt", b"inner");
        let err = fingerprint(&sub).await.expect_err("refused");
        assert!(matches!(err, FingerprintError::NotCheckoutRoot { .. }));
    }

    #[tokio::test]
    async fn a_deleted_tracked_file_is_fingerprinted_as_missing() {
        let repo = repo();
        write(&repo, "a.txt", b"edited\n");
        let edited = fingerprint(&repo.path).await.expect("fingerprint");
        fs::remove_file(repo.path.join("a.txt")).expect("delete");
        let deleted = fingerprint(&repo.path).await.expect("fingerprint");
        assert_eq!(deleted.dirty_paths, vec!["a.txt".to_string()]);
        assert_ne!(edited.digest, deleted.digest);
    }

    #[tokio::test]
    async fn every_in_progress_git_operation_marker_refuses_the_fingerprint() {
        for marker in ["REBASE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "BISECT_LOG", "index.lock"] {
            let repo = repo();
            write(&repo, "a.txt", b"edited\n");
            fs::write(repo.path.join(".git").join(marker), "x").expect("marker");
            let err = fingerprint(&repo.path).await.expect_err("refused");
            assert_eq!(err, FingerprintError::InProgress(marker.into()));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_changed_file_is_refused_not_hashed_as_empty() {
        use std::os::unix::fs::PermissionsExt;
        let repo = repo();
        write(&repo, "secret.txt", b"private");
        let path = repo.path.join("secret.txt");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");
        let readable_as_this_user = fs::File::open(&path).is_ok();
        let result = fingerprint(&repo.path).await;
        // Restore before any assertion so the temporary directory can be removed.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("restore");
        if readable_as_this_user {
            // Running with privileges that ignore permission bits; nothing to prove.
            return;
        }
        assert!(matches!(result, Err(FingerprintError::Unreadable { .. })));
    }
}
