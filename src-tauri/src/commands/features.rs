//! Commands for reading and toggling runtime feature flags.

use crate::config::features::FeatureFlags;

#[tauri::command]
pub async fn get_feature_flags(
    state: tauri::State<'_, crate::AppState>,
) -> Result<FeatureFlags, String> {
    Ok(state.features.read().await.clone())
}

/// Toggle one flag by name and persist the whole set.
///
/// An unrecognised name is an error rather than a silent no-op, so a typo in
/// the UI or a stale caller is visible immediately.
#[tauri::command]
pub async fn set_feature_flag(
    state: tauri::State<'_, crate::AppState>,
    name: String,
    enabled: bool,
) -> Result<FeatureFlags, String> {
    let mut flags = state.features.write().await;
    if !flags.set(&name, enabled) {
        return Err(format!("Unknown feature flag '{name}'"));
    }
    flags.save(&state.paths).map_err(|e| e.to_string())?;
    Ok(flags.clone())
}
