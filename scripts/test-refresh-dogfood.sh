#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Hermetic test of scripts/refresh-dogfood.sh. A disposable git repository holds
# the script; the build and the supervisor are stubs that log every call, so no
# instance is started, stopped or built. Each case names what it proves.
#
# usage: scripts/test-refresh-dogfood.sh
set -uo pipefail
export LC_ALL=C

src=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/refresh-dogfood.sh
work=$(mktemp -d "${TMPDIR:-/tmp}/refresh-dogfood-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
pass=0
fails=0
ok() { pass=$((pass + 1)); echo "ok   $1"; }
bad() { fails=$((fails + 1)); echo "FAIL $1"; }

repo=$work/repo
mkdir -p "$repo/scripts"
cp "$src" "$repo/scripts/refresh-dogfood.sh"
git -C "$repo" init -q
echo "target/" >"$repo/.gitignore"
git -C "$repo" config user.name "Refresh Test"
git -C "$repo" config user.email refresh-test@example.invalid
cp "$src" "$repo/scripts/refresh-dogfood.sh"; git -C "$repo" add -A; git -C "$repo" commit -qm base
echo one >"$repo/f"; git -C "$repo" add f; git -C "$repo" commit -qm one
old=$(git -C "$repo" rev-parse HEAD)
echo two >"$repo/f"; git -C "$repo" commit -qam two
new=$(git -C "$repo" rev-parse HEAD)
repo_real=$(cd "$repo" && pwd -P)

# Stub supervisor: status output and exit come from the environment, every
# restart is logged, and restart's exit status is configurable.
calls=$work/calls
cat >"$work/sup" <<STUB
#!/usr/bin/env bash
case "\$1" in
  status) cat "\$STUB_STATUS"; exit "\${STUB_STATUS_RC:-0}" ;;
  env) printf 'STATE_BASE       %s\nBINARY           %s\nSTATE_WRITER     %s\n' "$work/state" "\${STUB_BINARY:-$repo/target/dogfood/slashit-ui}" "$work/state/supervisor/state-writer" ;;
  restart) echo restart >>"$calls"; printf '%s\n' "\${SLASHIT_DOGFOOD_EXPECT_COMMIT-unset}" >>"$work/expect"; exit "\${STUB_RESTART_RC:-0}" ;;
  start) echo start >>"$calls"; printf '%s\n' "\${SLASHIT_DOGFOOD_EXPECT_COMMIT-unset}" >>"$work/expect"; exit 0 ;;
  *) echo "stub sup: \$*" >&2; exit 2 ;;
esac
STUB
# Stub build: promotes a binary named after HEAD with its sidecar. STUB_BUILD_RC
# fails it before promotion, STUB_BUILD_COMMIT makes the sidecar name another
# commit, and STUB_BUILD_KILL kills the refresh script right after promotion.
cat >"$work/build" <<STUB
#!/usr/bin/env bash
echo build >>"$calls"
[ -z "\${STUB_BUILD_RC:-}" ] || exit "\$STUB_BUILD_RC"
mkdir -p target/dogfood
printf 'binary %s\n' "\$(git rev-parse HEAD)" >target/dogfood/slashit-ui
sha=\$(sha256sum target/dogfood/slashit-ui | cut -d' ' -f1)
printf 'mode=embedded\nsha256=%s\nwasm=x.wasm\ncommit=%s\ndirty=no\n' "\$sha" "\${STUB_BUILD_COMMIT:-\$(git rev-parse HEAD)}" >target/dogfood/slashit-ui.frontend
if [ -n "\${STUB_BUILD_KILL:-}" ]; then
  # The victim is the refresh script's own process: the outermost ancestor that
  # runs scripts/refresh-dogfood.sh in this fixture. The refresh script's
  # subshells share its argv on some bash builds (bash 5.1 keeps it), so the
  # nearest match can be a subshell; the walk therefore keeps the outermost one.
  victim=
  p=\$PPID
  while [ "\$p" -gt 1 ]; do
    argv=\$(tr '\\0' ' ' </proc/\$p/cmdline 2>/dev/null)
    if [ "\$(readlink /proc/\$p/cwd 2>/dev/null)" = "$repo_real" ] && [[ "\$argv" == "bash scripts/refresh-dogfood.sh "* ]]; then
      victim=\$p
    fi
    p=\$(awk '{ print \$4 }' /proc/\$p/stat)
  done
  if [ -n "\$victim" ]; then kill -9 "\$victim"; exit 0; fi
