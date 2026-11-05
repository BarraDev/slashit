#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Tests scripts/supervise-dogfood.sh against a stub slashit-ui binary and a
# stub slashit CLI inside a throwaway directory (mirroring the real geometry:
# a workspace checkout holding target/debug beside the isolated state base).
# Every isolated environment value is redirected into the throwaway, so the
# test never touches the runner's real ~/.local/state tree, /run/user tree,
# or GUI, and never starts a real instance. Each case names what it proves; a
# failure prints the supervisor's own output.
#
# usage: scripts/test-supervise-dogfood.sh
set -uo pipefail

EXPECTED_TOTAL=50

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
repo="$work/repo"
bin_dir="$repo/target/debug"
socket="$runtime/slashit-app/slashit.sock"
mkdir -p "$bin_dir" \
  "$state/config/slashit-app" \
  "$state/data/slashit-app/logs" \
  "$state/data/slashit-app/pr-helper-logs" \
  "$state/data/slashit-app/worktrees" \
  "$stub_dir" "$runtime" "$runtime/slashit-app"

export SLASHIT_TEST_STUB_DIR="$stub_dir"
export SLASHIT_TEST_SOCKET="$socket"

# --- stubs -------------------------------------------------------------------

# Stub of the slashit-ui binary. Answers --version, records the environment it
# was exec'd with (the never-fall-back check reads that file), then runs until
# the stub CLI writes the quit flag, or until it is SIGTERMed externally. With
# SLASHIT_TEST_STUB_ROLE=noquit it ignores the flag, and the force escalation
# is exercised against it. Any staged boot-time write into state happens
# BEFORE the pid file is written: the supervisor's ping only sees the instance
# once that file exists, so a staged write is always visible to the restart's
# divergence check — the ordering makes the tamper case deterministic.
cat >"$bin_dir/slashit-ui" <<'STUB'
#!/usr/bin/env bash
stub_dir=$SLASHIT_TEST_STUB_DIR
if [ "${1:-}" = "--version" ]; then
  echo "slashit-ui 9.9.9-stub"
  exit 0
fi
env >"$stub_dir/environ"
if [ -n "${SLASHIT_TEST_STARTUP_TOUCH:-}" ]; then
  printf 'boot write\n' >"$SLASHIT_TEST_STARTUP_TOUCH"
fi
if [ -n "${SLASHIT_TEST_STARTUP_REMOVE:-}" ]; then
  rm -f "$SLASHIT_TEST_STARTUP_REMOVE"
fi
echo "$$" >"$stub_dir/pid"
rm -f "$stub_dir/quit"
touch "$SLASHIT_TEST_SOCKET"
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

# Stub of the `slashit` CLI: answers --version (provenance), answers ping
# while the stub instance is up, sends its quit flag otherwise, in the same
# JSON shape the real CLI prints.
cat >"$stub_dir/slashit" <<'STUB'
#!/usr/bin/env bash
stub_dir=$SLASHIT_TEST_STUB_DIR
last=
for a in "$@"; do
  case $a in --endpoint | --json) ;; *) last=$a ;; esac
done
case $last in
  --version)
    echo "slashit 9.9.9-stub"
    exit 0
    ;;
  ping)
    if [ -n "${SLASHIT_TEST_CLI_HANG:-}" ]; then
      exec sleep 300
    fi
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
git init -q "$repo"
git -C "$repo" config user.name "Supervise Test"
git -C "$repo" config user.email supervise-test@example.invalid
printf test >"$repo/provenance.txt"
git -C "$repo" add provenance.txt
git -C "$repo" commit -q -m provenance
commit_id=$(git -C "$repo" rev-parse HEAD)

# Application state that a restart must preserve.
printf 'settings v1\n' >"$state/config/slashit-app/settings.toml"
printf 'session 7\n' >"$state/data/slashit-app/board.toml"
printf 'transcript\n' >"$state/data/slashit-app/logs/agent.txt"
printf 'helper log\n' >"$state/data/slashit-app/pr-helper-logs/helper.txt"
printf 'ended\n' >"$state/data/slashit-app/terminal_sessions.toml"
printf 'uncommitted checkout content\n' >"$state/data/slashit-app/worktrees/task.txt"

