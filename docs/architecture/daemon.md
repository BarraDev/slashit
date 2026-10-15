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

## One process per data directory

The synchronization the backend performs is in-process: the per-task
lifecycle leases, the per-Project deletion gate
(`lifecycle::ProjectLifecycleGate`) and the board's shared write guard are
tokio locks inside one `AppState`, and `Storage`'s file writes are atomic per
file (temp file, rename) while holding no OS-level lock. A second process
mutating the same user data concurrently is therefore unsupported, and the
single-instance rule is what enforces that in practice: both entry points
claim the IPC endpoints *before* they read or write a byte of shared state,
and a second desktop app or `slashitd` that loses the claim exits untouched
("another SlashIt instance ... is already running"). The CLI never opens the
data directory itself — it talks to the one live instance through the control
channel.

With no live instance, crash recovery may find leftover cleanup records; that
is designed reconciliation, not a concurrency agreement. Tooling that
bypasses the endpoint (tests, a debugging harness) and touches the same user
data while an instance runs carries no such guarantee and is not supported.

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

An instance stays the exclusive owner of its endpoint for as long as it can
admit, run or persist anything that would conflict with another instance. The
ownership resource (the Unix `flock` lease and socket, or the Windows
first-instance named pipe) is held by the process supervisor, never by an accept
loop, and is released last.

`SIGTERM`, `SIGINT` or an IPC `Quit` (intentional), and the mandatory local
listener failing, an accept loop or a connection handler panicking, or every
listener being lost (a failure) all take the same path, in this order:

1. Admission closes. No Worker, AI review or fix, PR helper or PR operation,
   or Project Conversation Run can start, and mutating IPC requests are
   refused. A start that had already passed its check is waited for until its
   owner is registered, so none can slip in afterwards.
2. The listeners stop accepting and every connection handler is joined.
3. The queue poller returns.
4. Running work is drained. An intentional daemon shutdown first lets agents
   finish for up to 30 seconds; a failure, and the desktop app (where the user
   has already confirmed quitting), do not wait. Whatever remains is cancelled
   and reaped through the executor's existing owner-ending paths. These do not
   change any Task: a Task that was `in_progress` stays `in_progress`, and its
   checkout, branch, commits and recovery evidence are untouched, so the next
   start requeues it exactly as after a crash.
5. Only then is the endpoint released and the process exits.

A bounded wait is not proof that a provider process stopped. If an owner cannot
be reaped, the instance keeps its ownership, says so, and keeps retrying; it
does not exit as if the shutdown were clean. A failure-triggered shutdown
exits non-zero once drained.

Desktop commands go through the same fence. Every `#[tauri::command]` that can
change anything takes a `GuiMutation` argument (`commands/gui_admission.rs`):
Tauri builds it when the invocation is dispatched, so a command dispatched after
the fence closed is refused before its body runs, and the lease it holds lives
as long as the command's own future, so the drain waits for every command that
was admitted earlier. Commands that only read, and the two that perform the
shutdown itself (`force_quit`, `updater_restart`), are listed by name; a test
fails when a command is neither. The desktop therefore releases the endpoint
(at `RunEvent::Exit`) only after no admitted command is still running.

If the task running the server dies, the supervisor above it still holds the
ownership claim, the poller's join and the connection handlers. It closes the
fence, lets every handler that was running finish, waits for the poller to
return (a poll pass that began before the fence closed is not stopped by it),
and reaps every owner before releasing anything.

The handlers are never aborted. A handler that has started a `git` run is
waiting on a blocking call, and aborting the task stops only the wait: the `git`
process would finish by itself, possibly after the endpoint was released. The
server therefore hands its live handlers to the supervisor (`HandlerRescue`)
when it dies instead of dropping them, and the supervisor joins them. A handler
serves one request and is bounded by the operation it runs; one that does not
return keeps the instance alive and holding ownership,
as every other part of the drain does.

On Windows the updater plugin's install step launches the installer and then
calls `std::process::exit(0)` itself. SlashIt therefore downloads the installer
and stages it, and runs it from the shutdown path after the drain
(`install_staged_after_drain`); no code calls the plugin's combined
download-and-install. An installer, user or administrator that terminates the
process from outside is a forced external termination, below.

Not covered, and not promised:

- `SIGKILL`, `TerminateProcess`, an installer or service manager killing the
  process, and power loss. No supervisor runs. The kernel drops the lease at
  once, a successor can own the endpoint immediately, and any provider process
  the old instance started may outlive it. Startup recovery handles the Task
  state exactly as after a crash.
- Signals sent to a daemon that is already draining are ignored; only `SIGKILL`
  ends it early.
- A poll pass that began just before the fence closed may promote a Task to
  `in_progress`. It finishes before ownership is released (the poller is
  awaited), leaves no owner behind, and startup recovery requeues the Task.
- User terminals (PTY sessions) are interactive shells under the user's control,
  not provider processes, and the drain does not reap them. A command the user
  typed (`git`, a provider CLI) can keep running between the endpoint being
  released and the process ending. Starting a terminal and writing to one are
  refused after the fence closes.
- A Windows installer that fails to start after the drain is logged and the
  application restarts into the old version; no progress event reports it.
- Admitted desktop commands hold the drain until they return. A long download
  (`updater_download_and_install`) or a provider turn delays a quit; Project
  Runs are cancelled before the wait, so a Coordinator turn does not.
- The webview is not granted the updater plugin's own commands
  (`capabilities/default.json`), so JavaScript cannot reach the plugin's
  install step, which exits the process on Windows.

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
