#!/usr/bin/env bash
# Checks a release tag before the release workflow builds anything, and decides
# the release channel. Runs in the workflow's preflight job and locally.
#
# usage: scripts/release-preflight.sh <tag> [repo-root]
#
# On success it prints the channel and, when GITHUB_OUTPUT is set, writes
# `version=` and `channel=` to it. The channel is `prerelease` or `stable`, and
# it comes only from the declaration in .github/release-channels.txt. A tag's
# hyphen does not decide it, and a version with no declaration fails.
set -euo pipefail

fail() {
  echo "::error::$*" >&2
  exit 1
}

tag=${1:-}
[ -n "$tag" ] || fail "usage: release-preflight.sh <tag> [repo-root]"
root=${2:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
cd "$root"

[[ "$tag" == v* ]] || fail "Tag $tag is not a version tag (expected v<version>)."

# 1. The tag names the release. tauri.conf.json defines what gets built.
version="$(jq -er '.version' src-tauri/tauri.conf.json)"
echo "tauri.conf.json version: $version"
echo "pushed tag:              $tag"
if [ "$tag" != "v$version" ]; then
  fail "Tag $tag does not match the app version $version in src-tauri/tauri.conf.json (expected tag v$version). Bump every version listed in docs/releasing.md first."
fi

# 2. Every version manifest names the same version. Each Cargo manifest's
#    first `version = ` line is its package version.
for manifest in Cargo.toml src-tauri/Cargo.toml crates/*/Cargo.toml; do
  crate_version="$(grep -m1 '^version = ' "$manifest" | cut -d'"' -f2)"
  if [ "$crate_version" != "$version" ]; then
    fail "$manifest is at $crate_version but the release version is $version. See docs/releasing.md."
  fi
done
package_version="$(jq -er '.version' package.json)"
if [ "$package_version" != "$version" ]; then
  fail "package.json is at $package_version but the release version is $version. See docs/releasing.md."
fi

# 3. The version packages on every platform. Windows MSI takes
#    major.minor.patch with an optional numeric prerelease (0.1.0-1 becomes
#    0.1.0.1); a named prerelease (0.1.0-rc.1) hard-errors in the bundler.
if [[ ! "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)(-([0-9]+))?$ ]]; then
  fail "Version $version cannot be packaged as an MSI. Use major.minor.patch with an optional numeric prerelease (0.1.0-1), not a named one (0.1.0-rc.1), and do not add build metadata (+1)."
fi
[ "${BASH_REMATCH[1]}" -le 255 ]   || fail "MSI: major version must be <= 255 (got ${BASH_REMATCH[1]})."
[ "${BASH_REMATCH[2]}" -le 255 ]   || fail "MSI: minor version must be <= 255 (got ${BASH_REMATCH[2]})."
[ "${BASH_REMATCH[3]}" -le 65535 ] || fail "MSI: patch version must be <= 65535 (got ${BASH_REMATCH[3]})."
if [ -n "${BASH_REMATCH[5]:-}" ]; then
  [ "${BASH_REMATCH[5]}" -le 65535 ] || fail "MSI: prerelease number must be <= 65535 (got ${BASH_REMATCH[5]})."
fi

# 4. The channel is declared, once, for this version. A version with a
#    prerelease suffix must be a prerelease: a numeric suffix on a stable
#    channel would publish a build the updater treats as an upgrade target.
channels=.github/release-channels.txt
[ -f "$channels" ] || fail "$channels is missing. Every release names its channel there."
declarations="$(awk -v v="$version" '$0 !~ /^[[:space:]]*(#|$)/ && $1 == v' "$channels")"
declared_count="$(printf '%s' "$declarations" | awk 'NF' | wc -l)"
if [ "$declared_count" -eq 0 ]; then
  fail "$channels declares no channel for $version. Add a line '$version prerelease' or '$version stable'."
fi
if [ "$declared_count" -gt 1 ]; then
  fail "$channels declares $version $declared_count times. Keep one line per version."
fi
channel="$(printf '%s\n' "$declarations" | awk '{ print $2 }')"
case "$channel" in
  prerelease | stable) ;;
  *) fail "$channels declares $version as '$channel'. The channel must be 'prerelease' or 'stable'." ;;
esac
if [[ "$version" == *-* ]] && [ "$channel" != "prerelease" ]; then
  fail "$version has a prerelease suffix, so it must be declared a prerelease in $channels, not '$channel'."
fi

# 5. The release text leads with the curated CHANGELOG.md section for this
#    version, and its reference link names the tag or the comparison.
if ! grep -Fq -- "## [$version]" CHANGELOG.md; then
  fail "CHANGELOG.md has no '## [$version]' section. Move the [Unreleased] entries under the new version, then retag. See docs/releasing.md."
fi
if ! grep -Fq -- "[$version]: https://" CHANGELOG.md; then
  fail "CHANGELOG.md has no '[$version]: https://' link definition at its foot. Add it, pointing at the tag or at the comparison with the previous tag. See docs/releasing.md."
fi

echo "Version $version is releasable on the $channel channel."
if [ -n "${GITHUB_OUTPUT:-}" ]; then
  {
    echo "version=$version"
    echo "channel=$channel"
  } >> "$GITHUB_OUTPUT"
fi
