//! The Task board refuses what its types do not declare, at every depth.
//!
//! The persistent Rust types are the schema: nothing lists accepted names a
//! second time. These tests prove the claim without such a list. They build a
//! board that populates every field of every persistent type, then put an
//! unsupported key into each table of it, one table at a time, and require the
//! board to be refused. A table that accepted the key would be a place where a
//! later save drops data silently.
//!
//! They also guard the fixture itself: the unsupported key's refusal names the
//! fields a table declares, and every one of them must be present somewhere in
//! the fixture. A field or type added without the fixture growing with it
//! fails here, not in production.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{TimeZone, Utc};
use slashit_activity::{Column, Entry, Kind, RunResult};
use uuid::Uuid;

use super::storage::ProjectTasksFile;
use crate::domain::conversation::ExistingCheckoutEvidence;
use crate::domain::task::{
    ApprovedExecution, BranchOrigin, CheckoutAdmission, DependencyAdmission,
    DependencyRequirement, ExecutedApproval, ExternalRef, FixEffect, FixedContent,
    HumanReviewDecision, NewAttemptDecision, PendingRepublish, PrCommentKind, PrReviewApplyResult,
    PrReviewComment, PrReviewDecision, PrReviewItem, PrReviewPlan, QaSignoff, QaStatus, Subtask,
    Task, WorkerHandoff, WorkerHandoffStage, WorkerResultAcceptance,
};
use crate::domain::{IntegrationState, IntegrationStrategy, LocalIntegration};

const UNSUPPORTED: &str = "unsupported_future_key";

fn at(second: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 20, 10, 0, second).unwrap()
}

fn commit(c: char) -> String {
    c.to_string().repeat(40)
}

/// Every `Kind`, so every Activity payload is a table in the fixture. The
/// match has no wildcard: a new kind does not compile until it is listed.
fn every_activity_kind() -> Vec<Kind> {
    let kinds = vec![
        Kind::Moved { from: Column::Backlog, to: Column::Queue },
        Kind::RunStarted { run: 1, addressing_feedback: true },
        Kind::ToolUsed { run: 1, tool: "Read".into(), detail: Some("a.rs".into()), count: 2 },
        Kind::ToolsOmitted { run: 1, count: 3 },
        Kind::RunCompleted { run: 1 },
        Kind::RunResult { run: 1, result: RunResult::Available { text: "done".into() } },
        Kind::RunResult {
            run: 2,
            result: RunResult::Truncated { text: "cut".into(), original_chars: 9_000 },
        },
        Kind::RunResult { run: 3, result: RunResult::Unavailable { reason: "none".into() } },
        Kind::RecoveryRequired { run: Some(1), reason: "crashed".into() },
        Kind::NewAttemptAuthorized { run: 1 },
        Kind::RunFailed { run: Some(1), reason: "failed".into() },
        Kind::Stopped { from: Column::InProgress },
        Kind::Interrupted { from: Column::InProgress },
        Kind::AiReviewStarted { review: 1 },
        Kind::AiReviewApproved { review: 1 },
        Kind::AiReviewChangesRequested { review: 2, issues: 3 },
        Kind::AiReviewFailed { review: 3, reason: "failed".into() },
        Kind::AiReviewSkipped { review: 4, reason: "skipped".into() },
        Kind::AiFixStarted { review: 2 },
        Kind::AiFixApplied { review: 2 },
        Kind::AiFixUnchanged { review: 3 },
        Kind::AiFixFailed { review: 4, reason: "failed".into() },
        Kind::ReadyForReview { arrival: 1 },
        Kind::DeliveryFailed { reason: "refused".into() },
        Kind::PrLinked { url: "https://github.com/o/r/pull/1".into(), number: Some(1) },
        Kind::PrMerged { number: 1 },
        Kind::PrClosed { number: 2 },
        Kind::CompletedLocally {},
        Kind::IntegratedLocally {
            target: "main".into(),
            strategy: "fast_forward".into(),
            commit: commit('d'),
        },
    ];
    for kind in &kinds {
        match kind {
            Kind::Moved { .. }
            | Kind::RunStarted { .. }
            | Kind::ToolUsed { .. }
            | Kind::ToolsOmitted { .. }
            | Kind::RunCompleted { .. }
            | Kind::RunResult { .. }
            | Kind::RecoveryRequired { .. }
            | Kind::NewAttemptAuthorized { .. }
            | Kind::RunFailed { .. }
            | Kind::Stopped { .. }
            | Kind::Interrupted { .. }
            | Kind::AiReviewStarted { .. }
            | Kind::AiReviewApproved { .. }
            | Kind::AiReviewChangesRequested { .. }
            | Kind::AiReviewFailed { .. }
            | Kind::AiReviewSkipped { .. }
            | Kind::AiFixStarted { .. }
            | Kind::AiFixApplied { .. }
            | Kind::AiFixUnchanged { .. }
            | Kind::AiFixFailed { .. }
            | Kind::ReadyForReview { .. }
            | Kind::DeliveryFailed { .. }
            | Kind::PrLinked { .. }
            | Kind::PrMerged { .. }
            | Kind::PrClosed { .. }
            | Kind::CompletedLocally {}
            | Kind::IntegratedLocally { .. } => {}
        }
    }
    kinds
}

