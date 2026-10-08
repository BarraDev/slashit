//! Test-only Git fixture shared by the desktop journeys and ordinary helper tests.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// The smallest real git repository the product will accept as a project.
///
/// A real one, not a mock: the executor runs `git worktree add`, `git add` and
/// `git commit` against it, and the point of a product acceptance test is that
/// those are the actual commands that run.
pub(crate) struct GitFixture {
    pub(crate) path: PathBuf,
    pub(crate) state_root: PathBuf,
    /// This fixture's own bare `origin`, if it has one.
    pub(crate) origin: Option<PathBuf>,
}

impl GitFixture {
    pub(crate) fn create(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)
            .with_context(|| format!("could not create {}", path.display()))?;

        git(path, &["init", "--quiet", "--initial-branch=main"])?;
        // Repository-local, so the run depends on nothing about the machine's
        // global git configuration — and so the commit the executor makes
        // inside the worktree has an identity, since worktrees share this
        // configuration with the repository.
        git(path, &["config", "user.name", "SlashIt Acceptance"])?;
        git(path, &["config", "user.email", "acceptance@localhost"])?;
        // A developer with commit signing enabled globally would otherwise
        // have the product's `git commit` block on a passphrase prompt, and a
        // blocked commit inside the application is very hard to read from out
        // here.
        git(path, &["config", "commit.gpgsign", "false"])?;

        std::fs::write(path.join("README.md"), "Acceptance fixture repository.\n")
            .context("could not write the fixture's README")?;
        git(path, &["add", "README.md"])?;
        git(path, &["commit", "--quiet", "-m", "initial commit"])?;

        let state_root = path
            .parent()
            .context("the fixture has no parent directory")?
            .to_path_buf();

        // A bare `origin` beside it, inside the state root, with its `HEAD`
        // recorded locally the way `git clone` leaves it. The product starts
        // every ordinary task branch at `refs/remotes/origin/<default>` and
        // refuses a repository that has none, without ever fetching.
        //
        // Named after this fixture, because a journey can make several in one
        // state root, and taken with `create_dir`, which fails rather than
        // hand over a directory somebody already has. A shared origin would
        // receive every fixture's unrelated `main`, and all but the first
        // push would be rejected unless the commits happened to be identical.
        let name = path
            .file_name()
            .context("the fixture has no directory name")?
            .to_str()
            .context("the fixture's name is not UTF-8")?;
        if name.ends_with(".origin.git") {
            bail!("{name} is where another fixture would keep its origin");
        }
        let origin = state_root.join(format!("{name}.origin.git"));
        std::fs::create_dir(&origin)
            .with_context(|| format!("could not create {}", origin.display()))?;
        let url = origin.to_str().context("the origin path is not UTF-8")?;
        git(path, &["init", "--quiet", "--bare", url])?;
        git(path, &["remote", "add", "origin", url])?;
        git(path, &["push", "--quiet", "origin", "main"])?;
        git(path, &["remote", "set-head", "origin", "main"])?;

