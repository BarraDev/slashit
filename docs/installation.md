# Installing SlashIt

SlashIt 0.0.4 is an **experimental prerelease**. It is not production-ready, and
it has not been installed and launched on every platform. Read the
[known issues](#known-issues) before you install it.

This page names each installer the release produces, says what was verified for
it and what was not, and describes how to install, remove and update it. The
release notes in [`CHANGELOG.md`](../CHANGELOG.md) list what changed.

The files below are what the release workflow is expected to produce for a
`v0.0.4` tag. Their names follow the pattern used for earlier releases. They have
not been checked against built artifacts, because none exist yet. The verification
recorded for 0.0.3 and 0.0.2 applies to those builds only. It is in
[the 0.0.3 section](#verification-of-003-2026-10-29) of this page and in the
[0.0.2 guide](https://github.com/BarraDev/slashit/blob/v0.0.2/docs/installation.md).

## Choose an installer

| Platform | File | Architecture | Verified for 0.0.4 |
|---|---|---|---|
| Linux | `SlashIt_0.0.4_amd64.AppImage` | x86_64 | Not yet. |
| Linux (Debian, Ubuntu) | `SlashIt_0.0.4_amd64.deb` | x86_64 | Not yet. |
| Linux (Fedora, RHEL-like) | `SlashIt-0.0.4-1.x86_64.rpm` | x86_64 | Not yet. |
| Windows | `SlashIt_0.0.4_x64-setup.exe` (NSIS) | x64 | Not yet. |
| Windows | `SlashIt_0.0.4_x64_en-US.msi` | x64 | Not yet. |
| macOS, Apple silicon | `SlashIt_0.0.4_aarch64.dmg` | arm64 | Not yet. |
| macOS, Intel | `SlashIt_0.0.4_x64.dmg` | x86_64 | Not yet. |

The release also contains `.sig` files, `latest.json`, and macOS `.app.tar.gz`
files. These are for the updater, not for installation. Do not install them.

No artifact of this release has been installed or launched yet. The 0.0.3
AppImage was launched on one host, and that result does not carry over to 0.0.4.
Treat every platform as untested until this page says otherwise.

## Verification of 0.0.3 (2026-10-29)

This section records the 0.0.3 prerelease, which is still published. It does not
describe 0.0.4. The figures were recorded when 0.0.3 was published. The launch logs
behind them are not kept in this repository.

- **Linux AppImage startup.** The official `SlashIt_0.0.3_amd64.AppImage`
  started and exited cooperatively in 24 of 24 launches on one Arch Linux host
  running sway on Wayland. No launch was killed with SIGKILL. The official
  0.0.2 AppImage, without the mitigation, was killed with SIGKILL (exit 137) in
  2 of 2 attempts on the same host.
- **In-app update from 0.0.2 to 0.0.3.** On the same host, the in-app updater
  replaced an isolated copy of the official 0.0.2 AppImage with the official
  0.0.3 AppImage. The 0.0.2 process was started with its soft `RLIMIT_RTPRIO`
  lowered from outside SlashIt, so that it would start. The installed file
  matched the official 0.0.3 SHA-256. Restart and state persistence passed.
- **Not verified for 0.0.3.** The updater's own signature check was not
  observed, though the artifact's signature was verified separately. The embedded
  terminal check was waived. Media playback, macOS, Windows, other Linux
  distributions, the `.deb` and `.rpm` packages, and an interrupted update were
  not tested.

## Known issues

### Downgrade and your data

Before you install this version, back up your SlashIt data directories (see
[Your data](#your-data)).

A Project's board is one file, and 0.0.4 can write Task fields that 0.0.3 does not
declare. That happens only when a Task uses one of them: an AI reviewer model, a
pull-request publication, a pull-request reply record, a review-fix record, or a
pull-request review analysis. A board without them loads in 0.0.3. A board with
them is skipped by 0.0.3 at startup. 0.0.3 refuses to save that Project and leaves
the file unchanged. Other Projects load normally. Downgrading is therefore a manual
procedure, not a supported rollback. Restore a backup taken before the first 0.0.4
launch, or reinstall 0.0.4 to read the board again. The restore procedure has not
been tested with a 0.0.3 binary.

An older build also keeps a Task's Activity above 200 entries as it is until it
records a new entry. Then it removes the oldest tool entries first. It removes
lifecycle milestones only when no tool entry is left.

### The Linux AppImage may fail to start

On some Linux systems the operating system ends SlashIt during startup (a
SIGKILL). The AppImage bundles its own copy of WebKitGTK, and that copy's
realtime scheduling watchdog is the established trigger. This is tracked in
[issue #269](https://github.com/BarraDev/slashit/issues/269), which remains open.

This release keeps the temporary startup mitigation from 0.0.3. When the loaded
WebKitGTK is older than 2.54.1, or its version cannot be read, SlashIt lowers the
soft realtime-priority limit of its own process before the window opens. WebKitGTK
2.54.1 and later bypass the mitigation. The 0.0.3 AppImage was recorded on one
host only. The 0.0.4 AppImage has not been validated.

- If startup fails, you may start SlashIt again. That may succeed.
- If SlashIt is ended during startup on your system, the in-app updater cannot
  bring it to a newer version, because the updater runs inside the application
  that does not start. Install the newer release by hand from the release page,
  using the file for your system from the table above.
- Terminals and agent processes that SlashIt starts inherit the lower soft limit.
  A plain `chrt -f` is refused while it is in place. A process can raise its soft
  limit back to the inherited hard limit without privilege, and software that
  needs realtime scheduling may need to do so.
- The `.deb` and `.rpm` packages use the WebKitGTK installed on your system. The
  same version check applies to them. Whether the mitigation applies depends on
  that version. Other distributions were not tested.

### Git over HTTPS from the AppImage

The AppImage no longer passes its bundled library paths to the host `git` and
its helpers. Before this fix, a push from the packaged application could fail
with a symbol lookup error and no pull request was created. The fix is covered by
tests, but it has not yet been confirmed in a packaged 0.0.4 AppImage.

### The installers are not code-signed

- **Windows:** SmartScreen shows "Windows protected your PC" on first run. Choose
  "More info", then "Run anyway". Not tested on a Windows machine.
- **macOS:** Gatekeeper refuses the first launch of an app that is not notarized.
  On recent macOS versions, open System Settings, go to Privacy & Security, and
  choose Open Anyway for SlashIt. Control-click and Open may not bypass the check
  on newer systems. Not tested on a Mac. If macOS reports that the app is damaged,
  that is a different message, and this page does not cover it. Do not turn off
  Gatekeeper or other system-wide protections.

These warnings have not been observed on real machines for this release.

The updater's signatures are a separate mechanism. They check that an update
came from SlashIt's release process and was not altered. They do not sign the
installers and do not suppress the warnings above. See
[`releasing.md`](releasing.md), section 7.

### Installation and launch are not verified on every platform

For each platform, the table above and the sections below say what has been
checked. For this release, nothing in this guide is a claim of a successful
installation or launch.

Not verified for 0.0.4: installation of any artifact; launch of any artifact; the
embedded terminal; media playback; the `.deb` and `.rpm` packages on any system;
Windows and macOS installers of any kind; and every Linux distribution.

### The update path is not verified for this version

A controlled in-app update from the published 0.0.2 AppImage to the published
0.0.3 AppImage was recorded on one host (see the
[0.0.3 verification](#verification-of-003-2026-10-29)). No update to or from
0.0.4 has been run. The updater's own signature check was not observed in that
run. Interrupted updates have not been tested, and no other platform has been
tested.

## Linux

### AppImage

The contents of the 0.0.4 AppImage have not been checked. The release procedure
lists them for inspection in [`releasing.md`](releasing.md), section 5. The
AppImage is not expected to include the `slashit` command-line client. Use the
`.deb` or `.rpm` for that.

```bash
chmod +x SlashIt_0.0.4_amd64.AppImage
./SlashIt_0.0.4_amd64.AppImage
```

Not verified for 0.0.4. The 0.0.3 AppImage was launched on one Arch Linux host
(see the [0.0.3 verification](#verification-of-003-2026-10-29)). Other desktops
are not tested. Whether the AppImage adds a menu entry on your desktop has not been
checked.

Remove it by deleting the file. Your data stays (see [Your data](#your-data)).

### Debian and Ubuntu (`.deb`)

The package name is `slash-it`, not `slashit`.

```bash
sudo apt install ./SlashIt_0.0.4_amd64.deb
slashit --version
```

Not verified for 0.0.4. The 0.0.3 package declares dependencies on
`libwebkit2gtk-4.1-0`, `libgtk-3-0`, and `libayatana-appindicator3-1`, without
version constraints. Not verified: the 0.0.4 package's dependencies, an actual
install, and the commands above on a real system.

The 0.0.4 package is expected to install:

- `/usr/bin/slashit` (the command-line client)
- `/usr/bin/slashitd` (the headless daemon)
- `/usr/bin/slashit-ui` (the desktop app)
- a desktop entry, `SlashIt`, which starts `slashit-ui`, and its icons

It has no install or removal scripts. Removing it does not touch your data:

```bash
sudo apt remove slash-it
```

This removal command has not been run.

### Fedora and RHEL-like systems (`.rpm`)

```bash
sudo dnf install ./SlashIt-0.0.4-1.x86_64.rpm
slashit --version
```

Not tested. The package is expected to contain `/usr/bin/slashit`,
`/usr/bin/slashitd`, `/usr/bin/slashit-ui`, the desktop entry and the license.
Its system dependencies were not checked, so they may differ from the `.deb`.
The commands above are not verified on any distribution.

Remove it with `sudo dnf remove slash-it`. That command is not tested either.

### Arch Linux

Arch users can build the local `slashit-bin` recipe in
[`packaging/arch/`](../packaging/arch/README.md). That recipe still packages the
0.0.3 `.deb`. It is not yet a 0.0.4 package, and it is not published to the AUR.

## Windows

Not verified for 0.0.4. Expected installers:

- `SlashIt_0.0.4_x64-setup.exe` (NSIS)
- `SlashIt_0.0.4_x64_en-US.msi`

The release workflow builds both on a Windows runner and does not package the
command-line client for Windows. Whether the headless daemon is included has not
been checked against a real Windows artifact.

Run the installer, and expect the SmartScreen warning described above. Removal
through Settings > Apps is not tested.

## macOS

Not verified for 0.0.4. Expected installers:

- `SlashIt_0.0.4_aarch64.dmg` for Apple silicon
- `SlashIt_0.0.4_x64.dmg` for Intel

Open the disk image, drag SlashIt to Applications, and start it from the
Gatekeeper procedure described above. The command-line client is not part of the
macOS installer. Whether `slashitd` is included has not been checked.

## Updates

The in-app updater is on by default. It lists the published releases, skips
drafts, and offers a prerelease only to installs on the Prerelease update
channel.

A new install uses the **Stable** channel. No stable release exists yet, so after
you install 0.0.4 you will not be offered later updates until you select the
**Prerelease** channel in Settings. The Stable channel does not offer prereleases,
so 0.0.4 is offered only on the Prerelease channel. Choose it if you want to
receive later prereleases. It downloads over HTTPS only and verifies each update's
signature before installing it. Updates to a prerelease are offered only after the
prerelease is published.

To turn the updater off, set `auto_update = false` in `features.toml` in
SlashIt's configuration directory and restart SlashIt. The updater does not replace
installs made from `.deb` or `.rpm` packages. Install the newer package from the
release page for those.

## Your data

SlashIt keeps its state in these directories:

| Platform | Configuration | Data |
|---|---|---|
| Linux | `~/.config/slashit-app/` | `~/.local/share/slashit-app/` |
| macOS | `~/Library/Application Support/com.barradev.slashit-app/` | same as configuration |
| Windows | `%APPDATA%\barradev\slashit-app\config\` | `%APPDATA%\barradev\slashit-app\data\` |

Removing SlashIt does not remove these directories. The `.deb` and `.rpm` have no
removal scripts, and the AppImage is a single file. The Windows and macOS
uninstallers have not been tested, so do not assume they leave your data alone
or remove it. Back up these directories before you install this version, and
before you remove SlashIt if you want to keep your boards.

Projects can keep their board inside the repository (`.slashit/`). That folder is
not affected by uninstalling SlashIt. See
[`architecture/state-locations.md`](architecture/state-locations.md).

Before you install 0.0.4, and before you go back to 0.0.3, quit SlashIt and
`slashitd`, then copy these together, outside the directories themselves:

- the configuration and data directories above, including the `worktrees/`
  folder under the data directory, which can hold uncommitted work;
- the `.slashit/` folder of each Project that keeps its board in its repository;
- the Git state of each Project's repository, which holds the Task branches and
  the `refs/slashit/` references. Restoring the directories without it can leave
  worktree references inconsistent.

Record a checksum of the copy. Restore everything together, with SlashIt closed.
Changes made in 0.0.4 after the backup are lost when you restore it. Changes
made on GitHub, such as pull requests and comments, are not restored by any file
copy.

## Reporting a problem

Open an issue at <https://github.com/BarraDev/slashit/issues>. Include your
platform, the installer you used, and the first lines of any error message.
