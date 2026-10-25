#!/usr/bin/env bash
# Tests scripts/release-preflight.sh against the repository as it is and against
# small fixture trees. Each case names what it proves, and a failure prints the
# script's own message.
#
# usage: scripts/test-release-preflight.sh
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
preflight="$root/scripts/release-preflight.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/release-preflight-test.XXXXXX")
trap 'rm -rf "$work"' EXIT

passed=0
failed=0

# make_tree <dir> <version> <channels-file-body> <changelog-section yes|no> <changelog-link yes|no>
make_tree() {
  local dir=$1 version=$2 channels=$3 section=$4 link=$5
  mkdir -p "$dir/src-tauri" "$dir/crates/slashit-a" "$dir/crates/slashit-b" "$dir/.github"
  printf '{\n  "version": "%s"\n}\n' "$version" >"$dir/src-tauri/tauri.conf.json"
  printf '{\n  "name": "fixture",\n  "version": "%s"\n}\n' "$version" >"$dir/package.json"
  for manifest in Cargo.toml src-tauri/Cargo.toml crates/slashit-a/Cargo.toml crates/slashit-b/Cargo.toml; do
    printf '[package]\nname = "fixture"\nversion = "%s"\n' "$version" >"$dir/$manifest"
  done
  printf '%s' "$channels" >"$dir/.github/release-channels.txt"
  {
    echo "# Changelog"
    if [ "$section" = yes ]; then echo "## [$version]"; fi
    if [ "$link" = yes ]; then echo "[$version]: https://example.invalid/releases/tag/v$version"; fi
  } >"$dir/CHANGELOG.md"
}

# run_case <name> <expected yes|no> <expected channel or -> <tag> <root>
run_case() {
  local name=$1 expect=$2 want_channel=$3 tag=$4 tree=$5
  local output="$work/output-$passed-$failed" log="$work/log-$passed-$failed"
  : >"$output"
  if GITHUB_OUTPUT="$output" "$preflight" "$tag" "$tree" >"$log" 2>&1; then
    if [ "$expect" = yes ] && { [ "$want_channel" = - ] || grep -qx "channel=$want_channel" "$output"; }; then
      passed=$((passed + 1))
      echo "ok   $name"
    else
      failed=$((failed + 1))
      echo "FAIL $name: expected '$expect' with channel '$want_channel', got success; output:"
      sed 's/^/     /' "$log"
    fi
  else
    if [ "$expect" = no ]; then
      passed=$((passed + 1))
      echo "ok   $name (refused: $(grep -m1 '::error::' "$log" | sed 's/^::error:://'))"
    else
      failed=$((failed + 1))
      echo "FAIL $name: expected success, got refusal:"
      sed 's/^/     /' "$log"
    fi
  fi
}

echo "== the repository as it is"
run_case "v0.0.1 is a prerelease channel" yes prerelease v0.0.1 "$root"
run_case "v0.1.0 is refused while the repository is at 0.0.1" no - v0.1.0 "$root"

echo "== fixture trees"
good=$work/good
make_tree "$good" 0.1.0 $'0.1.0 stable\n' yes yes
run_case "a stable 0.1.0 declared stable is releasable" yes stable v0.1.0 "$good"

pre=$work/pre
make_tree "$pre" 0.1.0-1 $'0.1.0-1 prerelease\n' yes yes
run_case "a numeric prerelease declared prerelease is releasable" yes prerelease v0.1.0-1 "$pre"

undeclared=$work/undeclared
make_tree "$undeclared" 0.0.1 $'0.1.0 stable\n' yes yes
run_case "a version with no channel line is refused" no - v0.0.1 "$undeclared"

missing=$work/missing
make_tree "$missing" 0.0.1 $'0.0.1 prerelease\n' yes yes
rm "$missing/.github/release-channels.txt"
run_case "a missing channels file is refused" no - v0.0.1 "$missing"

twice=$work/twice
make_tree "$twice" 0.0.1 $'0.0.1 prerelease\n0.0.1 stable\n' yes yes
run_case "a version declared twice is refused" no - v0.0.1 "$twice"

named=$work/named
make_tree "$named" 0.0.1 $'0.0.1 beta\n' yes yes
run_case "an unknown channel is refused" no - v0.0.1 "$named"

stable_suffix=$work/stable-suffix
make_tree "$stable_suffix" 0.1.0-1 $'0.1.0-1 stable\n' yes yes
run_case "a numeric prerelease declared stable is refused" no - v0.1.0-1 "$stable_suffix"

mismatch=$work/mismatch
make_tree "$mismatch" 0.1.0 $'0.1.0 stable\n' yes yes
printf '[package]\nname = "fixture"\nversion = "0.0.1"\n' >"$mismatch/crates/slashit-b/Cargo.toml"
run_case "a crate manifest at another version is refused" no - v0.1.0 "$mismatch"

package_mismatch=$work/package-mismatch
make_tree "$package_mismatch" 0.1.0 $'0.1.0 stable\n' yes yes
printf '{\n  "name": "fixture",\n  "version": "0.0.1"\n}\n' >"$package_mismatch/package.json"
run_case "a package.json at another version is refused" no - v0.1.0 "$package_mismatch"

no_section=$work/no-section
make_tree "$no_section" 0.1.0 $'0.1.0 stable\n' no yes
run_case "a version with no CHANGELOG section is refused" no - v0.1.0 "$no_section"

no_link=$work/no-link
make_tree "$no_link" 0.1.0 $'0.1.0 stable\n' yes no
run_case "a version with no CHANGELOG link is refused" no - v0.1.0 "$no_link"

wrong_tag=$work/wrong-tag
make_tree "$wrong_tag" 0.1.0 $'0.1.0 stable\n' yes yes
run_case "a tag that does not match the app version is refused" no - v0.0.1 "$wrong_tag"

echo
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
