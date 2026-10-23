# AGENTS.md (src-tauri/src/agents)

Claude Code and OpenCode coding-run execution.

## Architecture

- **mod.rs** - Declares the `coding_run`, `opencode_run`, `provider_lease`, `roles` and `runner` modules
- **coding_run.rs** - `CodingRunner` and `CodingRunEvent`, the provider-neutral run boundary, over `runner.rs` (Claude) and `opencode_run.rs` (OpenCode over ACP)
- **opencode_run.rs** - `OpenCodeRunner`: one `opencode acp` process per run, its pinned permission policy (`OPENCODE_POLICY_CONTENT`), and its per-run configuration directory. Its module doc holds the known limits.
- **roles.rs** - `roles::AgentRole`, `AgentSlot` and `AgentSlotStatus`
- **runner.rs** - `ClaudeRunner`, which runs the Claude Code CLI and streams `CodingRunEvent`s

The Agent Communication Protocol client lives in `acp/`, not here.

## OpenCode policy

An OpenCode run starts with `OPENCODE_POLICY_CONTENT` and project configuration disabled. Read, glob, grep and edit are allowed, and every other tool is denied, including shell, web fetch, subagents and the code runtime. Paths outside the Task Checkout are refused as written. Three limits apply, and they are not closed:

- It is not an OS sandbox. The policy governs the tools OpenCode offers.
- `read` follows a symbolic link inside the checkout to a file outside it.
- Only Unix runs are supported. The provider lease and the process-group checks exist there.

The per-run configuration directory is removed only after its process has been reaped. Read the `opencode_run.rs` module doc before changing the policy or the directory's lifecycle.

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
- `permission_mode: Some("auto")` is the policy for the coding Worker and the
  review-fix agent. It passes `--permission-mode auto` with no
  `--allowedTools` list, so the provider's classifier reviews each call. A
  Claude Code older than `AUTO_MIN_VERSION` is refused before launch
  (`auto_unsupported_reason`); that check is a precondition only.
- Auto is proven by the stream's `init` event, not by the launch. When the
  CLI cannot run Auto (for example `permissions.disableAutoMode`) it starts
  the session in `default` mode without an error. Its tool calls that need
  approval are denied, yet the session still ends with `subtype: success`,
  and the model can retry a denied call many times. The stdout reader
  (`auto_mode_mismatch`) therefore stops an Auto run whose `init` does not
  report `"auto"`, or reports no mode, and `wait()` returns that reason.
- `permission_mode: None` (bypass) remains only for the approved PR apply
  agent, which still needs review. An Auto run never falls back to it.
- `isolate_customizations: true` for the coding Worker and the review-fix
  agent: `--setting-sources project` (the user's settings, hooks, plugins and
  `env` are not read), `--settings <path>` (every hook is off,
  `DISABLE_TELEMETRY` is `1`, the `PINNED_ENV` names are pinned to the
  child's values or empty, and the `HELPER_SETTINGS` commands are pinned to
  empty), and `--strict-mcp-config`. The file is an owner-only
  `IsolatedSettingsFile`, deleted when the provider exits; argv is readable by
  other users. The project's `env` block outranks the environment SlashIt
  passes, so the pins keep a repository from redirecting the API key, the provider, the shell or child processes; a pin
  is a string value, because `null` is skipped as invalid and the setting it
  was meant to disable still runs. The project's `CLAUDE.md` still loads.
  Claude Code also reads instruction files from every directory above the
  working directory; a Task Checkout sits under the user's home, so
  `~/.claude/CLAUDE.md` would load through that walk. `claudeMdExcludes` lists
  each ancestor's CLAUDE.md and CLAUDE.local.md, plain and dot-claude forms,
  by absolute path. `autoMemoryEnabled` is `false`. `--setting-sources` and
  `CLAUDE_CONFIG_DIR` do not stop the walk. `--safe-mode` and an empty
  `--setting-sources` were rejected because both also drop the project
  `CLAUDE.md`. Built-in CLI plugins load in
  every mode.

  Known limits of isolation, to revisit before any Worker runs on an
  untrusted repository or without a person watching the run:
  - Isolation does not limit the filesystem. The Worker can read and write
    anything the user can, outside the Task Checkout.
  - `PINNED_ENV` covers endpoints, proxies, CA bundles, loaders, `PATH`, all
    provider selectors and skip switches, and `CLOUD_ML_REGION`. Not pinned:
    git's inlets, `HOME`, credential names, `PYTHON*`.
  - The PR apply agent still runs with bypass (`permission_mode: None`).
  - The Auto classifier verdict and real-CLI cancellation have not been
    exercised in a live run. Both must be confirmed before unattended use.
  - Built-in CLI plugins load, and the CLI's own telemetry and network
    egress are not limited by this module.
  - Skills: in the tested launch shape, synthetic skills in `HOME`, in
    `CLAUDE_CONFIG_DIR` and in ancestor directories did not load. Four of
    thirteen skill names in a real run were not traced to a source, so skill
    isolation is not verified beyond that test.
