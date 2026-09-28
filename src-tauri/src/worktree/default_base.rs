//! The base an ordinary task branch starts from.
//!
//! A task needs a safe, explicit version-control base; a remote is optional.
//! The base is resolved once, before the branch exists, from local refs and
//! the project's own record only: nothing here fetches, runs `git remote
//! set-head --auto`, or asks GitHub. What is resolved is one branch `D` and
//! the exact commit it named, which is then where the task branch is created
//! and what the task records as its base. The primary checkout's `HEAD` is
//! never consulted: it is wherever the user happens to be, which may be a
//! feature branch or, in a JJ-colocated repository, a detached commit
//! holding work nobody pushed.
//!
//! The folder must first be a Git repository root, or a Jujutsu repository
//! colocated with Git (see [`super::vcs`]); anything else is refused saying
//! what to do. Then, in order:
//!
//! 1. `refs/remotes/origin/HEAD`, read as the exact ref, when it is a
//!    symbolic ref that resolves to `refs/remotes/origin/<D>` and that ref
//!    names a commit the repository has. This is what `git clone` records,
//!    and what `git remote set-head origin` records later. Git reports where
//!    a chain of symbolic refs ends, so one that reaches
//!    `refs/remotes/origin/<D>` through another symbolic ref resolves to
//!    `<D>`, that final branch of origin's; one that ends anywhere else is
//!    refused. A remote-backed repository therefore behaves exactly as it
//!    did before projects had a local base: origin's default branch wins
//!    whenever it can be read.
//! 2. When that ref is missing, dangling or not symbolic, and the repository
//!    is also a JJ repository (`<repo>/.jj`), JJ's `trunk()` alias, but only
//!    when it is exactly `<D>@origin` and `refs/remotes/origin/<D>` resolves
//!    as above. Anything else it can be -- the built-in default expression,
//!    another remote, a revset -- is not a branch SlashIt can name. `jj` not
//!    being installed is the same as this step not answering.
//! 3. The project's local base branch (`domain::ProjectBase`), when it has
//!    one: the commit `refs/heads/<D>` names now. It was captured when the
//!    project was registered or initialized, or chosen explicitly, so
//!    switching the primary checkout to another branch does not move it. A
//!    base branch that is gone or unusable is refused, not skipped.
//! 4. Otherwise the task is refused with a message saying what to do. A
//!    remote not called `origin` is never read as a remote default branch.
//!
//! A malformed `refs/remotes/origin/HEAD` (pointing outside `origin`, or at a
//! ref no branch can start from) is refused at step 1 even when the project
//! has a local base: it is a repository setup that needs fixing, not an
//! absent remote.

use super::checked_task_branch;
use super::restack::{exact_ref, has_commit};
use super::vcs;
use crate::domain::{BranchOrigin, ProjectBase};
use serde::Serialize;
use std::path::Path;

/// Where a resolved base came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseSource {
    /// Origin's default branch, from `refs/remotes/origin/HEAD` or JJ's
    /// `trunk()`: `refs/remotes/origin/<D>`.
    Remote,
    /// The project's local base branch: `refs/heads/<D>`.
    Local,
}

/// The branch an ordinary task branch starts from, and the commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedBase {
    /// `D`: a branch of origin's, or a local branch, per [`Self::source`].
    /// A branch name [`checked_task_branch`] accepts.
    pub branch: String,
    /// The full object ID of the commit the ref named when it was resolved.
    pub commit: String,
    pub source: BaseSource,
}

impl ResolvedBase {
    /// What a task branch created at this base records as its origin.
    pub fn branch_origin(&self) -> BranchOrigin {
        match self.source {
            BaseSource::Remote => BranchOrigin::DefaultBase { branch: Some(self.branch.clone()) },
            BaseSource::Local => BranchOrigin::LocalBase { branch: self.branch.clone() },
        }
    }

    /// The ref this base was read from, for messages.
    pub fn describe(&self) -> String {
        match self.source {
            BaseSource::Remote => format!("{}@origin", self.branch),
            BaseSource::Local => format!("the project's base branch {}", self.branch),
        }
    }
}

