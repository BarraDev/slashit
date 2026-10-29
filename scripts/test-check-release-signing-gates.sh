#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Tests scripts/check-release-signing-gates.sh against fixture workflows: one valid
# base changed by a single edit for each rule, and the bypass forms found in review.
# Each case names the rule it proves. A refusal must come from the checker's own
# FAIL line, so a crash cannot pass as a refusal.
#
# usage: scripts/test-check-release-signing-gates.sh
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
checker="$root/scripts/check-release-signing-gates.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/release-signing-gates-test.XXXXXX")
trap 'rm -rf "$work"' EXIT

passed=0
failed=0

# expect <pass|fail> <label> <workflow-file>
expect() {
  local want=$1 label=$2 file=$3 output status=0
  output=$("$checker" "$file" 2>&1) || status=$?
  if [ "$want" = pass ] && [ "$status" -eq 0 ] && [[ "$output" == ok* ]]; then
    echo "ok   $label"
    passed=$((passed + 1))
  elif [ "$want" = fail ] && [ "$status" -eq 1 ] && [[ "$output" == *"FAIL "* ]]; then
    echo "ok   $label (refused: $(printf '%s\n' "$output" | grep -m1 '^FAIL ' | sed 's/^FAIL //'))"
    passed=$((passed + 1))
  else
    echo "FAIL $label: expected $want, checker exited $status" >&2
    printf '%s\n' "$output" >&2
    failed=$((failed + 1))
  fi
}

# The valid base: a tag-triggered release with a gated signing job, a non-signing
# job, and a preflight that checks the tag against main with full history.
base="$work/base.yml"
cat >"$base" <<'EOF'
name: Release
on:
  push:
    tags:
      - 'v*'
jobs:
  preflight:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@0000000000000000000000000000000000000000 # v7.0.1
        with:
          fetch-depth: 0
      - name: Check the tag is on main
        env:
          TAG: ${{ github.ref_name }}
        run: scripts/check-release-tag-on-main.sh "$TAG"
  release:
    needs: preflight
    environment: release-signing
    runs-on: ubuntu-22.04
    steps:
      - uses: tauri-apps/tauri-action@0000000000000000000000000000000000000000 # v1.0.0
        env:
          TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
          TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}
  notes:
    needs: preflight
    runs-on: ubuntu-22.04
    steps:
      - run: echo "no secrets here"
EOF

echo "== the valid base"
expect pass "a tag-triggered release with a gated signing job and an ungated notes job passes" "$base"

echo "== the signing job's environment"
f="$work/no-env.yml"; sed '/^    environment: release-signing$/d' "$base" >"$f"
expect fail "a signing job without the protected environment is refused" "$f"

f="$work/typo-env.yml"; sed 's/environment: release-signing/environment: release-signin/' "$base" >"$f"
expect fail "a misspelled environment is refused, even on the signing job" "$f"

f="$work/extra-env.yml"; sed 's/^  notes:$/  notes:\n    environment: release-signing/' "$base" >"$f"
expect fail "a non-signing job declaring the protected environment is refused" "$f"

f="$work/other-env.yml"; sed 's/^  notes:$/  notes:\n    environment: staging/' "$base" >"$f"
expect fail "a non-signing job declaring some other environment is refused" "$f"

echo "== the tag path: preflight ordering and the ancestry check"
f="$work/no-needs.yml"; awk '/^    needs: preflight$/ && !dropped { dropped = 1; next } { print }' "$base" >"$f"
expect fail "a signing job that does not need preflight is refused" "$f"

f="$work/no-ancestry.yml"; sed 's|scripts/check-release-tag-on-main.sh "\$TAG"|echo "tag checked"|' "$base" >"$f"
expect fail "a preflight that does not run the ancestry check is refused" "$f"

f="$work/shallow.yml"; sed '/fetch-depth: 0/d' "$base" >"$f"
expect fail "a preflight without a full-history checkout is refused" "$f"

echo "== bypass forms found in review"
# Each fixture is a complete workflow in which one ungated job reads the key in a
# form the review found: the rule must refuse it, whatever the form.
ungated_job='  sign:
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
        run: echo x'

f="$work/quoted-job-id.yml"; cat >"$f" <<EOF
name: Release
on:
  push:
    tags:
      - 'v*'
