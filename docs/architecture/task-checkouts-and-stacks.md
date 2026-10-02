# Task Checkouts and stacks: decision

This note records how SlashIt creates a Task's isolated working copy, how a
Task that builds on another Task is stacked, and which tools that behavior
does and does not depend on. It describes `main` today; what is not built is
marked **not covered**. Vocabulary is in [product-model.md](product-model.md);
paths and storage are in [state-locations.md](state-locations.md#worktrees).

## Decision

Local Git is the source of truth for a Task's checkout and branch. SlashIt
drives it directly with `git`. No other tool decides where a checkout lives,
where a branch starts, or what a Task is stacked on.

| Core behavior depends on | Does not depend on |
|---|---|
| `git` | Worktrunk (`wt`) |
| | git-spice, or any stacking tool or its state |
| | `jj` on `PATH` |
| | a forge, for creating and working in a checkout |

Creating and working in a checkout needs no `gh` or GitHub. `gh` is used for
pull request operations and for importing GitHub issues (see
[Forge boundary](#forge-boundary)).

## Task Checkout creation

A new Task Checkout is a Git worktree created natively
(`WorktreeManager::create_or_adopt` in `worktree/manager.rs`):

1. The base is resolved from local refs, never from the primary checkout's
   `HEAD` (`worktree::default_base::resolve_default_base`; the order is in
   [product-model.md](product-model.md#base-of-a-tasks-branch)).
2. The branch is created at that exact commit with a create-only
   `git update-ref`, so it has no upstream whatever `branch.autoSetupMerge`
   says.
3. The worktree is attached with `git worktree add -- <path> <branch>` at the
   managed path under SlashIt's data directory. A worktree that cannot be
   added takes the branch it just created with it, so a retry is not blocked.
4. The checkout and where it started are recorded on the Task
   (`Task.worktree_path`, `branch_name`, `base_commit`, `branch_origin`)
   before an agent starts in it.

### Worktrunk is not a dependency

Whether `wt` is installed makes no difference to behavior. Earlier versions
delegated placement to it when it was on `PATH`, with its hooks disabled. That
delegation is gone; the `[worktree] placement` values `"managed"` and `"auto"`
both mean SlashIt's own root (`config/paths.rs`).

### Existing worktrees are adopted, never deleted

Before creating or reattaching anything, SlashIt looks for a worktree Git
already has registered for the Task's branch, at the managed path, the legacy
sibling path, or any other path (for example one `wt` placed under its own
template). It uses that worktree where it is
(`WorktreeManager::adopt_existing`, `adopt_any_registered`). The repository's
own primary checkout is never adopted. An adopted checkout records no starting
commit or origin, because its `HEAD` is not where the branch started. SlashIt keeps no
git-spice state, so there is none to migrate, and a Worktrunk placement is just
a Git worktree registration.

## Stacked Tasks

A Task that depends on another Task is stacked on the dependency's branch. A
stack is made of ordinary Git branches. Nothing else records it.

- **Creation.** `WorktreeManager::create_stacked_branch` reads the
  dependency's local branch tip, creates the Task's branch at exactly that
  commit, and attaches a worktree. The dependency must have a usable local
  branch, or the Task is refused rather than started off the default base. A
  branch of the Task's name left by an unfinished attempt is reattached only
  while it still contains the dependency's tip; otherwise it is refused and
  left alone.
- **Persisted semantics.** Two fields on the Task hold the stack, written
  when the branch is created and not inferred later from the dependency list
  or the branch's tip:
  - `Task.branch_origin = BranchOrigin::Stacked { parent_branch }` says what the
    branch currently starts from, and therefore what its pull request
    targets while the parent is open. The other values are `DefaultBase` and
    `LocalBase` (see `domain/task.rs`).
  - `Task.base_commit` is the parent's tip when SlashIt created, or resumed an
    unrecorded, stacked branch. For a stacked Task it
    is the fork point: `base_commit..<tip>` is exactly the Task's own
    commits.

  Both change only when SlashIt itself rewrites the branch onto another base,
  in the restack flows below. Branches recorded by earlier versions may carry
  no origin and keep what they have.
- **Pull request base.** `pr_base_for` in `commands/pr.rs` opens the pull
  request against the recorded `parent_branch` while the parent's pull request
  is open, and against the default branch once the parent is found merged. A
  Task with dependencies whose origin is unknown is refused rather than
  guessed.

### Parent integrated, child not published

When a Task's pull request is created, `restack_onto_landed_parent`
(`commands/pr.rs`) checks whether the parent's pull request was merged into
the default branch. If its commits landed as new commits (squash or rebase
merge), the child's own commits are replayed with
`git rebase --onto <landed base> <fork point> <branch>` (`worktree/restack.rs`)
before the first push, so the pull request does not list the parent's old
commits. This needs no extra approval and runs only when it is
proven safe: the branch has no pull request and is not on origin, the fork
point is still an ancestor of the tip, the worktree is clean, and the replay is
verified by patch-id. The old tip is kept in
`refs/slashit/restack-backup/<task>` and the replay is rolled back on failure.
Success records the new `base_commit` and turns the origin into `DefaultBase`.
A parent merged with a merge commit needs no rewrite and is not restacked.
Anything that cannot be proven refuses before anything is pushed, with the
manual commands.

### Parent integrated, child already published

A child whose branch is on origin with an open pull request is restacked only
on the user's explicit approval from the task drawer
(`commands/pr/republish.rs`, `Task.pending_republish`). The same replay is
used, then:

- the plan is persisted before anything changes, and the old tip is held in
  `refs/slashit/republish-backup/<task>`;
- the branch is pushed with
  `git push --force-with-lease=refs/heads/<branch>:<approved tip>`, so the
  push succeeds only while origin is still at the tip the user approved;
- the pull request base is then converged to the default branch, edited only
  when it differs, so GitHub having retargeted it already counts as done. A
  merge-commit landing is retarget only, with no rewrite;
- an interrupted restack is recoverable with Resume or Discard. While it is
  pending, agent runs, reviews, pull request operations and every cleanup of
  the Task are refused; unrelated Tasks are unaffected.

This is the one place SlashIt force-pushes, and only with that lease.

### Not covered

A child branch that is already on origin with no open pull request (never
opened, closed, or merged) is not restacked: the published flow needs an open
pull request and the unpublished flow refuses a branch already on origin. The
user is told how to restack by hand. Whether SlashIt should handle this case is
an open product decision, which is why the issue tracking stacked children
after a parent merge (#60) is not closed by this behavior. Deleting a whole
Project removes the Task record that holds `pending_republish`; the Git state
survives, but recovery through the record is lost.

## Forge boundary

Local Git decides what exists and where it started: the checkout, the branch,
its base, the fork point, and every rewrite. A forge is not consulted to create
or place a checkout or branch. `gh` is used for pull request operations:

- creating a pull request, finding one for a branch, retargeting it, reading
  review state, and replying to review (`gh`);
- learning that a parent's pull request was merged and into which branch, and
  the commit it landed as. A restack then fetches only that one
  remote-tracking ref (`restack::fetch_remote_branch`) and verifies the landed
  commit is in it.

Creating a checkout never fetches and never asks GitHub. A repository with no
remote runs, commits and reviews Tasks normally. GitHub's native stack
features are not used. If they are used later, they sit behind a capability
check and add to this behavior; they are never the record of what is stacked on
what, and no `gh` extension is required.

## `jj` is optional

Core behavior needs only `git`. In a repository colocated with Jujutsu, `jj`
is used if it is installed, for Jujutsu-specific operations such as
initializing a colocated repository (`worktree::vcs_init`), the `jj` view and
commands (`jj/`), exporting to Git before a push, and a trunk fallback when
choosing the default base. Work in a Task Checkout is committed with `git`.
When `jj` is missing, those paths are skipped or the Git path is used, and Task
Checkouts, commits and restacks work as in a plain Git repository. A Jujutsu
repository that is not colocated with Git cannot get a Task Checkout (see
[product-model.md](product-model.md#task-checkout)).
