//! Configuration and storage module
//!
//! This module handles persistent storage of application configuration,
//! projects, repositories, and tasks.

pub mod migration;
pub mod paths;
pub mod queue;
pub mod storage;
pub mod workspace_registry;

pub use queue::*;
pub use storage::*;
pub use workspace_registry::WorkspaceRegistry;