fn checkout_evidence(task: &Task) -> ExistingCheckoutEvidence {
    ExistingCheckoutEvidence {
        project_id: task.project_id,
        task_id: task.id,
        repository_path: "/repo".into(),
        checkout_path: "/repo/.checkouts/t".into(),
        git_entry_identity: Some("1:2".into()),
        branch: "task/full".into(),
        branch_ref: "refs/heads/task/full".into(),
        head: commit('e'),
        base_commit: commit('f'),
        run: 2,
    }
}

/// One Task with every optional field set, every list non-empty, every flag
/// that is omitted when false set to true, and the first variant of each
/// tagged payload.
fn full_task(project_id: Uuid) -> Task {
    let mut task = crate::test_helpers::create_test_task("Full");
    task.project_id = project_id;
    task.description = Some("described".into());
    task.last_observed_model = Some("observed".into());
    task.dependencies = vec![Uuid::new_v4()];
    task.worktree_id = Some(Uuid::new_v4());
    task.jj_change_id = Some("change".into());
    task.subtasks = vec![Subtask { id: Uuid::new_v4(), title: "sub".into(), completed: true }];
    task.github_issue_url = Some("https://github.com/o/r/issues/1".into());
    task.gitlab_issue_url = Some("https://gitlab.com/o/r/-/issues/1".into());
    task.linear_ticket_id = Some("LIN-1".into());
    task.jira_issue_key = Some("PLAT-1".into());
    task.pr_url = Some("https://github.com/o/r/pull/1".into());
    task.external_refs = vec![
        ExternalRef::GithubIssue {
            url: "https://github.com/o/r/issues/1".into(),
            number: 1,
            repo: "o/r".into(),
            state: Some("open".into()),
        },
        ExternalRef::GithubPr {
            url: "https://github.com/o/r/pull/1".into(),
            number: 1,
            repo: "o/r".into(),
            state: Some("MERGED".into()),
            merge_commit: Some(commit('a')),
        },
        ExternalRef::GitlabIssue { url: "https://gitlab.com/o/r/-/issues/1".into() },
        ExternalRef::JiraTicket { key: "PLAT-1".into(), project: "PLAT".into() },
        ExternalRef::LinearTicket { id: "LIN-1".into() },
    ];
    task.qa_signoff = Some(QaSignoff {
        status: QaStatus::Approved,
        issues_found: vec!["issue".into()],
        timestamp: at(1),
        session_id: Uuid::new_v4(),
    });
    task.human_review.record_arrival();
    task.human_review.push(HumanReviewDecision::Approved, Some("fine".into()), at(2));
    {
        let entry = task.human_review.entries.last_mut().unwrap();
        entry.approved_tip = Some(commit('b'));
        entry.tip_reconfirmed = true;
    }
    task.human_review.pr_error = Some("pr error".into());
    task.stuck_since = Some(at(3));
    task.error_message = Some("error".into());
    task.worktree_path = Some("/repo/.checkouts/t".into());
    task.branch_name = Some("task/full".into());
    task.checkout_admission = Some(CheckoutAdmission {
        format_version: CheckoutAdmission::FORMAT_VERSION,
        policy_version: CheckoutAdmission::POLICY_VERSION,
        task_id: task.id,
        repository_id: Uuid::new_v4(),
        branch: "task/full".into(),
        admitted_commit: commit('c'),
        dependencies: vec![
            DependencyAdmission {
                task_id: Uuid::new_v4(),
                requirement: DependencyRequirement::Commits(vec![commit('1'), commit('2')]),
            },
            DependencyAdmission {
                task_id: Uuid::new_v4(),
                requirement: DependencyRequirement::Branchless,
            },
        ],
    });
    task.base_commit = Some(commit('d'));
    task.branch_origin = Some(BranchOrigin::DefaultBase { branch: Some("main".into()) });
    task.pending_republish = Some(PendingRepublish {
        parent_branch: "parent".into(),
        parent_pr: 7,
        pr_number: Some(8),
        default_branch: "main".into(),
        fork_point: commit('1'),
        previous_tip: commit('2'),
        onto: commit('3'),
        rewritten_tip: Some(commit('4')),
    });
    task.local_integration = Some(LocalIntegration {
        state: IntegrationState::Integrated,
        target_branch: "main".into(),
        target_old_oid: commit('5'),
        approved_tip: commit('6'),
        result_oid: commit('7'),
        strategy: IntegrationStrategy::MergeCommit,
        checkout_path: Some("/repo".into()),
        started_at: at(4),
        completed_at: Some(at(5)),
        completes_task: true,
    });
    task.cleanup_in_flight = true;
    task.pr_review_plan = Some(PrReviewPlan {
        generated_at: at(6),
        pr_url: "https://github.com/o/r/pull/1".into(),
        review_decision: Some("APPROVED".into()),
        comments: vec![
            PrReviewComment {
                id: Some(1),
                kind: PrCommentKind::Inline,
                author: "reviewer".into(),
                author_association: Some("MEMBER".into()),
                body: "body".into(),
                path: Some("a.rs".into()),
                line: Some(3),
                url: Some("https://github.com/o/r/pull/1#c".into()),
                created_at: Some(at(7)),
                updated_at: Some(at(8)),
            },
            PrReviewComment {
                id: None,
                kind: PrCommentKind::Review,
                author: "other".into(),
                author_association: None,
                body: "plain".into(),
                path: None,
                line: None,
                url: None,
                created_at: None,
                updated_at: None,
            },
        ],
        items: vec![PrReviewItem {
            comment_id: Some(1),
            summary: "summary".into(),
            decision: PrReviewDecision::Fix,
            reasoning: "why".into(),
            proposed_change: "what".into(),
            approved: true,
            user_note: "note".into(),
            fix_done: true,
            fix_uncommitted: true,
            fix_commit: Some(commit('8')),
            fix_effect: Some(FixEffect { before_tree: commit('9'), after_tree: commit('0') }),
            reply_posted: true,
            last_agent_summary: Some("agent".into()),
            last_error: Some("error".into()),
            pr_reply_text: Some("reply".into()),
            reply_comment_id: Some(2),
        }],
        raw_plan: "raw".into(),
        last_apply: Some(PrReviewApplyResult {
            applied_at: at(9),
            agent_summary: "applied".into(),
            fixed_ids: vec![1],
            skipped_ids: vec![2],
            pushed: true,
            push_branch: Some("task/full".into()),
            replies_posted: 1,
            reply_errors: vec!["comment 1: no".into()],
            dry_run: true,
            failed_ids: vec![3],
            fix_errors: vec!["fix".into()],
            push_error: Some("push".into()),
            auto_reply: Some(true),
        }),
        fixed_content: vec![FixedContent { comment_id: 1, fingerprint: "print".into() }],
    });
    task.activity = every_activity_kind()
        .into_iter()
        .zip(1..)
        .map(|(kind, seq)| Entry { seq, at: at(10), kind })
        .collect();
    task.worker_handoff = Some(WorkerHandoff {
        task_id: task.id,
        project_id,
        repository_id: Some(Uuid::new_v4()),
        run: Some(1),
        branch: Some("task/full".into()),
        start_head: Some(commit('a')),
        stage: WorkerHandoffStage::Started {},
        acceptance: Some(WorkerResultAcceptance {
            decision_id: Uuid::new_v4(),
            commit: commit('b'),
            observed_commit: Some(commit('c')),
            decided_at: at(11),
        }),
    });
    task.new_attempt = Some(NewAttemptDecision {
        decision_id: Uuid::new_v4(),
        for_run: 2,
        recovery_reason: "crashed".into(),
        branch: "task/full".into(),
        checkout_path: "/repo/.checkouts/t".into(),
        observed_commit: Some(commit('d')),
        expected_head: commit('e'),
        dirty_fingerprint: "print".into(),
        dirty_paths: vec!["a.rs".into()],
        authorized_at: at(12),
    });
    let evidence = checkout_evidence(&task);
    task.approved_execution = Some(ApprovedExecution {
        approval_id: Uuid::new_v4(),
        digest: "digest".into(),
        for_run: 2,
        request: Some("do it".into()),
        checkout: Some(evidence.clone()),
    });
    task.executed_approvals = vec![ExecutedApproval {
        approval_id: Uuid::new_v4(),
        run: 1,
        digest: "digest".into(),
        checkout: Some(evidence),
    }];
    task
}

