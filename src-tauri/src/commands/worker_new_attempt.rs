//! A person's decision to start a new coding attempt after an interrupted
//! Worker, building on the Task Checkout as it stands.
//!
//! A Worker whose outcome SlashIt could not attribute leaves its checkout and
//! branch untouched (see [`super::worker_recovery`]). A new attempt continues
//! from that checkout. This module has two requests, kept apart on purpose:
//!
//! - [`authorize_new_attempt`] records the person's decision on the Task. It
//!   runs no provider and starts no run. The decision is confirmed only by a
//!   durable write made in this process.
//! - [`start_new_attempt`] asks the executor to start the run. The executor
//!   admits it only for that confirmed decision, after reading the Task and the
//!   checkout again under the lifecycle lease.
//!
//! The attempt reuses the checkout without reset, clean, stash or forced
//! checkout. Uncommitted work in it is kept, and the attempt's own commit
//! includes it (`git add -A`). The coding provider runs with full tool access,
//! so it can change files and run any command. SlashIt's code paths never do.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::commands::gui_admission::GuiMutation;
use crate::domain::task::{ActivityKind, NewAttemptDecision, Task, TaskStatus, WorkerHandoffStage};

/// The decision a person made on what the recovery view showed. Every field is
/// a compare-and-set expectation: the decision applies only if it still holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthorizeNewAttemptRequest {
    /// Client-generated; identifies this decision across retries.
    pub decision_id: Uuid,
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub repository_id: Uuid,
    /// The recovered run the new attempt follows.
    pub run: u32,
    /// Must be `recovery_required`.
    pub stage: String,
    /// The recovery reason the person was shown.
    pub recovery_reason: String,
    #[serde(default)]
    pub observed_commit: Option<String>,
    pub branch: String,
    pub checkout: String,
    /// The branch tip the person inspected.
    pub expected_head: String,
    /// The fingerprint of the uncommitted work the person inspected.
    pub dirty_fingerprint: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthorizeNewAttemptOutcome {
    /// The decision is durable and confirmed in this process. A new attempt may
    /// now be started.
    Authorized { task: Task },
    /// An identical decision was already confirmed in this process.
    AlreadyAuthorized { task: Task },
    /// Nothing was recorded for this request.
    Refused { reason: String },
    /// The decision is on the board, but its durability could not be confirmed.
    /// It cannot start an attempt; the person must decide again.
    DurabilityUnconfirmed { task: Task, detail: String },
}

/// What a person would be deciding on, read now. It is informational:
/// authorizing reads everything again and refuses if anything differs.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NewAttemptEvidence {
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub repository_id: Option<Uuid>,
    pub run: Option<u32>,
    pub recovery_reason: Option<String>,
    pub observed_commit: Option<String>,
    pub branch: Option<String>,
    pub checkout: Option<String>,
    /// The branch tip, which is also the checkout's HEAD.
    pub expected_head: Option<String>,
    pub dirty_fingerprint: Option<String>,
    /// The uncommitted paths. The new attempt's commit will include them.
    pub dirty_paths: Vec<String>,
    pub eligible: bool,
    /// Why a new attempt cannot be authorized now; empty when `eligible`.
    pub refusals: Vec<String>,
}

/// Reads what [`record_decision`] would bind, without recording anything.
pub(crate) async fn new_attempt_evidence(
    state: &crate::AppState,
    task_id: Uuid,
) -> Result<NewAttemptEvidence, String> {
    let task = state
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .ok_or("Task not found")?;
    let mut evidence = NewAttemptEvidence {
        task_id,
        project_id: task.project_id,
        repository_id: None,
        run: None,
        recovery_reason: None,
        observed_commit: None,
        branch: task.branch_name.clone(),
        checkout: task.worktree_path.clone(),
        expected_head: None,
        dirty_fingerprint: None,
        dirty_paths: Vec::new(),
        eligible: false,
        refusals: Vec::new(),
    };
    let Some(handoff) = task.worker_handoff.as_ref() else {
        evidence
            .refusals
            .push("This Task has no Worker recovery evidence.".into());
        return Ok(evidence);
    };
    evidence.repository_id = handoff.repository_id;
    evidence.run = handoff.run;
    let WorkerHandoffStage::RecoveryRequired {
        reason,
        observed_commit,
    } = &handoff.stage
    else {
        evidence
            .refusals
            .push("This Worker is not waiting for a recovery decision.".into());
        return Ok(evidence);
    };
    evidence.recovery_reason = Some(reason.clone());
    evidence.observed_commit = observed_commit.clone();

    let (Some(run), Some(repository_id), Some(branch), Some(checkout)) = (
        handoff.run,
        handoff.repository_id,
        task.branch_name.clone(),
        task.worktree_path.clone(),
    ) else {
        evidence
            .refusals
            .push("The recovery record is incomplete, so a new attempt cannot be checked.".into());
        return Ok(evidence);
    };
    let repo = match crate::commands::integration::repo_path_of(state, task.project_id).await {
        Ok(repo) => repo,
        Err(message) => {
            evidence.refusals.push(message);
            return Ok(evidence);
        }
    };
    let inspection = match crate::worktree::new_attempt::inspect(&repo, &checkout, &branch).await {
        Ok(inspection) => inspection,
        Err(message) => {
            evidence.refusals.push(message);
            return Ok(evidence);
        }
    };
    evidence.expected_head = Some(inspection.head.clone());
    evidence.dirty_fingerprint = Some(inspection.dirty_fingerprint.clone());
    evidence.dirty_paths = inspection.dirty_paths.clone();

    // The same checks the authorization makes, with a placeholder id: nothing
    // is recorded here, so the id is not used.
    let request = AuthorizeNewAttemptRequest {
        decision_id: Uuid::nil(),
        task_id,
        project_id: task.project_id,
        repository_id,
        run,
        stage: "recovery_required".into(),
        recovery_reason: reason.clone(),
        observed_commit: observed_commit.clone(),
        branch,
        checkout,
        expected_head: inspection.head,
        dirty_fingerprint: inspection.dirty_fingerprint,
    };
    match prepare(state, &task, &request).await {
        Ok(_) => evidence.eligible = true,
        Err(reasons) => evidence.refusals = reasons,
    }
    Ok(evidence)
}

