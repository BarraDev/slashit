use crate::jj::JjManager;
use std::sync::Arc;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone)]
pub struct JjState {
    jj_manager: Arc<JjManager>,
}

impl JjState {
    pub fn new() -> Self {
        Self {
            jj_manager: Arc::new(JjManager::new()),
        }
    }
}

impl Default for JjState {
    fn default() -> Self {
        Self::new()
    }
}

#[tauri::command]
pub async fn new_change(
    state: tauri::State<'_, crate::AppState>,
    workspace_path: String,
    description: String,
) -> Result<String, String> {
    state
        .jj
        .jj_manager
        .new_change(PathBuf::from(&workspace_path).as_path(), &description)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn describe_change(
    state: tauri::State<'_, crate::AppState>,
    workspace_path: String,
    change_id: String,
    description: String,
) -> Result<(), String> {
    state
        .jj
        .jj_manager
        .describe_change(PathBuf::from(&workspace_path).as_path(), &change_id, &description)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn abandon_change(
    state: tauri::State<'_, crate::AppState>,
    workspace_path: String,
    change_id: String,
) -> Result<(), String> {
    state
        .jj
        .jj_manager
        .abandon_change(PathBuf::from(&workspace_path).as_path(), &change_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn jj_get_workspace_status(
    state: tauri::State<'_, crate::AppState>,
    workspace_path: String,
) -> Result<crate::jj::JjStatus, String> {
    state
        .jj
        .jj_manager
        .get_status(PathBuf::from(&workspace_path).as_path())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn git_export(
    state: tauri::State<'_, crate::AppState>,
    workspace_path: String,
) -> Result<(), String> {
    state
        .jj
        .jj_manager
        .git_export(PathBuf::from(&workspace_path).as_path())
        .map_err(|e| e.to_string())
}

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
    state: tauri::State<'_, crate::AppState>,
    task_id: String,
) -> Result<String, String> {
    let task_uuid = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;
    Ok(fetch_task_diff(&state, task_uuid).await?.patch)
}

#[tauri::command]
pub async fn get_task_diff_stat(
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
        // `State<T>` up by `TypeId` in that map, so a command asking for
        // `State<JjState>` — a type that only ever exists as a *field* of
        // `AppState` — is rejected at invoke time with "state not managed for
        // field ... You must call `.manage()` before using this command".
        // `jj_get_workspace_status` backs the JjStatus component, so the panel
        // could never load.
        //
        // The helper is deliberately never awaited: `tauri::State` wraps a
        // private field and cannot be constructed outside a running app, so the
        // type check *is* the assertion. Reverting any signature to the
        // sub-state stops this file compiling.
        async fn binds_to_app_state(
            state: tauri::State<'_, crate::AppState>,
            workspace_path: String,
            change_id: String,
            description: String,
        ) {
            let _ = new_change(state.clone(), workspace_path.clone(), description.clone()).await;
            let _ = describe_change(
                state.clone(),
                workspace_path.clone(),
                change_id.clone(),
                description,
            )
            .await;
            let _ = abandon_change(state.clone(), workspace_path.clone(), change_id.clone()).await;
            let _ = jj_get_workspace_status(state.clone(), workspace_path.clone()).await;
            let _ = git_export(state.clone(), workspace_path).await;
            let _ = get_task_diff(state.clone(), change_id.clone()).await;
            let _ = get_task_diff_stat(state, change_id).await;
        }

        let _ = &binds_to_app_state;
    }
}
