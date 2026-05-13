# SlashIt

<p align="center">
  <img src="logo.svg" alt="SlashIt logo" width="132" height="132">
</p>

<p align="center">
  <strong>Mission control for AI coding agents — from "here's a task" to "the PR is merged" without leaving the window.</strong>
</p>

<p align="center">
  <a href="https://github.com/BarraDev/slashit/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/BarraDev/slashit/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-blue.svg"></a>
  <a href="https://github.com/BarraDev/slashit/releases"><img alt="GitHub release" src="https://img.shields.io/github/v/release/BarraDev/slashit?include_prereleases"></a>
  <a href="https://github.com/BarraDev/slashit/stargazers"><img alt="GitHub stars" src="https://img.shields.io/github/stars/BarraDev/slashit"></a>
</p>

Delegating code to an AI agent works fine for one task. Doing it for five tasks across three repos, each in its own branch, with reviewer comments coming in on yesterday's PR, is where it falls apart.

SlashIt is a desktop app that turns that into a workflow you can actually run. Stage tasks on a Kanban board, hand each one to an agent in its own isolated workspace, and queue several to run in parallel. Keep a terminal — or several — open next to each one for when you want to take over. When the agent comes back with a draft, review the diff, open a PR, and — when reviewers leave comments — triage them one by one, let the agent propose Fix / Skip / Question per comment, apply the approved fixes, and reply back on the PR without ever opening GitHub.

It's built for people who've already outgrown a single terminal tab and a stack of disposable branches.

## What you can do with it

**Plan and dispatch work**
- Kanban board across Backlog, Queue, In Progress, Review, and Done — per project, fully local.
- Pull issues in from GitHub or Jira instead of retyping them.
- Drop a card into the queue and an agent picks it up; configurable concurrency keeps things sane.

**Run many things at once**
- Every task gets its own isolated workspace — a real Git worktree (or a `jj` workspace, if you use Jujutsu) — so parallel agents never trample each other and you can keep reviewing one change while another keeps cooking.
- Each workspace has its own set of PTY-backed terminals — split them, group them by task, drop into one when you need to take over.
- A persistent session model is the long-term goal: close the window, the agents and terminals keep running in the background, reattach later and pick up exactly where you left off (think `tmux`, but for whole AI workflows).

**Group projects into a meta-workspace** *(in progress)*
- Bundle several related projects together and let an agent work across all of them at once — useful when a single change spans, say, a backend repo, a frontend repo, and a shared library.
- The agent runs from the meta-workspace as its working root, with memory and instructions defined once, and treats the bundled projects as context it can reason about together.
- AI-tooling artifacts (agent memory, prompts, scratch notes) live inside the meta-workspace instead of being scattered as `.claude/`, `.codex/`, etc. across every project repo — your project trees stay clean.
- Tasks still execute in their own isolated workspaces under the hood; the meta-workspace adds grouping and shared context, not shared mutable state.

**Close the loop on PRs**
- Open pull requests directly from a finished task.
- The PR review assistant pulls every reviewer comment, asks the agent for a per-comment recommendation, lets you edit the reasoning and the proposed change, then applies approved fixes in one pass and posts replies back on the PR.

