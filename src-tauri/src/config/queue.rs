// SPDX-License-Identifier: MPL-2.0
use serde::{Deserialize, Serialize};

/// Bounds for [`QueueConfig::parallel_task_limit`]. The settings form enforces
/// the same range; the backend enforces it too, because a persisted `0` would
/// silently stop every Task from running across restarts.
pub const MIN_PARALLEL_TASK_LIMIT: u32 = 1;
pub const MAX_PARALLEL_TASK_LIMIT: u32 = 10;

/// The Queue settings, persisted as the `[queue]` section of `config.toml`.
///
/// `#[serde(default)]` is what lets a file written before a field existed (or
/// before this section existed at all) load with that field at its default.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct QueueConfig {
    pub parallel_task_limit: u32,
    pub auto_promote: bool,
    pub fifo_ordering: bool,
    pub use_coderabbit: bool,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            parallel_task_limit: 3,
            auto_promote: true,
            fifo_ordering: true,
            use_coderabbit: true,
        }
    }
}

impl QueueConfig {
    /// Refuse a configuration the Queue cannot run under.
    pub fn validate(&self) -> Result<(), String> {
        if !(MIN_PARALLEL_TASK_LIMIT..=MAX_PARALLEL_TASK_LIMIT).contains(&self.parallel_task_limit)
        {
            return Err(format!(
                "Parallel Task limit must be between {MIN_PARALLEL_TASK_LIMIT} and \
                 {MAX_PARALLEL_TASK_LIMIT}, not {}",
                self.parallel_task_limit
            ));
        }
        Ok(())
    }

    /// The settings to run under when read from a file someone may have
    /// edited by hand: a limit outside the bounds is brought to the nearest
    /// bound, and the message says so. Every other field is kept.
    pub fn bounded(mut self) -> (Self, Option<String>) {
        let warning = self.validate().err();
        self.parallel_task_limit = self
            .parallel_task_limit
            .clamp(MIN_PARALLEL_TASK_LIMIT, MAX_PARALLEL_TASK_LIMIT);
        (self, warning)
    }
}
