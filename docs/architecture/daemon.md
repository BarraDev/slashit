# Headless daemon mode

**Status: implemented.** `slashitd` runs the queue executor and the IPC server
with no window. It is not a second implementation of SlashIt — it calls the
same state builder, constructs the same executor, and serves the same command
set as the desktop app.

## Running it

```bash
slashitd                          # run in the foreground
slashitd --verbose                # log every event, including agent output
slashitd --feature daemon_mode=true   # override a flag for this run
```

`slashit ping` reports which kind of instance answered; `slashit daemon status`
answers specifically whether a daemon is running.

A systemd user unit is the expected deployment:

```ini
# ~/.config/systemd/user/slashit.service
[Unit]
Description=SlashIt headless daemon
After=network.target

[Service]
ExecStart=%h/.local/bin/slashitd
Restart=on-failure
# The socket lives in $XDG_RUNTIME_DIR, which systemd already provides.

[Install]
WantedBy=default.target
```

## What is shared, and what differs

Of the whole backend, exactly three things depend on having a window. Each is
behind an abstraction, and everything else is common code:

| Concern | GUI | Daemon |
|---|---|---|
| Events | `TauriEventSink` -> webview | `LoggingEventSink` -> stderr |
| Window and quit | `GuiControl` | `DaemonControl` |
| Entry point | `lib.rs::run()` | `bin/slashitd.rs` |

Everything else — `AppState`, hydration, migrations, the queue executor, the
IPC command handlers, worktree management, agent execution — is one
implementation used by both.

### The event sink

`queue/executor.rs` used to hold a `tauri::AppHandle` purely to call `.emit()`
29 times. It now holds an `Arc<dyn EventSink>` (`src/events.rs`) and contains
no Tauri reference at all. Three sinks exist: the Tauri one, a logging one for
the daemon, and a discarding one for tests.

This generalises a pattern the codebase already had: `commands/pr.rs` threads a
`ProgressSink` through the review-apply flow so tests can observe progress
without a window.

### Instance control

`InstanceControl` (`src/instance.rs`) covers showing the window and requesting
a quit. The daemon's `show_window` returns an error rather than silently
succeeding, so `slashit show` against a daemon says there is no window instead
of reporting a success that never happened.

## The blockers that had to be fixed first

Both were real, both caused a crash rather than a missing feature:

1. **Blocking lock acquisition during startup.** `lib.rs` hydrated state with
   `blocking_write()` / `blocking_read()` on tokio locks. That worked only
   because `run()` executes before the runtime starts; the same code inside a
   `#[tokio::main]` daemon panics immediately. Hydration is now
   `app_core::build_state()`, which is async, and the GUI reaches it through
   `tauri::async_runtime::block_on` before the event loop starts.

2. **`request_quit` had the same latent bug.** It read `pty.sessions` with
   `blocking_lock()` and `agent.executions` with `blocking_read()`. That was
   safe only because its sole caller was the tray menu, which runs on the
   event-loop thread. Routing an IPC `Quit` through it would have panicked on a
   runtime worker. It is now async and spawned.

3. **Endpoint ownership.** The transport layer refuses to bind an endpoint a
   live instance already holds — a connect-first check on Unix, and
   `first_pipe_instance` on Windows. This matters more now than it did when
   only the GUI could bind: a daemon started next to a running app is routine
   rather than accidental.

## Shutdown

`SIGTERM` or `SIGINT` (or an IPC `Quit`) stops accepting connections, then
waits up to 30 seconds for running agents to finish. Tasks still running when
the grace period expires are left as they are and returned to the queue by the
next start's migration, and the daemon says so rather than exiting silently.

The PID file at `AppPaths::pid_file()` is informational only. Endpoint
ownership is the authoritative single-instance mechanism, because a PID file
goes stale on a hard kill and acting on a stale one would let a second daemon
start and steal the first one's clients.

## Deliberately out of scope

Dropping the `tauri` crate from the daemon build behind a `gui` Cargo feature
is a separate, much larger job: 21 files carry `tauri::State` and there are
~139 `#[tauri::command]` functions. The daemon keeps `tauri` linked and simply
never calls `Builder::run`. That costs a webkit2gtk link dependency on the
daemon and buys a tractable change. The command *logic* is already shared: IPC
handlers call the same state the Tauri commands do.
