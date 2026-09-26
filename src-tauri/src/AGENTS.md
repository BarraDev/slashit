# AGENTS.md (src-tauri/src)

Backend source for Tauri v2. This folder contains all Rust backend code organized by domain.

## Module Structure

- **lib.rs** - App state definition, Tauri command registration, app entry point
- **main.rs** - Backend entry point (calls `slashit_ui_lib::run()`)
- **commands/** - Tauri IPC command handlers organized by domain
- **domain/** - Shared domain models (Agent, Project, Repository, Session, Task, Workspace)
- **agents/** - Claude Code agent execution (see `agents/AGENTS.md`)
- **acp/** - Agent Communication Protocol implementation
- **jj/** - Jujutsu version control integration (see `jj/AGENTS.md`)
- **session/** - Session management
- **config/** - Persistent storage using TOML files in system directories
- **worktree/** - Git worktrees, the only Task Checkout backend, task diffs, and restacking a task branch onto a new base
- **queue/** - Task queue, admission and the executor that launches agent runs
- **pty/** - Terminal (PTY) sessions
- **ipc/** - Control channel server for the `slashit` CLI (see below)
- **bin/** - The `slashitd` daemon entry point

## AppState Pattern

All command handlers receive `AppState` via Tauri's `manage()` mechanism. State
is organized by domain: one field per feature area (`state.task`,
`state.queue`, `state.jj`, ...), plus shared services such as the event sink
and the resolved paths. The `AppState` struct in `lib.rs` is the authoritative
field list; do not copy it here.

Most per-domain state types are defined in their respective `commands/`
submodule.

## Tauri Commands

Commands are registered in `lib.rs` using `tauri::generate_handler!`. All commands must:
1. Accept parameters matching the frontend's `invoke()` call
2. Return `std::result::Result<T, E>` where E implements `serde::Serialize`
3. Use `AppState` via `tauri::State<AppState>` parameter if needed

## Two front ends, one backend

The backend runs under the desktop app *and* under `slashitd`, the headless
daemon. Almost everything is shared. Only three things differ, and each is
behind an abstraction — code below the command layer must use the abstraction
rather than reaching for Tauri:

- **app_core.rs** — `app_core::build_state()` builds `AppState` from disk.
  Both entry points call it. It is `async`: hydration takes tokio locks, and
  a `blocking_write()` / `blocking_read()` panics on a runtime thread. Never
  reintroduce a blocking lock acquisition on a startup path.
- **events.rs** — `EventSink` replaces `AppHandle::emit`. The GUI installs
  `TauriEventSink`, the daemon installs `LoggingEventSink`, tests use
  `NullEventSink` or `RecordingEventSink`. `AppState::events()` returns the
  installed sink.
- **instance.rs** — `InstanceControl` covers showing the window and requesting
  a quit. The daemon's `DaemonControl::show_window` returns an error rather
  than pretending to succeed.
- **daemon.rs** / **bin/slashitd.rs** — the headless entry point. See
  `docs/architecture/daemon.md`.

Do not add a `tauri::AppHandle` field to anything in `queue/`, `agents/`,
`worktree/`, `jj/`, `session/`, `acp/`, `pty/manager.rs` or `config/`. Those
modules are Tauri-free and must stay that way, or the daemon stops building.

## The control channel

**ipc/** serves the `slashit` CLI over the transports in the `slashit-ipc`
crate: a Unix socket on Linux and macOS, a named pipe on Windows, and an
optional loopback-only TCP listener that is off by default and requires a
bearer token.

Treat it as a privileged interface. `CreateTask` + `MoveTask(in_progress)`
reaches the queue executor, which spawns an agent with full tool access, so it
is a code-execution channel. Verbs with that reach are marked by
`IpcRequest::spawns_agent()` and are denied to any peer the OS did not
authenticate. Read `docs/architecture/ipc-security.md` before changing
anything in `ipc/` or in `crates/slashit-ipc`.

## Managed state

`lib.rs` calls `.manage()` exactly once, with `AppState`. Therefore **every
command must take `tauri::State<'_, AppState>`** and reach sub-state through a
field (`state.roadmap`, `state.jj`, ...). A command that asks for
`tauri::State<'_, RoadmapState>` compiles fine and then fails at run time with
"state not managed", because Tauri resolves by `TypeId` against the managed
map. This has already happened once; do not reintroduce it.

## Paths

`config/paths.rs` is the single source of truth for every path. Do not call
`ProjectDirs::from(...)` anywhere else. The runtime directory (socket, PID
file) comes from `slashit_ipc::endpoint::runtime_dir()` so the server and the
diagnostics cannot disagree.

The identity triple `("com", "barradev", "slashit-app")` and the bundle
identifier `com.barradev.slashit-app` are **frozen**. They are user-data and OS
identity, not binary names — the GUI binary is `slashit-ui` and the daemon is
`slashitd`, and neither name may leak into a storage path.
