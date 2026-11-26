# Agent review and delivery

An external agent can read a Task's review and delivery state, ask a person to
approve its changes, and, once a person authorizes it, open its pull request.
A person makes every approval and publication decision.

## Verbs

| CLI | IPC request | Effect |
|---|---|---|
| `slashit review <task>` | `GetReviewState` | reads decisions, approval coverage, branch commit, requests (JSON) |
| `slashit delivery <task>` | `GetDeliveryState` | reads the pull request, failures, requests to open one (JSON) |
| `slashit request <task> approve\|open-pr --commit <sha> --actor <name>` | `RequestOperation` | files a question for a person |
| `slashit open-pr <task> --request <id> --actor <name>` | `OpenPr` | spends one authorization |

There is no verb that decides a request, and no `approve_task` over IPC.
Request Changes, merging and force pushes are not reachable this way.

## Mechanism

Operation-specific authorization, confirmed by a person, held by the instance.

1. The agent files a request. It is bound to one Task, one full commit ID, one
   operation and the Task's current Human Review arrival.
2. The Task drawer shows the Task, the commit, the intended action and the name
   the agent gave itself. The person approves or refuses.
3. For `approve`, the person's decision records the Human Review approval of
   that commit, in the same board write. The agent executes nothing. The
   Changes tab must have shown that commit's diff before the drawer offers
   approval; a diff still loading, or loaded behind another tab, does not count.
4. For `open-pr`, a person must already have approved exactly that commit. The
   decision authorizes one use within 10 minutes and publishes nothing.
5. `OpenPr` rechecks the binding and the branch, durably marks the request used,
   and only then runs the same delivery the desktop's Create PR runs, limited to
   the authorized commit.

No code or token is given to the agent. The request ID only names a record, and
the record names the only thing it can be used for, so there is nothing to copy,
replay or move to another Task, commit or operation.

| Property | Rule |
|---|---|
| Expiry | a request waits 30 minutes; an authorization lasts 10. Repeating a decision does not extend it |
| Single use | `used` is written before any effect. A failed or interrupted delivery needs a new request and a new decision |
| Moved branch | refused at decision and at use, and recorded; the branch returning to the commit does not revive the request |
| Changed Task | a different approved commit or an existing pull request invalidates it |
| Left Human Review | the write that moves the Task out retires every undecided request and unused authorization for good; coming back revives none |
| Outcome | `pr_opened`; `pr_unsettled` (the pull request exists, recording it failed); `unconfirmed` (a push was started, no pull request confirmed); `failed` (nothing was pushed). Only the first is an `ok` reply |
| Durability | a write that is in place but not confirmed durable says so. A consumption in that state publishes nothing |
| Restack | a delivery that would restack the branch (a landed parent) is refused, because it publishes another commit; a person opens that one |
| Retry | asking again for the same operation and commit under the same name returns the open request; one request per operation is open at a time; a refused one is not asked again |
| Restart | requests, decisions and uses are stored on the Task. A use with no outcome means the outcome is unknown |
| Headless | `slashitd` refuses to file requests: nobody can be shown them |

The audit record keeps the requesting name, the person's decision and time, the
executing name and the outcome, separately. A Task keeps at most 50 requests;
only ones that never authorized anything are dropped. Portable export omits them.

## Threat model

**Protected against.** An agent, or a script, that uses the `slashit` CLI or
speaks the IPC protocol and tries to approve changes, authorize itself, publish
without a decision, publish a different commit, reuse a decision, or publish
twice after a lost reply or a crash. A client the operating system has not
identified (TCP) cannot file or execute a request at all.

**Not protected against.** A hostile process running as the same OS user. It can
drive the desktop window with input automation, edit the Task files while the
instance is stopped, replace the binaries, or run `git push` and `gh` itself.
The control channel was never a boundary against that user (see
[ipc-security.md](ipc-security.md)), and this design does not make it one. The
authorization governs what SlashIt does and records, not what the user's account
can do.

- The desktop command is reachable from the application's webview, not from the
  control channel. That separation is the approval control. `GuiMutation` is
  only a shutdown guard and authorizes nothing.
- "A person decided" means the desktop command was invoked. SlashIt does not
  authenticate who is at the screen.
- Agent names are self-declared and shown as such. They are for the record, not
  for access control, so any agent may use an authorization that was given.
- An agent can ask repeatedly for new commits. One open request per operation
  and the record limit bound this; there is no rate limit.

## Limitations

- A pending request is shown only in its Task's drawer. It raises no
  notification and does not add the Task to the attention list.
- A request to approve covers a first decision on the changes. Approving again
  after a branch moved stays in the Human Review panel.