/// Read-only: what a new attempt would be decided on for `task_id`.
#[tauri::command]
pub async fn get_new_attempt_evidence(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<NewAttemptEvidence, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    new_attempt_evidence(&state, task_id).await
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartNewAttemptRequest {
    pub task_id: Uuid,
    /// The authorization to start. It must be the one currently confirmed.
    pub decision_id: Uuid,
}

fn refused(reason: impl Into<String>) -> Result<AuthorizeNewAttemptOutcome, String> {
    Ok(AuthorizeNewAttemptOutcome::Refused {
        reason: reason.into(),
    })
}

fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `decision` records exactly what `request` asks for.
fn same_request(decision: &NewAttemptDecision, request: &AuthorizeNewAttemptRequest) -> bool {
    decision.for_run == request.run
        && decision.recovery_reason == request.recovery_reason
        && decision.observed_commit == request.observed_commit
        && decision.branch == request.branch
        && decision.checkout_path == request.checkout
        && decision.expected_head == request.expected_head
        && decision.dirty_fingerprint == request.dirty_fingerprint
}

/// Whether the Task still shows the recovery the person was shown.
fn still_shown(task: &Task, request: &AuthorizeNewAttemptRequest) -> bool {
    task.id == request.task_id
        && task.project_id == request.project_id
        && matches!(task.status, TaskStatus::Error | TaskStatus::Backlog)
        && task.branch_name.as_deref() == Some(request.branch.as_str())
        && task.worktree_path.as_deref() == Some(request.checkout.as_str())
        && task.worker_handoff.as_ref().is_some_and(|handoff| {
            handoff.run == Some(request.run)
                && handoff.task_id == request.task_id
                && handoff.project_id == request.project_id
                && handoff.repository_id == Some(request.repository_id)
                && handoff.branch.as_deref() == Some(request.branch.as_str())
                && matches!(&handoff.stage,
                    WorkerHandoffStage::RecoveryRequired { reason, observed_commit }
                        if *reason == request.recovery_reason
                            && *observed_commit == request.observed_commit)
        })
}

/// Whether another lifecycle operation owns the Task's Git state right now.
fn lifecycle_busy(task: &Task) -> bool {
    task.cleanup_in_flight
        || task.pending_republish.is_some()
        || task.local_integration.is_some()
        || task.pr_url.is_some()
}

/// Reads the Task and its checkout now, and returns the decision they support.
/// On failure, every reason it is not supported.
async fn prepare(
    state: &crate::AppState,
    task: &Task,
    request: &AuthorizeNewAttemptRequest,
) -> Result<NewAttemptDecision, Vec<String>> {
    let mut refusals: Vec<String> = Vec::new();

    if !still_shown(task, request) {
        refusals.push(
            "The recovery evidence changed after it was shown to you. Inspect it again.".into(),
        );
    }
    if !matches!(task.status, TaskStatus::Error | TaskStatus::Backlog) {
        refusals.push("The Task is not in a state where a new attempt can start.".into());
    }
    if lifecycle_busy(task) {
        refusals.push(
            "The Task has an unfinished cleanup, restack, integration or pull request.".into(),
        );
    }
    if let Some(executor) = state.executor.get() {
        if crate::lifecycle::ExecutionOwnership::is_task_running(executor.as_ref(), task.id).await {
            refusals.push("An execution or review currently owns this Task.".into());
        }
    }

    let start_head = task
        .worker_handoff
        .as_ref()
        .and_then(|handoff| handoff.start_head.clone())
        .filter(|sha| is_object_id(sha));
    if start_head.is_none() {
        refusals.push(
            "The branch HEAD before this run was not reliably recorded, so a new attempt cannot be checked against it."
                .into(),
        );
    }

    let registered_repository = state
        .project
        .projects
        .read()
        .await
        .get(&task.project_id)
        .and_then(|project| project.repository_id);
    if registered_repository != Some(request.repository_id) {
        refusals.push("The Project repository differs from the one recorded for this run.".into());
    }
    if let Err(message) = crate::worktree::new_attempt::board_can_confirm(&state.storage, task.project_id)
    {
        refusals.push(message);
    }

    let repo_path = match crate::commands::integration::repo_path_of(state, task.project_id).await
    {
        Ok(path) => Some(path),
        Err(message) => {
            refusals.push(message);
            None
        }
    };

    if let Some(repo) = repo_path.as_deref() {
        let repo_dir = std::path::Path::new(repo);
        let tip = super::worker_recovery::commit_at(
            repo_dir,
            &format!("refs/heads/{}", request.branch),
        )
        .await;
        if tip.is_none() {
            refusals.push("The recorded branch cannot be resolved to a commit.".into());
        }
        if let (Some(tip), Some(start)) = (tip.as_deref(), start_head.as_deref()) {
            if tip != start {
                match crate::worktree::restack::is_ancestor(repo_dir, start, tip).await {
                    Ok(true) => {}
                    Ok(false) => refusals.push(
                        "The branch tip does not descend from the commit the run started from."
                            .into(),
                    ),
                    Err(e) => refusals.push(format!("Ancestry could not be verified: {e}")),
                }
            }
        }

        // Another Task claiming the same branch makes ownership ambiguous.
        let (snapshot, repo_map) = {
            let all: Vec<Task> = state.task.tasks.read().await.values().cloned().collect();
            let projects = state.project.projects.read().await;
            let repositories = state.repository.repositories.read().await;
            let paths: HashMap<Uuid, String> = projects
                .iter()
                .filter_map(|(id, p)| {
                    Some((*id, repositories.get(&p.repository_id?)?.local_path.clone()))
                })
                .collect();
            (all, paths)
        };
        if crate::worktree::tasks_sharing_a_recorded_branch(&snapshot, &repo_map)
            .await
            .contains_key(&task.id)
        {
            refusals.push("Another Task records ownership of this Git branch.".into());
        }

        match crate::worktree::new_attempt::inspect(repo, &request.checkout, &request.branch).await
        {
            Err(message) => refusals.push(message),
            Ok(inspection) => {
                // The checkout must still be what the person was shown. The
                // executor repeats this check before it launches.
                let candidate = NewAttemptDecision {
                    decision_id: request.decision_id,
                    for_run: request.run,
                    recovery_reason: request.recovery_reason.clone(),
                    branch: request.branch.clone(),
                    checkout_path: request.checkout.clone(),
                    observed_commit: request.observed_commit.clone(),
                    expected_head: request.expected_head.clone(),
                    dirty_fingerprint: request.dirty_fingerprint.clone(),
                    dirty_paths: inspection.dirty_paths.clone(),
                    authorized_at: chrono::Utc::now(),
                };
                if let Err(message) = crate::worktree::new_attempt::confirm(&candidate, &inspection)
                {
                    refusals.push(message);
                }
                if refusals.is_empty() {
                    return Ok(candidate);
                }
            }
        }
    }

    Err(refusals)
}

/// Records the person's decision and confirms it, if it is still the recovery
/// they were shown. It runs no provider.
pub(crate) async fn record_decision(
    state: &crate::AppState,
    request: AuthorizeNewAttemptRequest,
) -> Result<AuthorizeNewAttemptOutcome, String> {
    if !is_object_id(&request.expected_head) {
        return refused("The branch HEAD you inspected is not a full object id.");
    }
    if !request.dirty_fingerprint.starts_with("sha256:") {
        return refused("The fingerprint you inspected is not valid.");
    }
    if request.stage != "recovery_required" {
        return refused("Only a Worker waiting in recovery can start a new attempt.");
    }
    let task_id = request.task_id;
    let _lease = state.task_lifecycle_locks.acquire(task_id).await?;

    let task = state
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .ok_or("Task not found")?;

    // Idempotency first. The same decision id with the same content, already
    // confirmed here, is reported and not written again. The same id with other
    // content is never reinterpreted.
    if let Some(recorded) = task
        .new_attempt
        .as_ref()
        .filter(|recorded| recorded.decision_id == request.decision_id)
    {
        if !same_request(recorded, &request) {
            return refused("This decision id was already used for a different request.");
        }
        if task.new_attempt_confirmed {
            return Ok(AuthorizeNewAttemptOutcome::AlreadyAuthorized { task });
        }
        // Recorded, but not confirmed in this process. Decide it again below,
        // which rewrites the same content and then confirms it.
    }

    let decision = match prepare(state, &task, &request).await {
        Ok(decision) => decision,
        Err(reasons) => return refused(reasons.join(" ")),
    };

    // Evaluated under the board's write lock, with the save that follows. A
    // `false` answer writes nothing.
    let revise = |staged: &mut HashMap<Uuid, Task>| -> bool {
        let Some(current) = staged.get_mut(&task_id) else {
            return false;
        };
        if !still_shown(current, &request) || lifecycle_busy(current) {
            return false;
        }
        current.new_attempt = Some(decision.clone());
        // Confirmation belongs to this write only. Any earlier confirmation
        // is withdrawn in the same transition.
        current.new_attempt_confirmed = false;
        current.record_activity(ActivityKind::NewAttemptAuthorized { run: request.run });
        true
    };

    match crate::lifecycle::record_typed_if_changed(
        &state.task.tasks,
        &state.storage,
        task_id,
        &revise,
    )
    .await
    {
        Err(crate::lifecycle::RecordWriteError::NotPublished(reason)) => {
            refused(format!("Nothing was recorded: {reason}"))
        }
        Err(crate::lifecycle::RecordWriteError::PublishedNotDurable(detail)) => {
            let task = state
                .task
                .tasks
                .read()
                .await
                .get(&task_id)
                .cloned()
                .ok_or("Task not found")?;
            // Deliberately unconfirmed. The launch refuses it, and the person
            // decides again.
            Ok(AuthorizeNewAttemptOutcome::DurabilityUnconfirmed { task, detail })
        }
        Ok(false) => refused(
            "The Task changed while the decision was being recorded, so nothing was recorded.",
        ),
        Ok(true) => {
            let mut tasks = state.task.tasks.write().await;
            let task = tasks.get_mut(&task_id).ok_or("Task not found")?;
            if task
                .new_attempt
                .as_ref()
                .is_some_and(|recorded| recorded.decision_id == request.decision_id)
            {
                task.new_attempt_confirmed = true;
            }
            Ok(AuthorizeNewAttemptOutcome::Authorized { task: task.clone() })
        }
    }
}

/// Records the person's decision to start a new attempt on an interrupted run.
#[tauri::command]
pub async fn authorize_new_attempt(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    request: AuthorizeNewAttemptRequest,
) -> Result<AuthorizeNewAttemptOutcome, String> {
    record_decision(&state, request).await
}

/// Starts the new attempt a person authorized. The executor admits it only for
/// the confirmed decision and only if the checkout still matches it.
#[tauri::command]
pub async fn start_new_attempt(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    request: StartNewAttemptRequest,
) -> Result<Task, String> {
    let executor = state.executor.get().ok_or("Executor not initialized")?;
    executor
        .start_new_attempt(request.task_id, request.decision_id)
        .await?;
    state
        .task
        .tasks
        .read()
        .await
        .get(&request.task_id)
        .cloned()
        .ok_or_else(|| "Task not found".to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::domain::task::{ActivityKind, TaskPhase, WorkerHandoff};
    use crate::domain::{AgentConfig, AgentType, Project, ProjectScope, Repository};
    use crate::test_helpers::{attach_test_executor, create_test_task_full, FakeProgram};
    use slashit_durable_fs::{fault, Stage};
    use std::path::{Path, PathBuf};

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(["-c", "user.email=t@example.com", "-c", "user.name=T"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        state: crate::AppState,
        repo: PathBuf,
        checkout: PathBuf,
        project_id: Uuid,
        repository_id: Uuid,
        task_id: Uuid,
        branch: String,
        start: String,
    }

    impl Fx {
        async fn task(&self) -> Task {
            self.state.task.tasks.read().await[&self.task_id].clone()
        }

        fn on_disk(&self) -> Task {
            self.state
                .storage
                .load_project_tasks(self.project_id)
                .unwrap()
                .into_iter()
                .find(|t| t.id == self.task_id)
                .unwrap()
        }

        fn checkout_path(&self) -> String {
            self.checkout.to_string_lossy().to_string()
        }

        fn head(&self) -> String {
            git(&self.checkout, &["rev-parse", "HEAD"])
        }

        /// Everything Git holds that a new attempt must not change.
        fn repo_state(&self) -> String {
            format!(
                "{}\n{}\n{}\n{}",
                git(&self.repo, &["for-each-ref"]),
                git(&self.repo, &["worktree", "list", "--porcelain"]),
                self.head(),
                git(
                    &self.checkout,
                    &["status", "--porcelain=v1", "--untracked-files=all"]
                ),
            )
        }

        /// The Task as the board holds it on disk, and in memory, as JSON.
        async fn board(&self) -> (String, String) {
            let memory = serde_json::to_string(&self.task().await).unwrap();
            let disk = serde_json::to_string(&self.on_disk()).unwrap();
            (memory, disk)
        }

        /// A request built from what the checkout holds now, as the recovery
        /// view would show it to the person.
        async fn request(&self, decision_id: Uuid) -> AuthorizeNewAttemptRequest {
            let dirty_fingerprint =
                crate::worktree::checkout_fingerprint::fingerprint(&self.checkout)
                    .await
                    .unwrap()
                    .digest;
            AuthorizeNewAttemptRequest {
                decision_id,
                task_id: self.task_id,
                project_id: self.project_id,
                repository_id: self.repository_id,
                run: 1,
                stage: "recovery_required".into(),
                recovery_reason: "interrupted".into(),
                observed_commit: None,
                branch: self.branch.clone(),
                checkout: self.checkout_path(),
                expected_head: self.head(),
                dirty_fingerprint,
            }
        }

        fn write(&self, name: &str, contents: &str) {
            std::fs::write(self.checkout.join(name), contents).unwrap();
        }

        async fn edit(&self, change: impl FnOnce(&mut Task)) {
            let mut tasks = self.state.task.tasks.write().await;
            change(tasks.get_mut(&self.task_id).unwrap());
            let board: Vec<Task> = tasks.values().cloned().collect();
            self.state
                .storage
                .save_project_tasks(self.project_id, &board)
                .unwrap();
        }
    }

    async fn fixture() -> Fx {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let (state, _) = crate::app_core::build_state_with_paths(paths)
            .await
            .unwrap();
        attach_test_executor(&state);

        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("base.txt"), "base").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        let start = git(&repo, &["rev-parse", "HEAD"]);
        let branch = "task-interrupted".to_string();
        let checkout = tmp.path().join("checkout");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                checkout.to_str().unwrap(),
                "main",
            ],
        );

        let repository_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        state.repository.repositories.write().await.insert(
            repository_id,
            Repository {
                id: repository_id,
                local_path: repo.to_string_lossy().to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        state.project.projects.write().await.insert(
            project_id,
            Project {
                id: project_id,
                name: "p".into(),
                repository_id: Some(repository_id),
                scope: ProjectScope::default(),
                state_location: crate::config::paths::StateLocation::External,
                base: None,
                agent_type: AgentType::ClaudeCode,
                agent_config: AgentConfig {
                    agent_type: AgentType::ClaudeCode,
                    command: "claude".into(),
                    args: Vec::new(),
                    env: HashMap::new(),
                    model: None,
                    api_key: None,
                },
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );

        let mut task = create_test_task_full("interrupted", project_id, TaskStatus::Error, 0);
        task.branch_name = Some(branch.clone());
        task.worktree_path = Some(checkout.to_string_lossy().to_string());
        task.phase = TaskPhase::Idle;
        task.error_message = Some("interrupted".into());
        task.worker_handoff = Some(WorkerHandoff {
            task_id: task.id,
            project_id,
            repository_id: Some(repository_id),
            run: Some(1),
            branch: Some(branch.clone()),
            start_head: Some(start.clone()),
            acceptance: None,
            stage: WorkerHandoffStage::RecoveryRequired {
                reason: "interrupted".into(),
                observed_commit: None,
            },
        });
        task.record_activity(ActivityKind::RunStarted {
            run: 1,
            addressing_feedback: false,
        });
        task.record_activity(ActivityKind::RecoveryRequired {
            run: Some(1),
            reason: "interrupted".into(),
        });
        let task_id = task.id;
        state.task.tasks.write().await.insert(task_id, task.clone());
        state
            .storage
            .save_project_tasks(project_id, &[task])
            .unwrap();

        let fx = Fx {
            _tmp: tmp,
            state,
            repo,
            checkout: checkout.clone(),
            project_id,
            repository_id,
            task_id,
            branch,
            start,
        };
        // The run committed its work, so the branch is ahead of where it started.
        std::fs::write(checkout.join("work.txt"), "work").unwrap();
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-q", "-m", "work"]);
        fx
    }

    fn refusal(outcome: Result<AuthorizeNewAttemptOutcome, String>) -> String {
        match outcome.expect("a refusal is an answer, not an error") {
            AuthorizeNewAttemptOutcome::Refused { reason } => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// Whether the stand-in provider starts within a few seconds. A registered
    /// run spawns its process from its own task.
    async fn agent_invoked(agent: &FakeProgram) -> bool {
        for _ in 0..100 {
            if agent.invocations().contains("claude") {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    fn new_attempt_runs(task: &Task) -> usize {
        task.activity
            .iter()
            .filter(|e| matches!(e.kind, ActivityKind::RunStarted { run: 2, .. }))
            .count()
    }

    #[tokio::test]
    async fn a_clean_checkout_is_authorized_confirmed_and_persisted_without_confirmation() {
        let fx = fixture().await;
        let request = fx.request(Uuid::new_v4()).await;

        let outcome = record_decision(&fx.state, request.clone()).await.unwrap();

        let AuthorizeNewAttemptOutcome::Authorized { task } = outcome else {
            panic!("expected Authorized, got {outcome:?}");
        };
        assert!(task.new_attempt_confirmed);
        let decision = task.new_attempt.as_ref().expect("recorded");
        assert_eq!(decision.expected_head, fx.head());
        assert!(decision.dirty_paths.is_empty());
        assert!(
            task.activity
                .iter()
                .any(|e| matches!(e.kind, ActivityKind::NewAttemptAuthorized { run: 1 })),
            "the authorization is in the timeline"
        );
        // Reloaded from disk, the decision is still there but never confirmed.
        let reloaded = fx.on_disk();
        assert_eq!(reloaded.new_attempt, task.new_attempt);
        assert!(!reloaded.new_attempt_confirmed);
        assert!(!reloaded.new_attempt_admits());
    }

    #[tokio::test]
    async fn a_dirty_checkout_is_authorized_with_the_paths_it_will_commit() {
        let fx = fixture().await;
        fx.write("work.txt", "edited after the run");
        fx.write("notes.txt", "a person's note");
        let request = fx.request(Uuid::new_v4()).await;

        let AuthorizeNewAttemptOutcome::Authorized { task } =
            record_decision(&fx.state, request).await.unwrap()
        else {
            panic!("expected Authorized");
        };

        let decision = task.new_attempt.unwrap();
        assert_eq!(decision.dirty_paths, vec!["notes.txt", "work.txt"]);
        assert!(!decision.dirty_fingerprint.is_empty());
    }

    #[tokio::test]
    async fn an_identical_repeat_is_reported_and_writes_nothing_again() {
        let fx = fixture().await;
        let request = fx.request(Uuid::new_v4()).await;
        record_decision(&fx.state, request.clone()).await.unwrap();
        let before = fx.board().await;

        let outcome = record_decision(&fx.state, request).await.unwrap();

        assert!(matches!(
            outcome,
            AuthorizeNewAttemptOutcome::AlreadyAuthorized { .. }
        ));
        assert_eq!(fx.board().await, before, "a repeat is not written again");
    }

    #[tokio::test]
    async fn a_reused_decision_id_with_other_content_is_refused_without_a_write() {
        let fx = fixture().await;
        let id = Uuid::new_v4();
        let request = fx.request(id).await;
        record_decision(&fx.state, request.clone()).await.unwrap();
        let before = fx.board().await;

        let mut other = request;
        other.recovery_reason = "a different reason".into();
        let reason = refusal(record_decision(&fx.state, other).await);

        assert!(reason.contains("already used"), "{reason}");
        assert_eq!(fx.board().await, before);
    }

    #[tokio::test]
    async fn content_changed_after_inspection_is_refused_even_when_git_status_is_unchanged() {
        let fx = fixture().await;
        fx.write("work.txt", "one");
        let request = {
            let mut r = fx.request(Uuid::new_v4()).await;
            r.dirty_fingerprint = crate::worktree::checkout_fingerprint::fingerprint(&fx.checkout)
                .await
                .unwrap()
                .digest;
            r
        };
        // The same status line (` M work.txt`), different bytes.
        fx.write("work.txt", "two");
        let before = fx.board().await;

        let reason = refusal(record_decision(&fx.state, request).await);

        assert!(reason.contains("changed after you inspected"), "{reason}");
        assert_eq!(fx.board().await, before, "nothing is recorded");
    }

    #[tokio::test]
    async fn a_branch_that_moved_after_inspection_is_refused() {
        let fx = fixture().await;
        let request = fx.request(Uuid::new_v4()).await;
        std::fs::write(fx.checkout.join("later.txt"), "later").unwrap();
        git(&fx.checkout, &["add", "-A"]);
        git(&fx.checkout, &["commit", "-q", "-m", "later"]);
        let before = fx.board().await;

        let reason = refusal(record_decision(&fx.state, request).await);

        assert!(reason.contains("branch moved"), "{reason}");
        assert_eq!(fx.board().await, before);
    }

    #[tokio::test]
    async fn a_task_no_longer_waiting_for_recovery_is_refused() {
        let fx = fixture().await;
        let request = fx.request(Uuid::new_v4()).await;
        fx.edit(|t| {
            t.worker_handoff.as_mut().unwrap().stage = WorkerHandoffStage::Started;
        })
        .await;

        let reason = refusal(record_decision(&fx.state, request).await);

        assert!(reason.contains("changed after it was shown"), "{reason}");
    }

    #[tokio::test]
    async fn a_replaced_checkout_path_is_refused() {
        let fx = fixture().await;
        let mut request = fx.request(Uuid::new_v4()).await;
        request.checkout = fx.repo.to_string_lossy().to_string();

        let reason = refusal(record_decision(&fx.state, request).await);

        assert!(reason.contains("changed after it was shown"), "{reason}");
    }

    #[tokio::test]
    async fn legacy_recovery_without_a_recorded_start_head_is_refused() {
        let fx = fixture().await;
        let request = fx.request(Uuid::new_v4()).await;
        fx.edit(|t| t.worker_handoff.as_mut().unwrap().start_head = None)
            .await;

        let reason = refusal(record_decision(&fx.state, request).await);

        assert!(reason.contains("not reliably recorded"), "{reason}");
    }

    #[tokio::test]
    async fn a_branch_claimed_by_another_task_is_refused() {
        let fx = fixture().await;
        let request = fx.request(Uuid::new_v4()).await;
        let mut other = create_test_task_full("other", fx.project_id, TaskStatus::Backlog, 0);
        other.branch_name = Some(fx.branch.clone());
        other.worktree_path = Some(fx.checkout_path());
        other.worker_handoff = Some(WorkerHandoff {
            task_id: other.id,
            project_id: fx.project_id,
            repository_id: Some(fx.repository_id),
            run: Some(1),
            branch: Some(fx.branch.clone()),
            start_head: Some(fx.start.clone()),
            acceptance: None,
            stage: WorkerHandoffStage::Started,
        });
        fx.state
            .task
            .tasks
            .write()
            .await
            .insert(other.id, other);

        let reason = refusal(record_decision(&fx.state, request).await);

        assert!(reason.contains("Another Task"), "{reason}");
    }

    #[tokio::test]
    async fn a_write_that_publishes_nothing_records_nothing() {
        let fx = fixture().await;
        let request = fx.request(Uuid::new_v4()).await;
        let before = fx.board().await;

        let reason = {
            let _fault = fault::fail_at(Stage::Replace, std::io::ErrorKind::Other);
            refusal(record_decision(&fx.state, request).await)
        };

        assert!(reason.contains("Nothing was recorded"), "{reason}");
        assert_eq!(fx.board().await, before);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_published_but_unconfirmed_decision_is_never_treated_as_authorized() {
        let fx = fixture().await;
        let request = fx.request(Uuid::new_v4()).await;

        let outcome = {
            // The first directory sync is the board check; the write's own sync fails.
            let _fault = fault::fail_after(Stage::SyncDirectory, std::io::ErrorKind::Other, 1);
            record_decision(&fx.state, request).await.unwrap()
        };

        let AuthorizeNewAttemptOutcome::DurabilityUnconfirmed { task, .. } = outcome else {
            panic!("expected DurabilityUnconfirmed, got {outcome:?}");
        };
        assert!(!task.new_attempt_confirmed);
        assert!(!task.new_attempt_admits());
    }

    #[tokio::test]
    async fn authorizing_changes_no_byte_in_the_checkout_or_git() {
        let fx = fixture().await;
        fx.write("work.txt", "dirty");
        let request = fx.request(Uuid::new_v4()).await;
        let repo_before = fx.repo_state();

        record_decision(&fx.state, request).await.unwrap();

        assert_eq!(fx.repo_state(), repo_before);
        assert_eq!(std::fs::read_to_string(fx.checkout.join("work.txt")).unwrap(), "dirty");
    }

    /// A confirmed decision starts exactly one run, consumes itself in the same
    /// record, and keeps the checkout's uncommitted work.
    #[tokio::test]
    async fn a_confirmed_decision_starts_exactly_one_run_and_keeps_the_checkout() {
        let fx = fixture().await;
        let agent = FakeProgram::install("claude", "cat >/dev/null; sleep 20").await;
        fx.write("notes.txt", "kept");
        let id = Uuid::new_v4();
        let request = fx.request(id).await;
        record_decision(&fx.state, request).await.unwrap();
        let executor = fx.state.executor.get().unwrap().clone();

        executor.start_new_attempt(fx.task_id, id).await.unwrap();

        let task = fx.task().await;
        assert_eq!(new_attempt_runs(&task), 1, "exactly one new run is recorded");
        assert!(task.new_attempt.is_none(), "the authorization is consumed");
        assert!(!task.new_attempt_confirmed);
        assert_eq!(
            std::fs::read_to_string(fx.checkout.join("notes.txt")).unwrap(),
            "kept"
        );
        assert!(agent_invoked(&agent).await, "the provider starts once");
        assert_eq!(agent.invocations().matches("claude").count(), 1);

        // Repeating the same authorization never starts a second run.
        let again = executor.start_new_attempt(fx.task_id, id).await;
        assert!(again.is_err(), "{again:?}");
        assert_eq!(new_attempt_runs(&fx.task().await), 1);
        let _ = executor.stop_task(fx.task_id).await;
    }

    #[tokio::test]
    async fn an_unconfirmed_decision_never_starts_a_run() {
        let fx = fixture().await;
        let agent = FakeProgram::install("claude", "cat >/dev/null; sleep 20").await;
        let id = Uuid::new_v4();
        let request = fx.request(id).await;
        {
            let _fault = fault::fail_after(Stage::SyncDirectory, std::io::ErrorKind::Other, 1);
            record_decision(&fx.state, request).await.unwrap();
        }
        let before = fx.board().await;
        let executor = fx.state.executor.get().unwrap().clone();

        let refused = executor.start_new_attempt(fx.task_id, id).await;

        assert!(refused.unwrap_err().contains("not confirmed"));
        assert_eq!(fx.board().await, before, "nothing is recorded");
        assert_eq!(agent.invocations(), "", "no provider starts");
    }

    #[tokio::test]
    async fn a_checkout_changed_after_authorization_refuses_the_start_and_records_nothing() {
        let fx = fixture().await;
        let agent = FakeProgram::install("claude", "cat >/dev/null; sleep 20").await;
        let id = Uuid::new_v4();
        record_decision(&fx.state, fx.request(id).await).await.unwrap();
        fx.write("notes.txt", "changed after the decision");
        let before = fx.board().await;
        let executor = fx.state.executor.get().unwrap().clone();

        let refused = executor.start_new_attempt(fx.task_id, id).await;

        let reason = refused.unwrap_err();
        assert!(reason.contains("uncommitted work changed"), "{reason}");
        assert_eq!(fx.board().await, before);
        assert_eq!(agent.invocations(), "");
        assert_eq!(new_attempt_runs(&fx.task().await), 0);
    }

    #[tokio::test]
    async fn a_branch_moved_after_authorization_refuses_the_start() {
        let fx = fixture().await;
        let agent = FakeProgram::install("claude", "cat >/dev/null; sleep 20").await;
        let id = Uuid::new_v4();
        record_decision(&fx.state, fx.request(id).await).await.unwrap();
        std::fs::write(fx.checkout.join("later.txt"), "later").unwrap();
        git(&fx.checkout, &["add", "-A"]);
        git(&fx.checkout, &["commit", "-q", "-m", "later"]);
        let executor = fx.state.executor.get().unwrap().clone();

        let reason = executor
            .start_new_attempt(fx.task_id, id)
            .await
            .unwrap_err();

        assert!(reason.contains("branch moved"), "{reason}");
        assert_eq!(agent.invocations(), "");
        assert_eq!(new_attempt_runs(&fx.task().await), 0);
    }

    #[tokio::test]
    async fn a_second_start_while_the_attempt_runs_is_refused_and_stopping_ends_it() {
        let fx = fixture().await;
        let agent = FakeProgram::install("claude", "cat >/dev/null; sleep 20").await;
        let id = Uuid::new_v4();
        record_decision(&fx.state, fx.request(id).await).await.unwrap();
        let executor = fx.state.executor.get().unwrap().clone();
        executor.start_new_attempt(fx.task_id, id).await.unwrap();
        assert!(agent_invoked(&agent).await);

        let second = executor.start_new_attempt(fx.task_id, id).await.unwrap_err();

        assert!(second.contains("already has an agent"), "{second}");
        assert_eq!(agent.invocations().matches("claude").count(), 1);
        executor.stop_task(fx.task_id).await.unwrap();
        assert!(!executor.is_task_running(fx.task_id).await);
    }

    /// A start was recorded, then SlashIt stopped while its provider kept
    /// running. The process that starts next finds the run `Started` with no
    /// owner. It holds the run for a person and launches nothing.
    #[tokio::test]
    async fn a_restart_after_the_attempt_started_holds_it_for_a_person_and_launches_nothing() {
        let fx = fixture().await;
        let agent = FakeProgram::install("claude", "cat >/dev/null; sleep 20").await;
        let id = Uuid::new_v4();
        record_decision(&fx.state, fx.request(id).await).await.unwrap();
        let first = fx.state.executor.get().unwrap().clone();
        first.start_new_attempt(fx.task_id, id).await.unwrap();
        assert!(agent_invoked(&agent).await);

        let restarted = crate::test_helpers::build_test_executor(&fx.state);
        restarted.poll_once_for_test().await;

        let task = fx.task().await;
        let handoff = task.worker_handoff.as_ref().unwrap();
        assert!(
            matches!(handoff.stage, WorkerHandoffStage::RecoveryRequired { .. }),
            "{:?}",
            handoff.stage
        );
        assert_eq!(handoff.run, Some(2));
        assert_eq!(
            agent.invocations().matches("claude").count(),
            1,
            "the restarted process launches no second Worker"
        );
        first.stop_task(fx.task_id).await.unwrap();
    }

    /// The start is published but not confirmed durable, and the hold that
    /// should follow cannot be written either. Nothing launches. The process
    /// that starts next holds the run for a person.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_attempt_published_but_not_durable_never_launches() {
        let fx = fixture().await;
        let agent = FakeProgram::install("claude", "cat >/dev/null; sleep 20").await;
        let id = Uuid::new_v4();
        record_decision(&fx.state, fx.request(id).await).await.unwrap();
        let executor = fx.state.executor.get().unwrap().clone();

        let refused = {
            // The first directory sync is the board check; the start's own sync fails.
            let _fault = fault::fail_after(Stage::SyncDirectory, std::io::ErrorKind::Other, 1);
            executor.start_new_attempt(fx.task_id, id).await
        };

        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(
            new_attempt_runs(&fx.task().await),
            1,
            "the start reached the published-but-not-durable path"
        );
        assert_eq!(agent.invocations(), "", "nothing launches on an unconfirmed start");

        let restarted = crate::test_helpers::build_test_executor(&fx.state);
        restarted.poll_once_for_test().await;

        let task = fx.task().await;
        assert!(
            matches!(
                task.worker_handoff.as_ref().map(|h| &h.stage),
                Some(WorkerHandoffStage::RecoveryRequired { .. })
            ),
            "{:?}",
            task.worker_handoff
        );
        assert_eq!(agent.invocations(), "");
        assert!(!executor.is_task_running(fx.task_id).await);
    }

    /// A decision that was persisted by an earlier process, and so is not
    /// confirmed in this one, never starts a run.
    #[tokio::test]
    async fn a_decision_persisted_by_an_earlier_process_cannot_start_a_run() {
        let fx = fixture().await;
        let agent = FakeProgram::install("claude", "cat >/dev/null; sleep 20").await;
        let id = Uuid::new_v4();
        record_decision(&fx.state, fx.request(id).await).await.unwrap();
        let reloaded = fx.on_disk();
        assert!(reloaded.new_attempt.is_some());
        fx.edit(move |t| *t = reloaded).await;
        let executor = fx.state.executor.get().unwrap().clone();

        let reason = executor
            .start_new_attempt(fx.task_id, id)
            .await
            .unwrap_err();

        assert!(reason.contains("not confirmed"), "{reason}");
        assert_eq!(agent.invocations(), "");
    }

    /// The ordinary start path never lifts the recovery refusal, even for a
    /// Task that holds a confirmed new-attempt decision.
    #[tokio::test]
    async fn the_ordinary_start_still_refuses_a_recovered_task_with_a_confirmed_decision() {
        let fx = fixture().await;
        let agent = FakeProgram::install("claude", "cat >/dev/null; sleep 20").await;
        record_decision(&fx.state, fx.request(Uuid::new_v4()).await)
            .await
            .unwrap();
        let executor = fx.state.executor.get().unwrap().clone();

        let refused = executor.execute_task(fx.task_id).await.unwrap_err();

        assert!(refused.contains("unresolved Worker recovery evidence"), "{refused}");
        assert_eq!(agent.invocations(), "");
        assert!(fx.task().await.new_attempt.is_some());
    }

    /// On a filesystem that cannot synchronize the board's directory, a write
    /// can succeed without surviving a power loss. Such a decision is refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_board_whose_directory_cannot_be_synchronized_authorizes_nothing() {
        let fx = fixture().await;
        let request = fx.request(Uuid::new_v4()).await;
        let before = fx.board().await;

        let reason = {
            let _fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Unsupported);
            refusal(record_decision(&fx.state, request).await)
        };

        assert!(reason.contains("cannot synchronize"), "{reason}");
        assert_eq!(fx.board().await, before);
    }

    /// A start is also refused on such a filesystem, before anything is written
    /// or any provider starts, even when the decision was made on a good one.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_start_is_refused_when_the_board_directory_cannot_be_synchronized() {
        let fx = fixture().await;
        let agent = FakeProgram::install("claude", "cat >/dev/null; sleep 20").await;
        let id = Uuid::new_v4();
        record_decision(&fx.state, fx.request(id).await).await.unwrap();
        let before = fx.board().await;
        let executor = fx.state.executor.get().unwrap().clone();

        let reason = {
            let _fault = fault::fail_at(Stage::SyncDirectory, std::io::ErrorKind::Unsupported);
            executor.start_new_attempt(fx.task_id, id).await.unwrap_err()
        };

        assert!(reason.contains("cannot synchronize"), "{reason}");
        assert_eq!(fx.board().await, before);
        assert_eq!(agent.invocations(), "");
    }

    /// A provider that fails on its own during a new attempt leaves the Task
    /// in recovery. The checkout still holds the prior run's work, so an
    /// ordinary retry must not be offered.
    #[tokio::test]
    async fn a_failing_new_attempt_provider_stays_in_recovery() {
        let fx = fixture().await;
        let _agent = FakeProgram::install("claude", "cat >/dev/null; exit 1").await;
        let id = Uuid::new_v4();
        record_decision(&fx.state, fx.request(id).await).await.unwrap();
        let executor = fx.state.executor.get().unwrap().clone();
        executor.start_new_attempt(fx.task_id, id).await.unwrap();

        let mut failed = None;
        for _ in 0..200 {
            let task = fx.task().await;
            if task
                .activity
                .iter()
                .any(|e| matches!(e.kind, ActivityKind::RunFailed { run: Some(2), .. }))
            {
                failed = Some(task);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let task = failed.expect("the failed new-attempt run was recorded");

        let handoff = task.worker_handoff.as_ref().expect("the handoff is kept");
        assert!(
            matches!(handoff.stage, WorkerHandoffStage::RecoveryRequired { .. }),
            "{:?}",
            handoff.stage
        );
        assert_eq!(handoff.run, Some(2));
        assert!(task.worker_recovery_refusal().is_some());
    }

    /// The snapshot the person decides on reports the same facts the
    /// authorization binds, and changes nothing.
    #[tokio::test]
    async fn the_evidence_read_is_eligible_with_its_dirty_paths_and_changes_nothing() {
        let fx = fixture().await;
        fx.write("notes.txt", "a person's note");
        let before = fx.board().await;
        let repo_before = fx.repo_state();

        let evidence = new_attempt_evidence(&fx.state, fx.task_id).await.unwrap();

        assert!(evidence.eligible, "{:?}", evidence.refusals);
        assert_eq!(evidence.run, Some(1));
        assert_eq!(evidence.expected_head.as_deref(), Some(fx.head().as_str()));
        assert_eq!(evidence.dirty_paths, vec!["notes.txt"]);
        assert!(evidence.dirty_fingerprint.as_deref().is_some_and(|d| d.starts_with("sha256:")));
        assert_eq!(fx.board().await, before);
        assert_eq!(fx.repo_state(), repo_before);
    }

    #[tokio::test]
    async fn the_evidence_read_refuses_a_task_that_is_not_waiting_for_recovery() {
        let fx = fixture().await;
        fx.edit(|t| t.worker_handoff.as_mut().unwrap().stage = WorkerHandoffStage::Started)
            .await;

        let evidence = new_attempt_evidence(&fx.state, fx.task_id).await.unwrap();

        assert!(!evidence.eligible);
        assert!(
            evidence.refusals.iter().any(|r| r.contains("not waiting")),
            "{:?}",
            evidence.refusals
        );
    }
}
