#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Tests scripts/updater-signing-probe.sh with disposable fixture keys. The keys
# are generated here by the Tauri CLI and exist only under target/. No
# production secret is read.
#
# usage: scripts/test-updater-signing.sh
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
probe="$root/scripts/updater-signing-probe.sh"
signer_cli=${SIGNING_CLI:-cargo tauri}
work="$root/target/updater-signing-test"
rm -rf "$work"
mkdir -p "$work"

passed=0
failed=0

# run_case <name> <expected exit> <command...>: runs the command with the
# environment the caller set, and compares its exit status.
run_case() {
  local name=$1 expected=$2
  shift 2
  local status=0 log="$work/case-$((passed + failed)).log"
  "$@" >"$log" 2>&1 || status=$?
  if [ "$status" -eq "$expected" ]; then
    passed=$((passed + 1))
    echo "ok   $name (exit $status)"
  else
    failed=$((failed + 1))
    echo "FAIL $name: expected exit $expected, got $status"
    sed 's/^/     /' "$log"
  fi
}

echo "== disposable fixture keys"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD='fixture-only-not-a-secret'
if ! $signer_cli --version >/dev/null 2>&1 && ! $signer_cli signer --help >/dev/null 2>&1; then
  echo "INPUT ERROR: the Tauri CLI ($signer_cli) is not available; install tauri-cli to run this test." >&2
  exit 2
fi
$signer_cli signer generate --ci -w "$work/a.key" -p "$TAURI_SIGNING_PRIVATE_KEY_PASSWORD" >/dev/null 2>&1
$signer_cli signer generate --ci -w "$work/b.key" -p "$TAURI_SIGNING_PRIVATE_KEY_PASSWORD" >/dev/null 2>&1
echo "ok   generated two fixture key pairs"

# Each case runs with exactly the environment it names.
unset TAURI_SIGNING_PRIVATE_KEY
export TAURI_SIGNING_PRIVATE_KEY_PATH="$work/a.key"
export SIGNING_CLI="$signer_cli"

echo "== the probe"
run_case "a key that matches its public key passes" 0 \
  "$probe" "$work/a.key.pub" "$work/run-match"

run_case "a key that does not match the public key fails" 1 \
  "$probe" "$work/b.key.pub" "$work/run-mismatch"

echo "== input and contract errors are not verdicts"
run_case "no signing key in the environment is an input error" 2 \
  env -u TAURI_SIGNING_PRIVATE_KEY_PATH "$probe" "$work/a.key.pub" "$work/run-nokey"

run_case "a missing public key file is an input error" 2 \
  "$probe" "$work/does-not-exist.pub" "$work/run-nopub"

run_case "a signer that does not exist is a contract error" 2 \
  env SIGNING_CLI="$work/no-such-signer" "$probe" "$work/a.key.pub" "$work/run-nocli"

run_case "a wrong password is a contract error, not a mismatch" 2 \
  env TAURI_SIGNING_PRIVATE_KEY_PASSWORD='wrong-password' "$probe" "$work/a.key.pub" "$work/run-badpw"

echo
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
