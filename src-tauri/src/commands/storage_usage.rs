//! How much disk SlashIt uses. Read-only: see [`crate::storage_accounting`].

use crate::domain::storage_usage::StorageStatus;

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
