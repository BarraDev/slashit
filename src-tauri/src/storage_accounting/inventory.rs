//! What SlashIt's directories hold, and whose it is.
//!
//! Only SlashIt's own roots are looked at: the data, configuration, cache and
//! runtime directories [`AppPaths`] resolves. Nothing a task, project or
//! board names is ever walked on that name's say-so. A Task Checkout is
//! found by listing `<data_dir>/worktrees/<project-key>/`, and only then
//! matched against the paths tasks record, so a recorded path cannot steer
//! the walk anywhere. Checkouts tasks record elsewhere, adopted or from
//! before SlashIt kept its own directory, are counted but not measured.
//!
//! Ownership comes from the task record, not from what a directory is
//! called. A checkout exactly one task records is that task's; any other
//! directory under the worktree root is [`StorageClassification::Unknown`].
//! Inside a checkout a task owns, a top-level `target/` or `dist/` is build
//! output only when it is a real directory that git reports as ignored and
//! that holds no tracked file; see [`GitBuildOutputProbe`]. Anything that
//! fails a check stays [`StorageClassification::WorkspaceSource`].

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::walk::{self, TreeSize, WalkError, WalkLimits};
use crate::config::paths::AppPaths;
use crate::domain::storage_usage::{
    CheckoutLifecycle, ConsumerKind, FilesystemSpace, Measurement, PressurePolicy,
    StorageClassification, StorageConsumer, StorageSummary,
};
use crate::domain::TaskStatus;

/// How many consumers a summary lists by name.
pub const LARGEST_CONSUMERS: usize = 12;

/// The top-level directories inside a Task Checkout that may be build
/// output. Only a candidate: [`BuildOutputProbe`] decides.
const BUILD_OUTPUT_DIRS: [&str; 2] = ["target", "dist"];

/// A Task Checkout as a task records it.
#[derive(Debug, Clone)]
pub struct RecordedCheckout {
    pub task_title: String,
    pub project_name: Option<String>,
    pub status: TaskStatus,
    pub cleanup_in_flight: bool,
    /// Whether an agent was working in the checkout when it was read.
    pub agent_attached: bool,
    pub path: PathBuf,
    /// The worktree root SlashIt would place this task's checkouts in,
    /// derived from its project's repository. `None` when the task's
    /// repository cannot be found, which no checkout can then be matched
    /// against.
    pub worktree_root: Option<PathBuf>,
}

/// Decides whether `checkout/<name>` is build output rather than source.
pub trait BuildOutputProbe {
    fn is_build_output(&self, checkout: &Path, name: &str) -> bool;
}

/// Build output is what git ignores and does not track.
///
/// Asks git in the checkout itself, with discovery fenced at the checkout's
/// parent so a checkout whose `.git` link is missing cannot be answered for
/// by some repository further up, and with any inherited `GIT_DIR`-style
/// override removed for the same reason. Reads only: optional locks are off,
/// so not even the index is refreshed, and `core.fsmonitor` is off, so no
/// hook program or monitor daemon a repository configures is started. Each
/// answer is an exit status alone and is bounded by [`GIT_TIMEOUT`]. Any
/// failure, including a timeout, answers "not build output".
pub struct GitBuildOutputProbe;

/// How long one git question may take before the probe gives up on it.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

impl BuildOutputProbe for GitBuildOutputProbe {
    fn is_build_output(&self, checkout: &Path, name: &str) -> bool {
        if fs::symlink_metadata(checkout.join(".git")).is_err() {
            return false;
        }
        let Some(ceiling) = checkout.parent() else {
            return false;
        };
        let git = |args: &[&str]| {
            let mut command = Command::new("git");
            command
                .arg("-C")
                .arg(checkout)
                // The second key is how Git for Windows 2.33 to 2.35 spelled
                // its built-in monitor; later versions ignore it.
                .args(["-c", "core.fsmonitor=false", "-c", "core.useBuiltinFSMonitor=false"])
                .args(args)
                .env("GIT_CEILING_DIRECTORIES", ceiling)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_COMMON_DIR")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            exit_code_within(command, GIT_TIMEOUT)
        };
        // 0 means ignored and 1 not. A directory holding a tracked file is
        // never reported ignored.
        if git(&["check-ignore", "-q", "--", name]) != Some(0) {
            return false;
        }
        // Belt and braces: `check-ignore` answers about the directory's
        // name, `ls-files` about what is actually tracked beneath it. 1 means
        // nothing is.
        git(&["ls-files", "--error-unmatch", "--", name]) == Some(1)
    }
}

/// Run `command` and return its exit code, or `None` if it could not start,
/// was killed by a signal, or did not finish within `timeout` (it is killed).
///
/// Only for commands whose output goes nowhere: a child blocked on a full
/// pipe would otherwise be mistaken for a slow one.
fn exit_code_within(mut command: Command, timeout: Duration) -> Option<i32> {
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Everything [`measure`] needs besides the filesystem itself.
pub struct Inputs<'a> {
    pub paths: &'a AppPaths,
    pub checkouts: &'a [RecordedCheckout],
    pub probe: &'a dyn BuildOutputProbe,
    pub space: &'a dyn Fn(&Path) -> io::Result<FilesystemSpace>,
    pub policy: PressurePolicy,
    pub limits: WalkLimits,
}

