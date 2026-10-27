#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Tests scripts/check-workflow-shells.sh against small fixture workflows, and
# against the repository's own workflows. Each case names what it proves, and a
# failure prints the checker's own message.
#
# usage: scripts/test-check-workflow-shells.sh
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
checker="$root/scripts/check-workflow-shells.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/workflow-shells-test.XXXXXX")
trap 'rm -rf "$work"' EXIT

passed=0
failed=0

# expect <pass|fail> <label> <workflow-file>
# Runs the checker on one fixture and records whether its exit status matches.
expect() {
  local want=$1 label=$2 file=$3 output status=0
  output=$("$checker" "$file" 2>&1) || status=$?
  # A refusal must come from the checker's own FAIL line. An exit status of 1
  # alone could also be a crash, such as a missing YAML library.
  if [ "$want" = pass ] && [ "$status" -eq 0 ]; then
    echo "ok   $label"
    passed=$((passed + 1))
  elif [ "$want" = fail ] && [ "$status" -eq 1 ] && [[ "$output" == FAIL* ]]; then
    echo "ok   $label (refused: $(printf '%s' "$output" | head -1 | sed 's/^FAIL //'))"
    passed=$((passed + 1))
  else
    echo "FAIL $label: expected $want, checker exited $status" >&2
    printf '%s\n' "$output" >&2
    failed=$((failed + 1))
  fi
}

# write <name> <body>: a workflow fixture in $work.
write() {
  printf '%s\n' "$2" >"$work/$1.yml"
  printf '%s' "$work/$1.yml"
}

echo "== a Windows job"
f=$(write windows-posix-no-shell 'name: t
on: workflow_dispatch
jobs:
  release:
    runs-on: windows-latest
    steps:
      - name: Write the release description
        run: |
          set -euo pipefail
          scripts/release-notes.sh "$VERSION"
')
expect fail "POSIX syntax without shell: on a Windows runner is refused" "$f"

f=$(write windows-posix-bash 'name: t
on: workflow_dispatch
jobs:
  release:
    runs-on: windows-latest
    steps:
      - name: Write the release description
        shell: bash
        run: |
          set -euo pipefail
          scripts/release-notes.sh "$VERSION"
')
expect pass "the same step with shell: bash is accepted" "$f"

f=$(write windows-plain-command 'name: t
on: workflow_dispatch
jobs:
  build:
    runs-on: windows-latest
    steps:
      - name: Install the Tauri CLI
        run: npm install --global @tauri-apps/cli@2.12.4
')
expect pass "a single command with no POSIX syntax needs no shell:" "$f"

echo "== matrix jobs"
f=$(write matrix-windows-posix 'name: t
on: workflow_dispatch
jobs:
  release:
    runs-on: ${{ matrix.platform }}
    strategy:
      matrix:
        include:
          - platform: ubuntu-22.04
          - platform: windows-latest
    steps:
      - name: Describe
        run: test "$RUNNER_TEMP" && echo ok
')
expect fail "a matrix that includes windows is checked" "$f"

f=$(write matrix-no-windows 'name: t
on: workflow_dispatch
jobs:
  release:
    runs-on: ${{ matrix.platform }}
    strategy:
      matrix:
        include:
          - platform: ubuntu-22.04
          - platform: macos-latest
    steps:
      - name: Describe
        run: test "$RUNNER_TEMP" && echo ok
')
expect pass "a matrix with no windows entry is not checked" "$f"

f=$(write ubuntu-posix 'name: t
on: workflow_dispatch
jobs:
  lint:
    runs-on: ubuntu-22.04
    steps:
      - name: Describe
        run: |
          set -euo pipefail
          echo "$HOME"
')
expect pass "a job that never runs on Windows is not checked" "$f"

echo "== explicit defaults"
f=$(write workflow-default 'name: t
on: workflow_dispatch
defaults:
  run:
    shell: bash
jobs:
  release:
    runs-on: windows-latest
    steps:
      - name: Describe
        run: set -euo pipefail
')
expect pass "a workflow-level default shell covers the step" "$f"

f=$(write job-default 'name: t
on: workflow_dispatch
jobs:
  release:
    runs-on: windows-latest
    defaults:
      run:
        shell: bash
    steps:
      - name: Describe
        run: set -euo pipefail
')
expect pass "a job-level default shell covers the step" "$f"

echo "== the repository's own workflows"
status=0
"$checker" >/dev/null 2>&1 || status=$?
if [ "$status" -eq 0 ]; then
  echo "ok   the checker passes on every workflow in this repository"
  passed=$((passed + 1))
else
  echo "FAIL the checker refuses a workflow in this repository" >&2
  "$checker" >&2 || true
  failed=$((failed + 1))
fi

echo
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