**Stay in flow**
- Works with whatever version-control setup you already have — plain Git is fine, and [Jujutsu](https://github.com/jj-vcs/jj) (`jj`) gets first-class treatment for stacked changes if you use it.
- A `slashit` CLI controls the running app from any terminal — handy for shell scripts and for other agents to talk to.
- System tray keeps agents and terminals alive when the window closes; signed auto-updates ship from GitHub Releases.

## Screenshots

### PR review assistant

Triage every reviewer comment on a pull request, decide Fix / Skip / Question per item, edit the agent's reasoning and proposed change inline, then apply all approved fixes in one pass with optional auto-push and auto-reply on the PR.

<p align="center">
  <img src="docs/assets/screenshots/pr-comment-review.png" alt="SlashIt PR Comment Review modal: per-comment Fix/Skip/Question dropdown, editable reasoning and proposed change, footer toggles for auto-push, auto-reply, and only-new filter, plus Re-discuss / Re-analyze / Apply actions" width="900">
</p>

### Workspace overview (in progress)

The screens below preview the broader workspace, parts of which are still being polished.

<p align="center">
  <img src="docs/assets/screenshots/dashboard-kanban.jpg" alt="SlashIt dashboard with Kanban task queue, agent queue, worktrees, and JJ status" width="900">
</p>

<p align="center">
  <img src="docs/assets/screenshots/agent-workspace.jpg" alt="SlashIt agent execution workspace with terminal logs, task phases, and session context" width="900">
</p>

<p align="center">
  <img src="docs/assets/screenshots/worktrees-prs.jpg" alt="SlashIt worktrees page with JJ status, PR readiness, and diff preview" width="900">
</p>

## Status and Roadmap

SlashIt is currently pre-1.0 software. The core app, CLI, IPC server, queue, terminals, updater wiring, and CI/release workflows are in place, but the public release is still being polished.

Near-term roadmap:

- Add installation walkthroughs and release notes polish.
- Track follow-up issues for GUI binary naming, optional daemon mode, and workspace layout cleanup.
- Continue hardening queue execution, agent recovery, and cross-platform packaging.

## Built with

Rust end to end — a [Leptos](https://leptos.dev/) 0.8 frontend compiled to WASM, a [Tauri](https://tauri.app/) v2 backend on the tokio runtime, and a standalone CLI that talks to the running app over a Unix domain socket (JSON-lines on `$XDG_RUNTIME_DIR/slashit.sock`). Module-level layout is documented in [`AGENTS.md`](AGENTS.md).

## Installation

### Pre-built Binaries

Download the latest release for your platform from [GitHub Releases](https://github.com/BarraDev/slashit/releases).

| Platform | Format |
|----------|--------|
| Linux | `.AppImage`, `.deb` |
| macOS | `.dmg` |
| Windows | `.msi`, `.exe` |

### Build from Source

**Prerequisites:**

- Rust (stable toolchain)
- [Trunk](https://trunkrs.dev/) (`cargo install trunk`)
- `wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`)
- System dependencies (Linux): `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev`

```bash
# Clone the repository
git clone https://github.com/BarraDev/slashit.git
cd slashit

# Development mode (recommended)
./dev.sh

# Or manually
NO_AT_BRIDGE=1 cargo tauri dev

# Production build
cargo tauri build

# Build CLI only
cargo build -p slashit --release
```

On non-GNOME Linux desktops such as i3 or sway, `NO_AT_BRIDGE=1` prevents a known WebKitGTK AT-SPI accessibility bridge crash.

## CLI Usage

The `slashit` CLI lets you control the running app from any terminal:

```bash
slashit status                              # App status overview
slashit projects                            # List projects
slashit tasks [--project ID]                # List tasks
slashit create --project ID "Task title"    # Create task
slashit move TASK_ID queue                  # Move task to status
slashit edit TASK_ID --title "New title"    # Edit task
slashit delete TASK_ID                      # Delete task
slashit queue                               # Queue status
slashit enqueue TASK_ID                     # Add task to queue
slashit terminals                           # List active terminals
slashit show                                # Bring window to front
```

Add `--json` to any command for machine-readable output. Use `--wait` to wait for the app to start.

## Configuration

Configuration is stored in your system config directory:

- Linux: `~/.config/slashit-app/`
- macOS: `~/Library/Application Support/com.barradev.slashit-app/`
- Windows: `%APPDATA%\com.barradev.slashit-app\`

## Development

Useful local checks:

```bash
cargo fmt --check
cargo clippy -p slashit-app -p slashit -p slashit-ipc -- -D warnings
cargo test -p slashit-app -p slashit-ipc
trunk build
```

This repository uses Jujutsu (`jj`) as the primary version-control workflow with a colocated Git repository for GitHub compatibility. Plain Git contributions are welcome.

To generate platform icons after updating `logo.svg`, run:

```bash
cargo tauri icon logo.svg
```

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

Security vulnerabilities should be reported privately; see [SECURITY.md](SECURITY.md).

## License

Copyright 2025 Barradev Digital Services

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.
