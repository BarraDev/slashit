//! Putting a project folder under version control, on explicit request only.
//!
//! A Task Checkout is a Git worktree on a branch created at a commit, so a
//! folder needs a repository *and* a commit before any task can run there;
//! `git init` alone is not enough for a folder that already holds files.
//! [`initialize`] therefore does both: it creates the repository and records
//! the folder's current files, with the repository's ignore rules applied,
//! as its first commit (an empty first commit for an empty folder). A Git
//! repository that exists but has no commit yet only gets the commit.
//!
//! [`preview`] says beforehand, without writing anything, which files that
//! commit would hold and whether a commit identity is configured, so the
//! person asking sees what will happen. Nothing here runs on its own: not
//! when a folder is opened, and never when a task is started.
//!
//! The branch name is never chosen here. `git init` runs without
//! `--initial-branch`, and the branch it made is read back from its
//! symbolic `HEAD`, so whatever `init.defaultBranch` (or Git's own fallback)
//! says is what the project uses. `jj git init` creates the colocated Git
//! repository the same way; SlashIt reads that unborn branch and names the
//! first commit's bookmark after it, since Jujutsu itself creates no
//! bookmark. `--colocate` is passed explicitly, whatever `git.colocate`
//! says: Task Checkouts are Git worktrees added from the project root, which
//! a non-colocated repository does not have (see [`super::vcs`]).
//!
//! Nothing is recorded that the preview did not account for: a folder
//! holding another Git repository is refused (Git would record only an empty
//! reference to it, Jujutsu would skip it), Jujutsu initialization is
//! refused when a file exceeds the person's own
//! `snapshot.max-new-file-size`, and a folder whose file count changed since
//! it was shown is refused.
//!
//! No identity is ever invented: a commit needs `user.name` and
//! `user.email`, and when either is missing nothing is changed and the
//! message says how to set them.

use super::vcs::{self, Head, Vcs};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How many file names a preview lists.
const SAMPLE: usize = 20;
/// The message of the first commit of a folder that holds files.
const SNAPSHOT_MESSAGE: &str = "Initial snapshot";
/// The message of the first commit of an empty folder.
const EMPTY_MESSAGE: &str = "Initial commit";

/// Which version control to initialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VcsInitKind {
    Git,
    Jujutsu,
}

/// What [`initialize`] would do in a folder, and whether it can.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InitPreview {
    /// The folder this preview describes. What was shown for one folder
    /// says nothing about another.
    pub path: String,
    pub vcs: Vcs,
    /// What initializing would do here, in words; `None` when it cannot.
    pub action: Option<String>,
    /// Why it cannot, with either tool, when it cannot.
    pub blocked: Option<String>,
    /// How many files the first commit would hold.
    pub files: usize,
    /// The first of those, by path.
    pub sample: Vec<String>,
    /// Why Git cannot commit here (no identity), if it cannot.
    pub git_identity_missing: Option<String>,
    /// Whether `jj` can be run at all.
    pub jj_available: bool,
    /// Why Jujutsu cannot record the first commit here (no identity, or
    /// files above its size limit for new files), if it cannot.
    pub jj_blocked: Option<String>,
}

/// The first commit [`initialize`] made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InitOutcome {
    /// The branch (bookmark, for Jujutsu) the commit is on: the one the
    /// tool itself chose.
    pub branch: String,
    pub commit: String,
    pub files: usize,
}

/// What the first commit of a folder would hold.
struct Candidates {
    /// Files the ignore rules keep.
    files: Vec<String>,
    /// Directories holding a Git repository of their own, which the ignore
    /// rules keep. `git add` would record each as an empty gitlink and JJ
    /// would leave it out, so their contents would be in neither.
    nested: Vec<String>,
}

impl Candidates {
    fn split(listed: Vec<String>) -> Self {
        // `ls-files --others` lists a nested repository as its directory,
        // with a trailing slash, rather than the files inside it.
        let (nested, files) = listed.into_iter().partition(|entry| entry.ends_with('/'));
        Candidates { files, nested }
    }

    /// Why nothing may be recorded, if something nested would be lost.
    fn nested_refusal(&self, path: &str) -> Option<String> {
        (!self.nested.is_empty()).then(|| {
            format!(
                "{path} contains another Git repository ({}). Its files would not be recorded \
                 in the first commit: Git would store only a reference to it, and Jujutsu would \
                 leave it out. Move it out of the folder or add it to .gitignore, then try again. \
                 Nothing was changed.",
                self.nested.join(", ")
            )
        })
    }
}

