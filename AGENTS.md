# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

SlashIt is a Tauri v2 desktop application for AI agent workspace management. It integrates with Claude Code Agent and Jujutsu (jj) version control. The app is built entirely in Rust - Leptos/WASM for the frontend and pure Rust for the Tauri backend.

## Development Commands

```bash
# Development (frontend only - serves on port 1420)
trunk serve

# Development (full Tauri app - runs trunk + tauri)
./dev.sh
# Or manually: NO_AT_BRIDGE=1 cargo tauri dev
# NOTE: NO_AT_BRIDGE=1 is required on non-GNOME desktops (i3, sway, etc)
# to prevent WebKitGTK segfault in AT-SPI accessibility bridge

# Production build
cargo tauri build

# Check code
cargo check
cargo clippy

# Run backend directly
cargo run
```

## Project Structure

```
slashit-app/
├── src/              # Frontend (Leptos/WASM)
│   ├── main.rs       # Frontend entry point
│   ├── app.rs        # App router and layout
│   ├── components/   # UI components
│   ├── pages/        # Page views (Dashboard, Agent, Spec, Context, Settings)
│   ├── services/     # Frontend Tauri IPC services
│   └── models/       # Frontend domain models
└── src-tauri/        # Backend (Rust - Tauri v2)
    ├── src/
    │   ├── main.rs       # Backend entry point
    │   ├── lib.rs        # App state and Tauri command registration
    │   ├── commands/     # Tauri IPC command handlers
    │   ├── domain/       # Domain models (Agent, Project, Repository, Session, Task, Workspace)
    │   ├── agents/       # Claude Code agent implementation via ACP
    │   ├── acp/          # Agent Communication Protocol
    │   ├── jj/           # Jujutsu version control integration
    │   ├── session/      # Session management
    │   └── config/       # Persistent storage (AppConfig, JjConfig, UiPreferences)
    ├── Cargo.toml
    └── tauri.conf.json
```

## Architecture

### Frontend (Leptos 0.8 - CSR)

- Router in `src/app.rs` uses signal-based page selection
- Pages: Dashboard, Agent, Spec, Context, Settings
- Components include: AppLayout, Sidebar, ProjectCard, TaskCard, Kanban board, WorkspacePanel, AgentPanel, LogViewer, JjStatus

### Backend (Tauri v2)

- **AppState**: Shared state container managing Repository, Project, Workspace, Task, Agent, Session, and Jj states
- **Commands**: Tauri IPC handlers organized by domain (repository, project, workspace, task, agent, session, jj)
- **ACP**: Custom Agent Communication Protocol for Claude Code integration
- **JJ**: Jujutsu integration for version control operations (new_change, describe_change, abandon_change, git_export)
- **Config**: TOML-based persistence using system directories (directories crate)

## Key Dependencies

- **Frontend**: leptos 0.8 (csr), wasm-bindgen, serde, chrono, uuid
- **Backend**: tauri 2, tokio (full features), async-trait, chrono, uuid, anyhow, sysinfo, directories, toml

## Build Configuration

- Dev server: `http://localhost:1420` (Trunk)
- Frontend dist: `../dist`
- Global Tauri: enabled (`withGlobalTauri: true`)
- WebSocket protocol: `ws` (for hot-reload during mobile development)

## Tauri + Leptos Best Practices

Per official [Tauri Leptos guide](https://tauri.app/start/frontend/leptos/):

- Use **SSG** (Static Site Generation) - Tauri doesn't officially support server-based solutions
- Ensure `ws_protocol = "ws"` in Trunk.toml for proper hot-reload websocket during mobile development
- Keep `withGlobalTauri: true` in tauri.conf.json to expose `window.__TAURI__`

## IPC Pattern

Frontend calls Tauri commands via `invoke()` through services in `src/services/`. Backend commands are registered in `src-tauri/src/lib.rs` with state managed through `AppState`.

## Version Control with Jujutsu (JJ)

This project uses **Jujutsu (JJ)** as the primary version control system with Git colocated backend for compatibility.

### Always Use JJ Over Git

When working on this project, prefer JJ commands:

- `jj status` instead of `git status`
- `jj log` instead of `git log`
- `jj new` instead of `git commit`
- `jj git push` instead of `git push`

### Why JJ?

- Better mental model for stacked changes
- Safe change manipulation (rebase, edit, abandon)
- Seamless Git interoperability via colocated backend

## Nested AGENTS.md

This project has additional `AGENTS.md` files in subdirectories with module-specific guidance:

- `src/AGENTS.md` -- Frontend (Leptos/WASM) patterns
- `src-tauri/src/AGENTS.md` -- Backend (Tauri v2) overview
- `src-tauri/src/domain/AGENTS.md` -- Domain models conventions
- `src-tauri/src/agents/AGENTS.md` -- Claude Code agent integration via ACP

Each is also exposed as a `CLAUDE.md` stub for cross-agent compatibility.
