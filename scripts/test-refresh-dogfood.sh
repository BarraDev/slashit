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

# Stub supervisor: status output and exit come from the environment, every
# restart is logged, and restart's exit status is configurable.
calls=$work/calls
cat >"$work/sup" <<STUB
#!/usr/bin/env bash
case "\$1" in
  status) cat "\$STUB_STATUS"; exit "\${STUB_STATUS_RC:-0}" ;;
  env) echo "STATE_BASE       $work/state" ;;
  restart) echo restart >>"$calls"; exit "\${STUB_RESTART_RC:-0}" ;;
  *) echo "stub sup: \$*" >&2; exit 2 ;;
esac
STUB
cat >"$work/build" <<STUB
#!/usr/bin/env bash
echo build >>"$calls"
mkdir -p target/dogfood
printf 'binary\n' >target/dogfood/slashit-ui
sha=\$(sha256sum target/dogfood/slashit-ui | cut -d' ' -f1)
printf 'mode=embedded\nsha256=%s\nwasm=x.wasm\ncommit=%s\ndirty=no\n' "\$sha" "\$(git rev-parse HEAD)" >target/dogfood/slashit-ui.frontend
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
if [ "$st" -eq 1 ] && [[ "$out" == *"does not descend"* ]] && [ ! -f "$calls" ]; then ok "a target older than the running commit is refused (no downgrade of state)"
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

echo "== a failed restart is reported as a failure"
status_stopped; reset
out=$(STUB_RESTART_RC=1 run --apply); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"restart did not complete"* ]]; then ok "a restart that fails exits 1 and says so"
else bad "failed restart not reported (exit $st)"; printf '%s\n' "$out" >&2; fi

echo
echo "$pass passed, $fails failed"
[ "$fails" -eq 0 ]
