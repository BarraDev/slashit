// SPDX-License-Identifier: MPL-2.0
//! Configuration and storage module
//!
//! This module handles persistent storage of application configuration,
//! projects, repositories, and tasks.

pub mod features;
pub mod migration;
pub mod paths;
pub mod queue;
pub mod storage;
pub mod updates;
pub mod workspace_registry;

#[cfg(test)]
mod task_board_schema_tests;

pub use queue::*;
pub use storage::*;
pub use workspace_registry::WorkspaceRegistry;
