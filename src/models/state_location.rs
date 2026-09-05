//! Mirrors `src-tauri/src/config/paths.rs` and `src-tauri/src/config/migration.rs`.
//!
//! The serde attributes here must stay identical to the backend's. Both enums
//! carry `rename_all = "snake_case"` there, so `InProject` travels as
//! `"in_project"`; getting that wrong fails silently at decode time rather
//! than at compile time.

use serde::{Deserialize, Serialize};

/// Where a project's shareable state (its board) is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateLocation {
    /// Outside the project, under the OS data directory.
    #[default]
    External,
    /// Inside the project at `<root>/.slashit/`.
    InProject,
    /// `InProject` when a populated `.slashit/` already exists, else `External`.
    Auto,
}

impl StateLocation {
    pub fn label(self) -> &'static str {
        match self {
            Self::External => "Outside the project",
            Self::InProject => "Inside the project",
            Self::Auto => "Detect automatically",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::External => {
                "Your repository stays exactly as it was. SlashIt keeps the board in its own \
                 folder. Recommended."
            }
            Self::InProject => {
                "The board is written to .slashit/ inside the repository, so it can be committed \
                 and shared with your team."
            }
            Self::Auto => {
                "Use .slashit/ if the project already has one, otherwise keep the board outside \
                 the project."
            }
        }
    }
}

/// A [`StateLocation`] with `Auto` already resolved against the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedLocation {
    External,
    InProject,
}

/// What to do when both locations hold data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Refuse and report the conflicting paths.
    #[default]
    Abort,
    PreferSource,
    PreferDestination,
}

/// Everything needed to describe a project's storage in settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateLocationInfo {
    pub project_id: String,
    pub location: StateLocation,
    pub resolved: ResolvedLocation,
    pub project_root: String,
    pub current_dir: String,
    pub external_dir: String,
    pub in_project_dir: String,
    pub can_choose: bool,
    /// RFC3339 timestamp; pass back as `known_updated_at` to `set_state_location`
    /// so a decision based on a stale read is rejected rather than applied.
    pub updated_at: String,
}

/// A read-only preview of a proposed migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub from: String,
    pub to: String,
    pub source_exists: bool,
    pub source_file_count: usize,
    pub source_bytes: u64,
    pub destination_exists: bool,
    pub destination_file_count: usize,
    pub conflicts: Vec<String>,
    pub requires_resolution: bool,
}

/// What actually happened. Mirrors the backend's internally tagged enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MigrationOutcome {
    NothingToDo,
    Migrated { files: usize, bytes: u64 },
    KeptDestination { archived_source: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationReport {
    pub outcome: MigrationOutcome,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Render a byte count for the migration preview.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
