//! The default base an ordinary task branch starts from.
//!
//! Resolved once, before the branch exists, from local refs only: nothing
//! here fetches, runs `git remote set-head --auto`, or asks GitHub. What is
//! resolved is one default branch `D` and the exact commit
//! `refs/remotes/origin/<D>` names, which is then where the branch is
//! created and what the task records as its base. The primary checkout's
//! `HEAD` is never a substitute: it is wherever the user happens to be,
//! which may be a feature branch or, in a JJ-colocated repository, a
//! detached commit holding work nobody pushed.
//!
//! In order:
//!
//! 1. `refs/remotes/origin/HEAD`, read as the exact ref, when it is a
//!    symbolic ref to `refs/remotes/origin/<D>` and that ref names a commit
//!    the repository has. This is what `git clone` records, and what
//!    `git remote set-head origin` records later.
//! 2. When that ref is missing, dangling or not symbolic, and the repository
//!    is also a JJ repository (`<repo>/.jj`), JJ's `trunk()` alias, but only
//!    when it is exactly `<D>@origin` and `refs/remotes/origin/<D>` resolves
//!    as above. Anything else it can be -- the built-in default expression,
//!    another remote, a revset -- is not a branch SlashIt can name. `jj` not
//!    being installed is the same as this step not answering.
//! 3. Otherwise the task is refused with a message saying what to run. A
//!    repository whose remote is not called `origin`, a fork layout, and a
//!    repository with no remote at all are not supported.

use super::checked_task_branch;
use super::restack::{exact_ref, has_commit};
use std::path::Path;

/// The default branch an ordinary task branch starts from, and the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBase {
    /// `D`, the branch `refs/remotes/origin/<D>` tracks on `origin`. A
    /// branch name [`checked_task_branch`] accepts.
    pub branch: String,
    /// The full object ID of the commit `refs/remotes/origin/<D>` named when
    /// it was resolved.
    pub commit: String,
}

const ORIGIN_HEAD: &str = "refs/remotes/origin/HEAD";
const ORIGIN_BRANCHES: &str = "refs/remotes/origin/";

/// What `refs/remotes/origin/HEAD` is, read as that exact ref.
enum OriginHead {
    /// A symbolic ref to `refs/remotes/origin/<D>`, which exists.
    Branch(String),
    /// A symbolic ref to a ref outside `refs/remotes/origin/`.
    Elsewhere(String),
    /// Not there, dangling, or not a symbolic ref.
    Missing,
}

/// Resolve the default base of the repository at `repo_path`, or say why
/// there is none SlashIt can use. See the module documentation.
pub async fn resolve_default_base(repo_path: &str) -> Result<ResolvedBase, String> {
    let repo = Path::new(repo_path);
    match origin_head(repo).await? {
        OriginHead::Branch(branch) => {
            let branch = checked_task_branch(&branch).map_err(|e| {
                format!(
                    "{ORIGIN_HEAD} in {repo_path} names a default branch SlashIt cannot use: \
                     {e}"
                )
            })?;
            match at_origin_branch(repo, branch).await? {
                OriginBranch::Found(base) => return Ok(base),
                OriginBranch::Unusable(why) => {
                    return Err(format!(
                        "{ORIGIN_HEAD} in {repo_path} names {ORIGIN_BRANCHES}{branch}, which \
                         {why}, so a task's branch cannot start there. Fetch origin again, or \
                         run `git remote set-head origin <default branch>` there, then start \
                         the task again."
                    ));
                }
                OriginBranch::Missing => {}
            }
        }
        OriginHead::Elsewhere(target) => {
            return Err(format!(
                "{ORIGIN_HEAD} in {repo_path} points at {target}, not at a branch of origin. \
                 SlashIt starts a task's branch from origin's default branch and does not \
                 support other layouts. Run `git remote set-head origin <default branch>` \
                 there, then start the task again."
            ));
        }
        OriginHead::Missing => {}
    }

    if repo.join(".jj").is_dir() {
        if let Some(branch) = jj_trunk_branch(repo).await {
            if let OriginBranch::Found(base) = at_origin_branch(repo, &branch).await? {
                return Ok(base);
            }
        }
    }

    Err(refusal(repo, repo_path).await)
}

/// What `for-each-ref` says about exactly `refname`.
enum Listed {
    /// Not listed: absent, or a symbolic ref whose target is not there.
    Absent,
    /// A ref naming an object directly.
    Direct,
    /// A symbolic ref to the ref named.
    Symbolic(String),
}