fi
STUB
chmod +x "$work/sup" "$work/build"
export SLASHIT_REFRESH_SUPERVISOR=$work/sup SLASHIT_REFRESH_BUILD=$work/build
export STUB_STATUS="$work/status"

status_running() { printf 'instance:       running (pid 1, mode gui, version x)\ncommit:         %s\n' "$1" >"$work/status"; }
status_stopped() { printf 'instance:       stopped\n' >"$work/status"; }
reset() { rm -f "$calls"; }
run() { (cd "$repo" && bash scripts/refresh-dogfood.sh "$@") 2>&1; }
calls_have() { [ -f "$calls" ] && grep -qx "$1" "$calls"; }

echo "== check is read-only and reports the state"
status_running "$old"; reset
out=$(run --check); st=$?
if [ "$st" -eq 0 ] && [[ "$out" == *"forward only:     yes"* ]] && [ ! -f "$calls" ]; then
  ok "--check on a clean, forward target exits 0 and calls neither build nor restart"
else bad "--check did not report a clean forward refresh (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== refusals before any change"
echo dirty >>"$repo/f"; reset
out=$(run --apply); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"uncommitted changes"* ]] && [ ! -f "$calls" ]; then ok "--apply refuses a dirty tree before building or restarting"
else bad "dirty tree not refused cleanly (exit $st)"; printf '%s\n' "$out" >&2; fi
git -C "$repo" checkout -q -- f

status_running "$new"; reset
out=$(run --apply "$old"); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"does not provably descend"* ]] && [ ! -f "$calls" ]; then ok "a target older than the running commit is refused (no downgrade of state)"
else bad "a downgrade target was not refused (exit $st)"; printf '%s\n' "$out" >&2; fi

printf 'instance:       unverifiable\n' >"$work/status"; reset
out=$(STUB_STATUS_RC=3 run --apply); st=$?
if [ "$st" -eq 3 ] && [ ! -f "$calls" ]; then ok "an unverifiable running instance exits 3 with no build and no restart"
else bad "unverifiable instance not refused (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== apply: skip the build when the promoted build is this commit, restart once"
status_running "$old"; reset
run --apply >/dev/null 2>&1
out=$(run --apply); st=$?
# The first run built and restarted; the second must see a verified build of
# the target and build nothing more.
rm -f "$calls"
out=$(run --apply); st=$?
if [ "$st" -eq 0 ] && ! calls_have build && calls_have restart; then ok "a verified promoted build of the target is reused: no rebuild, one restart"
else bad "verified build was not reused (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== apply: an unverified or stale build is rebuilt before the restart"
echo tamper >>"$repo/target/dogfood/slashit-ui"; reset
out=$(run --apply); st=$?
if [ "$st" -eq 0 ] && calls_have build && calls_have restart; then ok "a binary that no longer matches its sidecar is rebuilt, then restarted"
else bad "tampered build was not rebuilt (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== apply: nothing to do when the running instance already runs the target"
status_running "$new"; reset
out=$(run --apply); st=$?
if [ "$st" -eq 0 ] && [[ "$out" == *"nothing to do"* ]] && [ ! -f "$calls" ]; then ok "a running target is left alone (no build, no restart)"
else bad "running target was touched (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== an unknown running commit is refused, not restarted over"
status_running "unknown"; reset
out=$(run --apply); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"does not provably descend"* ]] && [ ! -f "$calls" ]; then ok "a running instance with no comparable commit is refused"
else bad "unknown running commit was not refused (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== a stopped instance is started, not restarted"
status_stopped; reset
out=$(run --apply); st=$?
if [ "$st" -eq 0 ] && calls_have start && ! calls_have restart; then ok "a stopped instance is started"
else bad "stopped instance was not started (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== a JJ-only checkout reports why it cannot refresh"
jjonly=$work/jjonly
mkdir -p "$jjonly/scripts"; cp "$src" "$jjonly/scripts/refresh-dogfood.sh"
out=$(cd "$jjonly" && bash scripts/refresh-dogfood.sh --check 2>&1); st=$?
if [ "$st" -eq 2 ] && [[ "$out" == *"not a Git repository"* ]]; then ok "a checkout without .git is named as such"
else bad "non-Git checkout error unclear (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== a failed restart is reported as a failure"
status_running "$old"; reset
out=$(STUB_RESTART_RC=1 run --apply); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"restart did not complete"* ]]; then ok "a restart that fails exits 1 and says so"
else bad "failed restart not reported (exit $st)"; printf '%s\n' "$out" >&2; fi

