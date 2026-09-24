# AGENTS.md (src-tauri/src/agents)

Claude Code agent execution.

## Architecture

- **mod.rs** - Declares the `roles` and `runner` modules
- **roles.rs** - `roles::AgentRole`, `AgentSlot` and `AgentSlotStatus`
- **runner.rs** - `ClaudeRunner`, which runs the Claude Code CLI as a child
  process and streams its output as `ClaudeEvent`s

The Agent Communication Protocol client lives in `acp/`, not here.
