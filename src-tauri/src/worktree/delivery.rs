//! What a Task Checkout can prove about its work reaching the remote.
//!
//! Two questions, both answered from Git alone:
//!
//! - [`checkout_snapshot`]: what exactly is in the checkout right now, as a
//!   tree object. Two snapshots differ exactly when something changed in
//!   between, which is how an apply knows that one fix agent edited a file.
//! - [`RemoteBranch`]: where the pull request's branch is on `origin` after a
//!   refresh, and whether a given commit is contained in it.
//!
//! Neither one decides whether a review reply may be posted; that rule lives
//! with the PR review commands. Nothing here is called from rendering code:
//! [`RemoteBranch::refresh`] reaches the network, so only a user-started
//! command that already talks to the forge may call it.

use std::path::Path;

use super::restack;

/// The tree of everything in the checkout at `working_dir`: tracked changes,
/// untracked files and deletions, without touching the checkout's index, its
/// files or its branch.
///
/// A scratch index is filled from `HEAD` and then from the working tree, and
/// written out as a tree. Files Git ignores are not part of it, as they are
/// not part of a commit.
pub async fn checkout_snapshot(working_dir: &str) -> Result<String, String> {
    let scratch = std::env::temp_dir().join(format!("slashit-snapshot-{}.index", uuid::Uuid::new_v4()));
    let result = snapshot_with_index(working_dir, &scratch).await;
    let _ = tokio::fs::remove_file(&scratch).await;
    result
}

