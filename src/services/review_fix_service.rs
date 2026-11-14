// SPDX-License-Identifier: MPL-2.0
//! Asking the backend to resolve an unresolved AI Review fix. The backend checks
//! the Task Checkout itself; a refusal comes back as the error text.

use uuid::Uuid;

use crate::services::worker_recovery_service::invoke;

/// Clears the unresolved AI Review fix `attempt_id` of `task_id`, when the
/// backend finds the Task Checkout exactly as that attempt left it.
pub async fn resolve_review_fix(task_id: Uuid, attempt_id: Uuid) -> Result<(), String> {
    invoke(
        "resolve_review_fix",
        serde_json::json!({ "taskId": task_id.to_string(), "attemptId": attempt_id.to_string() }),
    )
    .await
}
