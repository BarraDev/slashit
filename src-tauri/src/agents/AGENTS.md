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
  headless `-p` run cannot hang. `--restricted` needs Claude Code 2.1.248 or
  newer; an older CLI fails the run, which never falls back to running
  without the flag. `restricted_unsupported_reason` turns that CLI error
  into a message telling the user to update.
- Use `ReadOnly` for every run whose prompt carries text SlashIt did not
  write (PR comments, review bodies, diffs) unless the run must edit files.
  Today that is the PR triage, discuss and dry-run helpers
  (`commands/pr.rs`) and the executor's AI review leg.
- `ToolAccess::Full` is for agents that edit code: the coding agent, the
  approved PR apply agent and the review-fix agent. They have Bash.

`claude_args` builds the argument list; test capability changes there.

## Prompt transport

The prompt never goes in argv. `claude_args` passes a bare `-p`, and
`ClaudeRunner::start_program` writes the prompt to the child's stdin from a
separate task, then closes it; the CLI reads stdin to EOF before it starts.
An argument is capped at 128 KiB on Linux and is readable by any local
process, and prompts carry review text and diffs of any size. This holds for
every run, `ReadOnly` and `Full` alike. `--append-system-prompt` stays in
argv: SlashIt only passes a fixed rules text there.

- The writer starts at spawn and runs alongside the stdout and stderr
  drains, so a large prompt cannot deadlock against a full output pipe. The
  CLI also stops waiting for stdin if no data arrives within a few seconds.
- A failed prompt write fails the run, even on exit 0: the write errored
  (usually `EPIPE`), or was still unfinished when the child exited. A prompt
  small enough to sit whole in the pipe buffer counts as delivered once
  written, so a child that exits without reading it cannot be told apart.
  `wait()` reports a non-zero exit first, then the prompt failure, then a
  `result` error. `prompt_failure()` exposes it to callers that judge by
  `exit_status()`, like the PR helper.
- `kill()` and dropping the runner abort the writer, so a cancelled run
  never leaves it blocked on a pipe that a process outside the group still
  holds open.
- A stand-in `claude` in a test must read its stdin to EOF before answering,
  as the real one does. One that exits 0 without reading can fail the run.
