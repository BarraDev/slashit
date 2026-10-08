#!/usr/bin/env bash
# Build the application the desktop acceptance journeys run against.
#
# This is the only supported way to build it, locally and in CI, so the two
# cannot drift. It produces the ordinary debug, custom-protocol build that
# `cargo tauri build --debug --no-bundle` would, with one difference: the
# frontend module loses its WebAssembly `name` section (function names kept
# only for stack traces, about nine tenths of a debug module). WebKit loads
# and parses the whole module in every session, so this saves seconds per
# application start. Nothing else changes: same profile, debug assertions on,
# no wasm-opt, and a check that the code and data sections are byte-identical.
# The Subresource Integrity pin in dist/index.html moves to the stripped
# module, so the page loads it exactly as the product does.
#
#   1. `trunk build`, exactly what tauri.conf.json's beforeBuildCommand runs;
#   2. strip and verify dist/, and re-pin the module in dist/index.html
#      (crates/slashit-acceptance/src/wasm_names.rs);
#   3. `cargo tauri build --debug --no-bundle` with beforeBuildCommand removed
#      for this invocation only, so Trunk does not rebuild dist/ over the
#      stripped module before it is embedded;
#   4. check that the embedded dist/ is still stripped and that the binary was
#      rebuilt after it, then record the binary in target/acceptance-app.stamp
#      for scripts/run-desktop-acceptance.sh.
#
# `trunk serve`, `./dev.sh`, plain `cargo tauri build` and release builds are
# untouched: nothing here edits a configuration file.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

now_ms() { date +%s%3N; }
build_started=$(now_ms)
report_build_duration() {
  local status=$? ended
  ended=$(now_ms)
  printf 'ACCEPTANCE_PHASE {"phase":"build_total","duration_ms":%d,"status":%d}\n' \
    "$((ended - build_started))" "$status"
}
trap report_build_duration EXIT

run_timed() {
  local phase=$1 started ended status
  shift
  started=$(now_ms)
  if "$@"; then status=0; else status=$?; fi
  ended=$(now_ms)
  printf 'ACCEPTANCE_PHASE {"phase":"%s","duration_ms":%d,"status":%d}\n' \
    "$phase" "$((ended - started))" "$status"
  return "$status"
}

fail() {
  echo "build-acceptance-app: $*" >&2
  exit 1
}

binary=target/debug/slashit-ui
stamp=target/acceptance-app.stamp

# Step 1 replaces the configured beforeBuildCommand, so it must stay the same
# command. If the configuration changes, this script must change with it.
grep -Eq '"beforeBuildCommand": *"trunk build"' src-tauri/tauri.conf.json ||
  fail "src-tauri/tauri.conf.json no longer runs \`trunk build\` before a build; update this script to match"

# A failed run must not leave a stamp that vouches for a stale binary.
rm -f "$stamp"

echo "==> trunk build (debug frontend)"
run_timed frontend_build trunk build

echo "==> strip the WebAssembly name section from dist/"
run_timed wasm_strip cargo run --quiet -p slashit-acceptance --bin strip-wasm-names -- dist ||
  fail "stripping or verifying the frontend module failed; see above"

wasm=$(find dist -maxdepth 1 -name '*_bg.wasm' -print)

echo "==> cargo tauri build --debug --no-bundle (embedding the stripped dist/)"
run_timed tauri_app_build cargo tauri build --debug --no-bundle --config '{"build":{"beforeBuildCommand":null}}'

echo "==> verify the acceptance application"
run_timed build_verification cargo run --quiet -p slashit-acceptance --bin strip-wasm-names -- --check dist ||
  fail "dist/ was rebuilt during the Tauri build; the binary embeds an unstripped module"
[[ -x $binary ]] || fail "no application binary at $binary"
# dist/ is embedded with include_bytes!, so a changed module forces a rebuild.
# A binary older than the module means that did not happen.
[[ $binary -nt $wasm ]] ||
  fail "$binary is older than $wasm, so it does not embed the stripped module"

{
  echo "binary=$binary"
  echo "sha256=$(sha256sum "$binary" | cut -d' ' -f1)"
  echo "wasm=$wasm"
  echo "wasm_bytes=$(stat -c %s "$wasm")"
} >"$stamp"

echo "acceptance application: $binary (frontend module $(stat -c %s "$wasm") bytes, no name section, integrity pinned)"