BASEENV=(
  SLASHIT_DOGFOOD_STATE="$state"
  SLASHIT_DOGFOOD_RUNTIME="$runtime"
  SLASHIT_DOGFOOD_BINARY="$bin_dir/slashit-ui"
  SLASHIT_DOGFOOD_CLI="$stub_dir/slashit"
  SLASHIT_DOGFOOD_START_TIMEOUT=10
  SLASHIT_DOGFOOD_STOP_TIMEOUT=8
  SLASHIT_DOGFOOD_FORCE_TIMEOUT=5
  # A runner's desktop session may carry ambient XDG_* values; neutralized so
  # the run verb's ambient-agreement check behaves the same on every runner.
  XDG_CONFIG_HOME=
  XDG_DATA_HOME=
  XDG_CACHE_HOME=
  XDG_STATE_HOME=
  XDG_RUNTIME_DIR=
  GH_CONFIG_DIR=
)

# sv [ASSIGNMENTS...] [VERB/ARG...]: '='-assignments go to env, everything
# else goes to the supervisor's argv. env itself stops assignment parsing at
# the first non-assignment, so the split below follows the same rule; a bare
# verb must never reach env as the program to exec.
sv() {
  local a seen_arg=0
  local -a assignments=() argv=()
  for a in "$@"; do
    if [ "$seen_arg" = 0 ] && [ "${a#*=}" != "$a" ]; then
      assignments+=("$a")
    else
      seen_arg=1
      argv+=("$a")
    fi
  done
  # Every supervisor invocation is bounded: a step that never returns fails
  # its case with status 124 instead of hanging the whole suite.
  timeout -k 5 "${SV_TIMEOUT:-120}" env "${BASEENV[@]}" "${assignments[@]}" bash "$sup" "${argv[@]}"
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

expect() { # want <pass|fail|input> <label> <supervisor-output> <status> [reason]
  local want=$1 label=$2 output=$3 status=$4 reason=${5:-}
  local first=${output%%$'\n'*}
  local self=supervise-dogfood.sh
  case $want in
    pass)
      if [ "$status" -eq 0 ]; then ok "$label"; else
        bad "$label: expected success, exited $status"; printf '%s\n' "$output" >&2
      fi
      ;;
    fail)
      # A refusal counts only with the reason this case must carry. Any other
      # exit 1 is a failure, so an unrelated refusal cannot satisfy the case.
      if [ -z "$reason" ]; then
        bad "$label: harness error, no expected refusal reason given"
      elif [ "$status" -eq 1 ] && [[ "$output" == *"$reason"* ]]; then
        ok "$label (refused: $reason)"
      else
        bad "$label: expected exit 1 with '$reason', exited $status"; printf '%s\n' "$output" >&2
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

echo "== start, status and verify"
out=$(sv start 2>&1); st=$?
if [ "$st" -eq 0 ] && [[ "$out" == *"started pid "* ]]; then
  ok "the first start succeeds and reports the spawned pid"
else
  bad "the first start did not succeed with a pid report (exit $st)"; printf '%s\n' "$out" >&2
fi

live_pid=$(stub_pid)
out=$(sv status 2>&1)
if [[ "$out" == *"running (pid $live_pid"* ]]; then ok "status reports the live pid"; else bad "status lost the pid: $out"; fi
if [[ "$out" == *"binary:         $bin_dir/slashit-ui"* ]]; then ok "status reports the binary path"; else bad "status lost the binary: $out"; fi
if [[ "$out" == *"commit:         $commit_id"* ]]; then ok "status reports the provenance commit"; else bad "status lost the commit: $out"; fi
if [[ "$out" == *"endpoint:       unix:$socket"* ]]; then ok "status reports the endpoint"; else bad "status lost the endpoint: $out"; fi

out=$(sv verify 2>&1); expect pass "verify confirms the endpoint and environment" "$out" $?

out=$(sv start 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"already answers on"* ]]; then
  ok "a second start is refused while the instance answers (refused: ${out%%$'\n'*})"
else
  bad "the second start was not refused with the live-instance reason (exit $st)"; printf '%s\n' "$out" >&2
fi

