#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Hermetic test of scripts/build-dogfood-app.sh. Runs the script in a disposable
# copy of the repository layout with stub trunk and cargo on PATH, so no real
# compilation happens. Covers: promotion into target/dogfood, the sidecar bound
# to the promoted binary's sha256, refusal when the frontend is not embedded,
# immunity to a later overwrite of target/debug (the acceptance collision), and
# the previous promoted pair surviving failed builds and failed promotions,
# source edits during a build (dirty files included), a candidate swapped
# between validation and publication, two real builds contending the lock, and
# the source identity of Git and JJ-only checkouts (a build whose source has no
# commit identity is refused). The JJ tests need a pinned jj (mise exec -- jj);
# without one they are reported as SKIP, not as a pass.
#
# usage: scripts/test-build-dogfood-app.sh
set -uo pipefail

[ "$(uname -s)" = Linux ] || { echo "FAIL $0 is Linux only" >&2; exit 1; }

src_script=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/build-dogfood-app.sh
work=$(mktemp -d "${TMPDIR:-/tmp}/build-dogfood-test.XXXXXX")

# Every wait on a fixture is bounded, and nothing the test started outlives it:
# the holds are released, every process whose command line or cwd names $work is
# killed and gone, and only then is $work removed.
work_pids() { # pids (not this shell) whose command line or cwd is under $work
  local d pid
  ps -eo pid=,args= | while read -r pid args; do [[ "$args" == *"$work"* ]] && echo "$pid"; done
  for d in /proc/[0-9]*; do
    case "$(readlink "$d/cwd" 2>/dev/null)" in "$work"|"$work"/*) echo "${d#/proc/}";; esac
  done
}
cleanup() {
  trap - EXIT INT TERM
  local pid i d
  for d in $(find "$work" -type d -name "hold*" 2>/dev/null); do : >"$d/go"; done
  for i in 1 2 3 4 5 6 7 8 9 10; do
    for pid in $(work_pids | sort -u); do [ "$pid" = "$$" ] || [ "$pid" = "$BASHPID" ] || kill -KILL "$pid" 2>/dev/null; done
    wait 2>/dev/null
    [ -z "$(work_pids | grep -vx -e "$$" -e "$BASHPID")" ] && break
    sleep 0.1
  done
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

pass=0
fails=0
ok() { pass=$((pass + 1)); echo "ok   $1"; }
bad() { fails=$((fails + 1)); echo "FAIL $1"; }

# A disposable checkout: the script, the tauri.conf.json it greps, and stubs.
repo="$work/repo"
mkdir -p "$repo/scripts" "$repo/src-tauri" "$work/bin"
cp "$src_script" "$repo/scripts/build-dogfood-app.sh"
printf '{ "build": { "beforeBuildCommand": "trunk build" } }\n' >"$repo/src-tauri/tauri.conf.json"
# Every build needs a source identity: the fixture is a Git checkout with one commit.
printf 'target/\ndist/\n' >"$repo/.gitignore"
git -C "$repo" init -q
git -C "$repo" config user.name "Build Test"; git -C "$repo" config user.email build-test@example.invalid
git -C "$repo" add -A; git -C "$repo" commit -qm base

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
    [ -z "${STUB_EDIT_DURING:-}" ] || echo edited-during-build >>"$STUB_EDIT_DURING"
    # Hold the build open (and so the lock) until the test releases it, but
    # never longer than STUB_HOLD_TIMEOUT seconds: an unreleased hold fails the build.
    if [ -n "${STUB_HOLD_DIR:-}" ]; then
      echo "$PPID" >"$STUB_HOLD_DIR/started.tmp" && mv -f "$STUB_HOLD_DIR/started.tmp" "$STUB_HOLD_DIR/started"
      deadline=$((SECONDS + ${STUB_HOLD_TIMEOUT:-60}))
      while [ ! -e "$STUB_HOLD_DIR/go" ]; do
        [ "$SECONDS" -lt "$deadline" ] || { echo "stub cargo: hold timed out: never released" >&2; exit 3; }
        sleep 0.05
      done
    fi
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
# Stub sha256sum: when STUB_SWAP_AFTER_HASH is set, the first time a staged
# binary is hashed, it is replaced right after its hash was taken: a candidate
# swapped between validation and publication.
real_sha=$(command -v sha256sum)
cat >"$work/bin/sha256sum" <<STUB
#!/usr/bin/env bash
"$real_sha" "\$@"; st=\$?
if [ -n "\${STUB_SWAP_AFTER_HASH:-}" ] && [ ! -e "\$STUB_SWAP_AFTER_HASH" ] && [[ "\$*" == */slashit-ui.tmp.* ]]; then
  : >"\$STUB_SWAP_AFTER_HASH"; echo swapped >>"\${@: -1}"
