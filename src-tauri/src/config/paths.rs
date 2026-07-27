//! Single authoritative source for every persistent, runtime and worktree path.
//!
//! No other module may build a state path by hand. The reason is concrete: an
//! empty `.slashit/` directory used to be created inside every registered
//! workspace root by `WorkspaceRegistry::upsert`, because path construction was
//! scattered and nobody owned the question "where does this belong?". Routing
//! every path through [`AppPaths`] makes that class of mistake reviewable.
//!
//! # State classification
//!
//! Only one of these categories may ever live inside a user's project:
//!
//! | Category | Example | Relocatable |
//! |---|---|---|
//! | Shareable project state | kanban tasks, roadmap features | yes — [`StateLocation`] |
//! | Machine-local state | PTY scrollback, window layout | no — always `data_dir` |
//! | Runtime | daemon socket, PID file | no — always `runtime_dir` |
//! | Logs | agent transcripts, PR helper logs | no — always `data_dir` |
//! | Cache | derived/regenerable data | no — always `cache_dir` |
//! | Secrets | `AgentConfig::api_key`, daemon tokens | no — always `config_dir`, mode 0600 |
//!
//! Secrets are called out explicitly because `config.toml` already carries
//! `AgentConfig::api_key` (see `domain/project.rs`). That file must never become
//! relocatable into a project directory, where it would land in a user's git
//! history.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Component, Path, PathBuf};

/// Application identifiers used to derive OS directories.
///
/// These are deliberately frozen. `ProjectDirs::from("com", "barradev",
/// "slashit-app")` produces `~/.config/slashit-app` on Linux; changing the third
/// component would orphan every existing user's tasks and projects. The GUI
/// binary is renamed to `slashit-ui` separately — the *binary* name and the
/// *application data* name are independent, and only the former changes.
pub const QUALIFIER: &str = "com";
pub const ORGANIZATION: &str = "barradev";
pub const APPLICATION: &str = "slashit-app";

/// Directory name used when project state is stored inside the project.
pub const IN_PROJECT_DIR: &str = ".slashit";

/// Marker file written into a state directory so we can recognise ours.
///
/// Its presence is what makes an `.slashit/` directory "valid" for
/// [`StateLocation::Auto`], and what makes deleting a legacy empty directory
/// safe: we only ever remove a directory we can prove is empty.
pub const STATE_MARKER: &str = ".slashit-state.toml";

/// Where a project's *shareable* state is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateLocation {
    /// Outside the project, under the OS data directory. Keeps repositories
    /// pristine. Default for new projects.
    #[default]
    External,
    /// Inside the project at `<root>/.slashit/`. Opt-in, for teams who want the
    /// board committed to version control.
    InProject,
    /// `InProject` when a valid `.slashit/` already exists, else `External`.
    Auto,
}

impl StateLocation {
    /// `Auto`, as a function so it can be a `#[serde(default = ...)]`.
    ///
    /// Used by `Project::state_location` so that projects written before the
    /// field existed adopt whatever state directory they already have instead
    /// of jumping to the `External` default.
    pub fn auto() -> Self {
        Self::Auto
    }

    /// Resolve `Auto` against the filesystem. `External` and `InProject` are
    /// returned unchanged.
    pub fn resolve(self, project_root: &Path) -> ResolvedLocation {
        match self {
            Self::External => ResolvedLocation::External,
            Self::InProject => ResolvedLocation::InProject,
            Self::Auto => {
                if is_valid_in_project_dir(&project_root.join(IN_PROJECT_DIR)) {
                    ResolvedLocation::InProject
                } else {
                    ResolvedLocation::External
                }
            }
        }
    }
}

/// A [`StateLocation`] with `Auto` already resolved to a concrete choice.
///
/// Serializable because the settings UI shows the user where their state
/// *actually* is, which for `Auto` is only knowable after resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedLocation {
    External,
    InProject,
}

