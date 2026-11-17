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
    # Simulate source changes while the build runs: a commit, or a new file.
    [ -z "${STUB_COMMIT_DURING:-}" ] || git commit -q --allow-empty -m during-build
    [ -z "${STUB_TOUCH_DURING:-}" ] || : >during-build.txt
    if [ -n "${STUB_NO_EMBED:-}" ]; then printf 'no assets here\n' >target/debug/slashit-ui
    else printf 'ELF...dogfood_test_bg.wasm...%s\n' "${STUB_BUILD_ID:-}" >target/debug/slashit-ui; fi ;;
  *" -p slashit "*) printf '#!/bin/sh\necho cli\n' >target/debug/slashit; chmod +x target/debug/slashit ;;
  *) echo "stub cargo: unexpected: $*" >&2; exit 2 ;;
esac
STUB
# Stub mv: fails the CLI's promotion rename when STUB_MV_FAIL_CLI is set, or the
# sidecar's when STUB_MV_FAIL_SIDE is set, to simulate a promotion that fails
# after the binary (and possibly the CLI) was already replaced.
real_mv=$(command -v mv)
cat >"$work/bin/mv" <<STUB
#!/usr/bin/env bash
if [ -n "\${STUB_MV_FAIL_CLI:-}" ] && [[ "\$*" == *".tmp."* ]] && [[ "\$*" == */slashit ]]; then exit 1; fi
if [ -n "\${STUB_MV_FAIL_SIDE:-}" ] && [[ "\$*" == *".tmp."* ]] && [[ "\$*" == *.frontend ]]; then exit 1; fi
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

# 2b. Rollback candidate: a successful rebuild keeps the replaced pair, which
#     was self-consistent, in target/dogfood/previous/ and records provenance.
old_want=$want
out=$(STUB_BUILD_ID=second run_build); st=$?
want=$(sha256sum "$repo/target/dogfood/slashit-ui" 2>/dev/null | cut -d' ' -f1)
if [ "$st" -eq 0 ] && [ "$want" != "$old_want" ] &&
  [ "$(sha256sum "$repo/target/dogfood/previous/slashit-ui" 2>/dev/null | cut -d' ' -f1)" = "$old_want" ] &&
  grep -qx "sha256=$old_want" "$repo/target/dogfood/previous/slashit-ui.frontend" &&
  grep -qE '^commit=' "$repo/target/dogfood/slashit-ui.frontend" && grep -qE '^dirty=' "$repo/target/dogfood/slashit-ui.frontend"; then
  ok "a successful rebuild keeps the replaced, verified pair as the rollback candidate, and the sidecar records provenance"
else bad "previous verified pair was not kept (exit $st)"; printf '%s\n' "$out" >&2; fi

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

# 7. Regression: a sidecar rename that fails after the binary and the CLI were
#    both replaced restores all three files, so the sidecar still vouches for
#    the binary it describes, and the new build's binary is not left behind.
want=$(sha256sum "$repo/target/dogfood/slashit-ui" | cut -d' ' -f1)
cli_before=$(sha256sum "$repo/target/dogfood/slashit" | cut -d' ' -f1)
out=$(STUB_BUILD_ID=3 STUB_MV_FAIL_SIDE=1 run_build); st=$?
if [ "$st" -ne 0 ] && [[ "$out" == *"previous promoted pair is restored"* ]] &&
  promoted_pair_intact &&
  [ "$(sha256sum "$repo/target/dogfood/slashit" | cut -d' ' -f1)" = "$cli_before" ] &&
  [ -z "$(find "$repo/target/dogfood" -name '*.tmp.*' -print -quit)" ]; then
  ok "a failed sidecar promotion after the binary and CLI were replaced restores all three"
else bad "partial promotion left the promoted pair inconsistent (exit $st)"; printf '%s\n' "$out" >&2; fi

