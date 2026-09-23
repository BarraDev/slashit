# AGENTS.md

This file provides guidance to coding agents (Claude Code and others) when working with code in this repository.

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

# Desktop acceptance against the real application (Linux; needs a display and
# WebKitWebDriver). The build step is required: a plain `cargo build` binary
# points at localhost:1420 and never loads the embedded frontend.
cargo tauri build --debug --no-bundle
cargo test -p slashit-acceptance --features run-acceptance
```

See `crates/slashit-acceptance/README.md` for what the harness guarantees and
how to run it on Arch, which ships no `WebKitWebDriver`.

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

## Development Workflow

This project uses **Jujutsu (JJ)** as the primary version control system,
colocated with Git. The rules below are the contract for maintainers and
agents; [docs/development-workflow.md](docs/development-workflow.md) has the
procedure, commands, and rationale. External contributors may use plain Git
(see `CONTRIBUTING.md`).

Paths below are relative to the canonical repository root. Its parent
directory (the *project parent*) also holds developer workspaces and local
private state.

### Repository and workspaces

- The canonical repository root is the coordination checkout. Its `default`
  JJ workspace stays an empty change on `main`; product edits do not happen
  there.
- Normal work happens in a JJ workspace at `../workspaces/<slug>`, created
  from current `main` with the project-pinned `jj`.
- One mutating owner per workspace. Read-only reviewers may share a workspace
  but must not write to it or run builds that conflict with the owner's.
- Never run `git worktree add` against the canonical `.git`. A task that
  genuinely needs a real `.git` (for example exercising SlashIt's own Git
  worktree backend, or `git bisect`) uses a disposable independent clone at
  `../workspaces/<slug>-git`.
- Each workspace keeps its own `target/`; do not share a Cargo target
  directory between workspaces or set `CARGO_TARGET_DIR` globally.
- JJ workspaces have no `.git`; run `gh` with `-R BarraDev/slashit` or from
  the canonical root.

### Tooling

- Use the toolchain pinned in the project `mise.toml`; run JJ as
  `mise exec -- jj ...`. Do not rely on a globally installed or `latest` `jj`.
- In the colocated canonical root, an ordinary `jj` command may snapshot the
  working copy and import/export Git refs. For inspection there, use
  `mise exec -- jj --ignore-working-copy ...` or read-only Git plumbing
  (`git rev-parse`, `git cat-file`, `git ls-tree`, `git diff-tree`).

### Agents

- The main (orchestrating) agent owns decomposition, integration, the final
  report, and every public mutation.
- A subagent that writes gets its own workspace. Subagents do not spawn
  further subagents.
- Subagent output is evidence, not proof: the main agent re-verifies any
  claim a decision depends on.

### Reports and sessions

- Each substantial unit of work produces one authoritative report in
  `../slashit-private/reports/`, plus at most a bounded worklog.
  `slashit-private` is local, untracked maintainer state, not part of the
  repository. Do not store full chat transcripts.
- Start a fresh session at substantial unit boundaries. Lessons that must
  outlive a report are rewritten into project docs.

### Pull requests and published text

- A PR is a semantic delivery unit, not one PR per review finding. Stack PRs
  only for real dependencies, and keep every PR in a stack independently
  truthful.
- PR titles and bodies, commit messages, and source comments must stay useful
  to a future reader. They must not contain internal work-tracking labels,
  session narration, temporary verification SHAs, private report paths, or AI
  attribution (see below).
- Do not post transient agent-status comments on PRs.

### Authorization

- Local work inside your own workspace (edits, `jj` changes, builds, tests)
  is normal and needs no gate.
- Public or destructive operations (push, creating or editing PRs and issues,
  publication bookmarks, abandoning or deleting work or workspaces you do not
  own) require an explicit authorization appropriate to their blast
  radius.
- Merging, repository or GitHub settings changes, force-pushes, and deleting
  remote branches or tags require the repository owner's explicit approval
  every time.

### No AI Attribution in Public Repository Artifacts

Commits, PR titles and bodies, PR and issue comments, issues, release notes,
tags and any other published repository artifact must contain **no AI session
URLs, session identifiers, or AI attribution**, unless the user explicitly
asks for one in that specific artifact.

Never add, among others:

- `Claude-Session:` or any other session trailer
- `claude.ai/code/session/...` or an equivalent link for any AI tool
- `Co-Authored-By: Claude` (or another AI assistant)
- "Generated with/by Claude", "Made with <AI tool>" and similar

Before every commit, push, or PR/issue write, inspect what is about to become
public -- commit messages, the PR body, comment text, generated metadata --
and confirm none of the above is present. If attribution slipped into work
that is still unpublished or still amendable, remove it before finishing
rather than leaving it for a follow-up.

Tool and session provenance belongs in private working notes, not in the
repository's public history.

## Nested AGENTS.md

This project has additional `AGENTS.md` files in subdirectories with module-specific guidance:

- `src/AGENTS.md` -- Frontend (Leptos/WASM) patterns
- `src-tauri/src/AGENTS.md` -- Backend (Tauri v2) overview
- `src-tauri/src/domain/AGENTS.md` -- Domain models conventions
- `src-tauri/src/agents/AGENTS.md` -- Claude Code agent integration via ACP

Each is also exposed as a `CLAUDE.md` stub for cross-agent compatibility.
