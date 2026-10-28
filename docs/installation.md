# Installing SlashIt

SlashIt 0.0.2 is an **experimental prerelease**. It is not production-ready, and
it has not been installed and launched on every platform. Read the
[known issues](#known-issues) before you install it.

This page names each installer the release produces, says what was verified for
it and what was not, and describes how to install, remove and update it. The
release notes in [`CHANGELOG.md`](../CHANGELOG.md) list what changed.

The files below are what the release workflow produces for a `v0.0.2` tag.
The Windows and macOS installers were not built or installed by the maintainers
before this release, so their filenames follow the pattern the workflow uses
for the earlier 0.0.1 draft.

## Choose an installer

| Platform | File | Architecture | Verified here |
|---|---|---|---|
| Linux | `SlashIt_0.0.2_amd64.AppImage` | x86_64 | Built and inspected locally. Launched once, locally, with a non-representative WebKitGTK (see below). |
| Linux (Debian, Ubuntu) | `SlashIt_0.0.2_amd64.deb` | x86_64 | Built and inspected locally. Dependency resolution simulated on Ubuntu 22.04. Not installed. |
| Linux (Fedora, RHEL-like) | `SlashIt-0.0.2-1.x86_64.rpm` | x86_64 | Built and inspected locally. Payload listed. Not installed. Dependencies not checked. |
| Windows | `SlashIt_0.0.2_x64-setup.exe` (NSIS) | x64 | Not built locally. Not installed. |
| Windows | `SlashIt_0.0.2_x64_en-US.msi` | x64 | Not built locally. Not installed. |
| macOS, Apple silicon | `SlashIt_0.0.2_aarch64.dmg` | arm64 | Not built locally. Not installed. |
| macOS, Intel | `SlashIt_0.0.2_x64.dmg` | x86_64 | Not built locally. Not installed. |

The release also contains `.sig` files, `latest.json`, and macOS `.app.tar.gz`
files. These are for the updater, not for installation. Do not install them.

No Linux artifact is verified on a real desktop. The Linux checks in this
document were run in an isolated environment on the build host.

## Known issues

### The Linux AppImage may fail to start

On some Linux systems the operating system ends SlashIt during startup (a
SIGKILL). The AppImage bundles its own copy of WebKitGTK, and that copy's
realtime scheduling watchdog is the established trigger. This is tracked in
[issue #269](https://github.com/BarraDev/slashit/issues/269). It is not fixed in
this release.

- If it happens, you may start SlashIt again. That may succeed, but **repeated
  launches are not a guaranteed workaround**.
- The `.deb` and `.rpm` packages use the WebKitGTK installed on your system.
  On Ubuntu 22.04 that is WebKitGTK 2.50.4, the same version the AppImage bundles,
  so the `.deb` is not known to be unaffected there. Other distributions were not
  tested.

### The installers are not code-signed

- **Windows:** SmartScreen shows "Windows protected your PC" on first run. Choose
  "More info", then "Run anyway". Not tested on a Windows machine.
- **macOS:** Gatekeeper refuses the first launch of an app that is not notarized.
  On recent macOS versions, open System Settings, go to Privacy & Security, and
  choose Open Anyway for SlashIt. Control-click and Open may not bypass the check
  on newer systems. Not tested on a Mac. If macOS reports that the app is damaged,
  that is a different message, and this page does not cover it. Do not turn off
  Gatekeeper or other system-wide protections.

These warnings have not been observed on real machines for this build.

The updater's signatures are a separate mechanism. They check that an update
came from SlashIt's release process and was not altered. They do not sign the
installers and do not suppress the warnings above. See
[`releasing.md`](releasing.md), section 7.

### Installation and launch are not verified on every platform

Verified for this build, locally, on the build host:

- The AppImage was extracted, and its contents were inspected.
- The AppImage was launched once with an isolated profile under a private
  display. It was still running when a 30-second timeout ended the test. Its
  log contains only desktop-portal and accessibility warnings from the private
  display. The build host has WebKitGTK 2.54.1, which the
  AppImage bundled. The CI AppImage bundles 2.50.4, so this is
  not evidence about the released AppImage or about issue #269.
- The `.deb` dependencies resolved in a simulated install on an Ubuntu 22.04
  base image. Nothing was installed.

Not verified: installation of any artifact, launch of any released artifact on
any real desktop, Windows and macOS installers of any kind, and the `.rpm` on
any Fedora or RHEL-like system.

### The update path is not tested end to end

The updater signing probe passed for the production key. An update installed
through the application has been exercised only in a local experiment with a
disposable test key. It has not been run against a published release. Until
that test passes, treat in-app updates as unverified.

## Linux

### AppImage

Contents: `slashitd` and `slashit-ui`. The AppImage does not include the
`slashit` command-line client. Use the `.deb` or `.rpm` for that.

```bash
chmod +x SlashIt_0.0.2_amd64.AppImage
./SlashIt_0.0.2_amd64.AppImage
```

Tested only as described under [Installation and launch](#installation-and-launch-are-not-verified-on-every-platform).
Whether the AppImage adds a menu entry on your desktop has not been checked.

Remove it by deleting the file. Your data stays (see [Your data](#your-data)).

### Debian and Ubuntu (`.deb`)

The package name is `slash-it`, not `slashit`.

```bash
sudo apt install ./SlashIt_0.0.2_amd64.deb
slashit --version
```

Verified: the dependency set resolves on Ubuntu 22.04 in a simulated install.
The package requires `libwebkit2gtk-4.1-0`, `libgtk-3-0`, and
`libayatana-appindicator3-1`. Not verified: an actual install, and the commands
above on a real system.

The package installs:

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
sudo dnf install ./SlashIt-0.0.2-1.x86_64.rpm
slashit --version
```

Not tested. The package contains `/usr/bin/slashit`, `/usr/bin/slashitd`,
`/usr/bin/slashit-ui`, the desktop entry and the license. Its system
dependencies were not checked, so they may differ from the `.deb`. The
commands above are not verified on any distribution.

Remove it with `sudo dnf remove slash-it`. That command is not tested either.

## Windows

Not built locally and not installed. Expected installers:

- `SlashIt_0.0.2_x64-setup.exe` (NSIS)
- `SlashIt_0.0.2_x64_en-US.msi`

The release workflow builds both on a Windows runner and does not package the
command-line client for Windows. Whether the headless daemon is included has not
been checked against a real Windows artifact.

Run the installer, and expect the SmartScreen warning described above. Removal
through Settings > Apps is not tested.

## macOS

Not built locally and not installed. Expected installers:

- `SlashIt_0.0.2_aarch64.dmg` for Apple silicon
- `SlashIt_0.0.2_x64.dmg` for Intel

Open the disk image, drag SlashIt to Applications, and start it from the
Gatekeeper procedure described above. The command-line client is not part of the
macOS installer. Whether `slashitd` is included has not been checked.

## Updates

The in-app updater is on by default. It lists the published releases, skips
drafts, and offers a prerelease only to installs on the Prerelease update
channel.

A new install uses the **Stable** channel. No stable release exists yet, so
after you install 0.0.2 you will not be offered later updates until you select
the **Prerelease** channel in Settings. Choose it if you want to receive later
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
