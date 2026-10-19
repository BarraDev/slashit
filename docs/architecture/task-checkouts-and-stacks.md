# Task Checkouts and stacks: decision

This note records how SlashIt creates a Task's isolated working copy, how a
Task that builds on another Task is stacked, and which tools that behavior
does and does not depend on. It describes `main` today; what is not built is
marked **not covered**. Vocabulary is in [product-model.md](product-model.md);
paths and storage are in [state-locations.md](state-locations.md#worktrees).

## Decision

Local Git is the source of truth for a Task's checkout and branch. SlashIt
drives it directly with `git`. No other tool decides where a checkout lives or
what a Task is stacked on, and Git creates every branch. Where a branch starts
is SlashIt's choice from local refs; in a Jujutsu repository, when `jj` is
installed, its `trunk()` is one input to that choice (see
[`jj` is optional](#jj-is-optional)).

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
stack is made of ordinary Git branches; no tool's state records it. The Task
record stores its parent and fork point (below).

- **Creation.** `WorktreeManager::create_stacked_branch` reads the
  dependency's local branch tip, creates the Task's branch at exactly that
  commit, and attaches a worktree. The dependency must have a usable local
  branch, or the Task is refused rather than started off the default base. A
  branch of the Task's name left by an unfinished attempt is reattached only
  while it still contains the dependency's tip; otherwise it is refused and
  left alone.
- **Which base a new branch starts from.** The executor reads the Task's
  dependencies when it creates the branch; a Task with an existing branch is
  reattached and reads none. An explicit checkout (`create_worktree`) takes the
  same start, through the same executor path, so it can never give a Task a
  branch that an execution would not have given it. A Task holding an approved
  additional request gets its approved checkout instead, and that checkout is
  refused if it has changed since approval. The first matching rule decides:
  - One dependency with no branch recorded has no work SlashIt made, and the
    branch starts from the default base.
  - One dependency whose approved work was integrated locally, and whose
    approved tip is an ancestor of the base, starts from the base, so it
    receives exactly the approved commits.
  - One dependency that still has a branch, and is not shown to be in the
    base, is stacked on that branch. This includes a Task that is Done with no
    pull request: Done does not prove its work reached the base.
  - One dependency whose recorded branch is gone starts from the base only when
    its pull request was recorded as merged and the merge commit recorded with
    it is in the base. Otherwise the start is refused and names the branch, the
    pull request, or the commit the base lacks.
  - A dependency that is no longer on the board refuses the start.
  - A Task with several dependencies (legacy records; a new configuration names
    at most one) starts from the base only when every dependency with a
    recorded branch is shown to be integrated into it, or its pull request was
    recorded as merged with a merge commit in the base and its branch is gone.
    Otherwise the start is refused and names the first dependency that is not.
    It is never stacked on one of them. A dependency whose pull request is
    recorded as merged but whose branch still exists is refused here, although
    a single dependency in that state is stacked on its branch: the start stays
    conservative until the branch is deleted or its work is shown in the base.

  **Merge evidence.** A merge is proven by the merge commit GitHub reports for
  its pull request, not by the Task's approved tip, because a squash, rebase or
  merge-commit landing each produce a different commit on the default branch.
  The commit is recorded on the pull request reference together with the
  `MERGED` state, when the merge is observed by the background poll or by an
  explicit refresh of the Task's pull request. A merge recorded any other way,
  such as linking an existing pull request that is already merged, records no
  commit. A start checks that the merge commit is an ancestor of the base it
  resolves, and creates the new branch at that same resolved base commit.
  Nothing is fetched during the start. A merged reference with no recorded
  commit is refused, and Refresh on the dependency's Task card records the
  commit. A refresh writes only that commit; it does not finish the Task again.
  A base that does not contain the commit is refused, and the recovery is to
  fetch the repository (or update the project's base branch) and start again.

  A Task whose start adopts an existing checkout of its own branch (one created
  outside a start, or left by an unfinished one) adopts it only when that
  branch contains the commits its dependencies' work is known to be in: the
  approved tip of a dependency integrated locally, and the merge commit of a
  dependency whose pull request is recorded as merged. A dependency with no
  branch requires nothing, and the base the start resolves now is not
  required, so a checkout left before the base moved is still adopted. Git
  confirming that the checkout belongs to the branch is not enough. A branch
  that lacks one of those commits is refused and left as it is; bringing it up
  to date is a decision for the person.

  **Durable admission evidence.** A Task records a versioned
  `CheckoutAdmission` in the same durable board write that first records its
  checkout identity. It binds the Task and repository IDs, branch, a commit
  observed on that branch at admission, the exact dependency Task IDs and the
  commits checked for each, plus the format and admission-policy versions. A
  branchless dependency is recorded explicitly with no required commits; an
  empty dependency list means the Task had no dependencies. The admission
  commit is a Git ancestry boundary, not a replacement for `base_commit` and
  not a claim about the Task's diff boundary when the checkout was adopted.

  The planner records the same commits it proved before creating or adopting a
  new checkout: the dependency branch tip for a stacked start, the locally
  integrated approved tip, or the recorded merged-PR commit. Later starts
  require those commits to remain ancestors of the admission commit and the
  admission commit to remain an ancestor of the Task branch. They do not
  require a dependency branch to remain at its old tip. Dependency-list edits
  require a fresh admission check.

  An existing supported admission is checked against the Task ID, registered
  repository ID, recorded branch, current dependency IDs and Git ancestry
  before SlashIt reattaches the worktree. Missing, malformed or unsupported
  evidence is unknown, never inferred from a branch, `base_commit`, a run
  marker or a timestamp. A legacy checkout can be requalified without
  changing it only where saved stable evidence identifies the requirement
  (for example, a local integration's approved commit or a merged pull
  request's merge commit) and Git proves that commit is present. A stacked
  legacy checkout can also be requalified from its recorded parent branch and
  fork point when the Task still depends on that parent and both branches
  prove the fork point by ancestry. Other dependency-bearing legacy checkouts
  remain untouched and refuse ordinary execution until their requirements can
  be established.

  Exact-checkout approvals and Worker recovery decisions remain separate
  authorizations. Each continues to bind the checkout and state its existing
  contract requires; neither is converted into dependency-admission evidence.
  Recovery of an interrupted run may proceed only through its existing
  exact-checkout decision. An older SlashIt version may drop this optional
  field when it saves a Task; the next version then treats the evidence as
  unknown and requalifies or refuses. A supported restack carries the record
  forward in the same durable Task write when exact required commits remain on
  the landed base. For the exact stacked parent whose merge commit was just
  verified, its old branch commit requirement may be replaced with that merge
  commit when the old work is in the recorded fork. Other missing requirements
  refuse before changing the checkout. Dependency branch advances alone do not
  invalidate a supported admission.

  Admission validation is read-only and runs before reattaching an existing
  branch. A refusal leaves its refs, checkout, branch and user files in place.
  New branch creation and explicit checkout creation use the same planner and
  persist admission with checkout identity; a failed or unconfirmed write
  follows the existing durable-write cleanup rules and never launches a
  Worker without a durable run-start record.

  A refusal creates no branch or worktree, and changes no dependency, branch,
  worktree or user branch. The executor then records the refusal as the Task's
  error, status Error, as it does for any start that cannot attach a worktree,
  so the Task is not retried silently.
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
initializing a colocated repository (`worktree::vcs_init`), the `jj` commands
that PR delivery runs (`commands/pr.rs`), exporting to Git before a push, and
a trunk fallback when choosing the default base. Work in a Task Checkout is committed with `git`.
When `jj` is missing, those paths are skipped or the Git path is used, and Task
Checkouts, commits and restacks work as in a plain Git repository. A Jujutsu
repository that is not colocated with Git cannot get a Task Checkout (see
[product-model.md](product-model.md#task-checkout)).