const ORIGIN_HEAD: &str = "refs/remotes/origin/HEAD";
const ORIGIN_BRANCHES: &str = "refs/remotes/origin/";
const LOCAL_BRANCHES: &str = "refs/heads/";

/// What `refs/remotes/origin/HEAD` is, read as that exact ref.
pub(super) enum OriginHead {
    /// A symbolic ref to `refs/remotes/origin/<D>`, which exists.
    Branch(String),
    /// A symbolic ref to a ref outside `refs/remotes/origin/`.
    Elsewhere(String),
    /// Not there, dangling, or not a symbolic ref.
    Missing,
}

/// Resolve the base of the repository at `repo_path` for a project whose
/// local base is `project_base`, or say why there is none SlashIt can use.
/// See the module documentation.
pub async fn resolve_default_base(
    repo_path: &str,
    project_base: Option<&ProjectBase>,
) -> Result<ResolvedBase, String> {
    let repo = Path::new(repo_path);
    if let Some(refused) = vcs::detect(repo).await?.refusal(repo_path) {
        return Err(refused);
    }
    if let Some(base) = resolve_remote_base(repo, repo_path).await? {
        return Ok(base);
    }
    match project_base {
        Some(base) => resolve_local_base(repo, repo_path, base.branch()).await,
        None => Err(refusal(repo, repo_path).await),
    }
}

/// Steps 1 and 2 of the module documentation: origin's default branch, or
/// `None` when neither `refs/remotes/origin/HEAD` nor JJ names one.
pub(super) async fn resolve_remote_base(
    repo: &Path,
    repo_path: &str,
) -> Result<Option<ResolvedBase>, String> {
    match origin_head(repo).await? {
        OriginHead::Branch(branch) => {
            let branch = checked_task_branch(&branch).map_err(|e| {
                format!(
                    "{ORIGIN_HEAD} in {repo_path} names a default branch SlashIt cannot use: \
                     {e}"
                )
            })?;
            match at_origin_branch(repo, branch).await? {
                OriginBranch::Found(base) => return Ok(Some(base)),
                OriginBranch::Unusable(why) => {
                    return Err(format!(
                        "{ORIGIN_HEAD} in {repo_path} names {ORIGIN_BRANCHES}{branch}, which \
                         {why}, so a task's branch cannot start there. Fetch origin again, or \
                         use Detect default branch in Settings > Repository (`git remote \
                         set-head origin --auto`), then start the task again."
                    ));
                }
                OriginBranch::Missing => {}
            }
        }
        OriginHead::Elsewhere(target) => {
            return Err(format!(
                "{ORIGIN_HEAD} in {repo_path} points at {target}, not at a branch of origin, so \
                 SlashIt cannot tell which branch a task should start from. Use Detect default \
                 branch in Settings > Repository (`git remote set-head origin --auto`), or run \
                 `git remote set-head origin <default branch>` there, then start the task again."
            ));
        }
        OriginHead::Missing => {}
    }

    if repo.join(".jj").is_dir() {
        if let Some(branch) = jj_trunk_branch(repo).await {
            match at_origin_branch(repo, &branch).await? {
                OriginBranch::Found(base) => return Ok(Some(base)),
                OriginBranch::Unusable(why) => {
                    return Err(format!(
                        "{ORIGIN_HEAD} is not set in {repo_path}, and JJ's trunk() alias names \
                         {branch}@origin, but {ORIGIN_BRANCHES}{branch} {why}, so a task's branch \
                         cannot start there. Fetch origin again, or run `git remote set-head \
                         origin <default branch>` there, then start the task again."
                    ));
                }
                OriginBranch::Missing => {}
            }
        }
    }
    Ok(None)
}

