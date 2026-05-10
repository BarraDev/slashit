# CLAUDE.md (src-tauri/src)

Backend source for Tauri v2. This folder contains all Rust backend code organized by domain.

## Module Structure

- **lib.rs** - App state definition, Tauri command registration, app entry point
- **main.rs** - Backend entry point (calls `lib.rs::run()`)
- **commands/** - Tauri IPC command handlers organized by domain
- **domain/** - Shared domain models (Agent, Project, Repository, Session, Task, Workspace)
- **agents/** - Claude Code agent implementation via ACP protocol
- **acp/** - Agent Communication Protocol implementation
- **jj/** - Jujutsu version control integration (workspace, backend, manager)
- **session/** - Session management
- **config/** - Persistent storage using TOML files in system directories

## AppState Pattern

All command handlers receive `AppState` via Tauri's `manage()` mechanism. State is organized by domain:
- `repository: RepositoryState`
- `project: ProjectState`
- `workspace: WorkspaceState`
- `task: TaskState`
- `agent: AgentState`
- `session: SessionState`
- `jj: JjState`

Each state type is defined in its respective `commands/` submodule.

## Tauri Commands

Commands are registered in `lib.rs` using `tauri::generate_handler!`. All commands must:
1. Accept parameters matching the frontend's `invoke()` call
2. Return `Result<T, E>` where E implements `serde::Serialize`
3. Use `AppState` via `tauri::State<AppState>` parameter if needed
