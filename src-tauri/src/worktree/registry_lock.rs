//! One `git` at a time on a repository's worktree registrations.
//!
//! `git worktree add` registers a checkout by making
//! `<common dir>/worktrees/<id>/` and then writing `gitdir`, `commondir` and
//! `HEAD` into it, one file after another. For that moment the entry exists
//! and is incomplete. Every `git` that walks the registrations (another
//! `worktree add`, `worktree list`, ref walks such as `for-each-ref`, and
//! `worktree remove` taking an entry apart) can read it, and dies with `fatal: failed to read
//! .git/worktrees/<id>/commondir`, with either "No such file or directory"
//! or, when it catches the file created but not yet written, "Success"
//! (and `Invalid path .../worktrees/<id>` when it catches an entry being
//! removed).
//! Git takes no lock to prevent this, so two tasks of one project starting
//! at the same time can fail on it without anything of SlashIt's being at
//! fault.
//!
//! So SlashIt's own `git worktree add`, `list` and `remove` for one
//! repository run one at a time, whichever [`super::WorktreeManager`] starts
//! them: the lock is process-wide and keyed by the repository's git common
//! directory, not by manager and not global. Runs on different repositories
//! stay concurrent, and nothing but the one `git` process is held under the
//! lock, so it is never held across other work and never nested.
//!
//! What this cannot cover: a `git` started by another process (the `slashit`
//! CLI, `slashitd` beside the app, a person's terminal) is not behind this
//! lock.

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
    use std::process::Stdio;
    let lock = {
        let mut held = locks().lock().unwrap_or_else(PoisonError::into_inner);
        held.entry(registry_key(Path::new(repo_path))).or_default().clone()
    };
    // The lock guards no data, so a holder that panicked leaves nothing
    // half-written behind it.
    let _one_at_a_time = lock.lock().unwrap_or_else(PoisonError::into_inner);
    let mut command = std::process::Command::new(program);
    command.args(args).current_dir(repo_path);
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
    let repo_path = repo_path.to_string();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git_blocking(&program, &repo_path, &args, input)
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
