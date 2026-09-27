mod branch;
mod default_base;
mod diff;
mod manager;
mod ownership;
pub mod restack;

pub use branch::checked_task_branch;
pub use diff::{task_diff, TaskDiff, TaskDiffError};
pub use manager::{
    carries_locked_registration_notice, locked_registration_notice, CheckoutRegistration,
    CheckoutState, WorktreeInfo, WorktreeManager, WorktreeRecovery,
};
pub use ownership::{refuse_shared_task_branch, tasks_sharing_a_recorded_branch};
