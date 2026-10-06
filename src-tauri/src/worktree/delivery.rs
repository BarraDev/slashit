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
                .env("LC_ALL", "C")
                .env("LANGUAGE", "C")
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

/// What [`effect_survives_in`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Survival {
    /// The change is in the commit, where it was made.
    Present,
    /// The change is not in the commit, or can never be shown to be: an edit
    /// removed or moved it, a path cannot be named, or the trees that record
    /// the change no longer exist. The evidence is of no further use.
    Absent,
    /// Git could not be asked (it would not start, or failed on objects that
    /// exist). Nothing is known; the evidence must be kept.
    Unknown,
}

/// Whether the change between the trees `before` and `after` (what one fix
/// agent did) is still in `commit`, path by path.
///
/// Every path the change touched must be in the commit exactly as the agent
/// left it: absent if it deleted it, otherwise with the same file mode, and
/// either the very same content (blob) or, where another edit has since
/// changed the same file, with the agent's hunks still in place. That last
/// check needs three things of the path's own patch: it no longer applies
/// forwards to the commit; it reverse-applies to the commit; and the change
/// that reverse-applying leaves to be redone, diffed against the commit, has
/// the very hunk positions the agent's change had. A hunk that only fits
/// somewhere else in the file (an offset) therefore does not count. The
/// positions are compared from the diffs themselves, never from what Git
/// prints in words, so the user's language cannot loosen the rule. Fixes that
/// share a file but not its hunks are both present; overlapping, discarded or
/// shifted edits are not, a false negative being acceptable and a false
/// positive not.
///
/// Git runs with the C locale, with the user's diff, rename and whitespace
/// settings turned off. A path that is not valid UTF-8, or trees that are gone,
/// are [`Survival::Absent`]; a Git that cannot be run is [`Survival::Unknown`].
pub async fn effect_survives_in(working_dir: &str, before: &str, after: &str, commit: &str) -> Survival {
    let git = |args: Vec<String>, index: Option<std::path::PathBuf>| async move {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(["--literal-pathspecs", "-c", "apply.ignoreWhitespace=false", "-c", "apply.whitespace=nowarn"])
            .args(&args)
            .env("LC_ALL", "C")
            .env("LANGUAGE", "C")
            .current_dir(working_dir);
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd.output().await.ok()
    };
    let strings = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    let succeeded = |o: &Option<std::process::Output>| o.as_ref().is_some_and(|o| o.status.success());
    // Hunk positions of a patch: its `@@ -a,b +c,d @@` markers.
    let hunk_markers = |patch: &[u8]| -> Vec<String> {
        String::from_utf8_lossy(patch)
            .lines()
            .filter(|l| l.starts_with("@@ -"))
            .map(|l| l.split(" @@").next().unwrap_or(l).to_string())
            .collect()
    };

    // Objects that exist but could not be read are unknown; objects that are
    // gone are not coming back.
    for (object, kind) in [(before, "tree"), (after, "tree"), (commit, "commit")] {
        let probe = git(strings(&["cat-file", "-e", &format!("{object}^{{{kind}}}")]), None).await;
        match probe {
            None => return Survival::Unknown,
            Some(o) if !o.status.success() => return Survival::Absent,
            Some(_) => {}
        }
    }

    let raw = git(
        strings(&["diff-tree", "-r", "-z", "--raw", "--no-renames", "--no-ext-diff", "--no-color", "--full-index", before, after]),
        None,
    )
    .await;
    let Some(raw) = raw.filter(|o| o.status.success()) else { return Survival::Unknown };
    // A path that is not valid UTF-8 cannot be handed back to Git by name, so
    // nothing about it can be checked: never proven, never a lossy guess.
    let Ok(text) = String::from_utf8(raw.stdout) else { return Survival::Absent };
    // ":<old mode> <new mode> <old blob> <new blob> <status>\0<path>\0" ...
    let mut parts = text.split('\0').filter(|p| !p.is_empty());
    let mut changes: Vec<(String, String, String, String)> = Vec::new(); // status, mode, blob, path
    while let (Some(meta), Some(path)) = (parts.next(), parts.next()) {
        let fields: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        if fields.len() != 5 {
            return Survival::Unknown;
        }
        changes.push((fields[4].to_string(), fields[1].to_string(), fields[3].to_string(), path.to_string()));
    }
    if changes.is_empty() {
        return Survival::Absent;
    }

    let mut listing = strings(&["ls-tree", "-r", "-z", "--full-tree", commit, "--"]);
    listing.extend(changes.iter().map(|c| c.3.clone()));
    let listed = git(listing, None).await;
    let Some(listed) = listed.filter(|o| o.status.success()) else { return Survival::Unknown };
    let Ok(listed) = String::from_utf8(listed.stdout) else { return Survival::Absent };
    let mut in_commit: std::collections::HashMap<&str, (&str, &str)> = Default::default();
    for entry in listed.split('\0').filter(|e| !e.is_empty()) {
        let Some((meta, path)) = entry.split_once('\t') else { return Survival::Unknown };
        let mut f = meta.split(' ');
        let (Some(mode), Some(_kind), Some(blob)) = (f.next(), f.next(), f.next()) else { return Survival::Unknown };
        in_commit.insert(path, (mode, blob));
    }

    let mut needs_patch: Vec<&str> = Vec::new();
    for (status, mode, blob, path) in &changes {
        match (status.as_str(), in_commit.get(path.as_str())) {
            ("D", None) => {}
            ("D", Some(_)) => return Survival::Absent,
            (_, None) => return Survival::Absent,
            (_, Some((commit_mode, commit_blob))) => {
                if commit_mode != mode {
                    return Survival::Absent;
                }
                if commit_blob != blob {
                    needs_patch.push(path);
                }
            }
        }
    }
    if needs_patch.is_empty() {
        return Survival::Present;
    }

    let id = uuid::Uuid::new_v4();
    let index = std::env::temp_dir().join(format!("slashit-effect-{id}.index"));
    let patch_file = std::env::temp_dir().join(format!("slashit-effect-{id}.patch"));
    let verdict = async {
        if !succeeded(&git(strings(&["read-tree", commit]), Some(index.clone())).await) {
            return Survival::Unknown;
        }
        for path in needs_patch {
            let diff = git(
                strings(&["diff-tree", "-p", "--binary", "--full-index", "--no-renames", "--no-ext-diff", "--no-color", before, after, "--", path]),
                None,
            )
            .await;
            let Some(diff) = diff.filter(|o| o.status.success()) else { return Survival::Unknown };
            if diff.stdout.is_empty() || tokio::fs::write(&patch_file, &diff.stdout).await.is_err() {
                return Survival::Unknown;
            }
            let p = patch_file.display().to_string();
            let apply = |reverse: bool, check: bool| {
                let mut a = vec!["apply".to_string(), "--cached".to_string()];
                if reverse {
                    a.push("--reverse".into());
                }
                if check {
                    a.push("--check".into());
                }
                a.push(p.clone());
                git(a, Some(index.clone()))
            };
            // Still applicable forwards: the change is not there.
            let forward = apply(false, true).await;
            if forward.is_none() {
                return Survival::Unknown;
            }
            if succeeded(&forward) {
                return Survival::Absent;
            }
            // Take the change out of a scratch copy of the commit's tree.
            // Git exits non-zero when it does not fit at all.
            let reversed = apply(true, false).await;
            if reversed.is_none() {
                return Survival::Unknown;
            }
            if !succeeded(&reversed) {
                return Survival::Absent;
            }
            let Some(tree) = git(strings(&["write-tree"]), Some(index.clone())).await.filter(|o| o.status.success()) else {
                return Survival::Unknown;
            };
            let reverted = String::from_utf8_lossy(&tree.stdout).trim().to_string();
            // Putting it back must be a change at the same places the agent
            // made it: a hunk that only fit elsewhere moves them.
            let redo = git(
                strings(&["diff-tree", "-p", "--binary", "--full-index", "--no-renames", "--no-ext-diff", "--no-color", &reverted, commit, "--", path]),
                None,
            )
            .await;
            let Some(redo) = redo.filter(|o| o.status.success()) else { return Survival::Unknown };
            if hunk_markers(&redo.stdout) != hunk_markers(&diff.stdout) {
                return Survival::Absent;
            }
            // Next path starts from the commit again.
            if !succeeded(&git(strings(&["read-tree", commit]), Some(index.clone())).await) {
                return Survival::Unknown;
            }
        }
        Survival::Present
    }
    .await;
    let _ = tokio::fs::remove_file(&index).await;
    let _ = tokio::fs::remove_file(&patch_file).await;
    verdict
}

