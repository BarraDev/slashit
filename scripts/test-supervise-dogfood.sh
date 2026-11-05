#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Tests scripts/supervise-dogfood.sh against a stub slashit-ui binary and a
# stub slashit CLI inside a throwaway directory. Every isolated environment
# value is redirected into the throwaway, so the test never touches the
# runner's real ~/.local/state tree, /run/user tree, or GUI, and never starts
# a real instance. Each case names what it proves; a failure prints the
# supervisor's own output.
#
# usage: scripts/test-supervise-dogfood.sh
set -uo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
sup="$root/scripts/supervise-dogfood.sh"
[ -f "$sup" ] || { echo "FAIL $sup is missing" >&2; exit 1; }

work=$(mktemp -d "${TMPDIR:-/tmp}/supervise-dogfood-test.XXXXXX")
trap 'cleanup_instances; rm -rf "$work"' EXIT

passed=0
failed=0

ok() {
  echo "ok   $*"
  passed=$((passed + 1))
}

bad() {
  echo "FAIL $*"
  failed=$((failed + 1))
}

state="$work/state"
runtime="$work/run"
stub_dir="$work/stub"
bin_dir="$state/data/workspaces/slashit-dogfood-git/target/debug"
socket="$runtime/slashit-app/slashit.sock"
mkdir -p "$bin_dir" "$state/config/slashit-app" "$state/data/slashit-app/logs" "$stub_dir" "$runtime"

export SLASHIT_TEST_STUB_DIR="$stub_dir"
export SLASHIT_TEST_SOCKET="$socket"

# --- stubs -------------------------------------------------------------------

# Stub of the slashit-ui binary. Answers --version, records the environment it
# was exec'd with (the never-fall-back check reads that file), then runs until
# the stub CLI writes the quit flag, or until it is SIGTERMed externally. With
# SLASHIT_TEST_STUB_ROLE=noquit it ignores the flag, exercising the --force
# escalation.
cat >"$bin_dir/slashit-ui" <<'STUB'
#!/usr/bin/env bash
stub_dir=$SLASHIT_TEST_STUB_DIR
if [ "${1:-}" = "--version" ]; then
  echo "slashit-ui 9.9.9-stub"
  exit 0
fi
tr '\0' '\n' </proc/self/environ >"$stub_dir/environ"
echo "$$" >"$stub_dir/pid"
rm -f "$stub_dir/quit"
touch "$SLASHIT_TEST_SOCKET"
if [ -n "${SLASHIT_TEST_STARTUP_TOUCH:-}" ]; then
  printf 'boot write\n' >"$SLASHIT_TEST_STARTUP_TOUCH"
fi
if [ "${SLASHIT_TEST_STUB_ROLE:-quit}" = quit ]; then
  while [ ! -f "$stub_dir/quit" ]; do
    sleep 0.05
  done
  rm -f "$stub_dir/quit" "$stub_dir/pid" "$SLASHIT_TEST_SOCKET"
else
  sleep 300
fi
exit 0
STUB
chmod +x "$bin_dir/slashit-ui"

# Stub of the `slashit` CLI: answers ping while the stub instance is up, sends
# its quit flag otherwise, in the same JSON shape the real CLI prints.
cat >"$stub_dir/slashit" <<'STUB'
#!/usr/bin/env bash
stub_dir=$SLASHIT_TEST_STUB_DIR
last=
for a in "$@"; do
  case $a in --endpoint | --json) ;; *) last=$a ;; esac
done
case $last in
  ping)
    if [ -f "$stub_dir/pid" ]; then
      printf '{\n  "mode": "gui",\n  "version": "9.9.9-stub",\n  "protocol_version": 1,\n  "pid": %s,\n  "endpoint": "unix:%s"\n}\n' \
        "$(cat "$stub_dir/pid")" "$SLASHIT_TEST_SOCKET"
      exit 0
    fi
    exit 1
    ;;
  quit)
    if [ -f "$stub_dir/pid" ]; then
      touch "$stub_dir/quit"
      echo "Shutting down."
      exit 0
    fi
    echo "Error: nothing is answering" >&2
    exit 1
    ;;
  *)
    echo "usage error" >&2
    exit 2
    ;;
