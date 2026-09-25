# AGENTS.md

This file provides guidance to coding agents (Claude Code and others) when working with code in this repository.

## Project Overview

SlashIt is a Tauri v2 desktop application for AI agent task orchestration across software projects. It integrates with Claude Code Agent and Jujutsu (jj) version control. The app is built entirely in Rust - Leptos/WASM for the frontend and pure Rust for the Tauri backend.

## Product vocabulary

- A Project is one repository or root folder managed by SlashIt.
- Unqualified "Workspace" means the Product Workspace: a container that
  manages several Projects. Membership is stored on the Project
  (`domain::project::ProjectScope`).
- A Task Checkout is a Task's isolated working copy. Today it is always a Git
  worktree; JJ workspaces are not a Task Checkout backend.
- A JJ developer workspace used to develop SlashIt is unrelated to the Product
  Workspace.

Full model: [docs/architecture/product-model.md](docs/architecture/product-model.md).

## Development Commands

```bash
# Development (frontend only - serves on port 1420)
trunk serve

# Development (full Tauri app - runs trunk + tauri)
./dev.sh
# Or manually: NO_AT_BRIDGE=1 cargo tauri dev
# NOTE: NO_AT_BRIDGE=1 is required on non-GNOME desktops (i3, sway, etc)
# to prevent WebKitGTK segfault in AT-SPI accessibility bridge

# Production build
cargo tauri build

# Check code
cargo check
cargo clippy

# Run the backend (Tauri app binary) directly
cargo run -p slashit-ui

# Validate agent documentation structure (the same check CI runs)
scripts/check-agent-docs.sh

# Desktop acceptance against the real application (Linux; needs a display and
# WebKitWebDriver). The build step is required: a plain `cargo build` binary
# points at localhost:1420 and never loads the embedded frontend.
cargo tauri build --debug --no-bundle
cargo test -p slashit-acceptance --features run-acceptance
```

See `crates/slashit-acceptance/README.md` for what the harness guarantees and
how to run it on Arch, which ships no `WebKitWebDriver`.

## Project Structure

```
slashit-app/
├── crates/
│   ├── slashit-acceptance/  # Desktop acceptance harness
│   ├── slashit-cli/         # The `slashit` command-line client
│   └── slashit-ipc/         # CLI control-channel protocol and transports
├── docs/             # Architecture and process docs
├── scripts/          # Repository checks (agent docs)
├── src/              # Frontend (Leptos 0.8 CSR, compiled to WASM)
└── src-tauri/        # Backend (Rust, Tauri v2)
    ├── src/          # Backend modules
    └── tauri.conf.json
```

Module layout and conventions live in the nested docs: `src/AGENTS.md` for
the frontend, `src-tauri/src/AGENTS.md` for the backend (see the list at the
end). Dependencies and versions are in the `Cargo.toml` files.

## Tauri + Leptos Configuration

