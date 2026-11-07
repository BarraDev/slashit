// SPDX-License-Identifier: MPL-2.0
//! Export v1 and import preview: golden bytes, secret and path absence,
//! refusals, rejected bundles, conflicts, the no-write proof and the snapshot
//! barriers. Driven through the IPC dispatcher, so the production handlers are
//! what is tested.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use slashit_activity::{Column, Entry as ActivityEntry, Kind as ActivityKind, RunResult};
use slashit_durable_fs::{fault, Stage};
use slashit_ipc::{Endpoint, IpcRequest, IpcResponse};
use tempfile::TempDir;
use uuid::Uuid;

use crate::config::paths::AppPaths;
use crate::domain::publication::PrPublication;
use crate::domain::{
    AgentConfig, AgentType, ApprovedExecution, ExternalRef, HumanReviewDecision, HumanReviewEntry,
    HumanReviewRecord, Project, ProjectScope, QaSignoff, QaStatus, Repository, Task, TaskStatus,
    UnconfirmedPrCreate,
};
use crate::ipc::handlers::dispatch;
use crate::ipc::{IpcContext, PeerContext};
use crate::test_helpers::{create_test_task_full, ipc_test_context};

use super::bundle::{self, BundleError, Payload, FORMAT_VERSION, MAX_BUNDLE_BYTES};
use super::{export, preview};

const GOLDEN: &[u8] = include_bytes!("testdata/export-v1.golden.json");

pub(super) fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-01T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

pub(super) fn peer() -> PeerContext {
    PeerContext {
        endpoint: Endpoint::Unix {
            path: PathBuf::from("/nonexistent-for-tests"),
        },
        os_verified: true,
    }
}

pub(super) async fn test_ctx() -> (TempDir, IpcContext) {
    let tmp = TempDir::new().expect("tempdir");
    let paths = Arc::new(AppPaths::with_roots(
        tmp.path().join("config"),
        tmp.path().join("data"),
        tmp.path().join("cache"),
        tmp.path().join("runtime"),
    ));
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    fs::create_dir_all(tmp.path().join("data")).unwrap();
    let ctx = ipc_test_context(paths);
    (tmp, ctx)
}

pub(super) fn project_id() -> Uuid {
    Uuid::from_u128(1)
}

fn repository_id() -> Uuid {
    Uuid::from_u128(2)
}

pub(super) fn project(repository: Option<Uuid>) -> Project {
    Project {
        id: project_id(),
        name: "Fixture".into(),
        repository_id: repository,
        scope: ProjectScope::Standalone,
        state_location: Default::default(),
        base: None,
        agent_type: AgentType::ClaudeCode,
        agent_config: AgentConfig {
            agent_type: AgentType::ClaudeCode,
            command: "/SECRET-CMD/claude".into(),
            args: vec!["--password".into(), "SECRET-ARG".into()],
            env: HashMap::from([("TOKEN".to_string(), "SECRET-ENV".to_string())]),
            model: Some("sonnet".into()),
            api_key: Some("SECRET-APIKEY".into()),
        },
        created_at: at(),
        updated_at: at(),
    }
}

pub(super) fn repository(path: &Path) -> Repository {
    Repository {
        id: repository_id(),
        local_path: path.to_string_lossy().into_owned(),
        remote_url: Some("https://user:ghp_SECRETTOKEN@github.com/acme/app.git?token=x".into()),
        remote_type: Some(crate::domain::RemoteType::GitHub),
        created_at: at(),
    }
}

fn entry(seq: u32, kind: ActivityKind) -> ActivityEntry {
    ActivityEntry {
        seq,
        at: at(),
        kind,
    }
}

/// Two Tasks: `Alpha` in Backlog with a timeline, and `Beta` in Human Review,
/// depending on Alpha, with a current approval and run output in its timeline.
pub(super) fn fixture_tasks() -> Vec<Task> {
    let mut alpha = create_test_task_full("Alpha", project_id(), TaskStatus::Backlog, 0);
    alpha.id = Uuid::from_u128(10);
    alpha.sequence_number = 1;
    alpha.description = Some("First task".into());
    alpha.created_at = at();
    alpha.updated_at = at();
    alpha.worktree_path = Some("/SECRET-HOME/wt/alpha".into());
    alpha.branch_name = Some("SECRET-BRANCH".into());
    alpha.activity = vec![
        entry(
            1,
            ActivityKind::Moved {
                from: Column::Backlog,
                to: Column::Queue,
            },
        ),
        entry(
            2,
            ActivityKind::RunFailed {
                run: Some(1),
                reason: "API_KEY=SECRET-ASSIGN failed in /SECRET-HOME/wt/alpha".into(),
            },
        ),
        entry(
            3,
            ActivityKind::RunResult {
                run: 1,
                result: RunResult::Available {
                    text: "SECRET-RESULT".into(),
                },
            },
        ),
        entry(
            4,
            ActivityKind::ToolUsed {
                run: 1,
                tool: "bash".into(),
                detail: Some("SECRET-TOOL".into()),
                count: 1,
            },
        ),
    ];

    let mut beta = create_test_task_full("Beta", project_id(), TaskStatus::HumanReview, 1);
    beta.id = Uuid::from_u128(11);
    beta.sequence_number = 2;
    beta.created_at = at();
    beta.updated_at = at();
    beta.dependencies = vec![alpha.id];
    beta.base_commit = Some("1".repeat(40));
    beta.external_refs = vec![ExternalRef::GithubPr {
        url: "https://github.com/acme/app/pull/7".into(),
        number: 7,
        repo: "acme/app".into(),
        state: Some("OPEN".into()),
        merge_commit: None,
    }];
    beta.human_review = HumanReviewRecord {
        arrivals: 1,
        entries: vec![HumanReviewEntry {
            sequence: 1,
            arrival: 1,
            decision: HumanReviewDecision::Approved,
            feedback: None,
            decided_at: at(),
            approved_tip: Some("2".repeat(40)),
            tip_reconfirmed: false,
        }],
        pr_error: Some("SECRET-PR-ERROR".into()),
    };
    beta.qa_signoff = Some(QaSignoff {
        status: QaStatus::FixesApplied,
        issues_found: vec!["lint failed in /SECRET-HOME/wt/beta.rs".into()],
        timestamp: at(),
        session_id: Uuid::from_u128(70),
    });
    beta.activity = vec![
        entry(
            1,
            ActivityKind::RunStarted {
                run: 1,
                addressing_feedback: false,
            },
        ),
        entry(2, ActivityKind::ReadyForReview { arrival: 1 }),
        entry(
            3,
            ActivityKind::RecoveryRequired {
                run: Some(1),
                reason: "checkout /home/alice/wt is missing".into(),
            },
        ),
    ];
    vec![alpha, beta]
}

