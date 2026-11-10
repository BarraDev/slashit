#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Hermetic test of scripts/build-dogfood-app.sh. Runs the script in a disposable
# copy of the repository layout with stub trunk and cargo on PATH, so no real
# compilation happens. Covers: promotion into target/dogfood, the sidecar bound
# to the promoted binary's sha256, refusal when the frontend is not embedded,
# immunity to a later overwrite of target/debug (the acceptance collision), and
# the previous promoted pair surviving failed builds and failed promotions.
#
# usage: scripts/test-build-dogfood-app.sh
set -uo pipefail

[ "$(uname -s)" = Linux ] || { echo "FAIL $0 is Linux only" >&2; exit 1; }

src_script=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/build-dogfood-app.sh
work=$(mktemp -d "${TMPDIR:-/tmp}/build-dogfood-test.XXXXXX")
trap 'rm -rf "$work"' EXIT

pass=0
fails=0
ok() { pass=$((pass + 1)); echo "ok   $1"; }
bad() { fails=$((fails + 1)); echo "FAIL $1"; }

# A disposable checkout: the script, the tauri.conf.json it greps, and stubs.
repo="$work/repo"
mkdir -p "$repo/scripts" "$repo/src-tauri" "$work/bin"
cp "$src_script" "$repo/scripts/build-dogfood-app.sh"
printf '{ "build": { "beforeBuildCommand": "trunk build" } }\n' >"$repo/src-tauri/tauri.conf.json"

# Stub trunk: produces the frontend bundle name the embedding check looks for.
cat >"$work/bin/trunk" <<'STUB'
#!/usr/bin/env bash
[ -z "${STUB_TRUNK_FAIL:-}" ] || exit 1
mkdir -p dist && : >dist/dogfood_test_bg.wasm
STUB
# Stub cargo: the tauri build writes a binary that embeds the wasm name, or
# one that does not when STUB_NO_EMBED is set; `build -p slashit` writes the CLI.
cat >"$work/bin/cargo" <<'STUB'
#!/usr/bin/env bash
mkdir -p target/debug
case " $* " in
  *" tauri "*)
    if [ -n "${STUB_NO_EMBED:-}" ]; then printf 'no assets here\n' >target/debug/slashit-ui
    else printf 'ELF...dogfood_test_bg.wasm...%s\n' "${STUB_BUILD_ID:-}" >target/debug/slashit-ui; fi ;;
  *" -p slashit "*) printf '#!/bin/sh\necho cli\n' >target/debug/slashit; chmod +x target/debug/slashit ;;
  *) echo "stub cargo: unexpected: $*" >&2; exit 2 ;;
esac
STUB
# Stub mv: fails the CLI's promotion rename when STUB_MV_FAIL_CLI is set, to
# simulate a promotion that fails after the binary was already replaced.
real_mv=$(command -v mv)
cat >"$work/bin/mv" <<STUB
#!/usr/bin/env bash
if [ -n "\${STUB_MV_FAIL_CLI:-}" ] && [[ "\$*" == *".tmp."* ]] && [[ "\$*" == */slashit ]]; then exit 1; fi
exec "$real_mv" "\$@"
STUB
chmod +x "$work/bin/trunk" "$work/bin/cargo" "$work/bin/mv"

run_build() {
  (cd "$repo" && env PATH="$work/bin:$PATH" bash scripts/build-dogfood-app.sh) 2>&1
}

# 1. Happy path: promoted binary, CLI and a sidecar bound to the binary's sha.
out=$(run_build); st=$?
want=$(sha256sum "$repo/target/dogfood/slashit-ui" 2>/dev/null | cut -d' ' -f1)
if [ "$st" -eq 0 ] && [ -x "$repo/target/dogfood/slashit" ] &&
  grep -qx 'mode=embedded' "$repo/target/dogfood/slashit-ui.frontend" &&
  grep -qx "sha256=$want" "$repo/target/dogfood/slashit-ui.frontend" &&
  grep -qx 'wasm=dogfood_test_bg.wasm' "$repo/target/dogfood/slashit-ui.frontend"; then
  ok "promotes the binary and CLI and binds the sidecar to the promoted sha256"
