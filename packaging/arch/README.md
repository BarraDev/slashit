# Arch Linux packaging

These recipes package SlashIt for Arch Linux. They live in this repository so
that the repository is the single source for them. The AUR is not used as a
source of truth.

**Status.** Nothing here is published. The packages are not in the AUR, and no
`pacman -S slashit-bin` or AUR helper command is available yet. Build them
locally as described below. SlashIt 0.0.3 is an experimental prerelease; read
[the installation guide](../../docs/installation.md) before you install it.

## Packages

| Directory | Package | Contents | Build input |
|---|---|---|---|
| `slashit-bin/` | `slashit-bin` | The upstream `SlashIt_0.0.3_amd64.deb` release asset, unpacked and installed without changes. | Pinned `.deb` URL and SHA-256 on GitHub Releases. Nothing is compiled. |
| `slashit/` | `slashit` | The tagged source (`v0.0.3`), built as in `.github/workflows/release.yml`. | Pinned GitHub tag archive and SHA-256. Rust crates are fetched from crates.io and checked against the tagged `Cargo.lock`. |

The two packages conflict with each other, because both install
`/usr/bin/slashit-ui`. `slashit-bin` also provides `slashit`. Install one of
them, not both.

`slashit-bin` is the quicker option and runs the binaries that CI built and
the release published. `slashit` compiles from source with the same procedure
as the release workflow. It takes much longer, needs the build dependencies
below, and its output is not byte-identical to the release binaries.

## Build locally with makepkg

Run every command as a normal user. Do not run `makepkg` as root, and do not
use `makepkg -i`; install with `pacman -U` only when you decide to.

Keep build output out of the repository by setting the makepkg directories to a
scratch location:

```bash
export SLASHIT_SCRATCH="$HOME/.cache/agent-scratch/slashit-app/arch"
mkdir -p "$SLASHIT_SCRATCH"/{build,srcdest,pkgdest,srcpkgdest}
cd packaging/arch/slashit-bin          # or packaging/arch/slashit
BUILDDIR="$SLASHIT_SCRATCH/build" \
SRCDEST="$SLASHIT_SCRATCH/srcdest" \
PKGDEST="$SLASHIT_SCRATCH/pkgdest" \
SRCPKGDEST="$SLASHIT_SCRATCH/srcpkgdest" \
  makepkg -f --noconfirm
```

Regenerate `.SRCINFO` after any change to a PKGBUILD, with
`makepkg --printsrcinfo > .SRCINFO`. The AUR reads `.SRCINFO`, not the PKGBUILD.

Check a recipe before you build it:

```bash
namcap PKGBUILD
namcap "$SLASHIT_SCRATCH/pkgdest"/*.pkg.tar.zst
bsdtar -tvf "$SLASHIT_SCRATCH/pkgdest"/slashit-bin-0.0.3-1-x86_64.pkg.tar.zst
```

## Directories makepkg uses

| Variable | Default | Purpose |
|---|---|---|
| `BUILDDIR` | `$startdir` (the package directory) | Parent of `src/` and `pkg/`. Set it to scratch. |
| `srcdir` | `$BUILDDIR/<pkgbase>/src` | Where sources are extracted and built. |
| `pkgdir` | `$BUILDDIR/<pkgbase>/pkg` | The fake root the package is installed into. |
| `SRCDEST` | the package directory | Downloaded sources, reused between builds. |
| `PKGDEST` | the current directory | Where the finished `.pkg.tar.zst` is written. |
| `SRCPKGDEST` | the current directory | Where `--source` packages are written. |
| `CARGO_TARGET_DIR` | `srcdir/target` (set in the `slashit` PKGBUILD) | Compiler output for the source build. It is not shared with any other build. |

The source build has no separate compiler cache. The `slashit` recipe does not
use sccache or a shared Cargo target. Rust crates are cached by Cargo in
`$CARGO_HOME` (`~/.cargo` by default), which makepkg does not clean.

`makepkg -c` (`--clean`) removes `srcdir` and `pkgdir` after a successful build.
It does not remove `SRCDEST` (the downloaded tarball or `.deb`), the Cargo
registry, or the packages written to `PKGDEST`. Delete those yourself when you
no longer need them.

## Build dependencies

`slashit-bin` needs only the runtime dependencies listed in its PKGBUILD:
`webkit2gtk-4.1`, `gtk3`, `libsoup3`, `libayatana-appindicator` and `dbus`.

`slashit` also needs the makedepends in its PKGBUILD: `cargo`, `rust`,
`rust-wasm`, `trunk`, `wasm-bindgen`, `binaryen`, `librsvg` and `pkgconf`.
Install them with pacman yourself. The recipe does not install anything.

## Known gaps in the source recipe

These are open. They are listed so that nobody mistakes the recipe for a
verified build.

- It has not been built. `makepkg --verifysource` passes for the pinned tarball,
  and that is all that was run on it.
- `trunk` downloads Tailwind CSS 4.1.10 at build time (see `Trunk.toml`), and
  it uses a `wasm-bindgen` that must match `Cargo.lock` (0.2.114). The Arch
  `wasm-bindgen` package is 0.2.128, so `trunk` will fetch its own copy.
  Neither download is pinned by the PKGBUILD.
- Arch's `rust` is newer than the toolchain in `rust-toolchain.toml` (1.98.1).
  The system `cargo` ignores that file.

## Updates and the updater on Arch

SlashIt has a built-in updater. It checks which kind of install is running and
refuses to replace files it does not own. The refusal happens before any file is
changed. Source: `src-tauri/src/commands/updater.rs`, `install_kind` and
`unsupported_reason`.

- `slashit-bin`: the binary carries the Debian bundle marker that the upstream
  `.deb` build wrote into it. The updater classifies it as a Debian install and
  refuses to update. The refusal text says to use `apt` or `dpkg`, which is
  incorrect on Arch.
- `slashit`: the source build has no bundle marker, so the updater classifies it
  as an unpackaged binary and refuses to update. The refusal text suggests the
  AppImage, which is also not the Arch form.

On Arch, update by rebuilding the package from a newer recipe and installing
the result with `pacman -U`. The updater will not replace `/usr/bin/slashit-ui`.
The wording of the refusal is an open item for the maintainer.
