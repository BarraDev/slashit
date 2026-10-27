#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Checks a release tag before the release workflow builds anything, and decides
# the release channel. Runs in the workflow's preflight job and locally.
#
# usage: scripts/release-preflight.sh <tag> [repo-root]
#
# On success it prints the channel and, when GITHUB_OUTPUT is set, writes
# `version=`, `channel=` and `prerelease=` (true or false) to it. The channel
# comes only from the declaration in .github/release-channels.txt. A version
# with no declaration fails, and so does a version that is not plain
# MAJOR.MINOR.PATCH. `prerelease` is the value the release's GitHub
# `prerelease` flag must take.
set -euo pipefail

fail() {
  echo "::error::$*" >&2
  exit 1
}

tag=${1:-}
[ -n "$tag" ] || fail "usage: release-preflight.sh <tag> [repo-root]"
root=${2:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
cd "$root"

# The published version grammar: plain MAJOR.MINOR.PATCH, no leading zeros, no
# prerelease or build suffix, no fourth component. Each component is at most
# nine digits, so the arithmetic below cannot overflow.
plain_version_re='^(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})$'

# parse_plain <version>: sets pv_major, pv_minor and pv_patch; fails if the
# version is not plain.
parse_plain() {
  [[ "$1" =~ $plain_version_re ]] || return 1
  pv_major=${BASH_REMATCH[1]}
  pv_minor=${BASH_REMATCH[2]}
  pv_patch=${BASH_REMATCH[3]}
}

# version_is_greater <a> <b>: succeeds when plain version a is later than b.
version_is_greater() {
  parse_plain "$1" || return 1
  local a_major=$pv_major a_minor=$pv_minor a_patch=$pv_patch
  parse_plain "$2" || return 1
  if ((a_major != pv_major)); then ((a_major > pv_major)); return; fi
  if ((a_minor != pv_minor)); then ((a_minor > pv_minor)); return; fi
  ((a_patch > pv_patch))
}

# 1. The tag names the release. tauri.conf.json defines what gets built.
version="$(jq -er '.version' src-tauri/tauri.conf.json)"
echo "tauri.conf.json version: $version"
echo "pushed tag:              $tag"
if [ "$tag" != "v$version" ]; then
  fail "Tag $tag does not match the app version $version in src-tauri/tauri.conf.json (expected tag v$version). Bump every version listed in docs/releasing.md first."
fi

# 2. The version is plain MAJOR.MINOR.PATCH and packages on every platform.
#    Suffixes are refused on every platform, so the installer version is always
#    the application version with a zero build component.
if ! parse_plain "$version"; then
  fail "Version $version is not plain MAJOR.MINOR.PATCH. Published versions take no prerelease suffix (-1, -rc.1), no build metadata (+1), and no fourth component. The channel comes from .github/release-channels.txt."
fi
# The Windows installer takes major and minor up to 255 and patch up to 65535.
[ "$pv_major" -le 255 ]   || fail "MSI: major version must be <= 255 (got $pv_major)."
[ "$pv_minor" -le 255 ]   || fail "MSI: minor version must be <= 255 (got $pv_minor)."
[ "$pv_patch" -le 65535 ] || fail "MSI: patch version must be <= 65535 (got $pv_patch)."

# 3. Every version manifest names the same version. Each Cargo manifest's
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

# 4. The channel is declared once per version, in publication order. Each line
#    is `<version> <channel>`. Every line must be a plain version, a valid
#    channel, and a later version than the line above it, so a version cannot
#    be declared twice, cannot carry two channels, and published versions only
#    increase. The release must be the last declared version: a version
#    published after a later declaration would move the channel backwards.
channels=.github/release-channels.txt
[ -f "$channels" ] || fail "$channels is missing. Every release names its channel there."
channel=""
previous=""
seen=" "
while IFS= read -r line || [ -n "$line" ]; do
  [[ "$line" =~ ^[[:space:]]*(#|$) ]] && continue
  read -r -a fields <<<"$line"
  [ "${#fields[@]}" -eq 2 ] || fail "$channels line '$line' must be '<version> <channel>'."
  declared_version=${fields[0]}
  declared_channel=${fields[1]}
  parse_plain "$declared_version" || fail "$channels declares '$declared_version', which is not a plain MAJOR.MINOR.PATCH version."
  case "$declared_channel" in
    prerelease | stable) ;;
    *) fail "$channels declares $declared_version as '$declared_channel'. The channel must be 'prerelease' or 'stable'." ;;
  esac
  case "$seen" in
    *" $declared_version "*) fail "$channels declares $declared_version more than once. Keep one line per version." ;;
  esac
  seen="$seen$declared_version "
  if [ -n "$previous" ] && ! version_is_greater "$declared_version" "$previous"; then
    fail "$channels lists $declared_version after $previous. Each line must be a later version than the one above it, so published versions only increase."
  fi
  previous=$declared_version
  if [ "$declared_version" = "$version" ]; then
    channel=$declared_channel
  fi
done <"$channels"
if [ -z "$channel" ]; then
  fail "$channels declares no channel for $version. Add a line '$version prerelease' or '$version stable'."
fi
if version_is_greater "$previous" "$version"; then
  fail "$channels lists $previous, which is later than the release $version. A release must be the latest declared version."
fi

prerelease=false
[ "$channel" = prerelease ] && prerelease=true

# 5. The release text leads with the curated CHANGELOG.md section for this
#    version, and its reference link names the tag or the comparison.
if ! grep -Fq -- "## [$version]" CHANGELOG.md; then
  fail "CHANGELOG.md has no '## [$version]' section. Move the [Unreleased] entries under the new version, then retag. See docs/releasing.md."
fi
if ! grep -Fq -- "[$version]: https://" CHANGELOG.md; then
  fail "CHANGELOG.md has no '[$version]: https://' link definition at its foot. Add it, pointing at the tag or at the comparison with the previous tag. See docs/releasing.md."
fi

echo "Version $version is releasable on the $channel channel (GitHub prerelease: $prerelease)."
if [ -n "${GITHUB_OUTPUT:-}" ]; then
  {
    echo "version=$version"
    echo "channel=$channel"
    echo "prerelease=$prerelease"
  } >> "$GITHUB_OUTPUT"
fi
