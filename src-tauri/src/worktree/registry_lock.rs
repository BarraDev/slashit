//! One `git` at a time on a repository's worktree registrations.
//!
//! `git worktree add` registers a checkout by making
//! `<common dir>/worktrees/<id>/` and then writing `gitdir`, `commondir` and
//! `HEAD` into it, one file after another, and `git worktree remove` takes
//! the entry apart the same way. While that happens the entry is incomplete,
//! and any `git` that walks the registrations can read it and die with
//! `fatal: failed to read .git/worktrees/<id>/commondir` ("No such file or
//! directory", or "Success" when it catches the file created but not yet
//! written) or `Invalid path .../worktrees/<id>`. Git takes no lock against
//! this, so two tasks of one project started together can fail on it
//! without anything of SlashIt's being at fault.
//!
//! So SlashIt's own `git` runs that walk the registrations go through
//! [`run_git`] and its variants, and run one at a time per repository,
//! whichever [`super::WorktreeManager`] (or other caller) starts them. They
//! are `worktree add`, `list` and `remove`; the ref walks `for-each-ref` and
//! `rev-list --all` (`WorktreeManager`, the orphan scan, the version control
//! probes in `vcs`, the default base); and every `git` the restack runs
//! except `fetch` (a rebase checks where a branch is checked out; a fetch
//! never does, and holding the lock across the network would stall creating
//! worktrees). Runs that read one ref or one object (`rev-parse`,
//! `update-ref`, `cat-file`, `symbolic-ref`, `show-ref`, `merge-base`,
//! `rev-list <range>`), or only the files of one checkout (`add`, `commit`,
//! `diff`, `status`, `ls-files`), do not walk the registrations and are not
//! behind the lock.
//!
//! The lock is process-wide and keyed by the repository's git common
//! directory, not by manager and not global: runs on different repositories
//! stay concurrent. It is held for exactly one `git` process, taken and
//! released inside one blocking call, so it is never held across an `await`
//! or other work, and one run never starts another while holding it.
//!
//! What this cannot cover: a `git` started by another process (the `slashit`
//! CLI, `slashitd` beside the app, a hook, a person's terminal) is not
//! behind this lock, so the race with those is reduced to what Git itself
//! allows, not removed.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

fn locks() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(Mutex::default)
}

/// The directory that holds `worktrees/` for the repository `repo_path` is
/// in: the same for the primary checkout, a linked worktree and a bare
/// repository. Read from the file system, so no `git` is run to find it.
/// A path that is in no repository is its own key, and `git` refuses it
/// itself.
fn registry_key(repo_path: &Path) -> PathBuf {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let start = canonical(repo_path);
    for dir in start.ancestors() {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return canonical(&dot_git);
        }
        if let Ok(link) = std::fs::read_to_string(&dot_git) {
            if let Some(target) = link.trim().strip_prefix("gitdir:") {
                let git_dir = dir.join(target.trim());
                return match std::fs::read_to_string(git_dir.join("commondir")) {
                    Ok(common) => canonical(&git_dir.join(common.trim())),
                    Err(_) => canonical(&git_dir),
                };
            }
        }
        if dir.join("HEAD").is_file() && dir.join("objects").is_dir() {
            return dir.to_path_buf();
        }
    }
    start
}

/// What a `git` run under the lock is given on its standard input.
pub(super) enum Input {
    /// Nothing: standard input is closed.
    None,
    /// These bytes, then end of input.
    Bytes(Vec<u8>),
}

/// A `git` run's result, and, for [`Input::Bytes`], whether all of the input
/// reached it (`git` can exit before it reads it).
pub(super) struct Ran {
    pub output: Output,
    pub input_written: std::io::Result<()>,
}

/// Run `program args` in `repo_path` while holding the repository's
/// registration lock. Blocks the calling thread; async callers use
/// [`run_git`].
pub(super) fn run_git_blocking(
    program: &OsString,
    repo_path: &str,
    args: &[&str],
    input: Input,
) -> std::io::Result<Ran> {
    run_git_blocking_with(program, repo_path, args, &[], input)
}

fn run_git_blocking_with(
    program: &OsString,
    repo_path: &str,
    args: &[&str],
    envs: &[(String, String)],
    input: Input,
) -> std::io::Result<Ran> {
    use std::process::Stdio;
    let lock = {
        let mut held = locks().lock().unwrap_or_else(PoisonError::into_inner);
        held.entry(registry_key(Path::new(repo_path))).or_default().clone()
    };
    // The lock guards no data, so a holder that panicked leaves nothing
    // half-written behind it.
    let _one_at_a_time = lock.lock().unwrap_or_else(PoisonError::into_inner);
    let mut command = std::process::Command::new(program);
    command.args(args).envs(envs.iter().map(|(k, v)| (k, v))).current_dir(repo_path);
    // Standard input is closed unless given: `output()` does that for us.
    match input {
        Input::None => Ok(Ran { output: command.output()?, input_written: Ok(()) }),
        Input::Bytes(bytes) => {
            use std::io::Write;
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let mut stdin = child.stdin.take().expect("stdin was piped");
            // Written from its own thread, so a large input and a large
            // answer cannot wait on each other.
            let writer = std::thread::spawn(move || stdin.write_all(&bytes));
            let output = child.wait_with_output()?;
            let input_written = writer
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("the thread writing to git panicked")));
            Ok(Ran { output, input_written })
        }
    }
}

/// [`run_git_blocking`] on a thread meant for blocking, so an async task
/// waiting for its turn never holds a runtime worker.
pub(super) async fn run_git(
    program: OsString,
    repo_path: &str,
    args: &[&str],
    input: Input,
) -> std::io::Result<Ran> {
    run_git_with(program, repo_path, args, &[], input).await
}

/// [`run_git`] with environment variables set for the `git` run.
pub(super) async fn run_git_with(
    program: OsString,
    repo_path: &str,
    args: &[&str],
    envs: &[(&str, String)],
    input: Input,
) -> std::io::Result<Ran> {
    let repo_path = repo_path.to_string();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let envs: Vec<(String, String)> = envs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git_blocking_with(&program, &repo_path, &args, &envs, input)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// [`run_git`] for the many callers that give no input and want only the
/// output.
pub(super) async fn output(program: OsString, repo_path: &str, args: &[&str]) -> std::io::Result<Output> {
    run_git(program, repo_path, args, Input::None).await.map(|ran| ran.output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linked_worktree_and_its_primary_checkout_share_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(&repo).output().unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["-c", "user.email=a@b", "-c", "user.name=n", "commit", "-q", "--allow-empty", "-m", "i"]);
        let linked = dir.path().join("linked");
        git(&["worktree", "add", "-q", "-b", "other", "--", linked.to_str().unwrap()]);
        std::fs::create_dir_all(repo.join("sub")).unwrap();

        let primary = registry_key(&repo);
        assert_eq!(primary, registry_key(&repo.join("sub")));
        assert_eq!(primary, registry_key(&linked));
        assert_ne!(primary, registry_key(dir.path()));
    }
}
