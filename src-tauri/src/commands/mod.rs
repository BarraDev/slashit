// SPDX-License-Identifier: MPL-2.0
pub mod agent;
pub mod appearance;
pub mod attention;
pub mod changelog;
#[cfg(test)]
mod complete_and_integrate_tests;
pub mod conversation;
pub mod executor;
pub mod features;
pub mod file;
pub mod github;
pub mod gui_admission;
pub mod human_review;
pub mod integration;
pub mod jira;
pub mod jj;
pub mod mcp;
pub mod memory;
pub mod orphans;
pub mod portability;
pub mod pr;
pub mod project;
pub mod qa;
pub mod queue;
pub mod repository;
pub mod repository_setup;
pub mod review_fix;
pub mod roadmap;
pub mod session;
pub mod state_location;
pub mod storage_usage;
pub mod task;
pub mod task_boards;
pub mod tray;
pub mod updater;
pub mod worker_recovery;
pub mod workflow;
pub mod workspace;
pub mod worktree;

pub use agent::*;
pub use conversation::{
    act_on_project_conversation, get_conversation_draft, get_conversations_config,
    get_project_conversation, get_project_conversation_executions, open_project_conversation,
    retry_project_conversation_continuation, retry_project_message, save_conversation_draft,
    send_project_message, stop_project_conversation, update_conversations_config,
};
pub use human_review::*;
pub use jj::*;
pub use project::*;
pub use qa::*;
pub use queue::*;
pub use repository::*;
pub use repository_setup::*;
pub use session::*;
pub use task::*;
pub use worker_recovery::{accept_worker_result, get_worker_recovery_evidence};
pub use workspace::*;
pub mod worker_new_attempt;
pub use appearance::*;
pub use attention::get_attention_summary;
pub use changelog::*;
pub use executor::*;
pub use features::*;
pub use file::{check_is_git_repo, list_files, pick_folder, read_file};
pub use github::*;
pub use integration::*;
pub use mcp::*;
pub use memory::*;
pub use orphans::{reclaim_orphan_branch, reclaim_orphan_checkout, scan_orphans};
pub use pr::*;
pub use roadmap::*;
pub use state_location::*;
pub use storage_usage::{get_new_work_pause, get_storage_usage, refresh_storage_usage};
pub use task_boards::list_task_board_diagnostics;
pub use tray::*;
pub use worker_new_attempt::{authorize_new_attempt, get_new_attempt_evidence, start_new_attempt};
pub use worktree::*;
// Only the commands: the module's types are reached through
// `commands::updater::` so that `UpdaterState` cannot be confused with the
// plugin type of the same name.
pub use updater::{
    app_about, updater_check, updater_download_and_install, updater_restart, updater_set_channel,
    updater_status,
};
// workflow commands not yet wired up
