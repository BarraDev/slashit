//! Tasks need a safe, explicit version-control base; a remote is optional.
//!
//! Each test here builds a real repository (Git, colocated Jujutsu, or
//! non-colocated Jujutsu) in a temporary directory, registers it through the
//! same commands the Create Project form invokes (`create_repository`,
//! `create_project`), and acquires a task's checkout through
//! `create_worktree`, which resolves the base with the same code the queue
//! executor uses. The repository-settings actions (`set_project_base`,
//! `detect_remote_default_branch`, `initialize_project_vcs`) are the real
//! commands too. Remotes are local bare repositories: nothing touches the
//! network.
//!
//! The Git and Jujutsu configuration of the whole process is isolated once,
//! before any test runs: a commit identity, and `init.defaultBranch` set to
//! a name nothing in SlashIt could have assumed, so that every branch a test
//! sees came from the tool's own configuration. This is its own test binary
//! because that environment is process-wide.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock};

use serde_json::Value;
use tauri::Manager;
use tempfile::TempDir;

use slashit_ui_lib::config::paths::AppPaths;
use slashit_ui_lib::domain::{AgentType, BranchOrigin, TaskStatus};
use slashit_ui_lib::test_helpers::create_test_task_full;
use slashit_ui_lib::AppState;

/// The branch name `git init` and `jj git init` pick in this process.
const DEFAULT_BRANCH: &str = "trunk-xyz";

struct Environment {
    _root: TempDir,
}

static ENVIRONMENT: LazyLock<Environment> = LazyLock::new(|| {
    let root = TempDir::new().expect("environment root");
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let git_config = root.path().join("gitconfig");
    fs::write(
        &git_config,
        format!(
            "[user]\n\tname = Readiness Test\n\temail = readiness@example.com\n[init]\n\tdefaultBranch = {DEFAULT_BRANCH}\n[commit]\n\tgpgsign = false\n"
        ),
    )
    .unwrap();
    let jj_config = root.path().join("jjconfig.toml");
    fs::write(
        &jj_config,
        "[user]\nname = \"Readiness Test\"\nemail = \"readiness@example.com\"\n",
    )
    .unwrap();
    // Safety: one burst inside the initialiser, which every test forces
    // before it starts a runtime or spawns anything.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", root.path().join("xdg-config"));
        std::env::set_var("XDG_DATA_HOME", root.path().join("xdg-data"));
        std::env::set_var("XDG_CACHE_HOME", root.path().join("xdg-cache"));
        std::env::set_var("XDG_RUNTIME_DIR", root.path().join("xdg-runtime"));
        std::env::set_var("GIT_CONFIG_GLOBAL", &git_config);
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("JJ_CONFIG", &jj_config);
    }
    Environment { _root: root }
});

