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

**Planned, not yet modeled:**

- first-class Workspace defaults and configuration;
- aggregate views across member Projects;
- coordinated operations over several Projects;
- broader agent-context behavior beyond the execution working directory.

Workspace defaults and configuration are planned as a capability of the
Workspace itself, not as a separate entity.

## Task Checkout

"Task Checkout" is the product term for a Task's isolated working copy.
Product text, UI copy and user-facing docs use it. Backend docs and code name
the concrete backend, "Git worktree", when the implementation matters.

Today every Task Checkout is a Git worktree on its own branch, recorded on
the Task as its checkout path, branch name and base commit. SlashIt places
it under its data directory, or leaves placement to worktrunk when the user
has configured it; either way it is a Git worktree (see
[state-locations.md](state-locations.md#worktrees)).

A JJ workspace is not an available Task Checkout backend. A Jujutsu
Project gets Git worktrees for its Tasks only when its repository is
colocated with Git; a Jujutsu-only repository cannot get a Task Checkout.

Workspace and Task Checkout are distinct concepts at different levels:

| | Workspace | Task Checkout |
|---|---|---|
| Level | Several Projects | One Task |
| Holds | Shared context and, later, configuration | The files a Task edits |
| Lifetime | Long-lived, created by the user | Exists while its Task needs it |
| Is a working copy | No | Yes |

A Workspace root is a folder, but it is not meant to be a checkout of any
Project, and agents edit Task files in the Task Checkout, not through it. Treating the two as one concept
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
| Task Checkout | `Task.worktree_path`, `branch_name`, `base_commit`; `worktree::WorktreeManager` |
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
