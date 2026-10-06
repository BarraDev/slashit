# Product model

This document defines the words SlashIt uses for its own concepts and how
those concepts relate. "Workspace" has meant three different things in this
repository: a group of projects, the isolated copy a task works in, and a
Jujutsu working copy used while developing SlashIt. This document gives each
of them one name.

Where a statement describes behavior, it describes `main` today. Anything not
yet built is marked **planned**.

## Core concepts

**Workspace.** A product-level container over several Projects: the context
in which related Projects are managed together. Unqualified, "Workspace"
always means this Product Workspace, in product text, docs and UI.

**Project.** One project, repository, or root folder managed by SlashIt. A
Project owns its Tasks and its board.

**ProjectScope.** A Project's relationship to Workspaces: either `Standalone`
or `InWorkspace { workspace_id }`.

**Task.** One unit of work that belongs to one Project, tracked on that
Project's board.

**Task Checkout.** The isolated working copy a Task owns while it is being
worked on, so parallel Tasks never edit the same files. This is the
implementation-neutral product term.

**Git worktree.** The only Task Checkout backend that exists today.

**JJ workspace.** A Jujutsu concept: an additional working copy of a
Jujutsu repository. It is **not** a Task Checkout backend. SlashIt's
Jujutsu integration (status and change operations such as new, describe and
abandon) is separate from Task Checkouts.

**Developer workspace.** The isolated checkout a maintainer or agent uses
while developing SlashIt itself, normally a JJ workspace. It is part of the
development process, not of the product.

## Relationships

```text
Workspace  0..1 ── 0..n  Project     (membership stored on the Project)
Project    1    ── 0..n  Task
Task       1    ── 0..1  Task Checkout  (according to lifecycle state)
```

- **Membership lives on the Project.** `Project.scope` is the only record of
  which Workspace a Project belongs to. A Workspace does not keep a list of
  its Projects; the members of a Workspace are the Projects whose scope names
  it.
- **Zero or one Workspace per Project.** `ProjectScope` has no way to name
  two Workspaces. A Project that is in no Workspace is `Standalone`, which is
  also what every Project created by the app today starts as.
- **A Workspace with members cannot be deleted.** Deletion is refused while
  any Project's scope still names it, so no Project is left pointing at a
  Workspace that no longer exists.
- **A Task Checkout belongs to exactly one Task.** It is created or
  reattached when the Task is executed or when a checkout is requested for
  it, and it survives the end of a run, so the next run continues from the
  work already there. It is removed only by an explicit destructive action,
  such as finishing the Task. A Task that has never run, or whose checkout
  has been cleaned up, has none.

## Workspace purpose

A Workspace is meant to be more than visual grouping. Its intended role is
to manage several Projects as one context:

- manage multiple Projects together;
- give an overview across them;
- hold shared defaults and configuration;
- provide shared context to coding agents;
- coordinate work that spans Projects.

Only part of that exists today.

**Current:**

- A Workspace has an id, a name, and a root folder (`root_path`), and is
  kept in a registry outside any Project.
- Every Project is still created `Standalone`. A user can attach a
  standalone Project to a Workspace, and detach a member Project back to
  standalone, from the Workspaces page. Attaching a Project that already
  belongs to a Workspace is refused; detach it first. There is no
  aggregate view across a Workspace's member Projects yet beyond the
  membership list itself.
- When a member Project's Task is executed and the Workspace root exists on
  disk, the coding agent runs from the Workspace root, with the Task
  Checkout added as the directory it edits. Instruction files in the
  Workspace root (for example `AGENTS.md`) therefore act as shared agent
  context for every member Project. If the root is missing, the run falls
  back to the Task Checkout alone. Other agent runs, such as AI review and
  PR review fixes, do not use the Workspace root.
- **Execution and delivery boundary.** The agent may read anything it can
  reach, including the Workspace's instruction files and other member
  Projects. Its writes belong in the Task Checkout, and the coding prompt
  says so. SlashIt's diff, AI review, Human Review, commit and pull request
  cover only the Task Checkout. A file the agent edits elsewhere under the
  Workspace root, such as a sibling Project's primary checkout, is outside the
  Task's delivery: it is not in the Task's diff, not committed, and not sent to
  a pull request. SlashIt does not scan for or report such edits, and a
  Workspace does not make Task delivery span several repositories.

**Planned, not yet modeled:**