sums() { sha256sum "$repo/target/dogfood/slashit-ui" "$repo/target/dogfood/slashit-ui.frontend" 2>/dev/null; }
writer=$work/state/supervisor/state-writer
mkdir -p "$work/state/supervisor"

echo "== a stopped instance whose state a newer build wrote refuses an older target"
status_stopped; reset
printf 'commit=%s\nsha256=x\n' "$new" >"$writer"; wsum=$(sha256sum <"$writer"); before=$(sums)
git -C "$repo" checkout -q "$old"
out=$(run --apply); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"last build that wrote the state"* ]] && [ ! -f "$calls" ] &&
  [ "$(sha256sum <"$writer")" = "$wsum" ] && [ "$before" = "$(sums)" ]; then
  ok "stopped downgrade: refused from the state-writer record, with no build, no start, and the artifact and record unchanged"
else bad "a stopped downgrade was not refused (exit $st)"; printf '%s\n' "$out" >&2; fi
out=$(run --check); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"forward=NO"* ]]; then ok "--check reports the state-writer downgrade and exits 1"
else bad "--check did not report the state-writer downgrade (exit $st)"; printf '%s\n' "$out" >&2; fi
git -C "$repo" checkout -q "$new"; rm -f "$writer"

echo "== the build runs only when the checkout HEAD is the target"
status_stopped; reset
git -C "$repo" checkout -q "$old"
echo tamper >>"$repo/target/dogfood/slashit-ui"; before=$(sums)
out=$(run --apply "$new"); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"check out the target first"* ]] && [ ! -f "$calls" ] && [ "$before" = "$(sums)" ]; then
  ok "a target that is not the checkout HEAD is refused before any build or start; the artifact is unchanged"
else bad "a wrong-checkout build was not refused (exit $st)"; printf '%s\n' "$out" >&2; fi
git -C "$repo" checkout -q "$new"

echo "== the promoted build must name the target before anything starts"
status_stopped; reset
out=$(STUB_BUILD_COMMIT=$old run --apply); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"not a verified clean build"* ]] && calls_have build && ! calls_have start && ! calls_have restart &&
  grep -qx "commit=$old" "$repo/target/dogfood/slashit-ui.frontend"; then
  ok "a build whose sidecar names another commit is not started"
else bad "a mismatched promoted build was started or not refused (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== a failed build starts nothing and leaves the promoted build"
run --apply >/dev/null 2>&1; before=$(sums); status_running "$old"; reset
git -C "$repo" commit -q --allow-empty -m three; three=$(git -C "$repo" rev-parse HEAD)
out=$(STUB_BUILD_RC=1 run --apply); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"build failed"* ]] && calls_have build && ! calls_have restart && [ "$before" = "$(sums)" ]; then
  ok "a failed build or promotion exits 1 with no restart; the previous artifact checksums are unchanged"
else bad "a failed build was not contained (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== an update interrupted after the promotion is resumed, not repeated"
status_running "$old"; reset
out=$(STUB_BUILD_KILL=1 run --apply); st=$?
if [ "$st" -eq 137 ] && calls_have build && ! calls_have restart && grep -qx "commit=$three" "$repo/target/dogfood/slashit-ui.frontend"; then
  ok "a refresh killed after the promotion restarted nothing"
