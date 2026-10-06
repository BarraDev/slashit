//! Test helper utilities
//! Common test fixtures and utilities for backend testing

use crate::domain::{Task, TaskStatus, TaskCategory, TaskPriority, TaskComplexity, TaskImpact, SecuritySeverity, TaskPhase};
pub use uuid::Uuid;
use chrono::Utc;

/// The one process-global lock for every `--lib` unit test that mutates
/// process-wide `PATH` to install a fake command shim (a fixture `claude` or
/// `gh` binary at the front of `PATH`).
///
/// `PATH` belongs to the whole test *process*, not to any one module. Before
/// this lock existed, `queue::executor`'s review-lifecycle tests and
/// `commands::pr`'s repo-level PR-creation tests each guarded their own PATH
/// mutation with a private, module-local `static PATH_LOCK` of the same
/// name. Two distinct `Mutex`es do not serialize against each other, so
/// under default `cargo test` thread parallelism a thread running one
/// module's fixture install/restore could interleave with the other
/// module's, corrupting whichever one restored `PATH` last.
///
/// Concretely reproduced (see the Unit 5C2 corrective report): with both
/// suites running in the same default-parallel `cargo test -p slashit-ui
/// --lib` process,
/// `commands::pr::tests::repo_level_pr_creation::bulk_create_prs_applies_the_same_contract_as_create_pr`
/// escaped its own `MockGh` and reached the developer's real `gh` binary
/// (observed failure: `gh failed: none of the git remotes configured for
/// this repository point to a known GitHub host`), and
/// `queue::executor::tests::review_lifecycle::a_running_fix_agent_can_be_stopped_safely`
/// spawned something other than its own fixture script and never observed
/// its pidfile being written (`fixture never wrote its pid file; the
/// blocking role was never reached`).
///
/// Every test in this crate that mutates process-global `PATH` (or removes
/// it) to install a fake command must acquire this lock -- not a private
/// one -- before saving/mutating `PATH`, and must not release it until
/// `PATH` has been fully restored to what it was before the mutation
/// (typically by holding the guard for the test's whole body, so an owning
/// fixture's `Drop` impl runs and restores `PATH` before the lock itself is
/// released).
///
/// `#[cfg(test)]`: this is compiled only when building the crate's own unit
/// test binary (`cargo test -p slashit-ui --lib`), never into the release
/// artifact -- unlike the rest of this module, which stays uncfg'd so the
/// separate integration-test binaries under `tests/` can link it too.
#[cfg(test)]
pub static PATH_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// `PATH` replaced for as long as this value lives, with every
/// invocation of the fake programs it installs recorded in `log`.
///
/// Holds [`crate::test_helpers::PATH_LOCK`] itself, so `PATH` is restored
/// before the lock is released no matter how the test ends.
#[cfg(all(test, unix))]
pub struct FakeProgram {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    log: std::path::PathBuf,
    saved_path: Option<std::ffi::OsString>,
}

/// Where [`FakeProgram::without`] puts the directories standing in for
/// `PATH` entries. A shadow is never removed while this process runs: a
/// program a test that does not take [`PATH_LOCK`] started while `PATH` named
/// it (a `git` shell script looking up `sed`, say) may still be resolving
/// programs there after the test that made it has finished. Deleting it then
/// made such runs fail with "command not found" on machines where a hidden
/// program shares a directory with everything else (`wt` in `/usr/bin`).
///
/// The root is reclaimed when the process is gone, not before: at normal exit
/// by an `atexit` handler (a static is never dropped), and after a crash by
/// the next process to claim a root. See [`ShadowRoot`].
#[cfg(all(test, unix))]
static SHADOW_ROOT: std::sync::LazyLock<ShadowRoot> = std::sync::LazyLock::new(|| {
    let root = ShadowRoot::claim(&shadow_parent(&std::env::temp_dir())).expect("claim the shadow root");
    let _ = EXIT_ROOT.set(root.path().to_path_buf());
    // Safety: `reclaim_at_exit` is a plain `extern "C"` function that
    // touches no state but `EXIT_ROOT`, set above.
    unsafe {
        libc::atexit(reclaim_at_exit);
    }
    root
});

#[cfg(all(test, unix))]
static EXIT_ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

#[cfg(all(test, unix))]
extern "C" fn reclaim_at_exit() {
    if let Some(root) = EXIT_ROOT.get() {
        let _ = std::fs::remove_dir_all(root);
    }
}

