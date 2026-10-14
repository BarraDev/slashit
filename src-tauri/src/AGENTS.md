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
- **config/** - Persistent storage using TOML files in system directories. Every Task board, configuration and Project Conversation write goes through `slashit_durable_fs` (temporary file, file sync, atomic replace, directory sync). Its guarantee is per file, not a transaction across files, Git or processes. A write that fails before the replacement leaves the file as it was (`WriteError::NotPublished`); one that fails after it leaves the new content visible but not confirmed durable (`WriteError::PublishedNotDurable`, `ConfigWriteError::PublishedNotDurable` for configuration-class files). Configuration-class callers must treat the second as neither "nothing was saved" nor committed: memory follows the visible file (`adopt_published`), no destructive step that depends on the change runs until `Storage::confirm_config_durable` succeeds (startup uses `reconfirm_config_durable` before it deletes for an absent Project), and the message says the durability is unconfirmed. A filesystem that cannot synchronize a directory (`DirectorySync::Unsupported`) is not a confirmation: ordinary saves succeed, but `confirm_config_durable`/`reconfirm_config_durable` return `ConfigWriteError::Unconfirmable`, so Project deletion and startup cleanup keep their records and delete nothing, and a legacy Task board is not retired. Task board writers hold the board in memory, so after `WriteError::PublishedNotDurable` they adopt what the file now holds (`config::storage::board_write_published`; `lifecycle::publish`, the delete path and the executor's stop path do) and still return an error that says the change is in place; leaving memory on the previous board would let the next save of any sibling write it back over the published change. Conversations are re-read from disk at the start of every request, so they need no adoption. State-location migration (`config/migration.rs`) deletes the source and the retired destination only after `slashit_durable_fs::sync_tree` on staging and `sync_directory` on the parent both confirm; otherwise it keeps them and warns. The `slashit-ipc` crate's own writes (the TCP transport's credential in `crates/slashit-ipc/src/auth.rs`, and `IpcConfig::save`) are plain owner-only temp-file-and-rename IO outside this protocol: the token is regenerable, TCP is off by default, and `IpcConfig::save` has no production caller, so this sentence covers this crate's writes only.
- **worktree/** - Git worktrees, the only Task Checkout backend, committing a Task Checkout's changes, proving a commit reached a pull request's remote branch (`delivery`: an exact-commit ancestry check after an explicit refresh, never inferred from a clean tree; a fix's recorded effect, everything that changed in the checkout during its agent's run, must still be in the remote branch as refreshed when a reply is posted, and it means "attributed to that run", not "satisfies the review comment"), task diffs, restacking a task branch onto a new base, scanning for Task Checkouts and task branches no Task owns and reclaiming them one at a time on request, the base a task branch starts from (origin's default branch, else the project's local base, or only the local base when a person chose it over any remote), integrating a Task's approved commit into a local branch (`integration`: plan, compare-and-swap advance and crash observation in plumbing that never touches a working tree, index or the Task branch, except that a checked-out target is fast-forwarded by Git itself in the one checkout a person confirmed, after `inspect_checkout` proves it clean; shell-quoted manual commands; driven by `commands/integration.rs`), and a project's repository readiness and explicit setup (version control initialization, detecting origin's default branch)
- **queue/** - Task queue, admission, the disk-space start guard, the executor that launches agent runs, and the tool calls a run keeps for its task's activity timeline. The executor's handle maps are the one live-run registry: `task_run` and `live_runs` answer whether SlashIt owns a run for a task and in which `AgentStatus` (starting, running, stopping), and `AgentEvent::RunState` announces every change. A task's persisted status never answers that question
- **pty/** - Terminal (PTY) sessions
- **coordinator_reads.rs** - Read-only, Project-scoped, bounded Task lookups (inspect, list) the Project Coordinator can request; answered from the Task store, never persisted, never a mutation
- **conversation_actions.rs** - Human-gated Task operations proposed from a Project Conversation (create, edit): the proposal, the decision, and the recoverable write order that keeps a retried approval from repeating its effect
- **pr_status.rs** - What GitHub last said about the pull requests linked to tasks: the one place `gh pr view` runs for a status (bounded by a timeout), and the in-memory cache the executor's poll, the refresh commands and the board share. Never persisted
- **storage_accounting/** - Read-only measurement of the disk SlashIt uses: which of it SlashIt owns, which is rebuildable, and how full the disk is. Never deletes anything
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

Treat it as a privileged interface. `IpcRequest::CreateTask` + `IpcRequest::MoveTask(in_progress)`
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
