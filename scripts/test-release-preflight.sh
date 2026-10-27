#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
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

# run_case <name> <expected yes|no> <expected channel or -> <tag> <root> [expected prerelease]
run_case() {
  local name=$1 expect=$2 want_channel=$3 tag=$4 tree=$5 want_prerelease=${6:--}
  local output="$work/output-$passed-$failed" log="$work/log-$passed-$failed"
  : >"$output"
  if GITHUB_OUTPUT="$output" "$preflight" "$tag" "$tree" >"$log" 2>&1; then
    if [ "$expect" = yes ] \
      && { [ "$want_channel" = - ] || grep -qx "channel=$want_channel" "$output"; } \
      && { [ "$want_prerelease" = - ] || grep -qx "prerelease=$want_prerelease" "$output"; }; then
      passed=$((passed + 1))
      echo "ok   $name"
    else
      failed=$((failed + 1))
      echo "FAIL $name: expected '$expect' with channel '$want_channel' and prerelease '$want_prerelease', got success; output:"
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
# The repository's own version and channel are read, not assumed, so a legitimate
# version bump keeps these cases true. The refusal uses a different version.
repo_version=$(jq -er '.version' "$root/src-tauri/tauri.conf.json")
repo_channel=$(awk -v v="$repo_version" '$0 !~ /^[[:space:]]*(#|$)/ && $1 == v { print $2 }' "$root/.github/release-channels.txt")
repo_prerelease=false
[ "$repo_channel" = prerelease ] && repo_prerelease=true
other_version=0.0.0
[ "$repo_version" = 0.0.0 ] && other_version=0.0.1
run_case "v$repo_version is releasable on the channel the repository declares" yes "$repo_channel" "v$repo_version" "$root" "$repo_prerelease"
run_case "v$other_version is refused for the repository's version $repo_version" no - "v$other_version" "$root"

echo "== fixture trees"
good=$work/good
make_tree "$good" 0.1.0 $'0.1.0 stable\n' yes yes
run_case "a stable 0.1.0 declared stable is releasable and not a GitHub prerelease" yes stable v0.1.0 "$good" false

first=$work/first
make_tree "$first" 0.0.1 $'0.0.1 prerelease\n' yes yes
run_case "0.0.1 declared prerelease without a hyphen is releasable as a GitHub prerelease" yes prerelease v0.0.1 "$first" true

sequence=$work/sequence
make_tree "$sequence" 0.0.3 $'0.0.1 prerelease\n0.0.2 prerelease\n0.0.3 stable\n' yes yes
run_case "a later plain version declared stable is releasable on the stable channel" yes stable v0.0.3 "$sequence" false

undeclared=$work/undeclared
make_tree "$undeclared" 0.0.1 $'0.1.0 stable\n' yes yes
run_case "a version with no channel line is refused" no - v0.0.1 "$undeclared"

missing=$work/missing
make_tree "$missing" 0.0.1 $'0.0.1 prerelease\n' yes yes
rm "$missing/.github/release-channels.txt"
run_case "a missing channels file is refused" no - v0.0.1 "$missing"

twice=$work/twice
make_tree "$twice" 0.0.1 $'0.0.1 prerelease\n0.0.1 stable\n' yes yes
run_case "a version declared twice with conflicting channels is refused" no - v0.0.1 "$twice"

named=$work/named
make_tree "$named" 0.0.1 $'0.0.1 beta\n' yes yes
run_case "an unknown channel is refused" no - v0.0.1 "$named"

upper=$work/upper
make_tree "$upper" 0.0.1 $'0.0.1 Prerelease\n' yes yes
run_case "a channel name with the wrong case is refused" no - v0.0.1 "$upper"

numeric_prerelease=$work/numeric-prerelease
make_tree "$numeric_prerelease" 0.1.0-1 $'0.1.0-1 prerelease\n' yes yes
run_case "a numeric prerelease suffix is refused even when declared prerelease" no - v0.1.0-1 "$numeric_prerelease"

named_prerelease=$work/named-prerelease
make_tree "$named_prerelease" 0.1.0-rc.1 $'0.1.0-rc.1 prerelease\n' yes yes
run_case "a named prerelease suffix is refused" no - v0.1.0-rc.1 "$named_prerelease"

build_metadata=$work/build-metadata
make_tree "$build_metadata" 0.1.0+1 $'0.1.0+1 stable\n' yes yes
run_case "build metadata is refused" no - v0.1.0+1 "$build_metadata"

four_parts=$work/four-parts
make_tree "$four_parts" 0.0.1.1 $'0.0.1.1 prerelease\n' yes yes
run_case "a fourth version component is refused" no - v0.0.1.1 "$four_parts"

declared_suffix=$work/declared-suffix
make_tree "$declared_suffix" 0.0.2 $'0.0.1 prerelease\n0.0.1-1 prerelease\n0.0.2 stable\n' yes yes
run_case "a declaration line with a suffix is refused even for an older version" no - v0.0.2 "$declared_suffix"

out_of_order=$work/out-of-order
make_tree "$out_of_order" 0.0.1 $'0.0.2 stable\n0.0.1 prerelease\n' yes yes
run_case "declarations out of publication order are refused" no - v0.0.1 "$out_of_order"

superseded=$work/superseded
make_tree "$superseded" 0.0.1 $'0.0.1 prerelease\n0.0.2 prerelease\n' yes yes
run_case "a release older than the latest declared version is refused" no - v0.0.1 "$superseded"

msi_patch=$work/msi-patch
make_tree "$msi_patch" 0.0.65536 $'0.0.65536 stable\n' yes yes
run_case "a patch version the Windows installer cannot hold is refused" no - v0.0.65536 "$msi_patch"

msi_major=$work/msi-major
make_tree "$msi_major" 256.0.0 $'256.0.0 stable\n' yes yes
run_case "a major version above 255 is refused for the Windows installer" no - v256.0.0 "$msi_major"

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

echo "== the workflow consumes the declared channel"
# The GitHub prerelease flag comes from the preflight output, never from a
# second reading of the channel file or the tag.
if grep -q 'prerelease: \${{ needs.preflight.outputs.prerelease == .true. }}' "$root/.github/workflows/release.yml" \
  && grep -q "steps.version.outputs.prerelease" "$root/.github/workflows/release.yml"; then
  passed=$((passed + 1))
  echo "ok   the release job takes its prerelease flag from the preflight output"
else
  failed=$((failed + 1))
  echo "FAIL the release job does not take its prerelease flag from the preflight output"
fi

echo
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