/// The directory under `base` holding every process's [`ShadowRoot`],
/// private to the current user.
#[cfg(all(test, unix))]
fn shadow_parent(base: &std::path::Path) -> std::path::PathBuf {
    // Safety: `geteuid` has no preconditions.
    base.join(format!("slashit-test-shadows-{}", unsafe { libc::geteuid() }))
}

/// A directory of `PATH` shadows owned by one test process.
///
/// Liveness is an exclusive `flock` on `.alive` inside the root, held for as
/// long as the process lives. The kernel drops it when the process ends, for
/// any reason, so a lock that can be taken proves the owner is gone; unlike a
/// recorded PID it cannot be confused by PID reuse. The descriptor is
/// close-on-exec, so children never keep a dead owner's root looking alive.
///
/// [`ShadowRoot::claim`] first removes every root under `parent` whose owner
/// is gone. Claiming and reaping run under one lock on `parent/.registry`,
/// so a root that is still being created is never mistaken for a stale one.
/// Only direct children named `root-*`, that are real directories owned by
/// the current user, are removed, and `remove_dir_all` unlinks symlinks
/// without following them, so nothing outside the root is touched.
#[cfg(all(test, unix))]
struct ShadowRoot {
    path: std::path::PathBuf,
    _alive: std::fs::File,
}

