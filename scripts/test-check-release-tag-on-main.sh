#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Tests scripts/check-release-tag-on-main.sh against real git repositories: a bare
# origin and a working clone at a detached tag, as the release workflow has it.
# Each case names what it proves. A failure prints the checker's own output.
#
# usage: scripts/test-check-release-tag-on-main.sh
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
checker="$root/scripts/check-release-tag-on-main.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/release-tag-on-main-test.XXXXXX")
trap 'rm -rf "$work"' EXIT

passed=0
failed=0

origin="$work/origin.git"
repo="$work/repo"

# commit <message>: commits a new file on the current branch of $repo.
commit() {
  printf '%s\n' "$1" >"$repo/$1.txt"
  git -C "$repo" add "$1.txt"
  git -C "$repo" commit -q -m "$1"
}

git init -q --bare "$origin"
git init -q "$repo"
git -C "$repo" symbolic-ref HEAD refs/heads/main
# Repository-local identity: CI has no global one, and annotated tags need it.
git -C "$repo" config user.name "Release Test"
git -C "$repo" config user.email "release-test@example.invalid"
git -C "$repo" remote add origin "$origin"

commit c1
git -C "$repo" tag -a v0.0.1 -m "v0.0.1"
git -C "$repo" tag v0.0.2
git -C "$repo" push -q origin main --tags

# A commit that only a side branch reaches, tagged with an annotated tag.
git -C "$repo" checkout -q -b side
commit side
git -C "$repo" tag -a v9.9.9 -m "not on main"
git -C "$repo" checkout -q main

# main moves on, so v0.0.1 is an ancestor of origin/main, not its tip.
commit c3
git -C "$repo" push -q origin main
git -C "$repo" tag -a v0.1.0 -m "main tip"
git -C "$repo" push -q origin v0.1.0

# expect <pass|fail|input> <label> <tag> <checkout-ref>
# Runs the checker in a clone detached at <checkout-ref>, as CI checks out a tag.
expect() {
  local want=$1 label=$2 tag=$3 at=$4 output status=0
  local args=()
  [ -n "$tag" ] && args=("$tag")
  git -C "$repo" checkout -q --detach "$at"
  output=$(cd "$repo" && "$checker" ${args[@]+"${args[@]}"} 2>&1) || status=$?
  case "$want" in
    pass)
      if [ "$status" -eq 0 ] && [[ "$output" == ok* ]]; then
        echo "ok   $label"
        passed=$((passed + 1))
        return
      fi
      ;;
    fail)
      if [ "$status" -eq 1 ] && [[ "$output" == FAIL* ]]; then
        echo "ok   $label (refused: ${output#FAIL })"
        passed=$((passed + 1))
        return
      fi
      ;;
    input)
      if [ "$status" -eq 2 ] && [[ "$output" == INPUT\ ERROR* || "$output" == usage* ]]; then
        echo "ok   $label (input error: ${output%%$'\n'*})"
        passed=$((passed + 1))
        return
      fi
      ;;
  esac
  echo "FAIL $label: expected $want, checker exited $status" >&2
  printf '%s\n' "$output" >&2
  failed=$((failed + 1))
}

echo "== tags on main"
expect pass "an annotated tag on a commit reachable from main passes" v0.0.1 v0.0.1
expect pass "a lightweight tag on a commit reachable from main passes" v0.0.2 v0.0.2
expect pass "a tag on the current main tip passes" v0.1.0 v0.1.0

echo "== tags off main"
expect fail "an annotated tag on a commit only a side branch reaches is refused" v9.9.9 v9.9.9

echo "== checkout and tag disagree"
expect fail "a checkout at main is refused for a tag that names an older commit" v0.0.1 main

echo "== bad input"
expect input "a tag that does not exist is an input error" v7.7.7 main
expect input "a missing tag argument is an input error" "" main

echo "== origin unreachable"
git -C "$repo" checkout -q --detach v0.0.1
git -C "$repo" remote set-url origin "$work/missing.git"
status=0
output=$(cd "$repo" && "$checker" v0.0.1 2>&1) || status=$?
if [ "$status" -eq 2 ] && [[ "$output" == *"INPUT ERROR: could not fetch origin/main"* ]]; then
  echo "ok   an unreachable origin fails closed (input error)"
  passed=$((passed + 1))
else
  echo "FAIL an unreachable origin must fail closed: exited $status" >&2
  printf '%s\n' "$output" >&2
  failed=$((failed + 1))
fi
git -C "$repo" remote set-url origin "$origin"

echo
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
