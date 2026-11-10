# Arch Linux packaging

This recipe packages SlashIt for Arch Linux. It lives in this repository so that
the repository is the single source for it. The AUR is not used as a source of
truth.

**Status.** Nothing here is published. The package is not in the AUR, and no
`pacman -S` or AUR helper command is available yet. Build it locally as described
below. SlashIt 0.0.3 is an experimental prerelease; read
[the installation guide](../../docs/installation.md) before you install it.

## Packages

| Directory | Package | Contents | Build input |
|---|---|---|---|
| `slashit-bin/` | `slashit-bin` | The upstream `SlashIt_0.0.3_amd64.deb` release asset, unpacked and installed without changes. | Pinned `.deb` URL and SHA-256 on GitHub Releases. Nothing is compiled. |

`slashit-bin` provides and conflicts with the name `slashit`, which is reserved
for a future source package. Do not install another SlashIt package alongside it.

### Deferred: source package

There is no source package in this repository yet. A source recipe built from the
`v0.0.3` tag would install the updater as it stands on that tag. That code
classifies any process started with an inherited `APPIMAGE` variable as an
AppImage. The tag does not contain the guard added in PR #335, so a source build
from it could replace a package-managed binary. Reconsider a source package only
after an official tagged release contains that guard.

## Build locally with makepkg

Run every command as a normal user. Do not run `makepkg` as root, and do not
use `makepkg -i`; install with `pacman -U` only when you decide to.

Keep build output out of the repository by setting the makepkg directories to a
scratch location:

```bash
export SLASHIT_SCRATCH="$HOME/.cache/agent-scratch/slashit-app/arch"
mkdir -p "$SLASHIT_SCRATCH"/{build,srcdest,pkgdest,srcpkgdest}
cd packaging/arch/slashit-bin
BUILDDIR="$SLASHIT_SCRATCH/build" \
SRCDEST="$SLASHIT_SCRATCH/srcdest" \
PKGDEST="$SLASHIT_SCRATCH/pkgdest" \
SRCPKGDEST="$SLASHIT_SCRATCH/srcpkgdest" \
  makepkg -f --noconfirm
```

Regenerate `.SRCINFO` after any change to the PKGBUILD, with
`makepkg --printsrcinfo > .SRCINFO`. The AUR reads `.SRCINFO`, not the PKGBUILD.

Check the recipe and inspect the package it produced:

```bash
namcap PKGBUILD
namcap "$SLASHIT_SCRATCH/pkgdest/slashit-bin-0.0.3-1-x86_64.pkg.tar.zst"
bsdtar -tvf "$SLASHIT_SCRATCH/pkgdest/slashit-bin-0.0.3-1-x86_64.pkg.tar.zst"
```

`namcap` reports the binaries as unstripped. That is intentional: the PKGBUILD
sets `!strip` so the installed files stay byte-identical to the release.

## Directories makepkg uses

| Variable | Default | Purpose |
|---|---|---|
| `BUILDDIR` | `$startdir` (the package directory) | Parent of `src/` and `pkg/`. Set it to scratch. |
| `srcdir` | `$BUILDDIR/<pkgbase>/src` | Where sources are extracted. |
| `pkgdir` | `$BUILDDIR/<pkgbase>/pkg` | The fake root the package is installed into. |
| `SRCDEST` | the package directory | Downloaded sources, reused between builds. |
| `PKGDEST` | the current directory | Where the finished `.pkg.tar.zst` is written. |
| `SRCPKGDEST` | the current directory | Where `--source` packages are written. |

`makepkg -c` (`--clean`) removes `srcdir` and `pkgdir` after a successful build.
It does not remove `SRCDEST` (the downloaded `.deb`) or the packages written to
`PKGDEST`. Delete those yourself when you no longer need them.

## Dependencies

`makepkg` comes from the `base-devel` group. Install it with
`sudo pacman -S --needed base-devel` if it is missing.

`slashit-bin` needs the runtime dependencies listed in its PKGBUILD:
`webkit2gtk-4.1`, `gtk3`, `libsoup3`, `libayatana-appindicator` and `dbus`.
pacman installs them when you install the package.

## Install and remove

After a local build, install the package file with pacman:

```bash
sudo pacman -U "$SLASHIT_SCRATCH/pkgdest/slashit-bin-0.0.3-1-x86_64.pkg.tar.zst"
```

Remove it with:

```bash
sudo pacman -R slashit-bin
```

Pacman removes only the files the package owns. Your SlashIt data directories
are not owned by the package, so removing it leaves them in place.

## Updates and the updater on Arch

SlashIt has a built-in updater. It checks which kind of install is running and
refuses to replace files it does not own. The refusal happens before any file is
changed. Source: `src-tauri/src/commands/updater.rs`, `install_kind` and
`unsupported_reason`.

`slashit-bin` carries the Debian bundle marker that the upstream `.deb` build
wrote into the binary. The marker is checked before the `APPIMAGE` variable, so
an inherited `APPIMAGE` cannot change the classification. The updater refuses to
update and, after PR #335, names `pacman` as the package manager to use.

On Arch, update by building a newer recipe and installing it with `pacman -U`.
The updater does not replace `/usr/bin/slashit-ui`.

## Known gaps

- The binary package has been built and inspected locally. Its launch, daemon and
  desktop-integration behavior is not verified here.
- Isolated Pacman install and removal were run earlier; see the PR history.

## Owner decisions

These decisions were made by the owner and are recorded here so they stay with
the recipe. Source: the engineering plan in PR #334.

- Package names `slashit-bin` and `slashit` are approved, conditional on AUR
  namespace and ownership verification. This PR ships only `slashit-bin`; the
  name `slashit` is reserved for a future source package.
- Recipes live under `packaging/arch/` in this repository. No Git submodules.
- Local packaging development and validation are authorized. Public AUR
  publication is not authorized. No publication credential is configured, no AUR
  repository is pushed, and no package is installed into the owner's normal
  environment without a separate authorization.
- Maintainer identity: `Shingonoide <shingonoide at gmail dot com>`.