/// The remaining variants of the tagged payloads, one Task each, so every
/// variant's own table is in the fixture.
fn variant_tasks(project_id: Uuid) -> Vec<Task> {
    let stage = |stage: WorkerHandoffStage| {
        let mut task = crate::test_helpers::create_test_task("Variant");
        task.project_id = project_id;
        task.worker_handoff = Some(WorkerHandoff {
            task_id: task.id,
            project_id,
            repository_id: None,
            run: None,
            branch: None,
            start_head: None,
            stage,
            acceptance: None,
        });
        task
    };
    let mut stacked = stage(WorkerHandoffStage::CommittedAwaitingReview { commit: commit('a') });
    stacked.branch_origin = Some(BranchOrigin::Stacked { parent_branch: "parent".into() });
    let mut local = stage(WorkerHandoffStage::RecoveryRequired {
        reason: "crashed".into(),
        observed_commit: Some(commit('b')),
    });
    local.branch_origin = Some(BranchOrigin::LocalBase { branch: "develop".into() });
    let resolved = stage(WorkerHandoffStage::ResolvedToAiReview { commit: commit('c') });
    // The empty requirement and a task with no optional field set at all.
    vec![stacked, local, resolved, crate::test_helpers::create_test_task("Bare")]
}

fn full_board(project_id: Uuid) -> ProjectTasksFile {
    let mut tasks = vec![full_task(project_id)];
    tasks.extend(variant_tasks(project_id));
    for task in &mut tasks {
        task.project_id = project_id;
    }
    ProjectTasksFile { version: 1, tasks }
}

