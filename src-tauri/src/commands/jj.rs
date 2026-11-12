// SPDX-License-Identifier: MPL-2.0
//! Task diffs. The legacy JJ IPC commands that lived here were removed: JJ
//! workspaces have no ownership model in SlashIt, so the status read and the
//! mutating commands were disabled. Active Task diffs read the Git worktree;
//! a completed local Task reads its approved commit after checkout removal.

use crate::commands::gui_admission::GuiMutation;
use uuid::Uuid;

/// A task with no worktree and no completed local approval has no diff of its own.
///
/// This used to resolve the project's repository and diff that instead, so the
/// UI showed the user's own uncommitted work under the task's name -- a
/// finished task, whose `worktree_path` [`crate::lifecycle::terminalize`]
/// clears, showed whatever the user happened to be editing. Nothing here
/// writes, but a diff that belongs to someone else is still the wrong answer.
fn no_worktree_diff(task_id: Uuid) -> String {
    format!(
        "Task {task_id} has no worktree of its own, so there is no diff to show. Its commits are \
         still on its branch."
    )
}

/// The commit an approval would record, while the Task waits for its Human
/// decision in a checkout. `None` anywhere else, or when the branch tip cannot
/// be read (an approval is refused then too).
async fn review_tip(state: &crate::AppState, task: &crate::domain::Task) -> Option<String> {
    if task.status != crate::domain::TaskStatus::HumanReview || task.worktree_path.is_none() {
        return None;
    }
    crate::commands::integration::tip_to_approve(state, task).await
}

/// The Task's work up to `tip`, read from Git objects, so it is the same
/// content whatever the checkout holds.
async fn diff_to_tip(
    state: &crate::AppState,
    task: &crate::domain::Task,
    tip: &str,
) -> Result<crate::worktree::TaskDiff, crate::worktree::TaskDiffError> {
    let base = task.base_commit.as_deref().ok_or(crate::worktree::TaskDiffError::UnknownBoundary)?;
    let repo = crate::commands::integration::repo_path_of(state, task.project_id)
        .await
        .map_err(crate::worktree::TaskDiffError::Failed)?;
    crate::worktree::committed_task_diff(&repo, base, tip).await
}

fn describe(task_uuid: Uuid, e: crate::worktree::TaskDiffError) -> String {
    match e {
        crate::worktree::TaskDiffError::UnknownBoundary => format!(
            "Task {task_uuid}'s diff boundary is unknown: it has no recorded starting \
             commit, so there is nothing reliable to compare its worktree against."
        ),
        crate::worktree::TaskDiffError::Failed(msg) => {
            format!("Failed to compute the diff for task {task_uuid}: {msg}")
        }
    }
}

/// The one task diff answer shared by both IPC commands. While the Task owns a
/// checkout, use the canonical working-tree diff the AI reviewer sees, except
/// in Human Review, where the diff is the commit an approval would record: what
/// a person approves must be what they were shown. After local completion,
/// show only the commit that was approved.
pub(crate) async fn fetch_task_diff(
    state: &crate::AppState,
    task_uuid: Uuid,
) -> Result<crate::worktree::TaskDiff, String> {
    let task = {
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_uuid).cloned().ok_or("Task not found")?
    };

    let result = if let Some(wt_path) = task.worktree_path.as_deref() {
        match review_tip(state, &task).await {
            Some(tip) => diff_to_tip(state, &task, &tip).await,
            None => crate::worktree::task_diff(wt_path, task.base_commit.as_deref()).await,
        }
    } else if task.completed_locally() {
        let base = task.base_commit.as_deref().ok_or_else(|| {
            format!(
                "Task {task_uuid}'s diff boundary is unknown: it has no recorded starting commit."
            )
        })?;
        let approved = task.human_review.approved_tip().ok_or_else(|| {
            format!("Task {task_uuid} has no recorded approved commit to compare.")
        })?;
        let repo = crate::commands::integration::repo_path_of(state, task.project_id).await?;
        crate::worktree::committed_task_diff(&repo, base, approved).await
    } else {
        return Err(no_worktree_diff(task_uuid));
    };

    result.map_err(|e| describe(task_uuid, e))
}

/// What a person sees when deciding on a Task in Human Review. `patch` and
/// `stat` are the content of commit `tip`, which is what approving records.
/// `pending_*` are edits in the checkout that `tip` does not contain: they are
/// shown apart and are not part of the approval. Both are read against the
/// same `tip`, so they cannot describe different states.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReviewChanges {
    pub tip: String,
    pub patch: String,
    pub stat: String,
    pub pending_patch: String,
    pub pending_stat: String,
}

/// `None` unless the Task is waiting for its Human decision in a checkout.
pub(crate) async fn fetch_review_changes(
    state: &crate::AppState,
    task_uuid: Uuid,
) -> Result<Option<ReviewChanges>, String> {
    let task = {
        let tasks = state.task.tasks.read().await;
        tasks.get(&task_uuid).cloned().ok_or("Task not found")?
    };
    let (Some(tip), Some(wt_path)) = (review_tip(state, &task).await, task.worktree_path.as_deref()) else {
        return Ok(None);
    };
    let committed = diff_to_tip(state, &task, &tip).await.map_err(|e| describe(task_uuid, e))?;
    let pending = crate::worktree::pending_task_diff(wt_path, &tip).await.map_err(|e| describe(task_uuid, e))?;
    Ok(Some(ReviewChanges {
        tip,
        patch: committed.patch,
        stat: committed.stat,
        pending_patch: pending.patch,
        pending_stat: pending.stat,
    }))
}

#[tauri::command]
pub async fn get_task_review_changes(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<Option<ReviewChanges>, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    fetch_review_changes(&state, task_uuid).await
}

#[tauri::command]
pub async fn get_task_diff(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<String, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    Ok(fetch_task_diff(&state, task_uuid).await?.patch)
}

#[tauri::command]
pub async fn get_task_diff_stat(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<String, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    Ok(fetch_task_diff(&state, task_uuid).await?.stat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_ask_for_the_state_the_app_actually_manages() {
        // `run()` calls `.manage()` exactly once, with `AppState`. Tauri looks
        // `State<T>` up by `TypeId` in that map, so a command asking for a
        // sub-state type that only exists as a *field* of `AppState` is
        // rejected at invoke time. Reverting either command's signature to such
        // a type stops this file compiling.
        //
        // The helper is deliberately never awaited: `tauri::State` wraps a
        // private field and cannot be constructed outside a running app, so the
        // type check *is* the assertion.
        async fn binds_to_app_state(state: tauri::State<'_, crate::AppState>, change_id: String) {
            let _ = get_task_diff(GuiMutation::untracked(), state.clone(), change_id.clone()).await;
            let _ = get_task_diff_stat(GuiMutation::untracked(), state, change_id).await;
        }

        let _ = &binds_to_app_state;
    }
}