Follows the official [Tauri Leptos guide](https://tauri.app/start/frontend/leptos/):

- Client-side rendering only; Tauri does not officially support server-based
  frontends.
- Trunk serves `http://localhost:1420` in development (`Trunk.toml` keeps
  `ws_protocol = "ws"` for hot reload). `src-tauri/tauri.conf.json` points
  `devUrl` at that server and `frontendDist` at Trunk's build output.
- Keep `withGlobalTauri: true` there; the frontend reaches the backend through
  `window.__TAURI__`.

## IPC Pattern

Frontend calls Tauri commands via `invoke()` through services in `src/services/`. Backend commands are registered in `src-tauri/src/lib.rs` with state managed through `AppState`.

## Development Workflow

This project uses **Jujutsu (JJ)** as the primary version control system,
colocated with Git. The rules below are the contract for maintainers and
agents; [docs/development-workflow.md](docs/development-workflow.md) has the
procedure, commands, and rationale. External contributors may use plain Git
(see `CONTRIBUTING.md`).

Paths below are relative to the canonical repository root. Its parent
directory (the *project parent*) also holds developer workspaces and local
private state.

### Repository and developer workspaces

- The canonical repository root is the coordination checkout. Its `default`
  JJ workspace (a workspace name, not a bookmark) stays an empty,
  undescribed change on `main`; product edits do not happen there.
- All product edits happen in a JJ developer workspace at
  `../workspaces/<slug>`, created from current `main` with the
  project-pinned `jj`; substantial work gets a dedicated one.
- One mutating owner per workspace. Read-only reviewers may share a workspace
  but must not write to it or run builds that conflict with the owner's.
- Never run `git worktree add` against the canonical `.git`. A task that
  genuinely needs a real `.git` (for example exercising SlashIt's own Git
  worktree backend, or `git bisect`) uses a disposable independent clone at
  `../workspaces/<slug>-git`.
- Each workspace keeps its own `target/`; do not share a Cargo target
  directory between workspaces or set `CARGO_TARGET_DIR` globally.
- JJ workspaces have no `.git`; run `gh` with `-R BarraDev/slashit` or from
  the canonical root.

### Changes, checkpoints, and publication

- Describe a meaningful active change early with `jj describe`.
- When a change is complete and validated, run `jj new`. An idle or
  review-waiting workspace has an empty, undescribed `@` above its completed
  checkpoint.
- A publication bookmark targets the completed checkpoint by explicit
  revision (`-r <change>` or `--to <change>`). Bookmark commands default to
  `@`; never rely on that for publication.
- A published or reviewed checkpoint is preserved. Review fixes are child
  changes (describe the existing empty `@`), and the bookmark advances by
  fast-forward. This is project policy; `jj` does not enforce it.
- Rewriting or force-pushing a published checkpoint is exceptional and needs
  the repository owner's explicit approval.
- JJ experiments run in a disposable repository with an absolute pinned `jj`
  binary and a local remote; never `mise -C` or `mise exec --cd` into a
  SlashIt checkout.
- Retire a merged publication bookmark with `jj bookmark forget`, not
  `jj bookmark delete`.

### Tooling

- Use the toolchain pinned in the project `mise.toml`; run JJ as
  `mise exec -- jj ...`. Do not rely on a globally installed or `latest` `jj`.
- In the colocated canonical root, an ordinary `jj` command may snapshot the
  working copy and import/export Git refs. For inspection there, use
  `mise exec -- jj --ignore-working-copy ...` or read-only Git plumbing
  (`git rev-parse`, `git cat-file`, `git ls-tree`, `git diff-tree`).

### Agents

- The main (orchestrating) agent owns decomposition, integration, the final
  report, and every public mutation.
- A subagent that writes gets its own workspace. Subagents do not spawn
  further subagents.
- Subagent output is evidence, not proof: the main agent re-verifies any
  claim a decision depends on.

### Reports and sessions

- Each substantial unit of work produces one authoritative report in
  `../slashit-private/reports/`, plus at most a bounded worklog.
  `slashit-private` is local, untracked maintainer state, not part of the
  repository. Do not store full chat transcripts.
- Start a fresh session at substantial unit boundaries. Lessons that must
  outlive a report are rewritten into project docs.

### Pull requests and published text

- A PR is a semantic delivery unit, not one PR per review finding. Stack PRs
  only for real dependencies, and keep every PR in a stack independently
  truthful.
- PR titles and bodies, commit messages, and source comments must stay useful
  to a future reader. They must not contain internal work-tracking labels,
  session narration, temporary verification SHAs, private report paths, or AI
  attribution (see below).
- Do not post transient agent-status comments on PRs.

### Authorization

- Local work inside your own workspace (edits, `jj` changes, builds, tests)
  is normal and needs no gate.
- Public or destructive operations (push, creating or editing PRs and issues,
  publication bookmarks, abandoning or deleting work or workspaces you do not
  own) require an explicit authorization appropriate to their blast
  radius.
- Merging, repository or GitHub settings changes, force-pushes or other
  rewrites of published checkpoints, and deleting remote branches or tags
  require the repository owner's explicit approval every time.

### No AI Attribution in Public Repository Artifacts

Commits, PR titles and bodies, PR and issue comments, issues, release notes,
tags and any other published repository artifact must contain **no AI session
URLs, session identifiers, or AI attribution**, unless the user explicitly
asks for one in that specific artifact.

Never add, among others:

- `Claude-Session:` or any other session trailer
- `claude.ai/code/session/...` or an equivalent link for any AI tool
- `Co-Authored-By: Claude` (or another AI assistant)
- "Generated with/by Claude", "Made with <AI tool>" and similar

Before every commit, push, or PR/issue write, inspect what is about to become
public -- commit messages, the PR body, comment text, generated metadata --
and confirm none of the above is present. If attribution slipped into work
that is still unpublished or still amendable, remove it before finishing
rather than leaving it for a follow-up.

Tool and session provenance belongs in private working notes, not in the
repository's public history.

## Nested AGENTS.md

This project has additional `AGENTS.md` files in subdirectories with module-specific guidance:

- `src/AGENTS.md` -- Frontend (Leptos/WASM) patterns
- `src-tauri/src/AGENTS.md` -- Backend (Tauri v2) overview
- `src-tauri/src/domain/AGENTS.md` -- Domain models conventions
- `src-tauri/src/agents/AGENTS.md` -- Claude Code agent execution
- `src-tauri/src/jj/AGENTS.md` -- Jujutsu integration

Each is also exposed as a `CLAUDE.md` stub containing only `@AGENTS.md`.
`docs/agent-docs.toml` maps every other module directory to the parent
`AGENTS.md` that covers it; `scripts/check-agent-docs.sh` enforces both.