echo "== the unit entry point (run) is equally guarded"
out=$(sv run 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"already answers on"* ]]; then
  ok "run, the systemd entry point, refuses while an instance answers"
else
  bad "run did not refuse a second instance (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv XDG_CONFIG_HOME="/run/user/$(id -u)/ambient" run 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"disagrees with the declared value"* ]]; then
  ok "run refuses an ambient environment that disagrees with the declaration"
else
  bad "run did not refuse a disagreeing ambient environment (exit $st)"; printf '%s\n' "$out" >&2
fi

echo "== what is said when no CLI is available (instance is live)"
out=$(sv SLASHIT_DOGFOOD_CLI= status 2>&1); st=$?
if [ "$st" -eq 3 ] && [[ "$out" == *"cannot verify (no CLI)"* ]] && [[ "$out" != *"instance:       stopped"* ]]; then
  ok "status without a CLI reports cannot-verify, and never claims the live instance is stopped"
else
  bad "status without a CLI misreported a live instance: $out"
fi
out=$(sv SLASHIT_DOGFOOD_CLI= verify 2>&1); st=$?
if [ "$st" -eq 3 ] && [[ "$out" == *"cannot verify (no CLI)"* ]]; then
  ok "verify without a CLI reports cannot-verify instead of no-instance-answers"
else
  bad "verify without a CLI did not report cannot-verify (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv SLASHIT_DOGFOOD_CLI= stop 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"cannot stop gracefully (no CLI)"* ]]; then
  ok "a graceful stop is refused when only signals would be available"
else
  bad "stop without a CLI did not refuse (exit $st)"; printf '%s\n' "$out" >&2
fi

echo "== the declared environment reached the process"
env_ok=1
default_ok=1
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
if grep -q "^XDG_CONFIG_HOME=$HOME/.config$" "$stub_dir/environ" 2>/dev/null ||
  grep -q "^XDG_DATA_HOME=$HOME/.local/share$" "$stub_dir/environ" 2>/dev/null ||
  grep -q "^XDG_CACHE_HOME=$HOME/.cache$" "$stub_dir/environ" 2>/dev/null ||
  grep -q "^XDG_STATE_HOME=$HOME/.local/state$" "$stub_dir/environ" 2>/dev/null ||
  grep -q "^XDG_RUNTIME_DIR=/run/user/$(id -u)$" "$stub_dir/environ" 2>/dev/null; then
  default_ok=0
fi
if [ "$env_ok" = 1 ] && [ "$default_ok" = 1 ]; then
  ok "the process carries every declared XDG_* and GH_CONFIG_DIR value and no HOME-derived default"
else
  bad "the process environment diverges from the declaration (env_ok $env_ok, default_ok $default_ok)"
  printf '%s\n' "$(cat "$stub_dir/environ" 2>/dev/null)" >&2
fi

echo "== never fall back to default XDG directories"
out=$(sv SLASHIT_DOGFOOD_RUNTIME="/run/user/$(id -u)" start 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"default directory derived from HOME"* ]]; then
  ok "the default runtime directory is refused"
else
  bad "the default runtime directory was not refused (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv SLASHIT_DOGFOOD_STATE="$HOME/.local/state" start 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"default directory derived from HOME"* ]]; then
  ok "the default state home as the state base is refused"
else
  bad "the default state base was not refused (exit $st)"; printf '%s\n' "$out" >&2
fi

echo "== graceful stop"
out=$(sv stop 2>&1); stop_rc=$?
status_out=$(sv status 2>&1)
if [ "$stop_rc" -eq 0 ] &&
  [[ "$status_out" == *"instance:       stopped"* ]] &&
  ! kill -0 "$live_pid" 2>/dev/null; then
  ok "the graceful stop succeeds, status reports stopped and the recorded pid is gone"
else
  bad "graceful stop did not end the instance cleanly (exit $stop_rc): $status_out"; cleanup_instances
fi