- first-class Workspace defaults and configuration;
- aggregate views across member Projects;
- coordinated operations over several Projects;
- broader agent-context behavior beyond the execution working directory.

Workspace defaults and configuration are planned as a capability of the
Workspace itself, not as a separate entity.

## Task-linked coordination

Coordination is opt-in from the Task Drawer for an idle, unapproved Task with
an existing Task Checkout. A Conversation owns its ordered rounds, human
goals and decisions, proposals and approved requests, bounded results and Run
attempts. It is stored separately from Task. Human, Coordinator and Worker
are logical Participants; a fresh provider Run can execute each agent step.
The executor's actual owned handles remain the only live-run authority.

See [task-coordination.md](task-coordination.md) for the human gate,
projection contract, persistence and conservative recovery behavior.

## Task Checkout ownership

"Task Checkout" is the product term for a Task's isolated working copy.
Product text, UI copy and user-facing docs use it. Backend docs and code name
the concrete backend, "Git worktree", when the implementation matters.

Today every Task Checkout is a Git worktree on its own branch, recorded on
the Task as its checkout path, branch name and base commit, and whether the
branch starts from the default base, from the Project's local base, or is
stacked on a dependency's branch (which the Task's pull request then
targets). SlashIt places it under its data directory, and adopts a Git
worktree of the Task's branch that already exists elsewhere (see
[state-locations.md](state-locations.md#worktrees)).

How a checkout is created, how a stack is built and restacked, and which tools
core behavior does not depend on are in
[task-checkouts-and-stacks.md](task-checkouts-and-stacks.md).

### Base of a Task's branch

A Task needs a safe, explicit version-control base. A remote is optional:
running a Task, its checkout, its commits, AI Review and Human Review work
in a repository with no remote at all. Pushing and pull requests are
additional, and are offered only where a supported remote exists.

An ordinary branch starts at an exact commit resolved from local refs when
the branch is created, and never re-derived afterwards:

1. **Origin's default branch**, when `refs/remotes/origin/HEAD` names a
   fetched branch of `origin` (or, in a JJ repository without it, JJ's
   `trunk()` is exactly `<branch>@origin`). The Task records it as starting
   from the default base, and its pull request targets that branch. A
   remote-backed repository behaves as it always did.
2. Otherwise, the **Project's local base** (`Project.base`): a local branch
   (in a colocated JJ repository, a bookmark) captured when the Project was
   registered or its version control initialized, when that was unambiguous
   (Git's `HEAD` on a branch that agrees with `origin`, the bookmark JJ's
   `trunk()` names, or the bookmark Initialize with Jujutsu created), or
   chosen explicitly in Settings > Repository. A colocated JJ repository's
   only bookmark is suggested there, never captured on its own. Switching the
   primary checkout to another branch does not move it. The Task records it as
   starting from the local base; a pull request for it targets that branch on
   `origin` only once `origin` has it and contains the Task's starting commit.
3. Otherwise the Task is refused with what to do. The primary checkout's
   `HEAD` is never used.

A malformed `refs/remotes/origin/HEAD` (pointing outside `origin`) is
refused rather than skipped. "Detect default branch" in Settings >
Repository runs the equivalent of `git remote set-head origin --auto` on
explicit request: it reads origin over the network and writes only
`refs/remotes/origin/HEAD`, and refuses when origin's default branch is
ambiguous.

A folder with no version control, or a Git repository with no commit, is
registered in a setup-required state. It is never initialized on its own;
Initialize (in Create Project or Settings > Repository) creates the repository
without choosing a branch name, so the tool's own configuration names it, and
records the folder's current files, ignore rules applied, as the first commit.
Jujutsu is initialized colocated with Git (`jj git init --colocate`), since
Task Checkouts are Git worktrees. Initializing refuses, changing nothing,
rather than record less than it showed: when the folder holds another
repository (Git, or Jujutsu without Git colocation), when a file exceeds
Jujutsu's own `snapshot.max-new-file-size` (for Jujutsu), or when the folder
no longer holds the files that were shown. A Jujutsu initialization that fails
after `jj git init` removes the `.jj` and `.git` it created (and nothing
else), leaving the folder as it was; if even that fails, the error names what
was left behind. A Git initialization whose first commit fails leaves a
repository with no commit, which initializing again completes.

Branches created by earlier versions may record no origin, or a default
base with no branch, and keep what they recorded. Projects registered by
earlier versions have no local base until one is chosen. The record
describes where the branch starts now, and changes only when SlashIt itself
rewrites the branch onto another base: a stacked branch that was never
pushed, whose dependency's pull request was merged into the default branch,
is replayed onto the default branch before its first pull request is
opened, and from then on is recorded as starting there, at the exact commit
it was replayed onto.

A JJ workspace is not an available Task Checkout backend. A Jujutsu
Project gets Git worktrees for its Tasks only when its repository is
colocated with Git; a Jujutsu repository that is not (`jj git init
--no-colocate`) is recognised and refused, with `jj git colocation enable`
as the way forward, and cannot get a Task Checkout.

Work in a Task Checkout is recorded as a Git commit on its branch: the
coding agent's work when its run ends, and the fixes applied by AI review
and by PR review. This does not depend on `jj` being installed, and `jj`
is not run for it; a colocated jj repository sees the commit when jj next
imports from Git. Nothing is committed while the checkout does not have
the Task's branch checked out, is in the middle of a rebase, merge,
cherry-pick, revert or bisect, or has unresolved conflicts. A commit that
fails is reported on the Task: the agent's own work that cannot be
committed fails the Task, and PR review fixes that could not be committed
are not pushed. An apply in which any fix fails commits and pushes none
of its fixes, since the failed fix may have left partial edits anywhere in
the checkout; every edit stays on disk for the user to review. Applying the
PR review again commits and pushes the fixes an earlier apply made but did
not commit (because the commit failed, was cancelled, or was withheld after
a failed fix), without making them again. A fix already committed is
never committed again, and an apply that made no new fix and has no such
uncommitted fix commits nothing.

Workspace and Task Checkout are distinct concepts at different levels:

| | Workspace | Task Checkout |
|---|---|---|
| Level | Several Projects | One Task |
| Holds | Shared context and, later, configuration | The files a Task edits |
| Lifetime | Long-lived, created by the user | Exists while its Task needs it |
| Is a working copy | No | Yes |

A Workspace root is a folder, but it is not meant to be a checkout of any
Project, and agents edit Task files in the Task Checkout, not through it. Only
the Task Checkout is delivered (see Execution and delivery boundary above). Treating the two as one concept
would suggest that grouping Projects shares their mutable state, which it
does not.

## Developer terminology

Maintainers and agents develop SlashIt in JJ developer workspaces under
`../workspaces/<slug>`. That development infrastructure has nothing to do
with the Product Workspace domain object, and a developer workspace is not a
Task Checkout either. In development-process text, write "developer
workspace" or "JJ workspace" wherever the product meaning could be inferred.

The procedure is in [development-workflow.md](../development-workflow.md).

## Code map

| Term | Code |
|---|---|
| Workspace | `domain::Workspace`, `config::WorkspaceRegistry` (`workspaces.toml`), `commands/workspace.rs` |
| Project, ProjectScope | `domain::Project`, `domain::ProjectScope` (`Project.scope`); attach/detach in `commands/project.rs` |
| Task | `domain::Task` |
| Task Checkout | `Task.worktree_path`, `branch_name`, `base_commit`, `branch_origin` (`domain::BranchOrigin`); `worktree::WorktreeManager` |
| Base of a Task's branch | `worktree::default_base`, `Project.base` (`domain::ProjectBase`), `worktree::project_base` |
| Repository readiness and setup | `worktree::readiness`, `worktree::remote_head`, `worktree::vcs_init`; `commands/repository_setup.rs` |
| Workspace root as agent context | `TaskExecutor::resolve_workspace_launch` in `queue/executor.rs` |
| Task Checkout lifecycle | `lifecycle.rs` |
| Jujutsu integration | `jj/`, `commands/jj.rs` |

## Former names and compatibility

- **"Meta-workspace"** is retired. It referred to what is now simply a
  Workspace.
- **`workspace_id` on a Task** is a legacy name for its checkout reference.
  The field is now `worktree_id`, which current code no longer sets; the
  serde alias that still reads `workspace_id` must stay so older task
  records keep loading.
- **Task workspace identifiers.** Some code still names a Task Checkout
  "workspace", for example `resolve_task_workspace` and
  `no_workspace_refusal` in `commands/pr.rs`, and the unused frontend
  `Task.workspace_id`. They will be renamed to checkout terminology when
  those files are next changed.