else bad "the interrupted refresh did not stop where it was killed (exit $st)"; printf '%s\n' "$out" >&2; fi
reset; out=$(run --apply); st=$?
if [ "$st" -eq 0 ] && ! calls_have build && calls_have restart; then
  ok "the next refresh reuses the verified promoted build and restarts once"
else bad "the resumed refresh did not reuse the promoted build (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== a linked worktree and the main checkout resolve the same artifact"
d=$work/two
mkdir -p "$d/main/scripts"
cp "$src" "$d/main/scripts/refresh-dogfood.sh"
cp "$(dirname "$src")/supervise-dogfood.sh" "$d/main/scripts/supervise-dogfood.sh"
git -C "$d/main" init -q
git -C "$d/main" config user.name "Refresh Test"; git -C "$d/main" config user.email refresh-test@example.invalid
git -C "$d/main" add -A; git -C "$d/main" commit -qm base
git -C "$d/main" worktree add -q "$d/linked" -b linked 2>/dev/null
# The real supervisor, confined to a temporary state and runtime and given no CLI.
denv=(env -u SLASHIT_REFRESH_SUPERVISOR -u SLASHIT_DOGFOOD_BINARY SLASHIT_DOGFOOD_STATE="$d/state" SLASHIT_DOGFOOD_RUNTIME="$d/run"
  SLASHIT_DOGFOOD_CLI= XDG_CONFIG_HOME= XDG_DATA_HOME= XDG_CACHE_HOME= XDG_STATE_HOME= XDG_RUNTIME_DIR= GH_CONFIG_DIR=)
art_main=$(cd "$d/main" && "${denv[@]}" bash scripts/refresh-dogfood.sh --check 2>&1 | awk '/^artifact:/ { print $2 }')
art_linked=$(cd "$d/linked" && "${denv[@]}" bash scripts/refresh-dogfood.sh --check 2>&1 | awk '/^artifact:/ { print $2 }')
sup_main=$("${denv[@]}" bash "$d/main/scripts/supervise-dogfood.sh" env | awk '$1 == "BINARY" { print $2 }')
sup_linked=$("${denv[@]}" bash "$d/linked/scripts/supervise-dogfood.sh" env | awk '$1 == "BINARY" { print $2 }')
state_seen=$("${denv[@]}" bash "$d/linked/scripts/supervise-dogfood.sh" env | awk '$1 == "STATE_BASE" { print $2 }')
if [ "$art_main" = "$d/main/target/dogfood/slashit-ui" ] && [ "$art_linked" = "$art_main" ] &&
  [ "$sup_main" = "$art_main" ] && [ "$sup_linked" = "$art_main" ] && [ "$state_seen" = "$d/state" ]; then
  ok "refresh and the supervisor resolve one artifact path from the main checkout and from a linked worktree (state confined to the test)"
else bad "artifact paths differ: refresh main '$art_main', linked '$art_linked'; supervisor main '$sup_main', linked '$sup_linked'; state '$state_seen'"; fi

echo "== paths with spaces are read whole from the supervisor's env"
spaced="$work/with space"
mkdir -p "$spaced/state/supervisor/backup/20260101-000000" "$spaced/bin"
cat >"$work/sup-spaces" <<STUB
#!/usr/bin/env bash
case "\$1" in
  status) cat "\$STUB_STATUS"; exit 0 ;;
  env) printf 'STATE_BASE       %s\nBINARY           %s\nSTATE_WRITER     %s\n' "$spaced/state" "$spaced/bin/slashit-ui" "$spaced/state/supervisor/state-writer" ;;
  *) exit 2 ;;
esac
STUB
chmod +x "$work/sup-spaces"
printf 'commit=%s\nsha256=x\n' "$old" >"$spaced/state/supervisor/state-writer"
: >"$spaced/state/supervisor/backup/20260101-000000/complete"
status_stopped; reset
out=$(export SLASHIT_REFRESH_SUPERVISOR=$work/sup-spaces; run --check); st=$?
if [[ "$out" == *"artifact:         $spaced/bin/slashit-ui"* ]] &&
  [[ "$out" == *"latest backup:    $spaced/state/supervisor/backup/20260101-000000"* ]] &&
  [[ "$out" == *"state writer:     ${old:0:12} forward=yes"* ]]; then
  ok "artifact, state base and state-writer paths containing spaces are read whole"