/// Measure SlashIt's directories. Blocking; run it off any async runtime
/// thread. Never fails as a whole: whatever could not be measured is
/// reported as such inside the summary.
pub fn measure(inputs: &Inputs<'_>) -> StorageSummary {
    let started = Instant::now();
    let measured_at = chrono::Utc::now();
    let paths = inputs.paths;

    let (filesystem, filesystem_error) = match (inputs.space)(paths.data_dir()) {
        Ok(space) => (Some(space), None),
        Err(e) => (None, Some(e.to_string())),
    };

    let mut census = Census::new(inputs);
    let mut external_checkouts = 0u32;
    for checkout in inputs.checkouts {
        if !is_below(&checkout.path, &paths.worktrees_dir()) {
            external_checkouts = external_checkouts.saturating_add(1);
        }
    }

    // Each distinct root once. On macOS the data and configuration
    // directories are the same directory.
    let mut roots: Vec<(PathBuf, RootKind)> = Vec::new();
    for (path, kind) in [
        (paths.data_dir().to_path_buf(), RootKind::State),
        (paths.config_dir().to_path_buf(), RootKind::State),
        (paths.cache_dir().to_path_buf(), RootKind::Whole(ConsumerKind::Cache, "Cache")),
        (paths.runtime_dir().to_path_buf(), RootKind::Whole(ConsumerKind::Runtime, "Runtime files")),
    ] {
        if !roots.iter().any(|(seen, _)| *seen == path) {
            roots.push((path, kind));
        }
    }
    census.roots = roots.iter().map(|(path, _)| path.clone()).collect();

    for (root, kind) in &roots {
        match kind {
            RootKind::State => census.state_root(root),
            RootKind::Whole(kind, label) => {
                census.whole(root, Spec::new(*kind, *label, StorageClassification::Temporary))
            }
        }
    }

    census.into_summary(SummaryHead {
        measured_at,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        pressure: filesystem.map(|space| inputs.policy.classify(space)),
        thresholds: filesystem.map(|space| inputs.policy.thresholds(space.total_bytes)),
        filesystem,
        filesystem_error,
        external_checkouts,
    })
}

#[derive(Clone, Copy)]
enum RootKind {
    /// Configuration and data: listed entry by entry.
    State,
    /// Measured as one consumer.
    Whole(ConsumerKind, &'static str),
}

/// Whether `path` is strictly below `root`, lexically. A path with a `..`
/// component is never below anything: it is not a path SlashIt wrote.
fn is_below(path: &Path, root: &Path) -> bool {
    !path.components().any(|c| matches!(c, Component::ParentDir))
        && path != root
        && path.starts_with(root)
}

struct SummaryHead {
    measured_at: chrono::DateTime<chrono::Utc>,
    duration_ms: u64,
    filesystem: Option<FilesystemSpace>,
    filesystem_error: Option<String>,
    pressure: Option<crate::domain::storage_usage::DiskPressure>,
    thresholds: Option<crate::domain::storage_usage::PressureThresholds>,
    external_checkouts: u32,
}

/// What a consumer is, before it is measured.
struct Spec {
    kind: ConsumerKind,
    label: String,
    detail: Option<String>,
    classification: StorageClassification,
    lifecycle: Option<CheckoutLifecycle>,
    reclaimable: bool,
}

impl Spec {
    fn new(kind: ConsumerKind, label: impl Into<String>, classification: StorageClassification) -> Self {
        Self {
            kind,
            label: label.into(),
            detail: None,
            classification,
            lifecycle: None,
            reclaimable: false,
        }
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    fn lifecycle(mut self, lifecycle: CheckoutLifecycle) -> Self {
        self.lifecycle = Some(lifecycle);
        self
    }

    fn reclaimable(mut self, reclaimable: bool) -> Self {
        self.reclaimable = reclaimable;
        self
    }
}

struct Census<'a> {
    inputs: &'a Inputs<'a>,
    /// Every root being measured, so no root is also counted inside another.
    roots: Vec<PathBuf>,
    /// The fixed entries SlashIt writes in its data and configuration
    /// directories, and what each is.
    known: Vec<(PathBuf, ConsumerKind, &'static str)>,
    consumers: Vec<StorageConsumer>,
    /// A root or directory that could not even be listed.
    unlisted: bool,
    links_not_followed: u64,
    mounts_not_entered: u64,
}

impl<'a> Census<'a> {
    fn new(inputs: &'a Inputs<'a>) -> Self {
        let p = inputs.paths;
        let known = vec![
            (p.external_projects_dir(), ConsumerKind::ProjectState, "Project boards"),
            (p.logs_dir(), ConsumerKind::Logs, "Logs"),
            (p.pr_helper_logs_dir(), ConsumerKind::Logs, "Pull request helper logs"),
            (p.terminal_sessions_file(), ConsumerKind::TerminalSessions, "Terminal sessions"),
            (p.config_file(), ConsumerKind::Configuration, "Settings"),
            (p.workspaces_file(), ConsumerKind::Configuration, "Workspaces"),
            (p.credentials_file(), ConsumerKind::Configuration, "Credentials"),
            (p.feature_flags_file(), ConsumerKind::Configuration, "Feature flags"),
            (p.ipc_config_file(), ConsumerKind::Configuration, "Control channel settings"),
            (p.legacy_tasks_dir(), ConsumerKind::ProjectState, "Task files (earlier layout)"),
        ];
        Self {
            inputs,
            roots: Vec::new(),
            known,
            consumers: Vec::new(),
            unlisted: false,
            links_not_followed: 0,
            mounts_not_entered: 0,
        }
    }