echo "== one lifecycle operation at a time"
( sv start >"$work/conc_a.out" 2>&1; echo $? >"$work/conc_a.rc" ) &
pa=$!
( sv start >"$work/conc_b.out" 2>&1; echo $? >"$work/conc_b.rc" ) &
pb=$!
wait "$pa"; wait "$pb"
r_a=$(cat "$work/conc_a.rc"); r_b=$(cat "$work/conc_b.rc")
if [ "$((r_a + r_b))" -eq 1 ]; then
  if [ "$r_a" = 1 ]; then loser="$work/conc_a.out"; else loser="$work/conc_b.out"; fi
  if grep -q "refusing" "$loser" &&
    grep -qE "another lifecycle operation|already answers on" "$loser"; then
    ok "two concurrent starts yield exactly one instance and one refusal"
  else
    bad "the losing start exited for an unexpected reason: $(cat "$loser")"
  fi
else
  bad "concurrent starts did not produce exactly one success and one refusal (rc: $r_a and $r_b)"
  printf 'conc_a:\n' >&2; cat "$work/conc_a.out" >&2
  printf 'conc_b:\n' >&2; cat "$work/conc_b.out" >&2
fi

echo "== stop the concurrency winner so the provenance gates can be tested alone"
out=$(sv stop 2>&1); expect pass "the concurrency winner stops gracefully" "$out" $?

echo "== provenance gates (no instance, so each refusal is its own gate)"
out=$(sv SLASHIT_DOGFOOD_BINARY="$work/absent/slashit-ui" start 2>&1)
expect fail "a missing binary is refused" "$out" $? "the binary is missing"
printf '#!/usr/bin/env bash\nexit 3\n' >"$work/no-version"
chmod +x "$work/no-version"
out=$(sv SLASHIT_DOGFOOD_BINARY="$work/no-version" start 2>&1)
expect fail "a binary without --version is refused" "$out" $? "does not answer --version"
printf '#!/usr/bin/env bash\nexit 3\n' >"$work/no-version-cli"
chmod +x "$work/no-version-cli"
out=$(sv SLASHIT_DOGFOOD_CLI="$work/no-version-cli" start 2>&1)
expect fail "a CLI without --version is refused (an unverified slashit is not trusted)" "$out" $? "does not answer --version"

echo "== a graceful stop that needs --force"
export SLASHIT_TEST_STUB_ROLE=noquit
sv start >/dev/null 2>&1 || { bad "force fixture: start failed"; cleanup_instances; }
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop 2>&1); st=$?
unset SLASHIT_TEST_STUB_ROLE
if [ "$st" -eq 1 ] && [[ "$out" == *"--force"* ]]; then
  ok "a stuck graceful stop is refused with the --force instruction"
else
  bad "a stuck graceful stop did not refuse (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv SLASHIT_DOGFOOD_STOP_TIMEOUT=2 SLASHIT_DOGFOOD_FORCE_TIMEOUT=2 stop --force 2>&1); st=$?
if [ "$st" -eq 0 ]; then
  ok "stop --force escalates and ends the stuck instance"
else
  bad "stop --force failed (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== restart"
out=$(sv restart 2>&1); expect fail "a restart of a stopped instance is refused" "$out" $? "nothing found to operate on"

sv start >/dev/null 2>&1 || { bad "restart fixture: start failed"; }
before=$(settings_digest)
out=$(sv restart 2>&1)
expect pass "the controlled restart succeeds" "$out" $?
if [[ "$out" == *"checksum-identical before and after"* ]] && [ "$before" = "$(settings_digest)" ]; then
  ok "the restart proves the state files unchanged, byte-for-byte"
else
  bad "the restart did not prove the state unchanged (digest before '$before', after '$(settings_digest)')"; printf '%s\n' "$out" >&2
fi

