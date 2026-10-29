# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) before and after `1.0.0`.

## [Unreleased]

## [0.0.3]

An experimental prerelease that adds a temporary startup mitigation for the
intermittent Linux AppImage failure tracked in
[issue #269](https://github.com/BarraDev/slashit/issues/269). It is not
production-ready. It does not fix the upstream WebKitGTK behavior, and issue
#269 stays open until the official artifact of this release has been validated.

**Installing.** Choose the installer for your platform and follow
[`docs/installation.md`](https://github.com/BarraDev/slashit/blob/v0.0.3/docs/installation.md).
It names each file, says what was checked for it, and says what was not.

### Fixed

- **A startup mitigation for the intermittent Linux AppImage failure.** On some
  Linux systems the operating system ended SlashIt during startup with SIGKILL.
  WebKitGTK moves the IPC thread of its UI process to realtime scheduling when
  the window starts. When the kernel grants that, a 200 ms CPU-time budget
  applies to the thread, and the process is ended if the budget runs out during
  a startup burst. SlashIt now lowers the soft realtime-priority limit of its
  own process before the window opens, so the kernel refuses that scheduling
  request.
- The mitigation is temporary and version-aware. It applies when the loaded
  WebKitGTK is older than 2.54.1, or when its version cannot be read. WebKitGTK
  2.54.1 and later skip it. 2.54.1 is the earliest checked release that no longer
  uses the affected path; it is not a measured minimum. The version is read from
  the library loaded at runtime, so the `.deb` and `.rpm` packages, which use
  the WebKitGTK installed on your system, follow the same rule.
- Only the desktop app applies the limit. The headless `slashitd` daemon and
  the `slashit` command-line client do not, so the agents `slashitd` runs keep
  the limit they inherited.

### Known issues

- **Terminal processes inherit a reduced realtime-priority limit.** While the
  mitigation is active, SlashIt and the terminals and agent processes it starts
  have a soft `RLIMIT_RTPRIO` of 0. A plain `chrt -f` is refused. The hard limit
  is unchanged, so a process can raise its soft limit back to the inherited hard
  limit without privilege. Applications that need realtime scheduling may need
  to do so.
- **The upstream WebKitGTK behavior is not fixed.** The mitigation works in
  SlashIt's own process. It does not change the kernel, RealtimeKit or WebKitGTK.
- **Issue #269 remains open.** The official Linux artifact of this release has
  not been validated yet. Until it is, a startup failure on your system is still
  possible, and repeated launches are not a guaranteed workaround.
- **Media playback was not tested for this build.**
- **The installers are not code-signed.** Windows SmartScreen and macOS
  Gatekeeper warn on first launch. The updater's signatures are a different
  mechanism and do not remove those warnings.
- **Installation and launch are not verified on every platform.** Which
  artifacts were checked for this build, and which were not, is recorded in
  [`docs/installation.md`](https://github.com/BarraDev/slashit/blob/v0.0.3/docs/installation.md).
  Treat an unverified platform as untested.
- **Updates from a published release have not been run end to end.** An update
  installed through the application has been exercised only in a local
  experiment, with a disposable test key.
- **The AppImage update replaces the file without an atomic step.** An update
  interrupted by a crash or a power loss has not been tested.
- The product acceptance checks use fake agent providers. A real agent provider
  has not been run against this build.

## [0.0.2]

The first public experimental prerelease. It is not production-ready, and it
has not been validated on every platform. Read the known issues below before
you install it.

The feature set is the one described in the [0.0.1 section](https://github.com/BarraDev/slashit/blob/v0.0.2/CHANGELOG.md#001) of the changelog. That
version was prepared as a draft and never published, so there is no earlier
public install to upgrade from. This release carries 0.0.1's features forward
and changes the points listed here.

**Installing.** Choose the installer for your platform and follow
[`docs/installation.md`](https://github.com/BarraDev/slashit/blob/v0.0.2/docs/installation.md).
It names each file, says what was checked for it, and says what was not.

### Changed

- **The updater uses HTTPS only.** Release listings, update manifests and update
  downloads are fetched over HTTPS. A redirect from HTTPS to plain HTTP is
  refused, and a redirect chain is limited to five hops. An update whose
  download location is not HTTPS is refused before anything is downloaded.
  Signature checks and release selection are unchanged.
- The contribution guide explains how pull requests reference issues, and the
  README links to the issue tracker.

### Fixed

- The Windows release job stopped before it built any installer, because the
  step that writes the release description ran under PowerShell, which rejects
  the POSIX shell syntax the step uses. The step now names its shell. A
  heuristic check in CI refuses workflow steps that use POSIX syntax without
  naming a shell, so the same failure is caught before a tag is pushed.

### Known issues

- **The Linux AppImage may fail to start.** On some Linux systems the operating
  system ends SlashIt during startup (a SIGKILL). The AppImage bundles its own
  copy of WebKitGTK, and that copy's realtime scheduling watchdog is the
  established trigger. Tracked in
  [issue #269](https://github.com/BarraDev/slashit/issues/269). This release
  does not fix it. Starting SlashIt again may succeed, but repeated launches are
  not a guaranteed workaround. The `.deb` and `.rpm` packages use the WebKitGTK
  installed on your system instead, so their behavior depends on your
  distribution.
- **The installers are not code-signed.** Windows SmartScreen and macOS
  Gatekeeper warn on first launch. The updater's signatures are a different
  mechanism and do not remove those warnings; see
  [`docs/releasing.md`](https://github.com/BarraDev/slashit/blob/v0.0.2/docs/releasing.md).
- **Installation and launch are not verified on every platform.** Which
  artifacts were installed and launched for this build, and which were not, is
  recorded in [`docs/installation.md`](https://github.com/BarraDev/slashit/blob/v0.0.2/docs/installation.md). Treat an
  unverified platform as untested.
- **Updates from a published release have not been run end to end.** The
  updater signing key's compatibility probe passed. An update installed through
  the application has been exercised only in a local experiment, with a
  disposable test key, not with the release key or a published release.
- **The AppImage update replaces the file without an atomic step.** An update
  interrupted by a crash or a power loss has not been tested, and Windows
  staged-install lifecycle defects are known and not addressed in this release.
- The product acceptance checks use fake agent providers. A real agent provider
  has not been run against this build.
- A background job whose shell has already exited keeps running when its
  terminal closes or SlashIt quits. The quit and restart dialogs say so.

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
- The Linux packages were checked only as built locally, with a disposable
  signing key and not the release key. The `.deb` and `.rpm` contain `slashit`,
  `slashit-ui` and `slashitd`. The `.AppImage` contains `slashitd` and
  `slashit-ui`, and not the `slashit` client. No package contains the test
  fixture binary. The macOS and Windows installers have not been checked for
  `slashitd`, and the packages from the release workflow have not been checked.
- The installers are not code-signed, so Windows SmartScreen and macOS Gatekeeper
  warn on first launch. The warnings have not been observed on real machines.
- The update path has not been run against a published release. The updater
  signing probe passed on the commit this tag names, so the production key
  matches the public key the application ships with. The download and the
  installation of a signed update have not been exercised against a published
  release. The Stable channel has nothing to offer until a stable release newer
  than 0.0.1 is published, so it cannot be tested live before then.
- A background job whose shell has already exited keeps running when its
  terminal closes or SlashIt quits. The quit and restart dialogs say so.
- The product acceptance checks use fake agent providers. A real provider has
  not been run against this build.

[0.0.3]: https://github.com/BarraDev/slashit/compare/v0.0.2...v0.0.3
[0.0.2]: https://github.com/BarraDev/slashit/compare/v0.0.1...v0.0.2
[0.0.1]: https://github.com/BarraDev/slashit/releases/tag/v0.0.1
[Unreleased]: https://github.com/BarraDev/slashit/compare/v0.0.3...HEAD
