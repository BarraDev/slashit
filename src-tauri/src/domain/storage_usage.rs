//! How much disk SlashIt uses, and how much of it it could prove is safe to
//! give back.
//!
//! Informational only. Nothing here deletes, moves or prunes anything, and no
//! behavior elsewhere depends on these numbers yet. They are the foundation a
//! later disk-pressure guard or cleanup policy would be built on, so every
//! figure errs toward claiming less: bytes SlashIt cannot prove it owns are
//! [`StorageClassification::Unknown`], never owned, and never reclaimable.
//!
//! The totals do not overlap in the way their names might suggest:
//!
//! - `slashit_owned_bytes` is everything measured under SlashIt's own
//!   directories whose purpose SlashIt can name. It includes the next three.
//! - `active_workspace_bytes` is the part of that in Task Checkouts of tasks
//!   still on the board and not Done.
//! - `rebuildable_bytes` is the part that is build output or cache.
//! - `reclaimable_bytes` is the part of `rebuildable_bytes` that is also
//!   outside any checkout an agent may be using right now.
//! - `unknown_managed_bytes` is what sits under SlashIt's directories that it
//!   cannot account for. It is not part of `slashit_owned_bytes`.

use serde::{Deserialize, Serialize};

use crate::domain::TaskStatus;

pub const GIB: u64 = 1024 * 1024 * 1024;

/// What a set of bytes is, as far as SlashIt can prove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageClassification {
    /// A Task Checkout's working files: the task's source and whatever the
    /// agent or user left there. May hold uncommitted work.
    WorkspaceSource,
    /// Build output a build recreates: an ignored, untracked `target/` or
    /// `dist/` at the root of a SlashIt-owned Task Checkout.
    Rebuildable,
    /// Runtime files and caches.
    Temporary,
    /// SlashIt's own configuration, boards, logs and session metadata.
    AppData,
    /// Under SlashIt's directories, but not something SlashIt can name: a
    /// checkout no task records, an entry no version of SlashIt writes, a
    /// symbolic link.
    Unknown,
}

/// Where a consumer came from.
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

/// The state of the task a checkout belongs to, read from the task itself.
///
/// Carries [`TaskStatus`] rather than a status of its own, so the two cannot
/// disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum CheckoutLifecycle {
    /// Exactly one task of the project whose worktree root holds it records
    /// this checkout. `agent_attached` says whether an agent (an execution,
    /// an AI review or a pull request helper) was working in it when it was
    /// measured, which the status alone cannot say.
    Task { status: TaskStatus, agent_attached: bool },
    /// The task recording it started a cleanup that never recorded its
    /// outcome, so part of the checkout may already be gone.
    CleanupInterrupted { status: TaskStatus },
    /// No task records it.
    Unrecorded,
    /// More than one task records it, so it is not attributed to any.
    Contested,
    /// A task records it, but not under the worktree root of the task's own
    /// repository, where SlashIt would have put it: another project's root,
    /// or the repository has moved or cannot be found.
    Misplaced,
}

impl CheckoutLifecycle {
    /// Whether an agent may be working in the checkout now or at any
    /// moment, so that even its build output is in use: one is attached, or
    /// the task is running, in AI review, or queued to start.
    ///
    /// Says nothing about what the user runs there themselves.
    pub fn may_be_in_use(&self) -> bool {
        match self {
            Self::Task { status, agent_attached } => {
                *agent_attached
                    || matches!(status, TaskStatus::InProgress | TaskStatus::AiReview | TaskStatus::Queue)
            }
            // Not attributable, so not provably idle.
            Self::CleanupInterrupted { .. } | Self::Unrecorded | Self::Contested | Self::Misplaced => true,
        }
    }

    /// Whether the checkout belongs to a task still on the board and not
    /// finished.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Task { status, .. } if *status != TaskStatus::Done)
    }
}

/// How completely a consumer was measured.
///
/// Kept apart from the byte count so that "nothing there" and "could not
/// look" never read the same.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Measurement {
    Complete,
    /// Some entries could not be read, or the walk stopped at a limit. The
    /// byte count is a lower bound.
    Partial { skipped_entries: u64, reason: String },
    /// Nothing could be measured. The consumer's bytes are `None`.
    Failed { reason: String },
}

/// One directory or file SlashIt accounts for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageConsumer {
    pub kind: ConsumerKind,
    /// What to call it: a task's title, or a fixed name such as "Logs".
    pub label: String,
    /// A secondary identity where one helps: the project, the checkout's
    /// directory name. Never a full path.
    pub detail: Option<String>,
    /// `None` only when [`Measurement::Failed`].
    pub bytes: Option<u64>,
    pub classification: StorageClassification,
    /// Set for Task Checkouts and the build output inside them.
    pub lifecycle: Option<CheckoutLifecycle>,
    pub reclaimable: bool,
    pub measurement: Measurement,
}

/// Space on the filesystem holding SlashIt's data directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemSpace {
    pub total_bytes: u64,
    /// Space an unprivileged process can still write, which is what a build
    /// runs out of. May be less than the filesystem's free space when some
    /// is reserved.
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskPressure {
    Normal,
    Warning,
    Critical,
}