fn fixture_payload() -> Payload {
    let repo = repository(Path::new("/SECRET-HOME/repo"));
    export::build_payload(
        &project(Some(repository_id())),
        Some(&repo),
        &fixture_tasks().iter().collect::<Vec<_>>(),
    )
    .expect("the fixture is exportable")
}

pub(super) async fn seed(ctx: &IpcContext, repo: Option<&Repository>, tasks: &[Task]) {
    ctx.projects
        .write()
        .await
        .insert(project_id(), project(repo.map(|r| r.id)));
    if let Some(repo) = repo {
        ctx.repositories.write().await.insert(repo.id, repo.clone());
    }
    let mut map = ctx.tasks.write().await;
    for task in tasks {
        map.insert(task.id, task.clone());
    }
}

pub(super) async fn export_to(ctx: &IpcContext, path: &Path) -> IpcResponse {
    dispatch(
        IpcRequest::ExportProject {
            project_id: project_id().to_string(),
            path: path.to_string_lossy().into_owned(),
        },
        ctx,
        &peer(),
    )
    .await
    .response
}

async fn preview_of(ctx: &IpcContext, path: &Path) -> IpcResponse {
    dispatch(
        IpcRequest::PreviewImport {
            path: path.to_string_lossy().into_owned(),
        },
        ctx,
        &peer(),
    )
    .await
    .response
}

/// The bundle `seal` builds around a payload given as JSON.
fn seal(payload_json: &str) -> Vec<u8> {
    // A payload this build cannot parse keeps the fixture manifest: the parse
    // is what must fail, and it does so before the manifest is compared.
    let manifest = match serde_json::from_str::<Payload>(payload_json) {
        Ok(payload) => payload.manifest(),
        Err(_) => fixture_payload().manifest(),
    };
    let manifest = serde_json::to_string(&manifest).unwrap();
    format!(
        r#"{{"format":"slashit.project-export","format_version":1,"build":"test","created_at":"2026-11-01T10:00:00Z","manifest":{manifest},"checksum":"{}","payload":{payload_json}}}"#,
        bundle::sha256_hex(payload_json.as_bytes())
    )
    .into_bytes()
}

fn payload_json(mutate: impl FnOnce(&mut Value)) -> String {
    let mut value = serde_json::to_value(fixture_payload()).unwrap();
    mutate(&mut value);
    serde_json::to_string(&value).unwrap()
}

fn tree(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    let mut out = std::collections::BTreeMap::new();
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

fn collect_keys(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            for (key, child) in fields {
                out.push(key.clone());
                collect_keys(child, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|i| collect_keys(i, out)),
        _ => {}
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

// ---------------------------------------------------------------- determinism

#[test]
fn golden_export_is_byte_identical_and_deterministic() {
    let bytes = bundle::encode(&fixture_payload(), at(), "golden").unwrap();
    assert_eq!(
        bytes,
        bundle::encode(&fixture_payload(), at(), "golden").unwrap()
    );
    if std::env::var_os("PORTABILITY_WRITE_GOLDEN").is_some() {
        fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/portability/testdata/export-v1.golden.json"
            ),
            &bytes,
        )
        .unwrap();
    }
    assert_eq!(
        bytes, GOLDEN,
        "the committed golden bundle changed; review and regenerate it deliberately"
    );
}

#[test]
fn a_golden_bundle_decodes_back_to_the_same_bytes() {
    let decoded = bundle::decode(GOLDEN).expect("the golden bundle is valid");
    assert_eq!(decoded.build, "golden");
    assert_eq!(decoded.payload.tasks.len(), 2);
    let again = bundle::encode(&decoded.payload, decoded.created_at, &decoded.build).unwrap();
    assert_eq!(again, GOLDEN, "decode and encode agree on every byte");
}

#[test]
fn activity_key_insertion_order_never_changes_the_bytes() {
    // serde_json keeps insertion order for objects only with its preserve_order
    // feature, which a feature-unified build can enable. The same Activity
    // content with its keys inserted in reverse must still give the golden bytes.
    let mut reversed = fixture_payload();
    for task in &mut reversed.tasks {
        for value in &mut task.activity {
            *value = reverse_keys(value);
        }
    }
    assert_eq!(bundle::encode(&reversed, at(), "golden").unwrap(), GOLDEN);
}

fn reverse_keys(value: &Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut out = serde_json::Map::new();
            for (key, field) in fields.iter().rev() {
                out.insert(key.clone(), reverse_keys(field));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(reverse_keys).collect()),
        other => other.clone(),
    }
}