/// Who decides where a SlashIt-managed worktree is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreePlacement {
    /// Delegate to worktrunk (`wt`) when it is installed, otherwise behave as
    /// [`Self::Managed`].
    ///
    /// This is the default because `wt` is the user's own tool: it runs their
    /// configured hooks and honours their `worktree-path` template. `wt switch`
    /// has no target-path flag, so delegating means SlashIt cannot dictate the
    /// location — which is the point. Nothing lands inside the project either
    /// way.
    #[default]
    Auto,
    /// Always place worktrees under SlashIt's own external root, using plain
    /// `git worktree add` with an explicit path.
    Managed,
}

/// True when `dir` looks like a state directory SlashIt owns.
///
/// A directory counts as valid when it carries our marker file, or when it is
/// non-empty (a pre-marker directory from an earlier version). An empty
/// directory is explicitly *not* valid — that is the dead artefact this
/// architecture removes.
pub fn is_valid_in_project_dir(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    if dir.join(STATE_MARKER).is_file() {
        return true;
    }
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

/// A stable, collision-resistant identifier for a project directory.
///
/// Format: `<sanitised-directory-name>-<first 8 hex of sha256(absolute path)>`.
///
/// The hash is taken over the path, not over a git remote, on purpose:
///
/// - a fork and its upstream share a remote-derived key but are different
///   checkouts, so a remote-derived key would make them collide;
/// - repositories with no remote at all still need a key;
/// - two checkouts of the same repository (`app` and `app-review`) must not
///   share worktrees or task state.
///
/// The trade-off is that moving a project directory changes its key. That is
/// handled explicitly by [`ProjectKey::for_path`]'s caller: the workspace
/// registry stores the key alongside the path, so a moved project keeps its old
/// key until the user migrates it. Deriving a fresh key on every load would
/// silently orphan state instead.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectKey(String);

impl ProjectKey {
    /// Derive a key from a project root path.
    ///
    /// Canonicalises when possible. When canonicalisation fails — the directory
    /// does not exist yet, or is behind a broken symlink, or permissions deny
    /// the lookup — falls back to lexical absolutisation so the key stays
    /// deterministic instead of erroring. Both branches are recorded in
    /// [`KeyDerivation::canonicalized`] so callers can warn.
    pub fn for_path(root: &Path) -> KeyDerivation {
        let (path, canonicalized) = match root.canonicalize() {
            Ok(p) => (p, true),
            Err(_) => (absolutize(root), false),
        };

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let name = sanitize_component(&name, "project");

        let digest = Sha256::digest(path_bytes(&path));
        let hash = hex::encode(&digest[..4]);

        KeyDerivation {
            key: ProjectKey(format!("{name}-{hash}")),
            canonical_path: path,
            canonicalized,
        }
    }

    /// Reconstruct a key that was previously persisted.
    pub fn from_stored(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Result of deriving a [`ProjectKey`], including whether the path resolved.
#[derive(Debug, Clone)]
pub struct KeyDerivation {
    pub key: ProjectKey,
    pub canonical_path: PathBuf,
    /// `false` when the path could not be canonicalised (missing directory,
    /// permission error). The key is still deterministic, but it is derived
    /// from a lexical path and may differ from the key the same directory gets
    /// once it exists.
    pub canonicalized: bool,
}

/// Every root directory SlashIt uses, resolved once at startup.
#[derive(Debug, Clone)]
pub struct AppPaths {
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    runtime_dir: PathBuf,
}

impl AppPaths {
    /// Resolve OS directories and create the ones we always need.
    pub fn new() -> io::Result<Self> {
        let dirs = directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
            .ok_or_else(|| io::Error::other("could not determine OS project directories"))?;

        let runtime_dir = dirs
            .runtime_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::temp_dir().join("slashit"));

        let paths = Self {
            config_dir: dirs.config_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
            cache_dir: dirs.cache_dir().to_path_buf(),
            runtime_dir,
        };
        paths.ensure_base_dirs()?;
        Ok(paths)
    }

    /// Build with explicit roots. Used by tests to stay inside a tempdir.
    pub fn with_roots(
        config_dir: PathBuf,
        data_dir: PathBuf,
        cache_dir: PathBuf,
        runtime_dir: PathBuf,
    ) -> Self {
        Self {
            config_dir,
            data_dir,
            cache_dir,
            runtime_dir,
        }
    }

    fn ensure_base_dirs(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::create_dir_all(&self.data_dir)?;
        Ok(())
    }

    // --- Roots -------------------------------------------------------------

    /// Global configuration. Holds `config.toml`, which contains
    /// `AgentConfig::api_key` — never relocate this into a project.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Machine-local data: external project state, worktrees, PTY scrollback, logs.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Runtime files: daemon socket and PID file. Never inside a project, and
    /// never inside `data_dir` — these must not survive a reboot.
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    // --- Global files ------------------------------------------------------

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn workspaces_file(&self) -> PathBuf {
        self.config_dir.join("workspaces.toml")
    }

    /// Daemon credentials. Separate from `config.toml` so it can be locked to
    /// 0600 independently and excluded from any future config export.
    pub fn credentials_file(&self) -> PathBuf {
        self.config_dir.join("credentials.toml")
    }

    pub fn feature_flags_file(&self) -> PathBuf {
        self.config_dir.join("features.toml")
    }

    // --- Runtime -----------------------------------------------------------

    pub fn socket_path(&self) -> PathBuf {
        self.runtime_dir.join("slashit.sock")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.runtime_dir.join("slashitd.pid")
    }

    pub fn ensure_runtime_dir(&self) -> io::Result<&Path> {
        std::fs::create_dir_all(&self.runtime_dir)?;
        Ok(&self.runtime_dir)
    }

    // --- Logs --------------------------------------------------------------

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn pr_helper_logs_dir(&self) -> PathBuf {
        self.data_dir.join("pr-helper-logs")
    }

    // --- Project state -----------------------------------------------------

    /// External root for one project's shareable state.
    pub fn external_project_state(&self, key: &ProjectKey) -> PathBuf {
        self.data_dir.join("projects").join(key.as_str())
    }

    /// In-project root for one project's shareable state.
    pub fn in_project_state(project_root: &Path) -> PathBuf {
        project_root.join(IN_PROJECT_DIR)
    }

    /// Resolve where a project's shareable state lives right now.
    pub fn project_state_dir(
        &self,
        key: &ProjectKey,
        project_root: &Path,
        location: StateLocation,
    ) -> PathBuf {
        match location.resolve(project_root) {
            ResolvedLocation::External => self.external_project_state(key),
            ResolvedLocation::InProject => Self::in_project_state(project_root),
        }
    }

    // --- Worktrees ---------------------------------------------------------

    /// Root for all SlashIt-managed worktrees of one project.
    ///
    /// Keyed by [`ProjectKey`], so two repositories that merely share a
    /// directory name cannot collide.
    pub fn worktrees_root(&self, key: &ProjectKey) -> PathBuf {
        self.data_dir.join("worktrees").join(key.as_str())
    }

    /// Full path for one worktree.
    pub fn worktree_path(&self, key: &ProjectKey, branch: &str) -> PathBuf {
        self.worktrees_root(key).join(worktree_dir_name(branch))
    }
}

/// Directory name for a branch's worktree.
///
/// Branch names may contain `/`, which cannot appear in a single directory
/// component. Sanitising alone would make `feat/login` and `feat-login` collide,
/// so whenever sanitisation actually changes the name we append a short hash of
/// the *original* branch. Names that need no sanitisation stay readable and
/// stable.
pub fn worktree_dir_name(branch: &str) -> String {
    let sanitized = sanitize_component(branch, "branch");
    if sanitized == branch {
        return sanitized;
    }
    let digest = Sha256::digest(branch.as_bytes());
    format!("{sanitized}-{}", hex::encode(&digest[..3]))
}

/// Reduce an arbitrary string to a safe single path component.
///
/// Keeps `A-Za-z0-9._-`, replaces every other byte with `-`, collapses runs,
/// trims leading/trailing separators and dots, caps length, and rejects the
/// reserved names `.` and `..`. Returns `fallback` when nothing survives.
pub fn sanitize_component(input: &str, fallback: &str) -> String {
    const MAX: usize = 64;

    let mut out = String::with_capacity(input.len().min(MAX));
    let mut last_dash = false;
    for ch in input.chars() {
        let keep = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-');
        if keep {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }

    let trimmed = out.trim_matches(['-', '.']);
    let mut result: String = trimmed.chars().take(MAX).collect();
    let result_trimmed = result.trim_end_matches(['-', '.']);
    if result_trimmed.len() != result.len() {
        result = result_trimmed.to_string();
    }

    if result.is_empty() || result == "." || result == ".." {
        return fallback.to_string();
    }
    result
}

/// Make a path absolute without touching the filesystem.
///
/// Used when `canonicalize` fails. Resolves `.` and `..` lexically; a `..` that
/// would escape the root is dropped, matching `Path::components` semantics.
fn absolutize(path: &Path) -> PathBuf {
    let base = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    };

    let mut out = base;
    for component in path.components() {
        match component {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => {
                out.push(Component::RootDir.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Bytes to hash for a path.
///
/// On Unix the raw OS bytes are used so that two distinct non-UTF-8 paths
/// cannot collide through lossy conversion. Other platforms fall back to the
/// lossy string, which is deterministic there because Windows paths are
/// UTF-16 and convert without loss in practice.
fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn paths_in(tmp: &TempDir) -> AppPaths {
        let root = tmp.path();
        AppPaths::with_roots(
            root.join("config"),
            root.join("data"),
            root.join("cache"),
            root.join("runtime"),
        )
    }

    #[test]
    fn project_key_is_stable_across_calls() {
        let tmp = TempDir::new().unwrap();
        let a = ProjectKey::for_path(tmp.path());
        let b = ProjectKey::for_path(tmp.path());
        assert_eq!(a.key, b.key);
        assert!(a.canonicalized);
    }

    #[test]
    fn project_key_differs_for_same_name_in_different_places() {
        let tmp = TempDir::new().unwrap();
        let one = tmp.path().join("a/myapp");
        let two = tmp.path().join("b/myapp");
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();

        let ka = ProjectKey::for_path(&one).key;
        let kb = ProjectKey::for_path(&two).key;

        assert_ne!(ka, kb, "same directory name in different parents must differ");
        assert!(ka.as_str().starts_with("myapp-"));
        assert!(kb.as_str().starts_with("myapp-"));
    }

    #[test]
    fn project_key_falls_back_when_path_missing() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let d = ProjectKey::for_path(&missing);
        assert!(!d.canonicalized, "missing path cannot be canonicalized");
        assert!(d.key.as_str().starts_with("does-not-exist-"));
        // Still deterministic.
        assert_eq!(d.key, ProjectKey::for_path(&missing).key);
    }

    #[test]
    fn project_key_survives_relative_paths() {
        let d = ProjectKey::for_path(Path::new("./nonexistent-rel-dir"));
        assert!(!d.canonicalized);
        assert!(d.canonical_path.is_absolute());
    }

    #[test]
    fn sanitize_component_handles_hostile_input() {
        assert_eq!(sanitize_component("feat/login", "x"), "feat-login");
        assert_eq!(sanitize_component("../../etc/passwd", "x"), "etc-passwd");
        assert_eq!(sanitize_component("", "fallback"), "fallback");
        assert_eq!(sanitize_component("...", "fallback"), "fallback");
        assert_eq!(sanitize_component("..", "fallback"), "fallback");
        assert_eq!(sanitize_component("a b c", "x"), "a-b-c");
        assert_eq!(sanitize_component("ok-name_1.2", "x"), "ok-name_1.2");
        assert!(sanitize_component(&"z".repeat(200), "x").len() <= 64);
        // No component may contain a separator after sanitising.
        for probe in ["a/b", "a\\b", "a\0b", "..", "."] {
            let s = sanitize_component(probe, "fallback");
            assert!(!s.contains('/') && !s.contains('\\') && !s.contains('\0'));
            assert!(s != "." && s != "..");
        }
    }

    #[test]
    fn worktree_dir_name_avoids_collisions() {
        // Distinct branches that sanitise to the same string must not collide.
        let a = worktree_dir_name("feat/login");
        let b = worktree_dir_name("feat-login");
        assert_ne!(a, b);
        // A name needing no sanitisation stays readable.
        assert_eq!(b, "feat-login");
        assert!(a.starts_with("feat-login-"));
    }

    #[test]
    fn worktree_paths_do_not_collide_between_same_named_repos() {
        let tmp = TempDir::new().unwrap();
        let p = paths_in(&tmp);
        let one = tmp.path().join("a/myapp");
        let two = tmp.path().join("b/myapp");
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();

        let k1 = ProjectKey::for_path(&one).key;
        let k2 = ProjectKey::for_path(&two).key;
        assert_ne!(p.worktree_path(&k1, "main"), p.worktree_path(&k2, "main"));
    }

    #[test]
    fn worktrees_live_outside_the_project() {
        let tmp = TempDir::new().unwrap();
        let p = paths_in(&tmp);
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        let key = ProjectKey::for_path(&project).key;

        let wt = p.worktree_path(&key, "task-1");
        assert!(!wt.starts_with(&project), "worktree must not be inside the repo");
        assert!(wt.starts_with(p.data_dir()));
    }

    #[test]
    fn auto_resolves_to_external_for_a_clean_project() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("clean");
        std::fs::create_dir_all(&project).unwrap();
        assert_eq!(
            StateLocation::Auto.resolve(&project),
            ResolvedLocation::External
        );
    }

    #[test]
    fn auto_ignores_an_empty_legacy_dir() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("legacy");
        std::fs::create_dir_all(project.join(IN_PROJECT_DIR)).unwrap();
        // An empty .slashit/ is the dead artefact — it must not opt the project in.
        assert_eq!(
            StateLocation::Auto.resolve(&project),
            ResolvedLocation::External
        );
    }

    #[test]
    fn auto_honours_a_populated_dir() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("adopted");
        let state = project.join(IN_PROJECT_DIR);
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join(STATE_MARKER), "version = 1\n").unwrap();
        assert_eq!(
            StateLocation::Auto.resolve(&project),
            ResolvedLocation::InProject
        );
    }

    #[test]
    fn explicit_locations_ignore_the_filesystem() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("p");
        std::fs::create_dir_all(&project).unwrap();
        assert_eq!(
            StateLocation::External.resolve(&project),
            ResolvedLocation::External
        );
        assert_eq!(
            StateLocation::InProject.resolve(&project),
            ResolvedLocation::InProject
        );
    }

    #[test]
    fn secrets_and_runtime_never_land_in_a_project() {
        let tmp = TempDir::new().unwrap();
        let p = paths_in(&tmp);
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();

        for path in [
            p.config_file(),
            p.credentials_file(),
            p.workspaces_file(),
            p.feature_flags_file(),
            p.socket_path(),
            p.pid_file(),
            p.logs_dir(),
            p.pr_helper_logs_dir(),
        ] {
            assert!(
                !path.starts_with(&project),
                "{} must never live inside a project",
                path.display()
            );
        }
    }

    #[test]
    fn state_location_serde_roundtrip() {
        for loc in [
            StateLocation::External,
            StateLocation::InProject,
            StateLocation::Auto,
        ] {
            let s = toml::to_string(&Wrap { location: loc }).unwrap();
            let back: Wrap = toml::from_str(&s).unwrap();
            assert_eq!(back.location, loc);
        }
        // Default must be External so new projects stay pristine.
        assert_eq!(StateLocation::default(), StateLocation::External);
        // And an absent field must deserialize to that default.
        let back: Wrap = toml::from_str("").unwrap();
        assert_eq!(back.location, StateLocation::External);
    }

    #[derive(Serialize, Deserialize)]
    struct Wrap {
        #[serde(default)]
        location: StateLocation,
    }
}
