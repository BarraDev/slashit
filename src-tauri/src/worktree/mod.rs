mod branch;
mod commit;
mod default_base;
pub mod delivery;
mod diff;
mod manager;
mod orphans;
mod ownership;
pub mod project_base;
pub mod readiness;
pub mod remote_head;
mod registry_lock;
pub mod restack;
pub mod vcs;
pub mod vcs_init;

pub use branch::{checked_base_branch, checked_task_branch, looks_like_task_branch};
pub use commit::{commit_checkout, commit_checkout_even_if_empty, CheckoutCommit};
pub use diff::{task_diff, TaskDiff, TaskDiffError};
pub use manager::{
    carries_locked_registration_notice, locked_registration_notice, CheckoutRegistration,
    CheckoutState, WorktreeInfo, WorktreeManager, WorktreeRecovery,
};
pub use orphans::{owners_in_repository, OrphanScan, Owners};
pub use ownership::{refuse_shared_task_branch, tasks_sharing_a_recorded_branch};
