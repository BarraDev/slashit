//! Commands for reading and toggling runtime feature flags.

use crate::config::features::FeatureFlags;
use crate::config::paths::AppPaths;

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
    set_feature_flag_on(&state.features, &state.paths, &name, enabled).await
}

/// The logic behind `set_feature_flag`, factored out so it can be exercised
/// without a `tauri::State`/`AppState`.
///
/// The previous value is captured before mutating the shared flags so that,
/// if persisting fails, the in-memory value can be rolled back before
/// returning the error. Without this, a failed `save` would leave the shared
/// `RwLock` holding a value that disk never actually recorded, and every
/// subsequent read of `state.features` would report a setting that a restart
/// would silently revert.
async fn set_feature_flag_on(
    features: &tokio::sync::RwLock<FeatureFlags>,
    paths: &AppPaths,
    name: &str,
    enabled: bool,
) -> Result<FeatureFlags, String> {
    let mut flags = features.write().await;
    let previous = flags.clone();

    if !flags.set(name, enabled) {
        return Err(format!("Unknown feature flag '{name}'"));
    }

    if let Err(e) = flags.save(paths) {
        *flags = previous;
        return Err(e.to_string());
    }

    Ok(flags.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::sync::RwLock;

    /// `AppPaths` whose config dir cannot possibly be created, so any save
    /// through it fails: `create_dir_all` is asked to create a directory at a
    /// path where a plain file already sits, which is guaranteed to error on
    /// every platform without needing platform-specific permission tricks.
    fn paths_that_cannot_be_saved_to(temp: &TempDir) -> AppPaths {
        let blocking_file = temp.path().join("config");
        std::fs::write(&blocking_file, b"not a directory").unwrap();

        AppPaths::with_roots(
            blocking_file,
            temp.path().join("data"),
            temp.path().join("cache"),
            temp.path().join("runtime"),
        )
    }

    #[tokio::test]
    async fn failed_save_restores_previous_in_memory_value() {
        let temp = TempDir::new().unwrap();
        let paths = paths_that_cannot_be_saved_to(&temp);
        let features = RwLock::new(FeatureFlags::default());

        assert!(!features.read().await.daemon_mode);

        let result = set_feature_flag_on(&features, &paths, "daemon_mode", true).await;

        assert!(result.is_err(), "save should fail against an unwritable config dir");
        assert!(
            !features.read().await.daemon_mode,
            "a failed save must not leave the in-memory flag diverged from disk"
        );
    }

    #[tokio::test]
    async fn successful_save_keeps_the_new_value() {
        let temp = TempDir::new().unwrap();
        let paths = AppPaths::with_roots(
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("cache"),
            temp.path().join("runtime"),
        );
        let features = RwLock::new(FeatureFlags::default());

        let result = set_feature_flag_on(&features, &paths, "daemon_mode", true).await;

        assert!(result.is_ok());
        assert!(features.read().await.daemon_mode);
        assert!(FeatureFlags::load(&paths).daemon_mode);
    }

    #[tokio::test]
    async fn unknown_flag_name_leaves_value_untouched() {
        let temp = TempDir::new().unwrap();
        let paths = AppPaths::with_roots(
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("cache"),
            temp.path().join("runtime"),
        );
        let features = RwLock::new(FeatureFlags::default());

        let result = set_feature_flag_on(&features, &paths, "nonexistent", true).await;

        assert!(result.is_err());
        assert_eq!(*features.read().await, FeatureFlags::default());
    }
}