fi
exit \$st
STUB
chmod +x "$work/bin/trunk" "$work/bin/cargo" "$work/bin/mv" "$work/bin/sha256sum"

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

# 10. A dirty source is refused before anything is built or promoted: a verified
#     build must name a commit that holds exactly what was built.
echo dirty >"$main/f2"; git -C "$main" add f2; git -C "$main" commit -qm f2; echo dirty-before >>"$main/f2"
before=$(dog_sums)
out=$(main_build STUB_BUILD_ID=dirty-refused); st=$?
if [ "$st" -ne 0 ] && [[ "$out" == *"uncommitted changes"* ]] && [ "$(dog_sums)" = "$before" ]; then
  ok "a dirty source is refused before any build; the promoted pair is unchanged"
else bad "a dirty source was not refused cleanly (exit $st)"; printf '%s\n' "$out" >&2; fi
git -C "$main" checkout -q -- f2
before=$(dog_sums)
out=$(main_build STUB_BUILD_ID=edited STUB_EDIT_DURING=f2); st=$?
if [ "$st" -ne 0 ] && [[ "$out" == *"changed during the build"* ]] && [ "$(dog_sums)" = "$before" ]; then
  ok "an edit to an already-dirty file during the build is refused; the promoted pair is unchanged"
else bad "an edit to a dirty file during the build was not refused (exit $st)"; printf '%s\n' "$out" >&2; fi
git -C "$main" checkout -q -- f2

# 11. Publication conflict: the staged binary is replaced after it was hashed.
#     The re-check before the rename refuses it and the prior state is restored.
before=$(dog_sums)
out=$(main_build STUB_BUILD_ID=swap STUB_SWAP_AFTER_HASH="$work/swap.marker"); st=$?
if [ "$st" -ne 0 ] && [ -e "$work/swap.marker" ] && [[ "$out" == *"changed after it was verified"* ]] &&
  [ "$(dog_sums)" = "$before" ] && [ -z "$(find "$main/target/dogfood" -name '*.tmp.*' -print -quit)" ]; then
  ok "a candidate replaced between validation and publication is refused; the promoted pair is unchanged"
else bad "a swapped candidate was published or damaged state (exit $st)"; printf '%s\n' "$out" >&2; fi

# 12. Competing builds: two real processes on one artifact root. The second is
#     refused while the first holds the lock; the first then publishes its own
#     binary with a sidecar that names it.
hold=$work/hold; mkdir -p "$hold"
before=$(dog_sums)
(cd "$main" && env PATH="$work/bin:$PATH" STUB_BUILD_ID=first STUB_HOLD_DIR="$hold" bash scripts/build-dogfood-app.sh >"$work/first.log" 2>&1) &
first=$!
for _ in $(seq 200); do [ -e "$hold/started" ] && break; sleep 0.05; done
out=$(main_build STUB_BUILD_ID=second); st=$?
mid=$(dog_sums)
: >"$hold/go"; wait "$first"; st1=$?
if [ -e "$hold/started" ] && [ "$st" -ne 0 ] && [[ "$out" == *"another dogfood build"* ]] && [ "$mid" = "$before" ] &&
  [ "$st1" -eq 0 ] && grep -q first "$main/target/dogfood/slashit-ui" &&
  grep -qx "sha256=$(sha256sum "$main/target/dogfood/slashit-ui" | cut -d' ' -f1)" "$main/target/dogfood/slashit-ui.frontend"; then
  ok "a second concurrent build is refused while the first holds the lock; the first publishes its own binary"
