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
2. Move the `[Unreleased]` section of `CHANGELOG.md` under the new version, and write its reference link (see [Version links](#version-links-in-changelogmd)).
3. Commit, then tag `v<version>` and push the tag.
4. Watch the run. The preflight job rejects a bad version in seconds.
5. Download one artifact per platform, install it, launch it.
6. Publish the draft release.
7. If it is a stable release, confirm the updater endpoint resolves.

## 1. Bump the version

Eleven files carry the version, and the preflight job fails the release if any
of them disagree. There is no single source: Cargo needs it per manifest, and
Tauri reads its own.

| File | Field | What it controls |
|---|---|---|
| `src-tauri/tauri.conf.json` | `version` | **The released version.** Installer metadata, bundle version, the version the updater compares against |
| `src-tauri/Cargo.toml` | `[package] version` | The GUI crate (`slashit-ui`) |
| `Cargo.toml` (root) | `[package] version` | The Leptos/WASM frontend (`slashit-frontend`) |
| `crates/slashit-cli/Cargo.toml` | `[package] version` | `slashit --version` |
| `crates/slashit-ipc/Cargo.toml` | `[package] version` | The shared IPC crate |
| `crates/slashit-acceptance`, `slashit-activity`, `slashit-attention`, `slashit-durable-fs`, `slashit-review-content` (each `Cargo.toml`) | `[package] version` | Workspace members, not shipped on their own |
| `package.json` | `version` | The private Playwright end-to-end harness (`slashit-e2e`). Never published, but preflight checks it so the workspace has one version |

`tauri.conf.json` is authoritative because Tauri only falls back to
`src-tauri/Cargo.toml` when the key is absent, and here it is present. The
preflight job checks every file above against it, so a build whose installer
and whose `slashit --version` disagree cannot get out.

Then refresh the lockfile, which records workspace member versions:

```bash
cargo check --workspace
```

## 2. Choose a version the Windows installer accepts

Every published version is plain `MAJOR.MINOR.PATCH`: three numbers, no
prerelease or build suffix, and no fourth component. `0.0.1`, `0.0.2` and
`0.1.0` are versions; `0.1.0-1`, `0.1.0-rc.1` and `0.1.0+1` are not, and the
preflight job refuses them on every platform.

The reason is the Windows installer. `bundle.targets` is `"all"`, so a Windows
release builds both an MSI and an NSIS installer, and MSI is attempted first.
WiX requires a numeric `major.minor.patch.build` version, so `tauri-bundler`
converts the app version to that shape and refuses anything it cannot convert.
A plain version converts to `major.minor.patch.0`, which orders the same way:

| App version | WiX version | Result |
|---|---|---|
| `0.0.1` | `0.0.1.0` | builds |
| `0.1.0` | `0.1.0.0` | builds |
| `0.1.0-1` | — | **refused by the preflight**: a suffix is not a published version |
| `0.1.0-rc.1` | — | **refused by the preflight**, and an error in the bundler |

The ranges are bounded too: major and minor at most 255, patch at most 65535.

Left alone, a bad version is expensive and badly timed. It is not a
configuration error caught at startup — it happens at bundling time, after the
entire release build has compiled on the Windows runner, and only there. Linux
and macOS succeed, so the release is created and looks nearly complete while
Windows users have nothing. So the `preflight` job encodes the bundler's rule and
refuses the version up front, in seconds, with a message naming the allowed
form, before any platform build starts.

Versions only increase. Each line of `.github/release-channels.txt` must name a
later version than the line above it, and a release must be the latest declared
version. The updater compares these numbers, so a version published after a
later one would move the update channel backwards.

Build metadata (`0.1.0+1`) is refused as well. Semver excludes build metadata
from version precedence, so `0.1.0+1` and `0.1.0+2` are *equal* versions: the
updater would never offer one as an upgrade over the other while the two
installers claimed to differ.

### Why not the `wix.version` escape hatch

`bundle.windows.wix.version` in `tauri.conf.json` overrides the derived version
and would let `0.1.0-rc.1` through. It is rejected here for two reasons: it is
a second hard-coded version to keep in step with the other five, and when it
falls out of step nothing complains — the MSI simply advertises a version that
is not the app's. A guard that refuses a version the toolchain cannot package
is honest; an override that makes the installer lie about its version is not.

## 3. Tag and push

The tag must be exactly `v` followed by the version in `tauri.conf.json`. The
preflight job compares them and fails otherwise. The example below tags the
planned `0.0.1` prerelease; substitute the version your `tauri.conf.json` names.

```bash
git tag -a v0.0.1 -m 'SlashIt v0.0.1'
git push origin v0.0.1
```

This project's normal workflow is Jujutsu, but `jj` treats git tags as
read-only — it imports them and cannot create them. Create the tag with `git`
in the colocated repository, as above.

If the tag was wrong, delete it locally and on the remote, fix the versions,
and tag again. Never move a tag that has already produced a published release.

## 4. What the workflow does

`.github/workflows/release.yml`, triggered by the `v*` tag push:

1. **`preflight`** (Ubuntu, seconds, no build) — runs
   `scripts/release-preflight.sh "$TAG"`, which checks that the tag matches
   `tauri.conf.json`; that every version manifest matches it; that the version is
   packageable as an MSI; that `CHANGELOG.md` has a `## [<version>]` section and
   its `[<version>]: https://` link; and that `.github/release-channels.txt`
   declares the version's channel. It writes the channel as the job output that
   sets the release's `prerelease` flag. Everything else waits on this job.
2. **`release`** (four runners in parallel) — builds the frontend with Trunk
   and the app with `tauri-action`, then creates or updates a **draft** release
   and uploads each platform's installers plus `latest.json`, the updater
   manifest. The Linux runner first runs `cargo build -p slashit --release`,
   because the `.deb` and `.rpm` packages copy that CLI binary to
   `/usr/bin/slashit`. The Linux packages (`.deb`, `.rpm`, `.AppImage`) also carry
   `slashitd`, built from the `slashit-ui` package and bundled without a
   `tauri.conf.json` entry, so a change to the bin layout can drop it silently;
   section 5 says what to check. The macOS
   and Windows installers do not include the CLI, and whether they include
   `slashitd` is checked on the artifacts, not assumed.
3. **`next-steps`** — writes the remaining manual steps into the run summary,
   including whether this particular version will be reachable by the updater.

The release body links the `CHANGELOG.md` at the tag, which is the curated
account of the release. It does not list pull requests: GitHub's generated
notes would list every merged pull request in the repository (over 150 for the
first release), which describes the project's history rather than this version.
Commits pushed straight to `main` have no pull request and appear only in
`CHANGELOG.md`.

### Release channels

`.github/release-channels.txt` names the channel of every version the workflow
may publish, one line each: `<version> prerelease` or `<version> stable`. The
channel is read from that file and from nothing else. The tag's shape does not
decide it, so `v0.0.1` has no hyphen and is still a prerelease only because the
file says so.

The preflight job fails closed:

- a version with no line, or with two, is refused;
- a channel other than `prerelease` or `stable` is refused;
- a line that is not a plain version, or that is not later than the line above
  it, is refused, and so is a release that is not the latest declared version;
- the preflight output `prerelease` is the GitHub `prerelease` flag, so the
  flag cannot disagree with the declaration.

The version does not decide the channel: `0.0.1` is a prerelease because its
line says so, and a later `0.0.3 stable` line is a stable release with no hyphen.

The application reads the same file at build time (`src-tauri/build.rs`) and
records the channel the build was published on, so an installed build reports
`0.0.1 · Prerelease` whatever update channel the person has selected.

Declare a stable version in the same commit that prepares its CHANGELOG
section, and check the file's line before you tag.

`scripts/test-release-preflight.sh` runs the script against the repository and
against fixture trees, so a change to the checks can be tested locally:

```bash
scripts/test-release-preflight.sh
```

To check a tag without pushing it, run the script with the tag and read the
channel it prints:

```bash
scripts/release-preflight.sh v0.0.1
```

### Version links in CHANGELOG.md

Each version has a reference link at the foot of `CHANGELOG.md`, in Keep a
Changelog form. Write it when you cut the release, in the same commit that moves
`[Unreleased]` under the version:

- The first release, the `0.0.1` prerelease, links to its tag:

  ```
  [0.0.1]: https://github.com/BarraDev/slashit/releases/tag/v0.0.1
  [Unreleased]: https://github.com/BarraDev/slashit/compare/v0.0.1...HEAD
  ```

- Each later release links to the comparison with the tag before it. The
  first stable release, `0.1.0`, follows the `0.0.1` prerelease:

  ```
  [0.1.0]: https://github.com/BarraDev/slashit/compare/v0.0.1...v0.1.0
  [Unreleased]: https://github.com/BarraDev/slashit/compare/v0.1.0...HEAD
  ```

Use the release tags as the boundaries, not `main` or dates. The comparison is
the authoritative list of commits for the release, and it is fixed once the tag
exists. The preflight job refuses a tag without its `[<version>]: https://`
definition.

Individual noteworthy entries may link their pull request, for example
`([#123](https://github.com/BarraDev/slashit/pull/123))`. Entries are not
required to, and the changelog does not mirror the pull request list.

The release is named from the git ref, not from `tauri-action`'s `__VERSION__`
placeholder. `__VERSION__` resolves from `tauri.conf.json`, so a tag and an app
version that disagreed used to produce a release named after the app version
while the pushed tag said something else — with no error anywhere. Preflight
now makes that state unreachable, and the ref-based name means it cannot come
back.

## 5. Publish the draft

Nothing in CI verifies that an installer installs or that the app launches, so
that is the human's job:

- Linux: install the `.deb` or `.rpm`, or run the `.AppImage`.
- macOS: mount the `.dmg`, drag, launch. Gatekeeper will warn — see below.
- Windows: run the `.msi` and the NSIS `.exe`. SmartScreen will warn.
- Payload, for every artifact: the `.deb` and `.rpm` contain `usr/bin/slashit`,
  `usr/bin/slashitd` and `usr/bin/slashit-ui`, and no package contains
  `acp_fixture_agent`. List the `.deb` and `.rpm` with `bsdtar -tf`. Extract the
  `.AppImage` with `--appimage-extract` into an empty directory and list
  `squashfs-root/usr/bin`, which should hold `slashitd` and `slashit-ui` but not
  `slashit`. Check the macOS and Windows artifacts for `slashitd` the same way,
  then correct the CHANGELOG if they differ.

Then publish the release on GitHub. The prerelease flag is already set from the
channel the preflight job read, so there is nothing to adjust. Check that the
release page says "Pre-release" for a version declared `prerelease`, and does
not for a `stable` one, then press Publish.

## 6. Validate auto-update

The application does not read `releases/latest`. GitHub's `latest` excludes
prereleases and ranks by creation time, so it can neither offer a prerelease nor
pick the highest version. Instead the updater:

1. lists the repository's releases through the public REST API (at most three
   pages, bounded timeouts, no credentials, a five-minute cache);
2. drops drafts, and for the stable channel drops prereleases;
3. drops any release without a `latest.json` asset, reporting it when it is
   newer than the running build;
4. chooses the highest semantic version newer than the running build;
5. reads that release's own `latest.json` and refuses it unless its `version`
   matches the release tag;
6. hands the manifest to the Tauri updater, which verifies the signature of the
   payload before anything is installed.

A release that is not offered is not an error. A listing that cannot be read is
an error, and it is never reported as "up to date".

A published prerelease is offered to people who chose the Prerelease channel and
to nobody else. A stable release is offered to both channels. Nothing older than
the running build is ever offered.

After publishing a stable release, check the release listing and the manifest it
names:

```bash
curl -sSL https://api.github.com/repos/BarraDev/slashit/releases?per_page=5
curl -sSL https://github.com/BarraDev/slashit/releases/download/v<version>/latest.json
```

The manifest should contain the release's `version`, and a `platforms` entry per
target with a `url` and a `signature`. Then confirm end to end: install the
*previous* release, launch it, and let it offer the update on the channel it
has selected.

The configured `plugins.updater.endpoints` value in `tauri.conf.json` is the
plugin's own default. The application's checks always pass the release manifest
they selected, so that default is not consulted by SlashIt.

## 7. Signing, stated accurately

Two different things get called signing, and only one of them happens here.

**The updater is signed.** `TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` sign
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

**Checking that the secret matches the public key.** The build warns but does not
fail when the signing secret and `plugins.updater.pubkey` disagree, so a release
can be built and then refuse its own updates. `scripts/test-updater-signing.sh`
tests the probe with disposable keys. The manual **Updater signing probe**
workflow (`.github/workflows/updater-signing-probe.yml`) signs a fixed payload
with the production secrets and verifies it against the configured public key.
It runs only from `main`, publishes nothing, and reports PASS or FAIL. Until it
has passed, the production key is **not verified**.

The release workflow installs the Tauri CLI with `npm install --global
@tauri-apps/cli@2.12.4` and passes `tauriScript: tauri` to `tauri-action`, so
the action runs that binary rather than installing `@tauri-apps/cli@v2`, which
floats. The probe installs the same version. Change both places together.

This is a cost decision, not an oversight — both require a paid certificate or
membership. It has to be said plainly in the release notes and the README,
because a security warning that nobody warned users about is indistinguishable
from a compromised download.

## 8. Before the first release

Nothing has been published. The first tag planned is `v0.0.1`, a prerelease
declared in `.github/release-channels.txt`. Testers who choose the Prerelease
channel in Settings receive it and later prereleases; people on the Stable
channel receive nothing until a stable release is published. Until then the
README's Releases link goes to the prerelease page, and a Stable check reports
that no newer Stable release is available. That is expected, and not an error.