// --------------------------------------------------------- what must be absent

#[test]
fn credentials_runtime_state_and_local_paths_never_reach_the_bundle() {
    let bytes = text(&bundle::encode(&fixture_payload(), at(), "t").unwrap());
    for sentinel in [
        "SECRET-APIKEY",
        "SECRET-ENV",
        "SECRET-ARG",
        "SECRET-CMD",
        "SECRET-HOME",
        "SECRET-BRANCH",
        "SECRET-RESULT",
        "SECRET-TOOL",
        "SECRET-PR-ERROR",
        "SECRET-TOKEN",
        "ghp_",
        "/home/alice",
        "token=x",
    ] {
        assert!(
            !bytes.contains(sentinel),
            "{sentinel} leaked into the bundle"
        );
    }
    // The timeline keeps the event and loses the secret inside it.
    assert!(bytes.contains("run_failed"));
    assert!(
        bytes.contains("<path>"),
        "machine paths in free text are replaced"
    );
    assert!(
        bytes.contains("lint failed in <path>"),
        "QA issue text is scrubbed like the timeline"
    );
}

#[test]
fn live_runtime_and_execution_authority_fields_are_not_portable() {
    // Keys of the payload only: the manifest's omitted list names these fields
    // on purpose, so a substring search over the whole file would be wrong.
    let payload = serde_json::to_value(fixture_payload()).unwrap();
    let mut keys = Vec::new();
    collect_keys(&payload, &mut keys);
    for field in [
        "worktree_path",
        "branch_name",
        "approved_execution",
        "executed_approvals",
        "worker_handoff",
        "checkout_admission",
        "pending_republish",
        "pr_publication",
        "local_integration",
        "cleanup_in_flight",
        "new_attempt",
        "pr_review_plan",
        "pr_error",
        "state_location",
        "scope",
        "local_path",
    ] {
        assert!(
            !keys.iter().any(|k| k == field),
            "{field} must not be exported"
        );
    }
}

// ---------------------------------------------------------------- refusals

#[test]
fn a_task_that_may_still_run_blocks_the_whole_export() {
    for status in [
        TaskStatus::Queue,
        TaskStatus::InProgress,
        TaskStatus::AiReview,
    ] {
        let mut task = create_test_task_full("Busy", project_id(), status.clone(), 0);
        task.id = Uuid::from_u128(30);
        let err = export::build_payload(&project(None), None, &[&task]).unwrap_err();
        assert!(err.contains("Busy"), "{err}");
        assert!(err.contains(&format!("{status:?}")), "{err}");
    }
    let mut waiting = create_test_task_full("Waiting", project_id(), TaskStatus::HumanReview, 0);
    waiting.id = Uuid::from_u128(31);
    assert!(
        export::build_payload(&project(None), None, &[&waiting]).is_ok(),
        "a decision wait is not active"
    );
}

#[test]
fn every_blocked_task_is_named_in_one_refusal() {
    let mut a = create_test_task_full("First busy", project_id(), TaskStatus::Queue, 0);
    a.id = Uuid::from_u128(40);
    let mut b = create_test_task_full("Second busy", project_id(), TaskStatus::InProgress, 0);
    b.id = Uuid::from_u128(41);
    let err = export::build_payload(&project(None), None, &[&a, &b]).unwrap_err();
    assert!(
        err.contains("First busy") && err.contains("Second busy"),
        "{err}"
    );
}

type Setup = Box<dyn Fn(&mut Task)>;

#[test]
fn unsettled_lifecycle_records_refuse_the_export() {
    let cases: Vec<(&str, Setup)> = vec![
        (
            "cleanup",
            Box::new(|t: &mut Task| t.cleanup_in_flight = true),
        ),
        (
            "approved execution",
            Box::new(|t: &mut Task| {
                t.approved_execution = Some(ApprovedExecution {
                    approval_id: Uuid::from_u128(50),
                    digest: "d".into(),
                    for_run: 1,
                    request: None,
                    checkout: None,
                })
            }),
        ),
        (
            "unconfirmed pull request",
            Box::new(|t: &mut Task| {
                t.pr_create_unconfirmed = Some(UnconfirmedPrCreate {
                    branch: "b".into(),
                    commit: "c".repeat(40),
                    started_at: at(),
                })
            }),
        ),
        (
            "pending publication",
            Box::new(|t: &mut Task| {
                t.pr_publication = Some(PrPublication::pending(
                    Uuid::from_u128(51),
                    "c".repeat(40),
                    "branch".into(),
                    "https://github.com/acme/app/pull/1".into(),
                    None,
                    at(),
                ))
            }),
        ),
    ];
    for (label, apply) in cases {
        let mut task = create_test_task_full("Unsettled", project_id(), TaskStatus::Backlog, 0);
        task.id = Uuid::from_u128(60);
        apply(&mut task);
        assert!(
            export::build_payload(&project(None), None, &[&task]).is_err(),
            "{label} must refuse the export"
        );
    }
}

// ---------------------------------------------------------- rejected bundles

#[test]
fn a_bundle_round_trips_and_a_fixture_is_valid() {
    let bytes = bundle::encode(&fixture_payload(), at(), "t").unwrap();
    assert!(bundle::decode(&bytes).is_ok());
}