else bad "happy path (exit $st)"; printf '%s\n' "$out" >&2; fi

# 2. D2: an acceptance-style overwrite of target/debug must not invalidate the
#    promoted pair. The sidecar still matches the promoted binary.
printf 'acceptance build without assets\n' >"$repo/target/debug/slashit-ui"
now=$(sha256sum "$repo/target/dogfood/slashit-ui" | cut -d' ' -f1)
if [ "$now" = "$want" ] && grep -qx "sha256=$want" "$repo/target/dogfood/slashit-ui.frontend"; then
  ok "a later overwrite of target/debug leaves the promoted binary and sidecar valid"
else bad "promoted binary changed after a target/debug overwrite"; fi

# The last promoted pair must stay consistent: the binary and the sidecar that
# vouches for its sha256 are both the previous ones.
promoted_pair_intact() {
  [ "$(sha256sum "$repo/target/dogfood/slashit-ui" 2>/dev/null | cut -d' ' -f1)" = "$want" ] &&
    grep -qx "sha256=$want" "$repo/target/dogfood/slashit-ui.frontend" 2>/dev/null
}

# 3. Refusal: a build whose binary does not embed the frontend publishes
#    nothing, so the previous promoted pair is left intact.
out=$(STUB_NO_EMBED=1 run_build); st=$?
if [ "$st" -ne 0 ] && [[ "$out" == *"not embedded"* ]] && promoted_pair_intact; then
  ok "a non-embedded build is refused and the previous promoted pair is kept"
else bad "non-embedded build was not refused cleanly (exit $st)"; printf '%s\n' "$out" >&2; fi

# 4. Regression: a failing trunk build (before anything is promoted) must not
#    remove the sidecar of the last known-good promoted binary.
out=$(STUB_TRUNK_FAIL=1 run_build); st=$?
if [ "$st" -ne 0 ] && [[ "$out" == *"trunk build failed"* ]] && promoted_pair_intact; then
  ok "a failing trunk build keeps the previous promoted pair and its sidecar"
else bad "failing trunk build damaged the promoted pair (exit $st)"; printf '%s\n' "$out" >&2; fi

# 5. Atomic publication: no temporary files are left behind by a successful run.
run_build >/dev/null 2>&1
if [ -z "$(find "$repo/target/dogfood" -name '*.tmp.*' -print -quit)" ]; then
  ok "no temporary files remain after publication"
else bad "temporary files were left in target/dogfood"; fi

# 6. Regression: a promotion that fails after the binary was replaced (the CLI
#    destination is occupied by a directory, so the rename cannot succeed)
#    restores the previous binary and sidecar, and leaves the directory alone.
want=$(sha256sum "$repo/target/dogfood/slashit-ui" | cut -d' ' -f1)
cli_before=$(sha256sum "$repo/target/dogfood/slashit" | cut -d' ' -f1)
out=$(STUB_BUILD_ID=2 STUB_MV_FAIL_CLI=1 run_build); st=$?
if [ "$st" -ne 0 ] && [[ "$out" == *"previous promoted pair is restored"* ]] &&
  promoted_pair_intact && [ -x "$repo/target/dogfood/slashit" ] &&
  [ "$(sha256sum "$repo/target/dogfood/slashit" | cut -d' ' -f1)" = "$cli_before" ] &&
  [ -z "$(find "$repo/target/dogfood" -name '*.tmp.*' -print -quit)" ]; then
  ok "a failed promotion restores the previous binary, CLI and sidecar"
else bad "failed promotion left the promoted pair inconsistent (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "$pass passed, $fails failed"
[ "$fails" -eq 0 ]