fn run(program: &str, dir: &Path, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn");
    assert!(
        output.status.success(),
        "{program} {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn git(dir: &Path, args: &[&str]) -> String {
    run("git", dir, args)
}

fn jj(dir: &Path, args: &[&str]) -> String {
    let mut all = vec!["--color=never"];
    all.extend_from_slice(args);
    run("jj", dir, &all)
}

/// Every ref and `HEAD` of a repository: what a mutation of it would change.
fn fingerprint(repo: &Path) -> String {
    let refs = git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(symref)",
        ],
    );
    let head = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
    let head_oid = Command::new("git")
        .args(["rev-parse", "-q", "--verify", "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
    format!(
        "{refs}\nHEAD {} {}",
        String::from_utf8_lossy(&head.stdout).trim(),
        String::from_utf8_lossy(&head_oid.stdout).trim()
    )
}

/// A running application state over a fresh state root, and the scratch
/// directory repositories are made in.
struct World {
    tmp: TempDir,
    app: tauri::App<tauri::test::MockRuntime>,
}

impl World {
    async fn new() -> Self {
        LazyLock::force(&ENVIRONMENT);
        let tmp = TempDir::new().unwrap();
        let paths = Arc::new(AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let (mut state, _) = slashit_ui_lib::app_core::build_state_with_paths(paths)
            .await
            .expect("state");
        // Not about disk pressure: independent of the host's free space.
        state.start_guard = slashit_ui_lib::test_helpers::plenty_of_disk();
        let app = tauri::test::mock_app();
        app.manage(state);
        World { tmp, app }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let dir = self.tmp.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state(&self) -> tauri::State<'_, AppState> {
        self.app.state::<AppState>()
    }

    /// Register `folder` as Create Project does, optionally initializing it.
    async fn register(&self, folder: &Path, initialize: Option<&str>) -> Result<String, String> {
        let kind =
            initialize.map(|k| serde_json::from_value(Value::String(k.to_string())).unwrap());
        let repository = slashit_ui_lib::commands::create_repository(
            self.state(),
            folder.to_string_lossy().to_string(),
            None,
            kind,
            None,
        )
        .await?;
        let project = slashit_ui_lib::commands::create_project(
            self.state(),
            "readiness".to_string(),
            Some(repository.id.to_string()),
            AgentType::ClaudeCode,
        )
        .await?;
        Ok(project.id.to_string())
    }

    async fn readiness(&self, project: &str) -> Value {
        let report =
            slashit_ui_lib::commands::get_project_readiness(self.state(), project.to_string())
                .await
                .expect("readiness");
        serde_json::to_value(report).unwrap()
    }

    /// Create a task in `project` and acquire its checkout; the task as
    /// recorded afterwards, or why the checkout was refused.
    async fn start_task(&self, project: &str) -> Result<slashit_ui_lib::domain::Task, String> {
        let project_id = uuid::Uuid::parse_str(project).unwrap();
        let task = create_test_task_full("readiness task", project_id, TaskStatus::Backlog, 0);
        let task_id = task.id;
        let state: &AppState = self.state().inner();
        state.task.tasks.write().await.insert(task_id, task.clone());
        state
            .storage
            .save_project_tasks(project_id, &[task])
            .expect("seed board");
        slashit_ui_lib::commands::create_worktree(self.state(), task_id.to_string()).await?;
        Ok(state.task.tasks.read().await[&task_id].clone())
    }
}

/// A Git repository at `dir` with one commit holding a file, on whatever
/// branch Git names by default here.
fn git_repo(dir: &Path) -> String {
    git(dir, &["init", "-q"]);
    fs::write(dir.join("README.md"), "fixture\n").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-q", "-m", "first"]);
    git(dir, &["rev-parse", "HEAD"])
}

fn head_of(worktree: &str) -> String {
    git(Path::new(worktree), &["rev-parse", "HEAD"])
}

/// Git with commits and no remote: the branch the checkout was on at
/// registration is the project's base, and tasks start there.
#[tokio::test(flavor = "multi_thread")]
async fn a_repository_without_a_remote_runs_tasks_from_its_local_base() {
    let world = World::new().await;
    let repo = world.dir("local-only");
    let tip = git_repo(&repo);
    let project = world.register(&repo, None).await.unwrap();

    let report = world.readiness(&project).await;
    assert_eq!(report["project_base"], DEFAULT_BRANCH, "{report:#}");
    assert_eq!(report["remote"]["kind"], "no_origin");
    assert_eq!(report["base"]["source"], "local");

    let task = world.start_task(&project).await.expect("task starts");
    assert_eq!(task.base_commit.as_deref(), Some(tip.as_str()));
    assert_eq!(
        task.branch_origin,
        Some(BranchOrigin::LocalBase {
            branch: DEFAULT_BRANCH.to_string()
        })
    );
    assert_eq!(head_of(task.worktree_path.as_deref().unwrap()), tip);
}

/// Origin exists but `origin/HEAD` does not: the local base runs tasks;
/// Detect default branch then records `origin/HEAD` from origin without
/// changing anything on origin, and later tasks start at origin's default
/// branch.
#[tokio::test(flavor = "multi_thread")]
async fn a_remote_without_origin_head_does_not_block_tasks_and_can_be_detected() {
    let world = World::new().await;
    let repo = world.dir("repo");
    let tip = git_repo(&repo);
    let origin = world.tmp.path().join("origin.git");
    git(
        &repo,
        &[
            "clone",
            "-q",
            "--bare",
            repo.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&repo, &["fetch", "-q", "origin"]);
    // Git 2.48+ records it on fetch; 2.43 does not. Either way, gone.
    let _ = Command::new("git")
        .args(["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"])
        .current_dir(&repo)
        .output();
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "local, unpushed"],
    );
    let local_tip = git(&repo, &["rev-parse", "HEAD"]);
    let remote_before = fingerprint(&origin);

    let project = world.register(&repo, None).await.unwrap();
    let report = world.readiness(&project).await;
    assert_eq!(
        report["remote"]["kind"], "default_branch_unknown",
        "{report:#}"
    );
    let task = world
        .start_task(&project)
        .await
        .expect("local execution is not blocked");
    assert_eq!(task.base_commit.as_deref(), Some(local_tip.as_str()));
    assert_eq!(
        task.branch_origin,
        Some(BranchOrigin::LocalBase {
            branch: DEFAULT_BRANCH.to_string()
        })
    );

    let report =
        slashit_ui_lib::commands::detect_remote_default_branch(world.state(), project.clone())
            .await
            .expect("detected");
    let report = serde_json::to_value(report).unwrap();
    assert_eq!(
        report["remote"],
        serde_json::json!({ "kind": "default_branch", "branch": DEFAULT_BRANCH })
    );
    assert_eq!(
        git(&repo, &["symbolic-ref", "refs/remotes/origin/HEAD"]),
        format!("refs/remotes/origin/{DEFAULT_BRANCH}")
    );

    let task = world.start_task(&project).await.expect("task starts");
    assert_eq!(
        task.base_commit.as_deref(),
        Some(tip.as_str()),
        "origin's default branch wins once known"
    );
    assert_eq!(
        task.branch_origin,
        Some(BranchOrigin::DefaultBase {
            branch: Some(DEFAULT_BRANCH.to_string())
        })
    );

    assert_eq!(
        fingerprint(&origin),
        remote_before,
        "nothing was pushed and origin was not changed"
    );
}

/// A folder with files and no version control, initialized with Git from
/// Create Project: Git names the branch, the files are the first commit,
/// and tasks start there.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_git_repository_uses_the_branch_git_chose() {
    let world = World::new().await;
    let folder = world.dir("new-project");
    fs::write(folder.join("main.rs"), "fn main() {}\n").unwrap();
    fs::write(folder.join(".gitignore"), "target/\n").unwrap();
    fs::create_dir_all(folder.join("target")).unwrap();
    fs::write(folder.join("target/junk"), "built").unwrap();

    let preview =
        slashit_ui_lib::commands::preview_vcs_initialization(folder.to_string_lossy().to_string())
            .await
            .unwrap();
    let preview = serde_json::to_value(preview).unwrap();
    assert_eq!(preview["files"], 2, "{preview:#}");
    assert_eq!(preview["vcs"]["kind"], "none");
    assert!(!folder.join(".git").exists(), "a preview writes nothing");

    let project = world.register(&folder, Some("git")).await.unwrap();
    assert_eq!(
        git(&folder, &["symbolic-ref", "HEAD"]),
        format!("refs/heads/{DEFAULT_BRANCH}")
    );
    assert_eq!(
        git(&folder, &["ls-tree", "-r", "--name-only", "HEAD"]),
        ".gitignore\nmain.rs"
    );
    assert!(folder.join("target/junk").is_file());

    let report = world.readiness(&project).await;
    assert_eq!(report["project_base"], DEFAULT_BRANCH, "{report:#}");
    let task = world.start_task(&project).await.expect("task starts");
    assert_eq!(task.base_commit, Some(git(&folder, &["rev-parse", "HEAD"])));
}

