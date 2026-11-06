// SPDX-License-Identifier: MPL-2.0
//! Phase 1 checks: what the current persistence code actually does, before
//! the portable bundle relies on any of it.
//!
//! Each test states one claim from the portable-export investigation and
//! either confirms or refutes it. They stay in the tree as regression tests
//! for the behavior the bundle design depends on.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use tempfile::TempDir;
use uuid::Uuid;

use crate::config::paths::AppPaths;
use crate::config::{AppConfig, Storage};
use crate::domain::{
    AgentConfig, AgentType, HumanReviewDecision, HumanReviewRecord, Project, ProjectScope,
    Repository, Task,
};

fn storage() -> (Storage, TempDir) {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    fs::create_dir_all(root.join("config")).unwrap();
    fs::create_dir_all(root.join("data")).unwrap();
    let paths = AppPaths::with_roots(
        root.join("config"),
        root.join("data"),
        root.join("cache"),
        root.join("runtime"),
    );
    (Storage::with_paths(paths), temp)
}

/// Every file under `root` with its exact bytes, for before-and-after
/// comparisons.
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    out
}

fn project(id: Uuid, repository_id: Option<Uuid>) -> Project {
    Project {
        id,
        name: "Phase 1".into(),
        repository_id,
        scope: ProjectScope::Standalone,
        state_location: Default::default(),
        base: None,
        agent_type: AgentType::ClaudeCode,
        agent_config: AgentConfig {
            agent_type: AgentType::ClaudeCode,
            command: String::new(),
            args: Vec::new(),
            env: Default::default(),
            model: None,
            api_key: None,
        },
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn repository(id: Uuid, path: &Path) -> Repository {
    Repository {
        id,
        local_path: path.to_string_lossy().into_owned(),
        remote_url: None,
        remote_type: None,
        created_at: Utc::now(),
    }
}

/// One Project, its Repository at `path`, saved to the config file.
fn save_project_at(storage: &Storage, project_id: Uuid, path: &Path) -> Uuid {
    let repository_id = Uuid::new_v4();
    let mut config = AppConfig::default();
    config
        .repositories
        .insert(repository_id.to_string(), repository(repository_id, path));
    config.projects.insert(
        project_id.to_string(),
        project(project_id, Some(repository_id)),
    );
    storage.save_config(&config).expect("save config");
    repository_id
}

fn board_task(project_id: Uuid, title: &str) -> Task {
    let mut task = crate::test_helpers::create_test_task(title);
    task.project_id = project_id;
    task
}

/// Claim 1 (investigation hazard 1): a Project's board is keyed by its
/// repository path, so changing that path moves the board.
///
/// Confirmed if the board written under the old path is no longer found after
/// the path changes, while the file still exists on disk.
#[test]
fn board_key_follows_the_repository_path() {
    let (storage, temp) = storage();
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();
    let project_id = Uuid::new_v4();
    let repository_id = save_project_at(&storage, project_id, &repo_a);

    storage
        .save_project_tasks(project_id, &[board_task(project_id, "kept")])
        .unwrap();
    assert_eq!(storage.load_project_tasks(project_id).unwrap().len(), 1);

    // The same Repository record, pointed at another directory.
    let mut config = storage.load_config().unwrap();
    config
        .repositories
        .get_mut(&repository_id.to_string())
        .unwrap()
        .local_path = repo_b.to_string_lossy().into_owned();
    storage.save_config(&config).unwrap();

    assert!(
        storage.load_project_tasks(project_id).unwrap().is_empty(),
        "the board under the old key must not be found after the path changes"
    );
    let boards = count_named(&temp.path().join("data"), "tasks.toml");
    assert_eq!(
        boards, 1,
        "the original board file is still on disk, under the old key"
    );
}

/// Claim 2 (investigation hazard 2): a Project with no board file loads as an
/// empty board, not as an error.
#[test]
fn an_absent_board_loads_as_an_empty_board() {
    let (storage, temp) = storage();
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let project_id = Uuid::new_v4();
    save_project_at(&storage, project_id, &repo);

    let tasks = storage
        .load_project_tasks(project_id)
        .expect("absence is not an error");
    assert!(tasks.is_empty());
    assert!(storage.load_all_tasks().expect("load all").is_empty());
}

/// Claim 3 (investigation risk): the board reads are pure. Loading a board
/// must not change any byte of the store, which the preview depends on.
#[test]
fn board_reads_change_no_byte_of_the_store() {
    let (storage, temp) = storage();
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let project_id = Uuid::new_v4();
    save_project_at(&storage, project_id, &repo);
    storage
        .save_project_tasks(project_id, &[board_task(project_id, "read me")])
        .unwrap();

    let before = tree(&temp.path().join("data"));
    let config_before = fs::read(temp.path().join("config").join("config.toml")).unwrap();
    let _ = storage.load_project_tasks(project_id).unwrap();
    let _ = storage.load_all_tasks().unwrap();
    let _ = storage.task_board_diagnostics();
    assert_eq!(tree(&temp.path().join("data")), before);
    assert_eq!(
        fs::read(temp.path().join("config").join("config.toml")).unwrap(),
        config_before
    );
}

/// Claim 4 (investigation risk, verified by reading): loading the config is
/// not always a read. A config that does not parse is salvaged, and the
/// original file is moved aside. Preview therefore must never call
/// `load_config`.
#[test]
fn loading_an_unparseable_config_moves_the_file_aside() {
    let (storage, temp) = storage();
    let config_file = temp.path().join("config").join("config.toml");
    fs::write(&config_file, "this is = [ not valid toml").unwrap();

    let _ = storage
        .load_config()
        .expect("the recovery reader still answers");

    assert!(!config_file.exists(), "load_config moved the config file");
    assert!(config_file.with_extension("toml.corrupted").exists());
}

/// Claim 5 (investigation hazard 5): the approval a Human recorded stays the
/// current decision for as long as the arrival count is unchanged. Bumping the
/// count retires it, and a retired approval does not authorize the branch.
#[test]
fn an_approval_is_current_only_in_its_own_arrival() {
    let mut record = HumanReviewRecord::default();
    record.record_arrival();
    record.approve(Some("a".repeat(40)), Utc::now());
    assert!(record.is_approved());
    assert!(record.binds(true, Some(&"a".repeat(40))).is_ok());

    // A record copied as it stands would still be current.
    let copied = record.clone();
    assert!(
        copied.is_approved(),
        "identical arrivals keep the approval current"
    );

    // Withdrawing the approval (what a later run does) retires it.
    record.withdraw_approval();
    assert!(!record.is_approved());
    assert!(record.current_decision().is_none());
    assert!(
        record.binds(true, Some(&"b".repeat(40))).is_ok(),
        "no current approval to bind"
    );
    assert_eq!(
        record.entries.last().unwrap().decision,
        HumanReviewDecision::Approved
    );
}

fn count_named(root: &Path, name: &str) -> usize {
    let mut count = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == name) {
                count += 1;
            }
        }
    }
    count
}