# 8. A build in a linked worktree promotes into the checkout that owns the common
#    .git (where the supervisor launches from), and records the worktree's commit.
main=$work/wt-main
mkdir -p "$main/scripts" "$main/src-tauri"
cp "$src_script" "$main/scripts/build-dogfood-app.sh"
cp "$repo/src-tauri/tauri.conf.json" "$main/src-tauri/"
printf 'target/\ndist/\n' >"$main/.gitignore"
git -C "$main" init -q
git -C "$main" config user.name "Build Test"; git -C "$main" config user.email build-test@example.invalid
git -C "$main" add -A; git -C "$main" commit -qm base
git -C "$main" worktree add -q "$work/wt-linked" -b linked 2>/dev/null
echo change >"$work/wt-linked/f"; git -C "$work/wt-linked" add f; git -C "$work/wt-linked" commit -qm linked
wt_head=$(git -C "$work/wt-linked" rev-parse HEAD)
out=$(cd "$work/wt-linked" && env PATH="$work/bin:$PATH" bash scripts/build-dogfood-app.sh 2>&1); st=$?
if [ "$st" -eq 0 ] && [ -x "$main/target/dogfood/slashit-ui" ] && [ ! -e "$work/wt-linked/target/dogfood" ] &&
  grep -qx "commit=$wt_head" "$main/target/dogfood/slashit-ui.frontend" &&
  grep -qx "sha256=$(sha256sum "$main/target/dogfood/slashit-ui" | cut -d' ' -f1)" "$main/target/dogfood/slashit-ui.frontend"; then
  ok "a build in a linked worktree promotes into the main checkout's target/dogfood with the worktree's commit"
else bad "linked-worktree build promoted to the wrong place (exit $st)"; printf '%s\n' "$out" >&2; fi

# 9. Provenance: the sidecar names the commit the build started from, and the
#    build is refused when HEAD or the working-tree status changes during it.
main_build() { (cd "$main" && env PATH="$work/bin:$PATH" "$@" bash scripts/build-dogfood-app.sh) 2>&1; }
pre_head=$(git -C "$main" rev-parse HEAD)
out=$(main_build STUB_BUILD_ID=clean); st=$?
if [ "$st" -eq 0 ] && grep -qx "commit=$pre_head" "$main/target/dogfood/slashit-ui.frontend" &&
  grep -qx 'dirty=no' "$main/target/dogfood/slashit-ui.frontend" && [ -f "$main/target/dogfood/previous/slashit-ui" ]; then
  ok "an unchanged source records the pre-build HEAD and dirty=no"
else bad "clean build provenance is wrong (exit $st)"; printf '%s\n' "$out" >&2; fi
dog_sums() { (cd "$main/target/dogfood" && sha256sum slashit-ui slashit slashit-ui.frontend previous/slashit-ui previous/slashit-ui.frontend 2>&1); }
before=$(dog_sums); pre_head=$(git -C "$main" rev-parse HEAD)
out=$(main_build STUB_BUILD_ID=moved STUB_COMMIT_DURING=1); st=$?
post_head=$(git -C "$main" rev-parse HEAD)
if [ "$st" -ne 0 ] && [ "$post_head" != "$pre_head" ] && [[ "$out" == *"$pre_head"* ]] && [[ "$out" == *"$post_head"* ]] &&
  [[ "$out" == *"changed during the build"* ]] && [ "$(dog_sums)" = "$before" ]; then
  ok "a commit during the build is refused, naming both commits; the promoted pair and previous build are unchanged"
else bad "a commit during the build was not refused cleanly (exit $st)"; printf '%s\n' "$out" >&2; fi
before=$(dog_sums)
out=$(main_build STUB_BUILD_ID=touched STUB_TOUCH_DURING=1); st=$?
rm -f "$main/during-build.txt"
if [ "$st" -ne 0 ] && [[ "$out" == *"changed during the build"* ]] && [[ "$out" == *"during-build.txt"* ]] && [ "$(dog_sums)" = "$before" ]; then
  ok "an untracked file created during the build is refused; the promoted pair and previous build are unchanged"
else bad "a file created during the build was not refused cleanly (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "$pass passed, $fails failed"
[ "$fails" -eq 0 ]
