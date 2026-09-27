# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) before and after `1.0.0`.

## [Unreleased]

Nothing has been released yet. The sections below describe what the first
release will contain.

### Added

- **Headless daemon mode.** A `slashitd` binary runs the IPC server and the
  queue executor with no window and no webview, so agents and terminals can
  outlive the desktop app entirely. It is not a second implementation: the
  daemon and the GUI build the same state, serve the same command set and
  resolve the same feature flags, differing only in where events go and in
  whether there is a window to raise.
- **The control channel works on every platform.** A Unix domain socket on
  Linux and macOS, a named pipe on Windows, and — off by default — a loopback
  TCP listener for cases neither covers. The first two are access-controlled
  by the operating system, so reaching them already proves you are the owning
  user; TCP proves nothing, so it exists only alongside a bearer token held in
  a 0600 file and compared in constant time. See
  [`docs/architecture/ipc-security.md`](docs/architecture/ipc-security.md).
- **In-app updates.** The app checks for a new release, shows what is
  available, and installs it on request. Updates are verified against a
  signing key before they are applied; the installers themselves are not yet
  OS code-signed, which is stated plainly in
  [`docs/releasing.md`](docs/releasing.md).
- **Project storage location.** Each project chooses whether its board lives
  outside the project (the default, leaving your repository untouched) or
  inside it at `.slashit/` so it can be committed and shared. Changing the
  setting previews the move — file counts, sizes and any conflicts — and does
  nothing until confirmed. See
  [`docs/architecture/state-locations.md`](docs/architecture/state-locations.md).
- **Runtime feature flags** in `features.toml`, so unfinished work can ship
  off by default. A flag resolves through a command-line override, then
  `SLASHIT_FEATURE_<NAME>`, then the file, then its default, and `slashit
  features` reports the value *and* which layer decided it — "the flag is on"
  is not an actionable answer when someone is asking why.
- Architecture documentation for state locations, the IPC security model, the
  headless daemon and the release procedure.
- Meta-workspace model: a workspace folder that coordinates several projects.
- PR comment review workflow with per-item approve, fix and skip.
- Community health files for the public contribution flow.

### Changed

- **Worktrees are created outside your repository**, under
  `<data_dir>/worktrees/<project-key>/<branch>` instead of as siblings named
  `<repo>.<branch>`. Existing worktrees are adopted, not recreated.
- **Task Checkouts no longer depend on Worktrunk.** SlashIt creates, reattaches,
  stacks and removes every task worktree with `git` itself, whether or not
  `wt` is installed. Earlier builds delegated to `wt` when it was on `PATH`,
  always with its hooks turned off, so no Worktrunk hook ever ran for a task.
  Worktrees `wt` already created at its own template paths are adopted where
  they are, for reattaching, starting and cleanup alike.
- **An ordinary task branch starts at `origin`'s default branch**, at the exact
  commit `refs/remotes/origin/<default>` names, instead of at whatever the
  primary checkout had checked out (a feature branch, or unpushed work in a
  Jujutsu-colocated repository). The default branch comes from
  `refs/remotes/origin/HEAD`, or from Jujutsu's `trunk()` alias when that is
  exactly `<branch>@origin`, and nothing is fetched to find it. A repository
  with neither now refuses to start the task and says to run
  `git remote set-head origin --auto`; one without a remote named `origin` is
  not supported. The new branch tracks nothing until SlashIt pushes it, and its
  pull request is opened against that default branch explicitly. Tasks created
  earlier keep the origin they recorded and open their pull requests as before.
- **Task branches are named from the whole task id**, `task-<id>`, instead of
  its first eight hex digits, which two tasks could share, and with them one
  checkout. A task keeps the branch it already records, whatever its form. A
  task is refused, with the other task named, while another task in the same
  repository records its branch, whatever path the checkout is at. A task that
  records no branch is also refused while a branch of its old eight-digit name
  (`task-<8 hex>`) exists and no task records it: it may hold an earlier start
  whose record was never saved, or another task's work. To hand it to the
  task, rename it with `git branch -m task-<8 hex> task-<id>` and, if no
  worktree has it checked out, add one with
  `git worktree add <directory> task-<id>`; otherwise delete or rename it.
- **`[worktree] placement = "auto"` now means `"managed"`.** Both spellings
  still load, and new configurations are written with `"managed"`. Downgrade
  caveat: an earlier build reading a configuration that says `"auto"` delegates
  to `wt` again wherever it is installed. Change the setting to `"managed"`
  before going back if that matters. An earlier build that rewrites a
  project's `tasks.toml` also drops the default branch recorded for each task,
  so those tasks' pull requests lose their explicit `--base` and go to
  GitHub's default branch again.
- `config.toml` is written atomically and with owner-only permissions. It
  carries `AgentConfig::api_key`, and was previously world-readable.
- Task files move from one global directory into each project's own state
  directory. The old location is still read, so no task disappears on upgrade;
  the move happens on the next save.
- The frontend crate is now `slashit-frontend`, and the Playwright harness
  `slashit-e2e`. Nothing user-visible changes. Of the workspace's names —
  `slashit-ui` for the desktop app, `slashit` for the CLI, `slashit-ipc` for
  the shared protocol — only `slashit-app-ui` failed to say which piece it
  was. It could not simply become `slashit-ui`, because the Tauri crate
  already is: two packages with one name in a workspace is a Cargo error and
  would make `cargo -p slashit-ui` ambiguous. Application data still lives
  under `slashit-app`, which is frozen.

### Fixed

- **A release candidate would have shipped without a Windows installer.** WiX
  accepts only numeric version components, so `0.1.0-rc.1` made the MSI
  bundler bail — but not until the whole release build had already compiled on
  the Windows runner, and only there, leaving a release that looked nearly
  complete with nothing for Windows users. The release workflow now rejects an
  unpackageable version in seconds, before any platform build starts, and
  [`docs/releasing.md`](docs/releasing.md) documents the numeric form
  (`0.1.0-1`) that does work.
- **A release could be named after a version other than its tag.** The release
  name was substituted from `tauri.conf.json` rather than from the git ref, so
  pushing `v0.1.0-rc.1` against an app version of `0.1.0` silently produced a
  release called `v0.1.0`. The tag and the app version are now checked against
  each other, and the release takes its name from the ref.
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