/// The captured base stays the base: switching the primary checkout to a
/// feature branch with a new commit does not move where tasks start.
#[tokio::test(flavor = "multi_thread")]
async fn switching_the_primary_checkout_does_not_move_the_base() {
    let world = World::new().await;
    let repo = world.dir("repo");
    let tip = git_repo(&repo);
    let project = world.register(&repo, None).await.unwrap();

    git(&repo, &["checkout", "-q", "-b", "feature"]);
    fs::write(repo.join("feature.txt"), "feature\n").unwrap();
    git(&repo, &["add", "feature.txt"]);
    git(&repo, &["commit", "-q", "-m", "feature work"]);

    let task = world.start_task(&project).await.expect("task starts");
    assert_eq!(task.base_commit.as_deref(), Some(tip.as_str()));
    assert!(!Path::new(task.worktree_path.as_deref().unwrap())
        .join("feature.txt")
        .exists());
}

/// A colocated Jujutsu repository leaves Git's HEAD detached. The project
/// takes its bookmark as the base, tasks start at the bookmark without
/// relying on Git's HEAD, and JJ and Git still agree afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn a_colocated_jj_repository_with_a_detached_git_head_runs_tasks() {
    let world = World::new().await;
    let repo = world.dir("jj-repo");
    fs::write(repo.join("lib.rs"), "// base\n").unwrap();
    jj(&repo, &["git", "init", "--colocate"]);
    jj(&repo, &["describe", "-m", "base"]);
    jj(&repo, &["bookmark", "create", "trunk", "-r", "@"]);
    jj(&repo, &["new"]);
    fs::write(repo.join("wip.rs"), "// unpushed work in progress\n").unwrap();
    jj(&repo, &["status"]);
    let bookmark = git(&repo, &["rev-parse", "refs/heads/trunk"]);
    assert!(
        Command::new("git")
            .args(["symbolic-ref", "-q", "HEAD"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .code()
            != Some(0)
    );

    let project = world.register(&repo, None).await.unwrap();
    let report = world.readiness(&project).await;
    assert_eq!(report["vcs"]["kind"], "jj_colocated", "{report:#}");
    assert_eq!(report["head"]["kind"], "detached");
    // Being the only bookmark does not make it the trunk: it is suggested,
    // not captured, and nothing starts until the person confirms it.
    assert_eq!(report["project_base"], Value::Null, "{report:#}");
    assert_eq!(report["proposal"], "trunk");
    assert!(report["proposal_reason"]
        .as_str()
        .unwrap()
        .contains("only bookmark"));
    world
        .start_task(&project)
        .await
        .expect_err("no base until confirmed");
    slashit_ui_lib::commands::set_project_base(world.state(), project.clone(), "trunk".to_string())
        .await
        .expect("confirmed");

    let task = world.start_task(&project).await.expect("task starts");
    assert_eq!(task.base_commit.as_deref(), Some(bookmark.as_str()));
    let worktree = PathBuf::from(task.worktree_path.as_deref().unwrap());
    assert!(
        !worktree.join("wip.rs").exists(),
        "the working-copy change is not the base"
    );

    // Both views stay sane: JJ imports the task branch as a bookmark, sees no
    // conflict or divergence, and Git's view of the primary checkout is
    // unchanged.
    let status = jj(&repo, &["status"]);
    assert!(!status.contains("conflict"), "{status}");
    let bookmarks = jj(&repo, &["bookmark", "list"]);
    assert!(
        bookmarks.contains("trunk") && bookmarks.contains(task.branch_name.as_deref().unwrap()),
        "{bookmarks}"
    );
    assert!(!bookmarks.contains("conflict"), "{bookmarks}");
    let log = jj(
        &repo,
        &[
            "log",
            "-r",
            "all()",
            "--no-graph",
            "-T",
            "if(divergent, \"DIVERGENT\\n\", \"\")",
        ],
    );
    assert!(!log.contains("DIVERGENT"), "{log}");
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/trunk"]), bookmark);
    git(&repo, &["status", "--porcelain"]);
}

/// Ambiguity is refused with what to do, and nothing is guessed or written.
#[tokio::test(flavor = "multi_thread")]
async fn ambiguity_is_refused_and_nothing_is_guessed() {
    let world = World::new().await;

    // A detached HEAD at registration, no remote: no base is captured.
    let repo = world.dir("detached");
    let tip = git_repo(&repo);
    git(&repo, &["branch", "other"]);
    git(&repo, &["checkout", "-q", "--detach"]);
    let project = world.register(&repo, None).await.unwrap();
    let report = world.readiness(&project).await;
    assert_eq!(report["project_base"], Value::Null, "{report:#}");
    assert!(report["proposal_reason"]
        .as_str()
        .unwrap()
        .contains("detached HEAD"));
    let before = fingerprint(&repo);
    let refused = world.start_task(&project).await.expect_err("no base");
    assert!(
        refused.contains("Choose the local branch tasks should start from"),
        "{refused}"
    );
    assert_eq!(fingerprint(&repo), before, "nothing was created or moved");

    // An explicit choice resolves it.
    let report = slashit_ui_lib::commands::set_project_base(
        world.state(),
        project.clone(),
        "other".to_string(),
    )
    .await
    .expect("chosen");
    assert_eq!(report.project_base.as_deref(), Some("other"));
    let task = world.start_task(&project).await.expect("task starts");
    assert_eq!(task.base_commit.as_deref(), Some(tip.as_str()));

    // origin/HEAD pointing outside origin: refused even with a local base.
    let repo = world.dir("elsewhere");
    let tip = git_repo(&repo);
    let project = world.register(&repo, None).await.unwrap();
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            world.tmp.path().join("nowhere.git").to_str().unwrap(),
        ],
    );
    git(&repo, &["update-ref", "refs/remotes/upstream/trunk", &tip]);
    git(
        &repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/upstream/trunk",
        ],
    );
    let before = fingerprint(&repo);
    let refused = world
        .start_task(&project)
        .await
        .expect_err("origin/HEAD outside origin");
    assert!(refused.contains("not at a branch of origin"), "{refused}");
    assert_eq!(fingerprint(&repo), before);

    // Origin's HEAD detached at a commit two branches share: Detect default
    // branch refuses, and neither side changes.
    let repo = world.dir("ambiguous");
    let tip = git_repo(&repo);
    git(&repo, &["branch", "other"]);
    let origin = world.tmp.path().join("ambiguous.git");
    git(
        &repo,
        &[
            "clone",
            "-q",
            "--bare",
            repo.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    git(&origin, &["update-ref", "--no-deref", "HEAD", &tip]);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&repo, &["fetch", "-q", "origin"]);
    let _ = Command::new("git")
        .args(["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"])
        .current_dir(&repo)
        .output();
    let project = world.register(&repo, None).await.unwrap();
    let (local_before, remote_before) = (fingerprint(&repo), fingerprint(&origin));
    let refused =
        slashit_ui_lib::commands::detect_remote_default_branch(world.state(), project.clone())
            .await
            .expect_err("ambiguous remote");
    assert!(refused.contains("will not guess"), "{refused}");
    assert_eq!(
        (fingerprint(&repo), fingerprint(&origin)),
        (local_before, remote_before)
    );
}