/// Say what [`initialize`] would do in `path`, writing nothing.
pub async fn preview(path: &str) -> Result<InitPreview, String> {
    let dir = Path::new(path);
    let vcs = vcs::detect(dir).await?;
    let jj_available = vcs::jj_available().await;
    let mut preview = InitPreview {
        path: path.to_string(),
        vcs: vcs.clone(),
        action: None,
        blocked: None,
        files: 0,
        sample: Vec::new(),
        git_identity_missing: None,
        jj_available,
        jj_blocked: None,
    };
    let candidates = match candidates(dir, &vcs).await? {
        Ok(candidates) => candidates,
        Err(blocked) => {
            preview.blocked = Some(blocked);
            return Ok(preview);
        }
    };
    let files = &candidates.files;
    preview.files = files.len();
    preview.sample = files.iter().take(SAMPLE).cloned().collect();
    if let Some(refused) = candidates.nested_refusal(path) {
        preview.blocked = Some(refused);
        return Ok(preview);
    }
    preview.git_identity_missing = git_identity_missing(dir).await;
    if jj_available && vcs == Vcs::None {
        preview.jj_blocked = match jj_identity_missing(dir).await {
            Some(missing) => Some(missing),
            None => jj_oversized(dir, files).await.err(),
        };
    }
    let commit = if files.is_empty() {
        "an empty first commit, since the folder has no files to record".to_string()
    } else {
        format!(
            "a first commit holding the folder's {} current file(s); files its ignore rules \
             (.gitignore) exclude are left out and stay on disk",
            files.len()
        )
    };
    preview.action = Some(if vcs == Vcs::None {
        format!(
            "Create a repository in {path} (Git's own default branch name is used) and {commit}. \
             Nothing is pushed and no remote is added."
        )
    } else {
        format!("The Git repository in {path} has no commits yet. Create {commit}.")
    });
    Ok(preview)
}

/// What a first commit in `dir` would hold, or why there is none to make.
async fn candidates(dir: &Path, vcs: &Vcs) -> Result<Result<Candidates, String>, String> {
    let path = dir.to_string_lossy();
    Ok(Ok(Candidates::split(match vcs {
        Vcs::None => untracked_in_plain_folder(dir).await?,
        Vcs::Git if !vcs::has_commits(dir).await => files_to_commit(dir).await?,
        Vcs::Git | Vcs::JjColocated => {
            return Ok(Err(format!(
                "{path} already has version control with commits."
            )))
        }
        other => return Ok(Err(other.refusal(&path).unwrap_or_default())),
    })))
}

/// Put `path` under `kind` version control with a first commit, or give
/// the repository that exists there, with no commit yet, its first commit.
/// See the module documentation.
///
/// `expected_files` is how many files the preview the person saw counted.
/// When the folder no longer holds that many, nothing is changed: what
/// would be recorded is not what was shown.
pub async fn initialize(
    path: &str,
    kind: VcsInitKind,
    expected_files: Option<usize>,
) -> Result<InitOutcome, String> {
    let dir = Path::new(path);
    let vcs = vcs::detect(dir).await?;
    match (&vcs, kind) {
        (Vcs::None, _) => {}
        (Vcs::Git, VcsInitKind::Git) if !vcs::has_commits(dir).await => {}
        (Vcs::Git, VcsInitKind::Jujutsu) if !vcs::has_commits(dir).await => {
            return Err(format!(
                "{path} is already a Git repository with no commits. Initialize it with Git, or \
                 run `jj git init --colocate` there yourself."
            ))
        }
        (Vcs::Git | Vcs::JjColocated, _) => {
            return Err(format!(
                "{path} already has version control with commits; nothing was changed."
            ))
        }
        (other, _) => return Err(other.refusal(path).unwrap_or_default()),
    }
    let candidates = candidates(dir, &vcs).await??;
    if let Some(refused) = candidates.nested_refusal(path) {
        return Err(refused);
    }
    if let Some(expected) = expected_files {
        if expected != candidates.files.len() {
            return Err(format!(
                "{path} now holds {} file(s) to record, not the {expected} shown, so nothing was \
                 changed. Check the folder again before initializing.",
                candidates.files.len()
            ));
        }
    }
    match kind {
        VcsInitKind::Git => initialize_git(dir, path, vcs == Vcs::None).await,
        VcsInitKind::Jujutsu => initialize_jj(dir, path, &candidates.files).await,
    }
}

