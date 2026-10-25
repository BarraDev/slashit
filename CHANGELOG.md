# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) before and after `1.0.0`.

## [Unreleased]

## [0.1.0]

The first public release of SlashIt.

**Installing.** Download the installer for your platform from the GitHub
release page: the `.deb`, `.rpm` or `.AppImage` on Linux, the `.dmg` on macOS
(Apple Silicon and Intel), and the `.msi` or NSIS `.exe` on Windows. The
installers are not code-signed by Microsoft or Apple, so Windows SmartScreen and
macOS Gatekeeper warn on the first launch. On Windows, choose "More info", then
"Run anyway". On macOS, open the app from its context menu.

**Updating.** The in-app updater checks GitHub Releases for newer versions,
verifies each update against SlashIt's signing key, and installs it when you
ask. It is on by default. To turn it off, set `auto_update = false` in
`features.toml` in SlashIt's configuration directory, then restart SlashIt. The
`SLASHIT_FEATURE_AUTO_UPDATE` environment variable overrides the file. An installation from a `.deb` or `.rpm` package is not replaced
by the in-app updater; the app says so, and you can install the newer package
from the release page.

### Added

- **Product Workspaces.** A Product Workspace groups several related Projects.
  A Project belongs to at most one, and you attach or detach Projects on the
  Workspaces page. When a member Project's task runs and the Workspace folder
  exists, the coding agent starts in that folder, so instruction files kept
  there are shared by every member Project. Each task's diff, review, commit and
  pull request still cover only its own Task Checkout. There are no combined
  views across Projects yet. A Project whose Workspace is no longer registered
  is listed under "Unresolved membership"; detaching it makes it standalone.
- **Task Checkouts.** Each task works in its own Task Checkout: a Git worktree
  on its own branch, kept in SlashIt's data directory rather than in your
  repository. SlashIt creates and removes these worktrees with Git itself, so no
  additional tool is needed. Parallel tasks never edit the same files, and each
  checkout has its own terminals.
- **Projects without a remote.** A repository with commits but no remote runs
  tasks from start to finish, including AI Review and Human Review. Steps that
  need a remote, such as pushing and opening a pull request, are offered only
  where a supported remote exists.
- **Local completion.** In a project that cannot open pull requests, an approved
  task can be completed locally. Its Task Checkout is removed, the task is marked
  Done, and its branch is kept, unmerged and unpushed. Integrate locally moves a
  local branch to the exact commit you approved, after a confirmation that lists
  what will change, and it never pushes. Projects that can open pull requests
  keep them as the default way to finish a task.
- **Repository setup.** Settings > Repository shows whether a project can run
  tasks and, if it cannot, why. It can detect the default branch of `origin`, and
  it can initialize a folder that has no version control, with Git or with
  Jujutsu. Initializing never chooses a branch name for you, and it refuses if
  the folder changed after it was shown to you. A Jujutsu repository must be
  colocated with Git; one that is not is refused, with the command that fixes it.
- **Pull request review.** The PR review assistant lists every reviewer comment
  on a pull request with the agent's recommendation. For each comment, choose
  Fix, Skip or Question, edit the reasoning and the proposed change, and then
  apply the approved fixes in one pass. Replies can be posted back on the pull
  request.
- **Headless daemon.** The `slashitd` binary runs the control channel and the
  task queue without a window or webview. The installers do not include it; build
  it from source with `cargo build -p slashit-ui --bin slashitd --release`.
- **Control channel and CLI.** The `slashit` command-line client controls the
  running app from a terminal or a script. The `.deb` and `.rpm` packages install
  it at `/usr/bin/slashit`; the AppImage, macOS and Windows installers do not, so
  build it with `cargo build -p slashit --release` there. The channel uses a Unix domain socket
  on Linux and macOS and a named pipe on Windows, both protected by the operating
  system. A loopback TCP listener is available too, but it is off by default and
  requires a bearer token.
- **Window and tray.** Closing the main window hides it, and the app keeps
  running. A click on the system tray icon shows the window again.
- **Project storage.** Each project keeps its board outside the repository by
  default, or inside it at `.slashit/` so it can be committed and shared. The
  board can also be detected automatically. Settings > Storage previews a move,
  with file counts, sizes and any conflicts, and changes nothing until you
  confirm.
- **Feature flags.** Unfinished features can ship turned off. Flags are stored in
  `features.toml` in SlashIt's configuration directory, and the
  `SLASHIT_FEATURE_<NAME>` environment variable overrides them. `slashit
  features` shows each flag's value and which setting decided it.
- **Documentation and project files.** Design notes on storage locations, the
  IPC security model, the headless daemon and the release procedure are in
  `docs/`. A contribution guide, a code of conduct and a security policy are at
  the repository root.

[0.1.0]: https://github.com/BarraDev/slashit/releases/tag/v0.1.0
[Unreleased]: https://github.com/BarraDev/slashit/compare/v0.1.0...HEAD