/// A Jujutsu repository that is not colocated with Git is recognised and
/// refused with what to do; nothing is created in it.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_colocated_jj_repository_is_refused_truthfully() {
    let world = World::new().await;
    let repo = world.dir("jj-only");
    fs::write(repo.join("lib.rs"), "// base\n").unwrap();
    jj(&repo, &["git", "init", "--no-colocate"]);
    assert!(!repo.join(".git").exists());

    let project = world.register(&repo, None).await.unwrap();
    let report = world.readiness(&project).await;
    assert_eq!(report["vcs"]["kind"], "jj_not_colocated", "{report:#}");
    assert!(report["blocked"]
        .as_str()
        .unwrap()
        .contains("jj git colocation enable"));
    let refused = world.start_task(&project).await.expect_err("not colocated");
    assert!(refused.contains("not colocated with Git"), "{refused}");
    assert!(!repo.join(".git").exists(), "nothing was created");
}

/// A folder with no version control is registered in a setup-required
/// state, never initialized on its own, and becomes usable through the
/// explicit Initialize action, with Git or with Jujutsu.
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_without_version_control_needs_an_explicit_initialization() {
    let world = World::new().await;
    for kind in ["git", "jujutsu"] {
        let folder = world.dir(&format!("plain-{kind}"));
        fs::write(folder.join("notes.md"), "notes\n").unwrap();
        // Above JJ's default 1 MiB limit for new files: Git records it,
        // and Jujutsu initialization is refused rather than leave it out.
        fs::write(folder.join("large.bin"), vec![7u8; 2 * 1024 * 1024]).unwrap();

        let project = world.register(&folder, None).await.unwrap();
        let report = world.readiness(&project).await;
        assert_eq!(report["vcs"]["kind"], "none", "{report:#}");
        assert!(report["blocked"]
            .as_str()
            .unwrap()
            .contains("Initialize one from Settings > Repository"));
        let refused = world
            .start_task(&project)
            .await
            .expect_err("no version control");
        assert!(refused.contains("not under version control"), "{refused}");
        assert!(
            !folder.join(".git").exists() && !folder.join(".jj").exists(),
            "starting never initializes"
        );

        let kind_value = || serde_json::from_value(Value::String(kind.to_string())).unwrap();
        if kind == "jujutsu" {
            let refused = slashit_ui_lib::commands::initialize_project_vcs(
                world.state(),
                project.clone(),
                kind_value(),
                None,
            )
            .await
            .expect_err("a file above JJ's limit would be left out");
            assert!(
                refused.contains("large.bin") && refused.contains("snapshot.max-new-file-size"),
                "{refused}"
            );
            assert!(!folder.join(".jj").exists() && !folder.join(".git").exists());
            fs::remove_file(folder.join("large.bin")).unwrap();
        }
        // A count other than what the folder holds means the person saw
        // something else: refused, nothing changed.
        let refused = slashit_ui_lib::commands::initialize_project_vcs(
            world.state(),
            project.clone(),
            kind_value(),
            Some(99),
        )
        .await
        .expect_err("stale preview");
        assert!(refused.contains("not the 99 shown"), "{refused}");
        assert!(!folder.join(".jj").exists() && !folder.join(".git").exists());
        let shown = if kind == "jujutsu" { 1 } else { 2 };
        let report = slashit_ui_lib::commands::initialize_project_vcs(
            world.state(),
            project.clone(),
            kind_value(),
            Some(shown),
        )
        .await
        .expect("initialized");
        assert_eq!(
            report.project_base.as_deref(),
            Some(DEFAULT_BRANCH),
            "{kind}"
        );
        let tree = git(
            &folder,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                &format!("refs/heads/{DEFAULT_BRANCH}"),
            ],
        );
        let expected = if kind == "jujutsu" {
            "notes.md"
        } else {
            "large.bin\nnotes.md"
        };
        assert_eq!(tree, expected, "{kind}");
        if kind == "jujutsu" {
            assert!(
                folder.join(".jj").is_dir() && folder.join(".git").is_dir(),
                "colocated"
            );
        }
        let task = world.start_task(&project).await.expect("task starts");
        assert_eq!(
            task.base_commit,
            Some(git(
                &folder,
                &["rev-parse", &format!("refs/heads/{DEFAULT_BRANCH}")]
            ))
        );
    }
}

