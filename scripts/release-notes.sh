#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Writes the GitHub release description for one version: the curated section of
# CHANGELOG.md for that version, then the installation and signing notices that
# every release keeps. The release workflow runs it for the version the preflight
# job validated, so a description is built from one section only. A missing,
# empty or duplicated section is an error, never a fallback to another version.
#
# usage: scripts/release-notes.sh <version> [repo-root]
set -euo pipefail

fail() {
  echo "::error::$*" >&2
  exit 1
}

version=${1:-}
[ -n "$version" ] || fail "usage: release-notes.sh <version> [repo-root]"
root=${2:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}

# The same plain-version grammar as release-preflight.sh.
plain_version_re='^(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})$'
[[ "$version" =~ $plain_version_re ]] || fail "$version is not a plain MAJOR.MINOR.PATCH version."

changelog="$root/CHANGELOG.md"
channels="$root/.github/release-channels.txt"
[ -f "$changelog" ] || fail "$changelog is missing."
[ -f "$channels" ] || fail "$channels is missing."

# The channel comes from the same declaration the preflight job read.
channel=$(awk -v v="$version" '$1 == v { print $2 }' "$channels")
case "$channel" in
  prerelease | stable) ;;
  *) fail "$channels does not declare $version as prerelease or stable." ;;
esac

# The section is the lines after its exact `## [<version>]` heading, up to the
# next `## ` heading or the reference definitions at the foot of the file.
headings=$(grep -c -x -F -- "## [$version]" "$changelog" || true)
[ "$headings" -eq 1 ] || fail "CHANGELOG.md has $headings '## [$version]' headings; it needs exactly one."

section=$(awk -v heading="## [$version]" '
  $0 == heading { inside = 1; next }
  inside && (/^## / || /^\[[^]]+\]: /) { exit }
  inside { print }
' "$changelog")

# Drop leading and trailing blank lines, and refuse an empty section.
body=$(printf '%s\n' "$section" | awk '
  NF { started = 1 }
  started { lines[++n] = $0 }
  END {
    last = n
    while (last > 0 && lines[last] ~ /^[[:space:]]*$/) last--
    for (i = 1; i <= last; i++) print lines[i]
  }
')
[ -n "$(printf '%s' "$body" | tr -d '[:space:]')" ] || fail "The '## [$version]' section of CHANGELOG.md is empty."

printf '%s\n\n' "$body"

printf '%s\n' "---" ""
printf '%s\n' "### Installation and signing" ""
printf '%s\n' \
  "- **The installers are not OS code-signed.** The \`.msi\`, the NSIS \`.exe\` and the \`.dmg\` carry no Windows Authenticode certificate and no Apple Developer ID signature or notarization. Windows SmartScreen and macOS Gatekeeper warn on first launch." \
  "- **The update packages are signed with SlashIt's updater key.** The \`latest.json\` manifest and each update artifact carry a signature that the running app verifies before it installs an update. That signature is not OS code signing, and it does not remove the warnings above." \
  ""
if [ "$channel" = prerelease ]; then
  printf '%s\n' \
    "This is a **prerelease**. Installs on the Prerelease update channel are offered it once it is published. Installs on the Stable channel are not." ""
else
  printf '%s\n' \
    "This is a **stable** release. Installs on the Stable and Prerelease update channels are offered it once it is published." ""
fi
