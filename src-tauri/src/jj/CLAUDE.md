# CLAUDE.md (src-tauri/src/jj)

Jujutsu (jj) version control system integration.

## Modules

- **mod.rs** - Command handlers for Tauri IPC
- **workspace.rs** - JJ workspace operations
- **backend.rs** - Backend abstraction for JJ operations
- **manager.rs** - High-level JJ state management

## Tauri Commands Exposed

- `new_change` - Create a new JJ change
- `describe_change` - Add description to a change
- `abandon_change` - Abandon a change
- `jj_get_workspace_status` - Get current workspace status
- `git_export` - Export changes to git

## Pattern

JJ commands are thin wrappers around `manager::JjManager` which handles the actual `jj` CLI execution via shell commands.
