//! Commands for reading and toggling runtime feature flags.

use crate::config::features::FeatureFlags;
use crate::config::paths::AppPaths;

#[tauri::command]
pub async fn get_feature_flags(
    state: tauri::State<'_, crate::AppState>,
) -> Result<FeatureFlags, String> {
    Ok(state.features.read().await.clone())
}

/// Toggle one flag by name, persist it, and re-resolve what is in force.
///
/// An unrecognised name is an error rather than a silent no-op, so a typo in
/// the UI or a stale caller is visible immediately.
///
/// The persisted file is loaded, edited and saved on its own rather than
/// writing back the in-memory set. `AppState.features` is the *resolved* set,
/// which may carry environment overrides; saving that would silently bake a
/// temporary override into the user's configuration. After the write, the
/// resolved set is rebuilt so behaviour follows the new configuration without
/// a restart — and so a flag the environment is pinning stays pinned rather
/// than appearing to accept the toggle.
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
/// Edits and saves a freshly loaded *persisted* copy, never the shared
/// resolved set directly: `flags` (the `RwLock` guard) may currently hold a
/// value an environment variable is overriding, and `save`ing that would bake
/// the temporary override into `features.toml` on the next unrelated toggle.
/// The write guard is still taken up front and held across load, edit, save
/// and re-resolve, so two concurrent toggles cannot each load the same
/// on-disk file and have the second save silently discard the first's
/// change. Nothing touches `*flags` until after `save` has already
/// succeeded, so a failed save leaves the shared value exactly as it was —
/// there is no separate rollback to get right.
async fn set_feature_flag_on(
    features: &tokio::sync::RwLock<FeatureFlags>,
    paths: &AppPaths,
    name: &str,
    enabled: bool,
) -> Result<FeatureFlags, String> {
    let mut flags = features.write().await;

    let mut persisted = FeatureFlags::load(paths);
    if !persisted.set(name, enabled) {
        return Err(format!("Unknown feature flag '{name}'"));
    }
    persisted.save(paths).map_err(|e| e.to_string())?;

    // Rebuilt from what is now on disk, not assigned `persisted` directly, so
    // a flag the environment is pinning stays pinned rather than appearing to
    // accept the toggle.
    *flags = crate::config::features::resolve_startup_flags(paths);
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
    async fn toggling_one_flag_does_not_bake_an_env_override_of_another_into_disk() {
        // Regression guard: `state.features` is the *resolved* set, which can
        // carry a temporary environment override. Toggling an unrelated flag
        // must not write that override into `features.toml` -- doing so would
        // silently turn a session-only override into permanent configuration,
        // including for a flag like `remote_access` whose whole point is that
        // it should require deliberate, persistent opt-in.
        let _guard = crate::config::paths::ENV_LOCK.lock().await;

        // `remote_access` is the sharpest example: it gates accepting IPC
        // connections from outside the machine, so silently persisting a
        // temporary override would turn on a remote-code-execution surface
        // the operator never asked to keep.
        let key = crate::config::features::env_var_for("remote_access");
        let previous = std::env::var(&key).ok();
        // SAFETY: the lock above makes this the only thread touching the
        // environment for the duration of this test; restored before it drops.
        unsafe { std::env::set_var(&key, "yes") };

        let temp = TempDir::new().unwrap();
        let paths = AppPaths::with_roots(
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("cache"),
            temp.path().join("runtime"),
        );
        // What `build_state_with_paths` would actually hand `set_feature_flag`:
        // the environment-resolved set, not the empty persisted default.
        let features = RwLock::new(crate::config::features::resolve_startup_flags(&paths));
        assert!(
            features.read().await.remote_access,
            "the environment override should already be in force"
        );

        let result = set_feature_flag_on(&features, &paths, "auto_update", true).await;

        // SAFETY: as above.
        match &previous {
            Some(v) => unsafe { std::env::set_var(&key, v) },
            None => unsafe { std::env::remove_var(&key) },
        }
        let result = result.expect("toggling a valid, unrelated flag must succeed");

        assert!(
            !FeatureFlags::load(&paths).remote_access,
            "the environment-only override must not have been written to disk"
        );
        assert!(
            result.remote_access,
            "the re-resolved set must still reflect the (still-set, at read time) \
             environment override"
        );
        assert!(result.auto_update, "the actually-requested toggle must still apply");
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

/// Every flag with the value in force and the layer that decided it.
///
/// [`get_feature_flags`] returns what is persisted, which is only one of the
/// four layers; a user whose flag is being overridden by the environment needs
/// to be told that rather than shown a toggle that appears to disagree with the
/// application's behaviour. Same shape as the `slashit features` IPC reply, so
/// both surfaces report the same thing.
#[tauri::command]
pub async fn describe_feature_flags(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<slashit_ipc::FeatureFlagInfo>, String> {
    Ok(state.features.read().await.diagnostics())
}