async fn initialize_git(dir: &Path, path: &str, create: bool) -> Result<InitOutcome, String> {
    if let Some(missing) = git_identity_missing(dir).await {
        return Err(missing);
    }
    if create {
        // No `--initial-branch`: the branch is Git's to name.
        run(dir, "git", &["init", "-q"], &[]).await?;
        if vcs::detect(dir).await? != Vcs::Git {
            return Err(format!(
                "git init reported success, but {path} is not a Git repository root."
            ));
        }
    }
    let branch = match vcs::head(dir).await {
        Head::Unborn { branch } => branch,
        other => {
            return Err(format!(
                "The new repository's HEAD is {other:?}, not a branch waiting for its first \
                 commit, so SlashIt did not commit anything."
            ))
        }
    };
    super::checked_base_branch(&branch)?;
    run(dir, "git", &["add", "-A"], &[]).await?;
    let files = vcs::git_stdout(dir, &["ls-files"])
        .await?
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    let message = if files == 0 {
        EMPTY_MESSAGE
    } else {
        SNAPSHOT_MESSAGE
    };
    run(
        dir,
        "git",
        &["commit", "-q", "--allow-empty", "-m", message],
        &[],
    )
    .await?;
    let commit = committed_on(dir, &branch).await?;
    Ok(InitOutcome {
        branch,
        commit,
        files,
    })
}

async fn initialize_jj(dir: &Path, path: &str, files: &[String]) -> Result<InitOutcome, String> {
    if !vcs::jj_available().await {
        return Err("Jujutsu (`jj`) is not installed or cannot be run.".to_string());
    }
    if let Some(missing) = jj_identity_missing(dir).await {
        return Err(missing);
    }
    // JJ leaves new files above its `snapshot.max-new-file-size` out of the
    // snapshot with only a warning. The person's own limit is respected:
    // Jujutsu initialization is refused instead of recording less than it
    // said it would.
    jj_oversized(dir, files).await?;
    let jj = |args: &[&str]| {
        let mut all = vec!["--color=never"];
        all.extend_from_slice(args);
        all.iter().map(|s| s.to_string()).collect::<Vec<_>>()
    };

    run_owned(dir, "jj", &jj(&["git", "init", "--colocate"])).await?;
    if vcs::detect(dir).await? != Vcs::JjColocated {
        return Err(format!(
            "jj git init reported success, but {path} is not a Jujutsu repository colocated with Git."
        ));
    }
    // Read before any other `jj` command, which would detach Git's HEAD.
    let branch = match vcs::head(dir).await {
        Head::Unborn { branch } => branch,
        other => {
            return Err(format!(
                "The new repository's Git HEAD is {other:?}, not a branch waiting for its first \
                 commit, so SlashIt could not tell which name to give the first commit's bookmark."
            ))
        }
    };
    super::checked_base_branch(&branch)?;
    let message = if files.is_empty() {
        EMPTY_MESSAGE
    } else {
        SNAPSHOT_MESSAGE
    };
    run_owned(dir, "jj", &jj(&["describe", "-m", message])).await?;
    run_owned(dir, "jj", &jj(&["bookmark", "create", &branch, "-r", "@"])).await?;
    run_owned(dir, "jj", &jj(&["new"])).await?;
    let commit = committed_on(dir, &branch).await?;
    let recorded = vcs::git_stdout(dir, &["ls-tree", "-r", "--name-only", &commit]).await?;
    Ok(InitOutcome {
        branch,
        commit,
        files: recorded.lines().filter(|l| !l.is_empty()).count(),
    })
}

/// Refuse when any of `files` in `dir` is larger than the largest new file
/// JJ records here (`snapshot.max-new-file-size`, as JJ itself reports it,
/// which includes its built-in default).
async fn jj_oversized(dir: &Path, files: &[String]) -> Result<(), String> {
    let configured = run(
        dir,
        "jj",
        &[
            "--ignore-working-copy",
            "--color=never",
            "config",
            "get",
            "snapshot.max-new-file-size",
        ],
        &[],
    )
    .await?;
    let limit = parse_size(&configured).ok_or_else(|| {
        format!(
            "Jujutsu's snapshot.max-new-file-size is {configured:?}, which SlashIt cannot read, so \
             it cannot tell whether every file would be recorded. Initialize with Git instead, or \
             set a plain byte count. Nothing was changed."
        )
    })?;
    if limit == 0 {
        return Ok(());
    }
    let mut oversized: Vec<String> = files
        .iter()
        .filter_map(|file| {
            let size = std::fs::metadata(dir.join(file)).ok()?.len();
            (size > limit).then(|| format!("{file} ({size} bytes)"))
        })
        .collect();
    if oversized.is_empty() {
        return Ok(());
    }
    oversized.truncate(SAMPLE);
    Err(format!(
        "Jujutsu would leave out files larger than its snapshot.max-new-file-size ({configured}): \
         {}. Raise that setting (`jj config set --user snapshot.max-new-file-size <bytes>`), add \
         the files to .gitignore, or initialize with Git instead. Nothing was changed.",
        oversized.join(", ")
    ))
}