/// A Git repository with no commit (what `git init` alone leaves, and what
/// earlier versions' Initialize Git created) is refused saying so, and the
/// explicit Initialize action gives it its first commit.
#[tokio::test(flavor = "multi_thread")]
async fn a_repository_without_commits_gets_its_first_commit_on_request() {
    let world = World::new().await;
    let repo = world.dir("no-commits");
    git(&repo, &["init", "-q"]);
    fs::write(repo.join("draft.txt"), "draft\n").unwrap();
    let project = world.register(&repo, None).await.unwrap();

    let refused = world.start_task(&project).await.expect_err("no commit");
    assert!(refused.contains("no commits yet"), "{refused}");

    let kind = serde_json::from_value(Value::String("git".to_string())).unwrap();
    let report = slashit_ui_lib::commands::initialize_project_vcs(
        world.state(),
        project.clone(),
        kind,
        None,
    )
    .await
    .expect("first commit");
    assert_eq!(report.project_base.as_deref(), Some(DEFAULT_BRANCH));
    assert_eq!(
        git(&repo, &["ls-tree", "-r", "--name-only", "HEAD"]),
        "draft.txt"
    );
    world.start_task(&project).await.expect("task starts");
}

/// A colocated JJ repository whose own `trunk()` alias names a bookmark has
/// that bookmark captured at registration, among several.
#[tokio::test(flavor = "multi_thread")]
async fn a_colocated_jj_repository_whose_trunk_names_a_bookmark_captures_it() {
    let world = World::new().await;
    let repo = world.dir("jj-trunk");
    fs::write(repo.join("lib.rs"), "// base\n").unwrap();
    jj(&repo, &["git", "init", "--colocate"]);
    jj(&repo, &["describe", "-m", "base"]);
    jj(&repo, &["bookmark", "create", "stable", "-r", "@"]);
    jj(&repo, &["new"]);
    jj(&repo, &["describe", "-m", "feature"]);
    jj(&repo, &["bookmark", "create", "feature", "-r", "@"]);
    jj(&repo, &["new"]);
    jj(
        &repo,
        &[
            "config",
            "set",
            "--repo",
            "revset-aliases.\"trunk()\"",
            "stable",
        ],
    );
    let stable = git(&repo, &["rev-parse", "refs/heads/stable"]);

    let project = world.register(&repo, None).await.unwrap();
    let report = world.readiness(&project).await;
    assert_eq!(report["project_base"], "stable", "{report:#}");
    let task = world.start_task(&project).await.expect("task starts");
    assert_eq!(task.base_commit.as_deref(), Some(stable.as_str()));
}