    /// A data or configuration root: each entry by what SlashIt knows it
    /// to be.
    fn state_root(&mut self, root: &Path) {
        let Some(entries) = self.list(root, "SlashIt data") else {
            return;
        };
        let worktrees_dir = self.inputs.paths.worktrees_dir();
        for path in entries {
            if self.roots.contains(&path) {
                continue;
            }
            if path == worktrees_dir {
                self.worktrees(&path);
                continue;
            }
            let spec = match self.known.iter().find(|(known, _, _)| *known == path) {
                Some((_, kind, label)) => Spec::new(*kind, *label, StorageClassification::AppData),
                None => unrecognized(&path),
            };
            self.whole(&path, spec);
        }
    }

    /// `<data_dir>/worktrees`: one directory per project key, one checkout
    /// per directory inside it.
    fn worktrees(&mut self, dir: &Path) {
        if !is_real_dir(dir) {
            let spec = Spec::new(ConsumerKind::TaskCheckout, "Task checkouts", StorageClassification::Unknown)
                .detail("not a directory of SlashIt's own");
            return self.whole(dir, spec);
        }
        let Some(keys) = self.list(dir, "Task checkouts") else {
            return;
        };
        let mut recorded: HashMap<&Path, Vec<&RecordedCheckout>> = HashMap::new();
        for checkout in self.inputs.checkouts {
            recorded.entry(checkout.path.as_path()).or_default().push(checkout);
        }
        for key in keys {
            if !is_real_dir(&key) {
                self.whole(&key, unrecognized(&key));
                continue;
            }
            let Some(checkouts) = self.list(&key, "Task checkouts") else {
                continue;
            };
            let key_name = file_name(&key);
            for path in checkouts {
                let owners = recorded.get(path.as_path()).map(Vec::as_slice).unwrap_or(&[]);
                self.checkout(&path, &key_name, owners);
            }
        }
    }

    fn checkout(&mut self, path: &Path, key_name: &str, owners: &[&RecordedCheckout]) {
        let (owner, lifecycle) = match owners {
            [] => (None, CheckoutLifecycle::Unrecorded),
            // A task of one project recording a directory under another
            // project's root is not a checkout SlashIt made for it.
            [only] if only.worktree_root.as_deref() != path.parent() => (None, CheckoutLifecycle::Misplaced),
            [only] if only.cleanup_in_flight => {
                (Some(*only), CheckoutLifecycle::CleanupInterrupted { status: only.status.clone() })
            }
            [only] => (
                Some(*only),
                CheckoutLifecycle::Task { status: only.status.clone(), agent_attached: only.agent_attached },
            ),
            _ => (None, CheckoutLifecycle::Contested),
        };

        let Some(owner) = owner.filter(|_| is_real_dir(path)) else {
            // Not attributable to one task, or not a directory SlashIt
            // could have made: counted, never owned.
            let spec = Spec::new(ConsumerKind::TaskCheckout, "Unattributed checkout", StorageClassification::Unknown)
                .detail(format!("{key_name}/{}", file_name(path)))
                .lifecycle(lifecycle);
            return self.whole(path, spec);
        };

        // A cleanup that never recorded its outcome may be taking the
        // directory apart, so it is measured but not broken down.
        let outputs: Vec<&str> = if matches!(lifecycle, CheckoutLifecycle::Task { .. }) {
            BUILD_OUTPUT_DIRS
                .into_iter()
                .filter(|name| {
                    let candidate = path.join(name);
                    is_real_dir(&candidate)
                        && same_device(path, &candidate)
                        // A repository of its own, such as a `gh-pages`
                        // checkout kept at `dist/`, holds history no build
                        // recreates.
                        && fs::symlink_metadata(candidate.join(".git")).is_err()
                        && self.inputs.probe.is_build_output(path, name)
                })
                .collect()
        } else {
            Vec::new()
        };

        let mut source = Spec::new(ConsumerKind::TaskCheckout, owner.task_title.clone(), StorageClassification::WorkspaceSource)
            .lifecycle(lifecycle.clone());
        source.detail = owner.project_name.clone();
        self.push(walk::measure(path, &outputs, self.inputs.limits), source);

        let reclaimable = !lifecycle.may_be_in_use();
        for name in outputs {
            let spec = Spec::new(ConsumerKind::BuildOutput, owner.task_title.clone(), StorageClassification::Rebuildable)
                .detail(format!("{name}/ build output"))
                .lifecycle(lifecycle.clone())
                .reclaimable(reclaimable);
            self.push(walk::measure(&path.join(name), &[], self.inputs.limits), spec);
        }
    }