async fn snapshot_with_index(working_dir: &str, index: &Path) -> Result<String, String> {
    let run = |args: &'static [&'static str]| {
        let index = index.to_path_buf();
        async move {
            let output = tokio::process::Command::new("git")
                .args(args)
                .env("GIT_INDEX_FILE", &index)
                .current_dir(working_dir)
                .output()
                .await
                .map_err(|e| format!("could not run git: {e}"))?;
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                Err(format!(
                    "git {} failed: {}",
                    args.first().unwrap_or(&""),
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
        }
    };
    // A repository without a commit yet has no HEAD to read: start empty.
    let _ = run(&["read-tree", "HEAD"]).await;
    run(&["add", "-A"]).await?;
    run(&["write-tree"]).await
}

/// Whether the change between the trees `before` and `after` (what one fix
/// agent did) is still in `commit`, path by path.
///
/// Every path the change touched must be in the commit exactly as the agent
/// left it: absent if it deleted it, otherwise with the same file mode, and
/// either the very same content (blob) or, where another edit has since
/// changed the same file, with the agent's hunks still in place. That last
/// check reverse-applies the path's own patch to the commit's tree and
/// accepts it only if every hunk fits at its recorded line, with no offset or
/// fuzz (a line that merely exists elsewhere in the file is not the change),
/// and only if the patch no longer applies forwards. Fixes that share a file
/// but not its hunks are both proven; overlapping or discarded edits are not.
///
/// The check does not depend on the user's Git configuration (renames,
/// external diff drivers, whitespace settings are all turned off), and any
/// object this repository lost, empty change or Git failure answers `false`:
/// a missing proof is never read as a present one.
pub async fn effect_survives_in(working_dir: &str, before: &str, after: &str, commit: &str) -> bool {
    let git = |args: Vec<String>, index: Option<std::path::PathBuf>| async move {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(["--literal-pathspecs", "-c", "apply.ignoreWhitespace=false", "-c", "apply.whitespace=nowarn"])
            .args(&args)
            .current_dir(working_dir);
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd.output().await.ok()
    };
    let ok = |o: &Option<std::process::Output>| o.as_ref().is_some_and(|o| o.status.success());
    let strings = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();

    let raw = git(
        strings(&["diff-tree", "-r", "-z", "--raw", "--no-renames", "--no-ext-diff", "--no-color", "--full-index", before, after]),
        None,
    )
    .await;
    let Some(raw) = raw.filter(|o| o.status.success()) else { return false };
    // ":<old mode> <new mode> <old blob> <new blob> <status>\0<path>\0" ...
    // A path that is not valid UTF-8 cannot be handed back to Git by name, so
    // nothing about it can be checked: unproven, never a lossy guess.
    let Ok(text) = String::from_utf8(raw.stdout) else { return false };
    let mut parts = text.split('\0').filter(|p| !p.is_empty());
    let mut changes: Vec<(String, String, String, String)> = Vec::new(); // status, mode, blob, path
    while let (Some(meta), Some(path)) = (parts.next(), parts.next()) {
        let fields: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        if fields.len() != 5 {
            return false;
        }
        changes.push((fields[4].to_string(), fields[1].to_string(), fields[3].to_string(), path.to_string()));
    }
    if changes.is_empty() {
        return false;
    }

    let mut listing = strings(&["ls-tree", "-r", "-z", "--full-tree", commit, "--"]);
    listing.extend(changes.iter().map(|c| c.3.clone()));
    let listed = git(listing, None).await;
    let Some(listed) = listed.filter(|o| o.status.success()) else { return false };
    let Ok(listed) = String::from_utf8(listed.stdout) else { return false };
    let mut in_commit: std::collections::HashMap<&str, (&str, &str)> = Default::default();
    for entry in listed.split('\0').filter(|e| !e.is_empty()) {
        let Some((meta, path)) = entry.split_once('\t') else { return false };
        let mut f = meta.split(' ');
        let (Some(mode), Some(_kind), Some(blob)) = (f.next(), f.next(), f.next()) else { return false };
        in_commit.insert(path, (mode, blob));
    }

    let id = uuid::Uuid::new_v4();
    let index = std::env::temp_dir().join(format!("slashit-effect-{id}.index"));
    let mut needs_patch: Vec<&str> = Vec::new();
    for (status, mode, blob, path) in &changes {
        match (status.as_str(), in_commit.get(path.as_str())) {
            ("D", None) => {}
            ("D", Some(_)) => return false,
            (_, None) => return false,
            (_, Some((commit_mode, commit_blob))) => {
                if commit_mode != mode {
                    return false;
                }
                if commit_blob != blob {
                    needs_patch.push(path);
                }
            }
        }
    }

    let mut survives = true;
    if !needs_patch.is_empty() {
        survives = git(strings(&["read-tree", commit]), Some(index.clone())).await.as_ref().is_some_and(|o| o.status.success());
        for path in needs_patch {
            if !survives {
                break;
            }
            let diff = git(
                strings(&["diff-tree", "-p", "--binary", "--full-index", "--no-renames", "--no-ext-diff", "--no-color", before, after, "--", path]),
                None,
            )
            .await;
            let Some(diff) = diff.filter(|o| o.status.success() && !o.stdout.is_empty()) else {
                survives = false;
                break;
            };
            let patch = std::env::temp_dir().join(format!("slashit-effect-{id}.patch"));
            survives = tokio::fs::write(&patch, &diff.stdout).await.is_ok() && {
                let p = patch.display().to_string();
                let reverse = git(
                    vec!["apply".into(), "--cached".into(), "--reverse".into(), "--check".into(), "--verbose".into(), p.clone()],
                    Some(index.clone()),
                )
                .await;
                let in_place = ok(&reverse) && {
                    let said = String::from_utf8_lossy(&reverse.as_ref().unwrap().stderr).to_lowercase();
                    !said.contains("offset") && !said.contains("fuzz")
                };
                in_place && !ok(&git(
                    vec!["apply".into(), "--cached".into(), "--check".into(), p],
                    Some(index.clone()),
                )
                .await)
            };
            let _ = tokio::fs::remove_file(&patch).await;
        }
    }
    let _ = tokio::fs::remove_file(&index).await;
    survives
}

/// Where `origin` held a pull request's branch when it was last refreshed.
pub struct RemoteBranch {
    dir: std::path::PathBuf,
    tip: Result<String, String>,
}

impl RemoteBranch {
    /// Ask `origin` for `branch` now and record where it is. A branch that
    /// cannot be fetched (no network, no such branch, a refused name) leaves
    /// nothing proven: [`RemoteBranch::contains`] is then `false` for every
    /// commit, and [`RemoteBranch::unavailable`] says why. There is no
    /// fallback to a remote-tracking ref from an earlier run, which a
    /// force-push may have made stale.
    pub async fn refresh(working_dir: &str, branch: Option<&str>) -> Self {
        let dir = std::path::PathBuf::from(working_dir);
        let tip = match branch {
            None => Err("this task records no branch".to_string()),
            Some(branch) => match super::checked_task_branch(branch) {
                Ok(branch) => restack::fetch_remote_branch(&dir, branch).await,
                Err(e) => Err(e.to_string()),
            },
        };
        Self { dir, tip }
    }

    /// Why the branch could not be refreshed, if it could not.
    pub fn unavailable(&self) -> Option<&str> {
        self.tip.as_ref().err().map(String::as_str)
    }

    /// Whether `commit` is the remote branch's tip or one of its ancestors.
    /// Contains exactly this commit: an equal change under another commit ID
    /// is not it. A commit this repository does not have is not contained.
    pub async fn contains(&self, commit: &str) -> bool {
        let Ok(tip) = &self.tip else { return false };
        matches!(restack::is_ancestor(&self.dir, commit, tip).await, Ok(true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(dir).output().expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A checkout of branch `task` with one commit that `origin` (a bare
    /// repository beside it) already holds.
    fn checkout_with_origin() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let origin = tmp.path().join("origin.git");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(tmp.path(), &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(&work, &["init", "-q", "-b", "task"]);
        git(&work, &["config", "user.email", "t@example.com"]);
        git(&work, &["config", "user.name", "T"]);
        std::fs::write(work.join("a.txt"), "a\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "base"]);
        git(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&work, &["push", "-q", "-u", "origin", "task"]);
        (tmp, work)
    }

    #[tokio::test]
    async fn a_snapshot_changes_exactly_when_the_checkout_does() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        let clean = checkout_snapshot(dir).await.unwrap();
        assert_eq!(checkout_snapshot(dir).await.unwrap(), clean, "a snapshot is repeatable");

        std::fs::write(work.join("b.txt"), "new\n").unwrap();
        let with_new = checkout_snapshot(dir).await.unwrap();
        assert_ne!(with_new, clean, "an untracked file counts");
        std::fs::write(work.join("a.txt"), "edited\n").unwrap();
        assert_ne!(checkout_snapshot(dir).await.unwrap(), with_new, "an edit counts");
        std::fs::remove_file(work.join("b.txt")).unwrap();
        std::fs::write(work.join("a.txt"), "a\n").unwrap();
        assert_eq!(checkout_snapshot(dir).await.unwrap(), clean, "putting everything back is no change");

        assert_eq!(git(&work, &["status", "--porcelain"]), "", "the checkout's own index is untouched");
        assert_eq!(git(&work, &["diff", "--cached", "--name-only"]), "");
    }

    #[tokio::test]
    async fn the_remote_branch_contains_a_commit_it_was_pushed_but_not_one_it_never_saw() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        let pushed = git(&work, &["rev-parse", "HEAD"]);
        std::fs::write(work.join("fix.txt"), "fix\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "fix"]);
        let local_only = git(&work, &["rev-parse", "HEAD"]);

        let remote = RemoteBranch::refresh(dir, Some("task")).await;
        assert_eq!(remote.unavailable(), None);
        assert!(remote.contains(&pushed).await);
        assert!(!remote.contains(&local_only).await, "committed locally is not delivered");
        assert!(!remote.contains(&"0".repeat(40)).await, "an unknown commit is not contained");
        assert!(!remote.contains("not-an-object-id").await);

        git(&work, &["push", "-q", "origin", "task"]);
        assert!(RemoteBranch::refresh(dir, Some("task")).await.contains(&local_only).await);
    }

    #[tokio::test]
    async fn an_identical_change_under_another_commit_id_is_not_contained() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        let base = git(&work, &["rev-parse", "HEAD"]);
        std::fs::write(work.join("fix.txt"), "fix\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "fix"]);
        let fix = git(&work, &["rev-parse", "HEAD"]);
        let twin = git(&work, &["commit-tree", &format!("{fix}^{{tree}}"), "-p", &base, "-m", "twin"]);
        git(&work, &["push", "-q", "origin", &format!("{twin}:refs/heads/task")]);

        let remote = RemoteBranch::refresh(dir, Some("task")).await;
        assert!(remote.contains(&twin).await);
        assert!(!remote.contains(&fix).await, "the same tree and parent under another ID is another commit");
    }

    #[tokio::test]
    async fn a_force_push_that_drops_a_commit_is_seen_at_the_next_refresh() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        let base = git(&work, &["rev-parse", "HEAD"]);
        std::fs::write(work.join("fix.txt"), "fix\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "fix"]);
        let fix = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["push", "-q", "origin", "task"]);
        assert!(RemoteBranch::refresh(dir, Some("task")).await.contains(&fix).await);

        // Someone else rewrites the branch on the server.
        let origin = git(&work, &["remote", "get-url", "origin"]);
        git(&work, &["--git-dir", &origin, "update-ref", "refs/heads/task", &base]);
        // The tracking ref still says the fix is there until it is refreshed.
        assert_eq!(git(&work, &["rev-parse", "origin/task"]), fix);
        assert!(!RemoteBranch::refresh(dir, Some("task")).await.contains(&fix).await);
    }

    #[tokio::test]
    async fn a_remote_that_cannot_be_asked_proves_nothing_even_with_a_cached_tracking_ref() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        let pushed = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["remote", "set-url", "origin", "/nonexistent/remote.git"]);

        let remote = RemoteBranch::refresh(dir, Some("task")).await;
        assert!(remote.unavailable().is_some());
        assert!(!remote.contains(&pushed).await);
        assert!(RemoteBranch::refresh(dir, None).await.unavailable().is_some(), "no branch, no proof");
    }

    /// Snapshot trees before and after `change` runs in `work`, then put the
    /// checkout back as it was.
    async fn effect_of(work: &Path, change: impl FnOnce(&Path)) -> (String, String) {
        let dir = work.to_str().unwrap();
        let before = checkout_snapshot(dir).await.unwrap();
        change(work);
        let after = checkout_snapshot(dir).await.unwrap();
        git(work, &["reset", "-q", "--hard"]);
        git(work, &["clean", "-fdq"]);
        (before, after)
    }

    async fn survives(work: &Path, effect: &(String, String), commit: &str) -> bool {
        effect_survives_in(work.to_str().unwrap(), &effect.0, &effect.1, commit).await
    }

    /// A reverted change whose lines also exist, with identical context,
    /// elsewhere in the file is not "still there": the reverse patch fits only
    /// at an offset.
    #[tokio::test]
    async fn a_match_found_only_at_an_offset_elsewhere_is_not_survival() {
        let (_tmp, work) = checkout_with_origin();
        let pad = "pad\npad\npad\n";
        std::fs::write(work.join("f"), format!("{pad}q\nlog\nr\n{pad}mid\n{pad}q\nr\n{pad}")).unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "two blocks"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        let effect = effect_of(&work, |w| {
            std::fs::write(w.join("f"), format!("{pad}q\nlog\nr\n{pad}mid\n{pad}q\nlog\nr\n{pad}")).unwrap();
        })
        .await;
        assert!(!survives(&work, &effect, &head).await, "the change was reverted");
    }

    /// A change that only toggles a file's executable bit survives only if
    /// the commit has the mode.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_mode_only_change_survives_only_with_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, work) = checkout_with_origin();
        let head = git(&work, &["rev-parse", "HEAD"]);
        let effect = effect_of(&work, |w| {
            std::fs::set_permissions(w.join("a.txt"), std::fs::Permissions::from_mode(0o755)).unwrap();
        })
        .await;
        assert!(!survives(&work, &effect, &head).await, "the mode was never committed");

        std::fs::set_permissions(work.join("a.txt"), std::fs::Permissions::from_mode(0o755)).unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "mode"]);
        assert!(survives(&work, &effect, &git(&work, &["rev-parse", "HEAD"])).await);
    }

    /// A content change that also changes the mode is credited only if both
    /// are in the commit.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_content_change_carrying_a_mode_change_needs_both() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, work) = checkout_with_origin();
        let effect = effect_of(&work, |w| {
            std::fs::write(w.join("a.txt"), "a\nmore\n").unwrap();
            std::fs::set_permissions(w.join("a.txt"), std::fs::Permissions::from_mode(0o755)).unwrap();
        })
        .await;
        std::fs::write(work.join("a.txt"), "a\nmore\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "content only"]);
        assert!(!survives(&work, &effect, &git(&work, &["rev-parse", "HEAD"])).await);
    }

    /// The user's own Git configuration cannot loosen the check: a change that
    /// only collapsed whitespace is not found by ignoring whitespace.
    #[tokio::test]
    async fn user_whitespace_settings_do_not_loosen_the_proof() {
        let (_tmp, work) = checkout_with_origin();
        std::fs::write(work.join("w.txt"), "a  b\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "spaced"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        let effect = effect_of(&work, |w| std::fs::write(w.join("w.txt"), "a b\n").unwrap()).await;
        git(&work, &["config", "apply.ignoreWhitespace", "change"]);
        git(&work, &["config", "apply.whitespace", "fix"]);
        assert!(!survives(&work, &effect, &head).await, "the collapsed spacing was never committed");
    }

    /// A path that is not valid UTF-8 cannot be matched by name, so a change to
    /// it is never proven: a reverted deletion of such a file is not survival.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_path_that_is_not_utf8_is_never_proven() {
        use std::os::unix::ffi::OsStrExt;
        let (_tmp, work) = checkout_with_origin();
        let odd = work.join(std::ffi::OsStr::from_bytes(b"bad\xffname"));
        std::fs::write(&odd, "x\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "odd name"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        let effect = effect_of(&work, |w| {
            std::fs::remove_file(w.join(std::ffi::OsStr::from_bytes(b"bad\xffname"))).unwrap();
        })
        .await;
        assert!(!survives(&work, &effect, &head).await, "the file is still in the commit");
    }
}
