# AGENTS.md (src-tauri/src/domain)

Domain models shared across backend modules. These types define the core data structures.

## Models

- **agent.rs** - Agent types and status
- **integration.rs** - The durable record of integrating a Task's approved commit into a local branch, including the checkout a person confirmed moving it in
- **project.rs** - Project definition and metadata, including where its tasks start and whether a person chose the local branch over a remote default
- **repository.rs** - Repository configuration
- **session.rs** - Chat session with agents
- **storage_usage.rs** - Disk usage summary, what each total means, and the disk pressure policy
- **task.rs** - Task definition with dependencies
- **task_dependencies.rs** - The rules for the dependencies a person configures: parsing (refuses malformed, repeated and more-than-one identifiers), checks against the Project's Tasks (self, missing, other Project, cycles), and when a recorded set may change. Creation and `set_task_dependencies` both use it, so they decide alike. A Task with several recorded dependencies keeps them
- **workspace.rs** - Workspace: the Product Workspace that groups Projects (not a Task Checkout; see `docs/architecture/product-model.md`)

## Serialization

All domain models use `serde::{Serialize, Deserialize}` for JSON serialization via Tauri IPC. UUIDs use `uuid` crate with `serde` feature. Dates use `chrono` with `serde` feature.

## Naming Convention

Model files use singular form (e.g., `agent.rs`) to distinguish them from the plural feature modules that implement behavior for those models (e.g., `agents/`).