echo "== the backup contains exactly the compared snapshot set"
manifest=$(ls -1t "$state"/supervisor/backup/*/manifest.sha256 2>/dev/null | head -n 1)
if [ -n "$manifest" ] &&
  grep -q 'config/slashit-app/settings.toml' "$manifest" &&
  ! grep -qE 'slashit-app/(logs|pr-helper-logs)/|terminal_sessions\.toml|slashit-app/worktrees/' "$manifest"; then
  ok "a verified backup exists and carries only the compared set (no logs, pr-helper logs, terminal sessions, worktrees)"
else
  bad "the backup manifest is missing or covers the wrong set: ${manifest:-none}"
  [ -n "$manifest" ] && cat "$manifest" >&2
fi

echo "== a state change across the restart is caught"
printf 'settings v1\n' >"$state/config/slashit-app/settings.toml"
# The instance the previous restart left running is the fixture.
sv verify >/dev/null 2>&1 || bad "tamper fixture: no verified running instance"
# The restart spawns a fresh instance that (told by the stub variable) writes
# into a compared state file at boot, after the backup was taken. The stub
# writes the file before its pid file, so the write is always visible to the
# supervisor before its ping succeeds; the divergence is deterministic.
out=$(sv SLASHIT_TEST_STARTUP_TOUCH="$state/config/slashit-app/settings.toml" restart 2>&1)
status=$?
if [ "$status" -eq 1 ] && [[ "$out" == *"state files changed during the restart"* ]]; then
  ok "a boot-time write into state is refused as a restart divergence"
else
  bad "the tamper case did not fail as expected (exit $status)"; printf '%s\n' "$out" >&2
fi
cleanup_instances
printf 'settings v1\n' >"$state/config/slashit-app/settings.toml"

# A small protected-state fixture: two compared files and two excluded ones.
make_state() {
  mkdir -p "$1/config/slashit-app" "$1/data/slashit-app/logs" "$1/data/slashit-app/worktrees"
  printf 'settings v1\n' >"$1/config/slashit-app/settings.toml"
  printf 'session 7\n' >"$1/data/slashit-app/board.toml"
  printf 'transcript\n' >"$1/data/slashit-app/logs/agent.txt"
  printf 'uncommitted\n' >"$1/data/slashit-app/worktrees/task.txt"
}

echo "== default discovery: no explicit binary or CLI, inside a git checkout"
disc=$work/disc
mkdir -p "$disc/main/target/debug" "$disc/main/scripts"
cp "$bin_dir/slashit-ui" "$disc/main/target/debug/slashit-ui"
cp "$stub_dir/slashit" "$disc/main/target/debug/slashit"
cp "$sup" "$disc/main/scripts/supervise-dogfood.sh"
git -C "$disc/main" init -q
git -C "$disc/main" config user.name "Supervise Test"
git -C "$disc/main" config user.email supervise-test@example.invalid
git -C "$disc/main" add scripts/supervise-dogfood.sh
git -C "$disc/main" commit -q -m layout
d_env=(SLASHIT_DOGFOOD_STATE="$state" SLASHIT_DOGFOOD_RUNTIME="$runtime"
  XDG_CONFIG_HOME= XDG_DATA_HOME= XDG_CACHE_HOME= XDG_STATE_HOME= XDG_RUNTIME_DIR= GH_CONFIG_DIR=)
out=$(cd / && env "${d_env[@]}" timeout 60 bash "$disc/main/scripts/supervise-dogfood.sh" env 2>&1); st=$?
if [ "$st" -eq 0 ] && grep -qxE "BINARY +$disc/main/target/debug/slashit-ui" <<<"$out" &&
  grep -qxE "CLI +$disc/main/target/debug/slashit" <<<"$out"; then
  ok "default discovery finds the binary and the CLI beside it, with no overrides"
else
  bad "default discovery did not resolve the checkout layout (exit $st)"; printf '%s\n' "$out" >&2
fi

git -C "$disc/main" worktree add -q "$disc/task" -b task-probe >/dev/null 2>&1
out=$(cd / && env "${d_env[@]}" timeout 60 bash "$disc/task/scripts/supervise-dogfood.sh" env 2>&1); st=$?
if [ "$st" -eq 0 ] && grep -qxE "BINARY +$disc/main/target/debug/slashit-ui" <<<"$out"; then
  ok "discovery from a linked worktree (the Task Checkout geometry) resolves the main checkout's build"
else
  bad "discovery from a linked worktree did not resolve the main checkout (exit $st)"; printf '%s\n' "$out" >&2
fi

rm -f "$disc/main/target/debug/slashit"
out=$(cd / && env "${d_env[@]}" timeout 60 bash "$disc/main/scripts/supervise-dogfood.sh" status 2>&1); st=$?
if [ "$st" -eq 3 ] && [[ "$out" == *"cannot verify (no CLI)"* ]]; then
  ok "without a CLI beside the binary, status exits 3 and says it cannot verify"
else
  bad "status without a discovered CLI did not exit 3 (exit $st)"; printf '%s\n' "$out" >&2
fi

echo "== protected state: missing or unprovable evidence refuses before anything is stopped"
cleanup_instances
empty=$work/empty-state
mkdir -p "$empty/config" "$empty/data/slashit-app/logs" "$empty/data/slashit-app/worktrees"
printf 'log\n' >"$empty/data/slashit-app/logs/agent.txt"
printf 'wt\n' >"$empty/data/slashit-app/worktrees/task.txt"
out=$(sv SLASHIT_DOGFOOD_STATE="$empty" start 2>&1); st=$?
[ "$st" -eq 0 ] || { bad "empty-set fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2; }
out=$(sv SLASHIT_DOGFOOD_STATE="$empty" restart 2>&1); st=$?
still=$(stub_pid)
if [ "$st" -eq 1 ] && [[ "$out" == *"cannot be verified"* ]] && [ -n "$still" ] && kill -0 "$still" 2>/dev/null; then
  ok "a restart with no compared files is refused, and the running instance is left untouched"
else
  bad "a restart with no compared files was not refused before stopping (exit $st, pid '$still')"; printf '%s\n' "$out" >&2
fi
cleanup_instances

unread=$work/unread-state
make_state "$unread"
if [ "$(id -u)" -eq 0 ]; then
  bad "the unreadable-file case needs a non-root runner (root reads every file)"
else
  out=$(sv SLASHIT_DOGFOOD_STATE="$unread" start 2>&1); st=$?
  [ "$st" -eq 0 ] || { bad "unreadable fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2; }
  chmod 000 "$unread/config/slashit-app/settings.toml"
  out=$(sv SLASHIT_DOGFOOD_STATE="$unread" restart 2>&1); st=$?
  chmod 600 "$unread/config/slashit-app/settings.toml"
  still=$(stub_pid)
  if [ "$st" -eq 1 ] && [[ "$out" == *"cannot be verified"* ]] && [[ "$out" == *"Permission denied"* ]] &&
    [ -n "$still" ] && kill -0 "$still" 2>/dev/null; then
    ok "an unreadable compared file is refused with its cause, before the instance is stopped"
  else
    bad "an unreadable compared file was not refused before stopping (exit $st, pid '$still')"; printf '%s\n' "$out" >&2
  fi
fi
cleanup_instances

rmst=$work/rm-state
make_state "$rmst"
out=$(sv SLASHIT_DOGFOOD_STATE="$rmst" start 2>&1); st=$?
[ "$st" -eq 0 ] || { bad "removal fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2; }
out=$(sv SLASHIT_DOGFOOD_STATE="$rmst" SLASHIT_TEST_STARTUP_REMOVE="$rmst/config/slashit-app/settings.toml" restart 2>&1); st=$?
if [ "$st" -eq 1 ] && [[ "$out" == *"state files changed during the restart"* ]] && [[ "$out" == *"settings.toml"* ]]; then
  ok "a protected file removed at boot is refused as a restart divergence, and named"
else
  bad "a protected file removed at boot was not refused (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

expect fail "a relative state base is refused, not resolved against the caller's directory" \
  "$(sv SLASHIT_DOGFOOD_STATE=relative/state start 2>&1)" $? "must be an absolute path"

gone=$work/gone-state
make_state "$gone"
out=$(sv SLASHIT_DOGFOOD_STATE="$gone" start 2>&1); st=$?
[ "$st" -eq 0 ] || { bad "missing-directory fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2; }
rm -rf "$gone/config"
out=$(sv SLASHIT_DOGFOOD_STATE="$gone" restart 2>&1); st=$?
still=$(stub_pid)
if [ "$st" -eq 1 ] && [[ "$out" == *"cannot be verified"* ]] && [ -n "$still" ] && kill -0 "$still" 2>/dev/null; then
  ok "a protected directory missing before a restart is refused, and the running instance is left untouched"
else
  bad "a missing protected directory was not refused before stopping (exit $st, pid '$still')"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo "== status and verify follow the documented exit contract"
out=$(sv status 2>&1); st=$?
if [ "$st" -eq 0 ] && [[ "$out" == *"instance:       stopped"* ]]; then
  ok "status exits 0 and reports stopped when nothing runs"
else
  bad "status did not report a clean stop (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv verify 2>&1); expect fail "verify exits 1 when no instance answers" "$out" $? "no instance answers"
out=$(sv start 2>&1); st=$?
[ "$st" -eq 0 ] || { bad "exit-contract fixture: start failed (exit $st)"; printf '%s\n' "$out" >&2; }
out=$(sv status 2>&1); st=$?
if [ "$st" -eq 0 ] && [[ "$out" == *"running (pid"* ]]; then
  ok "status exits 0 for a running and verified instance"
else
  bad "status did not exit 0 for a running instance (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv verify 2>&1); expect pass "verify exits 0 for a running and verified instance" "$out" $?
out=$(sv SLASHIT_DOGFOOD_CLI= status 2>&1); st=$?
if [ "$st" -eq 3 ] && [[ "$out" == *"cannot verify (no CLI)"* ]]; then
  ok "status exits 3 when the endpoint is unverifiable (no CLI)"
else
  bad "status without a CLI did not exit 3 (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv SLASHIT_DOGFOOD_CLI= verify 2>&1); st=$?
if [ "$st" -eq 3 ] && [[ "$out" == *"cannot verify (no CLI)"* ]]; then
  ok "verify exits 3 when the endpoint is unverifiable (no CLI)"
else
  bad "verify without a CLI did not exit 3 (exit $st)"; printf '%s\n' "$out" >&2
fi
t0=$SECONDS
out=$(sv SLASHIT_TEST_CLI_HANG=1 SLASHIT_DOGFOOD_CLI_TIMEOUT=2 status 2>&1); st=$?
elapsed=$((SECONDS - t0))
if [ "$st" -eq 3 ] && [[ "$out" == *"did not answer within 2s"* ]] && [ "$elapsed" -lt 30 ]; then
  ok "a hung control channel is bounded (${elapsed}s) and reported as unverifiable, exit 3"
else
  bad "a hung control channel was not bounded and reported (exit $st, ${elapsed}s)"; printf '%s\n' "$out" >&2
fi
out=$(sv stop 2>&1); st=$?
[ "$st" -eq 0 ] || { bad "exit-contract fixture: graceful stop failed (exit $st)"; printf '%s\n' "$out" >&2; }
env XDG_CONFIG_HOME="$work/not-declared/config" XDG_DATA_HOME="$state/data" XDG_CACHE_HOME="$state/cache" \
  XDG_STATE_HOME="$state" XDG_RUNTIME_DIR="$runtime" GH_CONFIG_DIR="$HOME/.config/gh" \
  SLASHIT_TEST_STUB_DIR="$stub_dir" SLASHIT_TEST_SOCKET="$socket" \
  "$bin_dir/slashit-ui" >"$work/mismatch.log" 2>&1 &
for _ in $(seq 100); do [ -s "$stub_dir/pid" ] && break; sleep 0.05; done
out=$(sv status 2>&1); st=$?
if [ "$st" -eq 4 ] && [[ "$out" == *"MISMATCH"* ]]; then
  ok "status exits 4 for a live instance whose environment differs from the declaration"
else
  bad "status did not exit 4 for an environment mismatch (exit $st)"; printf '%s\n' "$out" >&2
fi
out=$(sv verify 2>&1); st=$?
if [ "$st" -eq 4 ] && [[ "$out" == *"MISMATCH"* ]]; then
  ok "verify exits 4 for the same environment mismatch"
else
  bad "verify did not exit 4 for an environment mismatch (exit $st)"; printf '%s\n' "$out" >&2
fi
cleanup_instances

echo
if [ "$((passed + failed))" -ne "$EXPECTED_TOTAL" ]; then
  bad "the case count drifted: $((passed + failed)) cases checked, expected $EXPECTED_TOTAL"
fi
echo "$passed passed, $failed failed ($EXPECTED_TOTAL expected)"
[ "$failed" -eq 0 ] && [ "$((passed + failed))" -eq "$EXPECTED_TOTAL" ]
