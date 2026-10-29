# Installing SlashIt

SlashIt 0.0.3 is an **experimental prerelease**. It is not production-ready, and
it has not been installed and launched on every platform. Read the
[known issues](#known-issues) before you install it.

This page names each installer the release produces, says what was verified for
it and what was not, and describes how to install, remove and update it. The
release notes in [`CHANGELOG.md`](../CHANGELOG.md) list what changed.

The files below are what the release workflow is expected to produce for a
`v0.0.3` tag. Their names follow the pattern used for 0.0.2, and they have not
been checked against the built artifacts. The verification recorded for the
0.0.2 prerelease applies to that build only. It is in
[the 0.0.2 guide](https://github.com/BarraDev/slashit/blob/v0.0.2/docs/installation.md).

## Choose an installer

| Platform | File | Architecture | Verified for 0.0.3 |
|---|---|---|---|
| Linux | `SlashIt_0.0.3_amd64.AppImage` | x86_64 | Partly: launched and quit cleanly on one host. See [the verification note](#verification-update-2026-10-29). |
| Linux (Debian, Ubuntu) | `SlashIt_0.0.3_amd64.deb` | x86_64 | Not yet. |
| Linux (Fedora, RHEL-like) | `SlashIt-0.0.3-1.x86_64.rpm` | x86_64 | Not yet. |
| Windows | `SlashIt_0.0.3_x64-setup.exe` (NSIS) | x64 | Not yet. |
| Windows | `SlashIt_0.0.3_x64_en-US.msi` | x64 | Not yet. |
| macOS, Apple silicon | `SlashIt_0.0.3_aarch64.dmg` | arm64 | Not yet. |
| macOS, Intel | `SlashIt_0.0.3_x64.dmg` | x86_64 | Not yet. |

The release also contains `.sig` files, `latest.json`, and macOS `.app.tar.gz`
files. These are for the updater, not for installation. Do not install them.

Apart from the Linux AppImage, which was launched on one host, no artifact of
this release has been installed or launched. Treat every other platform as
untested until this page says otherwise.

## Verification update (2026-10-29)

This note was added after the release was published. It supersedes the
statements about the Linux AppImage and the update path that appear below it,
and in the 0.0.3 entry of [`CHANGELOG.md`](../CHANGELOG.md). Those statements
are kept as they were written at publication.

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
- **Not verified.** The updater's own signature check was not observed, though
  the artifact's signature was verified separately. The embedded terminal check
  was waived for this release. Media playback, macOS, Windows, other Linux
  distributions, the `.deb` and `.rpm` packages, and an interrupted update were
  not tested.
- **Issue #269 remains open.** The upstream WebKitGTK behavior is not fixed. The
  mitigation is temporary.

## Known issues

### The Linux AppImage may fail to start

On some Linux systems the operating system ends SlashIt during startup (a
SIGKILL). The AppImage bundles its own copy of WebKitGTK, and that copy's
realtime scheduling watchdog is the established trigger. This is tracked in
[issue #269](https://github.com/BarraDev/slashit/issues/269), which remains open.

This release adds a temporary startup mitigation. When the loaded WebKitGTK is
older than 2.54.1, or its version cannot be read, SlashIt lowers the soft
realtime-priority limit of its own process before the window opens. WebKitGTK
2.54.1 and later bypass the mitigation. The official AppImage of this release
has been validated on one host only (see the
[verification note](#verification-update-2026-10-29)); other systems are not
covered.

- If startup fails, you may start SlashIt again. That may succeed, but **repeated
  launches are not a guaranteed workaround**.
- If SlashIt 0.0.2 is ended during startup on your system, the in-app updater
  cannot bring it to 0.0.3, because the updater runs inside the application that
  does not start. Install 0.0.3 by hand from the release page, using the file for
  your system from the table above.
- Terminals and agent processes that SlashIt starts inherit the lower soft limit.
  A plain `chrt -f` is refused while it is in place. A process can raise its soft
  limit back to the inherited hard limit without privilege, and software that
  needs realtime scheduling may need to do so.
- The `.deb` and `.rpm` packages use the WebKitGTK installed on your system. The
  same version check applies to them. Whether the mitigation applies depends on
  that version. Other distributions were not tested.

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

The Linux AppImage is the only artifact of this release that has been launched,
and only on one host. For each platform, the table above and the sections below
say what has been checked. Apart from that AppImage launch, nothing in this guide
is a claim of a successful installation or launch for 0.0.3.

Not verified for 0.0.3: installation of any artifact; launch of any artifact
other than the official AppImage, and of that AppImage on any other desktop;
the embedded terminal check, which was waived; the `.deb` and `.rpm` packages on
any system; Windows and macOS installers of any kind; and every Linux
distribution other than the test host.

### The update path is verified only in part

The updater signing probe passed for the production key. A controlled in-app
update from the published 0.0.2 AppImage to the published 0.0.3 AppImage passed
on one host (see the [verification note](#verification-update-2026-10-29)). That
run is not a general update test. The updater's own signature check was not
observed. Interrupted updates have not been tested, and no other platform has
been tested.

## Linux

### AppImage

Expected contents: `slashitd` and `slashit-ui`. The AppImage does not include the
`slashit` command-line client. Use the `.deb` or `.rpm` for that.

```bash
chmod +x SlashIt_0.0.3_amd64.AppImage
./SlashIt_0.0.3_amd64.AppImage
```

Launched on one Arch Linux host, with the results in the
[verification note](#verification-update-2026-10-29). Other desktops are not
tested. Whether the AppImage adds a menu entry on your desktop has not been
checked.

Remove it by deleting the file. Your data stays (see [Your data](#your-data)).

### Debian and Ubuntu (`.deb`)

The package name is `slash-it`, not `slashit`.

```bash
sudo apt install ./SlashIt_0.0.3_amd64.deb
slashit --version
```

Not verified for 0.0.3. The 0.0.2 package's dependency set resolved in a
simulated install on Ubuntu 22.04. The package is expected to require
`libwebkit2gtk-4.1-0`, `libgtk-3-0`, and `libayatana-appindicator3-1`. Not
verified: an actual install, and the commands above on a real system.

The package is expected to install:

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
sudo dnf install ./SlashIt-0.0.3-1.x86_64.rpm
slashit --version
```

Not tested. The package is expected to contain `/usr/bin/slashit`,
`/usr/bin/slashitd`, `/usr/bin/slashit-ui`, the desktop entry and the license.
Its system dependencies were not checked, so they may differ from the `.deb`.
The commands above are not verified on any distribution.

Remove it with `sudo dnf remove slash-it`. That command is not tested either.

## Windows

Not verified for 0.0.3. Expected installers:

- `SlashIt_0.0.3_x64-setup.exe` (NSIS)
- `SlashIt_0.0.3_x64_en-US.msi`

The release workflow builds both on a Windows runner and does not package the
command-line client for Windows. Whether the headless daemon is included has not
been checked against a real Windows artifact.

Run the installer, and expect the SmartScreen warning described above. Removal
through Settings > Apps is not tested.

## macOS

Not verified for 0.0.3. Expected installers:

- `SlashIt_0.0.3_aarch64.dmg` for Apple silicon
- `SlashIt_0.0.3_x64.dmg` for Intel

Open the disk image, drag SlashIt to Applications, and start it from the
Gatekeeper procedure described above. The command-line client is not part of the
macOS installer. Whether `slashitd` is included has not been checked.

## Updates

The in-app updater is on by default. It lists the published releases, skips
drafts, and offers a prerelease only to installs on the Prerelease update
channel.

A new install uses the **Stable** channel. No stable release exists yet, so
after you install 0.0.3 you will not be offered later updates until you select
the **Prerelease** channel in Settings. The same applies to an install of 0.0.2:
the Stable channel does not offer prereleases, so 0.0.3 is offered only on the
Prerelease channel. Choose it if you want to receive later
prereleases. It downloads over HTTPS only and verifies each update's signature before
installing it. Updates to a prerelease are offered only after the prerelease is
published.

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
or remove it. Back up these directories before you remove SlashIt if you want to
keep your boards.

Projects can keep their board inside the repository (`.slashit/`). That folder is
not affected by uninstalling SlashIt. See
[`architecture/state-locations.md`](architecture/state-locations.md).

## Reporting a problem

Open an issue at <https://github.com/BarraDev/slashit/issues>. Include your
platform, the installer you used, and the first lines of any error message.