esac
STUB
chmod +x "$stub_dir/slashit"

# A real git checkout wrapping the binary, for the provenance commit.
git init -q "$state/data/workspaces/slashit-dogfood-git"
git -C "$state/data/workspaces/slashit-dogfood-git" config user.name "Supervise Test"
git -C "$state/data/workspaces/slashit-dogfood-git" config user.email supervise-test@example.invalid
printf test >"$state/data/workspaces/slashit-dogfood-git/provenance.txt"
git -C "$state/data/workspaces/slashit-dogfood-git" add provenance.txt
git -C "$state/data/workspaces/slashit-dogfood-git" commit -q -m provenance
commit_id=$(git -C "$state/data/workspaces/slashit-dogfood-git" rev-parse HEAD)

# Application state that a restart must preserve.
printf 'settings v1\n' >"$state/config/slashit-app/settings.toml"
printf 'session 7\n' >"$state/data/slashit-app/board.toml"
printf 'transcript\n' >"$state/data/slashit-app/logs/agent.txt"

BASEENV=(
  SLASHIT_DOGFOOD_STATE="$state"
  SLASHIT_DOGFOOD_RUNTIME="$runtime"
  SLASHIT_DOGFOOD_BINARY="$bin_dir/slashit-ui"
  SLASHIT_DOGFOOD_CLI="$stub_dir/slashit"
  SLASHIT_DOGFOOD_START_TIMEOUT=10
  SLASHIT_DOGFOOD_STOP_TIMEOUT=8
  SLASHIT_DOGFOOD_FORCE_TIMEOUT=5
)

# sv [EXTRA ASSIGNMENTS...] -- args: runs the supervisor in the throwaway.
sv() {
  local args=() assignment=1 extra=()
  for a in "$@"; do
    if [ "$assignment" = 1 ]; then
      case $a in
        --) assignment=0 ;;
        *) extra+=("$a") ;;
      esac
    else
      args+=("$a")
    fi
  done
  env "${BASEENV[@]}" "${extra[@]}" bash "$sup" "${args[@]}"
}

cleanup_instances() {
  if [ -f "$stub_dir/pid" ]; then
    kill -9 "$(cat "$stub_dir/pid")" 2>/dev/null || true
  fi
  rm -f "$stub_dir/pid" "$stub_dir/quit" "$stub_dir/environ" "$socket"
}

stub_pid() {
  cat "$stub_dir/pid" 2>/dev/null || true
}

expect() { # want <pass|fail|input> <label> <supervisor-output> <status>
  local want=$1 label=$2 output=$3 status=$4
  local first=${output%%$'\n'*}
  case $want in
    pass)
      if [ "$status" -eq 0 ]; then ok "$label"; else
        bad "$label: expected success, exited $status"; printf '%s\n' "$output" >&2
      fi
      ;;
    fail)
      if [ "$status" -eq 1 ]; then
        case $first in
          "$sup: refusing"*) ok "$label (refused: ${first#"$sup: refusing: "})";;
          "$sup:"*) ok "$label (failed: ${first#"$sup: "})";;
          *) bad "$label: expected a refusal, got: $first"; printf '%s\n' "$output" >&2 ;;
        esac
      else
        bad "$label: expected refusal (exit 1), exited $status"; printf '%s\n' "$output" >&2
      fi
      ;;
    input)
      if [ "$status" -eq 2 ]; then ok "$label (usage error: $first)"; else
        bad "$label: expected usage error (exit 2), exited $status"; printf '%s\n' "$output" >&2
      fi
      ;;
  esac
}

settings_digest() {
  printf '%s' "$(cat "$state/config/slashit-app/settings.toml" 2>/dev/null)"
}

# --- cases -------------------------------------------------------------------

echo "== usage"
out=$(sv 2>&1); expect input "no arguments is a usage error" "$out" $?
out=$(sv bogus 2>&1); expect input "an unknown verb is a usage error" "$out" $?
out=$(sv env extra 2>&1); expect input "unexpected arguments are a usage error" "$out" $?
out=$(sv start --force 2>&1); expect input "start takes no --force" "$out" $?