/// Step 3: the commit the project's local base branch names now.
async fn resolve_local_base(
    repo: &Path,
    repo_path: &str,
    branch: &str,
) -> Result<ResolvedBase, String> {
    let branch = super::checked_base_branch(branch)?;
    match at_branch(repo, LOCAL_BRANCHES, branch).await? {
        Found::Commit(commit) => {
            Ok(ResolvedBase { branch: branch.to_string(), commit, source: BaseSource::Local })
        }
        Found::Missing => Err(format!(
            "This project's base branch {branch} does not exist in {repo_path} any more, so \
             SlashIt cannot tell where a new task should start. Choose the base branch again in \
             Settings > Repository."
        )),
        Found::Unusable(why) => Err(format!(
            "This project's base branch {branch} in {repo_path} {why}, so a task's branch cannot \
             start there. Choose another base branch in Settings > Repository."
        )),
    }
}

/// Whether a local branch `branch` is one a task branch can start at, and
/// its commit. For choosing and capturing a project's base.
pub(super) async fn local_branch_commit(repo: &Path, branch: &str) -> Result<Option<String>, String> {
    Ok(match at_branch(repo, LOCAL_BRANCHES, branch).await? {
        Found::Commit(commit) => Some(commit),
        Found::Missing | Found::Unusable(_) => None,
    })
}

/// What `for-each-ref` says about exactly `refname`.
enum Listed {
    /// Not listed: absent, or a symbolic ref whose target is not there.
    Absent,
    /// A ref naming an object directly.
    Direct,
    /// A symbolic ref, and the ref its chain of symbolic refs ends at.
    Symbolic(String),
}