#[test]
fn unsupported_versions_are_named_not_parsed() {
    let bytes = text(GOLDEN).replacen("\"format_version\": 1", "\"format_version\": 2", 1);
    assert_eq!(
        bundle::decode(bytes.as_bytes()).unwrap_err(),
        BundleError::UnsupportedVersion(2)
    );
    assert_eq!(FORMAT_VERSION, 1);
}

#[test]
fn a_tampered_payload_fails_the_checksum() {
    let bytes = text(GOLDEN).replacen("\"Alpha\"", "\"Alphb\"", 1);
    assert_eq!(
        bundle::decode(bytes.as_bytes()).unwrap_err(),
        BundleError::ChecksumMismatch
    );
}

#[test]
fn a_manifest_that_disagrees_with_the_payload_is_refused() {
    let bytes = text(GOLDEN).replacen("\"tasks\": 2", "\"tasks\": 3", 1);
    assert!(matches!(
        bundle::decode(bytes.as_bytes()),
        Err(BundleError::Malformed(_))
    ));
}

#[test]
fn unknown_fields_are_refused_even_with_a_valid_checksum() {
    let bytes = seal(&payload_json(|v| {
        v["tasks"][0]["approved_execution"] = serde_json::json!({"approval_id": "x"});
    }));
    assert!(matches!(
        bundle::decode(&bytes),
        Err(BundleError::Malformed(_))
    ));
}

#[test]
fn malformed_and_foreign_files_are_refused() {
    assert!(matches!(
        bundle::decode(b"not json"),
        Err(BundleError::Malformed(_))
    ));
    assert_eq!(
        bundle::decode(br#"{"format":"other"}"#).unwrap_err(),
        BundleError::NotAnExport
    );
    let no_version = text(GOLDEN).replacen("\"format_version\": 1,", "", 1);
    assert!(matches!(
        bundle::decode(no_version.as_bytes()),
        Err(BundleError::Malformed(_))
    ));
    assert_eq!(
        bundle::decode(&vec![b' '; (MAX_BUNDLE_BYTES + 1) as usize]).unwrap_err(),
        BundleError::TooLarge
    );
}

#[test]
fn states_an_export_never_writes_are_unsafe() {
    let queued = seal(&payload_json(|v| {
        v["tasks"][0]["status"] = Value::String("queue".into())
    }));
    assert!(matches!(
        bundle::decode(&queued),
        Err(BundleError::Unsafe(_))
    ));

    let run_output = seal(&payload_json(|v| {
        v["tasks"][0]["activity"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"seq": 9, "at": "2026-11-01T10:00:00Z", "kind": {"type": "run_result", "run": 1, "result": {"outcome": "available", "text": "raw"}}}));
    }));
    assert!(matches!(
        bundle::decode(&run_output),
        Err(BundleError::Unsafe(_))
    ));
}

#[test]
fn broken_dependencies_are_refused() {
    let missing = seal(&payload_json(|v| {
        v["tasks"][1]["dependencies"] = serde_json::json!(["00000000-0000-0000-0000-00000000dead"])
    }));
    assert!(
        matches!(bundle::decode(&missing), Err(BundleError::Malformed(m)) if m.contains("not in the export"))
    );

    let itself = seal(&payload_json(|v| {
        v["tasks"][0]["dependencies"] = serde_json::json!([v["tasks"][0]["id"].clone()])
    }));
    assert!(
        matches!(bundle::decode(&itself), Err(BundleError::Malformed(m)) if m.contains("itself"))
    );

    let cycle = seal(&payload_json(|v| {
        let a = v["tasks"][0]["id"].clone();
        let b = v["tasks"][1]["id"].clone();
        v["tasks"][0]["dependencies"] = serde_json::json!([b]);
        v["tasks"][1]["dependencies"] = serde_json::json!([a]);
    }));
    assert!(
        matches!(bundle::decode(&cycle), Err(BundleError::Malformed(m)) if m.contains("cycle"))
    );
}

#[test]
fn a_duplicate_task_identity_is_refused() {
    let dup = seal(&payload_json(|v| {
        let first = v["tasks"][0].clone();
        v["tasks"][1] = first;
    }));
    assert!(matches!(bundle::decode(&dup), Err(BundleError::Malformed(m)) if m.contains("twice")));
}

// --------------------------------------------------------- preview semantics

fn empty_target() -> (
    HashMap<Uuid, Project>,
    HashMap<Uuid, Repository>,
    HashMap<Uuid, Task>,
) {
    (HashMap::new(), HashMap::new(), HashMap::new())
}

#[test]
fn preview_describes_the_import_and_neutralizes_nothing_on_its_own() {
    let (projects, repositories, tasks) = empty_target();
    let target = preview::Target {
        projects: &projects,
        repositories: &repositories,
        tasks: &tasks,
        unreadable_boards: 0,
    };
    let report = preview::preview(GOLDEN, &target).expect("the golden bundle previews");
    assert!(report.importable);
    assert!(report.conflicts.is_empty());
    assert_eq!(report.counts.tasks, 2);
    assert_eq!(report.counts.dependencies, 1);
    assert_eq!(report.project.name, "Fixture");
    assert!(report.repository.as_ref().unwrap().path_remap_required);
    assert!(report.excluded.contains(&"agent_config.api_key"));
    assert!(report
        .normalization
        .iter()
        .any(|n| n.reason == "approval_current_in_bundle"));
    // Apply returns a Human Review Task to Backlog, so Preview must say so too.
    assert!(report
        .normalization
        .iter()
        .any(|n| n.reason == "human_review_without_checkout"));
    assert!(report
        .git_references
        .iter()
        .any(|g| g.field == "base_commit"));
    assert!(report
        .git_references
        .iter()
        .any(|g| g.field == "approved_tip"));
}

