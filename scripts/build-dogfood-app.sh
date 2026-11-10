#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Build the self-contained dogfood binary: the frontend (HTML, CSS, JS, WASM)
# is embedded at compile time, so the GUI needs no Trunk process and no
# server on localhost:1420, now or after a reboot. Development keeps its own
# loop: `trunk serve` / `./dev.sh` are untouched and nothing here edits a
# configuration file.
#
#   1. `trunk build`: the same command tauri.conf.json's beforeBuildCommand
#      runs, executed once here so the build step below can skip it;
#   2. `cargo tauri build --debug --no-bundle` with embedding: the Tauri CLI
#      enables the custom-protocol feature, which is what makes the binary load
#      assets from the bundle instead of build.devUrl;
#   3. prove the embedding: the WASM file name that dist/index.html loads must
#      appear inside the binary, otherwise the build is refused;
#   4. promote the binary and its `slashit` CLI into target/dogfood/. Cargo's
#      target/debug is shared with scripts/build-acceptance-app.sh, which
#      overwrites slashit-ui with a binary that has no embedded frontend. A
#      promoted copy keeps the dogfood sidecar's sha256 valid across acceptance
#      builds. Each file is published by rename, so a running GUI's executable
#      is never truncated in place;
#   5. write the sidecar target/dogfood/slashit-ui.frontend last, binding the
#      declaration "embedded" to the promoted binary's sha256.
#
# The sidecar is the declared frontend mode scripts/supervise-dogfood.sh reads.
# A binary without one is a development build and needs the dev server.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

fail() {
  echo "build-dogfood-app: $*" >&2
  exit 1
}

cargo_binary=target/debug/slashit-ui
cargo_cli=target/debug/slashit
promoted=target/dogfood
binary=$promoted/slashit-ui
cli=$promoted/slashit
sidecar=$binary.frontend

grep -Eq '"beforeBuildCommand": *"trunk build"' src-tauri/tauri.conf.json ||
  fail "src-tauri/tauri.conf.json no longer runs \`trunk build\`; update this script to match"

# A failed build must not leave a sidecar vouching for a stale binary.
rm -f "$sidecar"

echo "==> trunk build (release-style frontend into dist/)"
trunk build || fail "trunk build failed; see above"

echo "==> cargo tauri build --debug --no-bundle (assets embedded)"
cargo tauri build --debug --no-bundle \
  --config '{"build":{"beforeBuildCommand":null}}' ||
  fail "cargo tauri build failed; see above"

echo "==> cargo build -p slashit (the CLI the supervisor verifies against)"
cargo build -p slashit || fail "cargo build -p slashit failed; see above"

wasm=$(find dist -maxdepth 1 -name '*_bg.wasm' -printf '%f\n' | head -n 1)
[ -n "$wasm" ] || fail "no *_bg.wasm in dist/"
grep -a -q -F "$wasm" "$cargo_binary" ||
  fail "$cargo_binary does not contain $wasm: the frontend is not embedded (a development build?); nothing is promoted"
[ -x "$cargo_cli" ] || fail "$cargo_cli is missing after the build"

# Promotion: copy to a temporary name beside the destination, then rename. A
# rename replaces the directory entry and leaves a running executable intact.
mkdir -p "$promoted"
publish() { # source destination
  local tmp="$2.tmp.$$"
  cp -- "$1" "$tmp" && chmod 755 "$tmp" && mv -f -- "$tmp" "$2" ||
    { rm -f -- "$tmp"; fail "cannot promote $1 to $2"; }
}
publish "$cargo_binary" "$binary"
publish "$cargo_cli" "$cli"

# The sidecar is written beside its binary through a temporary file and a
# rename, so the supervisor never reads a half-written declaration.
sha=$(sha256sum "$binary" | cut -d' ' -f1)
side_tmp="$sidecar.tmp.$$"
{
  echo "mode=embedded"
  echo "sha256=$sha"
  echo "wasm=$wasm"
} >"$side_tmp" && mv -f -- "$side_tmp" "$sidecar" ||
  { rm -f -- "$side_tmp"; fail "cannot write the sidecar $sidecar"; }
echo "==> embedded frontend verified ($wasm); promoted to $binary and recorded in $sidecar"