/// When free space counts as low.
///
/// A level applies when available space is strictly below the larger of a
/// share of the filesystem and a fixed floor, so small disks reach it by the
/// floor and large disks by the share. Informational only: nothing refuses
/// or throttles work on it yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PressurePolicy {
    pub warning_percent: u8,
    pub warning_floor_bytes: u64,
    pub critical_percent: u8,
    pub critical_floor_bytes: u64,
}

impl Default for PressurePolicy {
    fn default() -> Self {
        Self {
            warning_percent: 15,
            warning_floor_bytes: 120 * GIB,
            critical_percent: 5,
            critical_floor_bytes: 40 * GIB,
        }
    }
}

/// The available-space thresholds a [`PressurePolicy`] sets for one
/// filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PressureThresholds {
    pub warning_below_bytes: u64,
    pub critical_below_bytes: u64,
}

impl PressurePolicy {
    pub fn thresholds(&self, total_bytes: u64) -> PressureThresholds {
        PressureThresholds {
            warning_below_bytes: share(total_bytes, self.warning_percent)
                .max(self.warning_floor_bytes),
            critical_below_bytes: share(total_bytes, self.critical_percent)
                .max(self.critical_floor_bytes),
        }
    }

    pub fn classify(&self, space: FilesystemSpace) -> DiskPressure {
        let thresholds = self.thresholds(space.total_bytes);
        if space.available_bytes < thresholds.critical_below_bytes {
            DiskPressure::Critical
        } else if space.available_bytes < thresholds.warning_below_bytes {
            DiskPressure::Warning
        } else {
            DiskPressure::Normal
        }
    }
}

/// `percent`% of `total`, rounded down, without overflowing.
fn share(total: u64, percent: u8) -> u64 {
    // The product needs u128. A share above 100% of the largest filesystem
    // does not fit in u64 and saturates.
    u64::try_from(u128::from(total) * u128::from(percent) / 100).unwrap_or(u64::MAX)
}

/// One complete measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageSummary {
    pub measured_at: chrono::DateTime<chrono::Utc>,
    pub duration_ms: u64,
    /// `None` when the filesystem could not be asked; see `filesystem_error`.
    pub filesystem: Option<FilesystemSpace>,
    pub filesystem_error: Option<String>,
    /// `None` exactly when `filesystem` is.
    pub pressure: Option<DiskPressure>,
    pub thresholds: Option<PressureThresholds>,
    pub slashit_owned_bytes: u64,
    pub active_workspace_bytes: u64,
    pub rebuildable_bytes: u64,
    pub reclaimable_bytes: u64,
    pub unknown_managed_bytes: u64,
    /// True when any consumer was measured only partly or not at all, so
    /// every total above is a lower bound.
    pub incomplete: bool,
    /// Symbolic links (and Windows reparse points) inside SlashIt's
    /// directories. Each is counted at its own size; what it points to is
    /// not measured.
    pub links_not_followed: u64,
    /// Directories inside SlashIt's directories that are on another
    /// filesystem, and so not measured.
    pub mounts_not_entered: u64,
    /// Checkouts tasks record outside SlashIt's own directories: adopted,
    /// or from before SlashIt kept its own. Not SlashIt's to account for,
    /// so never measured.
    pub external_checkouts: u32,
    /// The largest measured consumers, largest first.
    pub largest_consumers: Vec<StorageConsumer>,
    /// Consumers that could not be measured at all.
    pub unmeasured: Vec<StorageConsumer>,
}

