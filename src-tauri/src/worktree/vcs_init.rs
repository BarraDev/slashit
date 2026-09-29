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
//! A Jujutsu initialization that fails after `jj git init` is undone: the
//! `.jj` and `.git` directories it created, and only those, are removed (see
//! `undo_jj_init`). A Git initialization whose commit fails leaves a
//! repository with no commit, which [`initialize`] completes when asked again.
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
    fn split(dir: &Path, listed: Vec<String>) -> Self {
        // `ls-files --others` lists a nested Git repository as its
        // directory, with a trailing slash, rather than the files inside it.
        let (mut nested, files): (Vec<String>, Vec<String>) =
            listed.into_iter().partition(|entry| entry.ends_with('/'));
        // A nested Jujutsu repository without Git colocation is not one to
        // Git: its `.jj` ignores itself and its files are listed. JJ would
        // not record them in this repository. Any directory holding a listed
        // file is checked for a `.jj` of its own.
        let mut seen = std::collections::BTreeSet::new();
        for file in &files {
            let mut parent = Path::new(file).parent();
            while let Some(sub) = parent.filter(|p| !p.as_os_str().is_empty()) {
                if seen.insert(sub.to_path_buf()) && dir.join(sub).join(".jj").is_dir() {
                    nested.push(format!("{}/", sub.display()));
                }
                parent = sub.parent();
            }
        }
        nested.sort();
        Candidates { files, nested }
    }

    /// Why nothing may be recorded, if something nested would be lost.
    fn nested_refusal(&self, path: &str) -> Option<String> {
        (!self.nested.is_empty()).then(|| {
            format!(
                "{path} contains another repository ({}). Its files would not be recorded in \
                 the first commit: Git would store only a reference to a nested Git repository, \
                 and Jujutsu would leave a nested repository out. Move it out of the folder or add it to .gitignore, then try again. \
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
    Ok(Ok(Candidates::split(
        dir,
        match vcs {
            Vcs::None => untracked_in_plain_folder(dir).await?,
            Vcs::Git if !vcs::has_commits(dir).await => files_to_commit(dir).await?,
            Vcs::Git | Vcs::JjColocated => {
                return Ok(Err(format!(
                    "{path} already has version control with commits."
                )))
            }
            other => return Ok(Err(other.refusal(&path).unwrap_or_default())),
        },
    )))
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
    let files = git_paths(dir, &["ls-files", "-z"]).await?.len();
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
    // What this call may take back if it fails: only metadata it creates.
    // Neither name exists at this check, immediately before `jj git init`.
    // Something else creating one of them in the moment between the check
    // and the init is not ruled out; within this call, only `jj git init`
    // writes there, and the undo removes only real directories.
    refuse_existing_jj_metadata(dir, path)?;
    match initialize_jj_attempt(dir, path, files).await {
        Ok(outcome) => Ok(outcome),
        Err(failed) => Err(format!(
            "{}. {}",
            failed.trim_end().trim_end_matches('.'),
            undo_jj_init(dir, path)
        )),
    }
}

/// Refuse when `.jj` or `.git` is already at the root of `dir`, in any
/// form (directory, file or link), which a failed attempt must not take
/// away.
fn refuse_existing_jj_metadata(dir: &Path, path: &str) -> Result<(), String> {
    for name in JJ_INIT_METADATA {
        if std::fs::symlink_metadata(dir.join(name)).is_ok() {
            return Err(format!(
                "{path}/{name} already exists, so SlashIt will not initialize Jujutsu there. \
                 Nothing was changed."
            ));
        }
    }
    Ok(())
}

/// The two directories `jj git init --colocate` creates at the folder root.
const JJ_INIT_METADATA: [&str; 2] = [".jj", ".git"];

/// Remove what a failed [`initialize_jj_attempt`] created: exactly the
/// `.jj` and `.git` directories at the root of `dir`, never following a
/// symbolic link (`remove_dir_all` removes links, not their targets), and
/// nothing else. Initializing does not create or change any other file, so
/// the folder is then as it was. Says what happened, in words a person can
/// act on.
fn undo_jj_init(dir: &Path, path: &str) -> String {
    let mut removed = Vec::new();
    let mut left = Vec::new();
    for name in JJ_INIT_METADATA {
        let target = dir.join(name);
        let shown = format!("{:?}", target.display().to_string());
        match std::fs::symlink_metadata(&target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Ok(meta) if meta.is_dir() => match std::fs::remove_dir_all(&target) {
                Ok(()) => removed.push(name),
                Err(e) => left.push(format!(
                    "{shown}, which this attempt created and could not remove ({e}); it can be \
                     deleted"
                )),
            },
            Ok(_) => left.push(format!(
                "{shown}, which is not a directory and was left alone; check what it is before \
                 deleting it"
            )),
            Err(e) => left.push(format!("{shown}, which could not be examined ({e})")),
        }
    }
    if !left.is_empty() {
        return format!(
            "The initialization could not be fully undone. Left behind: {}. Initialize again \
             once it is gone; nothing else in the folder was created or changed",
            left.join("; ")
        );
    }
    if removed.is_empty() {
        format!(
            "Nothing was left behind, so {path} is as it was and initializing again starts afresh"
        )
    } else {
        format!(
            "The initialization was undone: the {} it created {} removed, so {path} is as it was \
             and initializing again starts afresh",
            removed.join(" and "),
            if removed.len() == 1 { "was" } else { "were" }
        )
    }
}

/// Everything from `jj git init` to the verified first commit, which
/// [`initialize_jj`] undoes as a whole if any step fails.
async fn initialize_jj_attempt(
    dir: &Path,
    path: &str,
    files: &[String],
) -> Result<InitOutcome, String> {
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
    let recorded = git_paths(dir, &["ls-tree", "-r", "-z", "--name-only", &commit]).await?;
    // Anything JJ left out (or added) is not the first commit the person
    // was shown; failing here undoes the whole attempt.
    let shown: std::collections::BTreeSet<&String> = files.iter().collect();
    let kept: std::collections::BTreeSet<&String> = recorded.iter().collect();
    if shown != kept {
        let list = |names: Vec<&&String>| {
            let mut names: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
            names.truncate(SAMPLE);
            names.join(", ")
        };
        let missing = list(shown.difference(&kept).collect());
        let extra = list(kept.difference(&shown).collect());
        return Err(format!(
            "Jujutsu's first commit does not hold what was shown (not recorded: [{missing}]; \
             recorded but not shown: [{extra}])"
        ));
    }
    Ok(InitOutcome {
        branch,
        commit,
        files: recorded.len(),
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
            // A file whose size cannot be read is not assumed small.
            match std::fs::symlink_metadata(dir.join(file)) {
                Ok(meta) => (meta.len() > limit).then(|| format!("{file} ({} bytes)", meta.len())),
                Err(e) => Some(format!("{file} (size unreadable: {e})")),
            }
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
    git_paths(
        dir,
        &[
            &format!("--git-dir={git_dir}"),
            &format!("--work-tree={work_tree}"),
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
        ],
    )
    .await
}

/// The paths a `-z` listing of `git <args>` in `dir` names, exactly as
/// they are on disk: without `-z`, Git quotes and escapes any path with a
/// non-ASCII or special character (`core.quotePath`), which no longer names
/// the file.
async fn git_paths(dir: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    let output = command("git")
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(|e| format!("Failed to run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let listed = String::from_utf8(output.stdout).map_err(|e| {
        let bad = e
            .as_bytes()
            .split(|b| *b == 0)
            .filter(|name| std::str::from_utf8(name).is_err())
            .map(|name| format!("{:?}", String::from_utf8_lossy(name)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{bad}: a file name that is not valid UTF-8, which SlashIt cannot record faithfully. \
             Rename it, or add it to .gitignore, then try again. Nothing was changed."
        )
    })?;
    Ok(listed
        .split('\0')
        .filter(|p| !p.is_empty())
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
    let mut files = git_paths(dir, &["ls-files", "-z", "--cached"]).await?;
    files.extend(git_paths(dir, &["ls-files", "-z", "--others", "--exclude-standard"]).await?);
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
        // A name Git would quote and escape without `-z`.
        std::fs::create_dir_all(folder.join("vendor/café")).unwrap();
        run_git(&folder.join("vendor/café"), &["init", "-q"]);
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
            assert!(
                refused.contains("vendor/lib/") && refused.contains("vendor/café/"),
                "{kind:?}: {refused}"
            );
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
        std::fs::write(folder.join("grande-é.bin"), vec![b'x'; 300]).unwrap();
        let path = folder.to_str().unwrap();
        let env = env_with_jj(temp.path(), "[snapshot]\nmax-new-file-size = 100\n");

        let shown = test_env::scope(env.clone(), preview(path)).await.unwrap();
        let jj_blocked = shown.jj_blocked.clone().expect("jj refused in the preview");
        assert!(jj_blocked.contains("big.txt (200 bytes)"), "{jj_blocked}");
        assert!(
            jj_blocked.contains("grande-é.bin (300 bytes)"),
            "{jj_blocked}"
        );
        assert!(
            shown.sample.iter().any(|f| f == "grande-é.bin"),
            "{:?}",
            shown.sample
        );
        assert!(jj_blocked.contains("initialize with Git"), "{jj_blocked}");
        assert!(shown.blocked.is_none() && shown.git_identity_missing.is_none());

        let refused = test_env::scope(env.clone(), initialize(path, VcsInitKind::Jujutsu, Some(3)))
            .await
            .expect_err("above the limit");
        assert!(
            refused.contains("big.txt") && refused.contains("snapshot.max-new-file-size"),
            "{refused}"
        );
        assert!(!folder.join(".jj").exists() && !folder.join(".git").exists());

        let outcome = test_env::scope(env, initialize(path, VcsInitKind::Git, Some(3)))
            .await
            .expect("Git records it");
        assert_eq!(outcome.files, 3);
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

/// What happens when an initialization fails partway: nothing the person
/// had is lost, and they can simply try again. Unix only, like the fake
/// programs and permission bits it relies on.
#[cfg(all(test, unix))]
mod partial_failure_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// Every file under `dir` except the root's `.git` and `.jj`, with its
    /// bytes, and every directory, so that any change to the person's own
    /// files shows.
    fn user_files(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                if dir == root && (relative == Path::new(".git") || relative == Path::new(".jj")) {
                    continue;
                }
                if entry.file_type().unwrap().is_dir() {
                    out.insert(relative, None);
                    walk(root, &path, out);
                } else {
                    out.insert(relative, Some(std::fs::read(&path).unwrap()));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    /// A folder with a file, a `.gitignore`, an ignored file and a nested
    /// directory.
    fn folder(temp: &Path) -> PathBuf {
        let folder = temp.join("project");
        std::fs::create_dir_all(folder.join("build")).unwrap();
        std::fs::create_dir_all(folder.join("src/deep")).unwrap();
        std::fs::write(folder.join("notes.md"), "notes\n").unwrap();
        std::fs::write(folder.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(folder.join("build/out.bin"), [0u8, 1, 2, 3]).unwrap();
        std::fs::write(folder.join("src/deep/lib.rs"), "// lib\n").unwrap();
        folder
    }

    /// Initialize `path` with Jujutsu while a `jj` that fails whenever it is
    /// given the argument `failing` stands in front of the real one.
    async fn initialize_with_jj_failing_at(temp: &Path, path: &str, failing: &str) -> String {
        let fake = crate::test_helpers::FakeProgram::install("unused", "").await;
        let real = fake.original("jj").expect("a real jj on PATH");
        fake.add(
            "jj",
            &format!(
                "for a in \"$@\"; do if [ \"$a\" = '{failing}' ]; then echo 'injected {failing} failure' >&2; exit 1; fi; done\nexec '{}' \"$@\"",
                real.display()
            ),
        );
        test_env::scope(
            test_env::isolated(temp, "trunk-xyz"),
            initialize(path, VcsInitKind::Jujutsu, None),
        )
        .await
        .expect_err("the injected failure")
    }

    /// Jujutsu initialization that fails after `jj git init` succeeded, at
    /// each later step: the error says what failed and that it was undone,
    /// the person's files are byte for byte what they were, no `.jj` or
    /// `.git` is left, and initializing again works.
    #[tokio::test]
    async fn a_jujutsu_initialization_failing_partway_is_undone() {
        for failing in ["describe", "bookmark", "new"] {
            let temp = tempfile::TempDir::new().unwrap();
            let folder = folder(temp.path());
            let path = folder.to_str().unwrap();
            let before = user_files(&folder);

            let refused = initialize_with_jj_failing_at(temp.path(), path, failing).await;

            assert!(
                refused.contains(&format!("injected {failing} failure")),
                "{failing}: {refused}"
            );
            assert!(refused.contains("undone"), "{failing}: {refused}");
            assert!(!refused.contains(".."), "{failing}: {refused}");
            assert_eq!(
                user_files(&folder),
                before,
                "{failing}: the person's files changed"
            );
            assert!(
                !folder.join(".jj").exists() && !folder.join(".git").exists(),
                "{failing}"
            );
            assert_eq!(vcs::detect(&folder).await, Ok(Vcs::None), "{failing}");

            let _path = crate::test_helpers::PATH_LOCK.lock().await;
            let outcome = test_env::scope(
                test_env::isolated(temp.path(), "trunk-xyz"),
                initialize(path, VcsInitKind::Jujutsu, None),
            )
            .await
            .unwrap_or_else(|e| panic!("{failing}: initializing again: {e}"));
            assert_eq!(outcome.branch, "trunk-xyz", "{failing}");
            assert_eq!(
                vcs::detect(&folder).await,
                Ok(Vcs::JjColocated),
                "{failing}"
            );
            assert_eq!(
                user_files(&folder),
                before,
                "{failing}: a successful init changed files"
            );
        }
    }

    /// Metadata that was there before is never taken away: a folder that is
    /// already a Git repository is refused before `jj` runs at all.
    #[tokio::test]
    async fn existing_metadata_is_never_removed() {
        let temp = tempfile::TempDir::new().unwrap();
        let folder = folder(temp.path());
        let path = folder.to_str().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&folder)
            .output()
            .unwrap();
        assert!(init.status.success());
        let refused = initialize_with_jj_failing_at(temp.path(), path, "describe").await;
        assert!(refused.contains("already a Git repository"), "{refused}");
        assert!(
            folder.join(".git").is_dir(),
            "the existing repository survives"
        );
        assert!(!folder.join(".jj").exists());
    }

    /// Git's own partial failure (the repository is created, the first
    /// commit is refused, here by a hook) leaves a repository with no
    /// commit, which initializing again completes.
    #[tokio::test]
    async fn a_git_initialization_whose_commit_fails_can_be_completed_again() {
        let temp = tempfile::TempDir::new().unwrap();
        let folder = folder(temp.path());
        let path = folder.to_str().unwrap();
        let before = user_files(&folder);
        let hooks = temp.path().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho 'hook says no' >&2\nexit 1\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut env = test_env::isolated(temp.path(), "trunk-xyz");
        env.push(("GIT_CONFIG_COUNT".to_string(), "1".to_string()));
        env.push(("GIT_CONFIG_KEY_0".to_string(), "core.hooksPath".to_string()));
        env.push((
            "GIT_CONFIG_VALUE_0".to_string(),
            hooks.to_string_lossy().to_string(),
        ));

        let refused = test_env::scope(env, initialize(path, VcsInitKind::Git, None))
            .await
            .expect_err("the hook refuses the commit");
        assert!(refused.contains("hook says no"), "{refused}");
        assert_eq!(vcs::detect(&folder).await, Ok(Vcs::Git));
        assert!(!vcs::has_commits(&folder).await);

        let outcome = test_env::scope(
            test_env::isolated(temp.path(), "trunk-xyz"),
            initialize(path, VcsInitKind::Git, None),
        )
        .await
        .expect("initializing again completes it");
        assert_eq!(outcome.branch, "trunk-xyz");
        assert_eq!(user_files(&folder), before);
    }

    /// Undoing takes away only real directories named `.jj` and `.git`: a
    /// symbolic link or a file under those names is left alone (with its
    /// target intact) and named, quoted, on its own; a directory that cannot
    /// be removed is named the same way, and nothing else is.
    #[tokio::test]
    async fn undoing_never_follows_links_and_names_exactly_what_is_left() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        // A space in the path: nothing said may split it.
        let folder = folder(&temp.path().join("My Project"));
        let path = folder.to_str().unwrap();
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("keep.txt"), "keep").unwrap();
        std::os::unix::fs::symlink(&elsewhere, folder.join(".jj")).unwrap();
        std::fs::write(folder.join(".git"), "gitdir: nowhere\n").unwrap();

        let said = undo_jj_init(&folder, path);
        assert!(said.contains("could not be fully undone"), "{said}");
        for name in [".jj", ".git"] {
            let quoted = format!(
                "{:?}, which is not a directory and was left alone",
                format!("{path}/{name}")
            );
            assert!(said.contains(&quoted), "{said}");
        }
        assert!(!said.contains("rm "), "no shell command to paste: {said}");
        assert!(
            elsewhere.join("keep.txt").is_file(),
            "a link's target is never touched"
        );
        assert!(folder.join(".git").is_file());

        // Only `.jj` left, and not removable: only `.jj` is named.
        std::fs::remove_file(folder.join(".jj")).unwrap();
        std::fs::remove_file(folder.join(".git")).unwrap();
        if crate::ipc::server::current_uid() != 0 {
            let locked = folder.join(".jj/locked");
            std::fs::create_dir_all(&locked).unwrap();
            std::fs::write(locked.join("f"), "x").unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
            let said = undo_jj_init(&folder, path);
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
            let quoted = format!(
                "{:?}, which this attempt created and could not remove",
                format!("{path}/.jj")
            );
            assert!(
                said.contains(&quoted) && said.contains("it can be deleted"),
                "{said}"
            );
            assert!(!said.contains(".git"), "only what is left is named: {said}");
            assert!(folder.join("notes.md").is_file());
            std::fs::remove_dir_all(folder.join(".jj")).unwrap();
        }

        // What is said about a clean undo is what was actually removed.
        assert!(undo_jj_init(&folder, path).starts_with("Nothing was left behind"));
        std::fs::create_dir_all(folder.join(".jj/repo")).unwrap();
        let said = undo_jj_init(&folder, path);
        assert!(said.contains("the .jj it created was removed"), "{said}");
        assert!(!folder.join(".jj").exists());
    }

    /// A stray `.git` file that Git does not take for a repository (so the
    /// folder is classified as having no version control) still stops a
    /// Jujutsu initialization before `jj git init`, and is left as it was.
    #[tokio::test]
    async fn existing_jj_or_git_names_stop_jujutsu_initialization_before_it_starts() {
        let temp = tempfile::TempDir::new().unwrap();
        let folder = folder(temp.path());
        let path = folder.to_str().unwrap();
        std::fs::write(folder.join(".git"), "not a repository\n").unwrap();
        assert_eq!(vcs::detect(&folder).await, Ok(Vcs::None));
        let _path = crate::test_helpers::PATH_LOCK.lock().await;
        let refused = test_env::scope(
            test_env::isolated(temp.path(), "trunk-xyz"),
            initialize(path, VcsInitKind::Jujutsu, None),
        )
        .await
        .expect_err("a .git is already there");
        assert!(refused.contains("/.git already exists"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(folder.join(".git")).unwrap(),
            "not a repository\n"
        );
        assert!(!folder.join(".jj").exists());
    }

    /// A nested Jujutsu repository without Git colocation is refused like a
    /// nested Git repository, for both tools.
    #[tokio::test]
    async fn a_nested_jujutsu_repository_is_refused_for_both_tools() {
        let temp = tempfile::TempDir::new().unwrap();
        let folder = folder(temp.path());
        let path = folder.to_str().unwrap();
        std::fs::create_dir_all(folder.join("src/deep/.jj/repo")).unwrap();
        std::fs::write(folder.join("src/deep/.jj/.gitignore"), "/*\n").unwrap();
        let env = test_env::isolated(temp.path(), "trunk-xyz");
        let shown = test_env::scope(env.clone(), preview(path)).await.unwrap();
        assert!(
            shown
                .blocked
                .as_deref()
                .is_some_and(|b| b.contains("src/deep/")),
            "{shown:?}"
        );
        for kind in [VcsInitKind::Git, VcsInitKind::Jujutsu] {
            let refused = test_env::scope(env.clone(), initialize(path, kind, None))
                .await
                .expect_err("nested repository");
            assert!(refused.contains("src/deep/"), "{kind:?}: {refused}");
            assert!(!folder.join(".git").exists() && !folder.join(".jj").exists());
        }
    }
}