/// Where `origin` held a pull request's branch when it was last refreshed.
pub struct RemoteBranch {
    dir: std::path::PathBuf,
    tip: Result<String, String>,
    absent: bool,
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
        let (tip, absent) = match branch {
            None => (Err("this task records no branch".to_string()), false),
            Some(branch) => match super::checked_task_branch(branch) {
                Ok(branch) => match restack::remote_branch_exists(&dir, branch).await {
                    Ok(false) => (Err(format!("origin has no branch {branch}")), true),
                    Ok(true) => (restack::fetch_remote_branch(&dir, branch).await, false),
                    Err(e) => (Err(e), false),
                },
                Err(e) => (Err(e.to_string()), false),
            },
        };
        Self { dir, tip, absent }
    }

    /// The commit the branch was at when refreshed, if it could be.
    pub fn tip(&self) -> Option<&str> {
        self.tip.as_deref().ok()
    }

    /// Why the branch could not be refreshed, if it could not.
    pub fn unavailable(&self) -> Option<&str> {
        self.tip.as_ref().err().map(String::as_str)
    }

    /// Whether the remote was successfully queried and definitely has no
    /// branch of this name. A failed query is not absence.
    pub fn is_absent(&self) -> bool {
        self.absent
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
        effect_survives_in(work.to_str().unwrap(), &effect.0, &effect.1, commit).await == Survival::Present
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

    /// Two identical blocks, the second already holding a `log` line. The agent
    /// adds `log` to block 1, and a later commit rewrites block 1. The reverse
    /// patch then fits only at block 2, at an offset, and the forward patch
    /// does not apply either, so only the in-place rule rejects it. It must
    /// do so whatever language the user's Git speaks: the environment here asks
    /// for German, which changes the words Git prints (the proof must not
    /// depend on them).
    #[tokio::test]
    async fn the_in_place_rule_holds_under_a_translated_git() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        let pad = "pad\npad\npad\n";
        let file = |first: &str| format!("{pad}{first}{pad}mid\n{pad}q\nlog\nr\n{pad}");
        std::fs::write(work.join("f"), file("q\nr\n")).unwrap();
        let before = checkout_snapshot(dir).await.unwrap();
        std::fs::write(work.join("f"), file("q\nlog\nr\n")).unwrap();
        let after = checkout_snapshot(dir).await.unwrap();
        // A later commit rewrites block 1.
        std::fs::write(work.join("f"), file("q\nX\nr\n")).unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "block one rewritten"]);
        let head = git(&work, &["rev-parse", "HEAD"]);

        let saved: Vec<_> = ["LC_ALL", "LANGUAGE"].iter().map(|k| (*k, std::env::var(k).ok())).collect();
        // Safety: only this test touches these two variables.
        unsafe {
            std::env::set_var("LC_ALL", "de_DE.UTF-8");
            std::env::set_var("LANGUAGE", "de");
        }
        let verdict = effect_survives_in(dir, &before, &after, &head).await;
        for (k, v) in saved {
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        assert_eq!(verdict, Survival::Absent, "the change only fits at block 2, not where it was made");
    }

    /// Reverse and forward both fit at the recorded place only for a patch
    /// that is ambiguous about where it applies: the forward check is what
    /// refuses it. (Found by search: the effect appends `b` to `a b b b b b`;
    /// the commit has a different `b b a b b b b`.)
    #[tokio::test]
    async fn a_change_the_commit_could_still_take_is_not_present() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        let lines = |l: &str| l.split(' ').map(|c| format!("{c}\n")).collect::<String>();
        std::fs::write(work.join("f"), lines("a b b b b b")).unwrap();
        let before = checkout_snapshot(dir).await.unwrap();
        std::fs::write(work.join("f"), lines("a b b b b b b")).unwrap();
        let after = checkout_snapshot(dir).await.unwrap();
        std::fs::write(work.join("f"), lines("b b a b b b b")).unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "other"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        assert_ne!(effect_survives_in(dir, &before, &after, &head).await, Survival::Present);
    }

    /// A file the agent deleted that the commit still has is not a deletion
    /// that survived.
    #[tokio::test]
    async fn a_deletion_the_commit_does_not_have_is_not_present() {
        let (_tmp, work) = checkout_with_origin();
        let head = git(&work, &["rev-parse", "HEAD"]);
        let effect = effect_of(&work, |w| std::fs::remove_file(w.join("a.txt")).unwrap()).await;
        assert!(!survives(&work, &effect, &head).await, "a.txt is still in the commit");
        git(&work, &["rm", "-q", "a.txt"]);
        git(&work, &["commit", "-q", "-m", "deleted"]);
        assert!(survives(&work, &effect, &git(&work, &["rev-parse", "HEAD"])).await);
    }

    /// Git that cannot be run says nothing: the verdict is Unknown, which
    /// callers must treat as no proof and no loss of evidence. Trees that no
    /// longer exist, by contrast, are Absent.
    #[tokio::test]
    async fn a_git_that_cannot_run_is_unknown_and_missing_trees_are_absent() {
        let (_tmp, work) = checkout_with_origin();
        let head = git(&work, &["rev-parse", "HEAD"]);
        let tree = git(&work, &["rev-parse", "HEAD^{tree}"]);
        let nowhere = work.join("no-such-directory");
        assert_eq!(
            effect_survives_in(nowhere.to_str().unwrap(), &tree, &tree, &head).await,
            Survival::Unknown
        );
        let gone = "1".repeat(40);
        assert_eq!(
            effect_survives_in(work.to_str().unwrap(), &gone, &tree, &head).await,
            Survival::Absent
        );
    }

    /// A binary file has no hunks to compare, so only the reverse patch can
    /// tell that the commit's bytes are neither the agent's nor the original.
    #[tokio::test]
    async fn a_binary_file_changed_again_is_not_present() {
        let (_tmp, work) = checkout_with_origin();
        let dir = work.to_str().unwrap();
        std::fs::write(work.join("b.bin"), b"A\0A\0A\0").unwrap();
        let before = checkout_snapshot(dir).await.unwrap();
        std::fs::write(work.join("b.bin"), b"B\0B\0B\0").unwrap();
        let after = checkout_snapshot(dir).await.unwrap();
        std::fs::write(work.join("b.bin"), b"C\0C\0C\0").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "binary, but another one"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        assert_ne!(effect_survives_in(dir, &before, &after, &head).await, Survival::Present);
    }
}
