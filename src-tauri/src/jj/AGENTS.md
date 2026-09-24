# AGENTS.md (src-tauri/src/jj)

Jujutsu (jj) version control integration.

## Modules

- **mod.rs** - Re-exports `manager`
- **manager.rs** - `JjManager`, which runs the `jj` CLI as a subprocess, and `JjStatus`

## Tauri Commands Exposed

The JJ commands are defined in `commands/jj.rs`, not in this module. That
file also holds `get_task_diff` and `get_task_diff_stat`, which diff a Task's
Git worktree and do not use jj.

- `new_change` - Create a new JJ change
- `describe_change` - Add description to a change
- `abandon_change` - Abandon a change
- `jj_get_workspace_status` - Get the status of a JJ working copy
- `git_export` - Export changes to git

## Pattern

JJ commands are thin wrappers around `manager::JjManager`, which runs the `jj`
binary directly as a subprocess (no shell).

This integration operates on an existing JJ working copy. It is not a Task
Checkout backend: every Task Checkout is a Git worktree (see
`docs/architecture/product-model.md`).