    fn whole(&mut self, path: &Path, spec: Spec) {
        self.push(walk::measure(path, &[], self.inputs.limits), spec)
    }

    fn push(&mut self, size: Result<TreeSize, WalkError>, spec: Spec) {
        if let Ok(size) = &size {
            self.links_not_followed = self.links_not_followed.saturating_add(size.links_not_followed);
            self.mounts_not_entered = self.mounts_not_entered.saturating_add(size.mounts_not_entered);
        }
        let (bytes, measurement) = match size {
            // Something that should be a directory of SlashIt's is a link
            // to somewhere else, such as a worktree root moved to another
            // disk: what it holds is not measured, and the total must not
            // read as complete.
            Ok(size) if size.root_is_link => (
                Some(size.bytes),
                Measurement::Partial {
                    skipped_entries: 1,
                    reason: "a link to somewhere else, which is not measured".to_string(),
                },
            ),
            Ok(size) if size.mounts_not_entered > 0 => (
                Some(size.bytes),
                Measurement::Partial {
                    skipped_entries: size.mounts_not_entered.saturating_add(size.skipped),
                    reason: format!(
                        "{} director{} on another filesystem not measured",
                        size.mounts_not_entered,
                        if size.mounts_not_entered == 1 { "y" } else { "ies" }
                    ),
                },
            ),
            Ok(size) if size.is_complete() => (Some(size.bytes), Measurement::Complete),
            Ok(size) => (
                Some(size.bytes),
                Measurement::Partial {
                    skipped_entries: size.skipped,
                    reason: size
                        .first_skip_reason
                        .unwrap_or_else(|| "stopped at the walk's size limit".to_string()),
                },
            ),
            // An optional root that does not exist, or an entry that
            // vanished between listing and measuring, uses no space.
            Err(WalkError::NotFound) => return,
            Err(WalkError::Unreadable(reason)) => (None, Measurement::Failed { reason }),
        };
        self.consumers.push(StorageConsumer {
            kind: spec.kind,
            label: spec.label,
            detail: spec.detail,
            bytes,
            classification: spec.classification,
            lifecycle: spec.lifecycle,
            reclaimable: spec.reclaimable && bytes.is_some(),
            measurement,
        });
    }

    /// The entries of `dir`, or `None` after recording that it could not be
    /// listed. A directory that does not exist has no entries.
    fn list(&mut self, dir: &Path, label: &str) -> Option<Vec<PathBuf>> {
        match fs::read_dir(dir) {
            Ok(entries) => {
                let mut paths: Vec<PathBuf> = Vec::new();
                for entry in entries {
                    match entry {
                        Ok(entry) => paths.push(entry.path()),
                        Err(_) => self.unlisted = true,
                    }
                }
                paths.sort();
                Some(paths)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                self.unlisted = true;
                self.push(
                    Err(WalkError::Unreadable(e.to_string())),
                    Spec::new(ConsumerKind::Unrecognized, label, StorageClassification::Unknown),
                );
                None
            }
        }
    }

    fn into_summary(self, head: SummaryHead) -> StorageSummary {
        let mut owned = 0u64;
        let mut active = 0u64;
        let mut rebuildable = 0u64;
        let mut reclaimable = 0u64;
        let mut unknown = 0u64;
        let mut incomplete = self.unlisted;
        let mut measured = Vec::new();
        let mut unmeasured = Vec::new();

        for consumer in self.consumers {
            if consumer.measurement != Measurement::Complete {
                incomplete = true;
            }
            let Some(bytes) = consumer.bytes else {
                unmeasured.push(consumer);
                continue;
            };
            if consumer.classification == StorageClassification::Unknown {
                unknown = unknown.saturating_add(bytes);
            } else {
                owned = owned.saturating_add(bytes);
                if consumer.classification == StorageClassification::Rebuildable {
                    rebuildable = rebuildable.saturating_add(bytes);
                }
                if consumer.reclaimable {
                    reclaimable = reclaimable.saturating_add(bytes);
                }
                if consumer.lifecycle.as_ref().is_some_and(CheckoutLifecycle::is_active) {
                    active = active.saturating_add(bytes);
                }
            }
            measured.push(consumer);
        }

        measured.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.label.cmp(&b.label)));
        measured.truncate(LARGEST_CONSUMERS);

        StorageSummary {
            measured_at: head.measured_at,
            duration_ms: head.duration_ms,
            filesystem: head.filesystem,
            filesystem_error: head.filesystem_error,
            pressure: head.pressure,
            thresholds: head.thresholds,
            slashit_owned_bytes: owned,
            active_workspace_bytes: active,
            rebuildable_bytes: rebuildable,
            reclaimable_bytes: reclaimable,
            unknown_managed_bytes: unknown,
            incomplete,
            links_not_followed: self.links_not_followed,
            mounts_not_entered: self.mounts_not_entered,
            external_checkouts: head.external_checkouts,
            largest_consumers: measured,
            unmeasured,
        }
    }
}