else bad "competing builds (second exit $st, first exit $st1)"; printf '%s\n' "$out" >&2; cat "$work/first.log" >&2; fi

# 13. The lock dies with its holder: after the build script is killed (its
#     children do not inherit the lock descriptor), the next build runs.
rm -f "$hold/started" "$hold/go"
(cd "$main" && env PATH="$work/bin:$PATH" STUB_HOLD_DIR="$hold" bash scripts/build-dogfood-app.sh >/dev/null 2>&1) &
victim=$!
for _ in $(seq 200); do [ -e "$hold/started" ] && break; sleep 0.05; done
kill -KILL "$(cat "$hold/started")"; wait "$victim" 2>/dev/null
out=$(main_build STUB_BUILD_ID=after-kill); st=$?
: >"$hold/go" # only now lets the orphaned stub cargo exit
if [ "$st" -eq 0 ] && grep -q after-kill "$main/target/dogfood/slashit-ui"; then
  ok "the lock is released when its holder is killed"
else bad "build after a killed holder failed (exit $st)"; printf '%s\n' "$out" >&2; fi

# 14. Git source identity: a detached HEAD is identified by its commit, never by
#     a branch name; an unborn HEAD and a directory with no VCS are refused
#     before anything is built.
git -C "$main" checkout -q --detach
det_head=$(git -C "$main" rev-parse HEAD)
out=$(main_build STUB_BUILD_ID=detached); st=$?
side=$main/target/dogfood/slashit-ui.frontend
if [ "$st" -eq 0 ] && grep -qx "commit=$det_head" "$side" && grep -qx 'vcs=git' "$side" && ! grep -qE 'master|main|linked' "$side"; then
  ok "a detached HEAD is recorded by its commit id, not a branch name"
else bad "detached HEAD identity is wrong (exit $st)"; printf '%s\n' "$out" >&2; fi

refused_cleanly() { # dir: no source identity -> refused, nothing built, nothing promoted
  mkdir -p "$1/scripts" "$1/src-tauri"
  cp "$src_script" "$1/scripts/build-dogfood-app.sh"; cp "$repo/src-tauri/tauri.conf.json" "$1/src-tauri/"
  local o s
  o=$(cd "$1" && env PATH="$work/bin:$PATH" "${@:2}" bash scripts/build-dogfood-app.sh 2>&1); s=$?
  [ "$s" -ne 0 ] && [[ "$o" == *"cannot establish the source identity"* ]] && [ ! -e "$1/dist" ] &&
    [ ! -e "$1/target/dogfood/slashit-ui" ] && [ ! -e "$1/target/dogfood/slashit-ui.frontend" ] || { printf '%s\n' "$o" >&2; return 1; }
}
unborn=$work/unborn; mkdir -p "$unborn"; git -C "$unborn" init -q
if refused_cleanly "$unborn"; then ok "an unborn HEAD is refused: no commit identity, nothing built or promoted"
else bad "an unborn HEAD was not refused cleanly"; fi
novcs=$work/novcs
if refused_cleanly "$novcs"; then ok "a checkout with neither Git nor JJ is refused, with no commit=unknown fallback"
else bad "a directory with no VCS was not refused cleanly"; fi

# JJ-only workspaces: no .git. Needs the pinned jj; hermetic user identity.
jj_real=$(cd "$(dirname "$src_script")/.." && { command -v mise >/dev/null 2>&1 && mise exec -- which jj 2>/dev/null; } || command -v jj)
if [ -z "$jj_real" ] || [ ! -x "$jj_real" ]; then
  echo "SKIP JJ-only identity tests: no jj found (install the pinned jj from mise.toml)"
