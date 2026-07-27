# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) before and after `1.0.0`.

## [Unreleased]

Nothing has been released yet. The sections below describe what the first
release will contain.

### Added

- **Project storage location.** Each project chooses whether its board lives
  outside the project (the default, leaving your repository untouched) or
  inside it at `.slashit/` so it can be committed and shared. Changing the
  setting previews the move — file counts, sizes and any conflicts — and does
  nothing until confirmed. See
  [`docs/architecture/state-locations.md`](docs/architecture/state-locations.md).
- **Runtime feature flags** in `features.toml`, so unfinished work can ship
  off by default.
- Architecture documentation for state locations, the IPC security model and
  the planned headless daemon.
- Meta-workspace model: a workspace folder that coordinates several projects.
- PR comment review workflow with per-item approve, fix and skip.
- Community health files for the public contribution flow.

### Changed

- **Worktrees are created outside your repository**, under
  `<data_dir>/worktrees/<project-key>/<branch>` instead of as siblings named
  `<repo>.<branch>`. Existing worktrees are adopted, not recreated. When
  worktrunk (`wt`) is installed, placement is still delegated to it.
- `config.toml` is written atomically and with owner-only permissions. It
  carries `AgentConfig::api_key`, and was previously world-readable.
- Task files move from one global directory into each project's own state
  directory. The old location is still read, so no task disappears on upgrade;
  the move happens on the next save.

### Fixed

- **A task branch could resolve to your main checkout.** Worktree lookup
  matched the branch name as a substring of the whole `git worktree list`
  block, including the line holding the repository path, so a branch whose
  name appeared in that path returned the primary working copy — and an agent
  would then have run there. Matching is now on the exact branch record.
- **Startup no longer strands relocated worktrees.** A recorded worktree path
  that did not resolve had its reference cleared, abandoning the branch and any
  uncommitted work in it. The worktree is now adopted at its current location
  first.
- **Starting a second instance no longer steals the first one's IPC socket.**
  The existing socket was removed unconditionally before binding; a liveness
  check now refuses startup instead.
- The IPC socket no longer falls back to a predictable path in the
  world-writable temp directory, and its parent directory is created with
  owner-only permissions before binding.
- IPC requests are capped at 1 MiB. A client that never sent a newline
  previously made the server buffer without bound.
- Registering a workspace no longer creates an empty `.slashit/` directory
  inside it. Existing empty ones can be cleaned up from settings.
- The Windows release build compiles again; the Unix-socket IPC module is now
  correctly gated.
- The bundler emits updater artifacts, without which the configured updater
  had nothing to serve.
- The browser tab no longer says "Tauri + Leptos App".

[Unreleased]: https://github.com/BarraDev/slashit/commits/main
