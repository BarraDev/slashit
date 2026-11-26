// SPDX-License-Identifier: MPL-2.0
//! Review and delivery for an external agent, with every decision kept for a
//! person. See `docs/architecture/agent-review-delivery.md`.
//!
//! The control channel can read review and delivery state, file a request
//! ([`create_request`]) and spend an authorization a person gave
//! ([`open_authorized_pr`]). It cannot decide a request: [`decide`] is
//! reached only through the desktop command [`decide_agent_request`].

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{Duration, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::commands::gui_admission::GuiMutation;
use crate::commands::human_review::{self, PrDelivery};
use crate::commands::pr::{task_branch_tip, task_branch_tip_read_only, COVERAGE_READ_BOUND};
use crate::domain::agent_request::{
    check_actor, AgentRequest, HumanDecision, RequestState, RequestUse, RequestedOperation,
    UseOutcome, AUTHORIZATION_LIFETIME_MINUTES, MAX_REQUESTS_PER_TASK,
};
use crate::domain::Task;

const MOVED: &str = "The branch is no longer at the commit this request names, so nothing was done \
                     and the request is no longer valid. Review the branch and ask again.";
const NOT_READ: &str = "The branch's commit could not be read, so nothing was done.";

/// A request as it is shown to a person or an agent.
#[derive(Debug, Clone, Serialize)]
pub struct AgentRequestView {
    pub task_id: Uuid,
    pub task_title: String,
    pub state: RequestState,
    #[serde(flatten)]
    pub request: AgentRequest,
}

/// What a person's decision left behind.
#[derive(Debug, Clone, Serialize)]
pub struct DecisionOutcome {
    pub task: Task,
    pub request: AgentRequestView,
}

fn view(task: &Task, request: &AgentRequest, tip: Option<&str>) -> AgentRequestView {
    AgentRequestView {
        task_id: task.id,
        task_title: task.title.clone(),
        state: request.state(task, tip, Utc::now()),
        request: request.clone(),
    }
}

async fn current(state: &crate::AppState, task_id: Uuid) -> Result<Task, String> {
    state
        .task
        .tasks
        .read()
        .await
        .get(&task_id)
        .cloned()
        .ok_or_else(|| "Task not found".to_string())
}

fn find(task: &Task, request_id: Uuid) -> Result<&AgentRequest, String> {
    task.agent_requests
        .iter()
        .find(|r| r.id == request_id)
        .ok_or_else(|| "This task has no such request.".to_string())
}

/// The branch tip for a read, which must not change the repository. `None` when
/// it cannot be read in time.
async fn shown_tip(state: &crate::AppState, task_id: Uuid) -> Option<String> {
    tokio::time::timeout(COVERAGE_READ_BOUND, task_branch_tip_read_only(state, task_id))
        .await
        .ok()?
        .ok()?
}

/// One durable change of one request, with its precondition checked in the same
/// write.
async fn amend_request(
    state: &crate::AppState,
    task_id: Uuid,
    request_id: Uuid,
    change: &(dyn Fn(&mut Task, usize) -> Result<(), String> + Send + Sync),
) -> Result<Task, String> {
    let revise = |staged: &mut HashMap<Uuid, Task>| {
        let task = staged.get_mut(&task_id).ok_or("Task not found")?;
        let index = task
            .agent_requests
            .iter()
            .position(|r| r.id == request_id)
            .ok_or("This task has no such request.")?;
        change(task, index)
    };
    crate::lifecycle::commit_task_checked(&state.task.tasks, &state.storage, task_id, &revise)
        .await?
        .ok_or_else(|| "Task not found".to_string())
}

async fn invalidate(state: &crate::AppState, task_id: Uuid, request_id: Uuid) -> String {
    let saved = amend_request(state, task_id, request_id, &|task, index| {
        task.agent_requests[index].invalidated_at.get_or_insert_with(Utc::now);
        Ok(())
    })
    .await;
    match saved {
        Ok(_) => MOVED.to_string(),
        Err(e) => format!("{MOVED} That could not be recorded: {e}"),
    }
}

/// File an agent's request for a person to decide. Nothing is approved,
/// authorized or published by it.
///
/// Asking again for the same operation and commit under the same name answers
/// with the open request, so a lost reply can be retried.
pub(crate) async fn create_request(
    state: &crate::AppState,
    task_id: Uuid,
    operation: RequestedOperation,
    commit: &str,
    actor: &str,
) -> Result<AgentRequestView, String> {
    check_actor(actor)?;
    if !crate::worktree::restack::is_full_object_id(commit) {
        return Err("Name the commit by its full object ID.".to_string());
    }
    let _lease = crate::commands::project::lease_active_task(state, task_id).await?;
    let tip = task_branch_tip_read_only(state, task_id).await?;
    if tip.as_deref() != Some(commit) {
        return Err("The task's branch is not at that commit, so no request was filed.".to_string());
    }
    if operation == RequestedOperation::OpenPr && state.task.deliveries.contains(task_id) {
        return Err("A pull request for this task is already being opened.".to_string());
    }

    let chosen = Mutex::new(None::<Uuid>);
    let revise = |staged: &mut HashMap<Uuid, Task>| {
        let task = staged.get_mut(&task_id).ok_or("Task not found")?;
        let now = Utc::now();
        let seen = task.clone();
        // The branch was just read at `commit`: an open request for any other
        // commit has been left behind.
        for request in &mut task.agent_requests {
            if request.commit != commit && request.state(&seen, None, now).is_open() {
                request.invalidated_at = Some(now);
            }
        }
        let open = task.agent_requests.iter().find(|r| {
            r.operation == operation && r.state(&seen, Some(commit), now).is_open()
        });
        let id = if let Some(open) = open {
            if open.requested_by != actor {
                return Err("Another agent's request for this operation is still open.".to_string());
            }
            open.id
        } else {
            let request = AgentRequest::new(
                operation,
                commit.to_string(),
                task.human_review.arrivals,
                actor.to_string(),
                now,
            );
            if let Some(refusal) = request.binding_refusal(task) {
                return Err(refusal.to_string());
            }
            let refused = task.agent_requests.iter().any(|r| {
                r.operation == operation
                    && r.commit == commit
                    && r.arrival == request.arrival
                    && r.decision.as_ref().is_some_and(|d| !d.authorized)
            });
            if refused {
                return Err("A person refused this operation for this commit.".to_string());
            }
            if task.agent_requests.len() >= MAX_REQUESTS_PER_TASK {
                // Only a request that never authorized anything is dropped.
                let spare = task.agent_requests.iter().position(|r| {
                    r.used.is_none() && !r.decision.as_ref().is_some_and(|d| d.authorized)
                });
                match spare {
                    Some(index) => task.agent_requests.remove(index),
                    None => return Err("This task cannot hold any more requests.".to_string()),
                };
            }
            let id = request.id;
            task.agent_requests.push(request);
            id
        };
        *chosen.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id);
        Ok(())
    };
    let task =
        crate::lifecycle::commit_task_checked(&state.task.tasks, &state.storage, task_id, &revise)
            .await
            .map_err(|e| format!("The request was not filed: {e}"))?
            .ok_or("Task not found")?;
    let id = chosen
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ok_or("The request was not filed.")?;
    Ok(view(&task, find(&task, id)?, Some(commit)))
}

