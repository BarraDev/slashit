# Releasing SlashIt

Pushing a `v*` tag builds installers for Linux, macOS (Intel and Apple
Silicon) and Windows, and opens a **draft** GitHub release with them attached.
Publishing that draft is a manual step, and until someone takes it nothing is
visible to users and nothing is visible to the auto-updater.

This document is the whole procedure, and the reasoning behind the two
constraints that are easy to get wrong: the version grammar the Windows
installer accepts, and what "signed" does and does not mean here.

## The short version

1. Bump every version in [the table below](#1-bump-the-version) to the same value.
2. Move the `[Unreleased]` section of `CHANGELOG.md` under the new version.
3. Commit, then tag `v<version>` and push the tag.
4. Watch the run. The preflight job rejects a bad version in seconds.
5. Download one artifact per platform, install it, launch it.
6. Publish the draft release.
7. If it is a stable release, confirm the updater endpoint resolves.

## 1. Bump the version

Five files carry the version, and the preflight job fails the release if any of
them disagree. There is no single source: Cargo needs it per manifest, and
Tauri reads its own.

| File | Field | What it controls |
|---|---|---|
| `src-tauri/tauri.conf.json` | `version` | **The released version.** Installer metadata, bundle version, the version the updater compares against |
| `src-tauri/Cargo.toml` | `[package] version` | The GUI crate (`slashit-ui`) |
| `Cargo.toml` (root) | `[package] version` | The Leptos/WASM frontend (`slashit-frontend`) |
| `crates/slashit-cli/Cargo.toml` | `[package] version` | `slashit --version` |
| `crates/slashit-ipc/Cargo.toml` | `[package] version` | The shared IPC crate |

`tauri.conf.json` is authoritative because Tauri only falls back to
`src-tauri/Cargo.toml` when the key is absent, and here it is present. The
preflight job checks the four manifests against it, so a build whose installer
and whose `slashit --version` disagree cannot get out.

Then refresh the lockfile, which records workspace member versions:

```bash
cargo check --workspace
```

`package.json` also says `0.1.0`, but it is the private Playwright end-to-end
harness (`slashit-e2e`), is never published, and is not checked by preflight.
Leave it alone unless you have a reason.

## 2. Choose a version the Windows installer accepts

**Named prereleases such as `0.1.0-rc.1` cannot be packaged as an MSI.** Use a
numeric prerelease instead: `0.1.0-1`, `0.1.0-2`.

`bundle.targets` is `"all"`, so a Windows release builds both an MSI and an
NSIS installer, and MSI is attempted first. WiX, which builds the MSI, requires
a numeric `major.minor.patch.build` version, so `tauri-bundler` converts the
app version to that shape and refuses anything it cannot convert:

| App version | WiX version | Result |
|---|---|---|
| `0.1.0` | `0.1.0` | builds |
| `0.1.0-1` | `0.1.0.1` | builds |
| `0.1.0-rc.1` | — | **error**, no Windows artifact |
| `0.1.0-alpha` | — | **error**, no Windows artifact |

The ranges are bounded too: major and minor at most 255, patch and the
prerelease number at most 65535.

Left alone, this failure is expensive and badly timed. It is not a
configuration error caught at startup — it happens at bundling time, after the
entire release build has compiled on the Windows runner, and only there. Linux
and macOS succeed, so the release is created and looks nearly complete while
Windows users have nothing.

So the workflow refuses the version up front. The `preflight` job encodes
exactly the bundler's rule and fails in seconds with a message naming the
allowed form, before any platform build starts.

Build metadata (`0.1.0+1`) is the one place the guard is deliberately stricter
than the bundler, which would convert it the same way a numeric prerelease is
converted. Semver excludes build metadata from version precedence, so `0.1.0+1`
and `0.1.0+2` are *equal* versions: the updater would never offer one as an
upgrade over the other while the two installers claimed to differ.

### Why not the `wix.version` escape hatch

`bundle.windows.wix.version` in `tauri.conf.json` overrides the derived version
and would let `0.1.0-rc.1` through. It is rejected here for two reasons: it is
a second hard-coded version to keep in step with the other five, and when it
falls out of step nothing complains — the MSI simply advertises a version that
is not the app's. A guard that refuses a version the toolchain cannot package
is honest; an override that makes the installer lie about its version is not.

### One caveat on numeric prereleases

Windows Installer compares only the first three fields of `ProductVersion`.
`0.1.0-1` and `0.1.0-2` become `0.1.0.1` and `0.1.0.2`, which it considers the
same version, so installing the second over the first is not treated as an
upgrade. Numeric prereleases are fine for handing a build to testers; they are
not a channel to iterate in on Windows. Ship the real patch version instead.

## 3. Tag and push

The tag must be exactly `v` followed by the version in `tauri.conf.json`. The
preflight job compares them and fails otherwise.

```bash
git tag -a v0.1.0 -m 'SlashIt v0.1.0'
git push origin v0.1.0
```

This project's normal workflow is Jujutsu, but `jj` treats git tags as
read-only — it imports them and cannot create them. Create the tag with `git`
in the colocated repository, as above.

If the tag was wrong, delete it locally and on the remote, fix the versions,
and tag again. Never move a tag that has already produced a published release.

## 4. What the workflow does

`.github/workflows/release.yml`, triggered by the `v*` tag push:

1. **`preflight`** (Ubuntu, seconds, no build) — the tag matches
   `tauri.conf.json`; all four manifests match it; the version is packageable
   as an MSI. Everything else waits on this job.
2. **`release`** (four runners in parallel) — builds the frontend with Trunk
   and the app with `tauri-action`, then creates or updates a **draft** release
   and uploads each platform's installers plus `latest.json`, the updater
   manifest.
3. **`next-steps`** — writes the remaining manual steps into the run summary,
   including whether this particular version will be reachable by the updater.

The release is named from the git ref, not from `tauri-action`'s `__VERSION__`
placeholder. `__VERSION__` resolves from `tauri.conf.json`, so a tag and an app
version that disagreed used to produce a release named after the app version
while the pushed tag said something else — with no error anywhere. Preflight
now makes that state unreachable, and the ref-based name means it cannot come
back.

## 5. Publish the draft

Nothing in CI verifies that an installer installs or that the app launches, so
that is the human's job:

- Linux: install the `.deb` or run the `.AppImage`.
- macOS: mount the `.dmg`, drag, launch. Gatekeeper will warn — see below.
- Windows: run the `.msi` and the NSIS `.exe`. SmartScreen will warn.

Then publish the release on GitHub. The prerelease flag is already set from the
tag, so there is nothing to adjust — press Publish.

## 6. Validate auto-update

The updater endpoint configured in `tauri.conf.json` is:

```
https://github.com/BarraDev/slashit/releases/latest/download/latest.json
```

**GitHub's `latest` excludes drafts and prereleases.** Two consequences worth
being explicit about, because both look like bugs otherwise:

- While the release is a draft, the endpoint resolves to the previous stable
  release or, before the first one, to nothing at all. Auto-update starts
  working when you press Publish, not when the build finishes.
- A published prerelease is still invisible to the updater. That is the
  intended behaviour — existing installs must not be auto-upgraded onto a
  release candidate — but it means a prerelease has no update channel. Testers
  install and update it by hand.

After publishing a stable release, check that the manifest is reachable and
points at the version you just shipped:

```bash
curl -sSL https://github.com/BarraDev/slashit/releases/latest/download/latest.json
```

It should contain the new `version`, and a `platforms` entry per target with a
`url` and a `signature`. Then confirm end to end: install the *previous*
release, launch it, and let it offer the update.

## 7. Signing, stated accurately

Two different things get called signing, and only one of them happens here.

**The updater is signed.** `TAURI_SIGNING_PRIVATE_KEY` and its password sign
`latest.json` and the update artifacts. The public key is embedded in the app
(`plugins.updater.pubkey`), so a running SlashIt verifies any update it
downloads and refuses one that was tampered with in transit or on the release
page. The private key lives only in GitHub Actions secrets; losing it means
existing installs can never be auto-updated again, and a new key only reaches
users through a manually installed build.

**The installers are not code-signed.** There is no Windows Authenticode
certificate and no Apple Developer ID signature or notarization. In practice:

- Windows SmartScreen shows "Windows protected your PC" on first run; the user
  has to choose "More info" then "Run anyway".
- macOS Gatekeeper refuses the first launch; the user has to open it from the
  context menu, or clear the quarantine attribute.

This is a cost decision, not an oversight — both require a paid certificate or
membership. It has to be said plainly in the release notes and the README,
because a security warning that nobody warned users about is indistinguishable
from a compromised download.

## 8. Before the first release

`0.1.0` has never been published. Until it is, the README's Releases link goes
to an empty page and the updater endpoint resolves to nothing — both expected,
neither a bug.