fn unrecognized(path: &Path) -> Spec {
    Spec::new(ConsumerKind::Unrecognized, "Unrecognized entry", StorageClassification::Unknown).detail(file_name(path))
}

/// A directory, and not a link or (on Windows) any other reparse point.
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && !walk::is_link(&meta))
}

#[cfg(unix)]
fn same_device(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

/// Off Unix a volume mounted inside a checkout is a reparse point, which
/// [`is_real_dir`] does not accept.
#[cfg(not(unix))]
fn same_device(_a: &Path, _b: &Path) -> bool {
    true
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::paths::ProjectKey;
    use crate::domain::storage_usage::{DiskPressure, GIB};

    const KIB: u64 = 1024;
    /// Allocation rounds each file and directory up to whole blocks.
    const SLACK: u64 = 64 * KIB;

    struct Fixture {
        _tmp: tempfile::TempDir,
        paths: AppPaths,
        outside: PathBuf,
        key_dir: PathBuf,
        checkouts: Vec<RecordedCheckout>,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            let paths = AppPaths::with_roots(
                root.join("config"),
                root.join("data"),
                root.join("cache"),
                root.join("runtime"),
            );
            fs::create_dir_all(paths.config_dir()).unwrap();
            fs::create_dir_all(paths.data_dir()).unwrap();
            let outside = root.join("outside");
            fs::create_dir_all(&outside).unwrap();
            let key_dir = paths.worktrees_root(&ProjectKey::from_stored("repo-0123abcd"));
            fs::create_dir_all(&key_dir).unwrap();
            Self {
                _tmp: tmp,
                paths,
                outside,
                key_dir,
                checkouts: Vec::new(),
            }
        }

        /// A git checkout under the worktree root, recorded by one task.
        fn checkout(&mut self, name: &str, status: TaskStatus) -> PathBuf {
            let path = self.key_dir.join(name);
            fs::create_dir_all(&path).unwrap();
            git(&path, &["init", "-q"]);
            self.record(&path, name, status);
            path
        }

        fn record(&mut self, path: &Path, title: &str, status: TaskStatus) {
            self.checkouts.push(RecordedCheckout {
                task_title: title.to_string(),
                project_name: Some("Project".to_string()),
                status,
                cleanup_in_flight: false,
                agent_attached: false,
                path: path.to_path_buf(),
                worktree_root: Some(self.key_dir.clone()),
            });
        }

        fn measure_with(&self, probe: &dyn BuildOutputProbe, available: u64) -> StorageSummary {
            let space = move |_: &Path| {
                Ok(FilesystemSpace {
                    total_bytes: 1000 * GIB,
                    available_bytes: available,
                })
            };
            measure(&Inputs {
                paths: &self.paths,
                checkouts: &self.checkouts,
                probe,
                space: &space,
                policy: PressurePolicy::default(),
                limits: WalkLimits::default(),
            })
        }

        fn measure(&self) -> StorageSummary {
            self.measure_with(&GitBuildOutputProbe, 500 * GIB)
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be installed to run this test");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// Incompressible content, so a compressing filesystem cannot make a
    /// file take less space than its length.
    fn write(path: &Path, len: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut state = 0x9e37_79b9_7f4a_7c15u64 ^ len;
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        fs::write(path, bytes).unwrap();
    }

    fn assert_near(actual: u64, expected: u64, what: &str) {
        assert!(
            actual >= expected && actual <= expected + SLACK,
            "{what}: {actual} bytes, expected {expected} plus at most {SLACK} of allocation slack"
        );
    }

    fn consumers(summary: &StorageSummary, kind: ConsumerKind) -> Vec<&StorageConsumer> {
        summary
            .largest_consumers
            .iter()
            .chain(&summary.unmeasured)
            .filter(|c| c.kind == kind)
            .collect()
    }

    /// The fixture: one checkout with source, an ignored `target/` and
    /// `dist/`, untracked work, and a link escaping it; an orphan checkout;
    /// an unrecognized file; and a checkout a task records outside
    /// SlashIt's directories.
    #[test]
    fn only_the_intended_bytes_land_in_each_total() {
        let mut fx = Fixture::new();
        let checkout = fx.checkout("task-a", TaskStatus::HumanReview);
        fs::write(checkout.join(".gitignore"), "/target\n/dist\n.env\n").unwrap();
        write(&checkout.join("src/main.rs"), 100 * KIB);
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-qm", "source"]);
        write(&checkout.join("target/debug/app"), 1024 * KIB);
        write(&checkout.join("dist/app.wasm"), 512 * KIB);
        // Untracked and ignored work that is not build output stays source.
        write(&checkout.join("notes.md"), 50 * KIB);
        write(&checkout.join(".env"), 10 * KIB);
        write(&fx.outside.join("huge"), 4096 * KIB);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&fx.outside, checkout.join("escape")).unwrap();

        write(&fx.key_dir.join("orphan/leftover"), 300 * KIB);
        write(&fx.paths.data_dir().join("mystery.bin"), 70 * KIB);
        write(&fx.paths.logs_dir().join("agent.log"), 20 * KIB);
        let external = fx.outside.join("repo.task-b");
        write(&external.join("big"), 2048 * KIB);
        fx.record(&external, "task-b", TaskStatus::InProgress);

        let summary = fx.measure();
        assert!(!summary.incomplete, "{summary:#?}");
        assert_eq!(summary.external_checkouts, 1);

        let target = 1024 * KIB;
        let dist = 512 * KIB;
        assert_near(summary.rebuildable_bytes, target + dist, "rebuildable");
        assert_eq!(summary.reclaimable_bytes, summary.rebuildable_bytes, "an idle task's build output");
        assert_near(summary.unknown_managed_bytes, 300 * KIB + 70 * KIB, "unknown");

        let sources = consumers(&summary, ConsumerKind::TaskCheckout);
        let source: Vec<_> = sources
            .iter()
            .filter(|c| c.classification == StorageClassification::WorkspaceSource)
            .collect();
        assert_eq!(source.len(), 1);
        let source_bytes = source[0].bytes.unwrap();
        // Source, notes and .env, plus git's own files; never the linked
        // 4 MiB, the build output, or the external checkout.
        assert!(source_bytes >= 160 * KIB, "{source_bytes}");
        assert!(source_bytes < 1024 * KIB, "{source_bytes}");
        assert!(!source[0].reclaimable);
        assert_eq!(
            summary.active_workspace_bytes,
            source_bytes + summary.rebuildable_bytes,
            "the whole of a Human Review task's checkout is active"
        );

        let orphan = sources
            .iter()
            .find(|c| c.lifecycle == Some(CheckoutLifecycle::Unrecorded))
            .expect("the unrecorded checkout is listed");
        assert_eq!(orphan.classification, StorageClassification::Unknown);
        assert!(!orphan.reclaimable);
        assert_eq!(orphan.detail.as_deref(), Some("repo-0123abcd/orphan"));

        let logs = consumers(&summary, ConsumerKind::Logs);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].classification, StorageClassification::AppData);

        // Owned is everything measured that is not unknown.
        let listed: u64 = summary
            .largest_consumers
            .iter()
            .filter(|c| c.classification != StorageClassification::Unknown)
            .filter_map(|c| c.bytes)
            .sum();
        assert_eq!(summary.slashit_owned_bytes, listed);
        assert_eq!(summary.pressure, Some(DiskPressure::Normal));
    }

    #[test]
    fn build_output_of_a_running_task_is_rebuildable_but_not_reclaimable() {
        let mut fx = Fixture::new();
        let checkout = fx.checkout("task-a", TaskStatus::InProgress);
        fs::write(checkout.join(".gitignore"), "target/\n").unwrap();
        write(&checkout.join("target/big"), 256 * KIB);

        let summary = fx.measure();
        assert_near(summary.rebuildable_bytes, 256 * KIB, "rebuildable");
        assert_eq!(summary.reclaimable_bytes, 0);
        let output = &consumers(&summary, ConsumerKind::BuildOutput)[0];
        assert!(!output.reclaimable);
        assert_eq!(
            output.lifecycle,
            Some(CheckoutLifecycle::Task { status: TaskStatus::InProgress, agent_attached: false })
        );
    }

    #[test]
    fn build_output_is_not_offered_while_an_agent_is_attached_or_the_task_is_queued() {
        let mut fx = Fixture::new();
        // A pull request helper at work: the status stays PR Created.
        let helped = fx.checkout("helped", TaskStatus::PrCreated);
        fx.checkouts.last_mut().unwrap().agent_attached = true;
        let queued = fx.checkout("queued", TaskStatus::Queue);
        for checkout in [&helped, &queued] {
            fs::write(checkout.join(".gitignore"), "/target\n").unwrap();
            write(&checkout.join("target/big"), 128 * KIB);
        }

        let summary = fx.measure();
        assert_near(summary.rebuildable_bytes, 256 * KIB, "rebuildable");
        assert_eq!(summary.reclaimable_bytes, 0, "{summary:#?}");
    }

    #[test]
    fn an_ignored_output_directory_that_is_a_repository_is_not_build_output() {
        let mut fx = Fixture::new();
        let checkout = fx.checkout("task-a", TaskStatus::HumanReview);
        fs::write(checkout.join(".gitignore"), "/dist\n").unwrap();
        // A `gh-pages` checkout kept at `dist/`.
        fs::create_dir_all(checkout.join("dist")).unwrap();
        git(&checkout.join("dist"), &["init", "-q"]);
        write(&checkout.join("dist/index.html"), 128 * KIB);

        let summary = fx.measure();
        assert_eq!(summary.rebuildable_bytes, 0, "{summary:#?}");
    }

    #[test]
    fn a_checkout_recorded_under_another_projects_root_is_not_attributed() {
        let mut fx = Fixture::new();
        let checkout = fx.checkout("task-a", TaskStatus::HumanReview);
        fs::write(checkout.join(".gitignore"), "/target\n").unwrap();
        write(&checkout.join("target/big"), 128 * KIB);
        // The recording task's own project places checkouts elsewhere.
        fx.checkouts.last_mut().unwrap().worktree_root =
            Some(fx.paths.worktrees_root(&ProjectKey::from_stored("other-89abcdef")));

        let summary = fx.measure();
        let checkout = &consumers(&summary, ConsumerKind::TaskCheckout)[0];
        assert_eq!(checkout.lifecycle, Some(CheckoutLifecycle::Misplaced));
        assert_eq!(checkout.classification, StorageClassification::Unknown);
        assert_eq!(summary.rebuildable_bytes, 0);
        assert_eq!(summary.slashit_owned_bytes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_worktree_root_that_is_a_link_leaves_the_totals_incomplete() {
        let mut fx = Fixture::new();
        let moved = fx.outside.join("worktrees");
        write(&moved.join("key/task/big"), 1024 * KIB);
        fs::remove_dir_all(fx.paths.worktrees_dir()).unwrap();
        std::os::unix::fs::symlink(&moved, fx.paths.worktrees_dir()).unwrap();
        fx.record(&fx.paths.worktrees_dir().join("key/task"), "task", TaskStatus::HumanReview);

        let summary = fx.measure();
        assert!(summary.incomplete, "{summary:#?}");
        assert_eq!(summary.links_not_followed, 1);
        let root = &consumers(&summary, ConsumerKind::TaskCheckout)[0];
        assert!(matches!(root.measurement, Measurement::Partial { .. }));
        assert!(summary.unknown_managed_bytes < 64 * KIB);
    }

    #[cfg(unix)]
    #[test]
    fn the_probe_runs_no_program_the_repository_configures() {
        use std::os::unix::fs::PermissionsExt;
        let mut fx = Fixture::new();
        let checkout = fx.checkout("task-a", TaskStatus::HumanReview);
        let marker = fx.outside.join("hook-ran");
        let hook = fx.outside.join("fsmonitor-hook");
        fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display())).unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        git(&checkout, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
        fs::write(checkout.join(".gitignore"), "/target\n").unwrap();
        write(&checkout.join("target/big"), 128 * KIB);

        let summary = fx.measure();
        assert_near(summary.rebuildable_bytes, 128 * KIB, "rebuildable");
        assert!(!marker.exists(), "measuring ran the repository's fsmonitor hook");
    }

    #[test]
    fn build_output_needs_git_to_ignore_it_and_track_nothing_in_it() {
        let mut fx = Fixture::new();
        // Not ignored at all.
        let plain = fx.checkout("plain", TaskStatus::HumanReview);
        write(&plain.join("target/big"), 128 * KIB);
        // Ignored, but with a tracked file inside.
        let tracked = fx.checkout("tracked", TaskStatus::HumanReview);
        fs::write(tracked.join(".gitignore"), "/dist\n").unwrap();
        write(&tracked.join("dist/vendored.js"), 128 * KIB);
        git(&tracked, &["add", "-f", "."]);
        git(&tracked, &["commit", "-qm", "vendored"]);
        // Ignored, but a link rather than a directory.
        let linked = fx.checkout("linked", TaskStatus::HumanReview);
        fs::write(linked.join(".gitignore"), "/target\n").unwrap();
        write(&fx.outside.join("shared-target/big"), 128 * KIB);
        #[cfg(unix)]
        std::os::unix::fs::symlink(fx.outside.join("shared-target"), linked.join("target")).unwrap();

        let summary = fx.measure();
        assert_eq!(summary.rebuildable_bytes, 0, "{summary:#?}");
        assert_eq!(summary.reclaimable_bytes, 0);
        assert!(consumers(&summary, ConsumerKind::BuildOutput).is_empty());
    }

    #[test]
    fn a_checkout_without_its_git_link_is_not_answered_for_by_a_parent_repository() {
        let mut fx = Fixture::new();
        // The whole state root is inside a repository that ignores `target`.
        let root = fx.paths.data_dir().parent().unwrap().to_path_buf();
        git(&root, &["init", "-q"]);
        fs::write(root.join(".gitignore"), "target\n").unwrap();
        let path = fx.key_dir.join("no-git");
        write(&path.join("target/big"), 128 * KIB);
        fx.record(&path, "no-git", TaskStatus::HumanReview);

        let summary = fx.measure();
        assert_eq!(summary.rebuildable_bytes, 0, "{summary:#?}");
    }

    #[test]
    fn checkouts_not_attributable_to_exactly_one_task_are_unknown() {
        let mut fx = Fixture::new();
        let shared = fx.checkout("shared", TaskStatus::HumanReview);
        fx.record(&shared, "other", TaskStatus::Backlog);
        fs::write(shared.join(".gitignore"), "/target\n").unwrap();
        write(&shared.join("target/big"), 128 * KIB);

        let mut interrupted = fx.checkout("interrupted", TaskStatus::Done);
        fx.checkouts.last_mut().unwrap().cleanup_in_flight = true;
        fs::write(interrupted.join(".gitignore"), "/target\n").unwrap();
        interrupted.push("target/big");
        write(&interrupted, 128 * KIB);

        let summary = fx.measure();
        let checkouts = consumers(&summary, ConsumerKind::TaskCheckout);
        let contested = checkouts
            .iter()
            .find(|c| c.lifecycle == Some(CheckoutLifecycle::Contested))
            .unwrap();
        assert_eq!(contested.classification, StorageClassification::Unknown);
        let cleanup = checkouts
            .iter()
            .find(|c| matches!(c.lifecycle, Some(CheckoutLifecycle::CleanupInterrupted { .. })))
            .unwrap();
        // Owned, but neither broken down nor offered back.
        assert_eq!(cleanup.classification, StorageClassification::WorkspaceSource);
        assert_eq!(summary.rebuildable_bytes, 0);
        assert_eq!(summary.reclaimable_bytes, 0);
        assert_eq!(summary.active_workspace_bytes, 0);
    }

    #[test]
    fn a_recorded_path_cannot_steer_the_walk_outside_the_worktree_root() {
        let mut fx = Fixture::new();
        write(&fx.outside.join("secret/big"), 1024 * KIB);
        // Spelled to start with the worktree root but climb out of it.
        let sneaky = fx.key_dir.join("..").join("..").join("..").join("outside").join("secret");
        fx.record(&sneaky, "sneaky", TaskStatus::HumanReview);

        let summary = fx.measure();
        assert_eq!(summary.external_checkouts, 1);
        assert!(consumers(&summary, ConsumerKind::TaskCheckout).is_empty());
        assert!(summary.slashit_owned_bytes < 1024 * KIB, "{summary:#?}");
    }

    #[cfg(unix)]
    #[test]
    fn one_unreadable_consumer_does_not_discard_the_others() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads everything
        }
        let mut fx = Fixture::new();
        let checkout = fx.checkout("task-a", TaskStatus::Error);
        fs::write(checkout.join(".gitignore"), "/target\n").unwrap();
        write(&checkout.join("target/big"), 256 * KIB);
        write(&checkout.join("locked/work"), 64 * KIB);
        write(&fx.paths.logs_dir().join("agent.log"), 32 * KIB);
        write(&fx.paths.external_projects_dir().join("k/board.toml"), 8 * KIB);

        let lock = |path: &Path, mode| fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        lock(&checkout.join("locked"), 0o000);
        lock(&fx.paths.logs_dir(), 0o000);
        let summary = fx.measure();
        lock(&checkout.join("locked"), 0o755);
        lock(&fx.paths.logs_dir(), 0o755);

        assert!(summary.incomplete);
        let logs = consumers(&summary, ConsumerKind::Logs);
        assert_eq!(logs[0].bytes, None, "a failed measurement is not zero bytes");
        assert!(matches!(logs[0].measurement, Measurement::Failed { .. }));
        assert!(!logs[0].reclaimable);
        assert!(summary.unmeasured.iter().any(|c| c.kind == ConsumerKind::Logs));

        let source = consumers(&summary, ConsumerKind::TaskCheckout)[0];
        assert!(matches!(source.measurement, Measurement::Partial { skipped_entries: 1, .. }));
        // The failed task's build output is still measured and offered.
        assert_near(summary.reclaimable_bytes, 256 * KIB, "reclaimable");
        assert_eq!(consumers(&summary, ConsumerKind::ProjectState).len(), 1);
    }

    #[test]
    fn filesystem_space_that_cannot_be_read_leaves_pressure_unknown_not_normal() {
        let fx = Fixture::new();
        let failing = |_: &Path| Err(io::Error::other("no statvfs"));
        let summary = measure(&Inputs {
            paths: &fx.paths,
            checkouts: &[],
            probe: &GitBuildOutputProbe,
            space: &failing,
            policy: PressurePolicy::default(),
            limits: WalkLimits::default(),
        });
        assert_eq!(summary.filesystem, None);
        assert_eq!(summary.pressure, None);
        assert_eq!(summary.filesystem_error.as_deref(), Some("no statvfs"));

        assert_eq!(fx.measure_with(&GitBuildOutputProbe, 10 * GIB).pressure, Some(DiskPressure::Critical));
    }

    #[test]
    fn roots_that_are_one_directory_are_counted_once() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("app");
        // As on macOS, where data and configuration share a directory.
        let paths = AppPaths::with_roots(shared.clone(), shared.clone(), tmp.path().join("cache"), tmp.path().join("run"));
        write(&paths.config_file(), 4 * KIB);
        write(&paths.logs_dir().join("a.log"), 40 * KIB);
        let space = |_: &Path| Ok(FilesystemSpace { total_bytes: 1000 * GIB, available_bytes: 500 * GIB });
        let summary = measure(&Inputs {
            paths: &paths,
            checkouts: &[],
            probe: &GitBuildOutputProbe,
            space: &space,
            policy: PressurePolicy::default(),
            limits: WalkLimits::default(),
        });
        assert_eq!(summary.unknown_managed_bytes, 0, "{summary:#?}");
        assert_eq!(consumers(&summary, ConsumerKind::Configuration).len(), 1);
        assert_near(summary.slashit_owned_bytes, 44 * KIB, "owned");
    }
}
