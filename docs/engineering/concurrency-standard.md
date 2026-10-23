# Concurrency and reliable state: technical reference

Status: internal reference for the rules in [async-safety.md](async-safety.md).
It describes SlashIt as it is on `main`. Work that exists only on an open pull
request is marked as such.

## 1. Versions and boundaries

Pinned in `Cargo.lock`: tokio 1.50.0, tokio-util 0.7.18, tauri 2.11.6. The
release notes and documentation of these exact versions govern. Newer docs
may differ.

The main concurrent parties are:

| Party | Where | Owns |
|---|---|---|
| Desktop UI (Leptos, WASM) | `src/` | Presentation and invoke calls only. It holds no authoritative state. |
| Tauri backend | `src-tauri/src/` | `AppState`, command handlers, the Tauri event bridge. |
| Queue executor | `src-tauri/src/queue/executor.rs` | Starting, running and settling a Task's coding run. |
| Lifecycle authority | `src-tauri/src/lifecycle.rs` | The per-task lease and the terminal transitions. |
| Coding Worker process | a child process, one per run | Its own process group, ended by SlashIt. |
| CLI and daemon control channel | `crates/slashit-ipc/`, `crates/slashit-cli/` | Framing, transport and command protocol. |
| Durable storage | `crates/slashit-durable-fs/` | Atomic replacement of one file, with directory sync. |

The UI must not decide a state transition that the backend commits. A UI
callback that starts a critical operation is a bug of ownership, even when the
backend also checks it.

## 2. Owners and lifecycles

Every spawned operation has an owner who observes its result and ends it.

- A run has one owner, the executor task that started it. It records the
  outcome on the Task. A run that outlives its executor is an orphan, and the
  executor must be able to find and end it.
- A child process belongs to the run that spawned it. Stopping the parent does
  not stop grandchildren, so a process group, not the immediate child, is the
  unit that SlashIt must end. Check that a change to process spawning keeps
  this.
- A per-task lease in `lifecycle.rs` serializes start, stop, worktree
  creation and removal, and delete for one Task. An operation that changes a
  Task's worktree or agent execution must take it first.
- A provider lease (`src-tauri/src/agents/provider_lease.rs`) marks that a
  provider process may hold a Task Checkout. A new start must prove the old
  lease is free. A test for a second start should show that it refuses while
  the first holds the lease.

Shutdown must answer three questions for each operation: does it drain, does
it checkpoint, or does it stop with its outcome unknown? An operation that is
cut short must not be recorded as failed if its effect may have happened.

## 3. State and commit points

- A Task change is durable when `lifecycle::commit_task` returns `Ok`. A
  missing Task is `Ok(None)` in the normal case. `commit_task` returns an error
  when the Task carries a worker recovery refusal, which fails closed for every
  caller of this transition, and when `record` fails. A delete that races the
  write is one such `record` failure, and nothing is written.
- Files are replaced with `slashit_durable_fs::write_atomic`. It stages a
  temporary file, syncs it, renames it, and syncs the directory. Code that
  writes Task or config state directly, without this path, must justify why.
- A value that gates an action is persisted before the action is published.
  Otherwise a restart can resume the action with no record of its approval.

## 4. Approvals and identity

A queued run carries a binding (`approved_execution`) that names the approval,
the run it authorizes, a digest of the input, and an optional additional
request and checkout. At start, `queue::prompt::check_approved_input`
recomputes the digest in the same write that records the start.

The digest must cover every field that changes what runs. On `main`, the digest
covers the prompt, the model and any additional request, but not the Coding
Worker, because `main` runs only the Claude Worker.

Open item, to remove when PR #243 merges: that PR adds the Worker to the digest
for a non-default Worker, so a Worker change after approval returns `Differs`
and the start fails closed. The Claude digest formula is kept, so approvals
for existing Claude Tasks still match.

A value changed after approval that is not covered by the digest is a defect.
Check each new field that the prompt, the model, the Worker or the checkout
depends on against the digest before merging.

## 5. Subprocesses and cancellation

- A cancellation or timeout ends the process group. A user stop settles the
  Task to `Backlog` and records it as stopped, not as failed. It does not roll
  back effects already made in the Task Checkout. An ambiguous failure, where
  an effect may have happened, is reported as outcome unknown.
- Output pipes must be drained or redirected. A child that fills an undrained
  pipe blocks, and that can look like a hang.
- A Worker that the policy refuses must end its run in a state the executor
  classifies. A refused permission request is not a success. The executor
  decides from the checkout whether work was done.

Open item, to remove when PR #243 merges: the OpenCode Worker exists only on
that pull request's branch. Its permission behavior and its limits are in that
pull request's review record, not here.

## 6. IPC and correlation

- Tauri commands are request/response over `invoke`. A command that starts
  long work must return an accepted or enqueued result, not the final result,
  unless it waits for the outcome and says so.
- The CLI and daemon channel (`slashit-ipc`) bounds frames in `framing` and
  authenticates by transport in `auth` and `transport`. A change to framing
  must keep the bound and the error on overflow.
- A pending request map must remove its entry on write failure, timeout,
  cancellation and disconnect. A late reply must not settle a newer request.

## 7. Channels and waiting

- Prefer a bounded channel with an explicit overflow behavior over an
  unbounded one.
- `Notify` and `watch` do not count events. Use them to wake a consumer that
  re-reads state, not to deliver a sequence of events.
- `tokio::select!` drops the branches that did not win. Check each branch for
  cancellation safety. `read_exact`, `read_to_end` and `write_all` are not safe
  to restart after partial progress.
- Mutex guards from `std::sync` are not held across `.await`.

## 8. Tests that exercise the contract

The evidence ladder for SlashIt:

| Change | Minimum evidence |
|---|---|
| Pure logic, no shared state | Unit test and the failure case. |
| Shared state, approvals, binding | Both orderings of the competing change (approval first, change first), and a restart or reload. |
| Process lifecycle | Cancellation and timeout, process-group end, and the lease released afterward. |
| IPC | Out-of-order replies, disconnect, late reply, and pending-entry cleanup. |
| Persistence with an external effect | A failure before and after commit, a duplicate retry, and a reload. |

Test helpers that matter:

- `test_helpers::PATH_LOCK` serializes every test that changes `PATH`. It is
  not reentrant. A test that installs two fake commands with `FakeProgram`
  deadlocks on its second install. Install one and add the rest with
  `FakeProgram::add`.
- A regression test for a fixed ordering defect needs a negative control.
  Remove the fix temporarily, confirm the test fails, restore the fix, and
  record both results. A test that passes both ways does not guard the bug.
  A new ordering test with no fix to remove forces its adverse order with a
  barrier or an injected error, as rule 9 of the async-safety rules requires,
  and a negative control is then optional.
- A test that waits must bound the wait and fail with a message. A bounded
  polling loop with a deadline is acceptable. A fixed sleep is not evidence.

Loom and Shuttle are optional and are described in
[async-safety.md](async-safety.md).

## 9. Open items

These are known gaps, not claims that the code is safe:

- Restart recovery for every interrupted provider run is covered by tests for
  the lease and the pending binding, not for every crash point.
- Cross-process approval races between two app instances are not covered by an
  in-process lock. Treat any shared-file decision as needing a durable compare.
- The effect of logging volume on timing has not been measured. Do not use a
  log line as evidence of an ordering.
