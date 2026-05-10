# CLAUDE.md (src-tauri/src/domain)

Domain models shared across backend modules. These types define the core data structures.

## Models

- **agent.rs** - Agent types and status
- **project.rs** - Project definition and metadata
- **repository.rs** - Repository configuration
- **session.rs** - Chat session with agents
- **task.rs** - Task definition with dependencies
- **workspace.rs** - Workspace state

## Serialization

All domain models use `serde::{Serialize, Deserialize}` for JSON serialization via Tauri IPC. UUIDs use `uuid` crate with `serde` feature. Dates use `chrono` with `serde` feature.

## Naming Convention

Model files use singular form (e.g., `agent.rs` not `agents.rs`) to distinguish from the feature modules that implement behavior for those models.
