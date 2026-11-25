// SPDX-License-Identifier: MPL-2.0
//! The update channel the person has chosen: `stable` or `prerelease`.
//!
//! This is a preference, not a feature flag, so it lives in its own file
//! beside `features.toml` and is written the same way. A missing file means
//! `stable`, which is also what a file written before this preference existed
//! gets, so existing installs keep the channel they had. A file that cannot be
//! read, or that names an unknown channel, also reads as `stable`: the safe
//! choice is the one that never offers a prerelease.
//!
//! The installed build's own channel is a different fact. It is fixed at build
//! time (see `release_channel`) and is never read from this file.

use super::paths::AppPaths;
use crate::config::ConfigWriteError;
use crate::release_channel::ReleaseChannel;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdatePreferences {
    pub channel: ReleaseChannel,
}

impl Default for UpdatePreferences {
    fn default() -> Self {
        Self {
            channel: ReleaseChannel::Stable,
        }
    }
}

/// The file's shape. The channel is stored as its spelling, so the file stays
/// readable and an unknown spelling is caught at load rather than at parse.
#[derive(Debug, Serialize, Deserialize)]
struct OnDisk {
    channel: String,
}

impl UpdatePreferences {
    /// Load the preference, falling back to `stable` on any problem.
    pub fn load(paths: &AppPaths) -> Self {
        Self::load_from(&paths.update_preferences_file())
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let stored: OnDisk = match toml::from_str(&contents) {
            Ok(stored) => stored,
            Err(error) => {
                eprintln!(
                    "[updates] ignoring {}: {error}. Using the stable channel.",
                    path.display()
                );
                return Self::default();
            }
        };
        match ReleaseChannel::parse(&stored.channel) {
            Some(channel) => Self { channel },
            None => {
                eprintln!(
                    "[updates] ignoring {}: unknown channel {:?}. Using the stable channel.",
                    path.display(),
                    stored.channel
                );
                Self::default()
            }
        }
    }

    /// Write the preference atomically. A failure leaves the previous file in
    /// place and is reported, never swallowed.
    pub fn save(&self, paths: &AppPaths) -> Result<(), ConfigWriteError> {
        let path = paths.update_preferences_file();
        let stored = OnDisk {
            channel: self.channel.as_str().to_string(),
        };
        let text = toml::to_string_pretty(&stored)
            .map_err(|e| ConfigWriteError::NotPublished(anyhow::Error::new(e)))?;
        slashit_durable_fs::write_atomic(&path, text.as_bytes(), slashit_durable_fs::Permissions::Preserve)
            .map(|_| ())
            .map_err(|e| {
                let published = e.is_published();
                let error = anyhow::Error::new(e).context("Failed to write the update channel");
                if published {
                    ConfigWriteError::PublishedNotDurable(error)
                } else {
                    ConfigWriteError::NotPublished(error)
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_first_launch_with_no_file_is_stable() {
        let tmp = TempDir::new().unwrap();
        let prefs = UpdatePreferences::load_from(&tmp.path().join("updates.toml"));
        assert_eq!(prefs.channel, ReleaseChannel::Stable);
    }

    #[test]
    fn a_malformed_or_unknown_file_reads_as_stable() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("updates.toml");
        for text in ["channel = \"beta\"\n", "channel = [\n", "channel = \"Prerelease\"\n"] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(UpdatePreferences::load_from(&path).channel, ReleaseChannel::Stable, "{text:?}");
        }
    }

    #[test]
    fn the_prerelease_choice_survives_a_restart() {
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        );
        UpdatePreferences {
            channel: ReleaseChannel::Prerelease,
        }
        .save(&paths)
        .unwrap();
        assert_eq!(UpdatePreferences::load(&paths).channel, ReleaseChannel::Prerelease);
        UpdatePreferences::default().save(&paths).unwrap();
        assert_eq!(UpdatePreferences::load(&paths).channel, ReleaseChannel::Stable);
    }
}
