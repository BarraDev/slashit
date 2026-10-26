# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) before and after `1.0.0`.

## [Unreleased]

## [0.0.1]

A prerelease for testers. Install it by hand the first time. After that, the
in-app updater offers later prereleases to people who choose the Prerelease
update channel, and never to people on the Stable channel. The features listed
below are the full feature set of this prerelease.

**Installing.** Download the installer for your platform from the GitHub
release page: the `.deb`, `.rpm` or `.AppImage` on Linux, the `.dmg` on macOS
(Apple Silicon and Intel), and the `.msi` or NSIS `.exe` on Windows. The
installers are not code-signed by Microsoft or Apple, so Windows SmartScreen and
macOS Gatekeeper are expected to warn on the first launch. On Windows, choose
"More info", then "Run anyway". On macOS, open the app from its context menu.
Neither warning has been seen on a real machine for this build.

**Updating.** The in-app updater is on by default. It checks GitHub Releases for
newer versions, verifies each update against SlashIt's signing key, and installs
it when you ask. For this prerelease the update path has not been run end to
end (see the known gaps). An installed 0.0.1 is designed to be offered the next
stable release once one is published. To turn the updater off, set
`auto_update = false` in `features.toml` in SlashIt's configuration directory,
then restart SlashIt. The `SLASHIT_FEATURE_AUTO_UPDATE` environment variable
overrides the file. An installation from a `.deb` or `.rpm` package is not
replaced by the in-app updater; the app says so, and you can install the newer
package from the release page.

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
  task queue without a window or webview. The Linux packages (`.deb`, `.rpm` and
  `.AppImage`) are meant to include it; the package contents are listed under the
  known gaps. Elsewhere, build it from source with
  `cargo build -p slashit-ui --bin slashitd --release`.
- **Control channel and CLI.** The `slashit` command-line client controls the
  running app from a terminal or a script. The `.deb` and `.rpm` packages are
  meant to install it at `/usr/bin/slashit`; the AppImage, macOS and Windows
  installers are not, so build it with `cargo build -p slashit --release` there.
  The channel uses a Unix domain socket on Linux and macOS and a named pipe on
  Windows, both protected by the operating system. A loopback TCP listener is
  available too, but it is off by default and requires a bearer token.
- **Window and tray.** Closing the main window hides it, and the app keeps
  running. A click on the system tray icon shows the window again.
- **Project storage.** Each project keeps its board outside the repository by
  default, or inside it at `.slashit/` so it can be committed and shared. The
  board can also be detected automatically. Settings > Storage previews a move,
  with file counts, sizes and any conflicts, and changes nothing until you
  confirm.
- **Update channels.** Settings > Application updates offers Stable (the default)
  and Prerelease. Stable offers stable releases only; Prerelease also offers
  prereleases. The installed build's channel is shown separately from the
  channel you select.
- **Feature flags.** Unfinished features can ship turned off. Flags are stored in
  `features.toml` in SlashIt's configuration directory, and the
  `SLASHIT_FEATURE_<NAME>` environment variable overrides them. `slashit
  features` shows each flag's value and which setting decided it.
- **Documentation and project files.** Design notes on storage locations, the
  IPC security model, the headless daemon and the release procedure are in
  `docs/`. A contribution guide, a code of conduct and a security policy are at
  the repository root.

Known gaps in this prerelease:

- No installer has been installed or launched on its target platform yet. That
  covers the `.deb`, `.rpm` and `.AppImage` on Linux, the `.msi` and NSIS `.exe`
  on Windows, and the `.dmg` on macOS. Upgrading over an earlier install has not
  been tested either.
- The contents of each package have not been checked on the built artifacts.
  Whether the `.deb`, `.rpm` and `.AppImage` include `slashitd`, and whether the
  `.deb` and `.rpm` include the `slashit` client, is not yet confirmed. The
  macOS and Windows installers have not been checked for `slashitd`.
- The installers are not code-signed, so Windows SmartScreen and macOS Gatekeeper
  warn on first launch. The warnings have not been observed on real machines.
- The update path has not been run against a published release. Updates are
  signed with a key held in the release workflow's secrets, and no signed 0.0.1
  artifact exists yet, so the signature check, the download and the install have
  not been exercised. The Stable channel has nothing to offer until a stable
  release newer than 0.0.1 is published, so it cannot be tested live before then.
- A background job whose shell has already exited keeps running when its
  terminal closes or SlashIt quits. The quit and restart dialogs say so.
- The product acceptance checks use fake agent providers. A real provider has
  not been run against this build.

[0.0.1]: https://github.com/BarraDev/slashit/releases/tag/v0.0.1
[Unreleased]: https://github.com/BarraDev/slashit/compare/v0.0.1...HEAD