#[test]
fn identity_collisions_are_reported_and_block_the_import() {
    let mut projects = HashMap::new();
    projects.insert(project_id(), project(None));
    let mut repositories = HashMap::new();
    let mut other = repository(Path::new("/elsewhere"));
    other.id = Uuid::from_u128(99);
    other.remote_url = Some("https://github.com/acme/app.git".into());
    repositories.insert(other.id, other);
    let mut tasks = HashMap::new();
    let task = create_test_task_full("occupied", project_id(), TaskStatus::Backlog, 0);
    let mut clash = task.clone();
    clash.id = Uuid::from_u128(10);
    tasks.insert(clash.id, clash);
    let target = preview::Target {
        projects: &projects,
        repositories: &repositories,
        tasks: &tasks,
        unreadable_boards: 0,
    };

    let report = preview::preview(GOLDEN, &target).unwrap();
    assert!(!report.importable);
    let kinds: Vec<&str> = report.conflicts.iter().map(|c| c.kind).collect();
    assert!(kinds.contains(&"project_id"), "{kinds:?}");
    assert!(kinds.contains(&"repository_remote"), "{kinds:?}");
    assert!(kinds.contains(&"task_id"), "{kinds:?}");
}

#[test]
fn unreadable_target_boards_block_a_conflict_free_verdict() {
    let (projects, repositories, tasks) = empty_target();
    let target = preview::Target {
        projects: &projects,
        repositories: &repositories,
        tasks: &tasks,
        unreadable_boards: 1,
    };
    let report = preview::preview(GOLDEN, &target).unwrap();
    assert!(!report.importable);
    assert!(report
        .conflicts
        .iter()
        .any(|c| c.kind == "unreadable_target_board"));
}

// ------------------------------------------------------------- IPC behavior

#[tokio::test]
async fn export_writes_a_new_owner_only_file_and_never_replaces_one() {
    let (tmp, ctx) = test_ctx().await;
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    let repo = repository(&repo_dir);
    seed(&ctx, Some(&repo), &fixture_tasks()).await;

    let destination = tmp.path().join("out").join("export.json");
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    let first = export_to(&ctx, &destination).await;
    assert!(first.ok, "{first:?}");
    assert_eq!(first.data["tasks"], 2);
    let written = fs::read(&destination).unwrap();
    assert!(bundle::decode(&written).is_ok());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let second = export_to(&ctx, &destination).await;
    assert!(!second.ok, "an existing path must not be replaced");
    assert_eq!(
        fs::read(&destination).unwrap(),
        written,
        "the existing file is unchanged"
    );
}