/// A byte count as JJ's configuration writes it: a plain integer, or an
/// integer with a unit (`B`, `KB`/`KiB`, `MB`/`MiB`, `GB`/`GiB`, `TB`/`TiB`;
/// decimal units are powers of 1000, binary ones of 1024). Anything else,
/// including a fraction, is `None`.
fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim().trim_matches('"').trim();
    let digits = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(digits);
    let number: u64 = number.parse().ok()?;
    let factor: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" | "k" => 1_000,
        "kib" => 1 << 10,
        "mb" | "m" => 1_000_000,
        "mib" => 1 << 20,
        "gb" | "g" => 1_000_000_000,
        "gib" => 1 << 30,
        "tb" | "t" => 1_000_000_000_000,
        "tib" => 1 << 40,
        _ => return None,
    };
    number.checked_mul(factor)
}

/// The commit `refs/heads/<branch>` names, which must exist now.
async fn committed_on(dir: &Path, branch: &str) -> Result<String, String> {
    super::restack::exact_ref(dir, &format!("refs/heads/{branch}"))
        .await?
        .ok_or_else(|| {
            format!("The first commit was made, but branch {branch} does not point at it.")
        })
}

/// The files a first commit of the plain folder `dir` would hold: every
/// file Git does not ignore there, read through a throwaway repository
/// outside the folder so that nothing is written into it.
async fn untracked_in_plain_folder(dir: &Path) -> Result<Vec<String>, String> {
    let scratch = Scratch::create()?;
    let git_dir = scratch.0.join("preview.git");
    let git_dir = git_dir
        .to_str()
        .ok_or("The scratch directory's path is not UTF-8")?;
    run(dir, "git", &["init", "-q", "--bare", git_dir], &[]).await?;
    let work_tree = dir.to_str().ok_or("The folder's path is not UTF-8")?;
    let listed = vcs::git_stdout(
        dir,
        &[
            &format!("--git-dir={git_dir}"),
            &format!("--work-tree={work_tree}"),
            "ls-files",
            "--others",
            "--exclude-standard",
        ],
    )
    .await?;
    Ok(listed
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// A directory of SlashIt's own under the system temporary directory,
/// removed when dropped.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn create() -> Result<Self, String> {
        let dir =
            std::env::temp_dir().join(format!("slashit-vcs-preview-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir)
            .map_err(|e| format!("Could not create a scratch directory: {e}"))?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The files a first commit of the Git repository `dir`, which has none
/// yet, would hold: what is staged and what is not ignored.
async fn files_to_commit(dir: &Path) -> Result<Vec<String>, String> {
    let staged = vcs::git_stdout(dir, &["ls-files", "--cached"]).await?;
    let untracked = vcs::git_stdout(dir, &["ls-files", "--others", "--exclude-standard"]).await?;
    let mut files: Vec<String> = staged
        .lines()
        .chain(untracked.lines())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// Why Git would have no identity to commit with in `dir`, if so.
async fn git_identity_missing(dir: &Path) -> Option<String> {
    let mut missing = false;
    for key in ["user.name", "user.email"] {
        let value = run(dir, "git", &["config", "--get", key], &[])
            .await
            .unwrap_or_default();
        missing |= value.is_empty();
    }
    missing.then(|| {
        "Git has no commit identity configured (user.name and user.email), and SlashIt will not \
         invent one. Set them with `git config --global user.name \"Your Name\"` and `git config \
         --global user.email you@example.com`, then try again. Nothing was changed."
            .to_string()
    })
}

/// Why Jujutsu would have no identity to commit with in `dir`, if so.
async fn jj_identity_missing(dir: &Path) -> Option<String> {
    let mut missing = false;
    for key in ["user.name", "user.email"] {
        let value = run(
            dir,
            "jj",
            &[
                "--ignore-working-copy",
                "--color=never",
                "config",
                "get",
                key,
            ],
            &[],
        )
        .await
        .unwrap_or_default();
        missing |= value.is_empty();
    }
    missing.then(|| {
        "Jujutsu has no commit identity configured (user.name and user.email), and SlashIt will \
         not invent one. Set them with `jj config set --user user.name \"Your Name\"` and `jj \
         config set --user user.email you@example.com`, then try again. Nothing was changed."
            .to_string()
    })
}

async fn run(
    dir: &Path,
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<String, String> {
    let output = command(program)
        .args(args)
        .envs(envs.iter().copied())
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(|e| format!("Failed to run {program}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn run_owned(dir: &Path, program: &str, args: &[String]) -> Result<String, String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(dir, program, &args, &[]).await
}

/// `program`, with the environment a test gave this task, if any.
fn command(program: &str) -> tokio::process::Command {
    let command = tokio::process::Command::new(program);
    #[cfg(test)]
    let command = test_env::apply(command);
    command
}

/// An environment for the programs this module runs, seen by one test's
/// task only: the process environment is shared by every test running in
/// parallel.
#[cfg(test)]
pub(crate) mod test_env {
    tokio::task_local! {
        static ENV: Vec<(String, String)>;
    }

    pub(super) fn apply(mut command: tokio::process::Command) -> tokio::process::Command {
        if let Ok(envs) = ENV.try_with(Clone::clone) {
            command.envs(envs);
        }
        command
    }

    /// Run `future` with `envs` added to every program this module starts.
    pub(crate) async fn scope<F: std::future::Future>(
        envs: Vec<(String, String)>,
        future: F,
    ) -> F::Output {
        ENV.scope(envs, future).await
    }

    /// Git and JJ configuration isolated under `root`, with a commit
    /// identity and `init.defaultBranch = default_branch`.
    pub(crate) fn isolated(root: &std::path::Path, default_branch: &str) -> Vec<(String, String)> {
        let git_config = root.join("gitconfig");
        std::fs::write(
            &git_config,
            format!(
                "[user]\n\tname = Test\n\temail = test@example.com\n[init]\n\tdefaultBranch = {default_branch}\n[commit]\n\tgpgsign = false\n"
            ),
        )
        .unwrap();
        let jj_config = root.join("jjconfig.toml");
        std::fs::write(
            &jj_config,
            "[user]\nname = \"Test\"\nemail = \"test@example.com\"\n",
        )
        .unwrap();
        vec![
            (
                "GIT_CONFIG_GLOBAL".to_string(),
                git_config.to_string_lossy().to_string(),
            ),
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
            (
                "JJ_CONFIG".to_string(),
                jj_config.to_string_lossy().to_string(),
            ),
        ]
    }

    /// Like [`isolated`], with no commit identity at all.
    pub(crate) fn without_identity(root: &std::path::Path) -> Vec<(String, String)> {
        let git_config = root.join("gitconfig-anonymous");
        std::fs::write(&git_config, "").unwrap();
        let jj_config = root.join("jjconfig-anonymous.toml");
        std::fs::write(&jj_config, "").unwrap();
        vec![
            (
                "GIT_CONFIG_GLOBAL".to_string(),
                git_config.to_string_lossy().to_string(),
            ),
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
            (
                "JJ_CONFIG".to_string(),
                jj_config.to_string_lossy().to_string(),
            ),
            ("HOME".to_string(), root.to_string_lossy().to_string()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `isolated` configuration with `extra_jj` appended to JJ's.
    fn env_with_jj(root: &Path, extra_jj: &str) -> Vec<(String, String)> {
        let env = test_env::isolated(root, "trunk-xyz");
        let jj_config = root.join("jjconfig.toml");
        let mut text = std::fs::read_to_string(&jj_config).unwrap();
        text.push_str(extra_jj);
        std::fs::write(&jj_config, text).unwrap();
        env
    }

    #[test]
    fn sizes_are_read_the_way_jj_writes_them() {
        assert_eq!(parse_size("1MiB"), Some(1 << 20));
        assert_eq!(parse_size("\"2KB\""), Some(2_000));
        assert_eq!(parse_size("2KiB"), Some(2_048));
        assert_eq!(parse_size("3000"), Some(3_000));
        assert_eq!(parse_size("0"), Some(0));
        assert_eq!(parse_size("2.5KiB"), None);
        assert_eq!(parse_size("lots"), None);
    }

    /// Another Git repository inside the folder would be recorded as an
    /// empty reference by Git and skipped by JJ: both refuse, before
    /// anything is written, naming it.
    #[tokio::test]
    async fn a_nested_repository_is_refused_for_both_tools() {
        let temp = tempfile::TempDir::new().unwrap();
        let folder = temp.path().join("project");
        std::fs::create_dir_all(folder.join("vendor/lib")).unwrap();
        std::fs::write(folder.join("notes.md"), "notes").unwrap();
        run_git(&folder.join("vendor/lib"), &["init", "-q"]);
        std::fs::write(folder.join("vendor/lib/code.rs"), "code").unwrap();
        let path = folder.to_str().unwrap();
        let env = test_env::isolated(temp.path(), "trunk-xyz");

        let shown = test_env::scope(env.clone(), preview(path)).await.unwrap();
        assert!(
            shown
                .blocked
                .as_deref()
                .is_some_and(|b| b.contains("vendor/lib/")),
            "{shown:?}"
        );
        assert_eq!(shown.action, None);
        for kind in [VcsInitKind::Git, VcsInitKind::Jujutsu] {
            let refused = test_env::scope(env.clone(), initialize(path, kind, None))
                .await
                .expect_err("nested repository");
            assert!(refused.contains("vendor/lib/"), "{kind:?}: {refused}");
            assert!(
                !folder.join(".git").exists() && !folder.join(".jj").exists(),
                "{kind:?}"
            );
        }

        // Ignored, it is not part of the first commit at all.
        std::fs::write(folder.join(".gitignore"), "vendor/\n").unwrap();
        let outcome = test_env::scope(env, initialize(path, VcsInitKind::Git, Some(2)))
            .await
            .expect("initialized");
        assert_eq!(outcome.files, 2);
    }

    /// The person's own `snapshot.max-new-file-size` is respected: a file
    /// above it refuses Jujutsu initialization, naming the file and the
    /// setting, and changes nothing. Git is not affected by it.
    #[tokio::test]
    async fn a_file_above_the_users_jj_limit_refuses_jujutsu_initialization() {
        // The real `jj`: other tests take it off `PATH` while they hold this.
        let _path = crate::test_helpers::PATH_LOCK.lock().await;
        let temp = tempfile::TempDir::new().unwrap();
        let folder = temp.path().join("project");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("small.txt"), "ok").unwrap();
        std::fs::write(folder.join("big.txt"), vec![b'x'; 200]).unwrap();
        let path = folder.to_str().unwrap();
        let env = env_with_jj(temp.path(), "[snapshot]\nmax-new-file-size = 100\n");

        let shown = test_env::scope(env.clone(), preview(path)).await.unwrap();
        let jj_blocked = shown.jj_blocked.clone().expect("jj refused in the preview");
        assert!(jj_blocked.contains("big.txt (200 bytes)"), "{jj_blocked}");
        assert!(jj_blocked.contains("initialize with Git"), "{jj_blocked}");
        assert!(shown.blocked.is_none() && shown.git_identity_missing.is_none());

        let refused = test_env::scope(env.clone(), initialize(path, VcsInitKind::Jujutsu, Some(2)))
            .await
            .expect_err("above the limit");
        assert!(
            refused.contains("big.txt") && refused.contains("snapshot.max-new-file-size"),
            "{refused}"
        );
        assert!(!folder.join(".jj").exists() && !folder.join(".git").exists());

        let outcome = test_env::scope(env, initialize(path, VcsInitKind::Git, Some(2)))
            .await
            .expect("Git records it");
        assert_eq!(outcome.files, 2);
    }

    /// `git.colocate = false` in the person's configuration does not make a
    /// repository SlashIt cannot create Task Checkouts from.
    #[tokio::test]
    async fn jujutsu_initialization_colocates_whatever_git_colocate_says() {
        // The real `jj`: other tests take it off `PATH` while they hold this.
        let _path = crate::test_helpers::PATH_LOCK.lock().await;
        let temp = tempfile::TempDir::new().unwrap();
        let folder = temp.path().join("project");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("notes.md"), "notes").unwrap();
        let path = folder.to_str().unwrap();
        let env = env_with_jj(temp.path(), "[git]\ncolocate = false\n");

        let outcome = test_env::scope(env, initialize(path, VcsInitKind::Jujutsu, Some(1)))
            .await
            .expect("initialized");
        assert_eq!(outcome.branch, "trunk-xyz");
        assert_eq!(vcs::detect(&folder).await, Ok(Vcs::JjColocated));
        assert!(folder.join(".git").is_dir());
    }
}
