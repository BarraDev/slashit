# AGENTS.md (src-tauri/src/agents)

Claude Code agent execution.

## Architecture

- **mod.rs** - Declares the `roles` and `runner` modules
- **roles.rs** - `roles::AgentRole`, `AgentSlot` and `AgentSlotStatus`
- **runner.rs** - `ClaudeRunner`, which runs the Claude Code CLI as a child
  process and streams its output as `ClaudeEvent`s

The Agent Communication Protocol client lives in `acp/`, not here.

## Tools and permissions

The `tools` field of `ClaudeRunConfig` is a `ToolAccess`, and that choice is
the security boundary of the run:

- `--allowedTools` only pre-approves tools. It removes nothing, and under
  `--dangerously-skip-permissions` every tool is approved anyway. Never use
  it as a restriction.
- `ToolAccess::ReadOnly` is the restriction: `--tools Read,Glob,Grep` (the
  only tools that exist), `--disallowedTools` for the rest, `--restricted`
  (file tools confined to the working directory, no user, project or local
  settings files and so no hooks from them), `--strict-mcp-config`, and
  `--permission-mode dontAsk`, which denies instead of prompting, so a
  headless `-p` run cannot hang.
- Use `ReadOnly` for every run whose prompt carries text SlashIt did not
  write (PR comments, review bodies, diffs) unless the run must edit files.
  Today that is the PR triage, discuss and dry-run helpers
  (`commands/pr.rs`) and the executor's AI review leg.
- `ToolAccess::Full` is for agents that edit code: the coding agent, the
  approved PR apply agent and the review-fix agent. They have Bash.

`claude_args` builds the argument list; test capability changes there.