#[test]
fn a_file_created_after_the_export_check_is_never_replaced() {
    let tmp = TempDir::new().unwrap();
    let destination = tmp.path().join("export.json");
    let result = super::export::write_bundle_with(&destination, b"bundle", || {
        // The race window: the existence check has passed, the publication has not run.
        fs::write(&destination, b"someone else's file").unwrap();
    });
    assert!(result.is_err(), "the export must refuse, not publish: {result:?}");
    assert_eq!(
        fs::read(&destination).unwrap(),
        b"someone else's file",
        "the competing file is unchanged"
    );
    let entries: Vec<_> = fs::read_dir(tmp.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(entries.len(), 1, "no staging file is left behind: {entries:?}");
}

#[tokio::test]
async fn export_refuses_relative_paths_and_missing_directories() {
    let (tmp, ctx) = test_ctx().await;
    seed(&ctx, None, &fixture_tasks()).await;
    let relative = export_to(&ctx, Path::new("relative.json")).await;
    assert!(!relative.ok && relative.error.unwrap().contains("absolute"));
    let missing = export_to(&ctx, &tmp.path().join("no-such-dir").join("x.json")).await;
    assert!(!missing.ok);
    assert!(
        !tmp.path().join("no-such-dir").exists(),
        "no directory is created"
    );
}

#[tokio::test]
async fn a_failed_write_leaves_no_file_and_no_temporary_behind() {
    let (tmp, ctx) = test_ctx().await;
    seed(&ctx, None, &fixture_tasks()).await;
    let directory = tmp.path().join("out");
    fs::create_dir_all(&directory).unwrap();
    let destination = directory.join("fault.json");
    let _fault = fault::fail_at_file(Stage::Replace, ErrorKind::Other, "fault.json");
    let answer = export_to(&ctx, &destination).await;
    assert!(!answer.ok, "the injected failure is reported");
    assert!(!destination.exists(), "no partial export is published");
    assert_eq!(
        fs::read_dir(&directory).unwrap().count(),
        0,
        "the temporary file is removed"
    );
}

#[tokio::test]
async fn export_waits_for_the_project_lifecycle_gate() {
    let (tmp, ctx) = test_ctx().await;
    seed(&ctx, None, &fixture_tasks()).await;
    let destination = tmp.path().join("gated.json");
    let gate = ctx.project_gate.write(project_id()).await;
    let blocked =
        tokio::time::timeout(Duration::from_millis(200), export_to(&ctx, &destination)).await;
    assert!(
        blocked.is_err(),
        "export must not snapshot while a deletion holds the gate"
    );
    assert!(!destination.exists());
    drop(gate);
    assert!(export_to(&ctx, &destination).await.ok);
}

#[tokio::test]
async fn export_waits_for_a_task_lifecycle_lease() {
    let (tmp, ctx) = test_ctx().await;
    let tasks = fixture_tasks();
    seed(&ctx, None, &tasks).await;
    let destination = tmp.path().join("leased.json");
    let lease = ctx.task_lifecycle_locks.acquire(tasks[0].id).await.unwrap();
    let blocked =
        tokio::time::timeout(Duration::from_millis(200), export_to(&ctx, &destination)).await;
    assert!(
        blocked.is_err(),
        "export must wait for a running transition"
    );
    drop(lease);
    assert!(export_to(&ctx, &destination).await.ok);
}

#[tokio::test]
async fn export_does_not_touch_the_repository_directory_or_live_tasks() {
    let (tmp, ctx) = test_ctx().await;
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(repo_dir.join(".git")).unwrap();
    fs::write(repo_dir.join("file.txt"), "untouched").unwrap();
    let repo = repository(&repo_dir);
    let tasks = fixture_tasks();
    seed(&ctx, Some(&repo), &tasks).await;
    let repo_before = tree(&repo_dir);
    let before: Vec<(Uuid, TaskStatus)> = tasks.iter().map(|t| (t.id, t.status.clone())).collect();

    let destination = tmp.path().join("export.json");
    assert!(export_to(&ctx, &destination).await.ok);

    assert_eq!(tree(&repo_dir), repo_before);
    let after: Vec<(Uuid, TaskStatus)> = ctx
        .tasks
        .read()
        .await
        .values()
        .map(|t| (t.id, t.status.clone()))
        .collect::<Vec<_>>();
    for (id, status) in before {
        assert!(after.contains(&(id, status)), "task {id} changed status");
    }
}

#[tokio::test]
async fn preview_writes_nothing_and_salvages_nothing() {
    let (tmp, ctx) = test_ctx().await;
    let bundle_path = tmp.path().join("in.json");
    fs::write(&bundle_path, GOLDEN).unwrap();

    // A damaged config, which a load would move aside. Preview must leave it alone.
    let config_file = tmp.path().join("config").join("config.toml");
    fs::write(&config_file, "this is = [ not valid toml").unwrap();
    let store_before = tree(&tmp.path().join("config"));
    let data_before = tree(&tmp.path().join("data"));
    let bundle_before = fs::read(&bundle_path).unwrap();

    let answer = preview_of(&ctx, &bundle_path).await;
    assert!(answer.ok, "{answer:?}");
    assert_eq!(answer.data["importable"], true);

    assert_eq!(
        tree(&tmp.path().join("config")),
        store_before,
        "the config directory is byte-identical"
    );
    assert_eq!(
        tree(&tmp.path().join("data")),
        data_before,
        "the data directory is byte-identical"
    );
    assert_eq!(fs::read(&bundle_path).unwrap(), bundle_before);
    assert!(
        ctx.projects.read().await.is_empty(),
        "no Project was created"
    );
    assert!(ctx.tasks.read().await.is_empty(), "no Task was created");
}

#[tokio::test]
async fn preview_rejects_a_damaged_bundle_without_side_effects() {
    let (tmp, ctx) = test_ctx().await;
    let bundle_path = tmp.path().join("bad.json");
    fs::write(
        &bundle_path,
        text(GOLDEN).replacen("\"Alpha\"", "\"Alphb\"", 1),
    )
    .unwrap();
    let answer = preview_of(&ctx, &bundle_path).await;
    assert!(!answer.ok);
    assert!(answer.error.unwrap().contains("checksum"));
}

#[tokio::test]
async fn preview_and_export_trigger_no_execution() {
    let (tmp, ctx) = test_ctx().await;
    seed(&ctx, None, &fixture_tasks()).await;
    let statuses = |ctx: &IpcContext| {
        let tasks = ctx.tasks.clone();
        async move {
            let mut rows: Vec<(Uuid, TaskStatus)> = tasks
                .read()
                .await
                .values()
                .map(|t| (t.id, t.status.clone()))
                .collect();
            rows.sort_by_key(|r| r.0);
            rows
        }
    };
    let before = statuses(&ctx).await;
    let bundle_path = tmp.path().join("in.json");
    fs::write(&bundle_path, GOLDEN).unwrap();
    assert!(preview_of(&ctx, &bundle_path).await.ok);
    assert!(export_to(&ctx, &tmp.path().join("out.json")).await.ok);
    assert_eq!(statuses(&ctx).await, before, "no Task changed state");
    assert!(ctx.executor.get().is_none(), "no executor was created");
}

#[tokio::test]
async fn exported_bundle_previews_as_importable_into_an_empty_target() {
    let (tmp, ctx) = test_ctx().await;
    seed(&ctx, None, &fixture_tasks()).await;
    let path = tmp.path().join("roundtrip.json");
    assert!(export_to(&ctx, &path).await.ok);

    let (other, other_ctx) = test_ctx().await;
    let answer = preview_of(&other_ctx, &path).await;
    assert!(answer.ok, "{answer:?}");
    assert_eq!(answer.data["importable"], true);
    drop(other);
}

#[test]
fn a_payload_built_from_the_fixture_matches_the_golden_manifest() {
    let decoded = bundle::decode(GOLDEN).unwrap();
    let manifest = decoded.payload.manifest();
    assert_eq!(manifest.tasks, 2);
    assert_eq!(manifest.dependencies, 1);
    assert_eq!(manifest.activity_entries, 5);
    assert_eq!(manifest.review_entries, 1);
    assert_eq!(manifest.repositories, 1);
}

#[test]
fn a_project_whose_repository_is_unregistered_is_refused_not_exported_bare() {
    let err = export::build_payload(&project(Some(repository_id())), None, &[]).unwrap_err();
    assert!(err.contains("not registered"), "{err}");
    assert!(
        export::build_payload(&project(None), None, &[]).is_ok(),
        "a Project with no repository exports"
    );
}

#[tokio::test]
async fn a_bundle_in_place_but_unconfirmed_is_reported_as_written_not_failed() {
    let (tmp, ctx) = test_ctx().await;
    seed(&ctx, None, &fixture_tasks()).await;
    let destination = tmp.path().join("durable.json");
    let _fault = fault::fail_at_file(Stage::SyncDirectory, ErrorKind::Other, "durable.json");

    let answer = export_to(&ctx, &destination).await;
    assert!(
        answer.ok,
        "the file is in place, so this is not a failure: {answer:?}"
    );
    assert_eq!(
        answer.data["durable"], false,
        "the unconfirmed directory entry is reported"
    );
    assert!(
        bundle::decode(&fs::read(&destination).unwrap()).is_ok(),
        "the complete bundle is readable"
    );
}

/// B1: an export must never write a bundle that its own preview refuses.
#[tokio::test]
async fn an_export_refuses_a_dependency_missing_from_the_export() {
    let (tmp, ctx) = test_ctx().await;
    let mut tasks = fixture_tasks();
    tasks[1].dependencies = vec![Uuid::from_u128(0xdead)];
    seed(&ctx, None, &tasks).await;
    let path = tmp.path().join("dangling.json");

    let answer = export_to(&ctx, &path).await;
    assert!(!answer.ok, "a dangling dependency must be refused: {answer:?}");
    assert!(!path.exists(), "nothing is written for a refused export");
}

/// B2, exporter side: the export applies the same shape rules as decode.
#[tokio::test]
async fn an_export_refuses_a_remote_that_decode_would_refuse() {
    let (tmp, ctx) = test_ctx().await;
    let mut repo = repository(Path::new("/tmp/unused-checkout"));
    repo.remote_url = Some("https://github.com/acme/ap p.git".into());
    seed(&ctx, Some(&repo), &fixture_tasks()).await;
    let path = tmp.path().join("spaced.json");

    let answer = export_to(&ctx, &path).await;
    assert!(!answer.ok, "a remote with a space must be refused: {answer:?}");
    assert!(!path.exists());
}

/// A hand-written bundle may put user information in a Task's web address. A
/// credential is refused, a malformed authority is refused, and a plain address
/// still imports. Export strips user information before it writes an address, so
/// it never writes the credential form.
#[test]
fn an_import_refuses_a_task_web_address_that_carries_user_information() {
    let credential = seal(&payload_json(|v| {
        v["tasks"][0]["pr_url"] = Value::from("https://user:PR_SECRET@github.com/acme/app/pull/1");
    }));
    assert!(
        matches!(bundle::decode(&credential), Err(BundleError::Unsafe(_))),
        "a credential in a web address is refused"
    );
    let malformed = seal(&payload_json(|v| {
        v["tasks"][0]["github_issue_url"] =
            Value::from("https://user:ISSUE_SECRET/acme/app/issues/1");
    }));
    assert!(
        matches!(bundle::decode(&malformed), Err(BundleError::Malformed(_))),
        "an authority that is not a host and port is refused"
    );
    let plain = seal(&payload_json(|v| {
        v["tasks"][0]["pr_url"] = Value::from("https://github.com:443/acme/app/pull/1");
    }));
    assert!(bundle::decode(&plain).is_ok(), "a plain address with a port still imports");
    // Spellings the `://` sanitizer does not reach must be refused too.
    for address in [
        "https:///user:pw@github.com/acme/app/pull/1",
        "https:user:tok@github.com/acme/app/pull/1",
        "https://a/https://user:tok@github.com/acme/app/pull/1",
        "https://github.com\\@evil.example/acme/app/pull/1",
    ] {
        let sealed = seal(&payload_json(|v| {
            v["tasks"][0]["pr_url"] = Value::from(address);
        }));
        assert!(
            matches!(bundle::decode(&sealed), Err(BundleError::Unsafe(_))),
            "{address} must be refused"
        );
    }
}

/// Export writes a Task's address only after the same check. The `://` form
/// with user information is stripped and exported; a spelling the stripping
/// cannot reach is refused, and the credential is in neither the answer nor the file.
#[tokio::test]
async fn an_export_never_writes_a_credential_in_a_task_web_address() {
    let (tmp, ctx) = test_ctx().await;
    let mut tasks = fixture_tasks();
    tasks[0].pr_url = Some("https://user:PR_TOKEN@github.com/acme/app/pull/1".into());
    seed(&ctx, None, &tasks).await;
    let stripped = tmp.path().join("stripped.json");
    let answer = export_to(&ctx, &stripped).await;
    assert!(answer.ok, "{answer:?}");
    let written = fs::read_to_string(&stripped).unwrap();
    assert!(!written.contains("PR_TOKEN"), "the user information must not be exported");
    assert!(written.contains("https://github.com/acme/app/pull/1"));

    tasks[0].pr_url = Some("https:///user:PR_TOKEN@github.com/acme/app/pull/1".into());
    seed(&ctx, None, &tasks).await;
    let refused = tmp.path().join("refused.json");
    let answer = export_to(&ctx, &refused).await;
    assert!(!answer.ok, "a spelling the sanitizer cannot strip is refused: {answer:?}");
    assert!(!format!("{answer:?}").contains("PR_TOKEN"));
    assert!(!refused.exists());
}

/// An scp-style remote can carry a credential in its user part, which is not
/// stripped the way a `://` URL's is. Export refuses it, and an ordinary
/// `git@host:path` remote, which has no credential, still exports.
#[tokio::test]
async fn an_export_refuses_an_scp_remote_that_carries_a_credential() {
    let (tmp, ctx) = test_ctx().await;
    let mut repo = repository(Path::new("/tmp/unused-checkout"));
    repo.remote_url = Some("deploy:SCP_SECRET@github.com:acme/app.git".into());
    seed(&ctx, Some(&repo), &fixture_tasks()).await;
    let path = tmp.path().join("scp.json");
    let answer = export_to(&ctx, &path).await;
    assert!(
        !answer.ok,
        "a credential-bearing remote must be refused: {answer:?}"
    );
    assert!(!format!("{answer:?}").contains("SCP_SECRET"));
    assert!(!path.exists());

    repo.remote_url = Some("git@github.com:acme/app.git".into());
    seed(&ctx, Some(&repo), &fixture_tasks()).await;
    let ssh = tmp.path().join("ssh.json");
    let answer = export_to(&ctx, &ssh).await;
    assert!(
        answer.ok,
        "an ordinary SSH remote must still export: {answer:?}"
    );

    // Export drops a query from any remote before the check, so the secret never reaches the file.
    repo.remote_url = Some("git@github.com:acme/app.git?access_token=QUERY_SECRET".into());
    seed(&ctx, Some(&repo), &fixture_tasks()).await;
    let query = tmp.path().join("query.json");
    let answer = export_to(&ctx, &query).await;
    assert!(answer.ok, "{answer:?}");
    let written = fs::read_to_string(&query).unwrap();
    assert!(!written.contains("QUERY_SECRET"), "the query must not be exported");

    let sealed = seal(&payload_json(|v| {
        v["repository"]["remote_url"] = Value::from("deploy:SCP_SECRET@github.com:acme/app.git");
    }));
    assert!(
        matches!(bundle::decode(&sealed), Err(BundleError::Unsafe(_))),
        "decode refuses the same remote"
    );
    // A hand-written bundle may still carry a query: decode refuses it.
    let queried = seal(&payload_json(|v| {
        v["repository"]["remote_url"] = Value::from("git@github.com:acme/app.git?token=x");
    }));
    assert!(
        matches!(bundle::decode(&queried), Err(BundleError::Malformed(_))),
        "decode refuses a query in an scp remote"
    );
}

/// B2, decode side: identity and Git evidence are checked by shape, and an
/// absent but well-formed commit is still accepted.
#[test]
fn repository_identity_and_git_fields_are_checked_on_decode() {
    let credentialed = seal(&payload_json(|v| {
        v["repository"]["remote_url"] =
            Value::from("https://user:ghp_SECRETTOKEN@github.com/acme/app.git");
    }));
    assert!(
        matches!(bundle::decode(&credentialed), Err(BundleError::Unsafe(_))),
        "credentials in a remote are refused"
    );

    let option_shaped = seal(&payload_json(|v| {
        v["tasks"][1]["base_commit"] = Value::from("--upload-pack=touch /tmp/x");
    }));
    assert!(
        matches!(bundle::decode(&option_shaped), Err(BundleError::Malformed(m)) if m.contains("base_commit")),
        "a base commit must be a commit identifier"
    );

    let option_branch = seal(&payload_json(|v| {
        v["project"]["base"] = serde_json::json!({"kind": "local_branch", "branch": "-delete"});
    }));
    assert!(
        matches!(bundle::decode(&option_branch), Err(BundleError::Malformed(m)) if m.contains("branch")),
        "a project base must be a branch name"
    );

    let absent_commit = seal(&payload_json(|v| {
        v["tasks"][1]["base_commit"] = Value::from("3".repeat(40));
    }));
    assert!(
        bundle::decode(&absent_commit).is_ok(),
        "historical commits are not required to exist in this repository"
    );
}

/// B3: Preview must answer for a named pipe rather than wait for a writer.
#[cfg(unix)]
#[test]
fn a_named_pipe_is_refused_without_blocking_the_preview() {
    let tmp = TempDir::new().unwrap();
    let pipe = tmp.path().join("pipe.json");
    assert!(std::process::Command::new("mkfifo")
        .arg(&pipe)
        .status()
        .unwrap()
        .success());
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let answer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (_dir, ctx) = test_ctx().await;
                preview_of(&ctx, &pipe).await
            });
        let _ = sender.send(answer);
    });

    let answer = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the preview must answer for a named pipe, not wait for a writer");
    assert!(!answer.ok, "a named pipe is not a bundle: {answer:?}");
}

/// B3: an endless or growing source is refused after one byte past the bound,
/// so the read never holds more than that.
#[test]
fn a_bounded_read_stops_at_the_limit_whatever_the_source() {
    assert_eq!(
        bundle::read_bounded(std::io::repeat(b'x')).unwrap_err(),
        BundleError::TooLarge.to_string()
    );
    assert_eq!(bundle::read_bounded(&b"{}"[..]).unwrap(), b"{}".to_vec());
}
