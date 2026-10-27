#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Signs a fixed, non-sensitive payload with the Tauri CLI and checks the result
# with the verifier the installed application uses. It proves whether the
# signing key in the environment belongs to the public key given here.
#
# usage: scripts/updater-signing-probe.sh <pubkey-file> <work-dir>
#
#   <pubkey-file>  the base64 public key, as in plugins.updater.pubkey of
#                  src-tauri/tauri.conf.json
#   <work-dir>     scratch directory for the payload and its signatures
#
# The signing key is not an argument and is never read by this script. The Tauri
# CLI takes it from TAURI_SIGNING_PRIVATE_KEY (the key itself) or
# TAURI_SIGNING_PRIVATE_KEY_PATH (a file), and the password from
# TAURI_SIGNING_PRIVATE_KEY_PASSWORD. Nothing here prints those values.
#
# The Tauri CLI command is `cargo tauri` unless SIGNING_CLI overrides it, for
# example SIGNING_CLI=tauri in CI where the release CLI is installed globally.
#
# Checks, in order:
#   1. A valid signature of the payload verifies.
#   2. The same signature over a changed payload is rejected.
#   3. A signature over a different payload is rejected for the original one.
#
# Exit status: 0 PASS. 1 FAIL: the key does not match the public key, or the
# verifier accepted what it must reject. 2 a CLI contract or input error, which
# is not evidence about the key.
set -euo pipefail

fail_input() {
  echo "INPUT ERROR: $*" >&2
  exit 2
}

[ "$#" -eq 2 ] || fail_input "usage: updater-signing-probe.sh <pubkey-file> <work-dir>"
pubkey=$1
work=$2
[ -s "$pubkey" ] || fail_input "the public key file is missing or empty"

if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ] && [ -z "${TAURI_SIGNING_PRIVATE_KEY_PATH:-}" ]; then
  fail_input "neither TAURI_SIGNING_PRIVATE_KEY nor TAURI_SIGNING_PRIVATE_KEY_PATH is set"
fi

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
signer_cli=${SIGNING_CLI:-cargo tauri}
mkdir -p "$work"
work=$(cd "$work" && pwd)

# The verifier is built from the repository, not taken from elsewhere. Its build
# output goes under the repository's target/ (the workspace's .cargo/config.toml
# redirects the target directory, so it is passed explicitly here).
probe_target="$root/target/updater-signing-probe"
cargo build --quiet --release --locked \
  --manifest-path "$root/tools/updater-signing-probe/Cargo.toml" \
  --target-dir "$probe_target" \
  || fail_input "the verifier does not build"
verifier="$probe_target/release/slashit-updater-signing-probe"

# sign <payload>: writes <payload>.sig with the Tauri CLI. A failure here is a
# CLI contract error (missing CLI, unreadable key, wrong password), not a verdict.
sign() {
  local log="$work/sign.log"
  if ! $signer_cli signer sign "$1" >"$log" 2>&1; then
    echo "CLI CONTRACT ERROR: the Tauri signer failed to sign the probe payload." >&2
    echo "The signer's own output follows; it does not include key material." >&2
    tail -n 20 "$log" >&2
    exit 2
  fi
  [ -s "$1.sig" ] || {
    echo "CLI CONTRACT ERROR: the signer exited 0 but wrote no $(basename "$1").sig." >&2
    exit 2
  }
}

# verify <signature> <payload>: 0 verified, 1 rejected, 2 input error.
verify() {
  local status=0
  "$verifier" verify "$pubkey" "$1" "$2" || status=$?
  return "$status"
}

payload="$work/probe-payload.txt"
other="$work/probe-other.txt"
printf 'SlashIt updater signing probe. This payload is not a release.\n' >"$payload"
printf 'SlashIt updater signing probe, a different payload.\n' >"$other"
printf 'SlashIt updater signing probe. This payload is not a release. Changed.\n' >"$work/probe-tampered.txt"

sign "$payload"
sign "$other"

verify "$payload.sig" "$payload" || status=$?
case "${status:-0}" in
  0) echo "ok   the release key's signature verifies against the configured public key" ;;
  1)
    echo "FAIL the signing key does not belong to the public key in tauri.conf.json" >&2
    echo "     A signature the key produced does not verify. Do not publish a release" >&2
    echo "     until the secret and the configured public key are reconciled." >&2
    exit 1
    ;;
  *) echo "INPUT ERROR: the verifier could not check the signature" >&2; exit 2 ;;
esac

# expect_rejected <label> <verifier status>: 1 is the only status that proves
# the rejection. 0 means the verifier accepted what it must reject (a FAIL). Any
# other status is an input error, not evidence about the key.
expect_rejected() {
  case "$2" in
    1) echo "ok   $1" ;;
    0) echo "FAIL $1 was accepted" >&2; exit 1 ;;
    *) echo "INPUT ERROR: the verifier could not check: $1 (status $2)" >&2; exit 2 ;;
  esac
}

status=0
verify "$payload.sig" "$work/probe-tampered.txt" || status=$?
expect_rejected "a signature over a changed payload is rejected" "$status"

status=0
verify "$other.sig" "$payload" || status=$?
expect_rejected "a signature over another payload is rejected" "$status"

echo "PASS the signing key matches the configured public key, and tampering is rejected."