/// A person's decision on a pending request. `commit` is the commit the person
/// was shown.
///
/// Approving a request to approve records the Human Review approval of that
/// commit in the same write. Authorizing a pull request publishes nothing: it
/// lets the agent start it once, for a limited time. Repeating the same
/// decision changes nothing and does not extend that time.
pub(crate) async fn decide(
    state: &crate::AppState,
    task_id: Uuid,
    request_id: Uuid,
    commit: &str,
    authorize: bool,
) -> Result<DecisionOutcome, String> {
    let _lease = crate::commands::project::lease_active_task(state, task_id).await?;
    let task = current(state, task_id).await?;
    let request = find(&task, request_id)?;
    if request.commit != commit {
        return Err("This request is for another commit than the one shown.".to_string());
    }
    if let Some(decision) = &request.decision {
        if decision.authorized != authorize {
            return Err("This request was already decided.".to_string());
        }
        let request = view(&task, request, None);
        return Ok(DecisionOutcome { task, request });
    }
    if request.state(&task, None, Utc::now()) != RequestState::Pending {
        return Err("This request is no longer waiting for a decision.".to_string());
    }
    let operation = request.operation;
    if authorize {
        let tip = match operation {
            // The read an approval made in the Human Review panel uses.
            RequestedOperation::Approve => {
                crate::commands::integration::tip_to_approve(state, &task).await
            }
            RequestedOperation::OpenPr => task_branch_tip(state, task_id).await?,
        };
        match tip.as_deref() {
            None => return Err(NOT_READ.to_string()),
            Some(tip) if tip != commit => return Err(invalidate(state, task_id, request_id).await),
            Some(_) => {}
        }
    }
    let task = amend_request(state, task_id, request_id, &|task, index| {
        let now = Utc::now();
        if task.agent_requests[index].state(task, None, now) != RequestState::Pending {
            return Err("The task changed while this was being decided.".to_string());
        }
        task.agent_requests[index].decision = Some(HumanDecision {
            authorized: authorize,
            decided_at: now,
            expires_at: (authorize && operation == RequestedOperation::OpenPr)
                .then(|| now + Duration::minutes(AUTHORIZATION_LIFETIME_MINUTES)),
        });
        if authorize && operation == RequestedOperation::Approve {
            task.human_review.approve(Some(commit.to_string()), now);
            task.human_review.pr_error = None;
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("The decision was not saved: {e}"))?;
    let request = view(&task, find(&task, request_id)?, None);
    Ok(DecisionOutcome { task, request })
}

/// Spend a person's authorization on opening the pull request it names.
///
/// The authorization is consumed durably before anything is pushed, so it is
/// used at most once whatever happens next: a failure, a lost reply or a crash
/// needs a new request and a new decision. A request left used with no outcome
/// means the outcome is unknown; the delivery state says what is on record.
///
/// `Err` means nothing was published by this call. A delivery that was started
/// and failed is `Ok`, with the failure as the request's outcome.
pub(crate) async fn open_authorized_pr(
    state: &crate::AppState,
    task_id: Uuid,
    request_id: Uuid,
    actor: &str,
) -> Result<AgentRequestView, String> {
    check_actor(actor)?;
    // Taken before the checks, as a desktop delivery does: an approval is not
    // renewed, and the Project is not deleted, under a delivery.
    let _in_flight = state.task.deliveries.start(task_id);
    let commit = {
        let _lease = crate::commands::project::lease_active_task(state, task_id).await?;
        let task = current(state, task_id).await?;
        let request = find(&task, request_id)?;
        if request.operation != RequestedOperation::OpenPr {
            return Err("This request does not authorize opening a pull request.".to_string());
        }
        check_usable(request, &task)?;
        let commit = request.commit.clone();
        match task_branch_tip(state, task_id).await?.as_deref() {
            None => return Err(NOT_READ.to_string()),
            Some(tip) if tip != commit => return Err(invalidate(state, task_id, request_id).await),
            Some(_) => {}
        }
        amend_request(state, task_id, request_id, &|task, index| {
            check_usable(&task.agent_requests[index], task)?;
            task.agent_requests[index].used = Some(RequestUse {
                executed_by: actor.to_string(),
                started_at: Utc::now(),
                outcome: None,
            });
            Ok(())
        })
        .await
        .map_err(|e| format!("Nothing was published: {e}"))?;
        commit
    };

    let outcome = match human_review::deliver(state, task_id, Some(&commit)).await {
        PrDelivery::Created { url } => UseOutcome::PrOpened { url },
        PrDelivery::Failed { reason } | PrDelivery::Unavailable { reason } => {
            UseOutcome::Failed { reason }
        }
    };
    let amend = |staged: &mut HashMap<Uuid, Task>| {
        let used = staged
            .get_mut(&task_id)
            .and_then(|t| t.agent_requests.iter_mut().find(|r| r.id == request_id))
            .and_then(|r| r.used.as_mut());
        if let Some(used) = used {
            used.outcome = Some(outcome.clone());
        }
    };
    crate::lifecycle::record(&state.task.tasks, &state.storage, task_id, &amend)
        .await
        .map_err(|e| format!("The delivery ended with {outcome:?}, but that was not saved: {e}"))?;
    let task = current(state, task_id).await?;
    Ok(view(&task, find(&task, request_id)?, None))
}

/// Refuses an authorization that cannot be spent, saying why.
fn check_usable(request: &AgentRequest, task: &Task) -> Result<(), String> {
    let why = match request.state(task, None, Utc::now()) {
        RequestState::Authorized => return Ok(()),
        RequestState::Pending => "A person has not authorized this request yet.",
        RequestState::Refused => "A person refused this request.",
        RequestState::Expired => "This request has expired.",
        RequestState::Used => {
            "This authorization was already used and cannot be used again. Read the delivery state \
             for what it did."
        }
        RequestState::Invalidated => {
            "The task or its branch changed, so this request is no longer valid."
        }
        RequestState::Approved => "This request does not authorize opening a pull request.",
    };
    Err(why.to_string())
}

/// The requests filed for a task, oldest first. Reads only.
pub(crate) async fn requests(
    state: &crate::AppState,
    task_id: Uuid,
) -> Result<Vec<AgentRequestView>, String> {
    let task = current(state, task_id).await?;
    if task.agent_requests.is_empty() {
        return Ok(Vec::new());
    }
    let tip = shown_tip(state, task_id).await;
    Ok(task.agent_requests.iter().map(|r| view(&task, r, tip.as_deref())).collect())
}

/// What an agent can know about a task's review. Reads only.
pub(crate) async fn review_state(
    state: &crate::AppState,
    task_id: Uuid,
) -> Result<serde_json::Value, String> {
    let approval = tokio::time::timeout(
        COVERAGE_READ_BOUND,
        human_review::approval_status(state, task_id),
    )
    .await
    .unwrap_or_else(|_| Err("Reading the branch took too long.".to_string()));
    // Read after the Git reads, so the record is not older than the answer.
    let task = current(state, task_id).await?;
    let (approval, approval_error) = match approval {
        Ok(approval) => (Some(approval), None),
        Err(e) => (None, Some(e)),
    };
    let tip = approval.as_ref().and_then(|a| a.branch_tip.as_deref());
    let requests: Vec<_> = task.agent_requests.iter().map(|r| view(&task, r, tip)).collect();
    Ok(serde_json::json!({
        "task_id": task.id,
        "title": task.title,
        "status": task.status,
        "branch": task.branch_name,
        "ai_review": task.qa_signoff,
        "approval": approval,
        "approval_error": approval_error,
        "decisions": task.human_review.entries,
        "requests": requests,
    }))
}

/// What an agent can know about a task's delivery, from what SlashIt holds.
/// Reads only, and asks nothing of Git or GitHub beyond the branch tip used to
/// judge open requests.
pub(crate) async fn delivery_state(
    state: &crate::AppState,
    task_id: Uuid,
) -> Result<serde_json::Value, String> {
    let requests: Vec<_> = requests(state, task_id)
        .await?
        .into_iter()
        .filter(|r| r.request.operation == RequestedOperation::OpenPr)
        .collect();
    let task = current(state, task_id).await?;
    let published = human_review::published_key(&task).and_then(Result::ok).map(|key| {
        human_review::published_head(state.pr_statuses.get(&key).as_ref(), Utc::now())
    });
    Ok(serde_json::json!({
        "task_id": task.id,
        "title": task.title,
        "status": task.status,
        "branch": task.branch_name,
        "approved_commit": task.human_review.approved_tip(),
        "pull_request": task.authoritative_pr_url(),
        "delivery_in_progress": state.task.deliveries.contains(task_id),
        "last_delivery_error": task.human_review.pr_error,
        "unconfirmed_pr_creation": task.pr_create_unconfirmed,
        "publication": task.pr_publication,
        "published_head": published,
        "local_integration": task.local_integration,
        "completed_locally": task.completed_locally(),
        "requests": requests,
    }))
}

#[tauri::command]
pub async fn list_agent_requests(
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Vec<AgentRequestView>, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    requests(&state, task_id).await
}

/// The only way a request is decided: a person, in the desktop app.
#[tauri::command]
pub async fn decide_agent_request(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    request_id: String,
    commit: String,
    authorize: bool,
) -> Result<DecisionOutcome, String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let request_id = Uuid::parse_str(&request_id).map_err(|e| e.to_string())?;
    crate::commands::project::refuse_if_task_project_removed(&state, task_id).await?;
    decide(&state, task_id, request_id, &commit, authorize).await
}
