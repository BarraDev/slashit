# CLAUDE.md (src-tauri/src/agents)

Claude Code Agent integration via ACP (Agent Communication Protocol).

## Architecture

- **mod.rs** - Agent trait definition and factory
- **claude_code.rs** - Claude Code agent implementation

## Agent Trait

All agents implement the `Agent` trait from `async_trait`:
- `start()` - Initialize the agent
- `stop()` - Shutdown the agent
- `send_message()` - Send a message to the agent
- `get_status()` - Get current agent status

## ACP Protocol

The `acp/` module implements the Agent Communication Protocol for message passing between the app and external agents.