        Ok(Self {
            path: path.to_path_buf(),
            state_root,
            origin: Some(origin),
        })
    }

    pub(crate) fn path(&self) -> String {
        self.path.to_string_lossy().to_string()
    }

    pub(crate) fn path_buf(&self) -> PathBuf {
        self.path.clone()
    }

    /// The run's state root, which everything this journey creates — including
    /// the worktrees the product makes — has to stay inside.
    pub(crate) fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Whether the repository still has `branch`.
    ///
    /// A task's branch is where the work of its executions lives, so a journey
    /// asking this is asking whether that work survived. `rev-parse --verify`
    /// answers with its exit status alone, which is why this does not go
    /// through [`git`], whose job is to fail the journey when a command fails.
    pub(crate) fn has_branch(&self, branch: &str) -> Result<bool> {
        let status = std::process::Command::new("git")
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .with_context(|| format!("could not look up branch {branch}"))?;
        Ok(status.status.success())
    }

    /// Whether git still has a worktree registration for `path`.
    ///
    /// The directory being gone and git having forgotten it are two different
    /// facts, and a destructive journey needs both: `git worktree remove`
    /// deletes a directory *and* a record under `.git/worktrees`, and a
    /// cleanup that left the record behind would leave the repository unable
    /// to give this branch a worktree again.
    pub(crate) fn registers_worktree(&self, path: &Path) -> Result<bool> {
        let output = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .context("could not list the fixture's worktrees")?;
        if !output.status.success() {
            bail!(
                "git worktree list failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let wanted = resolve(path);
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .any(|listed| resolve(Path::new(listed)) == wanted))
    }

    /// The commit `branch` points at, or `None` if there is no such branch.
    pub(crate) fn branch_tip(&self, branch: &str) -> Result<Option<String>> {
        let output = std::process::Command::new("git")
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .with_context(|| format!("could not resolve branch {branch}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    }

    /// The contents of `file` as `revision` has it, or `None` if either the
    /// revision or the path is not there.
    ///
    /// This is what separates work that is merely on disk from work that is
    /// committed: a file only in the worktree dies with the directory, while
    /// one in the commit survives for as long as something still points at
    /// that commit.
    pub(crate) fn file_at(&self, revision: &str, file: &str) -> Result<Option<String>> {
        let output = std::process::Command::new("git")
            .args(["show", &format!("{revision}:{file}")])
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .with_context(|| format!("could not read {file} at {revision}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&output.stdout).to_string()))
    }

    /// Every ref in the repository from which `commit` can still be reached.
    ///
    /// Empty means the commit is unreachable: still in the object database
    /// until git collects it, but with nothing naming it, and nothing in the
    /// product able to find it again. That is the question a destructive
    /// journey is really asking about the work an agent produced.
    pub(crate) fn refs_reaching(&self, commit: &str) -> Result<Vec<String>> {
        let output = std::process::Command::new("git")
            .args(["for-each-ref", "--contains", commit, "--format=%(refname)"])
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .with_context(|| format!("could not look for refs reaching {commit}"))?;
        if !output.status.success() {
            bail!(
                "git for-each-ref --contains failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect())
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("could not run git {args:?} in {}", dir.display()))?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn resolve(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(all(test, not(feature = "run-acceptance")))]
mod tests {
    use super::GitFixture;
    use anyhow::{bail, Context, Result};
    use std::path::{Path, PathBuf};

    /// A private parent for one test, so tests running in parallel in this
    /// process, or in another one, never look at each other's fixtures.
    fn scratch(label: &str) -> PathBuf {
        let parent = std::env::temp_dir().join(format!(
            "slashit-git-fixture-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).expect("create the scratch parent");
        parent
    }

    /// Run a read-only git query in `dir` and return its trimmed output.
    fn query(dir: &Path, args: &[&str]) -> Result<String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .with_context(|| format!("could not run git {args:?}"))?;
        if !output.status.success() {
            bail!(
                "git {args:?} failed in {}: {}",
                dir.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Where `fixture` pushes, and whether what is there is its own `main`.
    ///
    /// Asked of git rather than of the fixture, so the answer is what the product
    /// will actually see.
    fn own_origin(fixture: &Path) -> Result<PathBuf> {
        let origin = PathBuf::from(query(fixture, &["remote", "get-url", "--push", "origin"])?);
        let local = query(fixture, &["rev-parse", "refs/heads/main"])?;
        let pushed = query(&origin, &["rev-parse", "refs/heads/main"])?;
        if local != pushed {
            bail!(
                "{} holds {pushed} as main, but {} has {local}",
                origin.display(),
                fixture.display()
            );
        }
        Ok(origin)
    }

    /// Fixtures created at once, in one state root, as fast as threads can go:
    /// every one of them is created, and every one pushes to an origin no other
    /// fixture shares.
    #[test]
    fn fixtures_created_together_in_one_state_root_each_get_their_own_origin() {
        const FIXTURES: usize = 8;
        let parent = scratch("together");

        let fixtures: Vec<PathBuf> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..FIXTURES)
                .map(|index| {
                    let path = parent.join(format!("fixture-repo-{index}"));
                    scope.spawn(move || {
                        GitFixture::create(&path).expect("create a fixture");
                        path
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread"))
                .collect()
        });

        let origins: std::collections::BTreeSet<PathBuf> = fixtures
            .iter()
            .map(|fixture| own_origin(fixture).expect("the fixture's own origin"))
            .collect();
        assert_eq!(
            origins.len(),
            FIXTURES,
            "fixtures in one state root were handed a shared origin: {origins:?}"
        );

        std::fs::remove_dir_all(&parent).expect("clean up");
    }

    /// The case hosted CI hit: a second fixture made after the clock has moved
    /// on commits a `main` unrelated to the first one's, and must still be able
    /// to push it.
    #[test]
    fn a_fixture_created_in_a_later_second_still_pushes_its_own_main() {
        let parent = scratch("later-second");

        let first = parent.join("fixture-repo");
        GitFixture::create(&first).expect("create the first fixture");
        // Commit timestamps have one-second resolution; past this, the second
        // fixture's initial commit cannot be byte-identical to the first's.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = parent.join("fixture-repo-elsewhere");
        GitFixture::create(&second).expect("create a second fixture beside the first");

        let first_origin = own_origin(&first).expect("the first fixture's own origin");
        let second_origin = own_origin(&second).expect("the second fixture's own origin");
        assert_ne!(first_origin, second_origin);

        std::fs::remove_dir_all(&parent).expect("clean up");
    }

    /// An origin already on disk belongs to whoever made it. A fixture that
    /// would land on it again is refused, rather than reinitialising it and
    /// pushing into somebody else's repository.
    #[test]
    fn a_fixture_never_takes_over_an_origin_that_already_exists() {
        let parent = scratch("reuse");

        let path = parent.join("fixture-repo");
        GitFixture::create(&path).expect("create the fixture");
        let origin = own_origin(&path).expect("the fixture's own origin");
        let before = query(&origin, &["rev-parse", "refs/heads/main"]).expect("origin main");

        // Only the working repository goes, so a new fixture at the same place
        // gets as far as its own origin before anything can stop it.
        std::fs::remove_dir_all(&path).expect("remove the working repository");
        // The refusal has to be the claim on the origin itself: failing anywhere
        // later, on a push the old origin happens to reject, would pass here
        // while still writing into a repository this fixture does not own.
        let refusal = match GitFixture::create(&path) {
            Ok(_) => panic!("a new fixture was handed the origin an earlier one left behind"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            refusal.contains(&format!("could not create {}", origin.display())),
            "refused for some other reason than the existing origin: {refusal}"
        );
        let after = query(&origin, &["rev-parse", "refs/heads/main"]).expect("origin main");
        assert_eq!(
            before, after,
            "the refused fixture still changed the origin"
        );

        std::fs::remove_dir_all(&parent).expect("clean up");
    }
}