- `disable_mcp: true` on every production run. Personal connectors from the
  user's Claude configuration are not loaded.
- The child starts from an empty environment (`env_clear`), then gets only
  `provider_env`: identity and PATH, locale, temp directories, proxy and CA
  settings, `ANTHROPIC_*`, and the provider selectors and credentials for
  Bedrock (including its bearer token, role and web-identity variables),
  Vertex, Foundry, and `CLAUDE_CODE_OAUTH_TOKEN`, and on Windows the profile
  and system variables (`USERPROFILE`, `SYSTEMROOT`, and the like). `HOME` is
  passed through, never replaced. Session identifiers, `CLAUDE_CODE_MESSAGING_*`,
  and forge tokens are not passed. Add a name to `PROVIDER_ENV_NAMES` only
  with a reason in the commit that adds it.

`claude_args` builds the argument list; test capability changes there.

## Prompt transport

The prompt never goes in argv. `claude_args` passes a bare `-p`, and
`ClaudeRunner` writes the prompt to the child's stdin from a separate task
started at spawn, then closes it; the CLI reads stdin to EOF before it starts.
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
  `result` error (see Failure reasons). `prompt_failure()` exposes it to
  callers that judge by `exit_status()`, like the PR helper.
- `kill()` and dropping the runner abort the writer, so a cancelled run
  never leaves it blocked on a pipe that a process outside the group still
  holds open.
- A stand-in `claude` in a test must read its stdin to EOF before answering,
  as the real one does. One that exits 0 without reading can fail the run.

## Failure reasons

`wait()` gives the most specific reason the run left behind, and the PR
helper reads the stream the same way through `result_failure_reason`.

- The CLI reports some failures only in the stream-json `result` event,
  with exit status 1 and nothing on stderr. Running out of turns is
  `"subtype": "error_max_turns"` with `errors` and no `result` text. An API
  error is `"is_error": true` with `"subtype": "success"`, a
  `terminal_reason` and `api_error_status`; stderr then holds only a
  diagnostic tag.
- A non-zero exit reads `Exit code N — ` followed by the `result` event's
  reason (with the last stderr line after it, when there is one), else the
  last stderr line, else the prompt failure, else `no details`. A CLI too
  old for `--restricted` gets `restricted_unsupported_reason` instead.
- A child ended by a signal (a kill, including cancellation) reads
  `Terminated by signal N — ` and quotes only stderr: an error result it
  wrote before the kill is not why it ended.
- An exit 0 fails when any `result` event had `"is_error": true`, even if a
  later one succeeded. The reason is always the last `result` event that
  reported a failure, in the runner and the PR helper alike.
- Every quoted piece is cut to one bounded line. The `errors` list is
  preferred over the `result` text, which is model-written in general.