else bad "a path with spaces was truncated (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== an incomplete backup is never reported as the latest recovery point"
bdir=$spaced/state/supervisor/backup
mkdir -p "$bdir/20260102-000000"; printf 'partial\n' >"$bdir/20260102-000000/manifest.sha256"
out=$(export SLASHIT_REFRESH_SUPERVISOR=$work/sup-spaces; run --check)
if [[ "$out" == *"latest backup:    $bdir/20260101-000000"* ]] &&
  [[ "$out" == *"newest unmarked:  $bdir/20260102-000000 (not a verified recovery point"* ]]; then
  ok "a newer unmarked directory is listed apart; the latest complete backup is still reported"
else bad "an unmarked newer backup was reported as the latest recovery point"; printf '%s\n' "$out" >&2; fi
rm -f "$bdir/20260101-000000/complete"
out=$(export SLASHIT_REFRESH_SUPERVISOR=$work/sup-spaces; run --check)
if [[ "$out" == *"latest backup:    none verified (no backup is marked complete)"* ]]; then
  ok "with no backup marked complete, the report says none is verified rather than naming a directory"
else bad "a directory without the marker was reported as verified"; printf '%s\n' "$out" >&2; fi
rm -rf "$bdir/20260102-000000"; : >"$bdir/20260101-000000/complete"

echo "== an artifact override naming an older build is refused before anything is started"
old_art=$work/old-artifact; mkdir -p "$old_art"
printf 'older binary\n' >"$old_art/slashit-ui"
osha=$(sha256sum "$old_art/slashit-ui" | cut -d' ' -f1)
printf 'mode=embedded\nsha256=%s\nwasm=x.wasm\ncommit=%s\ndirty=no\n' "$osha" "$old" >"$old_art/slashit-ui.frontend"
status_stopped; reset
saved_head=$(git -C "$repo" rev-parse HEAD); git -C "$repo" checkout -q --detach "$new"
out=$(export STUB_BINARY=$old_art/slashit-ui; run --apply "$new"); st=$?
git -C "$repo" checkout -q --detach "$saved_head"
if [ "$st" -eq 1 ] && [[ "$out" == *"is not a verified clean build of ${new:0:12}"* ]] && ! grep -qE '^(start|restart)$' "$calls" 2>/dev/null; then
  ok "an override selecting an older artifact is refused; no start or restart reaches it"
else bad "an older artifact selected by override was not refused before start (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== a status that exits 1 is not a verified running instance"
status_running "$old"; reset
out=$(export STUB_STATUS_RC=1; run --apply "$new"); st=$?
if [ "$st" -ne 0 ] && [ ! -f "$calls" ]; then ok "a stale launch record (status exit 1) names a commit, but refresh does not restart over it"
else bad "a status exit 1 with a stale commit was accepted (exit $st)"; printf '%s\n' "$out" >&2; fi
head_now=$(git -C "$repo" rev-parse HEAD)
status_running "$head_now"; reset
out=$(export STUB_STATUS_RC=1; run --apply); st=$?
if [ "$st" -ne 0 ] && [[ "$out" != *"nothing to do"* ]] && [ ! -f "$calls" ]; then ok "a status exit 1 is never reported as the target already running"
else bad "a status exit 1 with the target commit reported success (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== the target is handed to the supervisor as its expected commit"
status_stopped; reset; rm -f "$work/expect"
out=$(run --apply); st=$?
if [ "$st" -eq 0 ] && [ "$(cat "$work/expect" 2>/dev/null)" = "$(git -C "$repo" rev-parse HEAD)" ]; then ok "start receives the requested target as SLASHIT_DOGFOOD_EXPECT_COMMIT"
else bad "the supervisor did not receive the expected commit (exit $st, got '$(cat "$work/expect" 2>/dev/null)')"; printf '%s\n' "$out" >&2; fi

