// SPDX-License-Identifier: MPL-2.0
//! Task diffs. The legacy JJ IPC commands that lived here were removed: JJ
//! workspaces have no ownership model in SlashIt, so the status read and the
//! mutating commands were disabled. Task diffs read the Git worktree only.

use crate::commands::gui_admission::GuiMutation;
use uuid::Uuid;

/// A task with no worktree has no diff of its own.
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

/// The one canonical task diff, shared by both `get_task_diff` and
/// `get_task_diff_stat` -- neither IPC command computes its own range; both
/// read the same [`crate::worktree::task_diff`] result the AI reviewer uses.
async fn fetch_task_diff(
    state: &tauri::State<'_, crate::AppState>,
    task_uuid: Uuid,
) -> Result<crate::worktree::TaskDiff, String> {
    let (worktree_path, base_commit) = {
        let tasks = state.task.tasks.read().await;
        match tasks.get(&task_uuid) {
            Some(t) => (t.worktree_path.clone(), t.base_commit.clone()),
            None => (None, None),
        }
    };

    let Some(wt_path) = worktree_path else {
        return Err(no_worktree_diff(task_uuid));
    };

    crate::worktree::task_diff(&wt_path, base_commit.as_deref())
        .await
        .map_err(|e| match e {
            crate::worktree::TaskDiffError::UnknownBoundary => format!(
                "Task {task_uuid}'s diff boundary is unknown: it has no recorded starting \
                 commit, so there is nothing reliable to compare its worktree against."
            ),
            crate::worktree::TaskDiffError::Failed(msg) => {
                format!("Failed to compute the diff for task {task_uuid}: {msg}")
            }
        })
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
