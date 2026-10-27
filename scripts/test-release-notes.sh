#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Tests scripts/release-notes.sh against small fixture changelogs and against the
# repository's own CHANGELOG.md. The point under test is that a version's
# description never carries another version's section.
#
# usage: scripts/test-release-notes.sh
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
notes="$root/scripts/release-notes.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/release-notes-test.XXXXXX")
trap 'rm -rf "$work"' EXIT

passed=0
failed=0

# A fixture tree: a changelog with three versions and an Unreleased section,
# and a declaration file naming 0.0.1 a prerelease and 0.0.2 stable.
make_fixture() {
  local dir=$1
  mkdir -p "$dir/.github"
  cat >"$dir/CHANGELOG.md" <<'EOF'
# Changelog

## [Unreleased]

- unreleased-body

## [0.0.2]

- two-body

### Added

- two-added

## [0.0.1]

- one-body

[0.0.2]: https://example.invalid/compare/v0.0.1...v0.0.2
[0.0.1]: https://example.invalid/releases/tag/v0.0.1
[Unreleased]: https://example.invalid/compare/v0.0.2...HEAD
EOF
  printf '0.0.1 prerelease\n0.0.2 stable\n' >"$dir/.github/release-channels.txt"
}

# run_case <name> <expected yes|no> <version> <tree> [text that must appear] [text that must not appear]
run_case() {
  local name=$1 expect=$2 version=$3 tree=$4 must=${5:-} must_not=${6:-}
  local output="$work/out-$passed-$failed" log="$work/log-$passed-$failed"
  local status=0
  "$notes" "$version" "$tree" >"$output" 2>"$log" || status=$?
  local ok=yes
  if [ "$expect" = yes ]; then
    [ "$status" -eq 0 ] || ok=no
  else
    [ "$status" -ne 0 ] || ok=no
  fi
  if [ "$ok" = yes ] && [ -n "$must" ] && ! grep -F -q -- "$must" "$output"; then
    ok=no
  fi
  if [ "$ok" = yes ] && [ -n "$must_not" ] && grep -F -q -- "$must_not" "$output"; then
    ok=no
  fi
  if [ "$ok" = yes ]; then
    passed=$((passed + 1))
    echo "ok   $name"
  else
    failed=$((failed + 1))
    echo "FAIL $name (exit $status)"
    sed 's/^/     /' "$log" "$output" | head -40
  fi
}

fixture="$work/fixture"
make_fixture "$fixture"

echo "== each version gets its own section"
run_case "0.0.1 carries its own section" yes 0.0.1 "$fixture" "- one-body" "two-body"
run_case "0.0.1 does not carry Unreleased" yes 0.0.1 "$fixture" "- one-body" "unreleased-body"
run_case "0.0.1 does not carry the link definitions" yes 0.0.1 "$fixture" "- one-body" "example.invalid"
run_case "0.0.2 carries its subsections" yes 0.0.2 "$fixture" "### Added" "one-body"
run_case "0.0.2 does not carry 0.0.1" yes 0.0.2 "$fixture" "- two-body" "one-body"

echo "== the channel comes from the declaration"
run_case "a prerelease says so" yes 0.0.1 "$fixture" "This is a **prerelease**" "**stable**"
run_case "a stable release says so" yes 0.0.2 "$fixture" "This is a **stable** release" "**prerelease**"

echo "== the notices are kept"
run_case "the unsigned installers are described" yes 0.0.1 "$fixture" "not OS code-signed"
run_case "the updater signature is described separately" yes 0.0.1 "$fixture" "updater key"

echo "== refusals"
run_case "a version with no section is refused" no 0.0.3 "$fixture"
run_case "a tag, not a version, is refused" no v0.0.1 "$fixture"
run_case "a prerelease suffix is refused" no 0.0.1-rc.1 "$fixture"
run_case "a version with no declared channel is refused" no 0.0.4 "$fixture"

empty="$work/empty"
make_fixture "$empty"
printf '%s\n' '## [0.0.1]' '' '## [0.0.2]' '- two-body' >"$empty/CHANGELOG.md"
run_case "an empty section is refused" no 0.0.1 "$empty"

duplicate="$work/duplicate"
make_fixture "$duplicate"
printf '%s\n' '## [0.0.1]' '- first' '## [0.0.1]' '- second' >"$duplicate/CHANGELOG.md"
run_case "a duplicated heading is refused" no 0.0.1 "$duplicate"

prefix="$work/prefix"
make_fixture "$prefix"
printf '%s\n' '## [0.0.10]' '- ten-body' >"$prefix/CHANGELOG.md"
printf '0.0.1 prerelease\n0.0.10 prerelease\n' >"$prefix/.github/release-channels.txt"
run_case "0.0.1 does not match 0.0.10" no 0.0.1 "$prefix"

echo "== the repository's own changelog"
run_case "the 0.0.1 description builds from the repository" yes 0.0.1 "$root" "Known gaps in this prerelease" "## [Unreleased]"

echo
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
