// SPDX-License-Identifier: MPL-2.0
//! The person's exit from an unresolved AI Review fix. The record and its rules
//! live in `review_fix_residue`; this only parses the request.

use uuid::Uuid;

use crate::commands::gui_admission::GuiMutation;

/// Clears the unresolved AI Review fix `attempt_id` of `task_id`, once the Task
/// Checkout is exactly as the attempt left it. Nothing is committed, approved or
/// removed, and the Task's Human Review status is unchanged.
#[tauri::command]
pub async fn resolve_review_fix(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
    attempt_id: String,
) -> Result<(), String> {
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    let attempt_id = Uuid::parse_str(&attempt_id).map_err(|e| e.to_string())?;
    let executor = state.executor.get().cloned();
    crate::review_fix_residue::resolve(
        &state.task_lifecycle_locks,
        &state.task.tasks,
        &state.storage,
        task_id,
        attempt_id,
        executor.as_deref(),
    )
    .await
}