#[derive(Clone, Debug)]
enum Step {
    Key(String),
    Index(usize),
}

fn describe(path: &[Step]) -> String {
    path.iter()
        .map(|step| match step {
            Step::Key(key) => format!(".{key}"),
            Step::Index(index) => format!("[{index}]"),
        })
        .collect()
}

/// The path of every table in `value`, the root included.
fn table_paths(value: &toml::Value, here: &mut Vec<Step>, out: &mut Vec<Vec<Step>>) {
    match value {
        toml::Value::Table(table) => {
            out.push(here.clone());
            for (key, child) in table {
                here.push(Step::Key(key.clone()));
                table_paths(child, here, out);
                here.pop();
            }
        }
        toml::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                here.push(Step::Index(index));
                table_paths(child, here, out);
                here.pop();
            }
        }
        _ => {}
    }
}

fn at_path_mut<'a>(mut value: &'a mut toml::Value, path: &[Step]) -> &'a mut toml::Value {
    for step in path {
        value = match step {
            Step::Key(key) => value.get_mut(key.as_str()).unwrap(),
            Step::Index(index) => value.get_mut(*index).unwrap(),
        };
    }
    value
}

/// The names a refusal says the table declares, from serde's message:
/// "unknown field `x`, expected one of `a`, `b`", "expected `a` or `b`".
fn declared_names(message: &str) -> BTreeSet<String> {
    let (_, declared) = message.split_once("expected").unwrap_or(("", ""));
    declared
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

fn board_text(value: &toml::Value) -> String {
    toml::to_string(value).unwrap()
}

#[test]
fn the_fixture_is_a_board_the_current_model_writes_and_reads() {
    let board = full_board(Uuid::new_v4());
    let value = toml::Value::try_from(&board).unwrap();
    let parsed: ProjectTasksFile = toml::from_str(&board_text(&value)).expect("fixture loads");
    assert_eq!(parsed.tasks.len(), board.tasks.len());
    assert_eq!(
        toml::Value::try_from(&parsed).unwrap(),
        value,
        "a save of what loaded writes the same board"
    );
}

#[test]
fn every_table_of_the_board_refuses_an_unsupported_key() {
    let value = toml::Value::try_from(full_board(Uuid::new_v4())).unwrap();
    let mut paths = Vec::new();
    table_paths(&value, &mut Vec::new(), &mut paths);
    assert!(paths.len() > 100, "the fixture reaches {} tables", paths.len());

    let mut accepted = Vec::new();
    // Declared names per shape, and the keys the fixture holds for that shape.
    let mut declared: BTreeMap<BTreeSet<String>, BTreeSet<String>> = BTreeMap::new();
    for path in &paths {
        for unsupported in [
            toml::Value::Boolean(false),
            toml::Value::Array(Vec::new()),
            toml::Value::Table(toml::Table::new()),
            toml::Value::String("newer".into()),
        ] {
            let mut edited = value.clone();
            at_path_mut(&mut edited, path)
                .as_table_mut()
                .unwrap()
                .insert(UNSUPPORTED.into(), unsupported);
            match toml::from_str::<ProjectTasksFile>(&board_text(&edited)) {
                Ok(_) => accepted.push(describe(path)),
                Err(error) => {
                    let message = error.message().to_string();
                    assert!(
                        message.contains(UNSUPPORTED),
                        "{} was refused for another reason: {message}",
                        describe(path)
                    );
                    let names = declared_names(&message);
                    if !names.is_empty() {
                        let present = at_path_mut(&mut value.clone(), path)
                            .as_table()
                            .unwrap()
                            .keys()
                            .cloned()
                            .collect::<BTreeSet<_>>();
                        declared.entry(names).or_default().extend(present);
                    }
                }
            }
        }
    }
    accepted.sort();
    accepted.dedup();
    assert!(accepted.is_empty(), "tables that accept an unsupported key: {accepted:?}");

    // Every field a table declares is populated somewhere in the fixture. A
    // legacy alias is declared but never written.
    for (names, present) in declared {
        let missing: Vec<_> = names
            .difference(&present)
            .filter(|name| name.as_str() != "workspace_id")
            .collect();
        assert!(
            missing.is_empty(),
            "the fixture never sets {missing:?} of {names:?}; extend it with the new field"
        );
    }
}

/// The tag of a tagged payload, given the key its table sits under. `state`
/// and `kind` are also ordinary fields elsewhere, so the owner decides.
fn tag_of(owner: Option<&str>) -> Option<&'static str> {
    match owner {
        None => Some("ref_type"),
        Some("stage") => Some("state"),
        Some("branch_origin" | "requirement") => Some("kind"),
        Some("kind") => Some("type"),
        Some("result") => Some("outcome"),
        Some(_) => None,
    }
}

