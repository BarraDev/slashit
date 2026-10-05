//! A stand-in for the `gh` executable, so a journey can open a pull request
//! without GitHub, a network or a credential.
//!
//! The same seam as [`crate::fake_agent`]: SlashIt starts `gh` by name, so a
//! link named `gh` in the fake agent's directory, which is first on the
//! application's `PATH`, is what it runs. The script is checked in and only
//! ever linked to, for the reason [`crate::fake_agent`] gives.
//!
//! It answers the calls SlashIt's pull request flow makes (`pr list`,
//! `pr create`, `pr view`, `repo view`), records every invocation, and keeps
//! each pull request it opened as a file named after its branch. What
//! `pr view` says can be scripted, for journeys about a pull request's status.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::fake_agent::{resolve_on_path, FakeAgent};

const EXECUTABLE: &str = "gh";

/// The variable naming the directory the script records into.
pub const DIR_VAR: &str = "SLASHIT_FAKE_GH_DIR";

/// The repository the fixture pretends to be, as an `origin` URL.
pub const REPOSITORY_URL: &str = "https://github.com/slashit-acceptance/fixture.git";

/// The file whose presence makes `pr create` fail.
const FAIL_CREATE: &str = "fail-create";

/// The file whose contents, a number of seconds, `pr create` sleeps first.
const DELAY_CREATE: &str = "delay-create";

/// The file whose contents `pr view` prints instead of its default answer.
const PR_VIEW: &str = "pr-view.json";

/// The file whose contents, a number of seconds, `pr view` sleeps first.
const DELAY_VIEW: &str = "delay-view";

/// What `pr create` prints on stderr when scripted to fail.
pub const CREATE_FAILURE: &str = "creating the pull request was scripted to fail";

pub struct FakeGh {
    dir: PathBuf,
}

impl FakeGh {
    /// Link the fixture beside `agent`'s, so the `PATH` the agent already
    /// gives the application resolves `gh` to it.
    pub fn install(root: &Path, agent: &FakeAgent) -> Result<Self> {
        let dir = root.join("gh-invocations");
        std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        let bin_dir = agent
            .executable()
            .parent()
            .context("the fake agent has no directory")?;
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(EXECUTABLE);
        let executable = bin_dir.join(EXECUTABLE);
        std::os::unix::fs::symlink(&script, &executable)
            .with_context(|| format!("could not point {} at {}", executable.display(), script.display()))?;
        match resolve_on_path(EXECUTABLE, agent.path_value()) {
            Some(found) if found == executable => {}
            other => bail!(
                "the composed PATH resolves {EXECUTABLE} to {other:?} instead of the fixture at {} \
                 -- refusing to run a journey that could reach the real GitHub",
                executable.display()
            ),
        }
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Make every following `pr create` fail, or succeed again.
    pub fn fail_pr_creation(&self, fail: bool) -> Result<()> {
        let marker = self.dir.join(FAIL_CREATE);
        if fail {
            std::fs::write(&marker, "").with_context(|| format!("could not write {}", marker.display()))
        } else {
            match std::fs::remove_file(&marker) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).with_context(|| format!("could not remove {}", marker.display())),
            }
        }
    }

    /// Make every following `pr create` take `seconds` before answering, or
    /// answer at once again with `None`.
    pub fn delay_pr_creation(&self, seconds: Option<u32>) -> Result<()> {
        let marker = self.dir.join(DELAY_CREATE);
        match seconds {
            Some(seconds) => std::fs::write(&marker, seconds.to_string())
                .with_context(|| format!("could not write {}", marker.display())),
            None => match std::fs::remove_file(&marker) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).with_context(|| format!("could not remove {}", marker.display())),
            },
        }
    }

    /// Make every following `pr view` print `answer`, the way GitHub would
    /// describe the pull request now. Replaced whole, so a `pr view` running
    /// at the same moment reads either the old answer or the new one.
    pub fn script_pr_view(&self, answer: &serde_json::Value) -> Result<()> {
        let target = self.dir.join(PR_VIEW);
        let staged = self.dir.join(format!("{PR_VIEW}.partial"));
        std::fs::write(&staged, answer.to_string()).with_context(|| format!("could not write {}", staged.display()))?;
        std::fs::rename(&staged, &target).with_context(|| format!("could not replace {}", target.display()))
    }

    /// Make `pr view <number>` print `answer`, whatever [`Self::script_pr_view`]
    /// says for every other pull request.
    pub fn script_pr_view_of(&self, number: u64, answer: &serde_json::Value) -> Result<()> {
        self.replace(&format!("pr-view-{number}.json"), &answer.to_string())
    }

    /// Make `pr list --head <branch>` print `answer`, a JSON array the way
    /// `gh pr list --json` prints one, instead of the pull request this fake
    /// opened for the branch.
    pub fn script_pr_list_of(&self, branch: &str, answer: &serde_json::Value) -> Result<()> {
        let directory = self.dir.join("pr-list");
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        self.replace(&format!("pr-list/{branch}.json"), &answer.to_string())
    }

    /// Make the REST listing of pull request `number`'s commits print
    /// `commits`, one object id per line.
    pub fn script_pr_commits(&self, number: u64, commits: &[String]) -> Result<()> {
        let directory = self.dir.join("pr-commits");
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        self.replace(&format!("pr-commits/{number}"), &format!("{}\n", commits.join("\n")))
    }

    /// Write `name` under the fixture's directory whole, so a `gh` running at
    /// the same moment reads the old contents or the new, never half.
    fn replace(&self, name: &str, contents: &str) -> Result<()> {
        let target = self.dir.join(name);
        let staged = self.dir.join(format!("{name}.partial"));
        std::fs::write(&staged, contents).with_context(|| format!("could not write {}", staged.display()))?;
        std::fs::rename(&staged, &target).with_context(|| format!("could not replace {}", target.display()))
    }

    /// Make every following `pr view` take `seconds` before answering, or
    /// answer at once again with `None`.
    pub fn delay_pr_view(&self, seconds: Option<u32>) -> Result<()> {
        let marker = self.dir.join(DELAY_VIEW);
        match seconds {
            Some(seconds) => std::fs::write(&marker, seconds.to_string())
                .with_context(|| format!("could not write {}", marker.display())),
            None => match std::fs::remove_file(&marker) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).with_context(|| format!("could not remove {}", marker.display())),
            },
        }
    }

    /// How many times `gh pr view` was run.
    pub fn view_count(&self) -> Result<usize> {
        Ok(self
            .invocations()?
            .iter()
            .filter(|args| args.first().map(String::as_str) == Some("pr") && args.get(1).map(String::as_str) == Some("view"))
            .count())
    }

    /// Every recorded invocation's arguments, in start order.
    pub fn invocations(&self) -> Result<Vec<Vec<String>>> {
        let mut numbered = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(number) = name.strip_prefix("invocation-").and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let args = std::fs::read_to_string(entry.path())?
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            numbered.push((number, args));
        }
        numbered.sort_by_key(|(n, _)| *n);
        Ok(numbered.into_iter().map(|(_, args)| args).collect())
    }

    /// How many times `gh pr create` was run, failed or not.
    pub fn create_attempts(&self) -> Result<usize> {
        Ok(self
            .invocations()?
            .iter()
            .filter(|args| args.first().map(String::as_str) == Some("pr") && args.get(1).map(String::as_str) == Some("create"))
            .count())
    }

    /// The pull request opened for `branch`, if one was.
    pub fn pr_for(&self, branch: &str) -> Result<Option<String>> {
        match std::fs::read_to_string(self.dir.join("prs").join(branch)) {
            Ok(url) => Ok(Some(url)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
