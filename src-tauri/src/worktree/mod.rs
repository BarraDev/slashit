mod branch;
mod diff;
mod manager;
pub mod restack;

pub use branch::checked_task_branch;
pub use diff::{task_diff, TaskDiff, TaskDiffError};
pub use manager::{WorktreeInfo, WorktreeManager, WorktreeRecovery};