#[test]
fn an_unknown_tag_of_every_tagged_payload_is_refused() {
    let value = toml::Value::try_from(full_board(Uuid::new_v4())).unwrap();
    let mut paths = Vec::new();
    table_paths(&value, &mut Vec::new(), &mut paths);
    let mut tagged = 0;
    for path in &paths {
        let owner = match path.last() {
            Some(Step::Key(key)) => Some(key.as_str()),
            // Only the elements of `external_refs` are tagged arrays.
            Some(Step::Index(_)) if matches!(path.get(path.len() - 2), Some(Step::Key(k)) if k == "external_refs") => None,
            _ => continue,
        };
        let Some(tag) = tag_of(owner) else { continue };
        let mut edited = value.clone();
        let table = at_path_mut(&mut edited, path).as_table_mut().unwrap();
        if !table.contains_key(tag) {
            continue;
        }
        tagged += 1;
        table.insert(tag.into(), "from_the_future".into());
        assert!(
            toml::from_str::<ProjectTasksFile>(&board_text(&edited)).is_err(),
            "{} accepted an unknown {tag}",
            describe(path)
        );
    }
    assert!(tagged >= 40, "only {tagged} tagged payloads were reached");
}

#[test]
fn an_unknown_field_inside_checkout_admission_evidence_is_refused_not_dropped() {
    let value = toml::Value::try_from(full_board(Uuid::new_v4())).unwrap();
    let mut edited = value.clone();
    let task = at_path_mut(&mut edited, &[Step::Key("tasks".into()), Step::Index(0)]);
    let admission = task.get_mut("checkout_admission").unwrap().as_table_mut().unwrap();
    admission.insert("verified_by".into(), "a newer build".into());
    let error = toml::from_str::<ProjectTasksFile>(&board_text(&edited)).unwrap_err();
    assert!(error.message().contains("verified_by"), "{error}");

    // And a malformed shape in the same place is an error, not an absent value.
    let mut edited = value;
    let task = at_path_mut(&mut edited, &[Step::Key("tasks".into()), Step::Index(0)]);
    task.as_table_mut().unwrap().insert("checkout_admission".into(), "garbage".into());
    assert!(toml::from_str::<ProjectTasksFile>(&board_text(&edited)).is_err());
}

#[test]
fn a_board_without_its_optional_fields_still_loads() {
    let mut bare = toml::Value::try_from(full_board(Uuid::new_v4())).unwrap();
    // The last Task sets no optional field, as an earlier record would not.
    let task = at_path_mut(&mut bare, &[Step::Key("tasks".into()), Step::Index(4)]);
    let keys: Vec<String> = task.as_table().unwrap().keys().cloned().collect();
    assert!(!keys.iter().any(|k| k == "checkout_admission" || k == "activity"));
    let parsed: ProjectTasksFile = toml::from_str(&board_text(&bare)).expect("bare record loads");
    assert!(parsed.tasks[4].checkout_admission.is_none());
    assert!(parsed.tasks[4].activity.is_empty());
}