else
  printf '#!/bin/sh\nexec "%s" "$@"\n' "$jj_real" >"$work/bin/jj"; chmod +x "$work/bin/jj"
  printf '[user]\nname = "Build Test"\nemail = "build-test@example.invalid"\n' >"$work/jj-config.toml"
  export JJ_CONFIG="$work/jj-config.toml"
  jjm=$work/jj-main
  mkdir -p "$jjm/scripts" "$jjm/src-tauri"
  cp "$src_script" "$jjm/scripts/build-dogfood-app.sh"; cp "$repo/src-tauri/tauri.conf.json" "$jjm/src-tauri/"
  printf 'target/\ndist/\n' >"$jjm/.gitignore"
  (cd "$jjm" && jj --quiet git init --no-colocate . && jj --quiet commit -m base) || bad "JJ fixture: init failed"
  jj_build() { (cd "${JJ_DIR:-$jjm}" && env PATH="$work/bin:$PATH" "$@" bash scripts/build-dogfood-app.sh) 2>&1; }
  jjid() { (cd "${JJ_DIR:-$jjm}" && jj log -r @ --no-graph -T "$1"); }
  jjparent() { (cd "${JJ_DIR:-$jjm}" && jj log -r @- --no-graph -T commit_id); }
  jj_dog_sums() { (cd "$jjm/target/dogfood" && sha256sum slashit-ui slashit slashit-ui.frontend 2>&1); }

  # 15. The case the old script got wrong: no .git, so it recorded commit=unknown.
  jjside=$jjm/target/dogfood/slashit-ui.frontend
  out=$(jj_build STUB_BUILD_ID=jj-clean); st=$?
  jj_commit=$(jjparent); jj_change=$(jjid change_id)
  if [ ! -e "$jjm/.git" ] && [ "$st" -eq 0 ] && grep -qx "commit=$jj_commit" "$jjside" && grep -qx "change=$jj_change" "$jjside" &&
    grep -qx 'vcs=jj' "$jjside" && grep -qx 'dirty=no' "$jjside" && ! grep -q unknown "$jjside" &&
    grep -qx "sha256=$(sha256sum "$jjm/target/dogfood/slashit-ui" | cut -d' ' -f1)" "$jjside"; then
    ok "a JJ-only build records the concrete commit id and the change id, never unknown"
  else bad "JJ-only identity is wrong (exit $st)"; printf '%s\n' "$out" >&2; cat "$jjside" >&2; fi

  # 16. Uncommitted work in a JJ working copy is refused before anything is built.
  echo wip >"$jjm/wip.txt"
  before=$(jj_dog_sums)
  out=$(jj_build STUB_BUILD_ID=jj-dirty); st=$?
  if [ "$st" -ne 0 ] && [[ "$out" == *"uncommitted changes"* ]] && [ "$(jj_dog_sums)" = "$before" ]; then
    ok "a JJ working copy with uncommitted files is refused before any build; the promoted pair is unchanged"
  else bad "a dirty JJ working copy was not refused cleanly (exit $st)"; printf '%s\n' "$out" >&2; fi
  rm -f "$jjm/wip.txt"

  # 17. Edits during the build (a new file, then a file created during the build) are refused.
  before=$(jj_dog_sums)
  out=$(jj_build STUB_BUILD_ID=jj-edit STUB_EDIT_DURING=wip.txt); st=$?
  if [ "$st" -ne 0 ] && [[ "$out" == *"changed during the build"* ]] && [ "$(jj_dog_sums)" = "$before" ]; then
    ok "an edit to the tree during a JJ build is refused; the promoted pair is unchanged"
  else bad "JJ edit during the build was not refused (exit $st)"; printf '%s\n' "$out" >&2; fi
  rm -f "$jjm/wip.txt"
  out=$(jj_build STUB_BUILD_ID=jj-touch STUB_TOUCH_DURING=1); st=$?
  if [ "$st" -ne 0 ] && [[ "$out" == *"changed during the build"* ]] && [[ "$out" == *"during-build.txt"* ]] && [ "$(jj_dog_sums)" = "$before" ]; then
    ok "a file created during a JJ build is refused; the promoted pair is unchanged"
  else bad "JJ file created during the build was not refused (exit $st)"; printf '%s\n' "$out" >&2; fi
  rm -f "$jjm/during-build.txt" "$jjm/wip.txt"

  # 18. A linked JJ workspace shares the primary's artifact root and lock.
  (cd "$jjm" && jj --quiet workspace add "$work/jj-linked") || bad "JJ fixture: workspace add failed"
  jjl=$work/jj-linked
  ls "$jjl/scripts/build-dogfood-app.sh" >/dev/null || bad "linked JJ workspace fixture lacks the script"
  rm -f "$hold/started" "$hold/go"
  (cd "$jjm" && env PATH="$work/bin:$PATH" STUB_BUILD_ID=jj-holder STUB_HOLD_DIR="$hold" bash scripts/build-dogfood-app.sh >"$work/jj-first.log" 2>&1) &
  jj_first=$!
  for _ in $(seq 200); do [ -e "$hold/started" ] && break; sleep 0.05; done
  before=$(jj_dog_sums)
  out=$(JJ_DIR=$jjl jj_build STUB_BUILD_ID=jj-second); st=$?
  if [ -e "$hold/started" ] && [ "$st" -ne 0 ] && [[ "$out" == *"another dogfood build"* ]] && [ "$(jj_dog_sums)" = "$before" ]; then
    ok "a build in a linked JJ workspace is refused while the primary's build holds the shared lock"
  else bad "linked JJ workspace did not contend the primary's lock (exit $st)"; printf '%s\n' "$out" >&2; fi
  : >"$hold/go"; wait "$jj_first"
  out=$(JJ_DIR=$jjl jj_build STUB_BUILD_ID=jj-linked); st=$?
  if [ "$st" -eq 0 ] && [ ! -e "$jjl/target/dogfood" ] && grep -qx "commit=$(JJ_DIR=$jjl jjparent)" "$jjside" &&
    grep -qx "change=$(JJ_DIR=$jjl jjid change_id)" "$jjside" &&
    grep -qx "sha256=$(sha256sum "$jjm/target/dogfood/slashit-ui" | cut -d' ' -f1)" "$jjside"; then
    ok "a linked JJ workspace promotes into the primary workspace's target/dogfood with its own commit and change id"
  else bad "linked JJ workspace promoted to the wrong place (exit $st)"; printf '%s\n' "$out" >&2; fi

  # 19. A failing jj means no identity: refused before the build, no fallback.
  mkdir -p "$work/failbin"; printf '#!/bin/sh\necho "jj: simulated failure" >&2; exit 1\n' >"$work/failbin/jj"; chmod +x "$work/failbin/jj"
  before=$(jj_dog_sums)
  out=$(cd "$jjm" && env PATH="$work/failbin:$work/bin:$PATH" bash scripts/build-dogfood-app.sh 2>&1); st=$?
  if [ "$st" -ne 0 ] && [[ "$out" == *"cannot establish the source identity"* ]] && [ "$(jj_dog_sums)" = "$before" ]; then
    ok "a JJ workspace whose jj fails is refused; the promoted pair is unchanged"
  else bad "a failing jj was not refused (exit $st)"; printf '%s\n' "$out" >&2; fi
