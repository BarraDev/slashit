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

# expect <pass|fail> <label> <workflow-file> [diagnostic the refusal must contain]
expect() {
  local want=$1 label=$2 file=$3 fragment=${4:-} output status=0
  output=$("$checker" "$file" 2>&1) || status=$?
  if [ "$want" = pass ] && [ "$status" -eq 0 ] && [[ "$output" == ok* ]]; then
    echo "ok   $label"
    passed=$((passed + 1))
  elif [ "$want" = fail ] && [ "$status" -eq 1 ] && [[ "$output" == *"FAIL "* ]] &&
    { [ -z "$fragment" ] || [[ "$output" == *"$fragment"* ]]; }; then
    echo "ok   $label (refused: $(printf '%s\n' "$output" | grep -m1 '^FAIL ' | sed 's/^FAIL //'))"
    passed=$((passed + 1))
  else
    echo "FAIL $label: expected $want, checker exited $status" >&2
    [ -z "$fragment" ] || echo "expected diagnostic: $fragment" >&2
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
permissions: {}
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
expect fail "a signing workflow on a branch push with no preflight is refused" "$f" "a push-triggered workflow reads the signing key but has no preflight job"

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

echo "== a dispatch-only workflow (the updater signing probe)"
probe="$work/probe.yml"
cat >"$probe" <<'EOF'
name: Probe
on:
  workflow_dispatch:
permissions: {}
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

echo "== bypasses found after the PyYAML rewrite"
# A condition on the signing job can run it after a failed preflight.
f="$work/always-signing.yml"; sed 's/^    environment: release-signing$/    if: ${{ always() }}\n    environment: release-signing/' "$base" >"$f"
expect fail "a signing job with 'if: always()' is refused, since it may run after a failed check" "$f"

f="$work/failure-signing.yml"; sed 's/^    environment: release-signing$/    if: failure()\n    environment: release-signing/' "$base" >"$f"
expect fail "a signing job with 'if: failure()' is refused, since it runs only after a failure" "$f"

# continue-on-error on the job lets a failed preflight count as success for `needs`.
f="$work/preflight-continue.yml"; sed 's/^  preflight:$/  preflight:\n    continue-on-error: true/' "$base" >"$f"
expect fail "a preflight with 'continue-on-error' is refused, since its failure would not stop the signing job" "$f"

# A command before the check in the same run block could exit first.
f="$work/exit-before-check.yml"; sed 's@^        run: scripts/check-release-tag-on-main.sh "\$TAG"$@        run: |\n          exit 0\n          scripts/check-release-tag-on-main.sh "$TAG"@' "$base" >"$f"
expect fail "an 'exit 0' ahead of the tag check in the same run block is refused" "$f"

f="$work/check-with-comment.yml"; sed 's@^        run: scripts/check-release-tag-on-main.sh "\$TAG"$@        run: |\n          # Refuse a tag that is not on main.\n          scripts/check-release-tag-on-main.sh "$TAG"@' "$base" >"$f"
expect pass "a comment line in the tag check's run block is ignored" "$f"

# A '}}' inside a single-quoted literal does not end the expression.
f="$work/literal-brace.yml"; cat >"$f" <<'EOF'
name: Release
on: workflow_dispatch
jobs:
  dump:
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: ${{ '}}' || secrets.TAURI_SIGNING_PRIVATE_KEY }}
        run: echo x
EOF
expect fail "a '}}' inside a string literal does not hide a read of the key" "$f"

f="$work/unterminated.yml"; cat >"$f" <<'EOF'
name: Release
on: workflow_dispatch
jobs:
  dump:
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY
        run: echo x
EOF
expect fail "an unterminated expression that names the key is refused" "$f"

f="$work/token-lookalike.yml"; cat >"$f" <<'EOF'
name: Release
on: workflow_dispatch
jobs:
  dump:
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: ${{ secrets.GITHUB_TOKEN2 }}
        run: echo x
EOF
expect fail "a secret named like GITHUB_TOKEN2 is not mistaken for the token" "$f"

# A merge key adds keys that a parser may or may not merge; GitHub reads them as written.
f="$work/merge-key.yml"; cat >"$f" <<'EOF'
name: Release
on: workflow_dispatch
jobs:
  sign:
    <<: {environment: release-signing}
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
        run: echo x
EOF
expect fail "a merge key (<<) is refused, so an environment cannot arrive by merge" "$f"

f="$work/alias.yml"; cat >"$f" <<'EOF'
name: Release
on: workflow_dispatch
jobs:
  probe:
    runs-on: &os ubuntu-22.04
    steps:
      - run: echo x
  other:
    runs-on: *os
    steps:
      - run: echo y
EOF
expect fail "an alias is refused, so the file has one reading" "$f"

f="$work/duplicate-gated-first.yml"; sed 's/^    environment: release-signing$/    environment: release-signing\n    environment: staging/' "$base" >"$f"
expect fail "a duplicated environment key is refused, whichever value a parser keeps" "$f"

f="$work/duplicate-staging-first.yml"; sed 's/^    environment: release-signing$/    environment: staging\n    environment: release-signing/' "$base" >"$f"
expect fail "a duplicated environment key is refused with the gated value last" "$f"

echo "== bypasses found in the second review"
f="$work/uppercase-context.yml"; cat >"$f" <<'EOF'
name: Release
on: workflow_dispatch
permissions: {}
jobs:
  dump:
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: ${{ SECRETS.TAURI_SIGNING_PRIVATE_KEY }}
        run: echo x
EOF
expect fail "an upper-case SECRETS context reads the key, so it is refused" "$f"

f="$work/yes-job-collision.yml"; cat >"$f" <<'EOF'
name: Release
on: workflow_dispatch
permissions: {}
jobs:
  release:
    environment: release-signing
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
        run: echo x
  yes:
    runs-on: ubuntu-22.04
    steps:
      - env:
          K: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
        run: echo y
EOF
expect fail "a job id that YAML 1.1 reads as a boolean (yes) is refused, so it cannot hide a job" "$f"

f="$work/root-true-key.yml"; cat >"$f" <<'EOF'
name: Release
on: workflow_dispatch
permissions: {}
true: {}
jobs:
  dump:
    runs-on: ubuntu-22.04
    steps:
      - run: echo x
EOF
expect fail "a root key that YAML 1.1 reads as a boolean is refused, even beside the trigger" "$f"

f="$work/check-shell-override.yml"; sed 's@^        run: scripts/check-release-tag-on-main.sh "\$TAG"$@&\n        shell: bash -c '"'"'exit 0'"'"' {0}@' "$base" >"$f"
expect fail "a shell override on the tag check step is refused, since it can stop the check from running" "$f"

f="$work/preflight-defaults.yml"; sed 's/^  preflight:$/  preflight:\n    defaults:\n      run:\n        shell: bash -c '"'"'exit 0'"'"' {0}/' "$base" >"$f"
expect fail "a preflight job with 'defaults' is refused, since it can change the shell of the tag check" "$f"

f="$work/tag-literal.yml"; sed 's@^          TAG: ${{ github.ref_name }}$@          TAG: v0.0.1@' "$base" >"$f"
expect fail "a tag check whose TAG is a literal, not github.ref_name, is refused" "$f"

f="$work/checkout-ref.yml"; sed 's@^          fetch-depth: 0$@          fetch-depth: 0\n          ref: refs/heads/main@' "$base" >"$f"
expect fail "a full-history checkout that names a ref is refused" "$f"

f="$work/actions-write.yml"; cat >"$f" <<'EOF'
name: Dispatcher
on:
  push:
    tags:
      - 'v*'
permissions:
  actions: write
jobs:
  dispatch:
    runs-on: ubuntu-22.04
    steps:
      - run: echo x
EOF
expect fail "a workflow that grants 'actions: write' is refused, since it can dispatch a signing workflow" "$f"

f="$work/no-top-permissions.yml"; sed '/^permissions: {}$/d' "$base" >"$f"
expect fail "a signing workflow with no top-level 'permissions' is refused" "$f"

echo "== an absent or broken Ruby fails closed"
fakebin="$work/broken-ruby"; mkdir -p "$fakebin"
printf '#!/usr/bin/env bash\nexit 127\n' >"$fakebin/ruby"; chmod +x "$fakebin/ruby"
status=0
output=$(PATH="$fakebin:$PATH" "$checker" "$base" 2>&1) || status=$?
if [ "$status" -eq 2 ] && [[ "$output" == "INPUT ERROR"* ]]; then
  echo "ok   a ruby that cannot load its YAML library is an input error, not a pass"
  passed=$((passed + 1))
else
  echo "FAIL a broken ruby must fail closed: exited $status" >&2
  printf '%s\n' "$output" >&2
  failed=$((failed + 1))
fi

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