echo "== preflight names the missing state-writer precondition for existing state"
status_stopped; reset
mkdir -p "$work/state/config/slashit-app" && echo "settings" >"$work/state/config/slashit-app/settings.toml"
rm -f "$work/state/supervisor/state-writer"
out=$(run --check); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"apply blocked:"*"no state-writer record"* ]] && [[ "$out" == *"state-writer prov: missing"* ]] && [ ! -f "$calls" ]; then
  ok "--check exits 1 and names the missing state-writer when state exists and no record does"
else bad "--check did not block on the missing state-writer (exit $st)"; printf '%s\n' "$out" >&2; fi
out=$(run --apply); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"refusing: no state-writer record"* ]] && [ ! -f "$calls" ]; then
  ok "--apply refuses the same precondition before any build, stop or start"
else bad "--apply did not refuse the missing state-writer (exit $st)"; printf '%s\n' "$out" >&2; fi
echo "== a state-writer with no accepted provenance blocks --check and --apply alike"
printf 'commit=%s\nsha256=0\n' "$new" >"$work/state/supervisor/state-writer"
status_stopped; reset
out=$(run --check); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"apply blocked:"*"names no accepted provenance"* ]] && [ ! -f "$calls" ]; then
  ok "--check exits 1 for an unmarked state-writer, the same case --apply refuses"
else bad "--check accepted an unmarked state-writer (exit $st)"; printf '%s\n' "$out" >&2; fi
out=$(run --apply); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"refusing: the state-writer record names no accepted provenance"* ]] && [ ! -f "$calls" ]; then
  ok "--apply refuses the unmarked state-writer before any build, stop or start"
else bad "--apply did not refuse the unmarked state-writer (exit $st)"; printf '%s\n' "$out" >&2; fi
mkdir -p "$work/state/supervisor"
printf 'commit=%s\nsha256=0\nprovenance=legacy-attested\n' "$new" >"$work/state/supervisor/state-writer"
out=$(run --check); st=$?
if [ "$st" -eq 0 ] && [[ "$out" == *"state-writer prov: legacy-attested"* ]] && [[ "$out" != *"apply blocked"* ]] && [ ! -f "$calls" ]; then
  ok "a legacy-attested writer that the target descends from is reported and is not a blocker"
else bad "a legacy-attested writer blocked a forward target (exit $st)"; printf '%s\n' "$out" >&2; fi
rm -rf "$work/state"

echo "== a running image whose launch copy was removed is identified by its bytes, not its path"
dead_copy=$work/launch-copy-sleep
cp -p "$(command -v sleep)" "$dead_copy"
"$dead_copy" 300 &
live_pid=$!
# Unlink only once the child has exec'd the copy (bounded wait), so /proc names this image.
exec_ok=no
for _ in $(seq 50); do
  [ "$(readlink -f "/proc/$live_pid/exe" 2>/dev/null)" = "$(readlink -f "$dead_copy")" ] && { exec_ok=yes; break; }
  sleep 0.1
done
[ "$exec_ok" = yes ] || bad "the fixture process did not execute its copy within 5 seconds"
rm -f "$dead_copy"
printf 'instance:       running (pid %s, mode gui, version x)\ncommit:         %s\nrecord:         started 2026-11-17T06:50:39Z, pid %s\n' "$live_pid" "$new" "$live_pid" >"$work/status"
want_sha=$(sha256sum "/proc/$live_pid/exe" | cut -d' ' -f1)
mkdir -p "$work/state/supervisor"
printf 'commit=%s\nsha256=%s\nprovenance=verified-sidecar\n' "$new" "$want_sha" >"$work/state/supervisor/state-writer"
out=$(run --check); st=$?
kill "$live_pid" 2>/dev/null; wait "$live_pid" 2>/dev/null
if [[ "$out" == *"running exe sha:  $want_sha (state-writer names it: yes)"* ]] && [[ "$out" != *"artifact-root mismatch"* ]] && [[ "$out" != *"apply blocked"* ]] && [ ! -f "$calls" ]; then
  ok "a deleted launch copy is still identified by sha256 through /proc and matches the writer"
else bad "a running image with a deleted path was misreported (exit $st)"; printf '%s\n' "$out" >&2; fi
rm -rf "$work/state"