fi

# 20. Fixture waits are bounded: a hold that is never released ends the build
#     with an error inside its bound, frees the lock, and leaves no process
#     whose command line names the scratch directory.
hold_never=$work/hold-never; mkdir -p "$hold_never"
t0=$SECONDS
out=$(cd "$main" && timeout 30 env PATH="$work/bin:$PATH" STUB_HOLD_DIR="$hold_never" STUB_HOLD_TIMEOUT=2 bash scripts/build-dogfood-app.sh 2>&1); st=$?
elapsed=$((SECONDS - t0))
lock_free=no; flock -n "$main/target/dogfood/.build.lock" true && lock_free=yes
left=$(ps -eo pid,args | grep -e "[/]${work#/}")
if [ -e "$hold_never/started" ] && [ "$st" -ne 0 ] && [ "$st" -ne 124 ] && [ "$elapsed" -lt 20 ] &&
  [[ "$out" == *"hold timed out"* ]] && [ "$lock_free" = yes ] && [ -z "$left" ]; then
  ok "an unreleased hold times out with an error, releases the build lock and leaves no process behind"
else bad "an unreleased hold was not bounded and released (exit $st after ${elapsed}s, lock free: $lock_free)"; printf '%s\n%s\n' "$out" "$left" >&2; fi

echo "$pass passed, $fails failed"
[ "$fails" -eq 0 ]
