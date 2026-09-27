mod branch;
mod commit;
mod default_base;
mod diff;
mod manager;
mod ownership;
pub mod restack;

pub use branch::checked_task_branch;
pub use commit::{commit_checkout, commit_checkout_even_if_empty, CheckoutCommit};
pub use diff::{task_diff, TaskDiff, TaskDiffError};
pub use manager::{WorktreeInfo, WorktreeManager, WorktreeRecovery};
pub use ownership::{refuse_shared_task_branch, tasks_sharing_a_recorded_branch};
