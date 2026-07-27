# IPC security model

SlashIt exposes a control channel so the `slashit` CLI can drive the running
app. This document states what that channel can do, what protects it today, and
what would have to exist before it could ever accept a connection from another
machine.

The short version: **the IPC channel is a code-execution channel**, it is
protected only by local filesystem permissions, and remote access is not
implemented on purpose.

## Transport

A Unix domain socket at `$XDG_RUNTIME_DIR/slashit-app/slashit.sock`, or
`<tmp>/slashit-<uid>/slashit.sock` when `XDG_RUNTIME_DIR` is unset. One
newline-delimited JSON request per connection, one response, close.

Protections in place:

| Control | Implementation |
|---|---|
| Directory is owner-only | created 0700 before `bind()` |
| Socket is owner-only | 0600 applied after `bind()` |
| No socket hijacking | connect-first liveness check; a live owner refuses startup |
| Bounded requests | 1 MiB cap, oversized requests rejected |
| Not in a shared namespace | never a fixed path under world-writable `/tmp` |

The **directory** is the real control. Permissions on the socket file can only
be applied after `bind()` returns, which leaves a window where the socket
exists at its final path with the process umask. A 0700 parent closes that
window, because no other user can traverse into it.

The liveness check matters more than it looks. The previous code removed any
existing socket unconditionally, so starting a second instance deleted the
first one's socket and rebound — silently capturing every subsequent CLI
connection from the running app.

## What a client can do

Twelve commands. Most are read-only, but two are not, and together they are a
full local code-execution primitive:

```
CreateTask { project_id, title, description, ... }
MoveTask   { task_id, status: "in_progress" }
```

Moving a task into `in_progress` reaches the queue executor, which creates a
git worktree and spawns the configured agent with the task's
attacker-controlled description as the prompt, using
`--dangerously-skip-permissions` and the full `Read,Edit,Write,Bash,Glob,Grep`
tool set.

There is no sandbox between "wrote a string into a task description" and
"arbitrary command execution in a checkout of your repository". Anyone who can
write to this socket can run code as you.

That is acceptable for a local, owner-only socket: an attacker who can already
write to a 0700 directory owned by you can also just run the command directly.
It is *not* acceptable for anything reachable over a network.

## Remote access

`remote_access` exists as a feature flag with nothing behind it. That is
deliberate. Shipping the current protocol on a network listener would be
publishing an unauthenticated remote shell.

Before any remote listener may be enabled, **all** of the following must exist:

1. **Mutually authenticated transport.** TLS with client certificates, or an
   equivalent. Not "plaintext behind a VPN" — a VPN is a network boundary, not
   an authentication decision, and it fails open for anyone already inside it.
2. **A capability model.** Verbs must be separately grantable, and the
   agent-spawning verbs (`CreateTask`, `MoveTask`, `EnqueueTask`) must be
   denied by default. A remote client should be able to read status without
   being able to execute code.
3. **Per-identity authorization.** Which projects a credential may touch, not
   just whether it may connect.
4. **Rate limiting and connection caps**, so a credential leak is not also a
   denial-of-service vector.
5. **An append-only audit log** of every mutating request with its
   authenticated identity, written outside any project directory.
6. **Explicit opt-in per listener**, defaulting to loopback, with the bound
   address shown in the UI while it is live.

Anything less should be refused rather than shipped weak. A partial
implementation here is worse than none, because it looks like security.

## Known gaps

These are real and currently accepted for the local-only channel:

- **No protocol version or handshake.** A newer CLI against an older app fails
  with a decode error rather than a clear message. Worth fixing before the
  first release that changes the wire format.
- **No authentication at all.** Filesystem permissions are the only control.
  Correct for a local socket, insufficient for anything else.
- **No audit log.** Mutating IPC requests are not recorded.
- **Secrets remain reachable indirectly.** `AgentConfig::api_key` is not
  exposed by any IPC command, but a spawned agent inherits the environment, so
  code execution implies key access. This follows from the code-execution
  primitive above rather than adding to it.

## Reporting

Security issues should go to the process in [`SECURITY.md`](../../SECURITY.md),
not to the public issue tracker.