# The supervisor's status for a verified instance whose frontend predates the
# promoted build: exit 1, with the process, endpoint and environment proven.
# The record line carries no pid, so the executable-provenance check (which needs
# a readable /proc entry and a verified state writer) is not part of these cases.
status_stale() { # commit, then optional extra lines that must refuse
  printf 'instance:       running (pid 1, mode gui, version x)\nendpoint:       unix:/x\nbinary:         %s\ncommit:         %s\nrecord:         started 2026-01-01T00:00:00Z\ncontrol channel: answering on the declared endpoint\nfrontend:       STALE INSTANCE — the binary was rebuilt after this instance started\nenvironment:    verified (the live process carries the declared values)\n%s\n' "$repo/target/dogfood/slashit-ui" "$1" "${2:-}" >"$work/status"
}

echo "== a verified instance with a stale frontend is restarted to the target"
git -C "$repo" checkout -q "$new"; status_stale "$old"; reset; rm -f "$work/expect"
out=$(export STUB_STATUS_RC=1; run --apply); st=$?
if [ "$st" -eq 0 ] && calls_have restart && [ "$(cat "$work/expect" 2>/dev/null)" = "$new" ]; then ok "a stale-frontend instance with a provably older commit is restarted, and the supervisor receives the target"
else bad "a verified stale-frontend instance was not restarted (exit $st)"; printf '%s\n' "$out" >&2; fi
out=$(export STUB_STATUS_RC=1; run --check); st=$?
if [ "$st" -eq 0 ] && [[ "$out" == *"frontend:         stale"* ]] && [[ "$out" != *"supervisor status: exit"* ]]; then ok "--check reports the stale frontend as a verified running state and exits 0"
else bad "--check did not report the stale frontend (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== a stale-frontend status is refused when any identity proof is missing"
status_stale "$old" "record:         STALE — it names pid 9, the endpoint is served by pid 1"; reset
out=$(export STUB_STATUS_RC=1; run --apply); st=$?
if [ "$st" -ne 0 ] && [ ! -f "$calls" ]; then ok "a record that names another pid is never a stale-frontend restart target"
else bad "a record mismatch was accepted as a stale frontend (exit $st)"; printf '%s\n' "$out" >&2; fi
status_stale "$old"; sed -i 's/^environment: .*/environment:    MISMATCH (a declared value differs)/' "$work/status"; reset
out=$(export STUB_STATUS_RC=4; run --apply); st=$?
if [ "$st" -ne 0 ] && [ ! -f "$calls" ]; then ok "an environment mismatch with a stale frontend is refused"
else bad "an environment mismatch was accepted (exit $st)"; printf '%s\n' "$out" >&2; fi
status_stale "$old"; sed -i "s|^control channel: .*|control channel: answering on 'unix:/other', which is NOT the declared endpoint|" "$work/status"; reset
out=$(export STUB_STATUS_RC=1; run --apply); st=$?
if [ "$st" -ne 0 ] && [ ! -f "$calls" ]; then ok "an instance answering on another endpoint is refused even with a stale frontend"
else bad "a wrong endpoint was accepted (exit $st)"; printf '%s\n' "$out" >&2; fi
status_stale "$old" ; sed -i 's/^frontend: .*/frontend:       UNAVAILABLE — this is a development build, which loads an unanswered server/' "$work/status"; reset
out=$(export STUB_STATUS_RC=1; run --apply); st=$?
if [ "$st" -ne 0 ] && [ ! -f "$calls" ]; then ok "a degraded frontend that is not STALE is still unverifiable"
else bad "a degraded frontend was accepted as stale (exit $st)"; printf '%s\n' "$out" >&2; fi

echo "== a stale-frontend instance still needs the ancestry gate"
git -C "$repo" checkout -q "$new"; status_stale "$new"; reset
out=$(export STUB_STATUS_RC=1; run --apply "$old"); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"does not provably descend"* ]] && [ ! -f "$calls" ]; then ok "a stale-frontend instance is not downgraded to an older target"
else bad "a stale-frontend downgrade was not refused (exit $st)"; printf '%s\n' "$out" >&2; fi

echo
echo "$pass passed, $fails failed"
[ "$fails" -eq 0 ]