echo "== env"
out=$(sv env 2>&1)
if [ "$out" = "$(printf 'STATE_BASE       %s\nXDG_CONFIG_HOME  %s/config\nXDG_DATA_HOME    %s/data\nXDG_CACHE_HOME   %s/cache\nXDG_STATE_HOME   %s\nXDG_RUNTIME_DIR  %s\nGH_CONFIG_DIR    %s\nBINARY           %s\nCLI              %s\nSOCKET           %s\nENDPOINT         unix:%s\n' \
  "$state" "$state" "$state" "$state" "$state" "$runtime" "$HOME/.config/gh" "$bin_dir/slashit-ui" "$stub_dir/slashit" "$socket" "$socket")" ]; then
  ok "env prints the whole declared environment"
else
  bad "env output does not match the declaration"; printf '%s\n' "$out" >&2
fi

echo "== start and status"
out=$(sv start 2>&1); expect pass "the first start succeeds" "$out" $?
if [[ "$out" == *"started pid "* ]]; then ok "start reports the spawned pid"; else bad "start gave no pid: $out"; fi

live_pid=$(stub_pid)
out=$(sv status 2>&1)
if [[ "$out" == *"running (pid $live_pid"* ]]; then ok "status reports the live pid"; else bad "status lost the pid: $out"; fi
if [[ "$out" == *"binary:         $bin_dir/slashit-ui"* ]]; then ok "status reports the binary path"; else bad "status lost the binary: $out"; fi
if [[ "$out" == *"commit:         $commit_id"* ]]; then ok "status reports the provenance commit"; else bad "status lost the commit: $out"; fi
if [[ "$out" == *"endpoint:       unix:$socket"* ]]; then ok "status reports the endpoint"; else bad "status lost the endpoint: $out"; fi

out=$(sv verify 2>&1); expect pass "verify confirms the endpoint and environment" "$out" $?
if [[ "$out" == *"endpoint:       unix:$socket — verified"* ]]; then ok "verify names the verified endpoint"; else bad "verify lost the endpoint: $out"; fi

out=$(sv start 2>&1); expect fail "a second start is refused while the instance answers" "$out" $?

echo "== the declared environment reached the process"
env_ok=1
while IFS=: read -r name want; do
  grep -Fxq "$name=$want" "$stub_dir/environ" || { env_ok=0; break; }
done <<EOF
XDG_CONFIG_HOME:$state/config
XDG_DATA_HOME:$state/data
XDG_CACHE_HOME:$state/cache
XDG_STATE_HOME:$state
XDG_RUNTIME_DIR:$runtime
GH_CONFIG_DIR:$HOME/.config/gh
EOF
if [ "$env_ok" = 1 ]; then
  ok "the process carries every declared XDG_* and GH_CONFIG_DIR value"
else
  bad "the process environment diverges from the declaration"
  printf '%s\n' "$(cat "$stub_dir/environ" 2>/dev/null)" >&2
fi
if ! grep -q "^XDG_CONFIG_HOME=$HOME/.config$" "$stub_dir/environ" &&
  ! grep -q "^XDG_DATA_HOME=$HOME/.local/share$" "$stub_dir/environ" &&
  ! grep -q "^XDG_CACHE_HOME=$HOME/.cache$" "$stub_dir/environ" &&
  ! grep -q "^XDG_STATE_HOME=$HOME/.local/state$" "$stub_dir/environ" &&
  ! grep -q "^XDG_RUNTIME_DIR=/run/user/$(id -u)$" "$stub_dir/environ"; then
  ok "no default XDG value derived from HOME is present"
else
  bad "a default XDG value derived from HOME reached the instance"
  printf '%s\n' "$(cat "$stub_dir/environ" 2>/dev/null)" >&2
fi

