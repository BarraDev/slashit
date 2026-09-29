//! How much disk SlashIt uses. Read-only: see [`crate::storage_accounting`].

use crate::domain::storage_usage::{StartBlock, StorageStatus};

/// The latest measurement, without measuring.
#[tauri::command]
pub async fn get_storage_usage(
    state: tauri::State<'_, crate::AppState>,
) -> Result<StorageStatus, String> {
    Ok(state.storage_accounting.status())
}

/// Measure now, or wait for the measurement already running, and return
/// the result.
#[tauri::command]
pub async fn refresh_storage_usage(
    state: tauri::State<'_, crate::AppState>,
) -> Result<StorageStatus, String> {
    Ok(state.storage_accounting.refresh().await)
}

/// Why new task executions are paused right now, or `None` when they are
/// not. Asks the filesystem afresh; it does not read the last measurement.
#[tauri::command]
pub async fn get_new_work_pause(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Option<StartBlock>, String> {
    Ok(state.start_guard.check().await.err())
}
