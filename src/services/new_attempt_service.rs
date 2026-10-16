//! Starting a new coding attempt after an interrupted Worker, building on its
//! Task Checkout as it stands: the read that shows the evidence, the decision
//! that authorizes it, and the start that uses it.

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::models::Task;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

async fn invoke<T: for<'de> Deserialize<'de>>(cmd: &str, args: serde_json::Value) -> Result<T, String> {
    let args = serde_wasm_bindgen::to_value(&args).map_err(|e| e.to_string())?;
    let response = JsFuture::from(raw_invoke(cmd, args)).await.map_err(|e| {
        e.as_string().unwrap_or_else(|| {
            js_sys::JSON::stringify(&e)
                .ok()
                .and_then(|s| s.as_string())
                .unwrap_or_else(|| format!("{cmd} failed"))
        })
    })?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

/// Mirrors the backend `NewAttemptEvidence`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NewAttemptEvidence {
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub repository_id: Option<Uuid>,
    pub run: Option<u32>,
    pub recovery_reason: Option<String>,
    pub observed_commit: Option<String>,
    pub branch: Option<String>,
    pub checkout: Option<String>,
    pub expected_head: Option<String>,
    pub dirty_fingerprint: Option<String>,
    pub dirty_paths: Vec<String>,
    pub eligible: bool,
    pub refusals: Vec<String>,
}

/// Mirrors the backend `AuthorizeNewAttemptRequest`. Every fact is a
/// compare-and-set expectation: the decision applies only if it still holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthorizeNewAttemptRequest {
    pub decision_id: Uuid,
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub run: u32,
    pub stage: String,
    pub recovery_reason: String,
    pub observed_commit: Option<String>,
    pub branch: String,
    pub checkout: String,
    pub expected_head: String,
    pub dirty_fingerprint: String,
}

impl AuthorizeNewAttemptRequest {
    /// The request that authorizes exactly what `evidence` shows, or `None`
    /// when the snapshot lacks a fact the decision must be bound to. The
    /// `decision_id` is made by the caller once per confirmation.
    pub fn for_evidence(evidence: &NewAttemptEvidence, decision_id: Uuid) -> Option<Self> {
        Some(Self {
            decision_id,
            task_id: evidence.task_id,
            project_id: evidence.project_id,
            repository_id: evidence.repository_id?,
            run: evidence.run?,
            stage: "recovery_required".into(),
            recovery_reason: evidence.recovery_reason.clone()?,
            observed_commit: evidence.observed_commit.clone(),
            branch: evidence.branch.clone()?,
            checkout: evidence.checkout.clone()?,
            expected_head: evidence.expected_head.clone()?,
            dirty_fingerprint: evidence.dirty_fingerprint.clone()?,
        })
    }
}

/// Mirrors the backend `AuthorizeNewAttemptOutcome`. The start is what changes
/// the Task on screen, so the authorized outcomes carry nothing the drawer uses.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthorizeNewAttemptOutcome {
    Authorized {},
    AlreadyAuthorized {},
    Refused { reason: String },
    DurabilityUnconfirmed { task: Task, detail: String },
}

/// Mirrors the backend `StartNewAttemptRequest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartNewAttemptRequest {
    pub task_id: Uuid,
    pub decision_id: Uuid,
}

pub async fn get_new_attempt_evidence(task_id: String) -> Result<NewAttemptEvidence, String> {
    invoke("get_new_attempt_evidence", serde_json::json!({ "taskId": task_id })).await
}

pub async fn authorize_new_attempt(
    request: AuthorizeNewAttemptRequest,
) -> Result<AuthorizeNewAttemptOutcome, String> {
    invoke("authorize_new_attempt", serde_json::json!({ "request": request })).await
}

pub async fn start_new_attempt(request: StartNewAttemptRequest) -> Result<Task, String> {
    invoke("start_new_attempt", serde_json::json!({ "request": request })).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence() -> NewAttemptEvidence {
        NewAttemptEvidence {
            task_id: Uuid::from_u128(1),
            project_id: Uuid::from_u128(2),
            repository_id: Some(Uuid::from_u128(3)),
            run: Some(2),
            recovery_reason: Some("interrupted".into()),
            observed_commit: None,
            branch: Some("task/one".into()),
            checkout: Some("/work/one".into()),
            expected_head: Some("b".repeat(40)),
            dirty_fingerprint: Some("sha256:abc".into()),
            dirty_paths: vec!["notes.txt".into()],
            eligible: true,
            refusals: Vec::new(),
        }
    }

    #[test]
    fn the_request_binds_every_fact_the_person_was_shown() {
        let id = Uuid::from_u128(9);
        let request = AuthorizeNewAttemptRequest::for_evidence(&evidence(), id).unwrap();
        assert_eq!(request.decision_id, id);
        assert_eq!(request.run, 2);
        assert_eq!(request.recovery_reason, "interrupted");
        assert_eq!(request.expected_head, "b".repeat(40));
        assert_eq!(request.dirty_fingerprint, "sha256:abc");
        assert_eq!(request.checkout, "/work/one");
    }

    #[test]
    fn evidence_missing_a_bound_fact_cannot_become_a_decision() {
        let id = Uuid::from_u128(9);
        let mut no_fingerprint = evidence();
        no_fingerprint.dirty_fingerprint = None;
        assert!(AuthorizeNewAttemptRequest::for_evidence(&no_fingerprint, id).is_none());
        let mut no_head = evidence();
        no_head.expected_head = None;
        assert!(AuthorizeNewAttemptRequest::for_evidence(&no_head, id).is_none());
    }
}