/// Read `refname` as that exact ref.
///
/// `for-each-ref` matches the full ref name only, never a branch or tag that
/// merely carries the same name, and does not list a symbolic ref whose
/// target is not there. Its pattern also matches refs below `refname/`, so
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
async fn origin_head(repo: &Path) -> Result<OriginHead, String> {
    Ok(match listed(repo, ORIGIN_HEAD).await? {
        Listed::Absent | Listed::Direct => OriginHead::Missing,
        Listed::Symbolic(target) => match target.strip_prefix(ORIGIN_BRANCHES) {
            Some(branch) => OriginHead::Branch(branch.to_string()),
            None => OriginHead::Elsewhere(target),
        },
    })
}

/// What `refs/remotes/origin/<branch>` is.
enum OriginBranch {
    /// A ref naming a commit object the repository has.
    Found(ResolvedBase),
    /// There is no such ref.
    Missing,
    /// There is, but no branch can start at it, for the reason given.
    Unusable(&'static str),
}

/// Read `refs/remotes/origin/<branch>` as that exact ref. Only a ref that
/// names a commit object directly, which the repository has, is a base; a
/// symbolic ref is not, since it says nothing about origin's own branch.
async fn at_origin_branch(repo: &Path, branch: &str) -> Result<OriginBranch, String> {
    let branch = checked_task_branch(branch)?;
    let refname = format!("{ORIGIN_BRANCHES}{branch}");
    match listed(repo, &refname).await? {
        Listed::Absent => return Ok(OriginBranch::Missing),
        Listed::Symbolic(_) => return Ok(OriginBranch::Unusable("is itself a symbolic ref")),
        Listed::Direct => {}
    }
    let Some(commit) = exact_ref(repo, &refname).await? else {
        return Ok(OriginBranch::Missing);
    };
    if !has_commit(repo, &commit).await? {
        return Ok(OriginBranch::Unusable("names a commit this repository does not have"));
    }
    if !is_commit_object(repo, &commit).await? {
        return Ok(OriginBranch::Unusable("names a tag object, not a commit"));
    }
    Ok(OriginBranch::Found(ResolvedBase { branch: branch.to_string(), commit }))
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
///
/// `--ignore-working-copy` keeps JJ from snapshotting the primary checkout.
/// Reading the configuration can still move a repository's legacy
/// configuration into JJ's per-user configuration directory, which is JJ's
/// own migration and harmless. Any failure, including `jj` not being on
/// `PATH`, answers `None`.
async fn jj_trunk_branch(repo: &Path) -> Option<String> {
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
    let value = value.strip_suffix('\n').unwrap_or(&value);
    let branch = value.strip_suffix("@origin")?;
    checked_task_branch(branch).ok().map(str::to_string)
}

/// Why no default base could be resolved, and what to run about it.
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
    if has_origin {
        format!(
            "SlashIt could not tell which branch of origin a new task branch should start \
             from in {repo_path}: {ORIGIN_HEAD} is not set, or names a branch that has not \
             been fetched.{jj} Run `git remote set-head origin --auto` (or `git remote \
             set-head origin <default branch>`) there, then start the task again. SlashIt \
             does not fetch or ask the network on its own."
        )
    } else {
        format!(
            "{repo_path} has no remote named origin, so there is no default branch to start a \
             task's branch from.{jj} SlashIt does not support local-only repositories or \
             remotes with another name. Add the remote as origin, fetch it, and run `git remote \
             set-head origin --auto`, then start the task again."
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
        resolve_default_base(repo.to_str().unwrap()).await
    }

    #[tokio::test]
    async fn origin_head_names_the_default_branch_and_its_exact_commit() {
        let (_temp, repo, tip) = cloned_repo();
        // Where the primary checkout is plays no part.
        git(&repo, &["checkout", "-q", "--detach"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "local"]);

        assert_eq!(
            resolve(&repo).await,
            Ok(ResolvedBase { branch: "trunk".to_string(), commit: tip })
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
            Ok(ResolvedBase { branch: "release/v2".to_string(), commit: tip })
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
    /// whose remote has another name, is refused as unsupported.
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
        assert_eq!(resolved, Ok(ResolvedBase { branch: "trunk".to_string(), commit: tip }));
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

    /// A `refs/remotes/origin/<D>` that is itself a symbolic ref says
    /// nothing about origin's own branch, and is not a base the JJ fallback
    /// accepts either.
    #[cfg(unix)]
    #[tokio::test]
    async fn jj_trunk_naming_a_symbolic_origin_ref_is_refused() {
        let fake = crate::test_helpers::FakeProgram::install("jj", "printf 'alias@origin\\n'").await;
        let (_temp, repo, _) = cloned_repo();
        std::fs::create_dir_all(repo.join(".jj")).unwrap();
        git(&repo, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
        git(&repo, &["symbolic-ref", "refs/remotes/origin/alias", "refs/remotes/origin/trunk"]);

        let refused = resolve(&repo).await.expect_err("a symbolic ref is not a base");
        assert!(refused.contains("trunk() alias did not name one"), "{refused}");
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
