# IPC security model

SlashIt exposes a control channel so the `slashit` CLI, and any daemon, can
drive a running instance. This document states what that channel can do, what
protects it, and what would have to exist before it could accept a connection
from another machine.

The short version: **the IPC channel is a code-execution channel**. Local
transports are protected by the operating system, TCP is off by default and
authenticated when on, and remote access is refused rather than shipped weak.

## Layers

The crate is split so that adding a transport never means touching a command,
and changing a command never means touching a platform:

| Layer | Module | Responsibility |
|---|---|---|
| Protocol | `protocol.rs` | the command set, the envelope, versioning |
| Framing | `framing.rs` | one newline-delimited JSON value, size-bounded |
| Transport | `transport/` | moving bytes, and what the OS proves about the peer |
| Authentication | `auth.rs` | bearer tokens for transports the OS does not control |
| Configuration | `config.rs` | which listeners exist, where clients look |
| Endpoint | `endpoint.rs` | platform-native addresses, bind-address policy |

## Transports

| Platform | Local transport | Peer identity established by |
|---|---|---|
| Linux, macOS | Unix domain socket at `$XDG_RUNTIME_DIR/slashit-app/slashit.sock` | the 0700 directory containing it |
| Windows | named pipe `\\.\pipe\slashit-<user>` | an explicit owner-only DACL (current user SID + `LocalSystem`); remote clients rejected |
| any | loopback TCP, **off by default** | nothing — a bearer token is required |

When `XDG_RUNTIME_DIR` is unset (normal on macOS and Windows) the fallback is
`<tmp>/slashit-<uid>`, never a fixed unqualified name: `/tmp` is world-writable
and shared between users, so a predictable path there can be pre-created or
squatted by another local user.

`endpoint::runtime_dir()` is the single source of truth for that directory.
`AppPaths` defers to it rather than deriving its own, because the two
disagreeing is not cosmetic — the server would bind one path while the PID file
and every diagnostic pointed at another.

### Why the directory, not the socket

Permissions on a socket file can only be applied *after* `bind()` returns,
which leaves a window in which the socket exists at its final path with the
process umask. A 0700 parent closes that window because no other user can
traverse into it. The 0600 on the socket itself is belt and braces for the case
where the directory is ever relaxed.

### Endpoint ownership

A socket left behind by a crash must be cleared, but one belonging to a live
instance must not: removing it and rebinding silently steals every subsequent
client connection from the running app. Connecting is the only reliable
liveness test — a bound socket accepts, an orphaned inode refuses. On Windows
the equivalent is `first_pipe_instance(true)`, which fails if another process
already owns the name.

This mattered less when only the GUI could bind. With a daemon it is routine
for two instances to be started, so the check is load-bearing.

## What a client can do

Most verbs are read-only. Three are not, and together they are a full
code-execution primitive:

```text
CreateTask  { project_id, title, description, ... }
MoveTask    { task_id, status: "in_progress" }
EnqueueTask { task_id }
```

Moving a task into `in_progress` reaches the queue executor, which creates a
worktree and spawns the configured agent with the task's attacker-controlled
description as its prompt, using `--dangerously-skip-permissions` and the full
`Read,Edit,Write,Bash,Glob,Grep` tool set.

There is no sandbox between "wrote a string into a task description" and
"arbitrary command execution in a checkout of your repository". These verbs are
marked in the protocol by `IpcRequest::spawns_agent()`, and the classification
is tested so a new verb cannot quietly join them unnoticed.

That is acceptable for a local, owner-only endpoint: an attacker who can
already write to a 0700 directory owned by you can equally run the command at a
shell. It is not acceptable for anything reachable over a network.

## Authentication and authorization

Two questions, answered in order, for every request:

1. **Did the OS establish who this is?** A Unix socket or a named pipe gives
   `TransportAuth::OsVerifiedOwner`. TCP gives `RequiresToken`, and the request
   is refused unless the envelope carries a token matching the one stored in
   `credentials.toml` (mode 0600). The comparison is constant-time; a
   byte-by-byte compare that returns early leaks the matching prefix length
   through timing.

2. **Is this peer allowed this verb?** A peer that is not OS-verified is denied
   every `spawns_agent()` verb outright, regardless of a valid token. A loopback
   TCP client can read status; it cannot cause code to run.

Every mutating request is written to an audit line with the verb, the endpoint
and whether the peer was OS-verified. Payloads are never logged — task
descriptions are user data — and neither is the token.

## TCP

Off by default. When enabled in `ipc.toml` it binds loopback only:

```toml
[tcp]
enabled = false
address = "127.0.0.1"
port = 8731
allow_remote = false
```

`check_bind_address` refuses any non-loopback bind, including `0.0.0.0` and
`::`, before the listener is created, so a refused address is never briefly
live. Setting `allow_remote = true` does not make remote access work — the bind
is still refused. The option exists so the refusal can name the missing
security controls instead of the missing option.

## Remote access

Not implemented, deliberately. Shipping the current command set on a network
listener would be publishing a remote shell.

Before any off-loopback listener may be enabled, **all** of the following must
exist:

1. **Mutually authenticated transport.** TLS with client certificates, or an
   equivalent. Not "plaintext behind a VPN" — a VPN is a network boundary, not
   an authentication decision, and it fails open for anyone already inside it.
2. **A capability model.** Verbs separately grantable, with the agent-spawning
   verbs denied by default. The `spawns_agent()` classifier is the hook this
   would build on.
3. **Per-identity authorization.** Which projects a credential may touch, not
   just whether it may connect.
4. **Rate limiting and connection caps**, so a credential leak is not also a
   denial-of-service vector.
5. **An append-only audit log** of every mutating request with its
   authenticated identity, written outside any project directory. The current
   audit line is a start, not this.
6. **Explicit opt-in per listener**, with the bound address shown in the UI
   while it is live.

Anything less should be refused rather than shipped weak. A partial
implementation here is worse than none, because it looks like security.

## Protocol hardening

| Control | Implementation |
|---|---|
| Bounded requests | 1 MiB cap; an oversized request is reported as too large, not parsed as a truncated prefix |
| Framing | one newline-delimited JSON value per message, enforced in one place for every transport |
| Versioning | `IpcEnvelope.version`; a mismatch is an explicit message naming both versions |
| Legacy clients | a pre-envelope bare request gets a protocol-version message, not a serde error |
| Timeouts | a peer that connects and stalls cannot hold a connection open indefinitely |
| Malformed input | rejected with a clear message |
| Endpoint theft | refused; see endpoint ownership above |

## Remaining gaps

Real and currently accepted:

- **The audit log is a log line, not an append-only store.** Sufficient for a
  local channel; item 5 above for anything more.
- **Secrets remain reachable indirectly.** `AgentConfig::api_key` is not
  exposed by any IPC command, but a spawned agent inherits the environment, so
  code execution implies key access. This follows from the code-execution
  primitive rather than adding to it.

## Reporting

Security issues should go to the process in [`SECURITY.md`](../../SECURITY.md),
not to the public issue tracker.