#[cfg(all(test, unix))]
impl ShadowRoot {
    fn claim(parent: &std::path::Path) -> std::io::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        // Safety: `geteuid` has no preconditions.
        let me = unsafe { libc::geteuid() };
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let meta = std::fs::symlink_metadata(parent)?;
        if !meta.is_dir() || meta.uid() != me {
            return Err(std::io::Error::other(format!(
                "{} is not a directory owned by the current user",
                parent.display()
            )));
        }
        let registry = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(parent.join(".registry"))?;
        registry.lock()?;
        for entry in std::fs::read_dir(parent)?.flatten() {
            let owned = entry.file_name().to_string_lossy().starts_with("root-")
                && entry.file_type().is_ok_and(|t| t.is_dir())
                && entry.metadata().is_ok_and(|m| m.uid() == me);
            if !owned {
                continue;
            }
            let owner_gone = match std::fs::File::open(entry.path().join(".alive")) {
                Ok(alive) => alive.try_lock().is_ok(),
                Err(error) => error.kind() == std::io::ErrorKind::NotFound,
            };
            if owner_gone {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let path = parent.join(format!("root-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        let alive = std::fs::OpenOptions::new().create_new(true).write(true).open(path.join(".alive"))?;
        alive.lock()?;
        Ok(ShadowRoot { path, _alive: alive })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[cfg(all(test, unix))]
impl FakeProgram {
    /// Take `PATH` over, with `entries` after the fake programs' own
    /// directory.
    async fn take_path(entries: impl FnOnce(&Option<std::ffi::OsString>) -> Vec<std::path::PathBuf>) -> Self {
        let lock = crate::test_helpers::PATH_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("invocations.log");
        let saved_path = std::env::var_os("PATH");
        let mut path = vec![dir.path().to_path_buf()];
        path.extend(entries(&saved_path));
        let new_path = std::env::join_paths(path).expect("join PATH");
        // Safety: serialized via PATH_LOCK, held by this value and
        // released only after `Drop` has restored PATH.
        unsafe {
            std::env::set_var("PATH", new_path);
        }
        FakeProgram { _lock: lock, dir, log, saved_path }
    }

    /// An executable `name` at the front of `PATH`. `body` runs after
    /// the invocation has been logged, in whatever directory the caller
    /// spawned it from.
    pub async fn install(name: &str, body: &str) -> Self {
        let fake = Self::take_path(|saved| {
            saved.as_ref().map(|p| std::env::split_paths(p).collect()).unwrap_or_default()
        })
        .await;
        fake.add(name, body);
        fake
    }

    /// The current `PATH` with the programs in `hidden` taken off it, so
    /// that they are absent whether or not this machine has them installed.
    ///
    /// Everything else stays reachable, because tests that do not take
    /// [`PATH_LOCK`] still run meanwhile and spawn whatever they need: a
    /// directory holding a hidden program is replaced by one that links
    /// every other entry in it.
    pub async fn without(hidden: &[&str]) -> Self {
        let shadows = SHADOW_ROOT.path().join(uuid::Uuid::new_v4().to_string());
        Self::take_path(|saved| {
            let Some(saved) = saved else { return Vec::new() };
            path_entries_without(saved, hidden, &shadows)
        })
        .await
    }

    /// Where `name` is on the `PATH` this value replaced, which no other
    /// test could have been changing while it was read: the program a fake
    /// can hand the calls it does not intercept to.
    pub fn original(&self, name: &str) -> Option<std::path::PathBuf> {
        let saved = self.saved_path.as_ref()?;
        std::env::split_paths(saved).map(|dir| dir.join(name)).find(|p| p.is_file())
    }

    /// Install one more fake program `name` beside the others.
    pub fn add(&self, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let program = self.dir.path().join(name);
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s %s\\n' {name:?} \"$*\" >> {:?}\n{body}\n",
                self.log
            ),
        )
        .expect("write fake program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake program");
    }

    pub fn invocations(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

/// The entries of `path` with the programs in `hidden` taken off them: a
/// directory holding a hidden program is replaced by one under `shadows`
/// that links every other entry in it, so everything else stays reachable,
/// for this test and for tests that do not take [`PATH_LOCK`] and run
/// meanwhile.
#[cfg(all(test, unix))]
pub fn path_entries_without(
    path: &std::ffi::OsStr,
    hidden: &[&str],
    shadows: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    std::env::split_paths(path)
        .enumerate()
        .map(|(i, dir)| {
            if !hidden.iter().any(|name| dir.join(name).exists()) {
                return dir;
            }
            let shadow = shadows.join(i.to_string());
            std::fs::create_dir_all(&shadow).expect("shadow dir");
            for entry in std::fs::read_dir(&dir).expect("read PATH dir").flatten() {
                let name = entry.file_name();
                if !hidden.iter().any(|h| name == std::ffi::OsStr::new(h)) {
                    let _ = std::os::unix::fs::symlink(entry.path(), shadow.join(&name));
                }
            }
            shadow
        })
        .collect()
}

#[cfg(all(test, unix))]
impl Drop for FakeProgram {
    fn drop(&mut self) {
        // Safety: PATH_LOCK is still held; `_lock` drops after this.
        unsafe {
            match &self.saved_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

/// A directory that refuses new files for as long as this value lives, with
/// what is already in it left readable.
///
/// Makes a durable write fail for real, through the ordinary code path,
/// while the last good file stays on disk to compare against. The atomic
/// write creates its temporary file beside the target, which a directory
/// without write permission refuses.
#[cfg(all(test, unix))]
pub struct UnwritableDir(std::path::PathBuf);

#[cfg(all(test, unix))]
impl UnwritableDir {
    /// Take write permission away from `dir`. `None`, with the permission
    /// given back, when the directory still accepts a file anyway -- as it
    /// does for root, who ignores permission bits -- so a caller can skip a
    /// test that could not prove anything.
    pub fn new(dir: &std::path::Path) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555))
            .expect("take write permission away");
        let guard = Self(dir.to_path_buf());
        let probe = dir.join(".write-probe");
        if std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(probe);
            return None;
        }
        Some(guard)
    }
}

#[cfg(all(test, unix))]
impl Drop for UnwritableDir {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// The IPC server, reachable from the integration tests.
///
/// `tests/ipc_integration.rs` is a separate crate, so it can only name items
/// that are public *and* reachable from the crate root. Re-exporting here means
/// the tests keep working whatever `lib.rs` decides about the visibility of the
/// `ipc` module itself.
pub use crate::ipc::{serve, IpcContext, IpcServer};

/// An [`IpcContext`] with empty state, rooted at explicit directories.
///
/// Everything the IPC server does — framing, protocol versioning,
/// authentication, authorization — is worth testing without a window, a webview
/// or the developer's real configuration. So the event sink discards, the
/// instance control is inert, and every path lands in whatever tempdir the
/// caller passed.
pub fn ipc_test_context(paths: std::sync::Arc<crate::config::paths::AppPaths>) -> IpcContext {
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{Mutex, RwLock};

    let tasks = Arc::new(RwLock::new(HashMap::new()));

    // Built field by field rather than through `PtyState::new`, which resolves
    // the real OS directories and rewrites the developer's live terminal
    // session file as a side effect.
    let pty = crate::pty::PtyState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        scrollback: crate::pty::store::ScrollbackManager::new(),
        store: Arc::new(
            crate::pty::store::SessionStore::with_paths(&paths)
                .expect("a session store under a tempdir should always be creatable"),
        ),
    };

    IpcContext {
        tasks: tasks.clone(),
        projects: Arc::new(RwLock::new(HashMap::new())),
        executions: Arc::new(RwLock::new(HashMap::new())),
        pty,
        queue_manager: Arc::new(RwLock::new(crate::queue::QueueManager::with_config(tasks))),
        storage: crate::config::Storage::with_paths((*paths).clone()),
        events: crate::events::null_sink(),
        control: Arc::new(crate::instance::InertControl),
        features: Arc::new(RwLock::new(
            crate::config::features::FeatureFlags::default(),
        )),
        repositories: Arc::new(RwLock::new(HashMap::new())),
        worktree_manager: Arc::new(crate::worktree::WorktreeManager::new(paths.clone())),
        task_lifecycle_locks: Arc::new(crate::lifecycle::TaskLifecycleLocks::new()),
        executor: Arc::new(tokio::sync::OnceCell::new()),
        feature_diagnostics: None,
        paths,
        start_guard: plenty_of_disk(),
    }
}

/// A filesystem a test controls, for tests about disk pressure. 500 GiB in
/// total, so the default policy's thresholds are its floors: Warning below
/// 120 GiB free and Critical below 40 GiB.
#[derive(Clone)]
pub struct FakeDisk {
    available: std::sync::Arc<std::sync::atomic::AtomicU64>,
    failing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl FakeDisk {
    pub const TOTAL: u64 = 500 * crate::domain::storage_usage::GIB;

    pub fn with_available(bytes: u64) -> Self {
        Self {
            available: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(bytes)),
            failing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reads: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// What the next reading reports as available.
    pub fn available(&self) -> u64 {
        self.available.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn set_available(&self, bytes: u64) {
        self.available.store(bytes, std::sync::atomic::Ordering::SeqCst);
        self.failing.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Make every reading fail, as a `statvfs` error would.
    pub fn fail(&self) {
        self.failing.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// How many times the filesystem has been read.
    pub fn reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn guard(&self) -> std::sync::Arc<crate::queue::start_guard::StartGuard> {
        let disk = self.clone();
        std::sync::Arc::new(crate::queue::start_guard::StartGuard::new(std::sync::Arc::new(move || {
            use std::sync::atomic::Ordering;
            disk.reads.fetch_add(1, Ordering::SeqCst);
            if disk.failing.load(Ordering::SeqCst) {
                return Err(std::io::Error::other("statvfs failed"));
            }
            Ok(crate::domain::storage_usage::FilesystemSpace {
                total_bytes: Self::TOTAL,
                available_bytes: disk.available.load(Ordering::SeqCst),
            })
        })))
    }
}

/// Pull request status that can never reach GitHub: the program it would run
/// does not exist, so every refresh fails at once as "not installed".
pub fn no_github() -> std::sync::Arc<crate::pr_status::PrStatuses> {
    std::sync::Arc::new(crate::pr_status::PrStatuses::with_program(
        "/nonexistent/slashit-test-gh",
        std::time::Duration::from_secs(1),
    ))
}

/// A start guard that always finds plenty of free space.
///
/// Tests build state on whatever disk the machine has, and a CI runner's is
/// below the critical floor, so a test that is not about disk pressure must
/// not depend on it. Tests that are about it build their own
/// [`crate::queue::start_guard::StartGuard`].
pub fn plenty_of_disk() -> std::sync::Arc<crate::queue::start_guard::StartGuard> {
    use crate::domain::storage_usage::{FilesystemSpace, GIB};
    std::sync::Arc::new(crate::queue::start_guard::StartGuard::new(std::sync::Arc::new(|| {
        Ok(FilesystemSpace { total_bytes: 1000 * GIB, available_bytes: 900 * GIB })
    })))
}

/// A real [`crate::queue::TaskExecutor`], wired to an already-built
/// [`crate::AppState`]'s own handles the same way `lib.rs`'s `setup()` wires
/// the production one -- sharing its `tasks`, `storage`, `task_lifecycle_locks`
/// and friends rather than building fresh ones -- and installed into
/// `state.executor`.
///
/// For a lifecycle-front-door test that needs a live execution/review owner
/// registered (see `TaskExecutor::register_fake_running_execution_for_test`
/// and its `reviewing_handles` sibling) to prove a command like
/// `update_task_status` or `reorder_task` ends that owner before committing
/// its own status write. A command reached through `tauri::State` sees
/// exactly this executor via `state.executor.get()`, the same as it would in
/// the running app.
pub fn attach_test_executor(state: &crate::AppState) -> std::sync::Arc<crate::queue::TaskExecutor> {
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let executor = Arc::new(crate::queue::TaskExecutor::new(
        crate::queue::executor::TaskExecutorConfig {
            tasks: state.task.tasks.clone(),
            queue_manager: state.queue.manager.clone(),
            executions: Arc::new(RwLock::new(HashMap::new())),
            logs: Arc::new(RwLock::new(HashMap::new())),
            projects: state.project.projects.clone(),
            repositories: state.repository.repositories.clone(),
            workspace_registry: state.workspace.registry.clone(),
            storage: state.storage.clone(),
            worktree_manager: state.worktree_manager.clone(),
            events: state.events(),
            lifecycle: state.task_lifecycle_locks.clone(),
            start_guard: state.start_guard.clone(),
            pr_statuses: state.pr_statuses.clone(),
        },
    ));
    let _ = state.executor.set(executor.clone());
    executor
}

/// Same as [`attach_test_executor`], for an [`IpcContext`] rather than an
/// [`crate::AppState`] -- the daemon's own handles, so an IPC handler test
/// can prove the same thing through `ctx.executor` that the desktop test
/// proves through `state.executor`.
pub async fn attach_test_executor_ipc(ctx: &IpcContext) -> std::sync::Arc<crate::queue::TaskExecutor> {
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let executor = Arc::new(crate::queue::TaskExecutor::new(
        crate::queue::executor::TaskExecutorConfig {
            tasks: ctx.tasks.clone(),
            queue_manager: ctx.queue_manager.clone(),
            executions: Arc::new(RwLock::new(HashMap::new())),
            logs: Arc::new(RwLock::new(HashMap::new())),
            projects: ctx.projects.clone(),
            repositories: ctx.repositories.clone(),
            workspace_registry: Arc::new(RwLock::new(
                crate::config::WorkspaceRegistry::load_from(
                    ctx.paths.config_dir().join("workspaces.toml"),
                )
                .expect("a nonexistent workspaces file loads as an empty registry"),
            )),
            storage: ctx.storage.clone(),
            worktree_manager: ctx.worktree_manager.clone(),
            events: ctx.events.clone(),
            lifecycle: ctx.task_lifecycle_locks.clone(),
            start_guard: ctx.start_guard.clone(),
            pr_statuses: no_github(),
        },
    ));
    let _ = ctx.executor.set(executor.clone());
    executor
}

/// Create a test task with default values
pub fn create_test_task(title: &str) -> Task {
    Task {
        id: Uuid::new_v4(),
        project_id: Uuid::new_v4(),
        title: title.to_string(),
        description: None,
        status: TaskStatus::Backlog,
        model: "test-model".to_string(),
        planning_mode: false,
        dependencies: Vec::new(),
        worktree_id: None,
        jj_change_id: None,
        category: TaskCategory::Feature,
        priority: TaskPriority::Medium,
        complexity: TaskComplexity::Moderate,
        impact: TaskImpact::Medium,
        security_severity: SecuritySeverity::None,
        phase: TaskPhase::Planning,
        phase_progress: 0,
        overall_progress: 0,
        subtasks: Vec::new(),
        sequence_number: 0,
        github_issue_url: None,
        gitlab_issue_url: None,
        linear_ticket_id: None,
        jira_issue_key: None,
        pr_url: None,
        external_refs: Vec::new(),
        qa_signoff: None,
        human_review: Default::default(),
        stuck_since: None,
        error_message: None,
        worktree_path: None,
        branch_name: None,
        base_commit: None,
        branch_origin: None,
        pending_republish: None,
        cleanup_in_flight: false,
        run_recovery: None,
        position: 0,
        pr_review_plan: None,
        activity: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// Create a test task with specific status
pub fn create_test_task_with_status(title: &str, status: TaskStatus) -> Task {
    let mut task = create_test_task(title);
    task.status = status;
    task
}

/// Create a test task with specific project_id, status, and position
pub fn create_test_task_full(
    title: &str,
    project_id: Uuid,
    status: TaskStatus,
    position: i32,
) -> Task {
    let mut task = create_test_task(title);
    task.project_id = project_id;
    task.status = status;
    task.position = position;
    task
}

/// Build a Task + PrReviewPlan pair suitable for exercising
/// `address_pr_review_inner`. Default plan has 2 items: one Fix (approved,
/// inline comment) and one Skip.
pub fn create_test_pr_review_setup() -> (Task, crate::domain::task::PrReviewPlan) {
    use crate::domain::task::{
        PrReviewComment, PrReviewDecision, PrReviewItem, PrReviewPlan, PrCommentKind,
    };

    let mut task = create_test_task("Fix login bug");
    task.pr_url = Some("https://github.com/test-org/test-repo/pull/42".to_string());
    task.branch_name = Some("test-branch".to_string());

    let comments = vec![
        PrReviewComment {
            id: Some(101),
            kind: PrCommentKind::Inline,
            author: "reviewer".to_string(),
            author_association: Some("MEMBER".to_string()),
            body: "This variable is unused.".to_string(),
            path: Some("src/lib.rs".to_string()),
            line: Some(42),
            url: None,
            created_at: None,
            updated_at: None,
        },
        PrReviewComment {
            id: Some(102),
            kind: PrCommentKind::Inline,
            author: "reviewer".to_string(),
            author_association: Some("MEMBER".to_string()),
            body: "Nit: rename for clarity.".to_string(),
            path: Some("src/lib.rs".to_string()),
            line: Some(60),
            url: None,
            created_at: None,
            updated_at: None,
        },
    ];

    let items = vec![
        PrReviewItem {
            comment_id: Some(101),
            summary: "Remove unused variable".to_string(),
            decision: PrReviewDecision::Fix,
            reasoning: "Confirmed unused.".to_string(),
            proposed_change: "Delete the variable.".to_string(),
            approved: true,
            user_note: String::new(),
            fix_done: false,
            fix_uncommitted: false,
            reply_posted: false,
            last_agent_summary: None,
            last_error: None,
        pr_reply_text: None,
        reply_comment_id: None,
        fix_commit: None,
        fix_effect: None,
        },
        PrReviewItem {
            comment_id: Some(102),
            summary: "Rename suggestion (skipped)".to_string(),
            decision: PrReviewDecision::Skip,
            reasoning: "Out of scope for this PR.".to_string(),
            proposed_change: String::new(),
            approved: false,
            user_note: String::new(),
            fix_done: false,
            fix_uncommitted: false,
            reply_posted: false,
            last_agent_summary: None,
            last_error: None,
        pr_reply_text: None,
        reply_comment_id: None,
        fix_commit: None,
        fix_effect: None,
        },
    ];

    let plan = PrReviewPlan {
        generated_at: Utc::now(),
        pr_url: task.pr_url.clone().unwrap(),
        review_decision: None,
        comments,
        items,
        raw_plan: String::new(),
        last_apply: None,
        fixed_content: Vec::new(),
    };

    (task, plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_test_task() {
        let task = create_test_task("Test Task");
        assert_eq!(task.title, "Test Task");
        assert!(matches!(task.status, TaskStatus::Backlog));
    }

    #[test]
    fn test_create_test_task_with_status() {
        let task = create_test_task_with_status("In Progress Task", TaskStatus::InProgress);
        assert!(matches!(task.status, TaskStatus::InProgress));
    }
}

/// What keeps `FakeProgram::without`'s `PATH` shadows alive for exactly as
/// long as a process that may still use them, and no longer.
#[cfg(all(test, unix))]
mod shadow_tests {
    use super::*;
    use std::process::{Command, Stdio};

    const PROBE: &str = "test_helpers::shadow_tests::process_probe";

    /// A directory holding a program to hide and one to keep.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("fixture dir");
        std::fs::write(dir.path().join("hide-me"), b"").expect("hidden program");
        let real = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
            .map(|d| d.join("true"))
            .find(|p| p.is_file())
            .expect("a `true` on PATH");
        std::os::unix::fs::symlink(real, dir.path().join("tool")).expect("link kept program");
        dir
    }

    fn roots_in(tmp: &std::path::Path) -> Vec<String> {
        let Ok(read) = std::fs::read_dir(shadow_parent(tmp)) else { return Vec::new() };
        read.flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("root-"))
            .collect()
    }

    /// Run [`process_probe`] in a fresh process of this same test binary,
    /// with its temporary directory, and so its shadow root, under `tmp`.
    fn run_probe(mode: &str, tmp: &std::path::Path, fixture: &std::path::Path) -> std::process::Output {
        let mut path = vec![fixture.to_path_buf()];
        path.extend(std::env::split_paths(&std::env::var_os("PATH").expect("PATH")));
        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args([PROBE, "--exact", "--test-threads=1"])
            .env("TMPDIR", tmp)
            .env("PATH", std::env::join_paths(path).expect("join PATH"))
            .env("SLASHIT_SHADOW_PROBE", mode)
            .output()
            .expect("run the probe");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed")
                || mode == "abort",
            "the probe did not run: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        output
    }

    /// Only does anything when [`run_probe`] launched it: takes a shadow the
    /// way a test does, then ends the process the requested way.
    #[tokio::test]
    async fn process_probe() {
        let Some(mode) = std::env::var_os("SLASHIT_SHADOW_PROBE") else { return };
        let fake = FakeProgram::without(&["hide-me"]).await;
        assert!(!roots_in(&std::env::temp_dir()).is_empty());
        drop(fake);
        if mode == "abort" {
            std::process::abort();
        }
    }

    #[test]
    fn a_shadow_outlives_the_fake_program_for_a_child_that_inherited_it() {
        let fixture = fixture();
        let shadows = SHADOW_ROOT.path().join(uuid::Uuid::new_v4().to_string());
        let entries = path_entries_without(fixture.path().as_os_str(), &["hide-me"], &shadows);
        let path = std::env::join_paths(&entries).expect("join PATH");
        let shadow = shadows.join("0");
        assert_eq!(entries, vec![shadow.clone()]);

        let mut child = Command::new("/bin/sh")
            .args(["-c", "read _; tool && ! command -v hide-me"])
            .env("PATH", &path)
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn child");

        // A `FakeProgram` coming and going while the child holds the shadow.
        let rt = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
        rt.block_on(async { drop(FakeProgram::install("unused", "").await) });
        assert!(shadow.join("tool").exists(), "the shadow went with the fake program");

        drop(child.stdin.take()); // lets `read` return
        assert!(child.wait().expect("wait").success(), "the child lost its tools");
    }

    #[test]
    fn a_claim_reaps_only_roots_whose_owner_is_gone() {
        let tmp = tempfile::tempdir().expect("tmp");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("precious"), b"x").expect("outside file");
        let parent = shadow_parent(tmp.path());

        let live = ShadowRoot::claim(&parent).expect("live");
        // A root whose owner died: an unlocked `.alive` and a symlink out.
        let stale = parent.join("root-1-stale");
        std::fs::create_dir_all(stale.join("0")).expect("stale tree");
        std::fs::write(stale.join(".alive"), b"").expect("stale lock file");
        std::os::unix::fs::symlink(outside.path(), stale.join("0/out")).expect("link out");
        // One that never got as far as a lock file.
        std::fs::create_dir(parent.join("root-2-unfinished")).expect("unfinished");
        // Neither a root nor a directory: left alone.
        std::fs::write(parent.join("keep-me"), b"").expect("bystander");

        let second = ShadowRoot::claim(&parent).expect("second");
        let mut names = roots_in(tmp.path());
        names.sort();
        let mut want = vec![
            live.path().file_name().unwrap().to_string_lossy().into_owned(),
            second.path().file_name().unwrap().to_string_lossy().into_owned(),
        ];
        want.sort();
        assert_eq!(names, want, "only live roots remain");
        assert!(parent.join("keep-me").exists());
        assert!(outside.path().join("precious").exists(), "cleanup followed a symlink out");
    }

    #[test]
    fn a_normal_exit_reclaims_the_root_and_processes_do_not_accumulate() {
        let tmp = tempfile::tempdir().expect("tmp");
        let fixture = fixture();
        for _ in 0..3 {
            assert!(run_probe("exit", tmp.path(), fixture.path()).status.success());
            assert_eq!(roots_in(tmp.path()), Vec::<String>::new());
        }
    }

    #[test]
    fn a_crashed_process_leaves_a_root_that_the_next_process_reaps() {
        let tmp = tempfile::tempdir().expect("tmp");
        let fixture = fixture();
        assert!(!run_probe("abort", tmp.path(), fixture.path()).status.success());
        assert_eq!(roots_in(tmp.path()).len(), 1, "an abort cannot run exit handlers");
        assert!(run_probe("exit", tmp.path(), fixture.path()).status.success());
        assert_eq!(roots_in(tmp.path()), Vec::<String>::new());
    }
}
