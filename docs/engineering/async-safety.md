# Async safety rules

Status: internal engineering standard. Detailed rationale and examples are in
[concurrency-standard.md](concurrency-standard.md). This page is the short,
binding part. Read it before changing any of the areas in the trigger list.

## Trigger

These rules apply to a change that touches any of:

- a spawned task, timer, child process or background callback that does work
  on Task, Project or Workspace state;
- shared state guarded by a lock, an atomic, a channel or a queue;
- IPC between the UI, the CLI, the daemon or a coding Worker;
- cancellation, timeout, retry, shutdown or process-group handling;
- persistence of Task state, approvals or other records that gate an action;
- provider or Worker selection, and permission or approval policy.

A sequential edit with no shared state, no external effect and no persisted
decision follows the normal validation only.

## Rules

1. **State the contract.** Name what success means, and distinguish
   accepted, persisted, running, completed, failed, cancelled and
   outcome-unknown. Do not report completion from a scheduled or enqueued
   operation.
2. **Name the owner of every spawned operation.** Someone must
   observe its result, cancel it, and wait for it or account for it at
   shutdown. A detached task needs a written lifecycle reason in the code.
3. **Protect logical operations, not memory accesses.** Two atomic operations
   in sequence are not one operation. A check and the write it authorizes
   must happen under one lock, one transaction, one version comparison or one
   owner. A lock inside one process does not protect another process.
4. **Bind approvals to the exact input and identity they authorize.** An
   approval names the input (by its digest), the run and, once a Task can run
   on more than one Worker, the Worker. A change to any of them after approval
   must refuse the start, and the
   refusal must survive a restart.
5. **Make cancellation and timeout honest.** A timeout does not roll back
   effects that already happened. After an ambiguous failure, report the
   outcome as unknown rather than failed, and do not repeat a non-idempotent
   effect without a reconciliation rule.
6. **Correlate IPC replies by ID.** Register the waiter before the write,
   match replies by request ID, remove pending entries on write error,
   cancellation, deadline and disconnect, and bound the frame size.
7. **Bound every queue and retry.** A channel, a pending-call map, a retry
   loop or a worker pool needs an explicit limit or a written reason for
   none.
8. **Do not treat logs, sleeps or yields as ordering.** A `sleep` in a test
   is not synchronization. Logging does not make a race go away.
9. **Test the adverse order.** For each rule above that the change touches,
   a test must force the feared interleaving, failure or cancellation with a
   barrier, an acknowledgement, an injected error or a controlled
   resolution. A test that only runs the happy path is not evidence for these
   rules.
10. **Report what was not exercised.** Use `INCONCLUSIVE` for any claim the
    evidence does not reach. Do not write "safe" for an unverified contract.

## Review gate

A change that matches the trigger list needs a review record in the pull request
description or a linked private report. It states, in this order:

```text
Contract and invariants:
Owner of each spawned operation, and its shutdown:
Shared state and the adverse orderings considered:
Approval or identity binding, and the restart case:
Cancellation, timeout and unknown outcomes:
Tests that force those cases, with the exact command and result:
Not exercised, and residual risk:
Verdict: APPROVE / REQUEST CHANGES / INCONCLUSIVE
```

Reviewers check the record against the diff. A reviewer who finds a rule
broken by the diff, and not by the description alone, requests changes. A
missing record for a change that matches the trigger list is itself a reason
to request changes.

## Tools

- Deterministic barriers and oneshot acknowledgements are the default
  technique. Use them before any schedule exploration.
- Loom and Shuttle are optional. They fit a small, self-contained
  synchronization core that uses their instrumented primitives. They do not
  model real sockets, real child processes or the Tauri runtime, so they are
  not a gate for SlashIt code.
- Do not add a CI framework that only checks whether a template was filled in.
  The review record is checked by people.
