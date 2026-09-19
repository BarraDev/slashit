# Headless daemon mode

**Status: designed, not implemented.** The `daemon_mode` feature flag exists
and is off. This document is the plan and the honest state of the blockers, so
the work can be picked up without re-deriving it.

## What exists today

Close-to-tray keeps the app alive with its window hidden: agents and terminals
continue running and the IPC socket stays up. What does not exist is a mode
with **no GUI process at all** — an IPC server plus queue executor suitable for
a systemd user unit.

## Why it is not a rewrite

The Tauri coupling is much shallower than the backend's size suggests. Of 82
backend files, 11 contain a Tauri reference and 4 of those are false positives
(Windows shell-detection strings, doc comments).

Already Tauri-free: `domain/`, `config/`, `worktree/`, `jj/`, `acp/`,
`session/`, `queue/manager.rs`, `queue/prompt.rs`, `queue/workflow.rs`,
`pty/manager.rs`, `pty/store.rs`.

The concentration is in `queue/executor.rs`, and every reference there is one
of three things: the `tauri::AppHandle` field, `tauri::async_runtime::spawn`,
or `.emit(...)`. There is no `.state()`, no window access, no `Manager` use.
That is the whole problem: **the executor needs to emit events, not to be a
GUI**.

## Design

### 1. An event sink

Replace direct `app.emit()` calls with a sink the executor holds:

```rust
pub type EventSink = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;
```

The GUI passes a sink that forwards to `AppHandle::emit`. The daemon passes one
that logs and discards, or later, one that fans out to connected IPC clients.

This is not a new idea in this codebase — `commands/pr.rs` already does exactly
this with `ProgressSink` and a `no_progress()` null sink. Generalising that
pattern is the bulk of the work: 35 raw `emit` call sites across 5 files,
5 distinct event names, of which 29 are in the executor.

### 2. Lift startup out of `run()`

`lib.rs::run()` mixes state construction with `tauri::Builder`. The first ~140
lines — resolve paths, build `Storage`, load config, hydrate repositories,
projects and tasks — are GUI-independent and belong in a
`fn build_state() -> AppState` both entrypoints call.

### 3. A daemon entrypoint

A `slashitd` binary, or `slashit-ui --headless`, that builds state, starts the
IPC server and the executor with a discarding sink, writes
`AppPaths::pid_file()`, and waits for a signal.

### 4. CLI and unit

`slashit daemon start|stop|status`, plus a
`~/.config/systemd/user/slashit.service` template.

## Blockers that must be fixed first

Both are real, both are cheap, and both cause a crash rather than a missing
feature:

1. **`blocking_write()` / `blocking_lock()` during startup.** `lib.rs` uses
   them to hydrate state. Tokio panics if these are called from inside a
   runtime thread, so they work today only because `run()` executes before the
   runtime starts. A `#[tokio::main]` daemon calling the same code panics
   immediately. Fix: make `build_state()` async, or hydrate before entering the
   runtime.

2. **Socket ownership.** Fixed in the commit that hardened IPC — the server now
   refuses to start if a live instance already owns the socket. Without that, a
   daemon started next to a running GUI silently stole its socket. Worth
   re-checking when the daemon lands, because it becomes routine rather than
   accidental for both to be running.

## Deliberately out of scope

Dropping the `tauri` crate from the daemon build behind a `gui` Cargo feature
is a *separate, much larger* job: 21 files carry `tauri::State`, there are 139
`#[tauri::command]` functions, and `pty/mod.rs` re-exports the Tauri command
module unconditionally. The MVP keeps `tauri` linked and simply never calls
`Builder::run`. That costs a webkit2gtk link dependency on the daemon and buys
a tractable change.

## Estimate

Medium — roughly two to three focused days, dominated by mechanically
rewriting the emit sites behind the sink and by splitting `run()`. The
`gui`-feature version is a large job and should not be on the MVP path.
