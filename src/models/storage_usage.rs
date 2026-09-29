//! Mirrors `src-tauri/src/domain/storage_usage.rs`. What each total means is
//! documented there.

use serde::{Deserialize, Serialize};

use super::task::TaskStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageClassification {
    WorkspaceSource,
    Rebuildable,
    Temporary,
    AppData,
    Unknown,
}

impl StorageClassification {
    pub fn label(self) -> &'static str {
        match self {
            Self::WorkspaceSource => "Task files",
            Self::Rebuildable => "Rebuildable",
            Self::Temporary => "Temporary",
            Self::AppData => "App data",
            Self::Unknown => "Unrecognized",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerKind {
    TaskCheckout,
    BuildOutput,
    ProjectState,
    Configuration,
    Logs,
    TerminalSessions,
    Cache,
    Runtime,
    Unrecognized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum CheckoutLifecycle {
    Task { status: TaskStatus, agent_attached: bool },
    CleanupInterrupted { status: TaskStatus },
    Unrecorded,
    Contested,
    Misplaced,
}

impl CheckoutLifecycle {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Task { agent_attached: true, .. } => "Agent working",
            Self::Task { status, .. } => match status {
                TaskStatus::Backlog => "Backlog",
                TaskStatus::Queue => "Queued",
                TaskStatus::InProgress => "In progress",
                TaskStatus::AiReview => "AI review",
                TaskStatus::HumanReview => "Human review",
                TaskStatus::PrCreated => "PR created",
                TaskStatus::Error => "Failed",
                TaskStatus::Done => "Done",
            },
            Self::CleanupInterrupted { .. } => "Cleanup interrupted",
            Self::Unrecorded => "No task records it",
            Self::Contested => "Recorded by several tasks",
            Self::Misplaced => "Not where SlashIt places this task's checkouts",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Measurement {
    Complete,
    Partial { skipped_entries: u64, reason: String },
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageConsumer {
    pub kind: ConsumerKind,
    pub label: String,
    pub detail: Option<String>,
    pub bytes: Option<u64>,
    pub classification: StorageClassification,
    pub lifecycle: Option<CheckoutLifecycle>,
    pub reclaimable: bool,
    pub measurement: Measurement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemSpace {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskPressure {
    Normal,
    Warning,
    Critical,
}

impl DiskPressure {
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Warning => "Warning",
            Self::Critical => "Critical",
        }
    }
}

/// Why SlashIt is not beginning new task executions right now. Mirrors the
/// backend's `domain::storage_usage::StartBlock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StartBlock {
    CriticalDisk {
        available_bytes: u64,
        critical_below_bytes: u64,
    },
    DiskSpaceUnavailable {
        reason: String,
    },
}

impl StartBlock {
    pub fn headline(&self) -> &'static str {
        match self {
            Self::CriticalDisk { .. } => "New work paused: critically low disk space",
            Self::DiskSpaceUnavailable { .. } => "New work paused: SlashIt couldn't verify free disk space",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Self::CriticalDisk { available_bytes, critical_below_bytes } => format!(
                "{} free. New tasks can start again once {} is free; running tasks carry on. \
                 Settings > Storage shows what SlashIt uses.",
                iec_bytes(*available_bytes),
                iec_bytes(*critical_below_bytes),
            ),
            Self::DiskSpaceUnavailable { reason } => format!(
                "{reason}. New tasks can start again once the check succeeds; running tasks carry on."
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PressureThresholds {
    pub warning_below_bytes: u64,
    pub critical_below_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageSummary {
    pub measured_at: chrono::DateTime<chrono::Utc>,
    pub duration_ms: u64,
    pub filesystem: Option<FilesystemSpace>,
    pub filesystem_error: Option<String>,
    pub pressure: Option<DiskPressure>,
    pub thresholds: Option<PressureThresholds>,
    pub slashit_owned_bytes: u64,
    pub active_workspace_bytes: u64,
    pub rebuildable_bytes: u64,
    pub reclaimable_bytes: u64,
    pub unknown_managed_bytes: u64,
    pub incomplete: bool,
    pub links_not_followed: u64,
    pub mounts_not_entered: u64,
    pub external_checkouts: u32,
    pub largest_consumers: Vec<StorageConsumer>,
    pub unmeasured: Vec<StorageConsumer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageStatus {
    pub summary: Option<StorageSummary>,
    pub measuring: bool,
    pub last_error: Option<String>,
}

/// Bytes in binary units, which is what the numbers are: "269 GiB".
pub fn iec_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    // Judged on the value as it will be printed, so rounding never shows
    // "1024 KiB" where "1.0 MiB" belongs.
    let shown = |value: f64| if value >= 100.0 { value.round() } else { (value * 10.0).round() / 10.0 };
    while shown(value) >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    // Whole numbers once they are large enough that a decimal is noise.
    if shown(value) >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_read_in_binary_units() {
        assert_eq!(iec_bytes(0), "0 B");
        assert_eq!(iec_bytes(1023), "1023 B");
        assert_eq!(iec_bytes(1024), "1.0 KiB");
        assert_eq!(iec_bytes(1536 * 1024), "1.5 MiB");
        assert_eq!(iec_bytes(269 * 1024 * 1024 * 1024), "269 GiB");
        assert_eq!(iec_bytes(2 * 1024 * 1024 * 1024 * 1024), "2.0 TiB");
        assert_eq!(iec_bytes(u64::MAX), "16384 PiB");
        // Just below a unit boundary rounds up into the next unit.
        assert_eq!(iec_bytes(1024 * 1024 - 1), "1.0 MiB");
        assert_eq!(iec_bytes(1023 * 1024 + 1000), "1.0 MiB");
        assert_eq!(iec_bytes(99_950), "97.6 KiB");
        assert_eq!(iec_bytes(102_350), "100 KiB");
    }

    #[test]
    fn the_backend_shape_deserializes() {
        let json = serde_json::json!({
            "summary": {
                "measured_at": "2026-09-28T12:00:00Z",
                "duration_ms": 12,
                "filesystem": { "total_bytes": 100, "available_bytes": 50 },
                "filesystem_error": null,
                "pressure": "warning",
                "thresholds": { "warning_below_bytes": 60, "critical_below_bytes": 20 },
                "slashit_owned_bytes": 10,
                "active_workspace_bytes": 5,
                "rebuildable_bytes": 3,
                "reclaimable_bytes": 2,
                "unknown_managed_bytes": 1,
                "incomplete": true,
                "links_not_followed": 4,
                "mounts_not_entered": 0,
                "external_checkouts": 1,
                "largest_consumers": [{
                    "kind": "build_output",
                    "label": "Fix login",
                    "detail": "target/ build output",
                    "bytes": 3,
                    "classification": "rebuildable",
                    "lifecycle": { "state": "task", "status": "human_review", "agent_attached": false },
                    "reclaimable": true,
                    "measurement": { "state": "partial", "skipped_entries": 1, "reason": "x" }
                }],
                "unmeasured": [{
                    "kind": "logs",
                    "label": "Logs",
                    "detail": null,
                    "bytes": null,
                    "classification": "app_data",
                    "lifecycle": null,
                    "reclaimable": false,
                    "measurement": { "state": "failed", "reason": "denied" }
                }]
            },
            "measuring": false,
            "last_error": null
        });
        let status: StorageStatus = serde_json::from_value(json).unwrap();
        let summary = status.summary.unwrap();
        assert_eq!(summary.pressure, Some(DiskPressure::Warning));
        assert_eq!(
            summary.largest_consumers[0].lifecycle,
            Some(CheckoutLifecycle::Task { status: TaskStatus::HumanReview, agent_attached: false })
        );
        assert_eq!(summary.unmeasured[0].bytes, None);
    }

    #[test]
    fn a_start_block_reads_the_backend_shape_and_explains_itself() {
        let critical: StartBlock = serde_json::from_value(serde_json::json!({
            "kind": "critical_disk",
            "available_bytes": 12u64 * 1024 * 1024 * 1024,
            "critical_below_bytes": 40u64 * 1024 * 1024 * 1024,
        }))
        .unwrap();
        assert_eq!(critical.headline(), "New work paused: critically low disk space");
        assert!(critical.detail().starts_with("12.0 GiB free."), "{}", critical.detail());
        assert!(critical.detail().contains("40.0 GiB"), "{}", critical.detail());
        assert!(critical.detail().contains("Settings > Storage"), "{}", critical.detail());

        let unknown: StartBlock = serde_json::from_value(serde_json::json!({
            "kind": "disk_space_unavailable",
            "reason": "permission denied",
        }))
        .unwrap();
        assert_eq!(unknown.headline(), "New work paused: SlashIt couldn't verify free disk space");
        assert!(unknown.detail().starts_with("permission denied."), "{}", unknown.detail());
    }
}