echo "== never fall back to default XDG directories"
out=$(sv -- SLASHIT_DOGFOOD_RUNTIME="/run/user/$(id -u)" start 2>&1)
expect fail "the default runtime directory is refused" "$out" $?
out=$(sv -- SLASHIT_DOGFOOD_STATE="$HOME/.local/state" start 2>&1)
expect fail "the default state home as the state base is refused" "$out" $?

echo "== provenance gate"
: >"$work/not-a-slashit-build"
out=$(sv -- SLASHIT_DOGFOOD_BINARY="$work/not-a-slashit-build" start 2>&1)
expect fail "a missing binary is refused" "$out" $?
printf '#!/usr/bin/env bash\nexit 3\n' >"$work/no-version"
chmod +x "$work/no-version"
out=$(sv -- SLASHIT_DOGFOOD_BINARY="$work/no-version" start 2>&1)
expect fail "a binary without --version is refused" "$out" $?

echo "== graceful stop"
out=$(sv stop 2>&1); expect pass "graceful stop succeeds" "$out" $?
out=$(sv status 2>&1)
if [[ "$out" == *"instance:       stopped"* ]]; then ok "status reports stopped after the quit"; else bad "status is not 'stopped': $out"; fi
if ! kill -0 "$live_pid" 2>/dev/null; then ok "the pid is gone after the quit"; else bad "pid $live_pid survived the quit"; cleanup_instances; fi

out=$(sv restart 2>&1); expect fail "a restart of a stopped instance is refused" "$out" $?

echo "== controlled restart preserves state"
sv start >/dev/null 2>&1 || { bad "restart fixture: start failed"; }
before=$(settings_digest)
out=$(sv restart 2>&1)
expect pass "the controlled restart succeeds" "$out" $?
if [[ "$out" == *"checksum-identical before and after"* ]]; then ok "restart proves the state files unchanged"; else bad "restart did not prove the state: $out"; fi
if [ "$before" = "$(settings_digest)" ]; then ok "the config file survived byte-for-byte"; else bad "the config file changed: before '$before' after '$(settings_digest)'"; fi
backups=$(ls -1d "$state/supervisor"/backup/* 2>/dev/null | wc -l)
if [ "$backups" -ge 1 ]; then ok "a verified backup was taken ($backups)"; else bad "no backup directory was left behind"; fi
if find "$state/supervisor"/backup -name manifest.sha256 | grep -q .; then ok "the backup carries its checksum manifest"; else bad "no checksum manifest in the backup"; fi
out=$(sv status 2>&1)
if [[ "$out" == *"running (pid "* ]]; then ok "status is live after the restart"; else bad "status after restart: $out"; fi
cleanup_instances

echo "== a state change across the restart is caught"
printf 'settings v1\n' >"$state/config/slashit-app/settings.toml"
sv start >/dev/null 2>&1 || bad "tamper fixture: start failed"
# The restart spawns a fresh instance that (told by the stub variable) writes
# into a compared state file at boot, after the backup was taken.
out=$(SLASHIT_TEST_STARTUP_TOUCH="$state/config/slashit-app/settings.toml" \
  env "${BASEENV[@]}" bash "$sup" restart 2>&1)
status=$?
if [ "$status" -eq 1 ] && [[ "$out" == *"state files changed during the restart"* ]]; then
  ok "a boot-time write into state is refused as a restart divergence"
else
  bad "the tamper case did not fail as expected (exit $status)"; printf '%s\n' "$out" >&2
fi
cleanup_instances
printf 'settings v1\n' >"$state/config/slashit-app/settings.toml"

echo "== graceful stop that needs --force"
export SLASHIT_TEST_STUB_ROLE=noquit
sv start >/dev/null 2>&1 || { bad "force fixture: start failed"; cleanup_instances; }
out=$(sv stop 2>&1)
st=$?
unset SLASHIT_TEST_STUB_ROLE
if [ "$st" -eq 1 ] && [[ "$out" == *"--force"* ]]; then
  ok "a stuck graceful stop is refused with the --force instruction"
else
  bad "a stuck graceful stop did not refuse (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv stop --force 2>&1)
st=$?
if [ "$st" -eq 0 ]; then ok "stop --force escalates and ends the stuck instance"; else
  bad "stop --force failed (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