/// Read `refname` as that exact ref.
///
/// `for-each-ref` matches the full ref name only, never a branch or tag that
/// merely carries the same name, and does not list a symbolic ref whose
/// target is not there. For a symbolic ref, `%(symref)` is where its whole
/// chain of symbolic refs ends, not the next ref along it. Its pattern also matches refs below `refname/`, so
/// only the line for the exact name is taken.
async fn listed(repo: &Path, refname: &str) -> Result<Listed, String> {
    let output = tokio::process::Command::new("git")
        .args(["for-each-ref", "--format=%(refname) %(symref)", refname])
        .current_dir(repo)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("Failed to run git for-each-ref: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "Could not read {refname} in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let listed = String::from_utf8_lossy(&output.stdout);
    let target = listed.lines().find_map(|line| {
        let (name, target) = line.split_once(' ')?;
        (name == refname).then(|| target.to_string())
    });
    Ok(match target {
        None => Listed::Absent,
        Some(target) if target.is_empty() => Listed::Direct,
        Some(target) => Listed::Symbolic(target),
    })
}

/// Read `refs/remotes/origin/HEAD`; see [`listed`].
pub(super) async fn origin_head(repo: &Path) -> Result<OriginHead, String> {
    Ok(match listed(repo, ORIGIN_HEAD).await? {
        Listed::Absent | Listed::Direct => OriginHead::Missing,
        Listed::Symbolic(target) => match target.strip_prefix(ORIGIN_BRANCHES) {
            Some(branch) => OriginHead::Branch(branch.to_string()),
            None => OriginHead::Elsewhere(target),
        },
    })
}

/// What `<prefix><branch>` is.
enum Found {
    /// A ref naming a commit object the repository has.
    Commit(String),
    /// There is no such ref.
    Missing,
    /// There is, but no branch can start at it, for the reason given.
    Unusable(&'static str),
}

/// What `refs/remotes/origin/<branch>` is.
enum OriginBranch {
    Found(ResolvedBase),
    Missing,
    Unusable(&'static str),
}

/// Read `refs/remotes/origin/<branch>` as that exact ref; see [`at_branch`].
async fn at_origin_branch(repo: &Path, branch: &str) -> Result<OriginBranch, String> {
    let branch = checked_task_branch(branch)?;
    Ok(match at_branch(repo, ORIGIN_BRANCHES, branch).await? {
        Found::Commit(commit) => OriginBranch::Found(ResolvedBase {
            branch: branch.to_string(),
            commit,
            source: BaseSource::Remote,
        }),
        Found::Missing => OriginBranch::Missing,
        Found::Unusable(why) => OriginBranch::Unusable(why),
    })
}

/// Read `<prefix><branch>` as that exact ref. Only a ref that names a commit
/// object directly, which the repository has, is a base; a symbolic ref is
/// not, since it says nothing about the branch itself.
async fn at_branch(repo: &Path, prefix: &str, branch: &str) -> Result<Found, String> {
    let refname = format!("{prefix}{branch}");
    match listed(repo, &refname).await? {
        Listed::Absent => return Ok(Found::Missing),
        Listed::Symbolic(_) => return Ok(Found::Unusable("is itself a symbolic ref")),
        Listed::Direct => {}
    }
    let Some(commit) = exact_ref(repo, &refname).await? else {
        return Ok(Found::Missing);
    };
    if !has_commit(repo, &commit).await? {
        return Ok(Found::Unusable("names a commit this repository does not have"));
    }
    if !is_commit_object(repo, &commit).await? {
        return Ok(Found::Unusable("names a tag object, not a commit"));
    }
    Ok(Found::Commit(commit))
}

/// Whether `oid` is itself a commit, rather than a tag that peels to one: a
/// branch created at a tag object is not a branch git can check out.
async fn is_commit_object(repo: &Path, oid: &str) -> Result<bool, String> {
    let output = tokio::process::Command::new("git")
        .args(["cat-file", "-t", oid])
        .current_dir(repo)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("Failed to run git cat-file: {e}"))?;
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "commit")
}

/// `D` when JJ's `trunk()` alias is exactly `<D>@origin`.
async fn jj_trunk_branch(repo: &Path) -> Option<String> {
    let value = jj_trunk_alias(repo).await?;
    let branch = value.strip_suffix("@origin")?;
    checked_task_branch(branch).ok().map(str::to_string)
}

/// The value of JJ's `trunk()` revset alias in the repository at `repo`.
///
/// `--ignore-working-copy` keeps JJ from snapshotting the primary checkout.
/// Reading the configuration can still move a repository's legacy
/// configuration into JJ's per-user configuration directory, which is JJ's
/// own migration and harmless. Any failure, including `jj` not being on
/// `PATH`, answers `None`.
pub(super) async fn jj_trunk_alias(repo: &Path) -> Option<String> {
    let output = tokio::process::Command::new("jj")
        .args([
            "--ignore-working-copy",
            "--color=never",
            "config",
            "get",
            r#"revset-aliases."trunk()""#,
        ])
        .current_dir(repo)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    Some(value.strip_suffix('\n').unwrap_or(&value).to_string())
}

/// Why no base could be resolved for a project with no local base branch,
/// and what to do about it.
async fn refusal(repo: &Path, repo_path: &str) -> String {
    let has_origin = tokio::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .is_ok_and(|output| output.status.success());
    let jj = if repo.join(".jj").is_dir() {
        " JJ's trunk() alias did not name one either: it is only used when it is exactly \
         `<branch>@origin` and that branch has been fetched."
    } else {
        ""
    };
    let no_commits = !vcs::has_commits(repo).await;
    if no_commits {
        return format!(
            "The repository at {repo_path} has no commits yet, so there is nothing a task's \
             branch can start from. Create an initial snapshot from Settings > Repository, or \
             commit there yourself, then start the task again."
        );
    }
    if !has_origin && vcs::local_branches(repo).await.is_empty() {
        return format!(
            "This project has no base branch for new tasks, and {repo_path} has neither a remote \
             named origin nor a local branch to start from.{jj} Create a branch first (for \
             example `git switch -c <name>`), then choose it in Settings > Repository and start \
             the task again. A remote is not required."
        );
    }
    if has_origin {
        format!(
            "SlashIt could not tell which branch a new task should start from in {repo_path}: \
             {ORIGIN_HEAD} is not set, or names a branch that has not been fetched, and this \
             project has no local base branch.{jj} In Settings > Repository, use Detect default \
             branch (`git remote set-head origin --auto`, which asks origin) or choose a local \
             base branch, then start the task again. SlashIt does not fetch or ask the network \
             on its own."
        )
    } else {
        format!(
            "This project has no base branch for new tasks, and {repo_path} has no remote named \
             origin to take one from.{jj} Choose the local branch tasks should start from in \
             Settings > Repository, then start the task again. A remote is not required."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(["-c", "user.email=test@example.com", "-c", "user.name=Test"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A repository at `<temp>/repo` with one commit on `trunk` -- not
    /// `main`, so that nothing below can pass by assuming the name --
    /// pushed to a bare `origin`, with `refs/remotes/origin/HEAD` recorded
    /// the way `git clone` leaves it.
    fn cloned_repo() -> (tempfile::TempDir, PathBuf, String) {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        let origin = temp.path().join("origin.git");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "trunk"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        git(&repo, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&repo, &["push", "-q", "origin", "trunk"]);
        git(&repo, &["remote", "set-head", "origin", "trunk"]);
        let tip = git(&repo, &["rev-parse", "refs/remotes/origin/trunk"]);
        (temp, repo, tip)
    }

    async fn resolve(repo: &Path) -> Result<ResolvedBase, String> {
        resolve_default_base(repo.to_str().unwrap(), None).await
    }

    #[tokio::test]
    async fn origin_head_names_the_default_branch_and_its_exact_commit() {
        let (_temp, repo, tip) = cloned_repo();
        // Where the primary checkout is plays no part.
        git(&repo, &["checkout", "-q", "--detach"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "local"]);

        assert_eq!(
            resolve(&repo).await,
            Ok(ResolvedBase { branch: "trunk".to_string(), commit: tip, source: BaseSource::Remote })
        );
    }

    /// A default branch under a path, as `git remote set-head` records it
    /// for one, is read whole.
    #[tokio::test]
    async fn a_default_branch_with_a_slash_is_read_whole() {
        let (_temp, repo, tip) = cloned_repo();
        git(&repo, &["push", "-q", "origin", "trunk:refs/heads/release/v2"]);
        git(&repo, &["fetch", "-q", "origin"]);
        git(&repo, &["remote", "set-head", "origin", "release/v2"]);

        assert_eq!(
            resolve(&repo).await,
            Ok(ResolvedBase { branch: "release/v2".to_string(), commit: tip, source: BaseSource::Remote })
        );
    }

    /// Missing, dangling, not symbolic, or pointing outside `origin`: with
    /// no JJ repository to ask, each is refused, saying what to run.
    #[tokio::test]
    async fn an_unusable_origin_head_is_refused_with_what_to_run() {
        type Arrange = fn(&Path, &str);
        let cases: [(&str, Arrange, &str); 6] = [
            ("missing", |repo, _| {
                git(repo, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
            }, "git remote set-head origin --auto"),
            ("dangling", |repo, _| {
                git(repo, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/gone"]);
            }, "git remote set-head origin --auto"),
            ("not symbolic", |repo, tip| {
                git(repo, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
                git(repo, &["update-ref", "refs/remotes/origin/HEAD", tip]);
            }, "git remote set-head origin --auto"),
            ("another remote", |repo, tip| {
                git(repo, &["update-ref", "refs/remotes/upstream/trunk", tip]);
                git(repo, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/upstream/trunk"]);
            }, "not at a branch of origin"),
            ("a name SlashIt will not pass on", |repo, tip| {
                git(repo, &["update-ref", "refs/remotes/origin/we@ird", tip]);
                git(repo, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/we@ird"]);
            }, "cannot use"),
            ("a tag object, not a commit", |repo, _| {
                git(repo, &["tag", "-a", "-m", "annotated", "v1"]);
                let tag = git(repo, &["rev-parse", "refs/tags/v1"]);
                git(repo, &["update-ref", "refs/remotes/origin/trunk", &tag]);
            }, "names a tag object, not a commit"),
        ];
        for (name, arrange, expected) in cases {
            let (_temp, repo, tip) = cloned_repo();
            arrange(&repo, &tip);
            let refused = resolve(&repo).await.expect_err(name);
            assert!(refused.contains(expected), "{name}: {refused}");
        }
    }

    /// `refs/remotes/origin/HEAD` pointing at another symbolic ref resolves
    /// to the branch of origin's the chain ends at, which is where a branch
    /// started from it is created. A chain that ends outside `origin` is
    /// refused.
    #[tokio::test]
    async fn an_origin_head_through_another_symbolic_ref_resolves_to_where_the_chain_ends() {
        let (_temp, repo, tip) = cloned_repo();
        git(&repo, &["symbolic-ref", "refs/remotes/origin/alias", "refs/remotes/origin/trunk"]);
        git(&repo, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/alias"]);

        assert_eq!(
            resolve(&repo).await,
            Ok(ResolvedBase { branch: "trunk".to_string(), commit: tip.clone(), source: BaseSource::Remote })
        );

        git(&repo, &["update-ref", "refs/remotes/upstream/trunk", &tip]);
        git(&repo, &["symbolic-ref", "refs/remotes/origin/alias", "refs/remotes/upstream/trunk"]);
        let refused = resolve(&repo).await.expect_err("a chain ending outside origin");
        assert!(refused.contains("not at a branch of origin"), "{refused}");
    }

    /// A branch or tag that merely carries the name of the ref, or of the
    /// branch it names, is never read in its place.
    #[tokio::test]
    async fn refs_that_only_share_a_name_are_not_read() {
        let (_temp, repo, tip) = cloned_repo();
        git(&repo, &["checkout", "-q", "-b", "elsewhere"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "elsewhere"]);
        let other = git(&repo, &["rev-parse", "HEAD"]);
        for name in [
            "refs/heads/refs/remotes/origin/HEAD",
            "refs/tags/refs/remotes/origin/HEAD",
            "refs/heads/origin/trunk",
            "refs/tags/origin/trunk",
            "refs/heads/refs/remotes/origin/trunk",
        ] {
            git(&repo, &["update-ref", name, &other]);
        }
        assert_eq!(resolve(&repo).await.map(|b| b.commit), Ok(tip.clone()));

        git(&repo, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
        assert!(resolve(&repo).await.is_err(), "the look-alikes do not stand in for it");
    }

    /// No remote named `origin`, whether a local-only repository or one
    /// whose remote has another name, and no project base: refused, saying
    /// to choose a base branch. A remote of another name is never read.
    #[tokio::test]
    async fn a_repository_without_origin_is_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        let refused = resolve(&repo).await.expect_err("local-only");
        assert!(refused.contains("no remote named origin"), "{refused}");

        let upstream = temp.path().join("upstream.git");
        git(&repo, &["init", "-q", "--bare", upstream.to_str().unwrap()]);
        git(&repo, &["remote", "add", "upstream", upstream.to_str().unwrap()]);
        git(&repo, &["push", "-q", "upstream", "main"]);
        git(&repo, &["remote", "set-head", "upstream", "main"]);
        let refused = resolve(&repo).await.expect_err("another remote");
        assert!(refused.contains("no remote named origin"), "{refused}");
    }

    fn local(branch: &str) -> ProjectBase {
        ProjectBase::LocalBranch { branch: branch.to_string() }
    }

    async fn resolve_with(repo: &Path, base: &ProjectBase) -> Result<ResolvedBase, String> {
        resolve_default_base(repo.to_str().unwrap(), Some(base)).await
    }

    /// A local-only repository starts tasks from the project's base branch,
    /// at the commit it names now, wherever the primary checkout has gone.
    #[tokio::test]
    async fn a_local_only_repository_starts_from_the_project_base() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "trunk-xyz"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        let tip = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "feature work"]);

        let base = resolve_with(&repo, &local("trunk-xyz")).await.expect("local base");
        assert_eq!(
            base,
            ResolvedBase { branch: "trunk-xyz".to_string(), commit: tip, source: BaseSource::Local }
        );
        assert_eq!(base.branch_origin(), BranchOrigin::LocalBase { branch: "trunk-xyz".to_string() });

        let refused = resolve_with(&repo, &local("gone")).await.expect_err("missing base");
        assert!(refused.contains("does not exist"), "{refused}");
        let refused = resolve_with(&repo, &local("-evil")).await.expect_err("unsafe name");
        assert!(refused.contains("will not use it"), "{refused}");
    }

    /// Origin's default branch wins over the project's local base whenever it
    /// can be read, so remote-backed projects start where they always did;
    /// with `origin/HEAD` gone the local base answers; with it malformed the
    /// task is refused rather than quietly falling back.
    #[tokio::test]
    async fn origin_head_takes_precedence_over_the_project_base() {
        let (_temp, repo, tip) = cloned_repo();
        git(&repo, &["checkout", "-q", "-b", "local-base"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "local only"]);
        let local_tip = git(&repo, &["rev-parse", "HEAD"]);
        let base = local("local-base");

        assert_eq!(resolve_with(&repo, &base).await.map(|b| (b.commit, b.source)), Ok((tip, BaseSource::Remote)));

        git(&repo, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
        assert_eq!(resolve_with(&repo, &base).await.map(|b| (b.commit, b.source)), Ok((local_tip.clone(), BaseSource::Local)));

        git(&repo, &["update-ref", "refs/remotes/upstream/trunk", &local_tip]);
        git(&repo, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/upstream/trunk"]);
        let refused = resolve_with(&repo, &base).await.expect_err("origin/HEAD outside origin");
        assert!(refused.contains("not at a branch of origin"), "{refused}");
    }

    /// No version control, a Jujutsu repository without Git colocation, and
    /// a repository with no commit are each refused saying what to do.
    #[tokio::test]
    async fn folders_that_cannot_hold_a_task_checkout_are_refused_with_what_to_do() {
        let temp = tempfile::TempDir::new().unwrap();
        let plain = temp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let refused = resolve_with(&plain, &local("trunk")).await.expect_err("no vcs");
        assert!(refused.contains("not under version control"), "{refused}");

        std::fs::create_dir_all(plain.join(".jj")).unwrap();
        let refused = resolve_with(&plain, &local("trunk")).await.expect_err("jj without git");
        assert!(refused.contains("not colocated with Git"), "{refused}");

        let empty = temp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        git(&empty, &["init", "-q"]);
        let refused = resolve_default_base(empty.to_str().unwrap(), None).await.expect_err("no commit");
        assert!(refused.contains("no commits yet"), "{refused}");
    }

    /// The JJ fallback, with a fake `jj` answering `config get` with
    /// `answer`, in a repository whose `origin/HEAD` is gone.
    #[cfg(unix)]
    async fn with_jj_answering(answer: &str) -> (Result<ResolvedBase, String>, String, String) {
        let fake = crate::test_helpers::FakeProgram::install("jj", &format!("printf '%s\\n' '{answer}'")).await;
        let (_temp, repo, tip) = cloned_repo();
        std::fs::create_dir_all(repo.join(".jj")).unwrap();
        git(&repo, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
        let resolved = resolve(&repo).await;
        (resolved, fake.invocations(), tip)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn jj_trunk_naming_a_fetched_origin_branch_is_the_default_base() {
        let (resolved, invocations, tip) = with_jj_answering("trunk@origin").await;
        assert_eq!(resolved, Ok(ResolvedBase { branch: "trunk".to_string(), commit: tip, source: BaseSource::Remote }));
        assert_eq!(
            invocations,
            "jj --ignore-working-copy --color=never config get revset-aliases.\"trunk()\"\n",
            "only the configuration is read, without snapshotting the working copy"
        );
    }

    /// Anything but exactly `<branch>@origin` for a fetched branch is not a
    /// default base: JJ's built-in expression, another remote, a revset, a
    /// bookmark with no remote-tracking ref, or no answer at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn any_other_jj_trunk_is_refused() {
        for answer in [
            r#"latest((present(trunk@origin) | present(main@origin)) & remote_bookmarks())"#,
            "trunk@upstream",
            "trunk",
            "present(trunk@origin)",
            "trunk@origin | main@origin",
            "dev@origin",
            "-evil@origin",
            "",
        ] {
            let (resolved, _, _) = with_jj_answering(answer).await;
            let refused = resolved.expect_err(answer);
            assert!(refused.contains("trunk() alias did not name one"), "{answer:?}: {refused}");
        }
    }

    /// A `refs/remotes/origin/<D>` that JJ's `trunk()` names but that no
    /// branch can start at -- itself a symbolic ref, a commit the repository
    /// does not have, or a tag object -- is refused saying so, not with the
    /// message for a branch that was never fetched.
    #[cfg(unix)]
    #[tokio::test]
    async fn jj_trunk_naming_an_unusable_origin_ref_is_refused_with_the_reason() {
        type Arrange = fn(&Path);
        let cases: [(&str, Arrange, &str); 3] = [
            ("symbolic ref", |repo| {
                git(repo, &["symbolic-ref", "refs/remotes/origin/alias", "refs/remotes/origin/trunk"]);
            }, "is itself a symbolic ref"),
            ("missing commit", |repo| {
                std::fs::write(
                    repo.join(".git/refs/remotes/origin/alias"),
                    "0123456789abcdef0123456789abcdef01234567\n",
                )
                .unwrap();
            }, "names a commit this repository does not have"),
            ("tag object", |repo| {
                git(repo, &["tag", "-a", "-m", "annotated", "v1"]);
                let tag = git(repo, &["rev-parse", "refs/tags/v1"]);
                git(repo, &["update-ref", "refs/remotes/origin/alias", &tag]);
            }, "names a tag object, not a commit"),
        ];
        let fake = crate::test_helpers::FakeProgram::install("jj", "printf 'alias@origin\\n'").await;
        for (name, arrange, why) in cases {
            let (_temp, repo, _) = cloned_repo();
            std::fs::create_dir_all(repo.join(".jj")).unwrap();
            git(&repo, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
            arrange(&repo);

            let refused = resolve(&repo).await.expect_err(name);
            assert!(refused.contains("trunk() alias names alias@origin"), "{name}: {refused}");
            assert!(refused.contains(&format!("refs/remotes/origin/alias {why}")), "{name}: {refused}");
            assert!(!refused.contains("has not been fetched"), "{name}: {refused}");
        }
        assert!(fake.invocations().contains("config get"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_jj_that_fails_or_is_absent_answers_nothing() {
        let fake = crate::test_helpers::FakeProgram::install(
            "jj",
            "echo 'Config error: Value not found' >&2; exit 1",
        )
        .await;
        let (_temp, repo, _) = cloned_repo();
        std::fs::create_dir_all(repo.join(".jj")).unwrap();
        git(&repo, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
        assert!(resolve(&repo).await.is_err());
        drop(fake);

        let _absent = crate::test_helpers::FakeProgram::without(&["wt", "jj"]).await;
        let refused = resolve(&repo).await.expect_err("jj is not installed");
        assert!(refused.contains("git remote set-head origin --auto"), "{refused}");
    }

    /// JJ is asked only in a JJ repository, and only when `origin/HEAD`
    /// does not answer.
    #[cfg(unix)]
    #[tokio::test]
    async fn jj_is_consulted_only_when_it_is_needed() {
        let fake = crate::test_helpers::FakeProgram::install("jj", "printf 'trunk@origin\\n'").await;

        let (_temp, repo, tip) = cloned_repo();
        std::fs::create_dir_all(repo.join(".jj")).unwrap();
        assert_eq!(resolve(&repo).await.map(|b| b.commit), Ok(tip));

        let (_temp, repo, _) = cloned_repo();
        git(&repo, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
        assert!(resolve(&repo).await.is_err(), "not a JJ repository");

        assert_eq!(fake.invocations(), "");
    }
}