jobs:
  preflight:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@0000000000000000000000000000000000000000
        with:
          fetch-depth: 0
      - run: scripts/check-release-tag-on-main.sh "\$TAG"
  "sign":
    needs: preflight
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: \${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
        run: echo x
EOF
expect fail "a signing job under a quoted job id is refused, not attributed to the previous job" "$f"

f="$work/jobs-comment.yml"; cat >"$f" <<EOF
name: Release
on:
  push:
    tags:
      - 'v*'
jobs: # the jobs follow
  preflight:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@0000000000000000000000000000000000000000
        with:
          fetch-depth: 0
      - run: scripts/check-release-tag-on-main.sh "\$TAG"
$ungated_job
    needs: preflight
EOF
expect fail "a comment after 'jobs:' does not hide an ungated signing job" "$f"

f="$work/to-json.yml"; cat >"$f" <<EOF
name: Release
on: workflow_dispatch
jobs:
  dump:
    runs-on: ubuntu-22.04
    steps:
      - env:
          ALL: \${{ toJSON(secrets) }}
        run: echo x
EOF
expect fail "toJSON(secrets) in an ungated job is refused" "$f"

f="$work/bracket.yml"; cat >"$f" <<EOF
name: Release
on: workflow_dispatch
jobs:
  read:
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: \${{ secrets['TAURI_SIGNING_PRIVATE_KEY'] }}
        run: echo x
EOF
expect fail "secrets['NAME'] in an ungated job is refused" "$f"

f="$work/commented-expression.yml"; cat >"$f" <<'EOF'
name: Release
on: workflow_dispatch
jobs:
  read:
    runs-on: ubuntu-22.04
    steps:
      - run: |
          # ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
          echo x
EOF
expect fail "an expression inside a shell comment in a run block is still read and refused" "$f"

f="$work/flow-trigger.yml"; cat >"$f" <<'EOF'
name: Release
on: {push: {tags: ['v*']}, pull_request: {}}
jobs:
  release:
    needs: preflight
    environment: release-signing
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
        run: echo x
EOF
expect fail "a signing workflow that also runs on pull_request is refused" "$f"

f="$work/push-no-tags.yml"; cat >"$f" <<'EOF'
name: Release
on: push
jobs:
  release:
    environment: release-signing
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
        run: echo x
EOF
expect fail "a signing workflow on a branch push with no preflight is refused" "$f"

f="$work/fake-preflight.yml"; sed 's/^      - name: Check the tag is on main$/      - name: Check the tag is on main\n        if: false/' "$base" >"$f"
expect fail "a tag check that is skipped by 'if: false' is refused" "$f"

f="$work/decoy-comment.yml"; cat >"$f" <<'EOF'
name: Release
on:
  push:
    tags:
      - 'v*'
jobs:
  preflight:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@0000000000000000000000000000000000000000
        with:
          fetch-depth: 0
      - run: |
          # scripts/check-release-tag-on-main.sh "$TAG"
          echo ok
  release:
    needs: preflight
    environment: release-signing
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
        run: echo x
EOF
expect fail "a tag check that appears only in a shell comment is refused" "$f"

echo "== a dispatch-only workflow (the probe and the recovery)"
probe="$work/probe.yml"
cat >"$probe" <<'EOF'
name: Probe
on:
  workflow_dispatch:
jobs:
  probe:
    runs-on: ubuntu-22.04
    environment: release-signing
    steps:
      - name: Sign
        env:
          TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
        run: scripts/updater-signing-probe.sh "$RUNNER_TEMP/key" "$RUNNER_TEMP/out"
EOF
expect pass "a dispatch-only probe that declares the environment passes without a tag preflight" "$probe"

f="$work/probe-no-env.yml"; sed '/^    environment: release-signing$/d' "$probe" >"$f"
expect fail "a dispatch-only probe without the environment is refused" "$f"

echo "== the repository's own workflows"
status=0
output=$("$checker" 2>&1) || status=$?
if [ "$status" -eq 0 ]; then
  echo "ok   the repository's workflows gate the signing key"
  passed=$((passed + 1))
else
  echo "FAIL the repository's workflows do not gate the signing key:" >&2
  printf '%s\n' "$output" >&2
  failed=$((failed + 1))
fi

echo
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
