//! Runtime feature flags.
//!
//! Flags exist so work that is not finished can ship dark rather than sitting
//! on a long-lived branch. They are read from
//! [`AppPaths::feature_flags_file`](super::paths::AppPaths::feature_flags_file)
//! at startup and are deliberately forgiving: a missing, unreadable or
//! malformed file yields defaults with a warning, because failing to start
//! over a flag file would be worse than having no flags at all.

use super::paths::AppPaths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Flags recognised by this build.
///
/// Every flag defaults to `false`: a feature behind a flag is off unless the
/// user asked for it. Unknown keys are preserved in `extra` so that flipping a
/// flag from a newer build and then downgrading does not silently discard it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeatureFlags {
    /// Run the queue and IPC server without a window. Not implemented yet;
    /// the flag exists so the daemon work can land incrementally.
    pub daemon_mode: bool,

    /// Accept IPC connections from outside this machine.
    ///
    /// There is no implementation behind this and there deliberately will not
    /// be one until the model in `docs/architecture/ipc-security.md` is built.
    /// A remote `CreateTask` is a remote shell.
    pub remote_access: bool,

    /// Unrecognised keys, kept so a downgrade does not drop them.
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

impl FeatureFlags {
    /// Load flags, falling back to defaults on any problem.
    pub fn load(paths: &AppPaths) -> Self {
        Self::load_from(&paths.feature_flags_file())
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match toml::from_str(&contents) {
            Ok(flags) => flags,
            Err(e) => {
                eprintln!(
                    "[features] ignoring {}: {e}. Using defaults.",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn save(&self, paths: &AppPaths) -> std::io::Result<()> {
        let path = paths.feature_flags_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    }

    /// Look a flag up by name, for the frontend toggle.
    pub fn get(&self, name: &str) -> Option<bool> {
        match name {
            "daemon_mode" => Some(self.daemon_mode),
            "remote_access" => Some(self.remote_access),
            _ => None,
        }
    }

    /// Set a flag by name. Returns false when the name is not recognised, so
    /// a typo is reported rather than silently stored in `extra`.
    pub fn set(&mut self, name: &str, enabled: bool) -> bool {
        match name {
            "daemon_mode" => self.daemon_mode = enabled,
            "remote_access" => self.remote_access = enabled,
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_are_all_off() {
        let flags = FeatureFlags::default();
        assert!(!flags.daemon_mode);
        assert!(!flags.remote_access);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let tmp = TempDir::new().unwrap();
        let flags = FeatureFlags::load_from(&tmp.path().join("absent.toml"));
        assert_eq!(flags, FeatureFlags::default());
    }

    #[test]
    fn malformed_file_yields_defaults_instead_of_failing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("features.toml");
        std::fs::write(&path, "this is not = = toml").unwrap();
        assert_eq!(FeatureFlags::load_from(&path), FeatureFlags::default());
    }

    #[test]
    fn roundtrips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        );

        let mut flags = FeatureFlags::default();
        assert!(flags.set("daemon_mode", true));
        flags.save(&paths).unwrap();

        assert_eq!(FeatureFlags::load(&paths), flags);
    }

    #[test]
    fn unknown_flag_names_are_rejected() {
        let mut flags = FeatureFlags::default();
        assert!(!flags.set("nonexistent", true));
        assert_eq!(flags.get("nonexistent"), None);
    }

    #[test]
    fn unknown_keys_survive_a_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("features.toml");
        std::fs::write(&path, "daemon_mode = true\nfrom_a_newer_build = true\n").unwrap();

        let flags = FeatureFlags::load_from(&path);
        assert!(flags.daemon_mode);
        assert!(flags.extra.contains_key("from_a_newer_build"));
    }

    #[test]
    fn unknown_table_and_scalar_values_survive_a_save_and_reload_roundtrip() {
        // `unknown_keys_survive_a_roundtrip` above only exercises `load_from`
        // against a hand-written file. This drives the actual `save` path
        // too — the one a real downgrade-then-upgrade cycle goes through —
        // with both an unknown table value and an unknown scalar value, since
        // TOML's own syntax rules (scalars before table headers at the same
        // level) make table-valued entries the riskier case to get wrong.
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        );

        let mut flags = FeatureFlags::default();
        let mut table = toml::map::Map::new();
        table.insert(
            "nested".to_string(),
            toml::Value::String("value".to_string()),
        );
        table.insert("count".to_string(), toml::Value::Integer(3));
        flags
            .extra
            .insert("future_table_flag".to_string(), toml::Value::Table(table));
        flags
            .extra
            .insert("future_scalar_flag".to_string(), toml::Value::Boolean(true));

        flags.save(&paths).unwrap();
        let reloaded = FeatureFlags::load(&paths);

        assert_eq!(
            reloaded.extra.get("future_table_flag"),
            flags.extra.get("future_table_flag"),
            "an unknown table-valued key must survive a save/reload cycle unchanged"
        );
        assert_eq!(
            reloaded.extra.get("future_scalar_flag"),
            flags.extra.get("future_scalar_flag"),
            "an unknown scalar-valued key must survive a save/reload cycle unchanged"
        );
        assert_eq!(
            reloaded, flags,
            "a full save/reload roundtrip must not lose or alter any unknown key"
        );
    }
}