/// What the Storage view shows: the latest measurement, if any, and whether
/// another one is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageStatus {
    pub summary: Option<StorageSummary>,
    pub measuring: bool,
    /// Why the most recent measurement produced nothing. The previous
    /// summary, if any, is kept.
    pub last_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(total: u64, available: u64) -> FilesystemSpace {
        FilesystemSpace {
            total_bytes: total,
            available_bytes: available,
        }
    }

    #[test]
    fn floors_govern_small_disks_and_shares_govern_large_ones() {
        let policy = PressurePolicy::default();
        // 500 GiB: 15% is 75 GiB, below the 120 GiB floor.
        let small = policy.thresholds(500 * GIB);
        assert_eq!(small.warning_below_bytes, 120 * GIB);
        assert_eq!(small.critical_below_bytes, 40 * GIB);
        // 2000 GiB: 15% is 300 GiB and 5% is 100 GiB, both above the floors.
        let large = policy.thresholds(2000 * GIB);
        assert_eq!(large.warning_below_bytes, 300 * GIB);
        assert_eq!(large.critical_below_bytes, 100 * GIB);
    }

    #[test]
    fn levels_apply_strictly_below_their_thresholds() {
        let policy = PressurePolicy::default();
        let total = 500 * GIB;
        assert_eq!(policy.classify(space(total, 120 * GIB)), DiskPressure::Normal);
        assert_eq!(policy.classify(space(total, 120 * GIB - 1)), DiskPressure::Warning);
        assert_eq!(policy.classify(space(total, 40 * GIB)), DiskPressure::Warning);
        assert_eq!(policy.classify(space(total, 40 * GIB - 1)), DiskPressure::Critical);
        assert_eq!(policy.classify(space(total, 0)), DiskPressure::Critical);

        let total = 2000 * GIB;
        assert_eq!(policy.classify(space(total, 300 * GIB)), DiskPressure::Normal);
        assert_eq!(policy.classify(space(total, 300 * GIB - 1)), DiskPressure::Warning);
        assert_eq!(policy.classify(space(total, 100 * GIB)), DiskPressure::Warning);
        assert_eq!(policy.classify(space(total, 100 * GIB - 1)), DiskPressure::Critical);
    }

    #[test]
    fn a_share_of_the_largest_filesystem_does_not_overflow() {
        let policy = PressurePolicy::default();
        let thresholds = policy.thresholds(u64::MAX);
        assert_eq!(thresholds.warning_below_bytes, u64::MAX / 100 * 15 + 15 * (u64::MAX % 100) / 100);
        assert_eq!(policy.classify(space(u64::MAX, u64::MAX)), DiskPressure::Normal);
        assert_eq!(share(u64::MAX, 255), u64::MAX, "saturates rather than wrapping");
    }

    #[test]
    fn only_idle_attributed_checkouts_are_out_of_use() {
        use TaskStatus::*;
        let task = |status: &TaskStatus, agent_attached| CheckoutLifecycle::Task { status: status.clone(), agent_attached };
        for status in [Backlog, HumanReview, PrCreated, Error, Done] {
            assert!(!task(&status, false).may_be_in_use(), "{status:?}");
            // A pull request helper works in a PR Created task's checkout
            // without changing its status.
            assert!(task(&status, true).may_be_in_use(), "{status:?} with an agent");
        }
        for status in [Queue, InProgress, AiReview] {
            assert!(task(&status, false).may_be_in_use(), "{status:?}");
        }
        assert!(CheckoutLifecycle::CleanupInterrupted { status: Done }.may_be_in_use());
        assert!(CheckoutLifecycle::Unrecorded.may_be_in_use());
        assert!(CheckoutLifecycle::Contested.may_be_in_use());
        assert!(CheckoutLifecycle::Misplaced.may_be_in_use());
    }

    /// The frontend's `models::storage_usage` test deserializes this same
    /// shape; change both together.
    #[test]
    fn the_wire_shape_is_what_the_frontend_reads() {
        let consumer = |kind, label: &str, bytes, classification, lifecycle, reclaimable, measurement| StorageConsumer {
            kind,
            label: label.to_string(),
            detail: None,
            bytes,
            classification,
            lifecycle,
            reclaimable,
            measurement,
        };
        let status = StorageStatus {
            summary: Some(StorageSummary {
                measured_at: "2026-09-28T12:00:00Z".parse().unwrap(),
                duration_ms: 12,
                filesystem: Some(space(100, 50)),
                filesystem_error: None,
                pressure: Some(DiskPressure::Warning),
                thresholds: Some(PressureThresholds { warning_below_bytes: 60, critical_below_bytes: 20 }),
                slashit_owned_bytes: 10,
                active_workspace_bytes: 5,
                rebuildable_bytes: 3,
                reclaimable_bytes: 2,
                unknown_managed_bytes: 1,
                incomplete: true,
                links_not_followed: 4,
                mounts_not_entered: 0,
                external_checkouts: 1,
                largest_consumers: vec![StorageConsumer {
                    detail: Some("target/ build output".to_string()),
                    ..consumer(
                        ConsumerKind::BuildOutput,
                        "Fix login",
                        Some(3),
                        StorageClassification::Rebuildable,
                        Some(CheckoutLifecycle::Task { status: TaskStatus::HumanReview, agent_attached: false }),
                        true,
                        Measurement::Partial { skipped_entries: 1, reason: "x".to_string() },
                    )
                }],
                unmeasured: vec![consumer(
                    ConsumerKind::Logs,
                    "Logs",
                    None,
                    StorageClassification::AppData,
                    None,
                    false,
                    Measurement::Failed { reason: "denied".to_string() },
                )],
            }),
            measuring: false,
            last_error: None,
        };
        let expected = serde_json::json!({
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
        assert_eq!(serde_json::to_value(&status).unwrap(), expected);
    }

    #[test]
    fn done_and_unattributed_checkouts_are_not_active() {
        assert!(CheckoutLifecycle::Task { status: TaskStatus::HumanReview, agent_attached: false }.is_active());
        assert!(!CheckoutLifecycle::Task { status: TaskStatus::Done, agent_attached: false }.is_active());
        assert!(!CheckoutLifecycle::Unrecorded.is_active());
        assert!(!CheckoutLifecycle::Contested.is_active());
        assert!(!CheckoutLifecycle::CleanupInterrupted { status: TaskStatus::InProgress }.is_active());
    }
}
